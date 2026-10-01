//! [`ThreadToolsServer`]: a fake of the orchestration layer's per-thread tool endpoint
//! (`docs/api/thread-tools-v1.md` of `vymalo/another-agentic-system`), for testing agents that call
//! it.
//!
//! It is the shape the real endpoint has: **stateless** streamable HTTP at
//! `/thread-tools/{thread}/mcp` (rmcp's `StreamableHttpService` over a `NeverSessionManager`, JSON
//! responses, no session id), a bearer check that answers `401` with `WWW-Authenticate: Bearer
//! error="invalid_token"` for a token that is not on its list (and plain `Bearer` for none), and a
//! tool list computed on every request. The built-in tool is `get_ui_catalog` with the contract's
//! input and output; more tools are added with [`ThreadToolsServer::add_tool`] and change the next
//! `tools/list` (the endpoint holds no state, a tool attached a moment ago is listed at once).
//!
//! | Tool | Arguments | Answers |
//! |---|---|---|
//! | `get_ui_catalog` | `knownDigest?` | the catalog set with [`set_catalog`](ThreadToolsServer::set_catalog) as `structuredContent` and as the same JSON text: `{catalogId, version, digest, unchanged, catalog?}`; `isError` ("this thread has no UI catalog; answer in text") without one |
//! | each tool of [`add_tool`](ThreadToolsServer::add_tool) | an object | the text it was given, with the arguments it was called with echoed after it |
//!
//! Test code, not a product: it panics when the machine cannot give it a port.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeRequestParams,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// The tool every endpoint has.
pub const GET_UI_CATALOG: &str = "get_ui_catalog";

/// What the endpoint saw, and what it answers. Shared by every request.
#[derive(Default)]
struct Shared {
    /// The tokens accepted (a request with another gets 401).
    tokens: Mutex<Vec<String>>,
    /// The catalog `get_ui_catalog` answers: `(catalogId, version, digest, catalog)`.
    catalog: Mutex<Option<(String, u64, String, Value)>>,
    /// The extra tools: `(tool, text it answers)`.
    extra: Mutex<Vec<(Tool, String)>>,
    initializations: AtomicUsize,
    lists: AtomicUsize,
    /// Every `tools/call`: `(name, arguments)`.
    calls: Mutex<Vec<(String, Value)>>,
    /// Every `Authorization` header that came in, accepted or not.
    authorizations: Mutex<Vec<String>>,
    /// Every request that was refused with 401.
    refused: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
struct Endpoint {
    shared: Arc<Shared>,
}

fn object(value: Value) -> Arc<Map<String, Value>> {
    match value {
        Value::Object(map) => Arc::new(map),
        _ => Arc::default(),
    }
}

fn get_ui_catalog_tool() -> Tool {
    Tool::new(
        GET_UI_CATALOG,
        "The thread's current UI catalog: the components the person's screen can draw.",
        object(json!({
            "type": "object",
            "properties": {"knownDigest": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$"}},
            "additionalProperties": false
        })),
    )
}

impl ServerHandler for Endpoint {
    fn get_info(&self) -> ServerConfig {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        self.shared.initializations.fetch_add(1, Ordering::SeqCst);
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.shared.lists.fetch_add(1, Ordering::SeqCst);
        let mut tools = vec![get_ui_catalog_tool()];
        tools.extend(
            lock(&self.shared.extra)
                .iter()
                .map(|(tool, _)| tool.clone()),
        );
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        lock(&self.shared.calls).push((name.clone(), arguments.clone()));
        if name == GET_UI_CATALOG {
            return Ok(CallToolResponse::Complete(self.catalog_answer(&arguments)));
        }
        let extra = lock(&self.shared.extra);
        match extra.iter().find(|(tool, _)| tool.name == name) {
            Some((_, text)) => Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                ContentBlock::text(format!("{text} {arguments}")),
            ]))),
            // What the real endpoint answers for a name nobody owns.
            None => Err(McpError::invalid_params(
                format!("unknown tool `{name}`"),
                None,
            )),
        }
    }
}

impl Endpoint {
    fn catalog_answer(&self, arguments: &Value) -> CallToolResult {
        let Some((catalog_id, version, digest, catalog)) = lock(&self.shared.catalog).clone()
        else {
            return CallToolResult::error(vec![ContentBlock::text(
                "this thread has no UI catalog; answer in text",
            )]);
        };
        let unchanged =
            arguments.get("knownDigest").and_then(Value::as_str) == Some(digest.as_str());
        let mut body = json!({
            "catalogId": catalog_id, "version": version, "digest": digest, "unchanged": unchanged
        });
        if !unchanged {
            body["catalog"] = catalog;
        }
        let mut result = CallToolResult::success(vec![ContentBlock::text(body.to_string())]);
        result.structured_content = Some(body);
        result
    }
}

