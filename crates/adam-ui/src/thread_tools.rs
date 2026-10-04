//! The thread-tools client: the endpoint a message announces (`thread-tools/v1`), read at every
//! model turn and called like any tool, and the refetch of the screen's catalog through it.
//!
//! A message from the orchestration layer carries `{url, token, expiresAt}` for the one thread it
//! belongs to; `adam-a2a-runtime`'s `vymalo_inbound` puts it in the run's inbound context under
//! [`CONTEXT_THREAD_TOOLS`], where it stays **until it expires** (the agent drops an entry whose
//! `expiresAt` has passed at the start of its next step) and survives a restart, so that a run
//! stepped by another replica can use it. The token is a credential: it never goes into a log line,
//! a tool result or an error message.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant L as LlmAgent (model step)
//!     participant T as ThreadTools (ToolSource)
//!     participant E as Thread-tools endpoint
//!     L->>T: specs(context)
//!     T->>T: the grant in the context, not expired?
//!     T->>E: tools/list (Bearer token)
//!     E-->>T: get_ui_catalog, and the tools attached since
//!     T-->>L: every listed tool, under its listed name
//!     L->>T: call(name, args), inside the journaled step
//!     T->>E: tools/call
//!     E-->>T: result (isError, text)
//!     T-->>L: ToolOutput (error result when it failed or the grant is gone)
//! ```
//!
//! ```mermaid
//! stateDiagram-v2
//!     [*] --> Absent: no grant in the context
//!     Absent --> Usable: a message brings one
//!     Usable --> Usable: a later message brings a newer one
//!     Usable --> Expired: expiresAt passes
//!     Expired --> Usable: a later message brings a new one
//!     Expired --> Absent: the next step drops the entry
//!     Usable --> Refused: the endpoint answers 401
//! ```
//!
//! **What a call carries and waits for.** The endpoint lists each tool with `_meta["thread-tools/v1"]`
//! (`{reportsStep, timeoutSecs}`); the listing is read once per model turn and its notes ride in the run's
//! state ([`ToolNote`]), so the call, made later and possibly elsewhere, knows them. A call waits as
//! long as the tool's `timeoutSecs` says, **capped** by [`McpPolicy::thread_tools_max_call`] (`THREAD_TOOLS_MAX_CALL_SECS`,
//! default 3600 s), and by the policy's call timeout (60 s) for a tool that says nothing; it is sent with
//! the request `_meta["thread-tools/v1"] = {callId, parentStepId?}`. The `callId` is made from the run
//! and the model's call id, both fixed by the journal, so a step retried after its lease expired sends the
//! one it sent before (the orchestrator deduplicates `ask_agent` by it and reports a relayed call's step
//! under it). A tool that `reportsStep` is reported by the orchestrator: the agent emits no step of its
//! own for it. A cancel of the run drops the call (the connection closes) instead of waiting out an hour.
//!
//! Nothing here fails a run: with no usable grant the source offers no tools and a call is an
//! error result that says the tools are gone; an endpoint that is down is a warning and no tools.

use std::sync::Arc;
use std::time::Duration;

use adam_a2a_runtime::{CONTEXT_THREAD_TOOLS, integral_numbers};
use adam_llm_agent::{Listing, SourceCtx, ToolCtx, ToolError, ToolNote, ToolOutput, ToolSource};
use adam_mcp::{CallOptions, Endpoint, EndpointError, McpPolicy, RemoteTool};
use adam_model::ToolSpec;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::catalog::Claimed;
use crate::mentions::mentioned_agents;
use crate::resolve::UiState;
use crate::show::{SHOW, describe_show};

/// The key of the thread-tools extension in a tool's `_meta` and in a request's `_meta`.
pub const META_KEY: &str = "thread-tools/v1";

/// The longest `callId` the contract takes, in bytes.
const MAX_CALL_ID_BYTES: usize = 256;

/// The tool of the endpoint that gives the thread's current catalog.
pub const GET_UI_CATALOG: &str = "get_ui_catalog";

/// The tool with which an agent says "this is my answer for this turn": `turn_output { text }`.
/// What it is told when the call succeeds is [`TURN_OUTPUT_DELIVERED`], and the text is the run's
/// answer and the end of the turn ([`ToolOutput::final_answer`]).
pub const TURN_OUTPUT: &str = "turn_output";

