//! [`BoundDef`] (tools bound, prompts rendered) and [`Assembly`] (the `LlmAgent`s).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adam_agent_fs::{AgentManifest, ModelRef, RemoteAgent};
use adam_llm_agent::{
    DEFAULT_MAX_PARALLEL, DynTool, DynToolSource, Limits, LlmAgent, LlmAgentBuilder,
    MAX_PARALLEL_LIMIT, SESSION_TOOLS, StepIo, ToolSource,
};
use adam_model::DynModel;
use adam_runtime::{RuntimeBuilder, SessionLimits};

use crate::def::{Node, Remote};
use crate::error::{AliasProblem, Error, Origin};
use crate::remote_session::RemoteRoundAgent;
use crate::scrub::{Scrub, Scrubbed};
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
    sources: Vec<DynToolSource>,
    aliases: Option<Vec<String>>,
    wait_poll: Option<Duration>,
    max_parallel: Option<usize>,
    session_limits: SessionLimits,
    #[cfg(feature = "a2a")]
    child_sessions: Option<adam_a2a_runtime::ChildSessions>,
    step_io: Option<StepIo>,
    scrub: Option<Scrub>,
    scrub_longest: Option<crate::scrub::Longest>,
    #[cfg(feature = "mcp")]
    prompt_commands: Vec<adam_mcp::PromptCommand>,
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
            .field("sources", &self.sources.len())
            .field("aliases", &self.aliases)
            .finish_non_exhaustive()
    }
}

impl BoundDef {
    pub(crate) fn new(
        manifest: AgentManifest,
        nodes: Vec<Node>,
        remotes: Vec<Remote>,
        session_limits: SessionLimits,
    ) -> Self {
        Self {
            manifest,
            nodes,
            remotes,
            state: Vec::new(),
            sources: Vec::new(),
            aliases: None,
            wait_poll: None,
            max_parallel: None,
            session_limits,
            #[cfg(feature = "a2a")]
            child_sessions: None,
            step_io: None,
            scrub: None,
            scrub_longest: None,
            #[cfg(feature = "mcp")]
            prompt_commands: Vec::new(),
        }
    }

    /// What the definition serves `child-sessions/v1` for, kept for [`Assembly::child_sessions`].
    #[cfg(feature = "a2a")]
    pub(crate) fn with_child_sessions(
        mut self,
        sessions: Option<adam_a2a_runtime::ChildSessions>,
    ) -> Self {
        self.child_sessions = sessions;
        self
    }

