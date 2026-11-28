//! [`BoundDef`] (tools bound, prompts rendered) and [`Assembly`] (the `LlmAgent`s).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use adam_agent_fs::{AgentManifest, ModelRef, RemoteAgent, ToolsLoading};
use adam_budget::DynAdmission;
use adam_llm_agent::{
    DynTool, DynToolSource, Limits, LlmAgent, LlmAgentBuilder, ModelBinding, ModelParking,
    SESSION_TOOLS, SessionAgent, SessionTools, StepIo, ToolSource, WINDOW_MARGIN_TOKENS,
};
use adam_model::{DEFAULT_MAX_OUTPUT_TOKENS, DynModel, ModelProfiles, Preset, ReasoningEffort};
use adam_runtime::{RuntimeBuilder, SessionBounds, SessionLimits};

use crate::bounds::parallel_limit;
use crate::def::{Node, Remote, SessionTarget};
use crate::error::{AliasProblem, CapSource, EffortSource, Error, Origin};
#[cfg(feature = "remote")]
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
    profiles: ModelProfiles,
    model_choices: bool,
    wait_poll: Option<Duration>,
    max_parallel: Option<usize>,
    model_parking: Option<ModelParking>,
    admission: Option<DynAdmission>,
    session_limits: SessionLimits,
    #[cfg(feature = "sessions")]
    child_sessions: Option<adam_a2a_runtime::ChildSessions>,
    step_io: Option<StepIo>,
    scrub: Option<Scrub>,
    scrub_longest: Option<crate::scrub::Longest>,
    /// The host's hooks, which every agent runs before the plugins its file names
    /// ([`hooks`](Self::hooks)).
    hooks: Option<adam_plugin::HookSet>,
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

/// What one agent is built with, beyond its node and its tools: its model alias, the reasoning
/// effort and the output cap resolved for that alias, and — for the root — the session tools whose
/// bounds are now known (ADR 0054 slice 1 part 2).
struct Built<'a> {
    alias: &'a str,
    effort: Option<ReasoningEffort>,
    cap: u32,
    sessions: Option<&'a SessionTools>,
}

