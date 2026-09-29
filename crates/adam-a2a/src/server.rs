//! Assembling the axum router.

use std::sync::Arc;
use std::time::Duration;

use a2a_server::StaticAgentCard;
use a2a_server::agent_card::agent_card_router;
use a2a_server::jsonrpc::jsonrpc_router;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use futures::StreamExt;

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

    /// [`router`](Self::router) with [`ServerOptions`].
    pub fn router_with_options(
        card: AgentCardConfig,
        backend: DynTaskBackend,
        auth: AuthConfig,
        options: ServerOptions,
    ) -> Router {
        let authenticator = Arc::new(Authenticator::new(auth));
        let agent_card = build_card(&card, authenticator.requires_bearer());

        let mut rpc = jsonrpc_router(BackendHandler::new(backend));
        if let Some(interval) = options.keepalive_interval {
            rpc = rpc.layer(middleware::from_fn_with_state(interval, keepalive));
        }

        Router::new()
            .merge(rpc)
            .merge(agent_card_router(Arc::new(StaticAgentCard::new(
                agent_card,
            ))))
            .route("/healthz", get(healthz))
            // Outermost, and over everything (including the fallback), so a
            // route added later is protected unless `auth::is_public` says so.
            .layer(middleware::from_fn_with_state(
                authenticator,
                auth::authenticate,
            ))
    }
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
