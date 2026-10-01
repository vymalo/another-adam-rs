//! Dev reload (feature `dev`): read the agent directory at run time, and swap the agents when a
//! file changes.
//!
//! [`LiveAssembly`] does what [`AgentDef::from_source`] + [`AgentDef::bind`] +
//! [`BoundDef::model`] do, but keeps the recipe, so it can do it again. It registers one stable
//! agent per name with the runtime; each `step` of such an agent asks for the current
//! [`LlmAgent`] and runs the whole transition with it. A reload that loads, binds and builds
//! swaps the versions in one step, and an invalid one changes nothing.
//!
//! The rules for runs that are in flight (the replay rule) are on [`LiveAssembly`]; the crate README
//! has the diagrams.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use adam_agent_fs::{Diagnostic, Dir, ManifestSource, Strictness};
use adam_core::RunId;
use adam_llm_agent::{Conversation, LlmAgent, ToolSet};
use adam_model::DynModel;
use adam_runtime::{Agent, AgentError, Ctx, Inbound, RuntimeBuilder, Transition};
use async_trait::async_trait;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::assembly::{AgentInfo, BoundDef};
use crate::def::AgentDef;
use crate::error::Error;
use crate::folder::{agent_dir, project_root};
#[cfg(feature = "mcp")]
use crate::mcp::McpBinding;

/// How long the files must be quiet before a watched change is loaded (an editor writes a file in
/// several steps).
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(150);

type DefHook = Arc<dyn Fn(AgentDef) -> AgentDef + Send + Sync>;
type BoundHook = Arc<dyn Fn(BoundDef) -> BoundDef + Send + Sync>;

/// Everything a reload repeats, in the order the stages run.
struct Recipe {
    root: PathBuf,
    default_name: Option<String>,
    strictness: Strictness,
    tools: ToolSet,
    defs: Vec<DefHook>,
    bounds: Vec<BoundHook>,
    model: DynModel,
    alias: String,
    debounce: Duration,
    /// The MCP connections made once by [`LiveBuilder::connect_mcp`], by root agent: given to
    /// every definition a load makes, so a reload reuses them.
    #[cfg(feature = "mcp")]
    mcp: BTreeMap<String, McpBinding>,
}

/// One agent of a load, ready to install.
struct Loaded {
    name: String,
    agent: Arc<LlmAgent>,
    shape: Shape,
    info: AgentInfo,
}

impl Recipe {
    fn dir(&self) -> Dir {
        let dir = Dir::new(self.root.clone());
        match &self.default_name {
            Some(name) => dir.default_name(name.clone()),
            None => dir,
        }
    }

    /// Load, bind and build: the whole pipeline, from the files. The warnings of a load that
    /// succeeds are returned.
    fn build(&self) -> Result<(Vec<Loaded>, Vec<Diagnostic>), Error> {
        let dir = self.dir();
        let report = dir.load()?;
        let warnings: Vec<Diagnostic> = report.warnings().cloned().collect();
        let package = report.into_package(self.strictness)?;
        let mut loaded = Vec::new();
        for manifest in package.agents {
            let mut def = AgentDef::from_manifest(manifest)?.resources_from(&dir)?;
            for hook in &self.defs {
                def = hook(def);
            }
            // After the hooks: the connections were made once, and a hook must not replace them.
            #[cfg(feature = "mcp")]
            if let Some(binding) = self.mcp.get(def.name()) {
                def = def.with_mcp_binding(binding.clone());
            }
            let mut bound = def.bind(self.tools.clone())?;
            for hook in &self.bounds {
                bound = hook(bound);
            }
            let assembly = bound.model(Arc::clone(&self.model), self.alias.clone())?;
            for (agent, info) in assembly.agents().iter().zip(assembly.info()) {
                loaded.push(Loaded {
                    name: info.name.clone(),
                    agent: Arc::new(agent.clone()),
                    shape: info.tools.iter().cloned().collect(),
                    info: info.clone(),
                });
            }
        }
        Ok((loaded, warnings))
    }

    /// What a watcher watches: `agent/` or `agents/`, recursively.
    fn watch_paths(&self) -> Result<Vec<PathBuf>, WatchError> {
        let paths: Vec<PathBuf> = ["agent", "agents"]
            .iter()
            .map(|name| self.root.join(name))
            .filter(|path| path.is_dir())
            .collect();
        if paths.is_empty() {
            return Err(WatchError::NothingToWatch {
                root: self.root.clone(),
            });
        }
        Ok(paths)
    }
}

