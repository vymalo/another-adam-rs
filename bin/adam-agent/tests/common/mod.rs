//! Shared fixtures: agent folders written (or copied from the shipped example) into a temp dir,
//! models scripted by what the files say, and a few HTTP helpers. Everything is offline.
#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use adam_model::{ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tempfile::TempDir;

pub mod pg;

/// The example folder the repository ships (`dev/agents/assistant`): a chat persona.
pub fn example_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/agents/assistant")
}

/// The researcher folder the repository ships (`dev/agents/researcher`).
pub fn researcher_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/agents/researcher")
}

/// A copy of the shipped example under `<tmp>/agent`: what a deployment mounts as
/// `ADAM_AGENT_DIR`, which a test then edits the way a deployment edits it.
pub fn assistant() -> TempDir {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    copy(&example_dir().join("agent"), &tmp.path().join("agent"));
    tmp
}

/// `agent/instructions.md` of a folder made by [`assistant`], rewritten by `edit`.
pub fn edit_instructions(folder: &TempDir, edit: impl FnOnce(String) -> String) {
    let path = folder.path().join("agent/instructions.md");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, edit(text)).unwrap();
}

/// The shipped example turned into the chat agent of the stack: `name: chat`, "Chat", a summary of
/// its own. What a deployment writes in a few lines.
pub fn chat() -> TempDir {
    let folder = assistant();
    edit_instructions(&folder, |text| {
        text.replacen("name: assistant", "name: chat", 1)
            .replacen("display_name: Assistant", "display_name: Chat", 1)
            .replacen("  name: Assistant", "  name: Chat", 1)
            .replacen(
                "In one sentence: I answer your questions in plain words and ask when I need to know more.",
                "In one sentence: I talk things through with you.",
                1,
            )
    });
    folder
}

/// A folder with one file, `agent/instructions.md`.
pub fn folder_with(instructions: &str) -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("agent")).unwrap();
    std::fs::write(tmp.path().join("agent/instructions.md"), instructions).unwrap();
    tmp
}

/// An OpenAI chat-completions reply that answers with text.
pub fn text_reply(text: &str) -> Value {
    json!({
        "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
}

/// An OpenAI chat-completions reply that calls one tool.
pub fn tool_reply(id: &str, name: &str, arguments: Value) -> Value {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": null, "tool_calls": [{
                "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()}
            }]},
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
}

/// A researcher: a persona, a skill and one MCP server `search` at `url`, which takes its token from
/// `${SEARCH_TOKEN}` and offers only `web_search`. What the stack's researcher is, in a folder.
pub fn researcher(url: &str) -> TempDir {
    let folder = folder_with(
        "---\nname: researcher\ndescription: Researches a question on the web and answers with its sources.\n\
vars:\n  display_name: Researcher\n\
card:\n  name: Researcher\n  skills:\n    - id: web-research\n      name: Web research\n      \
description: Searches the web and answers with the best source it found.\n      tags: [search, sources]\n---\n\
Your name is {{display_name}}.\nIn one sentence: I search the web and answer with my sources.\n\n\
Use `search__web_search` to look things up, then answer with the best source you found.\n",
    );
    std::fs::write(
        folder.path().join("agent/mcp.json"),
        format!(
            r#"{{"mcpServers": {{"search": {{"type": "http", "url": "{url}",
                "headers": {{"Authorization": "Bearer ${{SEARCH_TOKEN}}"}},
                "tools": ["web_search"]}}}}}}"#
        ),
    )
    .unwrap();
    folder
}

/// The researcher the repository ships (`dev/agents/researcher/agent`), copied under `<tmp>/agent`
/// with its search server at `url` instead of the compose name the shipped `mcp.json` has. The token
/// of the server is `${SEARCH_MCP_TOKEN}`, as shipped.
pub fn shipped_researcher(url: &str) -> TempDir {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    copy(&researcher_dir().join("agent"), &tmp.path().join("agent"));
    let mcp = tmp.path().join("agent/mcp.json");
    let shipped = std::fs::read_to_string(&mcp).unwrap();
    assert!(shipped.contains(SHIPPED_SEARCH_URL), "{shipped}");
    std::fs::write(&mcp, shipped.replace(SHIPPED_SEARCH_URL, url)).unwrap();
    tmp
}

