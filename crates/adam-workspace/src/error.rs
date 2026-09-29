//! Errors of the workspace crate.

use std::io;

/// Result alias used throughout the crate.
pub type WorkspaceResult<T> = Result<T, WorkspaceError>;

/// Everything that can go wrong preparing a workspace, running `git`, or
/// talking to a code host.
///
/// No variant ever carries a credential: messages that originate from `git`
/// or from an HTTP response are scrubbed of the token before they are stored.
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
    /// HTTP 5xx and 429, GitHub rate limiting.
    #[error("transient failure: {0}")]
    Transient(String),

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
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl WorkspaceError {
    /// Whether retrying the same operation later may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient(_))
    }

    pub(crate) fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}
