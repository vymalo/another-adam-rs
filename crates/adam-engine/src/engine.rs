//! [`Engine`] and [`EngineBuilder`]: the runtime and its worker on the host's tokio runtime, and
//! every call through the code the A2A server calls.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use a2a::{Message, Part, Role, Task};
use adam_a2a::{BackendError, Caller, STEER_EXTENSION, TaskBackend, TaskQuery};
use adam_a2a_runtime::{RuntimeTaskBackend, task_id_for};
use adam_agent_fs::{AgentManifest, Subagent};
use adam_assembly::AgentDef;
use adam_core::{DynSecretSource, DynStore, RunId};
use adam_llm_agent::DEFAULT_MAX_PARALLEL;
use adam_model::DynModel;
use adam_runtime::{BroadcastSink, Runtime, RuntimeBuilder, RuntimeError};
use adam_ui::Ui;
use tokio::sync::{RwLock, RwLockReadGuard, oneshot, watch};
use tokio::task::AbortHandle;

use crate::events::EngineSink;
use crate::{
    ConversationId, Delivered, EngineError, ListQuery, Page, RunEvents, RunInfo, RunState, Say,
    Turn, default_secrets,
};

/// The one subject every call of an engine is made as: the engine serves one person, and the
/// backend scopes runs to a subject. Not `anonymous`, which the backend lets continue nothing.
const SUBJECT: &str = "adam-engine";

/// The default of [`Limits::event_capacity`].
const DEFAULT_EVENT_CAPACITY: usize = 256;

/// The largest [`Limits::event_capacity`]: the channel allocates every slot at once.
const MAX_EVENT_CAPACITY: usize = 1 << 16;

/// What the engine runs within. Set the fields on [`Limits::default`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Runs stepped at the same time (the runtime's `concurrency`). Default 4, at least 1.
    pub concurrency: usize,
    /// Tool calls of one model turn run at the same time, for every agent; an agent's file may
    /// lower it. Default 10, 1 to 32.
    pub max_parallel_tools: usize,
    /// How long a run stays leased to the worker without renewal. Default 30 s; zero is refused.
    pub lease_ttl: Duration,
    /// The live events the engine's ring holds, every run's together: a reader of
    /// [`Engine::events`] that falls further behind loses the oldest and is told
    /// ([`EngineEvent::Behind`](crate::EngineEvent::Behind)). The channel rounds it up to a power
    /// of two. Default 256; zero is refused, and so is more than 65 536.
    pub event_capacity: usize,
    /// The id the worker writes into the leases of the runs it steps. Default `None`; `Some("")`
    /// is refused.
    ///
    /// * `None`: a random id at every start, and nothing released. Engines that share a store never
    ///   take each other's leases, and a restarted engine waits up to
    ///   [`lease_ttl`](Self::lease_ttl) for the runs its last process was stepping.
    /// * `Some(id)`: **the host promises that no other live engine, in any process, uses `id` on
    ///   this store.** `start` then releases every lease `id` holds
    ///   (`Store::release_worker_leases`), so the runs the last process was stepping are claimed at
    ///   once. Set one only for a store no other engine shares, such as a store per install. The
    ///   SQLite store needs none: its lock allows one process per file, and opening the file clears
    ///   the last process's leases.
    ///   Broken, the promise costs a run stepped twice, its tools run twice: the release takes the
    ///   leases of the runs the other engine is stepping, and both renew under the same id.
    ///
    /// Within one process `start` refuses a second live engine with the same id on the same store
    /// ([`EngineError::WorkerIdInUse`]); it cannot see other processes. The id is free again once
    /// the first engine's worker task has ended: when [`Engine::shutdown`] returns, or shortly
    /// after the last `Engine` is dropped (its worker is aborted, which takes effect at the task's
    /// next poll). A step the worker spawned is aborted with it and not waited for, so a tool that
    /// ignores cancellation (blocking code, a task of its own) can still run after the id is free;
    /// a host that restarts in the same process calls `shutdown` first, which lets the steps in
    /// flight finish.
    pub worker_id: Option<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_parallel_tools: DEFAULT_MAX_PARALLEL,
            lease_ttl: Duration::from_secs(30),
            event_capacity: DEFAULT_EVENT_CAPACITY,
            worker_id: None,
        }
    }
}

