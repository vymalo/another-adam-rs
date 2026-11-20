//! `use_server`: the fallback that loads one MCP server's tools **during** a run.
//!
//! A server whose `when` the task did not match is withheld from the run
//! ([`mcp_select`](crate::mcp_select)). The model may still need it, and the only way it can say so
//! is a tool: this one, a fixed tool that every agent with a triggered server has from its first
//! request, whatever the folder declares (`use_server { name }`, no per-turn listing, so the tool
//! list is the same bytes until something is loaded).
//!
//! The load is **state, not a wire edit**: the result carries
//! [`ToolOutput::loaded_servers`](adam_llm_agent::ToolOutput::loaded_servers), the loop makes it the
//! run's list (`Conversation::loaded_servers`), and from the next model call on the server stops
//! being withheld and its tools are appended after every other tool of the request. That append
//! costs one full history re-prefill — the provider's cache ends where the tools array grew — which
//! the result says in words and the run records in `Conversation::tools_appended`. The tool invents
//! no number: what the re-prefill cost is the next call's `Usage::cached_input_tokens`, which only
//! the provider reports.
//!
//! A load is refused, and changes nothing, when the name is not a server of the folder, when the
//! server has no `when` (it is eager, its tools are already there), when it is already loaded, or
//! when it offered no tool at bind (an `optional` server that was not reachable, or an allow-list
//! that selects nothing): each refusal is a result the model can act on.

