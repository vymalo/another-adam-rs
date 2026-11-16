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
//! | `screenshot` | none | one image block, a 1x1 PNG ([`PNG`]), as a browser's screenshot tool answers |
//! | `pdf` | none | one embedded blob resource, `application/pdf` ([`PDF`]), as a browser's PDF tool answers |
//! | `png` | `bytes`, `count?` | `count` image blocks (default 1, at most 20) of that many bytes each that start like a PNG and differ (at most 8 MiB each) |
//! | `env` | `name` | the value of the variable in the server's environment, or `(unset)` |
//! | `pid` | none | the server's process id |
//! | `exit` | none | nothing: the process exits during the call |
//! | `slow` | none | nothing, ever |
//! | `a.b` | none | `dotted`; a tool whose name no model provider accepts |
//!
//! A [`Script`] adds what a server says about itself (`instructions`) and the prompts it serves
//! ([`TestPrompt`]); [`TestHttpServer::start_scripted`] serves one.
//!
//! Test code, not a product: it panics and exits on purpose.

#![warn(missing_docs)]
// Test code: it panics when the machine cannot give it a port.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, GetPromptRequestParams,
    GetPromptResponse, GetPromptResult, InitializeRequestParams, InitializeResult,
    ListPromptsResult, ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument,
    PromptMessage, Resource, ResourceContents, Role, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Value, json};

mod http;
mod logs;
mod thread_tools;

pub use http::TestHttpServer;
pub use logs::{LogCapture, wait_until};
pub use thread_tools::{Call, GET_UI_CATALOG, TURN_OUTPUT, ThreadToolsServer};

