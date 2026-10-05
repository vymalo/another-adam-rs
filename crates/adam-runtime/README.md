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
| `Agent` (trait) | `name`, `init(Inbound) -> State`, `init_continuing(Inbound, &State, RunId) -> State` (default: `init`; see *Continuing another run*), `async step(&mut Ctx, State) -> Transition<State>` |
| `AgentStarter` (trait) | the start-only half of an agent: `name`, `init(Inbound) -> State`, `init_continuing(Inbound, &State, RunId) -> State` (default: `init`), no `step`; `State` is `Serialize + DeserializeOwned`, as the agent's is (new: a starter whose state was only `Serialize` must derive `Deserialize` too, because it reads the prior state). See *Starting without stepping* |
| `Transition` | `Continue`, `Park` (timer and/or inbound message), `Done`, `Fail` |
| `AgentError` | `Transient { retry_after, .. }` (`retry_after` is a minimum wait, e.g. `Retry-After`), `Permanent`, `NonDeterminism`, `Store`; `#[non_exhaustive]`, see *Errors* |
| `Ctx`, `Emitter` | `Ctx::step` journals a side effect's outcome; `Ctx::take_inbox` drains the delivered messages and `Ctx::peek_inbox` reads them without consuming; `Ctx::arrived()` counts the messages delivered while the transition runs, and `Ctx::reopen_on_arrival()` makes a `Done` that the transition returns go on instead (see *A message that arrives while a run finishes*); `Ctx::cancelled` / `CancelToken` observe a cancel; `Ctx::child_status(run)` reads one of the run's own children, and `Ctx::child_starter()` gives an owned `ChildStarter` that starts children of the run on the runtime stepping it (see *Child runs*) |
| `Runtime`, `RuntimeBuilder` | `Runtime::builder(store).agent(a).event_sink(s).build()`; `.starter(s)` registers a start-only agent; `start`, `start_with_id`, `start_child`, `start_continuing`, `start_with_id_continuing`, `deliver`, `cancel`, `view`, `run_worker(shutdown)`, `agent_names()` (every registered name); `.worker_id(..)`, `.claim_scope(ClaimScope)` (default `Any`; see *Pinning runs to a worker*) and the getters `worker_id()`, `claim_scope()` |
| `RunView`, `RuntimeError` | the durable read side, and errors (`#[non_exhaustive]`). `RunView::claimed` is true while a worker holds an unexpired lease on a runnable run (read with `Store::lease_until` against the runtime's clock, before the record, so a step that commits and releases between the two reads never reads as unclaimed at version 1): the first step of a run commits nothing until it ends, and this is how a reader, in any process, tells a run being stepped from one nobody has taken |
| `Classify`, `ErrorClass` | re-exported from `adam-error` |
| `Inbound` | a message delivered to a run |
| `child_run_id`, `ChildStatus`, `ChildStarter`, `RUN_FINISHED_KIND` | child runs: the id a parent derives for the child of a call, the payload of the finished message (also what `Ctx::child_status` returns), and the message's `Inbound::kind` (`adam.run.finished`) |
| `EventSink`, `RunEvent`, `BroadcastSink`, `CollectingSink`, `NoopSink`, `Artifact` | live, best-effort events: `Status` (after a commit, and `Status(Runnable, "claimed")` once, when a worker first takes a run), `Progress`, `Step`, `TextDelta`, `ReasoningDelta`, `Custom`, `Artifact` |
| `Artifact`, `ArtifactFile`, `ArtifactFileError`, `MAX_ARTIFACT_FILE_BYTES`, `MAX_ARTIFACT_FILENAME_BYTES`, `MAX_RUN_FILE_BYTES` | an output of the run, in two forms: a JSON artifact (`Artifact::new(name, mime_type, data)`) and a **file artifact** (`Artifact::file(name, media_type, filename, bytes)`, [ADR 0012](../../docs/decisions/0012-files-as-a2a-artifacts.md)), whose `file` holds the filename and the bytes and whose `data` is `null`. `Artifact` and `ArtifactFile` are `#[non_exhaustive]`; `RunEvent::Artifact` has a `file` member too. Journaled as `{"filename", "bytes": "<base64>"}`, and an artifact journaled before the file form existed has no `file` and reads as it did. `Artifact::file` refuses a file over **4 MiB** (`MAX_ARTIFACT_FILE_BYTES`), a filename that is not a name (empty, a path, control characters, over 255 bytes) and a media type that is not `type/subtype`; `Debug` prints a file's size, never its bytes. A run keeps at most `MAX_RUN_FILE_BYTES` (6 MiB) of files: the runtime keeps what it is given, and the agent loop (`adam-llm-agent`) enforces it. Journal cost: the bytes, as base64 (a third more), are in the journal entry of the step that made the file and in every commit of the run's state after it, which is why the caps are what they are |
| `StepEvent`, `StepKind`, `StepState`, `StepIcon`, `StepOutput`, `MAX_STEP_ID_BYTES`, `MAX_STEP_LABEL_CHARS`, `MAX_STEP_DETAIL_CHARS`, `STEP_INPUT_MAX_BYTES`, `STEP_INPUT_STRING_MAX_CHARS`, `STEP_OUTPUT_MAX_BYTES` | `RunEvent::Step`: a step of the run's work (a tool call, a sub-agent's work, a command) started, moved or ended, and which step it runs under; a tool call's step can carry what the tool was given (`input`) and answered (`output`), cut to the contract's bounds; see *Steps* |
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

