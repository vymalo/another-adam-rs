//! One classification model for every error of adam-rs.
//!
//! Variants say what happened; [`ErrorClass`] says what to do about it. Retry, HTTP status and
//! exit-code decisions match on [`Classify::class`], never on variants, so a new variant only
//! needs a class decision. The crate is pure: no I/O, no async, std only (`thiserror` is a
//! dev-dependency of its tests).

#![warn(missing_docs)]

use std::error::Error;
use std::fmt::Write as _;
use std::time::Duration;

/// A foreign error kept as the `source` of a port error (no driver or SDK type in a
/// trait signature, so adapters box theirs).
pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

/// What to do about an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorClass {
    /// May succeed later: network, 5xx, timeout, pool, crashed child.
    Transient,
    /// Slow down; honour [`Classify::retry_after`].
    RateLimited,
    /// Lost an optimistic-concurrency race: re-read, retry.
    Conflict,
    /// The input is wrong; the same input never succeeds.
    Invalid,
    /// Absent, or invisible to this caller.
    NotFound,
    /// Valid input, but the target's state forbids it (finished, busy, exists).
    Rejected,
    /// Credentials missing or refused.
    Unauthenticated,
    /// The peer does not offer this operation.
    Unsupported,
    /// Stored or received data breaks an invariant: alert.
    Corrupt,
    /// A bug, or unclassified: alert.
    Internal,
}

impl ErrorClass {
    /// Whether the same operation may succeed if repeated.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::RateLimited | Self::Conflict)
    }

    /// The opposite of [`is_retryable`](Self::is_retryable).
    pub const fn is_permanent(self) -> bool {
        !self.is_retryable()
    }

    /// Whether an operator should be told.
    pub const fn should_alert(self) -> bool {
        matches!(self, Self::Corrupt | Self::Internal)
    }
}

/// An error that knows its [`ErrorClass`].
pub trait Classify: Error {
    /// What to do about this error.
    fn class(&self) -> ErrorClass;

    /// How long to wait before retrying, when the peer said so.
    fn retry_after(&self) -> Option<Duration> {
        None
    }

    /// Derived from [`class`](Self::class); never hand-written.
    fn is_retryable(&self) -> bool {
        self.class().is_retryable()
    }
}

/// `a: b: c`, the error and its whole source chain on one line.
///
/// The only sanctioned way to flatten an error, used at trust and persistence boundaries
/// (outbox text, logs). A `Display` never interpolates its source, so nothing prints twice.
pub fn report(err: &(dyn Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(cause) = cur {
        let _ = write!(out, ": {cause}");
        cur = cause.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct Layer(&'static str, #[source] Option<Box<Layer>>);

    #[test]
    fn report_joins_a_three_level_chain() {
        let e = Layer(
            "a",
            Some(Box::new(Layer("b", Some(Box::new(Layer("c", None)))))),
        );
        assert_eq!(report(&e), "a: b: c");
        assert_eq!(report(&Layer("solo", None)), "solo");
    }

    #[test]
    fn the_truth_table() {
        use ErrorClass::*;
        // Exhaustive: a new class forces a decision here.
        let table = |c: ErrorClass| match c {
            Transient => (true, false),
            RateLimited => (true, false),
            Conflict => (true, false),
            Invalid => (false, false),
            NotFound => (false, false),
            Rejected => (false, false),
            Unauthenticated => (false, false),
            Unsupported => (false, false),
            Corrupt => (false, true),
            Internal => (false, true),
        };
        for c in [
            Transient,
            RateLimited,
            Conflict,
            Invalid,
            NotFound,
            Rejected,
            Unauthenticated,
            Unsupported,
            Corrupt,
            Internal,
        ] {
            let (retry, alert) = table(c);
            assert_eq!(c.is_retryable(), retry, "{c:?}");
            assert_eq!(c.is_permanent(), !retry, "{c:?}");
            assert_eq!(c.should_alert(), alert, "{c:?}");
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("classified")]
    struct Classified(ErrorClass);

    impl Classify for Classified {
        fn class(&self) -> ErrorClass {
            self.0
        }
    }

    #[test]
    fn classify_derives_retryability_from_the_class() {
        assert!(Classified(ErrorClass::Transient).is_retryable());
        assert!(Classified(ErrorClass::RateLimited).is_retryable());
        assert!(!Classified(ErrorClass::Invalid).is_retryable());
        assert_eq!(Classified(ErrorClass::Transient).retry_after(), None);
    }
}
