//! [`NotifyError`].

use adam_error::{Classify, ErrorClass};

/// Why configuring or running a [`PgNotify`](crate::PgNotify) failed.
///
/// Decide from [`Classify::class`]: `InvalidPrefix` is `Invalid`, `AlreadyRunning` is `Internal`
/// and `PoolClosed` is `Rejected`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NotifyError {
    /// The channel prefix is empty, longer than 40 characters, starts with a digit or contains
    /// anything but `[a-z0-9_]`.
    #[error("invalid channel prefix {0:?}")]
    InvalidPrefix(String),
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
            Self::InvalidPrefix(_) => ErrorClass::Invalid,
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
            NotifyError::InvalidPrefix(_) => ErrorClass::Invalid,
            NotifyError::AlreadyRunning => ErrorClass::Internal,
            NotifyError::PoolClosed => ErrorClass::Rejected,
        }
    }

    #[test]
    fn class_table() {
        for e in [
            NotifyError::InvalidPrefix("X".into()),
            NotifyError::AlreadyRunning,
            NotifyError::PoolClosed,
        ] {
            assert_eq!(e.class(), expected(&e), "{e}");
            assert!(!e.is_retryable(), "{e}");
        }
    }
}
