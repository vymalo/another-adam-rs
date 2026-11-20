# Store adapters

The `Store` port ([`adam-core`](../../crates/adam-core/README.md)) is the durable state: runs, journal,
leases, A2A push-notification configs and their delivery progress, a run's file bytes (blobs), a run's
transcript (body), and the listing of a caller's runs. Two adapters ship, and every adapter must pass
the same conformance suite ([`adam-store-testkit`](../../crates/adam-store-testkit/README.md)). The
PostgreSQL one runs on `tokio-postgres`, pooled by `deadpool-postgres`, with `rustls` on `aws-lc-rs`;
`sqlx` left the workspace with
[ADR 0053](../decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md)
decision 1, and the schema and statements did not change. The model they implement is in
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
on every transition. Read the run for its document (a store may have replaced a NUL in it);
`journal_put` is the exception and returns the payload it stored, because the loop decodes the step's
outcome from it.

**Every read that returns a record answers the same document** (ADR 0055, the contract release):
`load_run`, `load_head`, `claim_due`, `open_run_for_conversation` and `list_runs` serve the v5 **head**
when the store has a current one (`head_version == version`), and the whole `state` otherwise — a v4
run, or a head a **v4** commit did not rebuild (a tombstoned run, `state = null`, serves its head:
it is the only readable document). `state` is the tombstone (`null`) for a run whose head
is current, so the head is the run. A store that keeps no bodies (`keeps_bodies() == false`, the
default) is not written the contract: the runtime keeps the whole v4 `state` for it, and its reads
answer that, exactly as before.

`run_header` answers a run **without its state** — its id, agent, conversation, parent, status, wake
time, version, timestamps and lease — which is all a reader that only needs the run's own bookkeeping
(a cancel watch, a parent check) reads. The trait defaults it to `lease_until` + `load_run`; both
adapters override it with one projected read that never touches the state, and the conformance case
`run_header_matches_the_record` proves an implementation agrees with the two reads.

`PgStore::connect` sizes its pool from `ADAM_PG_POOL_SIZE` (2 to 65535, default 16, per process;
one is refused because it serializes every store call, and a migration holds its connection); 
`connect_with_pool_size` takes the size explicitly, `from_config` takes an already parsed
[`adam-postgres`](../../crates/adam-postgres/README.md) `PgConfig` (what `adam-service` does, so
`DATABASE_URL` is parsed once for the store and the notification listener), and `from_pool` uses the
`deadpool_postgres::Pool` you pass. A value that is not an integer in range fails the connect rather
than falling back to 16. TLS comes from `DATABASE_URL` (`sslmode`, `sslrootcert`, `sslcert`,
`sslkey`); there are no Cargo features for it any more.

`keeps_blobs` / `blob_stored` / `put_blob` / `get_blob` / `purge_blobs` keep a file artifact's bytes
**out of the run's state**, content-addressed by `BlobId` (their SHA-256, 64 lower-case hex; the digest is
computed in `adam-runtime`, which has the hash dependency). A commit names the digests it **adds**
(`RunUpdate.blobs`, a delta the store unions; a commit that names none keeps the earlier ones), and
`purge_finished` deletes a blob once no surviving run references it and it is older than the cutoff;
`purge_blobs(before)` is the same sweep on its own. `blob_stored(id)` is one indexed existence
check: the runtime checks it before it re-sends a file's bytes, so a file an earlier commit uploaded
is uploaded once. A store that keeps no blobs leaves the bytes in the state, exactly as before. **A hand-built `RunUpdate { .. }` literal stops compiling** when it
gains the field: add `blobs: Vec::new()` or use `RunUpdate::new`.