/// Builds an [`Engine`]: [`Engine::builder`], then [`start`](Self::start).
#[must_use = "an engine builder does nothing until it is started"]
pub struct EngineBuilder {
    store: DynStore,
    agent: AgentDef,
    model: DynModel,
    alias: String,
    secrets: DynSecretSource,
    limits: Limits,
}

impl std::fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("agent", &self.agent.name())
            .field("alias", &self.alias)
            .field("secrets", &self.secrets)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl EngineBuilder {
    /// Where the secrets the agent's files name come from. Default [`default_secrets`], which
    /// refuses every name. It replaces whatever source `agent` was given.
    pub fn secrets(mut self, secrets: DynSecretSource) -> Self {
        self.secrets = secrets;
        self
    }

    /// The limits the engine runs within. Default [`Limits::default`].
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Migrate the store, release the leases a fixed [`Limits::worker_id`] holds in it (a restarted
    /// engine claims its old runs at once), build the agent and start the worker on the current
    /// tokio runtime.
    ///
    /// The agent gets the screen's tools of an embedded agent (`ask_user`, `show`, `ui_catalog`,
    /// `plan_*`, from `adam_ui::Ui::embedded`) and nothing on the network: a file that names an
    /// MCP server or a remote subagent does not bind.
    ///
    /// # Errors
    ///
    /// * [`EngineError::NoRuntime`] outside a tokio runtime;
    /// * [`EngineError::Invalid`] for a zero [`Limits::lease_ttl`], an [`Limits::event_capacity`] of
    ///   zero or over 65 536, a [`Limits::worker_id`] of `Some("")`, and for an agent whose files
    ///   name an MCP server or a remote (`a2a:`) subagent, its subagents' included: the engine runs
    ///   neither yet, whatever features the build turns on;
    /// * [`EngineError::WorkerIdInUse`] when another live engine of this process uses the same
    ///   [`Limits::worker_id`] on the same store;
    /// * [`EngineError::Store`] when the store's migration or the release of the worker's leases
    ///   fails, with the store's class;
    /// * [`EngineError::Agent`] when the agent does not bind (an unknown tool, an MCP server, a
    ///   remote subagent, a secret refused) or the model alias is refused.
    pub async fn start(self) -> Result<Engine, EngineError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| EngineError::NoRuntime)?;
        let Self {
            store,
            agent,
            model,
            alias,
            secrets,
            limits,
        } = self;
        if limits.lease_ttl.is_zero() {
            return Err(EngineError::Invalid(
                "the lease of a run must last longer than zero".to_owned(),
            ));
        }
        if !(1..=MAX_EVENT_CAPACITY).contains(&limits.event_capacity) {
            return Err(EngineError::Invalid(format!(
                "the ring of live events holds 1 to {MAX_EVENT_CAPACITY} events, not {}",
                limits.event_capacity
            )));
        }
        if limits.worker_id.as_deref() == Some("") {
            return Err(EngineError::Invalid(
                "the worker's id must not be empty".to_owned(),
            ));
        }
        refuse_what_is_not_embedded(agent.manifest(), agent.name())?;
        let name = agent.name().to_owned();
        let ui = Ui::embedded();
        let assembly = agent
            .secrets(secrets)
            .bind(ui.tools())
            .map_err(|e| EngineError::Agent(Box::new(e)))?
            .tool_source(ui.screen())
            .max_parallel(limits.max_parallel_tools)
            .model(model, alias)
            .map_err(|e| EngineError::Agent(Box::new(e)))?;
        store.migrate().await.map_err(EngineError::Store)?;
        // Held until the worker task has ended, so a second engine of this process with the same id
        // on this store is refused before its release could take this one's leases.
        let claim = match &limits.worker_id {
            Some(id) => {
                let claim = IdClaim::take(&store, id)?;
                // The one live engine with this id (the host's promise): what it holds is the last
                // process's.
                store
                    .release_worker_leases(id)
                    .await
                    .map_err(EngineError::Store)?;
                Some(claim)
            }
            None => None,
        };

        let (runtime, events) =
            runtime_with_ring(assembly.register(Runtime::builder(store)), &limits);
        // The backend is given the same ring, for the A2A path that streams a task
        // (`TaskBackend::subscribe`), whose file parts are built from a live event's bytes. The engine
        // never calls that path: an artifact in this ring has its bytes withheld, so a caller that
        // streams a task through this backend must read files from the store.
        let backend = RuntimeTaskBackend::new(runtime.clone(), events.clone(), name.clone());

        let (stop, stopped) = oneshot::channel::<()>();
        let running = handle.spawn(async move {
            runtime
                .run_worker(async {
                    let _ = stopped.await;
                })
                .await
        });
        let abort = running.abort_handle();
        // A task of its own says how the worker ended, an abort included, so that every waiting
        // `shutdown` hears it and nobody has to hold the worker's handle to stop it.
        let (said, ended) = watch::channel(None);
        handle.spawn(async move {
            let how = match running.await {
                Ok(Ok(())) => Ended::Clean,
                Ok(Err(error)) => Ended::Failed(Arc::new(error)),
                Err(joined) if joined.is_cancelled() => Ended::Aborted,
                Err(joined) => Ended::Panicked(joined.to_string()),
            };
            // The worker's future is dropped by now: the id is free before `shutdown` returns.
            drop(claim);
            let _ = said.send(Some(how));
        });
        let caller = Caller::new(SUBJECT);
        Ok(Engine {
            shared: Arc::new(Shared {
                steering: caller.clone().with_extensions([STEER_EXTENSION]),
                caller,
                backend,
                events,
                agent: name,
                writes: RwLock::new(()),
                stopping: AtomicBool::new(false),
                worker: Worker {
                    stop: Mutex::new(Some(stop)),
                    abort,
                    ended,
                },
            }),
        })
    }
}

