//! The agent a process serves, put together from its folder: the card, the MCP servers, the
//! tools, the model, and the registration `adam_service::serve` hands to the runtime.

use adam::mcp::McpPolicy;
use adam::{AgentDef, Assembly};
use adam_a2a::AgentCardConfig;
use adam_llm_agent::{LlmStarter, StepIo};
use adam_model::DynModel;
use adam_service::{Agents, RuntimeOptions};
use adam_ui::Ui;
use url::Url;

use crate::error::AgentError;

/// The version a card advertises: this crate's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The A2A card the files declare, with `public_url` where clients POST JSON-RPC: the card
/// [`Assembly::card`](adam::Assembly::card) gives for the agent assembled from them, with no model
/// or tools.
///
/// # Errors
///
/// [`AgentError::Card`] when the folder declares neither `description` nor `card.description`.
pub fn card_of(def: &AgentDef, public_url: &Url) -> Result<AgentCardConfig, AgentError> {
    def.card(public_url.clone(), VERSION)
        .map(|card| {
            // The screen's extensions, `steps/v1` (every tool call is reported as a step to a client
            // that activates it) and `text-stream/v1` (the model's answers are sent as it writes
            // them, to a client that activates it).
            adam_ui::with_card_extensions(card)
                .with_extension(adam_a2a::ExtensionConfig::steps())
                .with_extension(adam_a2a::ExtensionConfig::text_stream())
        })
        .map_err(|e| AgentError::Card(Box::new(e)))
}

/// What a role that runs workers needs besides the files.
pub struct WorkerParts<'a> {
    /// The model client.
    pub model: DynModel,
    /// The model alias the agent asks for (`MODEL`).
    pub alias: &'a str,
    /// What the folder's MCP servers may be.
    pub mcp: McpPolicy,
    /// How the runtime that steps the runs is set up.
    pub options: RuntimeOptions,
    /// How the step of a tool call reports its input and output: the secrets of this process
    /// scrubbed from both ([`redact::step_io`](crate::redact::step_io)), then the contract's cuts.
    /// [`StepIo::default`] sends both unscrubbed.
    pub step_io: StepIo,
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
/// (the secrets to scrub from them: [`redact::step_io`](crate::redact::step_io)).
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
        .step_io(step_io);
    bound
        .model(model, alias)
        .map_err(|e| AgentError::Assembly(Box::new(e)))
}

/// The agent of `def` as [`Agents`], for [`adam_service::serve`] or a composition of your own:
///
/// * with `workers`, the whole agent (the root and its subagents, [`assemble`]d), registered on the
///   runtime;
/// * without, the start-only half of the root (name and `init`), which a control plane registers:
///   it starts, delivers to, cancels and views runs and needs no model, tools or MCP server.
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
    let agents = match workers {
        Some(parts) => {
            let assembly =
                assemble_with(def, parts.model, parts.alias, &parts.mcp, parts.step_io).await?;
            Agents::new(name, move |builder| assembly.register(builder)).options(parts.options)
        }
        None => {
            let starter = LlmStarter::new(&name);
            Agents::new(name, move |builder| builder.starter(starter))
        }
    };
    // The person's screen is the sender: their answers through a form, and the catalog and the tools of
    // the conversation, reach the run (the extensions the card lists).
    Ok(agents
        .card_if(card)
        .inbound(adam_a2a_runtime::vymalo_inbound))
}
