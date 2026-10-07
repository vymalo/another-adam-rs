# Embed adam in your own program

adam-rs is a library first ([ADR 0001](../decisions/0001-library-first-host-roles.md)): a host binary composes
crates, and the shipped `adam-agent` and `adam-coder` are two such compositions. If you only need to serve an
agent made of files, do not embed: [Write an agent](write-an-agent.md).

## Pick a level

| You want | Depend on | Entry point |
|---|---|---|
| a folder's agent inside your process | `adam-agent` (also a library, `publish = false`) and `adam-service` | `adam_agent::agents(def, card, workers)` then `adam_service::serve(&ServiceConfig, agents, shutdown)` ([`adam-agent` README](../../bin/adam-agent/README.md#library)) |
| your own agent with Rust tools | the `adam` facade | `use adam::prelude::*`: `#[tool]`, `tools!`, `LlmAgent`; `adam::include_agent!()`; features `macros` (default), `a2a`, `mcp`, `dev` ([`adam` README](../../crates/adam/README.md)) |
| only the process plumbing | `adam-host` | `Role` and `Host`: register components with `.control_plane(name, f)` and `.worker(name, f)`; `Host::run(shutdown)` starts what the role runs and stops the control plane before the workers ([README](../../crates/adam-host/README.md)) |
| a different database | implement `Store` | [Store adapters](../reference/store-adapters.md) |

## Steps

1. **Pin by full commit.** The crates are git dependencies on one 40-hex sha, never a branch or tag, all bumped
   together (two revs mean two copies of the same traits, with errors that name types that look equal).

   ```toml
   adam-host = { git = "https://github.com/vymalo/another-adam-rs", rev = "<40-hex sha>", default-features = false, features = ["supervisor"] }
   adam-runtime = { git = "https://github.com/vymalo/another-adam-rs", rev = "<same sha>" }
   ```

2. **One TLS backend per process.** `adam-store-postgres` and `adam-notify-postgres` default to `tls-rustls`; if
   your workspace selects another `sqlx` backend, use `default-features = false` on both.
3. **Configuration is the host's.** `adam-host` never reads the environment. With `adam-service` the variables
   are those of [Environment](../reference/environment.md#both-binaries-adam-service); a host of your own owns its names.
4. **Map a failed `serve` to an exit code** with `adam_service::exit_code` (78 configuration, 69 dependency down,
   71 OS, 70 internal), so a supervisor can tell restartable from not.
5. **Leave `dev` and `mcp` off** unless you need them: a release build must not watch files or start MCP processes
   unless it opts in.

## Pitfalls

* `Role` is a closed enum: a new role at a later rev is a compile error in your `match`, on purpose
  (`runs_control_plane()` and `runs_workers()` avoid it). So are `Placement` and `ClaimScope`.
* A required trait method added at a later rev breaks implementers; the `adam-upgrade` skill walks through a bump.
* A tool is a plain `async fn` as well: unit-test it by calling it. Tools must be **safe to repeat**.

A worked consumer is `vymalo/another-agentic-system`: its crate `orchestrator/crates/agent-adam` hosts adam
agents in the orchestrator's process behind the off-by-default feature `agent-local`.

For an AI assistant doing this, use the `adam-embed` skill (`npx skills add vymalo/another-adam-rs --skill adam-embed`).
