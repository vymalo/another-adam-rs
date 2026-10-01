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
//!   (another database, a purged run): what its environment holds is released
//!   ([`Environment::release`](adam_workspace::Environment::release); nothing, for the coder's own
//!   container), **then** its workspace is removed, every slot of it. A release that fails leaves the
//!   workspace for the next sweep: an environment that still holds the files is not left without
//!   them. The run's notes stay (`<root>/coder/<run>.json`) and so do its `agent/*` branches in the
//!   mirrors, the only copy of any unpushed commit;
//! * a directory whose name is not a run id is not the janitor's and is left alone.
//!
//! After that the sweep asks the environment what it still holds
//! ([`held_runs`](adam_workspace::Environment::held_runs)) and releases what belongs to a run that is
//! over or unknown, whether or not the run has a workspace on this volume: what a crash left behind.
//!
//! An error (the store is down, a volume refuses a removal) is logged and the sweep goes on; it never
//! stops the process. The janitor is a worker component of the host (`Agents::worker_component` of
//! `adam-service`), so it runs in the `all` and `worker` roles and stops with the workers.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use adam_core::{RunId, Store, StoreError};
use adam_error::BoxError;
use adam_workspace::{DynEnvironment, Local, Workspaces};
use tokio_util::sync::CancellationToken;

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// The runs whose workspace was removed.
    pub removed: Vec<String>,
    /// How many were left: runs that are open, and directories that are not runs.
    pub kept: usize,
    /// The runs the sweep could not decide, release or remove (the error is in the log).
    pub failed: Vec<String>,
    /// The runs whose environment was released although the run has no workspace on this volume
    /// (what a crash left behind).
    pub orphans: Vec<String>,
}

/// Removes the workspaces of runs that are over. See the [module documentation](self).
#[derive(Clone)]
pub struct Janitor {
    workspaces: Workspaces,
    every: Option<Duration>,
    environment: DynEnvironment,
}

impl std::fmt::Debug for Janitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Janitor")
            .field("workspaces", &self.workspaces)
            .field("every", &self.every)
            .finish_non_exhaustive()
    }
}

impl Janitor {
    /// A janitor of `workspaces` that sweeps every `every`; `None` never sweeps (it still runs as a
    /// component, and waits to be stopped).
    pub fn new(workspaces: Workspaces, every: Option<Duration>) -> Self {
        Self {
            workspaces,
            every,
            environment: Arc::new(Local),
        }
    }

    /// Release what `environment` holds for a run before its workspace is removed, and sweep what
    /// it holds for runs that are over. It must be the environment the tools run the processes of
    /// runs in ([`ToolEnv::with_environment`](crate::tools::ToolEnv::with_environment)); the default is
    /// [`Local`], which holds nothing.
    #[must_use]
    pub fn with_environment(mut self, environment: DynEnvironment) -> Self {
        self.environment = environment;
        self
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
        let mut released = HashSet::new();
        for run in runs {
            if stop.is_cancelled() {
                break;
            }
            match is_over(store, &run).await {
                // A directory that is not a run id is not ours.
                Ok(None) => report.kept += 1,
                Ok(Some(false)) => report.kept += 1,
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot ask the store about a run; its workspace stays");
                    report.failed.push(run);
                }
                Ok(Some(true)) => {
                    // What the environment holds goes first, and a workspace it could not let go of
                    // stays for the next sweep.
                    if let Err(e) = self.environment.release(&run).await {
                        tracing::warn!(%run, error = %adam_error::report(&e), "cannot release the environment of a finished run; its workspace stays");
                        report.failed.push(run);
                        continue;
                    }
                    released.insert(run.clone());
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
            }
        }
        self.release_orphans(store, stop, &released, &mut report)
            .await;
        report
    }

    /// Release what the environment holds for runs that are over and that the loop above did not
    /// see: the run has no workspace on this volume (it was removed, or the crash was in between).
    async fn release_orphans(
        &self,
        store: &dyn Store,
        stop: &CancellationToken,
        released: &HashSet<String>,
        report: &mut Sweep,
    ) {
        let held = match self.environment.held_runs().await {
            Ok(held) => held,
            Err(e) => {
                tracing::warn!(error = %adam_error::report(&e), "cannot list what the environment holds");
                return;
            }
        };
        for run in held {
            if stop.is_cancelled() {
                break;
            }
            if released.contains(&run) || report.failed.contains(&run) {
                continue;
            }
            match is_over(store, &run).await {
                Ok(Some(true)) => match self.environment.release(&run).await {
                    Ok(()) => {
                        tracing::info!(%run, "released the environment of a finished run that has no workspace here");
                        report.orphans.push(run);
                    }
                    Err(e) => {
                        tracing::warn!(%run, error = %adam_error::report(&e), "cannot release the environment of a finished run");
                        report.failed.push(run);
                    }
                },
                Ok(Some(false) | None) => {}
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot ask the store about a run; its environment stays");
                    report.failed.push(run);
                }
            }
        }
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

/// Whether `run` is over: `Some(true)` when it is finished or the store does not know it,
/// `Some(false)` when it is open (`runnable` or `parked`), `None` when `run` is not a run id.
async fn is_over(store: &dyn Store, run: &str) -> Result<Option<bool>, StoreError> {
    let Some(id) = run.parse().ok().map(RunId) else {
        return Ok(None);
    };
    Ok(Some(match store.load_run(id).await? {
        Some(record) => record.status.is_terminal(),
        None => true,
    }))
}