/// The tools of an agent, by name. Two versions with the same shape can step the same run.
type Shape = BTreeSet<String>;

struct Version {
    shape: Shape,
    agent: Arc<LlmAgent>,
}

/// One registered name: its newest version, the older versions that runs are still on, and which
/// run is on which.
struct Slot {
    current: Version,
    older: Vec<Version>,
    pins: HashMap<RunId, Shape>,
    present: bool,
}

impl Slot {
    fn new(version: Version) -> Self {
        Self {
            current: version,
            older: Vec::new(),
            pins: HashMap::new(),
            present: true,
        }
    }

    fn version_for(&self, shape: &Shape) -> &Version {
        if self.current.shape == *shape {
            return &self.current;
        }
        self.older
            .iter()
            .find(|v| v.shape == *shape)
            .unwrap_or(&self.current)
    }

    /// The agent that steps `run`: the one it is pinned to, else (and now pinned to) the newest.
    fn acquire(&mut self, run: RunId) -> Arc<LlmAgent> {
        let shape = self
            .pins
            .entry(run)
            .or_insert_with(|| self.current.shape.clone())
            .clone();
        Arc::clone(&self.version_for(&shape).agent)
    }

    fn release(&mut self, run: RunId) {
        self.pins.remove(&run);
        self.prune();
    }

    /// Put a new version in place. The same tool set replaces the current version for everyone; a
    /// different one becomes current and the old one stays for the runs pinned to it.
    fn install(&mut self, next: Version) {
        if next.shape == self.current.shape {
            self.current = next;
        } else {
            // A version older than this one with the shape of `next` is replaced: the runs on it
            // follow the newest of their tool set.
            self.older.retain(|v| v.shape != next.shape);
            let previous = std::mem::replace(&mut self.current, next);
            self.older.push(previous);
        }
        self.present = true;
        self.prune();
    }

    fn prune(&mut self) {
        let pins = &self.pins;
        self.older
            .retain(|v| pins.values().any(|shape| *shape == v.shape));
    }

    fn on_previous_tools(&self) -> usize {
        self.pins
            .values()
            .filter(|shape| **shape != self.current.shape)
            .count()
    }
}

/// The versions of every registered name, and what the last reload said.
struct Registry {
    slots: BTreeMap<String, Slot>,
    /// The number of loads that succeeded, the first one included.
    generation: u64,
    infos: Vec<AgentInfo>,
    last_error: Option<Arc<ReloadError>>,
}

