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
| `RuntimeTaskBackend::new(runtime, events, agent)` | `events` must be the `BroadcastSink` the runtime was built with; `agent` is the registered name, as an agent (`.agent`) or as a start-only starter (`.starter`) |
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

The backend only starts, delivers to, reads and cancels runs, never steps one,
so a front process can register the agent's `AgentStarter` instead of the
agent (`Runtime::builder(store).starter(starter)`), holding no model or
credentials, while workers with the full agent run in another process over the
same store. Nothing steps a task in the front itself.

Mapping (full table in `src/backend.rs`): a task is a run (`task_id` is the run
id); a new `SendMessage` is `Runtime::start`; a message with `taskId` is
`Runtime::deliver`, only while the task is `input-required`; `CancelTask` is
`Runtime::cancel`. Ownership is encoded in the run's durable conversation id
(`<subject>:<context id>`), so it survives restarts with no side table, and a
task owned by someone else looks like one that does not exist. Subscriptions
are built from `Runtime::view` and polling, so they work for a task started by
another process or before a restart; live events only reduce latency. Across
processes those events do not exist unless something carries them, so a
subscription of a task another process is stepping advances at the durable poll.
[`adam-notify-postgres`](../adam-notify-postgres/README.md) carries them (its
`PgEventSink` as the runtime's sink, with this backend subscribing to the
`BroadcastSink` it delivers into) and also wakes the worker on a start and
delivers a `CancelTask` to the running step at once (a `Notifier`), instead of at
the next poll. `adam-coder` wires it in for every role (`Coder::new_with`,
`Coder::control_plane_with`).

## Stable ids

The message id of a status is derived, not drawn at random per read, so a
consumer that keys on it sees each status once (a SHA-256 of length-prefixed
fields, laid out as a UUID of version 8):

* **Status messages.** The `message_id` of a task's status message is a
  function of the task id, the state and the message text. The stream event and
  every `tasks/get` snapshot of the same status carry the same id; another
  state or text gives another id. (Progress messages, which exist only in the
  live stream, keep a fresh id.)

## Errors

The backend has no error type of its own: it returns `adam_a2a::BackendError`.
`RuntimeError` is mapped by its class (see
[`adam-error`](../adam-error/README.md)), and is kept as the `source` of the
result so the A2A server can log the chain while the client sees only what the
mapping chose to say.

| `RuntimeError` class | `BackendError` | Client sees |
|---|---|---|
| `NotFound` | `TaskNotFound` | `-32001` |
| `Invalid`, `Rejected` | `InvalidParams` | `-32602` with a safe detail |
| `Transient`, `RateLimited`, `Conflict` | `Unavailable` | `-32603` "backend temporarily unavailable" |
| anything else | `Internal` | `-32603` "internal error" |

The `-32602` detail is one of: "task <id> is already <status>", "the
conversation already has an open task", the agent's own message for an `init`
rejection (`AgentError::Permanent`, such as an unreadable start message), a
store's `InvalidInput` message, or "the request was rejected". A transport or
driver text, and the conversation id (which holds the caller's subject), never
reach the client.

## Features and environment

No Cargo features, no environment variables at runtime.

## Tests

`tests/backend.rs` drives the backend with a real A2A client over HTTP and a
scripted agent: snapshot/progress/artifact/completed order, `input-required`
round trips, ownership between callers, context handling, an `init` rejection
that is `-32602` and not `-32603`
(`an_init_rejection_is_invalid_params_not_internal`,
`an_init_rejection_is_a_32602_over_http`), and a
`a_starter_only_front_accepts_a_task_a_separate_worker_completes_it`, and a
restart scenario in which a second backend (a second replica) rebuilds a
subscription from the store. Unit tests in `src/backend.rs`
(`runtime_errors_map_by_class`,
`what_a_client_is_told_carries_no_cause_and_no_conversation_id`) and
`src/convert.rs`.

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL` | also runs the restart scenario against PostgreSQL (`adam-store-postgres`); the in-memory variant always runs |
| `ADAM_TEST_REQUIRE_DB` | `1`: an unset Postgres URL fails instead of skipping (CI sets it) |

## See also

[`adam-a2a`](../adam-a2a/README.md),
[`adam-runtime`](../adam-runtime/README.md),
[`adam-coder`](../adam-coder/README.md),
[`adam-error`](../adam-error/README.md).
