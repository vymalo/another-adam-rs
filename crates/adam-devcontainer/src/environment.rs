//! [`DevContainer`]: the [`Environment`] that runs a run's processes in a devcontainer.
//!
//! The states of an environment, which `state.json` keeps (see [`crate::state`]):
//!
//! ```text
//! first need ─▶ Probing ─▶ Local        runtime off or unreachable: a step says so
//!                  └─────▶ Building ─▶ Checking ─▶ SettingUp ─▶ Ready
//!                              │           │            │         │ exec, reused within the run
//!                              └───────────┴────────────┴──▶ Broken (cached until the file changes
//!                                                                    or `rebuild`)
//! Ready ─▶ Building      a slot joined, or the container is gone
//! Ready, Broken ─▶ Released   the janitor releases the run
//! ```

use std::collections::{BTreeSet, HashMap, HashSet};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use adam_workspace::{
    EnvError, EnvProgress, EnvSession, EnvStep, EnvStepState, Environment, LocalSession,
    RunWorkspace, Slot,
};
use async_trait::async_trait;
use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::sync::{Mutex, OnceCell, OwnedMutexGuard};

use crate::cli::{CleanEnv, Finished, LogTail, RunError, log_text, phase_of, run};
use crate::config::{self, Discovered};
use crate::error::{TAIL_LINES, clip, scrub, tail};
use crate::override_file::{self, OverrideInput, SlotInfo};
use crate::podman::{Podman, RUN_LABEL, Row, runs_of};
use crate::policy::{self, Expect, Inspected, RawContext};
use crate::session::{CONTAINER_SESSION_DATA, DevSession, common_args};
use crate::state::{self, Phase, SlotRecord, State, StoredError, environments_dir};
use crate::tools;
use crate::{Runtime, Settings};

/// What the step of building an environment shows.
const STEP: &str = "environment";

/// What the process holds: the settings, the clients and the per-run locks.
pub(crate) struct Inner {
    pub(crate) settings: Settings,
    pub(crate) env: CleanEnv,
    pub(crate) podman: Podman,
    pub(crate) counter: AtomicU64,
    /// Makes the ids of commands unique across restarts of the coder.
    pub(crate) epoch: String,
    locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    probe: StdMutex<Option<(Instant, Result<(), String>)>>,
    tools: OnceCell<PathBuf>,
    announced: StdMutex<HashSet<String>>,
}

/// The devcontainer environment. Cheap to clone: the clones are one environment.
#[derive(Clone)]
pub struct DevContainer {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for DevContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevContainer")
            .field("runtime", &self.inner.settings.runtime)
            .field("deployment", &self.inner.settings.deployment)
            .finish_non_exhaustive()
    }
}

/// Holds a run's lock: in this process, and on the volume against other processes.
struct RunGuard {
    _local: OwnedMutexGuard<()>,
    _file: Option<std::fs::File>,
}

/// Reports the one step of an environment being made, at most once a second while it is running.
struct Reporter<'a> {
    progress: &'a dyn EnvProgress,
    label: String,
    started: Instant,
    last: StdMutex<Option<Instant>>,
}

impl<'a> Reporter<'a> {
    fn new(progress: &'a dyn EnvProgress, label: String) -> Self {
        Self {
            progress,
            label,
            started: Instant::now(),
            last: StdMutex::new(None),
        }
    }

    fn send(&self, state: EnvStepState, detail: Option<String>) {
        let mut step = EnvStep::new(STEP, &self.label, state);
        step.detail = detail;
        self.progress.step(step);
    }

    fn running(&self, detail: &str) {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        self.send(EnvStepState::Running, Some(detail.to_owned()));
    }

    fn completed(&self, detail: String) {
        self.send(EnvStepState::Completed, Some(detail));
    }

    fn failed(&self, detail: String) {
        self.send(EnvStepState::Failed, Some(detail));
    }
}

/// Why an environment is being made again.
enum Again {
    /// Never made, or made by a process that did not finish.
    First,
    /// A repository joined the workspace.
    Joined(Vec<String>),
    /// The container is gone.
    Lost,
}

/// What `ensure` found out about the workspace.
struct Plan {
    slots: Vec<SlotInfo>,
    records: Vec<SlotRecord>,
    /// How a person calls the first slot: `owner/name`, or the scratch project's name.
    label: String,
    discovered: Discovered,
    /// Slots whose own devcontainer is passed over because the first slot decides.
    ignored: Vec<String>,
}

