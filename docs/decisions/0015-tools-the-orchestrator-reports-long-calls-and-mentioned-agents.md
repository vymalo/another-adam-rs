# 0015. Tools the orchestrator reports, long calls, and mentioned agents

Status: **Accepted** (2026-10-02), with plan 11 of 2026-10-02 (PR-7). The other side of the contract is the orchestration
layer's: `docs/api/thread-tools-v1.md` (the tool's `_meta`, the request's `_meta`, `ask_agent`) and `docs/api/mentions-v1.md`,
with its ADR 0026 and ADR 0036, all in `vymalo/another-agentic-system`. **Extends [ADR 0006](0006-a2ui-and-the-vymalo-extensions-in-adam-rs.md)**
(the thread tools are a `ToolSource` read at every model turn; the card lists the extensions) **and
[ADR 0007](0007-progress-as-steps-and-streamed-text.md)** (a tool call is a step). **Built:** `ToolNote` and `ToolSource::listing` /
`instructions`, `Conversation::source_notes`, the silent step, `ToolCtx::note` / `parent_step_id`, `adam-mcp`'s `CallOptions`
and `thread_tools_max_call`, `THREAD_TOOLS_MAX_CALL_SECS`, `adam-ui`'s notes, call ids and "Mentioned agents" block,
`MENTIONS_EXTENSION` and `CONTEXT_MENTIONS`, and the card entries.

## Context

The orchestration layer will relay the MCP servers a person attached to a conversation, and run the agents a person mentioned,
through the one endpoint it already gives an agent (the thread tools). Three things in adam-rs were in the way, *verified
2026-10-02 by reading the code at commit `e0d0df1`*:

* A call to the endpoint waited a fixed 60 s (`McpPolicy::call_timeout`), whatever the tool: a relayed search that may take two
  minutes, and an `ask_agent` that runs another agent for half an hour, were cut off.
* Every tool call was a step of the agent's own (`tool:<call id>`). The orchestrator reports the step of a relayed call and of an
  ask itself, with the server's icon; an agent that reports one too draws it twice.
