//! A model call through an injector names its root run and holds no key (ADR 0067): every call of
//! `InjectedRun` carries `X-Run-Id` and no `Authorization`, the retry after a `401` included, and a
//! call that names no run sends nothing. Against a local mock server.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_model::{
    AfterRejection, CredentialHeaders, CredentialRequest, InjectedRun, Message, ModelClient,
    ModelCredentials, ModelDelta, ModelError, ModelRequest, Rejection,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use async_trait::async_trait;
use futures::StreamExt;
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A key in the configuration, which the injector must never send.
const KEY: &str = "sk-config-key";
const ROOT: &str = "run-root-7f3a";

fn client(server: &MockServer) -> OpenAiCompatible {
    let mut config = OpenAiConfig::new(format!("{}/v1", server.uri()), SecretString::from(KEY));
    config.timeout = Duration::from_secs(10);
    OpenAiCompatible::new(config)
        .expect("client")
        .with_credentials(Arc::new(InjectedRun::new()))
}

fn request(run: Option<&str>) -> ModelRequest {
    let mut req = ModelRequest::new("gw-model");
    req.messages.push(Message::user_text("hi"));
    req.run = run.map(str::to_owned);
    req
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

async fn serve(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(server)
        .await;
}

/// Each request's `X-Run-Id` and whether it had an `Authorization`.
fn seen(requests: &[wiremock::Request]) -> Vec<(Option<String>, bool)> {
    requests
        .iter()
        .map(|r| {
            (
                r.headers
                    .get("x-run-id")
                    .map(|v| v.to_str().unwrap().to_owned()),
                r.headers.contains_key("authorization"),
            )
        })
        .collect()
}

#[tokio::test]
async fn every_call_names_the_run_and_sends_no_key() {
    let server = MockServer::start().await;
    serve(&server, answer()).await;
    let client = client(&server);

    assert_eq!(
        client
            .complete(request(Some(ROOT)))
            .await
            .unwrap()
            .message
            .text(),
        "hello"
    );
    client.complete(request(Some(ROOT))).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        seen(&requests),
        vec![
            (Some(ROOT.to_owned()), false),
            (Some(ROOT.to_owned()), false)
        ]
    );
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(
        !body.contains(ROOT),
        "the run is a header, never the body: {body}"
    );
}

#[tokio::test]
async fn a_stream_names_the_run_and_sends_no_key() {
    let server = MockServer::start().await;
    serve(&server, sse()).await;

    let items: Vec<_> = client(&server)
        .stream(request(Some(ROOT)))
        .await
        .unwrap()
        .collect()
        .await;

    assert!(
        items
            .iter()
            .any(|i| matches!(i, Ok(ModelDelta::Finished(r)) if r.message.text() == "hello"))
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(seen(&requests), vec![(Some(ROOT.to_owned()), false)]);
}

#[tokio::test]
async fn a_call_that_names_no_run_sends_nothing() {
    let server = MockServer::start().await;
    serve(&server, answer()).await;
    let client = client(&server);

    let err = client.complete(request(None)).await.unwrap_err();
    assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
    assert!(
        err.to_string()
            .contains("a model call through the injector names no run"),
        "{err}"
    );
    let Err(err) = client.stream(request(None)).await else {
        panic!("a stream with no run must fail before it is sent");
    };
    assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        0,
        "nothing is sent"
    );
}

/// The injector, asking for the retry after a `401` (the injector itself gives up), and recording
/// the run and the attempt of each call it is asked about.
#[derive(Default)]
struct Retrying {
    asked: Mutex<Vec<(Option<String>, u32)>>,
}

#[async_trait]
impl ModelCredentials for Retrying {
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        self.asked
            .lock()
            .unwrap()
            .push((call.run.map(str::to_owned), call.attempt));
        InjectedRun::new().headers(call).await
    }

    async fn rejected(&self, call: CredentialRequest<'_>, _: &Rejection) -> AfterRejection {
        assert_eq!(call.run, Some(ROOT), "the rejected call names its run");
        AfterRejection::Retry
    }
}

#[tokio::test]
async fn the_retry_after_a_401_names_the_run_too() {
    for streaming in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        serve(&server, if streaming { sse() } else { answer() }).await;
        let provider = Arc::new(Retrying::default());
        let client = client(&server).with_credentials(provider.clone());

        if streaming {
            let items: Vec<_> = client
                .stream(request(Some(ROOT)))
                .await
                .unwrap()
                .collect()
                .await;
            assert!(items.iter().all(Result::is_ok), "{items:?}");
        } else {
            client.complete(request(Some(ROOT))).await.unwrap();
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            seen(&requests),
            vec![
                (Some(ROOT.to_owned()), false),
                (Some(ROOT.to_owned()), false)
            ],
            "streaming: {streaming}"
        );
        assert_eq!(
            *provider.asked.lock().unwrap(),
            vec![(Some(ROOT.to_owned()), 0), (Some(ROOT.to_owned()), 1)]
        );
    }
}
