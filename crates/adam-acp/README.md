# adam-acp

An ACP (Agent Client Protocol) client that drives a coding agent over stdio,
first of all `opencode acp`, plus a scripted fake agent for tests.

## Where it sits

An **adapter** for an external agent protocol; nothing in the core depends on
it. [`adam-coder`](../adam-coder/README.md) uses it to have OpenCode change
code inside a worktree prepared by
[`adam-workspace`](../adam-workspace/README.md). The client also *serves* the
agent's requests: file reads and writes under a policy root, and permission
prompts.

## API at a glance

| Item | What |
|---|---|
| `AcpCommand` | the program to spawn: `new(program, cwd)`, `opencode(cwd)`, `.arg(..)`, `.env(k, v)`, `.with_config_content(json)` |
| `AcpClient` | `spawn(cmd, policy)`, `spawn_with(..)` (with `AcpOptions`), `agent_info()`, `new_session(cwd, mcp_servers)`, `shutdown()`, `kill()` |
| `Session` | `prompt(text)` returns a stream of `AcpUpdate`; `cancel()`; `id()` |
| `AcpUpdate`, `PlanEntry`, `McpServerSpec` | streamed updates (text, thoughts, plan, tool calls, `TurnEnded { stop_reason }`) |
| `ClientPolicy` | `ClientPolicy::new(fs_root)`, `.with_permission(mode)`: the directory the agent may read and write, and how permissions are answered. `terminal: true` is not implemented and makes `spawn` fail with `AcpError::Config` |
| `PermissionMode` (`AllowWithinRoot` default, `DenyAll`, `Ask(prompt)`), `PermissionPrompt`, `StaticPrompt`, `PermissionRequest`, `PermissionDecision`, `PermissionChoice`, `PermissionKind` | how `session/request_permission` is answered |
| `AcpError`, `AcpResult` | errors; `#[non_exhaustive]`, see *Errors* |

```rust
use adam_acp::{AcpClient, AcpCommand, AcpUpdate, ClientPolicy};
use futures::StreamExt as _;

let dir = std::path::Path::new("/work/tree");
let client = AcpClient::spawn(AcpCommand::opencode(dir), ClientPolicy::new(dir)).await?;
let session = client.new_session(dir, vec![]).await?;
let mut turn = session.prompt("create hello.txt containing hi".into());
while let Some(update) = turn.next().await {
    if let AcpUpdate::TurnEnded { stop_reason } = update? {
        println!("done: {stop_reason}");
    }
}
client.shutdown().await?;
```

What the client answers for the agent: `fs/read_text_file` and
`fs/write_text_file` only under `ClientPolicy::fs_root` (writes are
idempotent), `session/request_permission` per `PermissionMode`, everything
else (`terminal/*`, ...) with `method_not_found`. Symlink and `..` escapes
out of `fs_root` are refused.

## The fake agent

`adam-acp-fake-agent` (`src/bin/adam-acp-fake-agent.rs`) is a scripted ACP
agent for tests, not for production. Build it with
`cargo build -p adam-acp --bin adam-acp-fake-agent`
(`target/<profile>/adam-acp-fake-agent`). Scenarios are chosen with
`FAKE_ACP_SCENARIO` (`script`, `slow`, `stubborn`, `crash`, `write-file`,
`hang-init`, `garbage-stdout`, `prompt-error`, `session-error`, `stop-reason`,
`crash-once`); the table of scenarios and the `FAKE_ACP_*` variables each one
reads is in the binary's module docs.

## Errors

`AcpError` implements `adam_error::Classify` (see
[`adam-error`](../adam-error/README.md)).

| Variant | Class |
|---|---|
| `Exited`, `Timeout` | `Transient` |
| `AuthRequired` | `Unauthenticated` |
| `Config`, `Rpc` with code `-32602` | `Invalid` |
| `Protocol` | `Corrupt` |
| `TurnInProgress`, `Closed` | `Rejected` |
| `Spawn`, any other `Rpc` | `Internal` |

`is_retryable()` (from `Classify`) keeps its meaning: retrying on a **fresh**
agent process may succeed, which holds only for a crashed agent (`Exited`) or a
stalled turn (`Timeout`). A missing binary, bad configuration and a protocol
violation are not retryable. `Spawn` keeps the OS error as its `source` and its
message no longer repeats it, so `adam_error::report` prints the cause once.

## Features and environment

No Cargo features. Runtime configuration is the `AcpCommand` (program,
arguments, working directory, `OPENCODE_CONFIG_CONTENT`); the crate reads no
environment variables itself.

*Unverified:* how `opencode acp` behaves (which requests it sends, its
configuration keys) is taken from OpenCode's sources as recorded in
[`adam-coder`'s `src/opencode.rs`](../adam-coder/src/opencode.rs), not
re-checked here.

## Tests

* `tests/fake_agent.rs`: the client against the fake agent (updates, `fs_root` and symlink
  escapes, the permission modes, cancel, idle timeout, crash and hang handling,
  killing the child and its process group). Always
  runs; the binary comes from `CARGO_BIN_EXE_adam-acp-fake-agent`.
* `tests/live_opencode.rs`: a real `opencode acp` creating a file.
* Unit tests in `src/error.rs` (`class_table`,
  `spawn_display_does_not_repeat_its_source`), `src/command.rs`, `src/guard.rs`
  and `src/update.rs`.

| Variable | Meaning |
|---|---|
| `ADAM_TEST_OPENCODE` | must be `1` (and `opencode` on `PATH`) to run the live test; otherwise it skips |
| `ADAM_TEST_OPENCODE_CONFIG` | optional inline JSON passed as `OPENCODE_CONFIG_CONTENT` |

The live test is not run in CI.

## See also

[`adam-coder`](../adam-coder/README.md),
[`adam-workspace`](../adam-workspace/README.md),
[`adam-error`](../adam-error/README.md).
