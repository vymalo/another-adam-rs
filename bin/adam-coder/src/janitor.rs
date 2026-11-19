//! The sweep of the workspaces of finished runs.
//!
//! A run's workspace (slots of worktrees and scratch projects, [ADR 0008](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0008-a-workspace-holds-several-repositories.md))
//! lives as long as the run: a run is an A2A task, it lives while it is parked on a question, and it
//! ends at a pull request, a failure or a cancel. Nothing removed a worktree before; the janitor does.
//!
//! Every `WORKSPACE_SWEEP_SECS` ([`crate::config`]; 300 by default, and once at startup) it walks the
//! runs that have a workspace on this worker's volume ([`Workspaces::runs`]) and asks the store
//! about each one, once: what that read learned is reused by the later passes of the same sweep.
//! The record's `status` is enough, so no state is decoded; the read still transfers the whole
//! record, because a status projection is not in this tree yet (P3 of the 2026-11-18 optimisation
//! review). What the store answers is:
//!
//! * the run is **open** (`runnable` or `parked`): its workspace stays, whatever it holds. A run
//!   that waits for the person's answer for a week keeps its workspace for a week;
//! * the run is **finished** (`done`, `failed`, which a cancel is) or the store **does not know it**
//!   (another database, a purged run): what its environment holds is released
//!   ([`Environment::release`](adam_workspace::Environment::release); nothing, for the coder's own
//!   container), **then** its workspace is removed, every slot of it. A release that fails leaves the
//!   workspace for the next sweep: an environment that still holds the files is not left without
//!   them. The run's notes stay (`<root>/coder/<run>.json`) and so do its `agent/*` branches in the
//!   mirrors, the only copy of any unpushed commit. **Its scratch projects stay**, though, while
//!   its conversation is alive (`SCRATCH_IDLE_DAYS`, [`Janitor::with_scratch_idle`]): a scratch
//!   project lives for the thread, the next task of the conversation takes it over
//!   ([`RunWorkspace::adopt_scratch`](adam_workspace::RunWorkspace::adopt_scratch)), and it is
//!   removed once the conversation has had no task for the idle time (an open task, or one that
//!   finished recently, keeps it). A run the store does not know has no conversation to ask about,
//!   and a project made without one belongs to its run: both go with it;
//! * a directory whose name is not a run id is not the janitor's and is left alone.
//!
//! The kept output of a finished run (`<root>/outputs/<run>`, [`crate::kept`]) is removed with its
//! workspace, and a sweep removes the outputs of finished runs that have no workspace any more.
//!
//! The images of a conversation (`<root>/images/<scope>`, [`crate::images`]) have a sweep of their
//! own, after the workspaces, because a scope can outlive every workspace (a crash between the two
//! removals, a workspace removed by hand, a conversation that only chatted). The run that kept the
//! images noted itself in the scope, so the sweep asks the store about **that run's conversation**:
//! a scope whose conversation has an open run is never removed, whatever `SCRATCH_IDLE_DAYS` is
//! (`0` included); one whose conversation has had no task for the idle time scratch projects use
//! goes. A scope that cannot be placed (no note, a run the store does not know, a note that does not
//! lead to the scope) is removed when nothing in it was written for [`IMAGE_SCOPE_BACKSTOP`] (30
//! days). When the store cannot answer, the scope stays.
//!
//! After that the sweep asks the environment what it still holds
//! ([`held_runs`](adam_workspace::Environment::held_runs)) and releases what belongs to a run that is
//! over or unknown, whether or not the run has a workspace on this volume: what a crash left behind.
//!
//! An error (the store is down, a volume refuses a removal) is logged and the sweep goes on; it never
//! stops the process. The janitor is a worker component of the host (`Agents::worker_component` of
//! `adam-service`), so it runs in the `all` and `worker` roles and stops with the workers.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adam_core::{ConversationScope, RunId, RunQuery, RunRecord, Store, StoreError};
use adam_error::BoxError;
use adam_workspace::{DynEnvironment, Local, Workspaces};
use tokio_util::sync::CancellationToken;

