//! The screen's own source, for an agent with no thread tools (an embedded agent, adam ADR 0047):
//! it offers no tool, adds the "Mentioned agents" block a message asks for and describes `show` with
//! the components of the screen's catalog this process holds. A build with thread tools does the
//! same in `ThreadTools` (feature `thread-tools`), so an agent registers one of the two, never both.

use std::sync::Arc;

use adam_llm_agent::{Declared, SourceCtx, ToolCtx, ToolError, ToolOutput, ToolSource};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::Value;

use crate::mentions::mentioned_agents;
use crate::resolve::UiState;
use crate::show::{SHOW, describe_show};

/// What [`Ui::screen`](crate::Ui::screen) gives: the mentioned agents and the description of
/// `show`, with no tool of its own and nothing read from anywhere.
#[derive(Clone)]
pub struct ScreenSource {
    ui: Arc<UiState>,
}

impl std::fmt::Debug for ScreenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenSource").finish_non_exhaustive()
    }
}

impl ScreenSource {
    pub(crate) fn new(ui: Arc<UiState>) -> Self {
        Self { ui }
    }
}

/// `show` described with the components of the screen's catalog, when this process holds it. Only
/// the conversation's own catalog describes it: a model turn asks nobody for it. With none (or before
/// a tool has read it) the description stays as the tool made it, which says to call `ui_catalog`,
/// and that call holds it from then on. Shared by this source and [`ThreadTools`'s][tt].
///
/// [tt]: https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-ui/src/thread_tools.rs
pub(crate) fn describe_show_in(ui: &UiState, ctx: &SourceCtx, specs: &mut [ToolSpec]) {
    let Some(show) = specs.iter_mut().find(|spec| spec.name == SHOW) else {
        return;
    };
    if let Some(catalog) = ui.current_if_held(ctx.context_map()) {
        show.description = describe_show(&catalog);
    }
}

#[async_trait]
impl ToolSource for ScreenSource {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// It offers no tool: the mentioned agents are instructions and `show`'s description is a
    /// refinement of a tool the agent already has. `All` of nothing, so it never makes a session's
    /// set of tools incomplete (ADR 0054 decision 4).
    fn declared(&self) -> Declared {
        Declared::All(Vec::new())
    }

    async fn instructions(&self, ctx: &SourceCtx) -> Option<String> {
        mentioned_agents(ctx.context_map())
    }

    async fn refine(&self, ctx: &SourceCtx, specs: &mut [ToolSpec]) {
        describe_show_in(&self.ui, ctx, specs);
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        None
    }
}
