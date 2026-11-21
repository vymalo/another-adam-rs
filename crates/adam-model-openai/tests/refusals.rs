//! A refusal is read by its problem type and its provider says whether it parks (ADR 0067
//! decision 3): under `InjectedRun` each slug of the table is `Refused` with its park, or
//! `ModelLocked`; under `StaticBearer` the same answers are today's errors. A refused call keeps no
//! body. A connection that cannot be made is `Unreachable`, a timeout is not. Against a local mock
//! server.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use adam_model::{
    Classify, DynCredentials, ErrorClass, InjectedRun, Message, ModelClient, ModelDelta,
    ModelError, ModelRequest, Park, StaticBearer,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use futures::StreamExt;
use secrecy::SecretString;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-config-key";
const ROOT: &str = "run-root-7f3a";
const MODEL: &str = "gw-model";
/// Text a refusal's body holds that no error may show: a credential-looking value and the
/// problem's own words.
const SECRET: &str = "sk-live-SECRET-0f9e8d";
const WORDS: &str = "the lease of this person";

fn timer(secs: u64) -> Park {
    Park::OnTimer(Duration::from_secs(secs))
}

fn injected() -> DynCredentials {
    Arc::new(InjectedRun::new())
}

fn static_bearer() -> DynCredentials {
    Arc::new(StaticBearer::new(SecretString::from(KEY)))
}

fn client_with(url: &str, credentials: DynCredentials, timeout: Duration) -> OpenAiCompatible {
    let mut config = OpenAiConfig::new(format!("{url}/v1"), SecretString::from(KEY));
    config.timeout = timeout;
    OpenAiCompatible::new(config)
        .expect("client")
        .with_credentials(credentials)
}

fn request() -> ModelRequest {
    let mut req = ModelRequest::new(MODEL);
    req.messages.push(Message::user_text("hi"));
    req.run = Some(ROOT.to_owned());
    req
}

/// A problem document whose `type` ends in `slug`, with words and a secret beside it.
fn problem_body(slug: &str) -> String {
    json!({
        "type": format!("https://gateway.example.test/problems/{slug}"),
        "title": WORDS,
        "detail": format!("Authorization: Bearer {SECRET}"),
        "credential": SECRET,
    })
    .to_string()
}

fn problem(status: u16, slug: &str, retry_after: Option<u64>) -> ResponseTemplate {
    let template =
        ResponseTemplate::new(status).set_body_raw(problem_body(slug), "application/problem+json");
    match retry_after {
        Some(secs) => template.insert_header("Retry-After", secs.to_string().as_str()),
        None => template,
    }
}

/// The error of one call answered `template`, completed or streamed, and how many requests reached
/// the server.
async fn refused_with(
    credentials: DynCredentials,
    template: ResponseTemplate,
    streaming: bool,
) -> (ModelError, usize) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(template)
        .mount(&server)
        .await;
    let client = client_with(&server.uri(), credentials, Duration::from_secs(10));
    let err = if streaming {
        match client.stream(request()).await {
            Err(err) => err,
            Ok(_) => panic!("a refusal fails before the stream starts"),
        }
    } else {
        client.complete(request()).await.unwrap_err()
    };
    let sent = server.received_requests().await.unwrap().len();
    (err, sent)
}

/// Nothing of the body in the error: not in its `Display`, its `Debug` or its source chain.
fn assert_holds_no_body(err: &ModelError) {
    let shown = format!("{err} | {err:?} | {}", adam_error::report(err));
    for hidden in [
        SECRET,
        WORDS,
        "Bearer",
        "credential",
        "gateway.example.test",
    ] {
        assert!(!shown.contains(hidden), "{hidden} leaked: {shown}");
    }
    assert!(std::error::Error::source(err).is_none(), "{err:?}");
}

