# 0087. Every child is a session, and the one-shot subagent tool goes

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"I don't like the idea of the parent waiting for the
one-shot. [...] We should remove the one-shot thing; a normal child session can also be one-shot. We can simply have
sealed sub-agents, where the user cannot talk to the child, or with other specs. I mean, while creating the sub-agent,
the agent has config options."* Decisions 2 (the defaults of `wait` and `access`), 3 (who sees a sealed session) and 6
(one release of a wrapper) are this ADR's choices for the owner to confirm. Reverses the one-shot child runs of
[ADR 0039](0039-the-agent-loop-takes-opencodes-structure-and-agents-get-plugins.md); amends
[ADR 0045](0045-a-subagent-can-run-in-the-background-as-a-session.md),
[ADR 0054](0054-a-child-session-can-be-configured-within-what-its-folder-allows.md) and
[ADR 0037](0037-parallel-subagent-calls-and-an-editing-worker.md); serves
[ADR 0085](0085-runs-are-exported-as-training-records.md).

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
   conversation id. A one-shot use is a session that closes after its first round (`keep_open: false`, as today's
   default).
2. **`subagent_start` gains two options, chosen per call:**
   * `wait` (boolean, default `false`): the call returns the session's first report as its result, and the parent parks
     until then, as a tool call does. Waiting is the parent's choice for one call, not a kind of child.
   * `access`, a closed enum (Rule 3), default `parent_and_person`:

     | Value | Who may send a round after the first |
     |---|---|
     | `parent_and_person` | the parent and the person |
     | `parent` | the parent only: the person cannot write to it |
     | `sealed` | nobody: one round, then closed (implies `keep_open: false`) |
3. **A person always sees a child,** whatever its `access`: its rounds, reports and status, as `child-sessions/v1`
   shows them today. `access` limits who writes, never who reads.
4. **A subagent's file sets the defaults and the ceiling.** The file gains optional `session:` defaults (`keep_open`,
   `wait`, `access`). A call may only narrow what the file allows, by ADR 0054's rule: a subagent filed
   `access: parent` cannot be opened to the person by its parent.
5. **Several children run side by side.** Two `subagent_start` calls in one turn start two sessions at once. That
   subsumes ADR 0037's parallel subagent calls. A remote subagent (A2A) takes the same options, as remote sessions do
   today.
6. **The subagents' own tools become wrappers for one release, then go.** In slice C1 each named tool calls
   `subagent_start` with `wait: true`, `keep_open: false` and `access: sealed`. That is today's behaviour exactly,
   plus a conversation id. Slice C4 removes them, and an agent file that names one in `tools:` then refuses to load
   with a message naming `subagent_start`.

## Slices

| Slice | What |
|---|---|
| C1 | Every child gets a session's conversation id; the named subagent tools wrap `subagent_start`. ADR 0085's exporter reaches every child. |
| C2 | `wait` and `access` on `subagent_start`, local and remote; the person's write is refused by `access`; tests for each value. |
| C3 | `session:` defaults in a subagent's file, narrowed by ADR 0054's rule; `docs/reference/agent-files.md`. |
| C4 | The named tools go; the coder's instructions, the dev agents and the parity page use `subagent_start`; `child-sessions/v1` carries `access`. |

## Consequences

* One shape for a child: the model learns one set of tools, and a training set teaches one way to delegate.
* The tool list changes once (C4), which moves the prompt's stable prefix once.
* `child-sessions/v1` gains a member, and the orchestration layer's ADR in vymalo/another-agentic-system follows it.
* `Runtime::start_child` stays for hosts that start plain child runs. The assembly stops using it for subagents.
* No `Store` or `Notifier` method changes.
