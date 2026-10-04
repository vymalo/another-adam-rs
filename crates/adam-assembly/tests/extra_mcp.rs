//! Extra MCP servers added to an agent's own (`AgentDef::with_extra_mcp_file`, the file
//! `ADAM_EXTRA_MCP_FILE` names): connected beside the agent's, never replacing one, with the same
//! parsing and `${VAR}` rules, and `optional` servers that may be down. Needs the feature `mcp`.
#![cfg(feature = "mcp")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use adam_assembly::{AgentDef, Error};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::ToolSet;
use adam_mcp::McpPolicy;
use adam_mcp_testkit::TestHttpServer;
use adam_model::MockModel;
use common::instructions;

const OWN_TOKEN: &str = "own-tok-51d0b7e2-secret";

fn def_with_own_server(url: &str) -> AgentDef {
    let mcp = format!(
        r#"{{"mcpServers": {{"linear": {{"type": "http", "url": "{url}",
            "headers": {{"Authorization": "Bearer ${{OWN_TOKEN}}"}}}}}}}}"#
    );
    common::def(&[
        (
            "agent/instructions.md",
            &instructions("name: root", "You are the root."),
        ),
        ("agent/mcp.json", &mcp),
    ])
    .env("OWN_TOKEN", OWN_TOKEN)
}

struct Extra {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn extra(text: &str) -> Extra {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, text).unwrap();
    Extra { _dir: dir, path }
}

fn tools_of(def: AgentDef) -> Vec<String> {
    let assembly = def
        .bind(ToolSet::new())
        .unwrap()
        .model(Arc::new(MockModel::new()), "default")
        .unwrap();
    let mut names: Vec<String> = assembly
        .info()
        .iter()
        .find(|i| i.name == "root")
        .unwrap()
        .tools
        .clone();
    names.sort();
    names
}

#[tokio::test]
async fn extra_servers_are_connected_beside_the_agents_own() {
    let own = TestHttpServer::start(None).await;
    let added = TestHttpServer::start(Some("extra-key-9a1c")).await;
    let file = extra(&format!(
        r#"{{"mcpServers": {{"websearch": {{"type": "http", "url": "{}", "tools": ["echo"],
            "headers": {{"Authorization": "Bearer ${{EXTRA_KEY}}"}}}}}}}}"#,
        added.url()
    ));
    let (def, warnings) = def_with_own_server(&own.url())
        .env("EXTRA_KEY", "extra-key-9a1c")
        .with_extra_mcp_file(&file.path)
        .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    // The names of both files' variables: what a deployment hides and redacts.
    assert_eq!(
        def.mcp_env_references().into_iter().collect::<Vec<_>>(),
        ["EXTRA_KEY", "OWN_TOKEN"]
    );
    let def = def.connect_mcp(&McpPolicy::default()).await.unwrap();
    let tools = tools_of(def);
    assert!(tools.contains(&"websearch__echo".to_owned()), "{tools:?}");
    assert!(tools.contains(&"linear__echo".to_owned()), "{tools:?}");
    assert!(
        !tools.contains(&"websearch__pid".to_owned()),
        "the allow-list applies: {tools:?}"
    );
}

#[tokio::test]
async fn a_server_both_files_have_is_refused_and_nothing_is_replaced() {
    let own = TestHttpServer::start(None).await;
    let file = extra(
        r#"{"mcpServers": {"linear": {"command": "evil"}, "other": {"command": "x"},
            "websearch": {"command": "y"}}}"#,
    );
    let error = def_with_own_server(&own.url())
        .with_extra_mcp_file(&file.path)
        .unwrap_err();
    assert_eq!(error.class(), ErrorClass::Invalid);
    let Error::Manifest(adam_agent_fs::Error::Invalid { diagnostics }) = &error else {
        panic!("{error}");
    };
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let text = diagnostics[0].to_string();
    assert!(
        text.contains("linear") && text.contains("mcp.json"),
        "{text}"
    );
    assert!(
        text.contains(&file.path.display().to_string()),
        "names the file: {text}"
    );
}

#[test]
fn a_file_that_cannot_be_read_or_has_errors_is_refused_with_its_path() {
    let def = || def_with_own_server("https://own.example.com/mcp");
    let missing = Path::new("/definitely/not/here/mcp.json");
    let error = def().with_extra_mcp_file(missing).unwrap_err();
    assert!(
        matches!(&error, Error::Manifest(adam_agent_fs::Error::Io { path, .. }) if path == missing),
        "{error}"
    );

    let broken = extra("{ not json");
    let error = def().with_extra_mcp_file(&broken.path).unwrap_err();
    let text = format!("{error:?}");
    assert!(text.contains("invalid JSON"), "{text}");
    assert_eq!(error.class(), ErrorClass::Invalid);

    // A server with a mistake (a url without a type) is an error too, not a server left out.
    let bad = extra(r#"{"mcpServers": {"s": {"url": "https://s.example.com"}}}"#);
    assert!(def().with_extra_mcp_file(&bad.path).is_err());
}

#[tokio::test]
async fn an_optional_extra_server_that_is_down_does_not_stop_startup() {
    let own = TestHttpServer::start(None).await;
    let mut down = TestHttpServer::start(None).await;
    let down_url = down.url();
    down.stop().await;
    let file = extra(&format!(
        r#"{{"mcpServers": {{"websearch": {{"type": "http", "url": "{down_url}", "optional": true}}}}}}"#
    ));
    let (def, _) = def_with_own_server(&own.url())
        .with_extra_mcp_file(&file.path)
        .unwrap();
    let def = def.connect_mcp(&McpPolicy::default()).await.unwrap();
    let tools = tools_of(def);
    assert!(
        tools.iter().all(|t| !t.starts_with("websearch__")),
        "{tools:?}"
    );
    assert!(tools.contains(&"linear__echo".to_owned()), "{tools:?}");

    // The same server, required: a transient error (exit 69 in the binaries).
    let required = extra(&format!(
        r#"{{"mcpServers": {{"websearch": {{"type": "http", "url": "{down_url}"}}}}}}"#
    ));
    let (def, _) = def_with_own_server(&own.url())
        .with_extra_mcp_file(&required.path)
        .unwrap();
    let error = def.connect_mcp(&McpPolicy::default()).await.unwrap_err();
    assert_eq!(error.class(), ErrorClass::Transient, "{error}");
}
