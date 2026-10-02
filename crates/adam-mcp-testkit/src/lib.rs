//! A scriptable MCP server for testing MCP clients, over stdio (the binary
//! `adam-mcp-test-server`) and over streamable HTTP ([`TestHttpServer`]).
//!
//! Both serve the same [`TestServer`], whose tools are made to be asserted on:
//!
//! | Tool | Arguments | Answers |
//! |---|---|---|
//! | `echo` | `text` | the text |
//! | `fail` | none | an error result (`isError`) saying `failed on purpose` |
//! | `big` | `bytes` | that many `x` |
//! | `mixed` | none | text, an image, an audio clip, an embedded text resource, a blob and a resource link |
//! | `env` | `name` | the value of the variable in the server's environment, or `(unset)` |
//! | `pid` | none | the server's process id |
//! | `exit` | none | nothing: the process exits during the call |
//! | `slow` | none | nothing, ever |
//! | `a.b` | none | `dotted`; a tool whose name no model provider accepts |
//!
//! Test code, not a product: it panics and exits on purpose.

#![warn(missing_docs)]
// Test code: it panics when the machine cannot give it a port.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, Resource, ResourceContents,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Value, json};

mod http;
mod logs;
mod thread_tools;

pub use http::TestHttpServer;
pub use logs::{LogCapture, wait_until};
pub use thread_tools::{GET_UI_CATALOG, ThreadToolsServer};

/// What a server has been asked, shared by every session of it.
#[derive(Debug, Default)]
pub struct Counters {
    /// `initialize` requests served.
    pub initializations: AtomicUsize,
    /// Tool calls that reached the handler (whether or not they finished).
    pub calls: AtomicUsize,
}

/// The MCP server behind the tools of the table in the [crate docs](crate).
#[derive(Debug, Clone, Default)]
pub struct TestServer {
    counters: Arc<Counters>,
}

impl TestServer {
    /// A server that counts what it is asked in `counters`.
    pub fn counting(counters: Arc<Counters>) -> Self {
        Self { counters }
    }

    /// The tools this server offers, in the order it lists them.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "echo", "fail", "big", "mixed", "env", "pid", "exit", "slow", "a.b",
        ]
    }

    fn tools() -> Vec<Tool> {
        let schema = |value: Value| match value {
            Value::Object(map) => Arc::new(map),
            _ => Arc::default(),
        };
        let none = || schema(json!({"type": "object", "properties": {}}));
        vec![
            // The only tool with a `title`: what its step is called.
            Tool::new(
                "echo",
                "Answers with the text it is given.",
                schema(json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                })),
            )
            .with_title("Echo it back"),
            Tool::new("fail", "Always answers with an error result.", none()),
            Tool::new(
                "big",
                "Answers with `bytes` bytes of text.",
                schema(json!({
                    "type": "object",
                    "properties": {"bytes": {"type": "integer"}},
                    "required": ["bytes"]
                })),
            ),
            Tool::new("mixed", "Answers with every kind of content block.", none()),
            Tool::new(
                "env",
                "Answers with the value of an environment variable of the server.",
                schema(json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                })),
            ),
            Tool::new("pid", "Answers with the process id of the server.", none()),
            Tool::new("exit", "Ends the server process during the call.", none()),
            Tool::new("slow", "Never answers.", none()),
            // No description, and a schema without a type: what a lazy server sends.
            Tool::new_with_raw("a.b", None, Arc::default()),
        ]
    }
}

fn text(text: impl Into<String>) -> CallToolResponse {
    CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(text)]))
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        self.counters.initializations.fetch_add(1, Ordering::SeqCst);
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(Self::tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.counters.calls.fetch_add(1, Ordering::SeqCst);
        let args = request.arguments.unwrap_or_default();
        let string = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| McpError::invalid_params(format!("`{key}` must be a string"), None))
        };
        match request.name.as_ref() {
            "echo" => Ok(text(string("text")?)),
            "fail" => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text("failed on purpose"),
            ]))),
            "big" => {
                let bytes = args
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| McpError::invalid_params("`bytes` must be an integer", None))?;
                Ok(text("x".repeat(
                    usize::try_from(bytes).unwrap_or(usize::MAX).min(8 << 20),
                )))
            }
            "mixed" => Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text("a line of text"),
                ContentBlock::image("aGVsbG8=", "image/png"),
                ContentBlock::audio("aGVsbG8=", "audio/wav"),
                ContentBlock::embedded_text("file:///notes.txt", "embedded text"),
                ContentBlock::resource(ResourceContents::BlobResourceContents {
                    uri: "file:///data.bin".into(),
                    mime_type: Some("application/octet-stream".into()),
                    blob: "AAAA".into(),
                    meta: None,
                }),
                ContentBlock::resource_link(Resource::new("file:///linked.txt", "linked")),
            ]))),
            "env" => {
                let name = string("name")?;
                Ok(text(
                    std::env::var(&name).unwrap_or_else(|_| "(unset)".to_owned()),
                ))
            }
            "pid" => Ok(text(std::process::id().to_string())),
            "exit" => std::process::exit(3),
            "slow" => {
                std::future::pending::<()>().await;
                Ok(text("unreachable"))
            }
            "a.b" => Ok(text("dotted")),
            other => Err(McpError::invalid_params(format!("no tool `{other}`"), None)),
        }
    }
}
