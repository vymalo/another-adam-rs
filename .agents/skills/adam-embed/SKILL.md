---
name: adam-embed
description: "Host adam-rs agents inside your own Rust program: depend on the adam crates by git rev, choose between serving a folder (adam_agent::agents plus adam_service::serve) and your own agent with #[tool] and include_agent!, run roles with the adam-host supervisor, or embed adam in an application with no server (adam-engine). Use for 'embed adam', 'run an adam agent in our binary', 'add a #[tool]', 'agent-local feature', 'adam in a desktop or phone app'."
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
   **Moving across ADR 0055's S3 is a deploy boundary, not a bump**: a binary built before the
   contract cannot read a run the contract wrote (its `state` is the tombstone, and only a v5 binary
   reads the head), so upgrade every process that shares the database together, with no old one
   running when the new one starts. The sequence is `adam-upgrade`'s S3 row.
2. **TLS is built in.** `adam-store-postgres` and `adam-notify-postgres` use `rustls` on
   `aws-lc-rs`, and `DATABASE_URL` chooses the mode (`sslmode`, `sslrootcert`, `sslcert`,
   `sslkey`). There are no TLS Cargo features: the `tls-rustls`/`tls-native-tls` features left with
   `sqlx` (ADR 0053, decision 1), so a consumer that set them removes the line.
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
     .state(..).model(model, alias)?` binds it. An `LlmAgent` built by hand (not through the assembly) gets
     `Limits::default().max_output_tokens`, 4,096, unless it sets one: ADR 0064's profile resolution and clamp (the
     128,000 fallback) apply through `AgentDef::bind`/`model`, not the loop. A `DynModel` you build yourself
     (`OpenAiCompatible`) needs `.with_profiles(profiles)` for a `reasoning_effort` to reach the provider: without
     it the level is dropped silently (ADR 0063 decision 6; `.models(..)` on the engine does not reach the client).
     Details and the tool rules:
     `crates/adam/README.md` ("`#[tool]`", "Agent directories"), `docs/reference/agent-files.md`.
   * **A wrapper agent must delegate the split.** `adam_runtime::Agent` has provided `split`/`join`
     methods (and so does `AgentStarter`) that let a run keep its transcript in an append-only body
     beside a small head; the default keeps the whole state in the head, the v4 format. An agent of
     yours that wraps another and delegates `init`/`step` must delegate `split`/`join` too, or its
     runs silently stay in the v4 format (the same trap as `init_continuing`).
   * **Only the process plumbing**: `adam-host` (`Role`, `Host`): register every component
     (`.control_plane(name, f)`, `.worker(name, f)`), `Host::run(shutdown)` starts the ones the
     role runs and stops the control plane before the workers (`crates/adam-host/README.md`).
4. **Configuration** is the host's: `adam-host` never reads the environment. With
   `adam-service` the variables are `ROLE`, `DATABASE_URL`, `A2A_BEARER_TOKENS` (fail closed:
   none, no A2A server), `PUBLIC_URL`, `LISTEN_ADDR`, `WORKERS`, `WORKER_ID`, `MODEL_BASE_URL`,
   `MODEL_API_KEY`, `MODEL`, and the optional `MODEL_CONTEXT_WINDOW` (the window a `usage/v1` report says for the alias
   `MODEL`) (`crates/adam-service/README.md`, "Environment"). Parse yours with
   `parse_or`, `parse_flag` into the same list of problems.
5. **Exit codes**: map a failed `serve` with `adam_service::exit_code` (78 configuration, 69
   dependency down, 71 OS, 70 internal) so a supervisor can tell restartable from not.
6. **Cargo features you do not need stay off** (`dev`, `mcp`): a release build must not watch
   files or start MCP processes unless it opts in.

## In an application: `adam-engine`

For an application (a desktop or a phone app) that runs adam itself, with no server, no protocol and nothing on
the network of its own (ADR 0047, `docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md`:
https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md), depend on
`crates/adam-engine` instead of the crates above. The human guide is `docs/guides/embed-in-an-app.md`:
https://github.com/vymalo/another-adam-rs/blob/main/docs/guides/embed-in-an-app.md.

