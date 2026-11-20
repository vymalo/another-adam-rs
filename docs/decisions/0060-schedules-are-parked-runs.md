# 0060. Schedules are parked runs

Status: **Accepted** (2026-11-20). Roadmap item 5 ("running schedules") is taken up for another-agentic-system#402
(milestone M6, item 1: `after`, `at` and cron, durable, one firing per due time, each firing a run with its trigger
recorded). This ADR is the design; it is built in the slices below. Number 0059 is reserved for another decision being
written at the same time.

*Amended* (2026-11-20, slice 3): the schedule's name is escaped in both conversation ids (`%`,
`/` and `@` as `%25`, `%2F` and `%40`), so a name cannot make one schedule's conversation another's
firing or a session's; `next_due` is built as `ScheduleSpec::due_after`.

## Context

An agent folder may hold `schedules/<name>.md`: frontmatter `cron:` and `timezone:`, the body a prompt
([agent files](../reference/agent-files.md#parsing-and-validation)). The loader checks the cron's shape and the
timezone's spelling, and nothing evaluates them: `adam-agent` and the coder log that they read schedules and do not run
them (`bin/adam-agent/src/folder.rs`, `bin/adam-coder/src/files.rs`).

What a schedule needs is already in the store, with no new `Store` method
([ADR 0053](0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md) adds SQLite behind the same trait):

1. **A durable timer.** A step that returns `Transition::Park { wake_at }` parks its run until `wake_at`
   (`crates/adam-runtime/src/agent.rs`); `claim_due` hands a due run to one worker under a lease, and a commit checks the
   run's version (`crates/adam-runtime/src/worker.rs`). A process that dies loses nothing: another worker, or the same
   one restarted, claims the run.
2. **One open run per conversation.** Creating a run in a conversation that has an open run is refused with
   `ConversationBusy`, whatever ids the runs carry (`crates/adam-store-testkit/src/lib.rs`, every store).
3. **An idempotent start.** `Runtime::start_with_id` creates a run with an id the caller chooses, and answers
   `Ok(false)` when that id exists (`crates/adam-runtime/src/runtime.rs`).

Two things are missing. A step cannot start a root run (`Ctx` gives only `child_starter`,
`crates/adam-runtime/src/ctx.rs`), and a run records nothing about what started it.

## Decision

1. **A schedule is a run of a built-in agent, `adam.schedule`, in the conversation `adam.schedule:<agent>/<name>`.**
   Its state is the schedule's spec (agent, name, when, timezone, prompt), the spec's hash, its anchor (decision 4)
   and the last firings it made. It parks until the next due time, fires, and parks again. A message that wakes it
   early changes nothing, and it parks again. The conversation, not the run's id, keeps a schedule to one open
   scheduler: its id is random. Rejected: a timer in memory (lost on restart, one per replica); a schedules table in
   the store (a new trait method every store must implement, for what a parked run already is); an id derived from
   the spec (a cancelled run keeps its id, so a schedule changed and then changed back could never start again).
2. **One firing per due time.** A firing is `start_with_id` of a run of the root agent, whose id is a UUID v8 of
   SHA-256 over a domain tag of its own, the agent, the schedule's name and the due time as UTC RFC 3339, built like
   `child_run_id` (`crates/adam-runtime/src/child.rs`). The scheduler reads the time with `Ctx::now_journaled` and
   fires in a journaled step, so a step replayed after a crash starts the same id again, and `start_with_id` answers
   that it exists. A transient retry reads the clock again, so it may skip a due time, which is counted in `missed`
   (decision 3); it never fires one twice. Because the id is the due time's, a scheduler that is cancelled while its
   firing step is in flight, and its successor, still fire a due time once. `Ctx::run_starter()` gives a step a
   `RunStarter`, which offers `start_with_id` for conversations under `adam.schedule:` and nothing else, as
   `ChildStarter` is bound to its parent.
3. **Missed due times fire once.** After downtime the scheduler fires the latest due time that has passed and records
   how many it skipped (`missed`, counted up to 10 000 and shown as at least that beyond). Rejected: firing none (a
   daily job silently skipped after a restart at the wrong minute) and firing all (a backlog of identical runs). An
   `at` or `after` that passed while nothing ran fires late, once.
4. **A schedule has exactly one of `cron`, `at` or `after`.** `cron` repeats; `at` is one instant (RFC 3339, or a
   civil date and time in `timezone`); `after` is one duration (`30m`, `2h`) from the scheduler's anchor, the time the
   reconciling runtime's clock read when it started the scheduler, kept in the scheduler's first state. A schedule
   that fired its `at` or `after` ends `Done`. The loader refuses none or more than one. The spec hash covers every
   field, prompt included, so a changed `after` counts from the change.
5. **Reconcile when the schedules are read.** A worker, at start and on a dev reload, reads the latest scheduler run
   of each schedule's conversation, and of every `adam.schedule:<agent>/` conversation no schedule asks for, with
   `list_runs`:
   - open with the same spec hash, or `Done` with it (a one-shot that fired): nothing;
   - open with another hash, or open for a removed schedule: cancel it, with the reason "schedule changed" or
     "schedule removed"; the cancel commits before anything starts;
   - none, cancelled, failed, or finished with another hash: start a scheduler with a new id.

   A start refused with `ConversationBusy` means another replica started one first; reconcile reads it again and
   applies the rules once more. A changed cron fires from now on. During a rolling deploy the process that reconciles
   last wins; decision 2 keeps an old and a new scheduler from firing one due time twice.
6. **Time zones are jiff's** ([ADR 0057](0057-time-is-jiff-with-the-bytes-chrono-wrote.md)), read from the bundled
   database by name (`TimeZoneDatabase::bundled()`, feature `tzdb-bundle-always`), never from the system's, so every
   container evaluates the same. A cron whose minute and hour fields are both restricted (they do not start with `*`)
   names times of day, evaluated on civil time: a time a DST change skips fires as jiff's `compatible` places it (02:30
   in a one-hour gap fires at 03:30), and a time it repeats fires once, at the earlier offset. Any other cron is an
   interval, evaluated on instants, as Vixie cron does: `*/15 * * * *` fires through a repeated hour and not in a
   skipped one. An `at` in civil time follows the times-of-day rule. An unknown `timezone` is refused when the folder
   is loaded (exit 78).
7. **Our own five-field cron parser.** Fields: minute, hour, day of month, month, day of week; `*`, lists, ranges,
   steps, `JAN`-`DEC`, `SUN`-`SAT`, `0` and `7` for Sunday, and `?` as `*`. When both day fields are restricted (do
   not start with `*`, so `*/2` is not), a day matches either (the Vixie rule). Rejected: the `cron` crate (six or seven
   fields), `croner` and `saffron` (both on chrono, which ADR 0057 removed).
8. **Where the code goes.** `adam-agent-fs` parses the format (`Cron::parse` replaces the shape check) and does no time
   arithmetic. A new crate, `adam-schedule`, holds the evaluation (`next_due`, `latest_due`, the ids), the
   `adam.schedule` agent and `reconcile`. `adam-assembly` registers the agent when a folder has schedules;
   `adam-service` gains `Agents::runtime_component` so a binary can reconcile with the `Runtime`; the worker roles
   seed, the A2A server role does not.

## Consequences

- A firing is an ordinary run of the root agent, in a conversation of its own,
  `adam.schedule:<agent>/<name>@<due>`, which `adam_schedule::trigger_of` reads back. It starts with no memory of
  earlier firings. Its result is in the store, read by listing the root agent's runs under
  `adam.schedule:<agent>/<name>@`; no A2A caller owns that conversation, so none lists it as a task. Delivering a
  firing's result somewhere (a push, a person's thread) is a later decision. `RunRecord` is unchanged.
