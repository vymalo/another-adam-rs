//! [`EngineError`]: what an engine call can fail with, and its class.

use std::sync::Arc;

use adam_a2a::BackendError;
use adam_core::{RunId, StoreError};
use adam_error::{BoxError, Classify, ErrorClass};
use adam_runtime::RuntimeError;

use crate::RunState;
use crate::engine::Expected;

/// Why an [`Engine`](crate::Engine) call failed. Decide from [`Classify::class`], never from the
/// variant: a new variant only needs a class.
///
/// | Variant | Class |
/// |---|---|
/// | `Agent` | the agent definition's own (`Invalid` for a file that does not bind) |
/// | `NoRuntime`, `Invalid` | `Invalid` |
/// | `NotFound` | `NotFound` |
/// | `NotWaiting`, `Finished`, `Stopped`, `WorkerIdInUse`, `Refused` | `Rejected` |
/// | `Unavailable` (a store that is down, or a run whose state moved under the call: try again) | `Transient` |
/// | `Store` | the store's own |
/// | `Schedules` | the reconcile's own |
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
    /// A child session's limits or state refuse the call (`child-sessions/v1`): a session too many
    /// (open or working), a message to a closed session, one past its rounds, or one while its rounds
    /// already work.
    #[error("refused: {0}")]
    Refused(String),
    /// The engine was shut down: nothing would step a run it started now.
    #[error("the engine was shut down")]
    Stopped,
    /// `start` with a fixed [`Limits::worker_id`](crate::Limits::worker_id) that another live
    /// engine of this process uses on the same store: its release would take that engine's leases.
    /// Shut the other engine down first, or give this one another id (or none).
    #[error(
        "another live engine of this process uses the worker id {id:?} on this store: shut it down \
         first, or give this engine another id"
    )]
    WorkerIdInUse {
        /// The id in use.
        id: String,
    },
    /// The store or the runtime is temporarily unavailable; the same call may succeed later.
    #[error("the engine is temporarily unavailable: {message}")]
    Unavailable {
        /// What was unavailable.
        message: String,
        /// The lower error.
        #[source]
        source: Option<BoxError>,
    },
    /// The store refused or failed: at `start`, its migration or the release of the worker's
    /// leases; or while [`file`](crate::Engine::file) read a file's bytes.
    #[error("the store failed")]
    Store(#[source] StoreError),
    /// `start` could not make the store's scheduler runs match the folder's `schedules/` (ADR
    /// 0060): a schedule that does not parse, or a store that failed while it was read or written.
    #[error("the agent's schedules cannot be set up")]
    Schedules(#[source] adam_schedule::ReconcileError),
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
            Self::NotWaiting { .. }
            | Self::Finished { .. }
            | Self::Stopped
            | Self::WorkerIdInUse { .. }
            | Self::Refused(_) => ErrorClass::Rejected,
            Self::Unavailable { .. } => ErrorClass::Transient,
            Self::Store(source) => source.class(),
            Self::Schedules(source) => source.class(),
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

    /// The refusal of a call that the backend refused for the run's state, from `read`, the run's
    /// state read after the refusal (`Ok(None)`: the run is gone): an ended run is `Finished`, a
    /// run gone is `NotFound`, an answer to a run that works is `NotWaiting`, and a state that
    /// would have taken the call (it moved under it) is `Unavailable`, worth trying again.
    pub(crate) fn by_state(
        run: RunId,
        read: Result<Option<RunState>, EngineError>,
        expected: Expected,
    ) -> Self {
        match read {
            Err(error) => error,
            Ok(None) => Self::NotFound(format!("run {run}")),
            Ok(Some(state)) if state.is_terminal() => Self::Finished { run },
            Ok(Some(state @ (RunState::Submitted | RunState::Working)))
                if expected == Expected::Waiting =>
            {
                Self::NotWaiting { run, state }
            }
            Ok(Some(_)) => Self::Unavailable {
                message: "the run's state changed under the call".to_owned(),
                source: None,
            },
        }
    }

    /// A read of a run's file that failed ([`file`](crate::Engine::file)): the store's own error
    /// keeps its class, anything else is a transient failure or a bug.
    pub(crate) fn of_read(err: RuntimeError) -> Self {
        match err {
            RuntimeError::Store(source) => Self::Store(source),
            err if err.is_retryable() => Self::Unavailable {
                message: "the store, reading a file".to_owned(),
                source: Some(Box::new(err)),
            },
            err => Self::Internal {
                message: "a file could not be read".to_owned(),
                source: Some(Box::new(err)),
            },
        }
    }

    /// A read of `run` through the runtime that failed (a schedule's firing, which the backend
    /// does not reach): a run gone under the call is `NotFound`, the store's own error keeps its
    /// class, anything else is a transient failure or a bug.
    pub(crate) fn of_run_read(err: RuntimeError, run: RunId) -> Self {
        match err {
            RuntimeError::NotFound(_) => Self::NotFound(format!("run {run}")),
            RuntimeError::Store(source) => Self::Store(source),
            err if err.is_retryable() => Self::Unavailable {
                message: "the store, reading a run".to_owned(),
                source: Some(Box::new(err)),
            },
            err => Self::Internal {
                message: "a run could not be read".to_owned(),
                source: Some(Box::new(err)),
            },
        }
    }

    /// The worker's end, at [`shutdown`](crate::Engine::shutdown).
    pub(crate) fn of_runtime(err: Arc<RuntimeError>) -> Self {
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
            | EngineError::Stopped
            | EngineError::WorkerIdInUse { .. }
            | EngineError::Refused(_) => ErrorClass::Rejected,
            EngineError::Unavailable { .. } => ErrorClass::Transient,
            EngineError::Store(source) => source.class(),
            EngineError::Schedules(source) => source.class(),
            EngineError::Internal { .. } => ErrorClass::Internal,
        }
    }

    /// The races a test cannot make happen on cue: the state read after a refusal decides.
    #[test]
    fn a_refusal_for_the_state_says_the_state_it_read() {
        let run = RunId::new();
        let by = |read, expected| EngineError::by_state(run, Ok(read), expected);
        for ended in [RunState::Done, RunState::Failed, RunState::Cancelled] {
            for expected in [Expected::Waiting, Expected::Open] {
                let e = by(Some(ended), expected);
                assert!(matches!(e, EngineError::Finished { .. }), "{e:?}");
                assert_eq!(e.class(), ErrorClass::Rejected);
            }
        }
        for gone in [Expected::Waiting, Expected::Open] {
            let e = by(None, gone);
            assert!(matches!(e, EngineError::NotFound(_)), "{e:?}");
        }
        for state in [RunState::Submitted, RunState::Working] {
            let e = by(Some(state), Expected::Waiting);
            assert!(
                matches!(e, EngineError::NotWaiting { state: s, .. } if s == state),
                "{e:?}"
            );
            assert_eq!(e.class(), ErrorClass::Rejected);
            // A steer or a cancel would have been taken: the state moved, try again.
            assert_eq!(
                by(Some(state), Expected::Open).class(),
                ErrorClass::Transient
            );
        }
        assert_eq!(
            by(Some(RunState::Waiting), Expected::Waiting).class(),
            ErrorClass::Transient
        );
        let down = EngineError::by_state(run, Err(EngineError::internal("x")), Expected::Open);
        assert_eq!(down.class(), ErrorClass::Internal);
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
