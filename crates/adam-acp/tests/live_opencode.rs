//! Live test against a real `opencode acp`. Skipped unless `ADAM_TEST_OPENCODE=1`,
//! `opencode` is on `PATH` and a model is configured (OpenCode's own config, or
//! `ADAM_TEST_OPENCODE_CONFIG` as inline JSON for `OPENCODE_CONFIG_CONTENT`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::time::Duration;

use adam_acp::{AcpClient, AcpCommand, AcpUpdate, ClientPolicy};
use futures::StreamExt as _;

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

#[tokio::test]
async fn opencode_creates_a_file() {
    if std::env::var("ADAM_TEST_OPENCODE").as_deref() != Ok("1") {
        eprintln!("skipping: ADAM_TEST_OPENCODE is not 1");
        return;
    }
    if !on_path("opencode") {
        eprintln!("skipping: opencode is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = AcpCommand::opencode(dir.path());
    if let Ok(config) = std::env::var("ADAM_TEST_OPENCODE_CONFIG") {
        cmd = cmd.with_config_content(config);
    }
    let client = AcpClient::spawn(cmd, ClientPolicy::new(dir.path()))
        .await
        .expect("spawn");
    eprintln!("agent: {:?}", client.agent_info());
    let session = client
        .new_session(dir.path(), vec![])
        .await
        .expect("new_session");
    let mut turn = session.prompt("create hello.txt containing hi".into());
    let mut stop = None;
    let result = tokio::time::timeout(Duration::from_secs(300), async {
        while let Some(item) = turn.next().await {
            match item.expect("turn item") {
                AcpUpdate::TurnEnded { stop_reason } => stop = Some(stop_reason),
                other => eprintln!("update: {other:?}"),
            }
        }
    })
    .await;
    assert!(result.is_ok(), "turn did not finish in time");
    assert_eq!(stop.as_deref(), Some("end_turn"));
    let content = std::fs::read_to_string(dir.path().join("hello.txt")).expect("hello.txt");
    assert!(content.contains("hi"), "{content:?}");
    client.shutdown().await.expect("shutdown");
}
