//! The screen's UI catalog as model tools for an adam-rs agent.
//!
//! The orchestration layer's chat tells an agent which components its screen can draw (the **UI
//! catalog**, `docs/api/ui-catalog-v1.md` of `vymalo/another-agentic-system`) and gives it one MCP
//! endpoint for the conversation (**thread tools**, `docs/api/thread-tools-v1.md`). This crate turns
//! both into what a model uses:
//!
//! | You get | What it is |
//! |---|---|
//! | [`AskUser`] (`ask_user { question, title?, description?, choices?, freeInput?, submitLabel? }`) | the plain question every agent has; with `choices`, one form (a title, why the agent asks, questions of type radio, checkboxes, a searchable list, a deck of pictures or a typed answer, options with a label, a description and a recommended mark) and the person's answers as the result; the subset an older screen has, or text, when the screen cannot draw it |
//! | [`Show`] (`show { blocks, title? }`) | draws blocks of the screen's components, each checked against the catalog's schema; the A2UI surface goes out as a run artifact |
//! | [`UiCatalogTool`] (`ui_catalog {}`) | the components, what they are for, and their schemas |
//! | [`PlanWork`], [`PlanCheck`], [`PlanAdd`] (`plan_work { title?, items }`, `plan_check { id, status, note? }`, `plan_add { items, after? }`) | the conversation's plan: a checklist the agent keeps and the person sees; each call returns the whole plan and draws it as one `Checklist` (or lines of `Text`) when the screen can |
//! | [`ThreadTools`] (a [`ToolSource`](adam_llm_agent::ToolSource)) | every tool the thread-tools endpoint lists, under its listed name, read once per grant (a turn reuses the listing; a new grant, a refused call or [`LISTING_TTL`] lists again); a successful `turn_output { text }` makes `text` the run's answer; a tool the orchestrator reports as a step (`reportsStep`) gets no step of the agent's, a call waits as long as the tool's `timeoutSecs` (capped by `THREAD_TOOLS_MAX_CALL_SECS`) and carries a stable `callId`; with mentions in the run's context it adds a "Mentioned agents" block to the instructions |
//! | [`card_extensions`] | the card entries that announce all this (A2UI v0.9.1, `ui-catalog/v1`, `thread-tools/v1`, `mentions/v1`, `steer/v1`) |
//!
//! A binary wires it in a few lines (the inbound function is `adam-a2a-runtime`'s `vymalo_inbound`):
//!
//! ```ignore
//! let ui = adam_ui::Ui::new(mcp_policy);
//! let tools = my_tools.extend(ui.tools());                 // ask_user, show, ui_catalog, plan_*
//! let bound = def.bind(tools)?.tool_source(ui.source());   // the thread tools, last
//! let card = adam_ui::with_card_extensions(card);          // announce them
//! let agents = Agents::new(name, register).inbound(vymalo_inbound);
//! ```
//!
//! # How a catalog reaches a tool
//!
//! A message from the screen's orchestrator carries the catalog's reference (`ui-catalog/v1`: version
//! and digest), sometimes the catalog itself (inline), and the thread-tools grant. `vymalo_inbound`
//! puts them in the run's inbound context; a tool looks at the context, then at the catalogs this
//! process already holds (by digest), and when the current one is neither inline nor held it reads it
//! again with `get_ui_catalog` over the thread tools (once; the digest of what it gets is checked).
//! The diagrams are in the [README](https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-ui/README.md#how-a-catalog-reaches-a-tool).
//!
//! A tool draws only what the renderer offered **in the message of this turn**
//! (`a2uiRendererCapabilities["1.0"]`, kept as `vymalo.a2ui.renderer`): without that entry, or with
//! one that does not list the current `catalogId`, it answers as it does with no catalog. A later
//! message with no capabilities draws nothing, whatever an earlier turn offered.
//!
//! Nothing here fails a run: with no catalog, or one that cannot be read, `ask_user` puts the options
//! in the question's text, `show` and `ui_catalog` return an error result that says to answer in text,
//! and with no grant (or an expired one) the thread tools offer nothing.

#![warn(missing_docs)]
#![forbid(unsafe_code)]
// These docs describe the thread tools; built without them (the embedded set, ADR 0047) their links to
// them have no target. The documented build, with every feature, checks every link.
#![cfg_attr(not(feature = "thread-tools"), allow(rustdoc::broken_intra_doc_links))]

mod ask;
mod cache;
mod catalog;
mod mentions;
mod plan;
mod resolve;
mod screen;
mod show;
mod surface;
#[cfg(feature = "thread-tools")]
mod thread_tools;

use std::sync::Arc;

use adam_a2a::{AgentCardConfig, ExtensionConfig};
use adam_llm_agent::ToolSet;
#[cfg(feature = "thread-tools")]
use adam_mcp::McpPolicy;

pub use ask::{ASK_USER, AskUser, DEFAULT_ASK_LEAD};
pub use cache::{CatalogCache, MAX_CACHED_CATALOGS};
pub use catalog::{
    Catalog, CatalogError, Claimed, Component, MAX_CATALOG_BYTES, MAX_COMPONENTS, canonical_json,
    catalog_digest,
};
pub use plan::{PLAN_ADD, PLAN_CHECK, PLAN_WORK, PlanAdd, PlanCheck, PlanWork};
pub use screen::ScreenSource;
pub use show::{MAX_BLOCKS, SHOW, Show, UI_CATALOG, UiCatalogTool};
pub use surface::{
    A2UI_VERSION, ask_attempt, ask_retry_surface_id, ask_surface_id, is_ask_surface_of,
};
#[cfg(feature = "thread-tools")]
pub use thread_tools::{
    Clock, GET_UI_CATALOG, LISTING_TTL, META_KEY, TURN_OUTPUT, TURN_OUTPUT_DELIVERED, ThreadTools,
    ThreadToolsClient,
};

