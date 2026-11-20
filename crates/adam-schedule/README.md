# adam-schedule

The schedules of an adam-rs agent folder ([ADR 0060](../../docs/decisions/0060-schedules-are-parked-runs.md)):
when each one is due, and the ids and conversations of the runs it fires. `adam-agent-fs` reads and checks
`schedules/<name>.md`; this crate evaluates them, without a clock or a store (every function takes the instants it
needs), and runs them: the `adam.schedule` agent, and `reconcile`, which reads the store and the runtime's clock.

**Built so far:** slice 3, the evaluation, and slice 4, the `adam.schedule` agent and `reconcile`. Nothing runs a
schedule until slice 5 registers the agent and reconciles at worker start.

## API at a glance

| Item | What |
|---|---|
| `ScheduleSpec::from_schedule(&Schedule)` | a schedule, with its `timezone` looked up in the time-zone database jiff bundles, never the host's (`tzdb-bundle-always`). `ScheduleError` (`agent`, `name`, `why`) for what the loader would have refused: none or more than one of `cron`, `at` and `after`, a value that does not parse, an unknown zone |
| `agent()`, `name()`, `when()`, `timezone()`, `prompt()`, `is_one_shot()` | what the file says; `When`, `Cron` and `At` are `adam-agent-fs`'s, re-exported |
| `hash()` | `sha256:<hex>` over agent, name, `cron`, `at`, `after`, `timezone` and prompt: any change to the file is a new spec, and a new scheduler (decision 5). The path is not in it |
| `due_after(after, anchor)` | the first due time strictly after `after`, or `None`. `anchor` is what an `after` counts from (the scheduler's anchor, decision 4); the other forms ignore it. A `cron` is searched up to eight years ahead (`0 0 29 2 *` across 2100) |
| `latest_due(since, now, anchor)` → `Option<Due { at, missed }>` | the latest due time in `(since, now]` and the count of those before it, which do not fire (decision 3); `missed` stops at `MISSED_MAX` (10 000), which means at least that many, and the latest is then found by searching back from `now`. A walk stops if a due time ever fails to move forward |
| `schedule_conversation_id(agent, name)` | `adam.schedule:<agent>/<name>`: the conversation of the scheduler run, one open per schedule (decision 1). The name is escaped (`%`, `/`, `@` as `%25`, `%2F`, `%40`; `team/weekly` is `team%2Fweekly`), so no name can make it another schedule's firing (`x@<due>`) or a session's (`s/<id>`) |
| `fired_conversation_id(agent, name, due)` | `adam.schedule:<agent>/<name>@<due>`, the name escaped the same way, the due time in UTC RFC 3339 as jiff writes it |
| `fired_run_id(agent, name, due)` | the id of the run a due time fires: a UUID v8 of SHA-256 over the tag `adam-schedule/fired-run/v1` and the length-prefixed agent, name and due time, the same on every replica and for every scheduler of the schedule (decision 2). Pinned by a test: changing it would fire every due time again after an upgrade |
| `trigger_of(conversation)` → `Option<Trigger { agent, name, due }>` | the trigger of a fired run, read back from its conversation (the name unescaped); `None` for a scheduler's conversation, another spelling of the due time or of the name's escapes, or any other conversation |

## The scheduler

| Item | What |
|---|---|
| `SchedulerAgent` (`SCHEDULER_AGENT`, `adam.schedule`) | a schedule as a parked run (decision 1). Each step reads the time from the journal (`Ctx::now_journaled`), fires the latest due time since the last one (`latest_due`) in one journaled step, `fire`, through `Ctx::run_starter`, and parks until the next due time; a one-shot that fired ends `Done` (output `{"fired": [...]}`). A one-shot that has not fired is due whenever its time is, so an `at` that had already passed when its scheduler started fires late, once (decision 3). A message that wakes it early is read and dropped, and it parks again. **It never fails for a firing:** a start that fails with a retryable error (the store unavailable) parks it for `FIRE_RETRY` (30 s) without consuming the due time, and a fresh transition reads the clock again (so a later due time may replace it, counted in `missed`); a refused start (an agent this runtime does not know, a first message it refuses) is recorded as a `Firing` with its `error`, and the schedule goes on to the next due time. Only a schedule that no longer evaluates fails it; `reconcile` then refuses it too |
| `SchedulerState` | `schedule` (a `ScheduleRecord`: the file's fields, so a restart evaluates what was started), `hash`, `anchor` (the reconciling runtime's clock when it started, what an `after` counts from), `last_due`, and `fired`, the last `FIRINGS_KEPT` (20) `Firing { due, run, missed, created, error }` |
| A firing | a run of the root agent under `fired_run_id`, in `fired_conversation_id`, started with kind `message` (`FIRING_KIND`) and the payload `{"text": <prompt>, "schedule": {"name", "due", "missed"}}`, which an `LlmAgent` reads as a person's message. `created` is false when the run existed (another scheduler, or a replay, fired the due time first) or the start was refused (`error`) |
| `reconcile(runtime, agent, schedules)` → `Reconciled { started, kept, cancelled }` | decision 5. For each `adam.schedule:<agent>/` conversation, the scheduler run that decides is its open one (at most one), else the one created last. Open with the same hash, or `Done` with it (a fired one-shot, or a `cron` that has no due time in the next eight years): kept; a `Done` run whose state carries no readable hash is kept too, so a fired one-shot never fires twice (only the `hash` field is read). Open with another hash: cancelled (`CHANGED_REASON`, `schedule changed`), the cancel committed, then started. None, failed (a cancel ends a run `Failed`), or finished with another hash: started with a new id and an anchor from the runtime's clock. An open scheduler no schedule asks for is cancelled (`REMOVED_REASON`). A start refused with `ConversationBusy` (another process started one first) reads the conversation again and decides again, up to three times; a schedule can then be named in both `cancelled` and `kept`. A schedule that does not evaluate is `ReconcileError::Schedule`, before anything changes. Schedules of another agent are ignored |

## Time

A `cron` whose minute and hour fields are both restricted (neither starts with `*` or `?`) names **times of day**,
evaluated on civil time in the zone. An interval's skips (to the next hour, to the next day) stop at the zone's next transition and never go back, so a 45-minute change (Pacific/Chatham) or a fall-back across midnight loses no minute and repeats none. A time that a DST change skips fires where jiff's `compatible` puts it: in
`Europe/Berlin`, 02:30 on the day clocks go forward is 03:30. A time that it repeats fires once, at the earlier
offset. Any other `cron` is an **interval**, matched on every minute of the instant line: `*/15 2 * * *` fires eight
times on the day clocks go back and not at all on the day they go forward, as Vixie cron does. An `at` in civil time
follows the times-of-day rule; an `at` with an offset and an `after` are instants.

## Tests

`cargo test -p adam-schedule`:

* `tests/due.rs` covers DST in `Europe/Berlin` (a skipped and a repeated time of day, an interval through both), UTC
  and a half-hour zone, the day rule, a date eight years ahead, the one-shots and their anchor, missed due times and
  the cap, the refusals, and what the hash covers.
* `tests/scheduler.rs`, on the memory store with a `ManualClock` and a real worker: an `after` that fires once,
  records its trigger and ends, and is kept (not fired again) a day later; a five-hour outage that fires one with
  `missed: 4` on a new process; two processes that start one scheduler and fire each of three hours once (the
  scheduler's own state agrees); removed, changed and changed back (three runs, one open, the reasons); removed and
  added back unchanged; a past `at` that fires late, once; a refused firing recorded while the schedule goes on; a
  firing the store fails, tried again after `FIRE_RETRY` and fired once (`FaultyStore`); a bad schedule that changes
  nothing and another agent's scheduler left open; a message that wakes a scheduler, is read, and fires nothing.
* Unit tests in `src/reconcile.rs`: a start refused as busy over a stale listing reads again and keeps the other
  process's scheduler; the open run decides over a finished one that looks newer.
* Unit tests in `src/ids.rs` cover the conversations, the trigger read back, and the fired id (stable, distinct,
  pinned).
