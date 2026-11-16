//! The [`TestServer`] on stdio, for the tests of clients that start
//! MCP servers as child processes.
//!
//! Anything on stdout is protocol. It writes one line to stderr when it starts, and, when
//! `ADAM_MCP_TEST_STDERR_ECHO` names an environment variable, that variable's value too (so a
//! test can check that a client keeps a value it expanded out of its logs). `ADAM_MCP_TEST_INSTRUCTIONS`
//! is the text it says about itself when it initializes, and `ADAM_MCP_TEST_PROMPT` the name of one
//! prompt it serves (it answers `From stdio.`).

use std::sync::Arc;

use adam_mcp_testkit::{Counters, Script, TestPrompt, TestServer};
use rmcp::ServiceExt;

#[tokio::main]
async fn main() {
    eprintln!("adam-mcp-test-server started, pid {}", std::process::id());
    if let Some(value) = std::env::var("ADAM_MCP_TEST_STDERR_ECHO")
        .ok()
        .and_then(|name| std::env::var(name).ok())
    {
        eprintln!("echoing the variable: {value}");
    }
    let running = match server().serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("cannot start: {error}");
            std::process::exit(1);
        }
    };
    let _ = running.waiting().await;
}

/// The server, with the instructions and the prompt the environment asks for.
fn server() -> TestServer {
    let mut script = Script {
        instructions: std::env::var("ADAM_MCP_TEST_INSTRUCTIONS").ok(),
        ..Script::default()
    };
    if let Ok(name) = std::env::var("ADAM_MCP_TEST_PROMPT") {
        script
            .prompts
            .push(TestPrompt::new(&name, "A prompt of the stdio server.").user("From stdio."));
    }
    TestServer::scripted(Arc::new(Counters::default()), Arc::new(script))
}
