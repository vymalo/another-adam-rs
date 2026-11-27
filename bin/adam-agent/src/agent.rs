//! The agent a process serves, put together from its folder: the card, the MCP servers, the
//! tools, the model, and the registration `adam_service::serve` hands to the runtime.

use adam::mcp::McpPolicy;
use adam::{AgentDef, AgentFolder, Assembly};
use adam_a2a::AgentCardConfig;
use adam_llm_agent::{DEFAULT_MAX_PARALLEL, LlmStarter, ModelParking, StepIo};
use adam_model::{DynModel, ModelProfiles};
use adam_service::{Agents, RuntimeOptions};
use adam_ui::Ui;
use url::Url;

use crate::error::AgentError;

/// This crate's version, the base of the version a card advertises ([`build_version`]).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The revision this binary was built from: the build argument `ADAM_BUILD_REVISION` of the image
/// (the commit), `None` for a build that was not given one (a local `cargo build`).
pub const BUILD_REVISION: Option<&str> = option_env!("ADAM_BUILD_REVISION");

/// The version a card advertises: [`VERSION`] and the build's revision as semver build metadata,
/// `0.1.0+6478fbc`, or `0.1.0+unknown` when the build was not given one.
pub fn build_version() -> String {
    adam_a2a::build_version(VERSION, BUILD_REVISION)
}

/// The A2A card the files declare, with `public_url` where clients POST JSON-RPC: the card
/// [`Assembly::card`](adam::Assembly::card) gives for the agent assembled from them, with no model
/// or tools. Its `version` carries the build's revision ([`build_version`]); the `build/v1`
/// extension, which also says the digest of the files, is [`card_of_folder`]'s, because a
/// definition does not know the digest of the folder it came from.
///
/// # Errors
///
/// [`AgentError::Card`] when the folder declares neither `description` nor `card.description`.
pub fn card_of(def: &AgentDef, public_url: &Url) -> Result<AgentCardConfig, AgentError> {
    def.card(public_url.clone(), build_version())
        .map(with_extensions)
        .map_err(|e| AgentError::Card(Box::new(e)))
}

/// The screen's extensions, `steps/v1` (every tool call is reported as a step to a client that
/// activates it), `text-stream/v1` (the model's answers are sent as it writes them, to a client
/// that activates it) and `usage/v1` (the tokens of every model call, to a client that activates it,
/// and a task's totals on the task).
fn with_extensions(card: AgentCardConfig) -> AgentCardConfig {
    adam_ui::with_card_extensions(card)
        .with_extension(adam_a2a::ExtensionConfig::steps())
        .with_extension(adam_a2a::ExtensionConfig::text_stream())
        .with_extension(adam_a2a::ExtensionConfig::usage())
}

/// [`card_of`] for the folder the process was started with: the same card, and `build/v1` saying
/// the build's revision and the folder's digest, so that an export of a thread can tell which build
/// and which files answered.
///
/// # Errors
///
/// As [`card_of`].
pub fn card_of_folder(
    folder: &AgentFolder,
    public_url: &Url,
) -> Result<AgentCardConfig, AgentError> {
    card_for_build(folder, public_url, BUILD_REVISION)
}

/// [`card_of_folder`] for a build of revision `revision`.
fn card_for_build(
    folder: &AgentFolder,
    public_url: &Url,
    revision: Option<&str>,
) -> Result<AgentCardConfig, AgentError> {
    let version = adam_a2a::build_version(VERSION, revision);
    let card = folder
        .def
        .card(public_url.clone(), version)
        .map_err(|e| AgentError::Card(Box::new(e)))?;
    Ok(
        with_extensions(card).with_extension(adam_a2a::ExtensionConfig::build(
            revision,
            folder.digest.as_str(),
        )),
    )
}

