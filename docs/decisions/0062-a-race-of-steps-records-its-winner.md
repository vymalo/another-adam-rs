# 0062. A race of steps records its winner

Status: **Accepted** (2026-11-21). Milestone M6 of another-agentic-system#402 asks for a "combinator: race" beside the
join that exists (*verified* 2026-11-21 in the issue). This ADR is the design; it is built in the same pull request.

## Context

`Ctx::step_all` is the runtime's join: it reserves one journal seq per step, runs the steps at the same time, and
records each outcome at its own seq (`crates/adam-runtime/src/ctx.rs`). There is no race: an agent that wants the
first of several attempts to succeed (two mirrors, two providers, a fast path and a slow one) writes it itself, and a
hand-written `select` inside a `Ctx::step` is easy to get wrong: a replay that runs the contenders again may pick
another winner, and the losers are not told to stop.

## Decision

1. **A race is one journaled step.** `Ctx::step_race(name, contenders)` takes one `seq`, named `name`, and records
   one outcome there: the winner's index and value (`Ok((index, value))`), or, when every contender failed, their
   errors in contender order (`Err(errors)`). It is `Ctx::step` over a closure that runs the contenders, so the
   journal format is unchanged: a payload, `ok` or not, at a seq.
2. **First success wins.** The contenders run at the same time; the first `Ok` to finish wins. An `Err` does not end
   the race while another contender runs. A race with no contender is refused before anything is journaled (a
   permanent error: the code is wrong, and a replay would refuse it the same way).
3. **The losers are cancelled, and stay so.** Each contender gets its own child of the run's cancel token. When a
   winner is found, every other contender's token fires and its future is dropped. The record names the winner, so a
   replay returns it without running any contender: no loser is run again, and the same winner is returned on every
   replay (after a crash, a lost lease or a stale commit).
4. **The effect is at-least-once, as for any step.** A crash after a contender's effect and before the record runs
   the race again, whose winner may then differ; once recorded, it never does. A transient retry of the transition
   abandons the record and races afresh (`Ctx::step`'s retry contract). Contenders that must not run twice make their
   effect idempotent, as any step does.

## Consequences

- New API: `Ctx::step_race`. Nothing else changes; a store adapter is untouched.
- A loser that ignores its token runs until its future is dropped, which is at once: a contender's work that must stop
  (a process, a task it spawned) watches the token.
- Docs: the `adam-runtime` README.
