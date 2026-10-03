//! `GitHubApp` against a mock server (no network): the JWT it signs, the installation token it
//! trades it for, how long it keeps it, and what it says when GitHub refuses.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping
#![cfg(all(feature = "github", feature = "test-util"))]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_workspace::testing::TestAppKey;
use adam_workspace::{
    AppKey, AppOwners, CodeHost, GitCredentials, GitHub, GitHubApp, HostScoped, Installation,
    MAX_CACHED_INSTALLATIONS, NewPullRequest, RepoRef, WorkspaceError,
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
    assert!(
        text.contains("installation_id: 67890"),
        "the pin is shown: {text}"
    );

    // An App that finds installations says so and names its owners; a token it minted for one is
    // in no output either.
    let world = World::new().await;
    world.install("acme", Kind::Org, 111);
    let app = world.app(AppOwners::only(["Acme", "other-org"]));
    let token = world.token(&app, "acme").await;
    let text = format!(
        "{app:?} {:?} {:?}",
        world.app(AppOwners::Any),
        HostScoped::new(["github.com"], world.app(AppOwners::only(["acme"])))
    );
    assert!(!text.contains(&token) && !text.contains("BEGIN"), "{text}");
    assert!(!text.contains("eyJ"), "no JWT: {text}");
    assert!(text.contains("found per owner"), "{text}");
    assert!(
        text.contains("acme") && text.contains("other-org"),
        "{text}"
    );
    assert!(text.contains("Any"), "{text}");
    assert!(text.contains("github.com"), "{text}");
    let installation = app.installation_for("acme").await.unwrap();
    assert_eq!(
        format!("{installation:?}"),
        "Installation { id: 111, account: \"acme\" }"
    );
}

// ---------------------------------------------------------------------------------------------
// An App that finds the installation of each owner (ADR 0017).
// ---------------------------------------------------------------------------------------------

/// How an account is installed: the organisation lookup answers for an organisation and the user
/// lookup for a person, and each says `404` for the other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Org,
    User,
}

struct Install {
    kind: Kind,
    id: u64,
    /// What `account.login` says, which is not the name asked for after a rename.
    login: String,
    suspended: bool,
}

/// How a lookup fails, for the tests of what is never cached.
#[derive(Clone, Copy)]
enum Fail {
    Status(u16),
    RateLimited,
    RateLimitedForbidden,
}

#[derive(Default)]
struct State {
    /// By the lowercased name asked for.
    installs: HashMap<String, Install>,
    /// `METHOD /path` of every request, in order.
    log: Vec<String>,
    /// The claims of every JWT the fake accepted.
    claims: Vec<Value>,
    /// Tokens minted per installation.
    minted: HashMap<u64, usize>,
    /// Installations whose mint answers `404`.
    gone: HashSet<u64>,
    fail_orgs: Option<Fail>,
    fail_users: Option<Fail>,
    app_fails: bool,
}

/// GitHub, as far as the App's endpoints go: `GET /app`, the two installation lookups and the
/// mint, each of which wants a JWT signed by the App's key.
struct Fake {
    key: Arc<TestAppKey>,
    clock: TestClock,
    state: Arc<Mutex<State>>,
    lookup_delay: Duration,
    mint_delay: Duration,
}

fn failure(fail: Fail) -> ResponseTemplate {
    match fail {
        Fail::Status(status) => ResponseTemplate::new(status).set_body_json(json!({
            "message": "trouble",
        })),
        Fail::RateLimited => ResponseTemplate::new(429).insert_header("retry-after", "7"),
        Fail::RateLimitedForbidden => ResponseTemplate::new(403)
            .insert_header("x-ratelimit-remaining", "0")
            .set_body_json(json!({"message": "API rate limit exceeded"})),
    }
}

