//! [`Endpoint`]: one MCP endpoint reached with a bearer token that is only known at run time,
//! one connection per request.
//!
//! The servers of an `mcp.json` ([`McpServers`](crate::McpServers)) are known when the process
//! starts, connected once and kept. An endpoint that a *message* announces (the per-thread tool
//! endpoint of the orchestration layer: a URL and a short-lived token in the message the agent is
//! sent) is not: it exists for one conversation, its token expires, and the replica that serves
//! the next step may not be the one that read the message. So each request opens its own
//! connection (initialize, the request, close), keeps nothing, and carries the token in the
//! `Authorization` header of every request. The server is expected to be stateless; a session id
//! it assigns anyway is honoured for the connection's own length.
//!
//! ```mermaid
//! sequenceDiagram
//!     participant C as caller (adam-ui)
//!     participant E as Endpoint
//!     participant S as MCP endpoint (stateless)
//!     C->>E: list_tools(), or call_tool(name, args)
//!     E->>S: initialize (Authorization: Bearer token)
//!     S-->>E: initialized
//!     E->>S: tools/list, or tools/call
//!     S-->>E: the tools, or the result (isError, text, structuredContent)
//!     E->>S: close
//!     E-->>C: RemoteTool list, or RemoteResult; or an EndpointError with the token scrubbed
//! ```
//!
//! The URL goes through the policy of every remote server (https, or plain `http` only to this
//! machine unless the deployment allows it: [`McpPolicy::allow_insecure`](crate::McpPolicy)), and
//! the token is registered with the redactor, so no error message and no result text this module
//! returns carries it.

use std::sync::Arc;
use std::time::Duration;

use adam_model::ToolSpec;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, MetaObject, RequestMetaObject, Tool as ListedTool,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Map, Value};

use crate::connection::{Recipe, Transport};
use crate::policy::McpPolicy;
use crate::redact::Redactor;
use crate::text::{MAX_DESCRIPTION_BYTES, cap_text};
use crate::tool::map_result;

/// The name the endpoint goes by in a message of this module.
const SERVER: &str = "thread tools";

/// How long a connection is given to close before it is dropped.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Why a request to an [`Endpoint`] did not give an answer. The text never carries the token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EndpointError {
    /// The URL or the token is not acceptable: the policy refuses the URL (plain `http` to
    /// another machine, credentials in it, not a URL), or the token cannot be a header value.
    #[error("{0}")]
    Refused(String),
    /// The endpoint did not answer within the time allowed.
    #[error("the endpoint did not answer within {0} ms")]
    Timeout(u128),
    /// The endpoint answered that the token is not accepted (HTTP 401 or 403): expired, or not
    /// for this thread.
    #[error("the endpoint did not accept the token")]
    Unauthorized,
    /// The endpoint answered the call with a protocol error (an unknown tool, bad parameters):
    /// the text is the server's own, scrubbed.
    #[error("the endpoint refused the call: {0}")]
    Rejected(String),
    /// The connection or the exchange failed: the text says how, scrubbed.
    #[error("{0}")]
    Failed(String),
}

/// A tool an endpoint lists.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTool {
    /// The tool's name on the endpoint.
    pub name: String,
    /// What it does (cut at 8 KiB); a default when the endpoint says nothing.
    pub description: String,
    /// The JSON Schema of its arguments (an object schema).
    pub input_schema: Value,
    /// The tool's human title (MCP's `title`), when the endpoint gave a non-blank one.
    pub title: Option<String>,
    /// The tool's own `_meta`, as the endpoint listed it (empty when it listed none). Where a key
    /// of it means something to the caller (`thread-tools/v1`) the caller reads it.
    pub meta: Map<String, Value>,
}

impl RemoteTool {
    /// The tool as the model is shown it: the same name, description and schema.
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
        }
    }
}

