//! [`RuntimeTaskBackend`]: the A2A [`TaskBackend`] over an `adam_runtime::Runtime`.

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Task, TaskState};
use adam_a2a::{BackendError, Caller, TaskBackend, TaskEvent};
use adam_core::{RunId, StoreError};
use adam_error::ErrorClass;
use adam_runtime::{AgentError, BroadcastSink, Classify, RunView, Runtime, RuntimeError};
use async_trait::async_trait;
use futures::stream::BoxStream;
use uuid::Uuid;

use crate::convert::{
    InboundFn, PromptFn, decode_conversation, default_inbound, default_prompt, encode_conversation,
    task_from_view,
};
use crate::ids::task_id_for;
use crate::subscribe;

/// Default interval at which a subscription re-reads the durable run.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Reason recorded on a run cancelled through A2A.
const CANCEL_REASON: &str = "canceled by client";

/// How often a submission retries when the conversation's open task finishes
/// under it.
const MAX_JOIN_ATTEMPTS: usize = 5;

/// A task is a run: `task_id` is the run id, `context_id` the conversation
/// (namespaced by the caller, see below).
///
/// # Mapping
///
/// | A2A | Runtime |
/// |---|---|
/// | `SendMessage` (new) | `Runtime::start_with_id` with [`task_id_for`], conversation `<subject>:<context id>` |
/// | `SendMessage` with `taskId` | `Runtime::deliver` (only while `input-required`) |
/// | `input-required` | run parked with no timer (`RunView::waiting`); the question comes from [`PromptFn`] |
/// | `completed` | `Done`; `output.text` is the status message, `RunView::artifacts` are the task artifacts |
/// | `failed` | `Failed`; the error is the status message |
/// | `canceled` | `Failed` with `cancelled: ...` (what `Runtime::cancel` writes) |
/// | `CancelTask` | `Runtime::cancel` |
///
/// # Idempotent submission
///
/// A new task's id is derived from the caller, the `contextId` and the
/// message's `messageId` ([`task_id_for`]), so a client that repeats a
/// `SendMessage` (a retry after a crash or a timeout) gets the task the first
/// attempt made, and the agent reads the input once. This holds for a request
/// that started a task; a message that was delivered to an already open task
/// (below) and a follow-up to a `taskId` are not recognised on a repeat: the
/// follow-up is refused as the task is no longer `input-required`, the
/// delivery to an open task is delivered again (the runtime keeps no record of
/// consumed inbound ids).
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
/// still works). The runtime must have `agent` registered, either as an
/// agent (`RuntimeBuilder::agent`) or as a start-only starter
/// (`RuntimeBuilder::starter`): the backend only starts, delivers to, reads and
/// cancels runs, and never steps one. Workers are run by the caller
/// (`Runtime::run_worker`); a process that registered only the starter has
/// nothing to step, so a worker with the full agent has to run elsewhere.
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

    /// A new-task submission: `(run id, context id)` of the task that took the
    /// message.
    ///
    /// The run id is [`task_id_for`], so a repeat of the same request (same
    /// caller, `contextId` and `messageId`) finds the run its first attempt
    /// created and starts nothing. If the conversation already has an open task
    /// the message is delivered to it, as `Runtime::start` does.
    async fn start_or_join(
        &self,
        caller: &Caller,
        message: &Message,
        inbound: adam_runtime::Inbound,
        context_id: Option<String>,
    ) -> Result<(RunId, String), BackendError> {
        // Without a message id there is nothing to recognise a repeat by.
        if message.message_id.is_empty() {
            let context = context_id.unwrap_or_else(a2a::new_context_id);
            let conversation = encode_conversation(&caller.subject, &context);
            let run = self
                .runtime
                .start(&self.agent, inbound, Some(&conversation))
                .await
                .map_err(map_err)?;
            return Ok((run, context));
        }
        let run = task_id_for(
            &self.agent,
            &caller.subject,
            context_id.as_deref(),
            &message.message_id,
        );
        let context = context_id.clone().unwrap_or_else(a2a::new_context_id);
        let conversation = encode_conversation(&caller.subject, &context);
        for _ in 0..MAX_JOIN_ATTEMPTS {
            match self
                .runtime
                .start_with_id(run, &self.agent, inbound.clone(), Some(&conversation))
                .await
            {
                Ok(true) => return Ok((run, context)),
                Ok(false) => {
                    // A repeat: the task exists, and its context is the one it
                    // was created with (a request without `contextId` got a
                    // generated one the first time).
                    let (_, _, context) =
                        self.owned(caller, &run.to_string()).await?.ok_or_else(|| {
                            BackendError::internal("a repeated submission found a foreign task")
                        })?;
                    return Ok((run, context));
                }
                Err(RuntimeError::ConversationBusy { .. }) => {
                    let open = self
                        .runtime
                        .store()
                        .open_run_for_conversation(&self.agent, &conversation)
                        .await
                        .map_err(|e| map_err(e.into()))?;
                    let Some(open) = open else { continue };
                    match self.runtime.deliver(open.id, inbound.clone()).await {
                        Ok(()) => return Ok((open.id, context)),
                        // It finished meanwhile: the context takes a new task.
                        Err(RuntimeError::Finished { .. } | RuntimeError::NotFound(_)) => {}
                        Err(e) => return Err(map_err(e)),
                    }
                }
                Err(e) => return Err(map_err(e)),
            }
        }
        Err(BackendError::unavailable("the conversation is contended"))
    }

    async fn view_of(&self, run: RunId) -> Result<RunView, BackendError> {
        self.runtime
            .view(run)
            .await
            .map_err(map_err)?
            .ok_or_else(|| BackendError::TaskNotFound(run.to_string()))
    }
}

