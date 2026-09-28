# adam-rs

A Rust framework for durable AI agents, in the spirit of [eve](https://eve.dev):
filesystem-first authoring, macros where eve uses file conventions, and every
piece of infrastructure behind a trait so each developer picks their own.

This repository currently contains the **durable-state layer**: the `Store`
trait, a shared conformance suite, and two production adapters.

| Crate | What it is |
|---|---|
| `adam-core` | `Store` trait, run/journal/lease types, in-memory reference store |
| `adam-store-testkit` | Conformance suite every store must pass (`store_conformance!`) |
| `adam-store-postgres` | PostgreSQL 12+ via `sqlx` 0.9 |
| `adam-store-mongodb` | MongoDB 5.0+ via the official driver; standalone `mongod` is enough |

## The model

A **run** is one execution of an agent. The runtime owns the agent loop as an
explicit state machine, serializes it into `RunRecord::state` (JSON), and
commits every transition with a compare-and-swap on `version`. A worker that
dies loses nothing: another worker picks the run up from the last commit.

The **journal** records the outcome of every side effect a tool performs
through `ctx.step(..)`, keyed by `(run, seq)`. On replay, the recorded outcome
is returned instead of running the side effect again. The first writer wins,
and a replay that asks for a different step name at the same `seq` fails with
`NonDeterminism` instead of silently doing the wrong thing.

**Leases** stop two workers from advancing the same run at once. They are an
efficiency mechanism; the version CAS is what guarantees correctness, so a
worker whose lease expired mid-step still cannot overwrite newer state.

**Scheduling** is one indexed range scan: each run stores a derived `sched_at`
and is due when `sched_at <= now`.

| Status | `wake_at` | Due |
|---|---|---|
| runnable | none | immediately |
| runnable | set | at `wake_at` (retry backoff) |
| parked | set | at `wake_at` (timers, `ctx.sleep`) |
| parked | none | never; resumed by committing it back to runnable (approval, inbound message) |
| done / failed | – | never |

**Conversations**: at most one open (runnable or parked) run per
`(agent, conversation_id)`, enforced by a unique index. Two inbound messages
racing on one conversation cannot start two runs; the loser gets
`ConversationBusy` and resumes the open run instead. Creating a run with a
deterministic id gives idempotent "fire once" semantics, e.g. one run per cron
tick across all replicas.

## Using a store

```rust
use std::sync::Arc;
use adam_core::{DynStore, NewRun, RunStatus, RunUpdate};
use serde_json::json;

// Pick one. Both take your existing pool/database handle too.
let store: DynStore = Arc::new(adam_store_postgres::PgStore::connect(&pg_url).await?);
let store: DynStore = Arc::new(adam_store_mongodb::MongoStore::connect(&mongo_uri, "myapp").await?);

store.migrate().await?; // idempotent, safe to run from every replica at boot

let run = store.create_run(NewRun::new("support-bot", json!({"turn": 0})).conversation("chat-42")).await?;
let run = store.commit_run(run.id, run.version, RunUpdate::new(RunStatus::Parked, json!({"turn": 1}))).await?;
```

## How each adapter guarantees the contract

| Guarantee | PostgreSQL | MongoDB |
|---|---|---|
| Commit CAS | `UPDATE .. WHERE version = $n RETURNING` | `findOneAndUpdate({_id, version: n}, {$inc: {version: 1}})` |
| Journal first-writer-wins | `INSERT .. ON CONFLICT DO NOTHING`, read winner | `insertOne` with `_id = "<run>:<seq>"`, duplicate key → read winner |
| Exclusive claiming | `FOR UPDATE SKIP LOCKED` in one statement | read candidates, `updateMany` re-checking due/lease in the filter with a claim token, read back by token |
| One open run per conversation | partial unique index | plain unique index on `open_key`; closed runs get `~<run id>`, so no partial/sparse index is needed |
| Journal deleted with run | `ON DELETE CASCADE` | journal deleted first, then runs, in batches |
| State storage | `JSONB` (queryable with SQL) | real BSON document (queryable with dot paths) |
| Transactions needed | none held open | none (works on a standalone `mongod`) |

### Data caveats

* **MongoDB keys.** Agent state often contains JSON Schema keys like `$ref` and
  `$defs`, dotted keys, and empty keys. These are escaped reversibly (`%`
  prefix plus percent-encoding of `%`, `.`, `$`, NUL); ordinary keys are stored
  as-is so `state.messages.role` still works in queries. See
  `adam-store-mongodb/src/codec.rs`.
* **MongoDB integers** above `i64::MAX` are rejected with `InvalidData`.
* **PostgreSQL NUL.** `JSONB` cannot hold `\u0000`; such state is rejected with
  `InvalidData` instead of a raw driver error.
* **Time** is truncated to milliseconds in every store (BSON dates are
  millisecond precision), so all backends compare timestamps identically.
  Lease expiry uses the `now` the caller passes in; keep worker clocks in sync
  (NTP) and leave lease TTLs well above expected clock skew.

## Testing

```sh
docker compose up -d
export ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test
export ADAM_TEST_MONGODB_URI=mongodb://localhost:27017
cargo test --workspace
```

Each database suite is skipped when its variable is unset, so `cargo test`
works with no databases (only the in-memory store runs). The suites isolate
cases by agent name, so they run in parallel on one shared database with no
cleanup between runs.

The suite (22 cases) covers: exact JSON roundtrip (unicode, i64 bounds,
floats, special keys), CAS conflicts, 16-way concurrent commits with a single
winner, journal ordering, first-writer-wins and 16-way races,
non-determinism detection, due rules, agent filtering and limits, 8 workers
claiming 60 runs with no double lease, lease expiry and takeover, renew and
release, one-open-run-per-conversation including a 16-way race, and purging.

To add a backend (SQLite, Redis, FoundationDB, ...), implement `Store` and add
one line: `adam_store_testkit::store_conformance!(make_store);`.

## Roadmap

1. ~~Store trait and adapters~~ (this repo)
2. Run state machine and `ctx.step` journaling
3. `#[tool]` macro (schemars)
4. `build.rs` discovery of `agent/` (instructions, tools, skills, subagents)
5. Parking, approvals, schedules
6. Dev TUI (`cargo adam dev`)
7. Host adapters (axum/tower), channels, sandboxes