## A message that arrives while a run finishes

A message delivered while a transition runs is not in the `Ctx` inbox of that transition: the next transition
reads it, and a `Park` that was returned meanwhile is woken by it. A `Done` is different: it ends the run, and
the message would stay unread in a run that is over. An agent that must not do that (the `steer/v1` agents of
`adam-llm-agent`, which promise an accepted message is never lost) calls `Ctx::reopen_on_arrival()` in the
transition. Then, **in the commit itself** (a compare-and-set on the record, which a delivery changes, so no
message can slip in between a check and the commit), a `Done` committed with a message that arrived meanwhile
is committed as a `Continue`: the run stays runnable with the state the `Done` carried, its output is dropped,
and the next transition reads the message before the agent can finish again. `Fail`, `Park` and `Continue` are
committed as always, and a transition that did not call it is committed as before. The agent promises that its
next `step` reads the inbox and that stepping it again after a `Done` is harmless. `Ctx::arrived()` is the live
(not journaled) count of the messages that came during the transition, for an agent that wants to answer them
in the same step instead of finishing and being reopened. Tests: `a_done_that_asked_to_reopen_goes_on_when_a_message_arrived`,
`a_done_that_asked_to_reopen_finishes_when_nothing_arrived` and `a_done_that_did_not_ask_finishes_past_a_message`,
on every store of the suite.

## Starting without stepping

Starting a run needs only a name and the initial state; stepping it needs the
whole agent, which usually holds a model, credentials and a sandbox. A process
that only accepts requests (an A2A front) registers an `AgentStarter` with
`RuntimeBuilder::starter` and holds none of those. A worker registers the
`Agent` under the same name and steps what the front started, over the same
store.

* `start`, `start_with_id` and their continuing variants work for both kinds of registration, and an
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

## Continuing another run

A new task in a conversation that already finished one should remember it. The runtime does not know
what of an agent's state is worth carrying, so the agent decides:

* `Agent::init_continuing(input, prior, prior_run)` and `AgentStarter::init_continuing(..)` return the
  first state of a run that continues run `prior_run`, whose last committed state is `prior`. The default
  ignores both and returns `init(input)`, so an agent that does not override it is unchanged. **A wrapper
  that delegates `init` to another agent must delegate `init_continuing` too**, or the continuation stops
  at the wrapper.
