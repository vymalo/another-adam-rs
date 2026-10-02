//! [`Runtime`]: starting, feeding, cancelling and observing runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use adam_core::{ClaimScope, DynStore, NewRun, RunId, RunRecord, RunStatus, RunUpdate, StoreError};
use adam_error::{BoxError, Classify, ErrorClass};

use crate::agent::{Agent, AgentError, AgentStarter, Inbound};
use crate::cancel::CancelToken;
use crate::child::ChildStatus;
use crate::clock::{Clock, DynClock, SystemClock};
use crate::envelope::Envelope;
use crate::erased::{Erased, ErasedAgent, ErasedStarter, StarterOnly};
use crate::events::{Artifact, DynEventSink, EventSink, NoopSink, RunEvent};
use crate::notify::{DynNotifier, Notifier, Signal};
use crate::retry::RetryPolicy;

/// How often a commit that lost a CAS race is retried before giving up.
pub(crate) const MAX_COMMIT_RETRIES: usize = 16;

/// Why a [`Runtime`] call failed.
///
/// Decide from [`Classify::class`]: `UnknownAgent` and `WrongAgent` are `Invalid`, `NotFound` is `NotFound`,
/// `Finished` and `ConversationBusy` are `Rejected`, `Corrupt` is `Corrupt`, `Contended` is
/// `Conflict`, and `Agent` and `Store` carry the class of the error they wrap.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// No agent with this name was registered on the builder.
    #[error("unknown agent {0:?}")]
    UnknownAgent(String),
    /// The run does not exist.
    #[error("run {0} not found")]
    NotFound(RunId),
    /// A run was named as the one to continue for an agent it does not belong to, so its state is
    /// not that agent's to read ([`Runtime::start_with_id_continuing`]). The text names the run,
    /// which the caller chose to name; `adam-a2a-runtime` never puts it in front of a client.
    #[error("run {run} is not a run of agent {agent:?}")]
    WrongAgent {
        /// The run that was named.
        run: RunId,
        /// The agent it was named for.
        agent: String,
    },
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
        /// The decoder's own error, when there is one.
        #[source]
        source: Option<BoxError>,
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

impl Classify for RuntimeError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::UnknownAgent(_) | Self::WrongAgent { .. } => ErrorClass::Invalid,
            Self::NotFound(_) => ErrorClass::NotFound,
            Self::Finished { .. } | Self::ConversationBusy { .. } => ErrorClass::Rejected,
            Self::Corrupt { .. } => ErrorClass::Corrupt,
            Self::Contended(_) => ErrorClass::Conflict,
            Self::Agent(e) => e.class(),
            Self::Store(e) => e.class(),
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Agent(e) => e.retry_after(),
            Self::Store(e) => e.retry_after(),
            _ => None,
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
    pub claim_scope: ClaimScope,
    pub lease_ttl: Duration,
    pub poll_interval: Duration,
    pub retry: RetryPolicy,
    pub concurrency: usize,
    pub lease_renewal: bool,
}

/// One entry of the registry: a full agent, or a start-only starter. Closed on
/// purpose: the runtime matches on it to decide what a name can do.
#[derive(Clone)]
pub(crate) enum Registered {
    Agent(Arc<dyn ErasedAgent>),
    Starter(Arc<dyn ErasedStarter>),
}

impl Registered {
    /// Every registration can start a run.
    pub(crate) fn starter(&self) -> &dyn ErasedStarter {
        match self {
            Self::Agent(a) => a.as_ref(),
            Self::Starter(s) => s.as_ref(),
        }
    }

    /// Only a full agent can step one.
    pub(crate) fn agent(&self) -> Option<&Arc<dyn ErasedAgent>> {
        match self {
            Self::Agent(a) => Some(a),
            Self::Starter(_) => None,
        }
    }
}

pub(crate) struct Inner {
    pub store: DynStore,
    pub agents: HashMap<String, Registered>,
    pub sink: DynEventSink,
    pub clock: DynClock,
    pub cfg: Config,
    /// Bumped whenever local work appears, so local workers do not wait for
    /// the next poll.
    pub wake: watch::Sender<u64>,
    /// Tells other processes about work and cancels, and them about ours.
    /// `None`: only polling crosses a process boundary.
    pub notifier: Option<DynNotifier>,
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

    /// A run of `agent` became runnable now: wake local workers and tell
    /// other processes.
    pub async fn announce_runnable(&self, run: RunId, agent: &str) {
        self.notify_workers();
        if let Some(notifier) = &self.notifier {
            notifier
                .publish(Signal::Runnable {
                    run,
                    agent: agent.to_owned(),
                })
                .await;
        }
    }

