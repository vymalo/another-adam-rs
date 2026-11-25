//! Every model request signed (ADR 0068, amended 2026-11-25): a client whose credentials are
//! `SignedRequests` sends `Turn-Signature` on every call, streamed or not, the retry after a `401`
//! included, and what the mock server received verifies with the key set: the run of `X-Run-Id`,
//! the method, the request target and the SHA-256 of the exact body it got. A client that cannot
//! sign sends nothing.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use adam_core::turn::{KeySet, ModelCall, TurnKeys, verify_request};
use adam_model::{
    InjectedRun, Message, ModelClient, ModelRequest, PromptStatement, RenewingCredentials,
    SignedRequests,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::StreamExt;
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ROOT: &str = "run-root-signed";

fn statement(jti: &str) -> String {
    format!(
        "{}.{}.c2ln",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"EdDSA"}"#),
        URL_SAFE_NO_PAD.encode(format!(r#"{{"jti":"{jti}"}}"#))
    )
}

fn answer() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}]
    }))
}

fn sse() -> ResponseTemplate {
    let body = [
        r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// Every received request carries a `Turn-Signature` that verifies over what was received.
fn assert_signed(requests: &[wiremock::Request], set: &KeySet, prompt_jti: Option<&str>) {
    let now = jiff::Timestamp::now().as_second();
    let mut nonces = Vec::new();
    for r in requests {
        let text = |name: &str| r.headers.get(name).map(|v| v.to_str().unwrap().to_owned());
        let jws = text("turn-signature").expect("every call is signed");
        let run = text("x-run-id").expect("every call names its run");
        let target = match r.url.query() {
            Some(q) => format!("{}?{q}", r.url.path()),
            None => r.url.path().to_owned(),
        };
        let verified = verify_request(
            set,
            &jws,
            &ModelCall {
                run: &run,
                method: r.method.as_str(),
                path: &target,
                body: &r.body,
                prompt_jti,
            },
            now,
        )
        .expect("the signature covers the request as received");
        nonces.push(verified.nonce);
    }
    nonces.sort();
    nonces.dedup();
    assert_eq!(nonces.len(), requests.len(), "each call signed anew");
}

#[tokio::test]
async fn every_call_carries_a_signature_of_the_request_as_sent() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    for streaming in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/gw/v1/chat/completions"))
            .respond_with(if streaming { sse() } else { answer() })
            .mount(&server)
            .await;
        let mut config =
            OpenAiConfig::new(format!("{}/gw/v1/", server.uri()), SecretString::from(""));
        config.timeout = Duration::from_secs(10);
        let client = OpenAiCompatible::new(config)
            .unwrap()
            .with_credentials(Arc::new(SignedRequests::new(
                Arc::new(InjectedRun::new()),
                keys.clone(),
            )));
        for words in ["one", "two"] {
            let mut req = ModelRequest::new("gw-model");
            req.messages
                .push(Message::user_text(format!("{words} é 😀")));
            req.run = Some(ROOT.to_owned());
            req.prompt_statement = Some(PromptStatement::new(statement("jti-9"), None).unwrap());
            if streaming {
                let _: Vec<_> = client.stream(req).await.unwrap().collect().await;
            } else {
                client.complete(req).await.unwrap();
            }
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_signed(&requests, &set, Some("jti-9"));
    }
}

/// The retry after a `401` is a new request: signed anew.
#[tokio::test]
async fn the_retry_after_a_401_is_signed_anew() {
    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let set = keys.key_set().await.unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer token-1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer token-2"))
        .respond_with(answer())
        .mount(&server)
        .await;
    let config = OpenAiConfig::new(format!("{}/v1", server.uri()), SecretString::from(""));
    let client = OpenAiCompatible::new(config)
        .unwrap()
        .with_credentials(Arc::new(SignedRequests::new(
            Arc::new(RenewingCredentials::new()),
            keys,
        )));
    let mut req = ModelRequest::new("m");
    req.messages.push(Message::user_text("hi"));
    req.run = Some(ROOT.to_owned());
    client.complete(req).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_signed(&requests, &set, None);
}

/// A call that names no run is not signed and not sent.
#[tokio::test]
async fn a_call_it_cannot_sign_is_not_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer())
        .mount(&server)
        .await;
    let config = OpenAiConfig::new(format!("{}/v1", server.uri()), SecretString::from("k"));
    let client = OpenAiCompatible::new(config)
        .unwrap()
        .with_credentials(Arc::new(SignedRequests::new(
            Arc::new(RenewingCredentials::new()),
            <dyn TurnKeys>::ephemeral().unwrap(),
        )));
    let mut req = ModelRequest::new("m");
    req.messages.push(Message::user_text("hi"));
    assert!(client.complete(req).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}
