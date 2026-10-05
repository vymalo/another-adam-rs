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
//! | `turn_output` (only after [`enable_turn_output`](ThreadToolsServer::enable_turn_output)) | `text` | `{"delivered": true}` as `structuredContent` and as the same JSON text, and the text is kept ([`announcements`](ThreadToolsServer::announcements)); `isError` for blank text (`text must not be empty`), more than 65536 bytes (`text must be at most 65536 bytes`) and, after [`end_turn`](ThreadToolsServer::end_turn), every call (`this turn is over`) |
//! | each tool of [`add_tool`](ThreadToolsServer::add_tool) | an object | the text it was given, with the arguments it was called with echoed after it |
//!
//! A tool can carry the `_meta` of the orchestrator's relayed tools
//! ([`add_tool_with_meta`](ThreadToolsServer::add_tool_with_meta):
//! `{"thread-tools/v1": {"reportsStep": true, "timeoutSecs": 125}}`), and can be slow
//! ([`set_delay`](ThreadToolsServer::set_delay), with [`in_flight`](ThreadToolsServer::in_flight) counting the
//! calls that have not answered). Every `tools/call` is recorded with the request's `_meta`
//! ([`requests`](ThreadToolsServer::requests)): the `callId` and `parentStepId` an agent sends.
//!
//! Test code, not a product: it panics when the machine cannot give it a port.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, InitializeRequestParams,
    InitializeResult, ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool,
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

/// The tool with which an agent announces its answer for the turn.
pub const TURN_OUTPUT: &str = "turn_output";

/// The longest answer `turn_output` takes, in bytes.
const MAX_TURN_OUTPUT_BYTES: usize = 65_536;

/// The state of the fake's `turn_output`.
#[derive(Default)]
struct TurnOutput {
    /// Whether the tool is listed.
    enabled: bool,
    /// Whether the turn is over: every call is refused.
    over: bool,
    /// The texts accepted, in order.
    announced: Vec<String>,
}

/// One `tools/call` the endpoint received.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The tool's name.
    pub name: String,
    /// The arguments (an object; empty when none were sent).
    pub arguments: Value,
    /// The request's `_meta` without the client library's own `progressToken` (`None` when nothing
    /// else was sent): where an agent puts the `thread-tools/v1` member with its `callId` and
    /// `parentStepId`.
    pub meta: Option<Value>,
}

