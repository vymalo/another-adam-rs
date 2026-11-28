//! The sidecar itself: one fallback route that signs and forwards every request.
//!
//! The flow, in order, is the order of the guarantees: the run's id, then the person and the prompt
//! id, then the body, then the run's keyring, then the signature, then the gateway. A request that
//! fails an early step is refused **before** anything is signed or forwarded, so a request with no
//! run is never signed (there would be no key to sign it with) and never reaches the gateway.
//!
//! Every refusal is an RFC 9457 problem document (`application/problem+json`) with `type`, `title`,
//! `status` and `detail`, and one `warn` log line whose reason is the same detail. Neither holds a
//! header value, a run id, a key, a certificate or a body byte.
//!
//! The gateway's answer is relayed as a stream ([`axum::body::Body::from_stream`]), so a model's
//! server-sent events reach the agent as they are produced: nothing buffers a response.

use std::future::Future;
use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::{ModelCall, sign_request};
use adam_error::{Classify, ErrorClass};
use adam_model::{PROMPT_STATEMENT_HEADER, RUN_HEADER};
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::net::TcpListener;
use url::Url;

use crate::config::SignerConfig;
use crate::error::SignerError;
use crate::keyrings::Keyrings;
use crate::statement;

/// The longest `X-Run-Id` accepted, in bytes: a run id is a UUID, and this is room to spare.
pub const MAX_RUN_ID_LEN: usize = 128;

/// The media type of a refusal (RFC 9457 §3).
const PROBLEM: &str = "application/problem+json";

/// The sidecar, built from a [`SignerConfig`] and ready to serve.
pub struct Signer {
    state: Arc<Shared>,
}

struct Shared {
    config: SignerConfig,
    keyrings: Arc<Keyrings>,
}

impl Signer {
    /// A sidecar for `config`, with its own registry of at most [`Limits::max_runs`] keyrings.
    ///
    /// [`Limits::max_runs`]: crate::Limits::max_runs
    pub fn new(config: SignerConfig) -> Self {
        let keyrings = Arc::new(Keyrings::new(
            Arc::clone(&config.issuer),
            config.limits.max_runs,
        ));
        Self::with_keyrings(config, keyrings)
    }

    /// The same, over a registry the caller keeps: what a test uses to read the very keyring the
    /// sidecar signed with.
    pub fn with_keyrings(config: SignerConfig, keyrings: Arc<Keyrings>) -> Self {
        Self {
            state: Arc::new(Shared { config, keyrings }),
        }
    }

    /// The router: one fallback route, for every method and every path.
    pub fn router(self) -> Router {
        Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&self.state))
    }
}

/// Bind `config.listen`, serve until `shutdown` resolves, then wait at most
/// [`Limits::shutdown_grace`](crate::Limits::shutdown_grace) for the requests in flight.
///
/// # Errors
///
/// [`SignerError::Bind`] when the address cannot be bound, [`SignerError::Serve`] when the listener
/// fails while serving.
pub async fn run(
    config: SignerConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), SignerError> {
    let addr = config.listen;
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| SignerError::Bind { addr, source })?;
    let bound = listener
        .local_addr()
        .map_err(|source| SignerError::Bind { addr, source })?;
    tracing::info!(listen = %bound, config = ?config, "adam-run-signer listening");
    serve(listener, Signer::new(config), shutdown).await
}

/// Serve a sidecar on a listener the caller bound (a test asks for port `0` and reads the address
/// back), until `shutdown` resolves. The requests in flight then get
/// [`Limits::shutdown_grace`](crate::Limits::shutdown_grace); when it elapses, whatever is still
/// running is closed, because the process is stopping.
///
/// # Errors
///
/// [`SignerError::Serve`] when the listener fails.
pub async fn serve(
    listener: TcpListener,
    signer: Signer,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), SignerError> {
    let grace = signer.state.config.limits.shutdown_grace;
    let server = axum::serve(listener, signer.router()).with_graceful_shutdown(shutdown);
    match tokio::time::timeout(grace, server).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(SignerError::Serve { source }),
        Err(_) => {
            tracing::warn!(
                grace_secs = grace.as_secs(),
                "the shutdown grace elapsed; the requests still in flight were closed"
            );
            Ok(())
        }
    }
}