`keeps_bodies` / `body_list` / `gc_body` keep a run's **transcript** beside its head (ADR 0055). The
contract release writes the head, the append-only body generation and the tombstone on every commit
of a v5 run **except a refused-split demotion** (which writes the whole v4 state and no head, so the
transcript is never put back in a head); the step path joins the head with the rows its claim read,
and `deliver`, `cancel_run` and `park_again` write the head they read, so it stays current. A generation is
`(run, generation, idx)`; `RunUpdate.body` is `{generation, rows, writer}`, where `generation: None`
appends the rows after the head's `len` and `Some(new)` is a compaction. A duplicate slot is this
attempt's retry when the row's `writer` is this attempt. A row another attempt wrote is adopted when
its message is the one this attempt would write; foreign rows older than the orphan grace (5 minutes,
far longer than the insert→CAS gap of a live attempt — milliseconds; the default lease is 30 s) are
collected and the insert retried once; a live attempt with a different message is a `Conflict`. The
delta win holds across every commit of a v5 run: a failed transition writes the head it read and no
rows, and the next split appends. A commit with `head: None` leaves the stored head and its
`head_version` alone — the runtime passes the head for a delivery, a cancel and a park, so `None` is
only a v4 commit (a process of the expand release). `load_head` is the observer's read
(`Runtime::view`). MongoDB answers it with one **projected** `find_one` that carries the head and the
record's own fields, never `state`; PostgreSQL with one `SELECT` whose `state` column is
`CASE WHEN jsonb_typeof(head) = 'object' AND (head_version = version OR state = 'null'::jsonb)
THEN head ELSE state END` (a JSONB `null` is not a head, and a tombstoned run serves its head). A
current head transfers only the head, and a v4 state the run still carries (a transcript, file
bytes) is not read. A stale or absent head serves the state — the `state` bytes are read only there.
That is the path of a run before its first split, of an agent whose `split` returns `None` (it never
writes a head), of a run the expand release converted and then a v4 process committed over, and of a
**cancelled** v4 run. The trait default is `load_run`, which answers the same document in the
contract release; a store that keeps no head answers the whole state exactly as before. A
`head_version != version` is **stale**, not corrupt: the state is served when it is the v4 envelope
a v4 commit wrote (and the next step re-splits and rewrites the head), and a **tombstoned** run
(`state = null`, the head behind the version — a pre-S3 process failed a converted run) serves the
head, which is the only readable document. That comparison is the whole mixed-fleet story in the
read direction. A store that keeps no bodies (`keeps_bodies()` is `false`, the default) answers
`body_list` empty and ignores both fields, exactly as before. **A hand-built `RunUpdate { .. }`
literal stops compiling**: add `head: None, body: None` or use `RunUpdate::new`.

Creating a run with a deterministic id gives idempotent "fire once" semantics, for example one run per
cron tick across all replicas. A second message on a conversation that has an open run gets
`ConversationBusy`; resume the open run instead.

## How each adapter keeps the contract

