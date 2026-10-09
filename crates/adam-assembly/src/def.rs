//! [`AgentDef`]: a manifest plus the values the code supplies, and [`AgentDef::bind`], which
//! checks it against the registered tools and renders the prompts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use adam_agent_fs::{
    AgentManifest, Diagnostic, EmbeddedAgent, InstructionPart, Instructions, Limits as FileLimits,
    ManifestSource, McpConfig, ModelRef, Strictness, Subagent, ToolList,
};
use adam_llm_agent::{DynTool, Limits, ToolSet};
use secrecy::SecretString;

use crate::assembly::BoundDef;
use crate::error::{Error, Origin, ToolClash};
use crate::mcp::{self, McpBinding};
use crate::remote::{RemoteSettings, RemoteSubagentTool};
use crate::skills::{self, SkillFiles};
use crate::subagent::SubagentTool;
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
    /// The MCP tools of each agent: connected from its `mcp.json`, or supplied by hand.
    mcp: McpBinding,
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
            mcp: McpBinding::default(),
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
    /// [`connect_mcp`](Self::connect_mcp) connects both and `bind` checks the sum. The servers of
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
    /// wins over the process environment, so a composition root that fetches its secrets from a
    /// vault (or a test) does not have to put them in the environment. The value is held as a
    /// secret: it is not shown by `Debug`, and no error carries it.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.remote.env).insert(name.into(), SecretString::from(value.into()));
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

    /// How long a parent waits for the task of a remote subagent before the call is answered with
    /// an error result (default one hour; the remote task is left where it is). A zero is raised to
    /// one millisecond.
    #[must_use]
    pub fn remote_timeout(mut self, max_wait: Duration) -> Self {
        self.remote.max_wait = max_wait.max(Duration::from_millis(1));
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

    /// Connect to the MCP servers of the root agent's `mcp.json` and of each local subagent's, and
    /// keep their tools for [`bind`](Self::bind). Only with the feature `mcp`.
    ///
    /// Each agent gets its own connections: two directories that both name a server `linear`
    /// connect twice, with their own headers. `${VAR}` in the files is expanded from
    /// [`env`](Self::env) first and then from the process environment, so a token given in code
    /// reaches a server the same way it reaches a remote subagent. Everything that can be wrong
    /// (an unset variable, `type: sse`, a local process the policy does not allow, a server that
    /// is down, an allow-list that names a tool the server lacks) is an error here, at startup:
    /// see `adam_mcp::McpServers::connect`.
    ///
    /// The connections live as long as the tools do: as long as the [`Assembly`](crate::Assembly)
    /// built from this definition. Call it after the [`env`](Self::env) calls it should see, and
    /// once: it connects again each time.
    ///
    /// # Errors
    ///
    /// [`Error::Mcp`], naming the agent and its `mcp.json`, for the first server that fails.
    #[cfg(feature = "mcp")]
    pub async fn connect_mcp(mut self, policy: &adam_mcp::McpPolicy) -> Result<Self, Error> {
        let env = adam_mcp::Env::from_values(Arc::clone(&self.remote.env));
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
            connected.push((name, config.clone(), servers.tools()));
        }
        for (name, config, tools) in connected {
            self.mcp.set(name, Some(config), tools);
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
    /// * a value supplied for an agent the definition does not contain.
    ///
    /// The root agent gets every registered tool when it has no `tools:`; a subagent gets none
    /// (decision D3 of `docs/reference/agent-files.md`), whatever its parent has. Each agent then gets a
    /// tool per local subagent, named after it ([`SubagentTool`]), after its own tools and its
    /// skills' tools. Two more things are refused here:
    ///
    /// * a subagent tool whose name is already a tool of the parent, or another subagent's
    ///   ([`Error::SubagentToolClash`]);
    /// * a subagent with a tool that asks the user ([`Error::SubagentAsksUser`]): nobody would
    ///   answer it.
    ///
    /// A remote subagent (`a2a:`) gets a tool of the same shape and the same name checks, and is
    /// checked here too: its URL must be https (or local, unless
    /// [`allow_insecure_remotes`](Self::allow_insecure_remotes)) and carry no credentials
    /// ([`Error::RemoteUrl`]), and the variable of its `auth: bearer:VAR` must hold a token
    /// ([`Error::RemoteAuth`]). The token is read now and kept in memory, and the network is not
    /// touched until the first call.
    ///
    /// An agent's MCP tools (`mcp.json`) come from its own directory only, and are part of the
    /// catalog `tools:` selects from, next to the registered tools (`tools: ['linear__*']`
    /// selects a server's tools; a subagent that lists none gets none). An agent whose `mcp.json`
    /// lists servers must have been given tools ([`connect_mcp`](Self::connect_mcp) or
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
            mcp: &self.mcp,
            nodes: Vec::new(),
            remotes: Vec::new(),
        };
        walk.agent(&self.manifest, self.manifest.name.clone(), None)?;
        let Walk { nodes, remotes, .. } = walk;

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
        Ok(BoundDef::new(self.manifest, nodes, remotes))
    }
}

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
    pub(crate) prompt: String,
    pub(crate) tools: Vec<(String, DynTool)>,
    pub(crate) limits: Limits,
    /// The skills the agent may use, in order.
    pub(crate) skills: Vec<String>,
    /// The skills whose body is in the prompt.
    pub(crate) preloaded: Vec<String>,
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
    mcp: &'a McpBinding,
    nodes: Vec<Node>,
    remotes: Vec<Remote>,
}