/// The one route: read, sign, forward, relay.
async fn handle(State(state): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();

    // 1. The run. A request that names none is refused here, before anything is signed.
    let Some(run_text) = header(&parts.headers, RUN_HEADER) else {
        return answer(400, "the request carries no X-Run-Id");
    };
    if run_text.len() > MAX_RUN_ID_LEN {
        return answer(400, "the request's X-Run-Id is too long");
    }
    let Some(run) = run_id(run_text) else {
        return answer(400, "the request's X-Run-Id is not a run id");
    };

    // 2. The person and the prompt the signature binds.
    let Some(statement) = header(&parts.headers, PROMPT_STATEMENT_HEADER) else {
        return answer(400, "the request carries no Prompt-Statement");
    };
    let identity = match statement::person_and_jti(statement) {
        Ok(identity) => identity,
        Err(error) => return answer(400, error.detail()),
    };

    // 3. The body, capped before it is buffered whole.
    let Ok(body) = axum::body::to_bytes(body, state.config.limits.max_body).await else {
        return answer(
            413,
            "the request body is larger than the sidecar's limit, or it was cut short",
        );
    };

    // 4. The run's key.
    let keyring = match state.keyrings.keyring(run, &identity.person).await {
        Ok(keyring) => keyring,
        Err(refusal) => return answer(refusal.status(), refusal.detail()),
    };

    // 5. The signature over the request as it will be sent.
    let method = parts.method.as_str().to_owned();
    let target = target_of(&parts.uri);
    let call = ModelCall {
        run: run_text,
        method: &method,
        path: &target,
        body: &body,
        prompt_jti: Some(&identity.jti),
    };
    let signature = match sign_request(&*keyring, &call).await {
        Ok(signature) => signature,
        Err(error) => {
            return if matches!(error.class(), ErrorClass::Transient) {
                answer(503, "the run's key is unavailable now")
            } else {
                answer(502, "signing the request failed")
            };
        }
    };

    // 6. Forward: the same method, path, query and bytes, the agent's headers, our signature.
    let Some(url) = upstream_url(&state.config.upstream, &target) else {
        return answer(
            502,
            "the gateway's base URL and the request's path do not make a URL",
        );
    };
    let mut outgoing = state.config.client.request(parts.method.clone(), url);
    for (name, value) in &parts.headers {
        if forwarded(name.as_str()) {
            outgoing = outgoing.header(name, value);
        }
    }
    let outgoing = outgoing
        .header(adam_core::turn::TURN_SIGNATURE_HEADER, &signature)
        .body(body);
    let Ok(response) = outgoing.send().await else {
        return answer(503, "the gateway is unavailable");
    };

    // 7. Relay the answer, streamed.
    relay(response)
}

/// The request target as received: its path and query, `/` when it has neither.
fn target_of(uri: &axum::http::Uri) -> String {
    uri.path_and_query()
        .map_or_else(|| "/".to_owned(), |target| target.as_str().to_owned())
}

/// `base` with the request's own `target` appended.
fn upstream_url(base: &Url, target: &str) -> Option<Url> {
    let base = base.as_str().trim_end_matches('/');
    Url::parse(&format!("{base}{target}")).ok()
}

/// The run id in `text`, when it is one.
fn run_id(text: &str) -> Option<RunId> {
    uuid::Uuid::parse_str(text).ok().map(RunId::from)
}

/// A header's value as text, trimmed, when it is there and not blank.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Whether a request header goes to the gateway.
///
/// The hop-by-hop ones (`Connection`, `Keep-Alive`, `Proxy-*`, `TE`, `Trailer`,
/// `Transfer-Encoding`, `Upgrade`) are the connection's, not the request's; `Host` and
/// `Content-Length` are recomputed by the client for the address and the bytes it sends; and a
/// `Turn-Signature` is dropped because this sidecar signs, so only its signature goes out.
fn forwarded(name: &str) -> bool {
    !hop_by_hop(name)
        && !matches!(name, "host" | "content-length")
        && !name.eq_ignore_ascii_case(adam_core::turn::TURN_SIGNATURE_HEADER)
}

