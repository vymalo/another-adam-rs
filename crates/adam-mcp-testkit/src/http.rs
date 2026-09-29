//! [`TestHttpServer`]: the [`TestServer`](crate::TestServer) over streamable HTTP on a local port,
//! with a bearer-token check and a record of what came in.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{Counters, TestServer};

/// What the server saw, shared by every incarnation of it (a [`restart`](TestHttpServer::restart)
/// keeps counting).
#[derive(Default)]
struct Seen {
    requests: AtomicUsize,
    posts: AtomicUsize,
    counters: Arc<Counters>,
    authorizations: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct Gate {
    seen: Arc<Seen>,
    token: Option<String>,
}

async fn gate(State(gate): State<Gate>, request: Request, next: Next) -> Response {
    gate.seen.requests.fetch_add(1, Ordering::SeqCst);
    if request.method() == Method::POST {
        gate.seen.posts.fetch_add(1, Ordering::SeqCst);
    }
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(given) = &given {
        gate.seen
            .authorizations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(given.clone());
    }
    if let Some(token) = &gate.token
        && given.as_deref() != Some(format!("Bearer {token}").as_str())
    {
        let mut response = (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    next.run(request).await
}

/// An MCP server on `127.0.0.1`, at `/mcp`, that can be stopped and started again on the same
/// port. With a token, a request without `Authorization: Bearer <token>` is answered 401.
pub struct TestHttpServer {
    addr: SocketAddr,
    gate: Gate,
    running: Option<Running>,
}

struct Running {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl TestHttpServer {
    /// Start a server. With `Some(token)` every request must carry `Authorization: Bearer
    /// <token>`.
    pub async fn start(require_token: Option<&str>) -> Self {
        let gate = Gate {
            seen: Arc::default(),
            token: require_token.map(str::to_owned),
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let addr = listener.local_addr().expect("the bound address");
        let running = Self::serve(listener, &gate);
        Self {
            addr,
            gate,
            running: Some(running),
        }
    }

    fn serve(listener: TcpListener, gate: &Gate) -> Running {
        let stop = CancellationToken::new();
        let counters = Arc::clone(&gate.seen.counters);
        let service = StreamableHttpService::new(
            move || Ok(TestServer::counting(Arc::clone(&counters))),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default().with_cancellation_token(stop.child_token()),
        );
        let app = Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn_with_state(gate.clone(), self::gate));
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await;
        });
        Running { stop, task }
    }

    /// The URL to put in an `mcp.json`.
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// The `Authorization` header of every request that had one, in order.
    pub fn authorizations(&self) -> Vec<String> {
        self.gate
            .seen
            .authorizations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many requests came in, of any kind, accepted or not.
    pub fn requests(&self) -> usize {
        self.gate.seen.requests.load(Ordering::SeqCst)
    }

    /// How many `POST` requests came in, accepted or not, across restarts: every JSON-RPC request
    /// and notification a client sends is one (a request that is sent again after a `404` counts
    /// twice, which is how a test proves a client did not send it again).
    pub fn posts(&self) -> usize {
        self.gate.seen.posts.load(Ordering::SeqCst)
    }

    /// How many MCP `initialize` requests were served (accepted by the token check), across
    /// restarts.
    pub fn initializations(&self) -> usize {
        self.gate
            .seen
            .counters
            .initializations
            .load(Ordering::SeqCst)
    }

    /// How many tool calls reached the handler, across restarts (a call that never finishes,
    /// like `slow`, counts as soon as it arrives).
    pub fn calls(&self) -> usize {
        self.gate.seen.counters.calls.load(Ordering::SeqCst)
    }

    /// Stop serving: the listener closes and the open sessions end. The port is kept for
    /// [`restart`](Self::restart).
    pub async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop.cancel();
            if tokio::time::timeout(Duration::from_secs(5), running.task)
                .await
                .is_err()
            {
                // Graceful shutdown waits for open connections; the test is over with them.
            }
        }
    }

    /// Serve again on the same port, with no sessions: a client that held one gets a 404.
    pub async fn restart(&mut self) {
        self.stop().await;
        let listener = TcpListener::bind(self.addr)
            .await
            .expect("bind the same port again");
        self.running = Some(Self::serve(listener, &self.gate));
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop.cancel();
            running.task.abort();
        }
    }
}
