//! The environment a run's processes run in.
//!
//! A run's files are a workspace ([`RunWorkspace`]), and the processes that act on them (the
//! project's checks, a command to look around, a coding agent) run **somewhere**. Today that is the
//! caller's own container: [`Local`]. This module is the seam that lets it be somewhere else without
//! the callers changing: they describe what to run ([`ExecSpec`]), ask the run's [`EnvSession`] to
//! [`prepare`](EnvSession::prepare) it into a command to spawn ([`PreparedCommand`]), and spawn
//! that. What stays in the caller, whatever the environment is: all git work, and the file tools,
//! because they act on the shared files; **paths are the same in every environment**, which keeps
//! file requests, working directories and the git snapshots valid without any mapping.
//!
//! ```text
//! caller ── ensure(workspace, progress) ──▶ Environment ──▶ Arc<dyn EnvSession>   (once per need)
//! caller ── prepare(ExecSpec) ───────────▶ EnvSession  ──▶ PreparedCommand
//! caller spawns it (its own process group, kill_on_drop, piped stdio)
//! caller ── kill(exec) ──────────────────▶ EnvSession      (on a timeout or a cancel)
//! janitor ─ release(run) ────────────────▶ Environment     (before the workspace is removed)
//! ```
//!
//! * [`Environment`] makes the session of a run on first need and reuses it
//!   ([`ensure`](Environment::ensure)); it frees what it holds for a run
//!   ([`release`](Environment::release), idempotent) and says which runs it holds something for
//!   ([`held_runs`](Environment::held_runs)), so that an orphan sweep can find what a crash left.
//! * [`EnvSession`] is the run's view of it: [`describe`](EnvSession::describe) for a person,
//!   [`prepare`](EnvSession::prepare) and [`kill`](EnvSession::kill). No secret is ever put in a
//!   command line, and an environment gives none to its processes.
//! * [`EnvProgress`] is how a slow `ensure` (building an image) tells the caller what it is doing,
//!   as [`EnvStep`]s the caller shows.
//!
//! [`Local`] is the one implementation here and behaves as the callers always did: the process is
//! a child of the caller, in the caller's environment, minus the names the caller asked to hide
//! ([`ExecSpec::hide`]).
//!
//! The traits are the port; an implementation that runs processes in a container is another crate's
//! (ADR 0009 of this repository: swappable at build time).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use adam_error::{Classify, ErrorClass};
use async_trait::async_trait;

use crate::run_workspace::RunWorkspace;

/// What to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Program {
    /// A shell command line, run by a login shell: `bash -lc`, else `sh -lc` ([`login_shell`]).
    Shell(String),
    /// A program and its arguments: the first is the program, looked up on `PATH` unless it has a
    /// directory part. Never empty.
    Argv(Vec<OsString>),
}

/// A process to run in a run's environment.
///
/// `cwd` is absolute and inside a slot of the run's workspace; the path means the same in every
/// environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSpec {
    /// The program, or the command line.
    pub program: Program,
    /// The working directory.
    pub cwd: PathBuf,
    /// Environment variables to set. **Never a secret.**
    pub env: BTreeMap<String, String>,
    /// Names of variables the process must not see, in an environment that passes its caller's
    /// along (the secrets of the caller's own process). An environment that starts the process
    /// with nothing of the caller's has nothing to hide and ignores it. Wins over `env`.
    pub hide: Vec<String>,
    /// The process is meant to **outlive the call** that starts it (a development server, a
    /// watcher): its [`ExecId`] starts with `bg-` ([`ExecId::is_background`]), which is how a pod's
    /// `adam-exec active-fg` leaves it out when it counts what keeps the pod from being made again.
    pub background: bool,
}

impl ExecSpec {
    /// A shell command line in `cwd`.
    pub fn shell(command: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: Program::Shell(command.into()),
            cwd: cwd.into(),
            env: BTreeMap::new(),
            hide: Vec::new(),
            background: false,
        }
    }

    /// A program and its arguments in `cwd`.
    pub fn argv<I, S>(argv: I, cwd: impl Into<PathBuf>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: Program::Argv(argv.into_iter().map(Into::into).collect()),
            cwd: cwd.into(),
            env: BTreeMap::new(),
            hide: Vec::new(),
            background: false,
        }
    }

    /// Mark the process as one that outlives the call that starts it ([`ExecSpec::background`]).
    #[must_use]
    pub fn background(mut self) -> Self {
        self.background = true;
        self
    }

    /// Set a variable (never a secret).
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(name.into(), value.into());
        self
    }

    /// Hide these variables of the caller's own environment from the process.
    #[must_use]
    pub fn hide<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.hide.extend(names.into_iter().map(Into::into));
        self
    }
}

