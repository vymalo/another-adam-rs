# adam-store-postgres

`adam_core::Store` on PostgreSQL 18 or later through `tokio-postgres`, pooled by `deadpool-postgres`
([ADR 0053](../../docs/decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md),
decision 1: the driver changed, the schema and the statements did not).

## Where it sits

An **adapter** of the store port in [`adam-core`](../adam-core/README.md),
checked by [`adam-store-testkit`](../adam-store-testkit/README.md). Nothing in
`adam-core` or `adam-runtime` names it; a binary chooses it at composition
time (for example [`adam-coder`](../../bin/adam-coder/README.md)).

## API at a glance

* `PgStore::connect(url)`: a pool of `ADAM_PG_POOL_SIZE` connections (default 16, 2 to 65535, per
  process).
* `ADAM_PG_STATEMENT_TIMEOUT_MS` (0 to 86400000, unset = none): sets `statement_timeout` on the store's own pool in `from_config` (so `connect*` too), not on `from_pool` pools or the notify listener; a cancelled statement is a transient error (`57014`) and `migrate` is exempt (`SET statement_timeout = 0` on its own connection).
* `PgStore::connect_with_pool_size(url, n)`: the same with an explicit size (2 to 65535).
* `PgStore::connect_lazy(url)`: the same `ADAM_PG_POOL_SIZE` pool, opening no connection until first use, and `ping()` opens one, for a process that
  checks its own configuration before it needs the database (`adam-service`'s `serve`).
* `PgStore::from_config(config, n)`: an already parsed [`adam-postgres`](../adam-postgres/README.md)
  `PgConfig` (what `adam-service` does, so `DATABASE_URL` is parsed once for the store and the
  notification listener), opening no connection.
* `PgStore::from_pool(pool)`: reuse your application's `deadpool_postgres::Pool`.
* `PgStore::with_table_prefix(prefix)`: table-name prefix (default `adam_`,
  only `[a-z0-9_]`, at most 40 characters).
* `PgStore::pool()`, `SCHEMA_VERSION` (8), `MIN_SERVER_VERSION_NUM` (`180000`: `migrate()` refuses an older server).
* `create_run` and `commit_run` return the record with `state` set to `null`: the caller supplied the
  state, and the state is the largest column, so a `RETURNING` that echoed it would parse it back on
  every transition. `load_run`, `load_head`, the claim, `open_run_for_conversation` and the listings
  answer the **contract document**, the virtual column `served`: the head when it is a JSON object
  and either `head_version == version` or the state is the JSON `null` tombstone, the whole `state`
  otherwise (ADR 0055, the contract release). `journal_put` returns the stored payload (the loop
  decodes the step's outcome from it).
* `run_header` is one statement over the primary key that selects the header columns and
  `lease_until`, never `state`: a reader that only needs the run's own bookkeeping (a cancel watch,
  a parent check) does not pay for the largest column.
* `load_head` is the same one-statement read as `load_run` (the `served` column), never a body row
  (`Runtime::view` is one `lease_until` + one `load_head`). `body_list` reads one
  generation in order; `gc_body` collects superseded generations older than its cutoff. **A `Store`
  wrapper must forward `load_head`** (as it must forward `keeps_bodies`, `body_list` and
  `gc_body`): the trait default calls the wrapper's `load_run`, so a view through a wrapper that
  does not delegate silently reads the whole state and loses the head. The wrappers in
  [`adam-store-testkit`](../adam-store-testkit/README.md) forward them.
* `commit_run` writes the head, the body rows and the blob references **in the same transaction as
  the version CAS** (the run row is locked `FOR UPDATE` first). A commit that writes none of them is
  the single `UPDATE` it always was, so a delivery or a cancel pays no `BEGIN`.
* The `Store` implementation: call `migrate()` once at boot (idempotent, safe
  from every replica). On a schema that is already current it runs no DDL and takes no table
  lock, so a start cannot deadlock with live traffic (see "Schema version 3").

```rust
use std::sync::Arc;
use adam_core::{DynStore, Store};

let store = adam_store_postgres::PgStore::connect(&url).await?;
store.migrate().await?;
let store: DynStore = Arc::new(store);
```

Tables are `<prefix>runs` (state as `JSONB`), `<prefix>journal` (primary key
`(run_id, seq)`, `ON DELETE CASCADE`), `<prefix>push` (the A2A push-notification configs, primary
key `(run_id, id)`, `ON DELETE CASCADE`, schema version 3; a partial index on
`(agent, next_attempt_at)` for active configs is the claim scan; `config` holds the webhook
credentials as the client gave them), `<prefix>blob` and `<prefix>run_blob` (file bytes out of a
run, schema version 5 part 1: see below) and `<prefix>meta`. Claiming is
`FOR UPDATE SKIP LOCKED`, and it leaves out the runs the caller says it is stepping (`AND id <> ALL($busy)`,
see [`adam-core`](../adam-core/README.md#runs-the-caller-is-stepping)); one open run per conversation is a partial unique
index. No transaction is held open while agent code runs. `JSONB` cannot hold
`\u0000`: the adapter replaces it with U+FFFD before it writes
(`adam_core::storable`), as every adapter does, so a state or a journal payload is
the same value in every backend ([ADR 0051](../../docs/decisions/0051-a-nul-is-stored-as-the-replacement-character.md)).
The guarantee table is in the [store adapters reference](../../docs/reference/store-adapters.md#how-each-adapter-keeps-the-contract).

Time columns (`wake_at`, `sched_at`, `lease_until`, `created_at`, `updated_at`, `recorded_at` and
the push scheduling fields) are `TIMESTAMPTZ`. `adam_core::time::Timestamp` is a `jiff::Timestamp`
newtype, and `postgres-types`' `with-jiff-0_2` feature binds it natively: microseconds since 2000,
the same bytes chrono wrote ([ADR 0057](../../docs/decisions/0057-time-is-jiff-with-the-bytes-chrono-wrote.md)).
There is no bridge type: the store converts at the edge (`t.0` to bind, `Timestamp(at)` to read).

## Pool

`connect` sizes the pool from `ADAM_PG_POOL_SIZE` (2 to 65535, default 16) and
`connect_with_pool_size` from its argument; `from_pool` uses the pool you pass. The pool is **per
process**: N replicas open up to N × the size against one server. **One is refused, not accepted**:
a one-connection pool serializes every store call behind whichever query is running, and a
migration holds its connection for the length of the schema check, so a healthy process could not
answer. The `adam-notify-postgres` listener now opens its own connection and does not take one from
the pool, but the floor stays: two is one for a long statement and one for everything else, and the
default leaves room for concurrency. A value that is not an integer in range fails the connect with
`InvalidInput`; it never falls back to 16 silently.

## Schema version 3: the push table

`migrate()` also creates the index `ListTasks` reads runs by (schema version 4's `<prefix>runs_list_status`: `(agent, conversation_id COLLATE "C", updated_at DESC, id DESC) INCLUDE (status)`; a conversation prefix is a range over it plus an exact `starts_with`, and the status filter is answered from the index, not the heap). It creates `<prefix>push` and its index **before** the statements that lock `runs`
exclusively, so a migration takes its locks in the order a `push_put` does and the two cannot
deadlock (found by the conformance suite, whose cases all migrate). Claiming push configs is
`FOR UPDATE SKIP LOCKED` in one statement, progress is `UPDATE .. WHERE version = $expected`, and
putting an id again is `INSERT .. ON CONFLICT DO UPDATE` that bumps the version.

## Schema version 4: the listing index covers the status filter, online

The listing index is now `<prefix>runs_list_status`: the same keys as version 3's
`<prefix>runs_list`, with `INCLUDE (status)`, so the status filter is answered without a heap fetch.
A page is still ordered across the conversations of a prefix (that sort remains); the index keeps
the filtered-out rows out of it and out of the fetch. A **fresh** database gets the index in the
migration transaction, on an empty table.

A **version 3** database gets it from a step that runs *before* the migration transaction, because a
concurrent statement cannot run inside one. `CREATE INDEX CONCURRENTLY` builds the new index beside
the old one; only when it is valid does `DROP INDEX CONCURRENTLY` remove the version 3 index. Both
take `ShareUpdateExclusiveLock`, which reads and writes do not conflict with, so `runs` stays
readable and writable for the whole build. (The non-concurrent pair held `AccessExclusiveLock` for
the drop and `ShareLock` for the build inside the transaction, and every query queued behind the
build.) The step is idempotent: `IF NOT EXISTS` and a validity check (an INVALID index left by a
failed build is dropped and rebuilt) make a crash between the statements re-run cleanly, and the old
index is dropped only after the new one is valid, so a listing always has one. A query does not name
an index, and the new one has the same key columns, so a process of an older release keeps using it
across a rolling upgrade. The upgrade still runs the version 2 `ALTER TABLE .. ADD COLUMN IF NOT
EXISTS owner`, which takes `AccessExclusiveLock` for the moment it takes to see the column exists;
the long part, the index build, no longer does.

A start on a current schema runs **no DDL**. Before opening its transaction `migrate()` takes a
session-level advisory lock on one connection, checks that the meta table exists (`to_regclass`) and
reads `schema_version`; at `SCHEMA_VERSION` or above (an older binary on a newer database counts) it
closes that connection (which releases the lock) and returns. This matters because even a no-op
`ALTER TABLE .. ADD COLUMN IF NOT EXISTS` takes `AccessExclusiveLock` and `CREATE INDEX IF NOT
EXISTS` takes `ShareLock`, and clients lock `push` and `runs` in both orders (`push_put`: push, then
the run; a purge: runs, then the cascaded push rows), so a start beside live traffic deadlocked, and
every restart took an exclusive lock on `runs`. Only an older or missing schema runs the statements.
The migration connection is always closed rather than pooled, so the session lock cannot outlive it.

## Schema version 5, part 1: file bytes out of the run

`<prefix>blob` is global and content-addressed: `digest` (64 lower-case hex, a SHA-256) is the
primary key, `bytes` is `BYTEA`, `size` is checked against `octet_length(bytes)`, and `created_at`
feeds the GC index `<prefix>blob_gc`. `<prefix>run_blob` is the append-only set of digests one run
references (`(run_id, digest)`, `ON DELETE CASCADE`, index `<prefix>run_blob_digest` for the GC's
"does any run still name this digest"). `put_blob` is `INSERT .. ON CONFLICT (digest) DO NOTHING`
(a retried put writes nothing), and a commit writes its references **in the same transaction as the
CAS**: a commit that loses the version check writes none.

A version 4 database upgrades through `upgrade_blobs`, its own step gated on `stored == Some(4)`,
run after the listing-index step. It is metadata-only: two new tables, their indexes and a foreign
key from a table that is empty on a version 4 database, so no row of `runs` is rewritten. A version
4 start does not re-check the version 4 listing index. A version 3 (or older) database runs the
listing-index step and gets the blob tables from the migration transaction that follows; the blob
step itself only runs from version 4.

`blob_stored(id)` is one `SELECT EXISTS` on the digest: the runtime checks it before it re-sends a
file's bytes, so a file an earlier commit uploaded is uploaded once. `purge_finished` sweeps
unreferenced blobs written before its cutoff in the same call (the run's references cascaded away),
and `purge_blobs(before)` is the explicit knob for a host that reclaims bytes without purging runs. A blob a surviving run references is kept, however old it is.

`RunUpdate` gained the public field `blobs` — a delta the store unions; a commit that names none
keeps the earlier ones — so **a hand-built `RunUpdate { .. }` literal stops compiling**: add
`blobs: Vec::new()` or use `RunUpdate::new(..)`/`.blobs(..)`. The four blob methods are
**defaulted** on `Store`, so a store of your own that keeps no blobs still compiles and behaves as
before.

## Schema version 5, part 2 (schema version 6): the head and the body

`<prefix>runs.head` is the v5 head and `<prefix>runs.head_version` the version it was written at;
`<prefix>body` is one row per transcript message, keyed `(run_id, generation, idx)`, with the
`writer` and `written_at` the duplicate rule needs, `ON DELETE CASCADE`. `SCHEMA_VERSION` is 6. The
upgrade from a version 5 database is its own step gated on `stored < 6` (not on the blob step: a
database stamped 5 already has the blob tables but no head or body) and is metadata-only: two
nullable columns and one table.

The contract release reads the same document from `load_run`, `load_head`, `claim_due`,
`open_run_for_conversation` and `list_runs` (since schema version 8 the `served` column, before it a
`CASE` each read repeated): the head when it is current, the whole state otherwise. `commit_run` with a head, a body or blob references is **one transaction**: the run row is locked
`FOR UPDATE`, the version is checked, the rows after the head's `len` are inserted
(`INSERT .. SELECT unnest(..) ON CONFLICT DO NOTHING`), the slots another writer owns are read
(an identical message is adopted; one older than the 5-minute orphan grace is collected and the
insert retried once; a live attempt's differing row is a `Conflict`), the refs are inserted, the CAS
runs, and the commit ends. The head's `body` is patched to the resolved `{generation, len}`. The
superseded generation is collected after the CAS, an hour of grace. A commit that loses writes
nothing (the transaction rolls back), so a loser cannot poison the next append. Every read serves the head when it is current; a head behind the version is not corrupt: the state
is served when it is the v4 envelope a v4 commit wrote (and the next split rewrites the head), and
the head is served when the state is the tombstone (a run a pre-S3 process failed).

## Schema version 7: the functions, and PostgreSQL 18

[ADR 0082](../../docs/decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md) moves the
store's rules into database functions, slice by slice. Slice P0 built the step they arrive in and
the floor, and **creates no function yet**:

* **The floor.** `migrate()` reads `server_version_num` first and refuses a server older than 18.0
  (`MIN_SERVER_VERSION_NUM`) with `StoreError::InvalidInput`, class `Invalid`, so a binary exits 78.
  The check runs before the advisory lock and before any DDL: a refused database holds nothing new.
  PostgreSQL 12 to 17, supported until this release, must be upgraded before it.
* **The step.** The functions are statements of the migration transaction, after every table they
  name and before the version row, each sent whole by `batch_execute` (one simple-protocol message,
  so a `BEGIN ATOMIC` body is parsed by the server and never split on `;`). Each is
  `CREATE OR REPLACE FUNCTION <prefix><name>`. A start on a current schema still runs no DDL, so a
  slice that adds, changes or drops a function **bumps `SCHEMA_VERSION`** (P0 is 7, the next is 8),
  and a function whose contract changes gets a new name, the old one dropped a release later.
  A version 6 database gets version 7 from the same migration transaction; nothing is rewritten.
* **The refusals.** A function refuses with one of adam's own SQLSTATEs, `AD001` to `AD004`; their
  classes are the table in [`adam-postgres`](../adam-postgres/README.md#adams-own-sqlstates), and the
  store builds the `StoreError` of each (below, *Errors*). No function raises one yet.

## Schema version 8: the served document is a column

Slice P1 of ADR 0082 (decision 12) writes the rule of which document a read serves **once, beside
the data**: `<prefix>runs.served` is a virtual generated column,

```sql
served JSONB GENERATED ALWAYS AS (
  CASE WHEN jsonb_typeof(head) = 'object'
            AND (head_version = version OR state = 'null'::jsonb)
       THEN head ELSE state END) VIRTUAL
```

and `load_run`, `load_head`, the claim, `open_run_for_conversation` and `list_runs` select it,
inline, as plain statements. It is computed when it is read: it stores nothing, cannot be written,
and adding it rewrites no row (a version 7 database keeps its table file; `tests/migrate.rs` checks
the file node). It is one `ALTER TABLE .. ADD COLUMN IF NOT EXISTS` at the end of the table
statements, for a fresh and an upgraded database alike, so both end with the same columns. A
process of the previous release never names the column, so a mixed fleet reads and writes as
before; this release needs it, so it reads only after `migrate()` (as every release that adds a
column). The column depends on `head`, `head_version`, `version` and `state`: a later migration
that alters one of them drops and recreates it in the same transaction. P1 creates no function.

**Changing the expression later.** `ADD COLUMN IF NOT EXISTS` never touches an existing column, so
editing the expression in the code would leave every version 8 database on the old one. A change
bumps `SCHEMA_VERSION` and adds `ALTER TABLE <prefix>runs ALTER COLUMN served SET EXPRESSION AS (..)`
to the migration; on a virtual column that rewrites no row (*verified 2026-11-27* on 18.7: the file
node is unchanged and the new expression is served). `tests/migrate.rs` checks that the stored
expression (`pg_get_expr`) is the expected one after an upgrade.

## Schema version and the owner column

The owner column is schema version 2. Version 2 adds `<prefix>runs.owner TEXT`, the worker a pinned claim
(`ClaimScope::Pinned`, see [`adam-core`](../adam-core/README.md#claim-scope-and-the-run-owner))
tied the run to. `migrate()` runs `ALTER TABLE .. ADD COLUMN IF NOT EXISTS owner TEXT`, so a
database made by version 1 upgrades in place and keeps its runs (they have no owner), then raises
the `schema_version` row to 2. It never lowers it: a process of an older release that migrates
against a newer database leaves the number alone. The pinned claim adds
`AND (owner IS NULL OR owner = $worker)` to the claiming statement and
`owner = COALESCE(owner, $worker)` to its `UPDATE`; `release_lease` does not touch `owner`. The
`Any` claim has no owner filter and never writes `owner`. There is no index on `owner`: the `runs_due` partial
index does the range scan and the owner is a filter on those rows.

## Errors

Failures are `adam_core::StoreError` (see [`adam-core`](../adam-core/README.md#errors)
and [`adam-error`](../adam-error/README.md)). A driver error becomes
`StoreError::Backend { class, source }`, with the `tokio_postgres::Error` as the source
and this adapter's choice of class ([`adam-postgres`](../adam-postgres/README.md) owns the mapping):

| Driver error | Class |
|---|---|
| I/O, TLS, a closed connection, a pool wait timeout or a closed pool | `Transient` |
| database error with SQLSTATE class `08` or `53`, or `40001`, `40P01`, `57P01`, `57P02`, `57P03` | `Transient` |
| a value or column that cannot be decoded, a missing column | `Corrupt` |
| a bad connection string, a missing certificate file, a server older than PostgreSQL 18 (at `migrate()`) | `Invalid` |
| any other database error (a rejected statement) and anything else | `Internal` |

adam's own SQLSTATEs (ADR 0082, decision 4; the table is `adam_postgres::AdamSqlState`), raised by
the store's functions, become these variants; a code that reaches `classify` without its call's
context keeps the class of the table:

| SQLSTATE | `StoreError` | Class |
|---|---|---|
| `AD001` | `InvalidInput` (the error's message) | `Invalid` |
| `AD002` | `NonDeterminism`, the recorded step's name from the error's `DETAIL` | `Corrupt` |
| `AD003` | `Corrupt` (the error's message) | `Corrupt` |
| `AD004` | `Conflict`, the version unchanged | `Conflict` |

One documented change from the `sqlx` mapping: `sqlx`'s `Protocol` and `WorkerCrashed` were
`Transient`; `tokio-postgres` has no equivalent, so what is not a database error, a closed
connection or an I/O/TLS failure is `Internal`.

A poisoned row is therefore `Corrupt`, not retryable and alerting, so the
runtime fails that run instead of re-leasing it for ever. The adapter's own
checks are `InvalidInput` (an integer above `i64::MAX`, a bad table prefix) and `Corrupt` (an unknown status, a negative version or `seq`, a
journal entry that vanished). *Unverified:* the SQLSTATE codes are from the
PostgreSQL manual's error-code appendix, recalled from memory, not re-checked.

## TLS

There are no Cargo features: TLS is always `rustls` on `aws-lc-rs`, and `DATABASE_URL` chooses the
mode (`sslmode`, `sslrootcert`, `sslcert`, `sslkey`; see
[`adam-postgres`](../adam-postgres/README.md#tls)). The old `tls-rustls`/`tls-native-tls` features
are gone with `sqlx`; a consumer that set them must remove the line.

## Tests

`tests/conformance.rs` runs the shared suite against a real server.
`tests/errors.rs` checks the classes: an undecodable row is `Corrupt` and not
retryable, a unique violation stays `AlreadyExists`, a lost connection and a
serialization failure are `Transient`, a bad statement is `Internal` (needs the
server), and an unreachable server is `Transient` (offline).
`tests/fixtures/schema-v5.sql` is the schema exactly as the `sqlx` build emitted it: `tests/fixture.rs`
applies it with `batch_execute`, opens the store and takes a create/load round trip with **no
migration**, then migrates to the current version and commits a head and a body. `tests/fixtures/schema-v8.sql` is the
current schema; a unit test (`the_statements_equal_the_version_8_fixture`) compares the live
statements with it whitespace-normalised, so they cannot drift; `ADAM_WRITE_FIXTURE=1` regenerates it.
The fixture is named for its version: a change to a statement, a function's included, without a bump
of `SCHEMA_VERSION` shows as an edit of a released version's file.
`tests/functions.rs` is ADR 0082's fresh-database test: it migrates a database of its own prefix,
compares the functions in `pg_proc` with its list `FUNCTIONS` (empty at slices P0 and P1; a slice that adds a
function adds it there with a call) and calls each; it also checks that the comparison reports an
unlisted and a missing function, and that a version 6 database gets the functions. A unit test
raises the floor above the server's own version and checks that `migrate()` is refused `Invalid`
with no object of the prefix left behind; `a_server_below_18_is_refused_before_any_ddl` does the same
against a real older server when `ADAM_TEST_POSTGRES_BELOW_FLOOR_URL` names one (CI runs none, so
it skips there).
`tests/body_writers.rs` writes a foreign body row directly and proves the three resolutions (adopted,
collected after the orphan grace, `Conflict`), and `tests/head_view.rs` counts the calls
`Runtime::view` makes (one lease, one head, no body) and `Runtime::state`'s (one head, one body).
`tests/migrate.rs` builds a version 1 schema by hand with a legacy run, migrates it and checks the
column, the version row, that the run survived and that pinning works on it; it also checks that an
older release does not lower the version. A version 2 schema migrates to the current one and gets the
push table, a version 3 schema's `runs_list` is replaced by `runs_list_status` (valid, old one
gone), a version 4 schema gets the blob tables and keeps its runs, a version 7 schema gets the
virtual `served` column without a rewrite (the same columns as a fresh table, its runs served as
before), and a start on a current schema
finishes while another connection holds `ROW EXCLUSIVE` locks on `runs` and `push` (it runs no
DDL). A unit test
(`the_listing_index_upgrade_does_not_wait_for_live_traffic`) holds a writer's lock open and writes
beside the upgrade, which fails if the rebuild ever queues for `AccessExclusiveLock` again.
`tests/time.rs` proves the wire: a `TIMESTAMPTZ` is microseconds since 2000 (read as PostgreSQL's
own `i64`), a `NULL` stays `NULL`, an out-of-range value is `Corrupt`, and a sub-millisecond cutoff
keeps its microseconds. Unit tests in `src/lib.rs` cover the pool-size rule, the floor and the
variant of each `AD` code, and provoke real SQLSTATEs (PostgreSQL's and adam's) when the server
variable is set.

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL` | server to test against; unset means the suite is skipped |
| `ADAM_TEST_TABLE_PREFIX` | table prefix of the test store (default `adam_test_`) |
| `ADAM_TEST_REQUIRE_DB` | `1`: an unset URL fails instead of skipping (CI sets it) |
| `ADAM_TEST_POSTGRES_BELOW_FLOOR_URL` | a server older than 18, for the refusal test; unset skips it, even under `ADAM_TEST_REQUIRE_DB` |

```sh
docker compose up -d   # from the repository root
ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
  cargo test -p adam-store-postgres
```

CI runs it against PostgreSQL 18, the floor, with MongoDB 5.0 and with 8.0 (`conformance` job in
`.github/workflows/ci.yml`; the image is pinned by tag and digest there).

## See also

[`adam-store-mongodb`](../adam-store-mongodb/README.md), the other adapter;
[`adam-error`](../adam-error/README.md).
