//! `adam-mcp` against the test server as a child process. These tests live in the testkit's
//! package because only the package that owns a binary gets `CARGO_BIN_EXE_<name>`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use adam_agent_fs::McpConfig;
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::{ToolCtx, ToolOutput, ToolSet};
use adam_mcp::{Env, Error, McpPolicy, McpServers};
use adam_mcp_testkit::{LogCapture, wait_until};
use adam_runtime::NoopSink;
use serde_json::{Value, json};

const SERVER: &str = env!("CARGO_BIN_EXE_adam-mcp-test-server");

/// One stdio server per `(name, command, env)`, as `mcp.json` reads it.
fn stdio_config(servers: &[(&str, &str, Value)]) -> McpConfig {
    let entries: serde_json::Map<String, Value> = servers
        .iter()
        .map(|(name, command, env)| ((*name).to_owned(), json!({"command": command, "env": env})))
        .collect();
    let text = json!({ "mcpServers": entries }).to_string();
    let mut diagnostics = Vec::new();
    adam_agent_fs::parse_mcp(Path::new("mcp.json"), &text, &mut diagnostics)
        .unwrap_or_else(|| panic!("{diagnostics:?}"))
}

fn one(env: Value) -> McpConfig {
    stdio_config(&[("t", SERVER, env)])
}

fn allowed() -> McpPolicy {
    McpPolicy::default().allow_stdio(true)
}

async fn call(tools: &ToolSet, name: &str, args: Value) -> ToolOutput {
    let ctx = ToolCtx::detached(name, "c1", std::sync::Arc::new(NoopSink));
    tools.get(name).unwrap().call(&ctx, args).await.unwrap()
}

async fn pid_of(tools: &ToolSet) -> u32 {
    call(tools, "t__pid", json!({}))
        .await
        .content
        .parse()
        .unwrap()
}

