//! The facade re-exports MCP tools under its own feature `mcp`, and only then.
#![cfg(feature = "mcp")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests assert by unwrapping

use std::sync::Arc;

use adam::agent_fs::ManifestSource;
use adam::mcp::{Env, McpPolicy, McpServers};
use adam::model::MockModel;
use adam::prelude::*;
use adam_mcp_testkit::TestHttpServer;

#[tokio::test]
async fn the_facade_connects_an_agents_mcp_json() {
    let server = TestHttpServer::start(None).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/instructions.md"),
        "---\nname: helper\ntools: ['tracker__*']\n---\nBe brief.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("agent/mcp.json"),
        format!(
            r#"{{"mcpServers": {{"tracker": {{"type": "http", "url": "{}", "tools": ["echo"]}}}}}}"#,
            server.url()
        ),
    )
    .unwrap();
    let package = adam::agent_fs::Dir::new(dir.path())
        .load()
        .unwrap()
        .into_package(adam::agent_fs::Strictness::Lenient)
        .unwrap();
    let assembly = AgentDef::from_manifest(package.agents[0].clone())
        .unwrap()
        .connect_mcp(&McpPolicy::default())
        .await
        .unwrap()
        .bind(ToolSet::new())
        .unwrap()
        .model(Arc::new(MockModel::new()), "my-model")
        .unwrap();
    assert_eq!(assembly.info()[0].tools, ["tracker__echo"]);

    // The same client, by hand: the types are the ones `connect_mcp` uses.
    let config = package.agents[0].mcp.as_ref().unwrap();
    let servers = McpServers::connect(config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    assert_eq!(servers.names(), ["tracker__echo"]);
}