| Guarantee | PostgreSQL 12+ (`tokio-postgres`) | MongoDB 5.0+ (standalone is enough) | SQLite 3.35+ (`rusqlite`, one file, one process) |
|---|---|---|---|
| Commit CAS | `UPDATE .. WHERE version = $n RETURNING` | `findOneAndUpdate({_id, version: n}, {$inc: {version: 1}})` | `UPDATE .. WHERE id = ? AND version = ? RETURNING`, inside one `BEGIN IMMEDIATE` |
| Journal first-writer-wins | `INSERT .. ON CONFLICT DO NOTHING`, read winner | `insertOne` on `_id = "<run>:<seq>"`; a duplicate key means an earlier writer won, and their entry is read back | `INSERT` on the primary key `(run_id, seq)`; a unique violation reads the winner, a foreign-key violation is `NotFound` |
| Exclusive claiming | `FOR UPDATE SKIP LOCKED` in one statement | read candidates, `updateMany` re-checking due/lease in the filter with a claim token, read back by token | one `BEGIN IMMEDIATE`: select due ids, update them, select the rows; one writer process needs no `SKIP LOCKED` |
| Busy runs never claimed | `AND id <> ALL($busy)` | `_id: { $nin: busy }` | `AND id NOT IN (?, …)` |
| Pinned claiming (`owner`, schema version 2) | `AND (owner IS NULL OR owner = $w)` and `owner = COALESCE(owner, $w)` | the same condition in the candidate and `updateMany` filters (a missing field is `null`) | `AND (owner IS NULL OR owner = ?)` and `owner = COALESCE(owner, ?)` |
| One open run per conversation | partial unique index | plain unique index on `open_key`; closed runs get `~<run id>`, so no partial or sparse index | partial unique index, the same predicate |
| Journal deleted with its run | `ON DELETE CASCADE` | journal first, then runs, in batches | `ON DELETE CASCADE` (`foreign_keys=ON`) |
| Push configs (`push_*`, schema version 3): put, claim, commit | `INSERT .. ON CONFLICT DO UPDATE` bumping `version`; one `FOR UPDATE SKIP LOCKED` claim; `UPDATE .. WHERE version = $n` | update-or-insert on `_id = "<run>:<id>"`; a loop of `findOneAndUpdate` claims, each atomic; `findOneAndUpdate({_id, version: n})` | `INSERT .. ON CONFLICT DO UPDATE` bumping `version`; one `BEGIN IMMEDIATE` claim; `UPDATE .. WHERE version = ? RETURNING` |
| Push configs deleted with their run | `ON DELETE CASCADE` | deleted with the journal by the purge | `ON DELETE CASCADE` |
| `list_runs` / `count_runs` (`ListTasks`), keyset on `(updated_at, id)` | `runs (agent, conversation_id COLLATE "C", updated_at DESC, id DESC) INCLUDE (status)`, a prefix is a range plus `starts_with`; a prefix page sorts the matching index entries (the status filter is answered from the index) | `(agent, conversation_id, updated_at, _id)`, and `(agent, status, conversation_id, updated_at, _id)` when a status filter is set: a prefix is an anchored escaped regex, so the index bounds the scan but the page still sorts the matches; a listing of one conversation is read in index order | `(agent, conversation_id, updated_at DESC, id DESC)` (BINARY collation is byte order), a prefix is a range plus `substr(conversation_id, 1, ?) = ?`; the keyset is `(updated_at < ? OR (updated_at = ? AND id < ?))`; `count_runs` ignores the position |
| State | `JSONB` (queryable with SQL) | real BSON document (queryable with dot paths) | JSON text (queryable with SQLite's JSON functions, not `JSONB` operators); the tombstone is the text `null` |
| Blobs (schema version 5, part 1): put, get | `INSERT .. ON CONFLICT (digest) DO NOTHING` into `blob`; one primary-key `SELECT` | `insertOne` on `_id = <digest>`; a duplicate key is a no-op; one `find_one` | `INSERT OR IGNORE` into `blob`; one primary-key `SELECT` |
| A commit's blob references | the CAS and `INSERT .. SELECT unnest($digests) ON CONFLICT DO NOTHING` in **one transaction** | one unordered `insertMany` **before** the CAS; a lost CAS leaves rows nothing names, deleted with the run's purge | the CAS and `INSERT OR IGNORE` in **one `BEGIN IMMEDIATE` transaction**; a lost CAS writes nothing |
| Blob GC | `DELETE .. WHERE created_at < $cutoff AND NOT EXISTS (a run_blob row)`, in batches; run references cascade with the run. The sweep and a commit's ref insert are **not one transaction**: under `READ COMMITTED` an uncommitted ref is invisible, so a sweep can collect a blob a commit is about to name; a later PostgreSQL change closes it with one data-modifying CTE | the same sweep in a loop; run references are deleted with the run. Its find-then-delete has the same uncommitted-ref window, closed by the insert, completeness check, then CAS ([ADR 0055](../decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md), decision 3) | `DELETE .. WHERE created_at < ? AND digest NOT IN (SELECT digest FROM run_blob)`, in the purge's transaction (or `purge_blobs`'s own); references cascade with the run |
| Blob references deleted with their run | `ON DELETE CASCADE` | deleted by the purge before the run document | `ON DELETE CASCADE` |
| Transcript rows (schema version 5, part 2): a commit's delta, `body_list`, `gc_body` | the CAS, the rows after the head's `len` and the refs are **one transaction** (`INSERT .. SELECT unnest($idx, $messages) ON CONFLICT DO NOTHING`, then the slots another writer owns are read: identical adopted, older than the 5-minute orphan grace collected and retried once, live and different `Conflict`). A compaction writes all rows under a new generation; the old one is collected an hour after the commit (`gc_body`) | a generation is append-only: one unordered `insertMany` of the rows after the old head's `len` **before** the CAS, the completeness check (`inserted + duplicate_keys == rows.len()`, a write-concern error aborts), and a lost CAS deletes the attempt's `writer` rows and refs. A duplicate foreign slot is adopted when its message matches, collected after the 5-minute orphan grace, else a `Conflict`. A compaction writes all rows under a new generation; the old one is collected an hour later (`gc_body`) | the CAS, the rows after the head's `len` and the refs are **one `BEGIN IMMEDIATE` transaction** (`INSERT OR IGNORE`, then the slots another writer owns are read: identical adopted, older than the 5-minute orphan grace collected and retried once, live and different `Conflict`). A compaction writes all rows under a new generation; the old one is collected an hour after the commit (`gc_body`) |
| The head (schema version 5, part 2; the contract release): `head`, `head_version`, `load_head`, and every read | `head` and `head_version` columns; the store patches the head's `body` to `{generation, len}`; `load_run`, `load_head`, `claim_due`, `open_run` and the listings select one `CASE` (`head` when it is an object and either `head_version == version` or the state is the tombstone, else `state`) as `state`. `Runtime::view` reads it: one lease read and one head read, no body. A v4 commit (the expand release) leaves both alone, so the head is stale and the state is served | `head` and `head_version` on the run document; the store patches the head's `body` to `{generation, len}`; `load_run`, the claim and the listings serve the head through `served_from_doc` (the current head, else `state`), and `load_head` is one projected `find_one` (the head and the record's fields, never `state`) with a second full read only when the head is stale. `Runtime::view` reads it, one projected query and no body on the current path | `head` and `head_version` columns; the store patches the head's `body` to `{generation, len}`; `load_run`, `load_head`, `claim_due`, `open_run_for_conversation` and the listings select one `CASE` (`head` when `json_type(head) = 'object'` and either `head_version == version` or `state = 'null'`, else `state`) as `state`; a commit with `head: None` leaves both alone; a JSON `null` head is not a head. `Runtime::view` reads it: one lease read and one head read, no body |
| Body rows deleted with their run | `ON DELETE CASCADE` | deleted by the purge with the journal and the references | `ON DELETE CASCADE` |
| `run_header`: a run without its state, with its lease | one primary-key statement selecting the header columns and `lease_until`, never `state` | one `find_one` projected to the header and `lease_until` | one primary-key statement selecting the header columns and `lease_until`, never `state` |
| Transactions | none held across calls; a commit that writes a head, a body row or blob references is one transaction (the run row locked `FOR UPDATE`, the version check, the rows, the refs and the CAS) | none (single-document writes only): the rows are written, checked for completeness, then the CAS | every change is one `BEGIN IMMEDIATE`; a commit that writes a head, a body row or refs does its whole work in it |
| Cross-process signals | `LISTEN`/`NOTIFY` via `adam-notify-postgres` | none: workers poll | none: one process per file, `LocalNotifier` in process |

## SQLite

`adam-store-sqlite` is one SQLite file, opened by one process
([ADR 0053](../decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md),
decision 2). It is what a test harness, a desktop or a client application uses when it has no
database server:

```rust
use std::sync::Arc;
use adam_core::{DynStore, Store};

let store = adam_store_sqlite::SqliteStore::open("/var/lib/adam/adam.sqlite")?;
store.migrate().await?;
let store: DynStore = Arc::new(store);
```

`open` takes an exclusive advisory lock on `<path>.lock` (on the file's real path, so a symlink and
its target share it) and refuses a second process with `StoreError::InvalidInput`. The operating
system releases the lock when the process exits, including a `SIGKILL`; `migrate` then clears every
lease it finds, because the lock proves no other process is alive. A killed process's runs are
claimable at once, not after a lease expires. `SCHEMA_VERSION` is 1 and a file stamped newer is
refused, never read.

The file is WAL; `foreign_keys=ON`; every change is one `BEGIN IMMEDIATE` transaction; every call
runs on a blocking thread, so the async runtime never waits on the file. A commit that writes a
head, body rows or blob references does its whole work in one transaction, as the other adapters do.
Time is whole milliseconds since the epoch (floored, as `truncate_ms` floors), and `state`/`head` are
JSON text.

`adam-service` picks it for `DATABASE_URL=sqlite://<path>`, and only for `ROLE=all`: a file holds one
process, so `ROLE=worker` and `ROLE=control-plane` are exit 78. Signals are in-process
(`LocalNotifier`), and there is no `notify` component. `sqlite:///abs/path` is absolute;
`sqlite://relative.db` and `sqlite://./relative.db` are relative to the working directory;
`sqlite://` with no path is exit 78.

**What it does not support**: a second process on the same file (the lock refuses it); signals across
processes (there is no `LISTEN`/`NOTIFY` equivalent, and a split role is refused); PostgreSQL's
`JSONB` operators (the state is text); TLS, roles or a connection pool (there is no server);
Windows and macOS (the lock is `flock`; the adapter is scoped to Unix). `ADAM_PG_POOL_SIZE` has no
effect on it. The conformance suite, the blob suite and the body suite run against it with no
environment variable, each case on its own file; `tests/process.rs` proves the lock and the killed
process's claim, and `tests/schema.rs` proves the newer-schema refusal and the corrupt-file class
([crate README](../../crates/adam-store-sqlite/README.md)).

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
* **Time** is `adam_core::time::Timestamp` (ADR 0057), a `jiff::Timestamp` whose serde bytes are the ones
  `chrono::DateTime<Utc>` wrote. Every store truncates it to milliseconds; PostgreSQL keeps it in `TIMESTAMPTZ`
  through `postgres-types`' `with-jiff-0_2` (microseconds since 2000, the bytes chrono wrote) and MongoDB in
  a BSON date, so the stored value is the same in every backend.
  Lease expiry uses the `now` the caller passes in: keep worker clocks in sync (NTP) and lease TTLs well above
  the expected skew.

## Adding a backend

Implement `Store` and add one line: `adam_store_testkit::store_conformance!(make_store);`, plus
`adam_store_testkit::blob_conformance!(make_store);` when the store keeps blobs and
`adam_store_testkit::body_conformance!(make_store);` when it keeps bodies. A `Notifier`
(Redis, NATS) runs `adam_notify_testkit::notifier_conformance!(make_pair);`. The `adam-store-adapter`
skill walks through it; adding a required method to a trait breaks implementers, so say so in the PR and
in the crate README.

A blob is global: a sweep deletes every unreferenced blob in the database, so the cases that read,
upload or sweep one must not run beside each other. The suite's blob cases and the cases that call
`purge_finished`/`purge_blobs` take a single-thread group (this repository's is
`.config/nextest.toml`, `[test-groups.store-global-sweeps]`) and the suite's `BLOB_SUITE` lock under
libtest. **A test that calls `purge_finished` or `purge_blobs` must carry `purge` in its name** (or
be listed in the group's filter), and a repository that runs its own store tests must put the same
cases in a single-thread group of its own.

**A new required type does the same**: `Timestamp` replaced `chrono::DateTime<Utc>` in
every `Store` signature (ADR 0057), so an implementation written before it must change with it.
