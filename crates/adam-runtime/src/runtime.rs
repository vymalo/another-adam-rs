//! [`Runtime`]: starting, feeding, cancelling and observing runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use adam_core::{DynStore, NewRun, RunId, RunRecord, RunStatus, RunUpdate, StoreError};

use crate::agent::{Agent, AgentError, Inbound};
use crate::cancel::CancelToken;
use crate::clock::{Clock, DynClock, SystemClock};
use crate::envelope::Envelope;
use crate::erased::{Erased, ErasedAgent};
use crate::events::{Artifact, DynEventSink, EventSink, NoopSink, RunEvent};
use crate::retry::RetryPolicy;

/// How often a commit that lost a CAS race is retried before giving up.
pub(crate) const MAX_COMMIT_RETRIES: usize = 16;

/// Why a [`Runtime`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// No agent with this name was registered on the builder.
    #[error("unknown agent {0:?}")]
    UnknownAgent(String),
    /// The run does not exist.
    #[error("run {0} not found")]
    NotFound(RunId),
    /// The run already finished, so it cannot take more input.
    #[error("run {run} is already {status}")]
    Finished {
        /// The run.
        run: RunId,
        /// Its terminal status.
        status: RunStatus,
    },
    /// The conversation already has an open run (only reported by
    /// [`Runtime::start_with_id`]; [`Runtime::start`] delivers instead).
    #[error("conversation {conversation_id:?} of agent {agent:?} already has an open run")]
    ConversationBusy {
        /// Agent name.
        agent: String,
        /// Conversation id.
        conversation_id: String,
    },
    /// The stored state is not a valid envelope of a supported version.
    #[error("run {run} has an unreadable state envelope: {reason}")]
    Corrupt {
        /// The run.
        run: RunId,
        /// What is wrong with it.
        reason: String,
    },
    /// Gave up after repeatedly losing commit races on the same run.
    #[error("too much contention on {0}, gave up retrying")]
    Contended(String),
    /// The agent's `init` failed.
    #[error(transparent)]
    Agent(#[from] AgentError),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl RuntimeError {
    /// Whether retrying the same call later may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Contended(_) => true,
            Self::Store(e) => matches!(e, StoreError::Backend(_) | StoreError::Conflict { .. }),
            _ => false,
        }
    }
}

/// A read-only snapshot of a run, built from its durable record only, so a
/// consumer can poll it to reconstruct everything after a restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunView {
    /// The run.
    pub id: RunId,
    /// Name of its agent.
    pub agent: String,
    /// Its conversation, if any.
    pub conversation_id: Option<String>,
    /// Lifecycle status.
    pub status: RunStatus,
    /// Timer: when a parked run wakes, or a runnable run (retry backoff)
    /// becomes due.
    pub wake_at: Option<DateTime<Utc>>,
    /// Parked with no timer: only an inbound message (or cancel) resumes it.
    /// This is the "input required" state.
    pub waiting: bool,
    /// The result, once `Done`.
    pub output: Option<Value>,
    /// The reason, once `Failed` (a cancel reads `cancelled: <reason>`).
    pub error: Option<String>,
    /// Failed tries of the current transition so far (`0` when healthy). It is
    /// what a retry backoff is computed from; after retries are exhausted it
    /// equals `max_attempts`.
    pub attempt: u32,
    /// Messages delivered but not yet consumed by the agent.
    pub pending_inbox: usize,
    /// Artifacts emitted by committed transitions, in order.
    pub artifacts: Vec<Artifact>,
    /// The agent's own serialized state (e.g. the question of a parked run).
    pub state: Value,
    /// Store version of the record (bumps on every commit).
    pub version: u64,
    /// When the run was created.
    pub created_at: DateTime<Utc>,
    /// Last commit time.
    pub updated_at: DateTime<Utc>,
}

impl RunView {
    fn new(rec: RunRecord, env: Envelope) -> Self {
        Self {
            id: rec.id,
            agent: rec.agent,
            conversation_id: rec.conversation_id,
            status: rec.status,
            wake_at: rec.wake_at,
            waiting: rec.status == RunStatus::Parked && rec.wake_at.is_none(),
            output: (rec.status == RunStatus::Done).then_some(env.output),
            error: env.error,
            attempt: env.attempt,
            pending_inbox: env.inbox.len(),
            artifacts: env.artifacts,
            state: env.agent,
            version: rec.version,
            created_at: rec.created_at,
            updated_at: rec.updated_at,
        }
    }
}

pub(crate) struct Config {
    pub worker_id: String,
    pub lease_ttl: Duration,
    pub poll_interval: Duration,
    pub retry: RetryPolicy,
    pub concurrency: usize,
    pub lease_renewal: bool,
}

