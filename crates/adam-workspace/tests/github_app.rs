//! `GitHubApp` against a mock server (no network): the JWT it signs, the installation token it
//! trades it for, how long it keeps it, and what it says when GitHub refuses.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping
#![cfg(all(feature = "github", feature = "test-util"))]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_workspace::testing::TestAppKey;
use adam_workspace::{
    AppKey, CodeHost, GitCredentials, GitHub, GitHubApp, HostScoped, NewPullRequest, RepoRef,
    WorkspaceError,
};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const APP_ID: &str = "12345";
const INSTALLATION: u64 = 67890;
const MINT_PATH: &str = "/app/installations/67890/access_tokens";

fn repo() -> RepoRef {
    RepoRef::new("https://github.com/octo/widgets.git", "main")
}

/// A clock a test moves.
#[derive(Clone)]
struct TestClock(Arc<Mutex<DateTime<Utc>>>);

impl TestClock {
    fn at(rfc3339: &str) -> Self {
        Self(Arc::new(Mutex::new(
            DateTime::parse_from_rfc3339(rfc3339)
                .unwrap()
                .with_timezone(&Utc),
        )))
    }

    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }

    fn advance(&self, by: TimeDelta) {
        *self.0.lock().unwrap() += by;
    }

    fn as_fn(&self) -> impl Fn() -> DateTime<Utc> + Send + Sync + 'static {
        let clock = self.clone();
        move || clock.now()
    }
}

/// What GitHub does with the trade: checks the JWT against the App's public key, then answers `201`
/// with token number n (`ghs_mock<n>`), good for an hour from the clock's now.
struct Mint {
    key: Arc<TestAppKey>,
    clock: TestClock,
    minted: Arc<AtomicUsize>,
    /// The claims of every JWT it accepted.
    claims: Arc<Mutex<Vec<Value>>>,
    delay: Duration,
}

impl Respond for Mint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let jwt = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        match self.key.verify_jwt(jwt) {
            Ok(claims) => {
                self.claims.lock().unwrap().push(claims);
                let n = self.minted.fetch_add(1, Ordering::SeqCst) + 1;
                ResponseTemplate::new(201)
                    .set_delay(self.delay)
                    .set_body_json(json!({
                        "token": format!("ghs_mock{n}"),
                        "expires_at": (self.clock.now() + TimeDelta::hours(1))
                            .to_rfc3339_opts(SecondsFormat::Secs, true),
                        "permissions": {"contents": "write", "pull_requests": "write"},
                    }))
            }
            Err(why) => ResponseTemplate::new(401).set_body_json(json!({
                "message": format!("A JSON web token could not be decoded: {why}"),
            })),
        }
    }
}

struct Rig {
    server: MockServer,
    key: Arc<TestAppKey>,
    clock: TestClock,
    minted: Arc<AtomicUsize>,
    claims: Arc<Mutex<Vec<Value>>>,
}

impl Rig {
    async fn new() -> Self {
        Self::with_delay(Duration::ZERO).await
    }

    async fn with_delay(delay: Duration) -> Self {
        let server = MockServer::start().await;
        let key = Arc::new(TestAppKey::generate());
        let clock = TestClock::at("2026-10-01T12:00:00Z");
        let minted = Arc::new(AtomicUsize::new(0));
        let claims = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path(MINT_PATH))
            .and(header("accept", "application/vnd.github+json"))
            .and(header("x-github-api-version", "2022-11-28"))
            .respond_with(Mint {
                key: key.clone(),
                clock: clock.clone(),
                minted: minted.clone(),
                claims: claims.clone(),
                delay,
            })
            .mount(&server)
            .await;
        Self {
            server,
            key,
            clock,
            minted,
            claims,
        }
    }

    fn app(&self) -> GitHubApp {
        GitHubApp::new(
            self.server.uri(),
            APP_ID,
            INSTALLATION,
            AppKey::from_pem(&self.key.pkcs1_pem).unwrap(),
        )
        .unwrap()
        .with_clock(self.clock.as_fn())
    }

    async fn token(&self, app: &GitHubApp) -> String {
        app.token_for(&repo())
            .await
            .unwrap()
            .expose_secret()
            .to_owned()
    }
}

