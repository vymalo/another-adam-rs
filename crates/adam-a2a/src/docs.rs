//! Swagger UI at `GET /docs` and the OpenAPI document at `GET /openapi.json`: public, the same bytes
//! for every caller, built once.
//!
//! The assets are Swagger UI's own, bundled in the binary by `utoipa-swagger-ui` with its `vendored`
//! feature (the zip ships in the `utoipa-swagger-ui-vendored` crate; nothing is downloaded). Only the
//! files the page loads are served, under a Content-Security-Policy that allows this origin alone.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use utoipa_swagger_ui::Config;

/// Where Swagger UI is served.
pub(crate) const DOCS_PATH: &str = "/docs";
/// Where the OpenAPI document is served.
pub(crate) const OPENAPI_PATH: &str = "/openapi.json";

/// The files of Swagger UI's `dist` that its `index.html` loads: the only ones served.
const ASSETS: &[&str] = &[
    "index.html",
    "index.css",
    "swagger-ui.css",
    "swagger-ui-bundle.js",
    "swagger-ui-standalone-preset.js",
    "swagger-initializer.js",
    "favicon-16x16.png",
    "favicon-32x32.png",
];

/// Scripts, styles, images, fonts and requests from this origin only, no frames, no forms, no
/// `<base>`. Inline `style` attributes are allowed because Swagger UI's React components set them;
/// `data:` images because its stylesheet embeds its icons. No script runs from anywhere else and
/// nothing is fetched from another origin (Try it out calls this server).
pub(crate) const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' \
    'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; \
    base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Whether `path` is one of the public routes of the docs: `/docs`, `/docs/`, a file the page
/// loads, or the document.
pub(crate) fn is_docs_path(path: &str) -> bool {
    path == DOCS_PATH
        || path == OPENAPI_PATH
        || path
            .strip_prefix(DOCS_PATH)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|file| file.is_empty() || ASSETS.contains(&file))
}

struct Asset {
    content_type: HeaderValue,
    bytes: Bytes,
}

struct Docs {
    document: Bytes,
    assets: HashMap<&'static str, Asset>,
}

/// The routes of the docs, serving `document` (already serialized).
pub(crate) fn router(document: Bytes) -> Router {
    // Relative to `/docs/`, so the page works wherever the router is nested.
    let config = Arc::new(
        Config::new(["../openapi.json"])
            .use_base_layout()
            .validator_url("none")
            .query_config_enabled(false)
            .persist_authorization(false)
            .try_it_out_enabled(false)
            .deep_linking(true)
            .display_request_duration(true),
    );
    let mut assets = HashMap::new();
    for name in ASSETS {
        match utoipa_swagger_ui::serve(name, config.clone()) {
            Ok(Some(file)) => {
                let content_type = HeaderValue::from_str(&file.content_type)
                    .unwrap_or(HeaderValue::from_static("application/octet-stream"));
                assets.insert(
                    *name,
                    Asset {
                        content_type,
                        bytes: Bytes::from(file.bytes.into_owned()),
                    },
                );
            }
            Ok(None) => tracing::error!(file = name, "Swagger UI does not ship this file"),
            Err(error) => {
                tracing::error!(file = name, %error, "Swagger UI's file cannot be served")
            }
        }
    }
    let docs = Arc::new(Docs { document, assets });
    Router::new()
        .route(OPENAPI_PATH, get(openapi))
        .route(DOCS_PATH, get(redirect))
        .route("/docs/", get(index))
        .route("/docs/{file}", get(asset))
        .with_state(docs)
        .layer(middleware::from_fn(headers))
}

async fn openapi(State(docs): State<Arc<Docs>>) -> Response {
    (
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        docs.document.clone(),
    )
        .into_response()
}

/// `/docs` to `/docs/`, with a relative `Location` so a nested router keeps its prefix.
async fn redirect() -> Response {
    (StatusCode::SEE_OTHER, [(LOCATION, "docs/")]).into_response()
}

async fn index(state: State<Arc<Docs>>) -> Response {
    asset(state, Path("index.html".to_owned())).await
}

async fn asset(State(docs): State<Arc<Docs>>, Path(file): Path<String>) -> Response {
    match docs.assets.get(file.as_str()) {
        Some(asset) => (
            [(CONTENT_TYPE, asset.content_type.clone())],
            asset.bytes.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_page_its_files_and_the_document_are_docs_paths() {
        for path in [
            "/docs",
            "/docs/",
            "/docs/index.html",
            "/docs/swagger-ui-bundle.js",
            "/docs/swagger-initializer.js",
            "/openapi.json",
        ] {
            assert!(is_docs_path(path), "{path}");
        }
        for path in [
            "/docs/oauth2-redirect.html",
            "/docs/swagger-ui-es-bundle.js",
            "/docs/../tasks",
            "/docs/index.html/x",
            "/docsx",
            "/openapi.yaml",
            "/",
        ] {
            assert!(!is_docs_path(path), "{path}");
        }
    }
}
