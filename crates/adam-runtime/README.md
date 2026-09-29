# adam-runtime

The durable agent-loop runtime: a run state machine, journaled side effects,
leases, retries and workers.

## Where it sits

The core of the framework. It depends on [`adam-core`](../adam-core/README.md)
only: the store (`DynStore`), the event sink and the clock are swappable
behind traits. Agents implement `Agent` and become durable; the runtime
commits every transition with a compare-and-swap on the run's version, so a
worker that dies loses nothing. Consumers:
[`adam-llm-agent`](../adam-llm-agent/README.md) (an `Agent` for model/tool
loops) and [`adam-a2a-runtime`](../adam-a2a-runtime/README.md) (serves runs
over A2A).

## API at a glance

| Item | What |
|---|---|
| `Agent` (trait) | `name`, `init(Inbound) -> State`, `async step(&mut Ctx, State) -> Transition<State>` |
| `AgentStarter` (trait) | the start-only half of an agent: `name`, `init(Inbound) -> State`, no `step`; `State` is only `Serialize`. See *Starting without stepping* |
| `Transition` | `Continue`, `Park` (timer and/or inbound message), `Done`, `Fail` |
| `AgentError` | `Transient { retry_after, .. }` (`retry_after` is a minimum wait, e.g. `Retry-After`), `Permanent`, `NonDeterminism`, `Store`; `#[non_exhaustive]`, see *Errors* |
| `Ctx`, `Emitter` | `Ctx::step` journals a side effect's outcome; `Ctx::cancelled` / `CancelToken` observe a cancel |
| `Runtime`, `RuntimeBuilder` | `Runtime::builder(store).agent(a).event_sink(s).build()`; `.starter(s)` registers a start-only agent; `start`, `start_with_id`, `deliver`, `cancel`, `view`, `run_worker(shutdown)`, `agent_names()` (every registered name) |
| `RunView`, `RuntimeError` | the durable read side, and errors (`#[non_exhaustive]`) |
| `Classify`, `ErrorClass` | re-exported from `adam-error` |
| `Inbound` | a message delivered to a run |
| `EventSink`, `RunEvent`, `BroadcastSink`, `CollectingSink`, `NoopSink`, `Artifact` | live, best-effort events |
| `RetryPolicy`, `MAX_RETRY_AFTER` | exponential backoff for transient errors |
| `Clock`, `SystemClock`, `ManualClock` | injectable time |

```rust
use adam_runtime::{BroadcastSink, Runtime};

let events = BroadcastSink::default();
let runtime = Runtime::builder(store)   // store: adam_core::DynStore
    .agent(my_agent)                    // my_agent: impl Agent
    .event_sink(events.clone())
    .build();
// let run = runtime.start("my-agent", inbound, Some("conversation-id")).await?;
// runtime.run_worker(shutdown_future).await?;   // next to your server
```

A recorded step outcome is never re-executed, but a side effect is
at-least-once (a crash between the effect and its journal write runs it
again): keep effects idempotent. The lifecycle and guarantees are in the crate
docs (`src/lib.rs`) and the [root README](../../README.md#the-model).

## Starting without stepping

Starting a run needs only a name and the initial state; stepping it needs the
whole agent, which usually holds a model, credentials and a sandbox. A process
that only accepts requests (an A2A front) registers an `AgentStarter` with
`RuntimeBuilder::starter` and holds none of those. A worker registers the
`Agent` under the same name and steps what the front started, over the same
store.

* `start` and `start_with_id` work for both kinds of registration, and an
  unknown name is still `UnknownAgent`.
* `run_worker` claims only the names registered with `.agent(..)`. A runtime
  with starters only warns once and claims nothing, so a run of a
  starter-only name stays `Runnable` until a worker with the agent takes it.
* The last registration of a name wins, whichever kind it is, with a warning.
* `starter.init` must return the state the agent of that name decodes; the
  runtime stores it as JSON and cannot check the types agree.
  `adam_llm_agent::LlmStarter` and `adam_coder::CoderStarter` are the two in
  this workspace.

```rust
let runtime = Runtime::builder(store)   // store: adam_core::DynStore
    .starter(MyStarter)                 // MyStarter: impl AgentStarter
    .build();
let run = runtime.start("my-agent", inbound, None).await?;
// a worker process: Runtime::builder(store).agent(MyAgent)...run_worker(..)
```

## Errors

`AgentError` and `RuntimeError` implement `adam_error::Classify`; the worker
decides from the class (see [`adam-error`](../adam-error/README.md)).

| `AgentError` | Class | The run |
|---|---|---|
| `Transient`, no `retry_after` | `Transient` | retried after `RetryPolicy`'s backoff |
| `Transient` with `retry_after` | `RateLimited` | retried after `max(backoff, retry_after)`, the hint capped at `MAX_RETRY_AFTER` |
| `Permanent` | `Invalid` | `Failed` |
| `NonDeterminism` | `Corrupt` | `Failed` |
| `Store(e)` | `e.class()` | `Failed` for `Corrupt` and `Invalid`; otherwise left to its lease and logged with its class |

| `RuntimeError` | Class |
|---|---|
| `UnknownAgent` | `Invalid` |
| `NotFound` | `NotFound` |
| `Finished`, `ConversationBusy` | `Rejected` |
| `Corrupt { run, reason, source }` | `Corrupt` |
| `Contended` | `Conflict` |
| `Agent(e)`, `Store(e)` | the class of `e` |

Build an `AgentError` with `AgentError::transient(msg)`,
`transient_after(msg, wait)`, `permanent(msg)` or `non_determinism(msg)`, then
`.with_retry_after(wait)` (only a `Transient` has one) and `.with_source(err)`.
`AgentError::from_classified(context, err)` turns any classified error into a
`Transient` (retryable, keeping its `retry_after`) or a `Permanent`, with `err`
as the source. `TransientAfter` no longer exists. A message describes its own
layer only; the run's stored failure text is a boundary, so the worker flattens
`message: cause` there once. A store error that fails a run, and every store
error the worker leaves to its lease, is logged with `adam_error::report`
(the second also with its class and `alert`).

## Features and environment

No Cargo features, no environment variables at runtime.

## Tests

`tests/runtime.rs` is one behavioural suite (including
`a_starter_only_runtime_starts_and_a_full_runtime_steps`) run against `MemoryStore` always,
against PostgreSQL and against MongoDB when their variables are set. Unit
tests sit in `src/cancel.rs`, `ctx.rs`, `events.rs` and `retry.rs`, and the
class tables of `AgentError` and `RuntimeError` in `src/agent.rs`
(`class_table`, `from_classified_maps_retryable_to_transient_and_keeps_the_hint`,
`a_message_never_repeats_its_source`) and `src/runtime.rs` (`error_tests`).

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL` | enables the PostgreSQL variants |
| `ADAM_TEST_MONGODB_URI`, `ADAM_TEST_MONGODB_DB` | enable the MongoDB variants (own collections, prefix `adam_rt_`) |
| `ADAM_TEST_REQUIRE_DB` | `1`: an unset Postgres URL fails instead of skipping |

```sh
ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 \
  cargo test -p adam-runtime
```

The MongoDB variant is gated by `std::env::var` directly and skips silently
when unset, even with `ADAM_TEST_REQUIRE_DB=1`.

## See also

[`adam-core`](../adam-core/README.md),
[`adam-llm-agent`](../adam-llm-agent/README.md),
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md),
[`adam-error`](../adam-error/README.md).