#[tokio::test]
async fn a_token_is_minted_with_a_jwt_the_apps_key_signed_and_is_what_the_rest_api_gets() {
    let rig = Rig::new().await;
    let app = HostScoped::new(["github.com"], rig.app());
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/pulls"))
        .and(header("authorization", "Bearer ghs_mock1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&rig.server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/pulls"))
        .and(header("authorization", "Bearer ghs_mock1"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": 7,
            "html_url": "https://github.com/octo/widgets/pull/7",
            "head": {"ref": "agent/x"},
            "base": {"ref": "main"},
        })))
        .expect(1)
        .mount(&rig.server)
        .await;
    let github = GitHub::new(Arc::new(app))
        .unwrap()
        .with_api_base(rig.server.uri());
    let pr = github
        .open_pull_request(NewPullRequest {
            repo: repo(),
            head: "agent/x".to_owned(),
            title: "t".to_owned(),
            body: "b".to_owned(),
            draft: false,
        })
        .await
        .unwrap();
    assert_eq!(pr.number, 7);
    // The listing and the creation both used the one token; one JWT was traded for it, signed with
    // the App's key (the mock checked the signature), saying what GitHub asks for.
    assert_eq!(rig.minted.load(Ordering::SeqCst), 1);
    let claims = rig.claims.lock().unwrap();
    assert_eq!(claims.len(), 1);
    let now = rig.clock.now().timestamp();
    assert_eq!(claims[0]["iss"], 12345);
    assert_eq!(
        claims[0]["iat"],
        now - 60,
        "a minute ago, against clock drift"
    );
    assert_eq!(
        claims[0]["exp"],
        now + 540,
        "nine minutes ahead: at most ten"
    );
}

#[tokio::test]
async fn a_client_id_is_the_issuer_as_a_string_and_a_pkcs8_key_signs_too() {
    let rig = Rig::new().await;
    let app = GitHubApp::new(
        rig.server.uri(),
        "Iv23liClientId",
        INSTALLATION,
        AppKey::from_pem(&rig.key.pkcs8_pem).unwrap(),
    )
    .unwrap()
    .with_clock(rig.clock.as_fn());
    assert_eq!(rig.token(&app).await, "ghs_mock1");
    assert_eq!(rig.claims.lock().unwrap()[0]["iss"], "Iv23liClientId");
}

