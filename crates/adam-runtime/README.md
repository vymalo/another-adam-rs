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
| `Ctx`, `Emitter` | `Ctx::step` journals a side effect's outcome; `Ctx::cancelled` / `CancelToken` observe a cancel; `Ctx::child_status(run)` reads one of the run's own children, and `Ctx::child_starter()` gives an owned `ChildStarter` that starts children of the run on the runtime stepping it (see *Child runs*) |
| `Runtime`, `RuntimeBuilder` | `Runtime::builder(store).agent(a).event_sink(s).build()`; `.starter(s)` registers a start-only agent; `start`, `start_with_id`, `start_child`, `deliver`, `cancel`, `view`, `run_worker(shutdown)`, `agent_names()` (every registered name); `.worker_id(..)`, `.claim_scope(ClaimScope)` (default `Any`; see *Pinning runs to a worker*) and the getters `worker_id()`, `claim_scope()` |
| `RunView`, `RuntimeError` | the durable read side, and errors (`#[non_exhaustive]`) |
| `Classify`, `ErrorClass` | re-exported from `adam-error` |
| `Inbound` | a message delivered to a run |
| `child_run_id`, `ChildStatus`, `ChildStarter`, `RUN_FINISHED_KIND` | child runs: the id a parent derives for the child of a call, the payload of the finished message (also what `Ctx::child_status` returns), and the message's `Inbound::kind` (`adam.run.finished`) |
| `EventSink`, `RunEvent`, `BroadcastSink`, `CollectingSink`, `NoopSink`, `Artifact` | live, best-effort events |
| `Notifier` (trait), `Signal`, `Delivery`, `LocalNotifier`, `DynNotifier` | cross-process wake-up and cancel; `RuntimeBuilder::notifier(..)`. See *Several processes* |
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
  runtime stores it as JSON and cannot check the types agree across
  processes. A mismatch starts the run, then fails it as permanent on the
  worker's first step, with an error that names the agent and points at the
  starter. `adam_llm_agent::LlmStarter` is the starter of an `LlmAgent`, and
  `adam_coder::CoderStarter` that of the coder agent.

```rust
let runtime = Runtime::builder(store)   // store: adam_core::DynStore
    .starter(MyStarter)                 // MyStarter: impl AgentStarter
    .build();
let run = runtime.start("my-agent", inbound, None).await?;
// a worker process: Runtime::builder(store).agent(MyAgent)...run_worker(..)
```

## Child runs

A run can start runs and wait for them (a subagent is one). `Runtime::start_child(parent, id, agent, input)`
is `start_with_id` that also records the parent (`RunRecord::parent_id`): it returns `true` if it created the
child and `false` if a run with that id already existed, so a step that runs again finds its child. Derive `id`
from the parent and a stable key (a tool call id) with `child_run_id(parent, key)`.

When a run that has a parent reaches `Done` or `Failed`, whether by a step, by `cancel`, or because its state
cannot be read, the runtime **delivers `adam.run.finished` to the parent after the commit**. Its `Inbound::id`
is the child's run id (so a parent deduplicates by it) and its payload is a `ChildStatus`:
`{"status": "done", "output": ..}` or `{"status": "failed", "error": ".."}`. Read one with
`ChildStatus::from_notice(&inbound)`.

The message is a hint. It is sent after the child's commit, so a crash between the two, a failed delivery or a
commit whose acknowledgement was lost drops it, and the child's worker only logs that. **A parent must
therefore wait with a timer** (`Transition::Park { wake_at: Some(..) }`) and, when it wakes without the
message, read the child with `Ctx::child_status(run)`: `Some(status)` (`is_finished()` tells if it is over),
or `None` if the run was purged. `child_status` is a live read, not journaled, and answers only for children
of the calling run (anything else is `AgentError::Permanent`). A parent that is finished or gone is not an
error for the child.

A step that runs code which cannot hold the `Ctx` (a tool of an agent) starts children with
`Ctx::child_starter()`: an owned, cloneable `ChildStarter` for the runtime that is stepping the run, whose only
possible parent is that run (`start(id, agent, input)` is `start_child` with the parent fixed). The agent has to
be registered on that runtime, as an agent or as a starter.

