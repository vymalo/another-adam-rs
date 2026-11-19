//! The credentials of each call come from a provider (ADR 0058): asked per call, told about a 401,
//! one retry at most, never a header that frames the request. Against a local mock server.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_model::{
    AfterRejection, Classify, CredentialHeaders, CredentialRequest, ErrorClass, Message,
    ModelClient, ModelCredentials, ModelDelta, ModelError, ModelRequest, Rejection,
    RenewingCredentials,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use async_trait::async_trait;
use futures::StreamExt;
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-config-key";

fn base(server: &MockServer) -> OpenAiCompatible {
    let mut config = OpenAiConfig::new(format!("{}/v1", server.uri()), SecretString::from(KEY));
    config.timeout = Duration::from_secs(10);
    OpenAiCompatible::new(config).expect("client")
}

fn request() -> ModelRequest {
    let mut req = ModelRequest::new("gw-model");
    req.messages.push(Message::user_text("hi"));
    req
}

fn answer() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}]
    }))
}

fn refused() -> ResponseTemplate {
    ResponseTemplate::new(401)
        .insert_header("www-authenticate", r#"Bearer error="invalid_token""#)
        .insert_header("dpop-nonce", "n-1")
        .set_body_json(json!({"error": {"message": "token expired"}}))
}

async fn on_token(server: &MockServer, token: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(response)
        .mount(server)
        .await;
}

fn bearers(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .map(|r| {
            r.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default()
        })
        .collect()
}

#[tokio::test]
async fn a_provider_replaces_the_key_and_is_asked_for_every_call() {
    let server = MockServer::start().await;
    on_token(&server, "token-1", answer()).await;
    let credentials = Arc::new(RenewingCredentials::new());
    let client = base(&server).with_credentials(credentials.clone());

    client.complete(request()).await.unwrap();
    client.complete(request()).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(bearers(&requests), vec!["Bearer token-1", "Bearer token-1"]);
    assert_eq!(
        credentials.issued(),
        2,
        "asked once per call, never cached by the client"
    );
}

#[tokio::test]
async fn a_401_renews_once_and_sends_the_same_body_again() {
    let server = MockServer::start().await;
    on_token(&server, "token-1", refused()).await;
    on_token(&server, "token-2", answer()).await;
    let credentials = Arc::new(RenewingCredentials::new());
    let client = base(&server).with_credentials(credentials.clone());

    let resp = client.complete(request()).await.unwrap();

    assert_eq!(resp.message.text(), "hello");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(bearers(&requests), vec!["Bearer token-1", "Bearer token-2"]);
    assert_eq!(
        requests[0].body, requests[1].body,
        "the retry sends the same request"
    );
    assert_eq!(
        credentials.rejections(),
        vec![Rejection {
            status: 401,
            www_authenticate: Some(r#"Bearer error="invalid_token""#.into()),
            dpop_nonce: Some("n-1".into()),
        }]
    );
}

#[tokio::test]
async fn a_second_401_is_the_answer() {
    let server = MockServer::start().await;
    on_token(&server, "token-1", refused()).await;
    on_token(&server, "token-2", refused()).await;
    let credentials = Arc::new(RenewingCredentials::new());

    let err = base(&server)
        .with_credentials(credentials.clone())
        .complete(request())
        .await
        .unwrap_err();

    assert!(matches!(err, ModelError::Auth(_)), "{err:?}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "one retry, never more"
    );
    assert_eq!(
        credentials.rejections().len(),
        1,
        "the provider hears of the first 401 only"
    );
}

#[tokio::test]
async fn the_configured_key_is_never_retried() {
    let server = MockServer::start().await;
    on_token(&server, KEY, refused()).await;

    let err = base(&server).complete(request()).await.unwrap_err();

    assert!(matches!(err, ModelError::Auth(_)), "{err:?}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_stream_renews_too() {
    let server = MockServer::start().await;
    on_token(&server, "token-1", refused()).await;
    let body = [
        r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    on_token(
        &server,
        "token-2",
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;

    let stream = base(&server)
        .with_credentials(Arc::new(RenewingCredentials::new()))
        .stream(request())
        .await
        .unwrap();
    let items: Vec<_> = stream.collect().await;

    let finished = items.iter().find_map(|i| match i {
        Ok(ModelDelta::Finished(resp)) => Some(resp.message.text()),
        _ => None,
    });
    assert_eq!(finished.as_deref(), Some("hello"));
}

/// Fails the way the test says, or gives the headers it was built with; records the calls.
struct Scripted {
    result: Result<CredentialHeaders, fn() -> ModelError>,
    seen: Mutex<Vec<(String, String, u32)>>,
}

#[async_trait]
impl ModelCredentials for Scripted {
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        self.seen
            .lock()
            .unwrap()
            .push((call.method.to_owned(), call.url.to_owned(), call.attempt));
        match &self.result {
            Ok(headers) => Ok(headers.clone()),
            Err(make) => Err(make()),
        }
    }

    async fn rejected(&self, _: CredentialRequest<'_>, _: &Rejection) -> AfterRejection {
        AfterRejection::GiveUp
    }
}

fn scripted(result: Result<CredentialHeaders, fn() -> ModelError>) -> Arc<Scripted> {
    Arc::new(Scripted {
        result,
        seen: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn a_proof_provider_is_told_the_method_and_the_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "DPoP at-1"))
        .and(header("dpop", "proof-1"))
        .respond_with(answer())
        .expect(1)
        .mount(&server)
        .await;
    let provider = scripted(Ok(CredentialHeaders::none()
        .with("authorization", SecretString::from("DPoP at-1"))
        .with("dpop", SecretString::from("proof-1"))));

    base(&server)
        .with_credentials(provider.clone())
        .complete(request())
        .await
        .unwrap();

    let seen = provider.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(
            "POST".to_owned(),
            format!("{}/v1/chat/completions", server.uri()),
            0
        )]
    );
}

#[tokio::test]
async fn a_provider_without_a_key_sends_no_authorization() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("x-run", "run-7"))
        .respond_with(answer())
        .expect(1)
        .mount(&server)
        .await;
    let provider = scripted(Ok(
        CredentialHeaders::none().with("x-run", SecretString::from("run-7"))
    ));

    base(&server)
        .with_credentials(provider)
        .complete(request())
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests[0].headers.get("authorization").is_none(),
        "the injector's mode: no key at all"
    );
}