    /// The prompts of the root's MCP servers, served as commands.
    #[cfg(feature = "mcp")]
    pub(crate) fn with_prompt_commands(mut self, commands: Vec<adam_mcp::PromptCommand>) -> Self {
        self.prompt_commands = commands;
        self
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

    /// Give every agent a [`ToolSource`]: tools it learns about while it runs, offered to the model
    /// at every turn after the agent's own tools ([`LlmAgentBuilder::tool_source`]). The sources are
    /// asked in the order they were added. An agent's `tools:` does not narrow them (they are not
    /// known when the files are read), so a deployment gives a source only the tools it wants an agent
    /// to have.
    #[must_use]
    pub fn tool_source(mut self, source: impl ToolSource) -> Self {
        self.sources.push(std::sync::Arc::new(source));
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

    /// How long an agent that waits for something (a subagent's child run, the task of a remote
    /// subagent) sleeps between looks: [`LlmAgentBuilder::wait_poll`], for every agent. The default
    /// is the loop's, 60 seconds. A child run tells its parent when it is done, so the timer is
    /// only its fallback; **a remote subagent has no such message**, so this is how often its task
    /// is looked at, and the longest a finished remote task waits to be noticed.
    #[must_use]
    pub fn wait_poll(mut self, every: Duration) -> Self {
        self.wait_poll = Some(every);
        self
    }

    /// The most tool calls of one model turn that run at the same time, for every agent: the
    /// deployment's `MAX_PARALLEL_TOOL_CALLS` ([`LlmAgentBuilder::max_parallel`], 1 to 32; the
    /// loop's default is 10). An agent's file may **lower** it for that agent with
    /// `limits.max_parallel`, never raise it: a larger value in a file is the deployment's, with a
    /// warning.
    #[must_use]
    pub fn max_parallel(mut self, n: usize) -> Self {
        self.max_parallel = Some(n);
        self
    }

    /// The limit of the agent `node`: the deployment's (or the loop's default) lowered by the
    /// agent's own file.
    fn parallel_of(&self, node: &Node) -> usize {
        let allowed = self
            .max_parallel
            .map_or(DEFAULT_MAX_PARALLEL, |n| n.clamp(1, MAX_PARALLEL_LIMIT));
        match node.max_parallel {
            Some(asked) if asked > allowed => {
                tracing::warn!(
                    agent = %node.name,
                    asked,
                    allowed,
                    "`limits.max_parallel` is more than the deployment allows (MAX_PARALLEL_TOOL_CALLS): \
                     the agent runs {allowed} tool calls at once"
                );
                allowed
            }
            Some(asked) => asked.max(1),
            None => allowed,
        }
    }

    /// How the step of a tool call reports its input and output, for every agent:
    /// [`LlmAgentBuilder::step_io`]. A deployment that holds secrets gives the redactor that knows
    /// them here ([`StepIo::redact`]); without it, both are sent as the model and the tool had them,
    /// cut to the contract's bounds.
    #[must_use]
    pub fn step_io(mut self, step_io: StepIo) -> Self {
        self.step_io = Some(step_io);
        self
    }

    /// Scrub secrets out of what the tools that run a subagent in the background give the model:
    /// the report of a session, and every error. The five session tools (`subagent_start` and the
    /// others, given to an agent that has subagents) are made here, not by the deployment, so the
    /// deployment's own wrapper around its tools never sees them; this is the same hook for them,
    /// and `scrub` is what that wrapper would apply (the coder's redactor). Without it the report is
    /// given as the subagent wrote it. The function is applied to a text and must return the same
    /// text with its secrets replaced.
    #[must_use]
    pub fn scrub(mut self, scrub: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.scrub = Some(Arc::new(scrub));
        self
    }

    /// How long, in bytes, the longest value [`scrub`](Self::scrub) replaces is (the coder's
    /// `Redactor::longest`): the session tools' [`ToolCtx::scrub_margin`](adam_llm_agent::ToolCtx::scrub_margin)
    /// is then the same as for the deployment's own tools. A session report is a text a child wrote,
    /// scrubbed whole; it is never a cut text, so no open edge is kept back for it.
    #[must_use]
    pub fn scrub_longest(mut self, longest: impl Fn() -> usize + Send + Sync + 'static) -> Self {
        self.scrub_longest = Some(Arc::new(longest));
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
        let rounds = self
            .nodes
            .iter()
            .flat_map(|node| node.round_agents.iter().cloned())
            .map(|round| match self.wait_poll {
                Some(every) => round.with_wait_poll(every),
                None => round,
            })
            .collect();
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
            rounds,
            info,
            remotes,
            session_limits: self.session_limits,
            #[cfg(feature = "a2a")]
            child_sessions: self.child_sessions,
            #[cfg(feature = "mcp")]
            prompt_commands: self.prompt_commands,
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

    /// The tool as the agent gets it: a session tool is scrubbed when the deployment gave a scrub.
    fn guarded(&self, name: &str, tool: &DynTool) -> DynTool {
        match &self.scrub {
            Some(scrub) if SESSION_TOOLS.contains(&name) => Arc::new(Scrubbed::new(
                Arc::clone(tool),
                Arc::clone(scrub),
                self.scrub_longest.clone(),
            )),
            _ => Arc::clone(tool),
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
    /// The remote subagents (slice S9b) join the same list in `bind`, where their names meet the
    /// local ones, and so do the MCP tools of `mcp.json` (slice S11), which `tools:` selects. A
    /// server whose `when` loads it only when a run's task matches gets a source of its own here,
    /// so its tools are withheld from a run that does not load it.
    fn build(
        &self,
        node: &Node,
        origin: &Origin,
        model: &DynModel,
        alias: &str,
    ) -> Result<LlmAgent, Error> {
        let mut builder = LlmAgent::builder(node.name.clone(), Arc::clone(model), alias)
            .instructions(node.prompt.clone())
            .limits(node.limits)
            .max_parallel(self.parallel_of(node));
        if let Some(every) = self.wait_poll {
            builder = builder.wait_poll(every);
        }
        if let Some(step_io) = &self.step_io {
            builder = builder.step_io(step_io.clone());
        }
        for (name, tool) in &node.tools {
            builder = builder.dyn_tool(self.guarded(name, tool));
        }
        if !node.session_agents.is_empty() {
            builder = builder.session_agents(node.session_agents.clone());
        }
        for apply in &self.state {
            builder = apply(builder);
        }
        // The agent's own sources first (the `mcp.json` servers a run loads only when its task
        // matches), then the deployment's, in the order it added them.
        for source in &node.sources {
            builder = builder.dyn_tool_source(Arc::clone(source));
        }
        for source in &self.sources {
            builder = builder.dyn_tool_source(Arc::clone(source));
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
    /// skills catalog, the preloaded skills and the instructions of its **eager** MCP servers. A
    /// server with a `when` adds its section to a run, when the run loads it.
    pub prompt: String,
    /// The names of its tools, in the order the model is shown them: its own, then `load_skill`
    /// and `read_skill_file` when it has skills to load and files to read, then one per local
    /// subagent (the subagent's name: the entries of `info()` whose `parent` is this agent). An
    /// MCP server with a `when` contributes its tools here too: a run that does not load it is not
    /// shown them.
    pub tools: Vec<String>,
    /// The skills it may use, in the order of its `skills:` (by name for `all`).
    pub skills: Vec<String>,
    /// The subset of [`skills`](Self::skills) whose body is in the prompt (`preload_skills:`).
    pub preloaded: Vec<String>,
    /// The loop's limits.
    pub limits: Limits,
}

/// A remote (A2A) subagent found in the files, as data. Its tool is in the parent's
/// [`AgentInfo::tools`], made by [`AgentDef::bind`](crate::AgentDef::bind) with the URL and the token
/// checked; this is what the files said.
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
    rounds: Vec<RemoteRoundAgent>,
    info: Vec<AgentInfo>,
    remotes: Vec<RemoteInfo>,
    session_limits: SessionLimits,
    #[cfg(feature = "a2a")]
    child_sessions: Option<adam_a2a_runtime::ChildSessions>,
    #[cfg(feature = "mcp")]
    prompt_commands: Vec<adam_mcp::PromptCommand>,
}

impl Assembly {
    /// The limits the root's sessions run under: the deployment's
    /// ([`AgentDef::session_limits`](crate::AgentDef::session_limits)) lowered by the root's file.
    pub fn session_limits(&self) -> SessionLimits {
        self.session_limits
    }

    /// What the root's A2A front serves `child-sessions/v1` for, as
    /// [`AgentDef::child_sessions`](crate::AgentDef::child_sessions) computed it when the
    /// definition was bound: `None` when the root has no subagent. Only with the feature `a2a`.
    #[cfg(feature = "a2a")]
    pub fn child_sessions(&self) -> Option<&adam_a2a_runtime::ChildSessions> {
        self.child_sessions.as_ref()
    }

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
        let builder = self
            .agents
            .iter()
            .fold(builder, |builder, agent| builder.agent(agent.clone()));
        self.rounds
            .iter()
            .fold(builder, |builder, round| builder.agent(round.clone()))
    }

    /// The agents that work the rounds of a session on a remote subagent of the root, registered
    /// as `<root>/<subagent>`: [`register`](Self::register) registers them with the rest. A
    /// composition root that registers [`agents`](Self::agents) one by one registers these too, or
    /// `subagent_start` on a remote subagent is refused in a process that does not know the name.
    pub fn remote_round_agents(&self) -> &[RemoteRoundAgent] {
        &self.rounds
    }

    /// What each agent in [`agents`](Self::agents) was made from, in the same order.
    pub fn info(&self) -> &[AgentInfo] {
        &self.info
    }

    /// The remote subagents the files name.
    pub fn remotes(&self) -> &[RemoteInfo] {
        &self.remotes
    }

    /// The prompts of the root agent's MCP servers as commands named `<server>__<prompt>`
    /// (`commands/v1`), read once when the servers were connected. A subagent's servers give none:
    /// commands are the root's. A server with a bearer per call, or with `prompts: false`, gives
    /// none. Only with the feature `mcp`.
    #[cfg(feature = "mcp")]
    pub fn prompt_commands(&self) -> &[adam_mcp::PromptCommand] {
        &self.prompt_commands
    }

    /// The manifest the root agent was made from: its skills, `mcp.json` and schedules (the
    /// skills and the MCP servers are bound; schedules are for a later slice).
    pub fn manifest(&self) -> &AgentManifest {
        &self.manifest
    }
}
