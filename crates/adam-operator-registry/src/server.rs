//! The router: `GET` and `HEAD` of the registry path, behind one static bearer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adam_operator_ports::AgentDirectory;
use axum::Router;
use axum::extract::State;
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderMap, HeaderName, HeaderValue,
    IF_NONE_MATCH, RETRY_AFTER, VARY, WWW_AUTHENTICATE,
};
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;

use crate::document::{self, MEDIA_TYPE, RFC9727_PROFILE};
use crate::token::Token;

/// The registry's path (the contract recommends it; a deployment may serve the document anywhere).
pub const PATH: &str = "/registry/v1/agents";

/// How long a reader may keep a copy: 30 seconds. The contract asks for 60 or less.
pub const MAX_AGE_SECS: u32 = 30;

/// What serves the registry: the directory it reads, the token it demands, and where it says it is.
pub struct Registry<D> {
    directory: D,
    token: Token,
    anchor: Option<String>,
    full: Arc<AtomicBool>,
}

impl<D: AgentDirectory + 'static> Registry<D> {
    /// A registry over `directory`. A caller that does not present `token` gets `401` and no body.
    pub fn new(directory: D, token: Token) -> Self {
        Self {
            directory,
            token,
            anchor: None,
            full: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The URL of the document itself, sent as its `anchor` (the contract: a server SHOULD, a client
    /// ignores it). Left unset, there is no `anchor`.
    #[must_use]
    pub fn with_anchor(mut self, anchor: impl Into<String>) -> Self {
        self.anchor = Some(anchor.into());
        self
    }

    /// A flag this registry sets whenever it has built a document that the contract's limits refuse
    /// (`true`), and clears when it has built one that fits (`false`). The controller reads the same
    /// flag to say `Listed: False`, reason `RegistryFull`, which is how a service learns that it is not
    /// listed because the registry would not truncate.
    #[must_use]
    pub fn with_full_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.full = flag;
        self
    }

    /// Read the directory and build the document, setting the full flag. This is the whole of a
    /// request but its headers; the operator also calls it on a timer so the flag is true to the
    /// directory when nobody asks.
    ///
    /// # Errors
    ///
    /// [`Outcome::NotReady`] when the directory cannot answer, [`Outcome::Full`] past a limit.
    pub async fn document(&self) -> Result<document::Built, Outcome> {
        let entries = self.directory.list().await.map_err(|e| {
            tracing::warn!("the registry cannot read the directory: {e}");
            Outcome::NotReady
        })?;
        match document::build(&entries, self.anchor.as_deref()) {
            Ok(built) => {
                self.full.store(false, Ordering::Release);
                for s in &built.skipped {
                    tracing::warn!("{} is not in the registry: {}", s.entry, s.reason);
                }
                Ok(built)
            }
            Err(overflow) => {
                if !self.full.swap(true, Ordering::AcqRel) {
                    tracing::error!("{overflow}: the registry lists nothing until it fits");
                }
                Err(Outcome::Full(overflow))
            }
        }
    }

    /// The router: `GET` (and so `HEAD`) of [`PATH`]. Everything else is `404`, or `405` for another method.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new().route(PATH, get(serve::<D>)).with_state(self)
    }
}

/// Why no document could be had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The directory has not synced or cannot be read: `503`, never an empty list that looks true.
    NotReady,
    /// The document would pass a limit of the contract: `503`, and the full flag is set.
    Full(document::Overflow),
}

/// `401` with no body.
fn unauthorised() -> Response<axum::body::Body> {
    (
        StatusCode::UNAUTHORIZED,
        [
            (WWW_AUTHENTICATE, HeaderValue::from_static("Bearer")),
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (VARY, HeaderValue::from_static("Authorization")),
        ],
    )
        .into_response()
}

/// `503` with no body, which a client takes for "the registry is unavailable".
fn unavailable() -> Response<axum::body::Body> {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            (RETRY_AFTER, HeaderValue::from_static("15")),
        ],
    )
        .into_response()
}

/// The token of `Authorization: Bearer <token>`. The scheme is case-insensitive (RFC 9110 §11.1).
fn presented(headers: &HeaderMap) -> Option<&[u8]> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        // Two credentials are no credential.
        return None;
    }
    let value = value.as_bytes();
    let space = value.iter().position(|b| *b == b' ')?;
    let (scheme, rest) = value.split_at(space);
    scheme
        .eq_ignore_ascii_case(b"bearer")
        .then(|| rest.trim_ascii())
}

/// Whether `If-None-Match` holds the validator: `*`, or a list of entity tags one of which is it. The
/// comparison of this header is the weak one (RFC 9110 §13.1.2), so `W/` is left off.
fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers.get_all(IF_NONE_MATCH).iter().any(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',').map(str::trim).any(|candidate| {
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
            })
        })
    })
}

fn caching(response: &mut Response<axum::body::Body>, etag: &str) {
    let headers = response.headers_mut();
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_str(&format!("private, max-age={MAX_AGE_SECS}"))
            .unwrap_or_else(|_| HeaderValue::from_static("private, max-age=30")),
    );
    headers.insert(VARY, HeaderValue::from_static("Authorization"));
    if let Ok(value) = HeaderValue::from_str(etag) {
        headers.insert(ETAG, value);
    }
}

async fn serve<D: AgentDirectory + 'static>(
    State(registry): State<Arc<Registry<D>>>,
    headers: HeaderMap,
) -> Response<axum::body::Body> {
    if !presented(&headers).is_some_and(|t| registry.token.matches(t)) {
        return unauthorised();
    }
    let built = match registry.document().await {
        Ok(built) => built,
        Err(_) => return unavailable(),
    };
    if not_modified(&headers, &built.etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        caching(&mut response, &built.etag);
        return response;
    }
    let content_type = format!("{MEDIA_TYPE}; profile=\"{RFC9727_PROFILE}\"");
    let mut response = (
        StatusCode::OK,
        [(
            CONTENT_TYPE,
            HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static(MEDIA_TYPE)),
        )],
        built.body,
    )
        .into_response();
    caching(&mut response, &built.etag);
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}
