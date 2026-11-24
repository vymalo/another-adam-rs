# 0074. A folder agent can sleep, and wait on a named queue

Status: **Proposed** (2026-11-24). Gap 1 of milestone M6 of another-agentic-system#402 ("Timers and waits: a sleep that
a folder agent's model can call, and named queues; a run has one inbox"). The premise was checked on `main` at
`91a052d` and on every open branch: no ADR covers a model-callable sleep or named queues (*verified* 2026-11-24 by
reading `docs/decisions/` and grepping it for `sleep`, `queue` and `inbox`). Number 0059 stays reserved; 0072 and 0073
are taken on open branches. Built in the slices below, none yet.

## Context

A run can already wait. `Transition::Park { state, wake_at }` commits the state and waits for a timer or any inbound
message (`crates/adam-runtime/src/agent.rs`); `claim_due` hands the run to one worker when it is due
([ADR 0060](0060-schedules-are-parked-runs.md)). The LLM agent uses it for a model that refused a call
(`Flow::Sleep(wake_at)` in `crates/adam-llm-agent/src/agent.rs`, [ADR 0067](0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md))
and for a child it waits on. The model itself cannot ask for it: no tool says "wait ten minutes", so an agent that polls
a build or a rate limit must spend turns, or a worker, doing nothing.

A run also has one inbox: an ordered list of `Inbound` messages, taken whole by `Ctx::take_inbox` and committed with the
transition (`crates/adam-runtime/src/ctx.rs`). Everything lands there, in order: the person's words, a steer
([ADR 0016](0016-a-message-sent-to-a-working-task-is-steered-into-it.md)), a child's notice. An agent that waits for
one kind of message (a webhook's payload, an answer from another system) must read and sort everything.

[ADR 0061](0061-a-step-names-its-retry-policy.md) (retry) and [ADR 0062](0062-a-race-of-steps-records-its-winner.md)
(race) show the shape of an answer: one journaled step, replay returns the recorded outcome, nothing is run twice.

## Decision

1. **`sleep` is a parked run, not a held worker.** It reuses [ADR 0060](0060-schedules-are-parked-runs.md)'s mechanism:
   the tool call returns a `Flow::Sleep(wake_at)`, the run commits and parks with `wake_at`, and no worker, lease or
   task is held while it sleeps. Rejected: a `tokio::time::sleep` inside the tool (a worker held for hours, lost on
   restart, and the lease must be renewed), and a new store method (a parked run already is a durable timer).
2. **The tool.** `sleep { seconds | until, reason? }`: `seconds` is an integer from 1 to the folder's maximum;
   `until` is an RFC 3339 instant, in the future, no later than now plus the maximum; exactly one of them. `reason` is
   a short text (at most 200 characters) shown to the client. The maximum is `sleep.max` of the deployment, default
   one hour, ceiling seven days; a run's sleeps together may not pass `sleep.budget` (default 24 hours), after which the
   tool is refused with a result that says so, and the model goes on. A value outside the bounds is a refusal the model
   reads, not a failure of the run. Time is jiff ([ADR 0057](0057-time-is-jiff-with-the-bytes-chrono-wrote.md)).
3. **A sleep is journaled once.** The wake time is `Ctx::now_journaled` plus the span, written into the run's state
   with the call's id before the park commits (`sleep: { call, wake_at, slept_since }`). A replay finds the call in the
   state and parks to the same `wake_at`, or, when it has passed, returns the result: it never reads the clock again
   and never sleeps twice. The tool's result is `{ "slept_seconds", "woken_early": bool, "woken_by"? }`.
4. **A message wakes a sleep early, and the model is told.** Any inbound message ends the park, as it ends any
   park. The run reads its inbox as usual, the sleep's result says `woken_early: true`, and the message is handled
   normally. A child's notice that the model is not waiting on does not shorten a sleep: the run parks again for the time
   left, without a new journal entry. A schedule's firing may sleep too: nobody can wake it, so it sleeps its time.
5. **Sleep is opt-in by the folder.** The tool is offered only when the folder's card lists `sleep` among its tools,
   and the deployment's maximum is not zero. A coder run keeps its tools as they are. A folder cannot raise the
   deployment's maximum, only lower it (`sleep: { max: 15m }`).
6. **A named queue is a label on a message, not a second inbox.** `Inbound` gains an optional `queue`
   (`[a-z0-9_-]{1,32}`); a message with none is today's, in today's inbox, unchanged. A run keeps at most 8 queues,
   each holds at most 64 messages of at most 16 KiB, and a delivery past a limit is refused to its sender with a
   permanent error that names the limit. Order is first in, first out within a queue and undefined across queues.
7. **Who may send to a queue.** Whoever may already send a message to the run: the conversation's A2A caller (the
   message carries `adam.queue` in its metadata) and the run's parent through `resume_child`. A queue adds no
   permission and no route. A message to a queue is **not a user turn**: the model never reads it unless the run waits on
   that queue.
8. **`wait_for { queue, timeout_seconds }` takes from a queue.** It is a parked run with a timer, as in decision 1:
   the call returns the oldest messages of the queue (up to 16, in order) at once if any are there; otherwise it parks
   until a message of that queue arrives or `timeout_seconds` passes (the same bounds as `sleep`), and returns an empty
   list on a timeout. A message of another queue, or of the main inbox, wakes the run, is handled as in decision 4,
   and the run waits again for the time left. The messages taken are recorded in the call's step result, so a replay
   returns the same ones and never takes them twice, and they are removed from the queue in the transition's commit.
