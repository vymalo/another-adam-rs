//! Errors of the workspace crate.

use std::io;
use std::time::Duration;

use adam_error::{BoxError, Classify, ErrorClass};

/// Result alias used throughout the crate.
pub type WorkspaceResult<T> = Result<T, WorkspaceError>;

/// Everything that can go wrong preparing a workspace, running `git`, or
/// talking to a code host.
///
/// No variant ever carries a credential: messages that originate from `git`
/// or from an HTTP response are scrubbed of the token before they are stored.
///
/// Decide from [`Classify::class`], not from the variant: `Auth` is
/// `Unauthenticated`, `NotFound` is `NotFound`, `Invalid` is `Invalid`,
/// `Transient` is `Transient`, `RateLimited` is `RateLimited`, `Conflict` is
/// `Rejected`, `Corrupt` is `Corrupt`, and `Git`, `Http` and `Io` are `Internal`.
/// A message describes this layer only; [`adam_error::report`] prints the chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// The credentials were rejected or are missing (HTTP 401/403, git
    /// authentication failure, unset token).
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The repository, branch or pull request does not exist (or is invisible
    /// to the token).
    #[error("not found: {0}")]
    NotFound(String),

    /// The request was understood but refused: bad input, a validation error
    /// from the code host (HTTP 422), a rejected push.
    #[error("invalid request: {0}")]
    Invalid(String),

    /// A failure that is likely to go away on retry: network errors, timeouts,
    /// HTTP 5xx.
    #[error("transient failure: {message}")]
    Transient {
        /// What happened, scrubbed, without the underlying error's text.
        message: String,
        /// The transport's own error, when there is one.
        #[source]
        source: Option<BoxError>,
    },

    /// The code host is rate limiting us (HTTP 429, or a GitHub rate limit
    /// answered with 403). Retry after `retry_after`, when the host said.
    #[error("the code host is rate limiting requests")]
    RateLimited {
        /// How long the host asked us to wait, when it said.
        retry_after: Option<Duration>,
    },

    /// The run id is already bound to a different repository.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A workspace on disk is in a state this crate will not touch on its own
    /// (call [`Workspaces::remove`](crate::Workspaces::remove) first).
    #[error("corrupt workspace: {0}")]
    Corrupt(String),

    /// `git` exited unsuccessfully and the failure fits no other category.
    #[error("git {command} failed ({status}): {message}")]
    Git {
        /// The git subcommand, e.g. `fetch`.
        command: String,
        /// Exit status, e.g. `exit status: 128`.
        status: String,
        /// Scrubbed stderr.
        message: String,
    },

    /// The code host answered with an unexpected status.
    #[error("code host returned HTTP {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Scrubbed response message.
        message: String,
    },

    /// A local I/O error (spawning `git`, reading or writing workspace files).
    #[error("{context}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl Classify for WorkspaceError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Auth(_) => ErrorClass::Unauthenticated,
            Self::NotFound(_) => ErrorClass::NotFound,
            Self::Invalid(_) => ErrorClass::Invalid,
            Self::Transient { .. } => ErrorClass::Transient,
            Self::RateLimited { .. } => ErrorClass::RateLimited,
            Self::Conflict(_) => ErrorClass::Rejected,
            Self::Corrupt(_) => ErrorClass::Corrupt,
            Self::Git { .. } | Self::Http { .. } | Self::Io { .. } => ErrorClass::Internal,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }
}

impl WorkspaceError {
    /// A failure that is likely to go away on retry, with a scrubbed message.
    pub(crate) fn transient(message: impl Into<String>) -> Self {
        Self::Transient {
            message: message.into(),
            source: None,
        }
    }

    /// Keep `err` as the source of a [`Transient`](Self::Transient).
    pub(crate) fn with_source(
        mut self,
        err: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        if let Self::Transient { source, .. } = &mut self {
            *source = Some(Box::new(err));
        }
        self
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &WorkspaceError) -> ErrorClass {
        match e {
            WorkspaceError::Auth(_) => ErrorClass::Unauthenticated,
            WorkspaceError::NotFound(_) => ErrorClass::NotFound,
            WorkspaceError::Invalid(_) => ErrorClass::Invalid,
            WorkspaceError::Transient { .. } => ErrorClass::Transient,
            WorkspaceError::RateLimited { .. } => ErrorClass::RateLimited,
            WorkspaceError::Conflict(_) => ErrorClass::Rejected,
            WorkspaceError::Corrupt(_) => ErrorClass::Corrupt,
            WorkspaceError::Git { .. } => ErrorClass::Internal,
            WorkspaceError::Http { .. } => ErrorClass::Internal,
            WorkspaceError::Io { .. } => ErrorClass::Internal,
        }
    }

    fn samples() -> Vec<WorkspaceError> {
        vec![
            WorkspaceError::Auth("x".into()),
            WorkspaceError::NotFound("x".into()),
            WorkspaceError::Invalid("x".into()),
            WorkspaceError::transient("x"),
            WorkspaceError::RateLimited {
                retry_after: Some(Duration::from_secs(9)),
            },
            WorkspaceError::Conflict("x".into()),
            WorkspaceError::Corrupt("x".into()),
            WorkspaceError::Git {
                command: "fetch".into(),
                status: "exit status 128".into(),
                message: "x".into(),
            },
            WorkspaceError::Http {
                status: 418,
                message: "x".into(),
            },
            WorkspaceError::io("cannot do it", io::Error::other("disk on fire")),
        ]
    }

    #[test]
    fn class_table() {
        for e in samples() {
            assert_eq!(e.class(), expected(&e), "{e}");
        }
        // Only these retry, as before the class model.
        let retryable: Vec<_> = samples()
            .iter()
            .filter(|e| e.is_retryable())
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            retryable,
            [
                "transient failure: x",
                "the code host is rate limiting requests"
            ]
        );
    }

    #[test]
    fn only_a_rate_limit_has_a_retry_after() {
        let after: Vec<_> = samples().iter().map(Classify::retry_after).collect();
        assert_eq!(after[4], Some(Duration::from_secs(9)));
        assert_eq!(after.iter().flatten().count(), 1);
    }

    /// Regression for A4: `{:#}` printed an `Io` cause twice, once in the message and once as the
    /// source.
    #[test]
    fn io_display_does_not_repeat_its_source() {
        let e = WorkspaceError::io(
            "cannot write run metadata",
            io::Error::other("disk on fire"),
        );
        let source = std::error::Error::source(&e).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("disk on fire"));
        assert!(!e.to_string().contains("disk on fire"), "{e}");
        assert_eq!(
            adam_error::report(&e),
            "cannot write run metadata: disk on fire"
        );
    }

    #[test]
    fn transient_keeps_a_source_without_printing_it_twice() {
        let e = WorkspaceError::transient("code host request failed")
            .with_source(io::Error::other("connection reset"));
        assert!(!e.to_string().contains("connection reset"));
        assert_eq!(
            adam_error::report(&e),
            "transient failure: code host request failed: connection reset"
        );
    }
}
