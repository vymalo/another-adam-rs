//! The backend seam: what the A2A server needs from the runtime behind it.
//!
//! [`TaskBackend`] is deliberately small and knows nothing about HTTP or
//! JSON-RPC. Wave 2 implements it on top of the durable runtime; until then
//! [`InMemoryBackend`](crate::InMemoryBackend) (feature `test-util`) is the
//! reference implementation of the semantics documented here.

use std::sync::Arc;

use a2a::{Message, Task, TaskArtifactUpdateEvent, TaskState, TaskStatusUpdateEvent};
use async_trait::async_trait;
use futures::stream::BoxStream;

/// The authenticated principal on whose behalf a request runs.
///
/// Built by the server's auth layer, never from client-supplied data.
/// Backends use it to scope tasks to their owner.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Caller {
    /// A stable, non-secret identifier: `token-<index>` for the matched bearer
    /// token, or `"anonymous"` under [`AuthConfig::AllowAnonymous`](crate::AuthConfig).
    /// Never the token itself.
    pub subject: String,
}

impl Caller {
    /// The subject used when authentication is explicitly disabled.
    pub const ANONYMOUS: &'static str = "anonymous";

    /// A caller with the given subject.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
        }
    }

    /// The anonymous caller (local development only).
    pub fn anonymous() -> Self {
        Self::new(Self::ANONYMOUS)
    }
}

/// One item of a task's event stream, wrapping the SDK's A2A 1.0 event types.
///
/// The issue text calls this `a2a::TaskEvent`; the SDK has no such type (its
/// closest is `StreamResponse`, which also carries `Message`, a shape a task
/// stream never needs), so the crate defines it.
#[derive(Clone, Debug)]
pub enum TaskEvent {
    /// The task as of the instant the subscription attached. Always the first
    /// item of a [`TaskBackend::subscribe`] stream.
    Snapshot(Task),
    /// The task's status changed.
    Status(TaskStatusUpdateEvent),
    /// An artifact was produced or extended.
    Artifact(TaskArtifactUpdateEvent),
}

impl TaskEvent {
    /// The task state this event reports, if it reports one.
    pub fn state(&self) -> Option<&TaskState> {
        match self {
            Self::Snapshot(task) => Some(&task.status.state),
            Self::Status(update) => Some(&update.status.state),
            Self::Artifact(_) => None,
        }
    }

    /// Whether a stream must end after this event: the task reached a terminal
    /// state, or is waiting on its caller (`input-required`, `auth-required`).
    pub fn ends_stream(&self) -> bool {
        self.state().is_some_and(state_ends_stream)
    }
}

/// Whether a task in `state` has nothing more to say until someone acts:
/// terminal, or interrupted (`input-required` / `auth-required`).
pub(crate) fn state_ends_stream(state: &TaskState) -> bool {
    state.is_terminal() || matches!(state, TaskState::InputRequired | TaskState::AuthRequired)
}

/// Why a [`TaskBackend`] call failed.
///
/// Each variant maps to one A2A error code (see the `From<BackendError>` impl
/// for `a2a::A2AError`).
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    /// No such task, or it belongs to another caller (deliberately
    /// indistinguishable). A2A code `-32001`.
    #[error("task not found: {0}")]
    TaskNotFound(String),
    /// The task is in a state that cannot be canceled. A2A code `-32002`.
    #[error("task {task_id} cannot be canceled in state {state}")]
    NotCancelable {
        /// The task that was asked to cancel.
        task_id: String,
        /// Its current state, rendered for humans.
        state: String,
    },
    /// The request is well-formed JSON-RPC but semantically unacceptable, for
    /// example a follow-up to a task that is not waiting for input.
    /// JSON-RPC code `-32602`.
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// A dependency (database, worker pool) is temporarily unavailable.
    /// Retryable; surfaces as JSON-RPC `-32603` with a generic message.
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    /// Anything else. JSON-RPC code `-32603`; the detail is logged, not sent
    /// to the client.
    #[error("internal error: {0}")]
    Internal(String),
}

impl BackendError {
    /// Whether retrying the same call later may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

impl From<BackendError> for a2a::A2AError {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::TaskNotFound(id) => a2a::A2AError::task_not_found(&id),
            BackendError::NotCancelable { task_id, .. } => {
                a2a::A2AError::task_not_cancelable(&task_id)
            }
            BackendError::InvalidParams(msg) => a2a::A2AError::invalid_params(msg),
            BackendError::Unavailable(detail) => {
                tracing::error!(%detail, "backend unavailable");
                a2a::A2AError::internal("backend temporarily unavailable")
            }
            BackendError::Internal(detail) => {
                tracing::error!(%detail, "backend internal error");
                a2a::A2AError::internal("internal error")
            }
        }
    }
}

/// What the A2A server needs from whatever actually runs tasks.
///
/// # Semantics every implementation must honour
///
/// * **Ownership.** A task belongs to the [`Caller`] that created it. Another
///   caller asking for it gets [`BackendError::TaskNotFound`], never a
///   distinct "forbidden".
/// * **The backend owns the work.** A task runs independently of any HTTP
///   connection: dropping a [`subscribe`](Self::subscribe) stream must not
///   cancel it. Only [`cancel`](Self::cancel) does.
/// * **Subscriptions** start with a [`TaskEvent::Snapshot`] taken atomically
///   with attaching (so nothing between "now" and the first event is lost),
///   then carry every later status/artifact event, and end after a terminal
///   state or after the task becomes `input-required` / `auth-required`. Subscribing to a
///   task already in such a state yields just the snapshot. This must work
///   for a task started by a different process or before a restart when the
///   backend is durable: that is the whole point of the seam.
/// * **Follow-ups.** [`submit`](Self::submit) with the id of a task that is
///   `input-required` resumes it and returns it already in `working`, so an
///   immediate `subscribe` does not see the stale interrupted state. A
///   follow-up to a task in any other state is
///   [`BackendError::InvalidParams`]; an unknown id is
///   [`BackendError::TaskNotFound`].
/// * **Cancel** of an already-canceled task returns it unchanged; cancel of a
///   task in another terminal state is [`BackendError::NotCancelable`].
#[async_trait]
pub trait TaskBackend: Send + Sync + 'static {
    /// New task, or a follow-up message to an existing task/context.
    async fn submit(
        &self,
        caller: Caller,
        message: Message,
        task_id: Option<String>,
        context_id: Option<String>,
    ) -> Result<Task, BackendError>;

    /// The task, or `None` if it does not exist (or is not the caller's).
    async fn get(&self, caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError>;

    /// Cancel a task and return it in its final state.
    async fn cancel(&self, caller: &Caller, task_id: &str) -> Result<Task, BackendError>;

    /// Status and artifact updates from now on, ending after a terminal state.
    ///
    /// Not `async`: the returned stream does the I/O. An unknown task is
    /// reported as a first item of `Err(TaskNotFound)`.
    fn subscribe(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> BoxStream<'static, Result<TaskEvent, BackendError>>;
}

/// A shareable, type-erased [`TaskBackend`].
pub type DynTaskBackend = Arc<dyn TaskBackend>;
