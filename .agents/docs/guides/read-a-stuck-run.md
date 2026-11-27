# Read a run's state when it is stuck: queries and evidence

For people: [Read a run's state when it is stuck](../../../docs/guides/read-a-stuck-run.md). That page decides; this one gives the queries and the evidence.
Checked at `257175c` on 2026-11-26 against the PostgreSQL schema (version 6). Read only: every statement below is a `SELECT`. Never `UPDATE` a run or a lease.

The task id of an A2A task is the run id, derived by `task_id_for(agent, caller, context, messageId)` (`crates/adam-a2a-runtime/src/ids.rs:50`, used by `start_with_id`).
Tables have the prefix `adam_` by default (`crates/adam-store-postgres/src/lib.rs:225`). Run `psql` from the database pod, as a role that reads `adam_*`.

## Queries

```sql
-- one run: what the client's state is made from
SELECT id, agent, conversation_id, parent_id, status, version, wake_at, sched_at,
       lease_owner, lease_until, owner, created_at, updated_at,
       coalesce(head->>'error', state->>'error') AS error
FROM adam_runs WHERE id = '<run id>';

-- its last steps: the name of the step that is long, or the one that failed
SELECT seq, name, ok, recorded_at FROM adam_journal WHERE run_id = '<run id>' ORDER BY seq DESC LIMIT 5;

-- the shape of an agent's runs
SELECT status, count(*), min(updated_at) FROM adam_runs WHERE agent = '<agent>' GROUP BY status;

-- due, and nobody holds a live lease: waiting for a worker
SELECT id, agent, sched_at, lease_owner, lease_until, owner
FROM adam_runs
WHERE sched_at <= now() AND (lease_until IS NULL OR lease_until < now())
ORDER BY sched_at LIMIT 20;

-- who holds leases now (compare with the worker pods that exist)
SELECT lease_owner, count(*), max(lease_until) FROM adam_runs
WHERE lease_until > now() GROUP BY lease_owner;

-- open runs pinned to a worker (affinity, isolated): compare with WORKER_ID of the live pods
SELECT owner, count(*) FROM adam_runs WHERE owner IS NOT NULL AND status IN ('runnable', 'parked') GROUP BY owner;

-- a webhook that was never delivered
SELECT run_id, id, state, attempts, last_error, next_attempt_at FROM adam_push WHERE state <> 'done' ORDER BY next_attempt_at LIMIT 20;

-- the schema version, after an upgrade
SELECT value FROM adam_meta WHERE key = 'schema_version';
```

A run `lease_until` in the future whose `version` and `adam_journal` stay put across two reads a minute apart is one long step. A run whose `lease_until` is in the
future for a `lease_owner` that is not a live pod is waiting out the lease. Compare with `kubectl get pods` for the worker ids (`WORKER_ID` is the pod name).

## Evidence

| Claim on the page for people | Where it is true |
|---|---|
| a run is due by `sched_at`: runnable at `wake_at`, or at `updated_at` with none; parked at `wake_at`, never with none; finished never | `crates/adam-core/src/store/mod.rs:1052-1063` |
| the client's state from the row | `crates/adam-a2a-runtime/src/convert.rs:132` (`task_state`); table in `docs/architecture.md` "The run lifecycle" |
| a front claims nothing, so its tasks stay `submitted` | `docs/architecture.md` "The run lifecycle" (after the state table) |
| lease 30 s, poll 250 ms, four runs per worker loop, renewal every third of the lease | `crates/adam-runtime/src/runtime.rs:1270-1274`; `docs/architecture.md` "The worker" |
| claiming is exclusive and skips locked rows | `crates/adam-store-postgres/src/lib.rs:906` (`FOR UPDATE SKIP LOCKED`) |
| a pinned run's owner is set by the first pinned claim and never cleared; a claim cannot tell busy from gone | `docs/reference/workspace-and-environments.md` "Placement" (lines 24-26, 55) |
| a cut step is run again from the journal; effects are at-least-once | `docs/architecture.md` "The worker" (Replay is safe) |
| a step's tool limit is 300 s unless it says otherwise | `docs/architecture.md` "The worker" (first bullet) |
| the run pod wait is 1020 s and outlasts idle (900) + sweep (60) + grace (2) | `deploy/coder/templates/statefulset.yaml:367-370`; chart README "Run pods" |
| an unreachable git sandbox is retried, never a fallback | `docs/reference/errors.md` "Exit codes" (the sandbox paragraph) |
| a model park waits 30 s, doubling to 15 min, until `MODEL_PARK_LIMIT` | `docs/reference/environment.md:64` |
| finished runs are deleted after `RUN_RETENTION_DAYS`, default 90 | `docs/reference/environment.md:58` |
| deleting a run cascades to journal, push configs, blobs, body | `crates/adam-store-postgres/src/lib.rs:619-760` (`ON DELETE CASCADE`) |
| a commit is a compare-and-swap on `version`, the loser drops its result | `docs/architecture.md` "The worker" (`commit_run`, Conflict) |
| the cancelled prefix | `docs/architecture.md` "The run lifecycle": a `Failed` run whose error starts `cancelled: ` is `canceled` |

## Unverified

* The `head->>'error'` and `state->>'error'` paths were read from the envelope description in `docs/architecture.md` ("What `state` holds"), and were not run against a database here.
* That a lease expired by clock skew behaves as described: the rule is documented (`docs/reference/store-adapters.md` "Data caveats"), and no test of skew was read.
* MongoDB and SQLite hold the same fields (`docs/architecture.md` "Data: the run store"); the SQL above is PostgreSQL's.
