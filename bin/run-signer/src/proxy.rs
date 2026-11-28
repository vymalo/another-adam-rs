//! The sidecar itself: one fallback route that signs and forwards every request.
//!
//! The flow, in order, is the order of the guarantees: the run's id (exactly one), then the person
//! and the prompt id (exactly one statement), then the target as received — refused if `Url::parse`
//! would rewrite it — then the body, then the run's keyring, then the signature, then the gateway. A
//! request that fails an early step is refused **before** anything is signed or forwarded, so a
//! request with no run is never signed (there would be no key to sign it with) and never reaches the
//! gateway.
//!
//! Every refusal is an RFC 9457 problem document (`application/problem+json`) with `type`, `title`,
//! `status` and `detail`, and one `warn` log line whose reason is the same detail. Neither holds a
//! header value, a run id, a key, a certificate or a body byte.
//!
//! The gateway's answer is relayed as a stream ([`axum::body::Body::from_stream`]), so a model's
//! server-sent events reach the agent as they are produced: nothing buffers a response.

use std::future::{Future, IntoFuture};
use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::{ModelCall, TurnKeyError, TurnKeys, sign_request};
use adam_error::{Classify, ErrorClass};
use adam_model::{PROMPT_STATEMENT_HEADER, PROMPT_USER_SIGNATURE_HEADER, RUN_HEADER};
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

