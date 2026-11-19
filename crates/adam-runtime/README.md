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
| `Agent` (trait) | `name`, `init(Inbound) -> State`, `init_continuing(Inbound, &State, RunId) -> State` (default: `init`; see *Continuing another run*), `async step(&mut Ctx, State) -> Transition<State>`, `async on_cancelled(RunId, Value)` (provided, does nothing; see *Cancelling a run*) |
| `AgentStarter` (trait) | the start-only half of an agent: `name`, `init(Inbound) -> State`, `init_continuing(Inbound, &State, RunId) -> State` (default: `init`), no `step`; `State` is `Serialize + DeserializeOwned`, as the agent's is (new: a starter whose state was only `Serialize` must derive `Deserialize` too, because it reads the prior state). See *Starting without stepping* |
| `Transition` | `Continue`, `Park` (timer and/or inbound message), `Done`, `Fail` |
| `AgentError` | `Transient { retry_after, .. }` (`retry_after` is a minimum wait, e.g. `Retry-After`), `Permanent`, `NonDeterminism`, `Store`; `#[non_exhaustive]`, see *Errors* |
| `Ctx`, `Emitter`, `ReservedStep` | `Ctx::step` journals a side effect's outcome; `Ctx::reserve_steps(&names)` takes the next seqs in order for steps that run at the same time (each `ReservedStep` has `seq()`, `name()`, `is_recorded()` and `run(f)`, with `step`'s contract), and `Ctx::step_all(steps, at_once)` runs them at most `at_once` at a time and returns the outcomes in the order given (see *Steps that run at the same time*); `Ctx::take_inbox` drains the delivered messages and `Ctx::peek_inbox` reads them without consuming; `Ctx::arrived()` counts the messages delivered while the transition runs, `Ctx::reopen_on_arrival()` makes a `Done` that the transition returns go on instead, and `Ctx::read_already(&ids)` names the messages the run has read, whose copies are not counted (see *A message that arrives while a run finishes*); `Ctx::cancelled` / `CancelToken` observe a cancel; `Ctx::child_status(run)` reads one of the run's own children and `Ctx::child_state(run)` its agent's own state (both live reads of the run's own children only), and `Ctx::child_starter()` gives an owned `ChildStarter` that starts children of the run on the runtime stepping it (see *Child runs*) |
| `Runtime`, `RuntimeBuilder` | `Runtime::builder(store).agent(a).event_sink(s).build()`; `.starter(s)` registers a start-only agent; `start`, `start_with_id`, `start_child`, `start_child_continuing`, `list_sessions`, `start_continuing`, `start_with_id_continuing`, `deliver`, `cancel`, `view`, `run_worker(shutdown)`, `agent_names()` (every registered name); `.worker_id(..)`, `.claim_scope(ClaimScope)` (default `Any`; see *Pinning runs to a worker*) and the getters `worker_id()`, `claim_scope()` |
| `RunView`, `RuntimeError` | the durable read side, and errors (`#[non_exhaustive]`). `RunView::claimed` is true while a worker holds an unexpired lease on a runnable run (read with `Store::lease_until` against the runtime's clock, before the record, so a step that commits and releases between the two reads never reads as unclaimed at version 1): the first step of a run commits nothing until it ends, and this is how a reader, in any process, tells a run being stepped from one nobody has taken |
| `Classify`, `ErrorClass` | re-exported from `adam-error` |
| `Inbound` | a message delivered to a run |
| `UsageEvent`, `UsageTotals`, `UsageTotal`, `Usage`, `MAX_USAGE_CALL_BYTES`, `MAX_USAGE_LABEL_BYTES`, `MAX_USAGE_TOTALS` | `RunEvent::Usage`: one model call completed, its tokens (`Usage`, re-exported from `adam-model`), its model, provider and context window, and the step of the task it ran under; and a task's totals, one entry per provider and model; see *Usage*. `Emitter::emit_for(run, event)` sends an event as another run's (a child's call is its root's) |
| `SandboxUsage`, `SandboxEntry`, `SandboxEnvironment`, `MAX_SANDBOX_ENTRIES` | the time a run's commands ran in a sandbox (a run pod), one `SandboxEntry` per shape (`cpu_millis`, `memory_bytes`, `commands`, `milliseconds`), at most 8, and the environment they ran in; what a tool reports (`ToolCtx::report_sandbox` in `adam-llm-agent`), what the agent keeps in its state next to the totals, and what the A2A server writes as `sandbox`; see *Usage* |
| `child_run_id`, `ChildStatus`, `ChildStarter`, `RUN_FINISHED_KIND`, `CHILD_REPORTED_KIND` | child runs: the id a parent derives for the child of a call, the payload of the finished message (also what `Ctx::child_status` returns), and the message's `Inbound::kind` (`adam.run.finished`, or `adam.child.reported` for a session round). `ChildStarter` also reads and acts on **the sessions of the run's conversation** (slice 2, below): `session_conversation()`, `start_round`, `session`, `sessions`, `steer`, `cancel_round`, `round_state`, `round_record` (the same with the round's status); `session_idle_ttl()` and `start_in_parent_conversation(id, agent, input)` (a timer run of a session, parked until it may expire, in a conversation made from the session's parent one and unique to the timer, which a run in it still reads as its parent's: `Runtime::start_child_in`); `SessionView::cleanup_rounds` and `countable_rounds()` (the rounds a limit counts: the closing rounds of a session that stays open only cancel what the round before left, and are not counted) |
| `session_conversation_id`, `session_conversation_prefix`, `parse_session_conversation_id`, `SessionState`, `RoundOutcome`, `SessionSummary`, `ReportRound`, `SessionList`, `SessionView`, `SessionHeader`, `SESSION_STATE_KEY`, `SESSION_CLOSE_KIND`, `SESSION_CLOSE_ROUND_KEY`, `closing_round_payload`, `SessionSummary::started_outside_a_run`, `SessionView::started_outside_a_run`, `Runtime::session_view` | child sessions (ADR 0045): the conversation id the rounds of a session share, what `list_sessions` and `session_view` return, the header a round's agent keeps in its state under `session`, the message that asks a working round to be the session's last, and the first message of a round that only closes its session (`closing_round_payload(&header)`, `{close_session: true, session: header}`, which `subagent_close` and the A2A front both build). `started_outside_a_run` is true for a round that is **its own parent** (`start_child_continuing(id, id, ..)`: a person started it, no run of an agent did): no run's end cancels it and none waits for it, the A2A front's `by: person` (`adam-a2a-runtime`, *Child sessions*). The two methods are additions; no trait changes |
| `EventSink`, `RunEvent`, `BroadcastSink`, `CollectingSink`, `NoopSink`, `Artifact` | live, best-effort events: `Status` (after a commit, and `Status(Runnable, "claimed")` once, when a worker first takes a run), `Progress`, `Step`, `TextDelta`, `ReasoningDelta`, `Usage`, `Custom`, `Artifact`. `BroadcastSink::subscribe_run(run)` starts with the run's recent events: see *A late subscriber* |
| `REPLAY_EVENTS_PER_RUN`, `REPLAY_MAX_AGE`, `REPLAY_MAX_RUNS`, `REPLAY_MAX_BYTES` | the bounds of `BroadcastSink`'s replay: 64 events per run, none older than 30 s, 64 runs, and 16 MiB in all (an estimate of the heap the events hold; past it the globally oldest events go first) |
| `Artifact`, `ArtifactFile`, `ArtifactFileError`, `MAX_ARTIFACT_FILE_BYTES`, `MAX_ARTIFACT_FILENAME_BYTES`, `MAX_RUN_FILE_BYTES` | an output of the run, in two forms: a JSON artifact (`Artifact::new(name, mime_type, data)`) and a **file artifact** (`Artifact::file(name, media_type, filename, bytes)`, [ADR 0012](../../docs/decisions/0012-files-as-a2a-artifacts.md)), whose `file` holds the filename and the bytes and whose `data` is `null`. `Artifact` and `ArtifactFile` are `#[non_exhaustive]`; `Artifact::file_sha256()` is the SHA-256 of a file's bytes as 64 lower-case hex digits (`None` for a JSON artifact): the name the person's screen keeps the file under, which an `ask_user` option names as its picture. `RunEvent::Artifact` has a `file` member too. **An artifact may have an id** (`Artifact::id`, `Artifact::with_id`, `RunEvent::Artifact::id`; absent from artifacts journaled before, `None` by default): a thing that changes while the run works (a plan) is **one** artifact. At commit (`worker.rs`, `add_artifact`) an artifact with an id the run already holds **replaces** it in place, keeping its position; one with no id, or an id no artifact has, is added after the others. No `Store` change: the list is part of the envelope the worker already writes. **A file artifact has no id and is never replaced nor replaces anything** (`with_id` leaves a file without one, and the commit drops an id on a file and keeps both). The id is the artifact's identity within the run: a later artifact of the run with the same id replaces it, **whichever tool made it**, so ids belong to what a tool replaces on purpose (a surface, data). **Breaking:** `Artifact` gains a public field `id` and `RunEvent::Artifact` a member `id`, so a constructor of either outside this crate (`RunEvent::Artifact { name, .. }`) must name it (or use `Artifact::new` and `From<Artifact>`), and an exhaustive pattern on the variant must add `..`; the serde forms are additive. Journaled as `{"filename", "bytes": "<base64>"}`, and an artifact journaled before the file form existed has no `file` and reads as it did. `Artifact::file` refuses a file over **4 MiB** (`MAX_ARTIFACT_FILE_BYTES`), a filename that is not a name (empty, a path, a `:`, control characters, over 255 bytes) and a media type that is not `type/subtype`; `Debug` prints a file's size, never its bytes. A run keeps at most `MAX_RUN_FILE_BYTES` (6 MiB) of files: the runtime keeps what it is given, and the agent loop (`adam-llm-agent`) enforces it. Journal cost: the bytes, as base64 (a third more), are in the journal entry of the step that made the file and in every commit of the run's state after it, which is why the caps are what they are. The journal entry is written with the tool's whole result, **before** the loop applies the run's cap, so a tool that returns several files it did not choose bounds its result by what the run may still share (`ReceivedFiles`, `ToolCtx::files_left`): one entry then stays within the run's 6 MiB (about 8 MiB of base64), under MongoDB's 16 MiB document |
| `SystemLine`, `is_system_line(line)`, `safe_file_name(name)` | the lines the system writes into a person's message about a picture, a file or a link (`[image not shown: ...]`, `[attached: ...]`, `[linked: ...]`, `src/lines.rs`): one place builds them and one recognises them, so a reader of the person's words (the coder's rule about the repositories a task named) skips them whatever a sender put in a name or a link. A name a line prints is a file name and nothing else: letters, digits, `.`, `_` and `-`, every other character `_`, at most 64 |
| `image_size(bytes)` | the width and height in pixels of a PNG, JPEG, GIF or WebP, **read from its header** (`src/file.rs`): hand-written, every index checked, a JPEG walked for at most 256 segments, no allocation, nothing decoded; `None` for anything else, a header cut short, a size of zero or an SVG. What a header says is a number, never a size to allocate. A property test runs it on arbitrary bytes and on cut real headers. `human_size(len)` is the `1.4 MiB` of a line |
| `checked_media_type(claimed, bytes)`, `sniff_image(bytes)`, `extension_of(media_type)`, `Artifact::shared_line()` (an image's line also names its SHA-256 for an `ask_user` option and a `show` `Image` block) | the rule every source of a shared file follows (`src/file.rs`): the media type claimed for the bytes (by a sender, or by a file's extension) stands unless the bytes disagree: a claimed PNG, JPEG, GIF, WebP or SVG must be that image by its bytes (`sniff_image`), bytes that are an image claimed as something else disagree, and both are `application/octet-stream`, as is a claim that is not `type/subtype` for bytes that are no image; `extension_of` names the usual extension (`bin` when unknown); `shared_line` is what a tool tells the model instead of the bytes, in one line, `Shared report.pdf (10 bytes, application/pdf).`, and for an image also how to show it inline by its name (what the person's screen resolves an image's source against, never a workspace path): `Shared chart.png (1.5 KiB, image/png). To show it in your answer, write ![description](chart.png).` (a name with a space, a parenthesis, `<`, `>` or `\` goes between `<` and `>`, CommonMark's form for such a destination). Used by the coder's `share_file`, `adam-mcp` and `adam-assembly` (`files: true`) |
| `ReceivedFiles`, `MAX_FILES_PER_RESULT` | the files of one tool result that another system sent (an MCP server's images and blobs, a remote agent's `raw` parts), shared as file artifacts as they are met: `new(stem, budget)`, `admit(claimed, len)` (every cap, on the length alone: call it before reading or copying the bytes), `share(claimed, filename, name, bytes)` (the line for the model), `refuse(claimed, why)`, `into_parts()` (the artifacts, and whether one was refused: then the result is an error result). What a sender says is untrusted: a file is named `<base>-<hash>.<ext>` (the sender's file name without its extension and cut to `[A-Za-z0-9._-]`, else the stem; the first 8 hexadecimal digits of the SHA-256 of the bytes, so names are unique within a run and stable on replay; the extension of the checked type), its type is the sender's, checked, its artifact name one line of at most 120 characters, and a refusal repeats the sender's type only when it is one. At most 4 MiB a file, 16 a result and `budget` in all: what the run may still share (`ToolCtx::files_left`), because the result is journaled before the loop's run cap applies ([ADR 0033](../../docs/decisions/0033-files-from-mcp-results-are-shared-files.md)) |
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
docs (`src/lib.rs`) and the [architecture](../../docs/architecture.md#the-mental-model).

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
in the same step instead of finishing and being reopened. Neither counts a child's `adam.run.finished` notice
(*Child runs*): a run that finishes has settled its waits, and the same notice a moment later is refused, so a
notice that races a parent's timer does not cost the parent another turn (a parked run is still woken by it).
Nor does either count **a copy of a message the run has read**: a sender whose delivery was not acknowledged
delivers the same message again under the same id, and an agent that tells the transition which ids it has read
(`Ctx::read_already(&ids)`, which `adam-llm-agent` does with the ids its state remembers) is not given a turn
with nothing new to answer. The copy stays in the inbox, where the next transition drops it.
Tests: `a_done_that_asked_to_reopen_goes_on_when_a_message_arrived`,
`a_done_that_asked_to_reopen_finishes_when_nothing_arrived`, `a_done_that_did_not_ask_finishes_past_a_message`,
`a_childs_notice_that_lands_while_a_run_finishes_does_not_reopen_it` and
`a_message_beside_a_childs_notice_still_reopens_the_run`, on every store of the suite.

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

**Cancelling a run cancels the runs that work for it**: a child whose parent is `Failed` (any reason) is failed with
`cancelled: the parent run ended` by its worker, before it steps (one `load_run(parent)`) and on the poll of the cancel
watch of a step in flight (the parent is read at most once a second). A parked child that works for a parent always carries
a wake no later than a minute away; at it the runtime reads the parent first and, when nothing else is due (no message, no
wake of the agent's own), parks the run again **without stepping the agent**, journaling nothing and calling no model, so
a child waiting for a person is reached too, and its own children after it. A `Done` parent changes nothing. A parent that
cannot be loaded (retention purged it, the store is down) is **unknown and cancels nothing**: keep retention longer than a
minute (`Store::purge_finished`). No `Store` method is involved. The runtime cancels no remote A2A task, but it calls
`Agent::on_cancelled(run, state)` so the agent can: after a cancel commit succeeds (`Runtime::cancel`, a parent that ended,
a time limit, the A2A front's cancel) it spawns the call with the agent state that commit holds, **once per commit,
best effort** (not retried, not durable, and the process may end first), and only in a process that registered the full
agent: a start-only registration has nothing to call, so a front that cancels sends nothing. A run that is already
terminal gets no call. The provided method is new and does nothing, so no implementer breaks.
`Runtime::start_child_within` starts a child with a time limit: the deadline is stored once, from the runtime's `Clock`,
`wake_at` is clamped to it, and a child past it fails with `cancelled: time limit: the run did not finish within N min`.
**The time a child spends waiting for a person is not counted** (it parked with no timer: the deadline is cleared and what is
left is kept, and the next step that follows a message sets it again); time on a timer or on a child still counts. A wake
that comes before the deadline (clocks that disagree) parks the run again to the deadline plus a second. The first
`RunView::wake_at` of such a run is the agent's own, not the runtime's. `start_child_continuing_within` starts a session
round with a limit (the session tools of `adam-llm-agent` call it). `CancelToken::child()` makes a token that fires with
its parent.

**Child sessions, slice 2 (ADR 0045).** A session is read from its rounds and nothing else:
`Runtime::session_view(conversation, session)` answers `None` for an id that is not the first round of a session of that
conversation, and otherwise the latest round (`SessionView`: its status and output as a parent reads a child, its number,
`SessionState`, whether the session takes more messages, which run started it). Whether a session is **closed** is read from
the latest round: its `SessionHeader` (`session` in the round's agent state: id, agent, description, `keep_open`, `closed`,
`closing`), a `SESSION_CLOSE_KIND` request waiting in its inbox, or a round that has ended in a session that is not kept open
(auto-close). A session whose rounds carry no header is kept open and never closes. `ChildStarter`'s session methods act on
the conversation of the run they were made for, so a later run of the conversation reaches a session an earlier one started
and no run reaches another conversation's. A close request is not counted as unread input (`Ctx::arrived`), so it costs no
model turn; one that lands while a round finishes stays in its inbox, where `read_latest` finds it.
**Breaking:** `SessionState` has a third variant, `Closed`, and `SessionSummary` five fields (`description`, `keep_open`,
`closed`, `closing`, `started_by`); `list_sessions` orders working, then idle, then closed. `closing` says that the latest
round only closes the session (it calls no model and reports nothing new), so a caller that waits for working sessions can
leave it alone. **Breaking (ADR 0045 slice 3):** `SessionSummary` has a sixth field, `report` (`ReportRound`: the run, whether it ended or was cancelled, who started it
and when it was last updated), the round the session's report is of: the latest that worked, so a closing round that follows does not hide it.
A struct literal of `SessionSummary` stops compiling.

**Breaking:** cancelling a parent cancels its children. A subagent run stops after 30 minutes unless its file says
otherwise (`limits.max_minutes`). Take the inbox in every step that can park, including the
step that starts the child: a message already in the inbox when a step starts is not "arrived during the step",
and an agent that parks without reading it sleeps until its timer. The design, the failure interleavings and
the tests that make each happen are in [`docs/reference/child-runs.md`](../../docs/reference/child-runs.md#failure-interleavings).

### Child sessions (ADR 0045, slices 1 and 2)

A *session* is a chain of child runs ("rounds") of **one agent** that share a conversation id. Nothing is
stored for it: the session id is round 1's run id, and the conversation id is
`session_conversation_id(parent_conversation, session)`.

* **The conversation id.** The parent's conversation id with `%` written `%25` and `/` written `%2F`, then
  `/s/` and the session id. A client chooses its A2A context id freely, so without the escape a parent
  whose id holds `/s/` would be indistinguishable from the session of another; with it the first `/` is
  the separator, and `parse_session_conversation_id` is the exact inverse (a property test holds it). The
  cost: a parent id that holds `%` or `/` is not a literal prefix of its sessions' ids (an owner prefix such
  as `subject:` still is, unless the subject holds those characters). The id is at most
  `MAX_SESSION_CONVERSATION_BYTES` (1024) long: it is an indexed column (PostgreSQL's b-tree entries are
  limited to about a third of a page, MongoDB indexes the string and a unique `open_key` that embeds it), so
  the bound is a margin below both (*unverified* against their current documentation).
* **`Runtime::start_child_continuing(parent, id, agent, input, conversation_id, prior)`** is `start_child`
  for a round. Round 1 passes `prior: None` and the session id as `id`; round n passes round n-1's run and
  starts from its state, as a task continues a finished one. It checks what it can see, and answers
  `RuntimeError::InvalidSession` (class `Invalid`, naming what did not match) for: a conversation id that is
  not a session's or is too long; round 1 without the session id, or a later round with it; an existing run
  with `id` that is not this round (another agent, conversation or, for round 1, parent); a `prior` of
  another conversation, or of a session whose round 1 is missing or has no parent (a root conversation that
  merely spells a session's id is no session). A `prior` of another agent is `WrongAgent`.
  A repeat of the same round answers `false`, before `prior` is read.
* **One open round, one agent.** The store allows one open run per agent and conversation, and a session
  has one agent (round 1's id is fixed and every `prior` must be the agent's own), so one round is open at a
  time. A start while one is open, with that round as `prior`, is `ConversationBusy`. Its class,
  `Rejected`, is shared with `Finished`, so a caller that must tell "steer the open round" from "that run is
  over" asks `RuntimeError::is_busy()`, the one place a caller decides from the variant. It steers with
  `deliver`, and the round reads that as it reads any steered message.
* **The notice.** A child with a conversation reports its end as **`adam.child.reported`**
  (`CHILD_REPORTED_KIND`, read with `ChildStatus::from_report`), not `adam.run.finished`. Id and payload are
  the same (the round's run id; a `ChildStatus`) and so is its standing: a hint that wakes the parent, which
  reads the round with `Ctx::child_status`. A child with no conversation keeps `adam.run.finished`
  unchanged. **A report is a wake-up, not input**: `LlmAgent` drops it (it is not the person's
  message) and the session tools read the round from the store (slice 2), and `Ctx::arrived` and the
  reopen-on-arrival rule do not count it, as they do not count a finished notice. Slice 3 makes a report reopen a
  turn on purpose (ADR 0045 decision 4).
* **`Runtime::list_sessions(parent_conversation, agents, offset, limit)`** lists the parent's sessions from
  the runs alone (`list_runs` with the prefix `escaped parent + "/s/"`; no new `Store` method): the latest
  round of each, working first, then most recently updated, then by id. `limit` is capped at
  `MAX_SESSION_PAGE` (100), `agents` names the agents to look at (a run listing is per agent; a name given
  twice counts once). A row needs round 1 (the run whose id is the session id) with a parent, so a session
  whose round 1 was purged is not listed. Each row has `state` (`Working` while its latest round is open,
  else `Idle`) and `outcome` (`Done`, `Failed` or `Cancelled`, `None` while working). `SessionState` is a
  closed enum on purpose (Rule 3): whether a session is closed is written in its header (slice 2), so
  `Closed` arrives then as a deliberate breaking addition. The round number counts the rounds still in the
  store.
* **Races and limits.** `list_runs` pages by `updated_at`, so a round that commits between two page reads
  can move past the cursor and be missed. The listing counts again after the scan and repeats it when the
  counts differ, up to three times, then fails with `RuntimeError::Contended`; it never returns a page it
  knows is short. More than `MAX_SESSION_ROUNDS_SCANNED` (5000) rounds of one agent under one parent is
  `RuntimeError::TooManyRounds` (class `Rejected`).
* **Purged rounds.** A round whose own record was purged and is asked for again is not recognised as a
  repeat (nothing is left to compare), as for `start_with_id_continuing`; keep `purge_finished` retention
  above any retry window and above `idle_ttl` (ADR 0045).

## A late subscriber

A streaming A2A send submits a run and subscribes to it afterwards, and a worker may take the run in between: one fast
model reply and the first tool call are over before anyone listens. Live events do not go in the durable record, so
`BroadcastSink` keeps a small replay of each run's recent ones, and `subscribe_run(run)` delivers it first and then the
live events, with no gap and no duplicate (`emit` records and sends, `subscribe_run` copies and subscribes, each under one
`std::sync::Mutex` that is never held across an `.await`).

* **Bounds** (`REPLAY_EVENTS_PER_RUN`, `REPLAY_MAX_AGE`, `REPLAY_MAX_RUNS`, `REPLAY_MAX_BYTES`): the newest 64 events
  of a run, none older than 30 seconds (pruned on `emit` and on `subscribe_run`), at most 64 runs (the run whose newest
  event is oldest goes first), and at most 16 MiB across every run, an estimate of the heap the events hold (past it the
  globally oldest events go first, so every run keeps its newest ones). Memory stays bounded by these four, whatever the
  runs do.
* **Not replayed:** `Status` (a prompt to read the durable record, which is the truth: a stale one is noise; any status
  also drops the run's buffer, so the replay is what happened since the run last changed state: a run that parked after a
  question and is taken up again replays only its new turn) and `Artifact` (durable in `RunView::artifacts`, and a file can be megabytes).
  `BroadcastSink::subscribe()`, the feed of every run, stays "from now on".
* **It is not the truth.** A subscriber still reads the durable run for state; the replay only keeps a step's start (the
  one report with its `input`), a text piece or a progress line from being lost to a race. A process that has no sink
  to a run's events (another process without `adam-notify-postgres`) replays nothing; with it, the events of other
  processes arrive through `BroadcastSink::emit` too and are replayed like local ones.
* **A repeat is possible, so a consumer must tolerate it:** subscribing to a run again within 30 seconds delivers its recent
  events again. A step is a snapshot of its `id`'s state, and a text piece carries its `offset`; both are safe to see twice
  (see `adam-a2a-runtime`, *Steps* and *Streamed text*).

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
(`#[non_exhaustive]`; the icons are `agent`, `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, `web`, `git`, `test`, `file`, `tool` and `opencode`, OpenCode's own, [ADR 0021](../../docs/decisions/0021-the-coder-is-adam-a-general-agent-that-can-code.md): a consumer that does not know a word drops the icon); `as_str()` is the word on the wire and `parse(..)` reads it. A step is best effort and not
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

## Usage

`RunEvent::Usage(UsageEvent)` says that one model call completed and what it cost: a `call` id unique within the task
(the same each time the call is reported, another one for a call made again), the `model` (the alias the request named),
the `provider` and the `context_window` when the model client says them, the tokens (`adam_model::Usage`: AG-UI's
`TokenUsage` accounting, parts never additions) and the `step` of the task it ran under (a subagent's). It is the
vocabulary of the orchestration layer's `usage/v1` extension, which `adam-a2a-runtime` serves to a client that asked for it
([ADR 0032](../../docs/decisions/0032-usage-per-model-call.md)). The constructor keeps the contract's bounds: a call id,
a step and labels of at most 128 bytes, cut on a character boundary.

**A child run's call is the task's work, so it is an event of the root run**: `Emitter::emit_for(root, event)` sends it
to the root's subscribers (the task's), and `under(step)` names the root's step it ran under; nothing is sent to the
child's own run. `UsageTotals` is what an agent keeps in its state to say the task's totals: one `UsageTotal` per provider
and model (at most `MAX_USAGE_TOTALS` = 32; a 33rd is not counted and is logged), `add`, `add_event` and `merge` (a
child's totals when it answers; `Ctx::child_state(run)` reads them). Live and best effort like every event; the totals
are the record. A new variant of `RunEvent`: an exhaustive `match` needs an arm, and a process that predates it
ignores the event. **This crate now depends on `adam-model`** for the data type of the tokens (no client).

`SandboxUsage` is the other cost of a run, the time its commands ran in a sandbox
([ADR 0043](../../docs/decisions/0043-sandbox-time-as-usage.md)): `SandboxUsage::in_environment(env)` says the run asked for an
environment, `SandboxUsage::ran_command(cpu_millis, memory_bytes, milliseconds)` that one command ran in a metered one (a run
pod; a request the pod does not make is `None`, not zero), and `merge` adds one to another (a call's report to the run's, a child's
to its parent's): one entry per shape, at most `MAX_SANDBOX_ENTRIES` = 8 (a 9th is not counted and is logged), saturating adds, and the
environment that measures wins. `SandboxEnvironment` is `Local`, `Devcontainer` or `Kubernetes`; only the last is measured
(`is_measured`), and an unmeasured one never has an entry, which reads as *not measured* and not as zero. Serialized as
`{"environment": ..., "usage": [...]}`, each member left out while empty, so a state from before reads.

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

A claim may hand back the run as it reads after the lease was taken (the MongoDB store sets the
lease, then reads the run), and a cancel does not touch the lease: a cancel that commits between the
two hands the worker a `Failed` run at the cancel's version. The worker steps no run that is not
open and only releases its lease, so no step commits over a cancel and reopens the run
(`worker::claim_tests` in `src/worker.rs`).

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
| `UnknownAgent`, `WrongAgent { run, agent }`, `InvalidSession { reason }` | `Invalid` |
| `NotFound` | `NotFound` |
| `Finished`, `ConversationBusy` (`is_busy()`), `TooManyRounds { runs, max }` | `Rejected` |
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

## Steps that run at the same time

[`Ctx::step`] takes `&mut self`, so two steps cannot overlap. An agent that runs several side effects at once (the
tool calls of one model turn, [ADR 0049](../../docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md))
reserves their seqs first and runs them after:

* `Ctx::reserve_steps(&names)` takes seqs `k .. k+n-1` in order and moves the `Ctx` to `k+n`, so the next `step` is
  at `k+n` whichever reserved step finishes first. It reads the journal at each seq at once: a recorded name that
  differs is `NonDeterminism` there and nothing is taken.
* Each `ReservedStep` is owned (no borrow of the `Ctx`). `run(f)` is `step`'s contract: the recorded outcome, or `f`
  recorded at the step's own seq (`journal_put` takes seqs in any order; the first writer wins).
* `Ctx::step_all(steps, at_once)` is the convenience form: `at_once` at a time, outcomes in the order given.
* A reserved step that is never run leaves a gap. That happens only when the transition fails (a cancel, an error),
  and the replay fills it. After a crash the unrecorded steps run again (at-least-once, as for `step`).

Because the names and seqs depend only on the order of the reservation, the journal of a batch equals the one a
sequential run writes, so a replay works whatever concurrency each side used.

## Features and environment

No Cargo features, no environment variables at runtime.

## Tests

`tests/runtime.rs` is one behavioural suite (including
`a_starter_only_runtime_starts_and_a_full_runtime_steps`, the continuation cases
(`a_run_continues_a_finished_run_from_a_start_only_front`,
`continuing_checks_the_prior_run_and_creates_nothing_when_it_refuses`,
`a_prior_state_that_does_not_decode_starts_the_run_fresh`,
`an_agent_without_an_override_starts_fresh_when_asked_to_continue`,
`start_continuing_delivers_to_an_open_run_and_otherwise_continues`; the session cases (`a_session_round_carries_the_previous_rounds_conversation`, `a_round_is_refused_while_one_is_open_and_deliver_steers`, `a_repeated_session_start_returns_the_same_round`, `the_notice_kind_depends_on_the_childs_conversation`, `the_sessions_of_a_parent_are_listed_sorted_paged_and_scoped`, `a_lost_round_report_is_recovered_by_reading_the_round`); a repeat of a started run answers `false`
even when its prior is wrong or gone), the pair
`pinned_workers_step_a_run_only_on_its_owner` (three workers, each first seeded alone with one
unfinished run so all three own something, then twelve six-step runs stepped together: each run
steps on one worker only) and its control
`any_workers_let_a_run_move_between_workers` (a run seeded by one worker is finished by another),
`a_claimed_run_says_so_before_its_first_commit` (`RunView::claimed` is false before a worker takes a run, true while its first step is in the air with nothing committed, false once done, and the first claim is announced once, right after the start, not the claim of the second step) and `a_lease_that_ran_out_is_not_a_claim` (a `ManualClock` moves past the lease under a gated step: the view reads unclaimed), `a_step_that_outlives_its_lease_is_not_claimed_again_by_its_worker` (a lease lapses under a gated step and
whole claim passes follow: `FaultyStore::claimed` says the store handed the run out once, and the step ran once), and the two
`notifier_*` cases: two runtimes over one store and one `LocalNotifier`, a 30 s poll, a 5 s deadline, and the child-run cases: `a_finished_child_wakes_its_parent_once`, `a_lost_notice_is_recovered_by_the_timer`, `a_parent_that_loses_its_lease_does_not_start_or_resume_twice` and the rest, which use gates, a `ManualClock` and `FaultyStore::fail_run` and never sleep for a fixed time) run against `MemoryStore` always,
against PostgreSQL and against MongoDB when their variables are set. `tests/step_all.rs` runs steps that finish in
reverse at the seqs of the sequential journal, and `ctx.rs` has the reservation cases (consecutive seqs, a wrong name
takes nothing, a replay after a crash runs only the unrecorded steps, a gap is filled, `step_all` timing and limit). Unit
tests sit in `src/cancel.rs`, `erased.rs` (an override reaches an agent and a starter, the default is `init`, a prior state that does not decode falls back to `init`), `child.rs` (the id derivation is pinned by a golden value, the notice payload), `ctx.rs` (`emit_for` reaches the other run and records no artifact, `child_state` reads the run's own children only), `events.rs`, `file.rs` (images by their first bytes, a claim that stands and every way the bytes overrule it, the extension table, the shared line and its sizes, `ReceivedFiles`: names, types, the caps, refusals), `usage.rs` (a report within its bounds, totals one entry per provider and model and at most 32), `step.rs` (the words of the kinds, states and icons are the contract's, a step is made within its bounds on a character boundary, the serde shape leaves out what a step does not say), `notify.rs` and `retry.rs`, and the
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

New in slice 4 ([ADR 0045](../../docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md)): `SessionLimits` (private fields with getters `max_open()`, `max_working()`, `max_rounds()`; 1-100, `max_working` 1-32 and at most open, `max_rounds` 2-200; defaults 16, 4, 20; `new`, `lowered_by`), `DEFAULT_SESSION_IDLE_TTL` (7 days), `RuntimeBuilder::session_idle_ttl` and `Runtime::session_idle_ttl`. `list_sessions` and `session_view` read a session closed (`idle_expired`) when its latest round ended at least the TTL ago and it would otherwise stay open (a session with no header included); a working round never expires, and nothing is stored. `SessionSummary::does_work()` is what the working limit counts (a round that only closes a session does not work). **Breaking:** `SessionSummary` gained `idle_expired`, so a literal of it stops compiling. Purge finished runs (`Store::purge_finished`) only after longer than the TTL. Tests: `a_session_idle_past_its_ttl_reads_closed`, `a_working_round_never_expires` in `runtime_suite!` (three stores), unit tests in `session.rs`.
