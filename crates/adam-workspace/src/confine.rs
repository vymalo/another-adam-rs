//! The OS limits on a command: the [`Confine`] port of
//! [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! decision 6, which sits beside [`Environment`](crate::Environment).
//!
//! A [`Confine`] gets a command that is about to start and the [`ProcessLimits`] of its pool. It may
//! rewrite the command (a transient `systemd` scope wraps it) or set what the child applies before
//! its program runs (rlimits, priority). It hands back a [`Confined`]: the handle that stops what the
//! command started and says, once the command ended, whether the operating system stopped it for a
//! limit ([`Stopped`]). A command stopped that way is **not an error**: the tool says so and the run
//! goes on.
//!
//! This crate holds the port and [`NoConfine`], which limits nothing and says so. The Unix backends
//! (`Rlimits`, `SystemdScope`) are in `adam-confine`, because setting a limit in the child needs
//! `pre_exec`, which is `unsafe`, and this crate forbids that.

use std::fmt;
use std::process::ExitStatus;
use std::sync::Arc;

use adam_error::{Classify, ErrorClass};
use tokio::process::Command;

/// What one command may use. A field left `None` is not limited.
///
/// Built with [`ProcessLimits::new`] and the `with_` methods; `#[non_exhaustive]`, so a limit a later
/// backend needs is not a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ProcessLimits {
    /// Memory, in bytes (`MemoryMax` of a scope; the address space where only rlimits exist).
    pub memory: Option<u64>,
    /// CPU, in thousandths of a core (`1500` is a core and a half).
    pub cpu_millicores: Option<u32>,
    /// Processes and threads the command may have at once (`TasksMax`). Never an `RLIMIT_NPROC`: on
    /// Linux that counts every process of the user.
    pub max_processes: Option<u32>,
    /// The largest file the command may write, in bytes (`RLIMIT_FSIZE`).
    pub file_size: Option<u64>,
    /// The CPU time the command may use in all, in seconds (`RLIMIT_CPU`): a backstop for a backend
    /// that cannot cap the rate.
    pub cpu_seconds: Option<u64>,
}

impl ProcessLimits {
    /// No limit at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Memory, in bytes.
    #[must_use]
    pub fn with_memory(mut self, bytes: u64) -> Self {
        self.memory = Some(bytes);
        self
    }

    /// CPU, in millicores.
    #[must_use]
    pub fn with_cpu_millicores(mut self, millicores: u32) -> Self {
        self.cpu_millicores = Some(millicores);
        self
    }

    /// Processes and threads at once.
    #[must_use]
    pub fn with_max_processes(mut self, n: u32) -> Self {
        self.max_processes = Some(n);
        self
    }

    /// The largest file, in bytes.
    #[must_use]
    pub fn with_file_size(mut self, bytes: u64) -> Self {
        self.file_size = Some(bytes);
        self
    }

    /// The CPU time in all, in seconds.
    #[must_use]
    pub fn with_cpu_seconds(mut self, seconds: u64) -> Self {
        self.cpu_seconds = Some(seconds);
        self
    }
}

/// How much of [`ProcessLimits`] a [`Confine`] enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Enforcement {
    /// Every limit is enforced.
    Full,
    /// Some limit is not; the list says which, in words a person can read (`"cpu share"`).
    Partial(Vec<&'static str>),
    /// Nothing is enforced.
    None,
}

impl fmt::Display for Enforcement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("full"),
            Self::Partial(missing) => write!(f, "partial (not enforced: {})", missing.join(", ")),
            Self::None => f.write_str("none"),
        }
    }
}

/// Why a command was stopped by a limit: what the model is told, never an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stopped {
    /// The command used more memory than its limit; `limit` is in bytes.
    Memory {
        /// The limit it was held to, in bytes.
        limit: u64,
    },
    /// The command used more CPU time than its limit; `seconds` is the limit.
    Cpu {
        /// The CPU time it was held to, in seconds.
        seconds: u64,
    },
    /// The command wrote a file over its limit; `limit` is in bytes.
    FileSize {
        /// The limit it was held to, in bytes.
        limit: u64,
    },
}

impl Stopped {
    /// The label of the reason in a metric (`adam_process_stopped_total{reason}`).
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Memory { .. } => "memory",
            Self::Cpu { .. } => "cpu",
            Self::FileSize { .. } => "file_size",
        }
    }
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory { limit } => write!(f, "memory limit {}", human_bytes(*limit)),
            Self::Cpu { seconds } => write!(f, "cpu time limit {seconds} s"),
            Self::FileSize { limit } => write!(f, "file size limit {}", human_bytes(*limit)),
        }
    }
}

/// `2147483648` as `2 GB`, `536870912` as `512 MB`: the largest unit that divides the value, else
/// the nearest tenth. Binary units, written the short way a person reads.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GB", 1 << 30),
        ("MB", 1 << 20),
        ("KB", 1 << 10),
        ("bytes", 1),
    ];
    for (name, size) in UNITS {
        if bytes >= size {
            if bytes.is_multiple_of(size) || size == 1 {
                return format!("{} {name}", bytes / size);
            }
            let tenths = bytes.saturating_mul(10) / size;
            return format!("{}.{} {name}", tenths / 10, tenths % 10);
        }
    }
    "0 bytes".to_owned()
}