/// A started process, for [`EnvSession::kill`]: unique within the session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecId(String);

impl ExecId {
    /// An id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the process was started as one that outlives its call ([`ExecSpec::background`]):
    /// the id starts with [`BACKGROUND_PREFIX`].
    pub fn is_background(&self) -> bool {
        self.0.starts_with(BACKGROUND_PREFIX)
    }
}

/// What an [`ExecId`] of a process that outlives its call starts with: `adam-exec active-fg` does
/// not count the processes whose record starts with it.
pub const BACKGROUND_PREFIX: &str = "bg-";

/// The id of the `n`th process a session starts, `<kind>-<n>`, with [`BACKGROUND_PREFIX`] in front
/// when `spec` says the process outlives its call.
pub fn exec_id_for(spec: &ExecSpec, kind: &str, n: impl fmt::Display) -> ExecId {
    let lead = if spec.background {
        BACKGROUND_PREFIX
    } else {
        ""
    };
    ExecId::new(format!("{lead}{kind}-{n}"))
}

impl fmt::Display for ExecId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the caller spawns, in its own process: the command line an environment made of an
/// [`ExecSpec`].
///
/// The caller sets what is its own to set (a process group of its own, `kill_on_drop`, piped
/// stdio) and spawns [`command`](Self::command), or reads the fields when it starts the process
/// some other way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCommand {
    /// The program to spawn.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// The working directory.
    pub cwd: PathBuf,
    /// Variables to set.
    pub env: BTreeMap<OsString, OsString>,
    /// Start from an empty environment (the process sees only `env`). [`Local`] does not;
    /// an environment that runs the process elsewhere does, so that nothing of the caller leaks.
    pub env_clear: bool,
    /// Variables of the caller's environment to remove. Wins over `env`.
    pub env_remove: Vec<String>,
    /// The id to give to [`EnvSession::kill`].
    pub exec: ExecId,
}

impl PreparedCommand {
    /// A [`tokio::process::Command`] with the program, the arguments, the working directory and the
    /// environment of this command (`env_clear`, then `env`, then `env_remove`, so a name in both
    /// `env` and `env_remove` is removed). Nothing else is set: no stdio, no process group.
    pub fn command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args).current_dir(&self.cwd);
        if self.env_clear {
            cmd.env_clear();
        }
        for (name, value) in &self.env {
            cmd.env(name, value);
        }
        for name in &self.env_remove {
            cmd.env_remove(name);
        }
        cmd
    }
}

/// What a sandbox reserves for the processes in it: the shape a command's time is weighted by when an
/// environment meters it ([`EnvSession::sandbox_shape`]).
///
/// A run pod's is the **requests** of the run container in the deployment's pod template (what the
/// scheduler reserves and the quota counts; limits are not a cost). A request the template does not
/// make is `None`, which is not zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SandboxShape {
    /// CPU, in thousandths of a core (`250` is `250m`).
    pub cpu_millis: Option<u64>,
    /// Memory, in bytes.
    pub memory_bytes: Option<u64>,
}

impl SandboxShape {
    /// A shape that reserves `cpu_millis` and `memory_bytes`.
    pub fn new(cpu_millis: Option<u64>, memory_bytes: Option<u64>) -> Self {
        Self {
            cpu_millis,
            memory_bytes,
        }
    }
}

/// What an environment is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvKind {
    /// The caller's own container.
    Local,
    /// A pod of its own in a Kubernetes cluster (`adam-env-kubernetes`), made from the deployment's
    /// pod template.
    Kubernetes {
        /// The pod's name.
        pod: String,
        /// The image of the container the processes run in, when the template names one.
        image: Option<String>,
    },
}

/// An environment, for a person to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvDescription {
    /// What it is.
    pub kind: EnvKind,
    /// One line.
    pub summary: String,
}

/// Where a step of making an environment stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvStepState {
    /// Working on it.
    Running,
    /// Done.
    Completed,
    /// Ended in failure.
    Failed,
}

/// One step of making an environment (pulling an image, building it, starting it): what the
/// caller shows while a slow [`Environment::ensure`] goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvStep {
    /// Names the step; the same id again updates it. Unique within one `ensure`.
    pub id: String,
    /// A plain one-line label.
    pub label: String,
    /// Where it stands.
    pub state: EnvStepState,
    /// Plain text: a result, a failure, what it is doing now. Never a secret.
    pub detail: Option<String>,
}