/// What one call to an endpoint's tool is sent with and waits for: [`Endpoint::call_tool_with`].
///
/// The default is a call with no request `_meta` that waits as long as the policy's call timeout.
///
/// ```
/// use std::time::Duration;
/// use adam_mcp::CallOptions;
/// use serde_json::json;
///
/// let options = CallOptions::new()
///     .timeout(Duration::from_secs(900))
///     .meta("thread-tools/v1", json!({"callId": "run-1:call_7"}));
/// assert_eq!(options.timeout_value(), Some(Duration::from_secs(900)));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct CallOptions {
    timeout: Option<Duration>,
    meta: Map<String, Value>,
}

impl CallOptions {
    /// A call with the policy's timeout and no `_meta`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait this long for the answer instead of the policy's call timeout. A zero is raised to one
    /// millisecond. The caller decides what the time is and caps it: the endpoint adds no limit
    /// of its own.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout.max(Duration::from_millis(1)));
        self
    }

    /// Send `value` under `key` in the request's `_meta` (a later call with the same key
    /// replaces it).
    #[must_use]
    pub fn meta(mut self, key: impl Into<String>, value: Value) -> Self {
        self.meta.insert(key.into(), value);
        self
    }

    /// The timeout this call was given, if any.
    pub fn timeout_value(&self) -> Option<Duration> {
        self.timeout
    }

    /// The request `_meta` this call is sent with.
    pub fn meta_value(&self) -> &Map<String, Value> {
        &self.meta
    }
}

/// What a call to an endpoint's tool answered.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteResult {
    /// The tool's own failure (`isError`): the text says what went wrong.
    pub is_error: bool,
    /// The content blocks as text, scrubbed and cut at 64 KiB: what a model is shown.
    pub text: String,
    /// The `structuredContent`, when the tool gave one.
    pub structured: Option<Value>,
}

/// One MCP endpoint and the bearer token to call it with. Cheap to make; holds no connection.
/// `Debug` shows the URL without its query and never the token.
pub struct Endpoint {
    recipe: Recipe,
    call_timeout: Duration,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = match &self.recipe.transport {
            Transport::Http { url, .. } => crate::url::shown(url),
            Transport::Stdio { .. } => String::new(),
        };
        f.debug_struct("Endpoint")
            .field("url", &url)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    /// The endpoint at `url`, called with `Authorization: Bearer <bearer>`.
    ///
    /// `policy` decides whether plain `http` to another machine is allowed
    /// ([`McpPolicy::insecure_allowed`]) and how long to wait ([`McpPolicy::connect_timeout_value`]
    /// for a connection and a listing, [`McpPolicy::call_timeout_value`] for a call).
    ///
    /// # Errors
    ///
    /// [`EndpointError::Refused`] when the URL is not acceptable under the policy or the token
    /// cannot be sent as a header.
    pub fn new(
        url: &str,
        bearer: &SecretString,
        policy: &McpPolicy,
    ) -> Result<Self, EndpointError> {
        let mut redactor = Redactor::default();
        redactor.add(bearer.expose_secret());
        let url = crate::url::check(SERVER, url, policy.insecure_allowed(), &redactor)
            .map_err(|e| EndpointError::Refused(redactor.scrub(&e.to_string())))?;
        let mut value = HeaderValue::from_str(&format!("Bearer {}", bearer.expose_secret()))
            .map_err(|_| EndpointError::Refused("the token is not a valid header value".into()))?;
        value.set_sensitive(true);
        Ok(Self {
            recipe: Recipe {
                server: SERVER.to_owned(),
                command_as_written: String::new(),
                transport: Transport::Http {
                    url,
                    headers: vec![(HeaderName::from_static("authorization"), value)],
                },
                redactor: Arc::new(redactor),
                connect_timeout: policy.connect_timeout_value(),
            },
            call_timeout: policy.call_timeout_value(),
        })
    }

