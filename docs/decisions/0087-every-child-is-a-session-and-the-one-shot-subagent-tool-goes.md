# 0087. Every child is a session, and the one-shot subagent tool goes

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"I don't like the idea of the parent waiting for the
one-shot. [...] We should remove the one-shot thing; a normal child session can also be one-shot. We can simply have
sealed sub-agents, where the user cannot talk to the child, or with other specs. I mean, while creating the sub-agent,
the agent has config options."* Decisions 2 (the defaults of `wait` and `mode`), 3 (who sees a sealed session) and 6
(one release of a wrapper) are this ADR's choices for the owner to confirm. Reverses the one-shot child runs of
[ADR 0039](0039-the-agent-loop-takes-opencodes-structure-and-agents-get-plugins.md); amends
[ADR 0045](0045-a-subagent-can-run-in-the-background-as-a-session.md),
[ADR 0054](0054-a-child-session-can-be-configured-within-what-its-folder-allows.md) and
[ADR 0037](0037-parallel-subagent-calls-and-an-editing-worker.md); serves
[ADR 0085](0085-runs-are-exported-as-training-records.md).

*Amended 2026-11-27, the owner:* *"We'll stay only with sub-agents with sessions, and have one option for sealed or
one-shot sub-agents while creating them."* Decision 2's `access` and ADR 0045's `keep_open` overlapped (a session that
closes after its round is already sealed), so they are **one option, `mode`**, below; `keep_open` is read for one
release and mapped onto it. The default `one_shot` is this ADR's choice for the owner to confirm.

*Amended 2026-11-27, slice C1 built:* a local subagent's own tool starts its child as round 1 of a session of the parent
(`ToolCtx::start_one_shot_session`, which shares `subagent_start`'s header and start path), under the same derived id,
and the parent parks on it as before: the same tool, schema, description, time limit and result for the model. The
internal equivalent of `mode: one_shot` is a header **closed from its start**, and of `wait: true` the call's
`ToolError::AwaitRun`; no tool parameter changed, and `keep_open` is as it was. A person sees the child (decision 3) in
`subagent_list` and `child-sessions/v1`, working and then closed, and cannot write to it: a message to its context was
refused already, and a steer naming its round is refused now, for every closed session (an answer to a question and a
resume still pass). Where C1 differs from decision 6: **a remote subagent's own tool is unchanged**, since it calls the
remote from the parent's step and has no child run to give a conversation to (C2); no session limit applies to the
wrapper, as none applied to the tool; a child a journal started before C1 (no conversation) is found by the call; and
the child reads the workspace of the run that calls it, as a `subagent_start` session does, which differs from before
only for a child of a person's round. Each such child is a round the session listings scan, which counts towards their
bound of 5,000 rounds of one subagent in one conversation.

## Context

An agent has two ways to use a subagent today (*verified 2026-11-27*):

| | The subagent's own tool (one-shot) | A child session |
|---|---|---|
| Tool | one per subagent, named after it, `{ "message": string }` (`SubagentTool`, `crates/adam-assembly/src/subagent.rs`) | `subagent_start`, `subagent_send`, `subagent_read`, `subagent_close`, `subagent_list` (`crates/adam-llm-agent/src/sessions.rs`) |
| Parent | parks until the child ends (`ToolError::AwaitRun`) | keeps working; reads reports |
| After the first answer | nobody can reach the child | the parent and the person send more rounds |
| Conversation id | none (`Runtime::start_child`: "it has no conversation") | `<parent>/s/<session>` |
| Options | the folder's limits only | `keep_open`, `config` (ADR 0054) |

So the same thing has two shapes, the one-shot shape blocks the parent, and its child cannot be followed up. Because it
has no conversation id, it is not listed by conversation, so the exporter of ADR 0085 cannot reach its trajectory.

## Decision

1. **Every child is a session.** A subagent is started only with `subagent_start`, and every child run has a session's
   conversation id. A one-shot use is a session that closes after its first round (`mode: one_shot`, as today's
   default).
2. **`subagent_start` takes two options, chosen per call** (amended 2026-11-27):
   * `mode`, a closed enum (Rule 3), the one option the owner asked for. It replaces `keep_open`:

     | Value | After the first round | Who may send the next one |
     |---|---|---|
     | `one_shot` (default) | the session closes | nobody |
     | `sealed` | the session stays open | the parent only: the person cannot write to it |
     | `open` | the session stays open | the parent and the person |

   * `wait` (boolean, default `false`): the call returns the session's first report as its result, and the parent parks
     until then, as a tool call does. Waiting is the parent's choice for one call, not a kind of child.
3. **A person always sees a child,** whatever its `mode`: its rounds, reports and status, as `child-sessions/v1`
   shows them today. `mode` limits who writes, never who reads.
4. **A subagent's file sets the defaults and the ceiling.** The file gains optional `session:` defaults (`mode`,
   `wait`). A call may only narrow what the file allows, by ADR 0054's rule (`open` is wider than `sealed`, which is
   wider than `one_shot`): a subagent filed `mode: sealed` cannot be opened to the person by its parent.
5. **Several children run side by side.** Two `subagent_start` calls in one turn start two sessions at once. That
   subsumes ADR 0037's parallel subagent calls. A remote subagent (A2A) takes the same options, as remote sessions do
   today.
