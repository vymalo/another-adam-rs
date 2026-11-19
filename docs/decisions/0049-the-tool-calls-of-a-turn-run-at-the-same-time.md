# 0049. The tool calls of a turn run at the same time

Status: **Accepted** (2026-11-17), an owner decision. Two sentences of the owner's decide it:

> *"adam should run tools in parallel with a max of 10, configurable. So that the model can request 4 tools and get them called at once."*

> *"we can make tool call sync on demand by the agent, and async by default."*

The owner then accepted every recommendation of the plan that follows ("Accept all", 2026-11-17). Amends
[ADR 0037](0037-parallel-subagent-calls-and-an-editing-worker.md) (decisions 1 to 3) and
[ADR 0039](0039-the-agent-loop-takes-opencodes-structure-and-agents-get-plugins.md) (its slice 2). Built in slices P0 to P9;
the status of each is in *Slices*.

## Context

What the loop does today, *verified 2026-11-17 by reading `49e2d2b`*:

| Fact | Where |
|---|---|
| `run_pending` takes the first owed call, runs it as the journaled step `tool:<call id>`, applies its result and takes the next. A turn of four reads takes four times as long as one. | `crates/adam-llm-agent/src/agent.rs` `run_pending`, `run_tool` |
| A cancel is checked between calls. A running call sees it through its `ToolCtx` token, a child of the run's (`within`). | `run_pending`, `within` |
| `doom_loop` is decided before a call from the committed state, and `repeats` is updated from each result, so call *i*'s decision depends on the result of call *i-1*. | `run_pending`, `record_answer` |
| `max_tool_calls` is counted when the model asks. | `model_turn` |
| `Ctx::step(&mut self, ..)` takes the next seq, runs the work if nothing is recorded, writes it, and moves to `seq + 1`. `Store::journal_put` takes seqs in any order. | `crates/adam-runtime/src/ctx.rs`, `crates/adam-core/src/store/mod.rs` |
| `Tool` has `asks_user`, `paged`, `time_limit` and `step_style`: no declaration of what a call shares with the others. | `crates/adam-llm-agent/src/tool.rs` |
| adam never sends `parallel_tool_calls`; `MODEL_EXTRA_BODY` may. | `crates/adam-model-openai/src/wire.rs`, `docs/reference/environment.md` |
| `run_command` snapshots its slot before the command and **restores** any difference after it, so an `edit_file` in the same slot during a command would be undone. | `bin/adam-coder/src/tools/inspect.rs` |
| `NotesStore::save` is atomic (temp file and rename) but not locked: two tools that load, change and save the notes at once lose a write. | `bin/adam-coder/src/tools/notes.rs` |
| `Environment::ensure` is single-flight per run. The git sandbox runs at most `GIT_SANDBOX_MAX_COMMANDS` (64) commands at once. | `adam-env-kubernetes`, `adam-devcontainer`, `adam-git-sandbox` |
| A2A `steps/v1` throttles per step id, so several running steps each get their start and their end. | `crates/adam-a2a-runtime/src/subscribe.rs` |

How OpenCode V2 does it, *verified 2026-11-17* at tag `v2.3.0` (`a6b15d33`) of `github.com/anomalyco/opencode`, the repository
npm `@opencode/cli@2.3.0` names (paths under `packages/`):

| Fact | Source |
|---|---|
| Every `tool-call` event of the model's stream forks a fiber at once, with no concurrency limit. | `core/src/session/runner/step.ts:135-158` |
| The step waits for every call; on a stop, cooperative tools are asked to cancel and the rest are interrupted after `COOPERATIVE_STOP` (5 s). | `step.ts:106-117,166-186,370,66` |
| Results go back to the model in the order of the calls, not the order they finish. | `core/src/session/runner/to-llm-message.ts:218-223` |
| Writes to one file are serialised by a process-local mutex keyed by path. | `core/src/file-mutation.ts:36-37,68-88` |
| `parallel_tool_calls` is sent only when configured; the system prompts of several models push batching. There is no per-call ordering flag. | `ai/src/protocols/open-responses.ts:847-852`; `core/src/plugin/system-prompt/*.txt` |