impl Registry {
    /// Swap in a load, or say which of its agent names the runtime does not know (and change
    /// nothing).
    fn install(&mut self, loaded: Vec<Loaded>) -> Result<Reloaded, Vec<String>> {
        let added: Vec<String> = loaded
            .iter()
            .filter(|l| !self.slots.contains_key(&l.name))
            .map(|l| l.name.clone())
            .collect();
        if !added.is_empty() {
            return Err(added);
        }

        let mut changed = Vec::new();
        let mut tool_changes = Vec::new();
        for l in &loaded {
            match self.infos.iter().find(|i| i.name == l.name) {
                Some(old) if *old == l.info => {}
                Some(old) => {
                    changed.push(l.name.clone());
                    let (before, after): (BTreeSet<_>, BTreeSet<_>) = (
                        old.tools.iter().cloned().collect(),
                        l.info.tools.iter().cloned().collect(),
                    );
                    if before != after {
                        tool_changes.push(ToolChange {
                            agent: l.name.clone(),
                            added: after.difference(&before).cloned().collect(),
                            removed: before.difference(&after).cloned().collect(),
                        });
                    }
                }
                None => changed.push(l.name.clone()),
            }
        }
        let names: BTreeSet<&str> = loaded.iter().map(|l| l.name.as_str()).collect();
        let mut retired = Vec::new();
        for (name, slot) in &mut self.slots {
            if !names.contains(name.as_str()) && slot.present {
                slot.present = false;
                retired.push(name.clone());
            }
        }

        self.infos = loaded.iter().map(|l| l.info.clone()).collect();
        for l in loaded {
            if let Some(slot) = self.slots.get_mut(&l.name) {
                slot.install(Version {
                    shape: l.shape,
                    agent: l.agent,
                });
            }
        }
        self.generation += 1;
        self.last_error = None;
        Ok(Reloaded {
            generation: self.generation,
            changed,
            tool_changes,
            retired,
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Shared {
    recipe: Recipe,
    registry: Mutex<Registry>,
    /// One reload at a time, so two triggers cannot install out of order.
    serial: Mutex<()>,
}

/// What a successful reload did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reloaded {
    /// The number of this load: `1` is the first, and each success adds one.
    pub generation: u64,
    /// The agents whose [`AgentInfo`] differs from the version before (prompt, tools, model alias,
    /// limits, skills, ...), by registration name. Empty when the files said the same. A change
    /// the info does not show (a tool's description, a value given in code) is applied all the
    /// same.
    pub changed: Vec<String>,
    /// The agents whose tool set changed. Runs already on such an agent keep their tool set; see
    /// the docs of [`LiveAssembly`].
    pub tool_changes: Vec<ToolChange>,
    /// Agents the files no longer contain. They stay registered with their last version.
    pub retired: Vec<String>,
}

/// A change to the tool set of one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolChange {
    /// The agent, by registration name.
    pub agent: String,
    /// Tools it has now and had not, sorted.
    pub added: Vec<String>,
    /// Tools it had and has not, sorted.
    pub removed: Vec<String>,
}

/// Why a reload changed nothing. The last good version stays in force.
#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    /// The files could not be read, do not validate, or do not bind: the same [`Error`] a startup
    /// with these files would have died of.
    #[error("{0}")]
    Load(#[from] Error),
    /// The files contain agents that were not there at startup. The runtime registers its agents
    /// once, when it is built, so these cannot run until the process restarts.
    #[error(
        "the files now contain agent(s) that were not there at startup ({}); the runtime registers \
         its agents when it is built, so restart the process to use them",
        added.join(", ")
    )]
    NeedsRestart {
        /// The registration names that are new.
        added: Vec<String>,
    },
}

impl ReloadError {
    /// Every finding in the files when that is what refused the reload (file, line and message),
    /// else none.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            Self::Load(Error::Manifest(adam_agent_fs::Error::Invalid { diagnostics })) => {
                diagnostics
            }
            _ => &[],
        }
    }
}

/// Why a file watcher could not start.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The root has neither `agent/` nor `agents/`.
    #[error("nothing to watch: {} has neither `agent/` nor `agents/`", root.display())]
    NothingToWatch {
        /// The directory that was looked in.
        root: PathBuf,
    },
    /// The operating system's file notification could not be set up for `path`.
    #[error("cannot watch {}: {message}", path.display())]
    Backend {
        /// What was being watched.
        path: PathBuf,
        /// What the backend said.
        message: String,
    },
    /// The thread that reloads could not be started.
    #[error("cannot start the reload thread: {0}")]
    Thread(#[source] std::io::Error),
}

/// Builds a [`LiveAssembly`]. Made by [`LiveAssembly::builder`]; the hooks are the things a
/// startup does between the stages, and they run again, in order, on every reload.
///
/// ```
/// # use std::sync::Arc;
/// # use adam_assembly::LiveAssembly;
/// # use adam_model::MockModel;
/// # let root = std::env::temp_dir().join("adam-assembly-doc-live-builder");
/// # let _ = std::fs::remove_dir_all(&root);
/// # std::fs::create_dir_all(root.join("agent")).unwrap();
/// # std::fs::write(root.join("agent/instructions.md"), "---\nname: helper\n---\nHi.\n").unwrap();
/// # let (model, env) = (Arc::new(MockModel::new()), Arc::new(String::from("state")));
/// let live = LiveAssembly::builder(&root, model, "my-model")
///     .configure(|def| def.remote_timeout(std::time::Duration::from_secs(30)))
///     .configure_bound(move |bound| bound.state(env.clone()).wait_poll(std::time::Duration::from_secs(5)))
///     .load()?;
/// # let _ = std::fs::remove_dir_all(&root);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct LiveBuilder {
    recipe: Recipe,
}

impl LiveBuilder {
    /// The tools `tools:` may name, as [`AgentDef::bind`] takes them. Default: none.
    #[must_use]
    pub fn tools(mut self, tools: ToolSet) -> Self {
        self.recipe.tools = tools;
        self
    }

