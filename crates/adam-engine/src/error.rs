//! [`EngineError`]: what an engine call can fail with, and its class.

use adam_a2a::BackendError;
use adam_core::{RunId, StoreError};
use adam_error::{BoxError, Classify, ErrorClass};
use adam_runtime::RuntimeError;

use crate::RunState;

/// Why an [`Engine`](crate::Engine) call failed. Decide from [`Classify::class`], never from the
/// variant: a new variant only needs a class.
///
/// | Variant | Class |
/// |---|---|
/// | `Agent` | the agent definition's own (`Invalid` for a file that does not bind) |
/// | `NoRuntime`, `Invalid` | `Invalid` |
/// | `NotFound` | `NotFound` |
/// | `NotWaiting`, `Finished`, `Stopped` | `Rejected` |
/// | `Unavailable` | `Transient` |
/// | `Store` | the store's own |
/// | `Internal` | `Internal` |
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    /// The agent's definition does not bind, or the model alias is refused: `start` builds
    /// nothing.
    #[error("the agent cannot be built")]
    Agent(#[source] Box<adam_assembly::Error>),
    /// `start` was called outside a tokio runtime: the worker has nowhere to run.
    #[error("the engine starts inside a tokio runtime, and none is current")]
    NoRuntime,
    /// The request is wrong whatever the run's state: a message with no text or no id.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// No such run or conversation for this engine.
    #[error("not found: {0}")]
    NotFound(String),
    /// [`answer`](crate::Engine::answer) to a run that is not waiting for one.
    #[error("run {run} is {state:?}, not waiting for an answer")]
    NotWaiting {
        /// The run that was answered.
        run: RunId,
        /// Its state when it was refused.
        state: RunState,
    },
    /// A message or a cancel for a run that has ended (a cancel of a cancelled run is not one: it
    /// answers the run as it is).
    #[error("run {run} has ended and cannot take this")]
    Finished {
        /// The run.
        run: RunId,
    },
    /// The engine was shut down: nothing would step a run it started now.
    #[error("the engine was shut down")]
    Stopped,
    /// The store or the runtime is temporarily unavailable; the same call may succeed later.
    #[error("the engine is temporarily unavailable: {message}")]
    Unavailable {
        /// What was unavailable.
        message: String,
        /// The lower error.
        #[source]
        source: Option<BoxError>,
    },
    /// The store refused or failed at `start` (its migration).
    #[error("the store failed")]
    Store(#[source] StoreError),
    /// A bug, or a failure nothing above describes.
    #[error("internal error: {message}")]
    Internal {
        /// What went wrong.
        message: String,
        /// The lower error.
        #[source]
        source: Option<BoxError>,
    },
}

impl Classify for EngineError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Agent(source) => source.class(),
            Self::NoRuntime | Self::Invalid(_) => ErrorClass::Invalid,
            Self::NotFound(_) => ErrorClass::NotFound,
            Self::NotWaiting { .. } | Self::Finished { .. } | Self::Stopped => ErrorClass::Rejected,
            Self::Unavailable { .. } => ErrorClass::Transient,
            Self::Store(source) => source.class(),
            Self::Internal { .. } => ErrorClass::Internal,
        }
    }
}

impl EngineError {
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
            source: None,
        }
    }

    /// The backend's refusal of a call about `run`, by its class. The backend's error stays the
    /// source of the two that keep one, so no A2A type is in a signature.
    pub(crate) fn of_backend(err: BackendError, run: Option<RunId>) -> Self {
        match err {
            BackendError::TaskNotFound(task) => Self::NotFound(format!("run {task}")),
            BackendError::NotCancelable { .. } | BackendError::UnsupportedOperation(_) => match run
            {
                Some(run) => Self::Finished { run },
                None => Self::internal("a run that ended was refused without its id"),
            },
            BackendError::InvalidParams(why) => Self::Invalid(why),
            err @ BackendError::Unavailable { .. } => Self::Unavailable {
                message: "the runtime or the store".to_owned(),
                source: Some(Box::new(err)),
            },
            err => Self::Internal {
                message: "the runtime failed".to_owned(),
                source: Some(Box::new(err)),
            },
        }
    }

    /// The worker's end, at [`shutdown`](crate::Engine::shutdown).
    pub(crate) fn of_runtime(err: RuntimeError) -> Self {
        if err.is_retryable() {
            Self::Unavailable {
                message: "the worker".to_owned(),
                source: Some(Box::new(err)),
            }
        } else {
            Self::Internal {
                message: "the worker failed".to_owned(),
                source: Some(Box::new(err)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &EngineError) -> ErrorClass {
        match e {
            EngineError::Agent(source) => source.class(),
            EngineError::NoRuntime | EngineError::Invalid(_) => ErrorClass::Invalid,
            EngineError::NotFound(_) => ErrorClass::NotFound,
            EngineError::NotWaiting { .. }
            | EngineError::Finished { .. }
            | EngineError::Stopped => ErrorClass::Rejected,
            EngineError::Unavailable { .. } => ErrorClass::Transient,
            EngineError::Store(source) => source.class(),
            EngineError::Internal { .. } => ErrorClass::Internal,
        }
    }

    #[test]
    fn every_backend_refusal_keeps_its_class() {
        let run = RunId::new();
        let cases = [
            (BackendError::TaskNotFound("t".into()), ErrorClass::NotFound),
            (
                BackendError::NotCancelable {
                    task_id: "t".into(),
                    state: "Completed".into(),
                },
                ErrorClass::Rejected,
            ),
            (
                BackendError::UnsupportedOperation("ended".into()),
                ErrorClass::Rejected,
            ),
            (BackendError::InvalidParams("x".into()), ErrorClass::Invalid),
            (BackendError::unavailable("db"), ErrorClass::Transient),
            (BackendError::internal("bug"), ErrorClass::Internal),
        ];
        for (err, class) in cases {
            let shown = err.to_string();
            assert_eq!(err.class(), class, "the backend's class of {shown}");
            let mapped = EngineError::of_backend(err, Some(run));
            assert_eq!(mapped.class(), class, "{shown} became {mapped}");
            assert_eq!(mapped.class(), expected(&mapped));
        }
        assert_eq!(
            EngineError::NotWaiting {
                run,
                state: RunState::Working
            }
            .class(),
            ErrorClass::Rejected
        );
    }
}