adam takes the results in call order, the per-path write lock (as scheduling, here) and a prompt line that pushes batching. It
does not take calls started while the model still streams (the journal needs the recorded turn first), an unbounded number
at once, or "interrupted" answers (adam replays).

## Decisions

1. **All calls of a turn run concurrently, up to a limit, and conflicts are scheduled.** The limit is 10 by default and 1 to
   32 (`LlmAgentBuilder::max_parallel`, and later the deployment's `MAX_PARALLEL_TOOL_CALLS`). More calls than the limit wait
   in a queue inside the turn and start in call order; a call is never refused for it. A limit of 1 is today's behaviour.
2. **A tool declares what a call shares: `Tool::access(&self, &Value) -> ToolAccess`**, from the call's arguments (which come
   from the recorded turn, so it is deterministic):

   ```rust
   #[non_exhaustive]
   pub enum ToolAccess {
       Independent,          // shares nothing with the run's other calls
       Uses(Vec<Use>),       // Use { mode: Read | Write, scope: String }
       Alone,                // the default
   }
   ```

   A scope is an opaque key with `/` levels (`files`, `files/src/a.rs`, `commands`). Two uses **overlap** when one scope equals
   the other or is a prefix of it at a `/`. Two calls **conflict** when one is `Alone`, or when they have overlapping uses and
   at least one writes. `ToolSource::access(&self, name, args)` does the same for a source's tools (the first source that does
   not answer `Alone` decides; a source answers `Independent` or `Uses` only for a tool it offers).
   `#[tool(parallel = "independent" | "read" | "write" | "exclusive", path = "<argument>", access = path::to::fn)]` and
   `FnTool::with_access` declare it for a function. `read` and `write` use `files`, or `files/<normalised path>` with `path`
   (lexical normalisation: `./` dropped, `a/./b` folded; an absolute path or one with `..` widens to `files`). A wrapper must
   forward `access`, as it forwards `asks_user`; one that does not makes the tool `Alone`, which is safe. The class is on
   `Tool`, not in `ToolSpec`, which is the model-facing type with public fields. `ToolAccess` and `Use` are
   `#[non_exhaustive]` **on purpose, and unlike the closed enums of `CLAUDE.md` Rule 3** (`Role`, `Placement`,
   `ClaimScope`): a new class or a new field of a use must not break every tool author's build, and a consumer that
   matches `ToolAccess` has no reason to fail to compile on a new variant (it treats an unknown one as `Alone`).
   `UseMode` is closed (`Read`, `Write`): a third mode would change what conflicts, so every consumer must see it.
3. **A tool that declares nothing is `Alone`** (fail closed): it is code adam does not know, and one attribute opts in. Every
   first-party tool declares its class (slices P7 and P8). MCP tools are `Independent` by default, with `"parallel": false`
   per server in `mcp.json` to make them `Alone` (slice P7).
4. **The loop forces `Alone`, whatever a tool says,** for `asks_user()` tools, the session tools (`SUBAGENT_START`, `SEND`,
   `READ`, `CLOSE`), and every call when the limit is 1. A tool that can park the run on a child or a remote task
   (`AwaitRun`, `AwaitSession`, `AwaitRemote`: the subagent tools) is **not** detected by the loop, because no signal says
   so before the call: it is `Alone` by declaring nothing (`SubagentTool` and `RemoteSubagentTool` do), and one that
   declares a class and parks anyway gets decision 11's error result. A call with the same key as the call just before it (for the first call, the committed
   `repeats.key`) is a **barrier** too, because `doom_loop` needs that result applied.
5. **Segments.** A *segment* is the run of calls between two barriers; an `Alone` call is a barrier and a segment of its own.
   A barrier starts when every earlier call has finished and its result has been **applied in call order**, and nothing after
   it starts before it is done. Inside a segment, call *j* waits for each earlier call *i* it conflicts with; otherwise it
   starts as soon as fewer than the limit are running. Ready calls start in call order.