/// The refusal of a target this sidecar would not forward byte for byte: `Url::parse` normalises
/// `..`, `%2e%2e` and a backslash, and percent-encodes `{}`, so a target it rewrote would be signed
/// over one path and sent over another.
const NOT_FORWARDED_UNCHANGED: &str =
    "the request target is not one this sidecar forwards unchanged";

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
/// The grace bounds the **drain**, never the serving: a sidecar answers requests for as long as it
/// runs, and only a shutdown starts the clock.
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
    let (draining, drain_began) = tokio::sync::oneshot::channel();
    let server = axum::serve(listener, signer.router()).with_graceful_shutdown(async move {
        shutdown.await;
        // The drain has begun: the timer below starts now, not at startup.
        let _ = draining.send(());
    });
    // `WithGracefulShutdown` is `IntoFuture`, not a `Future`: pin what it becomes, so the `select!`
    // below polls the server itself.
    let server = IntoFuture::into_future(server);
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.map_err(|source| SignerError::Serve { source }),
        () = async {
            if drain_began.await.is_err() {
                // The server ended without a shutdown: this arm must never fire.
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(grace).await;
        } => {
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
    let headers = &parts.headers;

    // 1. The run. A request that names none is refused here, before anything is signed, and one that
    // names two is refused rather than signed over one and forwarded with both.
    if headers.get_all(RUN_HEADER).iter().count() > 1 {
        return answer(400, "the request names its run twice");
    }
    let Some(run_text) = header(headers, RUN_HEADER) else {
        return answer(400, "the request carries no X-Run-Id");
    };
    if run_text.len() > MAX_RUN_ID_LEN {
        return answer(400, "the request's X-Run-Id is too long");
    }
    let Some(run) = run_id(run_text) else {
        return answer(400, "the request's X-Run-Id is not a run id");
    };

    // 2. The person and the prompt the signature binds.
    if headers.get_all(PROMPT_STATEMENT_HEADER).iter().count() > 1 {
        return answer(400, "the request names its prompt statement twice");
    }
    let Some(statement) = header(headers, PROMPT_STATEMENT_HEADER) else {
        return answer(400, "the request carries no Prompt-Statement");
    };
    let identity = match statement::person_and_jti(statement) {
        Ok(identity) => identity,
        Err(error) => return answer(400, error.detail()),
    };

    // 3. The target as received, and the URL it makes. This sidecar signs the target it forwards, so
    // a URL that normalised it is refused before anything is signed.
    let Some(target) = target_of(&parts.uri) else {
        return answer(400, NOT_FORWARDED_UNCHANGED);
    };
    let Some(url) = upstream_url(&state.config.upstream, &target)
        .filter(|url| forwards_unchanged(&state.config.upstream, url, &target))
    else {
        return answer(400, NOT_FORWARDED_UNCHANGED);
    };

    // 4. The body, capped before it is buffered whole.
    let Ok(body) = axum::body::to_bytes(body, state.config.limits.max_body).await else {
        return answer(
            413,
            "the request body is larger than the sidecar's limit, or it was not read whole",
        );
    };

    // 5. The run's key.
    let keyring = match state.keyrings.keyring(run, &identity.person).await {
        Ok(keyring) => keyring,
        Err(refusal) => {
            // The class, never the content: `Refusal::Issuer`'s cause is the factory's own opaque
            // error and may hold anything, and no header value or statement is ever logged.
            tracing::warn!(
                class = ?refusal.class(),
                "the run's keyring was refused"
            );
            return answer(refusal.status(), refusal.detail());
        }
    };

    // 6. The signature over the request as it will be sent.
    let method = parts.method.as_str().to_owned();
    let call = ModelCall {
        run: run_text,
        method: &method,
        path: &target,
        body: &body,
        prompt_jti: Some(&identity.jti),
    };
    let signature = match sign_with_retry(&*keyring, &call).await {
        Ok(signature) => signature,
        Err(error) => {
            // `TurnKeyError` and the `RunKeyError` behind it hold no value, so the chain is logged;
            // no header value and no statement ever is.
            tracing::warn!(
                class = ?error.class(),
                error = %adam_error::report(&error),
                "signing the request failed"
            );
            return signing_refusal(&error);
        }
    };

    // 7. Forward: the same method, path, query and bytes, the agent's headers, our signature.
    let connection = connection_named(headers);
    let user_signature = headers.get(PROMPT_USER_SIGNATURE_HEADER).cloned();
    let mut outgoing = state.config.client.request(parts.method.clone(), url);
    for (name, value) in headers {
        if forwarded(name.as_str(), &connection) {
            outgoing = outgoing.header(name, value);
        }
    }
    // Exactly the values that were validated, once each: the loop skipped both names.
    outgoing = outgoing
        .header(RUN_HEADER, run_text)
        .header(PROMPT_STATEMENT_HEADER, statement);
    if let Some(value) = user_signature {
        outgoing = outgoing.header(PROMPT_USER_SIGNATURE_HEADER, value);
    }
    let outgoing = outgoing
        .header(adam_core::turn::TURN_SIGNATURE_HEADER, &signature)
        .body(body);
    let Ok(response) = outgoing.send().await else {
        return answer(503, "the gateway is unavailable");
    };

    // 8. Relay the answer, streamed.
    relay(response)
}

/// [`sign_request`], tried once more when the keyring renewed between `current()` and the signature:
/// the key it was told to sign with is gone, and the retry signs with the one that replaced it.
async fn sign_with_retry(
    keys: &(impl TurnKeys + ?Sized),
    call: &ModelCall<'_>,
) -> Result<String, TurnKeyError> {
    match sign_request(keys, call).await {
        Err(error) if error.class() == ErrorClass::Conflict => sign_request(keys, call).await,
        answer => answer,
    }
}

/// The refusal of a request whose signature could not be made: `503` for a key that is not there now
/// (a transient issuer, or a renewal that survived the one retry), `502` for anything else.
fn signing_refusal(error: &TurnKeyError) -> Response {
    match error.class() {
        ErrorClass::Transient | ErrorClass::Conflict => {
            answer(503, "the run's key is unavailable now")
        }
        _ => answer(502, "signing the request failed"),
    }
}

/// The request target as received: its path and query, when it is an origin-form target (it starts
/// with `/`). An authority form (`CONNECT gateway:443`) and an asterisk form (`OPTIONS *`) are not
/// something a forward proxy passes on unchanged.
fn target_of(uri: &axum::http::Uri) -> Option<String> {
    let target = uri.path_and_query()?.as_str();
    target.starts_with('/').then(|| target.to_owned())
}

/// `base` with the request's own `target` appended.
fn upstream_url(base: &Url, target: &str) -> Option<Url> {
    if !target.starts_with('/') {
        return None;
    }
    let base = base.as_str().trim_end_matches('/');
    Url::parse(&format!("{base}{target}")).ok()
}

/// Whether `url` is `base`'s path with `target` appended **unchanged**: what remains of the URL's
/// path and query once the base's own path is taken off is the target, byte for byte.
fn forwards_unchanged(base: &Url, url: &Url, target: &str) -> bool {
    let mut actual = url.path().to_owned();
    if let Some(query) = url.query() {
        actual.push('?');
        actual.push_str(query);
    }
    let base_path = base.path().trim_end_matches('/');
    actual.strip_prefix(base_path) == Some(target)
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
/// `Transfer-Encoding`, `Upgrade`, and every name the request's own `Connection` header lists) are
/// the connection's, not the request's; `Host` and `Content-Length` are recomputed by the client for
/// the address and the bytes it sends; and a `Turn-Signature` is dropped because this sidecar signs,
/// so only its signature goes out. The three headers the sidecar validates — `X-Run-Id`,
/// `Prompt-Statement` and `Prompt-User-Signature` — are dropped here and added back once, with the
/// validated value.
fn forwarded(name: &str, connection: &[String]) -> bool {
    !hop_by_hop(name)
        && !connection.iter().any(|named| named == name)
        && !matches!(name, "host" | "content-length")
        && !name.eq_ignore_ascii_case(adam_core::turn::TURN_SIGNATURE_HEADER)
        && !name.eq_ignore_ascii_case(RUN_HEADER)
        && !name.eq_ignore_ascii_case(PROMPT_STATEMENT_HEADER)
        && !name.eq_ignore_ascii_case(PROMPT_USER_SIGNATURE_HEADER)
}

/// Whether a header is the connection's rather than the message's (RFC 9110 §7.6.1).
fn hop_by_hop(name: &str) -> bool {
    name.starts_with("proxy-")
        || matches!(
            name,
            "connection" | "keep-alive" | "te" | "trailer" | "transfer-encoding" | "upgrade"
        )
}

/// The header names the request's `Connection` header lists, lowercased: RFC 9110 §7.6.1 says a
/// proxy must drop them too, whatever they are.
fn connection_named(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|named| named.trim().to_ascii_lowercase())
        .filter(|named| !named.is_empty())
        .collect()
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
        kind: kind(status),
        title: title(status),
        status: status.as_u16(),
        detail,
    };
    let body = serde_json::to_string(&document).unwrap_or_else(|_| {
        format!(
            r#"{{"type":"{}","status":{}}}"#,
            kind(status),
            status.as_u16()
        )
    });
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM))],
        body,
    )
        .into_response()
}