    /// Whether warnings in the files fail a load too. Default [`Strictness::Lenient`]: warnings
    /// are logged, and only errors keep the last good version.
    #[must_use]
    pub fn strictness(mut self, strictness: Strictness) -> Self {
        self.recipe.strictness = strictness;
        self
    }

    /// The name of a root agent whose frontmatter has none, as [`Dir::default_name`].
    #[must_use]
    pub fn default_name(mut self, name: impl Into<String>) -> Self {
        self.recipe.default_name = Some(name.into());
        self
    }

    /// Change each [`AgentDef`] before it is bound: [`var`](AgentDef::var),
    /// [`env`](AgentDef::env), [`remote_timeout`](AgentDef::remote_timeout),
    /// [`allow_insecure_remotes`](AgentDef::allow_insecure_remotes). It runs once per agent
    /// definition on every load; `def.name()` says which. Hooks run in the order they were added.
    #[must_use]
    pub fn configure(
        mut self,
        hook: impl Fn(AgentDef) -> AgentDef + Send + Sync + 'static,
    ) -> Self {
        self.recipe.defs.push(Arc::new(hook));
        self
    }

    /// Change each [`BoundDef`] before its model is set: [`state`](BoundDef::state),
    /// [`model_aliases`](BoundDef::model_aliases), [`wait_poll`](BoundDef::wait_poll). The value a
    /// hook gives to `state` is shared by every version.
    #[must_use]
    pub fn configure_bound(
        mut self,
        hook: impl Fn(BoundDef) -> BoundDef + Send + Sync + 'static,
    ) -> Self {
        self.recipe.bounds.push(Arc::new(hook));
        self
    }

    /// How long the files must be quiet before a [`watch`](LiveAssembly::watch) loads them.
    /// Default [`DEFAULT_DEBOUNCE`].
    #[must_use]
    pub fn debounce(mut self, quiet: Duration) -> Self {
        self.recipe.debounce = quiet;
        self
    }

    /// Connect to the MCP servers of the agents' `mcp.json` files, once, now. Only with the
    /// features `dev` and `mcp`.
    ///
    /// Reads the directory, applies the [`configure`](Self::configure) hooks added so far (an
    /// [`env`](AgentDef::env) given there is what `${VAR}` sees), and calls
    /// [`AgentDef::connect_mcp`] for each root. The connections are kept and given to every load,
    /// so **they outlive reloads**: a reload does not start a process or open a session. Add the
    /// hooks that give the environment before this call.
    ///
    /// A reload whose `mcp.json` differs from the one that was connected is refused, as any load
    /// error is (the last good version stays, and the message says to restart): tools are
    /// discovered once, at startup, and are not looked up again.
    ///
    /// The connections are made here and not in [`load`](Self::load) or
    /// [`reload`](LiveAssembly::reload) because those are synchronous, and a reload may run on the
    /// thread of the file watcher: nothing there can wait for a network.
    ///
    /// # Errors
    ///
    /// Whatever loading the files and [`AgentDef::connect_mcp`] refuse.
    #[cfg(feature = "mcp")]
    pub async fn connect_mcp(mut self, policy: &adam_mcp::McpPolicy) -> Result<Self, Error> {
        let dir = self.recipe.dir();
        let package = dir.load()?.into_package(self.recipe.strictness)?;
        for manifest in package.agents {
            let mut def = AgentDef::from_manifest(manifest)?;
            for hook in &self.recipe.defs {
                def = hook(def);
            }
            let def = def.connect_mcp(policy).await?;
            self.recipe
                .mcp
                .insert(def.name().to_owned(), def.mcp_binding().clone());
        }
        Ok(self)
    }

    /// Load the files for the first time. An error here is a startup error, as it is for
    /// [`AgentDef::bind`]: there is no last good version to keep yet.
    ///
    /// # Errors
    ///
    /// Whatever [`AgentDef::from_source`], [`AgentDef::bind`] and [`BoundDef::model`] refuse.
    pub fn load(self) -> Result<LiveAssembly, Error> {
        let (loaded, warnings) = self.recipe.build()?;
        for warning in &warnings {
            tracing::warn!(%warning, "agent file warning");
        }
        let infos = loaded.iter().map(|l| l.info.clone()).collect();
        let slots = loaded
            .into_iter()
            .map(|l| {
                let version = Version {
                    shape: l.shape,
                    agent: l.agent,
                };
                (l.name, Slot::new(version))
            })
            .collect();
        tracing::warn!(
            dir = %self.recipe.root.display(),
            "dev reload is on: agent files are read from disk and re-read when they change; \
             not for production"
        );
        Ok(LiveAssembly {
            shared: Arc::new(Shared {
                recipe: self.recipe,
                registry: Mutex::new(Registry {
                    slots,
                    generation: 1,
                    infos,
                    last_error: None,
                }),
                serial: Mutex::new(()),
            }),
        })
    }
}

