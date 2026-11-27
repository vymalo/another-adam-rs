//! [`AgentDef`]: a manifest plus the values the code supplies, and [`AgentDef::bind`], which
//! checks it against the registered tools and renders the prompts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use adam_agent_fs::{
    AgentManifest, Diagnostic, EmbeddedAgent, InstructionPart, Instructions, Limits as FileLimits,
    ManifestSource, McpConfig, ModelRef, SkillSelection, Strictness, Subagent, ToolList,
    ToolsLoading,
};
use adam_core::{DynSecretSource, ProcessEnv};
use adam_llm_agent::{DynTool, DynToolSource, Limits, LlmStarter, SESSION_TOOLS, ToolSet};
use adam_model::ReasoningEffort;
use adam_runtime::{RuntimeBuilder, SessionLimits};
use secrecy::SecretString;

use crate::assembly::{BoundDef, Rounds};
use crate::error::{Error, Origin, ToolClash};
use crate::mcp::{self, McpBinding};
#[cfg(feature = "remote")]
use crate::remote::RemoteSubagentTool;
#[cfg(feature = "remote")]
use crate::remote_session::{RemoteRoundAgent, RemoteRoundStarter};
use crate::skills::{self, RunSkills, RunSkillsHandle, SkillFiles, SkillSet};
use crate::subagent::{DEFAULT_SUBAGENT_MINUTES, SubagentTool};
use crate::suggest::closest;
use crate::template::{self, Piece};

/// Something an [`AgentDef`] can be made from: the owned manifest that a directory produces, or
/// the `'static` one that `build.rs` embeds. Both end up as the same [`AgentManifest`], so an
/// agent binds the same way wherever its files came from.
pub trait IntoManifest {
    /// The owned manifest.
    ///
    /// # Errors
    ///
    /// An embedded manifest whose frontmatter cannot be decoded: the generated code and
    /// `adam-agent-fs` are not the same version.
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error>;

    /// The bytes of the files the skills bundle. The default is none, which is right for a
    /// manifest whose skills bundle nothing; a directory's manifest gets them from
    /// [`AgentDef::resources_from`] and an embedded agent brings its own.
    fn skill_files(&self) -> SkillFiles {
        SkillFiles::default()
    }
}

impl IntoManifest for AgentManifest {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        Ok(self)
    }
}

impl IntoManifest for &AgentManifest {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        Ok(self.clone())
    }
}

impl IntoManifest for EmbeddedAgent {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        self.to_manifest()
    }

    fn skill_files(&self) -> SkillFiles {
        SkillFiles::from_embedded(self)
    }
}

impl IntoManifest for &EmbeddedAgent {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        self.to_manifest()
    }

    fn skill_files(&self) -> SkillFiles {
        SkillFiles::from_embedded(self)
    }
}

/// How long a parent waits for a remote task before it answers the call with an error result
/// (the default of [`AgentDef::remote_timeout`]).
pub(crate) const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(60 * 60);

/// What a deployment gives the agent files that name a secret: the bearer token of a remote
/// subagent (`auth: bearer:VAR`) and the `${VAR}` of an `mcp.json`.
#[derive(Debug, Clone)]
#[cfg_attr(not(any(feature = "remote", feature = "mcp")), allow(dead_code))]
pub(crate) struct Secrets {
    /// Values given in code ([`AgentDef::env`]), taken before `source`.
    pub(crate) env: Arc<BTreeMap<String, SecretString>>,
    /// Where a name with no value in `env` is asked: the process environment unless the host
    /// says otherwise ([`AgentDef::secrets`], ADR 0047).
    pub(crate) source: DynSecretSource,
}

impl Default for Secrets {
    fn default() -> Self {
        Self {
            env: Arc::default(),
            source: Arc::new(ProcessEnv),
        }
    }
}

/// What a deployment decides about remote (`a2a:`) subagents, given to [`AgentDef`] and used at
/// bind. It exists without the feature `remote`, where no remote subagent binds, so the setters
/// keep their signatures in every build.
#[derive(Debug, Clone)]
pub(crate) struct RemoteSettings {
    /// Allow `http` to hosts that are not this machine.
    #[cfg_attr(not(feature = "remote"), allow(dead_code))]
    pub(crate) allow_insecure: bool,
    /// How long a parent waits for a remote task.
    pub(crate) max_wait: Duration,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            allow_insecure: false,
            max_wait: DEFAULT_MAX_WAIT,
        }
    }
}

/// An agent as its files describe it, before it is tied to tools, state and a model.
///
/// Made from a manifest with [`from_manifest`](Self::from_manifest) (the embedded one from
/// `adam::include_agent!()`, or the one a directory loads: the same code path), it takes the
/// values the code supplies for `{{placeholders}}`, and [`bind`](Self::bind) checks it against
/// the registered tools:
///
/// ```
/// # use adam_assembly::AgentDef;
/// # use adam_agent_fs::{Dir, ManifestSource, Strictness};
/// # use adam_llm_agent::ToolSet;
/// # let root = std::env::temp_dir().join("adam-assembly-doc-def");
/// # let _ = std::fs::remove_dir_all(&root);
/// # std::fs::create_dir_all(root.join("agent")).unwrap();
/// # std::fs::write(root.join("agent/instructions.md"),
/// #     "---\nname: helper\nvars: { tone: plain }\n---\nAnswer in a {{tone}} style.\n").unwrap();
/// let package = Dir::new(&root).load()?.into_package(Strictness::Lenient)?;
/// let def = AgentDef::from_manifest(package.agents[0].clone())?
///     .var("tone", "formal")
///     .bind(ToolSet::new())?;
/// # let _ = std::fs::remove_dir_all(&root);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct AgentDef {
    manifest: AgentManifest,
    /// Values supplied by the code, by agent (its registration name) and var.
    values: BTreeMap<String, BTreeMap<String, String>>,
    /// The bytes of the files the skills bundle.
    files: SkillFiles,
    /// What the deployment decides about remote (`a2a:`) subagents.
    remote: RemoteSettings,
    /// The values and the source of the secrets the files name.
    secrets: Secrets,
    /// The MCP tools of each agent: connected from its `mcp.json`, or supplied by hand.
    mcp: McpBinding,
    /// The skills the root's run finds on the way ([`run_skills`](Self::run_skills)).
    run_skills: Option<RunSkillsHandle>,
    /// What the deployment allows of the sessions of one conversation
    /// ([`session_limits`](Self::session_limits)).
    session_limits: SessionLimits,
    /// The root's reasoning level when its file sets none
    /// ([`reasoning_effort`](Self::reasoning_effort)).
    reasoning_effort: Option<ReasoningEffort>,
    /// The compiled plugins `plugins:` may name ([`plugin`](Self::plugin)).
    plugins: BTreeMap<String, PluginFactory>,
}

/// The plugins one agent runs, by registered name.
pub(crate) type NodePlugins = Vec<(String, Arc<dyn adam_plugin::Plugin>)>;

/// Makes a plugin from the options an agent file gives it.
#[derive(Clone)]
pub(crate) struct PluginFactory(Arc<MakePlugin>);

type MakePlugin = dyn Fn(&BTreeMap<String, String>) -> Arc<dyn adam_plugin::Plugin> + Send + Sync;

impl std::fmt::Debug for PluginFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginFactory")
    }
}