impl Respond for Fake {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let jwt = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        let mut state = self.state.lock().unwrap();
        let path = request.url.path().to_owned();
        state.log.push(format!("{} {path}", request.method));
        match self.key.verify_jwt(jwt) {
            Ok(claims) => state.claims.push(claims),
            Err(why) => {
                return ResponseTemplate::new(401).set_body_json(json!({
                    "message": format!("A JSON web token could not be decoded: {why}"),
                }));
            }
        }
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["app"]) if state.app_fails => ResponseTemplate::new(500),
            ("GET", ["app"]) => ResponseTemplate::new(200).set_body_json(json!({
                "id": 12345,
                "slug": "adam-test",
                "html_url": "https://github.com/apps/adam-test",
            })),
            ("GET", [kind @ ("orgs" | "users"), name, "installation"]) => {
                let kind = if *kind == "orgs" {
                    Kind::Org
                } else {
                    Kind::User
                };
                let fail = if kind == Kind::Org {
                    state.fail_orgs
                } else {
                    state.fail_users
                };
                if let Some(fail) = fail {
                    return failure(fail).set_delay(self.lookup_delay);
                }
                match state.installs.get(&name.to_ascii_lowercase()) {
                    Some(found) if found.kind == kind => ResponseTemplate::new(200)
                        .set_delay(self.lookup_delay)
                        .set_body_json(json!({
                            "id": found.id,
                            "account": {"login": found.login},
                            "app_id": 12345,
                            "app_slug": "adam-test",
                            "repository_selection": "all",
                            "suspended_at": found.suspended.then_some("2026-10-02T00:00:00Z"),
                        })),
                    _ => ResponseTemplate::new(404)
                        .set_delay(self.lookup_delay)
                        .set_body_json(json!({"message": "Not Found"})),
                }
            }
            ("POST", ["app", "installations", id, "access_tokens"]) => {
                let id: u64 = id.parse().unwrap();
                if state.gone.contains(&id) {
                    return ResponseTemplate::new(404)
                        .set_delay(self.mint_delay)
                        .set_body_json(json!({"message": "Not Found"}));
                }
                let n = state.minted.entry(id).or_default();
                *n += 1;
                ResponseTemplate::new(201)
                    .set_delay(self.mint_delay)
                    .set_body_json(json!({
                        "token": format!("ghs_{id}_{n}"),
                        "expires_at": (self.clock.now() + TimeDelta::hours(1))
                            .to_rfc3339_opts(SecondsFormat::Secs, true),
                    }))
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

struct World {
    server: MockServer,
    key: Arc<TestAppKey>,
    clock: TestClock,
    state: Arc<Mutex<State>>,
}

fn repo_of(owner: &str) -> RepoRef {
    RepoRef::new(format!("https://github.com/{owner}/widgets.git"), "main")
}

impl World {
    async fn new() -> Self {
        Self::with_delays(Duration::ZERO, Duration::ZERO).await
    }

    async fn with_delays(lookup_delay: Duration, mint_delay: Duration) -> Self {
        let server = MockServer::start().await;
        let key = Arc::new(TestAppKey::generate());
        let clock = TestClock::at("2026-10-03T12:00:00Z");
        let state = Arc::new(Mutex::new(State::default()));
        Mock::given(wiremock::matchers::any())
            .respond_with(Fake {
                key: key.clone(),
                clock: clock.clone(),
                state: state.clone(),
                lookup_delay,
                mint_delay,
            })
            .mount(&server)
            .await;
        Self {
            server,
            key,
            clock,
            state,
        }
    }

    fn app(&self, owners: AppOwners) -> GitHubApp {
        GitHubApp::discovering(
            self.server.uri(),
            APP_ID,
            AppKey::from_pem(&self.key.pkcs1_pem).unwrap(),
            owners,
        )
        .unwrap()
        .with_clock(self.clock.as_fn())
    }

    /// The App is installed on `name`, with that login.
    fn install(&self, name: &str, kind: Kind, id: u64) {
        self.install_as(name, kind, id, name);
    }

    fn install_as(&self, name: &str, kind: Kind, id: u64, login: &str) {
        self.state.lock().unwrap().installs.insert(
            name.to_ascii_lowercase(),
            Install {
                kind,
                id,
                login: login.to_owned(),
                suspended: false,
            },
        );
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    fn log(&self) -> Vec<String> {
        self.with_state(|s| s.log.clone())
    }

    /// How many requests the log has that are exactly `line` (`GET /orgs/acme/installation`).
    fn count(&self, line: &str) -> usize {
        self.with_state(|s| s.log.iter().filter(|l| *l == line).count())
    }

    /// How many requests the log has that start with `prefix`.
    fn count_prefix(&self, prefix: &str) -> usize {
        self.with_state(|s| s.log.iter().filter(|l| l.starts_with(prefix)).count())
    }

    fn minted(&self, id: u64) -> usize {
        self.with_state(|s| s.minted.get(&id).copied().unwrap_or(0))
    }

    async fn token(&self, app: &GitHubApp, owner: &str) -> String {
        app.token_for(&repo_of(owner))
            .await
            .unwrap()
            .expose_secret()
            .to_owned()
    }
}

#[tokio::test]
async fn without_an_installation_id_the_owners_installation_is_looked_up_with_the_jwt_and_kept() {
    let world = World::new().await;
    world.install("acme", Kind::Org, 111);
    let app = world.app(AppOwners::only(["acme"]));
    assert_eq!(app.pinned_installation(), None);

    assert_eq!(world.token(&app, "acme").await, "ghs_111_1");
    assert_eq!(
        world.log(),
        [
            "GET /orgs/acme/installation",
            "POST /app/installations/111/access_tokens"
        ],
        "the owner, not the repository, is looked up, and an organisation needs no second request"
    );
    // The lookup and the mint were each signed with the App's key (the fake checked), as the App.
    let claims = world.with_state(|s| s.claims.clone());
    assert_eq!(claims.len(), 2);
    for claim in &claims {
        assert_eq!(claim["iss"], 12345);
        assert_eq!(claim["iat"], world.clock.now().timestamp() - 60);
        assert_eq!(claim["exp"], world.clock.now().timestamp() + 540);
    }
    // Kept: the installation, and the token.
    let found: Installation = app.installation_for("acme").await.unwrap();
    assert_eq!((found.id, found.account.as_str()), (111, "acme"));
    assert_eq!(world.token(&app, "acme").await, "ghs_111_1");
    assert_eq!(world.log().len(), 2, "nothing more was asked of GitHub");
}

#[tokio::test]
async fn two_owners_on_two_installations_get_two_tokens_and_one_mint_each() {
    let world = World::new().await;
    world.install("acme", Kind::Org, 111);
    world.install("other-org", Kind::Org, 222);
    let app = world.app(AppOwners::only(["acme", "other-org"]));
    for _ in 0..3 {
        assert_eq!(world.token(&app, "acme").await, "ghs_111_1");
        assert_eq!(world.token(&app, "other-org").await, "ghs_222_1");
    }
    assert_eq!(world.minted(111), 1);
    assert_eq!(world.minted(222), 1);
    assert_eq!(world.count("GET /orgs/acme/installation"), 1);
    assert_eq!(world.count("GET /orgs/other-org/installation"), 1);
    // Two repositories of one owner share the installation's token.
    let other_repo = RepoRef::new("https://github.com/acme/gadgets", "main");
    assert_eq!(
        app.token_for(&other_repo).await.unwrap().expose_secret(),
        "ghs_111_1"
    );
    assert_eq!(world.log().len(), 4);
}

#[tokio::test]
async fn owner_logins_are_compared_without_case() {
    let world = World::new().await;
    // GitHub spells the login as its owner did.
    world.install_as("acme", Kind::Org, 111, "Acme");
    let app = world.app(AppOwners::only(["ACME"]));
    for owner in ["acme", "Acme", "ACME", "aCmE"] {
        assert_eq!(world.token(&app, owner).await, "ghs_111_1", "{owner}");
    }
    assert_eq!(
        world.count_prefix("GET /orgs/"),
        1,
        "one lookup for four spellings of one account"
    );
    assert_eq!(world.minted(111), 1);
    assert_eq!(app.installation_for("acme").await.unwrap().account, "Acme");
}

#[tokio::test]
async fn a_user_account_is_found_after_the_organisation_lookup_says_404() {
    let world = World::new().await;
    world.install("alice", Kind::User, 222);
    let app = world.app(AppOwners::only(["alice"]));
    assert_eq!(world.token(&app, "alice").await, "ghs_222_1");
    assert_eq!(
        world.log(),
        [
            "GET /orgs/alice/installation",
            "GET /users/alice/installation",
            "POST /app/installations/222/access_tokens"
        ]
    );
    // Kept, as an organisation's is.
    assert_eq!(world.token(&app, "alice").await, "ghs_222_1");
    assert_eq!(world.log().len(), 3);
}

#[tokio::test]
async fn an_owner_the_app_is_not_installed_on_is_an_auth_error_naming_the_app_and_the_owner() {
    let world = World::new().await;
    let app = world.app(AppOwners::only(["ghost", "ghost2"]));

    let err = app.token_for(&repo_of("ghost")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    assert_eq!(err.class(), ErrorClass::Unauthenticated);
    assert!(!err.is_retryable());
    let message = err.to_string();
    for needle in [
        "`adam-test`",
        "`ghost`",
        "https://github.com/apps/adam-test/installations/new",
    ] {
        assert!(message.contains(needle), "{message}");
    }
    assert!(!message.contains("GITHUB_APP_INSTALLATION_ID"), "{message}");
    assert!(!message.contains("eyJ"), "no JWT: {message}");
    assert_eq!(
        world.log(),
        [
            "GET /orgs/ghost/installation",
            "GET /users/ghost/installation",
            "GET /app"
        ],
        "both lookups, and the App's name for the message, and nothing was minted"
    );

    // "Not installed" is believed for 60 seconds: nothing is asked at +59 s ...
    world.clock.advance(TimeDelta::seconds(59));
    let err = app.token_for(&repo_of("ghost")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    assert!(err.to_string().contains("`adam-test`"), "{err}");
    assert_eq!(world.log().len(), 3, "no request at +59 s");

    // ... and GitHub is asked again at +61 s (still not installed: believed for 60 more seconds).
    world.clock.advance(TimeDelta::seconds(2));
    assert!(app.token_for(&repo_of("ghost")).await.is_err());
    assert_eq!(world.count("GET /orgs/ghost/installation"), 2);
    assert_eq!(world.count("GET /users/ghost/installation"), 2);
    assert_eq!(
        world.count("GET /app"),
        1,
        "the App's name is read once, however many owners lack it"
    );
    // Installing it meanwhile is not seen until the 60 seconds are over.
    world.install("ghost", Kind::Org, 333);
    assert!(app.token_for(&repo_of("ghost")).await.is_err());
    world.clock.advance(TimeDelta::seconds(61));
    assert_eq!(world.token(&app, "ghost").await, "ghs_333_1");
    // Another owner has an entry of its own.
    assert!(app.token_for(&repo_of("ghost2")).await.is_err());
    assert_eq!(world.count("GET /app"), 1);
}

#[tokio::test]
async fn the_app_is_named_by_its_id_when_its_slug_cannot_be_read() {
    let world = World::new().await;
    world.with_state(|s| s.app_fails = true);
    let app = world.app(AppOwners::Any);
    let err = app.token_for(&repo_of("ghost")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    assert!(
        err.to_string().contains("`12345`") && err.to_string().contains("`ghost`"),
        "{err}"
    );
}

#[tokio::test]
async fn an_owner_outside_the_allowed_owners_is_refused_before_anything_is_signed() {
    let world = World::new().await;
    world.install("evil", Kind::Org, 666);
    world.install("acme", Kind::Org, 111);
    let app = world.app(AppOwners::only(["acme"]));
    let scoped = HostScoped::new(["github.com"], world.app(AppOwners::only(["acme"])));
    for url in [
        "https://github.com/evil/widgets.git",
        "https://github.com/evil-acme/widgets.git",
        "/tmp/a/remote.git",
    ] {
        for err in [
            app.token_for(&RepoRef::new(url, "main")).await.unwrap_err(),
            scoped
                .token_for(&RepoRef::new(url, "main"))
                .await
                .unwrap_err(),
        ] {
            assert!(matches!(err, WorkspaceError::Invalid(_)), "{url}: {err:?}");
            assert_eq!(err.class(), ErrorClass::Invalid);
        }
    }
    let err = app.token_for(&repo_of("evil")).await.unwrap_err();
    assert!(
        err.to_string().contains("`evil`") && err.to_string().contains("GITHUB_APP_OWNERS"),
        "{err}"
    );
    let err = app.installation_for("evil").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(
        world.log().is_empty(),
        "no lookup and no mint for an owner that is not allowed: {:?}",
        world.log()
    );
    // What is not a login never gets into a path.
    for bad in ["", "..", "a/b", "a?b"] {
        let err = app.installation_for(bad).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Invalid(_)),
            "{bad:?}: {err:?}"
        );
    }
    assert!(world.log().is_empty());
    // The one that is allowed works.
    assert_eq!(world.token(&app, "acme").await, "ghs_111_1");
}

#[tokio::test]
async fn any_owner_is_allowed_only_when_said_so() {
    let world = World::new().await;
    world.install("acme", Kind::Org, 111);
    world.install("stranger", Kind::User, 999);
    // Any: every account the App is installed on.
    let any = world.app(AppOwners::Any);
    assert_eq!(world.token(&any, "acme").await, "ghs_111_1");
    assert_eq!(world.token(&any, "stranger").await, "ghs_999_1");
    // An empty list is not "any": it allows nothing, and asks nothing.
    let before = world.log().len();
    let none = world.app(AppOwners::only(Vec::<String>::new()));
    let err = none.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert_eq!(world.log().len(), before);
}

#[tokio::test]
async fn a_suspended_installation_is_refused() {
    let world = World::new().await;
    world.install("acme", Kind::Org, 111);
    world.with_state(|s| s.installs.get_mut("acme").unwrap().suspended = true);
    let app = world.app(AppOwners::Any);
    let err = app.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    assert!(
        err.to_string().contains("suspended") && err.to_string().contains("`acme`"),
        "{err}"
    );
    assert_eq!(
        world.minted(111),
        0,
        "no token for a suspended installation"
    );
    assert_eq!(world.count_prefix("POST"), 0);
    // Not kept: lifted, it works at once.
    world.with_state(|s| s.installs.get_mut("acme").unwrap().suspended = false);
    assert_eq!(world.token(&app, "acme").await, "ghs_111_1");
}

#[tokio::test]
async fn an_uninstalled_installation_is_looked_up_again_once() {
    let world = World::new().await;
    world.install("acme", Kind::Org, 111);
    let app = world.app(AppOwners::Any);
    assert_eq!(world.token(&app, "acme").await, "ghs_111_1");
    assert_eq!(world.count("GET /orgs/acme/installation"), 1);

    // Uninstalled and installed again under another ID; the token is about to expire.
    world.with_state(|s| {
        s.gone.insert(111);
    });
    world.install("acme", Kind::Org, 333);
    world.clock.advance(TimeDelta::minutes(56));
    assert_eq!(world.token(&app, "acme").await, "ghs_333_1");
    assert_eq!(
        world.count("GET /orgs/acme/installation"),
        2,
        "the 404 of the mint dropped the entry: one more lookup"
    );
    assert_eq!(world.count("POST /app/installations/111/access_tokens"), 2);
    assert_eq!(world.count("POST /app/installations/333/access_tokens"), 1);
    // And it is the new one that is kept.
    assert_eq!(world.token(&app, "acme").await, "ghs_333_1");
    assert_eq!(world.count("GET /orgs/acme/installation"), 2);

    // Gone for good: one more lookup and one more mint, then the error, and no loop.
    world.with_state(|s| {
        s.gone.insert(333);
    });
    world.clock.advance(TimeDelta::minutes(56));
    let err = app.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    assert_eq!(world.count("GET /orgs/acme/installation"), 3, "once more");
    assert_eq!(world.count("POST /app/installations/333/access_tokens"), 3);
    assert!(
        !err.to_string().contains("GITHUB_APP_INSTALLATION_ID"),
        "{err}"
    );
}

#[tokio::test]
async fn concurrent_callers_for_one_owner_make_one_lookup_and_one_mint() {
    let world = World::with_delays(Duration::from_millis(250), Duration::from_millis(250)).await;
    world.install("acme", Kind::Org, 111);
    let app = Arc::new(world.app(AppOwners::Any));
    let calls: Vec<_> = (0..16)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                app.token_for(&repo_of("acme"))
                    .await
                    .unwrap()
                    .expose_secret()
                    .to_owned()
            })
        })
        .collect();
    for call in calls {
        assert_eq!(call.await.unwrap(), "ghs_111_1");
    }
    assert_eq!(world.count("GET /orgs/acme/installation"), 1);
    assert_eq!(world.minted(111), 1);
    assert_eq!(
        world.log().len(),
        2,
        "one lookup and one mint for sixteen callers"
    );
}