pub(crate) struct Inner {
    pub store: DynStore,
    pub agents: HashMap<String, Arc<dyn ErasedAgent>>,
    pub sink: DynEventSink,
    pub clock: DynClock,
    pub cfg: Config,
    /// Bumped whenever local work appears, so local workers do not wait for
    /// the next poll.
    pub wake: watch::Sender<u64>,
    /// Cancellation tokens of the transitions this runtime is stepping now.
    pub in_flight: Mutex<HashMap<RunId, CancelToken>>,
}

impl Inner {
    /// Register the token of a transition about to run; the returned guard
    /// removes it again.
    pub fn track(self: &Arc<Self>, run: RunId, token: CancelToken) -> TrackGuard {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run, token);
        TrackGuard {
            inner: self.clone(),
            run,
        }
    }

    /// Fire the token of `run` if this runtime is stepping it.
    pub fn fire_cancel(&self, run: RunId) {
        let token = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&run)
            .cloned();
        if let Some(token) = token {
            token.cancel();
        }
    }

    pub fn notify_workers(&self) {
        self.wake.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub async fn emit_status(
        &self,
        run: RunId,
        agent: &str,
        status: RunStatus,
        detail: Option<String>,
    ) {
        self.sink
            .emit(run, agent, RunEvent::Status { status, detail })
            .await;
    }
}

/// Removes a run's cancellation token from the in-flight map on drop.
pub(crate) struct TrackGuard {
    inner: Arc<Inner>,
    run: RunId,
}

impl Drop for TrackGuard {
    fn drop(&mut self) {
        self.inner
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.run);
    }
}

/// Configures and builds a [`Runtime`].
pub struct RuntimeBuilder {
    store: DynStore,
    agents: HashMap<String, Arc<dyn ErasedAgent>>,
    sink: DynEventSink,
    clock: DynClock,
    cfg: Config,
}

impl RuntimeBuilder {
    /// Register an agent. Registering a second agent with the same name
    /// replaces the first.
    pub fn agent<A: Agent>(mut self, agent: A) -> Self {
        let erased = Erased(agent);
        let name = erased.name().to_owned();
        if self.agents.insert(name.clone(), Arc::new(erased)).is_some() {
            tracing::warn!(agent = %name, "agent registered twice, keeping the last");
        }
        self
    }

    /// Where progress events go. Default: [`NoopSink`].
    pub fn event_sink(mut self, sink: impl EventSink) -> Self {
        self.sink = Arc::new(sink);
        self
    }

    /// Identity written into leases. Must be unique per worker process (or
    /// per `Runtime` when several share a store). Default: random.
    pub fn worker_id(mut self, id: impl Into<String>) -> Self {
        self.cfg.worker_id = id.into();
        self
    }

    /// How long a claimed run stays leased without renewal. Default 30 s.
    pub fn lease_ttl(mut self, ttl: Duration) -> Self {
        self.cfg.lease_ttl = ttl;
        self
    }

    /// How often an idle worker polls for due runs. Default 250 ms.
    /// `deliver`/`start` on the same `Runtime` wake local workers at once.
    pub fn poll_interval(mut self, every: Duration) -> Self {
        self.cfg.poll_interval = every;
        self
    }

    /// Backoff for transient errors. Default: [`RetryPolicy::default`].
    pub fn retry(mut self, policy: RetryPolicy) -> Self {
        self.cfg.retry = policy;
        self
    }

    /// Runs advanced at the same time by one `run_worker`. Default 4, minimum 1.
    pub fn concurrency(mut self, n: usize) -> Self {
        self.cfg.concurrency = n.max(1);
        self
    }

    /// Whether a lease is renewed (every `ttl / 3`) while its run is being
    /// stepped. On by default; turn it off in tests that need a lease to
    /// expire under a slow step.
    pub fn lease_renewal(mut self, enabled: bool) -> Self {
        self.cfg.lease_renewal = enabled;
        self
    }

    /// Time source. Default: [`SystemClock`].
    pub fn clock(mut self, clock: impl Clock) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Build the runtime.
    pub fn build(self) -> Runtime {
        Runtime {
            inner: Arc::new(Inner {
                store: self.store,
                agents: self.agents,
                sink: self.sink,
                clock: self.clock,
                cfg: self.cfg,
                wake: watch::channel(0).0,
                in_flight: Mutex::default(),
            }),
        }
    }
}

