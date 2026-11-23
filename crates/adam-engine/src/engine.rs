//! [`Engine`] and [`EngineBuilder`]: the runtime and its worker on the host's tokio runtime, and
//! every call through the code the A2A server calls.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use a2a::{Message, Part, Role, Task};
use adam_a2a::{
    BackendError, CHILD_SESSIONS_EXTENSION, Caller, STEER_EXTENSION, TaskBackend, TaskQuery,
};
use adam_a2a_runtime::{Commands, RuntimeTaskBackend, default_inbound, task_id_for};
use adam_agent_fs::{AgentManifest, Command, Subagent};
use adam_assembly::AgentDef;
use adam_core::{ConversationScope, DynSecretSource, DynStore, RunId, RunQuery, RunStatus};
use adam_llm_agent::{DEFAULT_MAX_PARALLEL, ToolSet};
use adam_model::{DynModel, ModelProfiles, ReasoningEffort};
use adam_runtime::{
    BroadcastSink, FileRef, RetryPolicy, RunView, Runtime, RuntimeBuilder, RuntimeError,
    SCHEDULE_CONVERSATION_PREFIX, SessionLimits,
};
use adam_ui::Ui;
use tokio::sync::{RwLock, RwLockReadGuard, oneshot, watch};
use tokio::task::AbortHandle;

use crate::events::EngineSink;
use crate::{
    ConversationId, Delivered, EngineError, Firing, FiringQuery, ListQuery, Page, RunEvents,
    RunFile, RunInfo, RunState, Say, SessionHelper, SessionInfo, StartSession, Turn,
    default_secrets,
};

/// Why a firing a host cancelled ended (#289).
const FIRING_CANCEL_REASON: &str = "cancelled by the host";

/// The page token of [`Engine::firings`]: the position of the page's last firing and the schedule
/// the query names, so a token of another query is refused.
mod firing_token {
    use adam_core::RunId;
    use adam_core::time::Timestamp;

    use crate::EngineError;

    pub(super) fn write(filter: &str, at: Timestamp, run: RunId) -> String {
        format!("{at}|{run}|{filter}")
    }

    pub(super) fn read(token: &str, filter: &str) -> Result<(Timestamp, RunId), EngineError> {
        let bad = || EngineError::Invalid("the page token is not one of this query's".to_owned());
        let mut parts = token.splitn(3, '|');
        let at = parts.next().and_then(|t| t.parse().ok()).ok_or_else(bad)?;
        let run = parts
            .next()
            .and_then(|r| uuid::Uuid::parse_str(r).ok())
            .map(RunId)
            .ok_or_else(bad)?;
        (parts.next() == Some(filter))
            .then_some((at, run))
            .ok_or_else(bad)
    }
}

/// The one subject every call of an engine is made as: the engine serves one person, and the
/// backend scopes runs to a subject. Not `anonymous`, which the backend lets continue nothing.
const SUBJECT: &str = "adam-engine";

/// The default of [`Limits::event_capacity`].
const DEFAULT_EVENT_CAPACITY: usize = 256;

/// The largest [`Limits::event_capacity`]: the channel allocates every slot at once.
const MAX_EVENT_CAPACITY: usize = 1 << 16;

/// What the engine runs within. Set the fields on [`Limits::default`].
#[derive(Clone, Debug, PartialEq)]
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
    ///   SQLite store needs none: its lock allows one process per file, and `start` (its `migrate`)
    ///   clears every lease the file holds. Run one engine per SQLite file: the clearing takes a live
    ///   engine's leases of the same process too.
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
    /// The runtime's retry policy: what a transient failure that names no policy of its own (a
    /// model provider's outage, a tool with none) is retried under ([ADR
    /// 0061](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0061-a-step-names-its-retry-policy.md)).
    /// A folder's `retry:` and a tool's own policy win over it for their tools. Default
    /// [`RetryPolicy::default`]: 5 attempts, 1 s to 60 s.
    pub retry: RetryPolicy,
    /// The limits of the root's child sessions (`child-sessions/v1`, ADR 0045): how many are open
    /// and working at once in one conversation, and how many rounds one may have. The agent's
    /// file may lower them, never raise them. Default [`SessionLimits::default`]: 16 open, 4
    /// working, 20 rounds. It replaces what the agent's definition was given with
    /// `AgentDef::session_limits`.
    pub sessions: SessionLimits,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_parallel_tools: DEFAULT_MAX_PARALLEL,
            lease_ttl: Duration::from_secs(30),
            event_capacity: DEFAULT_EVENT_CAPACITY,
            worker_id: None,
            retry: RetryPolicy::default(),
            sessions: SessionLimits::default(),
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
    profiles: ModelProfiles,
    reasoning_effort: Option<ReasoningEffort>,
    tools: ToolSet,
}

