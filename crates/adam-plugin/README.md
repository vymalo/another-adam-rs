# adam-plugin

The vocabulary of the hooks around the agent loop ([ADR 0050](../../docs/decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md),
slice **H1**, first half): a compiled plugin may rewrite a tool call before it runs, refuse it, ask a
person, stop the run, or rewrite what the tool answered. **Nothing is wired into the loop yet**: this
crate is the types, and part 2 (the deciding points inside the journaled steps, the verdicts recorded
so a replay asks no hook again) follows.

## The API

| Item | What it is |
|---|---|
| `Plugin` | `name` plus `before_tool` and `after_tool`, each with a do-nothing default, so a plugin that cares about one point implements one method |
| `Verdict` | `Continue`, `Deny(reason)`, `Ask(reason)`, `Stop(reason)`: the first verdict other than `Continue` is the set's, and a later plugin is not asked |
| `HookError` | the plugin's name, an `adam-error` class and a reason (never an argument or a tool's answer); `is_retryable` follows the class |
| `HookLimits` | the deadline of one hook (1 s by default) and how many times a retryable failure is tried again (1) |
| `HookSet` | the ordered plugins and their limits; `before_tool` and `after_tool` run them, `catch_unwind` each call, and stop at the first decision |
| `HookedCall`, `HookedText` | what a hook sees: the tool's name and its arguments (rewritable), and the text the model will read (rewritable) |

## The rules

- **Fail closed.** A hook that errors, panics or misses its deadline never lets the call through: the
  set answers a `HookError` and the caller denies. A panic is `Invalid` (a buggy plugin is not
  unlucky); a deadline is `Transient`.
- **A transient failure is retried** `HookLimits::retries` times in the same step, then denied.
- **Order is declaration order**, and a decision short-circuits the rest of the set.

## Tests

`tests/conformance.rs` runs `adam_plugin_testkit::plugin_conformance!` (10 cases) against a set with a
20 ms deadline and one retry: an empty set continues, plugins run in order and the first verdict
decides, `Ask` and `Stop` are verdicts, a rewrite is visible, a panic is `Invalid`, an `Invalid` hook
denies, a transient failure is retried once then continues, one that keeps failing denies, a hook past
its deadline is retryable, and `after_tool` runs its plugins. The panic case was **seen red** with
`catch_unwind` removed (the panic escaped into the test).