* `Runtime::start_with_id_continuing(run, agent, input, conversation, prior)` is `start_with_id` for such
  a run (idempotent: `true` if created, `false` if `run` already existed, which it finds out **first**, before
  the prior is read or `init_continuing` runs, so a repeat works when the prior is gone), and
  `Runtime::start_continuing(agent, input, conversation, prior)` is `start` for one (it delivers to the
  conversation's open run if there is one, and then does not read `prior`). The prior state is read with
  `Store::load_run` (so every store works, and no store method was added) and decoded as the registered
  `State`, which is why a front process that registered only the `AgentStarter` can do it.
* **Checked:** `prior` exists (`RuntimeError::NotFound`) and is a run of `agent` (`RuntimeError::WrongAgent`,
  class `Invalid`, so its state is this agent's; its text names the run, and `adam-a2a-runtime` never shows it
  to a client). **Not checked:** whether the caller may continue it (same owner, same conversation):
  the runtime has no owners, and `adam-a2a-runtime` checks that before it calls. Its status does not
  matter; an agent that continues an unfinished run copes with a half-done turn.
* A `prior` state that does not decode as the agent's state is not an error: the run starts as `init`
  says, and a warning says so (the decoder's kind of error, never its text, which can quote the
  conversation).

The A2A side of this (`referenceTaskIds`) is in
[`adam-a2a-runtime`](../adam-a2a-runtime/README.md), and the decision in
[ADR 0003](../../docs/decisions/0003-a-new-task-continues-the-task-it-references.md).

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

## Steps

`RunEvent::Step(StepEvent)` says that something the run does (a tool call, a sub-agent's work, a command it ran)
started, moved or ended, and **under which step it runs**. It is the vocabulary of the orchestration layer's `steps/v1`
extension (`docs/api/steps-v1.md` of `vymalo/another-agentic-system`), which `adam-a2a-runtime` serves to a client
that asked for it ([ADR 0007](../../docs/decisions/0007-progress-as-steps-and-streamed-text.md)):

```rust
use adam_runtime::{RunEvent, StepEvent, StepIcon, StepKind, StepState};

ctx.emit(RunEvent::Step(
    StepEvent::new("acp:c2:1", StepKind::Command, "npm test", StepState::Failed)
        .under("tool:c2")           // a step reported earlier; none = at the top
        .with_icon(StepIcon::Execute)
        .with_detail("1 failed"),
))
.await;
```

The first report of an `id` starts the step, later ones update it, and a state that ends it (`completed`, `failed`,
`canceled`: `StepState::is_end`) ends it; a report after the end starts it again (a retry). The constructors keep the
contract's bounds: an id of at most 128 bytes (control characters become `_`), a one-line label of at most 200
characters, a detail of at most 1000 (`…` marks a cut). Kinds, states and icons are closed enums
(`#[non_exhaustive]`); `as_str()` is the word on the wire and `parse(..)` reads it. A step is best effort and not
durable, like every event; `adam-llm-agent` reports every tool call as one.

A tool call's step can also say **what the call was given and what it answered** ([ADR 0011](../../docs/decisions/0011-a-tool-calls-step-carries-its-input-and-output.md)):
`with_input(map)` on the report that starts it (the arguments, a JSON object) and `with_output(StepOutput::new(text, error))`
on the one that ends it. Both are cut to the contract's bounds by the constructors, never raised above them:

| | Bound |
|---|---|
| `input` | control characters other than `\n` and `\t` dropped; a string over `STEP_INPUT_STRING_MAX_CHARS` (512) characters cut, ending in `…`; an input still over `STEP_INPUT_MAX_BYTES` (4096) serialized replaced by `{"_cut": true, "bytes": <its size>}` |
| `output.text` | control characters dropped; over `STEP_OUTPUT_MAX_BYTES` (8192) bytes it keeps its head (three quarters) and its tail around a line `… n bytes not kept …`, the whole within 8192, with `truncated: true` and `bytes` the size of the whole; `error: true` when the call failed and `text` is its error |

`truncated`, `bytes` and `error` are absent from the JSON when they are not so. **Redact before you build one**: a cut
can leave half of a secret that a redactor would no longer recognise (`adam-llm-agent`'s `StepIo` does it in that order).
The orchestration layer keeps a budget per job on top; the agent keeps none.

## Streamed text

`RunEvent::TextDelta { stream, offset, text, last, abandoned }` is a piece of the text the model is writing, sent as it
arrives, so a client can show the words growing ([ADR 0007](../../docs/decisions/0007-progress-as-steps-and-streamed-text.md);
the contract is the orchestration layer's `text-stream/v1`, `docs/api/text-stream-v1.md` of
`vymalo/another-agentic-system`, which `adam-a2a-runtime` serves to a client that asked for it). The pieces of one
`stream` (an id of at most `MAX_STREAM_ID_BYTES` = 128 bytes, unique within the run) follow each other: `offset` is where
a piece begins in the whole text, **in UTF-8 bytes**; the last piece says `last` (its text may be empty), and
`abandoned` with it says the model failed, so the text so far is all there is. A piece holds whole characters and at most
`MAX_TEXT_DELTA_BYTES` = 1024 bytes (`floor_boundary` cuts text there), so that the event fits a `NOTIFY` payload between
processes. Live and meant to be lost, like every event: the whole text is what the run records and says
(`AGENT_TEXT_KIND`, the `agent_text` event of `adam-llm-agent`, names the stream of words that came before a tool call).

### Reasoning

`RunEvent::ReasoningDelta { stream, offset, text, last, abandoned }` has the shape and the bounds of a `TextDelta` and is the
reasoning a model in thinking mode writes **before** its answer, in a stream of its own (one per model turn that reasoned,
ended before the turn's words begin). It is **not the answer**: it is in no run output, no `turn_output` and no step output,
and it is not durable (no whole text follows, nothing records it: a client that wants it keeps the pieces).
`adam-a2a-runtime` serves it as `text-stream/v1` chunks marked `kind: "reasoning"`
([ADR 0020](../../docs/decisions/0020-reasoning-is-streamed-beside-the-answer-and-never-stored.md)). A new variant of `RunEvent`: an
exhaustive `match` needs an arm, and a process that predates it ignores the event.

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

## Runs the worker is stepping

The worker lists the runs it is stepping in every `claim_due` (`busy`, see
[`adam-core`](../adam-core/README.md#runs-the-caller-is-stepping)), so the store never gives it a run
it already holds, even when the lease on that run has lapsed under a slow step. It releases a run's
lease at the end of a step before it counts the run as free: a release matches the worker and not
the claim, and one that landed after a newer claim would clear that claim's lease. Another worker
may take a run over once its lease expired; the version CAS rejects whichever commit comes second,
as before.

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
| `UnknownAgent`, `WrongAgent { run, agent }` | `Invalid` |
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
`a_starter_only_runtime_starts_and_a_full_runtime_steps`, the continuation cases
(`a_run_continues_a_finished_run_from_a_start_only_front`,
`continuing_checks_the_prior_run_and_creates_nothing_when_it_refuses`,
`a_prior_state_that_does_not_decode_starts_the_run_fresh`,
`an_agent_without_an_override_starts_fresh_when_asked_to_continue`,
`start_continuing_delivers_to_an_open_run_and_otherwise_continues`; a repeat of a started run answers `false`
even when its prior is wrong or gone), the pair
`pinned_workers_step_a_run_only_on_its_owner` (three workers, each first seeded alone with one
unfinished run so all three own something, then twelve six-step runs stepped together: each run
steps on one worker only) and its control
`any_workers_let_a_run_move_between_workers` (a run seeded by one worker is finished by another),
`a_claimed_run_says_so_before_its_first_commit` (`RunView::claimed` is false before a worker takes a run, true while its first step is in the air with nothing committed, false once done, and the first claim is announced once, right after the start, not the claim of the second step) and `a_lease_that_ran_out_is_not_a_claim` (a `ManualClock` moves past the lease under a gated step: the view reads unclaimed), `a_step_that_outlives_its_lease_is_not_claimed_again_by_its_worker` (a lease lapses under a gated step and
whole claim passes follow: `FaultyStore::claimed` says the store handed the run out once, and the step ran once), and the two
`notifier_*` cases: two runtimes over one store and one `LocalNotifier`, a 30 s poll, a 5 s deadline, and the child-run cases: `a_finished_child_wakes_its_parent_once`, `a_lost_notice_is_recovered_by_the_timer`, `a_parent_that_loses_its_lease_does_not_start_or_resume_twice` and the rest, which use gates, a `ManualClock` and `FaultyStore::fail_run` and never sleep for a fixed time) run against `MemoryStore` always,
against PostgreSQL and against MongoDB when their variables are set. Unit
tests sit in `src/cancel.rs`, `erased.rs` (an override reaches an agent and a starter, the default is `init`, a prior state that does not decode falls back to `init`), `child.rs` (the id derivation is pinned by a golden value, the notice payload), `ctx.rs`, `events.rs`, `step.rs` (the words of the kinds, states and icons are the contract's, a step is made within its bounds on a character boundary, the serde shape leaves out what a step does not say), `notify.rs` and `retry.rs`, and the
class tables of `AgentError` and `RuntimeError` in `src/agent.rs`
(`class_table`, `from_classified_maps_retryable_to_transient_and_keeps_the_hint`,
`a_message_never_repeats_its_source`, `the_default_continuation_is_init_and_ignores_the_prior_state`) and `src/runtime.rs` (`error_tests`).

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