/// Counts a call as in flight until the handler's future is dropped or ends.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn enter(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What the endpoint saw, and what it answers. Shared by every request.
#[derive(Default)]
struct Shared {
    /// The tokens accepted (a request with another gets 401).
    tokens: Mutex<Vec<String>>,
    /// The catalog `get_ui_catalog` answers: `(catalogId, version, digest, catalog)`.
    catalog: Mutex<Option<(String, u64, String, Value)>>,
    /// The built-in `turn_output`, when the test asked for it.
    turn_output: Mutex<TurnOutput>,
    /// The extra tools: `(tool, text it answers)`.
    extra: Mutex<Vec<(Tool, String)>>,
    initializations: AtomicUsize,
    lists: AtomicUsize,
    /// Every `tools/call`.
    calls: Mutex<Vec<Call>>,
    /// How long each extra tool takes to answer (none: at once).
    delays: Mutex<HashMap<String, Duration>>,
    /// The calls that have not answered (or were dropped).
    in_flight: Arc<AtomicUsize>,
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

fn turn_output_tool() -> Tool {
    Tool::new(
        TURN_OUTPUT,
        "Say that this is your answer for this turn, as Markdown: 1 to 65536 bytes. Call it once \
         the answer is ready; the turn ends with the call.",
        object(json!({
            "type": "object",
            "properties": {"text": {"type": "string", "minLength": 1,
                "description": "Your answer for this turn, as Markdown: 1 to 65536 bytes."}},
            "required": ["text"],
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
        if lock(&self.shared.turn_output).enabled {
            tools.push(turn_output_tool());
        }
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
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let _in_flight = InFlight::enter(&self.shared.in_flight);
        let name = request.name.to_string();
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        lock(&self.shared.calls).push(Call {
            name: name.clone(),
            arguments: arguments.clone(),
            // The library moves the request's `_meta` into the context, and adds a `progressToken`
            // of its own to every request: neither is what the caller sent.
            meta: serde_json::to_value(&context.meta)
                .ok()
                .map(|mut meta| {
                    if let Some(object) = meta.as_object_mut() {
                        object.remove("progressToken");
                    }
                    meta
                })
                .filter(|meta| meta.as_object().is_some_and(|m| !m.is_empty())),
        });
        let delay = lock(&self.shared.delays).get(&name).copied();
        if let Some(delay) = delay {
            // A caller that gives up (drops the connection, or says `notifications/cancelled`) ends
            // the wait: the call is no longer in flight.
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = context.ct.cancelled() => {}
            }
        }
        if name == GET_UI_CATALOG {
            return Ok(CallToolResponse::Complete(self.catalog_answer(&arguments)));
        }
        if name == TURN_OUTPUT && lock(&self.shared.turn_output).enabled {
            return self.turn_output_answer(&arguments);
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
    fn turn_output_answer(&self, arguments: &Value) -> Result<CallToolResponse, McpError> {
        let refuse = |why: &str| {
            Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text(why),
            ])))
        };
        let Some(text) = arguments.get("text").and_then(Value::as_str) else {
            return Err(McpError::invalid_params(
                "`text` is required and must be a string",
                None,
            ));
        };
        let mut state = lock(&self.shared.turn_output);
        if state.over {
            return refuse("this turn is over");
        }
        if text.trim().is_empty() {
            return refuse("text must not be empty");
        }
        if text.len() > MAX_TURN_OUTPUT_BYTES {
            return refuse("text must be at most 65536 bytes");
        }
        state.announced.push(text.to_owned());
        let body = json!({"delivered": true});
        let mut result = CallToolResult::success(vec![ContentBlock::text(body.to_string())]);
        result.structured_content = Some(body);
        Ok(CallToolResponse::Complete(result))
    }

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

    /// List a tool from now on, as [`add_tool`](Self::add_tool), with `meta` as its `_meta`: what the
    /// orchestrator's relayed tools say about themselves, `{"thread-tools/v1": {"reportsStep": true,
    /// "timeoutSecs": 125}}`.
    ///
    /// # Panics
    ///
    /// When `meta` is not a JSON object.
    pub fn add_tool_with_meta(
        &self,
        name: &str,
        description: &str,
        input_schema: Value,
        text: &str,
        meta: Value,
    ) {
        let Value::Object(meta) = meta else {
            panic!("a tool's _meta is a JSON object");
        };
        let tool = Tool::new(
            name.to_owned(),
            description.to_owned(),
            object(input_schema),
        )
        .with_meta(MetaObject::from(meta));
        let mut extra = lock(&self.shared.extra);
        extra.retain(|(t, _)| t.name != name);
        extra.push((tool, text.to_owned()));
    }

    /// Make the tool `name` take `delay` to answer (it answers after that, whatever it is asked).
    pub fn set_delay(&self, name: &str, delay: Duration) {
        lock(&self.shared.delays).insert(name.to_owned(), delay);
    }

    /// How many `tools/call` requests are being served: received and not yet answered or dropped. A
    /// caller that gave up on a slow call (it dropped the connection) takes this back to zero.
    pub fn in_flight(&self) -> usize {
        self.shared.in_flight.load(Ordering::SeqCst)
    }

    /// List the built-in `turn_output` from now on, as the orchestrator does: it takes
    /// `{text}`, keeps what it accepted ([`announcements`](Self::announcements)) and answers
    /// `{"delivered": true}`.
    pub fn enable_turn_output(&self) {
        lock(&self.shared.turn_output).enabled = true;
    }

    /// The turn is over: from now on `turn_output` answers an error, `this turn is over`.
    pub fn end_turn(&self) {
        lock(&self.shared.turn_output).over = true;
    }

    /// The texts `turn_output` accepted, in order.
    pub fn announcements(&self) -> Vec<String> {
        lock(&self.shared.turn_output).announced.clone()
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
        lock(&self.shared.calls)
            .iter()
            .map(|call| (call.name.clone(), call.arguments.clone()))
            .collect()
    }

    /// Every `tools/call` that reached the handler, with the request's `_meta`, in order.
    pub fn requests(&self) -> Vec<Call> {
        lock(&self.shared.calls).clone()
    }

    /// The `knownDigest` of every `get_ui_catalog` call, in order (`None`: the call had none).
    pub fn catalog_requests(&self) -> Vec<Option<String>> {
        lock(&self.shared.calls)
            .iter()
            .filter(|call| call.name == GET_UI_CATALOG)
            .map(|call| {
                call.arguments
                    .get("knownDigest")
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
