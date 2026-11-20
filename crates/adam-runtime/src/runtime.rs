//! [`Runtime`]: starting, feeding, cancelling and observing runs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use adam_core::{
    ClaimScope, ConversationScope, DynStore, NewRun, RunHeader, RunId, RunQuery, RunRecord,
    RunStatus, RunUpdate, StoreError,
};
use adam_error::{BoxError, Classify, ErrorClass};

use crate::agent::{Agent, AgentError, AgentStarter, Inbound};
use crate::artifact_ref::{ArtifactRef, FileRef, FileSource};
use crate::blob::BlobCache;
use crate::cancel::CancelToken;
use crate::child::{CHILD_REPORTED_KIND, ChildStatus, RUN_FINISHED_KIND};
use crate::clock::{Clock, DynClock, SystemClock};
use crate::envelope::Envelope;
use crate::erased::{Erased, ErasedAgent, ErasedStarter, StarterOnly};
use crate::events::{DynEventSink, EventSink, NoopSink, RunEvent};
use crate::head::Stored;
use crate::notify::{DynNotifier, Notifier, Signal};
use crate::retry::RetryPolicy;
use crate::session::{
    MAX_SESSION_CONVERSATION_BYTES, MAX_SESSION_ROUNDS_SCANNED, SessionList, SessionView,
    page_of_sessions, parse_session_conversation_id, session_conversation_prefix, session_of,
};

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
    /// A session round was refused because what it names does not fit the session (ADR 0045): the
    /// conversation id is not a session's, an existing run is not this round, or the run to
    /// continue is not a finished round of this session. `reason` says what did not match and
    /// holds no run content.
    #[error("not a valid session round: {reason}")]
    InvalidSession {
        /// What did not match.
        reason: String,
    },
    /// A parent's sessions have more rounds than one listing reads.
    #[error("the sessions have {runs} rounds, more than the {max} one listing reads")]
    TooManyRounds {
        /// How many rounds the store counts.
        runs: u64,
        /// The most a listing reads.
        max: usize,
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

impl RuntimeError {
    /// Whether this is [`ConversationBusy`](Self::ConversationBusy): the conversation has an open
    /// run. Its class, `Rejected`, is shared with [`Finished`](Self::Finished) and others, so a
    /// caller that has to tell "steer the open round" from "that run is over" asks here. It is the
    /// one place a caller decides from the variant, because the two want different actions.
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::ConversationBusy { .. })
    }
}

impl Classify for RuntimeError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::UnknownAgent(_) | Self::WrongAgent { .. } | Self::InvalidSession { .. } => {
                ErrorClass::Invalid
            }
            Self::NotFound(_) => ErrorClass::NotFound,
            Self::Finished { .. } | Self::ConversationBusy { .. } | Self::TooManyRounds { .. } => {
                ErrorClass::Rejected
            }
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
    /// A worker holds an unexpired lease on the run: it is being stepped right now, though
    /// nothing has been committed yet. Only ever `true` for a runnable run (a parked, finished or
    /// failed run is not being stepped, whatever lease it still carries). The lease is the store's
    /// scheduling data, read beside the record in [`Runtime::view`], so a reader in another process
    /// than the worker sees it too. A lease that ran out (the worker died) reads `false` until
    /// another worker claims the run.
    ///
    /// This is what lets an A2A task that has not had its first commit report `working` instead
    /// of `submitted`: a whole turn commits once, and its first model call can take a long time.
    #[serde(default)]
    pub claimed: bool,
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
    /// Artifacts emitted by committed transitions, in order. A file artifact is a reference
    /// ([`ArtifactRef::file`]): its bytes are fetched with
    /// [`Runtime::artifact_bytes`](Runtime::artifact_bytes), so a poll of a run never transfers a
    /// file it does not serve.
    pub artifacts: Vec<ArtifactRef>,
    /// The agent's **head**: its state without the transcript for a v5 run, and the whole agent
    /// state of a v4 run, which has no head. The pointers a front reads — `pending_wait`,
    /// `pending_question`, `question`, `usage_totals`, `sandbox_usage`, `context`, `session` —
    /// are in the head.
    ///
    /// Use [`Runtime::state`](Runtime::state) for the joined state (the head and the body
    /// generation it names); this member is not it.
    pub state: Value,
    /// Store version of the record (bumps on every commit).
    pub version: u64,
    /// When the run was created.
    pub created_at: DateTime<Utc>,
    /// Last commit time.
    pub updated_at: DateTime<Utc>,
}

impl RunView {
    fn new(rec: RunRecord, stored: Stored, claimed: bool) -> Self {
        let parts = stored.into_parts();
        // The wake the runtime set for a watched run (to look at its parent, ADR 0044 slice 10) is
        // not the agent's: a view says when the agent asked to be woken, so a run waiting for a
        // person or a message still reads as waiting.
        let wake_at = if parts.hb && rec.status == RunStatus::Parked {
            parts.agent_wake
        } else {
            rec.wake_at
        };
        Self {
            id: rec.id,
            agent: rec.agent,
            conversation_id: rec.conversation_id,
            status: rec.status,
            wake_at,
            waiting: rec.status == RunStatus::Parked && wake_at.is_none(),
            claimed,
            output: (rec.status == RunStatus::Done).then_some(parts.output),
            error: parts.error,
            attempt: parts.attempt,
            pending_inbox: parts.inbox.len(),
            artifacts: parts.artifacts,
            state: parts.agent,
            version: rec.version,
            created_at: rec.created_at,
            updated_at: rec.updated_at,
        }
    }
}