/// The runtime `builder` builds (its agents registered) within `limits`, and the ring its events go
/// to: through [`EngineSink`], which withholds a file artifact's bytes.
pub(crate) fn runtime_with_ring(
    builder: RuntimeBuilder,
    limits: &Limits,
) -> (Runtime, BroadcastSink) {
    let events = BroadcastSink::new(limits.event_capacity);
    let builder = builder
        .event_sink(EngineSink(events.clone()))
        .concurrency(limits.concurrency)
        .lease_ttl(limits.lease_ttl);
    // `None` keeps the runtime's default, a random id.
    let builder = match &limits.worker_id {
        Some(id) => builder.worker_id(id.clone()),
        None => builder,
    };
    let runtime = builder.build();
    (runtime, events)
}

/// The fixed worker ids the live engines of this process use, each with the address of its store:
/// the store lives while the entry does (the worker holds it), so an address is not reused under it.
static LIVE_IDS: Mutex<BTreeSet<(usize, String)>> = Mutex::new(BTreeSet::new());

/// A fixed worker id on a store, taken by one live engine of this process; given back on drop.
struct IdClaim((usize, String));

impl IdClaim {
    fn take(store: &DynStore, id: &str) -> Result<Self, EngineError> {
        let key = (Arc::as_ptr(store).cast::<()>().addr(), id.to_owned());
        let mut live = LIVE_IDS.lock().unwrap_or_else(PoisonError::into_inner);
        if !live.insert(key.clone()) {
            return Err(EngineError::WorkerIdInUse { id: id.to_owned() });
        }
        Ok(Self(key))
    }
}