/// What a role that runs workers needs besides the files.
pub struct WorkerParts<'a> {
    /// The model client.
    pub model: DynModel,
    /// The model alias the agent asks for (`MODEL`).
    pub alias: &'a str,
    /// The deployment's per-model profiles (`MODEL_ALIASES_FILE`, ADR 0063/0064): what each model
    /// takes as a reasoning effort, and the presets an agent's `model_preset` may name. Empty when
    /// the file is unset: no model has a profile, so an agent that sets an effort is refused.
    pub profiles: ModelProfiles,
    /// What the folder's MCP servers may be.
    pub mcp: McpPolicy,
    /// How the runtime that steps the runs is set up.
    pub options: RuntimeOptions,
    /// How the step of a tool call reports its input and output: the secrets of this process
    /// scrubbed from both ([`redact::step_io`](crate::redact::step_io)), then the contract's cuts.
    /// [`StepIo::default`] sends both unscrubbed.
    pub step_io: StepIo,
    /// The most tool calls of one model turn that run at the same time (`MAX_PARALLEL_TOOL_CALLS`,
    /// 1 to 32): an agent file's `limits.max_parallel` may lower it for its agent.
    pub max_parallel: usize,
    /// How every agent waits on a model that refused it (ADR 0067): `MODEL_PARK_LIMIT`, and whether
    /// an unreachable endpoint parks (`MODEL_AUTH=injector`).
    pub parking: ModelParking,
}

/// Assemble the agent of `def` for a role that runs workers: connect the MCP servers of its
/// `mcp.json` (and each subagent's), bind the tools (`ask_user`, the tools of those servers, the
/// skills' tools and one per subagent), and give the root and each subagent the model.
///
/// # Errors
///
/// [`AgentError::Mcp`] for a server that is down or that the policy refuses, and
/// [`AgentError::Assembly`] when the files and the code disagree: an unknown tool in `tools:`, a
/// var declared without a value (nothing supplies values here), a placeholder `vars` does not
/// declare, or a model alias the assembly refuses.
pub async fn assemble(
    def: AgentDef,
    model: DynModel,
    alias: &str,
    mcp: &McpPolicy,
) -> Result<Assembly, AgentError> {
    assemble_with(def, model, alias, mcp, StepIo::default()).await
}

/// [`assemble`], with `step_io` saying how the step of each tool call reports its input and output
/// (the secrets to scrub from them: [`redact::step_io`](crate::redact::step_io)). The agents run
/// [`DEFAULT_MAX_PARALLEL`] tool calls of a turn at once.
///
/// # Errors
///
/// As [`assemble`].
pub async fn assemble_with(
    def: AgentDef,
    model: DynModel,
    alias: &str,
    mcp: &McpPolicy,
    step_io: StepIo,
) -> Result<Assembly, AgentError> {
    assemble_limited(def, model, alias, mcp, step_io, DEFAULT_MAX_PARALLEL).await
}

/// [`assemble_with`], with `max_parallel` the most tool calls of one model turn that run at the same
/// time for every agent (`MAX_PARALLEL_TOOL_CALLS`, 1 to 32); an agent file's `limits.max_parallel`
/// may lower it for that agent, never raise it.
///
/// # Errors
///
/// As [`assemble`].
pub async fn assemble_limited(
    def: AgentDef,
    model: DynModel,
    alias: &str,
    mcp: &McpPolicy,
    step_io: StepIo,
    max_parallel: usize,
) -> Result<Assembly, AgentError> {
    assemble_limited_with_profiles(
        def,
        model,
        alias,
        mcp,
        step_io,
        max_parallel,
        ModelProfiles::default(),
    )
    .await
}

/// [`assemble_limited`], with the deployment's per-model profiles (`MODEL_ALIASES_FILE`, ADR
/// 0063/0064): `BoundDef::model_profiles` resolves each agent's default reasoning effort from its
/// folder, its `model_preset` and the profile, and refuses a value the model does not take. A role
/// that runs workers passes its `ModelConfig`'s profiles here.
///
/// # Errors
///
/// As [`assemble`], and [`AgentError::Assembly`] when an agent's `reasoning_effort` or
/// `model_preset` is not one its model's profile allows.
pub async fn assemble_limited_with_profiles(
    def: AgentDef,
    model: DynModel,
    alias: &str,
    mcp: &McpPolicy,
    step_io: StepIo,
    max_parallel: usize,
    profiles: ModelProfiles,
) -> Result<Assembly, AgentError> {
    assemble_parked(
        def,
        model,
        alias,
        mcp,
        step_io,
        max_parallel,
        profiles,
        ModelParking::default(),
    )
    .await
}