    /// The tools the endpoint lists now (every page), in its order.
    ///
    /// # Errors
    ///
    /// [`EndpointError`]: the connection, the token, the time or the exchange.
    pub async fn list_tools(&self) -> Result<Vec<RemoteTool>, EndpointError> {
        let mut service = self
            .recipe
            .dial()
            .await
            .map_err(|e| self.failed(&e.to_string()))?;
        let listed =
            tokio::time::timeout(self.recipe.connect_timeout, service.peer().list_all_tools())
                .await;
        let _ = service.close_with_timeout(CLOSE_GRACE).await;
        match listed {
            Ok(Ok(tools)) => Ok(tools.into_iter().map(remote_tool).collect()),
            Ok(Err(e)) => Err(self.error_of(&e, self.recipe.connect_timeout)),
            Err(_) => Err(EndpointError::Timeout(
                self.recipe.connect_timeout.as_millis(),
            )),
        }
    }

    /// Call the tool `name` with `arguments`.
    ///
    /// The call has **no retry and no idempotency key**, as for any MCP call: an error that is
    /// not a refusal means it may or may not have run.
    ///
    /// # Errors
    ///
    /// [`EndpointError`]: the connection, the token, the time, the exchange, or a protocol error
    /// from the endpoint ([`EndpointError::Rejected`]). A tool that ran and failed is **not** an
    /// error here: it is a [`RemoteResult`] with `is_error`.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Map<String, Value>,
    ) -> Result<RemoteResult, EndpointError> {
        self.call_tool_with(name, arguments, CallOptions::new())
            .await
    }

    /// Call the tool `name` with `arguments`, as [`call_tool`](Self::call_tool), with the request
    /// `_meta` and the time to wait that `options` says: a tool of the thread-tools endpoint that may
    /// take an hour is waited for an hour, not for the policy's call timeout.
    ///
    /// # Errors
    ///
    /// As [`call_tool`](Self::call_tool); [`EndpointError::Timeout`] names the time of this call.
    pub async fn call_tool_with(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        options: CallOptions,
    ) -> Result<RemoteResult, EndpointError> {
        let wait = options.timeout.unwrap_or(self.call_timeout);
        let mut service = self
            .recipe
            .dial()
            .await
            .map_err(|e| self.failed(&e.to_string()))?;
        let mut params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        if !options.meta.is_empty() {
            params.meta = Some(RequestMetaObject(MetaObject::from(options.meta)));
        }
        let called = tokio::time::timeout(wait, service.peer().call_tool_once(params)).await;
        let _ = service.close_with_timeout(CLOSE_GRACE).await;
        match called {
            Ok(Ok(CallToolResponse::Complete(result))) => {
                let output = map_result(&result, &self.recipe.redactor, None);
                let structured = result.structured_content.clone().map(|mut v| {
                    scrub_value(&self.recipe.redactor, &mut v);
                    v
                });
                Ok(RemoteResult {
                    is_error: output.is_error,
                    text: output.content,
                    structured,
                })
            }
            Ok(Ok(_)) => Err(EndpointError::Failed(
                "the endpoint answered in a way this client does not understand (a task to \
                 poll, or a request for more input)"
                    .to_owned(),
            )),
            Ok(Err(e)) => Err(self.error_of(&e, wait)),
            Err(_) => Err(EndpointError::Timeout(wait.as_millis())),
        }
    }

    fn failed(&self, message: &str) -> EndpointError {
        if looks_unauthorized(message) {
            EndpointError::Unauthorized
        } else {
            EndpointError::Failed(self.recipe.scrub(message))
        }
    }

    fn error_of(&self, error: &ServiceError, wait: Duration) -> EndpointError {
        match error {
            ServiceError::McpError(e) => EndpointError::Rejected(self.recipe.scrub(&e.message)),
            ServiceError::Timeout { .. } => EndpointError::Timeout(wait.as_millis()),
            other => self.failed(&adam_error::report(other)),
        }
    }
}

/// Whether the text of a transport failure says the endpoint answered 401 or 403. The library does
/// not give a status code in a type, so the text is read; a wrong guess only changes the
/// wording of an error.
fn looks_unauthorized(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("auth required")
        || lower.contains("authrequired")
        || lower.contains("invalid_token")
}

