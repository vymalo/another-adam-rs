# 0016. A message sent to a working task is steered into it (`steer/v1`)

Status: **Accepted** (2026-10-02), with plan 11 of 2026-10-02 (PR-14). The other side of the contract is the orchestration
layer's: [`docs/api/steer-v1.md`](https://github.com/vymalo/another-agentic-system/blob/main/docs/api/steer-v1.md) and its
[ADR 0036](https://github.com/vymalo/another-agentic-system/blob/main/docs/decisions/0036-sending-while-an-agent-works.md)
("Sending while an agent works: steer, or stop and send"), in `vymalo/another-agentic-system`. **Extends
[ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)** (a message's extensions, the card lists them) **and
[ADR 0003](0003-a-new-task-continues-the-task-it-references.md)** (a follow-up is a new task; this is the other case, a message for
the task that is running). **Built:** `STEER_EXTENSION` and `ExtensionConfig::steer()`, the card entry of `adam-agent` and the coder,
`BackendError::UnsupportedOperation`, the steer path of `RuntimeTaskBackend::submit`, `Ctx::arrived` and
`Ctx::reopen_on_arrival`, `Conversation::read_ids`, and the recorded model step's `seen`.

*Amended 2026-10-03:* a task is `working` from the moment a worker claims its run, not from its first commit. A turn
commits once, so during the first model call the run still read as version 1 and the task as `submitted`, and the
orchestration layer, which steers only a thread that is `working`, held a message sent then until the turn was over. The
run's lease now says it (`Store::lease_until`, `RunView::claimed`), and the worker announces its first claim as
`Status(Runnable, claimed)`. A steer is accepted in `submitted` and in `working` exactly as before; only which of the two
a task reads has changed.

*Amended 2026-10-09:* a child's `adam.run.finished` notice is not a message that arrived. `Ctx::arrived` and the commit's
reopen rule (decision 4) count every other message, and the notice still wakes a parked run. The notice is a hint for a
parent that waits; a parent that is finishing has settled its wait, from the notice or from the store at its timer. When
the notice raced that timer and landed during the final step, the parent took another turn and asked the model again
with nothing new to read (the remote-subagent test of `adam-assembly` failed so, intermittently, on PostgreSQL 12).

## Context

The orchestration layer lets a person write while an agent works ("you were wrong since line 1"). A2A leaves a message with the
`taskId` of a `working` task undefined, and a terminal task refuses every message; ADR 0036 there makes it a promise of an
extension: **a message that names a running task is added to that task's input, and the agent reads it at its next step. An
accepted message is never lost.** What adam-rs did, *verified 2026-10-02 by reading the code at commit `b22d93e`*:

* `RuntimeTaskBackend::submit` accepted a message with a `taskId` only while the task was `input-required` (`view.waiting`);
  a `working` task answered `InvalidParams`, a finished one too.
* `Runtime::deliver` already appends to the run's inbox in the store (a compare-and-set on the record), wakes a parked run, and
  is merged with a worker's commit: **a message delivered while a step runs is read by the next step.** `LlmAgent::step` drains
  the inbox first and puts each user text in the history, behind the result of a tool call that is still owed.
* **The end of the run lost it.** A step that returns `Done` while a message was delivered during that step is committed as
  `Done`: the commit merges the message into the envelope and finishes the run, and the message is never read. That is exactly
  the window "a message sent during the final model call" falls in, and the contract asks for it to be answered (it says the
  agent MAY refuse instead, which would have made the extension useless in the one window a person is likeliest to hit: the model
  is writing its last answer *because* the work looks finished).
* **Nothing recognised a repeat.** The orchestration layer repeats a steer with the same `messageId` after a lost lease; the
  runtime "keeps no record of consumed inbound ids".

## Decision

1. **The URI, the card, the activation.** `STEER_EXTENSION` is `https://agents.vymalo.com/a2a/extensions/steer/v1` (the repository's
   constants are named `*_EXTENSION`, not `*_URI`) and `ExtensionConfig::steer()` is the entry the contract shows (description "Reads a
   message sent to its running task at its next step.", optional, no parameters). `adam_ui::card_extensions()` lists it, so
   `adam-agent` and the coder do, after `mentions/v1` and before `steps/v1` and `text-stream/v1`; the coder's golden card pins it.
   Declaring it is the **host's promise** that its agent reads a message at its next step and never loses an accepted one;
   `LlmAgent` keeps it (decisions 3 to 5). The activation is the one rule of the repository (`Caller::extensions`: the URI named in
   the `A2A-Extensions` header or in `message.extensions` **and** declared by the card, an exact match), not a second one.
2. **The backend delivers to the open task.** `RuntimeTaskBackend::submit` with a `taskId`:

   | The task | The request | Answer |
   |---|---|---|
   | `submitted` or `working` | activated | `Runtime::deliver` (durable: the run's inbox in the store, so a crash or a lease that moves loses nothing); the task as it is then |
   | `submitted` or `working` | not activated | `InvalidParams` "cannot take a follow-up", **exactly as before** |
   | `completed`, `failed`, `canceled` | any | `UnsupportedOperation`, A2A's `UnsupportedOperationError` (`-32004`) |
   | `submitted` or `working`, another context | activated | `TaskNotFound` (the contract's row; today's `InvalidParams` for the other cases) |
   | unknown, or another caller's | any | `TaskNotFound` |
   | `input-required` | any | the follow-up that resumes it, as before |

   `BackendError::UnsupportedOperation(String)` is new (`#[non_exhaustive]`, class `Rejected`); `InMemoryBackend` answers it for a
   finished task too, since it is the reference of the documented semantics. A task that finishes between the read and the delivery
   (`RuntimeError::Finished`) is the same error, one that vanished is `TaskNotFound`. **The terminal case changes without the
   extension too**: a message to a finished task was `InvalidParams` and is now A2A's own error, because the specification names it
   and the contract's table says so for every request; the orchestration layer reads no code.
3. **At its next step.** Nothing new is needed here: every step drains the inbox first, so a message delivered during a step
   is read by the following one, after the result of the tool call that was running (never in the middle of it, as the contract
   asks), and a message that arrives while the run waits on a child or a remote task wakes it and is held behind the owed result.
4. **A final answer is not the last word while a message is unread.** Three layers, because each closes a different window:

   * **The agent checks.** After the model answers with no tool call, `LlmAgent` asks `Ctx::arrived()` (the messages delivered
     since the transition began: a live read of the run's record, not journaled). If there are some, the answer stays in the
     history and is said as the words of a turn that goes on (so its stream, if it was streamed, has its whole text said), the
     step returns `Continue`, and the next step reads the message and calls the model again. This is the common case, and the one
     that keeps the text-stream bookkeeping right.
   * **The runtime closes the window to the commit.** A message can arrive after that check and before the commit.
     `Ctx::reopen_on_arrival()` (which `LlmAgent::step` calls first thing) asks that a `Done` **committed past a message that
     arrived while the transition ran** be committed as a `Continue`: the run stays runnable with the state the `Done` carried
     (the output is dropped), and the next step reads the message. It is decided in the commit itself, which is a compare-and-set on
     the record that a delivery changes, so no message can slip in between a check and the commit. It is opt-in by design: a
     transition that did not ask is committed as before, and an agent that asks promises that stepping it again after a `Done` is
     harmless. `Fail`, `Park` and `Continue` are unchanged (a parked run is woken by a message that arrived, as ever).
   * **A replayed answer is checked against what was read.** The model step records `seen`, how long the history was when the
     call was made. A transition that replays it (the commit after the recording was lost) and has read a message that came in
     between sees a longer history, so the recorded answer did not read the message: it is dropped (not said, not kept), and
     the model answers again with the message in front. An answer that asks for tool calls is not dropped (the calls are valid;
     the next turn reads the message). `seen` is a serde default: a journal from before it reads as "current".

   ```mermaid
   sequenceDiagram
     autonumber
     participant O as Orchestration layer
     participant B as RuntimeTaskBackend
     participant DB as Store
     participant W as Worker (LlmAgent step)
     participant M as Model

     O->>B: SendStreamingMessage {taskId, steer/v1 activated}
     B->>DB: Runtime::deliver: the message joins the inbox (commit, version bumps)
     B-->>O: the task, working (first event)
     Note over W,M: the worker is in the final model call
     W->>M: complete or stream (the first answer)
     M-->>W: "the colour is red"
     W->>DB: Ctx::arrived: the inbox holds one message since this step began
     W->>DB: commit Continue (the answer stays in the history)
     W->>DB: next step: take the inbox, call the model with the message after the answer
     M-->>W: "the colour is blue"
     W->>DB: commit Done
   ```

   ```mermaid
   stateDiagram-v2
     [*] --> Stepping: a worker claims the run
     Stepping --> Finished: Done, and nothing arrived meanwhile
     Stepping --> Runnable: Done, but a message arrived (the commit turns it into a Continue)
     Stepping --> Runnable: Continue, or the agent saw an unread message
     Runnable --> Stepping: the next step reads the inbox first
     Finished --> [*]
   ```
5. **A message is read once.** `Conversation::read_ids` keeps the ids (`Inbound::id`, which `default_inbound` sets to the A2A
   `messageId`) of the last 128 messages the run has read, oldest first; a message whose id is there is ignored, whether it is
   still in the inbox or was read transitions ago. It is the agent's state, because the runtime keeps no record of consumed ids
   and the backend cannot know which ones were read, and it is durable like the rest of the state (another worker, a restart). An
   empty id is nobody's, so it is never remembered or matched. The bound is a trade: the oldest id is given up, which lets
   through only a repeat of a message more than 128 messages old.
6. **What is not promised.** An agent that is not an `LlmAgent` and declares the extension must do decisions 3 to 5 itself
   (`reopen_on_arrival` is public; `arrived` too). The coder's wrapper can turn a final answer into a question (`stop_as_question`):
   that is a `Park`, which a message that arrived wakes, and the steered text then answers that question; and it can fail a run on
   its verdict, which ends it as any failure does.

## Consequences

* **`Ctx` has two methods, the commit has one rule, and `Recorded` and `Conversation` each have a member** (`seen`, `read_ids`),
  all additive and defaulted: older state and journals load, and a build that ignores them reads a newer journal. `BackendError`
  has a variant (it is `#[non_exhaustive]`; a `match` with a wildcard is unaffected).
* **A final answer can cost another model turn**, only when a message arrived. The first answer was already said as working text.
* **A task that finishes with a message unread no longer exists for `LlmAgent`**; it can for a host whose agent never asked.
* **A message sent to a task that is `Parked` with a timer** (waiting on a child or a remote task) wakes the run, which looks at
  the child once and parks again; the message waits behind the owed tool result and is read when the call is answered.
* **The terminal-task error changed for every client** (`-32004` for `-32602`), see decision 2.
* **The id of the next ADR may clash** with another branch of the same wave; renumber on merge.

## Alternatives considered

* **Refuse a steer that arrives while the run is in its final model call** (the contract allows it). Rejected: the window is the
  common one, and the orchestration layer's fallback (send it after the turn) is what the person was trying to avoid.
* **Make every `Done` reopen when a message arrived**, for every agent. Rejected: it changes the contract of `Agent::step` for
  agents that never asked, and a test agent that returns `Done` while a message arrives would be stepped again. Opt-in costs one
  call.
* **Check only in the agent, without the commit rule.** Rejected: a message between the check and the commit is lost, and the
  extension promises it is not.
* **Deduplicate in the backend.** Rejected: it would have to remember consumed ids (the runtime keeps none) or read the run's
  state; the agent has them already and they follow the run.
* **Keep a stale recorded answer and add the message after it.** Rejected: the history would end with an assistant message, which
  some providers refuse, and the answer is for a question that has changed; dropping it costs one model call in a rare window.
* **A dedicated inbound kind for a steer.** Rejected: the agent reads a message the same way whatever route it came by, and the
  contract says a steer carries no metadata of its own.

## Verified and unverified

*Verified 2026-10-02* against the contract page (`docs/api/steer-v1.md` of `vymalo/another-agentic-system`, which cites the A2A
specification): the activation rule, the refusals, the first event being the task, deduplication by `messageId`, and that a task
about to finish takes another step. *Verified 2026-10-02* in this repository by the tests named in
[`adam-a2a-runtime`](../../crates/adam-a2a-runtime/README.md#steering-a-running-task) and
[`adam-llm-agent`](../../crates/adam-llm-agent/README.md#messages-sent-while-the-run-works): each of the above, on the in-memory
store and on PostgreSQL. *Unverified:* against the orchestrator's real adapter (PR-13 of plan 11 builds it), against a live model
provider, and with the coder's OpenCode and gate steps in the middle of a long tool call.
