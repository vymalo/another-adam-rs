//! `MODEL_AUTH=injector` and `MODEL_CA_FILE` (ADR 0067), from the variable to the wire:
//! `ModelConfig::parse`, then `client()`, then a request to a local server that keeps the head it
//! was sent, or a TLS server whose certificate a test CA signed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use adam_model::{Message, ModelClient, ModelRequest};
use adam_service::ModelConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A server that answers one chat completion and gives the request's head, lower case.
async fn serve_once() -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head, head_end, length) = loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed before it finished the request");
            received.extend_from_slice(&chunk[..n]);
            if let Some(end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&received[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                break (head, end + 4, length);
            }
        };
        while received.len() < head_end + length {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed before it finished the body");
            received.extend_from_slice(&chunk[..n]);
        }
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
        head
    });
    (url, handle)
}

async fn head_sent(vars: &[(&str, &str)]) -> String {
    let (url, head) = serve_once().await;
    let mut all: HashMap<&str, String> =
        HashMap::from([("MODEL_BASE_URL", url), ("MODEL", "m".to_owned())]);
    for (k, v) in vars {
        all.insert(k, (*v).to_owned());
    }
    let mut problems = Vec::new();
    let config = ModelConfig::parse(&|name| all.get(name).cloned(), &mut problems);
    assert!(problems.is_empty(), "{problems:?}");
    let mut request = ModelRequest::new("m");
    request.messages.push(Message::user_text("hi"));
    request.run = Some("run-root-1".into());
    config.client().unwrap().complete(request).await.unwrap();
    head.await.unwrap()
}

#[tokio::test]
async fn the_injector_sends_the_run_and_no_key() {
    let head = head_sent(&[("MODEL_AUTH", "injector")]).await;
    assert!(head.contains("\r\nx-run-id: run-root-1"), "{head}");
    assert!(!head.contains("authorization"), "{head}");

    // The key, the default, sends its key and names no run.
    let head = head_sent(&[("MODEL_API_KEY", "sk-test")]).await;
    assert!(head.contains("authorization: bearer sk-test"), "{head}");
    assert!(!head.contains("x-run-id"), "{head}");
}

/// A TLS server on the loopback with the test certificate of `adam-model-openai`'s fixtures, which
/// answers each request with one completion. Returns its port and how many requests reached it.
fn tls_server() -> (u16, Arc<AtomicUsize>) {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::io::{BufRead, BufReader, Read, Write};

    let certs = CertificateDer::pem_file_iter(fixture("test-server.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("test-server.key")).unwrap();
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::new(AtomicUsize::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (config, count) = (config.clone(), count.clone());
            std::thread::spawn(move || -> std::io::Result<()> {
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
                let conn = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
                let mut tls = BufReader::new(rustls::StreamOwned::new(conn, stream));
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if tls.read_line(&mut line)? == 0 {
                        return Ok(());
                    }
                    let line = line.trim_end().to_ascii_lowercase();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                tls.read_exact(&mut body)?;
                count.fetch_add(1, Ordering::SeqCst);
                let reply =
                    r#"{"choices":[{"message":{"content":"over tls"},"finish_reason":"stop"}]}"#;
                let tls = tls.get_mut();
                write!(
                    tls,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                )?;
                tls.flush()?;
                tls.conn.send_close_notify();
                tls.flush()
            });
        }
    });
    (port, served)
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../adam-model-openai/tests/fixtures")
        .join(name)
}

/// The endpoint's certificate is signed by a CA no default root store holds: the call reaches it
/// only with `MODEL_CA_FILE`, from the variable to the connection.
#[tokio::test]
async fn the_model_endpoint_is_reached_only_with_model_ca_file() {
    let (port, served) = tls_server();
    let base = format!("https://localhost:{port}/v1");
    let ca = fixture("test-ca.pem").display().to_string();
    let call = |with_ca: bool| {
        let mut vars: HashMap<&str, String> = HashMap::from([
            ("MODEL_BASE_URL", base.clone()),
            ("MODEL", "m".to_owned()),
            ("MODEL_API_KEY", String::new()),
        ]);
        if with_ca {
            vars.insert("MODEL_CA_FILE", ca.clone());
        }
        let mut problems = Vec::new();
        let config = ModelConfig::parse(&|name| vars.get(name).cloned(), &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        async move {
            let mut request = ModelRequest::new("m");
            request.messages.push(Message::user_text("hi"));
            config.client().unwrap().complete(request).await
        }
    };

    let refused = call(false).await.unwrap_err();
    // A handshake that fails is a connection that could not be made (ADR 0067 decision 3).
    assert!(
        matches!(refused, adam_model::ModelError::Unreachable { .. }),
        "{refused:?}"
    );
    assert_eq!(served.load(Ordering::SeqCst), 0);

    let answer = call(true).await.unwrap();
    assert_eq!(answer.message.text(), "over tls");
    assert_eq!(served.load(Ordering::SeqCst), 1);
}