#[tokio::test]
async fn two_installations_mint_concurrently() {
    let mint = Duration::from_millis(500);
    let world = World::with_delays(Duration::ZERO, mint).await;
    world.install("acme", Kind::Org, 111);
    world.install("other-org", Kind::Org, 222);
    let app = Arc::new(world.app(AppOwners::Any));
    let started = std::time::Instant::now();
    let calls: Vec<_> = ["acme", "other-org"]
        .into_iter()
        .map(|owner| {
            let app = app.clone();
            tokio::spawn(async move {
                app.token_for(&repo_of(owner))
                    .await
                    .unwrap()
                    .expose_secret()
                    .to_owned()
            })
        })
        .collect();
    let mut tokens = Vec::new();
    for call in calls {
        tokens.push(call.await.unwrap());
    }
    let took = started.elapsed();
    assert_eq!(tokens, ["ghs_111_1", "ghs_222_1"]);
    assert!(
        took >= mint && took < mint * 2,
        "two mints of {mint:?} each ran side by side, not one after the other: {took:?}"
    );
}

#[tokio::test]
async fn a_rate_limited_or_failed_lookup_is_typed_and_not_cached() {
    let world = World::new().await;
    let app = world.app(AppOwners::Any);
    let lookups = || world.count_prefix("GET /orgs/") + world.count_prefix("GET /users/");

    world.with_state(|s| s.fail_orgs = Some(Fail::RateLimited));
    let err = app.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::RateLimited { retry_after: Some(d) } if *d == Duration::from_secs(7)),
        "{err:?}"
    );
    assert_eq!(err.class(), ErrorClass::RateLimited);

    world.with_state(|s| s.fail_orgs = Some(Fail::RateLimitedForbidden));
    let err = app.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::RateLimited { .. }), "{err:?}");
    assert_eq!(lookups(), 2, "asked again, not remembered");

    for status in [500, 502, 503] {
        world.with_state(|s| s.fail_orgs = Some(Fail::Status(status)));
        let err = app.token_for(&repo_of("acme")).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Transient { .. }),
            "{status}: {err:?}"
        );
        assert!(err.is_retryable());
    }
    assert_eq!(lookups(), 5);

    // The organisation lookup says 404 and the user lookup is the one in trouble: not "not
    // installed" either.
    world.with_state(|s| {
        s.fail_orgs = None;
        s.fail_users = Some(Fail::Status(500));
    });
    let err = app.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Transient { .. }), "{err:?}");

    // A refusal of the JWT is an Auth error that names the key and not an installation variable.
    world.with_state(|s| {
        s.fail_users = None;
        s.fail_orgs = Some(Fail::Status(401));
    });
    let err = app.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    assert!(
        err.to_string().contains("GITHUB_APP_PRIVATE_KEY_PATH")
            && !err.to_string().contains("GITHUB_APP_INSTALLATION_ID"),
        "{err}"
    );

    // None of it was kept: the moment GitHub is well and the App installed, it works.
    world.with_state(|s| s.fail_orgs = None);
    world.install("acme", Kind::Org, 111);
    assert_eq!(world.token(&app, "acme").await, "ghs_111_1");

    // A transport failure is transient too.
    let key = TestAppKey::generate();
    let unreachable = GitHubApp::discovering(
        "http://127.0.0.1:1",
        APP_ID,
        AppKey::from_pem(&key.pkcs8_pem).unwrap(),
        AppOwners::Any,
    )
    .unwrap();
    let err = unreachable.token_for(&repo_of("acme")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Transient { .. }), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn a_renamed_owner_is_not_cached_under_the_old_login() {
    let world = World::new().await;
    // GitHub answers for the old name with the account's new login.
    world.install_as("old-name", Kind::Org, 444, "new-name");
    let app = world.app(AppOwners::Any);
    let found = app.installation_for("old-name").await.unwrap();
    assert_eq!((found.id, found.account.as_str()), (444, "new-name"));
    assert_eq!(world.token(&app, "old-name").await, "ghs_444_1");
    assert_eq!(
        world.count("GET /orgs/old-name/installation"),
        2,
        "the old name is asked again every time: it may be somebody else's one day"
    );
    // The login it has now is known, with no lookup.
    assert_eq!(app.installation_for("New-Name").await.unwrap().id, 444);
    assert_eq!(world.count_prefix("GET /orgs/new-name"), 0);
    assert_eq!(world.minted(444), 1);

    // With a list of owners, the old name has to lead to an owner on the list.
    let listed = world.app(AppOwners::only(["old-name"]));
    let err = listed.token_for(&repo_of("old-name")).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string().contains("new-name") && err.to_string().contains("GITHUB_APP_OWNERS"),
        "{err}"
    );
    let listed = world.app(AppOwners::only(["old-name", "new-name"]));
    assert_eq!(world.token(&listed, "old-name").await, "ghs_444_2");
}