impl Walk<'_> {
    fn agent(
        &mut self,
        manifest: &AgentManifest,
        name: String,
        parent: Option<usize>,
    ) -> Result<(), Error> {
        let origin = Origin::new(name.clone(), manifest.path.clone());
        // The agent's own MCP tools, checked, then chosen from with the registered ones.
        let own_mcp = self.mcp_tools(manifest, &name)?;
        let catalog = self.catalog.with(&own_mcp);
        let mut tools = resolve_tools(
            &origin,
            manifest.frontmatter.tools.as_ref(),
            parent.is_none(),
            &catalog,
        )?;
        if parent.is_some() {
            refuse_asking_tools(&origin, &tools)?;
        }
        let mut prompt = render_prompt(&origin, manifest, self.values.get(&name))?;
        let registered = tools.len();
        let (skills, preloaded) =
            add_skills(&origin, manifest, self.files, &mut prompt, &mut tools)?;
        // The tools `add_skills` appended, so a clash can say whose tool it hit.
        let skill_tools: Vec<String> = tools[registered..].iter().map(|(n, _)| n.clone()).collect();
        let mcp_names: Vec<String> = own_mcp.iter().map(|(n, _)| n.clone()).collect();
        add_subagent_tools(
            &origin,
            manifest,
            &Taken {
                skill_tools: &skill_tools,
                mcp_tools: &mcp_names,
            },
            self.remote,
            &mut tools,
        )?;
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
            prompt,
            tools,
            limits: limits(manifest.frontmatter.limits.as_ref()),
            skills,
            preloaded,
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
    fn mcp_tools(
        &self,
        manifest: &AgentManifest,
        name: &str,
    ) -> Result<Vec<(String, DynTool)>, Error> {
        let declared = mcp::declared(manifest);
        let servers: Vec<String> = declared
            .map(|config| config.servers.keys().cloned().collect())
            .unwrap_or_default();
        let origin = Origin::new(name, mcp::file_of(manifest));
        let Some(given) = self.mcp.get(name) else {
            return if servers.is_empty() {
                Ok(Vec::new())
            } else {
                Err(Error::McpNotConnected { origin, servers })
            };
        };
        if given.config.as_ref().is_some_and(|c| Some(c) != declared) {
            return Err(Error::McpChanged { origin });
        }
        let mut entries: Vec<(String, DynTool)> = Vec::new();
        for tool in &given.tools {
            let tool_name = tool.spec().name;
            let belongs = servers.iter().any(|server| {
                tool_name
                    .strip_prefix(server.as_str())
                    .is_some_and(|rest| rest.starts_with("__"))
            });
            if !belongs {
                return Err(Error::McpForeignTool {
                    origin,
                    tool: tool_name,
                    servers,
                });
            }
            if entries.iter().any(|(n, _)| *n == tool_name) {
                return Err(Error::DuplicateTool { tool: tool_name });
            }
            if self.catalog.get(&tool_name).is_some() {
                return Err(Error::McpToolClash {
                    origin,
                    tool: tool_name,
                });
            }
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
        Ok(entries)
    }
}

/// Give the agent its skills: the catalog and the preloaded skills after the prompt, then the
/// tools `load_skill` and `read_skill_file` after the agent's own. Returns the names of the
/// selected and of the preloaded skills. Nothing is added for an agent without skills.
fn add_skills(
    origin: &Origin,
    manifest: &AgentManifest,
    files: &SkillFiles,
    prompt: &mut String,
    tools: &mut Vec<(String, DynTool)>,
) -> Result<(Vec<String>, Vec<String>), Error> {
    let Some(set) = skills::resolve(origin, manifest, files)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let set = Arc::new(set);
    let added = set.tools();
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
    let section = set.prompt_section();
    // A prompt is never empty (the loader refuses an agent without instructions), and a set
    // with skills always has a section.
    prompt.push_str("\n\n");
    prompt.push_str(&section);
    Ok((set.names(), set.preloaded()))
}

/// A subagent runs as a child of another run: there is nobody to answer a question, and a run
/// that asks parks until someone does. Refuse the tools that say they ask.
fn refuse_asking_tools(origin: &Origin, tools: &[(String, DynTool)]) -> Result<(), Error> {
    match tools.iter().find(|(_, tool)| tool.asks_user()) {
        Some((name, _)) => Err(Error::SubagentAsksUser {
            origin: origin.clone(),
            tool: name.clone(),
        }),
        None => Ok(()),
    }
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
    tools: &mut Vec<(String, DynTool)>,
) -> Result<(), Error> {
    // The subagent tools added so far, by name, with the file of the subagent that owns each.
    let mut subagents: Vec<(&str, &std::path::Path)> = Vec::new();
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
        if let Some(clash) = clash {
            return Err(Error::SubagentToolClash {
                origin: sub_origin,
                parent: origin.agent.clone(),
                tool: name.to_owned(),
                clash,
            });
        }
        let tool: DynTool = match sub {
            Subagent::Local(child) => Arc::new(SubagentTool::new(
                child.name.clone(),
                format!("{}/{}", origin.agent, child.name),
                child.frontmatter.description.as_deref().unwrap_or_default(),
            )),
            Subagent::Remote(agent) => {
                Arc::new(RemoteSubagentTool::bind(&sub_origin, agent, remote)?)
            }
        };
        subagents.push((name, file));
        tools.push((name.to_owned(), tool));
    }
    Ok(())
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
