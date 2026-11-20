# 0061. A step names its retry policy

Status: **Accepted** (2026-11-20). Milestone M6 of another-agentic-system#402 asks for "retries with backoff: a policy
per step". This ADR is the design; it is built in the slices below.

## Context

A run has one retry policy, the runtime's (`RetryPolicy` in `crates/adam-runtime/src/retry.rs`: `max_attempts`,
`initial_backoff`, `max_backoff`, `multiplier`). A transition that fails with `AgentError::Transient` is retried
whole: the worker counts the consecutive failures of the run's transitions in the envelope's `attempt`, abandons the
failed try's journal entries, and commits the run runnable with `wake_at = now + backoff(attempt)`, or fails it
once `attempt` reaches `max_attempts` (`crates/adam-runtime/src/worker.rs`). A tool's transient error becomes such
a transient `AgentError` (`crates/adam-llm-agent/src/agent.rs`), so every tool shares that one policy.

What one policy cannot say: a payment call that must not be tried five times; a web search that may be retried ten
times over minutes; a model provider that should retry a rate limit (`RateLimited`) but not an outage. And the class
of the error is lost on the way: `AgentError::from_classified` keeps whether it was retryable, not which class it was.

## Decision

1. **A policy says which classes it retries.** `RetryPolicy` gains `retry_on`, a set of `ErrorClass`es; the default is
   the classes that are retryable today (`ErrorClass::is_retryable`), so the runtime's default retries what it retries
   now. A transient error whose class the policy does not list fails the run at once.
2. **A transient error carries its class and may carry a policy.** `AgentError::Transient` gains `class` (set by
   `from_classified`; `ErrorClass::Transient` when built with `AgentError::transient`) and `retry: Option<StepRetry>`,
   a step's name and its policy, set with `with_retry(step, policy)`. An error with no policy is retried by the
   runtime's, exactly as today.
3. **Steps and tools name their policy.** `Ctx::step_retrying(name, policy, f)` is `Ctx::step` whose recorded `Err`,
   when transient, carries `(name, policy)`. A `Tool` names one with a provided `retry_policy()` (`None` by default,
   `#[tool(retry(...))]` for a `#[tool]` function), and `LlmAgent` attaches it, keyed by the tool's name, when a call
   of that tool fails transiently.
4. **Failures are counted per step.** The envelope records `retry: {step, policy, failures}` beside `attempt`: a
   failure of the same step adds one, a failure of another step starts again at one, and a transition that succeeds
   clears it. Whether to retry, and how long to wait, is a function of that record alone.
5. **The decision is recorded, never recomputed.** The policy in the record is the one the step named at its first
   failure; later attempts read it from the envelope, not from the code, so a deploy that changes a policy does not
   change the retries of a run already failing. The record is written in the same commit that schedules the retry or
   fails the run, so a crash before it changes nothing, and a replay of a committed run reads the same record.
6. **A folder agent sets a policy per tool.** The root's frontmatter gains `retry:`, a map from a tool's name to
   `{attempts, backoff, max_backoff, multiplier, on}` (`on` lists classes by their kebab-case names, `rate-limited`,
   ...); a name the agent has no tool for is an error, and a value out of range too. It overrides the tool's own
   `retry_policy()`.
7. **Unchanged:** the runtime's policy and its builder; a step that names nothing; `attempt` (still every consecutive
   failure of the run's transitions, for what reads it today); `retry_after` hints, which only lengthen a wait; and
   the abandoning of a failed try's journal entries.

## Consequences

- The envelope gains a field, optional and skipped when absent, so runs written before it read as they did and a run
  with no step policy writes the bytes it writes today.
- A store adapter is untouched: the envelope is the run's state.
- A policy that retries for long keeps the run, and the coder's workspace of it, for as long; the bounds of
  decision 6 cap a folder's.

## Slices

1. This ADR.
2. `adam-runtime`: `retry_on`, the error's class and `StepRetry`, the envelope's record, the worker; `Ctx::step_retrying`.
3. `adam-llm-agent` and `adam-macros`: `Tool::retry_policy`, `#[tool(retry(...))]`, attached to a tool's transient
   failure.
4. `adam-agent-fs` and `adam-assembly`: `retry:` in the root's frontmatter, checked, bound to the agent's tools.