use crate::images::FileImages;
use crate::kept::FileKept;
use crate::redact::Redactor;
use crate::tools::Commands;

/// How long a scope of images that the sweep cannot place stays after anything in it was last
/// written.
pub const IMAGE_SCOPE_BACKSTOP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// What one sweep learned about a run from its record's status: `Some(true)` over, `Some(false)`
/// open, `None` not a run id. A later pass of the same sweep reuses it instead of asking the store
/// again; the state is never decoded (the record's `status` is enough).
type Verdicts = HashMap<String, Option<bool>>;

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    /// The runs whose workspace was removed.
    pub removed: Vec<String>,
    /// How many were left: runs that are open, and directories that are not runs.
    pub kept: usize,
    /// The runs the sweep could not decide, release or remove (the error is in the log).
    pub failed: Vec<String>,
    /// The finished runs whose worktrees were removed and whose scratch projects stay, because their
    /// conversation is still alive. (Not in `kept`, which counts runs the sweep left whole.)
    pub kept_scratch: Vec<String>,
    /// The runs whose environment was released although the run has no workspace on this volume
    /// (what a crash left behind).
    pub orphans: Vec<String>,
    /// The scopes of images the sweep removed.
    pub images: Vec<String>,
}

/// Removes the workspaces of runs that are over. See the [module documentation](self).
#[derive(Clone)]
pub struct Janitor {
    workspaces: Workspaces,
    kept: FileKept,
    images: FileImages,
    every: Option<Duration>,
    environment: DynEnvironment,
    scratch_idle: Option<Duration>,
    /// The commands that keep running, stopped before what holds them is released.
    commands: Commands,
    /// Runs whose environment was released and whose scratch projects stayed, so that the sweeps
    /// after the first do not ask the environment again. Lost with the process, which only costs
    /// one more (idempotent) release.
    released: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for Janitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Janitor")
            .field("workspaces", &self.workspaces)
            .field("every", &self.every)
            .field("scratch_idle", &self.scratch_idle)
            .finish_non_exhaustive()
    }
}

impl Janitor {
    /// A janitor of `workspaces` that sweeps every `every`; `None` never sweeps (it still runs as a
    /// component, and waits to be stopped).
    pub fn new(workspaces: Workspaces, every: Option<Duration>) -> Self {
        Self {
            kept: FileKept::new(workspaces.root(), Redactor::default()),
            images: FileImages::new(workspaces.root()),
            workspaces,
            every,
            environment: Arc::new(Local),
            scratch_idle: None,
            commands: Commands::new(),
            released: Arc::default(),
        }
    }

