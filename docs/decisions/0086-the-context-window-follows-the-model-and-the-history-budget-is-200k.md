# 0086. The context window follows the model, and the history budget is 200,000 tokens

Status: **Accepted** (2026-11-27). The owner decided on 2026-11-27: *"No user, while using the agent locally, will do
export MODEL_CONTEXT_WINDOW. Normally, it should be removed and left to be configured by model. I'll rename it
DEFAULT_MODEL_CONTEXT_WINDOW, only used if nothing is specified. And today, we have models capable by default of 400k.
So 100k is too small. Should be raised to 200k."* Decisions 2 (the deprecated alias and the conflict rule) and 5 are this
ADR's choices for the owner to confirm. Amends [ADR 0032](0032-usage-per-model-call.md) (where the window came from)
and builds on [ADR 0064](0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md) (model profiles) and
[ADR 0054](0054-a-child-session-can-be-configured-within-what-its-folder-allows.md) (a session's `context_window`).

*Amended 2026-11-27:* **slice W1 is built.** A profile's `contextWindow` is read (1 to 2^53 − 1, else the document is
refused, as a cap of `0` is) and is the window of its alias over `MODEL_CONTEXT_WINDOW`, which is unchanged otherwise.
The one rule is `adam_model::resolve_context_window`: every `ModelClient` answers `context_window` through it, the
trait's default included (it now answers the profile's window where it answered `None`; no method is added), so the
compaction budget, the clamp of the output cap and each `usage/v1` report read the same window per alias, and a
subagent on another alias has its own. A preset sets no window: it is a fact of the model, and presets carry no limit
of the model either. `MAX_CONTEXT_WINDOW` moved to `adam-model` (`adam-service` re-exports it). The people's pages
named under W3 (`docs/reference/environment.md`, `docs/reference/agent-files.md`, the architecture's compaction
budget) already say that the profile's window wins; W3 still owns the 200,000 default. One limit, not decided here:
`EngineBuilder::models` gives the profiles to the assembly only, so an embedded host gives its client the same
profiles for their windows to apply.

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

*Amended 2026-11-28:* **slice W2 is built.** `DEFAULT_MODEL_CONTEXT_WINDOW` is read with the model
(`crates/adam-service/src/config.rs`, 1 to 2^53 − 1, else exit 78) and is the window of the alias `MODEL` names when
its profile has no `contextWindow`, as `MODEL_CONTEXT_WINDOW` was. `MODEL_CONTEXT_WINDOW` is the deprecated alias:
read only when the new variable is unset, and one warning at startup names the new variable whenever it is set. Both
set to the same value (compared as numbers) is that value with the warning; to different values one problem names
both variables and neither value (exit 78); a value one of them refuses is that variable's problem, not a conflict.
The chart's `config.modelContextWindow` renders `DEFAULT_MODEL_CONTEXT_WINDOW` and is refused beside either name in
`config.extraEnv`; no golden of the chart sets the value, so none changed, and the platform's parity note is
[gap 35](../reference/parity.md#gaps) (its operator renders the old name). The compose files moved to the new name;
`bench/` keeps the old one until its pinned image is newer than this slice. Two limits, not decided here: an image
built before W2 reads only the old name, so the chart and the image move together (as `image.tag` does); and the
alias has no removal date beyond "one minor release", which a later amendment names.

*Amended 2026-11-28:* **slice W3 is built.** The default is one constant, `adam_llm_agent::DEFAULT_MAX_HISTORY_TOKENS`
(200,000): `Limits::default()`'s, so a folder that omits `limits.max_history_tokens` takes it. The coder's folder states
`max_history_tokens: 200000` rather than omitting the key, so its budget is read beside its other limits and a later
change of the library's default does not move the coder. The budget's rule is unchanged (the smaller of the window's
budget and the limit; `0` the window's alone). One choice **for the owner to confirm**: the output cap's clamp
([ADR 0064](0064-model-profiles-reasoning-effort-and-output-cap-follow-the-model.md) decision 4) now reserves at most
half the window for the history, for every model, where it reserved the whole `max_history_tokens` for the agent's own
model. Without it the 200,000 default leaves one output token on any window up to 220,000 (the dev stack's 131,072, a
200,000-token model); with it a 200,000 window leaves 80,000 and 131,072 leaves 45,536. Decision 4's "a known smaller
window still wins" is the reason, and it applies to a folder's explicit value over half the window too. The people's
pages (`docs/reference/agent-files.md`, the architecture's compaction section) say 200,000.

*Amended 2026-11-28:* **the engine's gap in W1 is closed.** `EngineBuilder::models(profiles)` now reaches the model
client too: `start` wraps the host's client (`crates/adam-engine/src/profiled.rs`) so that, for an alias the client has
no profile of, its window is `resolve_context_window(the engine's profile, the client's own answer)` (the one rule),
and the history budget, the output cap's clamp and each `usage/v1` report read the profile's window. An embedded host
no longer gives its client the same profiles. A client that has its own profile of an alias keeps it whole, not merged
with the engine's: it is the profile the client's requests are sent by (its reasoning wire, its extra body, which stay
the client's). No trait changes.
