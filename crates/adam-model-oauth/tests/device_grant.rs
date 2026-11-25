//! The device grant against a local mock authorization server: sign-in, renewal, rotation, refusal,
//! a 401, and a person who has not answered yet. No network needed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_model::{
    AfterRejection, CredentialRequest, ModelCredentials, ModelError, Park, Refusal, Rejection,
};
use adam_model_oauth::{DeviceGrant, DeviceGrantSettings, SignInCode, SignInPrompt};
use secrecy::ExposeSecret;
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CLIENT: &str = "test-tools";

#[derive(Default)]
struct Shown(Mutex<Vec<SignInCode>>, Mutex<u32>);

impl SignInPrompt for Shown {
    fn show(&self, code: &SignInCode) {
        self.0.lock().unwrap().push(code.clone());
    }
    fn signed_in(&self) {
        *self.1.lock().unwrap() += 1;
    }
}

fn call() -> CredentialRequest<'static> {
    CredentialRequest {
        method: "POST",
        url: "https://models.example.test/v1/chat/completions",
        attempt: 0,
        run: None,
        prompt_statement: None,
    }
}

async fn bearer(grant: &DeviceGrant) -> Result<String, ModelError> {
    let headers = grant.headers(call()).await?;
    let (name, value) = headers.iter().next().expect("one header");
    assert_eq!(name, "authorization");
    Ok(value.expose_secret().to_owned())
}

/// A server with discovery and a device code (interval 1 s, valid 600 s).
async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": server.uri(),
            "device_authorization_endpoint": format!("{}/device_authorization", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/device_authorization"))
        .and(body_string_contains(format!("client_id={CLIENT}")))
        .and(body_string_contains("scope=offline_access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "dc-secret", "user_code": "ABCD-1234",
            "verification_uri": format!("{}/device", server.uri()),
            "verification_uri_complete": format!("{}/device?code=ABCD-1234", server.uri()),
            "expires_in": 600, "interval": 1
        })))
        .mount(&server)
        .await;
    server
}

fn token(access: &str, refresh: &str, expires_in: u64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": access, "token_type": "Bearer", "expires_in": expires_in, "refresh_token": refresh
    }))
}

fn oauth_error(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"error": code}))
}

/// The device-code poll: `pending` answers first, then `then`.
async fn poll(server: &MockServer, pending: u64, then: ResponseTemplate) {
    if pending > 0 {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("device_code=dc-secret"))
            .respond_with(oauth_error("authorization_pending"))
            .up_to_n_times(pending)
            .with_priority(1)
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("device_code=dc-secret"))
        .respond_with(then)
        .with_priority(2)
        .mount(server)
        .await;
}

async fn on_refresh(server: &MockServer, refresh: &str, answer: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains(format!("refresh_token={refresh}")))
        .respond_with(answer)
        .mount(server)
        .await;
}

fn grant(server: &MockServer, prompt: Arc<Shown>, renew_before: Duration) -> DeviceGrant {
    let mut settings = DeviceGrantSettings::new(server.uri(), CLIENT);
    settings.renew_before = renew_before;
    settings.sign_in_wait = Duration::from_secs(20);
    DeviceGrant::new(settings, prompt).unwrap()
}

/// A person's own sign-in never parks a run (ADR 0067 decision 3): a refusal fails as before.
#[tokio::test]
async fn the_device_grant_never_parks() {
    let grant = grant(
        &server().await,
        Arc::new(Shown::default()),
        Duration::from_secs(60),
    );
    for refusal in [
        Refusal::new(401),
        Refusal::new(403).with_slug("run_not_bound"),
        Refusal::new(402).with_slug("over_limit"),
        Refusal::new(429).with_slug("rate_limited"),
    ] {
        assert_eq!(grant.parks(&refusal), Park::No, "{refusal:?}");
    }
}

async fn requests_to(server: &MockServer, p: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == p)
        .count()
}

#[tokio::test]
async fn it_shows_the_code_waits_for_the_person_and_answers_with_the_token() {
    let server = server().await;
    poll(&server, 1, token("at-1", "rt-1", 900)).await;
    let shown = Arc::new(Shown::default());
    let grant = grant(&server, shown.clone(), Duration::from_secs(60));

    grant.start();
    assert_eq!(bearer(&grant).await.unwrap(), "Bearer at-1");
    assert_eq!(
        bearer(&grant).await.unwrap(),
        "Bearer at-1",
        "a valid token is reused"
    );

    let codes = shown.0.lock().unwrap().clone();
    assert_eq!(codes.len(), 1);
    assert_eq!(codes[0].user_code, "ABCD-1234");
    assert_eq!(codes[0].expires_in, Duration::from_secs(600));
    assert_eq!(
        *shown.1.lock().unwrap(),
        1,
        "told once that the person signed in"
    );
    assert!(grant.is_signed_in().await);
    assert_eq!(
        requests_to(&server, "/device_authorization").await,
        1,
        "one code"
    );
    assert_eq!(
        requests_to(&server, "/token").await,
        2,
        "one pending poll, then the token"
    );
}

#[tokio::test]
async fn the_first_call_starts_the_sign_in_when_nobody_did() {
    let server = server().await;
    poll(&server, 0, token("at-1", "rt-1", 900)).await;
    let grant = grant(&server, Arc::new(Shown::default()), Duration::from_secs(60));

    assert_eq!(bearer(&grant).await.unwrap(), "Bearer at-1");
}