impl Drop for IdClaim {
    fn drop(&mut self) {
        LIVE_IDS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// How the worker task ended.
#[derive(Clone)]
pub(crate) enum Ended {
    Clean,
    Failed(Arc<RuntimeError>),
    Aborted,
    Panicked(String),
}

struct Worker {
    /// Taken by the first `shutdown`.
    stop: Mutex<Option<oneshot::Sender<()>>>,
    /// Reachable until the engine is gone: aborting a task that ended does nothing.
    abort: AbortHandle,
    ended: watch::Receiver<Option<Ended>>,
}

/// Aborts the worker unless disarmed: a `shutdown` dropped before the worker ended stops it harder,
/// as dropping `Runtime::run_worker`'s future does.
struct AbortOnDrop(Option<AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(abort) = self.0.take() {
            abort.abort();
        }
    }
}

pub(crate) struct Shared {
    backend: RuntimeTaskBackend,
    /// The ring every reader of [`Engine::events`] subscribes to; the runtime emits into it through
    /// [`EngineSink`], which takes a file's bytes out.
    events: BroadcastSink,
    /// Every call but a steer.
    caller: Caller,
    /// The same subject, with `steer/v1` activated: a message for a running run is delivered.
    steering: Caller,
    agent: String,
    /// Held shared by every message for the length of its call, and exclusively by `shutdown` while
    /// it sets `stopping`: a message is either written before the worker is told to stop, or
    /// refused.
    writes: RwLock<()>,
    stopping: AtomicBool,
    worker: Worker,
}

impl Drop for Shared {
    /// The last engine is gone: a worker that still runs is aborted as a killed process would be,
    /// and its runs are claimed again once their leases run out.
    fn drop(&mut self) {
        self.worker.abort.abort();
    }
}

/// adam-rs in an application: one agent, its runs in the store the host gave, stepped by a worker
/// on the host's tokio runtime. Clones share everything.
///
/// Inside, every call is what the A2A server would do for one caller (`RuntimeTaskBackend` of
/// `adam-a2a-runtime`), so follow-ups, steering and answers have one implementation.
#[derive(Clone)]
pub struct Engine {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("agent", &self.shared.agent)
            .field("stopping", &self.shared.stopping.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// An engine for `agent` over `store`, whose agents talk to `model`; `alias` is the model
    /// alias of an agent whose file names none.
    pub fn builder(
        store: DynStore,
        agent: AgentDef,
        model: DynModel,
        alias: impl Into<String>,
    ) -> EngineBuilder {
        EngineBuilder {
            store,
            agent,
            model,
            alias: alias.into(),
            secrets: default_secrets(),
            limits: Limits::default(),
        }
    }

    /// Send `say` to a conversation: `None` starts a new one, `Some` continues it.
    ///
    /// In a conversation whose latest run has ended, the message starts a new run that continues
    /// it: the agent starts from the latest run's state, the transcript included. In one whose
    /// latest run is still open, the message is delivered to that run: as a steer while it works,
    /// as the answer while it waits. [`Turn::delivered`] says which. A `Say` sent again (the same
    /// id) after it started a run starts nothing: [`Delivered::Repeated`].
    ///
    /// # Errors
    ///
    /// [`EngineError::Invalid`] for a message with no text or no id, [`EngineError::NotFound`]
    /// for a conversation this engine has no run in, [`EngineError::Stopped`] after
    /// [`shutdown`](Self::shutdown), and the store's failures.
    pub async fn send(
        &self,
        conversation: Option<&ConversationId>,
        say: Say,
    ) -> Result<Turn, EngineError> {
        let _writing = self.writable().await?;
        say.check()?;
        let shared = &self.shared;
        let context = conversation.map(|c| c.as_str().to_owned());
        // The run this message starts, if it starts one: the backend derives it the same way, so
        // a repeat finds the run its first attempt started.
        let own = task_id_for(&shared.agent, SUBJECT, context.as_deref(), &say.id);
        if let Some(info) = self.get(own).await? {
            return Ok(Turn {
                conversation: info.conversation.clone(),
                run: own,
                delivered: Delivered::Repeated,
                info,
            });
        }
        let mut message = message(&say);
        let latest = match conversation {
            Some(conversation) => {
                let latest = self.latest_run(conversation).await?;
                // Continued once it has ended; while it is open, the backend delivers to it.
                message.reference_task_ids = Some(vec![latest.run.to_string()]);
                Some(latest)
            }
            None => None,
        };
        let task = shared
            .backend
            .submit(shared.caller.clone(), message, None, context)
            .await
            .map_err(|e| EngineError::of_backend(e, None))?;
        let info = RunInfo::of(&task)?;
        let delivered = match &latest {
            _ if info.run != own => match latest {
                Some(latest) if latest.run == info.run && latest.state == RunState::Waiting => {
                    Delivered::Answered
                }
                _ => Delivered::Steered,
            },
            Some(_) => Delivered::Continued,
            None => Delivered::Started,
        };
        Ok(Turn {
            conversation: info.conversation.clone(),
            run: info.run,
            delivered,
            info,
        })
    }

    /// Send `say` to `run` while it works: it reads it at its next step, and a run about to
    /// finish takes another step first. A run that is waiting takes it as its answer.
    ///
    /// # Errors
    ///
    /// [`EngineError::Finished`] for a run that has ended, [`EngineError::NotFound`] for one that
    /// does not exist, and as [`send`](Self::send).
    pub async fn steer(&self, run: RunId, say: Say) -> Result<RunInfo, EngineError> {
        let _writing = self.writable().await?;
        say.check()?;
        let shared = &self.shared;
        let steered = shared
            .backend
            .submit(
                shared.steering.clone(),
                message(&say),
                Some(run.to_string()),
                None,
            )
            .await;
        match steered {
            Ok(task) => RunInfo::of(&task),
            Err(BackendError::InvalidParams(_)) => Err(self.refused(run, Expected::Open).await),
            Err(e) => Err(EngineError::of_backend(e, Some(run))),
        }
    }

    /// Answer the question `run` waits on ([`RunState::Waiting`]); it goes on working.
    ///
    /// # Errors
    ///
    /// [`EngineError::NotWaiting`] (class `Rejected`) for a run that is not waiting, and
    /// [`EngineError::Finished`] for one that has ended; otherwise as [`steer`](Self::steer).
    pub async fn answer(&self, run: RunId, say: Say) -> Result<RunInfo, EngineError> {
        let _writing = self.writable().await?;
        say.check()?;
        let shared = &self.shared;
        let answered = shared
            .backend
            .submit(
                shared.caller.clone(),
                message(&say),
                Some(run.to_string()),
                None,
            )
            .await;
        match answered {
            Ok(task) => RunInfo::of(&task),
            // The message itself was checked above: what the backend refuses is the run's state.
            Err(BackendError::InvalidParams(_)) => Err(self.refused(run, Expected::Waiting).await),
            Err(e) => Err(EngineError::of_backend(e, Some(run))),
        }
    }

    /// Cancel `run`: it ends [`Cancelled`](RunState::Cancelled), and a step in flight is stopped.
    /// Cancelling a cancelled run answers it as it is.
    ///
    /// # Errors
    ///
    /// [`EngineError::Finished`] for a run that ended otherwise, [`EngineError::NotFound`] for one
    /// that does not exist.
    pub async fn cancel(&self, run: RunId) -> Result<RunInfo, EngineError> {
        let shared = &self.shared;
        match shared
            .backend
            .cancel(&shared.caller, &run.to_string())
            .await
        {
            Ok(task) => RunInfo::of(&task),
            // The run ended under the cancel: cancelled by another call, or otherwise.
            Err(BackendError::InvalidParams(_)) => match self.get(run).await {
                Ok(Some(info)) if info.state == RunState::Cancelled => Ok(info),
                read => Err(EngineError::by_state(
                    run,
                    read.map(|info| info.map(|i| i.state)),
                    Expected::Open,
                )),
            },
            Err(e) => Err(EngineError::of_backend(e, Some(run))),
        }
    }

    /// The events of `run`, from now on: see [`RunEvents`]. The run's recent events come first
    /// (what happened since it last changed state, from the runtime's replay), then the live ones,
    /// then [`EngineEvent::Ended`](crate::EngineEvent::Ended) once the store says it has ended. A
    /// run that has already ended gives `Ended` at once. A reader that falls more than
    /// [`Limits::event_capacity`] events behind loses the oldest, and is told with
    /// [`EngineEvent::Behind`](crate::EngineEvent::Behind), then a
    /// [`Snapshot`](crate::EngineEvent::Snapshot) read from the store.
    ///
    /// The subscription is made when this returns, so a reader may read later: what it missed in
    /// between is either in the ring or said by `Behind`. Once the worker has stopped
    /// ([`shutdown`](Self::shutdown), or every engine dropped) with the run unfinished, the stream
    /// ends with [`EngineError::Stopped`].
    ///
    /// # Errors
    ///
    /// [`EngineError::NotFound`] for a run this engine does not have (the stream would have nothing
    /// to say), and the store's failures.
    pub async fn events(&self, run: RunId) -> Result<RunEvents, EngineError> {
        // Subscribed before the read: an event emitted after the read is in the subscription.
        let subscription = self.shared.events.subscribe_run(run);
        let info = self
            .get(run)
            .await?
            .ok_or_else(|| EngineError::NotFound(format!("run {run}")))?;
        Ok(RunEvents::new(
            run,
            subscription,
            Arc::downgrade(&self.shared),
            self.shared.worker.ended.clone(),
            info,
        ))
    }

    /// The engine `shared` is, while any [`Engine`] is left.
    pub(crate) fn from_weak(shared: &Weak<Shared>) -> Option<Self> {
        shared.upgrade().map(|shared| Self { shared })
    }

    /// The runs that match `query`, the most recently updated first.
    ///
    /// # Errors
    ///
    /// [`EngineError::Invalid`] for a page token of another query, and the store's failures.
    pub async fn list(&self, query: ListQuery) -> Result<Page<RunInfo>, EngineError> {
        let shared = &self.shared;
        let mut tasks = TaskQuery::new();
        tasks.context_id = query.conversation.as_ref().map(|c| c.as_str().to_owned());
        tasks.page_size = query.resolved_page_size();
        tasks.page_token = query.page_token;
        let page = shared
            .backend
            .list(&shared.caller, &tasks)
            .await
            .map_err(|e| EngineError::of_backend(e, None))?;
        Ok(Page {
            items: page
                .tasks
                .iter()
                .map(RunInfo::of)
                .collect::<Result<_, _>>()?,
            next_page_token: page.next_page_token,
            total: page.total_size,
        })
    }

    /// `run` as the store has it now, or `None` when this engine has no such run.
    ///
    /// # Errors
    ///
    /// The store's failures.
    pub async fn get(&self, run: RunId) -> Result<Option<RunInfo>, EngineError> {
        let shared = &self.shared;
        shared
            .backend
            .get(&shared.caller, &run.to_string())
            .await
            .map_err(|e| EngineError::of_backend(e, Some(run)))?
            .as_ref()
            .map(RunInfo::of)
            .transpose()
    }

    /// Stop the worker: nothing new is claimed, the steps in flight finish and commit, and their
    /// leases are released. It returns once the worker has stopped, for every caller: a second
    /// call, concurrent or later, waits for the same end. Every clone stops; a later
    /// [`send`](Self::send), [`steer`](Self::steer) or [`answer`](Self::answer) is
    /// [`EngineError::Stopped`], while reads and [`cancel`](Self::cancel) still work. A message
    /// accepted before is in the store and is stepped by the next engine over it.
    ///
    /// **Dropping the future before it is done** (a timeout around it) stops the worker harder:
    /// the steps in flight are aborted, and their runs are claimed again once their leases run out.
    ///
    /// # Errors
    ///
    /// What the worker ended with.
    pub async fn shutdown(&self) -> Result<(), EngineError> {
        let worker = &self.shared.worker;
        {
            let _no_writes = self.shared.writes.write().await;
            self.shared.stopping.store(true, Ordering::SeqCst);
        }
        let stop = worker
            .stop
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        let mut harder = AbortOnDrop(Some(worker.abort.clone()));
        let mut ended = worker.ended.clone();
        let how = ended
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|how| how.clone());
        harder.0 = None;
        match how {
            // An abort is a stop too (a `shutdown` dropped before this one).
            Some(Ended::Clean | Ended::Aborted) | None => Ok(()),
            Some(Ended::Failed(error)) => Err(EngineError::of_runtime(error)),
            Some(Ended::Panicked(why)) => Err(EngineError::internal(format!(
                "the worker task panicked: {why}"
            ))),
        }
    }

