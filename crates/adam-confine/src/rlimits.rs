//! The fallback every Unix has: limits the child sets for itself.

use std::sync::Arc;

use adam_workspace::{
    Confine, ConfineError, Confined, Confinement, Ended, Enforcement, ProcessLimits, Stopped,
};
use async_trait::async_trait;
use tokio::process::Command;

use crate::child::{self, NICE, NOFILE, Plan};

/// `SIGXCPU`, which the kernel sends at the soft `RLIMIT_CPU`.
const SIGXCPU: i32 = 24;
/// `SIGXFSZ`, which the kernel sends when a file would pass `RLIMIT_FSIZE`.
const SIGXFSZ: i32 = 25;

/// Limits the child sets for itself: `RLIMIT_FSIZE`, `RLIMIT_NOFILE`, `RLIMIT_CPU`, `RLIMIT_AS` on
/// Linux, `nice` 10, idle I/O priority on Linux, and a process group of its own that the caller
/// kills as one.
///
/// Not enforced: the CPU **rate** and the number of processes (there is no per-command
/// `RLIMIT_NPROC`, and the real one would count every process of the user), and **resident**
/// memory: `RLIMIT_AS` caps the address space, which is larger than what a process holds, so a
/// runtime that reserves a lot of address space (a JVM, Node) needs a higher memory limit here than
/// under a scope. [`describe`](Confine::describe) says so.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rlimits {
    _private: (),
}

impl Rlimits {
    /// The rlimits backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// The plan for `limits`; the address space only where `address_space` says it is wanted.
pub(crate) fn plan(limits: &ProcessLimits, address_space: bool) -> Plan {
    Plan {
        file_size: limits.file_size,
        cpu_seconds: limits.cpu_seconds,
        address_space: address_space.then_some(limits.memory).flatten(),
        nofile: Some(NOFILE),
        nice: Some(NICE),
        idle_io: true,
    }
}

/// A limit that ended the command by a signal, or by a shell that reports one as `128 + signal`.
pub(crate) fn stopped_by_rlimit(limits: &ProcessLimits, ended: Ended) -> Option<Stopped> {
    let signal = ended
        .signal
        .or_else(|| ended.code.and_then(|c| c.checked_sub(128)));
    match signal? {
        SIGXFSZ => limits.file_size.map(|limit| Stopped::FileSize { limit }),
        SIGXCPU => limits.cpu_seconds.map(|seconds| Stopped::Cpu { seconds }),
        _ => None,
    }
}

struct Held;

#[async_trait]
impl Confinement for Held {
    async fn stop(&self) {}

    async fn finish(&self, limits: &ProcessLimits, ended: Ended) -> Option<Stopped> {
        stopped_by_rlimit(limits, ended)
    }
}

impl Confine for Rlimits {
    fn confine(&self, cmd: &mut Command, limits: &ProcessLimits) -> Result<Confined, ConfineError> {
        child::install(cmd, plan(limits, true));
        Ok(Confined::held(*limits, Arc::new(Held)))
    }

    fn describe(&self) -> Enforcement {
        Enforcement::Partial(vec![
            "cpu share",
            "process count",
            "resident memory (the address space is capped instead)",
        ])
    }
}
