//! [`RuntimeTaskBackend`]: the A2A [`TaskBackend`] over an `adam_runtime::Runtime`.

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Task, TaskState};
use adam_a2a::{BackendError, Caller, TaskBackend, TaskEvent};
use adam_core::RunId;
use adam_runtime::{BroadcastSink, Classify, RunView, Runtime, RuntimeError};
use async_trait::async_trait;
use futures::stream::BoxStream;
use uuid::Uuid;

use crate::convert::{
    InboundFn, PromptFn, decode_conversation, default_inbound, default_prompt, encode_conversation,
    task_from_view,
};
use crate::subscribe;

/// Default interval at which a subscription re-reads the durable run.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Reason recorded on a run cancelled through A2A.
const CANCEL_REASON: &str = "canceled by client";

/// A task is a run: `task_id` is the run id, `context_id` the conversation
/// (namespaced by the caller, see below).
///
/// # Mapping
///
/// | A2A | Runtime |
/// |---|---|
/// | `SendMessage` (new) | `Runtime::start`, conversation `<subject>:<context id>` |
/// | `SendMessage` with `taskId` | `Runtime::deliver` (only while `input-required`) |
/// | `input-required` | run parked with no timer (`RunView::waiting`); the question comes from [`PromptFn`] |
/// | `completed` | `Done`; `output.text` is the status message, `RunView::artifacts` are the task artifacts |
/// | `failed` | `Failed`; the error is the status message |
/// | `canceled` | `Failed` with `cancelled: ...` (what `Runtime::cancel` writes) |
/// | `CancelTask` | `Runtime::cancel` |
///
/// A message that carries a `contextId` but no `taskId` while that context
/// still has an open task is delivered to that task (the runtime allows one
/// open run per conversation); once it is finished the message starts a new
/// task in the same context.
///
/// # Ownership
///
/// The caller's subject is part of the run's conversation id
/// (`<subject>:<context id>`), which is durable, so ownership survives
/// restarts and needs no side table. A task owned by someone else is
/// indistinguishable from one that does not exist.
///
/// # Subscriptions survive restarts
///
/// [`subscribe`](TaskBackend::subscribe) is built from durable state: it
/// takes its snapshot from `Runtime::view` and then polls the view every
/// [`poll_interval`](Self::with_poll_interval), so it works for a task started
/// by another process or before a restart. Live events from the
/// [`BroadcastSink`] only add low latency and intermediate progress: they are
/// de-duplicated against what the durable record reports, and losing them
/// never loses a state change or an artifact.
///
/// # Wiring
///
/// The `events` sink must be the one the runtime was built with
/// (`RuntimeBuilder::event_sink`), otherwise no live events arrive (polling
/// still works). The runtime must have the agent named `agent` registered;
/// workers are run by the caller (`Runtime::run_worker`).
#[derive(Clone)]
pub struct RuntimeTaskBackend {
    pub(crate) runtime: Runtime,
    pub(crate) events: BroadcastSink,
    pub(crate) agent: String,
    pub(crate) poll: Duration,
    pub(crate) prompt: PromptFn,
    pub(crate) inbound: InboundFn,
}

impl std::fmt::Debug for RuntimeTaskBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeTaskBackend")
            .field("agent", &self.agent)
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

impl RuntimeTaskBackend {
    /// A backend serving `agent` on `runtime`, with live events from `events`.
    pub fn new(runtime: Runtime, events: BroadcastSink, agent: impl Into<String>) -> Self {
        Self {
            runtime,
            events,
            agent: agent.into(),
            poll: DEFAULT_POLL_INTERVAL,
            prompt: Arc::new(default_prompt),
            inbound: Arc::new(default_inbound),
        }
    }