/// [`assemble_limited_with_profiles`], with how every agent waits on a model that refused it
/// (`MODEL_PARK_LIMIT` and `MODEL_AUTH=injector`, ADR 0067).
#[allow(clippy::too_many_arguments)] // the parts of one assembly, each a setting of the process
async fn assemble_parked(
    def: AgentDef,
    model: DynModel,
    alias: &str,
    mcp: &McpPolicy,
    step_io: StepIo,
    max_parallel: usize,
    profiles: ModelProfiles,
    parking: ModelParking,
) -> Result<Assembly, AgentError> {
    // The servers are connected now, at startup, before the agent is bound: a server that is down,
    // a local process the policy does not allow, a `${VAR}` that is unset are startup errors with
    // their own exit code, never something found in the middle of a run.
    let def = def
        .connect_mcp(mcp)
        .await
        .map_err(|e| AgentError::Mcp(Box::new(e)))?;
    // The person's screen as tools (`ask_user` with choices, `show`, `ui_catalog`) and the tools of the
    // conversation's endpoint, offered at every model turn. The endpoint is an MCP server: the
    // deployment's policy decides whether its URL may be plain `http`.
    let ui = Ui::new(mcp.clone());
    let bound = def
        .bind(ui.tools())
        .map_err(|e| AgentError::Assembly(Box::new(e)))?
        .tool_source(ui.source())
        .max_parallel(max_parallel)
        .model_parking(parking)
        .step_io(step_io);
    bound
        .model_profiles(profiles)
        // A served agent takes the model a message picks (`model-pick/v1`): the card lists the
        // choices and the backend refuses anything outside them (ADR 0064, amended 2026-11-23).
        .allow_model_choice()
        .model(model, alias)
        .map_err(|e| AgentError::Assembly(Box::new(e)))
}