    /// Keep the scratch projects of a finished run while its conversation has had a task within
    /// `idle` (`SCRATCH_IDLE_DAYS`), and remove them when it has not. `None`, the default, removes a
    /// scratch project with its run.
    #[must_use]
    pub fn with_scratch_idle(mut self, idle: Option<Duration>) -> Self {
        self.scratch_idle = idle;
        self
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

    /// Stop the commands that keep running of the runs this janitor releases, before the environment
    /// that holds them is released (`ToolEnv::commands`): the backstop for a process whose run ended
    /// and was not seen to.
    #[must_use]
    pub fn with_commands(mut self, commands: Commands) -> Self {
        self.commands = commands;
        self
    }

    /// The registry of commands this janitor stops what runs for the runs it releases.
    pub fn commands(&self) -> &Commands {
        &self.commands
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
        // One read per run for the whole sweep: the passes over kept output and held runs reuse
        // what the loop over workspaces learned.
        let mut seen: Verdicts = Verdicts::new();
        for run in runs {
            if stop.is_cancelled() {
                break;
            }
            match standing(store, &run).await {
                // A directory that is not a run id is not ours.
                Ok(Standing::NotARun) => {
                    report.kept += 1;
                    seen.insert(run, None);
                }
                Ok(Standing::Open) => {
                    report.kept += 1;
                    seen.insert(run, Some(false));
                }
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot ask the store about a run; its workspace stays");
                    report.failed.push(run);
                }
                Ok(Standing::Over(record)) => {
                    seen.insert(run.clone(), Some(true));
                    // What runs in the environment goes first: the commands that keep running, and
                    // what a worker that is gone left in the records.
                    self.commands.stop_run(&run, "the run ended").await;
                    self.commands.settle_over_run(&self.kept, &run).await;
                    // What the environment holds goes next, and a workspace it could not let go of
                    // stays for the next sweep.
                    if !self.was_released(&run)
                        && let Err(e) = self.environment.release(&run).await
                    {
                        tracing::warn!(%run, error = %adam_error::report(&e), "cannot release the environment of a finished run; its workspace stays");
                        report.failed.push(run);
                        continue;
                    }
                    released.insert(run.clone());
                    self.commands.forget_run(&run);
                    let keep_scratch = match &record {
                        Some(record) => self.conversation_is_alive(store, record).await,
                        None => false,
                    };
                    let removed = match self.workspaces.run(&run) {
                        Ok(workspace) if keep_scratch => workspace.remove_keeping_scratch().await,
                        Ok(workspace) => workspace.remove().await.map(|()| 0),
                        Err(e) => Err(e),
                    };
                    // Its kept command output goes with it, whether or not scratch projects stay.
                    if removed.is_ok()
                        && let Err(e) = self.kept.remove_run(&run).await
                    {
                        tracing::warn!(%run, error = %adam_error::report(&e), "cannot remove the kept output of a finished run");
                    }
                    // The images of its conversation go when the conversation has no open run and has
                    // had no task for as long as a scratch project is kept (never kept, at 0).
                    if removed.is_ok() {
                        let wanted = match &record {
                            Some(record) => self.images_are_wanted(store, record).await,
                            None => false,
                        };
                        if !wanted {
                            let scope = adam_llm_agent::image_scope(
                                record.as_ref().and_then(|r| r.conversation_id.as_deref()),
                                &run,
                            );
                            if let Err(e) = self.images.remove_scope(&scope).await {
                                tracing::warn!(%run, error = %adam_error::report(&e), "cannot remove the images of a finished run");
                            }
                        }
                    }
                    match removed {
                        Ok(0) => {
                            tracing::info!(%run, "removed the workspace of a finished run");
                            self.forget_released(&run);
                            report.removed.push(run);
                        }
                        Ok(scratch) => {
                            tracing::debug!(%run, scratch, "kept the scratch projects of a finished run");
                            self.remember_released(&run);
                            report.kept_scratch.push(run);
                        }
                        Err(e) => {
                            tracing::warn!(%run, error = %adam_error::report(&e), "cannot remove the workspace of a finished run");
                            report.failed.push(run);
                        }
                    }
                }
            }
        }
        self.release_orphans(store, stop, &released, &seen, &mut report)
            .await;
        self.sweep_outputs(store, stop, &seen).await;
        self.sweep_images(store, stop, &mut report).await;
        report
    }

    /// Remove the scopes of images that no conversation wants any more, whatever became of their
    /// workspaces: see the [module documentation](self).
    async fn sweep_images(&self, store: &dyn Store, stop: &CancellationToken, report: &mut Sweep) {
        let scopes = match self.images.scopes().await {
            Ok(scopes) => scopes,
            Err(e) => {
                tracing::warn!(error = %adam_error::report(&e), "cannot list the images to sweep");
                return;
            }
        };
        for scope in scopes {
            if stop.is_cancelled() {
                break;
            }
            match self.scope_is_wanted(store, &scope).await {
                Ok(true) => {}
                Ok(false) => match self.images.remove_scope(&scope).await {
                    Ok(()) => {
                        tracing::info!(%scope, "removed the images of a conversation nobody wants");
                        report.images.push(scope);
                    }
                    Err(e) => {
                        tracing::warn!(%scope, error = %adam_error::report(&e), "cannot remove the images of a conversation");
                    }
                },
                Err(e) => {
                    tracing::warn!(%scope, error = %e, "cannot decide about the images of a conversation; they stay");
                }
            }
        }
    }

