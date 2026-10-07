//! The backend seam: what the A2A server needs from the runtime behind it.
//!
//! [`TaskBackend`] is deliberately small and knows nothing about HTTP or
//! JSON-RPC. Wave 2 implements it on top of the durable runtime; until then
//! [`InMemoryBackend`](crate::InMemoryBackend) (feature `test-util`) is the
//! reference implementation of the semantics documented here.

use std::sync::Arc;

use a2a::{Message, Task, TaskArtifactUpdateEvent, TaskState, TaskStatusUpdateEvent};
use adam_error::{BoxError, Classify, ErrorClass, report};
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::page::{TaskPage, TaskQuery};

/// The authenticated principal on whose behalf a request runs, and the extensions the request
/// activated.
///
/// Built by the server's auth layer and the handler, never from client-supplied data about who
/// the caller *is*. Backends use [`subject`](Self::subject) to scope tasks to their owner; ownership
/// never looks at [`extensions`](Self::extensions), which describe the request and not the person.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Caller {
    /// A stable, non-secret identifier: `token-<index>` for the matched bearer
    /// token, or `"anonymous"` under [`AuthConfig::AllowAnonymous`](crate::AuthConfig).
    /// Never the token itself.
    pub subject: String,
    /// The URIs of the extensions this request activated: the ones the client asked for (the
    /// `A2A-Extensions` header, and `message.extensions` of a message it sends) **that the card
    /// declares**, each once, in the order the client named them. A URI the client names and the
    /// card does not declare is not here: a client cannot switch on behaviour the agent never
    /// advertised. Empty for a caller built with [`Caller::new`] and for a request that named none.
    ///
    /// The request that is being served carries it: a backend that streams a task to a client
    /// reads it from the caller of the `subscribe` call, which is the caller of that very
    /// request (a resubscribe activates for itself).
    pub extensions: Vec<String>,
}

impl Caller {
    /// The subject used when authentication is explicitly disabled.
    pub const ANONYMOUS: &'static str = "anonymous";

    /// A caller with the given subject, and no extension activated.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            extensions: Vec::new(),
        }
    }

    /// The anonymous caller (local development only).
    pub fn anonymous() -> Self {
        Self::new(Self::ANONYMOUS)
    }

    /// This caller with `extensions` activated (a test, or a backend that calls another).
    #[must_use]
    pub fn with_extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extensions = extensions.into_iter().map(Into::into).collect();
        self
    }

    /// Whether the request activated the extension `uri`.
    pub fn has_extension(&self, uri: &str) -> bool {
        self.extensions.iter().any(|e| e == uri)
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
/// for `a2a::A2AError`), and to one [`ErrorClass`] (see the [`Classify`] impl): `TaskNotFound`
/// is `NotFound`, `NotCancelable` and `UnsupportedOperation` are `Rejected`, `InvalidParams` is `Invalid`, `Unavailable` is
/// `Transient` and `Internal` is `Internal`. The message of `Unavailable` and `Internal`
/// describes this layer only; the lower error is the [`source`](std::error::Error::source).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// The task is in a state that cannot take the message: a message to a task that has finished
    /// (A2A's `UnsupportedOperationError`, code `-32004`).
    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),
    /// The request is well-formed JSON-RPC but semantically unacceptable, for
    /// example a follow-up to a task that is not waiting for input.
    /// JSON-RPC code `-32602`.
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// A dependency (database, worker pool) is temporarily unavailable.
    /// Retryable; surfaces as JSON-RPC `-32603` with a generic message.
    #[error("backend unavailable: {message}")]
    Unavailable {
        /// What was unavailable, without the lower error's text.
        message: String,
        /// The lower error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// Anything else. JSON-RPC code `-32603`; the detail is logged, not sent
    /// to the client.
    #[error("internal error: {message}")]
    Internal {
        /// What went wrong, without the lower error's text.
        message: String,
        /// The lower error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
}

impl BackendError {
    /// A dependency is temporarily unavailable ([`Unavailable`](Self::Unavailable)).
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
            source: None,
        }
    }

    /// Anything else ([`Internal`](Self::Internal)).
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
            source: None,
        }
    }

    /// Keep `err` as the source of an `Unavailable` or `Internal`; other variants are returned
    /// as they are.
    #[must_use]
    pub fn with_source(mut self, err: impl Into<BoxError>) -> Self {
        if let Self::Unavailable { source, .. } | Self::Internal { source, .. } = &mut self {
            *source = Some(err.into());
        }
        self
    }
}

impl Classify for BackendError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::TaskNotFound(_) => ErrorClass::NotFound,
            Self::NotCancelable { .. } | Self::UnsupportedOperation(_) => ErrorClass::Rejected,
            Self::InvalidParams(_) => ErrorClass::Invalid,
            Self::Unavailable { .. } => ErrorClass::Transient,
            Self::Internal { .. } => ErrorClass::Internal,
        }
    }
}

