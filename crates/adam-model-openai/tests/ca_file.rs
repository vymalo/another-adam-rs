//! `MODEL_CA_FILE` (ADR 0067 decision 9): a model endpoint whose certificate a test CA signed is
//! reached only once the client trusts that CA beside its default roots. Against a local TLS
//! server; the CA, the server's certificate and its key are test fixtures, valid until 2126, that
//! sign nothing else (`tests/fixtures/`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use adam_model::{Message, ModelClient, ModelRequest};
use adam_model_openai::{
    ExtraRoots, MAX_CA_FILE_BYTES, OpenAiCompatible, OpenAiConfig, OpenAiConfigError,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use secrecy::SecretString;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A TLS server on the loopback that answers every request with one completion. Returns its port
/// and how many requests reached it (a refused handshake is none).
fn tls_server() -> (u16, Arc<AtomicUsize>) {
    let certs = CertificateDer::pem_file_iter(fixture("test-server.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("test-server.key")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let config = config.clone();
            let count = count.clone();
            std::thread::spawn(move || {
                let _ = answer(stream, config, &count);
            });
        }
    });
    (port, served)
}

fn answer(
    stream: std::net::TcpStream,
    config: Arc<rustls::ServerConfig>,
    served: &AtomicUsize,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let conn = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut tls = BufReader::new(rustls::StreamOwned::new(conn, stream));
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if tls.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    tls.read_exact(&mut body)?;
    served.fetch_add(1, Ordering::SeqCst);
    let reply = r#"{"choices":[{"message":{"content":"over tls"},"finish_reason":"stop"}]}"#;
    let tls = tls.get_mut();
    write!(
        tls,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
        reply.len()
    )?;
    tls.flush()?;
    tls.conn.send_close_notify();
    tls.flush()
}

fn client(port: u16) -> OpenAiCompatible {
    let mut config = OpenAiConfig::new(
        format!("https://localhost:{port}/v1"),
        SecretString::from(""),
    );
    config.timeout = Duration::from_secs(10);
    OpenAiCompatible::new(config).expect("client")
}

fn request() -> ModelRequest {
    let mut req = ModelRequest::new("gw-model");
    req.messages.push(Message::user_text("hi"));
    req
}

#[tokio::test]
async fn the_endpoint_is_reached_only_with_the_ca() {
    let (port, served) = tls_server();

    let err = client(port).complete(request()).await.unwrap_err();
    // A handshake that fails is no endpoint that waiting brings back: `Transient`, as before, and
    // never `Unreachable`, which parks a run once its retries are spent (ADR 0067 decision 3).
    assert!(
        matches!(err, adam_model::ModelError::Transient { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "transient model error: TLS handshake failed"
    );
    assert!(adam_model::Classify::is_retryable(&err));
    assert!(
        std::error::Error::source(&err).is_some(),
        "the transport's error is kept"
    );
    assert_eq!(
        served.load(Ordering::SeqCst),
        0,
        "the default roots do not hold the test CA"
    );

    let roots = ExtraRoots::read(&fixture("test-ca.pem")).unwrap();
    assert_eq!(roots.len(), 1);
    let answer = client(port)
        .with_extra_roots(&roots)
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    assert_eq!(answer.message.text(), "over tls");
    assert_eq!(served.load(Ordering::SeqCst), 1);
}

#[test]
fn a_file_that_cannot_be_used_is_refused() {
    let invalid = |e: Result<ExtraRoots, OpenAiConfigError>| match e {
        Err(OpenAiConfigError::InvalidCaFile { reason, .. }) => reason,
        other => panic!("expected InvalidCaFile, got {other:?}"),
    };
    let reason = invalid(ExtraRoots::read(&fixture("no-such-file.pem")));
    assert!(reason.contains("cannot be read"), "{reason}");
    for (pem, says) in [
        (&b""[..], "no certificate"),
        (b"not a pem file at all", "no certificate"),
        (
            // A key is PEM, and no certificate.
            &std::fs::read(fixture("test-server.key")).unwrap()[..],
            "no certificate",
        ),
        (
            b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
            "not PEM",
        ),
        (
            // Base64 that decodes, and is no certificate.
            b"-----BEGIN CERTIFICATE-----\naGVsbG8gd29ybGQ=\n-----END CERTIFICATE-----\n",
            "refuses",
        ),
    ] {
        let reason = invalid(ExtraRoots::from_pem(pem));
        assert!(reason.contains(says), "{says}: {reason}");
    }
    let roots = ExtraRoots::read(&fixture("test-ca.pem")).unwrap();
    assert_eq!(format!("{roots:?}"), "ExtraRoots(1)");

    // The cause is kept: a file that cannot be read says the reader's error.
    let err = ExtraRoots::read(&fixture("no-such-file.pem")).unwrap_err();
    assert!(std::error::Error::source(&err).is_some(), "{err:?}");

    // Over 1 MiB is no bundle of roots: refused before it is read whole.
    let dir = std::env::temp_dir().join(format!("adam-ca-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let big = dir.join("big.pem");
    let mut pem = std::fs::read(fixture("test-ca.pem")).unwrap();
    pem.resize(usize::try_from(MAX_CA_FILE_BYTES).unwrap() + 1, b'\n');
    std::fs::write(&big, &pem).unwrap();
    assert!(matches!(
        ExtraRoots::read(&big),
        Err(OpenAiConfigError::CaFileTooLarge { limit }) if limit == MAX_CA_FILE_BYTES
    ));
    pem.truncate(usize::try_from(MAX_CA_FILE_BYTES).unwrap());
    std::fs::write(&big, &pem).unwrap();
    assert_eq!(
        ExtraRoots::read(&big).unwrap().len(),
        1,
        "exactly the cap is read"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
