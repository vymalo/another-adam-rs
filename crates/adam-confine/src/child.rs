//! What the child does between `fork` and `exec`. The only `unsafe` of the crate is here.

#![allow(unsafe_code)]

use std::io;

use rustix::process::{Resource, Rlimit, getrlimit, setpriority_process, setrlimit};
use tokio::process::Command;

/// The nice value commands start with: low, so a build never competes with the person's screen.
pub(crate) const NICE: i32 = 10;

/// The most files a command may have open, when its own ceiling is higher.
pub(crate) const NOFILE: u64 = 8192;

/// What the child sets before its program runs.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Plan {
    pub(crate) file_size: Option<u64>,
    pub(crate) cpu_seconds: Option<u64>,
    pub(crate) address_space: Option<u64>,
    pub(crate) nofile: Option<u64>,
    pub(crate) nice: Option<i32>,
    pub(crate) idle_io: bool,
}

/// Lower `resource` to at most `wanted`, never raising a ceiling the child already has.
fn lower(resource: Resource, wanted: u64) -> io::Result<()> {
    set(resource, wanted, wanted)
}

/// Set the soft ceiling to at most `soft` and the hard one to at most `hard`, never raising a
/// ceiling the child already has.
fn set(resource: Resource, soft: u64, hard: u64) -> io::Result<()> {
    let have = getrlimit(resource);
    let hard = have.maximum.map_or(hard, |max| max.min(hard));
    setrlimit(
        resource,
        Rlimit {
            current: Some(soft.min(hard)),
            maximum: Some(hard),
        },
    )
    .map_err(io::Error::from)
}

/// Seconds between the soft `RLIMIT_CPU` (`SIGXCPU`, which a command can be told by) and the hard
/// one (`SIGKILL`, when it ignores the first).
const CPU_GRACE: u64 = 5;

fn apply(plan: &Plan) -> io::Result<()> {
    if let Some(n) = plan.file_size {
        lower(Resource::Fsize, n)?;
    }
    if let Some(n) = plan.cpu_seconds {
        set(Resource::Cpu, n, n.saturating_add(CPU_GRACE))?;
    }
    #[cfg(target_os = "linux")]
    if let Some(n) = plan.address_space {
        lower(Resource::As, n)?;
    }
    if let Some(n) = plan.nofile {
        lower(Resource::Nofile, n)?;
    }
    if let Some(nice) = plan.nice {
        setpriority_process(None, nice).map_err(io::Error::from)?;
    }
    #[cfg(target_os = "linux")]
    if plan.idle_io {
        // The idle class (3) of `ioprio_set`, for this process (who 1, id 0). Best effort: a kernel
        // or a scheduler without it leaves the priority as it was.
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE_SHIFTED: libc::c_long = 3 << 13;
        // SAFETY: a plain system call with integer arguments, and no memory is read or written.
        unsafe {
            libc::syscall(
                libc::SYS_ioprio_set,
                IOPRIO_WHO_PROCESS,
                0 as libc::c_long,
                IOPRIO_CLASS_IDLE_SHIFTED,
            );
        }
    }
    Ok(())
}

/// Make `cmd` set `plan` in the child, and start it in a process group of its own.
pub(crate) fn install(cmd: &mut Command, plan: Plan) {
    cmd.process_group(0);
    // SAFETY: `apply` makes only system calls (`getrlimit`, `setrlimit`, `setpriority`,
    // `ioprio_set`) on values it owns: no allocation, no lock and no panic, which is what `pre_exec`
    // requires of code that runs after `fork` in a process that has other threads.
    unsafe {
        cmd.pre_exec(move || apply(&plan));
    }
}
