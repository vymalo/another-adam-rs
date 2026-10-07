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
use crate::auth::{self, AuthConfig, Authenticator, JWKS_PATH};
use crate::backend::DynTaskBackend;
use crate::card::{AgentCardConfig, Flags, build_card, build_extended_card};
use crate::handler::BackendHandler;
use crate::push::PushSupport;
use crate::signing::CardSigner;

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
    /// Push notifications (see [`PushSupport`]). `None`, or a policy that allows no webhook: they
    /// are off, the card says so, and the four push methods answer `PushNotificationNotSupported`.
    pub push: Option<PushSupport>,
    /// Signs the public card and the extended card (see [`CardSigner`]). `None`: unsigned.
    pub card_signer: Option<CardSigner>,
}

impl ServerOptions {
    /// Also keep idle SSE streams alive every `interval`.
    #[must_use]
    pub fn with_keepalive_interval(mut self, interval: Duration) -> Self {
        self.keepalive_interval = Some(interval);
        self
    }

    /// Turn push notifications on, for the webhooks `push`'s policy allows. The card advertises
    /// `pushNotifications: true` only when the policy allows at least one. Run the delivery loop
    /// ([`PushSupport::deliverer`]) beside the server: this only accepts and stores the configs.
    #[must_use]
    pub fn with_push(mut self, push: PushSupport) -> Self {
        self.push = Some(push);
        self
    }

    /// Sign the public card, and the extended card if there is one, with `signer`, and serve the
    /// public key at `GET /.well-known/jwks.json`.
    #[must_use]
    pub fn with_card_signer(mut self, signer: CardSigner) -> Self {
        self.card_signer = Some(signer);
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
/// | `GET /.well-known/jwks.json` (only when the card is signed) | public |
/// | `POST /` JSON-RPC: `SendMessage`, `SendStreamingMessage` (SSE), `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask` (SSE), the four push-notification methods (when push is on), `GetExtendedAgentCard` (when configured) | required |
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
        let mut authenticator = Authenticator::new(auth);
        let bearer = authenticator.requires_bearer();

        // Push is on only when the deployment allowed a webhook; the extended card only when it
        // is configured **and** callers authenticate (an anonymous server has no "authenticated").
        let push = options.push.clone().filter(PushSupport::is_enabled);
        let extended_config = card.extended.as_ref().filter(|e| !e.is_empty());
        if extended_config.is_some() && !bearer {
            tracing::warn!(
                "an extended agent card is configured but the server does not authenticate callers: it is not served"
            );
        }
        let extended_config = extended_config.filter(|_| bearer);
        let flags = Flags {
            bearer,
            push: push.is_some(),
            extended: extended_config.is_some(),
        };
        let mut agent_card = build_card(&card, flags);
        let mut extended_card = extended_config.map(|e| build_extended_card(&card, e, flags));
        if let Some(signer) = &options.card_signer {
            agent_card = sign_or_warn(signer, agent_card, "public");
            extended_card = extended_card.map(|c| sign_or_warn(signer, c, "extended"));
            authenticator = authenticator.with_public_jwks();
        }
        let authenticator = Arc::new(authenticator);

        let declared: Arc<[String]> = card.extension_uris().into();
        let handler = BackendHandler::new(
            backend,
            card.extension_uris(),
            push,
            extended_card.map(Arc::new),
        );
        let mut rpc = jsonrpc_router(handler)
            .layer(middleware::from_fn_with_state(declared, echo_extensions))
            .layer(middleware::from_fn(json_rpc_rejections));
        if let Some(interval) = options.keepalive_interval {
            rpc = rpc.layer(middleware::from_fn_with_state(interval, keepalive));
        }

        let mut router = Router::new()
            .merge(rpc)
            .merge(agent_card_router(Arc::new(StaticAgentCard::new(
                agent_card,
            ))))
            .merge(Self::health_router());
        if let Some(signer) = &options.card_signer {
            let jwks = Arc::new(signer.jwks());
            router = router.route(
                JWKS_PATH,
                get(move || {
                    let jwks = jwks.clone();
                    async move { Json((*jwks).clone()) }
                }),
            );
        }
        router
            // Outermost, and over everything (including the fallback), so a
            // route added later is protected unless `auth::is_public` says so.
            .layer(middleware::from_fn_with_state(
                authenticator,
                auth::authenticate,
            ))
    }
}

/// Sign `card`; if signing fails (it does not with a key that loaded) serve it unsigned and say
/// so at error level, so the operator sees it. A client that requires a signature refuses the card.
fn sign_or_warn(signer: &CardSigner, card: a2a::AgentCard, which: &str) -> a2a::AgentCard {
    match signer.sign_card(card.clone()) {
        Ok(signed) => signed,
        Err(error) => {
            tracing::error!(card = which, error = %adam_error::report(&error), "signing the agent card failed; it is served unsigned");
            card
        }
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
    if let Some(id) = legacy_push_config(&bytes) {
        // The SDK reads a request as proto3 JSON, which has no such member: it would be dropped, the
        // task would start, and the client would wait for notifications that were never registered.
        let error = a2a::A2AError::invalid_params(
            "configuration.pushNotificationConfig is the name of an earlier draft: send \
             configuration.taskPushNotificationConfig",
        )
        .to_jsonrpc_error();
        return (StatusCode::OK, Json(a2a::JsonRpcResponse::error(id, error))).into_response();
    }
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

/// The id of a `SendMessage` or `SendStreamingMessage` request whose configuration names the push
/// notification config the way earlier drafts of the protocol did (`pushNotificationConfig`), and not
/// the 1.0 way (`taskPushNotificationConfig`).
fn legacy_push_config(body: &[u8]) -> Option<a2a::JsonRpcId> {
    use a2a::jsonrpc::methods::{SEND_MESSAGE, SEND_STREAMING_MESSAGE};
    // Cheap first: almost every request does not contain the word.
    const LEGACY: &[u8] = b"\"pushNotificationConfig\"";
    if !body.windows(LEGACY.len()).any(|w| w == LEGACY) {
        return None;
    }
    let request: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !matches!(
        request["method"].as_str(),
        Some(SEND_MESSAGE | SEND_STREAMING_MESSAGE)
    ) {
        return None;
    }
    let configuration = &request["params"]["configuration"];
    if configuration.get("pushNotificationConfig").is_none()
        || configuration.get("taskPushNotificationConfig").is_some()
    {
        return None;
    }
    Some(serde_json::from_value(request["id"].clone()).unwrap_or(a2a::JsonRpcId::Null))
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