    /// Whether the images under `scope` are still wanted: the run that noted itself in the scope is
    /// asked about. `Err` leaves them.
    async fn scope_is_wanted(&self, store: &dyn Store, scope: &str) -> Result<bool, BoxError> {
        use adam_llm_agent::image_scope;
        let owner = self.images.owner_of(scope).await?;
        let record = match owner.as_deref().and_then(|run| run.parse().ok().map(RunId)) {
            Some(id) => store.load_run(id).await?,
            None => None,
        };
        let run = owner.unwrap_or_default();
        let wanted = match &record {
            // The scope of the run's conversation: wanted while the conversation is.
            Some(record)
                if record
                    .conversation_id
                    .as_deref()
                    .is_some_and(|c| image_scope(Some(c), &run) == scope) =>
            {
                self.images_are_wanted(store, record).await
            }
            // The scope of a run with no conversation: wanted while the run is open.
            Some(record) if image_scope(None, &run) == scope => !record.status.is_terminal(),
            // A run the store does not know, and a scope that is its own: nothing wants it.
            None if !run.is_empty() && image_scope(None, &run) == scope => false,
            // Nobody can say whose it is: it goes when nothing was written in it for a long time.
            _ => match self.images.last_touched(scope).await? {
                Some(at) => SystemTime::now()
                    .duration_since(at)
                    .is_ok_and(|age| age < IMAGE_SCOPE_BACKSTOP),
                None => false,
            },
        };
        Ok(wanted)
    }

    /// Whether the images of the conversation of the finished run `record` are still wanted: a task
    /// of it is open, or the newest one was last touched within the idle time of a scratch project
    /// (none when `SCRATCH_IDLE_DAYS` is `0`). When the store cannot say, they are wanted.
    async fn images_are_wanted(&self, store: &dyn Store, record: &RunRecord) -> bool {
        let Some(conversation) = record.conversation_id.as_deref() else {
            return false;
        };
        let idle = self.scratch_idle.unwrap_or(Duration::ZERO);
        self.conversation_has_task_within(store, record, conversation, idle)
            .await
    }

