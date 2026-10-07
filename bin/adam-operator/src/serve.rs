//! The two small HTTP servers of the operator: health on 8081 (`/healthz`, `/readyz`) and metrics on
//! 9090 (`/metrics`, Prometheus text). `serve` also serves the registry's router on 8080 (`adam-operator-registry`).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use adam_operator_controller::{Metrics, Readiness};
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;

/// `/healthz`: the process is up (liveness). `/readyz`: the controllers have listed the cluster.
pub fn health(readiness: Readiness) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(ready))
        .with_state(readiness)
}

async fn ready(State(readiness): State<Readiness>) -> impl IntoResponse {
    if readiness.is_ready() {
        (StatusCode::OK, "ready\n")
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "the caches have not synced yet\n",
        )
    }
}

/// `/metrics`: the counters of the controllers in the Prometheus text format.
pub fn metrics(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(|State(m): State<Arc<Metrics>>| async move {
                (
                    [(
                        header::CONTENT_TYPE,
                        "text/plain; version=0.0.4; charset=utf-8",
                    )],
                    m.render(),
                )
            }),
        )
        .with_state(metrics)
}

/// Serve `router` on `addr` until `shutdown` resolves.
pub async fn serve(
    addr: SocketAddr,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .with_context(|| format!("serving {addr}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get(router: Router, path: &str) -> (StatusCode, String, Option<String>) {
        let res = router
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let content_type = res
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            String::from_utf8(body.to_vec()).unwrap(),
            content_type,
        )
    }

    #[tokio::test]
    async fn healthz_is_always_up_and_readyz_waits_for_the_caches() {
        let readiness = Readiness::default();
        let router = health(readiness.clone());
        assert_eq!(get(router.clone(), "/healthz").await.0, StatusCode::OK);
        assert_eq!(
            get(router.clone(), "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            get(router.clone(), "/metrics").await.0,
            StatusCode::NOT_FOUND
        );
        // The controllers hold a clone of the same handle and mark it when the caches have synced.
        readiness.mark_ready();
        assert_eq!(get(router, "/readyz").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_are_prometheus_text() {
        let m = Arc::new(Metrics::new());
        m.inc("adam_operator_runtime_signals_total", &[]);
        let (status, body, content_type) = get(metrics(m), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            content_type
                .unwrap()
                .starts_with("text/plain; version=0.0.4")
        );
        assert!(
            body.contains("adam_operator_runtime_signals_total 1"),
            "{body}"
        );
    }
}