/// The URL of the web-search server in the shipped researcher's `mcp.json`: the mock of the stack.
pub const SHIPPED_SEARCH_URL: &str = "http://mock-mcp-search:8080/mcp";

/// A model that answers the way the `mock-assistant` WireMock mapping of `dev/wiremock/mock-openai`
/// does: it reads the two persona lines at the top of its system prompt (`Your name is X.`, `In one
/// sentence: Y.`) and greets back with them. It is a scripted model whose script is the prompt, so
/// what a test edits in the folder is what the answer says. It records what it was sent.
#[derive(Default)]
pub struct PersonaModel {
    requests: Mutex<Vec<ModelRequest>>,
}

impl PersonaModel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The requests, oldest first.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

/// The greeting a persona prompt gives: `Hi! I'm <name>. <summary>.`
pub fn greeting_for(system: &str) -> Option<String> {
    let line = |prefix: &str| {
        system
            .lines()
            .find_map(|l| l.strip_prefix(prefix))
            .map(|rest| rest.trim_end_matches('.').to_owned())
    };
    Some(format!(
        "Hi! I'm {}. {}.",
        line("Your name is ")?,
        line("In one sentence: ")?
    ))
}

#[async_trait]
impl ModelClient for PersonaModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let system = req.system.clone().unwrap_or_default();
        self.requests.lock().unwrap().push(req);
        let answer = greeting_for(&system)
            .ok_or_else(|| ModelError::invalid_request("no persona lines in the system prompt"))?;
        Ok(ModelResponse::text(answer))
    }

    async fn stream(
        &self,
        _req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        Err(ModelError::invalid_request("the agent does not stream"))
    }
}

/// One raw HTTP/1.1 request; `(status, whole response)`.
pub async fn raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> (u16, String) {
    try_raw(addr, method, path, bearer)
        .await
        .expect("an HTTP response")
}

/// [`raw`], reporting a refused connection or a broken response as an error.
pub async fn try_raw(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
) -> std::io::Result<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"nope"}}"#;
    let auth = bearer.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let payload = if method == "POST" {
        format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n{auth}\r\n{body}",
            body.len()
        )
    } else {
        format!("{auth}\r\n")
    };
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{payload}")
                .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("no status line"))?;
    Ok((status, response))
}

/// The JSON of a raw HTTP response (see [`raw`]): from the first `{` to the last `}`, which skips
/// the status line and the headers and any chunked-encoding framing.
pub fn json_of(response: &str) -> Value {
    let start = response.find('{').expect("a JSON body");
    let end = response.rfind('}').expect("a JSON body");
    serde_json::from_str(&response[start..=end]).expect("the body is JSON")
}

/// The official A2A client for the server at `addr`, authenticating with `token`. The card says
/// where requests go (the server's `PUBLIC_URL`, which in a test is not where it listens), so the
/// card's URLs are pointed at `addr` first.
pub async fn a2a_client(
    addr: std::net::SocketAddr,
    token: &str,
) -> a2a_client::A2AClient<Box<dyn a2a_client::Transport>> {
    let mut card = a2a_client::agent_card::AgentCardResolver::new(None)
        .resolve(&format!("http://{addr}"))
        .await
        .expect("the agent card");
    for interface in &mut card.supported_interfaces {
        interface.url = format!("http://{addr}/");
    }
    a2a_client::A2AClientFactory::builder()
        .with_interceptor(Arc::new(a2a_client::auth::AuthInterceptor::bearer(token)))
        .build()
        .create_from_card(&card)
        .await
        .expect("an A2A client")
}

