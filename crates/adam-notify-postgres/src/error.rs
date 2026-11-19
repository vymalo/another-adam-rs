//! [`NotifyError`].

use adam_error::{Classify, ErrorClass};

/// Why configuring or running a [`PgNotify`](crate::PgNotify) failed.
///
/// Decide from [`Classify::class`]: `InvalidPrefix` and `InvalidLocalOnly` are `Invalid`,
/// `AlreadyRunning` is `Internal` and `PoolClosed` is `Rejected`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NotifyError {
    /// The channel prefix is empty, longer than 40 characters, starts with a digit or contains
    /// anything but `[a-z0-9_]`.
    #[error("invalid channel prefix {0:?}")]
    InvalidPrefix(String),
    /// `NOTIFY_LOCAL_ONLY` is set to something that is not `true`, `1`, `false` or `0`, and
    /// [`PgNotify::local_only`](crate::PgNotify::local_only) did not override it.
    #[error("NOTIFY_LOCAL_ONLY must be true or false, got {0:?}")]
    InvalidLocalOnly(String),
    /// [`PgNotify::run`](crate::PgNotify::run) is already running for this instance (or one of
    /// its clones).
    #[error("this PgNotify is already running")]
    AlreadyRunning,
    /// The pool was closed, so the listener has no connection to get.
    #[error("the connection pool is closed")]
    PoolClosed,
}

impl Classify for NotifyError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::InvalidPrefix(_) | Self::InvalidLocalOnly(_) => ErrorClass::Invalid,
            Self::AlreadyRunning => ErrorClass::Internal,
            Self::PoolClosed => ErrorClass::Rejected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &NotifyError) -> ErrorClass {
        match e {
            NotifyError::InvalidPrefix(_) | NotifyError::InvalidLocalOnly(_) => ErrorClass::Invalid,
            NotifyError::AlreadyRunning => ErrorClass::Internal,
            NotifyError::PoolClosed => ErrorClass::Rejected,
        }
    }

    #[test]
    fn class_table() {
        for e in [
            NotifyError::InvalidPrefix("X".into()),
            NotifyError::InvalidLocalOnly("yes".into()),
            NotifyError::AlreadyRunning,
            NotifyError::PoolClosed,
        ] {
            assert_eq!(e.class(), expected(&e), "{e}");
            assert!(!e.is_retryable(), "{e}");
        }
    }
}
