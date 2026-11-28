//! The broker adapter against WireMock: the contract v1 fixtures, the conformance suite, the
//! refusals, the token that is re-read and never rendered.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use adam_core::RunId;
use adam_core::turn::run_cert::testkit::FakeRunCertIssuer;
use adam_core::turn::{CertificateRequest, DynRunCertIssuer, RunCertError, RunCertIssuer};
use adam_error::{Classify, ErrorClass};
use adam_run_cert_broker::{BrokerIssuer, TokenError};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt as _;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SENTINEL: &str = "anr1310-projected-token-SENTINEL-do-not-log";

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/run-certificate-v1")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A token file under the target's temp directory (never `/tmp`, which the build host's rules keep
/// off the big disk): written with a trailing newline, as a projected secret is, and swapped in
/// atomically like the kubelet swaps a projected secret (a reader never sees a truncated file).
fn token_file(name: &str, token: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("run-cert-token-{name}"));
    replace_token(&path, token);
    path
}

fn replace_token(path: &Path, token: &str) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let temp = path.with_file_name(format!(
        ".token-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&temp, format!("{token}\n")).unwrap();
    std::fs::rename(&temp, path).unwrap();
}

fn request() -> CertificateRequest {
    let csr = fixture("request.v1.json")["csr"]
        .as_str()
        .expect("the fixture's csr")
        .to_owned();
    CertificateRequest::new(STANDARD.decode(csr).unwrap()).unwrap()
}

fn answer() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture("answer.v1.json"))
}

/// The suite's mock: it mints a real leaf for the CSR it receives, so two requests get two chains
/// (a fixed answer cannot pass `two_requests_get_two_chains`).
struct DynamicCa;

impl Respond for DynamicCa {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("the request is JSON");
        if body["v"] != 1 {
            return ResponseTemplate::new(400);
        }
        let csr = STANDARD
            .decode(body["csr"].as_str().expect("a csr"))
            .expect("the csr is base64");
        let request = CertificateRequest::new(csr).expect("a bounded csr");
        let chain = futures::executor::block_on(FakeRunCertIssuer::new().issue(&request))
            .expect("the fake CA issues");
        let chain: Vec<String> = chain
            .certificates()
            .iter()
            .map(|certificate| STANDARD.encode(certificate))
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({"v": 1, "chain": chain}))
    }
}

fn conformance_server() -> &'static MockServer {
    static SERVER: OnceLock<MockServer> = OnceLock::new();
    SERVER.get_or_init(|| {
        // A thread of its own: `make()` runs inside the suite's `#[tokio::test]`, and blocking a
        // runtime from within one is a panic.
        std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a test runtime");
            let server = runtime.block_on(MockServer::start());
            runtime.block_on(async {
                Mock::given(method("POST"))
                    .and(path_regex(r"^/internal/runs/[0-9a-f-]+/certificate$"))
                    .respond_with(DynamicCa)
                    .mount(&server)
                    .await;
            });
            server
        })
        .join()
        .expect("the conformance server starts")
    })
}

fn make() -> DynRunCertIssuer {
    // A token file per run: the three cases run in parallel, and one file rewritten by each would
    // be read empty between the truncate and the write.
    let run = RunId::new();
    let issuer = BrokerIssuer::new(
        conformance_server().uri(),
        token_file(&format!("conformance-{run}"), "conformance-token"),
        run,
    )
    .unwrap();
    Arc::new(issuer)
}

adam_core::run_cert_issuer_conformance!(make);

