# adam-service

The A2A service shared by every agent binary: the Postgres store, the runtime and its workers,
the A2A router, cross-process notifications, the process's common configuration and its exit codes.
`adam-coder` and `adam-agent` are each a composition of this crate and the agent they serve; what
differs between them (the tools, the files, the credentials) stays in the binary.

It is the generic half of what `adam-coder` used to hold itself, moved and not rewritten: the same
components, the same order, the same drain and the same log lines.

## Where it sits

```mermaid
flowchart LR
  bin["bin/adam-coder, bin/adam-agent"] --> svc["adam-service"]
  svc --> a2a["adam-a2a + adam-a2a-runtime"]
  svc --> rt["adam-runtime"]
  svc --> pg["adam-store-postgres + adam-notify-postgres"]
  svc --> host["adam-host (roles, supervisor)"]
  svc --> oa["adam-model-openai (ModelConfig::client)"]
  bin --> asm["adam / adam-assembly (the agent's files)"]
```

The binary reads its files and its own configuration, assembles its agent, and hands this crate
the agent's name, its card and a closure that registers it on the runtime. Nothing here knows what
an agent does.

## API at a glance

| Item | What |
|---|---|
| `ServiceConfig::parse(&lookup, &mut problems)` | `ROLE`, `DATABASE_URL`, `A2A_BEARER_TOKENS`, `PUBLIC_URL`, `LISTEN_ADDR`, and for a role that runs workers `WORKERS` and `WORKER_ID`. A role reads only what it uses; every problem is collected, none stops the parse |
| `WorkerSettings` | `WORKERS` (at least 1) and `WORKER_ID` (`is_worker_id`); `options()` gives the `RuntimeOptions` |
| `ModelConfig::parse`, `client()` | `MODEL_BASE_URL`, `MODEL_API_KEY` (may be empty, not unset), `MODEL`; the OpenAI-compatible client over them |
| `McpSettings::parse`, `policy()` (feature `mcp`) | `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS`; the `McpPolicy` for `AgentDef::connect_mcp` |
| `parse_or`, `parse_flag`, `is_worker_id` | the helpers a binary parses its own variables with, into the same list of problems |
| `ConfigError { problems }` | what a non-empty list becomes (`ConfigError::check(problems)`); `Classify` gives `Invalid` |
| `RuntimeOptions`, `LiveSignals` | how the runtime is set up (worker id, claim scope, concurrency, lease, poll), and how a process learns of other processes (`LiveSignals::local()` or the Postgres `NOTIFY` ones `serve` builds) |
| `Service { runtime, backend }` | `Service::new(builder, name, options)`, `new_with(.., live)`, `router(card, auth)`, `run_worker(shutdown)`; `router(&backend, card, auth)` for a composition that holds the parts itself |
| `Agents::new(name, register)` | the agent a process serves: its name, `.card(card)`, `.options(options)`, and `register`, a closure that puts it on the runtime builder (`Assembly::register`, or the starter only for a control plane) |
| `serve(&config, agents, shutdown)` | the whole process, until `shutdown` resolves; `ServeError` on failure |
| `ServeError` | `Connect`, `Migrate`, `NoCard`, `Bind`, `LocalAddr`, `Host`. The message names the step; the cause is the `source`, so a chain printed whole says it once |
| `claim_scope_for(placement)` | `Pinned` for a placement that pins runs, `Any` otherwise |
| `exit_code(&err)`, `exit_code_with(&err, classify)`, `EX_*` | the exit code of an error chain (below); `classify` is how a binary adds its own error types |

## Environment

| Variable | Meaning | Default |
|---|---|---|
| `ROLE` | `all`, `control-plane` or `worker` (`adam_host::Role`) | `all` |
| `DATABASE_URL` | Postgres for the run store | required |
| `A2A_BEARER_TOKENS` | comma-separated accepted tokens (fail closed: none = no server) | required by `all` and `control-plane` |
| `PUBLIC_URL` | where clients reach the JSON-RPC endpoint (agent card) | required by `all` and `control-plane` |
| `LISTEN_ADDR` | bind address: the A2A server, or a worker's `/healthz` listener | `0.0.0.0:8080` |
| `WORKERS` | runs advanced concurrently | `4` (roles that run workers) |
| `WORKER_ID` | lease identity: 1 to 128 of letters, digits, `.`, `_`, `-`, not starting with `.` | random per process (roles that run workers) |
| `MODEL_BASE_URL`, `MODEL_API_KEY`, `MODEL` | the model (`ModelConfig`) | required by the binaries that call `ModelConfig::parse`, for the roles that run workers |
| `MCP_ALLOW_STDIO`, `MCP_ALLOW_INSECURE`, `MCP_ALLOW_URL_VARS` | what an agent folder's MCP servers may be (`McpSettings`, feature `mcp`) | `false` each |