    /// How often a subscription re-reads the durable run (default
    /// [`DEFAULT_POLL_INTERVAL`]). This bounds the latency of a state change
    /// that no live event announced, e.g. one made by another replica.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll = interval.max(Duration::from_millis(1));
        self
    }

    /// Where the question of an `input-required` task comes from (default:
    /// [`default_prompt`](crate::default_prompt)).
    #[must_use]
    pub fn with_prompt(
        mut self,
        f: impl Fn(&RunView) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.prompt = Arc::new(f);
        self
    }

    /// How an A2A message becomes the agent's input (default:
    /// [`default_inbound`](crate::default_inbound)).
    #[must_use]
    pub fn with_inbound(
        mut self,
        f: impl Fn(&Message) -> Result<adam_runtime::Inbound, String> + Send + Sync + 'static,
    ) -> Self {
        self.inbound = Arc::new(f);
        self
    }

    /// The run behind `task_id` if it exists, belongs to this agent and to
    /// `caller`: `(run id, its durable view, context id)`.
    pub(crate) async fn owned(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> Result<Option<(RunId, RunView, String)>, BackendError> {
        let Ok(uuid) = Uuid::parse_str(task_id) else {
            return Ok(None);
        };
        let run = RunId(uuid);
        let Some(view) = self.runtime.view(run).await.map_err(map_err)? else {
            return Ok(None);
        };
        Ok(self
            .ownership(caller, view)
            .map(|(view, ctx)| (run, view, ctx)))
    }

    /// `Some((view, context id))` if the run is this agent's and the caller's.
    pub(crate) fn ownership(&self, caller: &Caller, view: RunView) -> Option<(RunView, String)> {
        if view.agent != self.agent {
            return None;
        }
        let (subject, context) = decode_conversation(view.conversation_id.as_deref()?)?;
        (subject == caller.subject).then_some((view, context))
    }

    async fn view_of(&self, run: RunId) -> Result<RunView, BackendError> {
        self.runtime
            .view(run)
            .await
            .map_err(map_err)?
            .ok_or_else(|| BackendError::TaskNotFound(run.to_string()))
    }
}

/// Map a runtime failure to the backend seam's error.
pub(crate) fn map_err(e: RuntimeError) -> BackendError {
    match e {
        RuntimeError::NotFound(run) => BackendError::TaskNotFound(run.to_string()),
        RuntimeError::Finished { run, status } => {
            BackendError::InvalidParams(format!("task {run} is already {status}"))
        }
        e if e.is_retryable() => BackendError::unavailable(e.to_string()),
        e => BackendError::internal(e.to_string()),
    }
}

#[async_trait]
impl TaskBackend for RuntimeTaskBackend {
    #[tracing::instrument(skip_all, fields(subject = %caller.subject))]
    async fn submit(
        &self,
        caller: Caller,
        message: Message,
        task_id: Option<String>,
        context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        let inbound = (self.inbound)(&message).map_err(BackendError::InvalidParams)?;

        let Some(task_id) = task_id else {
            let context = context_id.unwrap_or_else(a2a::new_context_id);
            let conversation = encode_conversation(&caller.subject, &context);
            let run = self
                .runtime
                .start(&self.agent, inbound, Some(&conversation))
                .await
                .map_err(map_err)?;
            let view = self.view_of(run).await?;
            let mut task = task_from_view(&view, &context, &self.prompt);
            let mut first = message;
            first.task_id = Some(task.id.clone());
            first.context_id = Some(context);
            task.history = Some(vec![first]);
            return Ok(task);
        };

        let (run, view, context) = self
            .owned(&caller, &task_id)
            .await?
            .ok_or_else(|| BackendError::TaskNotFound(task_id.clone()))?;
        if context_id.is_some_and(|c| c != context) {
            return Err(BackendError::InvalidParams(
                "contextId does not match the task".to_owned(),
            ));
        }
        if !view.waiting {
            return Err(BackendError::InvalidParams(format!(
                "task {task_id} is {:?} and cannot take a follow-up",
                crate::task_state(&view)
            )));
        }
        self.runtime.deliver(run, inbound).await.map_err(map_err)?;

        // The delivery made the run runnable, so this reads `working`; make
        // that certain even if the record is read before the commit is visible.
        let view = self.view_of(run).await?;
        let mut task = task_from_view(&view, &context, &self.prompt);
        if task.status.state == TaskState::InputRequired {
            task.status.state = TaskState::Working;
            task.status.message = None;
        }
        Ok(task)
    }

    async fn get(&self, caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        Ok(self
            .owned(caller, task_id)
            .await?
            .map(|(_, view, context)| task_from_view(&view, &context, &self.prompt)))
    }

    #[tracing::instrument(skip(self, caller), fields(subject = %caller.subject))]
    async fn cancel(&self, caller: &Caller, task_id: &str) -> Result<Task, BackendError> {
        let (run, view, context) = self
            .owned(caller, task_id)
            .await?
            .ok_or_else(|| BackendError::TaskNotFound(task_id.to_owned()))?;
        let mut state = crate::task_state(&view);
        let mut view = view;
        if !state.is_terminal() {
            self.runtime
                .cancel(run, CANCEL_REASON)
                .await
                .map_err(map_err)?;
            view = self.view_of(run).await?;
            state = crate::task_state(&view);
        }
        match state {
            TaskState::Canceled => Ok(task_from_view(&view, &context, &self.prompt)),
            other => Err(BackendError::NotCancelable {
                task_id: task_id.to_owned(),
                state: format!("{other:?}"),
            }),
        }
    }

    fn subscribe(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> BoxStream<'static, Result<TaskEvent, BackendError>> {
        subscribe::subscribe(self.clone(), caller.clone(), task_id.to_owned())
    }
}