    /// The gate a message passes: held for its whole call, so `shutdown` waits for it, and
    /// refused once `shutdown` has begun.
    async fn writable(&self) -> Result<RwLockReadGuard<'_, ()>, EngineError> {
        let gate = self.shared.writes.read().await;
        if self.shared.stopping.load(Ordering::SeqCst) {
            Err(EngineError::Stopped)
        } else {
            Ok(gate)
        }
    }

    /// The refusal of a call the backend refused for the run's state (a state that moved under
    /// the call): from the state read now, never one it did not read.
    async fn refused(&self, run: RunId, expected: Expected) -> EngineError {
        let read = self.get(run).await.map(|info| info.map(|i| i.state));
        EngineError::by_state(run, read, expected)
    }

    /// The run a conversation's next one continues: the most recently updated of its runs. The
    /// backend continues it only once it has ended; an open one takes the message instead.
    async fn latest_run(&self, conversation: &ConversationId) -> Result<RunInfo, EngineError> {
        let shared = &self.shared;
        let mut query = TaskQuery::new();
        query.context_id = Some(conversation.as_str().to_owned());
        query.page_size = 1;
        let page = shared
            .backend
            .list(&shared.caller, &query)
            .await
            .map_err(|e| EngineError::of_backend(e, None))?;
        let latest: Option<&Task> = page.tasks.first();
        match latest {
            Some(task) => RunInfo::of(task),
            None => Err(EngineError::NotFound(format!(
                "conversation {conversation}"
            ))),
        }
    }
}

