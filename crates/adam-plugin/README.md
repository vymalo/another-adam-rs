# adam-plugin

The vocabulary of the hooks around the agent loop ([ADR 0050](../../docs/decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md),
slices **H1**, **H1b** and **H5**): a compiled plugin may allow, rewrite or refuse a person's message before
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
| `Plugin` | `name`, `message_received`, `model_request` (inside the `model:<turn>` step: it sees `kind` — `Turn` or `Compaction` — and may rewrite `temperature`, `top_p`, `max_output_tokens`, `metadata`, `headers` and an `extra_body` of the call, or `Stop` the run; a header whose name looks like a credential or a body member the client owns fails the run; a replay asks none), `compaction_before` (inside the `compact:` step: it may add `context` to the summary request, or `Deny` it and the history is shortened instead), `run_ending` (`Deny` keeps the run going with the reason), `before_tool` and `after_tool`, each with a do-nothing default, so a plugin that cares about one point implements one method |
| `Verdict` | `Continue`, `Deny(reason)`, `Ask(reason)`, `Stop(reason)`: the first verdict other than `Continue` is the set's, and a later plugin is not asked; **except `Ask` at `before_tool`** (slice H8): the plugins after it are still asked, a later `Deny`, `Stop` or failure wins, and of two asks the first question stands |
| `HookError` | the plugin's name, an `adam-error` class and a reason (never an argument or a tool's answer); `is_retryable` follows the class |
| `HookLimits` | the deadline of one hook (1 s by default) and how many times a retryable failure is tried again (1) |
| `HookSet` | the ordered plugins and their limits; `message_received`, `before_tool` and `after_tool` run them, `catch_unwind` each call, and stop at the first decision (at `before_tool`, the first `Deny` or `Stop`: an `Ask` lets the rest decide) |
| `HookedCall`, `HookedText` | what a hook sees: the tool's name and its arguments (rewritable), and the text the model will read (rewritable) |
| `HookedModelRequest`, `ModelCallKind`, `HookedCompaction` | what a `model.request` hook sees (the call's kind, temperature, output bound and metadata) and what a `compaction.before` hook sees (the `context` it may add to the summary request). `HookedModelRequest` gained the public field `kind` (2026-11-26), so a hand-built literal needs it |
| `EventSpec`, `Visibility` (H5) | a custom event as it is declared: `EventSpec::new("demo.published")` is version 1, any data, `Visibility::Hooks`; `.published()` makes it `Visibility::Clients` (it also goes out as a `RunEvent::Custom`), `.with_version(n)`, `.with_schema(json)`. The name is `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*){1,4}$` and its first segment, the namespace, is not one of `RESERVED_NAMESPACES` (`run`, `message`, `turn`, `model`, `tool`, `permission`, `question`, `approval`, `compaction`, `child`, `session`, `usage`, `artifact`, `exec`, `hook`, `adam`; compared without regard to case). `EventSpec::validate`, `check_event_name`, `declared` (every declaration valid, two of a name equal) and `DeclarationError` (class `Invalid`) are the checks the agent's build runs. `DATA_MAX` (4 KiB), `STEP_EVENTS_MAX` (16) and `TRANSITION_EVENTS_MAX` (256) are the bounds `adam-llm-agent` holds an emit to |
| `Subscriptions` (H5) | what `Plugin::subscriptions` says an observer wants: `core()` (the default: every core event and no custom one, which is what an observer received before custom events), `none()`, `all()` (`*`), `of(["demo.*", "tool.completed"])` (an exact name, `ns.*` or `*`; no core event unless a pattern matches it) and `.with(pattern)`. `Subscriptions::check` and `HookSet::check_events` refuse a **dead** pattern at build: a namespace that is neither core nor declared, or an exact core name that is not in the catalogue. A subscription to a core name is legal, a declaration there is not. `Plugin::events` declares custom events for the namespaces and the conflict check |
| `HookEvent::custom`, `EventKind::Custom` (H5) | a custom event as an observer reads it: `kind` is `Custom`, `HookEvent::name()` the declared name, the fields `version` and `data` (scrubbed, held to the JSON bound). `EventKind::ALL` lists the core catalogue |
| `HookedMessage`, `MessageFile` | what a `message.received` hook sees: the text (rewritable) and the files **by reference** (name, media type, size, the SHA-256 the image step records), never their bytes |

## The rules

- **Fail closed.** A hook that errors, panics or misses its deadline never lets the call or the message
  through: the set answers a `HookError` and the caller refuses. A panic is `Invalid` (a buggy plugin is
  not unlucky); a deadline is `Transient`.
- **A transient failure is retried** `HookLimits::retries` times in the same step, then refused.
- **Order is declaration order**, and a decision short-circuits the rest of the set. The one exception is
  an `Ask` at `before_tool` (slice H8): a call a person approves runs, so every plugin after the one that
  asked has its say, and a refusal among them wins. `adam-llm-agent` parks the run on the question;
  `approval.asked` and `approval.answered` are the two `EventKind`s an observer reads about it.

## Tests

`tests/conformance.rs` runs `adam_plugin_testkit::plugin_conformance!` (12 cases) against a set with a
20 ms deadline and one retry: an empty set continues, plugins run in order and the first verdict
decides, `Ask` and `Stop` are verdicts, an `Ask` lets the plugins after it refuse (H8), a rewrite is visible, a panic is `Invalid`, an `Invalid` hook
denies, a transient failure is retried once then continues, one that keeps failing denies, a hook past
its deadline is retryable, `after_tool` runs its plugins, and `message_received` runs its plugins and a
refusal decides. The panic case was **seen red** with `catch_unwind` removed (the panic escaped into
the test).

`events_conformance!` (`adam-plugin-testkit`, slice H5) runs against the same factory: what the set's plugins declare is valid and agrees with itself, every subscription can match something, and an observer is offered a custom event only when its subscriptions name it. `src/custom.rs` has the unit tests of the name rule (the core namespaces, the case of the first segment, a core word deeper in the name), the patterns, the default and the dead subscription. **Seen red:** with the reserved-namespace check removed, `a_core_namespace_is_refused_on_the_first_segment_without_regard_to_case` and `a_subscription_to_a_core_event_is_legal_though_a_declaration_there_is_not` fail.

`Audit<W: Write + Send>` (ADR 0050, slice H6b): a plugin that writes each observed event as one JSON line (the event the loop built, already bounded and scrubbed) to `W`, once per event key, so a replay adds nothing. Off unless a deployment registers it and an agent file names it in `plugins:` (H6a); a failing writer is a counted observer failure. Tests: `tests/audit.rs` (golden `tests/golden/audit_lines.json`, written with `UPDATE_GOLDEN=1`).
