//! The sweep of the workspaces of finished runs.
//!
//! A run's workspace (slots of worktrees and scratch projects, [ADR 0008](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0008-a-workspace-holds-several-repositories.md))
//! lives as long as the run: a run is an A2A task, it lives while it is parked on a question, and it
//! ends at a pull request, a failure or a cancel. Nothing removed a worktree before; the janitor does.
//!
//! Every `WORKSPACE_SWEEP_SECS` ([`crate::config`]; 300 by default, and once at startup) it walks the
//! runs that have a workspace on this worker's volume ([`Workspaces::runs`]) and asks the store
//! about each one:
//!
//! * the run is **open** (`runnable` or `parked`): its workspace stays, whatever it holds. A run
//!   that waits for the person's answer for a week keeps its workspace for a week;
//! * the run is **finished** (`done`, `failed`, which a cancel is) or the store **does not know it**
//!   (another database, a purged run): its workspace is removed, every slot of it. The run's notes
//!   stay (`<root>/coder/<run>.json`) and so do its `agent/*` branches in the mirrors, the only copy
//!   of any unpushed commit;
//! * a directory whose name is not a run id is not the janitor's and is left alone.
//!
//! An error (the store is down, a volume refuses a removal) is logged and the sweep goes on; it never
//! stops the process. The janitor is a worker component of the host (`Agents::worker_component` of
//! `adam-service`), so it runs in the `all` and `worker` roles and stops with the workers.

use std::time::Duration;

use adam_core::{RunId, Store};
use adam_error::BoxError;
use adam_workspace::Workspaces;
use tokio_util::sync::CancellationToken;

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// The runs whose workspace was removed.
    pub removed: Vec<String>,
    /// How many were left: runs that are open, and directories that are not runs.
    pub kept: usize,
    /// The runs the sweep could not decide or remove (the error is in the log).
    pub failed: Vec<String>,
}

/// Removes the workspaces of runs that are over. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Janitor {
    workspaces: Workspaces,
    every: Option<Duration>,
}

impl Janitor {
    /// A janitor of `workspaces` that sweeps every `every`; `None` never sweeps (it still runs as a
    /// component, and waits to be stopped).
    pub fn new(workspaces: Workspaces, every: Option<Duration>) -> Self {
        Self { workspaces, every }
    }

    /// One sweep: the workspaces of finished and unknown runs are removed. Stops early, between
    /// two runs, when `stop` is cancelled.
    pub async fn sweep(&self, store: &dyn Store, stop: &CancellationToken) -> Sweep {
        let mut report = Sweep::default();
        let runs = match self.workspaces.runs().await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(error = %adam_error::report(&e), "cannot list the workspaces to sweep");
                return report;
            }
        };
        for run in runs {
            if stop.is_cancelled() {
                break;
            }
            // A directory that is not a run id is not ours.
            let Some(id) = run.parse().ok().map(RunId) else {
                report.kept += 1;
                continue;
            };
            let over = match store.load_run(id).await {
                Ok(Some(record)) => record.status.is_terminal(),
                Ok(None) => true,
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot ask the store about a run; its workspace stays");
                    report.failed.push(run);
                    continue;
                }
            };
            if !over {
                report.kept += 1;
                continue;
            }
            let removed = match self.workspaces.run(&run) {
                Ok(workspace) => workspace.remove().await,
                Err(e) => Err(e),
            };
            match removed {
                Ok(()) => {
                    tracing::info!(%run, "removed the workspace of a finished run");
                    report.removed.push(run);
                }
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot remove the workspace of a finished run");
                    report.failed.push(run);
                }
            }
        }
        report
    }

    /// The host component: a sweep at startup and then one every interval, until `stop` is
    /// cancelled. Never fails: what goes wrong is logged. Without an interval it only waits for
    /// `stop`.
    ///
    /// # Errors
    ///
    /// Never; the type is the component's.
    pub async fn run(
        self,
        store: adam_core::DynStore,
        stop: CancellationToken,
    ) -> Result<(), BoxError> {
        let Some(every) = self.every else {
            tracing::info!("the sweep of finished workspaces is off (WORKSPACE_SWEEP_SECS=0)");
            stop.cancelled().await;
            return Ok(());
        };
        loop {
            let report = self.sweep(store.as_ref(), &stop).await;
            if !report.removed.is_empty() || !report.failed.is_empty() {
                tracing::info!(
                    removed = report.removed.len(),
                    kept = report.kept,
                    failed = report.failed.len(),
                    "swept the workspaces of finished runs"
                );
            }
            tokio::select! {
                () = stop.cancelled() => return Ok(()),
                () = tokio::time::sleep(every) => {}
            }
        }
    }
}