impl EnvStep {
    /// A step with no detail.
    pub fn new(id: impl Into<String>, label: impl Into<String>, state: EnvStepState) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            state,
            detail: None,
        }
    }

    /// The same step with a detail.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// Where an [`Environment::ensure`] reports its steps. The caller maps them to what it shows.
pub trait EnvProgress: Send + Sync {
    /// A step started, moved on or ended.
    fn step(&self, step: EnvStep);
}

/// An [`EnvProgress`] that drops every step.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl EnvProgress for NoProgress {
    fn step(&self, _step: EnvStep) {}
}

/// A shared [`Environment`].
pub type DynEnvironment = Arc<dyn Environment>;

/// Where a workspace's processes run, and what that costs to hold.
///
/// One value serves every run of a process; the per-run state is in the [`EnvSession`] it
/// returns.
#[async_trait]
pub trait Environment: Send + Sync + 'static {
    /// The session of the run that owns `workspace`: made on first need, the same one after. It is
    /// **single-flight per run** (two calls for one run make it once; an implementation that makes
    /// something on a shared volume holds a lock for that, as the mirror lock does), and the
    /// caller may drop the future (a cancelled run) without leaving a half-made environment that a
    /// later call cannot use or [`release`](Self::release).
    ///
    /// # Errors
    ///
    /// [`EnvError`]: the environment cannot be had (not available, a configuration of the
    /// repository that is wrong, a build that failed, too slow). The caller reports it and the run
    /// goes on.
    async fn ensure(
        &self,
        workspace: &RunWorkspace,
        progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError>;

    /// Free what is held for `run`. **Idempotent**: a run that holds nothing is not an error. The
    /// janitor calls it before it removes the run's workspace.
    ///
    /// # Errors
    ///
    /// [`EnvError`] when something that is held cannot be freed; it stays held, and
    /// [`held_runs`](Self::held_runs) still says so.
    async fn release(&self, run: &str) -> Result<(), EnvError>;

    /// The runs this environment holds something for, for the sweep of what a crash left. [`Local`]
    /// holds nothing.
    ///
    /// # Errors
    ///
    /// [`EnvError`] when it cannot find out.
    async fn held_runs(&self) -> Result<Vec<String>, EnvError>;

    /// Make what is held for `workspace`'s run fit the slots it has **now**, right after the caller
    /// added one (a worktree, a scratch project, a project taken over from another run).
    ///
    /// The caller's file and git tools act on a slot by name and never call
    /// [`ensure`](Self::ensure), so an environment that shows the run its slots as places the run
    /// cannot rename or replace (a run pod mounts each slot on its own) must not wait for the next
    /// command to learn of the new one: until it does, a process of the run can still rename the
    /// slot's directory. `seal` throws away what is held when it lacks the new slot **and nothing
    /// runs in it**; the next `ensure` makes it again from the run's files.
    ///
    /// It never makes anything: a run that holds nothing has nothing to seal. The default does
    /// nothing, which is right for an environment that is the caller's own container or that sees
    /// the whole workspace ([`Local`]).
    ///
    /// # Errors
    ///
    /// [`EnvError::Unavailable`] when something is held that lacks the slot and a command runs in it
    /// (or it cannot be asked): the slot is added and stays, the caller tells whoever is waiting,
    /// and the next `ensure` makes the environment again once the command ends. Other errors as
    /// [`ensure`](Self::ensure).
    async fn seal(&self, workspace: &RunWorkspace) -> Result<(), EnvError> {
        let _ = workspace;
        Ok(())
    }

    /// Set or clear the image a run's commands run in (ADR 0052 decision 7), for a repository that
    /// brings its own `devcontainer.json`. `None` puts the run back on the deployment's image.
    ///
    /// The default refuses: only an environment that runs a run in a pod of its own has an image of
    /// the run's to choose (the caller's own container does not), so every outside
    /// implementation compiles unchanged and says why a call cannot work.
    ///
    /// An implementation records the choice where the run's state is and makes the pod again when it
    /// differs from the pod that is there; the choice is the conversation's, and the caller reads it
    /// again on every [`ensure`](Self::ensure).
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] when this environment has no image of a run's to choose, or the image is
    /// empty.
    async fn set_image(&self, run: &str, image: Option<&str>) -> Result<(), EnvError> {
        let _ = (run, image);
        Err(EnvError::Refused(
            "this environment has no image of a run's to choose: only run pods do".to_owned(),
        ))
    }

    /// The image prefixes this environment allows a repository to name, when it runs a run's
    /// commands in a pod of its own: `Some` for run pods (the list may be empty, which allows no
    /// repository image), `None` for an environment that has no run pod image to move (the caller's
    /// own container).
    ///
    /// The default is `None`. The tool that offers a repository's own image is offered only where
    /// this is `Some` with a prefix, and the deployment's admission policy is the control that
    /// holds whatever this says.
    fn repository_image_prefixes(&self) -> Option<&[String]> {
        None
    }
}