#[tokio::test]
async fn the_pinned_installation_makes_no_lookup() {
    let world = World::new().await;
    world.install("acme", Kind::Org, 111);
    let app = GitHubApp::new(
        world.server.uri(),
        APP_ID,
        INSTALLATION,
        AppKey::from_pem(&world.key.pkcs1_pem).unwrap(),
    )
    .unwrap()
    .with_clock(world.clock.as_fn());
    assert_eq!(app.pinned_installation(), Some(INSTALLATION));
    // One token for every owner, whatever they are, and no lookup.
    assert_eq!(world.token(&app, "acme").await, "ghs_67890_1");
    assert_eq!(world.token(&app, "anybody-else").await, "ghs_67890_1");
    assert_eq!(world.log(), ["POST /app/installations/67890/access_tokens"]);
    // A pinned App has no lookup to offer.
    let err = app.installation_for("acme").await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert_eq!(world.log().len(), 1);
}

#[tokio::test]
async fn the_caches_are_bounded() {
    // One account too many for the accounts' cache (four for each installation kept).
    let owners = 4 * MAX_CACHED_INSTALLATIONS + 1;
    let world = World::new().await;
    for i in 0..owners {
        world.install(&format!("o{i}"), Kind::Org, 1000 + i as u64);
    }
    let app = world.app(AppOwners::Any);
    for i in 0..owners {
        world.token(&app, &format!("o{i}")).await;
    }
    let lookups = |i: usize| world.count(&format!("GET /orgs/o{i}/installation"));
    let mints = |i: usize| world.minted(1000 + i as u64);

    // The most recent is kept whole.
    let last = owners - 1;
    world.token(&app, &format!("o{last}")).await;
    assert_eq!((lookups(last), mints(last)), (1, 1));
    // A middle one lost its token (only 64 are kept) but not its account (256 are): a mint, no lookup.
    world.token(&app, "o100").await;
    assert_eq!((lookups(100), mints(100)), (1, 2));
    // The first lost both: a lookup and a mint.
    world.token(&app, "o0").await;
    assert_eq!((lookups(0), mints(0)), (2, 2));
}

/// The bound is part of the API (`MAX_CACHED_INSTALLATIONS`): the redactor of the coder sizes itself
/// from it (ADR 0017, decision 5).
#[test]
fn the_default_bound_is_what_the_adr_says() {
    assert_eq!(MAX_CACHED_INSTALLATIONS, 64);
}