/// The agents of a directory, reloaded when its files change. Only with the feature `dev`.
///
/// # The swap and durable replay
///
/// A run is durable: its journal is keyed by step names (`model:3`, `tool:<call id>`), and a
/// transition that is replayed (a lease lost, a stale commit) must take the same steps. The prompt
/// is not journaled, and the model request is rebuilt on every turn, so **a new prompt, new limits,
/// a new model alias, a new tool description or a rotated token apply to every run at its next
/// step**. What would break a replay is a change to *which tools exist*: a replayed transition that
/// finds a journaled `tool:c1` where its code now answers "unknown tool" fails with
/// `NonDeterminism`. So the rule is:
///
/// * The **tool set** of an agent is its tools' names. Agents with the same tool set are
///   interchangeable for any run, and a reload with an unchanged tool set swaps for everyone.
/// * When a reload **changes** the tool set of an agent, runs that already took a step of it keep
///   the version with the tool set they started with, for as long as they live; runs that start
///   later (and runs no process has stepped yet) get the new one. The old version stays as it was
///   at the reload, except that a later reload that goes back to the same tool set updates it
///   again. [`LiveAssembly::runs_on_previous_tools`] counts those runs.
/// * A **new agent name** (a new subagent, a renamed root) cannot be served: the runtime
///   registers its agents once, at build. Such a reload is refused as a whole with
///   [`ReloadError::NeedsRestart`], and the last good version stays.
/// * A **removed** agent stays registered with its last version, for the runs that are on it and
///   for the parents that still have the tool for it (a run pinned to the old tool set). It is
///   listed by [`LiveAssembly::retired`]. A run never fails because a file went away.
/// * A **restart is a deploy**: the pins are in memory, and a new process loads the files as they
///   are and steps every run with them, as a production deploy of new code does.
///
/// The pins are dropped when a run ends (done, failed, or failed for good with a permanent
/// error). A run that is cancelled while nobody is stepping it leaves a small entry until the
/// process ends, and keeps an old version alive with it; that is a development tool's price.
///
/// # Not for production
///
/// The feature is off by default, so a release build cannot watch and reload prompts unless its
/// author turned it on; turning it on logs a warning at startup. (Reading a folder once, when the
/// process starts, needs no feature: [`AgentFolder`](crate::AgentFolder).) Tool code is Rust and changes
/// with a rebuild. `mcp.json` tools are connected once, at startup
/// (`LiveBuilder::connect_mcp`, feature `mcp`), and the connections outlive reloads: an edit of
/// `mcp.json` is refused with a message that says to restart, because a tool discovered at startup
/// is not looked up again (and a run in flight may have called it).
///
/// ```
/// use std::sync::Arc;
/// use adam_assembly::LiveAssembly;
/// use adam_model::MockModel;
/// use adam_runtime::Runtime;
///
/// # let root = std::env::temp_dir().join("adam-assembly-doc-live");
/// # let _ = std::fs::remove_dir_all(&root);
/// # std::fs::create_dir_all(root.join("agent")).unwrap();
/// # std::fs::write(root.join("agent/instructions.md"), "---\nname: helper\n---\nBe brief.\n").unwrap();
/// # let store: adam_core::DynStore = Arc::new(adam_core::MemoryStore::new());
/// # let model = Arc::new(MockModel::new());
/// let live = LiveAssembly::builder(&root, model, "my-model").load()?;
/// let runtime = live.register(Runtime::builder(store)).build(); // stable names, current version
/// let _watch = live.watch()?; // keep it alive; dropping it stops the watching
///
/// // An edit is loaded when the files have been quiet for a moment, and `reload` does it now.
/// std::fs::write(root.join("agent/instructions.md"), "---\nname: helper\n---\nBe thorough.\n")?;
/// live.reload().map_err(|e| e.to_string())?;
/// assert_eq!(live.info()[0].prompt, "Be thorough.");
/// # let _ = std::fs::remove_dir_all(&root);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct LiveAssembly {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for LiveAssembly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let registry = lock(&self.shared.registry);
        f.debug_struct("LiveAssembly")
            .field("dir", &self.shared.recipe.root)
            .field("generation", &registry.generation)
            .field("agents", &registry.slots.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl LiveAssembly {
    /// Start describing a live assembly of the agents in `dir` (the directory that holds `agent/`
    /// or `agents/`, or that directory itself), talking to `model` under the gateway alias
    /// `alias` when an agent names none: the arguments of [`BoundDef::model`]. The environment
    /// variable [`AGENT_DIR_ENV`](crate::AGENT_DIR_ENV) replaces `dir` when it is set.
    pub fn builder(
        dir: impl Into<PathBuf>,
        model: DynModel,
        alias: impl Into<String>,
    ) -> LiveBuilder {
        LiveBuilder {
            recipe: Recipe {
                root: project_root(agent_dir(dir)),
                default_name: None,
                strictness: Strictness::Lenient,
                tools: ToolSet::new(),
                defs: Vec::new(),
                bounds: Vec::new(),
                model,
                alias: alias.into(),
                debounce: DEFAULT_DEBOUNCE,
                #[cfg(feature = "mcp")]
                mcp: BTreeMap::new(),
            },
        }
    }

    /// Register every agent (the roots and each subagent) on a runtime builder, by the names they
    /// had when the process started. What the runtime steps is not an [`LlmAgent`] but a stand-in
    /// that takes the current version at each step.
    pub fn register(&self, builder: RuntimeBuilder) -> RuntimeBuilder {
        let names: Vec<String> = lock(&self.shared.registry).slots.keys().cloned().collect();
        names.into_iter().fold(builder, |builder, name| {
            builder.agent(LiveAgent {
                name,
                shared: Arc::clone(&self.shared),
            })
        })
    }

    /// Load the files again now, exactly as a watched change does: on success the versions are
    /// swapped, on failure nothing changes. Either way the outcome is logged, and a failure is
    /// kept for [`last_error`](Self::last_error). It reads the disk on the calling thread.
    ///
    /// # Errors
    ///
    /// A [`ReloadError`], shared with `last_error`.
    pub fn reload(&self) -> Result<Reloaded, Arc<ReloadError>> {
        let _serial = lock(&self.shared.serial);
        let outcome = match self.shared.recipe.build() {
            Ok((loaded, warnings)) => {
                for warning in &warnings {
                    tracing::warn!(%warning, "agent file warning");
                }
                lock(&self.shared.registry)
                    .install(loaded)
                    .map_err(|added| ReloadError::NeedsRestart { added })
            }
            Err(error) => Err(ReloadError::Load(error)),
        };
        match outcome {
            Ok(reloaded) => {
                log_reloaded(&reloaded);
                Ok(reloaded)
            }
            Err(error) => {
                let error = Arc::new(error);
                let mut registry = lock(&self.shared.registry);
                registry.last_error = Some(Arc::clone(&error));
                log_refused(registry.generation, &error);
                Err(error)
            }
        }
    }

    /// Start watching the agent directory: a change loads the files, once they have been quiet
    /// for the [debounce](LiveBuilder::debounce). The watching stops when the returned [`Watch`]
    /// is dropped. Failures of a reload are logged and kept for [`last_error`](Self::last_error).
    ///
    /// # Errors
    ///
    /// [`WatchError`] when there is nothing to watch or the platform refuses.
    pub fn watch(&self) -> Result<Watch, WatchError> {
        let paths = self.shared.recipe.watch_paths()?;
        let (tx, rx) = mpsc::channel::<Msg>();
        let events = tx.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) if is_change(&event.kind) => {
                    let _ = events.send(Msg::Changed);
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "the agent file watcher reported an error"),
            })
            .map_err(|e| WatchError::Backend {
                path: self.shared.recipe.root.clone(),
                message: e.to_string(),
            })?;
        for path in &paths {
            watcher
                .watch(path, RecursiveMode::Recursive)
                .map_err(|e| WatchError::Backend {
                    path: path.clone(),
                    message: e.to_string(),
                })?;
        }
        let live = self.clone();
        let quiet = self.shared.recipe.debounce;
        let thread = std::thread::Builder::new()
            .name("adam-dev-reload".into())
            .spawn(move || reload_when_quiet(&live, &rx, quiet))
            .map_err(WatchError::Thread)?;
        tracing::info!(paths = ?paths, "watching the agent files");
        Ok(Watch {
            watcher: Some(watcher),
            stop: tx,
            thread: Some(thread),
        })
    }

    /// The number of loads that succeeded: `1` after startup, and one more for each reload that
    /// swapped. A reload that was refused leaves it as it was.
    pub fn generation(&self) -> u64 {
        lock(&self.shared.registry).generation
    }

    /// Why the last reload was refused; `None` when it succeeded (or none has run). The
    /// diagnostics are in [`ReloadError::diagnostics`].
    pub fn last_error(&self) -> Option<Arc<ReloadError>> {
        lock(&self.shared.registry).last_error.clone()
    }

    /// What the agents in force were made from: the root of each package, then its subagents.
    /// Retired agents are not listed.
    pub fn info(&self) -> Vec<AgentInfo> {
        lock(&self.shared.registry).infos.clone()
    }

    /// Agents that are registered but no longer in the files: they keep their last version.
    pub fn retired(&self) -> Vec<String> {
        lock(&self.shared.registry)
            .slots
            .iter()
            .filter(|(_, slot)| !slot.present)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// How many runs are being stepped with a tool set that a reload has replaced.
    pub fn runs_on_previous_tools(&self) -> usize {
        lock(&self.shared.registry)
            .slots
            .values()
            .map(Slot::on_previous_tools)
            .sum()
    }

    /// The directory being read.
    pub fn dir(&self) -> &Path {
        &self.shared.recipe.root
    }
}