impl DevContainer {
    /// An environment with these settings. Nothing is started and nothing is read.
    pub fn new(settings: Settings) -> Self {
        let home = environments_dir(&settings.root).join(".cli-home");
        let env = CleanEnv::new(home, settings.container_host.clone());
        let podman = Podman::new(settings.podman.clone(), env.clone());
        let epoch = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        );
        Self {
            inner: Arc::new(Inner {
                settings,
                env,
                podman,
                counter: AtomicU64::new(1),
                epoch,
                locks: StdMutex::default(),
                probe: StdMutex::default(),
                tools: OnceCell::new(),
                announced: StdMutex::default(),
            }),
        }
    }

    /// The settings.
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// Whether the Podman service answers (the probe `ensure` makes, without its 30-second cache).
    ///
    /// # Errors
    ///
    /// [`EnvError::Unavailable`] with the reason, scrubbed.
    pub async fn probe(&self) -> Result<(), EnvError> {
        let secrets = self.secrets();
        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        self.inner
            .podman
            .info(self.inner.settings.probe_timeout)
            .await
            .map_err(|e| EnvError::Unavailable(scrub(&e.to_string(), &refs)))
    }

    /// Pull the default image, so that the first run that needs it does not wait for it.
    ///
    /// # Errors
    ///
    /// [`EnvError::Unavailable`].
    pub async fn prepull(&self) -> Result<(), EnvError> {
        let s = &self.inner.settings;
        self.inner
            .podman
            .pull(&s.default_image, s.up_timeout)
            .await
            .map_err(|e| EnvError::Unavailable(e.to_string()))
    }

    /// Write the directory of tools every container mounts and return it.
    /// `ensure` does this when it is first needed; a binary that wants to fail at startup calls it.
    ///
    /// # Errors
    ///
    /// [`EnvError::Io`], including an `opencode` that cannot be read.
    pub async fn install_tools(&self) -> Result<PathBuf, EnvError> {
        let root = self.inner.settings.root.clone();
        let opencode = self.inner.settings.opencode.clone();
        self.inner
            .tools
            .get_or_try_init(|| async move {
                tokio::task::spawn_blocking(move || tools::install(&root, opencode.as_deref()))
                    .await
                    .map_err(std::io::Error::other)?
            })
            .await
            .cloned()
            .map_err(EnvError::from)
    }

    /// Remove the images that nothing uses (the layers a failed build left). Per-run images are
    /// removed by [`release`](Environment::release); pulled base images stay.
    ///
    /// # Errors
    ///
    /// [`EnvError::Unavailable`].
    pub async fn prune(&self) -> Result<(), EnvError> {
        self.inner
            .podman
            .prune_images()
            .await
            .map_err(|e| EnvError::Unavailable(e.to_string()))
    }

    /// Throw away the environment of `run` and make it again on the next [`ensure`](Environment::ensure):
    /// the way out of a broken one. With `use_default` the repository's own file is ignored from
    /// now on, and the default image is used. The run's files are not touched.
    ///
    /// # Errors
    ///
    /// As [`release`](Environment::release).
    pub async fn rebuild(&self, run: &str, use_default: bool) -> Result<(), EnvError> {
        let dir = state::run_dir(&self.inner.settings.root, run)?;
        let _guard = self.lock_run(run, false).await?;
        if self.inner.settings.runtime == Runtime::Podman {
            let previous = State::read(&self.inner.settings.root, run)?;
            self.remove_container_and_images(run, previous.as_ref())
                .await?;
        }
        self.forget_announcements(run);
        let mut fresh = State::building(run, &self.inner.settings.deployment);
        fresh.use_default = use_default
            || State::read(&self.inner.settings.root, run)?.is_some_and(|s| s.use_default);
        if dir.exists() || self.inner.settings.runtime == Runtime::Podman {
            fresh.write(&self.inner.settings.root)?;
        }
        Ok(())
    }

    fn secrets(&self) -> Vec<String> {
        self.inner
            .settings
            .model_key
            .iter()
            .map(|k| k.expose_secret().to_owned())
            .collect()
    }

    fn lock_for(&self, run: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .inner
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.entry(run.to_owned()).or_default().clone()
    }

    /// The run's lock: the in-process one, then an exclusive `flock` on `<run dir>/lock`, so that
    /// two processes on one volume do not make one run's container twice.
    async fn lock_run(&self, run: &str, file: bool) -> Result<RunGuard, EnvError> {
        let local = self.lock_for(run).lock_owned().await;
        if !file {
            return Ok(RunGuard {
                _local: local,
                _file: None,
            });
        }
        let dir = state::run_dir(&self.inner.settings.root, run)?;
        let file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
            std::fs::create_dir_all(&dir)?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("lock"))?;
            file.lock()?;
            Ok(file)
        })
        .await
        .map_err(|e| EnvError::Io(std::io::Error::other(e)))??;
        Ok(RunGuard {
            _local: local,
            _file: Some(file),
        })
    }

    /// Say something once per run and kind (the fallbacks: one step per run, not one per command).
    fn announce_once(&self, run: &str, kind: &str) -> bool {
        self.inner
            .announced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(format!("{run}/{kind}"))
    }

    fn forget_announcements(&self, run: &str) {
        self.inner
            .announced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|k| !k.starts_with(&format!("{run}/")));
    }

    /// The probe, at most once every `probe_interval`.
    async fn probe_cached(&self) -> Result<(), String> {
        let s = &self.inner.settings;
        {
            let cache = self
                .inner
                .probe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((at, result)) = &*cache
                && at.elapsed() < s.probe_interval
            {
                return result.clone();
            }
        }
        let result = self.probe().await.map_err(|e| e.to_string());
        *self
            .inner
            .probe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((Instant::now(), result.clone()));
        result
    }

    /// What `ensure` needs to know about the workspace.
    async fn plan(
        &self,
        ws: &RunWorkspace,
        slots: &[Slot],
        use_default: bool,
    ) -> Result<Plan, EnvError> {
        let s = &self.inner.settings;
        let first = &slots[0];
        let discovered = config::discover(first.path(), &s.default_image, use_default)?;
        let mut ignored = Vec::new();
        if !use_default {
            for later in &slots[1..] {
                if config::has_file(later.path()).unwrap_or(false) {
                    ignored.push(slot_label(later));
                }
            }
        }
        let _ = ws;
        Ok(Plan {
            slots: slots.iter().map(slot_info).collect(),
            records: slots.iter().map(slot_record).collect(),
            label: slot_label(first),
            discovered,
            ignored,
        })
    }

    /// The session of a run whose container is there.
    fn session(
        &self,
        run: &str,
        state: &State,
        plan_slot0: &Path,
        label: &str,
    ) -> Result<DevSession, EnvError> {
        let s = &self.inner.settings;
        let env_dir = state::run_dir(&s.root, run)?;
        Ok(DevSession {
            inner: self.inner.clone(),
            run: run.to_owned(),
            run_dir: s.root.join("workspaces").join(run),
            config_slot: plan_slot0.to_owned(),
            config_file: state.config_source.as_ref().map(|rel| plan_slot0.join(rel)),
            override_file: env_dir.join("devcontainer.json"),
            container_id: state.container_id.clone().unwrap_or_default(),
            image: state.image.clone().unwrap_or_default(),
            label: label.to_owned(),
            has_model_key: state.model_key,
        })
    }

    /// Whether the container of `state` is there and running.
    async fn alive(&self, run: &str, state: &State) -> Result<bool, EnvError> {
        let rows = self
            .inner
            .podman
            .containers(RUN_LABEL, run)
            .await
            .map_err(|e| EnvError::Unavailable(e.to_string()))?;
        let Some(id) = state.container_id.as_deref().filter(|id| !id.is_empty()) else {
            return Ok(false);
        };
        let env_file = state::run_dir(&self.inner.settings.root, run)?.join("devcontainer.json");
        Ok(env_file.is_file()
            && rows.iter().any(|r: &Row| {
                r.running && (r.id == id || r.id.starts_with(id) || id.starts_with(&r.id))
            }))
    }

    /// `ensure` for a deployment that has a runtime.
    async fn ensure_podman(
        &self,
        ws: &RunWorkspace,
        slots: Vec<Slot>,
        progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError> {
        let s = &self.inner.settings;
        let run = ws.run();
        let state = State::read(&s.root, run)?;
        if state.as_ref().is_some_and(|st| st.phase == Phase::Local) {
            return Ok(LocalSession::shared());
        }
        let use_default = state.as_ref().is_some_and(|st| st.use_default);
        let plan = match self.plan(ws, &slots, use_default).await {
            Ok(plan) => plan,
            Err(e) => {
                // The file cannot be read: no environment, and the step says which file.
                let label = match &e {
                    EnvError::Config { file, .. } => format!(
                        "Building the environment from {} ({})",
                        file.display(),
                        slot_label(&slots[0])
                    ),
                    _ => "Building the environment".to_owned(),
                };
                Reporter::new(progress, label).failed(failure_detail(&e));
                return Err(e);
            }
        };

        let mut again = Again::First;
        if let Some(st) = &state {
            match st.phase {
                Phase::Ready if st.has_slots(&plan.records) => {
                    if self.alive(run, st).await? {
                        return Ok(Arc::new(self.session(
                            run,
                            st,
                            slots[0].path(),
                            &plan.label,
                        )?));
                    }
                    again = Again::Lost;
                }
                Phase::Ready => {
                    let known: HashSet<&str> = st.slots.iter().map(|r| r.dir.as_str()).collect();
                    again = Again::Joined(
                        plan.slots
                            .iter()
                            .filter(|slot| !known.contains(slot.dir.as_str()))
                            .map(|slot| slot.dir.clone())
                            .collect(),
                    );
                }
                Phase::Broken if st.config_digest == plan.discovered.digest => {
                    // The same file, the same answer: nothing is built, and the step says it again.
                    if let Some(error) = &st.error {
                        let error = error.to_error();
                        Reporter::new(progress, build_label(&plan)).failed(failure_detail(&error));
                        return Err(error);
                    }
                }
                Phase::Broken | Phase::Building | Phase::Local => {}
            }
        }
        let had_environment = state
            .as_ref()
            .is_some_and(|st| st.container_id.is_some() || st.phase == Phase::Ready);
        self.build(ws, plan, state, again, had_environment, progress)
            .await
    }

    /// Make the environment: probe, check the file, `read-configuration`, `up`, inspect, the
    /// lifecycle commands. A failure the person has to decide about is kept as `Broken`.
    async fn build(
        &self,
        ws: &RunWorkspace,
        plan: Plan,
        previous: Option<State>,
        again: Again,
        had_environment: bool,
        progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError> {
        let s = &self.inner.settings;
        let run = ws.run();

        if let Err(why) = self.probe_cached().await {
            if had_environment {
                return Err(EnvError::Unavailable(why));
            }
            let mut local = State::building(run, &s.deployment);
            local.phase = Phase::Local;
            local.local_reason = Some(why.clone());
            local.write(&s.root)?;
            let reporter = Reporter::new(
                progress,
                "The container runtime is not reachable: commands run in the coder's own environment".to_owned(),
            );
            reporter.completed(clip(&why, 400));
            return Ok(LocalSession::shared());
        }

        let label = match &again {
            Again::First => build_label(&plan),
            Again::Joined(dirs) => format!(
                "Restarting the environment: {} joined the workspace",
                dirs.join(", ")
            ),
            Again::Lost => "The environment was lost; rebuilding it".to_owned(),
        };
        let reporter = Reporter::new(progress, label);
        reporter.running("preparing");
        if !plan.ignored.is_empty() {
            progress.step(
                EnvStep::new(
                    "environment-ignored",
                    "Using the environment of the first repository",
                    EnvStepState::Completed,
                )
                .with_detail(format!(
                    "{}'s is used for the whole workspace; the devcontainer of {} is ignored",
                    plan.label,
                    plan.ignored.join(", ")
                )),
            );
        }
        if !plan.discovered.others.is_empty() {
            progress.step(
                EnvStep::new(
                    "environment-several",
                    "Several devcontainer.json files",
                    EnvStepState::Completed,
                )
                .with_detail(format!(
                    "used {}; also found {}",
                    plan.discovered
                        .source
                        .as_deref()
                        .map_or_else(String::new, |p| p.display().to_string()),
                    plan.discovered
                        .others
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }

        let remove_existing = !matches!(again, Again::First) || previous.is_some();
        let mut log = LogTail::default();
        let attempt = self
            .attempt(
                ws,
                &plan,
                previous.as_ref(),
                remove_existing,
                &reporter,
                &mut log,
            )
            .await;
        self.save_log(run, &log);
        match attempt {
            Ok((session, changes)) => {
                reporter.completed(format!(
                    "ready in {} s. {}",
                    reporter.started.elapsed().as_secs(),
                    changes.join("; ")
                ));
                Ok(Arc::new(session))
            }
            Err(error) => {
                // What was started is not left behind (a timeout leaves a partial container).
                if !matches!(error, EnvError::Io(_) | EnvError::Unavailable(_)) {
                    self.remove_containers_by_label(run).await;
                }
                if let Some(stored) = StoredError::of(&error) {
                    let mut broken = State::building(run, &s.deployment);
                    broken.phase = Phase::Broken;
                    broken.config_source = plan
                        .discovered
                        .source
                        .as_ref()
                        .map(|p| p.display().to_string());
                    broken.config_digest.clone_from(&plan.discovered.digest);
                    broken.use_default = previous.as_ref().is_some_and(|p| p.use_default);
                    broken.slots.clone_from(&plan.records);
                    broken.error = Some(stored);
                    if let Err(e) = broken.write(&s.root) {
                        tracing::warn!(error = %e, run, "cannot keep the broken state");
                    }
                }
                reporter.failed(failure_detail(&error));
                Err(error)
            }
        }
    }

    /// The part of [`build`](Self::build) whose failure is the environment's.
    async fn attempt(
        &self,
        ws: &RunWorkspace,
        plan: &Plan,
        previous: Option<&State>,
        remove_existing: bool,
        reporter: &Reporter<'_>,
        log: &mut LogTail,
    ) -> Result<(DevSession, Vec<String>), EnvError> {
        let s = &self.inner.settings;
        let run = ws.run();
        let secrets = self.secrets();
        let secret_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let first = &plan.slots[0];
        let file = plan
            .discovered
            .source
            .clone()
            .unwrap_or_else(|| PathBuf::from("devcontainer.json"));

        // The file itself, before anything is built (check 1).
        if plan.discovered.source.is_some() {
            let config_dir = first
                .path
                .join(&file)
                .parent()
                .map_or_else(|| first.path.clone(), Path::to_path_buf);
            policy::check_raw(
                &plan.discovered.value,
                &RawContext {
                    file: &file,
                    config_dir: &config_dir,
                    slot: &first.path,
                },
            )?;
        }

        let env_dir = state::run_dir(&s.root, run)?;
        tokio::fs::create_dir_all(env_dir.join("secrets")).await?;
        tokio::fs::create_dir_all(&self.inner.env.home()).await?;
        let has_key = self.write_model_key(&env_dir).await?;
        let tools_dir = self.install_tools().await?;
        let made = override_file::build(&OverrideInput {
            raw: &plan.discovered.value,
            run_dir: &s.root.join("workspaces").join(run),
            config_slot: first,
            slots: &plan.slots,
            tools_dir: &tools_dir,
            secrets_dir: has_key.then(|| env_dir.join("secrets")).as_deref(),
            network: s.network,
        })?;
        let override_path = env_dir.join("devcontainer.json");
        let text = serde_json::to_vec_pretty(&made.value).map_err(std::io::Error::other)?;
        tokio::fs::write(&override_path, text).await?;

        let mut building = State::building(run, &s.deployment);
        building.config_source = plan
            .discovered
            .source
            .as_ref()
            .map(|p| p.display().to_string());
        building.config_digest.clone_from(&plan.discovered.digest);
        building.use_default = previous.is_some_and(|p| p.use_default);
        building.slots.clone_from(&plan.records);
        building.model_key = has_key;
        building.tools = Some(tools_dir.display().to_string());
        building.write(&s.root)?;
        reporter.running(&format!("{} changes to the file", made.changes.len()));

        let config_file = plan
            .discovered
            .source
            .as_ref()
            .map(|rel| first.path.join(rel));
        let common = common_args(
            &self.inner,
            run,
            &first.path,
            config_file.as_deref(),
            &override_path,
        );
        let on_line = |line: &str| {
            let text = scrub(&log_text(line), &secret_refs);
            if let Some(phase) = phase_of(&text) {
                reporter.running(&phase);
            }
        };

        // 1. What the file says once features and the image's metadata are merged in (check 2).
        let mut args = vec![std::ffi::OsString::from("read-configuration")];
        args.extend(common.iter().cloned());
        args.extend(["--include-merged-configuration", "--log-format", "json"].map(Into::into));
        let done = self
            .cli("configuration", args, s.read_timeout, on_line, log)
            .await?;
        if !done.success() {
            return Err(failure(
                "the devcontainer CLI could not read the configuration",
                &file,
                &done,
                &secret_refs,
                true,
            ));
        }
        let configuration: Value = last_json(&done.stdout).unwrap_or(Value::Null);
        policy::check_merged(configuration.get("mergedConfiguration"), &made.binds, &file)?;

        // 2. Pull and build, create the container; the repository's own commands wait.
        let mut args = vec![std::ffi::OsString::from("up")];
        args.extend(common.iter().cloned());
        args.extend(
            [
                "--no-lockfile",
                "--skip-post-create",
                "--gpu-availability",
                "none",
                "--log-format",
                "json",
                "--container-session-data-folder",
                CONTAINER_SESSION_DATA,
            ]
            .map(Into::into),
        );
        if remove_existing {
            args.push("--remove-existing-container".into());
        }
        let done = self.cli("build", args, s.up_timeout, on_line, log).await?;
        let result = last_json(&done.stdout).unwrap_or(Value::Null);
        if !done.success() || result.get("outcome").and_then(Value::as_str) != Some("success") {
            let reason = result
                .get("description")
                .or_else(|| result.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("the devcontainer CLI could not make the container");
            return Err(failure(reason, &file, &done, &secret_refs, false));
        }
        let remote_user = result
            .get("remoteUser")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(container_id) = result
            .get("containerId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Err(failure(
                "the devcontainer CLI did not say which container it made",
                &file,
                &done,
                &secret_refs,
                false,
            ));
        };
        reporter.running("checking the container");

        // 3. The container as it is (check 3, which has the last word).
        let inspect = self
            .inner
            .podman
            .inspect(&container_id)
            .await
            .map_err(|e| EnvError::Unavailable(e.to_string()))?;
        let Inspected { image, running, .. } = policy::check_inspect(
            &inspect,
            &Expect {
                binds: &made.binds,
                network: s.network,
            },
            &file,
        )?;
        if !running {
            return Err(EnvError::Build {
                reason: "the container stopped right after it was made".to_owned(),
                log_tail: tail(&scrub(&log.as_plain(), &secret_refs), TAIL_LINES),
            });
        }

        // 4. The repository's lifecycle commands, as the remote user.
        let mut args = vec![std::ffi::OsString::from("run-user-commands")];
        args.extend(common.iter().cloned());
        args.extend(
            [
                "--log-format",
                "json",
                "--container-session-data-folder",
                CONTAINER_SESSION_DATA,
            ]
            .map(Into::into),
        );
        let done = self
            .cli("setup", args, s.setup_timeout, on_line, log)
            .await?;
        let result = last_json(&done.stdout).unwrap_or(Value::Null);
        if !done.success() || result.get("outcome").and_then(Value::as_str) == Some("error") {
            let reason = result
                .get("description")
                .or_else(|| result.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("a lifecycle command of the devcontainer failed");
            return Err(failure(reason, &file, &done, &secret_refs, false));
        }

        let mut ready = building;
        ready.phase = Phase::Ready;
        ready.container_id = Some(container_id.clone());
        ready.image = Some(image.clone());
        // The CLI gives a non-root remote user the coder's own uid (`--userns=keep-id`; verified in the
        // 0.89.0 bundle, function `uW`). Informational: nothing here depends on it.
        ready.keep_id = remote_user
            .as_deref()
            .is_some_and(|user| user != "root" && user != "0");
        ready.write(&s.root)?;
        let label = plan.label.clone();
        Ok((
            DevSession {
                inner: self.inner.clone(),
                run: run.to_owned(),
                run_dir: s.root.join("workspaces").join(run),
                config_slot: first.path.clone(),
                config_file,
                override_file: override_path,
                container_id,
                image,
                label,
                has_model_key: has_key,
            },
            made.changes.clone(),
        ))
    }

    /// Run the devcontainer CLI; its standard error is the log.
    async fn cli(
        &self,
        phase: &'static str,
        args: Vec<std::ffi::OsString>,
        timeout: Duration,
        on_line: impl FnMut(&str) + Send,
        log: &mut LogTail,
    ) -> Result<Finished, EnvError> {
        let s = &self.inner.settings;
        self.inner.env.ensure_home().await?;
        let cmd = self.inner.env.command(&s.cli, args);
        match run(cmd, timeout, on_line).await {
            Ok(done) => {
                for line in done.log.as_str().lines() {
                    log.push(line);
                }
                Ok(done)
            }
            Err(RunError::Spawn(e)) => Err(EnvError::Unavailable(format!(
                "cannot start the devcontainer CLI ({}): {e}",
                s.cli.display()
            ))),
            Err(RunError::Timeout) => Err(EnvError::Timeout {
                phase,
                secs: timeout.as_secs(),
            }),
            Err(RunError::Io(e)) => Err(EnvError::Io(e)),
        }
    }

    /// Write the model key for the container (mode 0600 in a directory of mode 0700), if there is one.
    async fn write_model_key(&self, env_dir: &Path) -> Result<bool, EnvError> {
        let Some(key) = &self.inner.settings.model_key else {
            return Ok(false);
        };
        let dir = env_dir.join("secrets");
        let file = dir.join("model-key");
        let value = key.expose_secret().to_owned();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::write(&file, value)?;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        })
        .await
        .map_err(|e| EnvError::Io(std::io::Error::other(e)))??;
        Ok(true)
    }

    /// Keep the end of what the CLI said, scrubbed, as `build.log`.
    fn save_log(&self, run: &str, log: &LogTail) {
        let secrets = self.secrets();
        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let Ok(dir) = state::run_dir(&self.inner.settings.root, run) else {
            return;
        };
        let text = scrub(&log.as_plain(), &refs);
        if let Err(e) = std::fs::write(dir.join("build.log"), text) {
            tracing::debug!(error = %e, run, "cannot write build.log");
        }
    }

    /// Remove every container with the run's label (a partial one included).
    async fn remove_containers_by_label(&self, run: &str) {
        if let Ok(rows) = self.inner.podman.containers(RUN_LABEL, run).await {
            for row in rows {
                self.inner.podman.remove(&row.id).await;
            }
        }
    }

    /// Release, without the lock: give the files back, remove the containers (and check), and the
    /// images the run built.
    async fn remove_container_and_images(
        &self,
        run: &str,
        state: Option<&State>,
    ) -> Result<(), EnvError> {
        let podman = &self.inner.podman;
        let list = |e: crate::podman::PodmanError| EnvError::Unavailable(e.to_string());
        let rows = podman.containers(RUN_LABEL, run).await.map_err(list)?;
        // What a process in the container made as another user must be the coder's to delete: give
        // the tree to the coder's own user and group. The container's id map is relative to the
        // service's user namespace, so the ids go there first (a rootless service calls its own
        // user, which is the coder's, 0); the script then maps them into the container.
        let work = self.inner.settings.root.join("workspaces").join(run);
        let running: Vec<_> = rows.iter().filter(|r| r.running).collect();
        if let (Ok(meta), false) = (std::fs::metadata(&work), running.is_empty()) {
            match podman.id_maps(self.inner.settings.probe_timeout).await {
                Ok(maps) => match maps.to_service(meta.uid(), meta.gid()) {
                    Some((uid, gid)) => {
                        for row in running {
                            if let Err(e) = podman
                                .exec_chown(&row.id, uid, gid, &work.display().to_string())
                                .await
                            {
                                tracing::warn!(error = %e, run, "could not give the files back before removing the container");
                            }
                        }
                    }
                    None => tracing::warn!(
                        run,
                        uid = meta.uid(),
                        gid = meta.gid(),
                        "the Podman service has no number for the coder's ids: the files are not given back"
                    ),
                },
                Err(e) => {
                    tracing::warn!(error = %e, run, "could not read the Podman service's id maps")
                }
            }
        }
        for attempt in 0..2 {
            let rows = podman.containers(RUN_LABEL, run).await.map_err(list)?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                podman.remove(&row.id).await;
            }
            if attempt == 1 {
                let left = podman.containers(RUN_LABEL, run).await.map_err(list)?;
                if !left.is_empty() {
                    return Err(EnvError::Unavailable(format!(
                        "{} container(s) of the run are still there after removing them",
                        left.len()
                    )));
                }
            }
        }
        if let Some(prefix) = state
            .and_then(|s| s.image.as_deref())
            .and_then(image_prefix)
        {
            match podman.images_starting_with(&prefix).await {
                Ok(names) => {
                    for name in names {
                        if let Err(e) = podman.remove_image(&name).await {
                            tracing::warn!(error = %e, image = name, "could not remove the image of the run");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, run, "could not list the images of the run"),
            }
        }
        Ok(())
    }

    async fn release_locked(&self, run: &str, dir: &Path) -> Result<(), EnvError> {
        let s = &self.inner.settings;
        if !dir.exists() {
            return Ok(());
        }
        let state = State::read(&s.root, run)?;
        if s.runtime == Runtime::Podman && state.as_ref().is_none_or(|st| st.phase != Phase::Local)
        {
            self.remove_container_and_images(run, state.as_ref())
                .await?;
        }
        tokio::fs::remove_dir_all(dir).await?;
        Ok(())
    }
}

#[async_trait]
impl Environment for DevContainer {
    async fn ensure(
        &self,
        workspace: &RunWorkspace,
        progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError> {
        let s = &self.inner.settings;
        let run = workspace.run();
        let _guard = self.lock_run(run, s.runtime == Runtime::Podman).await?;
        let slots = workspace.slots_in_join_order().await.map_err(|e| {
            EnvError::Unavailable(format!(
                "the workspace cannot be read: {}",
                adam_error::report(&e)
            ))
        })?;
        let Some(first) = slots.first() else {
            return Err(EnvError::Refused(
                "the workspace has no repository or project yet: there is nothing to run in"
                    .to_owned(),
            ));
        };
        // A workspace of the older layout is not one this crate mounts.
        if slots
            .iter()
            .any(|slot| !slot.path().starts_with(workspace.path()))
        {
            if self.announce_once(run, "legacy") {
                Reporter::new(
                    progress,
                    "Commands run in the coder's own environment".to_owned(),
                )
                .completed("this workspace has the layout of an older version".to_owned());
            }
            return Ok(LocalSession::shared());
        }
        if s.runtime == Runtime::Off {
            if config::has_file(first.path())? && self.announce_once(run, "off") {
                Reporter::new(
                    progress,
                    "This repository has a devcontainer, but this deployment runs without a container runtime: \
                     commands run in the coder's own environment"
                        .to_owned(),
                )
                .completed("a tool that only the devcontainer has will be reported as missing".to_owned());
            }
            return Ok(LocalSession::shared());
        }
        self.ensure_podman(workspace, slots, progress).await
    }

    async fn release(&self, run: &str) -> Result<(), EnvError> {
        let s = &self.inner.settings;
        let dir = state::run_dir(&s.root, run)?;
        let secs = s.release_timeout.as_secs();
        let work = async {
            let _guard = self.lock_run(run, false).await?;
            self.release_locked(run, &dir).await
        };
        let result = tokio::time::timeout(s.release_timeout, work)
            .await
            .map_err(|_| EnvError::Timeout {
                phase: "release",
                secs,
            })?;
        if result.is_ok() {
            self.forget_announcements(run);
            self.inner
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(run);
        }
        result
    }

    async fn rebuild(&self, run: &str, use_default: bool) -> Result<bool, EnvError> {
        // The inherent method of the same name does the work; with no runtime there is nothing
        // of its own to make again (a run is in the coder's own environment).
        DevContainer::rebuild(self, run, use_default).await?;
        Ok(self.inner.settings.runtime == Runtime::Podman)
    }

    async fn held_runs(&self) -> Result<Vec<String>, EnvError> {
        let s = &self.inner.settings;
        let mut runs = BTreeSet::new();
        if let Ok(entries) = std::fs::read_dir(environments_dir(&s.root)) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') || !entry.path().is_dir() {
                    continue;
                }
                match State::read(&s.root, &name) {
                    Ok(Some(st)) if st.deployment != s.deployment => {}
                    _ => {
                        runs.insert(name);
                    }
                }
            }
        }
        if s.runtime == Runtime::Podman {
            let rows = self
                .inner
                .podman
                .deployment_containers(&s.deployment)
                .await
                .map_err(|e| EnvError::Unavailable(e.to_string()))?;
            runs.extend(runs_of(&rows));
        }
        Ok(runs.into_iter().collect())
    }
}

/// The label of the step that builds the environment of `plan`.
fn build_label(plan: &Plan) -> String {
    match &plan.discovered.source {
        Some(file) => format!(
            "Building the environment from {} ({})",
            file.display(),
            plan.label
        ),
        None => format!(
            "Using the default environment ({})",
            plan.discovered
                .value
                .get("image")
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
    }
}

/// One line for the step of a failure.
fn failure_detail(error: &EnvError) -> String {
    clip(&error.to_string(), 400)
}

/// What a person calls a slot: `owner/name`, or the scratch project's name.
fn slot_label(slot: &Slot) -> String {
    match slot.worktree().and_then(|wt| wt.repo().locate().ok()) {
        Some(loc) if loc.is_local() => loc.name,
        Some(loc) => format!("{}/{}", loc.owner, loc.name),
        None => slot.dir().to_owned(),
    }
}

fn slot_info(slot: &Slot) -> SlotInfo {
    SlotInfo {
        dir: slot.dir().to_owned(),
        path: slot.path().to_owned(),
        mirror: slot.worktree().map(|wt| wt.mirror().to_owned()),
    }
}

fn slot_record(slot: &Slot) -> SlotRecord {
    SlotRecord {
        dir: slot.dir().to_owned(),
        seq: slot.seq(),
        mirror: slot.worktree().map(|wt| wt.mirror().display().to_string()),
    }
}

/// An error of a CLI call that did not succeed: what it said, scrubbed, as the end of its log.
fn failure(
    reason: &str,
    file: &Path,
    done: &Finished,
    secrets: &[&str],
    is_config: bool,
) -> EnvError {
    let log_tail = tail(&scrub(&done.log.as_plain(), secrets), TAIL_LINES);
    let reason = scrub(reason, secrets);
    if is_config {
        EnvError::Config {
            file: file.to_owned(),
            reason: format!("{reason}\n{log_tail}"),
        }
    } else {
        EnvError::Build {
            reason: format!("{}: {reason}", file.display()),
            log_tail,
        }
    }
}

/// The last line of `stdout` that is a JSON object.
fn last_json(stdout: &str) -> Option<Value> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| serde_json::from_str(l).ok())
}

/// What names the images the CLI built for a run: `vsc-<folder>-<hash>`, without the suffixes of
/// the stages (`-features`, `-uid`) and the registry and tag. `None` for an image that is not
/// one of these: a pulled base image is never removed.
fn image_prefix(image: &str) -> Option<String> {
    let name = image.strip_prefix("localhost/").unwrap_or(image);
    let name = name.split(':').next().unwrap_or(name);
    let mut base = name;
    while let Some(rest) = base
        .strip_suffix("-uid")
        .or_else(|| base.strip_suffix("-features"))
    {
        base = rest;
    }
    base.starts_with("vsc-").then(|| base.to_owned())
}
