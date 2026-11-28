//! The Unix backends of [`Confine`](adam_workspace::Confine)
//! ([ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! decision 6).
//!
//! | Backend | What it enforces | Needs |
//! |---|---|---|
//! | [`Rlimits`] | file size, open files, CPU time, address space (Linux), `nice` 10, idle I/O priority (Linux), a process group of its own | any Unix; no rights |
//! | [`SystemdScope`] | `Rlimits` minus the address space, plus one transient scope per command with `MemoryMax`, `CPUQuota` and `TasksMax` | a systemd user manager that delegates the `memory`, `cpu` and `pids` controllers |
//!
//! [`best_effort`] picks: the scope where it works, else `Rlimits`, and `describe()` of what it
//! returns says what is not enforced. **Never `RLIMIT_NPROC`**: on Linux it counts every process of
//! the user, so a limit meant for one command would stop the person's desktop.
//!
//! # `unsafe`
//!
//! Setting a limit in the child, before its program runs, takes `Command::pre_exec`, which is
//! `unsafe`. That call is the only `unsafe` here, in `child::install`; the closure it installs makes
//! nothing but system calls (`getrlimit`, `setrlimit`, `setpriority`, `ioprio_set`), with no
//! allocation and no lock, as `pre_exec` requires. No other crate of adam contains `unsafe`.

#![cfg(unix)]
#![warn(missing_docs)]
#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]

mod child;
mod rlimits;
mod scope;

use std::sync::Arc;

use adam_workspace::DynConfine;

pub use rlimits::Rlimits;
pub use scope::SystemdScope;

/// The best backend this system has: [`SystemdScope`] when a user manager delegates the memory
/// controller, else [`Rlimits`]. Probes the system once (a command of a second or so), so call it
/// at start, not per command.
#[must_use]
pub fn best_effort() -> DynConfine {
    match SystemdScope::detect() {
        Some(scope) if scope.enforces_memory() => Arc::new(scope),
        Some(scope) => {
            tracing::info!(
                missing = scope.describe_missing(),
                "the systemd user manager does not delegate the memory controller; \
                 falling back to rlimits"
            );
            Arc::new(Rlimits::new())
        }
        None => {
            tracing::info!("no systemd user manager to make scopes in; falling back to rlimits");
            Arc::new(Rlimits::new())
        }
    }
}
