//! `GET /internal/activity`: what the run store says about the work of this service's agents.
//!
//! The body is `{"busy": <runs a worker holds a live lease on>, "nextDueAt": <RFC 3339, omitted
//! when no open run is waiting>}`. It carries no run content and no secret. The route sits behind
//! the same bearer authentication as every route but `/healthz`.

use adam_core::{DynStore, store::now};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

/// The route for `agents`, over `store`.
pub(crate) fn router(store: DynStore, agents: Vec<String>) -> Router {
    Router::new()
        .route("/internal/activity", get(activity))
        .with_state((store, agents))
}

async fn activity(
    State((store, agents)): State<(DynStore, Vec<String>)>,
) -> Result<Json<Value>, StatusCode> {
    match store.activity(&agents, now()).await {
        Ok(activity) => {
            crate::store_busy::record_store_busy(
                &opentelemetry::global::meter(crate::store_busy::SCOPE),
                activity.busy,
            );
            let mut body = json!({ "busy": activity.busy });
            if let Some(at) = activity.next_due_at {
                body["nextDueAt"] = Value::String(at.to_string());
            }
            Ok(Json(body))
        }
        Err(error) => {
            tracing::warn!(error = %adam_error::report(&error), "reading the store's activity failed");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}
