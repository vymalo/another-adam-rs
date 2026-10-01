//! Assembling the axum router.

use std::sync::Arc;
use std::time::Duration;

use a2a::error_code::{INVALID_REQUEST, PARSE_ERROR};
use a2a_server::StaticAgentCard;
use a2a_server::agent_card::agent_card_router;
use a2a_server::jsonrpc::{MAX_REQUEST_BODY_BYTES, jsonrpc_router};
use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::StreamExt;

use crate::activation::{self, HEADER};
use crate::auth::{self, AuthConfig, Authenticator};
use crate::backend::DynTaskBackend;
use crate::card::{AgentCardConfig, build_card};
use crate::handler::BackendHandler;

/// The SSE comment frame (ignored by clients) used as a keepalive.
const KEEPALIVE_FRAME: &[u8] = b":\n\n";

/// The interval at which the SDK's own SSE layer sends comment frames. It is
/// a constant inside `a2a-server-lf`, not configurable.
pub const SDK_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Optional knobs for [`A2aServer::router_with_options`].
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ServerOptions {
    /// Send an SSE comment frame whenever a stream has been idle this long,
    /// *in addition to* the SDK's fixed 15 s keepalive. Set it below
    /// [`SDK_KEEPALIVE_INTERVAL`] for proxies with short idle timeouts (and for
    /// tests); `None` relies on the SDK's alone.
    pub keepalive_interval: Option<Duration>,
}

impl ServerOptions {
    /// Also keep idle SSE streams alive every `interval`.
    #[must_use]
    pub fn with_keepalive_interval(mut self, interval: Duration) -> Self {
        self.keepalive_interval = Some(interval);
        self
    }
}

/// Exposes an agent, through a [`TaskBackend`](crate::TaskBackend), as an A2A
/// (protocol 1.0) server.
///
/// The router serves:
///
/// | Route | Auth |
/// |---|---|
/// | `GET /.well-known/agent-card.json` | public |
/// | `GET /healthz` | public |
/// | `POST /` JSON-RPC: `SendMessage`, `SendStreamingMessage` (SSE), `GetTask`, `CancelTask`, `SubscribeToTask` (SSE) | required |
///
/// Any other route is also behind authentication (fail closed). Mount the
/// router at the root of a listener, or `nest` it and set
/// [`AgentCardConfig::url`] to the nested public URL.
#[derive(Debug)]
pub struct A2aServer;

impl A2aServer {
    /// Router with: `GET /.well-known/agent-card.json`, the JSON-RPC endpoint
    /// (`SendMessage`, `SendStreamingMessage` (SSE), `GetTask`, `CancelTask`,
    /// `SubscribeToTask` (SSE); the A2A 1.0 names for the issue's
    /// `message/send`, `message/stream`, `tasks/get`, `tasks/cancel`,
    /// `tasks/resubscribe`), and `GET /healthz`.
    pub fn router(card: AgentCardConfig, backend: DynTaskBackend, auth: AuthConfig) -> Router {
        Self::router_with_options(card, backend, auth, ServerOptions::default())
    }

    /// A router with `GET /healthz` alone: the same liveness route [`router`](Self::router)
    /// serves (200, body `ok`, no credential), for a process that has no A2A endpoint but must
    /// still answer probes, such as a worker that runs no front.
    pub fn health_router() -> Router {
        Router::new().route("/healthz", get(healthz))
    }

    /// [`router`](Self::router) with [`ServerOptions`].
    pub fn router_with_options(
        card: AgentCardConfig,
        backend: DynTaskBackend,
        auth: AuthConfig,
        options: ServerOptions,
    ) -> Router {
        let authenticator = Arc::new(Authenticator::new(auth));
        let agent_card = build_card(&card, authenticator.requires_bearer());

        let declared: Arc<[String]> = card.extension_uris().into();
        let mut rpc = jsonrpc_router(BackendHandler::new(backend, card.extension_uris()))
            .layer(middleware::from_fn_with_state(declared, echo_extensions))
            .layer(middleware::from_fn(json_rpc_rejections));
        if let Some(interval) = options.keepalive_interval {
            rpc = rpc.layer(middleware::from_fn_with_state(interval, keepalive));
        }

        Router::new()
            .merge(rpc)
            .merge(agent_card_router(Arc::new(StaticAgentCard::new(
                agent_card,
            ))))
            .merge(Self::health_router())
            // Outermost, and over everything (including the fallback), so a
            // route added later is protected unless `auth::is_public` says so.
            .layer(middleware::from_fn_with_state(
                authenticator,
                auth::authenticate,
            ))
    }
}

