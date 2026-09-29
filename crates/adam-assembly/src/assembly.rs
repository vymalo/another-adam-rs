//! [`BoundDef`] (tools bound, prompts rendered) and [`Assembly`] (the `LlmAgent`s).

use std::path::PathBuf;
use std::sync::Arc;

use adam_agent_fs::{AgentManifest, ModelRef, RemoteAgent};
use adam_llm_agent::{Limits, LlmAgent, LlmAgentBuilder};
use adam_model::DynModel;
use adam_runtime::RuntimeBuilder;

use crate::def::{Node, Remote};
use crate::error::{AliasProblem, Error, Origin};
use crate::suggest::closest;

/// Hands one shared value to an agent under construction.
type ApplyState = Arc<dyn Fn(LlmAgentBuilder) -> LlmAgentBuilder + Send + Sync>;

/// An agent definition whose tools are bound and whose prompts are rendered, waiting for the
/// state its tools need and for a model. Made by [`AgentDef::bind`](crate::AgentDef::bind).
///
/// ```text
/// AgentDef::from_manifest(&AGENT)?.bind(tools![..])?.state(env).model(model, "alias")?
/// ```
pub struct BoundDef {
    manifest: AgentManifest,
    nodes: Vec<Node>,
    remotes: Vec<Remote>,
    state: Vec<ApplyState>,
    aliases: Option<Vec<String>>,
}

impl std::fmt::Debug for BoundDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundDef")
            .field(
                "agents",
                &self
                    .nodes
                    .iter()
                    .map(|n| n.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("state", &self.state.len())
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

impl BoundDef {
    pub(crate) fn new(manifest: AgentManifest, nodes: Vec<Node>, remotes: Vec<Remote>) -> Self {
        Self {
            manifest,
            nodes,
            remotes,
            state: Vec::new(),
            aliases: None,
        }
    }

    /// Share `value` with the tools of every agent, as
    /// [`LlmAgentBuilder::state`] does: tools read it with `ctx.state::<T>()`, and
    /// [`model`](Self::model) fails when a tool declares a state that nobody gave.
    #[must_use]
    pub fn state<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        self.state
            .push(Arc::new(move |builder| builder.state(Arc::clone(&value))));
        self
    }

    /// The model aliases this deployment serves. With this set, [`model`](Self::model) refuses
    /// an agent whose `model:` (or the default alias) is not one of them, and suggests the
    /// closest. Without it any alias is accepted and a wrong one fails on the first model call.
    #[must_use]
    pub fn model_aliases<I, S>(mut self, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.aliases = Some(aliases.into_iter().map(Into::into).collect());
        self
    }

    /// Give the agents their model and build them.
    ///
    /// `model` is the one client every agent talks to (the composition root's endpoint), and
    /// `alias` is the gateway alias of an agent that names none. An agent uses, in this order,
    /// the alias its `model:` names, the alias of its parent when it says `inherit` (a
    /// subagent's default), and `alias` (the root's).
    ///
    /// # Errors
    ///
    /// * [`Error::ModelAlias`] for an alias that is empty, has whitespace, or is not one of
    ///   [`model_aliases`](Self::model_aliases);
    /// * [`Error::Build`] when a tool needs state that [`state`](Self::state) did not give.
    pub fn model(self, model: DynModel, alias: impl Into<String>) -> Result<Assembly, Error> {
        let default_alias = alias.into();
        let mut resolved: Vec<String> = Vec::with_capacity(self.nodes.len());
        let mut agents = Vec::with_capacity(self.nodes.len());
        let mut info = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let origin = Origin::new(node.name.clone(), node.file.clone());
            let alias = match (&node.model, node.parent) {
                (ModelRef::Alias(alias), _) => alias.clone(),
                (ModelRef::Inherit, Some(parent)) => resolved
                    .get(parent)
                    .cloned()
                    .unwrap_or_else(|| default_alias.clone()),
                (ModelRef::Inherit, None) => default_alias.clone(),
            };
            self.check_alias(&origin, &alias)?;
            agents.push(self.build(node, &origin, &model, &alias)?);
            info.push(AgentInfo {
                name: node.name.clone(),
                parent: node
                    .parent
                    .and_then(|p| self.nodes.get(p))
                    .map(|p| p.name.clone()),
                description: node.description.clone(),
                file: node.file.clone(),
                model_alias: alias.clone(),
                prompt: node.prompt.clone(),
                tools: node.tools.iter().map(|(name, _)| name.clone()).collect(),
                skills: node.skills.clone(),
                preloaded: node.preloaded.clone(),
                limits: node.limits,
            });
            resolved.push(alias);
        }
        let remotes = self
            .remotes
            .into_iter()
            .map(|r| RemoteInfo {
                parent: r.parent,
                agent: r.agent,
            })
            .collect();
        Ok(Assembly {
            manifest: self.manifest,
            agents,
            info,
            remotes,
        })
    }

    fn check_alias(&self, origin: &Origin, alias: &str) -> Result<(), Error> {
        let problem = if alias.trim().is_empty() {
            Some(AliasProblem::Empty)
        } else if alias.chars().any(|c| c.is_whitespace() || c.is_control()) {
            Some(AliasProblem::NotAToken)
        } else {
            self.aliases
                .as_ref()
                .filter(|allowed| !allowed.iter().any(|a| a == alias))
                .map(|allowed| AliasProblem::NotAllowed {
                    suggestion: closest(alias, allowed.iter().map(String::as_str))
                        .map(str::to_owned),
                    allowed: allowed.clone(),
                })
        };
        match problem {
            Some(problem) => Err(Error::ModelAlias {
                origin: origin.clone(),
                alias: alias.to_owned(),
                problem,
            }),
            None => Ok(()),
        }
    }

    /// One `LlmAgent` from one bound node.
    ///
    /// This is the seam the next slices extend. `bind` has already made the prompt and the tool
    /// list final: the catalog after the prompt (slice S7), and the tools in the order the model
    /// sees them: the agent's own, `load_skill` and `read_skill_file`, then one
    /// [`SubagentTool`](crate::SubagentTool) per local subagent (slice S9). They are added at
    /// bind, not here, so that a name clash is found before a model is needed, and so that
    /// [`AgentInfo::tools`] and the agent cannot disagree. This function only hands them over.
    /// The remote subagents (`remotes`, slice S9b) join the same list in `bind`, where their names
    /// meet the local ones; slice S11 adds the tools of `mcp.json` the same way.
    fn build(
        &self,
        node: &Node,
        origin: &Origin,
        model: &DynModel,
        alias: &str,
    ) -> Result<LlmAgent, Error> {
        let mut builder = LlmAgent::builder(node.name.clone(), Arc::clone(model), alias)
            .instructions(node.prompt.clone())
            .limits(node.limits);
        for (_, tool) in &node.tools {
            builder = builder.dyn_tool(Arc::clone(tool));
        }
        for apply in &self.state {
            builder = apply(builder);
        }
        builder.try_build().map_err(|source| Error::Build {
            origin: origin.clone(),
            source,
        })
    }
}