/// The durable agent-loop runtime: start runs, feed them messages, cancel and
/// observe them, and run workers that advance them.
///
/// Cheap to clone; clones share everything (including the local wake-up
/// channel). It holds no state of its own beyond configuration: everything
/// durable lives in the [`Store`](adam_core::Store).
#[derive(Clone)]
pub struct Runtime {
    pub(crate) inner: Arc<Inner>,
}

impl Runtime {
    /// Start configuring a runtime over `store`.
    pub fn builder(store: DynStore) -> RuntimeBuilder {
        RuntimeBuilder {
            store,
            agents: HashMap::new(),
            sink: Arc::new(NoopSink),
            clock: Arc::new(SystemClock),
            cfg: Config {
                worker_id: format!("worker-{}", uuid::Uuid::new_v4()),
                lease_ttl: Duration::from_secs(30),
                poll_interval: Duration::from_millis(250),
                retry: RetryPolicy::default(),
                concurrency: 4,
                lease_renewal: true,
            },
        }
    }

    /// The store this runtime commits to.
    pub fn store(&self) -> &DynStore {
        &self.inner.store
    }

    /// This runtime's lease identity.
    pub fn worker_id(&self) -> &str {
        &self.inner.cfg.worker_id
    }

    /// Names of the registered agents, sorted.
    pub fn agent_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.inner.agents.keys().cloned().collect();
        names.sort();
        names
    }

    fn agent(&self, name: &str) -> Result<&Arc<dyn ErasedAgent>, RuntimeError> {
        self.inner
            .agents
            .get(name)
            .ok_or_else(|| RuntimeError::UnknownAgent(name.to_owned()))
    }

    /// Start a run of `agent` from `input`.
    ///
    /// With a `conversation_id` that already has an open run, `input` is
    /// delivered to that run instead and its id is returned (no second run is
    /// ever created, even when two starts race).
    #[tracing::instrument(skip(self, input))]
    pub async fn start(
        &self,
        agent: &str,
        input: Inbound,
        conversation_id: Option<&str>,
    ) -> Result<RunId, RuntimeError> {
        let erased = self.agent(agent)?;
        for _ in 0..MAX_COMMIT_RETRIES {
            if let Some(conv) = conversation_id
                && let Some(open) = self
                    .inner
                    .store
                    .open_run_for_conversation(agent, conv)
                    .await?
                && let Some(id) = self.deliver_to_open(open.id, &input).await?
            {
                return Ok(id);
            }
            let new = self.new_run(erased.as_ref(), input.clone(), None, conversation_id)?;
            match self.inner.store.create_run(new).await {
                Ok(rec) => {
                    self.started(&rec).await;
                    return Ok(rec.id);
                }
                // Lost a race for the conversation: loop, and deliver instead.
                Err(StoreError::ConversationBusy { .. }) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Err(RuntimeError::Contended(format!(
            "conversation {conversation_id:?} of agent {agent:?}"
        )))
    }

    /// Start a run with a caller-chosen id, for idempotent "fire once"
    /// semantics (one run per cron tick across all replicas).
    ///
    /// Returns `true` if this call created the run and `false` if a run with
    /// that id already existed (then `input` is ignored). Unlike
    /// [`Runtime::start`] it never delivers into another run:
    /// a busy conversation is [`RuntimeError::ConversationBusy`].
    #[tracing::instrument(skip(self, input))]
    pub async fn start_with_id(
        &self,
        run_id: RunId,
        agent: &str,
        input: Inbound,
        conversation_id: Option<&str>,
    ) -> Result<bool, RuntimeError> {
        let erased = self.agent(agent)?;
        let new = self.new_run(erased.as_ref(), input, Some(run_id), conversation_id)?;
        match self.inner.store.create_run(new).await {
            Ok(rec) => {
                self.started(&rec).await;
                Ok(true)
            }
            Err(StoreError::AlreadyExists(_)) => Ok(false),
            Err(StoreError::ConversationBusy {
                agent,
                conversation_id,
            }) => Err(RuntimeError::ConversationBusy {
                agent,
                conversation_id,
            }),
            Err(e) => Err(e.into()),
        }
    }

    fn new_run(
        &self,
        agent: &dyn ErasedAgent,
        input: Inbound,
        id: Option<RunId>,
        conversation_id: Option<&str>,
    ) -> Result<NewRun, RuntimeError> {
        let state = agent.init(input)?;
        let mut new = NewRun::new(agent.name(), Envelope::new(state).encode()?);
        if let Some(id) = id {
            new = new.with_id(id);
        }
        if let Some(conv) = conversation_id {
            new = new.conversation(conv);
        }
        Ok(new)
    }

    async fn started(&self, rec: &RunRecord) {
        self.inner.notify_workers();
        self.inner
            .emit_status(
                rec.id,
                &rec.agent,
                RunStatus::Runnable,
                Some("started".into()),
            )
            .await;
    }

    /// `deliver`, but `None` if the run finished in the meantime.
    async fn deliver_to_open(
        &self,
        run: RunId,
        input: &Inbound,
    ) -> Result<Option<RunId>, RuntimeError> {
        match self.deliver(run, input.clone()).await {
            Ok(()) => Ok(Some(run)),
            Err(RuntimeError::Finished { .. } | RuntimeError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Append to the run's inbox. A parked run becomes runnable at once (also
    /// when it was waiting on a timer); a runnable run keeps its schedule; a
    /// run being stepped right now sees the message in its next transition
    /// without losing the one in flight. Retries CAS conflicts.
    ///
    /// Fails with [`RuntimeError::Finished`] if the run is done or failed.
    #[tracing::instrument(skip(self, inbound), fields(inbound = %inbound.id))]
    pub async fn deliver(&self, run: RunId, inbound: Inbound) -> Result<(), RuntimeError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let rec = self
                .inner
                .store
                .load_run(run)
                .await?
                .ok_or(RuntimeError::NotFound(run))?;
            if rec.status.is_terminal() {
                return Err(RuntimeError::Finished {
                    run,
                    status: rec.status,
                });
            }
            let mut env = Envelope::decode(run, &rec.state)?;
            env.inbox.push(inbound.clone());
            let woke = rec.status == RunStatus::Parked;
            let (status, wake_at) = if woke {
                (RunStatus::Runnable, None)
            } else {
                (rec.status, rec.wake_at)
            };
            let mut update = RunUpdate::new(status, env.encode()?);
            update.wake_at = wake_at;
            match self.inner.store.commit_run(run, rec.version, update).await {
                Ok(_) => {
                    self.inner.notify_workers();
                    if woke {
                        self.inner
                            .emit_status(
                                run,
                                &rec.agent,
                                RunStatus::Runnable,
                                Some("woken by inbound message".into()),
                            )
                            .await;
                    }
                    return Ok(());
                }
                Err(StoreError::Conflict { .. }) => tokio::task::yield_now().await,
                Err(e) => return Err(e.into()),
            }
        }
        Err(RuntimeError::Contended(format!("run {run}")))
    }

    /// Cancel a run: a parked or runnable run becomes `Failed` with
    /// `cancelled: <reason>` (and a status event); a finished run is left
    /// untouched. A run being stepped right now is failed by the same CAS
    /// commit, so the worker's later commit is rejected and it drops its
    /// result.
    ///
    /// A step that is running at that moment is told through its
    /// [`CancelToken`] ([`Ctx::cancelled`](crate::Ctx::cancelled)): at once if
    /// this runtime holds the run, within one poll interval if another
    /// process does. The step is not aborted; it may stop early or run to its
    /// end (its result is dropped either way).
    #[tracing::instrument(skip(self))]
    pub async fn cancel(&self, run: RunId, reason: &str) -> Result<(), RuntimeError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let rec = self
                .inner
                .store
                .load_run(run)
                .await?
                .ok_or(RuntimeError::NotFound(run))?;
            if rec.status.is_terminal() {
                // Finished already (maybe cancelled elsewhere): a step of ours
                // that is still running is pointless.
                self.inner.fire_cancel(run);
                return Ok(());
            }
            let mut env = Envelope::decode(run, &rec.state)?;
            let error = format!("cancelled: {reason}");
            env.error = Some(error.clone());
            env.inbox.clear();
            let update = RunUpdate::new(RunStatus::Failed, env.encode()?);
            match self.inner.store.commit_run(run, rec.version, update).await {
                Ok(_) => {
                    self.inner.fire_cancel(run);
                    self.inner
                        .emit_status(run, &rec.agent, RunStatus::Failed, Some(error))
                        .await;
                    return Ok(());
                }
                Err(StoreError::Conflict { .. }) => tokio::task::yield_now().await,
                Err(e) => return Err(e.into()),
            }
        }
        Err(RuntimeError::Contended(format!("run {run}")))
    }

    /// Snapshot of a run from its durable record (`None` if it does not exist).
    #[tracing::instrument(skip(self))]
    pub async fn view(&self, run: RunId) -> Result<Option<RunView>, RuntimeError> {
        let Some(rec) = self.inner.store.load_run(run).await? else {
            return Ok(None);
        };
        let env = Envelope::decode(run, &rec.state)?;
        Ok(Some(RunView::new(rec, env)))
    }
}