impl From<BackendError> for a2a::A2AError {
    fn from(err: BackendError) -> Self {
        match err {
            BackendError::TaskNotFound(id) => a2a::A2AError::task_not_found(&id),
            BackendError::NotCancelable { task_id, .. } => {
                a2a::A2AError::task_not_cancelable(&task_id)
            }
            BackendError::UnsupportedOperation(msg) => a2a::A2AError::unsupported_operation(msg),
            BackendError::InvalidParams(msg) => a2a::A2AError::invalid_params(msg),
            // The trust boundary: the client gets a generic message, the log gets the chain.
            err @ BackendError::Unavailable { .. } => {
                tracing::error!(error = %report(&err), "backend unavailable");
                a2a::A2AError::internal("backend temporarily unavailable")
            }
            err @ BackendError::Internal { .. } => {
                tracing::error!(error = %report(&err), "backend internal error");
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
///   follow-up to a task that has finished is
///   [`BackendError::UnsupportedOperation`] (A2A's error for a message to a
///   terminal task); to a task in any other state, [`BackendError::InvalidParams`]
///   (a backend may accept it when the caller activated an extension that says what such
///   a message is, as `steer/v1` does for a running task); an unknown id is
///   [`BackendError::TaskNotFound`].
/// * **Cancel** of an already-canceled task returns it unchanged; cancel of a
///   task in another terminal state is [`BackendError::NotCancelable`].
/// * **Listing** ([`list`](Self::list)) is the caller's own tasks, newest update first, by cursor.
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

    /// One page of the caller's tasks that match `query`, most recently updated first (ties by
    /// task id, descending), and how many match in all.
    ///
    /// * **Only the caller's own tasks**, whatever the filters say (A2A §13.1): scope the
    ///   query to the caller before anything else, so no count, no token and no error depends
    ///   on another caller's tasks.
    /// * **Cursor pagination**: continue after the position [`PageToken::decode`](crate::PageToken::decode) gives for
    ///   `query.page_token`, and issue the next token with [`PageToken::encode`](crate::PageToken::encode) when more
    ///   tasks follow. A token that does not decode is [`BackendError::InvalidParams`].
    /// * Tasks carry no history; the handler applies `historyLength` and `includeArtifacts`.
    ///
    /// The default answers [`BackendError::UnsupportedOperation`], so a backend written before
    /// `ListTasks` existed keeps compiling and keeps saying it cannot list.
    async fn list(&self, caller: &Caller, query: &TaskQuery) -> Result<TaskPage, BackendError> {
        let _ = (caller, query);
        Err(BackendError::UnsupportedOperation(
            "ListTasks is not supported".to_owned(),
        ))
    }
}

/// A shareable, type-erased [`TaskBackend`].
pub type DynTaskBackend = Arc<dyn TaskBackend>;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("pool exhausted")]
    struct Lower;

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &BackendError) -> ErrorClass {
        match e {
            BackendError::TaskNotFound(_) => ErrorClass::NotFound,
            BackendError::NotCancelable { .. } | BackendError::UnsupportedOperation(_) => {
                ErrorClass::Rejected
            }
            BackendError::InvalidParams(_) => ErrorClass::Invalid,
            BackendError::Unavailable { .. } => ErrorClass::Transient,
            BackendError::Internal { .. } => ErrorClass::Internal,
        }
    }

    fn samples() -> Vec<BackendError> {
        vec![
            BackendError::TaskNotFound("t".into()),
            BackendError::NotCancelable {
                task_id: "t".into(),
                state: "completed".into(),
            },
            BackendError::InvalidParams("x".into()),
            BackendError::unavailable("database").with_source(Lower),
            BackendError::internal("bug").with_source(Lower),
            BackendError::UnsupportedOperation("task t is completed".into()),
        ]
    }

    #[test]
    fn class_table() {
        for e in samples() {
            assert_eq!(e.class(), expected(&e), "{e}");
        }
        let retryable: Vec<bool> = samples().iter().map(Classify::is_retryable).collect();
        assert_eq!(retryable, [false, false, false, true, false, false]);
    }

    #[test]
    fn the_source_is_kept_and_not_repeated_in_the_message() {
        for e in samples().into_iter().skip(3).take(2) {
            let source = std::error::Error::source(&e).expect("source kept");
            assert!(source.is::<Lower>());
            assert!(!e.to_string().contains("pool exhausted"), "{e}");
            assert!(report(&e).ends_with(": pool exhausted"));
        }
    }

    /// The trust boundary: the client sees a generic message, never the cause.
    #[test]
    fn a2a_errors_do_not_leak_the_cause() {
        let codes: Vec<(i32, String)> = samples()
            .into_iter()
            .map(|e| {
                let e = a2a::A2AError::from(e);
                (e.code, e.message)
            })
            .collect();
        assert_eq!(codes[0].0, -32001);
        assert_eq!(codes[1].0, -32002);
        assert_eq!(codes[2].0, -32602);
        assert_eq!(codes[3], (-32603, "backend temporarily unavailable".into()));
        assert_eq!(codes[4], (-32603, "internal error".into()));
        assert_eq!(codes[5].0, -32004);
    }
}
