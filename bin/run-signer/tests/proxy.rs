//! The sidecar on a loopback listener, over wiremock (and, for the streaming case, a small axum
//! server) as the gateway: what it forwards, what it refuses, and the signature it adds.
//!
//! No network: every listener is `127.0.0.1`. The signatures are verified against the leaf the
//! sidecar really signed with (the same keyring, read from the registry the server holds), never
//! against a stub.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use adam_core::RunId;
use adam_core::TurnKeys;
use adam_core::time::Timestamp;
use adam_core::turn::run_cert::DynRunCertIssuer;
use adam_core::turn::run_cert::testkit::FakeRunCertIssuer;
use adam_core::turn::{ModelCall, TURN_SIGNATURE_HEADER, verify_request};
use adam_error::BoxError;
use adam_run_signer::{IssuerFactory, Keyrings, Signer, SignerConfig, serve};
use base64::Engine as _;
use bytes::Bytes;
use futures::StreamExt as _;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// -------------------------------------------------------------------------------------- fixtures

struct FakeFactory(Arc<FakeRunCertIssuer>);

impl IssuerFactory for FakeFactory {
    fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
        Ok(self.0.clone())
    }
}

/// A running sidecar and the registry it signs with, so a test can read the run's key itself.
struct Sidecar {
    base: String,
    keyrings: Arc<Keyrings>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Sidecar {
    async fn start(upstream: &str) -> Self {
        let factory: Arc<dyn IssuerFactory> =
            Arc::new(FakeFactory(Arc::new(FakeRunCertIssuer::new())));
        let keyrings = Arc::new(Keyrings::new(Arc::clone(&factory), 16));
        let upstream = upstream.to_owned();
        let config = SignerConfig::from_lookup(
            move |name| match name {
                "RUN_SIGNER_UPSTREAM" => Some(upstream.clone()),
                "RUN_SIGNER_LISTEN" => Some("127.0.0.1:0".to_owned()),
                _ => None,
            },
            factory,
            None,
        )
        .unwrap();
        let signer = Signer::with_keyrings(config, Arc::clone(&keyrings));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = serve(listener, signer, async move {
                let _ = stopped.await;
            })
            .await;
        });
        Self {
            base,
            keyrings,
            stop: Some(stop),
            task,
        }
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

/// A statement whose payload is `payload`: `header.payload.signature`, base64url without padding.
fn statement(payload: Value) -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(br#"{"alg":"EdDSA","typ":"prompt-statement/v1"}"#);
    let payload = engine.encode(serde_json::to_vec(&payload).unwrap());
    let signature = engine.encode(b"a signature");
    format!("{header}.{payload}.{signature}")
}

/// A request that a sidecar should sign, to `base`.
fn request(base: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new().post(format!("{base}/v1/chat/completions"))
}

/// Assert the answer is an RFC 9457 problem document of `status`, with its four members and no
/// header value or run id in it.
async fn problem(response: reqwest::Response, status: u16) -> String {
    assert_eq!(response.status(), status);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "application/problem+json"
    );
    let body = response.text().await.unwrap();
    let value: Value = serde_json::from_str(&body).unwrap();
    let members: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(members, ["detail", "status", "title", "type"], "{body}");
    assert_eq!(value["status"], json!(status), "{body}");
    body
}

// ---------------------------------------------------------------------------------------- cases

/// The acceptance: a signature the sidecar adds verifies against the run's leaf, over the exact
/// bytes the gateway received.
#[tokio::test]
async fn a_signature_it_adds_verifies_against_the_runs_leaf() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&upstream)
        .await;

    let sidecar = Sidecar::start(&upstream.uri()).await;
    let run = RunId::new();
    let run_text = run.to_string();
    let person = "person-1";
    let prompt = statement(json!({"jti": "jti-1", "sub": person}));
    let body = br#"{"model":"test","messages":[{"role":"user","content":"hi"}]}"#.to_vec();

    let response = request(&sidecar.base)
        .header("X-Run-Id", &run_text)
        .header("Prompt-Statement", &prompt)
        .header("Prompt-User-Signature", "the-persons-envelope")
        .header("Content-Type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");

    let seen = upstream.received_requests().await.unwrap();
    assert_eq!(seen.len(), 1, "the gateway did not see exactly one request");
    let seen = &seen[0];
    assert_eq!(seen.body, body, "the body was not forwarded byte for byte");
    assert_eq!(
        seen.headers
            .get("prompt-statement")
            .unwrap()
            .to_str()
            .unwrap(),
        prompt,
        "the statement was not forwarded byte for byte"
    );
    assert_eq!(
        seen.headers
            .get("prompt-user-signature")
            .unwrap()
            .to_str()
            .unwrap(),
        "the-persons-envelope"
    );
    assert_eq!(
        seen.headers.get("x-run-id").unwrap().to_str().unwrap(),
        run_text
    );
    let jws = seen
        .headers
        .get(TURN_SIGNATURE_HEADER)
        .expect("the sidecar added no signature")
        .to_str()
        .unwrap()
        .to_owned();

    // The run's leaf, as the sidecar's own registry has it.
    let keyring = sidecar.keyrings.keyring(run, person).await.unwrap();
    let set = keyring.key_set().await.unwrap();
    let call = ModelCall {
        run: &run_text,
        method: "POST",
        path: "/v1/chat/completions",
        body: &body,
        prompt_jti: Some("jti-1"),
    };
    let verified = verify_request(&set, &jws, &call, Timestamp::now().as_second())
        .expect("the signature does not verify against the run's leaf");
    assert_eq!(verified.kid, *keyring.current().await.unwrap().kid());
}

