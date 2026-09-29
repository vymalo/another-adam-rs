//! Helpers shared by the integration tests of `adam-mcp`.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use adam_agent_fs::McpConfig;
use adam_llm_agent::{ToolCtx, ToolOutput, ToolSet};
use adam_runtime::{CancelToken, NoopSink};
use serde_json::Value;

/// The config a JSON text parses to (`mcp.json`, as the loader reads it).
pub fn config(json: &str) -> McpConfig {
    let mut diagnostics = Vec::new();
    let config = adam_agent_fs::parse_mcp(Path::new("mcp.json"), json, &mut diagnostics)
        .unwrap_or_else(|| panic!("mcp.json did not parse: {diagnostics:?}"));
    assert!(
        diagnostics
            .iter()
            .all(|d| d.severity != adam_agent_fs::Severity::Error),
        "{diagnostics:?}"
    );
    config
}

/// A config with one streamable-HTTP server `name` at `url`, with `extra` JSON members added to
/// the server (`"tools": ["echo"]`, a `"headers": {..}`).
pub fn http_config(name: &str, url: &str, extra: &str) -> McpConfig {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(", {extra}")
    };
    config(&format!(
        r#"{{"mcpServers": {{"{name}": {{"type": "http", "url": "{url}"{extra}}}}}}}"#
    ))
}

/// A context detached from any run.
pub fn ctx(tool: &str) -> ToolCtx {
    ToolCtx::detached(tool, "c1", Arc::new(NoopSink))
}

/// A context whose run is cancelled when `token` is.
pub fn cancellable(tool: &str, token: &CancelToken) -> ToolCtx {
    ctx(tool).with_cancel_token(token.clone())
}

/// Call `name` of `tools` with `args`; an `Err` of the tool fails the test (an MCP tool answers
/// every problem with an error *result*).
pub async fn call(tools: &ToolSet, name: &str, args: Value) -> ToolOutput {
    let tool = tools
        .get(name)
        .unwrap_or_else(|| panic!("no tool `{name}` in {:?}", tools.names()));
    tool.call(&ctx(name), args)
        .await
        .unwrap_or_else(|e| panic!("`{name}` failed with an Err: {e}"))
}