- **Retention must never purge `adam.schedule` runs.** A purged `Done` one-shot would be started and fired again by the
  next reconcile. They are small: one per schedule and change. Nothing purges runs yet (`purge_finished` has no
  caller); whoever wires retention keeps this rule, and a test of it.
- Under `ClaimScope::Pinned` (the coder) a scheduler run belongs to the first worker that claimed it, so it stalls when
  that worker is gone for good. That is a known gap until a claim scope can be set per agent.
- A folder whose `timezone` is spelled like a zone but names none (`Mars/Olympus`) loads today and is refused once
  slice 2 lands. A folder with a `cron` schedule keeps its digest; the new keys change the digest of the folders that
  use them.

## Slices

1. This ADR.
2. `adam-agent-fs`: `at`, `after`, `Cron::parse`, the embedded form and the codegen; the agent-files page.
3. `adam-runtime`: `RunStarter`; `adam-schedule`: `When`, `next_due`, `latest_due` and the ids, with the time tests
   (DST in `Europe/Berlin` for times of day and intervals, the day rule).
4. The `adam.schedule` agent and `reconcile`, tested on the memory store with a manual clock: two workers fire once, a
   restart loses nothing, five hours down fires one with `missed: 4`, a removed schedule ends, a changed one restarts,
   a schedule changed back starts again, a fired one-shot is not fired again, the trigger is recorded.
5. The wiring: `adam-assembly`, `Agents::runtime_component`, seeding in `adam-agent`, and the warning removed.
6. The coder (with the `Pinned` caveat) and reconcile on a dev reload.
