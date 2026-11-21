# 0061. A step names its retry policy

Status: **Accepted** (2026-11-20). Milestone M6 of another-agentic-system#402 asks for "retries with backoff: a policy
per step" (*verified* 2026-11-20 in the issue). This ADR is the design; it is built in the slices below.

## Context

A run has one retry policy, the runtime's (`RetryPolicy` in `crates/adam-runtime/src/retry.rs`: `max_attempts`,
`initial_backoff`, `max_backoff`, `multiplier`). A transition that fails with `AgentError::Transient` is retried
whole: the worker counts the consecutive failures of the run's transitions in `attempt`, abandons the failed try's
journal entries (the retry starts at a fresh `seq`), and commits the run runnable with `wake_at = now +
backoff(attempt)`, or fails it once `attempt` reaches `max_attempts`; a transition that succeeds resets `attempt`
(`crates/adam-runtime/src/worker.rs`). A tool's `ToolError::Transient` becomes such an `AgentError` in `LlmAgent`'s
`conclude`, and so does a model provider's retryable error (`crates/adam-llm-agent/src/agent.rs`), so every tool and
the model share that one policy.

What one policy cannot say: a payment call that must not be tried five times; a web search that may be retried ten
times over minutes; a provider whose rate limit (`RateLimited`) should be retried and whose outage should not. And the
class of an error is lost on the way: `AgentError::from_classified` keeps whether it was retryable, not which class
it was, and `ToolError::Transient(String)` has no class at all (`crates/adam-llm-agent/src/tool.rs`).

## Decision

1. **A policy says which classes it retries.** `RetryPolicy` gains `retry_on`, a set of the retryable classes
   (`Transient`, `RateLimited`, `Conflict`; `ErrorClass::is_retryable`), all three by default, so the runtime's
   default retries what it retries now. A class that is not retryable is never retried by any policy: such an error is
   already permanent. `RetryPolicy` becomes `#[non_exhaustive]` with a builder (`RetryPolicy::new(attempts)`,
   `.backoff(initial, max)`, `.multiplier(m)`, `.retry_on(classes)`), and refuses a multiplier that is not finite and
   positive.
2. **Errors carry their class.** `AgentError::Transient` gains `class` (set by `from_classified`; `Transient` when
   built with `AgentError::transient`), and `AgentError::class()` returns it. A retryable `ToolError` carries its class
   in a new variant, `ToolError::Retryable { message, class }`, which `ToolError::from_classified` builds;
   `Transient(String)` stays and means class `Transient`, so journals written before decode as they did. A binary
   older than the one that wrote `Retryable` cannot read that journal entry: roll a deployment forward, not back.
3. **A step names its policy, under a key.** A transient error may carry `retry: Box<StepRetry>`, a key and a policy,
   set with `AgentError::with_retry(key, policy)`. `Ctx::step_retrying(name, key, policy, f)` is `Ctx::step` for an
   `E: Classify` whose recorded `Err`, when retryable, fails the transition with `from_classified(e).with_retry(key,
   policy)`. A `Tool` names its policy with a provided `retry_policy()` (`None` by default; `#[tool(retry(attempts =
   3, backoff = "1s", max_backoff = "30s", on = ["rate-limited"]))]`), and `LlmAgent` attaches it under the key
   `tool:<tool name>` (the name, never the call id, so a tool called again in a later turn counts as the same step).
   Which policy applies, first found: the folder's `retry:` for that tool (decision 6), the tool's own, the step's, the
   runtime's. The runtime's policy is not a ceiling over a step's (a step may retry more, or less, than it), and
   `RetryPolicy::none()` on the runtime still governs errors that name no policy.