#[tokio::test]
async fn each_slug_of_the_table_parks_under_the_injector() {
    let table: [(u16, &str, Option<u64>, Park); 9] = [
        (401, "unauthenticated", None, timer(30)),
        (403, "run_not_bound", None, timer(30)),
        (403, "lease_expired", None, timer(30)),
        (402, "over_limit", None, Park::UntilWoken),
        (429, "rate_limited", Some(120), timer(120)),
        (429, "rate_limited", Some(5), timer(30)),
        (429, "rate_limited", None, timer(30)),
        (503, "unavailable", Some(90), timer(90)),
        (503, "unavailable", None, timer(30)),
    ];
    for streaming in [false, true] {
        for (status, slug, header, park) in table {
            let retry_after = header.map(Duration::from_secs);
            let (err, sent) =
                refused_with(injected(), problem(status, slug, header), streaming).await;
            let ModelError::Refused {
                status: got_status,
                slug: got_slug,
                retry_after: got_retry_after,
                park: got_park,
            } = &err
            else {
                panic!("{status} {slug} (streaming {streaming}): {err:?}");
            };
            assert_eq!(
                (
                    *got_status,
                    got_slug.as_deref(),
                    *got_retry_after,
                    *got_park
                ),
                (status, Some(slug), retry_after, park),
                "streaming {streaming}"
            );
            assert_eq!(sent, 1, "one call, no retry by the client: {slug}");
            assert_holds_no_body(&err);
        }
    }
}

#[tokio::test]
async fn a_refusal_is_classed_by_its_status_on_the_wire() {
    let class = |err: &ModelError| (err.class(), err.retry_after());
    let (err, _) = refused_with(injected(), problem(403, "run_not_bound", None), false).await;
    assert_eq!(class(&err), (ErrorClass::Unauthenticated, None));
    let (err, _) = refused_with(injected(), problem(402, "over_limit", None), false).await;
    assert_eq!(class(&err), (ErrorClass::Rejected, None));
    let (err, _) = refused_with(injected(), problem(429, "rate_limited", Some(45)), false).await;
    assert_eq!(
        class(&err),
        (ErrorClass::RateLimited, Some(Duration::from_secs(45)))
    );
    let (err, _) = refused_with(injected(), problem(503, "unavailable", Some(45)), false).await;
    assert_eq!(class(&err), (ErrorClass::Transient, None));
}

#[tokio::test]
async fn a_slugless_401_or_403_parks_as_unauthenticated() {
    let plain = |status| {
        ResponseTemplate::new(status).set_body_raw(format!("denied: Bearer {SECRET}"), "text/plain")
    };
    // A `type` in a body that is not a problem document is not read.
    let json_body = |status| {
        ResponseTemplate::new(status).set_body_raw(problem_body("over_limit"), "application/json")
    };
    for status in [401, 403] {
        for (what, template) in [
            ("no body", ResponseTemplate::new(status)),
            ("a plain body", plain(status)),
            ("a JSON body", json_body(status)),
            (
                "a Retry-After",
                ResponseTemplate::new(status).insert_header("Retry-After", "600"),
            ),
        ] {
            let (err, sent) = refused_with(injected(), template, false).await;
            assert!(
                matches!(
                    &err,
                    ModelError::Refused { status: s, slug: None, park, .. }
                        if *s == status && *park == timer(30)
                ),
                "{status} with {what}: {err:?}"
            );
            assert_eq!(sent, 1);
            assert_holds_no_body(&err);
        }
    }
}

#[tokio::test]
async fn model_locked_is_its_own_error_for_every_provider() {
    for credentials in [injected(), static_bearer()] {
        for streaming in [false, true] {
            let (err, _) = refused_with(
                credentials.clone(),
                problem(403, "model_locked", None),
                streaming,
            )
            .await;
            assert!(
                matches!(&err, ModelError::ModelLocked { model } if model == MODEL),
                "{err:?}"
            );
            assert_eq!(err.class(), ErrorClass::Rejected);
            assert!(!err.is_retryable());
            assert_holds_no_body(&err);
        }
    }
}