/// What the model is told when `turn_output` delivered its answer. The endpoint's own `{"delivered":
/// true}` says nothing to a model about what to do next. The turn ends with the call (the run is
/// finished without another model call), so the model normally never reads this; it does when a
/// message reached the run meanwhile and it is called again with that message.
pub const TURN_OUTPUT_DELIVERED: &str = "Delivered to the person as your answer. Do not repeat it.";

/// The longest a listing waits for the endpoint, whatever the policy's connect timeout: it is paid
/// at every model turn.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// What an inbound context can say about the thread's tools, when it says nothing usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// The messages of the run carried no grant (the card was not listed, or the sender has none).
    Absent,
    /// The grant's `expiresAt` has passed.
    Expired,
    /// The entry is not `{url, token, expiresAt}` of non-empty strings and an RFC 3339 time.
    Malformed,
}

impl Unavailable {
    /// Why, for a message to the model (never the token).
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Absent => "this conversation announced no endpoint for its tools",
            Self::Expired => "the grant for this conversation's tools has expired",
            Self::Malformed => "the grant for this conversation's tools is not readable",
        }
    }
}

/// `{url, token, expiresAt}` of the thread-tools extension: where the endpoint is and the
/// credential that opens it until `expires_at`. `Debug` shows the URL and the expiry, never the
/// token.
#[derive(Clone)]
pub(crate) struct Grant {
    pub(crate) url: String,
    pub(crate) token: SecretString,
    pub(crate) expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// What the clock says now; replaced in tests.
pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Reaches the thread-tools endpoint a run's context announces.
#[derive(Clone)]
pub struct ThreadToolsClient {
    policy: McpPolicy,
    clock: Clock,
    /// How long a call to a tool that does not say how long it may take is waited for.
    default_call: Duration,
    /// The longest any call is waited for, whatever the tool says.
    max_call: Duration,
}

impl std::fmt::Debug for ThreadToolsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadToolsClient").finish_non_exhaustive()
    }
}

/// What `get_ui_catalog` answered.
pub(crate) struct Fetched {
    /// What the endpoint says the catalog is.
    pub(crate) claimed: Claimed,
    /// The document; `None` when the endpoint said it is `unchanged`, that is, the `knownDigest`.
    pub(crate) document: Option<Value>,
}

impl ThreadToolsClient {
    /// A client under `policy`: its URL rules (https, or plain `http` only to this machine unless
    /// [`McpPolicy::allow_insecure`] says otherwise) and its timeouts. A listing waits at most ten
    /// seconds, whatever the connect timeout, because it is paid at every model turn. A call waits
    /// as long as its tool says it may take ([`ToolNote::timeout`]), and at most
    /// [`McpPolicy::thread_tools_max_call`]; for a tool that says nothing, [`McpPolicy::call_timeout`]
    /// (60 s by default), under the same cap.
    pub fn new(policy: McpPolicy) -> Self {
        let listing = policy.connect_timeout_value().min(LIST_TIMEOUT);
        Self {
            default_call: policy.call_timeout_value(),
            max_call: policy.thread_tools_max_call_value(),
            policy: policy.connect_timeout(listing),
            clock: Arc::new(Utc::now),
        }
    }

    /// How long a call of a tool with `note` is waited for: the time the tool says, else the policy's
    /// call timeout, and never more than the cap.
    pub fn wait_for(&self, note: Option<&ToolNote>) -> Duration {
        note.and_then(ToolNote::timeout)
            .unwrap_or(self.default_call)
            .min(self.max_call)
    }

    /// Read the time from `clock` instead of the system clock (for tests of the expiry).
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The grant of a run's inbound context, if it has a usable one.
    pub(crate) fn grant(&self, context: &Map<String, Value>) -> Result<Grant, Unavailable> {
        let entry = context
            .get(CONTEXT_THREAD_TOOLS)
            .ok_or(Unavailable::Absent)?;
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        let (Some(url), Some(token), Some(expires)) =
            (text("url"), text("token"), text("expiresAt"))
        else {
            return Err(Unavailable::Malformed);
        };
        let expires_at = DateTime::parse_from_rfc3339(expires)
            .map_err(|_| Unavailable::Malformed)?
            .with_timezone(&Utc);
        if expires_at <= (self.clock)() {
            return Err(Unavailable::Expired);
        }
        Ok(Grant {
            url: url.to_owned(),
            token: SecretString::from(token.to_owned()),
            expires_at,
        })
    }

    /// The endpoint of `grant`, under the policy.
    pub(crate) fn endpoint(&self, grant: &Grant) -> Result<Endpoint, EndpointError> {
        Endpoint::new(&grant.url, &grant.token, &self.policy)
    }

