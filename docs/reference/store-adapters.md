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

A `create_run` or a `commit_run` returns the record **without its state** (`RunRecord::state` is `null`):
the caller has the state it wrote, and echoing it back would transfer the largest member of the record
on every transition. Read the run for the stored state (a store may have replaced a NUL in it);
`journal_put` is the exception and returns the payload it stored, because the loop decodes the step's
outcome from it.

`run_header` answers a run **without its state** — its id, agent, conversation, parent, status, wake
time, version, timestamps and lease — which is all a reader that only needs what the run is doing
(a status poll, a cancel watch) reads. The trait defaults it to `lease_until` + `load_run`; both
adapters override it with one projected read that never touches the state, and the conformance case
`run_header_matches_the_record` proves an implementation agrees with the two reads.

`PgStore::connect` sizes its pool from `ADAM_PG_POOL_SIZE` (2 to 65535, default 16, per process;
one is refused because the `adam-notify-postgres` listener holds a connection for the process's
life); `connect_with_pool_size` takes the size explicitly, and `from_pool` uses the pool you pass. A
value that is not an integer in range fails the connect rather than falling back to 16.

Creating a run with a deterministic id gives idempotent "fire once" semantics, for example one run per
cron tick across all replicas. A second message on a conversation that has an open run gets
`ConversationBusy`; resume the open run instead.

## How each adapter keeps the contract

| Guarantee | PostgreSQL 12+ (`sqlx`) | MongoDB 5.0+ (standalone is enough) |
|---|---|---|
| Commit CAS | `UPDATE .. WHERE version = $n RETURNING` | `findOneAndUpdate({_id, version: n}, {$inc: {version: 1}})` |
| Journal first-writer-wins | `INSERT .. ON CONFLICT DO NOTHING`, read winner | `insertOne` on `_id = "<run>:<seq>"`; a duplicate key means an earlier writer won, and their entry is read back |
| Exclusive claiming | `FOR UPDATE SKIP LOCKED` in one statement | read candidates, `updateMany` re-checking due/lease in the filter with a claim token, read back by token |
| Busy runs never claimed | `AND id <> ALL($busy)` | `_id: { $nin: busy }` |
| Pinned claiming (`owner`, schema version 2) | `AND (owner IS NULL OR owner = $w)` and `owner = COALESCE(owner, $w)` | the same condition in the candidate and `updateMany` filters (a missing field is `null`) |
| One open run per conversation | partial unique index | plain unique index on `open_key`; closed runs get `~<run id>`, so no partial or sparse index |
| Journal deleted with its run | `ON DELETE CASCADE` | journal first, then runs, in batches |
| Push configs (`push_*`, schema version 3): put, claim, commit | `INSERT .. ON CONFLICT DO UPDATE` bumping `version`; one `FOR UPDATE SKIP LOCKED` claim; `UPDATE .. WHERE version = $n` | update-or-insert on `_id = "<run>:<id>"`; a loop of `findOneAndUpdate` claims, each atomic; `findOneAndUpdate({_id, version: n})` |
| Push configs deleted with their run | `ON DELETE CASCADE` | deleted with the journal by the purge |
| `list_runs` / `count_runs` (`ListTasks`), keyset on `(updated_at, id)` | `runs (agent, conversation_id COLLATE "C", updated_at DESC, id DESC) INCLUDE (status)`, a prefix is a range plus `starts_with`; a prefix page sorts the matching index entries (the status filter is answered from the index) | `(agent, conversation_id, updated_at, _id)`, and `(agent, status, conversation_id, updated_at, _id)` when a status filter is set: a prefix is an anchored escaped regex, so the index bounds the scan but the page still sorts the matches; a listing of one conversation is read in index order |
| State | `JSONB` (queryable with SQL) | real BSON document (queryable with dot paths) |
| `run_header`: a run without its state, with its lease | one primary-key statement selecting the header columns and `lease_until`, never `state` | one `find_one` projected to the header and `lease_until` |
| Transactions | none held open | none |
| Cross-process signals | `LISTEN`/`NOTIFY` via `adam-notify-postgres` | none: workers poll |

## Data caveats

* **MongoDB keys.** Agent state often contains JSON Schema keys like `$ref` and `$defs`, dotted keys and
  empty keys. They are escaped reversibly (`%` prefix plus percent-encoding of `%`, `.`, `$`, NUL);
  ordinary keys are stored as-is so `state.messages.role` still works in queries
  (`crates/adam-store-mongodb/src/codec.rs`). The store replaces a NUL with U+FFFD before the codec
  (`adam_core::storable`), so the NUL escape is only for data written before ADR 0051.
* **MongoDB integers** above `i64::MAX` are rejected with `InvalidInput`.
* **NUL characters.** `JSONB` cannot hold `\u0000`. Every adapter replaces it with U+FFFD
  (`adam_core::storable`) before it writes a state, a journal payload or a push configuration, so the
  value is the same in every backend and a tool that printed a NUL does not fail the run
  ([ADR 0051](../decisions/0051-a-nul-is-stored-as-the-replacement-character.md)). `journal_put`,
  `push_put` and `push_commit` return the cleaned value they stored; a run's `create_run`/`commit_run`
  return no state, and a read returns the stored one.
* **Time** is truncated to milliseconds in every store. Lease expiry uses the `now` the caller passes in:
  keep worker clocks in sync (NTP) and lease TTLs well above the expected skew.

## Adding a backend

Implement `Store` and add one line: `adam_store_testkit::store_conformance!(make_store);`. A `Notifier`
(Redis, NATS) runs `adam_notify_testkit::notifier_conformance!(make_pair);`. The `adam-store-adapter`
skill walks through it; adding a required method to a trait breaks implementers, so say so in the PR and
in the crate README.
