//! `SignerConfig::from_env`, as a configuration: what it refuses, with which class and exit code,
//! what it shows, and which CA bundle the upstream client trusts.
//!
//! The process itself — its exit codes and its logs — is `tests/binary.rs`. The CA bundle is a real
//! one: the fake run certificate issuer (`FakeRunCertIssuer`) mints leaves and its own certificate,
//! and the test PEM-encodes them, so "the file is used" is proved by the number of roots the client
//! was given, not by a stub.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use adam_core::RunId;
use adam_core::TurnKeys;
use adam_core::turn::run_cert::testkit::FakeRunCertIssuer;
use adam_core::turn::run_cert::{DynRunCertIssuer, RunKeyring, RunSubject};
use adam_core::turn::{CertificateChain, KeyIdentity};
use adam_error::{BoxError, Classify, ErrorClass};
use adam_run_signer::{ConfigError, IssuerFactory, SignerConfig, SignerError, exit_code};
use base64::Engine as _;

const UPSTREAM: &str = "RUN_SIGNER_UPSTREAM";
const LISTEN: &str = "RUN_SIGNER_LISTEN";
const CA_FILE: &str = "RUN_SIGNER_UPSTREAM_CA_FILE";

struct FakeFactory;

impl IssuerFactory for FakeFactory {
    fn issuer(&self, _run: RunId) -> Result<DynRunCertIssuer, BoxError> {
        Ok(Arc::new(FakeRunCertIssuer::new()))
    }
}

/// A configuration read from `vars`, with `ca_pem` as the caller's default trust anchors.
fn read(vars: &[(&str, &str)], ca_pem: Option<Vec<u8>>) -> Result<SignerConfig, ConfigError> {
    let vars: HashMap<_, _> = vars.iter().copied().collect();
    SignerConfig::from_lookup(
        |name| vars.get(name).map(|value| (*value).to_owned()),
        Arc::new(FakeFactory),
        ca_pem,
    )
}

fn problems(vars: &[(&str, &str)], ca_pem: Option<Vec<u8>>) -> Vec<String> {
    read(vars, ca_pem)
        .map(|_| Vec::new())
        .unwrap_or_else(|e| e.problems)
}

/// The run's leaf and its CA, as DER: a chain a client can really trust.
async fn chain() -> CertificateChain {
    let keyring = RunKeyring::new(
        Arc::new(FakeRunCertIssuer::new()),
        RunSubject::new("person-1", RunId::new()).unwrap(),
    );
    let current = keyring.current().await.unwrap();
    let KeyIdentity::Certificate(chain) = current.identity else {
        panic!("a run's key is named by its certificate, got {current:?}");
    };
    chain
}

/// `certs` as a PEM bundle, the way a CA file holds them.
fn pem(certs: &[Vec<u8>]) -> Vec<u8> {
    let engine = base64::engine::general_purpose::STANDARD;
    let mut text = String::new();
    for der in certs {
        text.push_str("-----BEGIN CERTIFICATE-----\n");
        let encoded = engine.encode(der);
        for chunk in encoded.as_bytes().chunks(64) {
            text.push_str(std::str::from_utf8(chunk).unwrap());
            text.push('\n');
        }
        text.push_str("-----END CERTIFICATE-----\n");
    }
    text.into_bytes()
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_str().unwrap().to_owned()
}

// ------------------------------------------------------------------ the two variables and a bad bind

/// The acceptance: a non-loopback bind is refused at start, as a configuration problem naming the
/// variable, with the class and the exit code the binary uses.
#[test]
fn a_non_loopback_bind_is_refused() {
    for bind in ["0.0.0.0:8443", "[::]:8443", "10.0.0.1:8443"] {
        let e = read(&[(UPSTREAM, "http://gateway:8080"), (LISTEN, bind)], None).unwrap_err();
        assert_eq!(e.problems.len(), 1, "{bind}: {e}");
        assert!(e.problems[0].contains(LISTEN), "{bind}: {e}");
        assert!(e.problems[0].contains("loopback"), "{bind}: {e}");
        assert_eq!(e.class(), ErrorClass::Invalid);
        assert_eq!(exit_code(&SignerError::Config(e)), 78);
    }
}

/// The acceptance: an empty environment says which variable is required, and the listen default is
/// loopback.
#[test]
fn an_empty_environment_names_the_required_variable() {
    let e = read(&[], None).unwrap_err();
    assert_eq!(e.problems.len(), 1, "{e}");
    assert!(e.problems[0].contains(UPSTREAM), "{e}");
    assert!(e.problems[0].contains("required"), "{e}");
    assert_eq!(e.class(), ErrorClass::Invalid);
    assert_eq!(exit_code(&SignerError::Config(e)), 78);
}

/// The acceptance: the `Debug` holds neither a CA PEM nor a value that looks like a token or a path.
#[tokio::test]
async fn a_missing_ca_is_not_a_secret_in_debug() {
    // With no CA at all: the debug says so, and nothing else.
    let config = read(
        &[(UPSTREAM, "http://127.0.0.1:8080/secret-SENTINEL-path")],
        None,
    )
    .unwrap();
    let rendered = format!("{config:?}");
    assert!(rendered.contains("upstream_ca: \"none\""), "{rendered}");
    assert!(
        rendered.contains("upstream_host: Some(\"127.0.0.1\")"),
        "{rendered}"
    );
    assert!(!rendered.contains("SENTINEL"), "{rendered}");
    assert!(!rendered.contains("secret"), "{rendered}");

    // With a CA: the certificate's bytes are not in it, and the number of roots is.
    let chain = chain().await;
    let bundle = pem(chain.certificates());
    let config = read(
        &[(UPSTREAM, "http://127.0.0.1:8080/secret-SENTINEL-path")],
        Some(bundle.clone()),
    )
    .unwrap();
    let rendered = format!("{config:?}");
    assert!(rendered.contains("upstream_ca_roots: 2"), "{rendered}");
    assert!(!rendered.contains("BEGIN CERTIFICATE"), "{rendered}");
    let body = String::from_utf8(bundle).unwrap();
    let encoded = body
        .lines()
        .find(|line| !line.starts_with("-----"))
        .unwrap()
        .to_owned();
    assert!(!rendered.contains(&encoded), "{rendered}");
    assert!(!rendered.contains("SENTINEL"), "{rendered}");
}