/// The acceptance: no `X-Run-Id`, no signature, and nothing reaches the gateway.
#[tokio::test]
async fn a_request_without_a_run_id_is_refused_and_nothing_is_forwarded() {
    let upstream = MockServer::start().await;
    let sidecar = Sidecar::start(&upstream.uri()).await;
    let prompt = statement(json!({"jti": "jti-1", "sub": "person-1"}));

    let response = request(&sidecar.base)
        .header("Prompt-Statement", &prompt)
        .body("{}")
        .send()
        .await
        .unwrap();
    let body = problem(response, 400).await;
    assert!(body.contains("X-Run-Id"), "{body}");
    assert!(!body.contains("jti-1"), "{body}");
    assert!(!body.contains(&prompt), "{body}");
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        0,
        "a request with no run reached the gateway"
    );
}

/// A request with a run but no statement is refused, and nothing is forwarded: the person the key is
/// bound to is not optional.
#[tokio::test]
async fn a_request_without_a_prompt_statement_is_refused() {
    let upstream = MockServer::start().await;
    let sidecar = Sidecar::start(&upstream.uri()).await;
    let run_text = RunId::new().to_string();

    let response = request(&sidecar.base)
        .header("X-Run-Id", &run_text)
        .body("{}")
        .send()
        .await
        .unwrap();
    let body = problem(response, 400).await;
    assert!(body.contains("Prompt-Statement"), "{body}");
    assert!(!body.contains(&run_text), "{body}");
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        0,
        "a request with no statement reached the gateway"
    );
}

/// A run is one person's: a second person is refused, and the first person's key is not reused.
#[tokio::test]
async fn a_run_bound_to_one_person_is_refused_for_another() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&upstream)
        .await;
    let sidecar = Sidecar::start(&upstream.uri()).await;
    let run_text = RunId::new().to_string();
    let first = statement(json!({"jti": "jti-1", "sub": "person-1"}));
    let second = statement(json!({"jti": "jti-2", "sub": "person-2"}));

    let ok = request(&sidecar.base)
        .header("X-Run-Id", &run_text)
        .header("Prompt-Statement", &first)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    let refused = request(&sidecar.base)
        .header("X-Run-Id", &run_text)
        .header("Prompt-Statement", &second)
        .body("{}")
        .send()
        .await
        .unwrap();
    let body = problem(refused, 400).await;
    assert!(body.contains("another person"), "{body}");
    assert!(!body.contains("person-2"), "{body}");
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        1,
        "the second person's request reached the gateway"
    );
}

/// An answer that streams is relayed as it is produced: the first event arrives while the gateway
/// still holds the second.
#[tokio::test]
async fn a_streamed_response_is_relayed() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .fallback(streaming_gateway)
        .with_state(Arc::clone(&gate));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let sidecar = Sidecar::start(&upstream).await;
    let response = request(&sidecar.base)
        .header("X-Run-Id", RunId::new().to_string())
        .header(
            "Prompt-Statement",
            statement(json!({"jti": "jti-1", "sub": "person-1"})),
        )
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );

    let mut events = response.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("the first event never arrived: the answer was buffered")
        .unwrap()
        .unwrap();
    assert_eq!(first, Bytes::from_static(b"data: one\n\n"));

    // The gateway releases the second event only now: a sidecar that buffered would still be waiting
    // for the whole body, and the timeout above would have failed.
    gate.notify_one();
    let second = tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("the second event never arrived")
        .unwrap()
        .unwrap();
    assert_eq!(second, Bytes::from_static(b"data: two\n\n"));
    assert!(
        tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .unwrap()
            .is_none(),
        "the stream did not end"
    );

    server.abort();
}

/// The gateway of the streaming case: two server-sent events, the second after `gate` is notified.
async fn streaming_gateway(
    axum::extract::State(gate): axum::extract::State<Arc<tokio::sync::Notify>>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::header;
    use axum::response::IntoResponse as _;

    let first = futures::stream::once(async {
        Ok::<_, std::io::Error>(Bytes::from_static(b"data: one\n\n"))
    });
    let second = futures::stream::once(async move {
        gate.notified().await;
        Ok::<_, std::io::Error>(Bytes::from_static(b"data: two\n\n"))
    });
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(first.chain(second)),
    )
        .into_response()
}