4. **Failures are counted per step, and the run has a ceiling.** The run records `retry: {failures: {key → n},
   policies: {key → policy}}` beside `attempt`. A failure under a key adds one to that key (an error that names no
   policy counts under the key `runtime` with the runtime's policy), and it is retried while that key's count is under
   its policy's `max_attempts` **and** `attempt`, every consecutive failure of the run's transitions whatever their key,
   is under `MAX_CONSECUTIVE_FAILURES` (100). So two failing steps that take turns, as parallel tool calls may when the
   first error to finish decides, cannot retry for ever. A transition that succeeds clears the record and `attempt`.
   The wait is the failing key's policy's backoff, lengthened by a `retry_after` hint (each capped at
   `MAX_RETRY_AFTER`) and cut at the run's deadline, as now.
5. **The decision is recorded, never recomputed.** A key's policy is recorded at its first failure; later attempts read
   it from the record, not from the code, so a deploy that changes a policy does not change the retries of a run
   already failing. The record is in the run's state, not in its journal: a failed try's journal is abandoned and the
   retry starts at a fresh `seq`, so no journal entry of it is read again, while the state is written in the one commit
   that also sets `wake_at` or fails the run, so a crash before that commit changes nothing and a replay reads the same
   record. For a v5 run that state is the head (`Head`, `Head::of`, `HeadParts::carried`, `into_parts` in
   `crates/adam-runtime/src/head.rs`), which carries the record as it carries `attempt`; for a v4 run, the envelope.
6. **A folder agent sets a policy per tool.** An agent's frontmatter gains `retry:`, a map from a tool's name to
   `{attempts, backoff, max_backoff, multiplier, on}` (durations as `1s`, `30m`; `on` lists retryable classes by their
   kebab-case names, `transient`, `rate-limited`, `conflict`). Each agent, the root and each subagent, sets its own;
   none inherits. The loader checks the shape and the bounds (`attempts` 1 to 20, `max_backoff` at most 1 h); binding
   checks the names against the agent's tools and, for a name `<server>__<tool>`, against its MCP servers, whose tools
   are only known at startup: such a name is checked when the servers connect, and an unknown one is an error then.
7. **Stored values are plain.** In the record a policy is `{max_attempts, initial_backoff_ms, max_backoff_ms,
   multiplier, retry_on: ["rate-limited", ...]}`, durations as integer milliseconds; the names of classes map in
   `adam-runtime` (`adam-error` stays free of serde); a name the reading build does not know is dropped, so the policy
   retries less, never more. A record that does not decode is ignored, and the error is retried under the runtime's
   policy.

## Consequences

- **Breaking, for code that builds or matches the types:** `AgentError::Transient` gains fields (the coder's
  rebuild of an error, `bin/adam-coder/src/agent.rs`, must keep them); `RetryPolicy` is `#[non_exhaustive]`, so a
  struct literal of it (tests in `adam-ui` and `adam-assembly`) moves to the builder; `ToolError` gains a variant.
- Runs written before read as they did: the record is optional and skipped when absent, and a run whose steps name no
  policy writes the bytes it writes today. A store adapter is untouched.
- `RunView::attempt` no longer equals `max_attempts` when a run gives up under a step's policy; its doc and the
  worker's retry and give-up messages name the key and that key's limit.
- `Ctx::attempt` and `ToolCtx::attempt` are unchanged.
- A policy that retries for long keeps the run, and the coder's workspace of it, for as long: ten attempts with a 24 h
  hint each park a run for days, up to its deadline.
- Docs: `docs/reference/errors.md`, `docs/reference/agent-files.md`, `docs/guides/write-an-agent.md`, and the READMEs
  of `adam-runtime`, `adam-llm-agent`, `adam-macros`, `adam-agent-fs` and `adam-assembly`.

## Slices

1. This ADR.
2. `adam-runtime`: `RetryPolicy`'s `retry_on` and builder; `AgentError`'s class and `StepRetry`; the record in the head
   and the envelope; the worker's counting and ceiling; `Ctx::step_retrying`.
3. `adam-llm-agent` and `adam-macros`: `ToolError::Retryable`, `Tool::retry_policy`, `#[tool(retry(...))]`, the key
   `tool:<name>`, the model provider's class.
4. `adam-agent-fs` and `adam-assembly`: `retry:` in an agent's frontmatter, checked, and bound to its tools and MCP
   servers.

## Amended (2026-11-21), slice 2

Built in vymalo/another-adam-rs#264. Three points read differently from the text above:

- **The builder's names** are `RetryPolicy::new(attempts)`, `.with_backoff(initial, max)`, `.with_multiplier(m)`
  and `.with_retry_on(classes)`: `backoff(failures)` already names the delay. Slices 3 and 4 use these. A
  refused multiplier keeps the factor the policy had.
- **The bytes.** Every failing run writes the record, the runtime's key included, so the runtime's policy is
  recorded too (decision 5). The sentence in *Consequences* holds for a run that never fails: it writes the
  bytes it wrote before.
- **A record that does not decode** is ignored, as one written before records: the run's `attempt` so far is
  counted under `runtime`, and the failing error's own policy applies with its key's count started again (the
  ceiling still bounds it). A record keeps durations up to `i64::MAX` ms, what every store keeps, and a factor
  JSON cannot hold as one that backs off alike.
