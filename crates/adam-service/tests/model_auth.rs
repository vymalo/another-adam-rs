//! `MODEL_AUTH=injector` (ADR 0067), from the variable to the headers on the wire:
//! `ModelConfig::parse`, then `client()`, then a request to a local server that keeps the head it
//! was sent.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;

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