/// A run's view of its environment.
#[async_trait]
pub trait EnvSession: Send + Sync {
    /// What this environment is, for a person.
    fn describe(&self) -> EnvDescription;

    /// The command to spawn for `spec`.
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] for a spec this environment will not run (an empty `argv`, a `cwd`
    /// outside the workspace).
    fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError>;

    /// Stop the process `exec` and what it started, if it is running **in the environment**. The
    /// caller has already killed the process it spawned (its own process group), so this is for
    /// what lives where the caller cannot reach: [`Local`] has nothing more to do. It never fails:
    /// what it cannot do is logged, and a process that is already gone is not an error.
    async fn kill(&self, exec: &ExecId);

    /// What a process in this environment reserves, **when the environment meters the time of its
    /// processes**: the coder reports how long each command ran, weighted by this shape, as the run's
    /// sandbox usage (ADR 0043). `None`, the default, is an environment that is not metered (the
    /// caller's own container, a dev container): the usage says where the run was and not how long,
    /// which is *not measured* and not zero. An implementation that wraps another one must forward
    /// this ([`HidingEnvironment`] does), or the default hides the shape.
    fn sandbox_shape(&self) -> Option<SandboxShape> {
        None
    }
}

/// What can go wrong with an environment. No variant carries a secret.
///
/// Decide from [`Classify::class`]: `Unavailable`, `Lost` and `Timeout` are `Transient`, `Config`,
/// `Refused` and `Build` are `Invalid`, `Io` is `Internal`. A message describes this layer only;
/// [`adam_error::report`] prints the chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EnvError {
    /// The environment cannot be had now (its runtime is down, unreachable or not installed).
    #[error("the environment is not available: {0}")]
    Unavailable(String),
    /// A repository's configuration of its environment is wrong.
    #[error("{}: {reason}", file.display())]
    Config {
        /// The file.
        file: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The request is one this environment will not carry out.
    #[error("refused: {0}")]
    Refused(String),
    /// Making the environment failed.
    #[error("the environment could not be built: {reason}")]
    Build {
        /// What failed.
        reason: String,
        /// The end of the build's output, scrubbed.
        log_tail: String,
    },
    /// A phase took longer than it may.
    #[error("the environment did not get past {phase} in {secs} seconds")]
    Timeout {
        /// What it was doing (`pull`, `build`, `start`).
        phase: &'static str,
        /// The limit, in seconds.
        secs: u64,
    },
    /// The environment of a run that was there is gone (removed by someone, or its runtime
    /// restarted).
    #[error("the environment of the run was lost")]
    Lost,
    /// A local I/O error.
    #[error("the environment could not read or write a file")]
    Io(#[from] std::io::Error),
}

impl Classify for EnvError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Unavailable(_) | Self::Lost | Self::Timeout { .. } => ErrorClass::Transient,
            Self::Config { .. } | Self::Refused(_) | Self::Build { .. } => ErrorClass::Invalid,
            Self::Io(_) => ErrorClass::Internal,
        }
    }
}

/// The shell commands run in: `bash` when the image has one, else `sh`.
///
/// Models write bash (`${PIPESTATUS[0]}`, `[[ ]]`, arrays, `<(...)`), and where `sh` is dash they
/// fail with "Bad substitution" for reasons that have nothing to do with the project. Both are run
/// as login shells (`-l`), which is what keeps the toolchain's `PATH` from `/etc/profile.d`, since
/// Debian's `/etc/profile` resets it. Found once, on `PATH`.
pub fn login_shell() -> &'static str {
    static SHELL: OnceLock<&'static str> = OnceLock::new();
    SHELL.get_or_init(|| if on_path("bash") { "bash" } else { "sh" })
}

/// Whether an executable file called `name` is in a directory of `PATH`.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// The caller's own container as an [`Environment`]: nothing is made, nothing is held.
#[derive(Debug, Clone, Copy, Default)]
pub struct Local;