impl AgentDef {
    /// A definition from a manifest: `&AGENT` from `adam::include_agent!()`, or an
    /// [`AgentManifest`] from a directory.
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] when an embedded manifest cannot be decoded.
    pub fn from_manifest(manifest: impl IntoManifest) -> Result<Self, Error> {
        let files = manifest.skill_files();
        Ok(Self {
            manifest: manifest.into_manifest()?,
            values: BTreeMap::new(),
            files,
            remote: RemoteSettings::default(),
            secrets: Secrets::default(),
            mcp: McpBinding::default(),
            run_skills: None,
            session_limits: SessionLimits::default(),
            reasoning_effort: None,
            plugins: BTreeMap::new(),
        })
    }

    /// Read the files the skills bundle (`references/`, `scripts/`, `assets/`) from `source`,
    /// the source the manifest came from, so that `read_skill_file` can serve them. Only a
    /// manifest made without its source needs this: an embedded agent brings its files, and
    /// [`from_source`](Self::from_source) calls this itself.
    ///
    /// The files are read now, once, so that a missing one is a startup error and a run never
    /// touches the disk.
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] when a file cannot be read, and [`Error::SkillTooLarge`] for a skill
    /// whose files are over [`SKILL_RESOURCE_LIMIT`](adam_agent_fs::SKILL_RESOURCE_LIMIT).
    pub fn resources_from(mut self, source: &impl ManifestSource) -> Result<Self, Error> {
        self.files = SkillFiles::read(&self.manifest, source)?;
        Ok(self)
    }

    /// One definition per agent of a source: one for an `agent/` package, one per directory of
    /// an `agents/` package. `strictness` decides whether warnings fail the load, as they do for
    /// a build script (`Strictness::Strict` is `build("agent").strict()`).
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] when the source cannot be read or has errors.
    pub fn from_source(
        source: &impl ManifestSource,
        strictness: Strictness,
    ) -> Result<Vec<Self>, Error> {
        source
            .load()?
            .into_package(strictness)?
            .agents
            .into_iter()
            .map(|agent| Self::from_manifest(agent)?.resources_from(source))
            .collect()
    }

    /// The root agent's name.
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// The manifest this definition was made from.
    pub fn manifest(&self) -> &AgentManifest {
        &self.manifest
    }

    /// Add the MCP servers of `extra` to the root agent's own (`mcp.json`), so that
    /// `connect_mcp` (feature `mcp`) connects both and `bind` checks the sum. The servers of
    /// subagents are untouched. A name that both have is an error and nothing is added: a file
    /// that is added over an agent can never replace one of its servers. `file` names where
    /// `extra` came from, for the diagnostic.
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] with an [`Invalid`](adam_agent_fs::Error::Invalid) error that names `file`
    /// and every clashing server.
    pub fn with_extra_mcp(mut self, extra: McpConfig, file: &Path) -> Result<Self, Error> {
        let own = self.manifest.mcp.take().unwrap_or_default();
        match own.clone().merged_with(extra) {
            Ok(merged) => {
                self.manifest.mcp = Some(merged);
                Ok(self)
            }
            Err(names) => {
                let diagnostics = names
                    .iter()
                    .map(|name| {
                        Diagnostic::error(
                            file,
                            None,
                            format!(
                                "the server `{name}` is also in the agent's own `mcp.json`: \
                                 rename it in this file (a server of the agent is never replaced)"
                            ),
                        )
                    })
                    .collect();
                Err(Error::Manifest(adam_agent_fs::Error::Invalid {
                    diagnostics,
                }))
            }
        }
    }

    /// Read the file `file` (the `mcpServers` shape of `mcp.json`, parsed by the same loader) and
    /// add its servers with [`with_extra_mcp`](Self::with_extra_mcp). Returns the definition and
    /// what the loader warned about (an unknown key, a literal credential), for the caller to log.
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`]: the file cannot be read ([`Io`](adam_agent_fs::Error::Io)), has errors
    /// (invalid JSON, a bad server: every finding, as `path:line: error: ...`), or names a server
    /// the agent already has.
    pub fn with_extra_mcp_file(self, file: &Path) -> Result<(Self, Vec<Diagnostic>), Error> {
        let text = std::fs::read_to_string(file).map_err(|source| {
            Error::Manifest(adam_agent_fs::Error::Io {
                path: file.to_path_buf(),
                source,
            })
        })?;
        let mut diagnostics = Vec::new();
        let config = adam_agent_fs::parse_mcp(file, &text, &mut diagnostics);
        let Some(config) = config.filter(|_| !diagnostics.iter().any(Diagnostic::is_error)) else {
            return Err(Error::Manifest(adam_agent_fs::Error::Invalid {
                diagnostics,
            }));
        };
        Ok((self.with_extra_mcp(config, file)?, diagnostics))
    }

    /// The names of the environment variables the MCP servers of this definition (every local
    /// agent's, extra servers included) refer to as `${NAME}` or `${NAME:-default}`, sorted: what
    /// a deployment hides from the processes the agent starts and registers with its redactor.
    /// Names only, never values.
    pub fn mcp_env_references(&self) -> BTreeSet<String> {
        let mut agents = Vec::new();
        mcp::local_agents(&self.manifest, &self.manifest.name, &mut agents);
        agents
            .into_iter()
            .filter_map(|(_, manifest)| manifest.mcp.as_ref())
            .flat_map(McpConfig::env_references)
            .collect()
    }

    /// Supply the value of a var of the root agent. It overrides the default under `vars`, and
    /// is the only way to give a value to a var declared without one (`vars: { repo: }`).
    #[must_use]
    pub fn var(self, name: impl Into<String>, value: impl ToString) -> Self {
        let root = self.manifest.name.clone();
        self.agent_var(root, name, value)
    }

    /// Supply the value of a var of another agent, by the name it is registered under
    /// (`coder/reviewer`). Naming an agent the definition does not contain fails at
    /// [`bind`](Self::bind).
    #[must_use]
    pub fn agent_var(
        mut self,
        agent: impl Into<String>,
        name: impl Into<String>,
        value: impl ToString,
    ) -> Self {
        self.values
            .entry(agent.into())
            .or_default()
            .insert(name.into(), value.to_string());
        self
    }

    /// Give an environment variable a value here, in code, for the agent files that read one:
    /// `auth: bearer:BILLING_TOKEN` on a remote subagent reads `BILLING_TOKEN`. A value given here
    /// wins over the [`secrets`](Self::secrets) source (the process environment by default), so a composition root that fetches its secrets from a
    /// vault (or a test) does not have to put them in the environment. The value is held as a
    /// secret: it is not shown by `Debug`, and no error carries it.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.secrets.env).insert(name.into(), SecretString::from(value.into()));
        self
    }

    /// Where a variable that agent files name and [`env`](Self::env) does not give is asked: the
    /// process environment by default, which is a server's. An application that embeds adam gives
    /// its own source, or [`NoSecrets`](adam_core::NoSecrets), which refuses every name (ADR 0047,
    /// decision 5). It serves the bearer tokens of remote subagents (asked at [`bind`](Self::bind))
    /// and the `${VAR}` of the `mcp.json` files (asked at `connect_mcp`, so call this first).
    #[must_use]
    pub fn secrets(mut self, source: adam_core::DynSecretSource) -> Self {
        self.secrets.source = source;
        self
    }

    /// Allow remote subagents at plain `http` URLs that are not this machine: for development, or
    /// a service of the same cluster (`A2A_ALLOW_INSECURE_REMOTES` in `adam-agent`). **The messages
    /// and the bearer token cross the network in clear text**, so the network must be trusted on
    /// its own (a NetworkPolicy that admits only the caller, or a mesh with mTLS). Even then a card
    /// cannot widen it: an `https` card is never answered with a plain-`http` interface, and plain
    /// `http` goes only to the card's own host. Off by default: `bind` refuses such a URL with
    /// [`Error::RemoteUrl`], and the client refuses an agent card that offers such an interface.
    /// `localhost`, `*.localhost` and loopback addresses never need it.
    #[must_use]
    pub fn allow_insecure_remotes(mut self, allow: bool) -> Self {
        self.remote.allow_insecure = allow;
        self
    }

    /// What the deployment allows of the sessions of one conversation (ADR 0045 decision 5): the
    /// most open, the most working and the most rounds a session may have (default 16, 4 and 20).
    /// The root's file may **lower** each with `limits.max_open_sessions`,
    /// `limits.max_working_sessions` and `limits.max_session_rounds`, never raise it: a larger
    /// value is the deployment's, with a warning. A subagent's file has no say.
    #[must_use]
    pub fn session_limits(mut self, limits: SessionLimits) -> Self {
        self.session_limits = limits;
        self
    }

    /// The root's reasoning level when its file sets none (ADR 0063 decision 8): an embedded
    /// engine's host gives the level its folder would, and the root's own `reasoning_effort` wins
    /// over it. It is the **root's only**: it never applies to a subagent, which has its own file.
    /// It wins over the agent's `model_preset` and the profile. It is checked against the root
    /// model's profile at [`BoundDef::model`](crate::BoundDef::model) — not at
    /// [`bind`](Self::bind), which does not know the model — and the refusal reports the value as
    /// `reasoning_effort` on the agent's file even when this setter supplied it
    /// ([`EffortSource::Folder`](crate::EffortSource::Folder)): the setting is the file's, whether
    /// a file or the host gave it, so the message names the agent and its file. A level the profile
    /// does not take is [`Error::ReasoningEffort`]. `None` (the default) sets nothing.
    #[must_use]
    pub fn reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// The limits the sessions of the root agent run under: the deployment's
    /// ([`session_limits`](Self::session_limits)) lowered by the root's file. Pure, and the same
    /// on every process role, so a control plane that serves the card without binding the agent
    /// (its `maxOpen`) reads what the workers that run it enforce.
    pub fn effective_session_limits(&self) -> SessionLimits {
        lowered_by_file(
            self.session_limits,
            self.manifest.frontmatter.limits.as_ref(),
            None,
        )
    }

    /// What the root's A2A front serves `child-sessions/v1` for: each subagent of the root, local
    /// and remote, in the order of the manifest, with the time limit of its rounds, and the
    /// *effective* limits ([`effective_session_limits`](Self::effective_session_limits)). `None`
    /// when the root has no subagent: it has no session tools either. The same function on every
    /// process role and made without binding, so a control plane that never builds the agent
    /// serves the card and the sessions a worker's tools agree with.
    ///
    /// Only with the feature `sessions` (which `a2a` turns on).
    #[cfg(feature = "sessions")]
    pub fn child_sessions(&self) -> Option<adam_a2a_runtime::ChildSessions> {
        use adam_a2a_runtime::{ChildSessionAgent, ChildSessions};
        let limits = self.effective_session_limits();
        let helpers = session_targets(&self.manifest, &self.manifest.name, &self.remote)
            .into_iter()
            .map(|helper| {
                let agent = ChildSessionAgent::new(helper.name, helper.agent, helper.description);
                let agent = if helper.remote { agent.remote() } else { agent };
                match helper.within {
                    Some(within) => agent.within(within),
                    None => agent,
                }
            })
            .collect::<Vec<_>>();
        if helpers.is_empty() {
            return None;
        }
        match ChildSessions::new(helpers, limits.max_open()) {
            Ok(sessions) => Some(sessions.with_limits(limits)),
            Err(problem) => {
                tracing::warn!(agent = %self.manifest.name, %problem, "no child sessions");
                None
            }
        }
    }

    /// Register, on a process that serves the A2A front but does not step the root's helpers, the
    /// starters of each of them, local and remote, under `<root>/<name>`: what a person's session
    /// round needs to be started there. A worker with the full agents
    /// ([`Assembly::register`](crate::Assembly::register)) steps them. An agent with no subagent
    /// registers nothing. Do not call it on a runtime that also registers the assembly: a second
    /// registration under a name replaces the first.
    ///
    /// A process that registers only starters holds no endpoint, so it cannot send the
    /// `CancelTask` of a remote round it cancels. The front follows a cancel of a round, or an idle expiry, of a session
    /// on a remote helper with a closing round (`adam_a2a_runtime::ChildSessionAgent::remote`),
    /// which a worker steps and whose `RemoteRoundAgent` cancels the task. Without the feature
    /// `remote` a remote subagent gets no starter: [`bind`](Self::bind) refuses it
    /// ([`Error::RemoteNotEnabled`]).
    #[must_use]
    pub fn session_starters(&self, builder: RuntimeBuilder) -> RuntimeBuilder {
        let root = &self.manifest.name;
        self.manifest
            .subagents
            .iter()
            .fold(builder, |builder, sub| match sub {
                Subagent::Local(child) => {
                    builder.starter(LlmStarter::new(format!("{root}/{}", child.name)))
                }
                #[cfg(feature = "remote")]
                Subagent::Remote(agent) => {
                    builder.starter(RemoteRoundStarter::new(format!("{root}/{}", agent.name)))
                }
                // No remote subagent binds without the feature, so none has rounds to start.
                #[cfg(not(feature = "remote"))]
                Subagent::Remote(_) => builder,
            })
    }

    /// How long a parent waits for the task of a remote subagent before the call is answered with
    /// an error result (default one hour; the remote task is left where it is). A zero is raised to
    /// one millisecond.
    #[must_use]
    pub fn remote_timeout(mut self, max_wait: Duration) -> Self {
        self.remote.max_wait = max_wait.max(Duration::from_millis(1));
        self
    }

    /// Let the **root** agent load skills that its run finds on the way (the skills of a
    /// repository it prepared), through `skills`: a name with a `/` (`<source>/<skill>`) goes to
    /// it, a name without never does. With it set the root always has `load_skill` and
    /// `read_skill_file`, and its prompt gets one fixed sentence about them (also when the folder
    /// has no skill of its own), so what a run finds is a tool result and never changes the
    /// system prompt or the tool list. Subagents get none. `skills: []` in the root's
    /// frontmatter turns skills off altogether: no tool, no sentence, and `skills` is never called.
    ///
    /// Calling this again replaces it.
    #[must_use]
    pub fn run_skills(mut self, skills: Arc<dyn RunSkills>) -> Self {
        self.run_skills = Some(RunSkillsHandle(skills));
        self
    }

    /// Give an agent the tools of its MCP servers, made by code of your own: a client other than
    /// `adam-mcp`, or a test double. `agent` is the name the agent is registered under (`coder`,
    /// `coder/researcher`); each tool must be named `<server>__<tool>` after a server of **that
    /// agent's own** `mcp.json`. An agent whose `mcp.json` lists servers must be given tools here
    /// or by `connect_mcp` (feature `mcp`), or [`bind`](Self::bind) refuses it. Calling this again
    /// for the same agent replaces the tools.
    ///
    /// Checked at [`bind`](Self::bind): [`Error::McpForeignTool`], [`Error::McpToolClash`],
    /// [`Error::McpUnknownAgent`].
    #[must_use]
    pub fn mcp_tools(mut self, agent: impl Into<String>, tools: ToolSet) -> Self {
        self.mcp.set(agent, None, tools);
        self
    }

    /// Register a compiled plugin under `name`, so that an agent file may list it in `plugins:`
    /// (ADR 0050, slice H6). A subagent runs the plugins of its parent. Registering a name again
    /// replaces it. Only compiled plugins are named: an agent file never declares a program.
    ///
    /// The same plugin serves every agent that names it; use [`plugin_with`](Self::plugin_with)
    /// when the options of the file matter.
    ///
    /// Checked at [`bind`](Self::bind): [`Error::UnknownPlugin`] for a name nobody registered.
    #[must_use]
    pub fn plugin(self, name: impl Into<String>, plugin: Arc<dyn adam_plugin::Plugin>) -> Self {
        self.plugin_with(name, move |_| Arc::clone(&plugin))
    }

    /// Like [`plugin`](Self::plugin), made from the `options` the agent file gives (empty for a
    /// bare name).
    #[must_use]
    pub fn plugin_with(
        mut self,
        name: impl Into<String>,
        make: impl Fn(&BTreeMap<String, String>) -> Arc<dyn adam_plugin::Plugin> + Send + Sync + 'static,
    ) -> Self {
        self.plugins
            .insert(name.into(), PluginFactory(Arc::new(make)));
        self
    }

    /// Connect to the MCP servers of the root agent's `mcp.json` and of each local subagent's, and
    /// keep their tools for [`bind`](Self::bind). Only with the feature `mcp`.
    ///
    /// Each agent gets its own connections: two directories that both name a server `linear`
    /// connect twice, with their own headers. `${VAR}` in the files is expanded from
    /// [`env`](Self::env) first and then from the [`secrets`](Self::secrets) source (the process
    /// environment by default), so a token given in code reaches a server the same way it reaches
    /// a remote subagent. Everything that can be wrong
    /// (an unset variable, `type: sse`, a local process the policy does not allow, a server that
    /// is down, an allow-list that names a tool the server lacks) is an error here, at startup:
    /// see `adam_mcp::McpServers::connect`.
    ///
    /// The connections live as long as the tools do: as long as the [`Assembly`](crate::Assembly)
    /// built from this definition. Call it after the [`env`](Self::env) and
    /// [`secrets`](Self::secrets) calls it should see, and once: it connects again each time. A
    /// `secrets` called later reaches the remote subagents at `bind`, not these servers.
    ///
    /// # Errors
    ///
    /// [`Error::Mcp`], naming the agent and its `mcp.json`, for the first server that fails.
    #[cfg(feature = "mcp")]
    pub async fn connect_mcp(mut self, policy: &adam_mcp::McpPolicy) -> Result<Self, Error> {
        let env = adam_mcp::Env::from_values(Arc::clone(&self.secrets.env))
            .source(Arc::clone(&self.secrets.source));
        let mut agents = Vec::new();
        mcp::local_agents(&self.manifest, &self.manifest.name, &mut agents);
        let mut connected = Vec::new();
        for (name, manifest) in agents {
            let Some(config) = mcp::declared(manifest) else {
                continue;
            };
            let servers = adam_mcp::McpServers::connect(config, &env, policy)
                .await
                .map_err(|source| {
                    Error::mcp(Origin::new(name.clone(), mcp::file_of(manifest)), source)
                })?;
            connected.push((
                name,
                config.clone(),
                servers.tools(),
                servers.instructions_by_server(),
                servers.prompt_commands(),
            ));
        }
        for (name, config, tools, instructions, prompts) in connected {
            self.mcp
                .set_connected(name, config, tools, instructions, prompts);
        }
        Ok(self)
    }

    /// The MCP binding, to keep across reloads (dev reload).
    #[cfg(all(feature = "dev", feature = "mcp"))]
    pub(crate) fn mcp_binding(&self) -> &McpBinding {
        &self.mcp
    }

    /// Replace the MCP binding: dev reload gives a fresh definition the connections made once.
    #[cfg(all(feature = "dev", feature = "mcp"))]
    pub(crate) fn with_mcp_binding(mut self, binding: McpBinding) -> Self {
        self.mcp = binding;
        self
    }

    /// Check the definition against the registered tools and render the prompts.
    ///
    /// Everything that can be wrong with the files, given these tools, is found here, at
    /// startup, and not in the middle of a run:
    ///
    /// * a name in `tools:` that no tool has ([`Error::UnknownTool`], with a "did you mean"),
    ///   or a pattern (`linear__*`) that matches none;
    /// * a `{{placeholder}}` that `vars` does not declare, a var that is declared and never
    ///   used, a var with no value, a value supplied for an undeclared var, or a `{{` that is
    ///   not a placeholder;
    /// * a value supplied for an agent the definition does not contain;
    /// * a `retry:` name that is not one of the agent's tools once they are all bound
    ///   ([`Error::UnknownRetryTool`], with a "did you mean"; a tool of a declared MCP server that
    ///   gave no tools is skipped with a warning), or an entry that is no policy
    ///   ([`Error::InvalidRetry`]).
    ///
    /// The root agent gets every registered tool when it has no `tools:`; a subagent gets none
    /// (decision D3 of `docs/reference/agent-files.md`), whatever its parent has. Each agent then gets a
    /// tool per local subagent, named after it ([`SubagentTool`]), after its own tools and its
    /// skills' tools. Two more things are refused here:
    ///
    /// * a subagent tool whose name is already a tool of the parent, or another subagent's
    ///   ([`Error::SubagentToolClash`]);
    ///
    /// A remote subagent (`a2a:`) gets a tool of the same shape and the same name checks, and is
    /// checked here too: its URL must be https (or local, unless
    /// [`allow_insecure_remotes`](Self::allow_insecure_remotes)) and carry no credentials
    /// ([`Error::RemoteUrl`]), and the variable of its `auth: bearer:VAR` must hold a token
    /// ([`Error::RemoteAuth`]). The token is read now and kept in memory, and the network is not
    /// touched until the first call. Without the feature `remote` (on by default) there is no A2A
    /// client: the subagent is refused after the name checks and before any secret is read
    /// ([`Error::RemoteNotEnabled`]).
    ///
    /// An agent's MCP tools (`mcp.json`) come from its own directory only, and are part of the
    /// catalog `tools:` selects from, next to the registered tools (`tools: ['linear__*']`
    /// selects a server's tools; a subagent that lists none gets none). An agent whose `mcp.json`
    /// lists servers must have been given tools (`connect_mcp`, feature `mcp`, or
    /// [`mcp_tools`](Self::mcp_tools)): [`Error::McpNotConnected`] otherwise (fail closed). An
    /// empty set given for such an agent binds, with a warning naming the agent and its servers. The
    /// tools must come from the same `mcp.json` ([`Error::McpChanged`]), be named after its
    /// servers ([`Error::McpForeignTool`]) and not share a name with a registered tool
    /// ([`Error::McpToolClash`]).
    ///
    /// Nothing is built yet: the model and the state come next ([`BoundDef`]).
    ///
    /// # Errors
    ///
    /// The first problem found, in the order above; see [`Error`].
    pub fn bind(self, tools: ToolSet) -> Result<BoundDef, Error> {
        let catalog = Catalog::new(&tools)?;
        #[cfg(feature = "sessions")]
        let child_sessions = self.child_sessions();
        // An agent given MCP tools that the definition does not contain: a typo, found before the
        // walk, so that the message is about the name and not about a server that "is not
        // connected".
        let mut agents = Vec::new();
        mcp::local_agents(&self.manifest, &self.manifest.name, &mut agents);
        let agent_names: Vec<String> = agents.into_iter().map(|(name, _)| name).collect();
        if let Some(agent) = self.mcp.agents().find(|a| !agent_names.contains(a)) {
            return Err(Error::McpUnknownAgent {
                agent: agent.clone(),
                suggestion: closest(agent, agent_names.iter().map(String::as_str))
                    .map(str::to_owned),
                known: agent_names,
            });
        }
        let mut walk = Walk {
            catalog: &catalog,
            values: &self.values,
            files: &self.files,
            remote: &self.remote,
            secrets: &self.secrets,
            mcp: &self.mcp,
            plugins: &self.plugins,
            run_skills: self.run_skills.as_ref(),
            session_limits: lowered_by_file(
                self.session_limits,
                self.manifest.frontmatter.limits.as_ref(),
                Some(&self.manifest.name),
            ),
            // The host's level for the root when its file sets none (ADR 0063 decision 8).
            root_effort: self
                .reasoning_effort
                .map(|effort| effort.as_str().to_owned()),
            root_modes: None,
            nodes: Vec::new(),
            remotes: Vec::new(),
        };
        walk.agent(&self.manifest, self.manifest.name.clone(), None)?;
        let Walk {
            nodes,
            remotes,
            session_limits,
            ..
        } = walk;

        let known: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
        for agent in self.values.keys() {
            if !known.contains(agent) {
                return Err(Error::UnknownAgent {
                    agent: agent.clone(),
                    suggestion: closest(agent, known.iter().map(String::as_str)).map(str::to_owned),
                    known,
                });
            }
        }
        let root = self.manifest.name.clone();
        let bound = BoundDef::new(self.manifest, nodes, remotes, session_limits);
        #[cfg(feature = "sessions")]
        let bound = bound.with_child_sessions(child_sessions);
        #[cfg(feature = "mcp")]
        let bound = bound.with_prompt_commands(
            self.mcp
                .get(&root)
                .map(|given| given.prompts.clone())
                .unwrap_or_default(),
        );
        #[cfg(not(feature = "mcp"))]
        let _ = root;
        Ok(bound)
    }
}

/// The MCP tools of one agent: the tools `bind` may offer, and the names each server gave — from
/// the same check that ties a tool to its server ([`Walk::mcp_tools`]).
type McpTools = (Vec<(String, DynTool)>, BTreeMap<String, Vec<String>>);

/// One local agent, bound: its rendered prompt and its tools. The model comes later.
#[derive(Clone)]
pub(crate) struct Node {
    /// The registration name: `coder`, `coder/reviewer`.
    pub(crate) name: String,
    /// The index of the parent in the node list; `None` for the root.
    pub(crate) parent: Option<usize>,
    pub(crate) file: PathBuf,
    pub(crate) description: Option<String>,
    pub(crate) model: ModelRef,
    /// `reasoning_effort` of the agent's file (ADR 0063): the level its model calls ask for by
    /// default, as written. The assembly checks it against the model profile's levels.
    pub(crate) reasoning_effort: Option<String>,
    /// `model_preset` of the agent's file (ADR 0064): the named preset its model calls start from.
    pub(crate) model_preset: Option<String>,
    /// `model_fallback` of the agent's file (ADR 0064, amended 2026-11-23): the alias its run
    /// continues on, once, when its model is locked.
    pub(crate) model_fallback: Option<String>,
    pub(crate) prompt: String,
    pub(crate) tools: Vec<(String, DynTool)>,
    pub(crate) limits: Limits,
    /// `limits.max_output_tokens` of the agent's file, as written (ADR 0064): the cap the agent
    /// asks for, before the preset, the model's profile and the fallback. `None` when the file
    /// says nothing.
    pub(crate) max_output_tokens: Option<u32>,
    /// `limits.max_parallel` of the agent's file: it lowers the deployment's setting for this
    /// agent, and a larger value is clamped to it (ADR 0049, ADR 0054 decision 3).
    pub(crate) max_parallel: Option<usize>,
    /// `retry:` of the agent's file: the retry policy per tool, by name, each one of its tools
    /// (ADR 0061). The agent's own; a subagent's are its file's.
    pub(crate) retry: Vec<(String, adam_agent_fs::RetryDef, adam_runtime::RetryPolicy)>,
    /// The plugins the agent runs, by name, in order: its parent's, then its own (ADR 0050, H6).
    pub(crate) plugins: NodePlugins,
    /// The skills the agent may use, in order.
    pub(crate) skills: Vec<String>,
    /// The skills whose body is in the prompt.
    pub(crate) preloaded: Vec<String>,
    /// The registration names of the subagents a session can be started on, when the agent has the
    /// session tools; empty otherwise.
    pub(crate) session_agents: Vec<String>,
    /// The subagents a session can be started on, as [`add_subagent_tools`] found them (ADR 0054
    /// slice 1 part 2): the roster the session tools are made from, once the model — and with it
    /// the deployment's `MAX_PARALLEL_TOOL_CALLS` — is known. Empty unless the agent has them.
    pub(crate) session_targets: Vec<SessionTarget>,
    /// The agents that work the rounds of a session on a remote subagent, one per remote subagent
    /// of the root; empty for any other agent.
    pub(crate) round_agents: Rounds,
    /// The `mcp.json` servers whose `when` loads them only when a run's task matches: the server
    /// and its keywords, in name order. [`AgentInfo::when`](crate::AgentInfo::when) carries them, so
    /// a definition that differs only in a `when` compares unequal to one that does not.
    pub(crate) when: Vec<(String, Vec<String>)>,
    /// `tools_loading` of the agent's file (ADR 0066, decision 1): `eager` offers every tool in
    /// every request, `lazy` (the default) lets a run load them in groups. Part of the info because
    /// an eager agent with a `when` server behaves differently: nothing is withheld.
    pub(crate) tools_loading: ToolsLoading,
    /// Sources this agent alone has, added at [`BoundDef::build`](crate::BoundDef): the
    /// `mcp.json` servers whose `when` loads them only when a run's task matches.
    pub(crate) sources: Vec<DynToolSource>,
    /// The modes of `agent-mode/v1` the **root** declares (ADR 0039, decision 3), on every agent:
    /// a child starts in its parent's mode, so its tools are narrowed by the same set.
    pub(crate) modes: Option<std::sync::Arc<crate::modes::Modes>>,
}

/// One subagent a session may be started on, as `bind` found it ([ADR 0054] slice 1 part 2). What
/// [`BoundDef::build`](crate::BoundDef) still needs to make its [`SessionBounds`]: the values of
/// the folder's own are on the subagent's [`Node`], and the rest is the deployment's, known only
/// once the model is.
///
/// The same list makes the helpers of `child-sessions/v1` ([`AgentDef::child_sessions`]), so the
/// card, the A2A front and the session tools cannot disagree about who a session may run on.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
/// [`SessionBounds`]: adam_runtime::SessionBounds
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionTarget {
    /// The name the model writes in `agent`.
    pub(crate) name: String,
    /// The name it is registered under on the runtime (`coder/reviewer`).
    pub(crate) agent: String,
    pub(crate) description: String,
    /// The time limit of each of its rounds (`None`: none).
    pub(crate) within: Option<Duration>,
    pub(crate) remote: bool,
}

/// A remote (A2A) subagent, found on the way: what [`Assembly::remotes`](crate::Assembly::remotes) lists.
/// Its tool is made in `add_subagent_tools`.
#[derive(Debug, Clone)]
pub(crate) struct Remote {
    /// The registration name of the agent that owns it.
    pub(crate) parent: String,
    pub(crate) agent: adam_agent_fs::RemoteAgent,
}

/// The registered tools by name, in registration order.
struct Catalog {
    entries: Vec<(String, DynTool)>,
}

impl Catalog {
    fn new(tools: &ToolSet) -> Result<Self, Error> {
        let mut entries: Vec<(String, DynTool)> = Vec::new();
        for tool in tools {
            let name = tool.spec().name;
            if entries.iter().any(|(n, _)| *n == name) {
                return Err(Error::DuplicateTool { tool: name });
            }
            entries.push((name, tool.clone()));
        }
        Ok(Self { entries })
    }

    /// This catalog and `more` after it: the registered tools and one agent's own MCP tools.
    fn with(&self, more: &[(String, DynTool)]) -> Self {
        let mut entries = self.entries.clone();
        entries.extend(more.iter().cloned());
        Self { entries }
    }

    fn names(&self) -> Vec<String> {
        self.entries.iter().map(|(n, _)| n.clone()).collect()
    }

    fn get(&self, name: &str) -> Option<&(String, DynTool)> {
        self.entries.iter().find(|(n, _)| n == name)
    }
}

/// The walk over the agent tree, root first, each agent before its subagents.
struct Walk<'a> {
    catalog: &'a Catalog,
    values: &'a BTreeMap<String, BTreeMap<String, String>>,
    files: &'a SkillFiles,
    remote: &'a RemoteSettings,
    secrets: &'a Secrets,
    mcp: &'a McpBinding,
    plugins: &'a BTreeMap<String, PluginFactory>,
    /// What the root's run finds ([`AgentDef::run_skills`]); a subagent has none.
    run_skills: Option<&'a RunSkillsHandle>,
    /// What the root's sessions run under: the deployment's, lowered by the root's file.
    session_limits: SessionLimits,
    /// The root's reasoning level when its file sets none ([`AgentDef::reasoning_effort`]),
    /// as its wire word; it never applies to a subagent, which has its own file.
    root_effort: Option<String>,
    /// The root's modes (`modes/<name>.md`), once the root's tools are known: every agent of the
    /// assembly narrows by them, because a child run starts in its parent's mode.
    root_modes: Option<std::sync::Arc<crate::modes::Modes>>,
    nodes: Vec<Node>,
    remotes: Vec<Remote>,
}

impl Walk<'_> {
    /// The plugins of one agent: its parent's first, then its own that the parent does not have.
    /// A name nobody registered is [`Error::UnknownPlugin`].
    fn plugins_of(
        &self,
        origin: &Origin,
        manifest: &AgentManifest,
        parent: Option<usize>,
    ) -> Result<NodePlugins, Error> {
        let mut out: NodePlugins = parent
            .map(|index| self.nodes[index].plugins.clone())
            .unwrap_or_default();
        for entry in &manifest.frontmatter.plugins {
            let name = entry.name();
            let Some(factory) = self.plugins.get(name) else {
                let available: Vec<String> = self.plugins.keys().cloned().collect();
                return Err(Error::UnknownPlugin {
                    origin: origin.clone(),
                    plugin: name.to_owned(),
                    suggestion: closest(name, available.iter().map(String::as_str))
                        .map(str::to_owned),
                    available,
                });
            };
            if out.iter().all(|(have, _)| have != name) {
                out.push((name.to_owned(), (factory.0)(&entry.options())));
            }
        }
        Ok(out)
    }

    fn agent(
        &mut self,
        manifest: &AgentManifest,
        name: String,
        parent: Option<usize>,
    ) -> Result<(), Error> {
        let origin = Origin::new(name.clone(), manifest.path.clone());
        // The agent's own MCP tools, checked, then chosen from with the registered ones. The
        // grouping by server comes from the same loop that decides which server a tool belongs to.
        let (own_mcp, mcp_by_server) = self.mcp_tools(manifest, &name)?;
        let catalog = self.catalog.with(&own_mcp);
        let mut tools = resolve_tools(
            &origin,
            manifest.frontmatter.tools.as_ref(),
            parent.is_none(),
            &catalog,
        )?;
        let mut prompt = render_prompt(&origin, manifest, self.values.get(&name))?;
        // The servers whose `when` makes them groups: their tools are withheld by the source below
        // until the task holds a keyword or the run loads the group, and their instructions are the
        // group's note.
        //
        // An `eager` folder keeps every tool in every request (ADR 0066, decision 1): a `when`
        // withholds nothing, so its server's section stays in the prompt as it did before lazy
        // loading existed.
        let eager = manifest.frontmatter.tools_loading == ToolsLoading::Eager;
        let triggers: Vec<(String, Vec<String>)> = if eager {
            Vec::new()
        } else {
            mcp::declared(manifest)
                .into_iter()
                .flat_map(|config| config.servers.iter())
                .filter_map(|(server, entry)| {
                    entry
                        .when()
                        .map(|when| (server.clone(), when.keywords.clone()))
                })
                .collect()
        };
        // What the agent's MCP servers said about themselves, once, here: the prompt is the same
        // for every model call of the process, so the cached prefix holds. A triggered server's
        // section is its group's note: `Groups` gives it to the first prompt when the task loads
        // the group, and to the load's result when the run loads it.
        if let Some(given) = self.mcp.get(&name) {
            let eager: Vec<(String, String)> = given
                .instructions
                .iter()
                .filter(|(server, _)| !triggers.iter().any(|(triggered, _)| triggered == server))
                .cloned()
                .collect();
            let text = crate::mcp::join_sections(&eager);
            if !text.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&text);
            }
        }
        // The loaders reserve `load__`: a registered tool with the prefix is refused at bind even
        // when this agent does not select it (the catalogue is the host's), and so is a selected
        // MCP tool (a server named `load` is refused earlier, but a hand-built config may slip).
        if let Some(reserved) = self
            .catalog
            .names()
            .into_iter()
            .chain(tools.iter().map(|(tool, _)| tool.clone()))
            .find(|tool| tool.starts_with(adam_llm_agent::RESERVED_LOADER_PREFIX))
        {
            return Err(Error::ReservedLoaderName {
                origin: origin.clone(),
                tool: reserved,
            });
        }
        let mut sources: Vec<DynToolSource> = Vec::new();
        if !triggers.is_empty() {
            // Each server whose `when` did not match the task is one group, named after the
            // server: its tools are withheld until the group's loader is called (or the task holds
            // one of its keywords), and its instructions are the group's note, said in the load's
            // result rather than in the prompt (ADR 0066, decisions 2 and 3).
            let defs: Vec<crate::groups::Group> = triggers
                .iter()
                .map(|(server, keywords)| {
                    // Only the tools this agent selected: a load may not offer a tool the agent
                    // was never given (its `tools:` narrowed the server's list).
                    let members: Vec<String> = mcp_by_server
                        .get(server)
                        .map(|names| {
                            names
                                .iter()
                                .filter(|name| tools.iter().any(|(selected, _)| selected == *name))
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    let note = self
                        .mcp
                        .get(&name)
                        .and_then(|given| {
                            given
                                .instructions
                                .iter()
                                .find(|(named, _)| named == server)
                                .map(|(_, text)| text.clone())
                        })
                        .unwrap_or_default();
                    crate::groups::Group::for_server(server.clone(), note, members)
                        .with_keywords(keywords.clone())
                })
                .collect();
            let groups = crate::groups::Groups::bind(&origin, defs, &tools)?;
            tools.extend(groups.loader_tools());
            sources.push(Arc::new(groups));
        }
        // An agent that has an MCP tool can get a long result that is kept and paged: it reads the
        // rest with `read_output` (a folder agent keeps it in the run's state, up to 4 MiB).
        let has_mcp_tool = tools
            .iter()
            .any(|(n, _)| own_mcp.iter().any(|(m, _)| m == n));
        if has_mcp_tool && !tools.iter().any(|(n, _)| n == adam_llm_agent::READ_OUTPUT) {
            tools.push((
                adam_llm_agent::READ_OUTPUT.to_owned(),
                Arc::new(adam_llm_agent::ReadOutput),
            ));
        }
        let registered = tools.len();
        let (skills, preloaded) = add_skills(
            &origin,
            manifest,
            self.files,
            self.run_skills.filter(|_| parent.is_none()),
            &mut prompt,
            &mut tools,
        )?;
        // The tools `add_skills` appended, so a clash can say whose tool it hit.
        let skill_tools: Vec<String> = tools[registered..].iter().map(|(n, _)| n.clone()).collect();
        let mcp_names: Vec<String> = own_mcp.iter().map(|(n, _)| n.clone()).collect();
        // The session tools (ADR 0045) go to the root only: sessions are one level deep.
        let (session_agents, session_targets, round_agents) = add_subagent_tools(
            &origin,
            manifest,
            &Taken {
                skill_tools: &skill_tools,
                mcp_tools: &mcp_names,
            },
            self.remote,
            self.secrets,
            &mut tools,
            parent.is_none().then_some(self.session_limits),
        )?;
        let retry = retry_policies(&origin, manifest, &tools)?;
        let plugins = self.plugins_of(&origin, manifest, parent)?;
        // The root's modes: a name in a mode's `tools:` is one of the agent's tools (fail closed),
        // and every agent narrows by the same set (a child starts in its parent's mode).
        if parent.is_none() {
            self.root_modes = resolve_modes(&origin, manifest, &tools)?;
        }
        let index = self.nodes.len();
        self.nodes.push(Node {
            name: name.clone(),
            parent,
            file: manifest.path.clone(),
            description: manifest.frontmatter.description.clone(),
            model: manifest
                .frontmatter
                .model
                .clone()
                .unwrap_or(ModelRef::Inherit),
            reasoning_effort: manifest
                .frontmatter
                .reasoning_effort
                .clone()
                .or_else(|| parent.is_none().then(|| self.root_effort.clone()).flatten()),
            model_preset: manifest.frontmatter.model_preset.clone(),
            model_fallback: manifest.frontmatter.model_fallback.clone(),
            prompt,
            tools,
            limits: limits(manifest.frontmatter.limits.as_ref()),
            max_output_tokens: manifest
                .frontmatter
                .limits
                .as_ref()
                .and_then(|file| file.max_output_tokens),
            max_parallel: manifest
                .frontmatter
                .limits
                .as_ref()
                .and_then(|file| file.max_parallel)
                .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
            retry,
            plugins,
            skills,
            preloaded,
            session_agents,
            session_targets,
            round_agents,
            when: triggers,
            tools_loading: manifest.frontmatter.tools_loading,
            sources,
            modes: self.root_modes.clone(),
        });
        for sub in &manifest.subagents {
            match sub {
                Subagent::Local(child) => {
                    self.agent(child, format!("{name}/{}", child.name), Some(index))?;
                }
                Subagent::Remote(remote) => self.remotes.push(Remote {
                    parent: name.clone(),
                    agent: remote.clone(),
                }),
            }
        }
        Ok(())
    }
}

impl Walk<'_> {
    /// The MCP tools of one agent: what was connected or supplied for it, checked against its own
    /// `mcp.json` and the registered tools. Empty when it has no servers.
    fn mcp_tools(&self, manifest: &AgentManifest, name: &str) -> Result<McpTools, Error> {
        let declared = mcp::declared(manifest);
        let servers: Vec<String> = declared
            .map(|config| config.servers.keys().cloned().collect())
            .unwrap_or_default();
        let origin = Origin::new(name, mcp::file_of(manifest));
        let Some(given) = self.mcp.get(name) else {
            return if servers.is_empty() {
                Ok((Vec::new(), BTreeMap::new()))
            } else {
                Err(Error::McpNotConnected { origin, servers })
            };
        };
        if given.config.as_ref().is_some_and(|c| Some(c) != declared) {
            return Err(Error::McpChanged { origin });
        }
        let mut entries: Vec<(String, DynTool)> = Vec::new();
        let mut by_server: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for tool in &given.tools {
            let tool_name = tool.spec().name;
            // The one place a tool is tied to its server: the name check below and the grouping
            // the selector reads come from this find.
            let Some(server) = servers.iter().find(|server| {
                tool_name
                    .strip_prefix(server.as_str())
                    .is_some_and(|rest| rest.starts_with("__"))
            }) else {
                return Err(Error::McpForeignTool {
                    origin,
                    tool: tool_name,
                    servers,
                });
            };
            if entries.iter().any(|(n, _)| *n == tool_name) {
                return Err(Error::DuplicateTool { tool: tool_name });
            }
            if self.catalog.get(&tool_name).is_some() {
                return Err(Error::McpToolClash {
                    origin,
                    tool: tool_name,
                });
            }
            by_server
                .entry(server.clone())
                .or_default()
                .push(tool_name.clone());
            entries.push((tool_name, Arc::clone(tool)));
        }
        if entries.is_empty() && !servers.is_empty() {
            // Not refused: an empty set can be meant (supplied by hand, or servers that offer no
            // tool this agent can use). But it is never silent.
            tracing::warn!(
                agent = %name,
                servers = ?servers,
                "the agent's mcp.json lists servers, and it has no MCP tool"
            );
        }
        Ok((entries, by_server))
    }
}

/// Give the agent its skills: the catalog and the preloaded skills after the prompt, then the
/// tools `load_skill` and `read_skill_file` after the agent's own. Returns the names of the
/// selected and of the preloaded skills. Nothing is added for an agent without skills.
fn add_skills(
    origin: &Origin,
    manifest: &AgentManifest,
    files: &SkillFiles,
    run: Option<&RunSkillsHandle>,
    prompt: &mut String,
    tools: &mut Vec<(String, DynTool)>,
) -> Result<(Vec<String>, Vec<String>), Error> {
    // `skills: []` is off for everything: what the run finds too.
    let off = matches!(
        &manifest.frontmatter.skills,
        Some(SkillSelection::Named(names)) if names.is_empty()
    );
    let run = run.filter(|_| !off);
    let set = match skills::resolve(origin, manifest, files)? {
        Some(set) => set,
        None if run.is_some() => SkillSet::empty(),
        None => return Ok((Vec::new(), Vec::new())),
    };
    let set = Arc::new(set);
    let added = set.tools(run);
    for tool in &added {
        let name = tool.spec().name;
        if tools.iter().any(|(n, _)| *n == name) {
            return Err(Error::ReservedToolName {
                origin: origin.clone(),
                tool: name,
            });
        }
    }
    tools.extend(added.into_iter().map(|tool| (tool.spec().name, tool)));
    let section = set.prompt_section(run.is_some());
    // A prompt is never empty (the loader refuses an agent without instructions), and a set
    // with skills, or an agent given `RunSkills`, always has a section.
    prompt.push_str("\n\n");
    prompt.push_str(&section);
    Ok((set.names(), set.preloaded()))
}

/// Where some of an agent's tools came from, so that a name clash can say whose tool it hit.
struct Taken<'a> {
    /// The tools `add_skills` appended.
    skill_tools: &'a [String],
    /// The agent's own MCP tools (selected or not: only a selected one is in `tools`).
    mcp_tools: &'a [String],
}

/// Give the agent a tool for each of its subagents, local and remote, named after the subagent and
/// placed after the agent's own tools and its skills' tools, in the order of the manifest. The name
/// must not be one the agent's model already sees.
fn add_subagent_tools(
    origin: &Origin,
    manifest: &AgentManifest,
    taken: &Taken<'_>,
    remote: &RemoteSettings,
    secrets: &Secrets,
    tools: &mut Vec<(String, DynTool)>,
    session_limits: Option<SessionLimits>,
) -> Result<(Vec<String>, Vec<SessionTarget>, Rounds), Error> {
    let sessions = session_limits.is_some();
    // The subagent tools added so far, by name, with the file of the subagent that owns each.
    let mut subagents: Vec<(&str, &std::path::Path)> = Vec::new();
    #[cfg_attr(not(feature = "remote"), allow(unused_mut))]
    let mut rounds = Rounds::default();
    for sub in &manifest.subagents {
        let (name, file) = match sub {
            Subagent::Local(child) => (child.name.as_str(), child.path.as_path()),
            Subagent::Remote(agent) => (agent.name.as_str(), agent.path.as_path()),
        };
        let clash = if let Some((_, file)) = subagents.iter().find(|(n, _)| *n == name) {
            Some(ToolClash::Subagent {
                file: file.to_path_buf(),
            })
        } else if tools.iter().any(|(n, _)| n == name) {
            // Whose tool it is depends on what `add_skills` and the MCP servers added, not on
            // the name: without skills, a registered tool may be called `load_skill`.
            Some(if taken.skill_tools.iter().any(|t| t == name) {
                ToolClash::SkillTool
            } else if taken.mcp_tools.iter().any(|t| t == name) {
                ToolClash::McpTool {
                    server: name.split_once("__").map_or(name, |(s, _)| s).into(),
                }
            } else {
                ToolClash::Tool
            })
        } else {
            None
        };
        let sub_origin = Origin::new(format!("{}/{name}", origin.agent), file);
        if name.starts_with(adam_llm_agent::RESERVED_LOADER_PREFIX) {
            return Err(Error::ReservedLoaderName {
                origin: sub_origin,
                tool: name.to_owned(),
            });
        }
        if let Some(clash) = clash {
            return Err(Error::SubagentToolClash {
                origin: sub_origin,
                parent: origin.agent.clone(),
                tool: name.to_owned(),
                clash,
            });
        }
        let tool: DynTool = match sub {
            Subagent::Local(child) => Arc::new(
                SubagentTool::new(
                    child.name.clone(),
                    format!("{}/{}", origin.agent, child.name),
                    child.frontmatter.description.as_deref().unwrap_or_default(),
                )
                .with_run_limit(run_limit(child.frontmatter.limits.as_ref())),
            ),
            #[cfg(feature = "remote")]
            Subagent::Remote(agent) => {
                let tool = RemoteSubagentTool::bind(&sub_origin, agent, remote, secrets)?;
                // The rounds of a session on it are runs of an agent of its own, for the root.
                if sessions {
                    rounds.push(RemoteRoundAgent::new(
                        format!("{}/{}", origin.agent, agent.name),
                        Arc::clone(tool.endpoint()),
                        remote.max_wait,
                    ));
                }
                Arc::new(tool)
            }
            // Refused before any secret is read: this build has no A2A client to call it with.
            #[cfg(not(feature = "remote"))]
            Subagent::Remote(_) => {
                let _ = secrets;
                return Err(Error::RemoteNotEnabled { origin: sub_origin });
            }
        };
        subagents.push((name, file));
        tools.push((name.to_owned(), tool));
    }
    if !sessions || manifest.subagents.is_empty() {
        return Ok((Vec::new(), Vec::new(), Rounds::default()));
    }
    // The tools that run a subagent in the background: after the one tool per subagent, and named
    // so that a clash with a tool or a subagent is found now.
    let targets = session_targets(manifest, &origin.agent, remote);
    for name in SESSION_TOOLS {
        if tools.iter().any(|(n, _)| n == name) {
            let clash = manifest
                .subagents
                .iter()
                .find_map(|sub| match sub {
                    Subagent::Local(child) if child.name == name => Some(child.path.clone()),
                    Subagent::Remote(agent) if agent.name == name => Some(agent.path.clone()),
                    _ => None,
                })
                .map_or(ToolClash::Tool, |file| ToolClash::Subagent { file });
            return Err(Error::SubagentToolClash {
                origin: origin.clone(),
                parent: origin.agent.clone(),
                tool: name.to_owned(),
                clash,
            });
        }
    }
    // The five tools themselves are made at `BoundDef::build`, where the deployment's
    // `MAX_PARALLEL_TOOL_CALLS` and the model are known: each subagent's bounds need both (ADR
    // 0054 slice 1 part 2). Their names and order are fixed here, so a clash is still found
    // before a model is needed, and `AgentInfo::tools` names them.
    let mut names: Vec<String> = targets.iter().map(|t| t.agent.clone()).collect();
    names.sort();
    Ok((names, targets, rounds))
}

/// The subagents of the root a session can be started on, in the order of the manifest: the one
/// list the session tools, the card and the A2A front are made from, so they cannot disagree. A
/// local subagent's rounds are limited by its `limits.max_minutes`, a remote one's by the
/// deployment's remote wait.
fn session_targets(
    manifest: &AgentManifest,
    root: &str,
    remote: &RemoteSettings,
) -> Vec<SessionTarget> {
    manifest
        .subagents
        .iter()
        .map(|sub| match sub {
            Subagent::Local(child) => SessionTarget {
                name: child.name.clone(),
                agent: format!("{root}/{}", child.name),
                description: child.frontmatter.description.clone().unwrap_or_default(),
                within: run_limit(child.frontmatter.limits.as_ref()),
                remote: false,
            },
            Subagent::Remote(agent) => SessionTarget {
                name: agent.name.clone(),
                agent: format!("{root}/{}", agent.name),
                description: agent.description.clone(),
                within: Some(remote.max_wait),
                remote: true,
            },
        })
        .collect()
}

/// The deployment's session limits lowered by the root's file: each value of the file that is
/// larger than the deployment's is the deployment's, and with `warn_for` (the agent's name) it
/// says so once. `max_working` stays at most `max_open`.
fn lowered_by_file(
    deployment: SessionLimits,
    file: Option<&FileLimits>,
    warn_for: Option<&str>,
) -> SessionLimits {
    let Some(file) = file else {
        return deployment;
    };
    if let Some(agent) = warn_for {
        for (key, setting, asked, allowed) in [
            (
                "max_open_sessions",
                "SESSION_MAX_OPEN",
                file.max_open_sessions,
                deployment.max_open(),
            ),
            (
                "max_working_sessions",
                "SESSION_MAX_WORKING",
                file.max_working_sessions,
                deployment.max_working(),
            ),
            (
                "max_session_rounds",
                "SESSION_MAX_ROUNDS",
                file.max_session_rounds,
                deployment.max_rounds(),
            ),
        ] {
            if let Some(asked) = asked.filter(|asked| *asked > allowed) {
                tracing::warn!(
                    agent,
                    asked,
                    allowed,
                    "`limits.{key}` is more than the deployment allows ({setting}): \
                     the sessions run under {allowed}"
                );
            }
        }
    }
    deployment.lowered_by(
        file.max_open_sessions,
        file.max_working_sessions,
        file.max_session_rounds,
    )
}

/// The time limit of a subagent's run: `limits.max_minutes` of its file, 30 minutes when it says
/// nothing, none for `0`. The loader refuses more than a day; a hand-made value is clamped.
fn run_limit(file: Option<&FileLimits>) -> Option<Duration> {
    let minutes = file
        .and_then(|limits| limits.max_minutes)
        .unwrap_or(DEFAULT_SUBAGENT_MINUTES);
    (minutes > 0)
        .then(|| Duration::from_secs(u64::from(minutes.min(adam_agent_fs::MAX_MINUTES_LIMIT)) * 60))
}

/// The loop's limits: what the frontmatter sets, the loop's default for the rest.
fn limits(file: Option<&FileLimits>) -> Limits {
    let default = Limits::default();
    let Some(file) = file else {
        return default;
    };
    Limits {
        max_turns: file.max_turns.unwrap_or(default.max_turns),
        max_tool_calls: file.max_tool_calls.unwrap_or(default.max_tool_calls),
        max_output_tokens: file.max_output_tokens.unwrap_or(default.max_output_tokens),
        max_history_tokens: file
            .max_history_tokens
            .unwrap_or(default.max_history_tokens),
    }
}

/// The tools an agent gets, by name, in the order its `tools:` lists them.
/// The root's modes, resolved against the tools the agent ends up with (ADR 0039, decision 3): a
/// pattern is expanded (one that matches nothing is an error), a plain name the agent does not have
/// is dropped with a warning (a deployment's environment decides some tools, and a folder's `tools:`
/// may narrow them: a mode only narrows what exists, so it can never widen anything by it), and
/// exactly one mode is the default (the loader checked it). `None` for a folder without `modes/`.
fn resolve_modes(
    origin: &Origin,
    manifest: &AgentManifest,
    tools: &[(String, DynTool)],
) -> Result<Option<std::sync::Arc<crate::modes::Modes>>, Error> {
    use crate::modes::{Modes, ResolvedMode};

    if manifest.modes.is_empty() {
        return Ok(None);
    }
    let available: Vec<String> = tools.iter().map(|(name, _)| name.clone()).collect();
    let mut resolved = Vec::with_capacity(manifest.modes.len());
    for mode in &manifest.modes {
        let at = origin.in_file(mode.path.clone());
        let allowed = match &mode.tools {
            None => None,
            Some(ToolList::All) => Some(available.clone()),
            Some(ToolList::Named(names)) => {
                let mut chosen: Vec<String> = Vec::new();
                for wanted in names {
                    if wanted.contains('*') {
                        let mut any = false;
                        for name in available.iter().filter(|name| glob(wanted, name)) {
                            any = true;
                            if !chosen.contains(name) {
                                chosen.push(name.clone());
                            }
                        }
                        if !any {
                            return Err(Error::NoToolMatches {
                                origin: at.clone(),
                                pattern: wanted.clone(),
                                available: available.clone(),
                            });
                        }
                    } else if available.contains(wanted) {
                        if !chosen.contains(wanted) {
                            chosen.push(wanted.clone());
                        }
                    } else {
                        // A tool the deployment does not offer (one a setting adds), or one the
                        // agent's own `tools:` left out: the mode cannot offer it either, and a
                        // warning says the list is not what its author thought.
                        tracing::warn!(
                            agent = %at.agent,
                            mode = %mode.name,
                            tool = %wanted,
                            suggestion = closest(wanted, available.iter().map(String::as_str)),
                            "a mode's `tools` names a tool the agent does not have; it is left out"
                        );
                    }
                }
                Some(chosen)
            }
        };
        resolved.push(ResolvedMode {
            name: mode.name.clone(),
            title: mode.title.clone(),
            description: mode.description.clone(),
            tools: allowed,
            prompt: mode.prompt.clone(),
        });
    }
    let default = manifest
        .modes
        .iter()
        .find(|mode| mode.default)
        .map_or_else(|| manifest.modes[0].name.clone(), |mode| mode.name.clone());
    Ok(Modes::new(resolved, default).map(std::sync::Arc::new))
}

fn resolve_tools(
    origin: &Origin,
    list: Option<&ToolList>,
    is_root: bool,
    catalog: &Catalog,
) -> Result<Vec<(String, DynTool)>, Error> {
    let names = match list {
        // A root agent with no `tools:` gets everything registered; a subagent gets nothing.
        None if is_root => return Ok(catalog.entries.clone()),
        None => return Ok(Vec::new()),
        Some(ToolList::All) => return Ok(catalog.entries.clone()),
        Some(ToolList::Named(names)) => names,
    };
    let mut chosen: Vec<(String, DynTool)> = Vec::new();
    let mut add = |entry: &(String, DynTool)| {
        if !chosen.iter().any(|(n, _)| *n == entry.0) {
            chosen.push(entry.clone());
        }
    };
    for wanted in names {
        if wanted.contains('*') {
            let mut any = false;
            for entry in catalog.entries.iter().filter(|(n, _)| glob(wanted, n)) {
                any = true;
                add(entry);
            }
            if !any {
                return Err(Error::NoToolMatches {
                    origin: origin.clone(),
                    pattern: wanted.clone(),
                    available: catalog.names(),
                });
            }
        } else if let Some(entry) = catalog.get(wanted) {
            add(entry);
        } else {
            let available = catalog.names();
            return Err(Error::UnknownTool {
                origin: origin.clone(),
                tool: wanted.clone(),
                suggestion: closest(wanted, available.iter().map(String::as_str))
                    .map(str::to_owned),
                available,
            });
        }
    }
    Ok(chosen)
}

/// The policies of `retry:` (ADR 0061, decision 6), each for one of the agent's `tools`: its own,
/// the MCP servers' (connected before binding, so known here) and the skill and subagent tools.
fn retry_policies(
    origin: &Origin,
    manifest: &AgentManifest,
    tools: &[(String, DynTool)],
) -> Result<Vec<(String, adam_agent_fs::RetryDef, adam_runtime::RetryPolicy)>, Error> {
    let mut policies = Vec::new();
    let declared: Vec<&String> = mcp::declared(manifest)
        .map(|config| config.servers.keys().collect())
        .unwrap_or_default();
    for (tool, def) in &manifest.frontmatter.retry {
        // A tool of a declared server that gave no tools (an `optional` one that is down, an empty
        // set given in code): the server is already warned about, and its policy waits for it.
        if let Some((server, _)) = tool.split_once("__")
            && declared.iter().any(|d| d.as_str() == server)
            && !tools.iter().any(|(name, _)| {
                name.strip_prefix(server)
                    .is_some_and(|r| r.starts_with("__"))
            })
        {
            tracing::warn!(
                agent = %origin.agent,
                tool = %tool,
                server = %server,
                "the MCP server of a `retry` entry gave no tools; the entry is not used"
            );
            continue;
        }
        if !tools.iter().any(|(name, _)| name == tool) {
            let available: Vec<String> = tools.iter().map(|(name, _)| name.clone()).collect();
            return Err(Error::UnknownRetryTool {
                origin: origin.clone(),
                tool: tool.clone(),
                suggestion: closest(tool, available.iter().map(String::as_str)).map(str::to_owned),
                available,
            });
        }
        let invalid = |reason: &str| Error::InvalidRetry {
            origin: origin.clone(),
            tool: tool.clone(),
            reason: reason.to_owned(),
        };
        let attempts = def
            .attempts
            .filter(|n| (1..=adam_agent_fs::MAX_RETRY_ATTEMPTS).contains(n))
            .ok_or_else(|| invalid("`attempts` is from 1 to 20"))?;
        let (Some(initial), Some(bound)) = (def.initial_backoff(), def.bound()) else {
            return Err(invalid("a backoff is not a duration"));
        };
        if initial.max(bound) > adam_agent_fs::MAX_RETRY_BACKOFF {
            return Err(invalid("a backoff is over 1h"));
        }
        if def.max_backoff.is_some() && bound < initial {
            return Err(invalid("`max_backoff` is shorter than `backoff`"));
        }
        let mut policy = adam_runtime::RetryPolicy::new(attempts).with_backoff(initial, bound);
        if def.multiplier.is_some() {
            let factor = def
                .factor()
                .filter(|m| *m > 0.0)
                .ok_or_else(|| invalid("`multiplier` is not a positive number"))?;
            policy = policy.with_multiplier(factor);
        }
        if let Some(on) = &def.on {
            let classes = adam_runtime::RetryClasses::from_names(on.iter().map(String::as_str))
                .map_err(|name| invalid(&format!("`{name}` is not a retryable class")))?;
            if classes == adam_runtime::RetryClasses::NONE {
                return Err(invalid("`on` names no class"));
            }
            policy = policy.with_retry_on(classes);
        }
        policies.push((tool.clone(), def.clone(), policy));
    }
    Ok(policies)
}

/// Whether `name` matches `pattern`, where `*` stands for any run of characters.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    // `split` yields at least one piece; without a `*` there is exactly one.
    let (first, rest) = (parts[0], &parts[1..]);
    let Some((last, middles)) = rest.split_last() else {
        return pattern == name;
    };
    let Some(mut tail) = name.strip_prefix(first) else {
        return false;
    };
    for middle in middles {
        let Some(at) = tail.find(middle) else {
            return false;
        };
        tail = &tail[at + middle.len()..];
    }
    tail.ends_with(last)
}

/// Render the prompt of one agent: `instructions.md` and each `instructions/*.md`, then joined
/// the way [`Instructions::prompt`] joins them.
fn render_prompt(
    origin: &Origin,
    manifest: &AgentManifest,
    supplied: Option<&BTreeMap<String, String>>,
) -> Result<String, Error> {
    let declared = &manifest.frontmatter.vars;
    let no_values = BTreeMap::new();
    let supplied = supplied.unwrap_or(&no_values);

    // The files of the prompt. A part sits in `instructions/` next to the instructions file.
    let parts_dir = manifest.path.with_file_name("instructions");
    let files: Vec<(PathBuf, &str)> =
        std::iter::once((manifest.path.clone(), manifest.instructions.body.as_str()))
            .chain(
                manifest
                    .instructions
                    .parts
                    .iter()
                    .map(|p| (parts_dir.join(&p.file), p.body.as_str())),
            )
            .collect();

    let mut parsed: Vec<(PathBuf, Vec<Piece>)> = Vec::with_capacity(files.len());
    for (file, text) in files {
        let pieces = template::parse(text).map_err(|syntax| Error::Template {
            origin: origin.in_file(&file),
            line: syntax.line,
            problem: syntax.problem,
        })?;
        parsed.push((file, pieces));
    }

    let declared_names = || declared.keys().map(String::as_str);
    let mut used: BTreeSet<&str> = BTreeSet::new();
    for (file, pieces) in &parsed {
        for piece in pieces {
            let Piece::Var { name, line } = piece else {
                continue;
            };
            if !declared.contains_key(name) {
                return Err(Error::UnknownVar {
                    origin: origin.in_file(file),
                    line: *line,
                    var: name.clone(),
                    suggestion: closest(name, declared_names()).map(str::to_owned),
                    declared: declared_names().map(str::to_owned).collect(),
                });
            }
            used.insert(name);
        }
    }
    if let Some(name) = supplied.keys().find(|n| !declared.contains_key(*n)) {
        return Err(Error::UnknownVarValue {
            origin: origin.clone(),
            var: name.clone(),
            suggestion: closest(name, declared_names()).map(str::to_owned),
            declared: declared_names().map(str::to_owned).collect(),
        });
    }
    if let Some(name) = declared_names().find(|n| !used.contains(n)) {
        return Err(Error::UnusedVar {
            origin: origin.clone(),
            var: name.to_owned(),
        });
    }
    let mut values = declared.clone();
    values.extend(supplied.clone());
    if let Some(name) = used
        .iter()
        .find(|n| values.get(**n).is_none_or(String::is_empty))
        .copied()
    {
        return Err(Error::UnsetVar {
            origin: origin.clone(),
            var: (*name).to_owned(),
        });
    }

    let mut rendered = parsed
        .iter()
        .map(|(_, pieces)| template::render(pieces, &values));
    let body = rendered.next().unwrap_or_default();
    let parts = manifest
        .instructions
        .parts
        .iter()
        .zip(rendered)
        .map(|(part, body)| InstructionPart {
            file: part.file.clone(),
            body,
        })
        .collect();
    Ok(Instructions { body, parts }.prompt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_matches_any_run() {
        assert!(glob("linear__*", "linear__list_issues"));
        assert!(glob("linear__*", "linear__"));
        assert!(!glob("linear__*", "github__list"));
        assert!(!glob("linear__*", "linear_"));
        assert!(glob("*_issues", "linear__list_issues"));
        assert!(glob("*", "anything"));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(glob("a*b*c", "abc"));
        assert!(!glob("a*b*c", "acb"));
        assert!(!glob("a*b*c", "aXbY"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
        // The pieces of a pattern cannot overlap: `ab*ba` needs four characters.
        assert!(!glob("ab*ba", "aba"));
    }

    #[test]
    fn a_subagents_run_limit_is_30_minutes_unless_its_file_says_otherwise() {
        let minutes = |m: Option<u32>| {
            let limits = FileLimits {
                max_minutes: m,
                ..FileLimits::default()
            };
            run_limit(Some(&limits))
        };
        let min = |n: u64| Some(Duration::from_secs(n * 60));
        assert_eq!(run_limit(None), min(30));
        assert_eq!(minutes(None), min(30));
        assert_eq!(minutes(Some(5)), min(5));
        assert_eq!(minutes(Some(0)), None, "0 is no limit");
        assert_eq!(minutes(Some(1_440)), min(1_440));
        assert_eq!(minutes(Some(99_999)), min(1_440), "clamped to a day");
    }

    #[test]
    fn limits_take_the_file_over_the_default() {
        assert_eq!(limits(None), Limits::default());
        let set = FileLimits {
            max_turns: Some(7),
            max_history_tokens: Some(0),
            ..FileLimits::default()
        };
        let got = limits(Some(&set));
        assert_eq!(got.max_turns, 7);
        assert_eq!(got.max_history_tokens, 0);
        assert_eq!(got.max_tool_calls, Limits::default().max_tool_calls);
        assert_eq!(got.max_output_tokens, Limits::default().max_output_tokens);
    }
}