/// What one bound agent is, without its `LlmAgent` (which does not expose it). Two definitions
/// that bind to equal `AgentInfo`s make equal agents; that is how the tests compare an embedded
/// agent with the same files read from a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// The name the agent is registered under: the root's name, then `<parent>/<name>`.
    pub name: String,
    /// The registration name of the parent; `None` for the root.
    pub parent: Option<String>,
    /// The frontmatter `description`: what a parent reads as the subagent's tool description.
    pub description: Option<String>,
    /// The agent's file, relative to the source root.
    pub file: PathBuf,
    /// The gateway alias its model calls use.
    pub model_alias: String,
    /// The system prompt the model sees: the instructions with the vars substituted, then the
    /// skills catalog and the preloaded skills.
    pub prompt: String,
    /// The names of its tools, in the order the model is shown them: its own, then `load_skill`
    /// and `read_skill_file` when it has skills to load and files to read, then one per local
    /// subagent (the subagent's name: the entries of `info()` whose `parent` is this agent).
    pub tools: Vec<String>,
    /// The skills it may use, in the order of its `skills:` (by name for `all`).
    pub skills: Vec<String>,
    /// The subset of [`skills`](Self::skills) whose body is in the prompt (`preload_skills:`).
    pub preloaded: Vec<String>,
    /// The loop's limits.
    pub limits: Limits,
}

/// A remote (A2A) subagent found in the files. It is data here: the tool that calls it is a
/// later slice (S9b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    /// The registration name of the agent that owns it.
    pub parent: String,
    /// Its name, description, agent-card URL and credentials.
    pub agent: RemoteAgent,
}

/// The agents an [`AgentDef`](crate::AgentDef) makes: the root and one [`LlmAgent`] for each
/// local subagent definition, ready to register on a runtime.
///
/// A subagent is an agent of its own, registered under `<parent>/<name>` with its own prompt,
/// tools, skills, model alias and limits, and it inherits nothing from its parent: not the tools
/// (a subagent with no `tools:` has none), not the skills, not the history. Its parent gets one
/// tool named after it ([`SubagentTool`](crate::SubagentTool)) whose call starts the subagent as a
/// durable child run on the runtime that steps the parent, so **every agent of the assembly must
/// be registered on that runtime**, which [`register`](Self::register) does.
#[derive(Debug)]
pub struct Assembly {
    manifest: AgentManifest,
    agents: Vec<LlmAgent>,
    info: Vec<AgentInfo>,
    remotes: Vec<RemoteInfo>,
}

impl Assembly {
    /// Every agent, the root first and then each subagent (depth first, in name order). What
    /// `Runtime::builder(..).agent(a)` takes, one by one; [`register`](Self::register) does the
    /// loop.
    pub fn agents(&self) -> &[LlmAgent] {
        &self.agents
    }

    /// The root agent.
    pub fn root(&self) -> &LlmAgent {
        // `model` builds the root first, so the list is never empty.
        &self.agents[0]
    }

    /// Register every agent on a runtime builder: the root and each subagent. A subagent tool
    /// starts its child on the runtime that steps the parent, and finds it by the name it is
    /// registered under, so there is no handle to attach and nothing to forget: what registers the
    /// agents makes the tools work. A runtime that steps the root but does not know a subagent
    /// (a process that registered `assembly.root()` alone) refuses the call for good, and the model
    /// sees an error result naming the missing agent. In a split deployment, register the
    /// subagents as starters on the process that steps the parent.
    pub fn register(&self, builder: RuntimeBuilder) -> RuntimeBuilder {
        self.agents
            .iter()
            .fold(builder, |builder, agent| builder.agent(agent.clone()))
    }

    /// What each agent in [`agents`](Self::agents) was made from, in the same order.
    pub fn info(&self) -> &[AgentInfo] {
        &self.info
    }

    /// The remote subagents the files name.
    pub fn remotes(&self) -> &[RemoteInfo] {
        &self.remotes
    }

    /// The manifest the root agent was made from: its skills, `mcp.json` and schedules (the
    /// skills are bound; `mcp.json` and schedules are for later slices).
    pub fn manifest(&self) -> &AgentManifest {
        &self.manifest
    }
}