/// Refuse an agent whose files, or a local subagent's, name an MCP server (`mcp.json`) or a remote
/// (`a2a:`) subagent. The engine links no MCP client and no A2A client (ADR 0047 decision 2), and
/// what it accepts does not depend on the features another crate of the build turns on in
/// `adam-assembly`.
fn refuse_what_is_not_embedded(manifest: &AgentManifest, name: &str) -> Result<(), EngineError> {
    if manifest
        .mcp
        .as_ref()
        .is_some_and(|mcp| !mcp.servers.is_empty())
    {
        return Err(EngineError::Invalid(format!(
            "the agent `{name}` names MCP servers (`mcp.json`): the embedded engine runs no MCP \
             server yet"
        )));
    }
    for subagent in &manifest.subagents {
        match subagent {
            Subagent::Local(child) => {
                refuse_what_is_not_embedded(child, &format!("{name}/{}", child.name))?;
            }
            Subagent::Remote(remote) => {
                return Err(EngineError::Invalid(format!(
                    "the subagent `{name}/{}` is a remote A2A agent (`a2a:`): the embedded engine \
                     runs no remote subagent yet",
                    remote.name
                )));
            }
        }
    }
    Ok(())
}

/// What a refused call needed of the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expected {
    /// An answer: the run waits.
    Waiting,
    /// A steer or a cancel: the run has not ended.
    Open,
}