* **The host gives everything**: `Engine::builder(store, agent, model, alias)` takes a `DynStore`
  (`adam_store_sqlite::SqliteStore::open(path)`, a file a killed process's next start resumes, one engine per file;
  or `adam_core::MemoryStore`), an `AgentDef` (`AgentFolder::load` or
  `include_agent!`), a `DynModel` with its credentials (the engine links no model client) and, with `.secrets(..)`,
  a `SecretSource` (the default refuses every name), with `.tools(..)` a `ToolSet` of its own (`#[tool]`
  functions) that the files select with `tools:`, with `.models(..)` the deployment's `ModelProfiles` (without
  them an agent whose file sets `reasoning_effort` or `model_preset` is refused at `start`, fail closed) and with
  `.reasoning_effort(..)` the root's level when its file sets none — the folder's own key wins, the builder's level
  wins over the agent's `model_preset` and the profile, and it is validated when it applies (a file's own level is
  validated instead). `Say::with_model(alias)` asks that a message's run be answered by a model that has a
  profile there (anything else is `EngineError::Invalid`, nothing written; the choice holds for the run until
  a later message names another), and a folder's `model_fallback:` is the alias a run continues on, once, when
  the endpoint refuses its model as locked (ADR 0064, amended 2026-11-23). The engine reads no environment variable.
* **Then** `start().await?`, `send`, `steer`, `answer`, `cancel`, `list`, `get`, `events`, `commands` (the folder's
  `/name` commands, which a message expands), `files` and `file` (what a tool handed over, by SHA-256), `firings` (the runs
  `schedules/` fired, which `get`, `events` and `cancel` reach too), `helpers`, `start_session`, `sessions` and
  `close_session` (child sessions of the local subagents, `child-sessions/v1`) and `shutdown`; decide from `EngineError`'s class
  (`adam_error::Classify`), and match `RunState` and `EngineEvent` with a `_` arm (both `#[non_exhaustive]`). `events(run)` is bounded: a slow reader gets `Behind`, then a `Snapshot` read from the store.
* **Keep the embedded set**: anything else you depend on takes `adam-a2a` without `server`, `adam-a2a-runtime` without
  `push`, `adam-assembly` (and `adam`) without `remote`, and `adam-ui` without `thread-tools`, or Cargo turns them back
  on. adam-rs checks the engine's own tree with `tools/embedded-deps/check.sh`
  (https://github.com/vymalo/another-adam-rs/blob/main/tools/embedded-deps/check.sh); in yours,
  `cargo tree -e features -i adam-assembly` shows what turns `remote` back on. Your own model client may bring an HTTP
  client and TLS: that is the host's choice, not adam's.
* **The worked example** is `crates/adam-engine/examples/chat` (`main.rs` builds the engine, `app.rs` is the loop a
  test drives): https://github.com/vymalo/another-adam-rs/tree/main/crates/adam-engine/examples/chat. The API and its
  refusals: https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-engine/README.md.
* **Not yet**: approvals, the file and command tools, MCP
  servers and remote subagents (`start` refuses an agent that names them).

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
* `RunEvent` is not `#[non_exhaustive]`: a host that `match`es the events it observes needs an arm for each new one. Since
  ADR 0032 that includes `RunEvent::Usage` (one per completed model call); a **subagent's** report is an event of the
  **root** run (`Emitter::emit_for`), not of the child's, under the root's step it ran under. A model client of your own says
  its `provider()` and `context_window(alias)` (both default to none), and a hand-written agent that wants the task's totals
  on its A2A task keeps `usage_totals` (`adam_runtime::UsageTotals`) in its state
  (`docs/decisions/0032-usage-per-model-call.md`), and one whose tools run commands in a sandbox it meters keeps
  `sandbox_usage` (`adam_runtime::SandboxUsage`) beside it (`docs/decisions/0043-sandbox-time-as-usage.md`).

## See also

* `docs/guides/embed-adam.md`, `docs/architecture.md`, `crates/adam-host/README.md`, `crates/adam-service/README.md`,
  `crates/adam/README.md`, `bin/adam-agent/README.md`.
* `adam-agent-folder`, `adam-store-adapter`, `adam-upgrade`.
* https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-service/README.md