/// Whether a header is the connection's rather than the message's (RFC 9110 §7.6.1).
fn hop_by_hop(name: &str) -> bool {
    name.starts_with("proxy-")
        || matches!(
            name,
            "connection" | "keep-alive" | "te" | "trailer" | "transfer-encoding" | "upgrade"
        )
}

/// The gateway's answer, its headers minus the hop-by-hop ones and `Content-Length`, its body as a
/// stream.
fn relay(response: reqwest::Response) -> Response {
    let status = response.status();
    let headers = response.headers().clone();
    let stream = response.bytes_stream();
    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        if !hop_by_hop(name.as_str()) && name.as_str() != "content-length" {
            builder = builder.header(name, value);
        }
    }
    builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
        // Only a header the response builder refuses fails here, and the headers come from hyper.
        answer(502, "the gateway's answer could not be relayed")
    })
}

/// A refusal: one `warn` line with the reason, and an RFC 9457 problem document with no value in it.
fn answer(status: u16, detail: &'static str) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    tracing::warn!(
        status = status.as_u16(),
        reason = detail,
        "run signer refused a request"
    );
    problem(status, detail)
}

/// The problem document: `{type, title, status, detail}`.
fn problem(status: StatusCode, detail: &'static str) -> Response {
    #[derive(serde::Serialize)]
    struct Document {
        #[serde(rename = "type")]
        kind: &'static str,
        title: &'static str,
        status: u16,
        detail: &'static str,
    }
    let document = Document {
        kind: "about:blank",
        title: title(status),
        status: status.as_u16(),
        detail,
    };
    let body = serde_json::to_string(&document)
        .unwrap_or_else(|_| String::from(r#"{"type":"about:blank","status":500}"#));
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM))],
        body,
    )
        .into_response()
}

/// The reason phrase of `status`.
fn title(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "Bad Request",
        413 => "Content Too Large",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_upstream_url_appends_the_requests_own_target() {
        let base = Url::parse("http://gateway:8080").unwrap();
        assert_eq!(
            upstream_url(&base, "/v1/chat/completions?x=1")
                .unwrap()
                .as_str(),
            "http://gateway:8080/v1/chat/completions?x=1"
        );
        let base = Url::parse("http://gateway:8080/model/").unwrap();
        assert_eq!(
            upstream_url(&base, "/v1/chat").unwrap().as_str(),
            "http://gateway:8080/model/v1/chat"
        );
    }

    #[test]
    fn a_run_id_is_a_uuid_or_nothing() {
        let run = RunId::new();
        assert_eq!(run_id(&run.to_string()), Some(run));
        for bad in ["", "not-a-uuid", "1"] {
            assert_eq!(run_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_connection_headers_do_not_go_out() {
        for name in [
            "connection",
            "keep-alive",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
            "turn-signature",
        ] {
            assert!(!forwarded(name), "{name} was forwarded");
        }
        for name in [
            "content-type",
            "authorization",
            "accept",
            "prompt-statement",
            "prompt-user-signature",
            "x-run-id",
        ] {
            assert!(forwarded(name), "{name} was dropped");
        }
    }

    #[tokio::test]
    async fn the_problem_document_has_the_four_members() {
        let response = problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "the gateway is unavailable",
        );
        assert_eq!(response.status(), 503);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            PROBLEM
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "about:blank",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "the gateway is unavailable",
            })
        );
    }

    #[test]
    fn a_header_is_read_trimmed_and_a_blank_one_is_absent() {
        let mut headers = HeaderMap::new();
        headers.insert("x-run-id", HeaderValue::from_static("  run-1 "));
        assert_eq!(header(&headers, "x-run-id"), Some("run-1"));
        headers.insert("x-run-id", HeaderValue::from_static("   "));
        assert_eq!(header(&headers, "x-run-id"), None);
        assert_eq!(header(&headers, "absent"), None);
    }

    #[test]
    fn a_target_is_its_path_and_query() {
        let uri: axum::http::Uri = "/v1/chat?stream=1".parse().unwrap();
        assert_eq!(target_of(&uri), "/v1/chat?stream=1");
    }
}