6. **The journal is the sequential journal.** `Ctx::reserve_steps` takes the seqs `k .. k+n-1` of a segment's calls in call
   order and `ReservedStep::run` records each at its own seq (`Ctx::step_all` is the convenience form). Only barrier calls can
   be followed by extra steps (`usage:`, `ctx.now`), so the tool steps of a segment have the names and seqs the sequential loop
   gives them: a replay works with any limit, a rollback to a sequential build replays, and changing the limit mid-run is
   harmless.
7. **Results go back in the model's order.** Messages, artifacts, the file budget, kept outputs, the plan and the doom-loop
   count are applied **in call order** after the segment, whatever order the calls finished in. Step events are said live: a
   `running` event when a call actually starts and its end when it finishes, both with the call's id; if applying the result
   changes the outcome (a file refused by the budget), a corrected end event follows. The room for kept outputs, the room
   for files (`ToolCtx::spend_files`) and the image gate are one budget for the segment and are enforced again when
   results are applied.
8. **A crash in the middle of a segment** replays the same seqs: recorded calls are not run again and unrecorded calls run
   again (at-least-once, as today). A `Read` is harmless to repeat, a `Write` must be retry-safe (the coder's write tools
   are), an `Independent` call has the contract of one MCP call today, and an `Alone` call is alone, so at most one is in
   flight. No call is answered "interrupted": without a start marker adam cannot tell "never started" from "started", and
   a marker would be a second journal write per call. OpenCode answers that way because it has no replay.
9. **A transient failure of one call** stops new starts, waits for the running calls to end within their time limits, and
   fails the transition transiently; the retry from a fresh seq runs the turn's calls again. (`Tool`'s doc said "every tool of
   the same model turn that ran before it"; it now says every call of the turn that started in the failed try.)
10. **A cancel** is checked before each start. Calls not yet started never start. Running calls have the run's token, as
    today, and the loop waits up to `TOOL_STOP_GRACE` (5 s) for them, then drops their futures and fails the run `cancelled`.
11. **A call that cannot run in a segment of several** and still returns `NeedsInput`, `Await*` or a result with
    `reported` rounds there (a declaration bug; `ToolOutput::reported` is honoured only from tools that run alone) is
    answered with an error result, "this tool cannot run alongside others: call it alone". The outcome is recorded, so a replay
    agrees, and a test catches the bug.