/// A run's view, built from a record the caller already holds: the same value
/// [`Runtime::view`] returns, without a second read of the run.
///
/// `claimed` is the lease, which a record does not carry: a caller that lists records reads it
/// beside the one record it needs it for (`adam-a2a-runtime` reads it only for a runnable run at
/// version 1, the only record whose task state can still flip from `submitted` to `working`) and
/// compares it against [`Runtime::now`], so the result agrees with [`Runtime::view`]. An
/// unreadable state is [`RuntimeError::Corrupt`].
///
/// A record from [`Store::list_runs`](adam_core::Store::list_runs) carries the whole v4 state in
/// the expand release, so its view is the v4 one; a record read with
/// [`Store::load_head`](adam_core::Store::load_head) may carry a v5 head.
///
/// # Errors
///
/// [`RuntimeError::Corrupt`] when `rec.state` is neither a valid envelope nor a valid head.
pub fn view_from_record(rec: RunRecord, claimed: bool) -> Result<RunView, RuntimeError> {
    let stored = Stored::decode(rec.id, &rec.state)?;
    Ok(RunView::new(rec, stored, claimed))
}

pub(crate) struct Config {
    pub worker_id: String,
    pub claim_scope: ClaimScope,
    pub lease_ttl: Duration,
    /// How often an idle worker polls the store for due runs.
    pub poll_interval: Duration,
    /// How often a step's cancel watch re-reads the run it steps.
    pub cancel_poll_interval: Duration,
    pub retry: RetryPolicy,
    pub concurrency: usize,
    pub lease_renewal: bool,
    pub session_idle_ttl: Duration,
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
    /// The file bytes a view's reader has fetched, bounded and shared by every clone: a poll of a
    /// run whose file was already served does not read the blob again.
    pub blobs: Mutex<BlobCache>,
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

    /// The run's record and its joined agent state; see [`Runtime::state`].
    async fn joined_state(&self, run: RunId) -> Result<Option<(RunRecord, Value)>, RuntimeError> {
        let Some(first) = self.store.load_head(run).await? else {
            return Ok(None);
        };
        let stored = Stored::decode(run, &first.state)?;
        if let Some(state) = self.join_stored(&first, &stored).await? {
            return Ok(Some((first, state)));
        }
        // The rows do not match the head: either the generation was collected between the two
        // reads or the head moved. Re-read the head once; a version that changed names a new set.
        let Some(second) = self.store.load_head(run).await? else {
            return Ok(None);
        };
        if second.version == first.version {
            return Err(RuntimeError::Corrupt {
                run,
                reason: format!(
                    "the head names {} body rows and the generation does not hold them",
                    stored.body().map_or(0, |body| body.len)
                ),
                source: None,
            });
        }
        let stored = Stored::decode(run, &second.state)?;
        match self.join_stored(&second, &stored).await? {
            Some(state) => Ok(Some((second, state))),
            None => Err(RuntimeError::Corrupt {
                run,
                reason: "the head moved and its body generation is still incomplete".into(),
                source: None,
            }),
        }
    }