// --------------------------------------------------------- the upstream client's trust anchors

/// The override's bundle is the client's: the caller's default is ignored when it is set.
#[tokio::test]
async fn the_override_bundle_is_used_when_it_is_set() {
    let chain = chain().await;
    let dir = tempdir();
    let override_file = write(dir.path(), "upstream.pem", &pem(chain.certificates()));
    // A default with one certificate, and the override with two: the Debug counts say which one.
    let default = pem(&chain.certificates()[1..]);

    let config = read(
        &[
            (UPSTREAM, "https://gateway:8443"),
            (CA_FILE, &override_file),
        ],
        Some(default),
    )
    .unwrap();
    let rendered = format!("{config:?}");
    assert!(rendered.contains(CA_FILE), "{rendered}");
    assert!(rendered.contains("upstream_ca_roots: 2"), "{rendered}");
    assert!(!rendered.contains("RUN_CERT_CA_FILE"), "{rendered}");
}

/// Without the override, the caller's `ca_pem` is the client's: the default, unchanged.
#[tokio::test]
async fn the_default_bundle_is_used_when_the_override_is_unset() {
    let chain = chain().await;
    let config = read(
        &[(UPSTREAM, "https://gateway:8443")],
        Some(pem(chain.certificates())),
    )
    .unwrap();
    let rendered = format!("{config:?}");
    assert!(rendered.contains("RUN_CERT_CA_FILE"), "{rendered}");
    assert!(rendered.contains("upstream_ca_roots: 2"), "{rendered}");
    assert!(!rendered.contains(CA_FILE), "{rendered}");
}

/// A set override makes a broken default irrelevant: the default is not used, so it is not reported.
#[tokio::test]
async fn a_set_override_makes_a_broken_default_irrelevant() {
    let chain = chain().await;
    let dir = tempdir();
    let override_file = write(dir.path(), "upstream.pem", &pem(chain.certificates()));
    let config = read(
        &[
            (UPSTREAM, "https://gateway:8443"),
            (CA_FILE, &override_file),
        ],
        Some(b"this is not a pem bundle at all".to_vec()),
    )
    .unwrap();
    let rendered = format!("{config:?}");
    assert!(rendered.contains(CA_FILE), "{rendered}");
    assert!(rendered.contains("upstream_ca_roots: 2"), "{rendered}");
}

/// A file that is not there names the variable.
#[test]
fn a_missing_override_file_names_the_variable() {
    let e = read(
        &[
            (UPSTREAM, "https://gateway:8443"),
            (CA_FILE, "/nonexistent/upstream-ca.pem"),
        ],
        None,
    )
    .unwrap_err();
    assert_eq!(e.problems.len(), 1, "{e}");
    assert!(e.problems[0].contains(CA_FILE), "{e}");
    assert!(e.problems[0].contains("cannot be read"), "{e}");
    assert!(e.problems[0].contains("upstream-ca.pem"), "{e}");
    assert_eq!(e.class(), ErrorClass::Invalid);
    assert_eq!(exit_code(&SignerError::Config(e)), 78);
}

/// A file over the cap names the variable and says the bound.
#[test]
fn an_over_cap_override_file_names_the_variable() {
    let dir = tempdir();
    let path = write(
        dir.path(),
        "big.pem",
        &vec![b'A'; adam_run_cert_broker::MAX_CA_PEM_BYTES + 1],
    );
    let e = read(
        &[(UPSTREAM, "https://gateway:8443"), (CA_FILE, &path)],
        None,
    )
    .unwrap_err();
    assert_eq!(e.problems.len(), 1, "{e}");
    assert!(e.problems[0].contains(CA_FILE), "{e}");
    assert!(
        e.problems[0].contains(&adam_run_cert_broker::MAX_CA_PEM_BYTES.to_string()),
        "{e}"
    );
}

/// A file that is not PEM, and one that is empty, name the variable too.
#[test]
fn a_file_that_is_no_pem_names_the_variable() {
    let dir = tempdir();
    for (name, bytes) in [
        ("junk.pem", b"not a pem".to_vec()),
        ("empty.pem", Vec::new()),
    ] {
        let path = write(dir.path(), name, &bytes);
        let e = read(
            &[(UPSTREAM, "https://gateway:8443"), (CA_FILE, &path)],
            None,
        )
        .unwrap_err();
        assert_eq!(e.problems.len(), 1, "{name}: {e}");
        assert!(e.problems[0].contains(CA_FILE), "{name}: {e}");
        assert_eq!(e.class(), ErrorClass::Invalid);
    }
}

/// A CA file that is a directory names the variable (it cannot be read as a file).
#[test]
fn a_directory_override_names_the_variable() {
    let dir = tempdir();
    let p = problems(
        &[
            (UPSTREAM, "https://gateway:8443"),
            (CA_FILE, dir.path().to_str().unwrap()),
        ],
        None,
    );
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].contains(CA_FILE), "{p:?}");
}
