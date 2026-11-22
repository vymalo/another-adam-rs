//! `MODEL_PROMPT_CACHE_KEY` and `MODEL_ALIASES_FILE`, from the variable to the bytes on the wire:
//! `ModelConfig::parse`, then `client()`, then a request to a local server that keeps what it was
//! sent.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;

use adam_model::{Message, ModelClient, ModelRequest, ReasoningEffort};
use adam_service::ModelConfig;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A server that answers one chat completion and says what body it was sent.
async fn serve_once() -> (String, tokio::task::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut chunk = [0u8; 4096];
        let (head_end, length) = loop {
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
                break (end + 4, length);
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
        serde_json::from_slice(&received[head_end..head_end + length]).unwrap()
    });
    (url, handle)
}

async fn sent(extra: &[(&str, &str)]) -> Value {
    sent_with(extra, None).await
}

async fn sent_with(extra: &[(&str, &str)], level: Option<ReasoningEffort>) -> Value {
    let (url, body) = serve_once().await;
    let mut vars: HashMap<&str, String> = HashMap::from([
        ("MODEL_BASE_URL", url),
        ("MODEL_API_KEY", String::new()),
        ("MODEL", "m".to_owned()),
    ]);
    for (k, v) in extra {
        vars.insert(k, (*v).to_owned());
    }
    let mut problems = Vec::new();
    let config = ModelConfig::parse(&|name| vars.get(name).cloned(), &mut problems);
    assert!(problems.is_empty(), "{problems:?}");
    let mut request = ModelRequest::new("m");
    request.messages.push(Message::user_text("hi"));
    request.prompt_cache_key = Some("0123456789abcdef0123456789abcdef".into());
    request.reasoning_effort = level;
    config.client().unwrap().complete(request).await.unwrap();
    body.await.unwrap()
}

#[tokio::test]
async fn the_variable_decides_whether_the_key_goes_on_the_wire() {
    let on = sent(&[("MODEL_PROMPT_CACHE_KEY", "conversation")]).await;
    assert_eq!(on["prompt_cache_key"], "0123456789abcdef0123456789abcdef");
    for off in [&[][..], &[("MODEL_PROMPT_CACHE_KEY", "off")][..]] {
        let body = sent(off).await;
        assert!(body.get("prompt_cache_key").is_none(), "{body}");
    }
}

#[tokio::test]
async fn an_extra_body_member_wins_over_the_key() {
    let body = sent(&[
        ("MODEL_PROMPT_CACHE_KEY", "conversation"),
        ("MODEL_EXTRA_BODY", r#"{"prompt_cache_key":"deployment"}"#),
    ])
    .await;
    assert_eq!(body["prompt_cache_key"], "deployment");
}

/// The profiles file is the only input: a temp `MODEL_ALIASES_FILE` gives `m` a wire, and the
/// typed level reaches the body through it — after `MODEL_EXTRA_BODY`, which it wins over. With no
/// file the level sends nothing, which is the bytes of today.
#[tokio::test]
async fn the_profiles_file_puts_the_typed_level_on_the_wire() {
    let dir = std::env::temp_dir().join(format!("adam-model-wire-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("model-profiles.json");
    std::fs::write(
        &file,
        r#"{"profiles":{"m":{"reasoningEfforts":["low","medium"],"reasoningWire":"reasoning_effort"}}}"#,
    )
    .unwrap();
    let shown = file.display().to_string();
    let level = Some(ReasoningEffort::Low);

    let body = sent_with(
        &[
            ("MODEL_ALIASES_FILE", &shown),
            ("MODEL_EXTRA_BODY", r#"{"reasoning_effort":"medium"}"#),
        ],
        level,
    )
    .await;
    assert_eq!(body["reasoning_effort"], "low", "{body}");

    // No file: the request's level is not a wire, so nothing is sent.
    let body = sent_with(&[], level).await;
    assert!(body.get("reasoning_effort").is_none(), "{body}");

    // A model the file does not declare is the same as no file.
    std::fs::write(
        &file,
        r#"{"profiles":{"other":{"reasoningEfforts":["low"],"reasoningWire":"reasoning_effort"}}}"#,
    )
    .unwrap();
    let body = sent_with(&[("MODEL_ALIASES_FILE", &shown)], level).await;
    assert!(body.get("reasoning_effort").is_none(), "{body}");

    std::fs::remove_dir_all(&dir).unwrap();
}