/// Whether a process is still running (Linux). A killed child that nobody has reaped yet is a
/// zombie (state `Z`): its `/proc/<pid>` is still there, and it is dead. When the runtime is gone
/// nothing reaps it until the test process ends, so a zombie counts as gone.
fn alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) S ...`: `comm` may hold spaces and parentheses, so the state is the first
    // field after the last `)`.
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next());
    !matches!(state, Some("Z" | "X"))
}

#[tokio::test]
async fn lists_and_calls_over_stdio() {
    let servers = McpServers::connect(&one(json!({})), &Env::new(), &allowed())
        .await
        .unwrap();
    let names = servers.names();
    assert!(
        names.contains(&"t__echo".to_owned()) && !names.iter().any(|n| n.contains("a.b")),
        "{names:?}"
    );
    let tools = servers.tools();
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "over a pipe"}))
            .await
            .content,
        "over a pipe"
    );
    assert!(call(&tools, "t__fail", json!({})).await.is_error);
    servers.shutdown().await;
}

#[tokio::test]
async fn declared_env_expanded_into_child() {
    let env = Env::new().var("MCP_TEST_GREETING", "hello from the environment");
    let config = one(json!({
        "GREETING": "${MCP_TEST_GREETING}",
        "LEVEL": "${MCP_TEST_SURELY_UNSET_9C1D:-info}",
    }));
    let servers = McpServers::connect(&config, &env, &allowed())
        .await
        .unwrap();
    let tools = servers.tools();
    // The child has the value: what it reports is the whole of it, and it comes back to the model
    // scrubbed, as everything that came out of a `${VAR}` does (a result is not a place for a
    // credential). `(unset)` or a different text would not have been replaced.
    let greeting = call(&tools, "t__env", json!({"name": "GREETING"})).await;
    assert!(!greeting.is_error, "{}", greeting.content);
    assert_eq!(greeting.content, "[REDACTED]");
    assert!(!greeting.content.contains("hello"));
    // A default written in the file is not a secret: it is in the file already.
    assert_eq!(
        call(&tools, "t__env", json!({"name": "LEVEL"}))
            .await
            .content,
        "info"
    );
}

/// A variable set in this process, with its value, that is not one of the few a child is given
/// (`CARGO` if there is one; else the first by name).
fn a_variable_not_passed_on() -> (String, String) {
    const PASSED_ON: [&str; 4] = ["PATH", "HOME", "LANG", "TMPDIR"];
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, v)| !PASSED_ON.contains(&k.as_str()) && !v.is_empty() && v != "(unset)")
        .collect();
    vars.sort();
    let at = vars.iter().position(|(k, _)| k == "CARGO").unwrap_or(0);
    assert!(!vars.is_empty(), "this process has no variable to look for");
    vars.swap_remove(at)
}

#[tokio::test]
async fn child_does_not_inherit_env_by_default() {
    // A variable of this process that the child is not given by default: a child that inherited
    // would see it. (`cargo test` and `cargo nextest` set `CARGO`; a runner started some other way
    // may not, so any other will do.)
    let (name, here) = a_variable_not_passed_on();
    let servers = McpServers::connect(&one(json!({})), &Env::new(), &allowed())
        .await
        .unwrap();
    let tools = servers.tools();
    assert_eq!(
        call(&tools, "t__env", json!({"name": name})).await.content,
        "(unset)"
    );
    // What a child needs to find its programs is passed on.
    let path = call(&tools, "t__env", json!({"name": "PATH"}))
        .await
        .content;
    assert_eq!(path, std::env::var("PATH").unwrap());
    servers.shutdown().await;

    // The escape hatch.
    let inheriting = allowed().inherit_env(true);
    let servers = McpServers::connect(&one(json!({})), &Env::new(), &inheriting)
        .await
        .unwrap();
    assert_eq!(
        call(&servers.tools(), "t__env", json!({"name": name}))
            .await
            .content,
        here
    );
}

#[tokio::test]
async fn stdio_refused_without_opt_in() {
    // A command that does not exist: a spawn would fail with `Spawn`, so `StdioNotAllowed` shows
    // that nothing was tried.
    let config = stdio_config(&[("t", "adam-mcp-surely-not-a-command", json!({}))]);
    let error = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::StdioNotAllowed { server } if server == "t"),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid);
}

#[tokio::test]
async fn missing_command_fails_startup() {
    let config = stdio_config(&[("t", "adam-mcp-surely-not-a-command", json!({}))]);
    let error = McpServers::connect(&config, &Env::new(), &allowed())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Spawn { server, command, .. }
        if server == "t" && command == "adam-mcp-surely-not-a-command"),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Transient);
}

#[tokio::test]
async fn child_exiting_mid_call_gives_error_result_then_respawns() {
    let servers = McpServers::connect(&one(json!({})), &Env::new(), &allowed())
        .await
        .unwrap();
    let tools = servers.tools();
    let first = pid_of(&tools).await;

    let died = call(&tools, "t__exit", json!({})).await;
    assert!(died.is_error, "{}", died.content);
    assert!(
        died.content.contains("may or may not have run"),
        "{}",
        died.content
    );

    // The next call starts the server again, from the same recipe.
    let second = pid_of(&tools).await;
    assert_ne!(first, second, "a new process");
    wait_until("the first child to be gone", || async { !alive(first) }).await;
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "back"}))
            .await
            .content,
        "back"
    );
}

#[tokio::test]
async fn dropping_servers_kills_child() {
    let servers = McpServers::connect(&one(json!({})), &Env::new(), &allowed())
        .await
        .unwrap();
    let pid = {
        let tools = servers.tools();
        pid_of(&tools).await
    };
    assert!(alive(pid));
    // The connection lives as long as the servers or a tool of theirs does: both are gone here.
    drop(servers);
    wait_until("the child to be killed", || async { !alive(pid) }).await;
}

#[test]
fn child_is_killed_when_the_servers_are_dropped_outside_the_runtime() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (servers, pid) = runtime.block_on(async {
        let servers = McpServers::connect(&one(json!({})), &Env::new(), &allowed())
            .await
            .unwrap();
        let pid = pid_of(&servers.tools()).await;
        (servers, pid)
    });
    // No runtime context on this thread: dropping must not panic, and the child must go.
    drop(servers);
    runtime.block_on(wait_until("the child to be killed", || async {
        !alive(pid)
    }));
}

#[test]
fn child_is_killed_when_the_runtime_goes_first() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (servers, pid) = runtime.block_on(async {
        let servers = McpServers::connect(&one(json!({})), &Env::new(), &allowed())
            .await
            .unwrap();
        let pid = pid_of(&servers.tools()).await;
        (servers, pid)
    });
    drop(runtime);
    drop(servers);
    // Nothing drives a reactor any more: poll the process table on this thread. The child is
    // killed and stays a zombie (`alive` counts it as gone) until this process ends or a runtime
    // reaps orphans.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the child outlived its runtime"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn a_later_failure_kills_the_children_already_started() {
    let logs = LogCapture::start();
    // `a` starts; `b` cannot: startup fails, and `a`'s child must not be left behind.
    let config = stdio_config(&[
        ("a", SERVER, json!({})),
        ("b", "adam-mcp-surely-not-a-command", json!({})),
    ]);
    let error = McpServers::connect(&config, &Env::new(), &allowed())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Spawn { server, .. } if server == "b"),
        "{error}"
    );

    // The child says who it is on stderr, which is logged.
    let mut pid = None;
    wait_until("the child's greeting in the logs", || {
        let text = logs.text();
        pid = text
            .split("started, pid ")
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|digits| digits.parse::<u32>().ok());
        async move { pid.is_some() }
    })
    .await;
    let pid = pid.unwrap();
    wait_until("the first child to be killed", || async { !alive(pid) }).await;
}

#[tokio::test]
async fn stderr_is_logged_without_expanded_values() {
    let logs = LogCapture::start();
    let secret = "s3cr3t-value-4e88";
    let env = Env::new().var("MCP_TEST_SECRET", secret);
    let config = one(json!({
        "THE_SECRET": "${MCP_TEST_SECRET}",
        "ADAM_MCP_TEST_STDERR_ECHO": "THE_SECRET",
    }));
    let servers = McpServers::connect(&config, &env, &allowed())
        .await
        .unwrap();
    wait_until("the child's stderr in the logs", || async {
        logs.text().contains("echoing the variable")
    })
    .await;
    let text = logs.text();
    assert!(text.contains("echoing the variable: [REDACTED]"), "{text}");
    assert!(!text.contains(secret), "the secret is in the logs");
    servers.shutdown().await;
}