fn log_reloaded(reloaded: &Reloaded) {
    tracing::info!(
        generation = reloaded.generation,
        changed = ?reloaded.changed,
        "reloaded the agent files"
    );
    for change in &reloaded.tool_changes {
        tracing::info!(
            agent = %change.agent,
            added = ?change.added,
            removed = ?change.removed,
            "the tool set changed: runs already started keep theirs, new runs get this one"
        );
    }
    for name in &reloaded.retired {
        tracing::warn!(agent = %name, "the agent is no longer in the files; it keeps its last version");
    }
}

fn log_refused(generation: u64, error: &ReloadError) {
    for diagnostic in error.diagnostics() {
        tracing::error!(%diagnostic, "agent file problem");
    }
    tracing::error!(
        generation,
        %error,
        "reload refused, keeping the last good version"
    );
}

/// Whether an event may have changed what a load reads. Reads (`Access`) do not, except a file
/// closed after a write, and a reload that read the disk must not trigger the next one.
fn is_change(kind: &EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, MetadataKind, ModifyKind};
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime | MetadataKind::Extended,
        )) => false,
        EventKind::Any
        | EventKind::Create(_)
        | EventKind::Modify(_)
        | EventKind::Remove(_)
        | EventKind::Other => true,
    }
}

enum Msg {
    Changed,
    Stop,
}

