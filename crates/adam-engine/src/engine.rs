//! [`Engine`] and [`EngineBuilder`]: the runtime and its worker on the host's tokio runtime, and
//! every call through the code the A2A server calls.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use a2a::{Message, Part, Role, Task};
use adam_a2a::{BackendError, Caller, STEER_EXTENSION, TaskBackend, TaskQuery};
use adam_a2a_runtime::{RuntimeTaskBackend, task_id_for};
use adam_assembly::AgentDef;
use adam_core::{DynSecretSource, DynStore, RunId};
use adam_llm_agent::DEFAULT_MAX_PARALLEL;
use adam_model::DynModel;
use adam_runtime::{BroadcastSink, Runtime, RuntimeError};
use adam_ui::Ui;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::{
    ConversationId, EngineError, ListQuery, Page, RunInfo, RunState, Say, Turn, default_secrets,
};

/// The one subject every call of an engine is made as: the engine serves one person, and the
/// backend scopes runs to a subject. Not `anonymous`, which the backend lets continue nothing.
const SUBJECT: &str = "adam-engine";

/// What the engine runs within. Set the fields on [`Limits::default`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Runs stepped at the same time (the runtime's `concurrency`). Default 4, at least 1.
    pub concurrency: usize,
    /// Tool calls of one model turn run at the same time, for every agent; an agent's file may
    /// lower it. Default 10, 1 to 32.
    pub max_parallel_tools: usize,
    /// How long a run stays leased to the worker without renewal. Default 30 s.
    pub lease_ttl: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_parallel_tools: DEFAULT_MAX_PARALLEL,
            lease_ttl: Duration::from_secs(30),
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

    /// Migrate the store, build the agent and start the worker on the current tokio runtime.
    ///
    /// The agent gets the screen's tools of an embedded agent (`ask_user`, `show`, `ui_catalog`,
    /// `plan_*`, from `adam_ui::Ui::embedded`) and nothing on the network: a file that names an
    /// MCP server or a remote subagent does not bind.
    ///
    /// # Errors
    ///
    /// * [`EngineError::NoRuntime`] outside a tokio runtime;
    /// * [`EngineError::Store`] when the store's migration fails;
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

        let events = BroadcastSink::default();
        let runtime = assembly
            .register(Runtime::builder(store))
            .event_sink(events.clone())
            .concurrency(limits.concurrency)
            .lease_ttl(limits.lease_ttl)
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, name.clone());

        let (stop, stopped) = oneshot::channel::<()>();
        let worker = handle.spawn(async move {
            runtime
                .run_worker(async {
                    let _ = stopped.await;
                })
                .await
        });
        let caller = Caller::new(SUBJECT);
        Ok(Engine {
            shared: Arc::new(Shared {
                steering: caller.clone().with_extensions([STEER_EXTENSION]),
                caller,
                backend,
                agent: name,
                stopping: AtomicBool::new(false),
                worker: Mutex::new(Some(Worker {
                    stop,
                    handle: worker,
                })),
            }),
        })
    }
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), RuntimeError>>,
}

struct Shared {
    backend: RuntimeTaskBackend,
    /// Every call but a steer.
    caller: Caller,
    /// The same subject, with `steer/v1` activated: a message for a running run is delivered.
    steering: Caller,
    agent: String,
    stopping: AtomicBool,
    worker: Mutex<Option<Worker>>,
}

impl Drop for Shared {
    /// The last engine is gone without a [`shutdown`](Engine::shutdown): the worker is aborted as a
    /// killed process would be, and its runs are claimed again once their leases run out.
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(worker) = worker {
            worker.handle.abort();
        }
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
    /// latest run is still open (working or waiting), the message is delivered to that run, which
    /// reads it at its next step ([`Turn::started`] is `false`).
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
        self.writable()?;
        say.check()?;
        let context = conversation.map(|c| c.as_str().to_owned());
        let mut message = message(&say);
        if let Some(conversation) = conversation {
            let latest = self.latest_run(conversation).await?;
            message.reference_task_ids = Some(vec![latest.to_string()]);
        }
        let shared = &self.shared;
        let task = shared
            .backend
            .submit(shared.caller.clone(), message, None, context.clone())
            .await
            .map_err(|e| EngineError::of_backend(e, None))?;
        let info = RunInfo::of(&task)?;
        let started = info.run == task_id_for(&shared.agent, SUBJECT, context.as_deref(), &say.id);
        Ok(Turn {
            conversation: info.conversation.clone(),
            run: info.run,
            started,
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
        self.writable()?;
        say.check()?;
        let shared = &self.shared;
        let task = shared
            .backend
            .submit(
                shared.steering.clone(),
                message(&say),
                Some(run.to_string()),
                None,
            )
            .await
            .map_err(|e| EngineError::of_backend(e, Some(run)))?;
        RunInfo::of(&task)
    }

    /// Answer the question `run` waits on ([`RunState::Waiting`]); it goes on working.
    ///
    /// # Errors
    ///
    /// [`EngineError::NotWaiting`] (class `Rejected`) for a run that is not waiting, and
    /// [`EngineError::Finished`] for one that has ended; otherwise as [`steer`](Self::steer).
    pub async fn answer(&self, run: RunId, say: Say) -> Result<RunInfo, EngineError> {
        self.writable()?;
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
            Err(BackendError::InvalidParams(_)) => {
                let state = self
                    .get(run)
                    .await?
                    .map_or(RunState::Working, |info| info.state);
                Err(EngineError::NotWaiting { run, state })
            }
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
        let task = shared
            .backend
            .cancel(&shared.caller, &run.to_string())
            .await
            .map_err(|e| EngineError::of_backend(e, Some(run)))?;
        RunInfo::of(&task)
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
    /// leases are released. Every clone stops; a later [`send`](Self::send), [`steer`](Self::steer)
    /// or [`answer`](Self::answer) is [`EngineError::Stopped`], while reads and
    /// [`cancel`](Self::cancel) still work. A second call does nothing.
    ///
    /// # Errors
    ///
    /// What the worker ended with.
    pub async fn shutdown(&self) -> Result<(), EngineError> {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let worker = self
            .shared
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(Worker { stop, handle }) = worker else {
            return Ok(());
        };
        let _ = stop.send(());
        match handle.await {
            Ok(ended) => ended.map_err(EngineError::of_runtime),
            Err(joined) => Err(EngineError::Internal {
                message: "the worker task did not end cleanly".to_owned(),
                source: Some(Box::new(joined)),
            }),
        }
    }

    fn writable(&self) -> Result<(), EngineError> {
        if self.shared.stopping.load(Ordering::SeqCst) {
            Err(EngineError::Stopped)
        } else {
            Ok(())
        }
    }

    /// The run a conversation's next one continues: the most recently updated of its runs. The
    /// backend continues it only once it has ended; an open one takes the message instead.
    async fn latest_run(&self, conversation: &ConversationId) -> Result<RunId, EngineError> {
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
            Some(task) => RunInfo::of(task).map(|info| info.run),
            None => Err(EngineError::NotFound(format!(
                "conversation {conversation}"
            ))),
        }
    }
}

/// `say` as the message the backend reads: its text, under its id.
fn message(say: &Say) -> Message {
    let mut message = Message::new(Role::User, vec![Part::text(say.text.clone())]);
    message.message_id = say.id.clone();
    message
}