/// Why a command could not be confined: the command is not started.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfineError {
    /// The limits cannot be applied here (the backend is not available on this system).
    #[error("the limits cannot be applied: {0}")]
    Unavailable(String),
    /// The limits are not valid (a value the system cannot take).
    #[error("invalid limits: {0}")]
    Invalid(String),
}

impl Classify for ConfineError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Unavailable(_) => ErrorClass::Unsupported,
            Self::Invalid(_) => ErrorClass::Invalid,
        }
    }
}

/// How a confined command ended, for [`Confined::finish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    /// The exit code, `None` when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended it, on Unix.
    pub signal: Option<i32>,
}

impl From<ExitStatus> for Ended {
    fn from(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: status.code(),
            signal,
        }
    }
}

/// What a backend keeps for one confined command: it stops what the command started, and says why
/// the operating system ended it. A backend implements it for what it made.
#[async_trait::async_trait]
pub trait Confinement: Send + Sync {
    /// Stop everything the command started, however it hid from its process group. Safe to call
    /// more than once, and when nothing is left.
    async fn stop(&self);

    /// The command ended: whether a limit stopped it, and clean what the backend made. Called once.
    async fn finish(&self, limits: &ProcessLimits, ended: Ended) -> Option<Stopped>;
}

/// The handle of one confined command: [`stop`](Self::stop) it, and [`finish`](Self::finish) it when
/// it ended.
pub struct Confined {
    limits: ProcessLimits,
    inner: Option<Arc<dyn Confinement>>,
}

impl Confined {
    /// A command nothing holds beyond its process group ([`NoConfine`]).
    #[must_use]
    pub fn unconfined() -> Self {
        Self {
            limits: ProcessLimits::new(),
            inner: None,
        }
    }

    /// A command held by `inner`, to `limits`.
    #[must_use]
    pub fn held(limits: ProcessLimits, inner: Arc<dyn Confinement>) -> Self {
        Self {
            limits,
            inner: Some(inner),
        }
    }

    /// The limits the command was held to.
    #[must_use]
    pub fn limits(&self) -> &ProcessLimits {
        &self.limits
    }

    /// Stop everything the command started.
    pub async fn stop(&self) {
        if let Some(inner) = &self.inner {
            inner.stop().await;
        }
    }

    /// The command ended: whether a limit stopped it. A command a timeout or a cancel killed is
    /// not asked: the caller knows why that one ended.
    pub async fn finish(&self, ended: Ended) -> Option<Stopped> {
        match &self.inner {
            Some(inner) => inner.finish(&self.limits, ended).await,
            None => None,
        }
    }
}

impl fmt::Debug for Confined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Confined")
            .field("limits", &self.limits)
            .field("held", &self.inner.is_some())
            .finish()
    }
}

/// The port that limits what a command may take of the machine.
///
/// Call [`confine`](Self::confine) on a fresh command (one made from
/// [`PreparedCommand::command`](crate::PreparedCommand::command)) **before** setting its standard
/// streams, process group and `kill_on_drop`: a backend that wraps the command makes a new one and
/// copies the program, arguments, working directory and variables set on it, and nothing else.
///
/// Run `adam_confine_testkit::confine_conformance!` over an implementation.
pub trait Confine: Send + Sync {
    /// Apply `limits` to `cmd`, which is not started yet.
    ///
    /// # Errors
    ///
    /// [`ConfineError`] when the limits cannot be applied: the command is not started.
    fn confine(&self, cmd: &mut Command, limits: &ProcessLimits) -> Result<Confined, ConfineError>;

    /// What this backend enforces, so that a deployment and the person can be told, and
    /// [`Enforcement::Partial`] names what is not.
    fn describe(&self) -> Enforcement;
}

/// A shared [`Confine`].
pub type DynConfine = Arc<dyn Confine>;

/// A [`Confine`] that limits nothing: for an environment that runs a command somewhere else (a
/// pod has its own limits), for a test, and for a host with no backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoConfine;

impl Confine for NoConfine {
    fn confine(
        &self,
        _cmd: &mut Command,
        _limits: &ProcessLimits,
    ) -> Result<Confined, ConfineError> {
        Ok(Confined::unconfined())
    }

    fn describe(&self) -> Enforcement {
        Enforcement::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_said_the_way_a_person_reads_them() {
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2 GB");
        assert_eq!(human_bytes(512 * 1024 * 1024), "512 MB");
        assert_eq!(human_bytes(1536 * 1024 * 1024), "1.5 GB");
        assert_eq!(human_bytes(100), "100 bytes");
        assert_eq!(human_bytes(0), "0 bytes");
    }

    #[test]
    fn a_stop_names_the_limit() {
        let stopped = Stopped::Memory {
            limit: 2 * 1024 * 1024 * 1024,
        };
        assert_eq!(stopped.to_string(), "memory limit 2 GB");
        assert_eq!(stopped.reason(), "memory");
    }

    #[test]
    fn partial_names_what_is_missing() {
        let e = Enforcement::Partial(vec!["cpu share", "process count"]);
        assert_eq!(
            e.to_string(),
            "partial (not enforced: cpu share, process count)"
        );
    }

    #[test]
    fn no_confine_says_it_enforces_nothing() {
        let mut cmd = Command::new("true");
        let confined = NoConfine
            .confine(&mut cmd, &ProcessLimits::new().with_memory(1))
            .unwrap();
        assert_eq!(NoConfine.describe(), Enforcement::None);
        assert!(confined.inner.is_none());
    }
}
