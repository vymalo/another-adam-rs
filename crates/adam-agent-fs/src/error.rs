//! The errors of this crate.
//!
//! A problem *in the files* is a [`Diagnostic`], not an error: loading collects them all so that
//! one run reports every mistake. An [`Error`] is for the two things that stop a load: the
//! source cannot be read, or the caller asked for a result and there are errors in it.

use std::io;
use std::path::PathBuf;

use adam_error::{Classify, ErrorClass};

use crate::Diagnostic;

/// Why a load or a check failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory could not be read.
    #[error("cannot read {}", path.display())]
    Io {
        /// The path that failed, as given to the source.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The files loaded, but they contain errors (or warnings, under
    /// [`Strictness::Strict`](crate::Strictness::Strict)).
    #[error("{}", summary(diagnostics))]
    Invalid {
        /// Every finding, in discovery order (not only the failing ones).
        diagnostics: Vec<Diagnostic>,
    },
    /// The manifest could not be encoded for its digest, or an embedded manifest could not be
    /// decoded: the generated code and this crate disagree about the schema.
    #[error("cannot {action} the manifest of `{what}`")]
    Codec {
        /// `encode` or `decode`.
        action: &'static str,
        /// The agent, file or field concerned.
        what: String,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },
}

fn summary(diagnostics: &[Diagnostic]) -> String {
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let first = diagnostics
        .iter()
        .find(|d| d.is_error())
        .or_else(|| diagnostics.first());
    match first {
        Some(first) if errors > 0 => {
            format!("{errors} error(s) in the agent files; first: {first}")
        }
        Some(first) => format!(
            "{} warning(s) in the agent files, refused by a strict build; first: {first}",
            diagnostics.len()
        ),
        None => "invalid agent files".to_owned(),
    }
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                ErrorClass::NotFound
            }
            Self::Io { .. } | Self::Codec { .. } => ErrorClass::Internal,
            Self::Invalid { .. } => ErrorClass::Invalid,
        }
    }
}

impl Error {
    pub(crate) fn codec(
        action: &'static str,
        what: impl Into<String>,
        source: serde_json::Error,
    ) -> Self {
        Self::Codec {
            action,
            what: what.into(),
            source,
        }
    }

    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