/// `say` as the message the backend reads: its text, under its id.
fn message(say: &Say) -> Message {
    let mut message = Message::new(Role::User, vec![Part::text(say.text.clone())]);
    message.message_id = say.id.clone();
    message
}

#[cfg(test)]
mod tests {
    use adam_core::{MemoryStore, RunStatus};
    use adam_runtime::{Agent, AgentError, Artifact, Ctx, Inbound, RunEvent, Transition};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::WithheldFile;

    const FILE: &[u8] = b"the bytes of the file, kept with the run";

    /// One step: it makes a file, and is done.
    struct MakesAFile;

    #[async_trait]
    impl Agent for MakesAFile {
        type State = Value;

        fn name(&self) -> &str {
            "maker"
        }

        fn init(&self, _: Inbound) -> Result<Value, AgentError> {
            Ok(Value::Null)
        }

        async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
            let file = Artifact::file("notes", "text/plain", "notes.txt", FILE.to_vec())
                .map_err(AgentError::permanent)?;
            ctx.emit(file.into()).await;
            Ok(Transition::Done {
                state,
                output: json!("made"),
            })
        }
    }

    /// The engine's runtime puts a file's event into its ring with the bytes withheld, while the
    /// run's committed record keeps every byte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_engines_runtime_withholds_file_bytes_from_the_ring_only() {
        let store: DynStore = Arc::new(MemoryStore::new());
        store.migrate().await.unwrap();
        let (runtime, ring) = runtime_with_ring(
            Runtime::builder(store).agent(MakesAFile),
            &Limits::default(),
        );
        let run = runtime
            .start("maker", Inbound::new("message", json!({})), None)
            .await
            .unwrap();
        let mut reader = ring.subscribe_run(run);
        let (stop, stopped) = oneshot::channel::<()>();
        let worker = {
            let runtime = runtime.clone();
            tokio::spawn(async move {
                runtime
                    .run_worker(async {
                        let _ = stopped.await;
                    })
                    .await
            })
        };

        let event = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let event = reader.recv().await.expect("the ring is open");
                if matches!(event, RunEvent::Artifact { .. }) {
                    return event;
                }
            }
        })
        .await
        .expect("the file's event");
        let RunEvent::Artifact {
            file: Some(file), ..
        } = &event
        else {
            panic!("not a file: {event:?}");
        };
        assert!(
            file.bytes.is_empty(),
            "{} bytes in the ring",
            file.bytes.len()
        );
        let sha256 = Artifact::file("notes", "text/plain", "notes.txt", FILE.to_vec())
            .unwrap()
            .file_sha256()
            .unwrap();
        assert_eq!(
            WithheldFile::of(&event),
            Some(WithheldFile {
                len: FILE.len() as u64,
                sha256
            })
        );

        let view = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let view = runtime.view(run).await.unwrap().expect("the run");
                if view.status == RunStatus::Done {
                    return view;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the run is done");
        let kept = view.artifacts[0].file.as_ref().expect("a file");
        let bytes = runtime
            .artifact_bytes(kept)
            .await
            .unwrap()
            .expect("its bytes");
        assert_eq!(&*bytes, FILE, "the record keeps the whole file");

        let _ = stop.send(());
        worker.await.unwrap().unwrap();
    }
}
