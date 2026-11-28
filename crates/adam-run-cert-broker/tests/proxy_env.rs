//! The broker's client ignores a proxy from the environment, in a process of its own so the
//! variables are not shared with another test.
//!
//! This client carries the projected token in an `Authorization` header and a CSR naming the run:
//! an `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` inherited from the pod would hand both to another
//! host, so it is built with `.no_proxy()`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::{Path, PathBuf};

use adam_core::RunId;
use adam_core::turn::{CertificateRequest, RunCertIssuer};
use adam_run_cert_broker::BrokerIssuer;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/run-certificate-v1")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn token_file(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("run-cert-proxy-{name}"));
    std::fs::write(&path, "a-projected-token\n").unwrap();
    path
}

fn request() -> CertificateRequest {
    let csr = fixture("request.v1.json")["csr"]
        .as_str()
        .expect("the fixture's csr")
        .to_owned();
    CertificateRequest::new(STANDARD.decode(csr).unwrap()).unwrap()
}

#[tokio::test]
async fn a_proxy_in_the_environment_is_not_used() {
    // A port nothing listens on: a client that used the proxy could not reach the broker.
    for name in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        // SAFETY: this integration test is its own process; nothing else reads the environment here.
        unsafe { std::env::set_var(name, "http://127.0.0.1:1") };
    }

    let run = RunId::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/internal/runs/{run}/certificate")))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("answer.v1.json")))
        .expect(1)
        .mount(&server)
        .await;

    let issuer = BrokerIssuer::new(server.uri(), token_file("a"), run).unwrap();
    let chain = issuer
        .issue(&request())
        .await
        .expect("the certificate request reaches the broker with a proxy in the environment");
    assert!(!chain.certificates().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}