    /// Ask the endpoint for the thread's current catalog: `get_ui_catalog` with `known_digest`
    /// when the caller holds a copy. The text of the error says what happened, without the token.
    pub(crate) async fn fetch_catalog(
        &self,
        grant: &Grant,
        known_digest: Option<&str>,
    ) -> Result<Fetched, String> {
        let endpoint = self.endpoint(grant).map_err(|e| e.to_string())?;
        let mut arguments = Map::new();
        if let Some(digest) = known_digest {
            arguments.insert("knownDigest".into(), Value::String(digest.to_owned()));
        }
        let result = endpoint
            .call_tool(GET_UI_CATALOG, arguments)
            .await
            .map_err(|e| e.to_string())?;
        if result.is_error {
            return Err(result.text);
        }
        let mut body = match result.structured {
            Some(structured) => structured,
            None => serde_json::from_str(&result.text)
                .map_err(|_| "get_ui_catalog answered something that is not JSON".to_owned())?,
        };
        integral_numbers(&mut body);
        fetched_from(&body)
    }
}

/// `{catalogId, version, digest, unchanged, catalog?}` as [`Fetched`].
fn fetched_from(body: &Value) -> Result<Fetched, String> {
    let bad = |what: &str| format!("get_ui_catalog answered without a usable {what}");
    let catalog_id = body
        .get("catalogId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("catalogId"))?;
    let digest = body
        .get("digest")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad("digest"))?;
    let version = body
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v >= 1)
        .ok_or_else(|| bad("version"))?;
    let unchanged = body.get("unchanged").and_then(Value::as_bool) == Some(true);
    let document = match (unchanged, body.get("catalog")) {
        (true, _) => None,
        (false, Some(catalog)) if catalog.is_object() => Some(catalog.clone()),
        (false, _) => return Err(bad("catalog")),
    };
    Ok(Fetched {
        claimed: Claimed {
            catalog_id: catalog_id.to_owned(),
            version,
            digest: digest.to_owned(),
        },
        document,
    })
}

/// What the endpoint's listing says about `tool` for the agent (`_meta["thread-tools/v1"]`): whether
/// the orchestrator reports each call as a step (`reportsStep: true`) and how long it lets a call run
/// (`timeoutSecs`, a positive whole number). `None` when it says neither, or says something that is not
/// that shape (nothing is assumed from a malformed entry).
fn note_of(tool: &RemoteTool) -> Option<ToolNote> {
    let meta = tool.meta.get(META_KEY)?.as_object()?;
    let mut note = ToolNote::new(tool.name.clone());
    if meta.get("reportsStep").and_then(Value::as_bool) == Some(true) {
        note = note.reporting_steps();
    }
    let secs = meta.get("timeoutSecs").and_then(|v| {
        v.as_u64().or_else(|| {
            v.as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 1.0 && *f <= 1e9)
                .map(|f| f as u64)
        })
    });
    if let Some(secs) = secs.filter(|s| *s >= 1) {
        note = note.with_timeout(Duration::from_secs(secs));
    }
    (note.reports_step || note.timeout_ms.is_some()).then_some(note)
}

