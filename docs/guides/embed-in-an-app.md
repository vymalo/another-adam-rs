# Embed adam in an application

`adam-engine` runs adam-rs inside an application (a desktop or a phone app): no server, no protocol, nothing on the
network of its own ([ADR 0047](../decisions/0047-adam-rs-can-be-embedded-in-an-application.md)). To host agents in a
server program that serves A2A, read [Embed adam](embed-adam.md) instead.

## What the engine is

A typed Rust API over adam's runtime, on the host's tokio runtime: `Engine::builder(store, agent, model, alias)`,
`start`, then `send`, `steer`, `answer`, `cancel`, `list`, `get`, `events` and `shutdown`. No A2A type is in a
signature. Inside, every call is the code the A2A server calls, so follow-ups, steering and answers behave the same.
The calls, their refusals and their defaults: the [crate README](../../crates/adam-engine/README.md).

## What the host gives

| The host gives | As | Today |
|---|---|---|
| the store | `DynStore` | `adam_store_sqlite::SqliteStore::open(path)`, a file of the application's own that outlives the process (a killed process's runs are finished by the next one); `adam_core::MemoryStore` (a restart forgets every run) |
| the agent | `AgentDef` | a folder read at startup (`AgentFolder::load`), or one compiled in (`include_agent!`); its `schedules/` run, each firing a run of the agent in the store (not in `list`: nobody sent it); its `commands/` are expanded from `/name args`, and `Engine::commands()` lists them for a `/` menu |
| the model | `DynModel`, with its credentials | any `ModelClient`: the engine links no model client and reads no key |
| secrets the agent's files name | `SecretSource` (`EngineBuilder::secrets`) | none: `default_secrets()` refuses every name |
| a tokio runtime | the current one at `start` | the worker runs on it |
| limits | `Limits` (`EngineBuilder::limits`) | `Limits::default()`; among them `retry`, the runtime's retry policy (5 attempts, 1 s to 60 s) |

The engine reads no environment variable and no file it was not given.

**A restart that claims its runs at once is opt-in.** By default (`Limits::worker_id: None`) every start has a random
worker id and releases nothing, so a restarted application waits up to `lease_ttl` for the runs it was stepping, on
a store that keeps the leases (the SQLite store does not: its lock allows one process per file, and `start` clears
every lease the file holds; so run one engine per SQLite file, or a second one would take the first one's runs). Set `Some(id)` only for a store no other engine shares (a store per install): the host promises that no other live engine uses that id on the store, and `start` releases the
leases the id holds. In one process `start` refuses a second live engine with the same id on the same store
(`WorkerIdInUse`); it cannot see other processes, and a broken promise steps a run twice. Restarting in the same process,
call `shutdown` first.

## The example

`crates/adam-engine/examples/chat` is a terminal chat: `main.rs` builds the engine, `app.rs` is the loop (a test drives
it, `tests/example.rs`). Its model client, `adam-model-openai`, is a dev-dependency: the engine's own tree has none.

```rust
let model: DynModel = Arc::new(OpenAiCompatible::new(OpenAiConfig::new(base_url, key))?);
let folder = AgentFolder::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/chat"))?;
let store: DynStore = Arc::new(MemoryStore::new());
let engine = Engine::builder(store, folder.def, model, alias).start().await?;
app::run(engine, BufReader::new(tokio::io::stdin()), std::io::stdout()).await?;
```

**Ship the agent inside the application.** The example reads its folder from disk, which is the development path:
`CARGO_MANIFEST_DIR` is the build machine's path, compiled into the binary. An application compiles the folder in:

```rust
// build.rs: adam_agent_fs::build("agent").emit()?;
adam::include_agent!(); // AGENT, the folder's agent
let engine = Engine::builder(store, AgentDef::from_manifest(AGENT)?, model, alias).start().await?;
```

```sh
MODEL_BASE_URL=https://gateway.example.com/v1 MODEL_API_KEY=... MODEL=my-model \
  cargo run -p adam-engine --example chat
```

A plain line is `send` to the conversation; `/steer <text>`, `/answer <text>`, `/cancel`, `/list` and `/quit` are the
other calls. A send while the run waits is its answer, and one while it works steers it (`Turn::delivered` says which).
When the input ends (`printf 'hi\n' | ...`), the example follows the run to its end or its question, then shuts down.

## How events work

`events(run)` is a stream of `EngineEvent`: `Live(RunEvent)` (a step, a piece of text, a status), `Behind { skipped }`
(the reader lost events), `Snapshot(RunInfo)` (the run read from the store, after `Behind`) and `Ended(RunInfo)`, the
last item. A live event is a prompt and the store is the truth: on a `Status`, the example calls `get(run)` to learn
whether the run waits, and on what question. The stream is bounded however slowly it is read, and a file's bytes are
withheld from it.

```mermaid
sequenceDiagram
  participant A as app.rs
  participant E as Engine
  participant B as RuntimeTaskBackend
  participant S as Store
  participant W as Runtime worker
  participant M as ModelClient
  participant R as RunEvents
  A->>E: send(None, Say)
  E->>B: submit
  B->>S: create_run
  E-->>A: Turn
  A->>E: events(run)
  E-->>A: RunEvents
  W->>S: claim_due
  W->>M: stream
  M-->>W: ModelDelta Text
  W->>R: RunEvent TextDelta
  R-->>A: EngineEvent Live
  W->>S: commit_run, parked on ask_user
  W->>R: RunEvent Status
  R-->>A: EngineEvent Live
  A->>E: get(run)
  E-->>A: RunInfo Waiting, with the question
  A->>E: answer(run, Say)
  E->>B: submit, with the run's id
  W->>S: claim_due, then commit_run Done
  R-->>A: EngineEvent Ended, read from the store
```

## Not there yet

* **Approvals** (slice 5): a run waits only on a question today.
* **The person's directory** (slice 7): the file and command tools are a library now (slice 6), but the engine does not
  give them to its agent yet, which has the screen's tools only (`ask_user`, `show`, `ui_catalog`, `plan_*`).
* **Reading a file an agent made**: `RunInfo` carries no artifacts yet, and an event withholds a file's bytes.
* MCP servers and remote (`a2a:`) subagents: `start` refuses an agent that names them.

For an AI assistant doing this, use the `adam-embed` skill (`npx skills add vymalo/another-adam-rs --skill adam-embed`).