/// The problem's `type`.
///
/// `about:blank` says the status is all there is (RFC 9457 §4.2.1), which `adam-model` reads as no
/// type at all: the run is not parked, and a refusal it could retry is treated as final. So a `503`
/// — "not now, and the same call may work later" — carries `unavailable`, a relative reference
/// `adam-model` reads as a slug and parks the run on a timer for (ADR 0067 decision 3). The others
/// (`400`, `413`, `502`) are the request's or the answer's fault, and stay `about:blank`.
fn kind(status: StatusCode) -> &'static str {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        "unavailable"
    } else {
        "about:blank"
    }
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
        let url = upstream_url(&base, "/v1/chat/completions?x=1").unwrap();
        assert_eq!(url.as_str(), "http://gateway:8080/v1/chat/completions?x=1");
        assert!(forwards_unchanged(&base, &url, "/v1/chat/completions?x=1"));
        let base = Url::parse("http://gateway:8080/model/").unwrap();
        let url = upstream_url(&base, "/v1/chat").unwrap();
        assert_eq!(url.as_str(), "http://gateway:8080/model/v1/chat");
        assert!(forwards_unchanged(&base, &url, "/v1/chat"));
    }

    /// The targets `Url::parse` rewrites are not the ones this sidecar signed: each is refused.
    #[test]
    fn a_target_the_url_normalises_is_not_forwarded_unchanged() {
        let base = Url::parse("http://gateway:8080").unwrap();
        for target in [
            "/v1/../admin/keys",
            "/v1/%2e%2e/admin",
            "/v1/a\\..\\admin",
            "/v1/x{1}",
            "/v1/x y",
        ] {
            let url = upstream_url(&base, target);
            let forwarded = url
                .as_ref()
                .is_some_and(|url| forwards_unchanged(&base, url, target));
            assert!(
                !forwarded,
                "{target:?} was forwarded as {:?}",
                url.map(|u| u.to_string())
            );
        }
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
            "x-run-id",
            "prompt-statement",
            "prompt-user-signature",
        ] {
            assert!(!forwarded(name, &[]), "{name} was forwarded");
        }
        for name in ["content-type", "authorization", "accept"] {
            assert!(forwarded(name, &[]), "{name} was dropped");
        }
        // A name the request's own `Connection` header lists goes too, whatever it is.
        let connection = vec!["x-secret-hop".to_owned(), "x-other".to_owned()];
        assert!(!forwarded("x-secret-hop", &connection));
        assert!(!forwarded("x-other", &connection));
        assert!(forwarded("x-kept", &connection));
    }

    #[test]
    fn a_connection_header_names_what_it_drops() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("X-Hop, keep-alive"));
        headers.append("connection", HeaderValue::from_static("X-Other"));
        assert_eq!(
            connection_named(&headers),
            ["x-hop", "keep-alive", "x-other"]
        );
        assert!(connection_named(&HeaderMap::new()).is_empty());
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
                "type": "unavailable",
                "title": "Service Unavailable",
                "status": 503,
                "detail": "the gateway is unavailable",
            })
        );
    }

    /// A `503` is the one `adam-model` parks the run on (ADR 0067 decision 3): it carries a type
    /// `adam-model` reads as a slug. The others say the status is all there is.
    #[tokio::test]
    async fn only_a_503_names_a_problem_type() {
        for (status, kind) in [
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            (StatusCode::BAD_REQUEST, "about:blank"),
            (StatusCode::PAYLOAD_TOO_LARGE, "about:blank"),
            (StatusCode::BAD_GATEWAY, "about:blank"),
        ] {
            let response = problem(status, "a reason");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["type"], kind, "{status}");
        }
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
        assert_eq!(target_of(&uri).as_deref(), Some("/v1/chat?stream=1"));
        // No target at all is not an origin-form one.
        let uri: axum::http::Uri = "gateway:443".parse().unwrap();
        assert_eq!(target_of(&uri), None);
        let uri: axum::http::Uri = "*".parse().unwrap();
        assert_eq!(target_of(&uri), None);
    }

    /// A keyring that has renewed between `current()` and the signature answers `NotSigning`; the
    /// retry signs with the key that replaced it.
    #[tokio::test]
    async fn a_renewal_between_the_key_and_the_signature_is_retried_once() {
        struct Renewing {
            attempts: std::sync::atomic::AtomicUsize,
            inner: adam_core::turn::Ed25519TurnKeys,
        }

        #[async_trait::async_trait]
        impl TurnKeys for Renewing {
            async fn key_set(
                &self,
            ) -> Result<std::sync::Arc<adam_core::turn::KeySet>, TurnKeyError> {
                self.inner.key_set().await
            }

            async fn current(&self) -> Result<adam_core::turn::CurrentKey, TurnKeyError> {
                self.inner.current().await
            }

            async fn sign(
                &self,
                kid: &adam_core::turn::KeyId,
                input: &[u8],
            ) -> Result<adam_core::turn::Signature, TurnKeyError> {
                let attempt = self
                    .attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if attempt == 0 {
                    return Err(TurnKeyError::NotSigning { kid: kid.clone() });
                }
                self.inner.sign(kid, input).await
            }
        }

        let keys = Renewing {
            attempts: std::sync::atomic::AtomicUsize::new(0),
            inner: adam_core::turn::Ed25519TurnKeys::ephemeral().unwrap(),
        };
        let body = b"{}";
        let call = ModelCall {
            run: "run-1",
            method: "POST",
            path: "/v1/chat/completions",
            body,
            prompt_jti: Some("jti-1"),
        };
        let signature = sign_with_retry(&keys, &call).await.unwrap();
        assert!(!signature.is_empty());
        assert_eq!(
            keys.attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the signing was not retried exactly once"
        );
        // What a 200 would carry: a signature the gateway verifies over the request.
        let set = keys.key_set().await.unwrap();
        adam_core::turn::verify_request(
            &set,
            &signature,
            &call,
            adam_core::time::Timestamp::now().as_second(),
        )
        .expect("the retried signature does not verify");
    }

    /// A `Conflict` that survives the retry is a `503` (the caller may try again), never a `502`.
    #[tokio::test]
    async fn a_surviving_conflict_is_a_503() {
        struct Never(adam_core::turn::Ed25519TurnKeys);

        #[async_trait::async_trait]
        impl TurnKeys for Never {
            async fn key_set(
                &self,
            ) -> Result<std::sync::Arc<adam_core::turn::KeySet>, TurnKeyError> {
                self.0.key_set().await
            }

            async fn current(&self) -> Result<adam_core::turn::CurrentKey, TurnKeyError> {
                self.0.current().await
            }

            async fn sign(
                &self,
                kid: &adam_core::turn::KeyId,
                _input: &[u8],
            ) -> Result<adam_core::turn::Signature, TurnKeyError> {
                Err(TurnKeyError::NotSigning { kid: kid.clone() })
            }
        }

        let keys = Never(adam_core::turn::Ed25519TurnKeys::ephemeral().unwrap());
        let call = ModelCall {
            run: "run-1",
            method: "POST",
            path: "/v1/chat",
            body: b"{}",
            prompt_jti: None,
        };
        let error = sign_with_retry(&keys, &call).await.unwrap_err();
        assert_eq!(error.class(), ErrorClass::Conflict);
        assert_eq!(signing_refusal(&error).status(), 503);
        // An `Unauthenticated` refusal is not retryable and is the gateway-facing 502.
        assert_eq!(
            signing_refusal(&TurnKeyError::Refused("a policy".into())).status(),
            502
        );
    }
}