    /// The joined state of one record, or `None` when the body set does not match the head.
    async fn join_stored(
        &self,
        rec: &RunRecord,
        stored: &Stored,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(body) = stored.body() else {
            return Ok(Some(stored.agent().clone()));
        };
        let Some(registered) = self.agents.get(&rec.agent) else {
            return Err(RuntimeError::UnknownAgent(rec.agent.clone()));
        };
        let Some(generation) = body.generation else {
            return Err(RuntimeError::Corrupt {
                run: rec.id,
                reason: "the head names no body generation".into(),
                source: None,
            });
        };
        let rows = self.store.body_list(rec.id, generation).await?;
        if rows.len() != body.len as usize
            || rows
                .iter()
                .enumerate()
                .any(|(at, row)| row.idx as usize != at)
        {
            return Ok(None);
        }
        let rows: Vec<Value> = rows.into_iter().map(|row| row.message).collect();
        registered
            .starter()
            .join(stored.agent().clone(), rows)
            .map(Some)
            .map_err(RuntimeError::Agent)
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

    /// Fail `run` with `cancelled: <reason>`; see [`Runtime::cancel`]. A run that is terminal is
    /// left alone, so asking twice is a no-op (and asking for a run that has finished fires the token
    /// of a step of ours that is still running it). Also how a child run follows a parent that
    /// ended (`worker.rs`).
    pub async fn cancel_run(&self, run: RunId, reason: &str) -> Result<(), RuntimeError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let rec = self
                .store
                .load_run(run)
                .await?
                .ok_or(RuntimeError::NotFound(run))?;
            if rec.status.is_terminal() {
                // Finished already (maybe cancelled elsewhere): a step of ours
                // that is still running is pointless.
                self.fire_cancel(run);
                return Ok(());
            }
            let mut stored = Stored::decode(run, &rec.state)?;
            let error = format!("cancelled: {reason}");
            *stored.error_mut() = Some(error.clone());
            // A request to close the session survives: a cancel must not reopen it.
            stored
                .inbox_mut()
                .retain(|i| i.kind == crate::SESSION_CLOSE_KIND);
            // The hook receives the whole agent state. A v4 run carries it; a v5 head is joined
            // with its body here, once, so the hook never sees a head with no transcript. A set
            // that does not join is skipped: the cancel still lands, and the hook is best effort.
            // A process that registered no full agent has no hook to tell, so it reads no body.
            let hook_state = if self
                .agents
                .get(&rec.agent)
                .and_then(Registered::agent)
                .is_some()
            {
                match self.join_stored(&rec, &stored).await {
                    Ok(state) => state,
                    Err(e) => {
                        tracing::warn!(%run, error = %adam_error::report(&e), "the cancelled run's state could not be joined; its agent is not told");
                        None
                    }
                }
            } else {
                None
            };
            let update = RunUpdate::new(RunStatus::Failed, stored.encode()?);
            match self.store.commit_run(run, rec.version, update).await {
                Ok(committed) => {
                    // First, and on a task of its own: firing the token below lets the step end,
                    // and the worker then drops the cancel watch that may be running this very
                    // call, so anything awaited after it can be cut short.
                    if let Some(state) = hook_state {
                        self.run_cancel_hook(run, &rec.agent, state);
                    }
                    self.fire_cancel(run);
                    self.announce_finished(run).await;
                    self.emit_status(run, &rec.agent, RunStatus::Failed, Some(error.clone()))
                        .await;
                    // A failed run reports its error; its output is not part of the notice.
                    self.notify_parent(&committed, &Value::Null, Some(&error))
                        .await;
                    return Ok(());
                }
                Err(StoreError::Conflict { .. }) => tokio::task::yield_now().await,
                Err(e) => return Err(e.into()),
            }
        }
        Err(RuntimeError::Contended(format!("run {run}")))
    }

    /// Tell the agent of a run this process just cancelled, on a task of its own: best effort, and
    /// only where the full agent is registered.
    fn run_cancel_hook(&self, run: RunId, agent: &str, state: serde_json::Value) {
        if let Some(agent) = self.agents.get(agent).and_then(Registered::agent) {
            let agent = agent.clone();
            tokio::spawn(async move { agent.on_cancelled(run, state).await });
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
    /// ([`RUN_FINISHED_KIND`](crate::RUN_FINISHED_KIND), or
    /// [`CHILD_REPORTED_KIND`](crate::CHILD_REPORTED_KIND) when the child has a conversation).
    ///
    /// `output` and `error` are the values the caller just committed. The store returns the record
    /// without its state ([`Store::commit_run`](adam_core::Store::commit_run)), so the caller
    /// passes what it wrote instead of decoding what it sent; `child` supplies the ids, the status
    /// and the parent.
    ///
    /// Best effort by design, and never an error for the child: this runs after the child's own
    /// commit, so a crash or a store error here loses the message and nothing else. The parent
    /// keeps a timer and reads the child when it fires (`Ctx::child_status`), which is what makes
    /// the whole exchange at-least-once. A parent that is finished or gone has nobody to tell.
    pub async fn notify_parent(&self, child: &RunRecord, output: &Value, error: Option<&str>) {
        let Some(parent) = child.parent_id else {
            return;
        };
        if child.status.is_open() {
            return;
        }
        // A child with a conversation is a session round (ADR 0045); a one-shot child has none.
        let kind = if child.conversation_id.is_some() {
            CHILD_REPORTED_KIND
        } else {
            RUN_FINISHED_KIND
        };
        let notice = ChildStatus::from_parts(child.status, output, error).notice(child.id, kind);
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
    ///
    /// This is the worker's idle poll, **not** how fast a step learns of a cancel: that is
    /// [`cancel_poll_interval`](Self::cancel_poll_interval).
    pub fn poll_interval(mut self, every: Duration) -> Self {
        self.cfg.poll_interval = every;
        self
    }

    /// How often a step's cancel watch re-reads the run it is stepping. Default 1 s.
    ///
    /// The watch is how a step learns that its run turned terminal in another process (a cancel, a
    /// time limit, a parent that ended) when no [`notifier`](Self::notifier) told it, and it is the
    /// fallback for the signal: a `Signal::Finished` fires the step's token at once. It reads the
    /// run's **header** ([`Store::run_header`](adam_core::Store::run_header)): an adapter that
    /// overrides that method projects it and never reads the state; the trait's default (what
    /// `MemoryStore` uses) reads the lease and then the record. The bound it implies: a state
    /// change that no event announced reaches a step's cancel watch within this interval.
    ///
    /// Slower than [`poll_interval`](Self::poll_interval) on purpose: the worker's idle poll finds
    /// work that appeared, which a person waits for, while the cancel watch waits on something a
    /// person asked for and a signal normally delivers.
    pub fn cancel_poll_interval(mut self, every: Duration) -> Self {
        self.cfg.cancel_poll_interval = every;
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

    /// How long a kept-open session may sit idle before it reads closed (ADR 0045 slice 4): its
    /// latest round ended at least this long ago, and nobody has written to it since. Default
    /// [`DEFAULT_SESSION_IDLE_TTL`](crate::DEFAULT_SESSION_IDLE_TTL), seven days.
    ///
    /// It decides what a listing and a session read *say*; nothing is written, so a longer TTL
    /// reopens what a shorter one closed. Set it the same on every process that serves the same
    /// store. Purge finished runs only after more than this (see `Store::purge_finished`): a
    /// purged latest round makes the session read wrongly.
    pub fn session_idle_ttl(mut self, ttl: Duration) -> Self {
        self.cfg.session_idle_ttl = ttl;
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
                blobs: Mutex::default(),
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
                cancel_poll_interval: Duration::from_secs(1),
                retry: RetryPolicy::default(),
                concurrency: 4,
                lease_renewal: true,
                session_idle_ttl: crate::DEFAULT_SESSION_IDLE_TTL,
            },
        }
    }

    /// The store this runtime commits to.
    pub fn store(&self) -> &DynStore {
        &self.inner.store
    }

    /// The runtime's current time, from its clock ([`RuntimeBuilder::clock`]).
    ///
    /// A caller that builds a view from a record it holds ([`view_from_record`]) reads the run's
    /// lease against this, so the `claimed` it computes agrees with [`view`](Self::view)'s.
    pub fn now(&self) -> DateTime<Utc> {
        self.inner.clock.now()
    }

    /// This runtime's lease identity.
    pub fn worker_id(&self) -> &str {
        &self.inner.cfg.worker_id
    }

    /// How long a kept-open session may sit idle before it reads closed
    /// ([`RuntimeBuilder::session_idle_ttl`]).
    pub fn session_idle_ttl(&self) -> Duration {
        self.inner.cfg.session_idle_ttl
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
    /// The child is an ordinary run: it has no conversation. **It does not outlive a parent that
    /// failed**: when `parent` is `Failed` for any reason (a [`cancel`](Self::cancel) included), the
    /// worker that holds the child fails it with `cancelled: the parent run ended` before it steps and
    /// on the poll of its cancel watch, and a parked child is looked at once a minute. `parent` is not
    /// checked when the child is started, and a parent that is not in the store is unknown, not ended:
    /// it cancels nothing.
    pub async fn start_child(
        &self,
        parent: RunId,
        id: RunId,
        agent: &str,
        input: Inbound,
    ) -> Result<bool, RuntimeError> {
        self.start_child_within(parent, id, agent, input, None)
            .await
    }

    /// [`start_child`](Self::start_child) with a time limit: the child must finish within `within`
    /// of now, by the runtime's own clock (`None`: no limit, which is `start_child`). The deadline is
    /// stored with the run. Past it the worker fails the child with `cancelled: time limit: the run
    /// did not finish within N min`, before a step and, for a step in flight, within one
    /// cancel poll interval (the step's token fires), and a child that is parked is woken at it.
    /// The parent gets the failure as it gets any: the child's error.
    #[tracing::instrument(skip(self, input))]
    pub async fn start_child_within(
        &self,
        parent: RunId,
        id: RunId,
        agent: &str,
        input: Inbound,
        within: Option<Duration>,
    ) -> Result<bool, RuntimeError> {
        let registered = self.registered(agent)?;
        let new = self
            .new_run_within(registered.starter(), input, None, Some(id), None, within)?
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

    /// [`start_child`](Self::start_child) for a child that lives in `conversation_id`, which is not
    /// a session's: a timer run of a session ([`ChildStarter::start_in_parent_conversation`](crate::ChildStarter::start_in_parent_conversation)) sits
    /// in a conversation made from the one its session belongs to, so that it can read the session
    /// later and is not in the way of another timer.
    pub async fn start_child_in(
        &self,
        parent: RunId,
        id: RunId,
        agent: &str,
        input: Inbound,
        conversation_id: &str,
    ) -> Result<bool, RuntimeError> {
        let registered = self.registered(agent)?;
        let new = self
            .new_run_within(
                registered.starter(),
                input,
                None,
                Some(id),
                Some(conversation_id),
                None,
            )?
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

    /// Start a round of a child **session** (ADR 0045): [`start_child`](Self::start_child) for a
    /// child that has a conversation, `conversation_id` (build it with
    /// [`session_conversation_id`](crate::session_conversation_id)), and, from round 2 on, that
    /// continues `prior`, the previous round, the way a task continues a finished one
    /// ([`start_with_id_continuing`](Self::start_with_id_continuing)). Round 1 passes no `prior`
    /// and its `id` is the session id.
    ///
    /// Because the child has a conversation, its end is reported to `parent` as
    /// [`CHILD_REPORTED_KIND`](crate::CHILD_REPORTED_KIND), not `adam.run.finished`. **No agent
    /// reads that report until ADR 0045 slice 2**: `LlmAgent` drops it, and the runtime does not
    /// count it as unread input (see `docs/reference/child-runs.md`).
    ///
    /// Idempotent like `start_child`: `true` if this call created the round, `false` if a run with
    /// `id` already exists **and is this round** (same agent and conversation, and for round 1 the
    /// same parent); then `input` is ignored and `prior` is not read.
    ///
    /// A session has one agent: a round of another agent is refused, because `prior` must be an
    /// agent's own finished round and round 1's id is fixed. Because the store allows one open
    /// run per agent and conversation, one round is open at a time.
    ///
    /// # Errors
    ///
    /// * [`RuntimeError::ConversationBusy`] (class `Rejected`, [`is_busy`](RuntimeError::is_busy))
    ///   when another round of the session is still open: nothing is created, and the caller
    ///   steers that round with [`deliver`](Self::deliver) instead.
    /// * [`RuntimeError::InvalidSession`] (class `Invalid`) when `conversation_id` is not a session
    ///   conversation id or is longer than
    ///   [`MAX_SESSION_CONVERSATION_BYTES`](crate::MAX_SESSION_CONVERSATION_BYTES), `id` does not
    ///   fit the round (round 1's id is the session id, later rounds' is not), a run with `id`
    ///   exists that is not this round, or `prior` is not a round of this session (another conversation, or the
    ///   session has no first round). A `prior` that is still open is the busy case above.
    /// * [`RuntimeError::NotFound`] / [`RuntimeError::WrongAgent`] for a `prior` that does not
    ///   exist or is a run of another agent.
    /// * [`RuntimeError::UnknownAgent`] and the store's errors.
    ///
    /// A round whose own record was purged and is asked for again is not recognised as a repeat
    /// (nothing is left to compare), as for [`start_with_id_continuing`](Self::start_with_id_continuing).
    pub async fn start_child_continuing(
        &self,
        parent: RunId,
        id: RunId,
        agent: &str,
        input: Inbound,
        conversation_id: &str,
        prior: Option<RunId>,
    ) -> Result<bool, RuntimeError> {
        self.start_child_continuing_within(parent, id, agent, input, conversation_id, prior, None)
            .await
    }

    /// [`start_child_continuing`](Self::start_child_continuing) for a round that must finish within
    /// `within` of now (the same limit as [`start_child_within`](Self::start_child_within), counted
    /// per round: each round has its own deadline). Past it the round fails and the session stays
    /// usable, as it does when its parent is cancelled.
    #[tracing::instrument(skip(self, input))]
    #[allow(clippy::too_many_arguments)] // `start_child_continuing` and the time limit
    pub async fn start_child_continuing_within(
        &self,
        parent: RunId,
        id: RunId,
        agent: &str,
        input: Inbound,
        conversation_id: &str,
        prior: Option<RunId>,
        within: Option<Duration>,
    ) -> Result<bool, RuntimeError> {
        fn invalid<T>(reason: impl Into<String>) -> Result<T, RuntimeError> {
            Err(RuntimeError::InvalidSession {
                reason: reason.into(),
            })
        }
        let registered = self.registered(agent)?;
        let Some((_, session)) = parse_session_conversation_id(conversation_id) else {
            return invalid("the conversation id is not a session's");
        };
        if conversation_id.len() > MAX_SESSION_CONVERSATION_BYTES {
            return invalid(format!(
                "the conversation id is {} bytes, more than the {MAX_SESSION_CONVERSATION_BYTES} allowed",
                conversation_id.len()
            ));
        }
        match prior {
            None if id != session => return invalid("round 1's run id is the session id"),
            Some(_) if id == session => return invalid("only round 1 has the session's id"),
            _ => {}
        }
        if let Some(existing) = self.inner.store.load_run(id).await? {
            let same = existing.agent == agent
                && existing.conversation_id.as_deref() == Some(conversation_id)
                && (prior.is_some() || existing.parent_id == Some(parent));
            return if same {
                Ok(false)
            } else {
                invalid("a run with this id exists and is not this round")
            };
        }
        let state = match prior {
            None => None,
            Some(prior) => {
                let rec = self
                    .inner
                    .store
                    .load_run(prior)
                    .await?
                    .ok_or(RuntimeError::NotFound(prior))?;
                if rec.agent != agent {
                    return Err(RuntimeError::WrongAgent {
                        run: prior,
                        agent: agent.to_owned(),
                    });
                }
                if rec.conversation_id.as_deref() != Some(conversation_id) {
                    return invalid("the run to continue is not in this session's conversation");
                }
                if rec.status.is_open() {
                    // The session's open round: the caller steers it instead.
                    return Err(RuntimeError::ConversationBusy {
                        agent: agent.to_owned(),
                        conversation_id: conversation_id.to_owned(),
                    });
                }
                // A client picks its own context ids, so a conversation id of this shape does not
                // make a session: round 1 must be there, started as a child.
                let first = if prior == session {
                    Some(rec.clone())
                } else {
                    self.inner.store.load_run(session).await?
                };
                let first_is_a_round = first.is_some_and(|f| {
                    f.agent == agent
                        && f.parent_id.is_some()
                        && f.conversation_id.as_deref() == Some(conversation_id)
                });
                if !first_is_a_round {
                    return invalid("the session has no first round");
                }
                Some(Envelope::decode(prior, &rec.state)?.agent)
            }
        };
        let new = self
            .new_run_within(
                registered.starter(),
                input,
                prior.zip(state.as_ref()),
                Some(id),
                Some(conversation_id),
                within,
            )?
            .parent(parent);
        self.create_with_id(new).await
    }

    /// The sessions `parent_conversation` holds, one row per session with its latest round, read
    /// from the runs of `agents` alone (ADR 0045; no store method of its own).
    ///
    /// `agents` names the agents whose rounds to look for (a session's agent is a subagent of the
    /// parent's folder, and a run listing is per agent; a name given twice counts once). Rows are
    /// ordered working first, then most recently updated, then by id, so a page boundary is
    /// unambiguous; `offset` and `limit` page them (`limit` is capped at
    /// [`MAX_SESSION_PAGE`](crate::MAX_SESSION_PAGE)). A run counts as a session's only when the session's round 1, the run
    /// whose id is the session id, exists with a parent: a root conversation that merely looks like
    /// a session's is not listed, and a session whose round 1 was purged is not either. Only the
    /// sessions of the parent itself are listed, not those of its sessions.
    ///
    /// A session's header, outcome and report live in its latest round's **state** (the envelope's
    /// `agent` member), so the scan reads whole records: a projection that dropped `state` could
    /// not say what a session is, whether it is closed or how its round ended. The count before the
    /// scan is the guard for that bound, not a consistency check: it refuses a parent with more than
    /// [`MAX_SESSION_ROUNDS_SCANNED`](crate::MAX_SESSION_ROUNDS_SCANNED) rounds without reading one
    /// of them (the count after the scan, below, is the consistency check).
    ///
    /// The scan pages `list_runs`, which orders by `updated_at`, so a round that commits between
    /// two page reads can move past the cursor and be missed. The listing counts again after the
    /// scan and repeats it when the counts differ, up to three times, then fails with
    /// [`RuntimeError::Contended`]; it never returns a page it knows is short.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::TooManyRounds`] when the parent has more than
    /// [`MAX_SESSION_ROUNDS_SCANNED`](crate::MAX_SESSION_ROUNDS_SCANNED) rounds of one agent,
    /// [`RuntimeError::Contended`] as above, and the store's errors.
    #[tracing::instrument(skip(self))]
    pub async fn list_sessions(
        &self,
        parent_conversation: &str,
        agents: &[&str],
        offset: usize,
        limit: usize,
    ) -> Result<SessionList, RuntimeError> {
        const SCAN_PAGE: usize = 200;
        const ATTEMPTS: usize = 3;
        let scope = ConversationScope::Prefix(session_conversation_prefix(parent_conversation));
        let mut agents: Vec<&str> = agents.to_vec();
        agents.sort_unstable();
        agents.dedup();

        let mut rounds: HashMap<(String, String), Vec<RunRecord>> = HashMap::new();
        'attempt: for _ in 0..ATTEMPTS {
            rounds.clear();
            for agent in &agents {
                let mut query = RunQuery::new(*agent, scope.clone(), SCAN_PAGE);
                let expected = self.inner.store.count_runs(&query).await?;
                if expected > MAX_SESSION_ROUNDS_SCANNED as u64 {
                    return Err(RuntimeError::TooManyRounds {
                        runs: expected,
                        max: MAX_SESSION_ROUNDS_SCANNED,
                    });
                }
                let mut seen: HashMap<RunId, RunRecord> = HashMap::new();
                loop {
                    let page = self.inner.store.list_runs(&query).await?;
                    let after = page.last().map(|r| (r.updated_at, r.id));
                    let full = page.len() == SCAN_PAGE;
                    seen.extend(page.into_iter().map(|r| (r.id, r)));
                    match after {
                        Some(after) if full => query.after = Some(after),
                        _ => break,
                    }
                }
                if self.inner.store.count_runs(&query).await? != seen.len() as u64 {
                    continue 'attempt;
                }
                for rec in seen.into_values() {
                    if let Some(conv) = rec.conversation_id.clone()
                        && session_of(parent_conversation, &conv).is_some()
                    {
                        rounds
                            .entry((rec.agent.clone(), conv))
                            .or_default()
                            .push(rec);
                    }
                }
            }
            return Ok(page_of_sessions(
                parent_conversation,
                rounds,
                offset,
                limit,
                self.inner.clock.now(),
                self.inner.cfg.session_idle_ttl,
            ));
        }
        Err(RuntimeError::Contended(format!(
            "the sessions of {parent_conversation:?} changed while they were listed"
        )))
    }

    /// One session of `parent_conversation`, read from its rounds: `None` when `session` is not
    /// the first round of a session of that conversation (an unknown id, another conversation's
    /// session, a run that merely spells a session's conversation id). The only way to a session
    /// by its id; the caller needs no agent name, because round 1 says it.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::TooManyRounds`] when the session has more than
    /// [`MAX_SESSION_ROUNDS_SCANNED`](crate::MAX_SESSION_ROUNDS_SCANNED) rounds, and the store's
    /// errors.
    #[tracing::instrument(skip(self))]
    pub async fn session_view(
        &self,
        parent_conversation: &str,
        session: RunId,
    ) -> Result<Option<SessionView>, RuntimeError> {
        const SCAN_PAGE: usize = 200;
        let Some(first) = self.inner.store.load_run(session).await? else {
            return Ok(None);
        };
        let Some(conversation) = first.conversation_id.clone() else {
            return Ok(None);
        };
        if first.parent_id.is_none()
            || session_of(parent_conversation, &conversation) != Some(session)
        {
            return Ok(None);
        }
        let mut query = RunQuery::new(
            first.agent.clone(),
            ConversationScope::Exact(conversation),
            SCAN_PAGE,
        );
        let expected = self.inner.store.count_runs(&query).await?;
        if expected > MAX_SESSION_ROUNDS_SCANNED as u64 {
            return Err(RuntimeError::TooManyRounds {
                runs: expected,
                max: MAX_SESSION_ROUNDS_SCANNED,
            });
        }
        let mut rounds: Vec<RunRecord> = Vec::new();
        loop {
            let page = self.inner.store.list_runs(&query).await?;
            let after = page.last().map(|r| (r.updated_at, r.id));
            let full = page.len() == SCAN_PAGE;
            rounds.extend(page);
            match after {
                Some(after) if full => query.after = Some(after),
                _ => break,
            }
        }
        Ok(SessionView::from_rounds(
            session,
            rounds,
            self.inner.clock.now(),
            self.inner.cfg.session_idle_ttl,
        ))
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
        self.new_run_within(agent, input, prior, id, conversation_id, None)
    }

    /// [`new_run`](Self::new_run) for a run that must finish within `within` of now, by the
    /// runtime's own clock.
    fn new_run_within(
        &self,
        agent: &dyn ErasedStarter,
        input: Inbound,
        prior: Option<(RunId, &Value)>,
        id: Option<RunId>,
        conversation_id: Option<&str>,
        within: Option<Duration>,
    ) -> Result<NewRun, RuntimeError> {
        let state = match prior {
            Some((run, state)) => agent.init_continuing(input, state, run)?,
            None => agent.init(input)?,
        };
        let mut envelope = Envelope::new(state);
        if let Some(within) = within {
            envelope = envelope.with_limit(self.inner.clock.now(), within);
        }
        let mut new = NewRun::new(agent.name(), envelope.encode()?);
        if let Some(id) = id {
            new = new.with_id(id);
        }
        if let Some(conv) = conversation_id {
            new = new.conversation(conv);
        }
        Ok(new)
    }

    /// The agent state that `prior`'s last commit holds, for a run of `agent` that continues it.
    ///
    /// A v5 prior run is joined from its head and the body generation the head names (one body
    /// read); a v4 one carries its whole state in the record.
    async fn prior_state(&self, agent: &str, prior: RunId) -> Result<Value, RuntimeError> {
        let (rec, state) = self
            .inner
            .joined_state(prior)
            .await?
            .ok_or(RuntimeError::NotFound(prior))?;
        if rec.agent != agent {
            // The caller named a run of another agent, whose state is not this agent's to read.
            return Err(RuntimeError::WrongAgent {
                run: prior,
                agent: agent.to_owned(),
            });
        }
        Ok(state)
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
    /// [`Signal::Finished`]), otherwise within one cancel poll interval. The step is
    /// not aborted by the runtime: a step that listens to the token stops
    /// early (`LlmAgent` drops its model request, the coder kills its
    /// commands), one that does not runs to its end (its result is dropped
    /// either way).
    ///
    /// **Cancelling a run cancels the runs that work for it** (ADR 0044 slice 10): a child run, whose
    /// parent is `Failed` for any reason (this cancel included), is failed with
    /// `cancelled: the parent run ended` by the worker that holds it, before it steps and on the
    /// poll of its cancel watch (at most once a second), and its own children after it in turn. A
    /// running child is told within a second or so, a runnable one when it is claimed, a parked one
    /// within a minute: a parked child that works for a parent always carries a wake no later than a
    /// minute away, and at it the runtime reads the parent **before** the agent is stepped, then parks
    /// the run again without stepping the agent when nothing else is due. A parent that is `Done`
    /// changes nothing, and one that cannot be loaded (purged by retention, or the store is down) is
    /// unknown and cancels nothing. A task a run started on a remote A2A agent is not cancelled by the
    /// runtime: after a successful cancel commit it calls [`Agent::on_cancelled`] of the run's agent
    /// (on a spawned task, best effort, once per commit, and only in a process that registered the
    /// full agent), and the agent decides. Asking twice, or for a finished run, is a no-op.
    #[tracing::instrument(skip(self))]
    pub async fn cancel(&self, run: RunId, reason: &str) -> Result<(), RuntimeError> {
        self.inner.cancel_run(run, reason).await
    }

    /// Snapshot of a run from its durable record (`None` if it does not exist).
    ///
    /// One read of the run's head when the store has a current one
    /// ([`Store::load_head`](adam_core::Store::load_head)), and the whole state when it does not (a
    /// v4 run, or a head a v4 commit left stale). No body is read: a head-only view is the read
    /// win of ADR 0055.
    ///
    /// Polled: the span is `debug` so a control plane at `info` pays nothing per poll.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn view(&self, run: RunId) -> Result<Option<RunView>, RuntimeError> {
        // The lease is read **before** the record. The two reads are not one snapshot, and this
        // order makes every interleaving harmless: a claim that lands between them reads as not
        // yet (the next view has it, and a worker emits a status event at once to prompt that
        // read), and a step that commits and releases between them is read with the version it
        // committed, so it is `working` from its version. Read the other way round, that step
        // read as version 1 with no lease, `submitted`, and a subscriber saw `working` go back.
        // It costs one primary-key read for a view of a parked or finished run.
        let until = self.inner.store.lease_until(run).await?;
        let Some(rec) = self.inner.store.load_head(run).await? else {
            return Ok(None);
        };
        let stored = Stored::decode(run, &rec.state)?;
        let claimed = adam_core::claimed(rec.status, until, self.inner.clock.now());
        Ok(Some(RunView::new(rec, stored, claimed)))
    }

    /// The run's **whole** agent state (`None` if the run does not exist): the head and the body
    /// generation it names, joined by the agent's
    /// [`join`](crate::Agent::join). This is the only read of a v5 run's transcript.
    ///
    /// A v4 run's state is whole in its record, so it is returned as it is with no body read. For
    /// a v5 run the rows the head names are read (`body_list`) and the join must reproduce the
    /// state: a set that does not match the head's `len` is read again once, in case the head moved
    /// between the two reads, and is [`RuntimeError::Corrupt`] if it did not.
    ///
    /// The agent must be registered on this runtime (an agent or a starter: both have `join`), and
    /// its `join` must be the one its `split` wrote the head with. A registration that only starts
    /// runs answers here too, because the join is on [`AgentStarter`](crate::AgentStarter).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::UnknownAgent`] when the run's agent is not registered,
    /// [`RuntimeError::Corrupt`] when the head or a body row does not decode or the generation does
    /// not match the head, and the agent's own error when `join` refuses the rows.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn state(&self, run: RunId) -> Result<Option<Value>, RuntimeError> {
        Ok(self.inner.joined_state(run).await?.map(|(_, state)| state))
    }

    /// The bytes of a file artifact of a view ([`ArtifactRef::file`]).
    ///
    /// An inline file (a v4 run) answers its own bytes. A blob is read once
    /// ([`Store::get_blob`](adam_core::Store::get_blob)) and cached by content address in a
    /// bounded, shared cache (32 MiB, least recently used first), so polling the same run does not
    /// read the file again. `None` when the blob is gone (purged once no run referenced it): that
    /// artifact cannot be served, and the run is intact.
    ///
    /// # Errors
    ///
    /// The store's errors. A missing blob is `Ok(None)`, not an error.
    pub async fn artifact_bytes(&self, file: &FileRef) -> Result<Option<Arc<[u8]>>, RuntimeError> {
        match &file.source {
            FileSource::Inline(bytes) => Ok(Some(Arc::from(bytes.as_slice()))),
            FileSource::Blob(id) => {
                if let Some(bytes) = self
                    .inner
                    .blobs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(id)
                {
                    return Ok(Some(bytes));
                }
                let Some(bytes) = self.inner.store.get_blob(id).await? else {
                    return Ok(None);
                };
                let bytes: Arc<[u8]> = Arc::from(bytes);
                self.inner
                    .blobs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .put(id.clone(), bytes.clone());
                Ok(Some(bytes))
            }
        }
    }

    /// The header of a run from its durable record (`None` if it does not exist): its status, its
    /// version and whether a worker holds it, without the state.
    ///
    /// One store call ([`Store::run_header`](adam_core::Store::run_header)): an adapter that
    /// overrides it projects the header and the lease; the trait's default (what `MemoryStore`
    /// uses) reads the lease and then the record. Use it for a reader that only needs the run's own
    /// bookkeeping — a step's cancel watch, a parent check. An A2A task's state also needs
    /// `waiting`, which is in the state, so a status poll reads [`view`](Self::view).
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn header(&self, run: RunId) -> Result<Option<RunHeader>, RuntimeError> {
        Ok(self.inner.store.run_header(run).await?)
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
            RuntimeError::InvalidSession { .. } => ErrorClass::Invalid,
            RuntimeError::TooManyRounds { .. } => ErrorClass::Rejected,
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
            RuntimeError::InvalidSession { reason: "x".into() },
            RuntimeError::TooManyRounds { runs: 2, max: 1 },
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
                false, false, false, false, false, false, true, false, true, true, true, false,
                false, false
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

#[cfg(test)]
mod defaults {
    use std::sync::Arc;

    use adam_core::MemoryStore;

    use super::*;

    /// The two polls are separate, with the fallbacks the docs promise: an idle worker every
    /// 250 ms (a person waits for work to start), a step's cancel watch every second (a signal
    /// normally tells it, and its read is one header).
    #[test]
    fn the_worker_and_the_cancel_watch_poll_at_their_own_intervals() {
        let builder = Runtime::builder(Arc::new(MemoryStore::new()));
        assert_eq!(builder.cfg.poll_interval, Duration::from_millis(250));
        assert_eq!(builder.cfg.cancel_poll_interval, Duration::from_secs(1));
        let builder = builder
            .poll_interval(Duration::from_millis(5))
            .cancel_poll_interval(Duration::from_millis(7));
        assert_eq!(builder.cfg.poll_interval, Duration::from_millis(5));
        assert_eq!(builder.cfg.cancel_poll_interval, Duration::from_millis(7));
    }
}
