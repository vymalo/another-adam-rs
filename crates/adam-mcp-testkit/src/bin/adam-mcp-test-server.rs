//! The [`TestServer`](adam_mcp_testkit::TestServer) on stdio, for the tests of clients that start
//! MCP servers as child processes.
//!
//! Anything on stdout is protocol. It writes one line to stderr when it starts, and, when
//! `ADAM_MCP_TEST_STDERR_ECHO` names an environment variable, that variable's value too (so a
//! test can check that a client keeps a value it expanded out of its logs).

use adam_mcp_testkit::TestServer;
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
    let running = match TestServer::default().serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("cannot start: {error}");
            std::process::exit(1);
        }
    };
    let _ = running.waiting().await;
}