/// The `callId` of a call: stable across a retry of the journaled step, and different for every call of
/// the thread. It is made from the run (a task is a run, and a thread's jobs are different runs) and the
/// model's call id (`ToolCall::id`, recorded with the model's answer), both of which a retry, a replay
/// and another worker see unchanged. Over 256 bytes (a model with long call ids) the call id is
/// replaced by its SHA-256, which is as stable.
fn call_id_of(ctx: &ToolCtx) -> String {
    let id = format!("{}:{}", ctx.run_id(), ctx.call_id());
    if id.len() <= MAX_CALL_ID_BYTES {
        return id;
    }
    let digest = Sha256::digest(ctx.call_id().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:sha256-{hex}", ctx.run_id())
}

/// The request `_meta` member of a call: `{callId, parentStepId?}`.
fn request_meta(ctx: &ToolCtx) -> Value {
    let mut meta = json!({"callId": call_id_of(ctx)});
    if let Some(parent) = ctx
        .parent_step_id()
        .filter(|p| p.len() <= MAX_CALL_ID_BYTES)
    {
        meta["parentStepId"] = json!(parent);
    }
    meta
}

/// Whether `name` can be shown to a model as a tool name: letters, digits, `-` and `_`, at most 64
/// characters, not starting with `_` (what every provider accepts).
fn fits(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('_')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `name` is the name of a tool the orchestration layer relays from a server attached to the
/// conversation: `<server>__<tool>`, the shape the agent's own MCP tools have too (`websearch__fetch`).
fn is_relayed(name: &str) -> bool {
    name.split_once("__")
        .is_some_and(|(server, tool)| !server.is_empty() && !tool.is_empty())
}

/// The tools of the thread-tools endpoint a run's messages announced, as a
/// [`ToolSource`]: every tool the endpoint lists, under its listed name, listed again at every
/// model turn, so the tools of later slices of the orchestration layer (the relayed tools of attached
/// servers, `ask_agent`) appear with no change here.
///
/// With no usable grant the source offers nothing. A call goes straight to the endpoint (it
/// answers a name it does not have with a protocol error, which the model reads as an error
/// result), so the source belongs **last** among the sources of an agent.
///
/// **`turn_output`.** The one tool of the endpoint this source knows by name: when a call to it
/// succeeds, its `text` is announced as the run's answer **and ends the turn**
/// ([`ToolOutput::final_answer`]): once the calls of that model turn are answered, the run finishes
/// with `text` as its output, so the A2A `completed` status carries the Markdown the person was
/// shown, and the model is not called again to write a closing line after it (a line the person
/// would read under the answer, and the orchestration layer would log as one more message). The
/// model is told [`TURN_OUTPUT_DELIVERED`]; it reads it only when a message reached the run
/// meanwhile and the model is called again. A call the endpoint refuses (the turn is over, the text is empty or too long, the
/// endpoint is down) announces nothing and the model reads the error.
///
/// **A server the agent already has.** A conversation can attach a server (a web search) that the
/// agent's own `mcp.json` names too: the endpoint then lists `websearch__web_search` and the agent
/// has the tool of that name, which wins. That is expected at every model turn, so the source says
/// so ([`ToolSource::expects_repeat`]) and the loop logs the omission at debug level; any other
/// clash (a plain name the agent has too) is still a warning.
///
/// [`Ui::source`](crate::Ui::source) makes one that **hides `get_ui_catalog`** from the model (the
/// agent has `ui_catalog` for that, and two tools for one thing made models call whichever they
/// remembered; the catalog is still read through the endpoint, by this crate, when a message does
/// not carry it) and that **describes `show` with the components of the screen** of the
/// conversation (see [`refine`](ToolSource::refine)).
#[derive(Clone)]
pub struct ThreadTools {
    client: Arc<ThreadToolsClient>,
    /// The tools of the endpoint the model is not shown, and cannot call through this source.
    hidden: Vec<&'static str>,
    /// What describes `show` with the screen's components: the catalogs this process has read.
    ui: Option<Arc<UiState>>,
}

impl std::fmt::Debug for ThreadTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadTools")
            .field("hidden", &self.hidden)
            .field("describes_show", &self.ui.is_some())
            .finish_non_exhaustive()
    }
}

impl ThreadTools {
    /// The source over `client`: every tool the endpoint lists, as it lists it.
    pub fn new(client: Arc<ThreadToolsClient>) -> Self {
        Self {
            client,
            hidden: Vec::new(),
            ui: None,
        }
    }

    /// The source of a [`Ui`](crate::Ui): `get_ui_catalog` is hidden (`ui_catalog` is the tool for the
    /// catalog) and `show` is described with the screen's components.
    pub(crate) fn of_ui(state: Arc<UiState>) -> Self {
        Self {
            client: Arc::clone(&state.client),
            hidden: vec![GET_UI_CATALOG],
            ui: Some(state),
        }
    }

    /// Do not show the model the tool `name` of the endpoint, and refuse a call to it (as a name
    /// nobody has).
    #[must_use]
    pub fn hiding(mut self, name: &'static str) -> Self {
        self.hidden.push(name);
        self
    }
}

fn error_result(text: impl Into<String>) -> Option<Result<ToolOutput, ToolError>> {
    Some(Ok(ToolOutput::error(text)))
}

#[async_trait]
impl ToolSource for ThreadTools {
    async fn specs(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        self.listing(ctx).await.specs
    }

    fn expects_repeat(&self, name: &str, _taken: &[ToolSpec]) -> bool {
        is_relayed(name)
    }

