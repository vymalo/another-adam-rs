# adam-a2a-runtime

`RuntimeTaskBackend`: the `adam_a2a::TaskBackend` implemented over
`adam_runtime::Runtime`, so any adam-rs agent can be served as an A2A agent.

## Where it sits

The glue between two ports: it implements the backend seam of
[`adam-a2a`](../adam-a2a/README.md) using [`adam-runtime`](../adam-runtime/README.md)
(and through it whatever `Store` the runtime holds). It is reusable by any
agent; [`adam-coder`](../adam-coder/README.md) uses it.

## API at a glance

| Item | What |
|---|---|
| `RuntimeTaskBackend::new(runtime, events, agent)` | `events` must be the `BroadcastSink` the runtime was built with; `agent` is the registered agent's name |
| `.with_poll_interval(..)`, `DEFAULT_POLL_INTERVAL` | how often a subscription re-reads the durable run |
| `.with_prompt(..)`, `.with_inbound(..)` | override how the `input-required` question is derived (`PromptFn`) and how an A2A message becomes an `Inbound` (`InboundFn`) |
| `default_prompt`, `default_inbound`, `task_state`, `artifact_of`, `artifact_id` | the default mappings |

```rust
use std::sync::Arc;
use adam_a2a::{A2aServer, AgentCardConfig, AuthConfig};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_runtime::{BroadcastSink, Runtime};

let events = BroadcastSink::default();
let runtime = Runtime::builder(store).agent(agent).event_sink(events.clone()).build();
let backend = RuntimeTaskBackend::new(runtime.clone(), events, "my-agent");
let card = AgentCardConfig::new("my-agent", "Does things", "http://localhost:8080/".parse().unwrap(), "0.1.0");
let app = A2aServer::router(card, Arc::new(backend), AuthConfig::AllowAnonymous);
// serve `app` with axum, and run `runtime.run_worker(shutdown)` next to it
```

Mapping (full table in `src/backend.rs`): a task is a run (`task_id` is the run
id); a new `SendMessage` is `Runtime::start`; a message with `taskId` is
`Runtime::deliver`, only while the task is `input-required`; `CancelTask` is
`Runtime::cancel`. Ownership is encoded in the run's durable conversation id
(`<subject>:<context id>`), so it survives restarts with no side table, and a
task owned by someone else looks like one that does not exist. Subscriptions
are built from `Runtime::view` and polling, so they work for a task started by
another process or before a restart; live events only reduce latency.

## Features and environment

No Cargo features, no environment variables at runtime.

## Tests

`tests/backend.rs` drives the backend with a real A2A client over HTTP and a
scripted agent: snapshot/progress/artifact/completed order, `input-required`
round trips, ownership between callers, context handling, and a
restart scenario in which a second backend (a second replica) rebuilds a
subscription from the store.

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL` | also runs the restart scenario against PostgreSQL (`adam-store-postgres`); the in-memory variant always runs |
| `ADAM_TEST_REQUIRE_DB` | `1`: an unset Postgres URL fails instead of skipping (CI sets it) |

## See also

[`adam-a2a`](../adam-a2a/README.md),
[`adam-runtime`](../adam-runtime/README.md),
[`adam-coder`](../adam-coder/README.md).
