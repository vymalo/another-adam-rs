# Child runs and remote tasks

A subagent is a run started by a tool of another run. This page is the full design of the runtime half:
the local case, the remote (A2A) case, every failure interleaving with the test that makes it happen,
and what the design refuses. The picture is in [Architecture](../architecture.md#child-runs); how a
folder declares subagents is in [Agent files](agent-files.md#subagents-at-run-time).

Code: `crates/adam-runtime` (`start_child`, `child_status`, the notice), `crates/adam-llm-agent`
(`pending_wait`, `AwaitRun`, `AwaitRemote`), `crates/adam-assembly` (`SubagentTool`, `RemoteSubagentTool`).

## A local child

* **The child id is derived.** `child_run_id(parent, key)` is a UUID (version 8) from a SHA-256 over the
  parent id and the key, the same way the A2A adapter derives task ids. `ToolCtx::child_run_id()` is that
  for the current tool call. A tool that runs again, after a crash, a lost lease or a transient retry, asks
  for the same child, and `start_child` (which is `start_with_id` plus the parent) answers `false` and
  leaves the first one alone. The only side effects of the tool are that creation and reads.
* **The message is a hint, the timer is the guarantee.** The runtime sends the message after the child's
  commit, in the worker that made it. It cannot be part of that commit (the store commits one run at a
  time), so a crash or a store error in between loses it, and so does a commit whose acknowledgement is lost
  (the worker then believes the commit failed and sends nothing). The parent therefore never waits without a
  timer: `LlmAgentBuilder::wait_poll` (60 s by default) bounds how late it can be, and each wake without the
  message is one `load_run`. A failed send is logged at `warn`; a parent that is finished or gone is logged
  at `debug` and is not an error.
* **The message is deduplicated by its id.** `Inbound::id` is the child's run id. The `LlmAgent` matches it
  to the run it waits for, uses it once (the wait is cleared in the same commit that consumes it) and drops
  every other: copies of a message already used, a message for a run it does not wait for, one that does not
  parse, one whose status is not final. Such messages are never read as user text.
* **The answer.** A finished child's `output.text` (what an `LlmAgent` child ends with), else its output as
  JSON, is the tool result. A `Failed` child, a cancelled one (`cancelled: <reason>`), one whose state cannot
  be read and one that was purged are an error result (`the run failed: <why>`): the model sees it and the
  run goes on. Nothing in the parent fails because a child did.
* **Who may be asked about.** `Ctx::child_status` answers only for children of the calling run. An id that
  belongs to a run of another parent (or to none) is a permanent error, so an agent cannot read the results
  of unrelated runs, and a derived id that collided with a stranger's run would fail loudly and not answer
  with the stranger's output.
* **`Conversation::pending_wait`** is what the parent is parked on: a `Question` for the user (this field was
  `pending_question`, and state stored under that name still loads) or a `Run` for a child. A2A reads a run
  parked on a timer as `working` and only a run parked with no timer as `input-required`, so a parent
  waiting for a child is `working`.


## Failure interleavings

Each one has a test that makes it happen without sleeping: steps are held at gates, the 60 s timer is
moved with a `ManualClock`, and a fault is scripted on one run of the `FaultyStore` (`fail_run`,
`fail_run_after_apply`). Names are in `crates/adam-runtime/tests/runtime.rs` (memory, PostgreSQL and
MongoDB) and `crates/adam-llm-agent/tests/child_runs.rs`.

| # | What happens | What the design does | Test |
|---|---|---|---|
| 1 | The child's terminal commit lands, then its process dies, or delivering to the parent fails | The parent stays parked on its timer. At the timer it reads the child, finds it terminal and answers. Nothing else knows anything is missing | `a_lost_notice_is_recovered_by_the_timer`, `a_lost_notice_is_recovered_when_the_timer_fires` |
| 2 | The child's terminal commit is applied but its acknowledgement is lost | The worker treats the commit as failed and sends nothing (the run is `Done` in the store and nobody claims it again). Same recovery as 1 | `a_lost_terminal_ack_sends_no_notice_and_the_timer_recovers`, `a_lost_terminal_ack_is_recovered_when_the_timer_fires` |
| 3 | The message is delivered twice, or three times, or again after the answer is in, or after the run is over | The first match answers the call and clears the wait. Later copies match no wait and are dropped. On a finished parent `deliver` answers `Finished`, which the sender ignores. The model is not asked again | `a_duplicate_notice_is_ignored`, `only_the_awaited_childs_notice_answers` |
| 4 | The parent is cancelled while it waits | **The child is cancelled too** ([below](#cancel-cascades-and-time-limits)): its worker sees the parent is `Failed` within one poll, fails the child with `cancelled: the parent run ended` and fires its step's token. What the child would have answered is dropped, and the parent stays `Failed` with `cancelled: <reason>`, untouched | `cancelling_a_waiting_parent_cancels_its_child`, `cancelling_the_parent_cancels_the_child_too`, `a_running_child_and_its_child_are_told_within_one_poll`, `cancel_task_reaches_the_subagent_run` |
| 5 | The child fails, is cancelled, or was purged | The parent gets `status: failed` and the reason (or, for a purged child, finds it gone) and the tool result is an error result. The run continues | `a_failed_or_cancelled_child_tells_its_parent_why`, `a_failing_child_is_an_error_result`, `a_cancelled_child_is_an_error_result`, `a_child_that_is_gone_is_an_error_result`, `a_purged_child_reads_as_gone` |
| 6 | The parent's lease is lost while it waits, or while it starts the child | Fencing is the version compare-and-swap, not the lease. The worker that took over replays the step: the journal has the start, and `start_child` would answer `false` anyway, so there is one child. The first worker's late commit is refused. The message is delivered with the same compare-and-swap, so it lands on whichever version is current. The parent resumes once. The same holds for the step that answers the call: its consumption of the message commits with it, so a worker that takes over sees the message again and answers the same way | `a_parent_that_loses_its_lease_does_not_start_or_resume_twice`, `a_worker_that_loses_its_lease_while_answering_changes_nothing` |
| 7 | The message arrives before the parent has parked | While the tool step runs: the message waits in the inbox, and the commit turns the park into a wake-up (the rule that already keeps a parked agent from sleeping through a message that arrived during its step). In a replay or a retry of that step: the snapshot the step starts from already holds the message but the wait is not recorded yet, so the agent matches it *after* the tool has returned `AwaitRun`, in the same transition, and never parks | `a_notice_that_beats_the_park_is_not_lost`, `a_notice_that_beats_the_park_is_used`, `a_retry_finds_the_notice_before_the_wait_is_recorded` |
| 8 | The timer fires and the child is still working | One `child_status` read, no answer, a new timer one interval later. No second child, no model call | `a_parent_woken_early_parks_again`, `the_parent_looks_at_the_child_when_the_timer_fires` |
| 9 | A user message arrives while the parent waits | It wakes the parent, which finds nothing to answer with and parks again. The message is kept and appended after the tool result, as for any owed result | `a_message_while_waiting_queues_behind_the_result` |
| 10 | A forged, stray or malformed `adam.run.finished` | Matched by the child's run id only, and only a final status counts. Otherwise it is dropped without becoming user text. `child_status` refuses runs that are not the caller's children | `only_the_awaited_childs_notice_answers`, `child_status_reads_only_the_callers_children` |
| 11 | Two workers start the same child | `create_run` refuses the second (`AlreadyExists`), `start_child` returns `false`. One row | `start_child_records_the_parent_and_is_idempotent` |
| 12 | The process that finishes the child has never heard of the parent's agent | Delivering needs only the parent's run id: it does not have to register the parent's agent. The parent's own worker steps it | the split `front` and `back` runtimes of cases 1, 2, 5 and 6 |
| 13 | The message races the timer: the parent reads the finished child at its timer, and the message lands while that step finishes (during the final model call, or before its commit) | The message answers nothing: the wait is settled. `Ctx::arrived` and the commit's reopen rule do not count a child's notice, so the parent finishes with its answer, as it would if the message came after the commit (then `deliver` answers `Finished`). The model is not asked again | `a_childs_notice_that_lands_while_a_run_finishes_does_not_reopen_it`, `a_notice_that_lands_while_the_parent_finishes_costs_no_model_turn` |

| 14 | A child runs past its time limit | The runtime fails it with `cancelled: time limit: the run did not finish within 30 min`: before a step, within one poll for a step in flight (its token fires), and at the wake it set at the deadline for a parked one. The parent gets "the run failed: cancelled: time limit: ..." as the tool result and goes on | `a_child_past_its_deadline_fails_and_wakes_at_it`, `a_child_past_its_limit_is_an_error_result_for_its_parent` |
| 15 | A child is created after its parent was cancelled | It is cancelled when it is claimed, before it steps. A `Done` parent changes nothing, and a parent that is not in the store is unknown and cancels nothing | `a_child_created_after_the_cancel_never_steps`, `a_child_follows_a_parent_that_failed_but_not_one_that_is_done`, `a_done_parent_that_was_purged_leaves_its_open_child_running`, `a_child_parked_on_a_question_is_cancelled_when_its_parent_is`, `a_grandchild_under_a_parked_child_follows`, `a_healthy_parked_child_is_woken_without_stepping_the_agent` |
| 16 | The same cancel arrives twice | The second is a no-op: a terminal run is left as it is | `a_child_created_after_the_cancel_never_steps` |

## Cancel cascades and time limits

[ADR 0044](../decisions/0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md) slice 10,
[ADR 0037](../decisions/0037-parallel-subagent-calls-and-an-editing-worker.md) decision 7. **No `Store` method was added**
(a cascade over `list_runs` would scan every run, and a `Store::children` would be a new required method): each child looks
at its **parent**, which it already knows (`RunRecord::parent_id`).

```mermaid
sequenceDiagram
    autonumber
    participant U as Caller
    participant R as Runtime
    participant DB as Store
    participant W as Worker holding a child
    participant C as Child's step

    U->>R: cancel(parent, reason)
    R->>DB: commit the parent Failed, cancelled: reason (compare-and-swap)
    loop before each transition of the child and on every poll of its cancel watch
        W->>DB: load_run(parent)
    end
    DB-->>W: the parent is Failed
    W->>DB: cancel_run(child, the parent run ended): commit Failed
    W->>C: fire the step's CancelToken
    Note over W,DB: the child's own children do the same when they next look at the child
```

```mermaid
stateDiagram-v2
    [*] --> Working: start_child, with a deadline when started within a limit
    Working --> Failed: the parent is Failed (cancelled: the parent run ended)
    Working --> Working: parked, looked at once a minute, the agent not stepped
    Working --> Failed: the clock passes the deadline (cancelled: time limit)
    Working --> Done: the child answers
    Working --> Working: the parent is Done or still working
    Failed --> [*]
    Done --> [*]
```

* **The rule.** A run with a `parent_id` is cancelled (`cancelled: the parent run ended`) when its parent is **loaded and
  `Failed`**, for any reason (a cancel included). A `Done` parent changes nothing: it has no working rounds, and a session
  kept open stays usable. A parent that cannot be loaded (the store is down, or retention purged a finished run) is
  unknown, not ended: the child goes on (debug log). This assumes retention is longer than a minute
  (`Store::purge_finished`).
* **Where it is checked** (`crates/adam-runtime/src/worker.rs`). Before a child's transition, with one `load_run(parent)`;
  on the cancel watch of a step in flight, at most once a second per child (`PARENT_CHECK_EVERY`; the watch itself polls
  every `poll_interval`, 250 ms by default, and reads the child's own run each time); and for a **parked** child at its
  look (below). The cancel is `Inner::cancel_run`, the body of `Runtime::cancel`: idempotent, a no-op for a run that is
  already terminal.
* **The look at a parked child.** A parked run that works for a parent, or has a deadline, is committed with a wake no later
  than `CHILD_WAIT_POLL` (a minute) away, and the envelope keeps the agent's own wake (`agent_wake`) and `hb`. When that
  wake claims the run, the runtime reads the parent **before** the agent: the parent ended, the child is cancelled; the
  parent is alive and nothing else is due (no message, the agent's own wake not yet), the run is parked again with the next
  wake **without stepping the agent, without a journal entry and without a model call**. So a child waiting for an
  answer to its own question, or with `max_minutes: 0`, is reached too, and so are its own children one level at a time.
  The cost of a parked child is one read and one commit a minute. `RunView::wake_at` and `waiting` say what the agent asked,
  not the runtime's wake.
* **How fast.** `Runtime::cancel` commits the parent first, unchanged. A running child is told within about a second
  (its token fires, and the coder's `shell.rs` kills its command); a runnable one when it is claimed; a parked one within
  a minute. Grandchildren follow one level at a time. A child that the parent's last step created after the cancel never
  steps. The A2A `CancelTask` path is unchanged and answers `Canceled` at once.
* **Not cancelled: a remote A2A task.** A subagent on another agent (`a2a:`) is a task there, and the cascade does not
  send it `CancelTask` ([ADR 0045](../decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md) slice 5).
  It keeps running until the remote end stops it. Cancelling the parent stops the wait and nothing else.
* **Run pods** are keyed by the root's workspace, so the janitor removes the pod with the failed root.
* **Time limit of a child.** `Runtime::start_child_within(parent, id, agent, input, Some(limit))` (also
  `ChildStarter::start_within`, `ToolCtx::start_child_within` and, for a session round,
  `start_child_continuing_within`; the last is the embedder's entry point until the session tools of ADR 0045 arrive)
  stores `deadline` in the run's envelope, set **once** from the runtime's own `Clock`, not from a transition. `wake_at`
  is clamped to it, runnable or parked, so a child wakes at it. **The time a child waits for a person is not counted**: when
  it parks with no timer the deadline is cleared and what is left is kept (`remaining_secs`), and the next step that
  follows a message sets it again; time on a timer, or on a child, still counts. A wake that arrives before the deadline
  (the store's clock and the runtime's disagree) parks the run again to the deadline plus a second, without stepping the
  agent, so a skew cannot make a tight loop. `SubagentTool` passes
  `limits.max_minutes` of the subagent's file: **30 minutes** when it says nothing, `0` for none, at most 1,440
  ([agent files](agent-files.md#the-subagent-tool)). The limit is per run: each round of a session has its own.
* **Time limit of a tool call.** A call that runs past `Tool::time_limit` (300 s when it declares none) is stopped
  inside its journaled step and answered with an error, so a replay never waits ([errors](errors.md#what-the-model-is-told)).

What the design refuses to do, and why:

* **No parallel fan-out yet.** An `LlmAgent` runs the calls of one model turn in order, and the first
  `AwaitRun` parks the run, so three subagent calls in one turn run one after the other. Starting all the
  children first and waiting for all of them needs `pending_wait` to hold several runs.
* **No asking tools in a subagent.** A child that parks with no timer, for instance because one of its tools asked the
  user something, waits for an answer nobody is placed to give; the time limit above now ends it at its deadline
  (30 minutes by default), and a cancel of the parent does not wake it. The subagent binding
  (`adam-assembly`) closes this at startup instead of at run time: a tool that can ask says so
  (`Tool::asks_user()`), and `bind` refuses a subagent that has one, so the situation cannot arise from an
  agent directory. A tool that returns `NeedsInput` without declaring it still parks its child, and a child
  can still be cancelled by id: the parent then gets the error result.
* **The output travels whole.** The child's output is the payload of the message, then the text of the tool
  result in the parent's history. Long histories are shortened for the model by `max_history_tokens` as for any
  tool output, but the stored conversation keeps it.
* **No guarantee for a child purged early.** `purge_finished` deletes a finished run with its journal. A
  parent that has not looked yet then reads `None` and reports "the child run no longer exists". Keep the
  retention above the parent's longest wait plus one `wait_poll`.
* **No mixed versions.** The journal entry `AwaitRun` and the `pending_wait` field are not read by a build
  that predates them (it would fail the run as non-deterministic on replay). Roll the workers before the
  tools that start children are enabled. The other direction is safe: journals and states written before
  (`ToolError` without `AwaitRun`, `Conversation` with `pending_question`) load and replay in the new build.

Code that runs inside a step but cannot hold the `Ctx`, such as a tool of an `LlmAgent`, starts a child through
`Ctx::child_starter()`: an owned `ChildStarter` for the runtime that is stepping the run, which can start only
children of that run (`start` is `start_child` with the parent fixed). `LlmAgent` hands it to every tool as
`ToolCtx::start_child(agent, message)` (under `ToolCtx::child_run_id()`), so a tool needs no `Runtime` handle,
and the child starts on the runtime that steps the parent, whichever process that is. The subagent tool of
`adam-assembly` is exactly this call followed by `AwaitRun`; see [subagents](agent-files.md#the-subagent-tool).

A hand-written `Agent` can wait for children too: start them with `start_child`, `Park` with a timer, and on
the next step read `take_inbox()` for messages of kind `adam.run.finished` (`ChildStatus::from_notice`) and
`Ctx::child_status` when there is none. It must take its inbox in the step that parks as well: a message
already in the inbox when a step starts is not "arrived during the step", so an agent that parks without
reading it sleeps until its timer.

## Sessions (ADR 0045, slice 1)

[ADR 0045](../decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md) lets a subagent run in the
background as a session that is sent more messages. Only the runtime half exists so far (`crates/adam-runtime`).

**What exists.**

* `Runtime::start_child_continuing` starts a child run **with a conversation id**: the parent's id (with `%` and
  `/` escaped, so a client's context id cannot spell another session's), then `/s/` and the session id
  (`session_conversation_id`). Round 1's run id, `child_run_id(parent, call_id)`, is the session id. From round 2
  on the child starts from the previous round's state (`init_continuing`, ADR 0003). A session has one agent,
  and the store allows one open run per agent and conversation, so a start while a round is open is
  `ConversationBusy` (`is_busy()`): the caller sends a message with `Runtime::deliver`, which the open round reads
  as a steered message. Everything the runtime can check is refused as `InvalidSession`: a `prior` of another
  conversation or of a session with no first round, a run id that exists and is not this round, a conversation id
  that is not a session's or is over 1024 bytes. A repeat with the same ids answers `false`; a round purged
  before its repeat is not recognised (retention must outlast retries).
* A child with a conversation reports its end with **`adam.child.reported`** instead of `adam.run.finished`.
  Same id, same payload, same standing: a wake-up, and the parent reads the round with `Ctx::child_status`, so
  the lost-notice cases above hold for a round. A child without a conversation is unchanged.
  **No agent reads a report yet.** `LlmAgent` drops it instead of making it a person's message, and `Ctx::arrived`
  and the reopen-on-arrival rule do not count it, as they do not count a finished notice, so a report that lands
  while a run finishes does not reopen it. Slices 2 and 3 make a report reopen a turn on purpose (decision 4).
* `Runtime::list_sessions` lists a parent's sessions over `list_runs` with the conversation prefix: the latest
  round of each with its state (`Working` or `Idle`) and how it ended (`Done`, `Failed`, `Cancelled`). A run counts
  only if the session's round 1 exists with a parent, so a top-level run in a session-shaped conversation is not
  listed, and a parent never sees another's (the prefix ends in `/s/`, so `ctx1` does not match `ctx10`).

* A round takes the same time limit as any child (`start_child_continuing_within`, per round), and a cancelled or failed
  parent fails the round that is open (`cancelled: the parent run ended`) while the session stays usable: a later parent
  run continues it from that round (`a_cancelled_parent_fails_the_open_round_and_the_session_continues`).

**What does not exist yet.** The tools (`subagent_start` and the others), `Conversation::session`, closing (so no
`Closed` state), a reader of `adam.child.reported` in `LlmAgent` or the coder, `PendingWait::Session`, remote
sessions and the A2A extension.

## Remote tasks: the same wait without a message

A subagent on another A2A agent (`a2a:` in its file) is the same idea with one thing missing: nothing tells
the parent when the remote task is over. The tool starts the task, returns `ToolError::AwaitRemote { task,
timeout_ms }`, and the parent records `PendingWait::Remote { call_id, tool, task, deadline }` and parks with
the `wait_poll` timer. Each time it fires the agent asks the tool how the task stands, as a journaled step,
until it is over. **The timer is the mechanism.**

```mermaid
sequenceDiagram
    autonumber
    participant P as Parent run<br/>LlmAgent
    participant J as Journal
    participant T as RemoteSubagentTool
    participant A as Remote A2A agent
    participant DB as Store

    P->>J: step tool:c1
    J->>T: call(message)
    T->>A: SendMessage, returnImmediately, messageId = child_run_id(parent, c1)
    A-->>T: Task, working
    J-->>P: Err(AwaitRemote { task, timeout_ms }), journaled
    P->>J: now_journaled, to fix the deadline
    P->>DB: commit Park, wake_at = now + wait_poll, pending_wait = Remote { c1, tool, task, deadline }
    Note over P,DB: the timer fires and the run is claimed again
    P->>J: now_journaled, deadline not reached
    P->>J: step poll:c1
    J->>T: poll_remote(task)
    T->>A: GetTask
    A-->>T: Task, working
    J-->>P: Ok(Working), journaled
    P->>DB: commit Park, wake_at = now + wait_poll
    Note over P,DB: the next wake finds the task final
    J-->>P: Ok(Ready(result)), journaled
    P->>P: the result answers c1, the wait is cleared, the loop goes on
```

```mermaid
stateDiagram-v2
    [*] --> Calling: the model calls the tool
    Calling --> Answered: the reply is final already (a message, or a task in a final state)
    Calling --> Waiting: AwaitRemote journaled, deadline fixed, run parked with a timer
    Waiting --> Waiting: timer wake, poll:c1 says working (or a user message queued behind the result)
    Waiting --> Answered: poll:c1 says ready
    Waiting --> Answered: the deadline has passed (an error result, no poll)
    Waiting --> Answered: a permanent poll error, or the tool is gone (an error result)
    Waiting --> Waiting: a transient poll error (the wake is retried with backoff)
    Waiting --> Cancelled: the parent is cancelled (the remote task is not)
    Answered --> [*]: tool result is the tool's answer
    Cancelled --> [*]
```

* **Every look is a journal step.** `poll:<call id>` records `Working` or `Ready(result)`, and, when the wait
  has a deadline, `ctx.now` is read through the journal (`Ctx::now_journaled`) so that a replay takes the
  same branch (a replay that skipped a recorded `poll:` step because the clock had moved would meet the
  wrong step name at the next `seq` and fail as non-deterministic). A transient poll error fails the wake as
  `AgentError::Transient`, and the retry looks again from a fresh `seq`.
* **The start is idempotent by message id, not by run id.** A child run is started under an id the runtime
  refuses to create twice; a remote task is started by a `SendMessage` whose `messageId` is
  `child_run_id(parent run, call id)`. The guarantee is only as good as the remote's memory of that id
  (`adam-a2a-runtime` starts the task under `task_id_for(agent, caller, context, messageId)`, so a repeat reaches
  the same task). The journal makes the call once per recorded outcome regardless.
* **Failure interleavings.**

| # | What happens | What the design does | Test |
|---|---|---|---|
| 1 | The process dies while the parent waits | The wait is in the run's committed state. A new process claims the run at the timer and polls the recorded task: no second send | `a_restart_mid_wait_resumes_polling_without_sending_again` (`adam-assembly`), `a_new_process_keeps_polling_without_starting_the_task_again` (`adam-llm-agent`) |
| 2 | The send reaches the remote and its response is lost | The step fails transiently and is retried; the retry sends the same `messageId`, and a deduplicating remote returns the task it made | `a_send_whose_response_is_lost_is_retried_under_the_same_message_id` |
| 3 | The remote finishes while nobody polls | The next timer wake finds it final and answers | the restart case above |
| 4 | The task never ends | At the deadline (default one hour, `AgentDef::remote_timeout`) the call is answered with an error result; the remote task keeps running | `a_task_that_never_ends_is_given_up_on_after_the_limit`, `a_task_that_outlives_its_timeout_is_an_error_result_without_another_look` |
| 5 | The remote fails, cancels, rejects, or wants input | An error result naming the state and the remote's message; the run goes on | `a_remote_task_that_fails_is_an_error_result_and_the_parent_goes_on`, `a_remote_task_that_is_canceled_is_an_error_result`, `a_remote_that_needs_input_is_an_error_result_because_nobody_can_answer` |
| 6 | The remote answers 401 or the card points the token elsewhere | An error result; nothing is sent to the other origin | `a_wrong_token_is_an_error_result_not_a_failed_run_and_the_token_stays_out_of_it`, `a_card_that_points_the_token_at_another_origin_is_refused_before_anything_is_sent` |

* **What it refuses to do.** No streaming yet (`SubscribeToTask` would end the wait sooner; the poll stays as
  the fallback); no cancel of the remote task when the parent is cancelled or the wait times out (that needs a
  tool hook for cancellation); no `contextId` continuity between calls. And, as with child runs, **no mixed
  versions**: a build that predates `AwaitRemote` cannot read a journal that contains it.

