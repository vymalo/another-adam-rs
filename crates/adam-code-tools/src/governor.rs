//! What a command is held to before and while it runs: a seat of the process pool and the OS limits
//! of [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! (decisions 5 and 6).
//!
//! A [`Governor`] pairs the process's [`Admission`] with a `Confine`. A command asks for one seat of
//! [`Pool::Processes`], waits for it up to `Governor::wait`, and runs confined to the limits the
//! budget gives one command. A command that cannot get a seat in time is refused as `Busy`
//! (`RateLimited`), which the tool reports as a retryable error: a busy pool never fails a run.

use std::sync::Arc;
use std::time::Duration;

use adam_budget::{Admission, Ask, Budget, ConversationId, DynAdmission, Permit, Pool, Refused};
use adam_workspace::{DynConfine, ProcessLimits};

/// The largest file one command may write, when the deployment says nothing: 4 GiB.
pub const FILE_SIZE_MAX: u64 = 4 * 1024 * 1024 * 1024;

/// How long a command waits for a seat of the process pool before it is refused as busy.
pub const WAIT: Duration = Duration::from_secs(60);

/// The process pool and the OS limits of the commands of one process.
#[derive(Clone)]
pub struct Governor {
    admission: DynAdmission,
    confine: DynConfine,
    limits: ProcessLimits,
    wait: Duration,
}

impl Governor {
    /// A governor that asks `admission` for a seat of [`Pool::Processes`] and confines the command
    /// to `limits` with `confine`.
    #[must_use]
    pub fn new(admission: DynAdmission, confine: DynConfine, limits: ProcessLimits) -> Self {
        Self {
            admission,
            confine,
            limits,
            wait: WAIT,
        }
    }

    /// A governor whose limits are the budget's: the memory and the CPU one command may use, and
    /// [`FILE_SIZE_MAX`]. A ceiling the budget leaves unlimited (`u64::MAX`, `u32::MAX`) limits
    /// nothing.
    #[must_use]
    pub fn from_budget(admission: DynAdmission, confine: DynConfine, budget: &Budget) -> Self {
        Self::new(admission, confine, limits_of(budget))
    }

    /// How long a command waits for a seat.
    #[must_use]
    pub fn with_wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// The limits of one command.
    #[must_use]
    pub fn limits(&self) -> &ProcessLimits {
        &self.limits
    }

    /// The backend, to say what it enforces.
    #[must_use]
    pub fn confine(&self) -> &DynConfine {
        &self.confine
    }

    /// A seat of the process pool for `conversation`.
    ///
    /// # Errors
    ///
    /// [`Refused`] when no seat frees within the wait.
    pub async fn seat(&self, conversation: ConversationId) -> Result<Permit, Refused> {
        self.admission
            .admit(Ask::new(Pool::Processes, conversation).wait(self.wait))
            .await
    }
}

impl std::fmt::Debug for Governor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Governor")
            .field("limits", &self.limits)
            .field("wait", &self.wait)
            .field("enforces", &self.confine.describe())
            .finish()
    }
}

/// The limits of one command under `budget`.
#[must_use]
pub fn limits_of(budget: &Budget) -> ProcessLimits {
    let mut limits = ProcessLimits::new().with_file_size(FILE_SIZE_MAX);
    if budget.command_memory != u64::MAX {
        limits = limits.with_memory(budget.command_memory);
    }
    if budget.command_cpu != u32::MAX {
        limits = limits.with_cpu_millicores(budget.command_cpu);
    }
    limits
}

/// The governor behind a shared pointer, as a workspace hands it out.
pub type DynGovernor = Arc<Governor>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlimited_budget_limits_only_the_file_size() {
        let limits = limits_of(&Budget::UNLIMITED);
        assert_eq!(limits.memory, None);
        assert_eq!(limits.cpu_millicores, None);
        assert_eq!(limits.file_size, Some(FILE_SIZE_MAX));
    }

    #[test]
    fn a_budget_gives_a_command_its_memory_and_cpu() {
        let budget = Budget::builder()
            .command_memory(2 * 1024 * 1024 * 1024)
            .build();
        let limits = limits_of(&budget);
        assert_eq!(limits.memory, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(limits.cpu_millicores, None);
    }
}