/// The image the `screenshot` tool answers with: a 1x1 PNG.
pub const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0\x1f\x15\xc4\x89\0\0\0\nIDATx\x9cc\0\x01\0\0\x05\0\x01\r\n-\xb4\0\0\0\0IEND\xaeB`\x82";

/// The document the `pdf` tool answers with.
pub const PDF: &[u8] = b"%PDF-1.4\n% a page of the test server\n%%EOF\n";

/// What a server has been asked, shared by every session of it.
#[derive(Debug, Default)]
pub struct Counters {
    /// `initialize` requests served.
    pub initializations: AtomicUsize,
    /// Tool calls that reached the handler (whether or not they finished).
    pub calls: AtomicUsize,
    /// `prompts/get` requests that reached the handler.
    pub prompt_gets: AtomicUsize,
}

/// One prompt a [`Script`] serves: what `prompts/list` says and what `prompts/get` answers.
#[derive(Debug, Clone)]
pub struct TestPrompt {
    prompt: Prompt,
    messages: Vec<PromptMessage>,
    /// `prompts/get` never answers.
    hangs: bool,
    /// `prompts/get` answers with this MCP error.
    fails: Option<String>,
}

impl TestPrompt {
    /// A prompt called `name` that answers with no message yet.
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            prompt: Prompt::new(name, Some(description), None),
            messages: Vec::new(),
            hangs: false,
            fails: None,
        }
    }

    /// Declare an argument, in order.
    #[must_use]
    pub fn argument(mut self, name: &str, required: bool) -> Self {
        let mut argument = PromptArgument::new(name);
        argument.required = Some(required);
        self.prompt
            .arguments
            .get_or_insert_with(Vec::new)
            .push(argument);
        self
    }

    /// Answer with one more text message of the user.
    #[must_use]
    pub fn user(mut self, text: &str) -> Self {
        self.messages
            .push(PromptMessage::new_text(Role::User, text));
        self
    }

    /// Answer with one more text message of the assistant.
    #[must_use]
    pub fn assistant(mut self, text: &str) -> Self {
        self.messages
            .push(PromptMessage::new_text(Role::Assistant, text));
        self
    }

    /// Never answer `prompts/get` for this prompt.
    #[must_use]
    pub fn hangs(mut self) -> Self {
        self.hangs = true;
        self
    }

    /// Answer `prompts/get` for this prompt with an MCP error that says `message`.
    #[must_use]
    pub fn fails(mut self, message: &str) -> Self {
        self.fails = Some(message.to_owned());
        self
    }

    /// Answer with one more message of the user that is an image.
    #[must_use]
    pub fn image(mut self) -> Self {
        self.messages.push(PromptMessage::new(
            Role::User,
            ContentBlock::image(base64(PNG), "image/png"),
        ));
        self
    }
}

/// What a server says about itself and serves besides its tools.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// The `instructions` of its `initialize` answer.
    pub instructions: Option<String>,
    /// The prompts it serves. `prompts/get` fills `{{name}}` in a text with the argument of that
    /// name, so that a test can see what was sent.
    pub prompts: Vec<TestPrompt>,
    /// `prompts/list` never answers (a server that has the capability and does not use it).
    pub hang_prompt_list: bool,
    /// How long `tools/list` waits before it answers, in milliseconds.
    pub list_tools_delay_ms: u64,
}

/// The MCP server behind the tools of the table in the [crate docs](crate).
#[derive(Debug, Clone, Default)]
pub struct TestServer {
    counters: Arc<Counters>,
    script: Arc<Script>,
}

impl TestServer {
    /// A server that counts what it is asked in `counters`.
    pub fn counting(counters: Arc<Counters>) -> Self {
        Self::scripted(counters, Arc::new(Script::default()))
    }

    /// A server that counts what it is asked in `counters` and says and serves what `script` says.
    pub fn scripted(counters: Arc<Counters>, script: Arc<Script>) -> Self {
        Self { counters, script }
    }

    /// The tools this server offers, in the order it lists them.
    pub fn tool_names() -> Vec<&'static str> {
        vec![
            "echo",
            "fail",
            "big",
            "mixed",
            "screenshot",
            "pdf",
            "png",
            "env",
            "pid",
            "exit",
            "slow",
            "a.b",
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
                "screenshot",
                "Answers with an image of the page, a PNG.",
                none(),
            ),
            Tool::new("pdf", "Answers with the page as a PDF resource.", none()),
            Tool::new(
                "png",
                "Answers with `count` PNG images (default 1) of `bytes` bytes each.",
                schema(json!({
                    "type": "object",
                    "properties": {"bytes": {"type": "integer"}, "count": {"type": "integer"}},
                    "required": ["bytes"]
                })),
            ),
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

fn base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn text(text: impl Into<String>) -> CallToolResponse {
    CallToolResponse::Complete(CallToolResult::success(vec![ContentBlock::text(text)]))
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = if self.script.prompts.is_empty() && !self.script.hang_prompt_list {
            ServerCapabilities::builder().enable_tools().build()
        } else {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build()
        };
        let info = InitializeResult::new(capabilities);
        match &self.script.instructions {
            Some(text) => info.with_instructions(text.clone()),
            None => info,
        }
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

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        if self.script.hang_prompt_list {
            std::future::pending::<()>().await;
        }
        Ok(ListPromptsResult::with_all_items(
            self.script
                .prompts
                .iter()
                .map(|p| p.prompt.clone())
                .collect(),
        ))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
        self.counters.prompt_gets.fetch_add(1, Ordering::SeqCst);
        let Some(found) = self
            .script
            .prompts
            .iter()
            .find(|p| p.prompt.name == request.name)
        else {
            return Err(McpError::invalid_params(
                format!("no prompt `{}`", request.name),
                None,
            ));
        };
        if found.hangs {
            std::future::pending::<()>().await;
        }
        if let Some(message) = &found.fails {
            return Err(McpError::invalid_params(message.clone(), None));
        }
        let arguments = request.arguments.unwrap_or_default();
        let messages = found
            .messages
            .iter()
            .map(|message| match &message.content {
                ContentBlock::Text(text) => {
                    let mut filled = text.text.clone();
                    for (name, value) in &arguments {
                        if let Some(value) = value.as_str() {
                            filled = filled.replace(&format!("{{{{{name}}}}}"), value);
                        }
                    }
                    PromptMessage::new_text(message.role.clone(), filled)
                }
                _ => message.clone(),
            })
            .collect();
        Ok(GetPromptResponse::Complete(GetPromptResult::new(messages)))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        if self.script.list_tools_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(
                self.script.list_tools_delay_ms,
            ))
            .await;
        }
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
            "screenshot" => Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::image(base64(PNG), "image/png"),
            ]))),
            "pdf" => Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::resource(ResourceContents::BlobResourceContents {
                    uri: "testkit://capture/page.pdf".into(),
                    mime_type: Some("application/pdf".into()),
                    blob: base64(PDF),
                    meta: None,
                }),
            ]))),
            "png" => {
                let bytes = args
                    .get("bytes")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| McpError::invalid_params("`bytes` must be an integer", None))?;
                let count = args
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .min(20);
                let len = usize::try_from(bytes).unwrap_or(usize::MAX).min(8 << 20);
                let images = (0..count)
                    .map(|n| {
                        let mut image = PNG.to_vec();
                        // Each image its own bytes, so each is its own file.
                        image.resize(len, u8::try_from(n).unwrap_or(u8::MAX));
                        ContentBlock::image(base64(&image), "image/png")
                    })
                    .collect();
                Ok(CallToolResponse::Complete(CallToolResult::success(images)))
            }
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