/// Wait for a change, then for the files to be quiet, then reload; until told to stop.
fn reload_when_quiet(live: &LiveAssembly, rx: &mpsc::Receiver<Msg>, quiet: Duration) {
    loop {
        match rx.recv() {
            Ok(Msg::Changed) => {}
            Ok(Msg::Stop) | Err(_) => return,
        }
        loop {
            match rx.recv_timeout(quiet) {
                Ok(Msg::Changed) => {}
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        // The outcome is logged and kept by `reload`.
        let _ = live.reload();
    }
}

/// A running file watch. Dropping it stops the watching and the thread that reloads.
pub struct Watch {
    watcher: Option<RecommendedWatcher>,
    stop: mpsc::Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watch").finish_non_exhaustive()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // The watcher first, so that no event races the stop message.
        drop(self.watcher.take());
        let _ = self.stop.send(Msg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The agent the runtime steps: a name, and whatever version the registry holds for it when a
/// step starts.
struct LiveAgent {
    name: String,
    shared: Arc<Shared>,
}

impl LiveAgent {
    fn agent_for_start(&self) -> Option<Arc<LlmAgent>> {
        lock(&self.shared.registry)
            .slots
            .get(&self.name)
            .map(|slot| Arc::clone(&slot.current.agent))
    }
}

/// Whether the run cannot step again after this outcome, so its pin can go.
fn ends_run<S>(outcome: &Result<Transition<S>, AgentError>) -> bool {
    matches!(
        outcome,
        Ok(Transition::Done { .. } | Transition::Fail { .. })
            | Err(AgentError::Permanent { .. } | AgentError::NonDeterminism { .. })
    )
}

#[async_trait]
impl Agent for LiveAgent {
    type State = Conversation;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Conversation, AgentError> {
        match self.agent_for_start() {
            Some(agent) => agent.init(input),
            None => Err(AgentError::permanent(format!(
                "agent `{}` is not loaded",
                self.name
            ))),
        }
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Conversation,
        prior_run: RunId,
    ) -> Result<Conversation, AgentError> {
        match self.agent_for_start() {
            Some(agent) => agent.init_continuing(input, prior, prior_run),
            None => Err(AgentError::permanent(format!(
                "agent `{}` is not loaded",
                self.name
            ))),
        }
    }

    async fn step(
        &self,
        ctx: &mut Ctx,
        state: Conversation,
    ) -> Result<Transition<Conversation>, AgentError> {
        let run = ctx.run_id();
        // The lock is held to pick the version and not across the step.
        let agent = lock(&self.shared.registry)
            .slots
            .get_mut(&self.name)
            .map(|slot| slot.acquire(run));
        let Some(agent) = agent else {
            return Err(AgentError::permanent(format!(
                "agent `{}` is not loaded",
                self.name
            )));
        };
        let outcome = agent.step(ctx, state).await;
        if ends_run(&outcome)
            && let Some(slot) = lock(&self.shared.registry).slots.get_mut(&self.name)
        {
            slot.release(run);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_model::MockModel;
    use notify::event::{AccessKind, AccessMode, CreateKind, MetadataKind, ModifyKind, RemoveKind};

    use super::*;

    fn version(tools: &[&str]) -> Version {
        Version {
            shape: tools.iter().map(|t| (*t).to_owned()).collect(),
            agent: Arc::new(
                LlmAgent::builder("a", Arc::new(MockModel::new()), "alias")
                    .instructions(tools.join(","))
                    .build(),
            ),
        }
    }

    fn same(a: &Arc<LlmAgent>, b: &Arc<LlmAgent>) -> bool {
        Arc::ptr_eq(a, b)
    }

    #[test]
    fn a_run_stays_on_the_tool_set_it_started_with() {
        let mut slot = Slot::new(version(&["a"]));
        let (r1, r2) = (RunId::new(), RunId::new());
        let first = slot.acquire(r1);

        // The same tool set: swapped for everyone, r1 included.
        slot.install(version(&["a"]));
        assert!(!same(&slot.acquire(r1), &first));
        assert!(slot.older.is_empty());
        assert_eq!(slot.on_previous_tools(), 0);

        // A different tool set: r1 keeps the last version of its own, r2 (new) gets the new one.
        let before = slot.acquire(r1);
        slot.install(version(&["a", "b"]));
        assert!(same(&slot.acquire(r1), &before));
        let newest = Arc::clone(&slot.current.agent);
        assert!(same(&slot.acquire(r2), &newest));
        assert_eq!(slot.on_previous_tools(), 1);

        // Going back to the old tool set updates the version r1 is on.
        slot.install(version(&["a"]));
        assert!(!same(&slot.acquire(r1), &before));
        assert!(same(&slot.acquire(r1), &slot.current.agent.clone()));
        // r2 keeps `a, b`.
        assert_eq!(slot.on_previous_tools(), 1);
        assert_eq!(slot.older.len(), 1);

        // A run that ends lets its version go.
        slot.release(r2);
        assert!(slot.older.is_empty());
        assert_eq!(slot.on_previous_tools(), 0);
    }

    #[test]
    fn a_version_nobody_is_on_is_dropped_at_once() {
        let mut slot = Slot::new(version(&["a"]));
        slot.install(version(&["b"]));
        assert!(slot.older.is_empty(), "no run was pinned to `a`");
    }

    #[test]
    fn only_writes_and_structure_changes_trigger_a_reload() {
        assert!(is_change(&EventKind::Create(CreateKind::File)));
        assert!(is_change(&EventKind::Remove(RemoveKind::Folder)));
        assert!(is_change(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        // Reading the files (which a reload does) must not start another reload.
        assert!(!is_change(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_change(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!is_change(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
        ))));
    }
}