9. **A client sees a sleeping run as working, with a reason.** The task stays `working` (it is not `input-required`:
   nobody is asked). A new status metadata key, `adam.waiting`, says `{ "kind": "sleep" | "queue", "until", "queue"?,
   "reason"? }`, and the `steps/v1` step of the call stays open with the same fields until the wake. A client that
   knows nothing of the key sees a working task that is quiet, as for a model park. *Unverified:* that the A2A task
   status can carry the key in the form `crates/adam-a2a-runtime/src/model_park.rs` uses for a model park; slice 4 reads
   that file first and amends this decision if it cannot.
10. **The agent's card says it.** An agent with `sleep` or `wait_for` lists the extension `waits/v1` in its card, with
    the maximum and the queue limits, so an orchestrator can show a sleeping run and send to a queue only where one
    exists. The extension is an optional
    announcement, as the others are: capability is read live and never assumed.

## Consequences

- A run that sleeps for an hour costs a row, not a worker. The run is claimed again at `wake_at` by any worker of
  its agent; under `ClaimScope::Pinned` the gap recorded in ADR 0060 applies to it too.
- A retention rule that purges runs must not purge a parked one: it is not finished.
- `Inbound` gains a field. A store that keeps the inbox as an opaque value needs no change; one that indexes it must be
  told, and the conformance testkit of `adam-store-testkit` gains the cases of slice 1.
- The model's tool list grows by two names for a folder that asks. The prefix of a request stays stable
  ([ADR 0044](0044-paged-tools-a-stable-prompt-prefix-a-plan-skills-commands-and-project-context.md)): the tools are
  added once, in name order, and only when the folder lists them.

## Slices

Each row is one pull request. Every test is named here and written in the slice.

| # | Slice | Files | Decisive test | Needs |
|---|---|---|---|---|
| 1 | `Inbound.queue`, its limits and the delivery refusal; the testkit's cases | `crates/adam-runtime/src/agent.rs`, `crates/adam-runtime/src/runtime.rs`, `crates/adam-store-testkit/src/lib.rs` | `named_queue_keeps_order_and_limits`: two queues interleaved keep their own order; the 65th message and the 9th queue are refused, on Memory, SQLite and PostgreSQL | none |
| 2 | The `sleep` tool: the state, the journaled `wake_at`, the park, the result | `crates/adam-llm-agent/src/agent.rs`, new `crates/adam-llm-agent/src/sleep.rs` | `sleep_survives_a_crash_and_does_not_sleep_twice` on a manual clock: a restart mid-sleep wakes at the same `wake_at`; a replay after the wake returns the recorded result | none |
| 3 | An early wake and the budget | the same, `crates/adam-llm-agent/tests/sleep.rs` | `a_message_ends_a_sleep_early_and_a_notice_does_not`; `a_sleep_past_the_budget_is_refused_not_failed` | 2 |
| 4 | `adam.waiting` in the task status and the open step | `crates/adam-a2a-runtime/src/model_park.rs`, `crates/adam-a2a-runtime/src/steps.rs`, new `crates/adam-a2a-runtime/src/waits.rs` | `a_sleeping_task_is_working_and_says_until`: `GetTask` and a subscription show the key, and it clears at the wake | 2 |
| 5 | `wait_for`: take from a queue, the timer, the replay | `crates/adam-llm-agent/src/sleep.rs`, `crates/adam-llm-agent/tests/queues.rs` | `wait_for_returns_each_message_once_across_a_crash`; `an_empty_wait_times_out_with_an_empty_list` | 1, 2 |
| 6 | Sending to a queue over A2A (`adam.queue`) and by a parent | `crates/adam-a2a-runtime/src/backend.rs`, `crates/adam-a2a-runtime/src/convert.rs`, `crates/adam-runtime/src/child.rs` | `a_caller_sends_to_a_queue_and_a_stranger_cannot`: the caller's message lands in the queue, a message to a run of another caller is refused as today | 1 |
| 7 | The folder's `sleep` key, the deployment's `sleep.max` and `sleep.budget`, the `waits/v1` card entry | `crates/adam-agent-fs/src/`, `crates/adam-assembly/src/`, `docs/reference/agent-files.md`, `docs/reference/environment.md` | `a_folder_cannot_raise_the_deployments_maximum`; a card golden with `waits/v1` | 2, 5 |
| 8 | The dev scenario and the docs | `dev/` (a scripted model that calls `sleep` and `wait_for`), `docs/guides/write-an-agent.md`, `docs/roadmap.md`, `docs/README.md` | `dev/waits-e2e.sh`: a run sleeps 3 s, a queue message wakes a `wait_for`, the card lists `waits/v1` | 4, 6, 7 |

## Open for the owner

- The defaults of decision 2 (one hour per sleep, 24 hours per run) and the queue limits of decision 6 are proposals.
- Whether a schedule's firing (decision 4) may sleep, or should be refused so that an unattended run cannot hold a
  slot of its conversation for hours.