6. **The subagents' own tools become wrappers for one release, then go.** In slice C1 each named tool calls
   `subagent_start` with `mode: one_shot` and `wait: true`. That is today's behaviour exactly,
   plus a conversation id. Slice C4 removes them, and an agent file that names one in `tools:` then refuses to load
   with a message naming `subagent_start`.

## Slices

| Slice | What |
|---|---|
| C1 | Every child gets a session's conversation id; the named subagent tools wrap `subagent_start`. ADR 0085's exporter reaches every child. |
| C2 | `mode` and `wait` on `subagent_start`, local and remote; `keep_open` read for one release and mapped onto `mode`; the person's write is refused by `mode`; tests for each value. |
| C3 | `session:` defaults in a subagent's file, narrowed by ADR 0054's rule; `docs/reference/agent-files.md`. |
| C4 | The named tools go; the coder's instructions, the dev agents and the parity page use `subagent_start`; `child-sessions/v1` carries `mode`. |

## Consequences

* One shape for a child: the model learns one set of tools, and a training set teaches one way to delegate.
* The tool list changes once (C4), which moves the prompt's stable prefix once.
* `child-sessions/v1` gains a member, and the orchestration layer's ADR in vymalo/another-agentic-system follows it.
* `Runtime::start_child` stays for hosts that start plain child runs. The assembly stops using it for subagents.
* No `Store` or `Notifier` method changes.

*Amended 2026-11-28, slice C2 built:* `subagent_start` takes `mode` (`one_shot`, the default, `sealed` or `open`) and `wait`
(default `false`), for local and remote subagents alike. `mode` is a closed enum, `adam_runtime::SessionMode`, and the
session's header holds it (`SessionHeader::mode`) in place of `keep_open`. A header of before reads as what its `keep_open`
meant (`true` is `open`, `false` is `one_shot`), and a header of now still writes `keep_open` beside `mode` for one release,
so a worker of before reads it (to such a worker a `sealed` session is only kept open). C1's header is `one_shot` and
`closed` from its start: unlike a `subagent_start` with `mode: one_shot`, nobody can steer its round, not even the parent. A
call's `keep_open` is read for one release and mapped the same way; beside a `mode` that disagrees it is an error result
naming both. **Who may write:** the parent may send to a `sealed` session; a person's message to an agent's session that is
not `open`, and their steer of its round, are refused (`UnsupportedOperation`, naming the mode), on the A2A front's three
paths, a message to the child context, a steer by the round's `taskId` and `steer/v1`. The person may still close it,
cancel a round, answer a question and resume a parked round. Only an `open` round may ask the person. Where C2 differs from or
adds to the decision:

* **A person's own session is theirs whatever its mode**: it names no root run, so the refusal does not apply to it (and a
  header written before `root`, which names none either, reads as a person's). A person's `start` takes `mode` `one_shot` or
  `open`; `sealed` is refused, since it would leave the session to an agent that does not exist. So `one_shot` refuses a
  person's steer of an agent's working round, which it did not before C2: `one_shot` is narrower than `sealed` (decision 4).
* **`wait: true` reuses `ToolError::AwaitSession`**, not `AwaitRun`: the call parks as `subagent_read` with `wait: true`
  does, and its result is that read's first page, whose header line names the session, so the model can send a `sealed` or
  `open` session more. The report is counted as read and is never told again as a report. Observers see `session.started`
  at the park and `session.round.ended` at the settle (ADR 0050, amended the same day).
* **`child-sessions/v1` carries `mode`** on every session object, the parent's `sessions` rows, the events and the round
  stream, with `keepOpen` beside it, so a client knows whether the person may write. This was C4's in the table of slices;
  it is additive, and it is built here.
* **A remote subagent:** a remote session honours `mode` and `wait` as a local one does, because its rounds are local runs
  carrying the header; the remote agent is told nothing of the mode. **The remote subagent's own tool is unchanged**: it
  calls the remote from the parent's step and starts no child run, so it has no session or conversation id. It shares a
  `files: true` answer as the parent's artifacts, which a round does not, and a round agent exists only for a root that has
  session tools. C4, which removes the tool, replaces it with `subagent_start` with `mode: one_shot` and `wait: true`.
* The tool's description and schema changed once (`mode` and `wait` in, `keep_open` out of the schema), so the prompt's
  stable prefix moved once; `subagent_list` says `mode=<word>` where it said `keep_open=<bool>`.

Proof: `crates/adam-llm-agent/tests/sessions.rs` (`each_mode_is_kept_in_the_header_and_decides_what_follows_the_round`,
`a_start_that_waits_answers_its_first_report_once`, `keep_open_maps_onto_mode_and_a_disagreement_is_refused`,
`a_model_started_session_asks_only_when_the_client_shows_child_sessions`), `crates/adam-a2a-runtime/tests/child_sessions.rs`
(`a_sealed_or_one_shot_session_of_the_agent_refuses_the_persons_message_and_steer`),
`crates/adam-assembly/tests/remote_sessions.rs` (`a_remote_session_takes_mode_and_wait_as_a_local_one_does`) and the units
of `crates/adam-runtime/src/session.rs` and `crates/adam-a2a-runtime/src/child_sessions.rs`.