impl BoundDef {
    /// The names of the root agent's tools, in the order the model is shown them.
    ///
    /// The same list [`AgentInfo::tools`] gives after a full assembly, without building one: what
    /// a test needs to prove that a tool the deployment turns on is offered — and that nothing is
    /// offered when it is off.
    #[must_use]
    pub fn tool_names(&self) -> Vec<String> {
        self.nodes
            .iter()
            .find(|node| node.parent.is_none())
            .map(|node| node.tools.iter().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default()
    }

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
            profiles: ModelProfiles::default(),
            model_choices: false,
            wait_poll: None,
            max_parallel: None,
            model_parking: None,
            admission: None,
            session_limits,
            #[cfg(feature = "sessions")]
            child_sessions: None,
            step_io: None,
            scrub: None,
            scrub_longest: None,
            hooks: None,
            #[cfg(feature = "mcp")]
            prompt_commands: Vec::new(),
        }
    }

    /// What the definition serves `child-sessions/v1` for, kept for [`Assembly::child_sessions`].
    #[cfg(feature = "sessions")]
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

    /// The deployment's per-model profiles (ADR 0063, ADR 0064): what each model takes as a
    /// reasoning effort, which preset it starts from, and its output cap. [`model`](Self::model)
    /// resolves each agent's default effort from its folder, its `model_preset`, the profile and
    /// the profile's preset, and refuses a value the model does not take; it resolves the cap the
    /// same way (then 128,000) and clamps it to the model's limit and window. Without them no model
    /// has a profile, so an agent that sets an effort is refused, one that sets none sends none, and
    /// every cap is the folder's or 128,000.
    #[must_use]
    pub fn model_profiles(mut self, profiles: ModelProfiles) -> Self {
        self.profiles = profiles;
        self
    }

    /// Let a message pick the model of its run among the deployment's profiles
    /// ([`MODEL_CONTEXT_KEY`](adam_llm_agent::MODEL_CONTEXT_KEY), ADR 0064 amended 2026-11-23):
    /// every agent is given a binding, with its cap and reasoning level, for each model that has a
    /// profile ([`model_profiles`](Self::model_profiles)). Off by default: a served agent's callers
    /// may not choose what a run costs, so the key is ignored there. The engine turns it on.
    #[must_use]
    pub fn allow_model_choice(mut self) -> Self {
        self.model_choices = true;
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
    /// `limits.max_parallel`; a larger value in a file is clamped to the deployment's, with a
    /// warning (ADR 0049, ADR 0054 decision 3), so a folder stays usable under a deployment that
    /// allows less.
    #[must_use]
    pub fn max_parallel(mut self, n: usize) -> Self {
        self.max_parallel = Some(n);
        self
    }

    /// How every agent waits on a model that refused it or could not be reached
    /// ([`LlmAgentBuilder::model_parking`], [ADR 0067]): the deployment's `MODEL_PARK_LIMIT`, and
    /// whether an unreachable endpoint parks (`MODEL_AUTH=injector`). Without it the loop's
    /// default: a limit of 24 hours, and an unreachable endpoint that fails the run.
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    #[must_use]
    pub fn model_parking(mut self, parking: ModelParking) -> Self {
        self.model_parking = Some(parking);
        self
    }

    /// The process's pools ([`adam_budget::Admission`], [ADR 0065]): every agent built here asks it
    /// for every model call and every tool call. Without one, nothing is admitted.
    ///
    /// [ADR 0065]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md
    #[must_use]
    pub fn admission(mut self, admission: DynAdmission) -> Self {
        self.admission = Some(admission);
        self
    }

    /// The deployment's `MAX_PARALLEL_TOOL_CALLS` given on the definition
    /// ([`AgentDef::session_max_parallel`](crate::AgentDef::session_max_parallel)), unless
    /// [`max_parallel`](Self::max_parallel) already says another.
    pub(crate) fn with_default_max_parallel(mut self, n: Option<usize>) -> Self {
        self.max_parallel = self.max_parallel.or(n);
        self
    }

    /// The limit of the agent `node`: the deployment's (or the loop's default) **lowered by the
    /// agent's own file**. A file above the deployment's is **clamped** to it, 1 to
    /// `MAX_PARALLEL_LIMIT`, with a warning naming both values: a folder is portable across
    /// deployments, so a deployment that lowered the limit narrows the agent rather than refusing
    /// it to start, and the effective setting says what the agent runs at (ADR 0049, ADR 0054
    /// decision 3 — "clamped to the bounds of that time").
    ///
    /// This is the startup relationship between a folder and its deployment. A *round's requested*
    /// `config` above a bound is the runtime question, and stays a refusal by
    /// [`SessionBounds::admit`], naming the option (ADR 0054 decision 2); see `bounds_of`.
    fn parallel_of(&self, node: &Node) -> usize {
        let allowed = parallel_limit(self.max_parallel, None);
        if let Some(asked) = node.max_parallel
            && asked > allowed
        {
            tracing::warn!(
                agent = %node.name,
                asked,
                allowed,
                "`limits.max_parallel` is more than the deployment allows (MAX_PARALLEL_TOOL_CALLS): \
                 the agent runs {allowed} tool calls at once (ADR 0049, ADR 0054 decision 3)"
            );
        }
        parallel_limit(self.max_parallel, node.max_parallel)
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

    /// The host's own hooks (ADR 0050), for **every** agent built here, the root and each subagent:
    /// they run first, in their order and under the set's limits, then the plugins the agent's file
    /// names in `plugins:` ([`AgentDef::plugin`](crate::AgentDef::plugin)). A host that must be asked
    /// before a tool runs (an application that embeds the engine, ADR 0047 slice 5) gives the plugin
    /// here, so no agent file can leave it out. [`AgentInfo::plugins`] lists the file's plugins only.
    /// Calling this again replaces the set.
    #[must_use]
    pub fn hooks(mut self, hooks: adam_plugin::HookSet) -> Self {
        self.hooks = Some(hooks);
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
    /// * [`Error::UnknownModelPreset`], [`Error::InvalidReasoningEffort`] or
    ///   [`Error::ReasoningEffort`] when the effort an agent resolves to is not one of its model's
    ///   levels ([`model_profiles`](Self::model_profiles));
    /// * [`Error::OutputCap`] when a preset its `model_preset` names asks for more output tokens
    ///   than the model documents;
    /// * [`Error::Build`] when a tool needs state that [`state`](Self::state) did not give.
    pub fn model(self, model: DynModel, alias: impl Into<String>) -> Result<Assembly, Error> {
        let default_alias = alias.into();
        // The five session tools are made once, here, because their bounds need what `bind` does
        // not know: the deployment's `MAX_PARALLEL_TOOL_CALLS` (ADR 0054 slice 1 part 2). The root
        // is the only agent that has them, and the root is built first.
        let session_tools = match self.nodes.first() {
            Some(root) => self.session_tools(root),
            None => None,
        };
        let session_bounds = session_tools
            .as_ref()
            .map(SessionTools::bounds)
            .unwrap_or_default();
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
            let effort = self.default_effort(node, &origin, &alias)?;
            let cap = self.output_cap(node, &origin, &alias, &model, false)?;
            let fallback = self.fallback_of(node, &origin, &alias, &model)?;
            let choices = self.choices_of(node, &origin, &alias, &model);
            let (model_fallback, model_choices) = (
                fallback.as_ref().map(|b| b.alias.clone()),
                choices.iter().map(|b| b.alias.clone()).collect(),
            );
            agents.push(self.build(
                node,
                &origin,
                &model,
                Built {
                    alias: &alias,
                    effort,
                    cap,
                    sessions: if node.parent.is_none() {
                        session_tools.as_ref()
                    } else {
                        None
                    },
                },
                fallback,
                choices,
            )?);
            info.push(AgentInfo {
                name: node.name.clone(),
                parent: node
                    .parent
                    .and_then(|p| self.nodes.get(p))
                    .map(|p| p.name.clone()),
                description: node.description.clone(),
                file: node.file.clone(),
                model_alias: alias.clone(),
                model_fallback,
                model_choices,
                reasoning_effort: effort,
                prompt: node.prompt.clone(),
                tools: {
                    let mut tools: Vec<String> =
                        node.tools.iter().map(|(name, _)| name.clone()).collect();
                    // The five session tools are built at `model`, not at `bind` (ADR 0054
                    // slice 1 part 2): they come after everything `bind` made, as they always
                    // have.
                    if node.parent.is_none() && !node.session_targets.is_empty() {
                        tools.extend(SESSION_TOOLS.iter().map(|name| (*name).to_owned()));
                    }
                    tools
                },
                skills: node.skills.clone(),
                preloaded: node.preloaded.clone(),
                when: node.when.clone(),
                tools_loading: node.tools_loading,
                plugins: node.plugins.iter().map(|(name, _)| name.clone()).collect(),
                retry: node
                    .retry
                    .iter()
                    .map(|(tool, def, _)| (tool.clone(), def.clone()))
                    .collect(),
                limits: Limits {
                    max_output_tokens: cap,
                    ..node.limits
                },
            });
            resolved.push(alias);
        }
        let mut rounds = Rounds::default();
        for node in &self.nodes {
            rounds.extend(&node.round_agents);
        }
        if let Some(every) = self.wait_poll {
            rounds = rounds.with_wait_poll(every);
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
            rounds,
            info,
            remotes,
            session_limits: self.session_limits,
            // The bounds the session tools hold a `config` to, handed to the A2A front too, so a
            // person's `start.config` and the model's `config` are admitted by the same numbers
            // (ADR 0054 decision 2).
            #[cfg(feature = "sessions")]
            child_sessions: self
                .child_sessions
                .map(|sessions| sessions.with_bounds(session_bounds.clone())),
            #[cfg(feature = "mcp")]
            prompt_commands: self.prompt_commands,
            session_bounds,
        })
    }

    /// The alias `node`'s run continues on when its own model is locked (ADR 0064, amended
    /// 2026-11-23): the folder's `model_fallback`, else the one of the preset its `model_preset`
    /// names, else the one of its model's profile, else of that profile's base preset.
    fn fallback_name<'a>(
        &'a self,
        node: &'a Node,
        origin: &Origin,
        alias: &str,
    ) -> Result<Option<&'a str>, Error> {
        if let Some(name) = node.model_fallback.as_deref() {
            return Ok(Some(name));
        }
        Ok(self
            .named_preset(node, origin)?
            .and_then(|p| p.fallback.as_deref())
            .or_else(|| {
                self.profiles
                    .profile(alias)
                    .and_then(|p| p.fallback.as_deref())
            })
            .or_else(|| {
                self.profiles
                    .preset_of(alias)
                    .and_then(|p| p.fallback.as_deref())
            }))
    }

    /// The model `node`'s run continues on when its own is locked, with the output cap and the
    /// reasoning level its **own** profile resolves (what the folder and its preset set is for the
    /// agent's model, which the fallback may not take). Refused when the alias is not one the
    /// deployment serves, is the agent's own, or has a cap or a level that cannot be resolved.
    fn fallback_of(
        &self,
        node: &Node,
        origin: &Origin,
        alias: &str,
        model: &DynModel,
    ) -> Result<Option<ModelBinding>, Error> {
        let Some(name) = self.fallback_name(node, origin, alias)? else {
            return Ok(None);
        };
        self.check_alias(origin, name)?;
        if name == alias {
            return Err(Error::ModelAlias {
                origin: origin.clone(),
                alias: name.to_owned(),
                problem: AliasProblem::SameAsModel,
            });
        }
        let cap = self.output_cap(node, origin, name, model, true)?;
        Ok(Some(
            ModelBinding::new(name, cap).reasoning_effort(self.own_effort(origin, name)?),
        ))
    }

    /// The models a message may pick for `node`'s run, when the deployment allows it: one binding
    /// for each model with a profile, but the agent's own. A model whose cap cannot be resolved for
    /// this agent is not offered.
    fn choices_of(
        &self,
        node: &Node,
        origin: &Origin,
        alias: &str,
        model: &DynModel,
    ) -> Vec<ModelBinding> {
        if !self.model_choices {
            return Vec::new();
        }
        self.profiles
            .profiles
            .keys()
            .filter(|name| name.as_str() != alias)
            .filter(|name| {
                self.aliases
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(name))
            })
            .filter_map(|name| {
                let cap = self.output_cap(node, origin, name, model, true).ok()?;
                let effort = self.own_effort(origin, name).ok()?;
                Some(ModelBinding::new(name, cap).reasoning_effort(effort))
            })
            .collect()
    }

    /// The level a model's own profile asks for: its `reasoningEffort`, else its base preset's,
    /// which must be one of the model's levels.
    fn own_effort(&self, origin: &Origin, alias: &str) -> Result<Option<ReasoningEffort>, Error> {
        let profile = self.profiles.profile(alias);
        let Some(effort) = profile.and_then(|p| p.reasoning_effort).or_else(|| {
            self.profiles
                .preset_of(alias)
                .and_then(|p| p.reasoning_effort)
        }) else {
            return Ok(None);
        };
        let allowed: Vec<ReasoningEffort> = profile
            .and_then(|p| p.reasoning_efforts.clone())
            .unwrap_or_default();
        if !allowed.contains(&effort) {
            let allowed: Vec<String> = allowed.iter().map(|e| e.as_str().to_owned()).collect();
            return Err(Error::ReasoningEffort {
                origin: origin.clone(),
                effort: effort.as_str().to_owned(),
                set_by: EffortSource::Profile,
                model: alias.to_owned(),
                suggestion: closest(effort.as_str(), allowed.iter().map(String::as_str))
                    .map(str::to_owned),
                allowed,
            });
        }
        Ok(Some(effort))
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

    /// The effort the agent's model calls ask for by default (ADR 0063 decision 4, ADR 0064
    /// decision 3): its `reasoning_effort`, else the preset its `model_preset` names, else the
    /// profile's own `reasoningEffort`, else the profile's base preset. Every value must be one of
    /// the model's levels; a model with no profile takes none.
    fn default_effort(
        &self,
        node: &Node,
        origin: &Origin,
        alias: &str,
    ) -> Result<Option<ReasoningEffort>, Error> {
        let profile = self.profiles.profile(alias);
        let allowed: &[ReasoningEffort] = profile
            .and_then(|p| p.reasoning_efforts.as_deref())
            .unwrap_or(&[]);
        let preset = self.named_preset(node, origin)?;
        let chosen = if let Some(word) = &node.reasoning_effort {
            let effort = word.parse::<ReasoningEffort>().map_err(|source| {
                Error::InvalidReasoningEffort {
                    origin: origin.clone(),
                    source,
                }
            })?;
            Some((effort, EffortSource::Folder))
        } else if let Some(effort) = preset.and_then(|p| p.reasoning_effort) {
            Some((
                effort,
                EffortSource::Preset(node.model_preset.as_deref().unwrap_or_default().to_owned()),
            ))
        } else if let Some(effort) = profile.and_then(|p| p.reasoning_effort) {
            Some((effort, EffortSource::Profile))
        } else {
            self.profiles
                .preset_of(alias)
                .and_then(|p| p.reasoning_effort)
                .map(|effort| (effort, EffortSource::Profile))
        };
        let Some((effort, set_by)) = chosen else {
            return Ok(None);
        };
        if !allowed.contains(&effort) {
            let allowed: Vec<String> = allowed.iter().map(|e| e.as_str().to_owned()).collect();
            return Err(Error::ReasoningEffort {
                origin: origin.clone(),
                effort: effort.as_str().to_owned(),
                set_by,
                model: alias.to_owned(),
                suggestion: closest(effort.as_str(), allowed.iter().map(String::as_str))
                    .map(str::to_owned),
                allowed,
            });
        }
        Ok(Some(effort))
    }

    /// The preset the agent's `model_preset` names, when it names one (ADR 0064). An undeclared
    /// name is refused here, whether or not another setting wins over the preset's values.
    fn named_preset<'a>(
        &'a self,
        node: &Node,
        origin: &Origin,
    ) -> Result<Option<&'a Preset>, Error> {
        let Some(name) = node.model_preset.as_deref() else {
            return Ok(None);
        };
        match self.profiles.preset(name) {
            Some(preset) => Ok(Some(preset)),
            None => {
                let known: Vec<String> = self.profiles.presets.keys().cloned().collect();
                Err(Error::UnknownModelPreset {
                    origin: origin.clone(),
                    preset: name.to_owned(),
                    suggestion: closest(name, known.iter().map(String::as_str)).map(str::to_owned),
                    known,
                })
            }
        }
    }

    /// The output cap the agent's model calls ask for (ADR 0064 decision 3): its folder's
    /// `limits.max_output_tokens`, else the preset its `model_preset` names, else the profile's
    /// `maxOutputTokens`, else the profile's base preset, else
    /// [`DEFAULT_MAX_OUTPUT_TOKENS`]. The value is then clamped to the model's
    /// `maxOutputTokensLimit` and, when the window is known, to what the window leaves after
    /// [`WINDOW_MARGIN_TOKENS`] and the history the agent keeps (`max_history_tokens`, or half the
    /// window when it is `0`; an omitted key keeps the 100,000 default).
    ///
    /// A preset above the model's limit is refused, not clamped: a preset is the deployment's
    /// promise about the model, where the folder's value is the agent's own choice. The folder's
    /// value is clamped, and so is the profile's, so a model that documents less than the value
    /// never receives more than it allows.
    ///
    /// With `own`, the cap of a model the agent does not run on itself (its fallback, a model a
    /// message picks): the folder's value and its preset's are for the agent's model and are left
    /// out, the model's own profile decides, and the history the window leaves room for is at
    /// most half of that model's window.
    fn output_cap(
        &self,
        node: &Node,
        origin: &Origin,
        alias: &str,
        model: &DynModel,
        own: bool,
    ) -> Result<u32, Error> {
        let profile = self.profiles.profile(alias);
        let agent_preset = if own {
            None
        } else {
            self.named_preset(node, origin)?
        };
        let base_preset = self.profiles.preset_of(alias);
        let limit = profile.and_then(|p| p.max_output_tokens_limit);
        if let Some(limit) = limit {
            for (name, preset, from_profile) in [
                (node.model_preset.as_deref(), agent_preset, false),
                (profile.and_then(|p| p.preset.as_deref()), base_preset, true),
            ] {
                if let (Some(name), Some(cap)) = (name, preset.and_then(|p| p.max_output_tokens))
                    && cap > limit
                {
                    // The loader refuses a profile's named preset above its limit for a parsed
                    // file; a hand-built `ModelProfiles` can reach it here, and the value is in
                    // the profiles file, not in the agent's file.
                    let origin = if from_profile {
                        Origin::new(alias.to_owned(), "MODEL_ALIASES_FILE")
                    } else {
                        origin.clone()
                    };
                    return Err(Error::OutputCap {
                        origin,
                        set_by: CapSource::Preset(name.to_owned()),
                        cap,
                        limit,
                        model: alias.to_owned(),
                    });
                }
            }
        }
        // A hand-built `ModelProfiles` may hold a `0` the loader refuses; a cap of 0 is no cap, so
        // every source is filtered and the result is at least 1.
        let chosen = node
            .max_output_tokens
            .filter(|cap| !own && *cap > 0)
            .or(agent_preset
                .and_then(|p| p.max_output_tokens)
                .filter(|cap| *cap > 0))
            .or(profile
                .and_then(|p| p.max_output_tokens)
                .filter(|cap| *cap > 0))
            .or(base_preset
                .and_then(|p| p.max_output_tokens)
                .filter(|cap| *cap > 0))
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let mut cap = chosen;
        if let Some(limit) = limit {
            cap = cap.min(limit);
        }
        let window = model.context_window(alias);
        let history = window.map(|window| {
            if node.limits.max_history_tokens > 0 {
                let own_half = if own { window / 2 } else { u64::MAX };
                u64::from(node.limits.max_history_tokens).min(own_half)
            } else {
                window / 2
            }
        });
        if let (Some(window), Some(history)) = (window, history) {
            let room = window
                .saturating_sub(WINDOW_MARGIN_TOKENS.saturating_add(history))
                .max(1);
            cap = cap.min(u32::try_from(room).unwrap_or(u32::MAX));
        }
        let cap = cap.max(1);
        if cap < chosen {
            tracing::warn!(
                agent = %origin.agent,
                model = alias,
                chosen,
                cap,
                limit,
                window,
                history,
                margin = WINDOW_MARGIN_TOKENS,
                "the output cap is lowered to what the model allows (its maxOutputTokensLimit or \
                 the window; ADR 0064)"
            );
        }
        Ok(cap)
    }

    /// The five session tools of `node`, or `None` when it has no subagent (ADR 0054 slice 1
    /// part 2): each subagent's [`SessionBounds`], made here because the deployment's
    /// `MAX_PARALLEL_TOOL_CALLS` is known and `bind` did not know it. The names and order are the
    /// ones `bind` fixed.
    fn session_tools(&self, node: &Node) -> Option<SessionTools> {
        if node.session_targets.is_empty() {
            return None;
        }
        let mut agents = Vec::with_capacity(node.session_targets.len());
        for target in &node.session_targets {
            agents.push(SessionAgent {
                name: target.name.clone(),
                agent: target.agent.clone(),
                description: target.description.clone(),
                within: target.within,
                remote: target.remote,
                bounds: self.bounds_of(target),
            });
        }
        Some(SessionTools::new(agents).with_limits(self.session_limits))
    }

    /// The bounds of one subagent a session may be started on (ADR 0054 decision 2): the one
    /// function of `crate::bounds`, which `AgentDef::session_bounds` calls too, so a worker's tools
    /// and a control plane's A2A front agree on every number. They differ in one fact only a worker
    /// has: the names of the tools the subagent is bound with
    /// ([`bound_tool_names`](Self::bound_tool_names)).
    fn bounds_of(&self, target: &SessionTarget) -> SessionBounds {
        let (names, complete) = self.bound_tool_names(target);
        crate::bounds::bounds_of(
            &self.manifest,
            target,
            self.max_parallel,
            self.session_limits.max_rounds(),
            names,
            complete,
        )
    }

    /// The names of the tools the subagent `target` can be offered, for `tools` and `deny_tools` to
    /// be checked against (ADR 0054 decision 4), and whether they are **every** tool it can have: the
    /// node whose registered name is the target's, its own tools (its `tools:` and the `mcp.json`
    /// servers' as `server__tool`), and what every source of that node declares
    /// ([`ToolSource::declared`]).
    ///
    /// The set is **incomplete** when a source cannot name its tools before a run calls it
    /// ([`adam_llm_agent::Declared::Unknown`]): an endpoint whose listing is another system's and may change with the
    /// conversation. A remote target, and a name no node has, give no names.
    fn bound_tool_names(&self, target: &SessionTarget) -> (Vec<String>, bool) {
        if target.remote {
            return (Vec::new(), false);
        }
        let Some(node) = self.nodes.iter().find(|node| node.name == target.agent) else {
            return (Vec::new(), false);
        };
        let mut names: Vec<String> = node.tools.iter().map(|(name, _)| name.clone()).collect();
        let mut complete = true;
        for source in self.sources_of(node) {
            match source.declared().names() {
                Some(declared) => {
                    for name in declared {
                        if !names.contains(name) {
                            names.push(name.clone());
                        }
                    }
                }
                None => complete = false,
            }
        }
        (names, complete)
    }

    /// Every [`ToolSource`] the agent of `node` is built with, in the order it asks them: the modes'
    /// (which only holds tools back), the node's own (the `mcp.json` servers a run loads when its
    /// task matches), then the deployment's. [`build`](Self::build) gives them to the agent in this
    /// order, and [`bound_tool_names`](Self::bound_tool_names) reads what each declares from the
    /// same list, so the agent and the session bounds cannot disagree.
    fn sources_of(&self, node: &Node) -> Vec<DynToolSource> {
        let mut sources: Vec<DynToolSource> = Vec::new();
        if let Some(modes) = &node.modes {
            sources.push(Arc::new(crate::modes::ModeSource::new(Arc::clone(modes))));
        }
        sources.extend(node.sources.iter().cloned());
        sources.extend(self.sources.iter().cloned());
        sources
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
        Built {
            alias,
            effort,
            cap,
            sessions,
        }: Built<'_>,
        fallback: Option<ModelBinding>,
        choices: Vec<ModelBinding>,
    ) -> Result<LlmAgent, Error> {
        let mut builder = LlmAgent::builder(node.name.clone(), Arc::clone(model), alias)
            .instructions(node.prompt.clone())
            .limits(Limits {
                max_output_tokens: cap,
                ..node.limits
            })
            .max_parallel(self.parallel_of(node))
            .reasoning_effort(effort)
            .model_choices(choices);
        if let Some(fallback) = fallback {
            builder = builder.model_fallback(fallback);
        }
        if let Some(every) = self.wait_poll {
            builder = builder.wait_poll(every);
        }
        if let Some(parking) = self.model_parking {
            builder = builder.model_parking(parking);
        }
        if let Some(admission) = &self.admission {
            builder = builder.admission(Arc::clone(admission));
        }
        for (tool, _, policy) in &node.retry {
            builder = builder.tool_retry(tool.clone(), *policy);
        }
        // The host's hooks first (`hooks`), then the plugins the file names.
        let host = self.hooks.as_ref().filter(|hooks| !hooks.is_empty());
        if host.is_some() || !node.plugins.is_empty() {
            let hooks = node
                .plugins
                .iter()
                .fold(host.cloned().unwrap_or_default(), |hooks, (_, plugin)| {
                    hooks.with(Arc::clone(plugin))
                });
            builder = builder.hooks(hooks);
        }
        if let Some(step_io) = &self.step_io {
            builder = builder.step_io(step_io.clone());
        }
        for (name, tool) in &node.tools {
            builder = builder.dyn_tool(self.guarded(name, tool));
        }
        // The five session tools come after everything `bind` made, as they always have: they are
        // made at `model`, where each subagent's bounds are known (ADR 0054 slice 1 part 2).
        if let Some(sessions) = sessions {
            for tool in sessions.tools() {
                let name = tool.spec().name;
                builder = builder.dyn_tool(self.guarded(&name, &tool));
            }
        }
        if !node.session_agents.is_empty() {
            builder = builder.session_agents(node.session_agents.clone());
        }
        for apply in &self.state {
            builder = apply(builder);
        }
        // The modes of `agent-mode/v1` first: they hold back what the mode does not offer, and
        // every other source's holds still apply beside theirs (a mode only narrows). Then the
        // agent's own sources (the `mcp.json` servers a run loads only when its task matches), then
        // the deployment's, in the order it added them: one list, which `bound_tool_names` reads
        // the declarations of too.
        for source in self.sources_of(node) {
            builder = builder.dyn_tool_source(source);
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
    /// The alias its run continues on when its model is locked, resolved from its folder, its
    /// preset and its model's profile (ADR 0064, amended 2026-11-23).
    pub model_fallback: Option<String>,
    /// The aliases a message may pick for its run ([`BoundDef::allow_model_choice`]): the models
    /// with a profile that this agent can be bound to, its own left out. Empty unless allowed.
    pub model_choices: Vec<String>,
    /// The reasoning effort its model calls ask for by default, resolved from its folder, its
    /// `model_preset`, the model profile and the profile's preset (ADR 0063 decision 4), or `None`
    /// when none of them sets one.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// The system prompt the model sees: the instructions with the vars substituted, then the
    /// instructions of its **eager** MCP servers, then the skills catalog and the preloaded skills.
    /// A server with a `when` adds its section to a run, when the run loads it.
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
    /// The MCP servers whose `when` loads them only when the run's task mentions one of their
    /// keywords: the server and its keywords, in name order. Empty for an agent whose servers are
    /// all eager. It is part of the info because a definition that differs only here **behaves**
    /// differently: its tools are withheld from a run the task does not select, and its
    /// instructions are added only to such a run.
    pub when: Vec<(String, Vec<String>)>,
    /// `tools_loading` of its file (ADR 0066, decision 1): whether every tool is offered or a run
    /// loads them in groups. An `eager` agent with a `when` server withholds nothing.
    pub tools_loading: ToolsLoading,
    /// `retry:` of its file: the retry policy per tool, by name, as the file says it (ADR 0061).
    /// Each is one of [`tools`](Self::tools). Part of the info because an agent that retries a
    /// tool differently behaves differently.
    pub retry: Vec<(String, adam_agent_fs::RetryDef)>,
    /// The plugins the agent runs, by registered name, in order: its parent's first (ADR 0050,
    /// slice H6).
    pub plugins: Vec<String>,
    /// The loop's limits, with [`max_output_tokens`](Limits::max_output_tokens) resolved for the
    /// agent's model (ADR 0064): the folder's value, its preset's, the profile's, the fallback,
    /// then clamped to the model's limit and window.
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
    rounds: Rounds,
    info: Vec<AgentInfo>,
    remotes: Vec<RemoteInfo>,
    session_limits: SessionLimits,
    session_bounds: Vec<(String, SessionBounds)>,
    #[cfg(feature = "sessions")]
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

    /// The bounds of each subagent the root may start a session on ([ADR 0054] decision 2), by the
    /// name the model writes in `agent`, in the order of the root's manifest: what the five
    /// session tools' [`SessionBounds::admit`] holds a `subagent_start`'s `config` to. Each is
    /// what the subagent's folder declares, narrowed by what the deployment allows; read from the
    /// [`SessionTools`] the root was built with, so a card and a client see what the tool enforces.
    /// Empty when the root has no subagent.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    pub fn session_bounds(&self) -> &[(String, SessionBounds)] {
        &self.session_bounds
    }

    /// The modes the root agent declares (`agent-mode/v1`, ADR 0039 decision 3), as the card lists
    /// them and the A2A front refuses a message for: `None` when the folder has no `modes/`. What
    /// the source narrows is the same set, so the card and the run cannot disagree.
    #[cfg(feature = "sessions")]
    pub fn agent_modes(&self) -> Option<adam_a2a::AgentModes> {
        crate::modes::agent_modes_of(&self.manifest)
    }

    /// What the root's A2A front serves `child-sessions/v1` for, as
    /// [`AgentDef::child_sessions`](crate::AgentDef::child_sessions) computed it when the
    /// definition was bound: `None` when the root has no subagent. Only with the feature `sessions` (which `a2a` turns on).
    #[cfg(feature = "sessions")]
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
        self.rounds.register(builder)
    }

    /// The agents that work the rounds of a session on a remote subagent of the root, registered
    /// as `<root>/<subagent>`: [`register`](Self::register) registers them with the rest. A
    /// composition root that registers [`agents`](Self::agents) one by one registers these too, or
    /// `subagent_start` on a remote subagent is refused in a process that does not know the name.
    /// Only with the feature `remote`.
    #[cfg(feature = "remote")]
    pub fn remote_round_agents(&self) -> &[RemoteRoundAgent] {
        &self.rounds.agents
    }

    /// The round agents, for a dev reload that registers them as they were at startup.
    #[cfg(feature = "dev")]
    pub(crate) fn rounds(&self) -> &Rounds {
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

/// The agents that work the rounds of the sessions on remote subagents, one per remote subagent of
/// the root. Always empty without the feature `remote`, where no remote subagent binds.
#[derive(Debug, Clone, Default)]
pub(crate) struct Rounds {
    #[cfg(feature = "remote")]
    agents: Vec<RemoteRoundAgent>,
}

#[cfg(feature = "remote")]
impl Rounds {
    pub(crate) fn push(&mut self, round: RemoteRoundAgent) {
        self.agents.push(round);
    }

    pub(crate) fn extend(&mut self, other: &Self) {
        self.agents.extend(other.agents.iter().cloned());
    }

    /// Each round agent with this longest wait between two looks at its task.
    #[must_use]
    pub(crate) fn with_wait_poll(self, every: Duration) -> Self {
        let agents = self
            .agents
            .into_iter()
            .map(|round| round.with_wait_poll(every))
            .collect();
        Self { agents }
    }

    /// Register each round agent on a runtime builder.
    pub(crate) fn register(&self, builder: RuntimeBuilder) -> RuntimeBuilder {
        self.agents
            .iter()
            .fold(builder, |builder, round| builder.agent(round.clone()))
    }
}

/// Without the feature `remote` there is never a round agent: each of these does nothing.
#[cfg(not(feature = "remote"))]
impl Rounds {
    pub(crate) fn extend(&mut self, _other: &Self) {}

    #[must_use]
    pub(crate) fn with_wait_poll(self, _every: Duration) -> Self {
        self
    }

    pub(crate) fn register(&self, builder: RuntimeBuilder) -> RuntimeBuilder {
        builder
    }
}