    /// Remove the kept output of runs that are over and have no workspace here any more (a crash
    /// between the two removals, a workspace removed by hand). An open run's stays. A run this
    /// sweep already judged is not read again.
    async fn sweep_outputs(&self, store: &dyn Store, stop: &CancellationToken, seen: &Verdicts) {
        let runs = match self.kept.runs().await {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(error = %adam_error::report(&e), "cannot list the kept output to sweep");
                return;
            }
        };
        for run in runs {
            if stop.is_cancelled() {
                break;
            }
            let over = match seen.get(&run) {
                Some(over) => Ok(*over),
                None => is_over(store, &run).await,
            };
            match over {
                Ok(Some(true)) => {
                    self.commands.settle_over_run(&self.kept, &run).await;
                    if let Err(e) = self.kept.remove_run(&run).await {
                        tracing::warn!(%run, error = %adam_error::report(&e), "cannot remove the kept output of a finished run");
                    }
                }
                Ok(Some(false) | None) => {}
                Err(e) => {
                    tracing::warn!(%run, error = %adam_error::report(&e), "cannot ask the store about a run; its kept output stays");
                }
            }
        }
    }

    /// Whether the conversation of the finished run `record` may go on, so that its scratch projects
    /// stay: a task of it is open, or the newest one was last touched within the idle time. When the
    /// store cannot say, the projects stay (they are removed at a later sweep, never by a guess).
    async fn conversation_is_alive(&self, store: &dyn Store, record: &RunRecord) -> bool {
        let Some(idle) = self.scratch_idle else {
            return false;
        };
        let Some(conversation) = record.conversation_id.as_deref() else {
            return false;
        };
        self.conversation_has_task_within(store, record, conversation, idle)
            .await
    }

    /// Whether `conversation` has a task open, or had one touched within `idle`. A store that
    /// cannot say makes it so.
    async fn conversation_has_task_within(
        &self,
        store: &dyn Store,
        record: &RunRecord,
        conversation: &str,
        idle: Duration,
    ) -> bool {
        let newest = async {
            if store
                .open_run_for_conversation(&record.agent, conversation)
                .await?
                .is_some()
            {
                return Ok::<_, StoreError>(None);
            }
            let query = RunQuery::new(
                record.agent.clone(),
                ConversationScope::Exact(conversation.to_owned()),
                1,
            );
            let latest = store.list_runs(&query).await?;
            Ok(Some(
                latest
                    .first()
                    .map_or(record.updated_at, |run| {
                        run.updated_at.max(record.updated_at)
                    })
                    .timestamp_millis(),
            ))
        }
        .await;
        match newest {
            // An open task: the conversation goes on.
            Ok(None) => true,
            Ok(Some(last_ms)) => {
                let age = now_ms().saturating_sub(last_ms);
                u128::try_from(age).is_ok_and(|age| age < idle.as_millis())
            }
            Err(e) => {
                tracing::warn!(run = %record.id, error = %adam_error::report(&e), "cannot ask the store about the conversation of a finished run; what it holds stays");
                true
            }
        }
    }

    fn was_released(&self, run: &str) -> bool {
        self.released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(run)
    }

    fn remember_released(&self, run: &str) {
        self.released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run.to_owned());
    }

    fn forget_released(&self, run: &str) {
        self.released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(run);
    }

    /// The commands that keep running of `run` are stopped, and then what the environment holds for
    /// it is released.
    async fn release_after_commands(&self, run: &str) -> Result<(), adam_workspace::EnvError> {
        self.commands.stop_run(run, "the run ended").await;
        self.commands.settle_over_run(&self.kept, run).await;
        self.environment.release(run).await
    }

    /// Release what the environment holds for runs that are over and that the loop above did not
    /// see: the run has no workspace on this volume (it was removed, or the crash was in between).
    /// A run this sweep already judged is not read again.
    async fn release_orphans(
        &self,
        store: &dyn Store,
        stop: &CancellationToken,
        released: &HashSet<String>,
        seen: &Verdicts,
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
            let over = match seen.get(&run) {
                Some(over) => Ok(*over),
                None => is_over(store, &run).await,
            };
            match over {
                Ok(Some(true)) => match self.release_after_commands(&run).await {
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

/// What the store says of a directory of the workspace volume.
enum Standing {
    /// The name is not a run id.
    NotARun,
    /// The run is `runnable` or `parked`.
    Open,
    /// The run is finished, or the store does not know it (`None`).
    Over(Option<RunRecord>),
}

/// What the store says of `run`.
async fn standing(store: &dyn Store, run: &str) -> Result<Standing, StoreError> {
    let Some(id) = run.parse().ok().map(RunId) else {
        return Ok(Standing::NotARun);
    };
    Ok(match store.load_run(id).await? {
        Some(record) if record.status.is_terminal() => Standing::Over(Some(record)),
        Some(_) => Standing::Open,
        None => Standing::Over(None),
    })
}

/// Whether `run` is no longer open: finished (a cancel is a failure), or not known to the store.
/// A name that is not a run id is not a run at all, and is over for no one: `false`.
pub(crate) async fn run_is_over(store: &dyn Store, run: &str) -> Result<bool, StoreError> {
    Ok(is_over(store, run).await?.unwrap_or(false))
}

/// Whether `run` is over: `Some(true)` when it is finished or the store does not know it,
/// `Some(false)` when it is open (`runnable` or `parked`), `None` when `run` is not a run id.
async fn is_over(store: &dyn Store, run: &str) -> Result<Option<bool>, StoreError> {
    Ok(match standing(store, run).await? {
        Standing::NotARun => None,
        Standing::Open => Some(false),
        Standing::Over(_) => Some(true),
    })
}

/// Now, in milliseconds since the Unix epoch.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