async fn guard(State(shared): State<Arc<Shared>>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(given) = &given {
        lock(&shared.authorizations).push(given.clone());
    }
    let accepted = given
        .as_deref()
        .and_then(|g| g.strip_prefix("Bearer "))
        .is_some_and(|token| lock(&shared.tokens).iter().any(|t| t == token));
    if !accepted {
        shared.refused.fetch_add(1, Ordering::SeqCst);
        let challenge = if given.is_some() {
            "Bearer error=\"invalid_token\""
        } else {
            "Bearer"
        };
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(challenge),
        );
        return response;
    }
    next.run(request).await
}

/// The fake endpoint, on `127.0.0.1` at an ephemeral port.
pub struct ThreadToolsServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl ThreadToolsServer {
    /// Start a server that accepts `Authorization: Bearer <token>` for each of `tokens`.
    pub async fn start(tokens: &[&str]) -> Self {
        let shared = Arc::new(Shared::default());
        *lock(&shared.tokens) = tokens.iter().map(|t| (*t).to_owned()).collect();
        let endpoint = Endpoint {
            shared: Arc::clone(&shared),
        };
        let service = StreamableHttpService::new(
            move || Ok(endpoint.clone()),
            Arc::new(NeverSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true),
        );
        let app = Router::new()
            .route_service("/thread-tools/{thread}/mcp", service)
            .layer(middleware::from_fn_with_state(Arc::clone(&shared), guard));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let addr = listener.local_addr().expect("the bound address");
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, shared, task }
    }

    /// The endpoint's URL for `thread`, as a message would announce it.
    pub fn url(&self, thread: &str) -> String {
        format!("http://{}/thread-tools/{thread}/mcp", self.addr)
    }

    /// Accept exactly `tokens` from now on.
    pub fn set_tokens(&self, tokens: &[&str]) {
        *lock(&self.shared.tokens) = tokens.iter().map(|t| (*t).to_owned()).collect();
    }

    /// Answer `get_ui_catalog` with this catalog (and no catalog for `None`).
    pub fn set_catalog(&self, catalog: Option<(&str, u64, &str, Value)>) {
        *lock(&self.shared.catalog) = catalog
            .map(|(id, version, digest, doc)| (id.to_owned(), version, digest.to_owned(), doc));
    }

    /// List a tool from now on. A call answers `"{text} {arguments}"`. A name that is listed
    /// already is replaced.
    pub fn add_tool(&self, name: &str, description: &str, input_schema: Value, text: &str) {
        let tool = Tool::new(
            name.to_owned(),
            description.to_owned(),
            object(input_schema),
        );
        let mut extra = lock(&self.shared.extra);
        extra.retain(|(t, _)| t.name != name);
        extra.push((tool, text.to_owned()));
    }

    /// Stop listing the tool `name`.
    pub fn remove_tool(&self, name: &str) {
        lock(&self.shared.extra).retain(|(t, _)| t.name != name);
    }

    /// How many `initialize` requests were served.
    pub fn initializations(&self) -> usize {
        self.shared.initializations.load(Ordering::SeqCst)
    }

    /// How many `tools/list` requests were served.
    pub fn lists(&self) -> usize {
        self.shared.lists.load(Ordering::SeqCst)
    }

    /// Every `tools/call` that reached the handler: the tool and its arguments, in order.
    pub fn calls(&self) -> Vec<(String, Value)> {
        lock(&self.shared.calls).clone()
    }

    /// The `knownDigest` of every `get_ui_catalog` call, in order (`None`: the call had none).
    pub fn catalog_requests(&self) -> Vec<Option<String>> {
        lock(&self.shared.calls)
            .iter()
            .filter(|(name, _)| name == GET_UI_CATALOG)
            .map(|(_, args)| {
                args.get("knownDigest")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    /// Every `Authorization` header that came in, accepted or not.
    pub fn authorizations(&self) -> Vec<String> {
        lock(&self.shared.authorizations).clone()
    }

    /// How many requests were refused with 401.
    pub fn refused(&self) -> usize {
        self.shared.refused.load(Ordering::SeqCst)
    }
}

impl Drop for ThreadToolsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