fn remote_tool(tool: ListedTool) -> RemoteTool {
    let name = tool.name.to_string();
    let description = tool
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            tool.title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("`{name}` from {SERVER}."));
    let mut schema: Map<String, Value> = (*tool.input_schema).clone();
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".to_owned()));
    let meta = tool.meta.map(|m| (*m).clone()).unwrap_or_default();
    let title = tool
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    RemoteTool {
        name,
        title,
        description: cap_text(description, MAX_DESCRIPTION_BYTES),
        input_schema: Value::Object(schema),
        meta,
    }
}

/// `value` with every string scrubbed of what the redactor knows.
fn scrub_value(redactor: &Redactor, value: &mut Value) {
    match value {
        Value::String(text) => *text = redactor.scrub(text),
        Value::Array(items) => items.iter_mut().for_each(|v| scrub_value(redactor, v)),
        Value::Object(map) => map.values_mut().for_each(|v| scrub_value(redactor, v)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str) -> SecretString {
        SecretString::from(text.to_owned())
    }

    #[test]
    fn the_url_goes_through_the_policy_and_the_token_never_shows() {
        let secure = McpPolicy::default();
        for ok in [
            "https://orchestrator.example/thread-tools/t/mcp",
            "http://127.0.0.1:9/thread-tools/t/mcp",
            "http://localhost:9/mcp",
        ] {
            assert!(Endpoint::new(ok, &token("tok-1"), &secure).is_ok(), "{ok}");
        }
        let refused = Endpoint::new(
            "http://orchestrator:8080/thread-tools/t/mcp",
            &token("tok-1"),
            &secure,
        )
        .unwrap_err();
        assert!(
            matches!(&refused, EndpointError::Refused(m) if m.contains("plain http")),
            "{refused}"
        );
        let allowed = McpPolicy::default().allow_insecure(true);
        assert!(
            Endpoint::new(
                "http://orchestrator:8080/thread-tools/t/mcp",
                &token("tok-1"),
                &allowed
            )
            .is_ok()
        );
        // A token in the URL (a mistake of the sender) is scrubbed from what the refusal says.
        let leaked =
            Endpoint::new("ftp://tok-1.example.com/", &token("tok-1"), &allowed).unwrap_err();
        assert!(!leaked.to_string().contains("tok-1"), "{leaked}");
        assert!(matches!(
            Endpoint::new("https://user:pw@example.com/", &token("t"), &secure),
            Err(EndpointError::Refused(_))
        ));
        // A token that cannot be a header value is refused without showing it.
        let bad = Endpoint::new("https://example.com/", &token("bad\ntoken"), &secure).unwrap_err();
        assert!(!bad.to_string().contains("bad"), "{bad}");
    }

    #[test]
    fn debug_shows_the_url_without_a_query_and_never_the_token() {
        let e = Endpoint::new(
            "https://example.com/mcp?x=secret-query",
            &token("tok-77"),
            &McpPolicy::default(),
        )
        .unwrap();
        let shown = format!("{e:?}");
        assert!(shown.contains("https://example.com/mcp"), "{shown}");
        assert!(
            !shown.contains("secret-query") && !shown.contains("tok-77"),
            "{shown}"
        );
    }

    #[test]
    fn a_listed_tool_keeps_its_name_and_gets_an_object_schema_and_a_description() {
        let bare = ListedTool::new_with_raw("a.b".to_owned(), None, Arc::new(Map::new()));
        let tool = remote_tool(bare);
        assert_eq!(tool.name, "a.b");
        assert_eq!(tool.input_schema["type"], "object");
        assert_eq!(tool.description, "`a.b` from thread tools.");
        let spec = tool.spec();
        assert_eq!(spec.name, "a.b");
    }

    #[test]
    fn the_wording_of_a_401_is_recognised() {
        for text in [
            "HTTP status client error (401 Unauthorized)",
            "Auth required, www-authenticate header: Bearer error=\"invalid_token\"",
            "403 Forbidden",
        ] {
            assert!(looks_unauthorized(text), "{text}");
        }
        assert!(!looks_unauthorized("connection refused"));
    }
}