#[async_trait]
impl Environment for Local {
    async fn ensure(
        &self,
        _workspace: &RunWorkspace,
        _progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError> {
        Ok(Arc::new(LocalSession))
    }

    async fn release(&self, _run: &str) -> Result<(), EnvError> {
        Ok(())
    }

    async fn held_runs(&self) -> Result<Vec<String>, EnvError> {
        Ok(Vec::new())
    }
}

/// The session of [`Local`]: stateless, so a value is a session.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalSession;

impl LocalSession {
    /// A session as a shared trait object.
    pub fn shared() -> Arc<dyn EnvSession> {
        Arc::new(Self)
    }
}

#[async_trait]
impl EnvSession for LocalSession {
    fn describe(&self) -> EnvDescription {
        EnvDescription {
            kind: EnvKind::Local,
            summary: "this container".to_owned(),
        }
    }

    fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
        let (program, args) = match &spec.program {
            Program::Shell(command) => (
                PathBuf::from(login_shell()),
                vec![OsString::from("-lc"), OsString::from(command)],
            ),
            Program::Argv(argv) => {
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| EnvError::Refused("there is no program to run".to_owned()))?;
                (PathBuf::from(program), args.to_vec())
            }
        };
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Ok(PreparedCommand {
            program,
            args,
            cwd: spec.cwd.clone(),
            env: spec
                .env
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value)))
                .collect(),
            env_clear: false,
            env_remove: spec.hide.clone(),
            exec: exec_id_for(spec, "local", NEXT.fetch_add(1, Ordering::Relaxed)),
        })
    }

    async fn kill(&self, _exec: &ExecId) {}
}

/// An [`Environment`] that hides some variables of the caller's own environment from **every**
/// process of every run, whatever the spec says: the names are added to each [`ExecSpec::hide`]
/// before the inner session prepares it.
///
/// This is how a process that holds secrets keeps them from the code it runs for others (a
/// repository's checks, a command the model asked for, a coding agent): the names are the
/// deployment's, known at startup, and a caller that builds a spec cannot forget them. Everything
/// else is the inner environment's: describe, kill, secrets, tool paths, release.
///
/// ```
/// use adam_workspace::{ExecSpec, HidingEnvironment, Local, NoProgress};
/// # async fn demo(workspace: &adam_workspace::RunWorkspace) -> Result<(), adam_workspace::EnvError> {
/// use adam_workspace::Environment as _;
/// let env = HidingEnvironment::new(std::sync::Arc::new(Local), ["SEARCH_MCP_TOKEN"]);
/// let session = env.ensure(workspace, &NoProgress).await?;
/// let command = session.prepare(&ExecSpec::shell("env", "/work"))?;
/// assert_eq!(command.env_remove, ["SEARCH_MCP_TOKEN"]);
/// # Ok(()) }
/// ```
pub struct HidingEnvironment {
    inner: DynEnvironment,
    hidden: Arc<Vec<String>>,
}

impl HidingEnvironment {
    /// `inner`, with `names` hidden from every process. Duplicates and blanks are dropped.
    pub fn new<I, S>(inner: DynEnvironment, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut hidden: Vec<String> = Vec::new();
        for name in names.into_iter().map(Into::into) {
            if !name.trim().is_empty() && !hidden.contains(&name) {
                hidden.push(name);
            }
        }
        Self {
            inner,
            hidden: Arc::new(hidden),
        }
    }

    /// The names hidden, in the order they were given.
    pub fn hidden(&self) -> &[String] {
        &self.hidden
    }
}

impl fmt::Debug for HidingEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HidingEnvironment")
            .field("hidden", &self.hidden)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Environment for HidingEnvironment {
    async fn ensure(
        &self,
        workspace: &RunWorkspace,
        progress: &dyn EnvProgress,
    ) -> Result<Arc<dyn EnvSession>, EnvError> {
        let inner = self.inner.ensure(workspace, progress).await?;
        Ok(Arc::new(HidingSession {
            inner,
            hidden: Arc::clone(&self.hidden),
        }))
    }

    async fn release(&self, run: &str) -> Result<(), EnvError> {
        self.inner.release(run).await
    }

    async fn held_runs(&self) -> Result<Vec<String>, EnvError> {
        self.inner.held_runs().await
    }

    async fn seal(&self, workspace: &RunWorkspace) -> Result<(), EnvError> {
        self.inner.seal(workspace).await
    }

    async fn set_image(&self, run: &str, image: Option<&str>) -> Result<(), EnvError> {
        self.inner.set_image(run, image).await
    }

    fn repository_image_prefixes(&self) -> Option<&[String]> {
        self.inner.repository_image_prefixes()
    }
}