// ---------------------------------------------------------------------- a web-search MCP server

/// What the stateless web-search MCP server saw.
#[derive(Default)]
pub struct Seen {
    /// The arguments of every `tools/call`, in order.
    pub calls: Vec<Value>,
    /// The `Authorization` header of every request that had one.
    pub authorizations: Vec<String>,
    /// How many `GET` and `DELETE` requests were refused with 405.
    pub refused: usize,
    /// How many `initialize` requests came in.
    pub initializations: usize,
}

/// A **stateless** MCP server over streamable HTTP, the shape a small mock takes: `POST /mcp` with
/// JSON responses (never an event stream), no `Mcp-Session-Id`, `405` for `GET` and `DELETE`, and one
/// tool, `web_search { query }`, that answers a numbered list of results (`[mock:empty]` in the query:
/// "No results."; `[mock:error]`: an `isError` result). With a token, a request without
/// `Authorization: Bearer <token>` is refused with 401.
pub struct SearchServer {
    addr: std::net::SocketAddr,
    seen: Arc<Mutex<Seen>>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct SearchState {
    seen: Arc<Mutex<Seen>>,
    token: Option<String>,
}

impl SearchServer {
    pub async fn start(token: Option<&str>) -> Self {
        use axum::Router;
        use axum::routing::any;
        let seen = Arc::new(Mutex::new(Seen::default()));
        let state = SearchState {
            seen: seen.clone(),
            token: token.map(str::to_owned),
        };
        let app = Router::new()
            .route("/mcp", any(search_endpoint))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { addr, seen, task }
    }

    /// The URL to put in an `mcp.json`.
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    pub fn calls(&self) -> Vec<Value> {
        self.seen.lock().unwrap().calls.clone()
    }

    pub fn authorizations(&self) -> Vec<String> {
        self.seen.lock().unwrap().authorizations.clone()
    }

    pub fn refused(&self) -> usize {
        self.seen.lock().unwrap().refused
    }

    pub fn initializations(&self) -> usize {
        self.seen.lock().unwrap().initializations
    }
}

impl Drop for SearchServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The one result list every search answers.
pub const SEARCH_RESULT: &str = "1. The Rust language — https://example.org/mock-search/1\n   A language empowering everyone.\n2. Async in Rust — https://example.org/mock-search/2\n   How futures work.";

async fn search_endpoint(
    axum::extract::State(state): axum::extract::State<SearchState>,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(given) = &given {
        state
            .seen
            .lock()
            .unwrap()
            .authorizations
            .push(given.clone());
    }
    if let Some(token) = &state.token
        && given.as_deref() != Some(format!("Bearer {token}").as_str())
    {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if method != axum::http::Method::POST {
        state.seen.lock().unwrap().refused += 1;
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let id = request.get("id").cloned();
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => {
            state.seen.lock().unwrap().initializations += 1;
            json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-mcp-search", "version": "0.0.1"},
            })
        }
        // A notification has no answer.
        m if m.starts_with("notifications/") => return StatusCode::ACCEPTED.into_response(),
        "ping" => json!({}),
        "tools/list" => json!({"tools": [{
            "name": "web_search",
            "description": "Search the web and list the best results.",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string", "description": "What to search for"}},
                "required": ["query"],
            },
        }]}),
        "tools/call" => {
            let arguments = request["params"]["arguments"].clone();
            state.seen.lock().unwrap().calls.push(arguments.clone());
            let query = arguments["query"].as_str().unwrap_or_default();
            let (text, is_error) = if query.contains("[mock:error]") {
                ("The search backend is down.", true)
            } else if query.contains("[mock:empty]") {
                ("No results.", false)
            } else {
                (SEARCH_RESULT, false)
            };
            json!({"content": [{"type": "text", "text": text}], "isError": is_error})
        }
        _ => {
            let error = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "method not found"}});
            return axum::Json(error).into_response();
        }
    };
    axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}
