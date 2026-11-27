# 0086. The context window follows the model, and the history budget is 200,000 tokens

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"No user, while using the agent locally, will do
export MODEL_CONTEXT_WINDOW. Normally, it should be removed and left to be configured by model. I'll rename it
DEFAULT_MODEL_CONTEXT_WINDOW, only used if nothing is specified. And today, we have models capable by default of 400k.
So 100k is too small. Should be raised to 200k."* Decisions 2 (the deprecated alias and the conflict rule) and 5 are this
ADR's choices for the owner to confirm. Amends [ADR 0032](0032-usage-per-model-call.md) (where the window came from)
and builds on [ADR 0064](0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md) (model profiles) and
[ADR 0054](0054-a-child-session-can-be-configured-within-what-its-folder-allows.md) (a session's `context_window`).

## Context

The history budget decides when a run compacts. It is the smaller of two values
(`budget` in `crates/adam-llm-agent/src/compaction.rs`, *verified 2026-11-27*):

* the model's context window, less 20,000 tokens and the request's output cap;
* the agent's `limits.max_history_tokens`.

Three facts made that budget wrong for current models:

* **The window is one environment variable.** `MODEL_CONTEXT_WINDOW` gives the window of the one alias `MODEL` names
  (`crates/adam-service/src/config.rs`). A subagent whose `model:` names another alias has no window. Nobody running
  an agent locally sets the variable, and the chart leaves `config.modelContextWindow` unset by default.
* **The model profiles document already has a place for it.** A profile may carry `contextWindow`, planned by ADR 0054
  slice 11, but the loader ignores it (`crates/adam-model/src/profiles.rs`).
* **The default budget is 100,000 tokens**: the `LlmAgent` default (`crates/adam-llm-agent/src/agent.rs`) and the
  coder's folder (`bin/adam-coder/agent/instructions.md`, `limits.max_history_tokens: 100000`). Models with a
  400,000-token window are compacted at a quarter of it.

## Decision

1. **A model profile's `contextWindow` is the model's window.** The profiles loader reads it (1 to 2^53 − 1 tokens;
   anything else refuses the document, as for `maxOutputTokensLimit`). A call's window is its model alias's profile
   window, for the root and for every subagent, so a subagent on another alias has its own window.
2. **`DEFAULT_MODEL_CONTEXT_WINDOW` is used only when the model's profile names none.**
   * `MODEL_CONTEXT_WINDOW` is kept as a **deprecated alias** for one minor release: read when the new variable is
     unset, with one warning at startup.
   * Both set to different values is a configuration error (exit 78): two answers to one question fail closed.
   * The chart's `config.modelContextWindow` renders `DEFAULT_MODEL_CONTEXT_WINDOW`. That changes what the chart
     renders for that value, which another-agentic-platform's parity goldens read: the pull request says so.
3. **The resolved window is used everywhere the variable was:** the compaction budget, the clamp of the output cap
   (ADR 0064), and each `usage/v1` call report's `contextWindow` (ADR 0032), now per alias.
4. **The default history budget is 200,000 tokens**: the `LlmAgent` default when `limits.max_history_tokens` is
   omitted, and the coder's folder. The rule of the context stays: the smaller of that and the window's budget, so a
   known smaller window still wins, and `0` still means "the window's budget alone".
5. **An unknown window keeps working.** With no profile window and no default, the budget is 200,000 tokens, and a
   provider's "prompt too long" rejection still compacts and retries (the overflow path). A model served with a small
   window (a local model on a small GPU) should carry `contextWindow` in its profile, or each long run pays one
   rejected call before it compacts.
6. **A session's `config.context_window` (ADR 0054) narrows the resolved window**, as it narrows the model's today.

## Slices

| Slice | What |
|---|---|
| W1 | The profiles loader reads `contextWindow`; the window is resolved per alias; compaction, the output cap and `usage/v1` use it. |
| W2 | `DEFAULT_MODEL_CONTEXT_WINDOW`, the deprecated alias and its conflict rule; the chart renders the new name; the goldens and the platform's parity note. |
| W3 | The 200,000-token default in `LlmAgent` and the coder's folder; the docs (`docs/reference/environment.md`, `docs/reference/agent-files.md`, the architecture's compaction section). |

## Consequences

* A deployment configures the window where it configures the model: in the profiles document, one entry per model.
* Runs compact half as often at the default, and each call can carry up to twice the history. With a provider's
  prompt cache most of that is cache reads, but a call without a cache costs more.
* A deployment that set `MODEL_CONTEXT_WINDOW` keeps working for a release and is told to rename it.
* No `Store`, `Notifier` or `ModelClient` method changes.