#[tokio::test]
async fn what_the_injector_does_not_park_is_todays_error() {
    // An unknown slug, a slugless answer of another status, and a problem type in a body that is
    // not a problem document.
    let (err, _) = refused_with(injected(), problem(400, "something_else", None), false).await;
    assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
    let (err, _) = refused_with(injected(), problem(401, "invalid_token", None), false).await;
    assert!(matches!(err, ModelError::Auth(_)), "{err:?}");
    let (err, _) = refused_with(injected(), ResponseTemplate::new(500), false).await;
    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
    let (err, _) = refused_with(
        injected(),
        ResponseTemplate::new(402).set_body_raw(problem_body("over_limit"), "application/json"),
        false,
    )
    .await;
    assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
}

/// A deployment with its own key fails as before (ADR 0058 decision 3): every answer of the table
/// but `model_locked` is the error it was, word for word the error of the same body sent as plain
/// JSON, which no change read.
#[tokio::test]
async fn a_static_key_fails_as_before() {
    let table: [(u16, &str, Option<u64>); 7] = [
        (401, "unauthenticated", None),
        (403, "run_not_bound", None),
        (403, "lease_expired", None),
        (402, "over_limit", None),
        (429, "rate_limited", Some(120)),
        (503, "unavailable", Some(90)),
        (401, "", None),
    ];
    for streaming in [false, true] {
        for (status, slug, header) in table {
            let template = if slug.is_empty() {
                ResponseTemplate::new(status)
            } else {
                problem(status, slug, header)
            };
            let (err, sent) = refused_with(static_bearer(), template, streaming).await;
            assert_eq!(sent, 1);
            let expected = match status {
                401 | 403 => matches!(err, ModelError::Auth(_)),
                402 => matches!(err, ModelError::InvalidRequest { .. }),
                429 => matches!(
                    err,
                    ModelError::RateLimited { retry_after: Some(d) } if d == Duration::from_secs(120)
                ),
                503 => matches!(err, ModelError::Transient { .. }),
                _ => unreachable!(),
            };
            assert!(expected, "{status} {slug} (streaming {streaming}): {err:?}");

            if !slug.is_empty() {
                let mut as_json = ResponseTemplate::new(status)
                    .set_body_raw(problem_body(slug), "application/json");
                if let Some(secs) = header {
                    as_json = as_json.insert_header("Retry-After", secs.to_string().as_str());
                }
                let (before, _) = refused_with(static_bearer(), as_json, streaming).await;
                assert_eq!(err.to_string(), before.to_string(), "{status} {slug}");
            }
        }
    }
}

/// An error object inside a stream is the upstream model's, never a refusal: it does not park.
#[tokio::test]
async fn an_error_inside_a_stream_is_not_a_refusal() {
    let server = MockServer::start().await;
    let body = format!(
        "data: {}\n\n",
        json!({"error": {"type": "over_limit", "message": "over the limit"}})
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let items: Vec<_> = client_with(&server.uri(), injected(), Duration::from_secs(10))
        .stream(request())
        .await
        .unwrap()
        .collect()
        .await;
    let err = items
        .into_iter()
        .find_map(Result::<ModelDelta, _>::err)
        .expect("an error");
    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
}

/// A port with nothing listening: bind, then drop, so it is very likely closed.
fn closed_port() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

#[tokio::test]
async fn a_refused_connection_is_unreachable() {
    for credentials in [injected(), static_bearer()] {
        let client = client_with(&closed_port(), credentials, Duration::from_secs(10));
        let err = client.complete(request()).await.unwrap_err();
        assert!(matches!(err, ModelError::Unreachable { .. }), "{err:?}");
        assert_eq!(err.class(), ErrorClass::Transient);
        assert!(err.is_retryable());
        let Err(err) = client.stream(request()).await else {
            panic!("nothing to stream from");
        };
        assert!(matches!(err, ModelError::Unreachable { .. }), "{err:?}");
    }
}

#[tokio::test]
async fn a_timeout_is_not_unreachable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(3))
                .set_body_json(json!({"choices": [{"message": {"content": "late"}}]})),
        )
        .mount(&server)
        .await;
    let client = client_with(&server.uri(), injected(), Duration::from_millis(200));
    let err = client.complete(request()).await.unwrap_err();
    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
    let Err(err) = client.stream(request()).await else {
        panic!("expected a timeout");
    };
    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
}