pub use adam_a2a_runtime::{comparable, is_format_or_control};

use resolve::UiState;

/// The UI tools and the thread-tools source of one agent process, sharing the catalogs the process
/// has read.
///
/// Made once, at startup, with the deployment's [`McpPolicy`] (the thread-tools endpoint is an MCP
/// server: https, or plain `http` only to this machine unless `MCP_ALLOW_INSECURE` says otherwise,
/// and the call timeout). `Clone` shares the cache.
#[derive(Clone)]
pub struct Ui {
    state: Arc<UiState>,
    ask_lead: String,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui")
            .field("cached_catalogs", &self.state.cache.len())
            .finish()
    }
}

impl Ui {
    /// The tools and the source over a thread-tools client under `policy`.
    #[cfg(feature = "thread-tools")]
    pub fn new(policy: McpPolicy) -> Self {
        Self::with_client(ThreadToolsClient::new(policy))
    }

    /// As [`new`](Self::new), over a client you made (one with a clock of its own, in a test).
    #[cfg(feature = "thread-tools")]
    pub fn with_client(client: ThreadToolsClient) -> Self {
        Self {
            state: Arc::new(UiState::new(Arc::new(client))),
            ask_lead: DEFAULT_ASK_LEAD.to_owned(),
        }
    }

    /// The tools with no thread-tools client, for an embedded agent (adam ADR 0047) or any agent that
    /// is never sent an MCP endpoint: a catalog a message carries is used, and one that would have
    /// to be read again is not, so the agent answers in text. Available in every build.
    pub fn embedded() -> Self {
        Self {
            state: Arc::new(UiState::without_thread_tools()),
            ask_lead: DEFAULT_ASK_LEAD.to_owned(),
        }
    }

    /// Open the description of `ask_user` with `lead` (when to ask, in the agent's own words) instead
    /// of [`DEFAULT_ASK_LEAD`]. What follows it, about `choices`, is the same.
    #[must_use]
    pub fn with_ask_lead(mut self, lead: impl Into<String>) -> Self {
        self.ask_lead = lead.into();
        self
    }

    /// `ask_user`, `show`, `ui_catalog`, `plan_work`, `plan_check` and `plan_add`, in that order (the
    /// order of the specs is part of the prompt's stable prefix). Register them with the agent's own
    /// tools (`ToolSet::extend`); a folder's `tools:` can narrow them by name. `ask_user` replaces
    /// any other tool of that name an agent has: register one of them.
    pub fn tools(&self) -> ToolSet {
        ToolSet::new()
            .tool(AskUser::new(Arc::clone(&self.state), self.ask_lead.clone()))
            .tool(Show::new(Arc::clone(&self.state)))
            .tool(UiCatalogTool::new(Arc::clone(&self.state)))
            .tool(PlanWork::new(Arc::clone(&self.state)))
            .tool(PlanCheck::new(Arc::clone(&self.state)))
            .tool(PlanAdd::new(Arc::clone(&self.state)))
    }

    /// The thread tools as a tool source: every tool the endpoint of the run's messages lists, under
    /// its listed name, read once per grant, **except `get_ui_catalog`** (the model has
    /// `ui_catalog` for that: [`ThreadTools`]). The source also describes `show` with the components
    /// of the conversation's screen, so register [`tools`](Self::tools) and this source on the same
    /// agent. Add it **last** among an agent's sources.
    #[cfg(feature = "thread-tools")]
    pub fn source(&self) -> ThreadTools {
        ThreadTools::of_ui(Arc::clone(&self.state))
    }

    /// The screen's source without thread tools: the mentioned agents and `show` described with the
    /// screen's components, no tool and nothing read from anywhere. Register it, **last** among an
    /// agent's sources, on an agent that has no thread-tools source (an embedded one); never both.
    pub fn screen(&self) -> ScreenSource {
        ScreenSource::new(Arc::clone(&self.state))
    }

    /// The catalogs this process has read.
    pub fn cache(&self) -> &CatalogCache {
        &self.state.cache
    }
}

/// The card entries of an agent that draws on a screen: A2UI v0.9.1 (with `acceptsInlineCatalogs`),
/// `ui-catalog/v1`, `thread-tools/v1`, `mentions/v1` and `steer/v1`. All optional, no parameters beyond A2UI's own; a client
/// that does not know one ignores it.
pub fn card_extensions() -> Vec<ExtensionConfig> {
    vec![
        ExtensionConfig::a2ui_v0_9_1(),
        ExtensionConfig::ui_catalog(),
        ExtensionConfig::thread_tools(),
        ExtensionConfig::mentions(),
        ExtensionConfig::steer(),
    ]
}

/// `card` with [`card_extensions`] declared.
pub fn with_card_extensions(card: AgentCardConfig) -> AgentCardConfig {
    card_extensions()
        .into_iter()
        .fold(card, AgentCardConfig::with_extension)
}