struct HidingSession {
    inner: Arc<dyn EnvSession>,
    hidden: Arc<Vec<String>>,
}

#[async_trait]
impl EnvSession for HidingSession {
    fn describe(&self) -> EnvDescription {
        self.inner.describe()
    }

    fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
        let mut spec = spec.clone();
        for name in self.hidden.iter() {
            if !spec.hide.contains(name) {
                spec.hide.push(name.clone());
            }
        }
        self.inner.prepare(&spec)
    }

    async fn kill(&self, exec: &ExecId) {
        self.inner.kill(exec).await;
    }

    fn sandbox_shape(&self) -> Option<SandboxShape> {
        self.inner.sandbox_shape()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_is_bash_where_there_is_one_and_sh_otherwise() {
        assert_eq!(login_shell() == "bash", on_path("bash"));
        assert!(on_path("sh"));
        assert!(!on_path("no-such-program-anywhere"));
    }

    #[test]
    fn local_prepares_a_shell_command_as_a_login_shell_that_keeps_the_callers_environment() {
        let spec = ExecSpec::shell("cargo test", "/work/workspaces/r/app")
            .env("CI", "1")
            .hide(["GITHUB_TOKEN", "DATABASE_URL"]);
        let prepared = LocalSession.prepare(&spec).unwrap();
        assert_eq!(prepared.program, PathBuf::from(login_shell()));
        assert_eq!(prepared.args, ["-lc", "cargo test"]);
        assert_eq!(prepared.cwd, PathBuf::from("/work/workspaces/r/app"));
        assert_eq!(
            prepared.env.get(&OsString::from("CI")),
            Some(&OsString::from("1"))
        );
        assert!(!prepared.env_clear, "the caller's environment is kept");
        assert_eq!(prepared.env_remove, ["GITHUB_TOKEN", "DATABASE_URL"]);
    }

    #[test]
    fn local_prepares_a_program_and_its_arguments_as_they_are() {
        let spec = ExecSpec::argv(["git", "status"], "/w");
        let prepared = LocalSession.prepare(&spec).unwrap();
        assert_eq!(prepared.program, PathBuf::from("git"));
        assert_eq!(prepared.args, ["status"]);
        assert!(prepared.env.is_empty() && prepared.env_remove.is_empty());
    }

    #[test]
    fn an_empty_argv_is_refused() {
        let err = LocalSession
            .prepare(&ExecSpec::argv(Vec::<OsString>::new(), "/w"))
            .unwrap_err();
        assert!(matches!(err, EnvError::Refused(_)), "{err}");
        assert_eq!(err.class(), ErrorClass::Invalid);
    }

    #[test]
    fn every_command_gets_an_id_of_its_own() {
        let spec = ExecSpec::shell("true", "/w");
        let a = LocalSession.prepare(&spec).unwrap().exec;
        let b = LocalSession.prepare(&spec).unwrap().exec;
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("local-"), "{a}");
    }

    #[test]
    fn local_says_what_it_is() {
        assert_eq!(LocalSession.describe().kind, EnvKind::Local);
    }

    #[test]
    fn errors_classify_as_the_port_says() {
        let transient = [
            EnvError::Unavailable("the run pod is down".into()),
            EnvError::Lost,
            EnvError::Timeout {
                phase: "build",
                secs: 600,
            },
        ];
        for e in transient {
            assert_eq!(e.class(), ErrorClass::Transient, "{e}");
        }
        let invalid = [
            EnvError::Config {
                file: PathBuf::from(".devcontainer/devcontainer.json"),
                reason: "no image".into(),
            },
            EnvError::Refused("no".into()),
            EnvError::Build {
                reason: "exit 1".into(),
                log_tail: String::new(),
            },
        ];
        for e in invalid {
            assert_eq!(e.class(), ErrorClass::Invalid, "{e}");
        }
        let io = EnvError::from(std::io::Error::other("disk"));
        assert_eq!(io.class(), ErrorClass::Internal);
        assert!(
            std::error::Error::source(&io).is_some(),
            "the cause is the source"
        );
    }

    #[tokio::test]
    async fn a_prepared_command_hides_what_it_is_told_to_and_the_hiding_wins() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ExecSpec::shell(
            r#"printf '%s|%s' "${SET-unset}" "${HIDDEN-unset}""#,
            dir.path(),
        )
        .env("SET", "yes")
        .env("HIDDEN", "set by the caller")
        .hide(["HIDDEN"]);
        let prepared = LocalSession.prepare(&spec).unwrap();
        let out = prepared.command().output().await.unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "yes|unset");
    }

    #[tokio::test]
    async fn a_prepared_command_can_start_from_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut prepared = LocalSession
            .prepare(&ExecSpec::argv(["/usr/bin/env"], dir.path()).env("ONLY", "this"))
            .unwrap();
        prepared.env_clear = true;
        let out = prepared.command().output().await.unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ONLY=this");
    }

    #[tokio::test]
    async fn local_holds_nothing() {
        assert!(Local.held_runs().await.unwrap().is_empty());
        Local.release("any-run").await.unwrap();
        Local.release("any-run").await.unwrap();
    }

    fn a_run(dir: &std::path::Path) -> RunWorkspace {
        crate::Workspaces::new(dir.join("root"), Arc::new(crate::StaticToken::new("t")))
            .run("run-1")
            .unwrap()
    }

    /// An environment that holds nothing and records the runs it was asked to seal.
    #[derive(Default)]
    struct Sealing {
        sealed: std::sync::Mutex<Vec<String>>,
        refuses: std::sync::atomic::AtomicBool,
        images: std::sync::Mutex<Vec<(String, Option<String>)>>,
        prefixes: Vec<String>,
    }

    #[async_trait]
    impl Environment for Sealing {
        async fn ensure(
            &self,
            _workspace: &RunWorkspace,
            _progress: &dyn EnvProgress,
        ) -> Result<Arc<dyn EnvSession>, EnvError> {
            Err(EnvError::Lost)
        }

        async fn release(&self, _run: &str) -> Result<(), EnvError> {
            Ok(())
        }

        async fn held_runs(&self) -> Result<Vec<String>, EnvError> {
            Ok(Vec::new())
        }

        async fn seal(&self, workspace: &RunWorkspace) -> Result<(), EnvError> {
            self.sealed.lock().unwrap().push(workspace.run().to_owned());
            if self.refuses.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(EnvError::Unavailable("a command runs".to_owned()));
            }
            Ok(())
        }

        async fn set_image(&self, run: &str, image: Option<&str>) -> Result<(), EnvError> {
            self.images
                .lock()
                .unwrap()
                .push((run.to_owned(), image.map(str::to_owned)));
            Ok(())
        }

        fn repository_image_prefixes(&self) -> Option<&[String]> {
            (!self.prefixes.is_empty()).then_some(self.prefixes.as_slice())
        }
    }

    #[tokio::test]
    async fn local_has_nothing_to_seal() {
        let dir = tempfile::tempdir().unwrap();
        Local.seal(&a_run(dir.path())).await.unwrap();
        assert!(Local.held_runs().await.unwrap().is_empty());
    }

    /// ADR 0052 decision 7: the image of a run is the port's, and the default says no, so every
    /// outside implementation compiles unchanged and a call that cannot work says why.
    #[tokio::test]
    async fn local_has_no_image_of_a_run_to_choose_and_no_prefixes() {
        let error = Local
            .set_image("run-1", Some("ghcr.io/acme/dev:1"))
            .await
            .unwrap_err();
        assert!(matches!(error, EnvError::Refused(_)), "{error}");
        assert!(error.to_string().contains("only run pods"), "{error}");
        assert_eq!(Local.repository_image_prefixes(), None);
    }

    #[tokio::test]
    async fn a_hiding_environment_forwards_the_image_and_the_prefixes_of_the_environment_it_wraps()
    {
        let inner = Arc::new(Sealing {
            prefixes: vec!["ghcr.io/acme/".to_owned()],
            ..Sealing::default()
        });
        let env = HidingEnvironment::new(inner.clone(), ["MCP_SECRET"]);
        assert_eq!(
            env.repository_image_prefixes(),
            Some(["ghcr.io/acme/".to_owned()].as_slice())
        );
        env.set_image("run-1", Some("ghcr.io/acme/dev:1"))
            .await
            .unwrap();
        env.set_image("run-1", None).await.unwrap();
        assert_eq!(
            *inner.images.lock().unwrap(),
            [
                ("run-1".to_owned(), Some("ghcr.io/acme/dev:1".to_owned())),
                ("run-1".to_owned(), None),
            ]
        );
    }

    #[tokio::test]
    async fn a_hiding_environment_seals_what_the_environment_it_wraps_holds() {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(Sealing::default());
        let env = HidingEnvironment::new(inner.clone(), ["MCP_SECRET"]);
        env.seal(&a_run(dir.path())).await.unwrap();
        assert_eq!(*inner.sealed.lock().unwrap(), ["run-1"]);
        // What the wrapped environment says is what the caller hears.
        inner
            .refuses
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let refused = env.seal(&a_run(dir.path())).await.unwrap_err();
        assert!(matches!(refused, EnvError::Unavailable(_)), "{refused}");
    }

    #[tokio::test]
    async fn a_hiding_environment_hides_its_names_from_every_command_and_keeps_the_specs_own() {
        let dir = tempfile::tempdir().unwrap();
        let env = HidingEnvironment::new(
            Arc::new(Local),
            ["MCP_SECRET", " ", "MCP_SECRET", "OTHER_SECRET"],
        );
        assert_eq!(env.hidden(), ["MCP_SECRET", "OTHER_SECRET"]);
        let workspace = crate::Workspaces::new(
            dir.path().join("root"),
            Arc::new(crate::StaticToken::new("t")),
        )
        .run("run-1")
        .unwrap();
        let session = env.ensure(&workspace, &NoProgress).await.unwrap();
        assert_eq!(session.describe().kind, EnvKind::Local);

        // The caller's spec names one variable of its own and not ours; the command still has
        // both removed, and the spec's own `env` for a hidden name does not bring it back.
        let spec = ExecSpec::shell(
            r#"printf '%s|%s|%s' "${MCP_SECRET-unset}" "${OTHER_SECRET-unset}" "${KEPT-unset}""#,
            dir.path(),
        )
        .env("MCP_SECRET", "set by the caller")
        .env("OTHER_SECRET", "set by the caller")
        .env("KEPT", "kept")
        .hide(["GITHUB_TOKEN"]);
        let prepared = session.prepare(&spec).unwrap();
        assert_eq!(
            prepared.env_remove,
            ["GITHUB_TOKEN", "MCP_SECRET", "OTHER_SECRET"]
        );
        let out = prepared.command().output().await.unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|unset|kept");

        // The rest is the inner environment's.
        assert!(env.held_runs().await.unwrap().is_empty());
        env.release("any").await.unwrap();
    }

    /// An environment whose sessions say they are metered.
    struct Metered;

    struct MeteredSession;

    #[async_trait]
    impl EnvSession for MeteredSession {
        fn describe(&self) -> EnvDescription {
            EnvDescription {
                kind: EnvKind::Kubernetes {
                    pod: "adam-run-x".to_owned(),
                    image: None,
                },
                summary: "a metered pod".to_owned(),
            }
        }

        fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
            LocalSession.prepare(spec)
        }

        async fn kill(&self, _exec: &ExecId) {}

        fn sandbox_shape(&self) -> Option<SandboxShape> {
            Some(SandboxShape::new(Some(250), Some(512 * 1024 * 1024)))
        }
    }

    #[async_trait]
    impl Environment for Metered {
        async fn ensure(
            &self,
            _workspace: &RunWorkspace,
            _progress: &dyn EnvProgress,
        ) -> Result<Arc<dyn EnvSession>, EnvError> {
            Ok(Arc::new(MeteredSession))
        }

        async fn release(&self, _run: &str) -> Result<(), EnvError> {
            Ok(())
        }

        async fn held_runs(&self) -> Result<Vec<String>, EnvError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn an_environment_is_not_metered_unless_its_session_says_so_and_wrapping_it_keeps_what_it_says()
     {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            LocalSession.sandbox_shape(),
            None,
            "the caller's own container is not metered"
        );

        let hiding = HidingEnvironment::new(Arc::new(Local), ["MCP_SECRET"]);
        let session = hiding
            .ensure(&a_run(dir.path()), &NoProgress)
            .await
            .unwrap();
        assert_eq!(session.sandbox_shape(), None);

        let hiding = HidingEnvironment::new(Arc::new(Metered), ["MCP_SECRET"]);
        let session = hiding
            .ensure(&a_run(dir.path()), &NoProgress)
            .await
            .unwrap();
        assert_eq!(
            session.sandbox_shape(),
            Some(SandboxShape::new(Some(250), Some(512 * 1024 * 1024))),
            "the wrapper forwards the shape: the default would hide it"
        );
    }
}