/// Map a runtime failure to the backend seam's error by its [`ErrorClass`], keeping the runtime
/// error as the source. The A2A server logs the whole chain and sends the client only what this
/// function chose to say.
///
/// | class | backend error |
/// |---|---|
/// | `NotFound` | `TaskNotFound` |
/// | `Invalid`, `Rejected` | `InvalidParams` (the agent rejected the request, or the task's state forbids it) |
/// | `Transient`, `RateLimited`, `Conflict` | `Unavailable` |
/// | anything else | `Internal` |
pub(crate) fn map_err(e: RuntimeError) -> BackendError {
    match e.class() {
        ErrorClass::NotFound => {
            let task = match &e {
                RuntimeError::NotFound(run) | RuntimeError::Store(StoreError::NotFound(run)) => {
                    run.to_string()
                }
                _ => "unknown".to_owned(),
            };
            BackendError::TaskNotFound(task)
        }
        ErrorClass::Invalid | ErrorClass::Rejected => {
            BackendError::InvalidParams(client_detail(&e))
        }
        ErrorClass::Transient | ErrorClass::RateLimited | ErrorClass::Conflict => {
            BackendError::unavailable("the task runtime is unavailable").with_source(e)
        }
        _ => BackendError::internal("the task runtime failed").with_source(e),
    }
}

/// What a client may be told about a request the runtime refused: the agent's own message about
/// the request, the state that forbids it, or a fixed sentence. Never a transport or driver text.
fn client_detail(e: &RuntimeError) -> String {
    match e {
        RuntimeError::Finished { run, status } => format!("task {run} is already {status}"),
        RuntimeError::ConversationBusy { .. } => "the conversation already has an open task".into(),
        RuntimeError::Agent(AgentError::Permanent { message, .. }) => message.clone(),
        RuntimeError::Store(StoreError::InvalidInput(message)) => message.clone(),
        _ => "the request was rejected".into(),
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
            let (run, context) = self
                .start_or_join(&caller, &message, inbound, context_id)
                .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lower() -> std::io::Error {
        std::io::Error::other("connection reset by peer")
    }

    fn run() -> RunId {
        RunId(Uuid::nil())
    }

    /// The A2A error a client is told, by kind.
    #[derive(Debug, Clone, Copy)]
    enum Told {
        TaskNotFound,
        InvalidParams,
        Unavailable,
        Internal,
    }

    /// One row per class the runtime can report: the error and what the client is told. The
    /// class itself is only printed on failure; the runtime's own tests pin it.
    #[test]
    fn runtime_errors_map_by_class() {
        use adam_core::store::StoreError as S;
        let cases: Vec<(RuntimeError, Told)> = vec![
            (RuntimeError::NotFound(run()), Told::TaskNotFound),
            (RuntimeError::Store(S::NotFound(run())), Told::TaskNotFound),
            (
                RuntimeError::Finished {
                    run: run(),
                    status: adam_core::RunStatus::Done,
                },
                Told::InvalidParams,
            ),
            (
                RuntimeError::ConversationBusy {
                    agent: "a".into(),
                    conversation_id: "secret-subject:ctx".into(),
                },
                Told::InvalidParams,
            ),
            (
                RuntimeError::Agent(AgentError::permanent("unusable start message: empty")),
                Told::InvalidParams,
            ),
            (
                RuntimeError::Store(S::InvalidInput("no NUL please".into())),
                Told::InvalidParams,
            ),
            (RuntimeError::UnknownAgent("x".into()), Told::InvalidParams),
            (RuntimeError::Contended("run x".into()), Told::Unavailable),
            (
                RuntimeError::Store(S::unavailable(lower())),
                Told::Unavailable,
            ),
            (
                RuntimeError::Agent(AgentError::transient_after(
                    "slow down",
                    Duration::from_secs(3),
                )),
                Told::Unavailable,
            ),
            (
                RuntimeError::Corrupt {
                    run: run(),
                    reason: "x".into(),
                    source: None,
                },
                Told::Internal,
            ),
            (RuntimeError::Store(S::internal(lower())), Told::Internal),
        ];
        for (e, told) in cases {
            let shown = e.to_string();
            let class = e.class();
            let mapped = map_err(e);
            let ok = matches!(
                (told, &mapped),
                (Told::TaskNotFound, BackendError::TaskNotFound(_))
                    | (Told::InvalidParams, BackendError::InvalidParams(_))
                    | (Told::Unavailable, BackendError::Unavailable { .. })
                    | (Told::Internal, BackendError::Internal { .. })
            );
            assert!(
                ok,
                "{shown} ({class:?}) mapped to {mapped:?}, expected {told:?}"
            );
        }
    }

    #[test]
    fn what_a_client_is_told_carries_no_cause_and_no_conversation_id() {
        let busy = map_err(RuntimeError::ConversationBusy {
            agent: "a".into(),
            conversation_id: "secret-subject:ctx".into(),
        });
        let told = a2a::A2AError::from(busy);
        assert!(!told.message.contains("secret-subject"), "{}", told.message);

        let down = map_err(RuntimeError::Store(StoreError::unavailable(lower())));
        assert!(std::error::Error::source(&down).is_some());
        let told = a2a::A2AError::from(down);
        assert_eq!(told.code, -32603);
        assert_eq!(told.message, "backend temporarily unavailable");
        assert!(!told.message.contains("connection reset"));
    }
}
