# 0060. Schedules are parked runs

Status: **Accepted** (2026-11-20). Roadmap item 5 ("running schedules") is taken up for another-agentic-system#402
(milestone M6, item 1: `after`, `at` and cron, durable, one firing per due time, each firing a run with its trigger
recorded). This ADR is the design; it is built in the slices below.

## Context

An agent folder may hold `schedules/<name>.md`: frontmatter `cron:` and `timezone:`, the body a prompt
([agent files](../reference/agent-files.md), "Schedules"). The loader checks the cron's shape and nothing evaluates it:
`adam-agent` and the coder log that they read schedules and do not run them (`bin/adam-agent/src/folder.rs`,
`bin/adam-coder/src/files.rs`).

What a schedule needs is already in the store, with no new `Store` method
([ADR 0053](0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md) adds SQLite behind the same trait):

1. **A durable timer.** A step that returns `Transition::Park { wake_at }` parks its run with `sched_at = wake_at`
   (`crates/adam-runtime/src/agent.rs`); `claim_due` hands it to one worker under a lease when the time comes, and a
   commit checks the run's version (`crates/adam-runtime/src/worker.rs`). A process that dies loses nothing: another
   worker, or the same one restarted, claims the run.
2. **An idempotent start.** `Runtime::start_with_id` creates a run with an id the caller chooses, and answers
   `Ok(false)` when that id exists (`crates/adam-runtime/src/runtime.rs`), so replicas that start the same id start
   one run.

Two things are missing. A step cannot start a root run (`Ctx` gives only `child_starter`,
`crates/adam-runtime/src/ctx.rs`), and a run records nothing about what started it.

## Decision

1. **A schedule is a run of a built-in agent, `adam.schedule`.** Its state is the schedule (agent, name, when, prompt)
   and what it fired. It parks until the next due time, fires, and parks again. Its id is derived from the root agent,
   the schedule's name and a hash of its spec; its conversation is `adam.schedule:<agent>/<name>`, so at most one is open
   per schedule. Rejected: a timer in memory (lost on restart, and one per replica); a schedules table in the store (a
   new trait method every store must implement, for what a parked run already is).
2. **One firing per due time.** A firing is `start_with_id` of a run of the root agent, whose id is a UUID v8 of
   SHA-256 over (agent, schedule, due time), as `child_run_id` derives a child's (`crates/adam-runtime/src/child.rs`).
   The id does not depend on the scheduler run's, so two scheduler runs of the same schedule (D5) still fire a due
   time once. The firing is a journaled step after a journaled `now`, so a step retried after a crash starts the same
   run again and `start_with_id` answers that it exists. `Ctx::run_starter()` gives a step a `RunStarter`, which
   offers `start_with_id` and nothing else.
3. **Missed due times fire once.** After downtime the scheduler fires the latest due time that has passed and records
   how many it skipped (`missed`). Rejected: firing none (a daily job silently skipped after a restart at the wrong
   minute) and firing all (a backlog of identical runs). An `at` or `after` that passed while nothing ran fires late,
   once.
4. **A schedule has exactly one of `cron`, `at` or `after`.** `cron` repeats; `at` is one instant (RFC 3339, or a civil
   date and time in `timezone`); `after` is one duration (`30m`, `2h`) from the moment the scheduler run was created. A
   schedule that fired its `at` or `after` ends. The loader refuses none or more than one.
5. **Reconcile when the schedules are read.** A worker, at start and on a dev reload, cancels the open scheduler runs
   of its agent that no schedule asks for (a schedule removed, or changed, which gives it a new id), and starts the
   expected ones with `start_with_id`. A changed cron fires from now on. During a rolling deploy the last process to
   start wins; D2 keeps an old and a new scheduler from firing one due time twice.
6. **Time zones are jiff's** ([ADR 0057](0057-time-is-jiff-with-the-bytes-chrono-wrote.md)), with the time-zone
   database bundled (`tzdb-bundle-always`), so a container without `/usr/share/zoneinfo` evaluates the same. A local
   time that a DST change skips fires at the next valid instant; one that it repeats fires once, at the earlier
   offset. An unknown `timezone` is refused when the folder is loaded (exit 78).
7. **Our own five-field cron parser.** Fields: minute, hour, day of month, month, day of week; `*`, lists, ranges,
   steps, `JAN`-`DEC`, `SUN`-`SAT`, `0` and `7` for Sunday, and `?` as `*`. When both day fields are restricted, a
   day matches either (the Vixie rule). Rejected: the `cron` crate (six or seven fields), `croner` and `saffron` (both
   on chrono, which ADR 0057 removed).
8. **Where the code goes.** `adam-agent-fs` parses the format (`Cron::parse` replaces the shape check) and does no time
   arithmetic. A new crate, `adam-schedule`, holds the evaluation (`next_due`, `latest_due`, the ids), the
   `adam.schedule` agent and `reconcile`. `adam-assembly` registers the agent when a folder has schedules;
   `adam-service` gains `Agents::runtime_component` so a binary can reconcile with the `Runtime`; the worker roles
   seed, the A2A server role does not.

## Consequences

- A firing is an ordinary run of the root agent. Its trigger is in its conversation id,
  `adam.schedule:<agent>/<name>@<due>`, which `adam_schedule::trigger_of` reads back; the scheduler's state lists the
  last firings it made. `RunRecord` is unchanged.
- Scheduler runs are runs: a retention policy that purges finished runs must leave the open `adam.schedule` runs alone,
  and the finished ones may go.
- Under `ClaimScope::Pinned` (the coder) a scheduler run belongs to the first worker that claimed it, so it stalls when
  that worker is gone for good. That is a known gap until a claim scope can be set per agent.
- A folder with a `cron` schedule keeps its digest; the new keys change the digest of the folders that use them.

## Slices

1. This ADR.
2. `adam-agent-fs`: `at`, `after`, `Cron::parse`, the embedded form and the codegen; the agent-files page.
3. `adam-runtime`: `RunStarter`; `adam-schedule`: `When`, `next_due`, `latest_due` and the ids, with the time tests
   (DST in `Europe/Berlin`, the day rule).
4. The `adam.schedule` agent and `reconcile`, tested on the memory store with a manual clock: two workers fire once, a
   restart loses nothing, five hours down fires one with `missed: 4`, a removed schedule ends, a changed one restarts,
   the trigger is recorded.
5. The wiring: `adam-assembly`, `Agents::runtime_component`, seeding in `adam-agent`, and the warning removed.
6. The coder (with the `Pinned` caveat) and reconcile on a dev reload.
