---
name: adam-embed
description: "Host adam-rs agents inside your own Rust program: depend on the adam crates by git rev, choose between serving a folder (adam_agent::agents plus adam_service::serve) and your own agent with #[tool] and include_agent!, run roles with the adam-host supervisor. Use for 'embed adam', 'run an adam agent in our binary', 'add a #[tool]', 'agent-local feature'."
---

# Embed adam-rs in your own process

adam-rs is a library first (ADR 0001, `docs/decisions/0001-library-first-host-roles.md`): a host
binary composes crates; the shipped binaries `adam-agent` and `adam-coder` are two such
compositions. This skill is for a repository that is not adam-rs and wants adam agents in its own
process. If you only need to serve an agent made of files, do not embed: use `adam-agent-folder`.

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main`
by that revision). Entry point:
https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-service/README.md. A worked
consumer is `vymalo/another-agentic-system`: its crate `orchestrator/crates/agent-adam` hosts adam
agents in the orchestrator's process behind the off-by-default Cargo feature `agent-local`, with
the adam crates pinned in `orchestrator/Cargo.toml`.

## When to use

* A Rust binary of yours must run adam agents, with its own tools (`#[tool]`), its own store
  or its own composition.
* You need the process roles (`all`, `control-plane`, `worker`) and graceful shutdown that
  `adam-host` gives.
* Not for adding a persona, MCP servers or skills to an agent: that is files (`adam-agent-folder`).
* Not for implementing a store: `adam-store-adapter`.

## Procedure

1. **Pin by full commit.** The adam crates are consumed as git dependencies pinned to one full
   sha, never a branch or a tag, all bumped together:

   ```toml
   adam-host = { git = "https://github.com/vymalo/another-adam-rs", rev = "<40-hex sha>", default-features = false, features = ["supervisor"] }
   adam-runtime = { git = "https://github.com/vymalo/another-adam-rs", rev = "<same sha>" }
   ```

   Take only the crates you use (`crates/adam-host`, `adam-core`, `adam-runtime`, `adam-a2a`,
   `adam-a2a-runtime`, `adam-store-postgres`, `adam-notify-postgres`, `adam-service`, `adam`).
2. **One TLS backend per process.** `adam-store-postgres` and `adam-notify-postgres` default to
   the feature `tls-rustls` (`sqlx/tls-rustls`); `tls-native-tls` is the other. If your workspace
   selects another sqlx TLS backend, set `default-features = false` on both crates, as the consumer
   does in `orchestrator/Cargo.toml`.
3. **Pick the composition level:**
   * **A folder's agent in your process**: `bin/adam-agent` is also a library (`adam_agent`):
     `adam_agent::agents(def, card, workers)` returns the `Agents` for
     `adam_service::serve(&ServiceConfig, agents, shutdown)`, or for a composition of your own.
     `adam_agent::folder::load(path)`, `card_of`, `assemble` are the pieces
     (`bin/adam-agent/README.md`, "Library"). `bin/adam-agent` is `publish = false`: depend on it
     by git rev too.
   * **Your own agent with Rust tools**: depend on the `adam` facade
     (`crates/adam/README.md`): `use adam::prelude::*` gives `#[tool]`, `tools!`, `Tool`, `State`,
     `LlmAgent`; features `macros` (default), `a2a` (the card), `mcp` (`mcp.json` tools), `dev`
     (reload). `adam::include_agent!()` includes the agent directory a `build.rs` embedded with
     `adam_agent_fs::build("agent").emit()`; `AgentDef::from_manifest(AGENT)?.bind(tools![..])?
     .state(..).model(model, alias)?` binds it. Details and the tool rules:
     `crates/adam/README.md` ("`#[tool]`", "Agent directories"), `docs/authoring.md`.
   * **Only the process plumbing**: `adam-host` (`Role`, `Host`): register every component
     (`.control_plane(name, f)`, `.worker(name, f)`), `Host::run(shutdown)` starts the ones the
     role runs and stops the control plane before the workers (`crates/adam-host/README.md`).
4. **Configuration** is the host's: `adam-host` never reads the environment. With
   `adam-service` the variables are `ROLE`, `DATABASE_URL`, `A2A_BEARER_TOKENS` (fail closed:
   none, no A2A server), `PUBLIC_URL`, `LISTEN_ADDR`, `WORKERS`, `WORKER_ID`, `MODEL_BASE_URL`,
   `MODEL_API_KEY`, `MODEL` (`crates/adam-service/README.md`, "Environment"). Parse yours with
   `parse_or`, `parse_flag` into the same list of problems.
5. **Exit codes**: map a failed `serve` with `adam_service::exit_code` (78 configuration, 69
   dependency down, 71 OS, 70 internal) so a supervisor can tell restartable from not.
6. **Cargo features you do not need stay off** (`dev`, `mcp`): a release build must not watch
   files or start MCP processes unless it opts in.

## Verify

* `cargo check` in your workspace with the features you enabled.
* Run the adam crates' own contract for what you added: a tool has a unit test calling the
  function directly (it stays a plain `async fn`); a store runs the testkit (`adam-store-adapter`).
* The consumer's pattern: its crate is tested alone (`cargo test -p orch-agent-adam`) and the
  binary's feature as a whole (`cargo test -p orchestrator --features agent-local`).
* Your own lints: adam-rs builds with `cargo clippy --workspace --all-targets --locked -- -D warnings`
  and its MSRV is `rust-version` in its `Cargo.toml`; check it against yours at the rev you pin.

## Pitfalls

* A branch or tag in `rev`, or crates at different revs: two copies of the same traits in one
  build, with errors that name types that look equal.
* Two TLS backends in one process (step 2).
* `Role` is closed (no `#[non_exhaustive]`): a new role at a later rev is a compile error in
  your `match`, on purpose; use `runs_control_plane()` and `runs_workers()` to avoid it.
* `ADAM_AGENT_DIR`, `ROLE` and the other variable names belong to the binaries and
  `adam-service`; a host of your own owns its names.
* A required trait method added at a later rev breaks implementers (`adam-upgrade`).

## See also

* `docs/architecture.md`, `crates/adam-host/README.md`, `crates/adam-service/README.md`,
  `crates/adam/README.md`, `bin/adam-agent/README.md`.
* `adam-agent-folder`, `adam-store-adapter`, `adam-upgrade`.
* https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-service/README.md