    async fn listing(&self, ctx: &SourceCtx) -> Listing {
        let grant = match self.client.grant(ctx.context_map()) {
            Ok(grant) => grant,
            Err(Unavailable::Absent) => return Listing::default(),
            Err(why) => {
                tracing::debug!(reason = why.reason(), "no thread tools are offered");
                return Listing::default();
            }
        };
        let listed = match self.client.endpoint(&grant) {
            Ok(endpoint) => endpoint.list_tools().await,
            Err(e) => Err(e),
        };
        match listed {
            Ok(tools) => {
                let tools: Vec<RemoteTool> = tools
                    .into_iter()
                    .filter(|tool| !self.hidden.contains(&tool.name.as_str()))
                    .filter(|tool| {
                        let fine = fits(&tool.name);
                        if !fine {
                            tracing::warn!(
                                tool = %tool.name,
                                "the thread tools list a tool whose name cannot be shown to a model: left out"
                            );
                        }
                        fine
                    })
                    .collect();
                Listing {
                    specs: tools.iter().map(RemoteTool::spec).collect(),
                    notes: tools.iter().filter_map(note_of).collect(),
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "the thread tools could not be listed; none are offered this turn");
                Listing::default()
            }
        }
    }

    async fn instructions(&self, ctx: &SourceCtx) -> Option<String> {
        mentioned_agents(ctx.context_map())
    }

