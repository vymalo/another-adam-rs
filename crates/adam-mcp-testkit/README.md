# adam-mcp-testkit

A scriptable MCP server for testing MCP clients, over stdio and over streamable HTTP. Not published
(`publish = false`): it is test code, and it panics and exits on purpose. It serves the tests of
[`adam-mcp`](../adam-mcp/README.md), [`adam-assembly`](../adam-assembly/README.md) and the `adam` facade.

Built on the server side of [`rmcp`](https://crates.io/crates/rmcp) 3.5 (features `server`, `transport-io`,
`transport-streamable-http-server`), so a client is tested against a real MCP peer and not a mock of the
client's own idea of one.

## The tools

`TestServer` is the `ServerHandler`. Its tools are made to be asserted on:

| Tool | Arguments | Answers |
|---|---|---|
| `echo` | `text` | the text |
| `fail` | none | an error result (`isError`) saying `failed on purpose` |
| `big` | `bytes` | that many `x` (at most 8 MiB) |
| `mixed` | none | text, an image, an audio clip, an embedded text resource, a blob and a resource link |
| `env` | `name` | the value of the variable in the server's environment, or `(unset)` |
| `pid` | none | the server's process id |
| `exit` | none | nothing: the process exits during the call |
| `slow` | none | nothing, ever |
| `a.b` | none | `dotted`: a tool whose name no model provider accepts (listed with no description and a schema without a type) |

## Over stdio

The binary `adam-mcp-test-server` serves `TestServer` on stdin and stdout; `env!("CARGO_BIN_EXE_adam-mcp-test-server")`
is its path in the tests of this package (`tests/stdio.rs`, which tests `adam-mcp` against a child process; the
stdio tests live here because only the owning package gets that variable). It writes `adam-mcp-test-server
started, pid N` to stderr, and, when `ADAM_MCP_TEST_STDERR_ECHO` names an environment variable, that variable's
value too, so a test can check that a client keeps a value it expanded out of its logs.

## Over HTTP

```rust
let mut server = TestHttpServer::start(Some("s3cret")).await;   // 401 without `Authorization: Bearer s3cret`
let url = server.url();                 // http://127.0.0.1:<port>/mcp
server.authorizations();                // the `Authorization` header of every request that had one
server.requests();                      // every request, accepted or not
server.posts();                         // the POSTs among them: each JSON-RPC request or notification, and a resend
server.initializations();               // MCP `initialize` requests served, across restarts
server.calls();                         // tool calls that reached the handler (`slow` counts on arrival)
server.stop().await;                    // the listener closes and the sessions end
server.restart().await;                 // the same port again, with no sessions
```

The server keeps its counters across `restart`, so a test can say "one `initialize`, then one more after the server
came back".

## Test helpers

* `LogCapture::start()`: everything logged on this thread (at every level, so from `rmcp`, `hyper` and `reqwest`
  too) while it lives, as `text()`. Use it in `#[tokio::test]` (a current-thread runtime), where the tasks of the
  test run on the test's thread. One global subscriber is installed once and lines are routed by thread: scoped
  subscribers (`tracing::subscriber::set_default`) miss events when tests share a process, because a callsite
  caches whether anybody listens.
* `wait_until(what, || async { condition })`: polls with a 20 second deadline and fails the test with `what`; the
  tests never sleep for a fixed time and hope.

## Tests

`tests/stdio.rs` (see [`adam-mcp`](../adam-mcp/README.md#tests)). The rest of the kit is exercised by the tests
that use it.