use adam_llm_agent::{StepStyle, Tool, ToolAccess, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::mcp_select::Triggered;

/// The tool's name, as the model calls it. It is a reserved name: a folder whose tools already
/// have it is refused at bind, as a folder with a skill tool of that name is.
pub(crate) const USE_SERVER: &str = "use_server";

/// What the tool says it does. One fixed text whatever servers exist, so the tool list of a folder
/// is the same bytes at every turn.
const DESCRIPTION: &str = "Load an MCP server's tools that this run does not have yet. The server \
    is named in the agent's mcp.json with a `when`: its tools are offered only when the task \
    mentions one of its keywords, and this call is how you ask for one anyway. The tools are \
    available from your next call, and they are added after the ones you have. The call costs one \
    full re-read of the prompt, so load a server only when you need its tools, and once: a server \
    already loaded, or one that is always loaded, is refused. Example: {\"name\":\"linear\"}";

/// What the model is told when it calls a server that offered no tool at bind.
fn unavailable(name: &str) -> String {
    format!(
        "The MCP server `{name}` offered no tools when this agent started, so there is nothing to \
         load: it may not have been reachable, or neither its `tools:` allow-list nor this agent's \
         selects any of its tools. Do not call it again; use what you have."
    )
}

/// The fixed `use_server` tool of one agent.
pub(crate) struct UseServer {
    /// Every server the agent's `mcp.json` declares, in name order: what an unknown name is
    /// answered with, and what tells an eager server from a triggered one.
    declared: Vec<String>,
    /// The triggered servers, in name order, with the tools a load makes available.
    deferred: Vec<Triggered>,
}

impl UseServer {
    pub(crate) fn new(declared: Vec<String>, deferred: Vec<Triggered>) -> Self {
        Self { declared, deferred }
    }
}

#[async_trait]
impl Tool for UseServer {
    /// Loads state; it touches no file.
    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    /// A person watching sees the load as its own step; its output is the result, which says the
    /// next call re-reads the whole prompt.
    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("Load an MCP server")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: USE_SERVER.to_owned(),
            description: DESCRIPTION.to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The server's name, as its tools' names begin: `linear` for `linear__list_issues`."
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return Ok(ToolOutput::error(
                "`use_server` takes a string argument `name`: the MCP server to load.",
            ));
        };
        let Some(server) = self.deferred.iter().find(|server| server.server == name) else {
            if self.declared.iter().any(|declared| declared == name) {
                return Ok(ToolOutput::error(format!(
                    "The MCP server `{name}` has no `when`: it is always loaded, and its tools are \
                     already in your tool list. There is nothing to load."
                )));
            }
            return Ok(ToolOutput::error(format!(
                "There is no MCP server `{name}` in this agent's mcp.json. The servers it declares \
                 are: {}.",
                self.declared.join(", ")
            )));
        };
        if !server.available() {
            return Ok(ToolOutput::error(unavailable(name)));
        }
        let already = if server.matches(ctx.task()) {
            Some(
                "the task mentions one of its keywords, so it has been loaded since the first call",
            )
        } else if ctx.loaded_servers().iter().any(|loaded| loaded == name) {
            Some("this run loaded it already")
        } else {
            None
        };
        if let Some(why) = already {
            return Ok(ToolOutput::error(format!(
                "The MCP server `{name}` is already loaded ({why}): its tools are available ({}). \
                 Do not load it again.",
                server.tools().collect::<Vec<_>>().join(", ")
            )));
        }
        let mut loaded: Vec<String> = ctx.loaded_servers().to_vec();
        loaded.push(name.to_owned());
        let tools = server.tools().collect::<Vec<_>>().join(", ");
        Ok(ToolOutput::text(format!(
            "Loaded the MCP server `{name}`. Its tools are available from your next call, added \
             after the ones you have: {tools}.\n\
             That call re-reads the whole prompt, because the tool list grew: it is slower than \
             the calls before it."
        ))
        .with_loaded_servers(loaded))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_runtime::NoopSink;

    use super::*;

    fn triggered(server: &str, keywords: &[&str], tools: &[&str]) -> Triggered {
        Triggered::new(
            server.to_owned(),
            keywords.iter().map(|word| (*word).to_owned()).collect(),
            tools
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_owned(),
                    description: format!("the {name} tool"),
                    parameters: json!({"type": "object", "properties": {}}),
                })
                .collect(),
            None,
        )
    }

    fn tool() -> UseServer {
        UseServer::new(
            vec!["github".to_owned(), "linear".to_owned(), "docs".to_owned()],
            vec![
                triggered("linear", &["linear", "ticket"], &["linear__list_issues"]),
                triggered("docs", &["docs"], &["docs__search"]),
            ],
        )
    }

    fn ctx(task: Option<&str>, loaded: &[&str]) -> ToolCtx {
        ToolCtx::detached("use_server", "c1", Arc::new(NoopSink))
            .with_task(task.map(Arc::from))
            .with_loaded_servers(loaded.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[tokio::test]
    async fn a_load_names_the_tools_and_records_the_whole_list() {
        let out = tool()
            .call(&ctx(Some("fix a typo"), &[]), json!({"name": "docs"}))
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("docs__search"), "{}", out.content);
        assert!(out.content.contains("re-reads"), "{}", out.content);
        assert!(!out.content.contains("token"), "{}", out.content);
        assert_eq!(out.loaded_servers, ["docs"]);
        let again = tool()
            .call(
                &ctx(Some("fix a typo"), &["docs"]),
                json!({"name": "linear"}),
            )
            .await
            .unwrap();
        assert_eq!(again.loaded_servers, ["docs", "linear"]);
    }

    #[tokio::test]
    async fn an_unknown_server_is_refused_and_names_the_ones_declared() {
        let out = tool()
            .call(&ctx(Some("fix a typo"), &[]), json!({"name": "nope"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(
            out.content.contains("no MCP server `nope`"),
            "{}",
            out.content
        );
        assert!(out.content.contains("github"), "{}", out.content);
        assert!(out.loaded_servers.is_empty());
    }

    #[tokio::test]
    async fn an_eager_server_is_refused() {
        let out = tool()
            .call(&ctx(Some("fix a typo"), &[]), json!({"name": "github"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("always loaded"), "{}", out.content);
        assert!(out.loaded_servers.is_empty());
    }

    #[tokio::test]
    async fn a_server_the_task_loaded_or_the_run_loaded_is_refused() {
        let out = tool()
            .call(
                &ctx(Some("fix the linear ticket"), &[]),
                json!({"name": "linear"}),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("already loaded"), "{}", out.content);
        assert!(
            out.content.contains("linear__list_issues"),
            "{}",
            out.content
        );
        assert!(out.loaded_servers.is_empty());

        let out = tool()
            .call(&ctx(Some("fix a typo"), &["docs"]), json!({"name": "docs"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("already loaded"), "{}", out.content);
        assert!(out.loaded_servers.is_empty());
    }

    #[tokio::test]
    async fn a_server_that_offered_no_tools_is_refused() {
        let tool = UseServer::new(
            vec!["docs".to_owned()],
            vec![triggered("docs", &["docs"], &[])],
        );
        let out = tool
            .call(&ctx(Some("fix a typo"), &[]), json!({"name": "docs"}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("offered no tools"), "{}", out.content);
        assert!(out.loaded_servers.is_empty());
    }

    #[tokio::test]
    async fn a_call_with_no_name_is_refused() {
        let out = tool()
            .call(&ctx(Some("fix a typo"), &[]), json!({}))
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("`name`"), "{}", out.content);
    }
}