#[tokio::test]
async fn a_token_is_kept_until_five_minutes_before_it_expires_and_then_replaced() {
    let rig = Rig::new().await;
    let app = rig.app();
    assert_eq!(rig.token(&app).await, "ghs_mock1");
    // Six minutes to go: the same token, nothing asked of GitHub.
    rig.clock.advance(TimeDelta::minutes(54));
    assert_eq!(rig.token(&app).await, "ghs_mock1");
    assert_eq!(rig.minted.load(Ordering::SeqCst), 1);
    // Just under five minutes to go: a new token, from a new JWT that says the new time.
    rig.clock
        .advance(TimeDelta::minutes(1) + TimeDelta::seconds(1));
    assert_eq!(rig.token(&app).await, "ghs_mock2");
    assert_eq!(rig.minted.load(Ordering::SeqCst), 2);
    {
        let claims = rig.claims.lock().unwrap();
        assert_eq!(claims[1]["iat"], rig.clock.now().timestamp() - 60);
        assert_ne!(claims[0]["iat"], claims[1]["iat"]);
    }
    // And the new one is kept in its turn.
    rig.clock.advance(TimeDelta::minutes(30));
    assert_eq!(rig.token(&app).await, "ghs_mock2");
    assert_eq!(rig.minted.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sixteen_callers_at_once_share_one_request() {
    let rig = Rig::with_delay(Duration::from_millis(300)).await;
    let app = Arc::new(rig.app());
    let calls: Vec<_> = (0..16)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                app.token_for(&repo())
                    .await
                    .unwrap()
                    .expose_secret()
                    .to_owned()
            })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap(), "ghs_mock1");
    }
    assert_eq!(
        rig.minted.load(Ordering::SeqCst),
        1,
        "one POST for sixteen callers"
    );
    assert_eq!(rig.server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_foreign_host_is_refused_before_anything_is_minted() {
    let rig = Rig::new().await;
    let scoped = HostScoped::new(["github.com", "ghe.example.com:8443"], rig.app());
    for url in [
        "https://evil.example/octo/widgets.git",
        "https://github.com.evil.example/octo/widgets.git",
        "https://ghe.example.com/octo/widgets.git",
        "http://127.0.0.1:9/octo/widgets.git",
        "/tmp/a/remote.git",
        "file:///tmp/a/remote.git",
    ] {
        let err = scoped
            .token_for(&RepoRef::new(url, "main"))
            .await
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::Invalid(_)), "{url}: {err:?}");
        assert!(!err.to_string().contains("ghs_"), "{err}");
    }
    assert!(
        rig.server.received_requests().await.unwrap().is_empty(),
        "nothing was asked of GitHub for a host that is not allowed"
    );
    // The allowed hosts get a token, a name matching any port.
    for url in [
        "https://github.com/octo/widgets.git",
        "https://ghe.example.com:8443/octo/widgets.git",
    ] {
        assert!(
            scoped.token_for(&RepoRef::new(url, "main")).await.is_ok(),
            "{url}"
        );
    }
    assert_eq!(rig.minted.load(Ordering::SeqCst), 1, "the token is shared");
}

/// A mock that answers every trade with `response`.
async fn refusing(response: ResponseTemplate) -> (MockServer, GitHubApp) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(response)
        .mount(&server)
        .await;
    let key = TestAppKey::generate();
    let app = GitHubApp::new(
        server.uri(),
        APP_ID,
        INSTALLATION,
        AppKey::from_pem(&key.pkcs1_pem).unwrap(),
    )
    .unwrap();
    (server, app)
}

#[tokio::test]
async fn a_refusal_names_the_variables_and_is_not_retried() {
    for (status, said) in [
        (401, "Bad credentials"),
        (403, "Forbidden"),
        (404, "Not Found"),
    ] {
        let (_server, app) =
            refusing(ResponseTemplate::new(status).set_body_json(json!({"message": said}))).await;
        let err = app.token_for(&repo()).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Auth(_)), "{status}: {err:?}");
        assert_eq!(err.class(), ErrorClass::Unauthenticated);
        assert!(!err.is_retryable());
        let message = err.to_string();
        for needle in [
            format!("HTTP {status}"),
            said.to_owned(),
            "GITHUB_APP_ID".to_owned(),
            "GITHUB_APP_INSTALLATION_ID".to_owned(),
            "GITHUB_APP_PRIVATE_KEY_PATH".to_owned(),
        ] {
            assert!(message.contains(&needle), "{status}: {message}");
        }
    }
}

