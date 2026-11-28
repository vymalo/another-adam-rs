//! The upstream client ignores a proxy from the environment, in a process of its own so the
//! variables are not shared with another test.
//!
//! A signed request carries the prompt, the person's envelope and a valid `Turn-Signature`: an
//! `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` inherited from the pod would hand all three to another
//! host, so the client is built with `.no_proxy()`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::run_cert::DynRunCertIssuer;
use adam_core::turn::run_cert::testkit::FakeRunCertIssuer;
use adam_error::BoxError;
use adam_run_signer::{IssuerFactory, Signer, SignerConfig, serve};
use base64::Engine as _;
use serde_json::json;
use tokio::net::TcpListener;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

struct FakeFactory(Arc<FakeRunCertIssuer>);

impl IssuerFactory for FakeFactory {
    fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
        Ok(self.0.clone())
    }
}

fn statement(payload: serde_json::Value) -> String {
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = engine.encode(br#"{"alg":"EdDSA","typ":"prompt-statement/v1"}"#);
    let payload = engine.encode(serde_json::to_vec(&payload).unwrap());
    let signature = engine.encode(b"a signature");
    format!("{header}.{payload}.{signature}")
}

#[tokio::test]
async fn a_proxy_in_the_environment_is_not_used() {
    // A port nothing listens on: a client that used the proxy could not reach the gateway.
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
    ] {
        // SAFETY: this integration test is its own process; nothing else reads the environment here.
        unsafe { std::env::set_var(name, "http://127.0.0.1:1") };
    }

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&upstream)
        .await;

    let factory: Arc<dyn IssuerFactory> = Arc::new(FakeFactory(Arc::new(FakeRunCertIssuer::new())));
    let upstream_uri = upstream.uri();
    let config = SignerConfig::from_lookup(
        move |name| match name {
            "RUN_SIGNER_UPSTREAM" => Some(upstream_uri.clone()),
            _ => None,
        },
        factory,
        None,
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = serve(listener, Signer::new(config), async move {
            let _ = stopped.await;
        })
        .await;
    });

    // The test's own client bypasses the proxy too: only the sidecar's client is under test.
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/v1/chat/completions"))
        .header("X-Run-Id", RunId::new().to_string())
        .header(
            "Prompt-Statement",
            statement(json!({"jti": "jti-1", "sub": "person-1"})),
        )
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        200,
        "the signed request did not reach the gateway with a proxy in the environment"
    );
    assert_eq!(response.text().await.unwrap(), "ok");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let _ = stop.send(());
    task.abort();
}
