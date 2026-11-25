# adam-plugin

The vocabulary of the hooks around the agent loop ([ADR 0050](../../docs/decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md),
slices **H1** and **H1b**): a compiled plugin may allow, rewrite or refuse a person's message before
it reaches a model, rewrite a tool call before it runs, refuse it, ask a person, stop the run, or
rewrite what the tool answered.

`adam-llm-agent` wires a set into the loop: `LlmAgentBuilder::hooks(HookSet)` (or `.plugin(one)`) runs
`message_received` in the step that takes a message in (a step of its own, taken only when a set is
installed), and `before_tool` and `after_tool` **inside** the call's journaled step `tool:<call id>`,
recording the verdict in that step, so a replay that finds it recorded asks no hook again. A hook gets
no `Ctx` and cannot add a step of its own. What a call's step records is `adam_llm_agent::HookedOutput`
(`output`, flattened, plus `hooks` and the rewritten `args`), and what a message's step records is
`MessageDecision` (`text` when a hook rewrote it, `hook` when it was refused); a record written before
hooks existed is a bare output and still decodes. This crate is only the vocabulary: it depends on
neither the loop nor the runtime.

## The API

| Item | What it is |
|---|---|
| `Plugin` | `name`, `message_received`, `model_request` (inside the `model:<turn>` step: it may rewrite `temperature`, `max_output_tokens` and `metadata` of the call, or `Stop` the run; a replay asks none), `before_tool` and `after_tool`, each with a do-nothing default, so a plugin that cares about one point implements one method |
| `Verdict` | `Continue`, `Deny(reason)`, `Ask(reason)`, `Stop(reason)`: the first verdict other than `Continue` is the set's, and a later plugin is not asked |
| `HookError` | the plugin's name, an `adam-error` class and a reason (never an argument or a tool's answer); `is_retryable` follows the class |
| `HookLimits` | the deadline of one hook (1 s by default) and how many times a retryable failure is tried again (1) |
| `HookSet` | the ordered plugins and their limits; `message_received`, `before_tool` and `after_tool` run them, `catch_unwind` each call, and stop at the first decision |
| `HookedCall`, `HookedText` | what a hook sees: the tool's name and its arguments (rewritable), and the text the model will read (rewritable) |
| `HookedMessage`, `MessageFile` | what a `message.received` hook sees: the text (rewritable) and the files **by reference** (name, media type, size, the SHA-256 the image step records), never their bytes |

## The rules

- **Fail closed.** A hook that errors, panics or misses its deadline never lets the call or the message
  through: the set answers a `HookError` and the caller refuses. A panic is `Invalid` (a buggy plugin is
  not unlucky); a deadline is `Transient`.
- **A transient failure is retried** `HookLimits::retries` times in the same step, then refused.
- **Order is declaration order**, and a decision short-circuits the rest of the set.

## Tests

`tests/conformance.rs` runs `adam_plugin_testkit::plugin_conformance!` (11 cases) against a set with a
20 ms deadline and one retry: an empty set continues, plugins run in order and the first verdict
decides, `Ask` and `Stop` are verdicts, a rewrite is visible, a panic is `Invalid`, an `Invalid` hook
denies, a transient failure is retried once then continues, one that keeps failing denies, a hook past
its deadline is retryable, `after_tool` runs its plugins, and `message_received` runs its plugins and a
refusal decides. The panic case was **seen red** with `catch_unwind` removed (the panic escaped into
the test).

`Audit<W: Write + Send>` (ADR 0050, slice H6b): a plugin that writes each observed event as one JSON line (the event the loop built, already bounded and scrubbed) to `W`, once per event key, so a replay adds nothing. Off unless a deployment registers it and an agent file names it in `plugins:` (H6a); a failing writer is a counted observer failure. Tests: `tests/audit.rs` (golden `tests/golden/audit_lines.json`, written with `UPDATE_GOLDEN=1`).