/// Say which extensions a request activated, as the A2A specification asks: the response carries
/// an `A2A-Extensions` header that lists them (*verified* 2026-10-01,
/// <https://a2a-protocol.org/latest/topics/extensions/>). The extensions are the ones the request
/// named, in its header or, for a send, in `message.extensions`, that the card declares
/// ([`activation::activated`], the rule the handler fills
/// [`Caller::extensions`](crate::Caller::extensions) with). No header when there are none.
///
/// Inside [`json_rpc_rejections`], so the body it reads is already bounded.
async fn echo_extensions(
    State(declared): State<Arc<[String]>>,
    request: Request,
    next: Next,
) -> Response {
    if declared.is_empty() {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let header: Vec<String> = parts
        .headers
        .get_all(HEADER)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_owned)
        .collect();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES).await else {
        return rejection(INVALID_REQUEST, "invalid request: the body is too large");
    };
    let message = message_extensions(&bytes);
    let activated = activation::activated(
        &declared,
        header.iter().map(String::as_str),
        message.iter().map(String::as_str),
    );
    let mut response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    if !activated.is_empty()
        && let Ok(value) = activated.join(", ").parse()
    {
        response.headers_mut().insert(HEADER, value);
    }
    response
}

/// `params.message.extensions` of a `SendMessage` or `SendStreamingMessage` body; nothing for any
/// other body (a method that carries no message, or something that is not a JSON-RPC request).
fn message_extensions(body: &[u8]) -> Vec<String> {
    use a2a::jsonrpc::methods::{SEND_MESSAGE, SEND_STREAMING_MESSAGE};
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    if !matches!(
        request["method"].as_str(),
        Some(SEND_MESSAGE | SEND_STREAMING_MESSAGE)
    ) {
        return Vec::new();
    }
    request["params"]["message"]["extensions"]
        .as_array()
        .map(|uris| {
            uris.iter()
                .filter_map(|u| u.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Turn the SDK extractor's plain-text rejections into JSON-RPC error objects.
///
/// The SDK reads the body with axum's `Json` extractor, so a body that is not a JSON-RPC request
/// is refused before its handler runs, with a plain-text 400 (not JSON), 415 (not declared as
/// JSON), 422 (well-formed JSON of the wrong shape, or a document that ends early) or 413 (too
/// large). A JSON-RPC client cannot read those. This layer, inside the authentication layer (so an
/// anonymous caller still gets a 401 and learns nothing about how requests are read), answers
/// them the way the SDK answers every other error: HTTP 200 with an error object, code -32700
/// (parse error) for a body that is not JSON and -32600 (invalid request) for the rest, and a null
/// id because none could be read. The extractor's text is dropped; it is not part of the protocol.
///
/// The body is buffered (at most the SDK's own limit) so a 422 can be told apart: the extractor
/// reports `[1,2` as a shape error, but it is not JSON.
async fn json_rpc_rejections(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES).await else {
        return rejection(INVALID_REQUEST, "invalid request: the body is too large");
    };
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes.clone())))
        .await;
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if is_json {
        return response;
    }
    let status = response.status();
    let not_json = || serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).is_err();
    let (code, message) = match status {
        StatusCode::BAD_REQUEST => (PARSE_ERROR, NOT_JSON),
        StatusCode::UNPROCESSABLE_ENTITY if not_json() => (PARSE_ERROR, NOT_JSON),
        StatusCode::UNPROCESSABLE_ENTITY => (
            INVALID_REQUEST,
            "invalid request: the body is not a JSON-RPC request",
        ),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => (
            INVALID_REQUEST,
            "invalid request: the content type must be application/json",
        ),
        StatusCode::PAYLOAD_TOO_LARGE => {
            (INVALID_REQUEST, "invalid request: the body is too large")
        }
        _ => return response,
    };
    tracing::debug!(%status, code, "malformed JSON-RPC request");
    rejection(code, message)
}

const NOT_JSON: &str = "parse error: the body is not valid JSON";
/// HTTP 200 with a JSON-RPC error object and a null id.
fn rejection(code: i32, message: &str) -> Response {
    let error = a2a::A2AError::new(code, message).to_jsonrpc_error();
    let body = a2a::JsonRpcResponse::error(a2a::JsonRpcId::Null, error);
    (StatusCode::OK, Json(body)).into_response()
}

async fn healthz() -> &'static str {
    "ok"
}

/// Wrap SSE responses so an idle stream emits a comment frame every
/// `interval`. The backend's work is independent of this body: dropping it
/// (client disconnect) never cancels a task.
async fn keepalive(State(interval): State<Duration>, request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    let is_sse = response
        .headers()
        .get(CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"text/event-stream"));
    if !is_sse {
        return response;
    }
    let (parts, body) = response.into_parts();
    let stream = futures::stream::unfold(body.into_data_stream(), move |mut data| async move {
        match tokio::time::timeout(interval, data.next()).await {
            Ok(Some(item)) => Some((item, data)),
            Ok(None) => None,
            Err(_idle) => Some((Ok(Bytes::from_static(KEEPALIVE_FRAME)), data)),
        }
    });
    Response::from_parts(parts, Body::from_stream(stream))
}