A binary that uses these keeps its own variables beside them (the coder's `GITHUB_TOKEN`,
`WORKSPACE_*`, ...). What a role does not use is not validated, so a chart may set a variable for every
role. Secrets are `SecretString`s and never in `Debug` output.

## The process

```mermaid
sequenceDiagram
  participant B as binary (main)
  participant S as serve
  participant P as Postgres
  participant H as Host (adam-host)
  B->>B: parse the configuration, read the agent's files, assemble the agent
  B->>S: serve(config, Agents { name, card, register, options }, shutdown)
  S->>P: connect, migrate
  S->>S: PgNotify (LISTEN/NOTIFY) as LiveSignals
  S->>S: Service::new_with(register(Runtime::builder(store)), name, options, live)
  S->>S: bind LISTEN_ADDR, log `listening` (addr, role, workers, worker_id)
  S->>H: the components of the role
  H-->>S: shutdown: drain the server, let the steps finish, flush notify
  S-->>B: Ok(()), or the first error
```

```mermaid
stateDiagram-v2
  [*] --> Connecting
  Connecting --> Listening: migrated, bound
  Connecting --> Failed: Postgres down or schema refused, address taken, no card
  Listening --> Draining: the shutdown future resolves
  Listening --> Failed: a component stops on its own
  Draining --> [*]: components stopped, notify flushed
  Failed --> [*]: the others are stopped the same way, the error is returned
```

| Role | Components | Also |
|---|---|---|
| `all` (default) | `a2a-server`, `worker`, `notify` | |
| `control-plane` | `a2a-server`, `notify` | the runtime knows the agent as a starter only: no model, no tools |
| `worker` | `worker`, `health`, `notify` | `/healthz` on `LISTEN_ADDR`, no A2A |

`notify` is the `adam-notify-postgres` listener and publisher: live events and wake-up/cancel signals cross
processes over `LISTEN`/`NOTIFY`, so a worker takes a run another process started at once and a control plane
streams the progress of a run a worker steps. It is a latency optimisation: polling stays on and correctness
never depends on a notification. With a worker it stops only after the worker has finished, then sends what is
still queued, so the last step's events normally still reach other processes (best effort, like any
notification).

The service backend serves one agent, `Agents::name`: a task is a run of that agent. Several services
may share one database, each under its own name, because a run of another agent is refused
(`RuntimeTaskBackend`) and a worker claims only the agents it registered.

## Exit codes

`exit_code` walks the error chain from the outside in and returns the first code that fits, so a supervisor
can tell a deployment that is misconfigured (do not restart) from one whose database is down (restart later)
from a bug. The values are those of BSD `sysexits.h` (*unverified*, from memory; the header is not part of this
repository's sources).

| Code | Name | Root cause |
|---|---|---|
| 0 | | clean shutdown after a signal |
| 78 | `EX_CONFIG` | `ConfigError`, `OpenAiConfigError`, `ServeError::NoCard`, or any error whose class is `Invalid` (including what a binary classifies with `exit_code_with`: a mistake in the agent's files) |
| 69 | `EX_UNAVAILABLE` | a `Transient`, `RateLimited` or `Conflict` error: Postgres, an MCP server |
| 71 | `EX_OSERR` | an `io::Error`, `ServeError::Bind`: a port that cannot be bound |
| 70 | `EX_SOFTWARE` | `HostError` (a component stopped, panicked or ended while still needed), a panicked task, a `Corrupt` or `Internal` error |
| 1 | | anything else |

A `HostError` decides whatever its source is: an `io::Error` from the server is not a 71, and a store error
from a worker not a 69.

## Features

| Feature | Default | Adds |
|---|---|---|
| `mcp` | no | `McpSettings` (brings `adam-mcp` for `McpPolicy`) |

## Tests

* `src/config.rs`: the parsers per role, problems collected together, `WORKERS` and `WORKER_ID`, the model, the
  MCP flags, secrets hidden from `Debug`.
* `src/exit.rs`: the exit-code table, a `HostError` deciding over its source, a binary's own classifier.
* `src/service.rs`: `LiveSignals::local`, the defaults.
* `tests/service.rs`: a `Service` over the in-memory store with a scripted agent: the card is served, `/healthz`
  answers, a bearer token is required, a task completes, a control plane and a worker over one store meet in it,
  and two services of different names share a store without stealing each other's runs.
* `tests/serve.rs` (needs `ADAM_TEST_POSTGRES_URL`, skipped without it; `ADAM_TEST_REQUIRE_DB=1` makes a
  missing URL a failure): `serve` end to end on Postgres, for each role.

```sh
docker run -d -e POSTGRES_PASSWORD=postgres -p 5432:5432 postgres:16
ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres ADAM_TEST_REQUIRE_DB=1 \
  cargo test -p adam-service --features mcp
```