12. **Sync on demand** is a reserved optional boolean argument, `after_previous`, injected into every schema when the limit is
    above 1 and stripped before the tool, `Tool::access`, `call_key`, hooks and the step's input see the call. A call with it
    is a barrier. Slice P6 built it: a one-sentence addition to the instructions says what it means. A bad value answers an error and the call takes no step; a tool whose own schema has the name keeps it (an own tool of the agent is a `BuildError::ReservedArgument`, a source's tool is left alone and logged); the flag is kept in the state with the turn's pending calls, and the names of the tools that have one of their own in the recorded model answer, so a replay does not depend on the sources. Alternatives weighed:
    `after: [index]` (positions are error-prone for models, and needs cycle checks), a wrapper `run_in_order { calls }` (inner
    arguments no schema validates, one result for many calls), and a barrier tool `then {}` (a meaningless call and result in
    the history). The fallback if the extra tokens (an estimate: 300 to 400 per request for about 40 tools) matter is `then {}`.
13. **`parallel_tool_calls` is not sent.** adam leaves it to the provider (on OpenAI it defaults to `true`, *verified
    2026-10-11*, `openai/openai-openapi`, recorded in ADR 0037); `MODEL_EXTRA_BODY` can force it. At a limit of 1 `false` is
    not sent either: the model would make more turns. Whether the owner's gateway and model ask for several calls per turn is
    *unverified*. No call starts while the model is still streaming.
14. **Commands, notes and one-shot subagents.** In v1 the coder runs one command at a time per workspace (`Write commands`,
    alongside reads), because `run_command` restores any change it did not make. `NotesStore::update` (a per-run async mutex
    around load, change and atomic save) makes concurrent note writes safe. One-shot subagent calls stay `Alone`; background
    sessions ([ADR 0045](0045-a-subagent-can-run-in-the-background-as-a-session.md)) are the way to children side by side, and
    ADR 0037's `waits` and `held` are a later slice (P10). There is no per-process cap: `WORKERS` times the limit bounds it,
    and the git sandbox queues beyond 64 commands.

The owner's examples:

| Turn | What runs |
|---|---|
| 4 reads (`read_file`, `search_files`, `list_files`, `find_files`, any files) | all 4 at once, about 1x instead of 4x |
| 2 reads and an edit | all 3 at once; a read of the edited file keeps the model's order with the edit, and the other read runs alongside |
| 3 commands (`run_command`) | one after another, as today, alongside any reads or MCP calls of the turn |
| 4 GitHub MCP reads | all 4 at once |

```mermaid
sequenceDiagram
    autonumber
    participant M as model turn<br/>recorded model:N
    participant R as run_pending
    participant S as schedule<br/>(pure)
    participant X as Ctx
    participant T as tools
    participant K as EventSink

    M-->>R: calls c1 read, c2 read, c3 write, c4 alone
    R->>S: classes from Tool::access, barriers, the limit
    S-->>R: segment c1 c2 c3, then the barrier c4
    R->>X: reserve_steps(tool:c1, tool:c2, tool:c3) at seq k, k+1, k+2
    par up to the limit, in the transition's task
        R->>K: Step tool:c1 running
        R->>T: c1 call, recorded at k
    and
        R->>K: Step tool:c2 running
        R->>T: c2 call, recorded at k+1
    and
        R->>K: Step tool:c3 running, after c1 if it conflicts
        R->>T: c3 call, recorded at k+2
    end
    T-->>R: results, in whatever order they finish
    R->>R: apply c1, c2, c3 in call order: messages, artifacts, budgets, doom loop
    R->>X: step(tool:c4) at k+3, alone, after the segment is applied
    R-->>M: the next model call sees the results in call order
```

```mermaid
stateDiagram-v2
    [*] --> Owed: in the recorded turn
    Owed --> Barrier: Alone, a repeated key or a limit of 1
    Owed --> Queued: in a segment
    Barrier --> Running: every earlier call finished and applied
    Queued --> Running: no conflicting earlier call running, fewer than the limit
    Running --> Finished: the tool answered, or its time limit passed
    Running --> Cancelled: the run was cancelled
    Running --> Failed: a transient failure, no new call starts
    Queued --> Cancelled: never started
    Finished --> Applied: in call order, with the segment
    Applied --> [*]
    Cancelled --> [*]: the run fails cancelled after TOOL_STOP_GRACE
    Failed --> [*]: the transition is retried from a fresh seq
```

## Consequences

*Rule 2 of `CLAUDE.md`.*

* **Additive:** `Tool::access`, `ToolSource::access`, `ToolAccess`, `Use`, `UseMode`, the `#[tool]` options,
  `FnTool::with_access`, `Ctx::reserve_steps`, `ReservedStep`, `Ctx::step_all`, `LlmAgentBuilder::max_parallel`
  (slice P4), `NotesStore::update` (slice P8: a lock per run around load, change and save; the coder's read-change-write sites moved to it, and the gates that run alone keep one `load` and one `save`).
* **Not additive, built in P5 to P7:** `adam_agent_fs::Limits` gained `max_parallel` and `McpSettings` gained `parallel`; `adam_service::WorkerSettings`, `adam_agent::WorkerParts` (and `assemble_with` now delegates to `assemble_limited`) and adam-coder's `WorkerConfig` and `CoderSettings` gained a field; `BuildError` gained the variant `ReservedArgument`; `Conversation` gained `ordered_calls` (emptied once the turn's calls are answered); `Tool` gained the provided method `schema_is_foreign` (an MCP tool's schema is its server's, so a property `after_previous` in it is kept and is no build error; a wrapper must forward it); `ToolCtx::spend_files_of` takes the room for a result's files by compare-and-swap and drops a file that does not fit; the chart renders `MAX_PARALLEL_TOOL_CALLS` only when `config.maxParallelToolCalls` is set; the prompt prefix changed once (the schemas and one sentence of the instructions). **Folder agents:** the MCP calls of one model reply now overlap (`"parallel": false` per server opts out).
* **Slices P1 to P4 change no behaviour in production:** every first-party tool is `Alone` until its class is declared, so
  the loop runs one call at a time as before. Tests of the loop with declared test tools prove the rest.
* **A scope is a path as written.** A read through a symlink inside a slot and a write of its target do not conflict,
  so the order of a model's read-then-edit is not kept across an alias; the read sees one whole file (a write renames
  a new file over the old) and `after_previous` orders what the model needs ordered. Accepted: resolving every path
  against the slot would put a file system call in the scheduler.
