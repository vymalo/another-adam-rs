//! What a write or a read can refuse, and what class each refusal carries.

use adam_error::{BoxError, Classify, ErrorClass};
use thiserror::Error;

/// An L1 memory refusal. Callers decide from [`Classify::class`], never from the variant.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MemoryError {
    /// A text still held the redaction marker after the scrub: a secret, or a marker a caller
    /// copied back. Nothing was stored.
    #[error("{field} still carries the redaction marker after scrubbing")]
    Redacted {
        /// The field that carried it.
        field: &'static str,
    },
    /// A text was longer than its byte bound after scrubbing. Nothing was stored.
    #[error("{field} is {bytes} bytes, over the bound of {limit}")]
    TooManyBytes {
        /// The field that was too long.
        field: &'static str,
        /// Its length after scrubbing.
        bytes: usize,
        /// The bound.
        limit: usize,
    },
    /// A root cause was longer than its character bound after scrubbing. Nothing was stored.
    #[error("{field} is {chars} characters, over the bound of {limit}")]
    TooManyChars {
        /// The field that was too long.
        field: &'static str,
        /// Its length after scrubbing.
        chars: usize,
        /// The bound.
        limit: usize,
    },
    /// An agent-scoped text named a repository, a path, a URL, a host or an e-mail address. Nothing
    /// was stored.
    #[error("{field} names a {what} in an agent scope")]
    NamesAddress {
        /// The field that named it.
        field: &'static str,
        /// What it named.
        what: &'static str,
    },
    /// A scope component was empty.
    #[error("the scope names an empty {field}")]
    EmptyScope {
        /// The component.
        field: &'static str,
    },
    /// A text a write needs was empty.
    #[error("{field} is empty")]
    EmptyField {
        /// The field.
        field: &'static str,
    },
    /// The scope already held as many problems as it may.
    #[error("the scope holds {limit} problems already")]
    ScopeFull {
        /// The bound.
        limit: usize,
    },
    /// The problem already held as many attempts as it may.
    #[error("the problem holds {limit} attempts already")]
    ProblemFull {
        /// The bound.
        limit: usize,
    },
    /// An attempt carried more anchors than it may.
    #[error("an attempt carries {anchors} anchors, over the bound of {limit}")]
    TooManyAnchors {
        /// How many it carried.
        anchors: usize,
        /// The bound.
        limit: usize,
    },
    /// No problem with this root cause is in the scope.
    #[error("no problem {0:?} in this scope")]
    ProblemNotFound(String),
    /// No attempt with this id is in the problem.
    #[error("no attempt {0} in problem {1:?}")]
    AttemptNotFound(String, String),
    /// A file in the memory folder does not parse, or belongs to another scope.
    #[error("corrupt memory file {path}: {reason}")]
    Corrupt {
        /// The file.
        path: String,
        /// What is wrong with it.
        reason: String,
    },
    /// The folder could not be read or written. An adapter chooses the class; a local file system
    /// failure is transient at run time (ADR 0081 decision 12).
    #[error("memory storage failed")]
    Backend {
        /// The file system's own error.
        #[source]
        source: BoxError,
    },
}

impl MemoryError {
    /// A storage failure.
    pub(crate) fn backend(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            source: Box::new(error),
        }
    }
}

impl Classify for MemoryError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Redacted { .. }
            | Self::TooManyBytes { .. }
            | Self::TooManyChars { .. }
            | Self::NamesAddress { .. }
            | Self::EmptyScope { .. }
            | Self::EmptyField { .. }
            | Self::TooManyAnchors { .. } => ErrorClass::Invalid,
            Self::ScopeFull { .. } | Self::ProblemFull { .. } => ErrorClass::Rejected,
            Self::ProblemNotFound(_) | Self::AttemptNotFound(..) => ErrorClass::NotFound,
            Self::Corrupt { .. } => ErrorClass::Corrupt,
            Self::Backend { .. } => ErrorClass::Transient,
        }
    }
}

/// A result from an L1 memory adapter.
pub type MemoryResult<T> = Result<T, MemoryError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_class_table_is_exhaustive() {
        // Every variant, matched here: a new variant forces a class decision.
        let cases: Vec<(MemoryError, ErrorClass)> = vec![
            (
                MemoryError::Redacted { field: "remedy" },
                ErrorClass::Invalid,
            ),
            (
                MemoryError::TooManyBytes {
                    field: "remedy",
                    bytes: 2,
                    limit: 1,
                },
                ErrorClass::Invalid,
            ),
            (
                MemoryError::TooManyChars {
                    field: "root cause",
                    chars: 2,
                    limit: 1,
                },
                ErrorClass::Invalid,
            ),
            (
                MemoryError::NamesAddress {
                    field: "remedy",
                    what: "path",
                },
                ErrorClass::Invalid,
            ),
            (
                MemoryError::EmptyScope { field: "caller" },
                ErrorClass::Invalid,
            ),
            (
                MemoryError::EmptyField {
                    field: "root cause",
                },
                ErrorClass::Invalid,
            ),
            (MemoryError::ScopeFull { limit: 1 }, ErrorClass::Rejected),
            (MemoryError::ProblemFull { limit: 1 }, ErrorClass::Rejected),
            (
                MemoryError::TooManyAnchors {
                    anchors: 2,
                    limit: 1,
                },
                ErrorClass::Invalid,
            ),
            (
                MemoryError::ProblemNotFound("p".to_owned()),
                ErrorClass::NotFound,
            ),
            (
                MemoryError::AttemptNotFound("a".to_owned(), "p".to_owned()),
                ErrorClass::NotFound,
            ),
            (
                MemoryError::Corrupt {
                    path: "p".to_owned(),
                    reason: "r".to_owned(),
                },
                ErrorClass::Corrupt,
            ),
            (
                MemoryError::backend(std::io::Error::other("disk")),
                ErrorClass::Transient,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.class(), expected, "{error}");
        }
    }

    #[test]
    fn the_marker_is_the_one_the_framework_uses() {
        assert_eq!(
            crate::REDACTION_MARKER,
            "[redacted: secret removed, do not reuse]"
        );
    }
}