#[tokio::test]
async fn the_v1_fixture_crosses_the_wire_unchanged() {
    let request = fixture("request.v1.json");
    let answer = fixture("answer.v1.json");
    let run = RunId::new();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!("/internal/runs/{run}/certificate")))
        .and(header("authorization", "Bearer fixture-token"))
        .and(header("accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer.clone()))
        .mount(&server)
        .await;

    let issuer =
        BrokerIssuer::new(server.uri(), token_file("fixture", "fixture-token"), run).unwrap();
    let csr = STANDARD.decode(request["csr"].as_str().unwrap()).unwrap();
    let chain = issuer
        .issue(&CertificateRequest::new(csr).unwrap())
        .await
        .unwrap();

    let expected: Vec<Vec<u8>> = answer["chain"]
        .as_array()
        .unwrap()
        .iter()
        .map(|certificate| STANDARD.decode(certificate.as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(
        chain.certificates(),
        expected.as_slice(),
        "the chain the fixture answers is the chain adam carries"
    );

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let sent: Value = serde_json::from_slice(&received[0].body).unwrap();
    assert_eq!(sent, request, "the request body is the v1 fixture's shape");
    assert_eq!(
        received[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer fixture-token"
    );
}

#[tokio::test]
async fn a_wrong_version_is_refused() {
    let server = MockServer::start().await;
    let mut answer = fixture("answer.v1.json");
    answer["v"] = json!(2);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer))
        .mount(&server)
        .await;

    let issuer =
        BrokerIssuer::new(server.uri(), token_file("wrong-v", "a-token"), RunId::new()).unwrap();
    let error = issuer.issue(&request()).await.unwrap_err();
    assert!(
        matches!(error, RunCertError::Internal(_)),
        "a wrong v is Internal, got {error:?}"
    );
    let source = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<adam_run_cert_broker::AnswerError>());
    assert!(
        matches!(
            source,
            Some(adam_run_cert_broker::AnswerError::Version { found: 2 })
        ),
        "the error says which version arrived: {error:?}"
    );
}

#[tokio::test]
async fn each_refusal_has_its_class_and_the_body_is_not_echoed() {
    for (file, status, class) in [
        (
            "error-unauthenticated.v1.json",
            401,
            ErrorClass::Unauthenticated,
        ),
        (
            "error-run-not-bound.v1.json",
            403,
            ErrorClass::Unauthenticated,
        ),
        (
            "error-lease-expired.v1.json",
            403,
            ErrorClass::Unauthenticated,
        ),
        ("error-not-signed.v1.json", 403, ErrorClass::Unauthenticated),
        ("error-bad-csr.v1.json", 400, ErrorClass::Invalid),
        ("error-unavailable.v1.json", 503, ErrorClass::Transient),
    ] {
        let body = fixture(file);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
            .mount(&server)
            .await;
        let issuer = BrokerIssuer::new(
            server.uri(),
            token_file(file, "refusal-token"),
            RunId::new(),
        )
        .unwrap();
        let error = issuer.issue(&request()).await.unwrap_err();
        assert_eq!(error.class(), class, "{file}");
        let rendered = format!("{error} {error:?}");
        assert!(
            !rendered.contains(body["detail"].as_str().unwrap()),
            "{file}: the problem document's body was echoed: {rendered}"
        );
        assert!(
            !rendered.contains("refusal-token"),
            "{file}: the token was rendered: {rendered}"
        );
    }
}

#[tokio::test]
async fn the_token_is_read_again_on_every_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer())
        .mount(&server)
        .await;
    let path = token_file("reread", "first-token");
    let issuer = BrokerIssuer::new(server.uri(), &path, RunId::new()).unwrap();
    issuer.issue(&request()).await.unwrap();
    replace_token(&path, "second-token");
    issuer.issue(&request()).await.unwrap();

    let received = server.received_requests().await.unwrap();
    let sent: Vec<String> = received
        .iter()
        .map(|request| {
            request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(sent, ["Bearer first-token", "Bearer second-token"]);
}

#[tokio::test]
async fn a_missing_token_file_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer())
        .mount(&server)
        .await;
    let issuer = BrokerIssuer::new(
        server.uri(),
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("run-cert-token-never-written"),
        RunId::new(),
    )
    .unwrap();
    let error = issuer.issue(&request()).await.unwrap_err();
    assert_eq!(error.class(), ErrorClass::Unauthenticated);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a token that cannot be read sends no request"
    );
    assert!(
        std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<TokenError>())
            .is_some(),
        "the source says it was the token: {error:?}"
    );
}

#[tokio::test]
async fn an_answer_that_is_not_a_chain_is_internal() {
    for (label, answer) in [
        ("empty", json!({"v": 1, "chain": []})),
        ("bad base64", json!({"v": 1, "chain": ["not base64!"]})),
        (
            "too many",
            json!({"v": 1, "chain": ["YQ==", "YQ==", "YQ==", "YQ==", "YQ=="]}),
        ),
        ("no v", json!({"chain": ["YQ=="]})),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer))
            .mount(&server)
            .await;
        let issuer = BrokerIssuer::new(
            server.uri(),
            token_file("bad-answer", "a-token"),
            RunId::new(),
        )
        .unwrap();
        let error = issuer.issue(&request()).await.unwrap_err();
        assert!(
            matches!(error, RunCertError::Internal(_)),
            "{label}: {error:?}"
        );
    }
}

#[tokio::test]
async fn no_rendering_or_log_holds_the_token() {
    let issued = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(answer())
        .mount(&issued)
        .await;
    let refused = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&refused)
        .await;

    let path = token_file("sentinel", SENTINEL);
    let issuer = Arc::new(BrokerIssuer::new(issued.uri(), &path, RunId::new()).unwrap());
    let dynamic: DynRunCertIssuer = issuer.clone();
    let refused_issuer = BrokerIssuer::new(refused.uri(), &path, RunId::new()).unwrap();

    // The subscriber is global, not thread-local: a callsite hit by another test with no
    // subscriber is cached as disabled, and a thread-local default does not revive it.
    let buffer = logs();
    buffer.lock().unwrap().clear();
    issuer.issue(&request()).await.unwrap();
    let error = refused_issuer.issue(&request()).await.unwrap_err();

    let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("the broker issued the run's certificate"),
        "the capture is not vacuous: {logs}"
    );
    let renderings = [
        format!("{issuer:?}"),
        format!("{dynamic:?}"),
        format!("{refused_issuer:?}"),
        format!("{error}"),
        format!("{error:?}"),
        logs,
    ];
    for rendering in &renderings {
        assert!(
            !rendering.contains(SENTINEL),
            "a rendering leaks the token: {rendering}"
        );
    }

    // The token really was presented: the absence above is not "no token was used".
    let received = issued.received_requests().await.unwrap();
    assert_eq!(
        received[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        format!("Bearer {SENTINEL}")
    );
}

/// The process's log buffer: one subscriber, installed once, writing only this crate's target.
fn logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || BufferWriter(writer.clone()))
                .with_filter(Targets::new().with_target(
                    "adam_run_cert_broker",
                    tracing::level_filters::LevelFilter::TRACE,
                )),
        );
        tracing::subscriber::set_global_default(subscriber)
            .expect("the test binary installs one subscriber");
        buffer
    })
    .clone()
}

/// A `MakeWriter` that keeps what a subscriber formats, so a test can assert what was logged.
struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn the_issuer_names_itself_and_the_trait_debug_shows_only_that() {
    let issuer = BrokerIssuer::new(
        "https://broker.internal:8443",
        token_file("kind", "a-token"),
        RunId::new(),
    )
    .unwrap();
    assert_eq!(issuer.kind(), "broker");
    let dynamic: DynRunCertIssuer = Arc::new(issuer);
    assert_eq!(format!("{dynamic:?}"), "RunCertIssuer(broker)");
}