    async fn refine(&self, ctx: &SourceCtx, specs: &mut [ToolSpec]) {
        let Some(ui) = &self.ui else {
            return;
        };
        let Some(show) = specs.iter_mut().find(|spec| spec.name == SHOW) else {
            return;
        };
        // Only the conversation's own catalog describes it, and only one this process holds: a model
        // turn asks nobody for it. With none (or before a tool has read it) the description stays as
        // the tool made it, which says to call `ui_catalog`, and that call holds it from then on.
        if let Some(catalog) = ui.current_if_held(ctx.context_map()) {
            show.description = describe_show(&catalog);
        }
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        if self.hidden.contains(&name) {
            return None;
        }
        let grant = match self.client.grant(ctx.context_map()) {
            Ok(grant) => grant,
            // Nothing was announced: the name is nobody's here.
            Err(Unavailable::Absent) => return None,
            Err(why) => {
                return error_result(format!(
                    "`{name}` cannot be called: {}. Go on without it.",
                    why.reason()
                ));
            }
        };
        let arguments = match args {
            Value::Object(map) => map,
            Value::Null => Map::new(),
            _ => return error_result(format!("`{name}` takes a JSON object of arguments")),
        };
        // What the endpoint was given, for a tool whose input is the answer.
        let announced = if name == TURN_OUTPUT {
            arguments
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
        } else {
            None
        };
        // The call waits as long as its tool says it may take, within the cap, carries the id the
        // orchestrator reports its step under (and dedupes an ask by), and stops at once when the
        // run is cancelled: an hour is too long to wait for a call nobody will read.
        let options = CallOptions::new()
            .timeout(self.client.wait_for(ctx.note()))
            .meta(META_KEY, request_meta(ctx));
        let called = match self.client.endpoint(&grant) {
            Ok(endpoint) => {
                tokio::select! {
                    biased;
                    () = ctx.cancelled() => {
                        return error_result(format!(
                            "the call to `{name}` was cancelled: the run was stopped"
                        ));
                    }
                    called = endpoint.call_tool_with(name, arguments, options) => called,
                }
            }
            Err(e) => Err(e),
        };
        match called {
            Ok(result) if result.is_error => Some(Ok(ToolOutput::error(result.text))),
            Ok(result) => Some(Ok(match announced {
                Some(text) => ToolOutput::text(TURN_OUTPUT_DELIVERED).final_answer(text),
                None => ToolOutput::text(result.text),
            })),
            Err(EndpointError::Unauthorized) => error_result(format!(
                "`{name}` was refused: the endpoint does not accept this conversation's grant any \
                 more (it may have expired). Go on without it."
            )),
            Err(EndpointError::Rejected(why)) => {
                error_result(format!("`{name}` was refused by the endpoint: {why}"))
            }
            Err(other) => error_result(format!(
                "the call to `{name}` did not finish: {other}. It may or may not have run: check \
                 before you repeat it."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use adam_mcp_testkit::ThreadToolsServer;
    use serde_json::json;

    use super::*;

    fn context(url: &str, token: &str, expires_at: &str) -> Map<String, Value> {
        json!({CONTEXT_THREAD_TOOLS: {"url": url, "token": token, "expiresAt": expires_at}})
            .as_object()
            .cloned()
            .unwrap()
    }

    fn at(time: &str) -> Clock {
        let now = DateTime::parse_from_rfc3339(time)
            .unwrap()
            .with_timezone(&Utc);
        Arc::new(move || now)
    }

    fn client() -> ThreadToolsClient {
        ThreadToolsClient::new(McpPolicy::default()).with_clock(at("2026-10-01T12:00:00Z"))
    }

    #[test]
    fn a_grant_is_usable_until_it_expires_and_is_never_shown() {
        let c = client();
        let ok = c
            .grant(&context(
                "http://127.0.0.1:1/t",
                "secret-tok",
                "2026-10-01T12:00:01Z",
            ))
            .unwrap();
        assert_eq!(ok.url, "http://127.0.0.1:1/t");
        assert!(!format!("{ok:?}").contains("secret-tok"));
        // At the instant it expires, and after, it is gone.
        for gone in ["2026-10-01T12:00:00Z", "2026-10-01T11:59:59Z"] {
            assert_eq!(
                c.grant(&context("http://x/", "t", gone)).unwrap_err(),
                Unavailable::Expired,
                "{gone}"
            );
        }
        assert_eq!(c.grant(&Map::new()).unwrap_err(), Unavailable::Absent);
        for bad in [
            json!({"url": "", "token": "t", "expiresAt": "2026-10-01T13:00:00Z"}),
            json!({"url": "u", "token": "t"}),
            json!({"url": "u", "token": "t", "expiresAt": "tomorrow"}),
            json!("nope"),
        ] {
            let mut map = Map::new();
            map.insert(CONTEXT_THREAD_TOOLS.into(), bad.clone());
            assert_eq!(c.grant(&map).unwrap_err(), Unavailable::Malformed, "{bad}");
        }
    }

    #[test]
    fn the_answer_of_get_ui_catalog_is_read_and_a_poor_one_is_said() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let full = fetched_from(&json!({
            "catalogId": "c", "version": 3, "digest": digest, "unchanged": false,
            "catalog": {"catalogId": "c", "components": {}}}))
        .unwrap();
        assert_eq!(full.claimed.version, 3);
        assert!(full.document.is_some());
        let same = fetched_from(
            &json!({"catalogId": "c", "version": 3, "digest": digest, "unchanged": true}),
        )
        .unwrap();
        assert!(same.document.is_none());
        for bad in [
            json!({"version": 1, "digest": "d", "unchanged": true}),
            json!({"catalogId": "c", "version": 0, "digest": "d", "unchanged": true}),
            json!({"catalogId": "c", "version": 1.5, "digest": "d", "unchanged": true}),
            json!({"catalogId": "c", "version": 1, "unchanged": true}),
            json!({"catalogId": "c", "version": 1, "digest": "d", "unchanged": false}),
            json!({"catalogId": "c", "version": 1, "digest": "d", "unchanged": false, "catalog": 3}),
        ] {
            assert!(fetched_from(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_names_a_model_provider_accepts_are_offered() {
        for ok in [
            "get_ui_catalog",
            "relay__search",
            "a-b",
            "x".repeat(64).as_str(),
        ] {
            assert!(fits(ok), "{ok}");
        }
        for bad in ["", "_hidden", "a.b", "a b", "é", "x".repeat(65).as_str()] {
            assert!(!fits(bad), "{bad}");
        }
    }

    fn source(server_time: &str) -> ThreadTools {
        ThreadTools::new(Arc::new(
            ThreadToolsClient::new(McpPolicy::default()).with_clock(at(server_time)),
        ))
    }

    #[tokio::test]
    async fn every_listed_tool_is_offered_under_its_name_and_listed_again_each_time() {
        let server = ThreadToolsServer::start(&["tok"]).await;
        let source = source("2026-10-01T12:00:00Z");
        let ctx = SourceCtx::detached(context(&server.url("t1"), "tok", "2026-10-01T14:00:00Z"));

        let first = source.specs(&ctx).await;
        assert_eq!(
            first.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["get_ui_catalog"]
        );
        assert_eq!(first[0].parameters["type"], "object");
        // A tool the orchestrator attached since is there on the next turn.
        server.add_tool(
            "relay__search",
            "Search.",
            json!({"type": "object"}),
            "found",
        );
        server.add_tool(
            "bad.name",
            "Not a tool name.",
            json!({"type": "object"}),
            "x",
        );
        let second = source.specs(&ctx).await;
        assert_eq!(
            second.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["get_ui_catalog", "relay__search"],
            "a name no provider accepts is left out"
        );
        assert_eq!(server.lists(), 2);
    }

    #[tokio::test]
    async fn the_source_of_a_ui_hides_the_catalog_tool_the_model_has_its_own_for() {
        let server = ThreadToolsServer::start(&["tok"]).await;
        server.add_tool(
            "relay__search",
            "Search.",
            json!({"type": "object"}),
            "found",
        );
        let client =
            ThreadToolsClient::new(McpPolicy::default()).with_clock(at("2026-10-01T12:00:00Z"));
        let ui = crate::Ui::with_client(client);
        let source = ui.source();
        let ctx = SourceCtx::detached(context(&server.url("t1"), "tok", "2026-10-01T14:00:00Z"));
        let offered = source.specs(&ctx).await;
        assert_eq!(
            offered.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["relay__search"],
            "get_ui_catalog is not shown: ui_catalog is the tool for it"
        );
        // Nor can it be called through the source (a name nobody has), though the endpoint has it.
        let tool_ctx = ToolCtx::detached("get_ui_catalog", "c1", Arc::new(adam_runtime::NoopSink))
            .with_context(context(&server.url("t1"), "tok", "2026-10-01T14:00:00Z"));
        assert!(
            source
                .call(&tool_ctx, GET_UI_CATALOG, json!({}))
                .await
                .is_none()
        );
        // The tools it does list are called as before.
        assert!(
            source
                .call(&tool_ctx, "relay__search", json!({}))
                .await
                .is_some()
        );
        // The refetch of the catalog, which this crate does itself, still reaches the endpoint
        // (`tests/tools.rs` pins that): hiding is for the model.
        // A plain source offers everything, and `hiding` hides what it is told to.
        let plain = ThreadTools::new(Arc::clone(&ui.state.client));
        assert_eq!(plain.specs(&ctx).await.len(), 2);
        let hiding = ThreadTools::new(Arc::clone(&ui.state.client)).hiding("relay__search");
        assert_eq!(
            hiding
                .specs(&ctx)
                .await
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["get_ui_catalog"]
        );
    }

    #[tokio::test]
    async fn with_no_usable_grant_nothing_is_offered_and_the_endpoint_is_not_asked() {
        let server = ThreadToolsServer::start(&["tok"]).await;
        let source = source("2026-10-01T12:00:00Z");
        let none = SourceCtx::detached(Map::new());
        assert!(source.specs(&none).await.is_empty());
        let expired =
            SourceCtx::detached(context(&server.url("t1"), "tok", "2026-10-01T11:00:00Z"));
        assert!(source.specs(&expired).await.is_empty());
        let wrong =
            SourceCtx::detached(context(&server.url("t1"), "other", "2026-10-01T14:00:00Z"));
        assert!(
            source.specs(&wrong).await.is_empty(),
            "a refused token offers nothing"
        );
        assert_eq!(server.lists(), 0);
    }

    #[tokio::test]
    async fn a_call_goes_to_the_endpoint_and_its_answer_is_the_result() {
        let server = ThreadToolsServer::start(&["tok"]).await;
        server.add_tool(
            "relay__search",
            "Search.",
            json!({"type": "object"}),
            "found",
        );
        let source = source("2026-10-01T12:00:00Z");
        let ctx = ToolCtx::detached("relay__search", "c1", Arc::new(adam_runtime::NoopSink))
            .with_context(context(&server.url("t1"), "tok", "2026-10-01T14:00:00Z"));

        let ok = source
            .call(&ctx, "relay__search", json!({"q": "rust"}))
            .await
            .unwrap()
            .unwrap();
        assert!(!ok.is_error);
        assert_eq!(ok.content, r#"found {"q":"rust"}"#);
        // `null` arguments are an empty object.
        let none = source
            .call(&ctx, "relay__search", Value::Null)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(none.content, "found {}");
        // A name the endpoint does not have is an error result the model can read.
        let ghost = source
            .call(&ctx, "ghost", json!({}))
            .await
            .unwrap()
            .unwrap();
        assert!(ghost.is_error);
        assert!(
            ghost.content.contains("refused by the endpoint"),
            "{}",
            ghost.content
        );
        // Arguments that are not an object.
        let bad = source
            .call(&ctx, "relay__search", json!([1]))
            .await
            .unwrap()
            .unwrap();
        assert!(bad.is_error && bad.content.contains("JSON object"));
        // A tool that failed on its own is `isError`, as the endpoint said.
        let failing = source
            .call(&ctx, "get_ui_catalog", json!({}))
            .await
            .unwrap()
            .unwrap();
        assert!(failing.is_error);
        assert!(
            failing.content.contains("no UI catalog"),
            "{}",
            failing.content
        );
    }

    #[tokio::test]
    async fn a_delivered_turn_output_tells_the_model_what_to_do_and_announces_its_text() {
        let server = ThreadToolsServer::start(&["tok"]).await;
        server.enable_turn_output();
        let source = source("2026-10-01T12:00:00Z");
        let ctx = ToolCtx::detached("turn_output", "c1", Arc::new(adam_runtime::NoopSink))
            .with_context(context(&server.url("t1"), "tok", "2026-10-01T14:00:00Z"));

        let ok = source
            .call(&ctx, TURN_OUTPUT, json!({"text": "## Answer\n\n42"}))
            .await
            .unwrap()
            .unwrap();
        assert!(!ok.is_error);
        // The model reads what to do next, not the endpoint's `{"delivered": true}`.
        assert_eq!(ok.content, TURN_OUTPUT_DELIVERED);
        assert_eq!(ok.answer.as_deref(), Some("## Answer\n\n42"));
        assert!(ok.ends_turn, "the answer ends the turn");
        assert_eq!(server.announcements(), ["## Answer\n\n42"]);

        // Refused by the endpoint: the model reads the error, and nothing is announced.
        for (args, said) in [
            (json!({"text": "   "}), "text must not be empty"),
            (
                json!({"text": "x".repeat(65_537)}),
                "text must be at most 65536 bytes",
            ),
        ] {
            let refused = source.call(&ctx, TURN_OUTPUT, args).await.unwrap().unwrap();
            assert!(refused.is_error, "{said}");
            assert_eq!(refused.content, said);
            assert_eq!(refused.answer, None);
        }
        server.end_turn();
        let over = source
            .call(&ctx, TURN_OUTPUT, json!({"text": "late"}))
            .await
            .unwrap()
            .unwrap();
        assert!(over.is_error && over.answer.is_none());
        assert_eq!(over.content, "this turn is over");
        assert_eq!(server.announcements(), ["## Answer\n\n42"]);

        // Another tool's answer is never an announcement, whatever its arguments say.
        server.add_tool("relay__say", "Say.", json!({"type": "object"}), "said");
        let other = source
            .call(&ctx, "relay__say", json!({"text": "not an answer"}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(other.answer, None);
    }

    #[tokio::test]
    async fn a_turn_output_whose_grant_the_endpoint_refuses_announces_nothing() {
        // The grant is refused (a token the endpoint does not accept): an error result, no answer.
        let server = ThreadToolsServer::start(&["tok"]).await;
        server.enable_turn_output();
        let source = source("2026-10-01T12:00:00Z");
        let ctx = ToolCtx::detached("turn_output", "c1", Arc::new(adam_runtime::NoopSink))
            .with_context(context(&server.url("t1"), "wrong", "2026-10-01T14:00:00Z"));
        let refused = source
            .call(&ctx, TURN_OUTPUT, json!({"text": "answer"}))
            .await
            .unwrap()
            .unwrap();
        assert!(refused.is_error);
        assert_eq!(refused.answer, None);
        assert!(server.announcements().is_empty());
    }

    #[tokio::test]
    async fn a_call_degrades_to_an_error_result_when_the_grant_is_gone_or_refused_and_never_shows_the_token()
     {
        let server = ThreadToolsServer::start(&["tok"]).await;
        let source = source("2026-10-01T12:00:00Z");
        let ctx_with =
            |token: &str, expires: &str| {
                ToolCtx::detached("t", "c1", Arc::new(adam_runtime::NoopSink))
                    .with_context(context(&server.url("t1"), token, expires))
            };
        // No grant at all: the name is nobody's here.
        let bare = ToolCtx::detached("t", "c1", Arc::new(adam_runtime::NoopSink));
        assert!(source.call(&bare, "t", json!({})).await.is_none());
        // Expired.
        let expired = source
            .call(
                &ctx_with("secret-tok", "2026-10-01T11:00:00Z"),
                "t",
                json!({}),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            expired.is_error && expired.content.contains("expired"),
            "{}",
            expired.content
        );
        // The endpoint refuses the token.
        let refused = source
            .call(
                &ctx_with("secret-tok", "2026-10-01T14:00:00Z"),
                "t",
                json!({}),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            refused.is_error && refused.content.contains("does not accept"),
            "{}",
            refused.content
        );
        assert!(!refused.content.contains("secret-tok") && !expired.content.contains("secret-tok"));
        assert!(server.calls().is_empty());
    }
}
