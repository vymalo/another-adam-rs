# Store adapters

The `Store` port ([`adam-core`](../../crates/adam-core/README.md)) is the durable state: runs, journal,
leases, A2A push-notification configs and their delivery progress, and the listing of a caller's runs. Two adapters ship, and every adapter must pass the same conformance suite
([`adam-store-testkit`](../../crates/adam-store-testkit/README.md)). The model they implement is in
[Architecture](../architecture.md#the-mental-model); the schema is in
[Data: the run store](../architecture.md#data-the-run-store).

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

Creating a run with a deterministic id gives idempotent "fire once" semantics, for example one run per
cron tick across all replicas. A second message on a conversation that has an open run gets
`ConversationBusy`; resume the open run instead.

## How each adapter keeps the contract

| Guarantee | PostgreSQL 12+ (`sqlx`) | MongoDB 5.0+ (standalone is enough) |
|---|---|---|
| Commit CAS | `UPDATE .. WHERE version = $n RETURNING` | `findOneAndUpdate({_id, version: n}, {$inc: {version: 1}})` |
| Journal first-writer-wins | `INSERT .. ON CONFLICT DO NOTHING`, read winner | `insertOne` with `_id = "<run>:<seq>"`, duplicate key: read winner |
| Exclusive claiming | `FOR UPDATE SKIP LOCKED` in one statement | read candidates, `updateMany` re-checking due/lease in the filter with a claim token, read back by token |
| Busy runs never claimed | `AND id <> ALL($busy)` | `_id: { $nin: busy }` |
| Pinned claiming (`owner`, schema version 2) | `AND (owner IS NULL OR owner = $w)` and `owner = COALESCE(owner, $w)` | the same condition in the candidate and `updateMany` filters (a missing field is `null`) |
| One open run per conversation | partial unique index | plain unique index on `open_key`; closed runs get `~<run id>`, so no partial or sparse index |
| Journal deleted with its run | `ON DELETE CASCADE` | journal first, then runs, in batches |
| Push configs (`push_*`, schema version 3): put, claim, commit | `INSERT .. ON CONFLICT DO UPDATE` bumping `version`; one `FOR UPDATE SKIP LOCKED` claim; `UPDATE .. WHERE version = $n` | update-or-insert on `_id = "<run>:<id>"`; a loop of `findOneAndUpdate` claims, each atomic; `findOneAndUpdate({_id, version: n})` |
| Push configs deleted with their run | `ON DELETE CASCADE` | deleted with the journal by the purge |
| `list_runs` / `count_runs` (`ListTasks`), keyset on `(updated_at, id)` | `runs (agent, conversation_id COLLATE "C", updated_at DESC, id DESC)`, a prefix is a range plus `starts_with` | `(agent, conversation_id, updated_at, _id)`, a prefix is an anchored escaped regex |
| State | `JSONB` (queryable with SQL) | real BSON document (queryable with dot paths) |
| Transactions | none held open | none |
| Cross-process signals | `LISTEN`/`NOTIFY` via `adam-notify-postgres` | none: workers poll |

## Data caveats

* **MongoDB keys.** Agent state often contains JSON Schema keys like `$ref` and `$defs`, dotted keys and
  empty keys. They are escaped reversibly (`%` prefix plus percent-encoding of `%`, `.`, `$`, NUL);
  ordinary keys are stored as-is so `state.messages.role` still works in queries
  (`crates/adam-store-mongodb/src/codec.rs`).
* **MongoDB integers** above `i64::MAX` are rejected with `InvalidInput`.
* **PostgreSQL NUL.** `JSONB` cannot hold `\u0000`; such state is rejected with `InvalidInput`.
* **Time** is truncated to milliseconds in every store. Lease expiry uses the `now` the caller passes in:
  keep worker clocks in sync (NTP) and lease TTLs well above the expected skew.

## Adding a backend

Implement `Store` and add one line: `adam_store_testkit::store_conformance!(make_store);`. A `Notifier`
(Redis, NATS) runs `adam_notify_testkit::notifier_conformance!(make_pair);`. The `adam-store-adapter`
skill walks through it; adding a required method to a trait breaks implementers, so say so in the PR and
in the crate README.
