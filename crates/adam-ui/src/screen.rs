//! The screen's own source, for an agent with no thread tools (an embedded agent, adam ADR 0047):
//! it offers no tool, adds the "Mentioned agents" block a message asks for and describes `show` with
//! the components of the screen's catalog this process holds. A build with thread tools does the
//! same in [`ThreadTools`](crate::ThreadTools), so an agent registers one of the two, never both.

use std::sync::Arc;

use adam_llm_agent::{SourceCtx, ToolCtx, ToolError, ToolOutput, ToolSource};
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

#[async_trait]
impl ToolSource for ScreenSource {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn instructions(&self, ctx: &SourceCtx) -> Option<String> {
        mentioned_agents(ctx.context_map())
    }

    async fn refine(&self, ctx: &SourceCtx, specs: &mut [ToolSpec]) {
        let Some(show) = specs.iter_mut().find(|spec| spec.name == SHOW) else {
            return;
        };
        if let Some(catalog) = self.ui.current_if_held(ctx.context_map()) {
            show.description = describe_show(&catalog);
        }
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