/// The agent of `def` as [`Agents`], for [`adam_service::serve`] or a composition of your own:
///
/// * with `workers`, the whole agent (the root and its subagents, [`assemble`]d), registered on the
///   runtime;
/// * without, the start-only half of the root (name and `init`) and of each of its helpers, which a
///   control plane registers: it starts, delivers to, cancels and views runs and needs no model,
///   tools or MCP server.
///
/// A root with a subagent lists `child-sessions/v1` on the card `serve` serves (the helpers and the
/// effective session limits of `def`).
///
/// `card` is what the A2A server serves; a role that serves A2A needs one.
///
/// # Errors
///
/// As [`assemble`], for a role that runs workers.
pub async fn agents(
    def: AgentDef,
    card: Option<AgentCardConfig>,
    workers: Option<WorkerParts<'_>>,
) -> Result<Agents, AgentError> {
    let name = def.name().to_owned();
    // The commands the card lists (`commands/v1`): the backend expands a message that names one.
    let commands = adam_a2a_runtime::Commands::from_manifest(&def.manifest().commands);
    // The helpers a person may start a session with (`child-sessions/v1`), from the folder alone and
    // with the effective limits, so every role that serves A2A lists and serves the same.
    let sessions = def.child_sessions();
    let schedules = def.manifest().schedules.clone();
    // The largest upload the card lists (`uploads/v1`): the folder's `limits.max_inline_upload_bytes`;
    // without it the environment's and then the default are used by `serve`.
    let uploads_inline_max_bytes = def
        .manifest()
        .frontmatter
        .limits
        .as_ref()
        .and_then(|limits| limits.max_inline_upload_bytes);
    let (agents, commands, card, choices, efforts, session_bounds) = match workers {
        Some(parts) => {
            let profiles = parts.profiles;
            let assembly = assemble_parked(
                def,
                parts.model,
                parts.alias,
                &parts.mcp,
                parts.step_io,
                parts.max_parallel,
                profiles.clone(),
                parts.parking,
            )
            .await?;
            // The reasoning levels of each agent's model (`reasoning/v1`): what the backend checks
            // a message's effort against, and the root's the card lists.
            let efforts = adam_a2a::ReasoningEfforts::from_models(
                assembly.info().iter().map(|info| {
                    (
                        info.name.as_str(),
                        info.model_alias.as_str(),
                        info.reasoning_effort,
                    )
                }),
                &profiles,
            );
            // The models a message may pick (`model-pick/v1`): the root's choices, the same list
            // its card advertises, each with its endpoint label when the deployment names one.
            let choices = assembly
                .info()
                .first()
                .map(|root| {
                    adam_a2a::ModelChoices::from_profiles(root.model_choices.clone(), &profiles)
                })
                .filter(|choices| !choices.is_empty());
            // The prompts of the root's MCP servers are commands too, known only now that the
            // servers are connected: a role without workers connects none, so it serves none.
            let (commands, listed) =
                adam_service::with_prompt_commands(commands, assembly.prompt_commands());
            let card = card
                .map(|card| adam_service::card_with_commands(card, &listed))
                // The card says the levels the backend reads; a card that already lists
                // `reasoning/v1` keeps its own entry.
                .map(|card| adam_service::card_with_reasoning(card, &efforts, &name));
            // What a person's `start.config` is held to: the bounds the session tools use, so the
            // two cannot disagree (ADR 0054 decision 2).
            let session_bounds = assembly.session_bounds().to_vec();
            let root = name.clone();
            let scheduler = adam_schedule::SchedulerAgent::new(&name);
            (
                // The agent's scheduler steps the folder's schedules beside it (ADR 0060), and the
                // `schedules` component makes its scheduler runs match the folder at start: with none,
                // it cancels what schedules a folder dropped.
                Agents::new(name, move |builder| {
                    assembly.register(builder).agent(scheduler)
                })
                .options(parts.options)
                .worker_component("schedules", move |store, stop| async move {
                    adam_schedule::reconcile_until_stopped(store, root, schedules, stop)
                        .await
                        .map_err(Into::into)
                }),
                commands,
                card,
                choices,
                Some(efforts),
                session_bounds,
            )
        }
        None => {
            let starter = LlmStarter::new(&name);
            let helpers = def.clone();
            // A control plane connects no MCP server, so it cannot expand a prompt: a message that
            // names the prompt of one of the folder's servers is refused, not sent to the model as
            // plain text.
            let commands =
                adam_service::refuse_prompt_commands(commands, def.manifest().mcp.as_ref());
            (
                // The helpers' starters too, so a person's session round can be started here and a
                // worker steps it.
                Agents::new(name, move |builder| {
                    helpers.session_starters(builder.starter(starter))
                }),
                commands,
                card,
                // A control plane assembles no agent, so it knows no choices and no levels: its
                // card lists neither.
                None,
                None,
                // A control plane binds no agent, so it knows no bounds: it refuses a
                // `start.config`.
                Vec::new(),
            )
        }
    };
    // The person's screen is the sender: their answers through a form, and the catalog and the tools of
    // the conversation, reach the run (the extensions the card lists).
    let agents = agents
        .card_if(card)
        .inbound(adam_a2a_runtime::vymalo_inbound)
        .commands(commands);
    let agents = match uploads_inline_max_bytes {
        Some(bytes) => agents.uploads_inline_max_bytes(bytes),
        None => agents,
    };
    let agents = match choices {
        Some(choices) => agents.model_choices(choices),
        None => agents,
    };
    let agents = match efforts {
        Some(efforts) => agents.reasoning_efforts(efforts),
        None => agents,
    };
    Ok(match sessions {
        Some(sessions) => agents.child_sessions(sessions.with_bounds(session_bounds)),
        None => agents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_says_which_build_and_which_folder_answer() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        std::fs::create_dir_all(tmp.path().join("agent")).expect("the agent directory");
        std::fs::write(
            tmp.path().join("agent/instructions.md"),
            "---\nname: chat\ndescription: Chats.\n---\nHello.\n",
        )
        .expect("the instructions");
        let folder = AgentFolder::load(tmp.path()).expect("the folder loads");
        let url: Url = "https://agents.example.com/chat/".parse().expect("a URL");

        let sha = "6478fbc1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7";
        let card = card_for_build(&folder, &url, Some(sha)).expect("a card");
        assert_eq!(card.version, format!("{VERSION}+6478fbc"));
        let build = card
            .extensions
            .iter()
            .find(|e| e.uri == adam_a2a::BUILD_EXTENSION)
            .expect("the card declares build/v1");
        assert!(!build.required);
        assert_eq!(build.params["revision"], sha);
        assert_eq!(build.params["folderDigest"], folder.digest.as_str());

        let card = card_for_build(&folder, &url, None).expect("a card");
        assert_eq!(card.version, format!("{VERSION}+unknown"));

        // Another prompt is another folder digest on the same build.
        std::fs::write(
            tmp.path().join("agent/instructions.md"),
            "---\nname: chat\ndescription: Chats.\n---\nHello again.\n",
        )
        .expect("the instructions");
        let edited = AgentFolder::load(tmp.path()).expect("the folder loads");
        let again = card_for_build(&edited, &url, Some(sha)).expect("a card");
        let digest = |card: &AgentCardConfig| {
            card.extensions
                .iter()
                .find(|e| e.uri == adam_a2a::BUILD_EXTENSION)
                .map(|e| e.params["folderDigest"].clone())
        };
        assert_ne!(
            digest(&again),
            digest(&card_for_build(&folder, &url, Some(sha)).expect("a card"))
        );
    }
}