* The plan for hooks becomes ADR 0050.

## Slices

| # | Slice | State |
|---|---|---|
| P0 | this ADR, the notes on ADRs 0037 and 0039, `docs/architecture.md` | built |
| P1 | `Ctx::reserve_steps`, `ReservedStep`, `Ctx::step_all` | built |
| P2 | `ToolAccess`, `Use`, `Tool::access`, `ToolSource::access`, `#[tool(parallel, path, access)]`, `FnTool::with_access`, forwarding in every wrapper | built |
| P3 | `schedule.rs`: segments, conflicts, barriers, the queue (pure) | built |
| P4 | `run_pending` by segments, `LlmAgentBuilder::max_parallel` | built |
| P5 | `MAX_PARALLEL_TOOL_CALLS`, `limits.max_parallel`, the chart key | built |
| P6 | `after_previous` | built |
| P7 | classes of the library tools: MCP, UI, assembly, `read_output` | built (`"parallel": false` per server in `mcp.json`; the thread-tools source stays `Alone`; folder agents' MCP calls now overlap) |
| P8 | the coder: `NotesStore::update`, the class table, atomic writes | built (`tests/tool_access.rs` pins the table; `write_file` and `edit_file` were already atomic) |
| P9 | the batching sentence, the bench pin, calls per turn in the bench | built, except the pin: `bench/pins.json` moves to the coder image of the merge, which does not exist yet |
| P10 | ADR 0037 decision 4 (`waits`, `held`) so subagent and remote calls join segments | later |

*Unverified:* that GitHub tag `v2.3.0` is byte for byte the published binaries; that a provider requires tool results in call
order (they are in call order anyway); the owner's gateway and model asking for several calls per turn; the steps panel of
`vymalo/another-agentic-system` with concurrent steps; that the coder's `write_file` and `edit_file` write atomically; the
token estimate for `after_previous`.

## Amended 2026-11-18: a call starts when its arguments are complete

The last sentence of decision 13 ("No call starts while the model is still streaming") no longer holds. A call starts as
soon as its arguments are whole, while the model is still writing the rest of its answer, as OpenCode does; the journal
still gets the turn before any of its calls, so the guarantees of decisions 6 and 8 are unchanged. Everything else in
decision 13 stands: `parallel_tool_calls` is not sent.

**What the client says.** `adam_model::ModelDelta` gains `ToolCallComplete(ToolCall)`, and the enum is `#[non_exhaustive]`
(a consumer that matches it has one more arm). A streaming client says it **once per call**: the first time the
accumulated argument text parses as a JSON **object**, and, for every call it did not say it for, immediately before
`Finished`. Empty argument text is not complete on its own (it parses as `{}`), so a provider that sends the id and the name
first and the arguments after is not announced as `{}`. The call it carries is the one the turn will hold: the same id (a
made-up id is fixed at the announcement and the response keeps it), the same name and the same arguments, and a call whose
text never parses as JSON is announced with `invalid_arguments` set. The position of a call is the order the client
announced it, which is call order unless a provider interleaves its indices; the turn checks the batch against the calls it
recorded. A client that cannot stream says nothing: a non-streaming model starts nothing early. **Decision 12 is unchanged**:
a call that says `after_previous: true` is still a barrier and waits for the turn, the gate never starts one, and a value
that is not a boolean still answers an error and takes no step.

**The gate** (`crates/adam-llm-agent/src/early.rs`, pure, like `schedule.rs`). A call starts early only when starting it
early cannot change what the turn would have done. All of these must hold:

* `max_parallel` is above 1, fewer calls than it are running, and the call does not conflict with one that started early
  and is still running (decision 1: two conflicting calls never overlap);
* the turn is not the closing one, and the run is not cancelled;
* the call takes a step at all (not a shortcut: arguments that are not JSON, a tool nobody has with no source to ask);
* the call is not a barrier: its access is not `Alone`, it does not say `after_previous: true`, and its key differs from the
  call before it (the committed `repeats.key` for the first call of the turn);
* no earlier call of the turn was a barrier: the call is in the first segment;
* `tool_calls + i <= max_tool_calls` for its 1-based position `i`;
* for the first call of the turn only, `doom_loop` would not refuse it (for a later call that decision needs the result of
  the one before it, which does not exist yet).

A call that fails the gate waits for the turn, exactly as before, so the segments, the queue, the barriers, the results in
call order and the step events of decisions 5 and 7 are unchanged for it. A call whose `after_previous` is not a boolean
does not run at all, so it is not started early either.

**The loop.** `model_turn` creates the batch (`Early`) and moves it into the journaled step `model:N`; the stream polls its
futures beside the deltas (`tokio::select!`), so a started call really runs while the model writes, and says its `running`
step event when it starts and its end when it finishes. When the step returns, the turn is journaled, and `run_pending`
hands the batch to the turn: a started call whose recorded call is equal (id, name, arguments) is recorded by
`ReservedStep::run` with the outcome it gave, instead of running again. A batch that does not match the turn's calls at
their positions is dropped whole, and every call runs at the turn as before (the calls it started ran, and run again:
at-least-once, decision 8).

**The journal is unchanged.** No tool step is written before its turn: `Ctx::step` writes `model:N` when the closure
returns, and only then does the turn reserve the seqs of its calls. At any limit the journal is `model:N`, then
`tool:<id>` in call order at consecutive seqs. A crash in the middle of the stream writes nothing and the replay asks the
model again and runs the calls again; a result that finished before the turn was recorded waits in memory, and a crash
loses it.

**A stream error** (a transient failure of the model call) waits for the started calls within their own time limits,
discards their results and fails the transition transiently; the retry asks the model again and runs the calls again.

**A cancel** fires the run's token, which is the parent of each call's. The stream returns at once; the loop then waits up
to `TOOL_STOP_GRACE` (5 s) for the started calls and drops the futures, and the run fails `cancelled`, as a cancel between
two calls of a segment does (decision 10). The model step is not written: the run was already failed in the store.

**What is not decided here.** A turn over `max_tool_calls` may have started its first calls early (their position was
within the limit when each was announced) and then answers all of them "not run": their results are dropped, and the run
goes to the closing turn. The step events of those calls say `running` and their end, and the answer says "not run"; the
alternative (never starting a call early until the whole turn is known) is not possible while the model writes. An early
call that runs in a segment of one (because the call after it is a barrier) is still answered like a call of a segment for
`NeedsInput`, `Await*` and `reported` rounds (decision 11): a tool that asks or parks must declare `Alone`, and the gate
never starts one.

## Amended 2026-11-19: a conflict with a call that did not start early

The gate's first condition above is incomplete. "It does not conflict with one that started early and is still running" is
not enough to keep decision 5's order: a call the gate **refused** (because it conflicted with one already running) runs at
the turn, after every early call, so a later call that conflicts with *that* call must wait for the turn too. Otherwise it
starts early, before the refused call, and the model's order is reversed.

The case that found it (`tests/early_calls.rs`, `a_call_that_conflicts_with_an_earlier_refused_call_waits_for_it`): c1
reads `files/x` and is still running; c2 writes `files`, so the gate refuses it (it conflicts with c1); c3 writes
`files/y`, which conflicts with c2 but not with c1, so it was admitted and ran before c2. c2's write then overwrote c3's:
the model asked for c2 then c3 and got c3 then c2. `Early` now keeps the access of every call the gate refused (`waiting`)
and a conflict with one is a refusal, so the condition is: does not conflict with one that started early and is still
running, **or with an earlier call of the turn that did not start early**. The call's own access is what the turn's queue
uses, so the two agree.

The gate is otherwise unchanged: a refused call still waits for the turn, exactly as before, and the journal, the cancel
and the at-least-once rules are the ones above. The empty-name fix of the same day is in `adam-model-openai`: a call whose
arguments are whole before its name arrives is announced when the name arrives, not silently skipped.