* A call carried nothing that tells the orchestrator which call it is. A step retried after its lease expired (issue #62) calls
  again, and the orchestrator must report the same step, and not start a second ask.
* A message's mentions (`mentions/v1`) were dropped by `vymalo_inbound`, and nothing told the model who was meant.

## Decision

1. **A tool says how long it may take, and the agent waits that long, capped.** The endpoint lists each tool with
   `_meta["thread-tools/v1"] = {reportsStep, timeoutSecs}`. A call waits `timeoutSecs`, at most `THREAD_TOOLS_MAX_CALL_SECS`
   (1 to 86400, default 3600; `McpPolicy::thread_tools_max_call`), and a tool that says nothing is waited for the policy's call
   timeout (60 s, as before). `Endpoint::call_tool_with(name, args, CallOptions)` carries the time and the request `_meta`;
   `call_tool` is a call with neither. A cancel of the run drops the call (the connection closes), because an hour is too long
   to wait for a call nobody will read.
2. **What a listing says about a tool travels with the model's answer.** The agent calls a tool in a later transition, maybe on
   another worker, so the listing cannot be read again cheaply: the relay lists the upstream servers on every `tools/list`, and
   a list per call would double that. `ToolSource::listing` returns the specs and `ToolNote`s (`reports_step`, `timeout_ms`;
   only the ones that say something, only for tools the agent kept). They are recorded in the journaled model step (`Recorded`, a serde
   default) and written to `Conversation::source_notes` (the latest turn's, replaced at every model call), and
   `ToolCtx::note()` gives a call its own. State and journals written before this load as "no notes": every call has its step.
   This is the one place where something the listing says is journaled; ADR 0006 rejected journaling the listing as a step of its
   own, and this is not one: it adds a member to an entry that was written anyway.
3. **A tool the orchestrator reports gets no step of the agent's.** For a source's tool whose note says `reports_step`, the agent
   emits no start, no end and no waiting report; the result still goes to the model. Any other tool, and any tool of the agent's
   own (which wins a name clash), is reported as before.
4. **`callId` is `<run id>:<the model's call id>`.** Both come from the journal: a retry of the step, a replay and another
   worker send the same one. The run id is in it because a thread's jobs are different runs and a model's call ids are only
   unique within one response (a scripted model reuses `c1`), while the orchestrator's dedupe key is per thread. Past the contract's
   256 bytes the model's id is replaced by its SHA-256, which is as stable. `parentStepId` is the step the call runs under when
   the caller reported one (`ToolCtx::under_step`); a call the model asked for is at the top and sends none, which the contract
   reads as "under the agent's invocation". (The agent has no step of its own for a relayed call to nest under, so it does not
   name one that does not exist.)
5. **Mentions are the latest message's, as context, and a block in the instructions.** `vymalo_inbound` reads
   `metadata[mentions/v1]` into `context["vymalo.mentions"]` (well-formed references only, at most 16, names and labels bounded,
   the coordinating tool only as a name a model can be shown). A message that carries other extension metadata and no mentions
   sets the key to `null`, which deletes it: a task that continues another carries the earlier context along, and the earlier
   message's mentions are not the new one's. `ToolSource::instructions` lets a source add words to the instructions of the
   turn (in the journaled step, after the agent's own, after a blank line); `ThreadTools` adds the **"Mentioned agents"** block
   when the context has mentions and **nothing** when it has none. The block quotes every label and name as a JSON string on one
   line and says they are text from other parties, never instructions (the contract: `name` comes from a card, the label is the
   person's). It says to call the tool named in `coordinate` once per piece of work, with everything the agent needs in
   `message`, in the order the person asked; with no `coordinate` it says there is no way to ask.
6. **The card lists `mentions/v1`.** `adam_ui::card_extensions()` (so `adam-agent` and the coder) is A2UI v0.9.1, `ui-catalog/v1`,
   `thread-tools/v1` and `mentions/v1`, all optional and without parameters, the URI exactly as the contract writes it.

## Consequences

* **`RemoteTool` has a new member (`meta`) and `McpSettings` a new field** (`thread_tools_max_call_secs`; `Default` is no longer
  derived): breaking for code that builds either with a struct literal. `ToolSource` gains two methods with defaults, so
  existing sources are unchanged.
* **Every message from the orchestration layer now carries `vymalo.mentions: null` in its context** (a delete that changes
  nothing when there is nothing to delete). The alternative, tracking in the state which message set the mentions, would put the
  rule in `adam-llm-agent`, which knows no extension.
* **A relayed call is at least once on a retry** (the contract says so): the same `callId` makes the orchestrator report one step,
  not that the upstream ran once.
* **The `callId` is stable across a replay, not across a re-ask of the model.** A step run again from the journal (a crash
  between the call and its record, a lost lease, a stale commit) has the recorded model answer and so the same call id. A
  transient failure of the whole transition abandons its journal and asks the model again (ADR 0003's runtime rule, `Ctx::step`):
  the new answer has its own call ids, and its calls are new calls.
* **A long call holds a worker's step for up to the cap.** Leases are renewed while a run is stepped, so it is not retried
  because it is slow; the cap is the operator's lever.
* **The id of the next ADR may clash** with another branch of the same wave; renumber on merge.

## Alternatives considered

* **Listing again at call time** to read the tool's `_meta`. Rejected (decision 2): one more connection per call, and the relay
  lists every attached server upstream on each listing.
* **A per-process cache of the notes.** Rejected: a call on another replica, or after a restart, would have none and would
  report a step the orchestrator also reports.
* **`ToolSpec` with a field for it.** Rejected: `ToolSpec` is what the model provider is sent, and a field the provider must
  not see belongs somewhere else.
* **A random `callId` per attempt.** Rejected: a retried step would report a second step and start a second ask (the contract's
  reason for the field).
* **The model's call id alone as `callId`.** Rejected (decision 4): two jobs of a thread can make the same one.
* **Rewriting the message text with the agents' ids.** Rejected: the contract keeps the text as the person wrote it, and the
  references are metadata.