Cancelling a parent does not cancel its children. Take the inbox in every step that can park, including the
step that starts the child: a message already in the inbox when a step starts is not "arrived during the step",
and an agent that parks without reading it sleeps until its timer. The design, the failure interleavings and
the tests that make each happen are in [`docs/architecture.md`](../../docs/architecture.md#child-runs).
`adam-llm-agent` does all of this for a tool that returns `ToolError::AwaitRun`.

## Several processes

Processes share nothing but the store. A worker finds due runs by polling it
(`poll_interval`, 250 ms by default), and a step learns of a cancel issued by
another process when its worker next reads the run. Both are correct, and both
cost up to one poll interval.

A `Notifier` removes that latency. Configure one with
`RuntimeBuilder::notifier(..)` on the front and on the workers:

* `start` and `deliver` publish `Signal::Runnable { run, agent }`. A worker of
  any process that steps `agent` polls at once.
* `cancel` publishes `Signal::Finished { run }`. A process that is stepping the
  run fires the step's `CancelToken` at once.
* `Delivery::Resync` means signals may have been lost (a lagging subscriber, a
  dropped connection): the worker polls and re-reads every run it is stepping.

**A signal is a hint, never the truth.** It may be lost, duplicated or late.
Correctness rests on the version compare-and-swap and the lease, and polling
stays on with a notifier configured (timers and retry backoffs are found by
polling only). Without a notifier the behaviour is exactly what it was.

`LocalNotifier` connects runtimes inside one process (tests, or a process that
is front and worker in one). Across processes use an adapter:
[`adam-notify-postgres`](../adam-notify-postgres/README.md) (`PgNotifier`, over
`LISTEN`/`NOTIFY`). A new adapter is checked by
[`adam-notify-testkit`](../adam-notify-testkit/README.md)'s
`notifier_conformance!`. Live *events* of a run stepped elsewhere are a separate
port, `EventSink` (`PgEventSink` carries them); a `Notifier` carries only the two
signals above.

## Pinning runs to a worker

By default a run moves between workers at every step: a `Continue` is committed, the lease is
released, and the next claim (by any worker) takes it. That is right when every worker sees the
same files. It is wrong when a run's files live on one worker's disk, because a step that lands
elsewhere finds no files (for the coder, a second clone, a second branch and a second pull request;
see [ADR 0002](../../docs/decisions/0002-workspace-placement.md)).

`RuntimeBuilder::claim_scope(ClaimScope::Pinned)` makes the worker claim with
`adam_core::ClaimScope::Pinned`: it takes only runs without an owner or owned by its
`worker_id`, and the first claim makes it the owner. Then a run always steps on one worker.

* **Set a stable `worker_id`.** The default id is random per process, so after a restart the
  worker would not recognise its own runs. Use a name that survives restarts (a StatefulSet pod
  name).
* **A run whose owner never comes back is stranded.** Nothing adopts it and nothing reports it.
  Adoption is future work (ADR 0002).
* The scope is the runtime's choice; `adam-runtime` does not depend on `adam-host`. A host maps
  `Placement::pins_runs()` to `ClaimScope::Pinned`. `adam-coder` does it from `WORKSPACE_PLACEMENT`.
* `start`, `deliver`, `cancel` and `view` are unaffected: they never claim.

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
`a_starter_only_runtime_starts_and_a_full_runtime_steps`, the pair
`pinned_workers_step_a_run_only_on_its_owner` (three workers, each first seeded alone with one
unfinished run so all three own something, then twelve six-step runs stepped together: each run
steps on one worker only) and its control
`any_workers_let_a_run_move_between_workers` (a run seeded by one worker is finished by another), and the two
`notifier_*` cases: two runtimes over one store and one `LocalNotifier`, a 30 s poll, a 5 s deadline, and the child-run cases: `a_finished_child_wakes_its_parent_once`, `a_lost_notice_is_recovered_by_the_timer`, `a_parent_that_loses_its_lease_does_not_start_or_resume_twice` and the rest, which use gates, a `ManualClock` and `FaultyStore::fail_run` and never sleep for a fixed time) run against `MemoryStore` always,
against PostgreSQL and against MongoDB when their variables are set. Unit
tests sit in `src/cancel.rs`, `child.rs` (the id derivation is pinned by a golden value, the notice payload), `ctx.rs`, `events.rs`, `notify.rs` and `retry.rs`, and the
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
[`adam-notify-postgres`](../adam-notify-postgres/README.md),
[`adam-notify-testkit`](../adam-notify-testkit/README.md),
[`adam-error`](../adam-error/README.md).
