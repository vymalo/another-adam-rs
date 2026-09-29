//! What validation reports: a severity, a place and a message.

use std::fmt;
use std::path::{Path, PathBuf};

/// How bad a finding is.
///
/// An [`Error`](Severity::Error) means the file (or item) was skipped or cannot be used, and a
/// build should fail. A [`Warning`](Severity::Warning) means the item was kept; a strict build
/// ([`Strictness::Strict`](crate::Strictness::Strict)) treats it as an error too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Kept, but worth fixing (an unknown key, a spec violation the client guide tolerates).
    Warning,
    /// Skipped or unusable.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

/// One finding about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Error or warning.
    pub severity: Severity,
    /// The file (or directory) the finding is about, relative to the root of the source.
    pub path: PathBuf,
    /// The 1-based line, when the finding points at one.
    pub line: Option<u32>,
    /// What is wrong and, where possible, what to do.
    pub message: String,
}

impl Diagnostic {
    /// An error about `path`.
    pub fn error(path: impl Into<PathBuf>, line: Option<u32>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            path: path.into(),
            line,
            message: message.into(),
        }
    }

    /// A warning about `path`.
    pub fn warning(
        path: impl Into<PathBuf>,
        line: Option<u32>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            path: path.into(),
            line,
            message: message.into(),
        }
    }

    /// Whether this is an [`Error`](Severity::Error).
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    /// `path:line: severity: message`, the shape editors and CI annotate.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        write!(f, ": {}: {}", self.severity, self.message)
    }
}

/// The collector the loaders write into. Crate-private: callers get a `Vec<Diagnostic>`.
pub(crate) struct Sink<'a> {
    pub(crate) out: &'a mut Vec<Diagnostic>,
    pub(crate) path: &'a Path,
}

impl Sink<'_> {
    pub(crate) fn error(&mut self, line: Option<u32>, message: impl Into<String>) {
        self.out.push(Diagnostic::error(self.path, line, message));
    }

    pub(crate) fn warn(&mut self, line: Option<u32>, message: impl Into<String>) {
        self.out.push(Diagnostic::warning(self.path, line, message));
    }
}