    /// `run` was finished by a cancel: tell other processes stepping it.
    pub async fn announce_finished(&self, run: RunId) {
        if let Some(notifier) = &self.notifier {
            notifier.publish(Signal::Finished { run }).await;
        }
    }

    /// Append `inbound` to the inbox of `run`; see [`Runtime::deliver`].
    pub async fn deliver(&self, run: RunId, inbound: Inbound) -> Result<(), RuntimeError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let rec = self
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
            match self.store.commit_run(run, rec.version, update).await {
                Ok(_) => {
                    self.announce_runnable(run, &rec.agent).await;
                    if woke {
                        self.emit_status(
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

    /// `child` just reached a terminal state: if it has a parent, tell it
    /// ([`RUN_FINISHED_KIND`](crate::RUN_FINISHED_KIND)).
    ///
    /// Best effort by design, and never an error for the child: this runs after the child's own
    /// commit, so a crash or a store error here loses the message and nothing else. The parent
    /// keeps a timer and reads the child when it fires (`Ctx::child_status`), which is what makes
    /// the whole exchange at-least-once. A parent that is finished or gone has nobody to tell.
    pub async fn notify_parent(&self, child: &RunRecord) {
        let Some(parent) = child.parent_id else {
            return;
        };
        if child.status.is_open() {
            return;
        }
        let notice = ChildStatus::from_record(child).notice(child.id);
        match self.deliver(parent, notice).await {
            Ok(()) => {}
            Err(RuntimeError::Finished { .. } | RuntimeError::NotFound(_)) => {
                tracing::debug!(child = %child.id, %parent, "the parent is finished or gone; nobody to tell");
            }
            Err(e) => {
                tracing::warn!(child = %child.id, %parent, error = %adam_error::report(&e), "telling the parent failed; it learns of the result when its timer fires");
            }
        }
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
    agents: HashMap<String, Registered>,
    sink: DynEventSink,
    clock: DynClock,
    notifier: Option<DynNotifier>,
    cfg: Config,
}

impl RuntimeBuilder {
    /// Register an agent: it can start runs and step them. Registering a
    /// second agent or starter with the same name replaces the first.
    pub fn agent<A: Agent>(self, agent: A) -> Self {
        let erased = Erased(agent);
        let name = ErasedStarter::name(&erased).to_owned();
        self.register(name, Registered::Agent(Arc::new(erased)))
    }

    /// Register a start-only agent: this runtime can start runs of its name
    /// but never steps them, so [`Runtime::run_worker`] does not claim them.
    /// A worker with the full [`Agent`] of the same name steps them.
    /// Registering a second starter or agent with the same name replaces the
    /// first.
    ///
    /// **The starter's `State` must be the [`Agent::State`] of the agent that
    /// steps the run**, and `init` must produce what that agent's `init` would.
    /// Nothing can check this across processes: a mismatch starts the run
    /// successfully, then fails it as permanent on the worker's first step,
    /// with an error naming the agent whose state did not decode.
    pub fn starter<S: AgentStarter>(self, starter: S) -> Self {
        let name = starter.name().to_owned();
        self.register(name, Registered::Starter(Arc::new(StarterOnly(starter))))
    }

    fn register(mut self, name: String, entry: Registered) -> Self {
        if self.agents.insert(name.clone(), entry).is_some() {
            tracing::warn!(agent = %name, "agent registered twice, keeping the last");
        }
        self
    }

    /// Where progress events go. Default: [`NoopSink`].
    pub fn event_sink(mut self, sink: impl EventSink) -> Self {
        self.sink = Arc::new(sink);
        self
    }

    /// Where signals about new work and cancels go, and come from, so that
    /// workers of other processes react at once instead of at their next
    /// poll. Default: none, and only polling crosses a process boundary.
    ///
    /// Signals are hints: polling stays on with a notifier configured, and
    /// correctness never depends on one arriving. See [`Notifier`].
    pub fn notifier(mut self, notifier: impl Notifier) -> Self {
        self.notifier = Some(Arc::new(notifier));
        self
    }

    /// Identity written into leases. Must be unique per worker process (or
    /// per `Runtime` when several share a store). Default: random.
    ///
    /// With [`claim_scope(ClaimScope::Pinned)`](Self::claim_scope) it is also the
    /// run *owner*, so it must be **stable across restarts** (a StatefulSet pod
    /// name, not a random id), or the runs it owns are never claimed again.
    pub fn worker_id(mut self, id: impl Into<String>) -> Self {
        self.cfg.worker_id = id.into();
        self
    }

    /// Whose runs [`Runtime::run_worker`] may claim. Default: [`ClaimScope::Any`],
    /// where any worker takes any due run.
    ///
    /// With [`ClaimScope::Pinned`] a run stays on the worker that first claimed
    /// it: no other worker steps it, not even after the owner's lease expired.
    /// Use it when a run's files live on one worker (workspace placements
    /// `affinity` and `isolated`). A run whose owner never returns is stranded;
    /// see [ADR 0002](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0002-workspace-placement.md).
    /// Set a stable [`worker_id`](Self::worker_id) with it.
    pub fn claim_scope(mut self, scope: ClaimScope) -> Self {
        self.cfg.claim_scope = scope;
        self
    }

    /// How long a claimed run stays leased without renewal. Default 30 s.
    pub fn lease_ttl(mut self, ttl: Duration) -> Self {
        self.cfg.lease_ttl = ttl;
        self
    }

    /// How often an idle worker polls for due runs. Default 250 ms.
    /// `deliver`/`start` on the same `Runtime` wake local workers at once, and
    /// with a [`notifier`](Self::notifier) so do those of other processes.
    /// Timers and retry backoffs are found by polling only.
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
                notifier: self.notifier,
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
/// channel and the notifier). It holds no state of its own beyond configuration: everything
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
            notifier: None,
            cfg: Config {
                worker_id: format!("worker-{}", uuid::Uuid::new_v4()),
                claim_scope: ClaimScope::Any,
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

    /// Whose runs this runtime's worker claims.
    pub fn claim_scope(&self) -> ClaimScope {
        self.inner.cfg.claim_scope
    }

    /// Names of every registration, agents and starters alike, sorted.
    pub fn agent_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.inner.agents.keys().cloned().collect();
        names.sort();
        names
    }

    fn registered(&self, name: &str) -> Result<&Registered, RuntimeError> {
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
        self.start_in(agent, input, conversation_id, None).await
    }

    /// [`start`](Self::start), for a run that **continues** `prior`: where `start` would create a
    /// run, this creates one whose first state comes from [`Agent::init_continuing`] (or
    /// [`AgentStarter::init_continuing`]) given the prior run's last committed state, instead of
    /// from `init`. When the conversation has an open run, `input` is delivered to it as `start`
    /// does and `prior` is not read.
    ///
    /// See [`start_with_id_continuing`](Self::start_with_id_continuing) for what `prior` may be
    /// and what the runtime checks.
    #[tracing::instrument(skip(self, input))]
    pub async fn start_continuing(
        &self,
        agent: &str,
        input: Inbound,
        conversation_id: Option<&str>,
        prior: RunId,
    ) -> Result<RunId, RuntimeError> {
        self.start_in(agent, input, conversation_id, Some(prior))
            .await
    }

    async fn start_in(
        &self,
        agent: &str,
        input: Inbound,
        conversation_id: Option<&str>,
        prior: Option<RunId>,
    ) -> Result<RunId, RuntimeError> {
        let registered = self.registered(agent)?;
        // Read once, and only if a run is about to be created.
        let mut prior_state: Option<Value> = None;
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
            if let (Some(run), None) = (prior, &prior_state) {
                prior_state = Some(self.prior_state(agent, run).await?);
            }
            let new = self.new_run(
                registered.starter(),
                input.clone(),
                prior.zip(prior_state.as_ref()),
                None,
                conversation_id,
            )?;
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
        let registered = self.registered(agent)?;
        let new = self.new_run(
            registered.starter(),
            input,
            None,
            Some(run_id),
            conversation_id,
        )?;
        self.create_with_id(new).await
    }

    /// [`start_with_id`](Self::start_with_id) for a run that **continues** `prior`: the new run's
    /// first state is what [`Agent::init_continuing`] (or [`AgentStarter::init_continuing`], for a
    /// start-only registration, which is all an A2A front has) makes of the prior run's last
    /// committed state and `input`, instead of what `init` makes of `input` alone. It is what lets
    /// a new task remember the one before it.
    ///
    /// The new run is an ordinary run: its own id, journal, limits and conversation. Nothing links
    /// the two beyond the state the agent chose to carry. Idempotent like `start_with_id`:
    /// `true` if this call created the run, `false` if a run with `run_id` already existed (then
    /// `input` is ignored, and `prior` is neither read nor checked: it may be gone, or wrong).
    ///
    /// # What the runtime checks, and what it leaves to the caller
    ///
    /// * `prior` must exist ([`RuntimeError::NotFound`]) and be a run of `agent`, so that its state
    ///   is this agent's and not another's ([`RuntimeError::WrongAgent`], class `Invalid`). Its status does not
    ///   matter: the state is whatever its last commit holds, and an agent that continues from an
    ///   unfinished run must cope with a half-done turn (`LlmAgent` drops a tool call that never
    ///   got its result).
    /// * Whether the caller may continue `prior` (same owner, same conversation) is **not**
    ///   checked: the runtime has no owners. `adam-a2a-runtime` checks all of it before calling.
    /// * A `prior` state that does not decode as the agent's state is not an error: the run starts
    ///   as `start_with_id` would, and a warning says so.
    ///
    /// It works on a runtime that only registered the agent's starter, because the prior state is
    /// read from the store and decoded as the starter's `State`.
    #[tracing::instrument(skip(self, input))]
    pub async fn start_with_id_continuing(
        &self,
        run_id: RunId,
        agent: &str,
        input: Inbound,
        conversation_id: Option<&str>,
        prior: RunId,
    ) -> Result<bool, RuntimeError> {
        let registered = self.registered(agent)?;
        // A repeat of a request whose first attempt already started the run answers as
        // `start_with_id` does, before anything is read or initialised for it: the prior may be
        // purged by now, and `init_continuing` may be costly. (A race with a first attempt that
        // has not committed yet is settled by `create_run` below.)
        if self.inner.store.load_run(run_id).await?.is_some() {
            return Ok(false);
        }
        let state = self.prior_state(agent, prior).await?;
        let new = self.new_run(
            registered.starter(),
            input,
            Some((prior, &state)),
            Some(run_id),
            conversation_id,
        )?;
        self.create_with_id(new).await
    }

    /// Create a run under its own id: `true` if created, `false` if it already existed.
    async fn create_with_id(&self, new: NewRun) -> Result<bool, RuntimeError> {
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

    /// Start a run of `agent` as a child of `parent`, under a caller-chosen id.
    ///
    /// The child records `parent` (`RunRecord::parent_id`), and when it reaches a terminal state the
    /// runtime delivers a [`RUN_FINISHED_KIND`](crate::RUN_FINISHED_KIND) message to the parent
    /// (payload: [`ChildStatus`], message id: the child's run id). The message is a hint sent after
    /// the child's commit, so it can be lost; the parent reads the child with
    /// [`Ctx::child_status`](crate::Ctx::child_status) on a timer to be sure.
    ///
    /// Idempotent like [`start_with_id`](Self::start_with_id): returns `true` if this call created
    /// the child and `false` if a run with that id already existed (then `input` is ignored). Derive
    /// the id from the parent and something stable, such as a tool call id, with
    /// [`child_run_id`](crate::child_run_id), and a step that runs again finds the child it started
    /// instead of starting another.
    ///
    /// The child is an ordinary run: it has no conversation, and cancelling the parent does not
    /// cancel it (it finishes within its own limits and its message to the finished parent is
    /// dropped). `parent` is not checked, so a child started for a parent that does not exist runs
    /// and its message finds nobody.
    #[tracing::instrument(skip(self, input))]
    pub async fn start_child(
        &self,
        parent: RunId,
        id: RunId,
        agent: &str,
        input: Inbound,
    ) -> Result<bool, RuntimeError> {
        let registered = self.registered(agent)?;
        let new = self
            .new_run(registered.starter(), input, None, Some(id), None)?
            .parent(parent);
        match self.inner.store.create_run(new).await {
            Ok(rec) => {
                self.started(&rec).await;
                Ok(true)
            }
            Err(StoreError::AlreadyExists(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The record of a new run. With `prior` (the run continued and its stored agent state) the
    /// first state comes from `init_continuing`, otherwise from `init`.
    fn new_run(
        &self,
        agent: &dyn ErasedStarter,
        input: Inbound,
        prior: Option<(RunId, &Value)>,
        id: Option<RunId>,
        conversation_id: Option<&str>,
    ) -> Result<NewRun, RuntimeError> {
        let state = match prior {
            Some((run, state)) => agent.init_continuing(input, state, run)?,
            None => agent.init(input)?,
        };
        let mut new = NewRun::new(agent.name(), Envelope::new(state).encode()?);
        if let Some(id) = id {
            new = new.with_id(id);
        }
        if let Some(conv) = conversation_id {
            new = new.conversation(conv);
        }
        Ok(new)
    }

    /// The agent state that `prior`'s last commit holds, for a run of `agent` that continues it.
    async fn prior_state(&self, agent: &str, prior: RunId) -> Result<Value, RuntimeError> {
        let rec = self
            .inner
            .store
            .load_run(prior)
            .await?
            .ok_or(RuntimeError::NotFound(prior))?;
        if rec.agent != agent {
            // The caller named a run of another agent, whose state is not this agent's to read.
            return Err(RuntimeError::WrongAgent {
                run: prior,
                agent: agent.to_owned(),
            });
        }
        Ok(Envelope::decode(prior, &rec.state)?.agent)
    }

    async fn started(&self, rec: &RunRecord) {
        self.inner.announce_runnable(rec.id, &rec.agent).await;
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
        self.inner.deliver(run, inbound).await
    }

    /// Cancel a run: a parked or runnable run becomes `Failed` with
    /// `cancelled: <reason>` (and a status event); a finished run is left
    /// untouched. A run being stepped right now is failed by the same CAS
    /// commit, so the worker's later commit is rejected and it drops its
    /// result.
    ///
    /// A step that is running at that moment is told through its
    /// [`CancelToken`] ([`Ctx::cancelled`](crate::Ctx::cancelled)): at once if
    /// this runtime holds the run, and if another process does, at once too
    /// when both runtimes share a [`Notifier`] (a
    /// [`Signal::Finished`]), otherwise within one poll interval. The step is
    /// not aborted by the runtime: a step that listens to the token stops
    /// early (`LlmAgent` drops its model request, the coder kills its
    /// commands), one that does not runs to its end (its result is dropped
    /// either way).
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
                Ok(committed) => {
                    self.inner.fire_cancel(run);
                    self.inner.announce_finished(run).await;
                    self.inner
                        .emit_status(run, &rec.agent, RunStatus::Failed, Some(error))
                        .await;
                    self.inner.notify_parent(&committed).await;
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

#[cfg(test)]
mod error_tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("lower")]
    struct Lower;

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &RuntimeError) -> ErrorClass {
        match e {
            RuntimeError::UnknownAgent(_) => ErrorClass::Invalid,
            RuntimeError::WrongAgent { .. } => ErrorClass::Invalid,
            RuntimeError::NotFound(_) => ErrorClass::NotFound,
            RuntimeError::Finished { .. } => ErrorClass::Rejected,
            RuntimeError::ConversationBusy { .. } => ErrorClass::Rejected,
            RuntimeError::Corrupt { .. } => ErrorClass::Corrupt,
            RuntimeError::Contended(_) => ErrorClass::Conflict,
            RuntimeError::Agent(e) => e.class(),
            RuntimeError::Store(e) => e.class(),
        }
    }

    #[test]
    fn class_table() {
        let run = RunId::new();
        let samples = [
            RuntimeError::UnknownAgent("x".into()),
            RuntimeError::WrongAgent {
                run,
                agent: "x".into(),
            },
            RuntimeError::NotFound(run),
            RuntimeError::Finished {
                run,
                status: RunStatus::Done,
            },
            RuntimeError::ConversationBusy {
                agent: "a".into(),
                conversation_id: "c".into(),
            },
            RuntimeError::Corrupt {
                run,
                reason: "x".into(),
                source: None,
            },
            RuntimeError::Contended("run x".into()),
            RuntimeError::Agent(AgentError::permanent("x")),
            RuntimeError::Agent(AgentError::transient_after("x", Duration::from_secs(3))),
            RuntimeError::Store(StoreError::unavailable(Lower)),
            RuntimeError::Store(StoreError::Conflict {
                run,
                expected: 1,
                actual: 2,
            }),
            RuntimeError::Store(StoreError::NotFound(run)),
        ];
        for e in &samples {
            assert_eq!(e.class(), expected(e), "{e}");
        }
        // One opinion on store errors, shared with AgentError: only an unavailable backend, a
        // lost race, contention and a transient agent failure retry.
        let retryable: Vec<bool> = samples.iter().map(Classify::is_retryable).collect();
        assert_eq!(
            retryable,
            [
                false, false, false, false, false, false, true, false, true, true, true, false
            ]
        );
        assert_eq!(samples[8].retry_after(), Some(Duration::from_secs(3)));
    }

    #[test]
    fn corrupt_keeps_the_decoder_error_as_its_source() {
        let e = Envelope::decode(
            RunId::new(),
            &serde_json::json!({"v": 1, "agent": null, "seq": "not a number"}),
        )
        .expect_err("garbage does not decode");
        assert!(
            matches!(
                e,
                RuntimeError::Corrupt {
                    source: Some(_),
                    ..
                }
            ),
            "{e:?}"
        );
        assert!(std::error::Error::source(&e).is_some_and(|s| s.is::<serde_json::Error>()));
    }
}