#[tokio::test]
async fn rate_limits_are_rate_limits_and_server_trouble_is_transient() {
    let (_s, app) = refusing(ResponseTemplate::new(429).insert_header("retry-after", "42")).await;
    let err = app.token_for(&repo()).await.unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::RateLimited { retry_after: Some(d) } if *d == Duration::from_secs(42)),
        "{err:?}"
    );
    let (_s, app) = refusing(
        ResponseTemplate::new(403)
            .insert_header("x-ratelimit-remaining", "0")
            .set_body_json(json!({"message": "API rate limit exceeded"})),
    )
    .await;
    let err = app.token_for(&repo()).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::RateLimited { .. }), "{err:?}");
    assert_eq!(err.class(), ErrorClass::RateLimited);
    for status in [500, 502, 503] {
        let (_s, app) = refusing(ResponseTemplate::new(status)).await;
        let err = app.token_for(&repo()).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Transient { .. }),
            "{status}: {err:?}"
        );
        assert!(err.is_retryable());
    }
    // A success that is not a token, and a token with no usable expiry.
    for body in [
        ResponseTemplate::new(201).set_body_string("not json"),
        ResponseTemplate::new(201).set_body_json(json!({"token": "ghs_x"})),
        ResponseTemplate::new(201).set_body_json(json!({"token": "ghs_x", "expires_at": "soon"})),
    ] {
        let (_s, app) = refusing(body).await;
        let err = app.token_for(&repo()).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Transient { .. }), "{err:?}");
        assert!(!err.to_string().contains("ghs_x"), "{err}");
    }
    // Something else GitHub may say: an error with its status.
    let (_s, app) =
        refusing(ResponseTemplate::new(422).set_body_json(json!({"message": "spammed"}))).await;
    let err = app.token_for(&repo()).await.unwrap_err();
    assert!(
        matches!(err, WorkspaceError::Http { status: 422, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_unreachable_github_is_transient() {
    let key = TestAppKey::generate();
    let app = GitHubApp::new(
        "http://127.0.0.1:1",
        APP_ID,
        INSTALLATION,
        AppKey::from_pem(&key.pkcs8_pem).unwrap(),
    )
    .unwrap();
    let err = app.token_for(&repo()).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Transient { .. }), "{err:?}");
    assert!(err.is_retryable());
}

/// A failed trade is not remembered: the next call tries again, and the JWT that a server echoes
/// back in its message is never in what the caller is told.
#[tokio::test]
async fn a_failed_mint_is_tried_again_and_the_jwt_is_never_in_the_error() {
    struct Echo {
        key: Arc<TestAppKey>,
        calls: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
    }
    impl Respond for Echo {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let jwt = request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or_default()
                .to_owned();
            self.seen.lock().unwrap().push(jwt.clone());
            assert!(self.key.verify_jwt(&jwt).is_ok());
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                // A server that repeats what it was sent.
                ResponseTemplate::new(401)
                    .set_body_json(json!({"message": format!("bad token {jwt}")}))
            } else {
                ResponseTemplate::new(201).set_body_json(json!({
                    "token": "ghs_second",
                    "expires_at": (Utc::now() + TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
                }))
            }
        }
    }
    let server = MockServer::start().await;
    let key = Arc::new(TestAppKey::generate());
    let seen = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(Echo {
            key: key.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
            seen: seen.clone(),
        })
        .mount(&server)
        .await;
    let app = GitHubApp::new(
        server.uri(),
        APP_ID,
        INSTALLATION,
        AppKey::from_pem(&key.pkcs1_pem).unwrap(),
    )
    .unwrap();
    let err = app.token_for(&repo()).await.unwrap_err();
    let jwt = seen.lock().unwrap()[0].clone();
    assert!(!jwt.is_empty());
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    assert!(
        !format!("{err} {err:?}").contains(&jwt),
        "the JWT is not in the error"
    );
    assert!(err.to_string().contains("[REDACTED]"), "{err}");
    // The next call is a new try, and succeeds.
    let token = app.token_for(&repo()).await.unwrap();
    assert_eq!(token.expose_secret(), "ghs_second");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn nothing_secret_is_in_debug_output() {
    let rig = Rig::new().await;
    let app = rig.app();
    let token = rig.token(&app).await;
    let text = format!("{app:?} {:?}", HostScoped::new(["github.com"], rig.app()));
    assert!(!text.contains(&token) && !text.contains("BEGIN"), "{text}");
    assert!(text.contains("github.com"), "{text}");
}
