//! `adam-mcp` against the `mock-github-mcp` WireMock of `compose.yaml`: the GitHub MCP server's
//! streamable HTTP endpoint as the dev stack's coder sees it (`dev/coder-agent/mcp.json`).
//!
//! Runs only when `ADAM_TEST_MOCK_GITHUB_MCP_URL` is set (the endpoint, for example
//! `http://127.0.0.1:8085/mcp`); otherwise it passes without doing anything. Start the mock with
//! `docker compose up -d --wait mock-github-mcp`. The mock is stateless (JSON answers, no session
//! id, no standalone stream), and what this proves is that the client of this crate, `rmcp`,
//! accepts that framing: it connects, lists the twelve tools of the coder's allow-list in order,
//! calls one, and is turned away without a bearer.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use adam_error::{Classify, ErrorClass};
use adam_mcp::{Env, McpPolicy, McpServers};
use common::{call, http_config};
use serde_json::json;

/// The allow-list of the coder's `mcp.json`, in the order it lists them.
const GITHUB_TOOLS: [&str; 12] = [
    "get_me",
    "search_repositories",
    "get_file_contents",
    "list_branches",
    "list_commits",
    "get_commit",
    "search_code",
    "list_issues",
    "issue_read",
    "search_issues",
    "list_pull_requests",
    "pull_request_read",
];

fn allow_list() -> String {
    let names: Vec<String> = GITHUB_TOOLS.iter().map(|t| format!("\"{t}\"")).collect();
    format!(r#""tools": [{}]"#, names.join(", "))
}

#[tokio::test]
async fn the_dev_stacks_github_mcp_mock_is_a_server_this_client_can_use() {
    let Some(url) = std::env::var("ADAM_TEST_MOCK_GITHUB_MCP_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    else {
        eprintln!("skipping: ADAM_TEST_MOCK_GITHUB_MCP_URL not set");
        return;
    };
    let url = url.trim().to_owned();
    // Plain http to a container is for development only: the policy says so.
    let policy = McpPolicy::default().allow_insecure(true);

    // With the bearer the stack gives it, the way `dev/coder-agent/mcp.json` writes it.
    let config = http_config(
        "github",
        &url,
        &format!(
            r#""headers": {{"Authorization": "Bearer ${{GITHUB_MCP_TOKEN:-dev-github-mcp-token}}"}}, {}"#,
            allow_list()
        ),
    );
    let servers = McpServers::connect(&config, &Env::new(), &policy)
        .await
        .expect("the mock is connected and its tools listed");
    let expected: Vec<String> = GITHUB_TOOLS
        .iter()
        .map(|t| format!("github__{t}"))
        .collect();
    assert_eq!(servers.names(), expected, "the allow-list's order");

    let tools = servers.tools();
    let spec = tools.get("github__list_branches").unwrap().spec();
    assert_eq!(spec.parameters["required"], json!(["owner", "repo"]));

    let branches = call(
        &tools,
        "github__list_branches",
        json!({"owner": "local", "repo": "sandbox"}),
    )
    .await;
    assert!(!branches.is_error);
    assert_eq!(branches.content, r#"[{"name":"main"}]"#);
    let me = call(&tools, "github__get_me", json!({})).await;
    assert_eq!(me.content, r#"{"login":"dev-user"}"#);
    // The mock scripts two tools. Another is an error *result*, which the model reads.
    let other = call(
        &tools,
        "github__get_commit",
        json!({"owner": "o", "repo": "r", "sha": "s"}),
    )
    .await;
    assert!(other.is_error, "{other:?}");
    assert!(other.content.contains("not scripted"), "{other:?}");
    servers.shutdown().await;

    // A tool the server does not have is a startup error, as for any server (fail closed).
    let config = http_config(
        "github",
        &url,
        r#""headers": {"Authorization": "Bearer dev"}, "tools": ["get_me", "create_pull_request"]"#,
    );
    let error = McpServers::connect(&config, &Env::new(), &policy)
        .await
        .expect_err("a tool the server lacks is refused");
    assert!(error.to_string().contains("create_pull_request"), "{error}");

    // Without a bearer the mock answers 401, and the client does not connect: the endpoint is
    // refused at startup, not found missing in the middle of a run.
    let config = http_config("github", &url, "");
    let error = McpServers::connect(&config, &Env::new(), &policy)
        .await
        .expect_err("401 without a bearer");
    assert_eq!(error.class(), ErrorClass::Transient, "{error}");
}