impl std::fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("agent", &self.agent.name())
            .field("alias", &self.alias)
            .field("secrets", &self.secrets)
            .field("limits", &self.limits)
            .field("reasoning_effort", &self.reasoning_effort)
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

    /// The host's own tools (`#[tool]` functions, `tools!`, an `FnTool`), beside the engine's own
    /// (`ask_user`, `show`, `ui_catalog`, the plan tools): together they are the catalog the
    /// agent's `tools:` selects from, as on a server built with `adam-host`. Default none; a later
    /// call replaces the set. A name used twice (one of the engine's included), or one the files
    /// name and nobody gives, is refused at [`start`](Self::start).
    pub fn tools(mut self, tools: ToolSet) -> Self {
        self.tools = tools;
        self
    }

    /// The limits the engine runs within. Default [`Limits::default`].
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The deployment's per-model profiles (ADR 0063, ADR 0064): what each model takes as a
    /// reasoning effort, which preset it starts from, and its output cap. They reach the agent's
    /// definition as [`BoundDef::model_profiles`](adam_assembly::BoundDef::model_profiles) does:
    /// the folder's `reasoning_effort` and `model_preset` resolve against them, and the effort and
    /// the cap of each model call are checked and resolved per model. Default
    /// [`ModelProfiles::default`]: no model has a profile, so an agent whose file sets an effort or
    /// a preset is refused at [`start`](Self::start) (fail closed), one that sets neither sends
    /// none, and every cap is the folder's, the preset's or the profile's, then clamped to the
    /// model's limit and the window.
    pub fn models(mut self, profiles: ModelProfiles) -> Self {
        self.profiles = profiles;
        self
    }

    /// The reasoning level the root's model calls ask for when the root's file sets none (ADR 0063
    /// decision 8): an embedded agent's host gives the level its folder would. It **replaces**
    /// what [`AgentDef::reasoning_effort`](adam_assembly::AgentDef::reasoning_effort) was given,
    /// wins over the agent's `model_preset` and the profile, and loses to the root's own
    /// `reasoning_effort`. It must be one of the root model's levels ([`models`](Self::models))
    /// **when it applies** — when the root's file sets none; a level the file supplies is validated
    /// instead, and a file's level the profile does not take is refused at [`start`](Self::start)
    /// all the same. Default none: nothing is sent unless the folder, the preset or the profile
    /// sets a level.
    pub fn reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    /// Migrate the store, release the leases a fixed [`Limits::worker_id`] holds in it (a restarted
    /// engine claims its old runs at once), build the agent, register its scheduler and make the
    /// store's scheduler runs match the folder's `schedules/` (ADR 0060), and start the worker on
    /// the current tokio runtime.
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
    ///   remote subagent, a secret refused), when the model alias is refused, or when the effort,
    ///   the `model_preset` or the output cap the agent resolves to is refused: a level its model's
    ///   profile does not take, a preset the profiles do not declare, a preset above the model's
    ///   limit (the folder's, the builder's and the profile's values, ADR 0063 and ADR 0064);
    /// * [`EngineError::Schedules`] when the scheduler runs cannot be made to match the folder, with
    ///   the reconcile's class: on a retryable one, call `start` again (what was done is kept).
    pub async fn start(self) -> Result<Engine, EngineError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| EngineError::NoRuntime)?;
        let Self {
            store,
            agent,
            model,
            alias,
            secrets,
            limits,
            profiles,
            reasoning_effort,
            tools,
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
        let schedules = agent.manifest().schedules.clone();
        let commands = agent.manifest().commands.clone();
        let ui = Ui::embedded();
        // The host's level for an embedded agent whose file sets none (ADR 0063 decision 8): the
        // folder's own `reasoning_effort` wins.
        let agent = match reasoning_effort {
            Some(effort) => agent.reasoning_effort(Some(effort)),
            None => agent,
        };
        let assembly = agent
            .secrets(secrets)
            .session_limits(limits.sessions)
            .bind(ui.tools().extend(tools))
            .map_err(|e| EngineError::Agent(Box::new(e)))?
            .tool_source(ui.screen())
            .max_parallel(limits.max_parallel_tools)
            .model_profiles(profiles)
            .allow_model_choice()
            .model(model, alias)
            .map_err(|e| EngineError::Agent(Box::new(e)))?;
        let sessions = assembly.child_sessions().cloned();
        // The models a message may name: what the root agent can be bound to (its own, and each
        // model with a profile that its caps and the deployment's aliases allow).
        let models: Vec<String> = assembly
            .info()
            .iter()
            .find(|info| info.parent.is_none())
            .map(|root| {
                let mut models = root.model_choices.clone();
                models.push(root.model_alias.clone());
                models
            })
            .unwrap_or_default();
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

        // The folder's schedules are stepped beside the agent by its scheduler (ADR 0060), as the
        // binaries do; it is registered whatever the folder holds, so a scheduler run a folder
        // dropped is still stepped to its cancel.
        let scheduler = adam_schedule::SchedulerAgent::new(&name);
        let lister = store.clone();
        let (runtime, events) = runtime_with_ring(
            assembly.register(Runtime::builder(store)).agent(scheduler),
            &limits,
        );
        // Make the store's scheduler runs match the folder before the worker starts: a schedule
        // added is started, one changed is replaced, one dropped is cancelled.
        adam_schedule::reconcile(&runtime, &name, &schedules)
            .await
            .map_err(EngineError::Schedules)?;
        // The backend is given the same ring, for the A2A path that streams a task
        // (`TaskBackend::subscribe`), whose file parts are built from a live event's bytes. The engine
        // never calls that path: an artifact in this ring has its bytes withheld, so a caller that
        // streams a task through this backend must read files from the store.
        // The folder's commands (`commands/v1`): a message whose text is `/name args` is expanded
        // from `commands/<name>.md` before it is stored, as on the server surfaces.
        let backend = RuntimeTaskBackend::new(runtime.clone(), events.clone(), name.clone())
            .with_commands(Commands::from_manifest(&commands))
            .with_inbound(inbound);
        // The helpers a person may start a session with (`child-sessions/v1`), as `adam-agent`
        // serves them: every local subagent of the root, under the effective limits.
        let backend = match &sessions {
            Some(sessions) => backend.with_child_sessions(sessions.clone()),
            None => backend,
        };
        let reader = runtime.clone();

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
        // The engine's caller activates `child-sessions/v1` when the agent has helpers: a message to
        // a session's conversation, and the listing of its rounds, are refused without it.
        let extensions: &[&str] = if sessions.is_some() {
            &[CHILD_SESSIONS_EXTENSION]
        } else {
            &[]
        };
        let caller = Caller::new(SUBJECT).with_extensions(extensions.iter().copied());
        let steering = Caller::new(SUBJECT)
            .with_extensions(extensions.iter().copied().chain([STEER_EXTENSION]));
        let helpers = sessions
            .iter()
            .flat_map(|s| s.agents())
            .map(|a| SessionHelper {
                name: a.name.clone(),
                description: a.description.clone(),
            })
            .collect();
        let helper_agents = sessions
            .iter()
            .flat_map(|s| s.agents())
            .map(|a| a.agent.clone())
            .collect();
        Ok(Engine {
            shared: Arc::new(Shared {
                models,
                steering,
                helpers,
                helper_agents,
                caller,
                backend,
                runtime: reader,
                store: lister,
                events,
                agent: name,
                commands,
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
        .lease_ttl(limits.lease_ttl)
        .retry(limits.retry);
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
    /// The aliases a [`Say::model`] may name: the root agent's own and the models it can be bound
    /// to (they have a profile).
    models: Vec<String>,
    /// What [`Engine::file`] reads a file's bytes from, once the backend has said the run is this
    /// engine's caller's.
    runtime: Runtime,
    /// What [`Engine::firings`] lists the schedule's runs from: the host's store.
    store: DynStore,
    /// The ring every reader of [`Engine::events`] subscribes to; the runtime emits into it through
    /// [`EngineSink`], which takes a file's bytes out.
    events: BroadcastSink,
    /// Every call but a steer.
    caller: Caller,
    /// The same subject, with `steer/v1` activated: a message for a running run is delivered.
    steering: Caller,
    agent: String,
    /// The agent's commands, from its `commands/`: what [`Engine::commands`] lists.
    commands: Vec<Command>,
    /// The helpers a person may start a session with: what [`Engine::helpers`] lists.
    helpers: Vec<SessionHelper>,
    /// Their names as registered on the runtime (`<root>/<helper>`), in the same order: what a
    /// round's id is derived from.
    helper_agents: Vec<String>,
    /// Held shared by every message for the length of its call, and exclusively by `shutdown` while
    /// it sets `stopping`: a message is either written before the worker is told to stop, or
    /// refused.
    writes: RwLock<()>,
    stopping: AtomicBool,
    worker: Worker,
}

impl Shared {
    /// Refuse a message that names a model for a run that has already switched to its fallback: the
    /// run continues on the fallback for good, so the pick would change nothing, and is told so
    /// instead of being ignored. A message that names none reaches the run as always.
    async fn refuse_pick_after_switch(&self, run: RunId, say: &Say) -> Result<(), EngineError> {
        if say.model.is_none() {
            return Ok(());
        }
        let Ok(Some(state)) = self.runtime.state(run).await else {
            return Ok(());
        };
        let Ok(state) = serde_json::from_value::<adam_llm_agent::Conversation>(state) else {
            return Ok(());
        };
        match state.model_switch {
            Some(switch) => Err(EngineError::Refused(format!(
                "run {run} was switched from `{}` to its fallback `{}` and continues on it, so a \
                 message cannot pick another model; the next run of the conversation starts on \
                 the agent's own model again",
                switch.from, switch.to
            ))),
            None => Ok(()),
        }
    }

    /// Refuse a message that names a model with no profile.
    fn allows(&self, say: &Say) -> Result<(), EngineError> {
        match say.model.as_deref() {
            Some(alias) if !self.models.iter().any(|m| m == alias) => {
                Err(EngineError::Invalid(format!(
                    "the message names the model `{alias}`, which the agent cannot be bound to (it \
                     needs a model profile it can resolve, `EngineBuilder::models`); allowed: {}",
                    self.models.join(", ")
                )))
            }
            _ => Ok(()),
        }
    }
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
            profiles: ModelProfiles::default(),
            reasoning_effort: None,
            tools: ToolSet::new(),
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
    /// for a conversation this engine has no run in, [`EngineError::Refused`] for a child session
    /// that refuses it (closed, at its rounds, or its rounds already working), [`EngineError::Stopped`]
    /// after [`shutdown`](Self::shutdown), and the store's failures.
    pub async fn send(
        &self,
        conversation: Option<&ConversationId>,
        say: Say,
    ) -> Result<Turn, EngineError> {
        let _writing = self.writable().await?;
        say.check()?;
        self.shared.allows(&say)?;
        let shared = &self.shared;
        let context = conversation.map(|c| c.as_str().to_owned());
        let session = conversation.filter(|c| is_session(c));
        // The runs this message may start: at the root, a run of the agent; in a child session, a
        // round of its helper (one of the helpers: the session's is not known before it is read).
        // The backend derives each the same way, so a repeat finds what its first attempt started.
        let owns: Vec<RunId> = match session {
            Some(session) => shared
                .helper_agents
                .iter()
                .map(|helper| task_id_for(helper, SUBJECT, Some(session.as_str()), &say.id))
                .collect(),
            None => vec![task_id_for(
                &shared.agent,
                SUBJECT,
                context.as_deref(),
                &say.id,
            )],
        };
        for own in &owns {
            if let Some(info) = self.get(*own).await? {
                return Ok(Turn {
                    conversation: info.conversation.clone(),
                    run: *own,
                    delivered: Delivered::Repeated,
                    info,
                });
            }
        }
        let mut message = message(&say);
        let latest = match conversation {
            Some(conversation) => {
                let latest = self.latest_run(conversation).await?;
                // Continued once it has ended; while it is open, the backend delivers to it.
                message.reference_task_ids = Some(vec![latest.run.to_string()]);
                if matches!(latest.state, RunState::Working | RunState::Waiting) {
                    shared.refuse_pick_after_switch(latest.run, &say).await?;
                }
                Some(latest)
            }
            None => None,
        };
        let task = shared
            .backend
            .submit(shared.caller.clone(), message, None, context)
            .await
            .map_err(|e| match e {
                // A child session's limits or state (closed, at its rounds, its rounds working).
                BackendError::UnsupportedOperation(why) if session.is_some() => {
                    EngineError::Refused(why)
                }
                e => EngineError::of_backend(e, latest.as_ref().map(|l| l.run)),
            })?;
        let info = RunInfo::of(&task)?;
        let delivered = match &latest {
            _ if !owns.contains(&info.run) => match latest {
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
        self.shared.allows(&say)?;
        self.shared.refuse_pick_after_switch(run, &say).await?;
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
        self.shared.allows(&say)?;
        self.shared.refuse_pick_after_switch(run, &say).await?;
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
            Err(BackendError::TaskNotFound(_)) => self.cancel_firing(run).await,
            Err(e) => Err(EngineError::of_backend(e, Some(run))),
        }
    }

    /// [`cancel`](Self::cancel) of a run a schedule fired, which the backend does not reach: as
    /// the backend cancels a caller's run, through the runtime, then read again.
    async fn cancel_firing(&self, run: RunId) -> Result<RunInfo, EngineError> {
        let gone = || EngineError::NotFound(format!("run {run}"));
        let (view, _) = self.firing_view(run).await?.ok_or_else(gone)?;
        if !view.status.is_terminal() {
            self.shared
                .runtime
                .cancel(run, FIRING_CANCEL_REASON)
                .await
                .map_err(|e| EngineError::of_run_read(e, run))?;
        }
        let info = self.get(run).await?.ok_or_else(gone)?;
        match info.state {
            RunState::Cancelled => Ok(info),
            state => Err(EngineError::by_state(run, Ok(Some(state)), Expected::Open)),
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

    /// The files the agent handed over in `run` (its file artifacts), in the order they were
    /// emitted, without their bytes. An event withholds a file's bytes too; this list is what a
    /// host that missed the event (or reads the run later) finds them by.
    ///
    /// # Errors
    ///
    /// [`EngineError::NotFound`] when this engine has no such run, and the store's failures.
    pub async fn files(&self, run: RunId) -> Result<Vec<RunFile>, EngineError> {
        Ok(self
            .file_refs(run)
            .await?
            .into_iter()
            .map(|(file, _)| file)
            .collect())
    }

    /// The bytes of the file of `run` whose SHA-256 is `sha256` (a [`RunFile`]'s, or the
    /// [`WithheldFile`](crate::WithheldFile) an event says). `None` when the run has no such
    /// file, or its bytes are gone from the store.
    ///
    /// # Errors
    ///
    /// [`EngineError::NotFound`] when this engine has no such run, and the store's failures.
    pub async fn file(&self, run: RunId, sha256: &str) -> Result<Option<Arc<[u8]>>, EngineError> {
        let Some((_, file)) = self
            .file_refs(run)
            .await?
            .into_iter()
            .find(|(file, _)| file.sha256 == sha256)
        else {
            return Ok(None);
        };
        self.shared
            .runtime
            .artifact_bytes(&file)
            .await
            .map_err(EngineError::of_read)
    }

    /// The file artifacts of `run`, if this engine shows it (the caller's, or a firing of its
    /// schedules, as for `get`): one read of the run, and no file's bytes.
    async fn file_refs(&self, run: RunId) -> Result<Vec<(RunFile, FileRef)>, EngineError> {
        let shared = &self.shared;
        let owned = shared
            .backend
            .owned_view(&shared.caller, &run.to_string())
            .await
            .map_err(|e| EngineError::of_backend(e, Some(run)))?;
        let view = match owned {
            Some(view) => view,
            None => {
                self.firing_view(run)
                    .await?
                    .ok_or_else(|| EngineError::NotFound(format!("run {run}")))?
                    .0
            }
        };
        Ok(view
            .artifacts
            .into_iter()
            .filter_map(|artifact| {
                let file = artifact.file?;
                let listed = RunFile {
                    name: artifact.name,
                    mime_type: artifact.mime_type,
                    filename: file.filename.clone(),
                    len: file.size,
                    sha256: file.digest().as_str().to_owned(),
                };
                Some((listed, file))
            })
            .collect())
    }

    /// The helpers a person may start a child session with (`child-sessions/v1`, ADR 0045): every
    /// local subagent of the root, with its description. Empty when the agent has none.
    pub fn helpers(&self) -> &[SessionHelper] {
        &self.shared.helpers
    }

    /// Start a child session of `start.helper` in `conversation`, a conversation of this engine,
    /// with `say` as its first message: its first round is the run [`Turn`] names, in the
    /// session's own conversation ([`Turn::conversation`]). Talk to the session with
    /// [`send`](Self::send) to that conversation (each message to an idle session is its next
    /// round, one to a working round is delivered into it), list its rounds with
    /// [`list`](Self::list), and close it with [`close_session`](Self::close_session). The parent
    /// conversation takes no turn.
    ///
    /// # Errors
    ///
    /// [`EngineError::Invalid`] when the agent has no helper of that name, the description is
    /// blank, longer than 80 characters or not one line, `say` has no text, or the conversation is
    /// a session's own; [`EngineError::NotFound`] for a conversation this engine has no run in;
    /// [`EngineError::Refused`] for a session too many, open or working ([`Limits::sessions`]); a
    /// repeated `say` answers the session it started (`Repeated`);
    /// [`EngineError::Stopped`] after [`shutdown`](Self::shutdown); and the store's failures.
    pub async fn start_session(
        &self,
        conversation: &ConversationId,
        start: StartSession,
        say: Say,
    ) -> Result<Turn, EngineError> {
        let _writing = self.writable().await?;
        say.check()?;
        self.shared.allows(&say)?;
        let shared = &self.shared;
        if shared.helpers.is_empty() {
            return Err(EngineError::Invalid(
                "this agent has no helper to start a session with".to_owned(),
            ));
        }
        if is_session(conversation) {
            return Err(EngineError::Invalid(
                "a session's conversation holds no session: start it in the parent's".to_owned(),
            ));
        }
        // The parent conversation is one of this engine's, as for `send`.
        self.latest_run(conversation).await?;
        // A start's first round is the session itself, its id derived from the helper, the parent
        // conversation and the message: a repeat finds it.
        if let Some(position) = shared.helpers.iter().position(|h| h.name == start.helper) {
            let first = task_id_for(
                &shared.helper_agents[position],
                SUBJECT,
                Some(conversation.as_str()),
                &say.id,
            );
            if let Some(info) = self.get(first).await? {
                return Ok(Turn {
                    conversation: info.conversation.clone(),
                    run: first,
                    delivered: Delivered::Repeated,
                    info,
                });
            }
        }
        let mut message = message(&say);
        message.metadata.get_or_insert_default().insert(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            serde_json::json!({ "start": {
                "agent": start.helper,
                "description": start.description,
                "keepOpen": start.keep_open,
            }}),
        );
        let task = shared
            .backend
            .submit(
                shared.caller.clone(),
                message,
                None,
                Some(conversation.as_str().to_owned()),
            )
            .await
            .map_err(|e| match e {
                BackendError::UnsupportedOperation(why) => EngineError::Refused(why),
                e => EngineError::of_backend(e, None),
            })?;
        let info = RunInfo::of(&task)?;
        Ok(Turn {
            conversation: info.conversation.clone(),
            run: info.run,
            delivered: Delivered::Started,
            info,
        })
    }

    /// The child sessions of `conversation`: the open ones first, then the most recently closed
    /// (at most 16 of those), as the parent task of the conversation says them.
    ///
    /// # Errors
    ///
    /// [`EngineError::Invalid`] for a session's own conversation, [`EngineError::NotFound`] for a
    /// conversation this engine has not, [`EngineError::Unavailable`] when the sessions could not
    /// be listed (it never answers "none" then), and the store's failures.
    pub async fn sessions(
        &self,
        conversation: &ConversationId,
    ) -> Result<Vec<SessionInfo>, EngineError> {
        if is_session(conversation) {
            return Err(EngineError::Invalid(
                "a session's conversation holds no session".to_owned(),
            ));
        }
        if self.shared.helpers.is_empty() {
            self.latest_run(conversation).await?;
            return Ok(Vec::new());
        }
        let latest = self.latest_run(conversation).await?;
        let shared = &self.shared;
        let task = shared
            .backend
            .get(&shared.caller, &latest.run.to_string())
            .await
            .map_err(|e| EngineError::of_backend(e, Some(latest.run)))?
            .ok_or_else(|| EngineError::NotFound(format!("conversation {conversation}")))?;
        let listed = task
            .metadata
            .as_ref()
            .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
            .and_then(|entry| entry.get("sessions"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| EngineError::Unavailable {
                message: "the sessions of the conversation could not be listed".to_owned(),
                source: None,
            })?;
        listed.iter().map(SessionInfo::of).collect()
    }

    /// Close the child session whose conversation is `session` ([`SessionInfo::conversation`]): an
    /// idle session closes by a round of its own, a working one when its round ends, or at once
    /// with `cancel` (its round is cancelled first). The answer is the round that closes it, or
    /// the working round. A closed session takes no more messages.
    ///
    /// # Errors
    ///
    /// [`EngineError::Invalid`] for a conversation that is not a session's (or no helper at all),
    /// [`EngineError::Stopped`] after [`shutdown`](Self::shutdown), and the store's failures.
    /// Closing a closed session answers its latest round.
    pub async fn close_session(
        &self,
        session: &ConversationId,
        cancel: bool,
    ) -> Result<RunInfo, EngineError> {
        let _writing = self.writable().await?;
        let shared = &self.shared;
        // Only a session's conversation: on any other, the close would be read as a message.
        if shared.helpers.is_empty() || !is_session(session) {
            return Err(EngineError::Invalid(format!(
                "{session} is not a child session's conversation"
            )));
        }
        // The close carries no text of the person's: the backend's closing round is its own.
        let mut message = message(&Say::new("close the session"));
        message.metadata = Some(std::collections::HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            serde_json::json!({ "close": { "cancel": cancel } }),
        )]));
        let task = shared
            .backend
            .submit(
                shared.caller.clone(),
                message,
                None,
                Some(session.as_str().to_owned()),
            )
            .await
            .map_err(|e| match e {
                BackendError::UnsupportedOperation(why) => EngineError::Refused(why),
                e => EngineError::of_backend(e, None),
            })?;
        RunInfo::of(&task)
    }

    /// The agent's commands (`commands/<name>.md`), in name order: what a person may type as
    /// `/name args`, which the engine expands from the command's template before it is stored. A
    /// host lists them in its `/` menu with their `description` and `argument_hint`, as a client
    /// of the server surfaces reads them from the card (`commands/v1`).
    pub fn commands(&self) -> &[Command] {
        &self.shared.commands
    }

    /// `run` as the store has it now, or `None` when this engine has no such run.
    ///
    /// # Errors
    ///
    /// The store's failures.
    pub async fn get(&self, run: RunId) -> Result<Option<RunInfo>, EngineError> {
        let shared = &self.shared;
        let task = shared
            .backend
            .get(&shared.caller, &run.to_string())
            .await
            .map_err(|e| EngineError::of_backend(e, Some(run)))?;
        match task {
            Some(task) => RunInfo::of(&task).map(Some),
            None => match self.firing_view(run).await? {
                Some((view, _)) => RunInfo::of(&shared.backend.unowned_task(&view)).map(Some),
                None => Ok(None),
            },
        }
    }

    /// The runs the agent's `schedules/` fired (ADR 0060), the most recently updated first, each
    /// with the schedule and the due time that fired it. A firing is nobody's message, so it is in
    /// no [`list`](Self::list); [`get`](Self::get), [`events`](Self::events),
    /// [`cancel`](Self::cancel), [`files`](Self::files) and [`file`](Self::file) reach it, while
    /// [`steer`](Self::steer) and [`answer`](Self::answer) do not (nobody answers a firing).
    ///
    /// # Errors
    ///
    /// [`EngineError::Invalid`] for a page token of another query, and the store's failures.
    pub async fn firings(&self, query: FiringQuery) -> Result<Page<Firing>, EngineError> {
        let shared = &self.shared;
        let agent = &shared.agent;
        let prefix = match &query.schedule {
            // The scheduler's conversation, then `@`: only that schedule's firings, never one of a
            // schedule whose name starts with the same text (names are escaped, `@` among them).
            Some(name) => format!("{}@", adam_schedule::schedule_conversation_id(agent, name)),
            None => format!("{SCHEDULE_CONVERSATION_PREFIX}{agent}/"),
        };
        let size = query.resolved_page_size();
        let filter = query.schedule.as_deref().unwrap_or("");
        let mut runs = RunQuery::new(agent.clone(), ConversationScope::Prefix(prefix), size + 1);
        if let Some(token) = &query.page_token {
            runs.after = Some(firing_token::read(token, filter)?);
        }
        let total = shared
            .store
            .count_runs(&runs)
            .await
            .map_err(EngineError::Store)?;
        let mut records = shared
            .store
            .list_runs(&runs)
            .await
            .map_err(EngineError::Store)?;
        let more = records.len() > size;
        records.truncate(size);
        let next_page_token = more
            .then(|| records.last())
            .flatten()
            .map(|last| firing_token::write(filter, last.updated_at, last.id));
        let mut items = Vec::with_capacity(records.len());
        for record in records {
            let Some(trigger) = record
                .conversation_id
                .as_deref()
                .and_then(adam_schedule::trigger_of)
            else {
                continue;
            };
            // The record holds the state a view is built from, as `list` builds it: the lease is
            // read only where it can still say `working`, a runnable run at version 1, so a held
            // firing is `Working` as `get` says.
            let lease = if record.status == RunStatus::Runnable && record.version <= 1 {
                shared
                    .store
                    .lease_until(record.id)
                    .await
                    .map_err(EngineError::Store)?
            } else {
                None
            };
            let claimed = adam_core::claimed(record.status, lease, shared.runtime.now());
            let id = record.id;
            let view = match adam_runtime::view_from_record(record, claimed) {
                Ok(view) => view,
                Err(RuntimeError::Corrupt { .. }) => {
                    tracing::warn!(run = %id, "a firing has an unreadable state; it is not listed");
                    continue;
                }
                Err(e) => return Err(EngineError::of_run_read(e, id)),
            };
            let info = RunInfo::of(&shared.backend.unowned_task(&view))?;
            items.push(Firing { info, trigger });
        }
        Ok(Page {
            items,
            next_page_token,
            total: usize::try_from(total).unwrap_or(usize::MAX),
        })
    }

    /// `run` as the runtime has it, when it is a run of this engine's agent that one of its
    /// schedules fired: the one kind of run beyond the caller's that the engine shows its host
    /// (#289). A run of another agent, a scheduler's, or a conversation of a caller is `None`.
    async fn firing_view(
        &self,
        run: RunId,
    ) -> Result<Option<(RunView, adam_schedule::Trigger)>, EngineError> {
        let shared = &self.shared;
        let Some(view) = shared
            .runtime
            .view(run)
            .await
            .map_err(|e| EngineError::of_run_read(e, run))?
        else {
            return Ok(None);
        };
        if view.agent != shared.agent {
            return Ok(None);
        }
        let trigger = view
            .conversation_id
            .as_deref()
            .and_then(adam_schedule::trigger_of)
            .filter(|t| t.agent == shared.agent);
        Ok(trigger.map(|t| (view, t)))
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

/// Whether `conversation` is a child session's own (`<parent>/s/<session>`).
fn is_session(conversation: &ConversationId) -> bool {
    adam_runtime::parse_session_conversation_id(conversation.as_str()).is_some()
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

/// The metadata key a [`Say::model`] travels under, and the context key the agent reads
/// ([`adam_llm_agent::MODEL_CONTEXT_KEY`]).
const MODEL_KEY: &str = adam_llm_agent::MODEL_CONTEXT_KEY;

/// How the engine's backend reads a message: as [`default_inbound`] does, and the model the message
/// names (its metadata under [`MODEL_KEY`]) goes into the run's context. Only this engine reads it
/// there: a served agent's front does not, so a remote caller cannot pick a run's model.
fn inbound(message: &Message) -> Result<adam_runtime::Inbound, String> {
    let mut inbound = default_inbound(message)?;
    let alias = message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(MODEL_KEY))
        .and_then(|entry| entry.get("model"))
        .and_then(serde_json::Value::as_str);
    if let (Some(alias), Some(payload)) = (alias, inbound.payload.as_object_mut()) {
        payload.insert(
            "context".to_owned(),
            serde_json::json!({ MODEL_KEY: { "model": alias } }),
        );
    }
    Ok(inbound)
}

/// `say` as the message the backend reads: its text, under its id, and the model it names.
fn message(say: &Say) -> Message {
    let mut message = Message::new(Role::User, vec![Part::text(say.text.clone())]);
    message.message_id = say.id.clone();
    if let Some(alias) = &say.model {
        message.metadata = Some(std::collections::HashMap::from([(
            MODEL_KEY.to_owned(),
            serde_json::json!({ "model": alias }),
        )]));
    }
    message
}

#[cfg(test)]
mod tests {
    use adam_core::{MemoryStore, RunStatus};
    use adam_runtime::{Agent, AgentError, Artifact, Ctx, Inbound, RunEvent, Transition};
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;

    /// The page token of `firings` reads back what it wrote: a timestamp to the nanosecond, a
    /// schedule name with `|` in it, and it refuses a token of another filter or a malformed one.
    #[test]
    fn a_firing_page_token_reads_back_and_refuses_another_query() {
        let at: adam_core::time::Timestamp = "2026-11-21T09:00:00.123456789Z".parse().unwrap();
        let run = RunId::new();
        for filter in ["", "daily", "a|b|c"] {
            let token = firing_token::write(filter, at, run);
            assert_eq!(firing_token::read(&token, filter).unwrap(), (at, run));
            let other = firing_token::read(&token, "weekly").unwrap_err();
            assert!(matches!(other, EngineError::Invalid(_)), "{other:?}");
        }
        for bad in ["", "x|y|", "2026-11-21T09:00:00Z|not-a-uuid|"] {
            assert!(matches!(
                firing_token::read(bad, ""),
                Err(EngineError::Invalid(_))
            ));
        }
    }
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