#[tokio::test]
async fn a_provider_error_is_the_calls_and_nothing_is_sent() {
    let server = MockServer::start().await;
    on_token(&server, "anything", answer()).await;

    let signed_out = scripted(Err(|| ModelError::Auth("signed out".into())));
    let err = base(&server)
        .with_credentials(signed_out)
        .complete(request())
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Unauthenticated, "{err:?}");
    assert!(!err.is_retryable());

    let issuer_down = scripted(Err(|| ModelError::transient("the issuer did not answer")));
    let err = base(&server)
        .with_credentials(issuer_down)
        .complete(request())
        .await
        .unwrap_err();
    assert!(err.is_retryable(), "{err:?}");

    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_provider_may_not_frame_the_request() {
    let server = MockServer::start().await;
    on_token(&server, "anything", answer()).await;

    for name in [
        "Content-Type",
        "content-length",
        "accept",
        "host",
        "transfer-encoding",
    ] {
        let provider = scripted(Ok(
            CredentialHeaders::none().with(name, SecretString::from("x"))
        ));
        let err = base(&server)
            .with_credentials(provider)
            .complete(request())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ModelError::InvalidRequest { .. }),
            "{name}: {err:?}"
        );
    }
    let bad_value = scripted(Ok(
        CredentialHeaders::none().with("x-ok", SecretString::from("a\nb"))
    ));
    let err = base(&server)
        .with_credentials(bad_value)
        .complete(request())
        .await
        .unwrap_err();
    assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
    assert!(
        !format!("{err}").contains("a\nb"),
        "the value is never in the error"
    );

    assert!(server.received_requests().await.unwrap().is_empty());
}