#[tokio::test]
async fn it_renews_before_expiry_and_keeps_the_rotated_refresh_token() {
    let server = server().await;
    // Tokens live 100 s and are renewed 200 s early: every call renews.
    poll(&server, 0, token("at-1", "rt-1", 100)).await;
    on_refresh(&server, "rt-1", token("at-2", "rt-2", 100)).await;
    on_refresh(&server, "rt-2", token("at-3", "rt-3", 100)).await;
    let grant = grant(
        &server,
        Arc::new(Shown::default()),
        Duration::from_secs(200),
    );

    grant.start();
    // The sign-in's token is already within the renewal window, so the first call renews it.
    assert_eq!(bearer(&grant).await.unwrap(), "Bearer at-2");
    assert_eq!(bearer(&grant).await.unwrap(), "Bearer at-3");
    let refreshes: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .filter(|b| b.contains("grant_type=refresh_token"))
        .collect();
    assert_eq!(refreshes.len(), 2);
    assert!(refreshes[0].contains("refresh_token=rt-1"));
    assert!(
        refreshes[1].contains("refresh_token=rt-2"),
        "the rotated token is the one spent next"
    );
    assert!(
        refreshes
            .iter()
            .all(|b| b.contains(&format!("client_id={CLIENT}")))
    );
}

#[tokio::test]
async fn a_refused_renewal_signs_out_and_asks_for_no_new_code() {
    let server = server().await;
    poll(&server, 0, token("at-1", "rt-1", 100)).await;
    on_refresh(&server, "rt-1", oauth_error("invalid_grant")).await;
    let grant = grant(
        &server,
        Arc::new(Shown::default()),
        Duration::from_secs(200),
    );
    grant.start();

    // The sign-in's token is within the renewal window: the first call renews, and is refused.
    let result = bearer(&grant).await;
    assert!(
        matches!(&result, Err(ModelError::Auth(m)) if m.contains("invalid_grant")),
        "{result:?}"
    );
    assert!(
        matches!(bearer(&grant).await, Err(ModelError::Auth(_))),
        "signed out stays signed out"
    );
    assert!(!grant.is_signed_in().await);
    assert_eq!(
        requests_to(&server, "/device_authorization").await,
        1,
        "no new code on its own"
    );
}

#[tokio::test]
async fn a_denied_sign_in_is_an_auth_error() {
    let server = server().await;
    poll(&server, 0, oauth_error("access_denied")).await;
    let grant = grant(&server, Arc::new(Shown::default()), Duration::from_secs(60));

    let err = bearer(&grant).await.unwrap_err();
    assert!(
        matches!(&err, ModelError::Auth(m) if m.contains("denied")),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_401_renews_at_once() {
    let server = server().await;
    poll(&server, 0, token("at-1", "rt-1", 900)).await;
    on_refresh(&server, "rt-1", token("at-2", "rt-2", 900)).await;
    let grant = grant(&server, Arc::new(Shown::default()), Duration::from_secs(60));
    assert_eq!(bearer(&grant).await.unwrap(), "Bearer at-1");

    let after = grant
        .rejected(
            call(),
            &Rejection {
                status: 401,
                ..Rejection::default()
            },
        )
        .await;

    assert_eq!(after, AfterRejection::Retry);
    assert_eq!(bearer(&grant).await.unwrap(), "Bearer at-2");
}

#[tokio::test]
async fn a_call_that_waits_too_long_for_the_person_is_transient() {
    let server = server().await;
    poll(&server, 1000, token("at-1", "rt-1", 900)).await; // the person never answers here
    let mut settings = DeviceGrantSettings::new(server.uri(), CLIENT);
    settings.sign_in_wait = Duration::from_millis(300);
    let grant = DeviceGrant::new(settings, Arc::new(Shown::default())).unwrap();

    let err = bearer(&grant).await.unwrap_err();

    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
    assert!(
        !format!("{err}").contains("ABCD-1234"),
        "the code goes to the prompt, not into a run's error"
    );
}

#[tokio::test]
async fn endpoints_on_another_origin_are_refused() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_authorization_endpoint": "https://elsewhere.example.test/device_authorization",
            "token_endpoint": format!("{}/token", server.uri()),
        })))
        .mount(&server)
        .await;
    let grant = grant(&server, Arc::new(Shown::default()), Duration::from_secs(60));

    let err = bearer(&grant).await.unwrap_err();

    assert!(
        matches!(&err, ModelError::Auth(m) if m.contains("another origin")),
        "{err:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "only the discovery document was read"
    );
}

#[tokio::test]
async fn a_server_that_is_down_leaves_the_sign_in_to_be_tried_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let grant = grant(&server, Arc::new(Shown::default()), Duration::from_secs(60));

    let err = bearer(&grant).await.unwrap_err();

    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
    assert!(!grant.is_signed_in().await);
}

#[tokio::test]
async fn with_the_openai_client_a_401_renews_and_the_call_succeeds() {
    use adam_model::{Message, ModelClient, ModelRequest};
    use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
    use secrecy::SecretString;
    use wiremock::matchers::header;

    let idp = server().await;
    poll(&idp, 0, token("at-1", "rt-1", 900)).await;
    on_refresh(&idp, "rt-1", token("at-2", "rt-2", 900)).await;
    let models = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer at-1"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "expired"}})),
        )
        .mount(&models)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer at-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}]
        })))
        .mount(&models)
        .await;
    let grant = grant(&idp, Arc::new(Shown::default()), Duration::from_secs(60));
    grant.start();
    let client = OpenAiCompatible::new(OpenAiConfig::new(
        format!("{}/v1", models.uri()),
        SecretString::from(""),
    ))
    .unwrap()
    .with_credentials(Arc::new(grant));

    let mut req = ModelRequest::new("m");
    req.messages.push(Message::user_text("hi"));
    let resp = client.complete(req).await.unwrap();

    assert_eq!(resp.message.text(), "hello");
    assert_eq!(models.received_requests().await.unwrap().len(), 2);
}
