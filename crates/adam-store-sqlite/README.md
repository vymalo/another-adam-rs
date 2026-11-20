# adam-store-sqlite

`adam_core::Store` on one SQLite file, for a test harness, a desktop or a client application that
has no database server ([ADR 0053](../../docs/decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md),
decisions 2 and 3; it builds [ADR 0047](../../docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md)
decision 3 in the main workspace).

## Where it sits

An **adapter** of the store port in [`adam-core`](../adam-core/README.md), checked by
[`adam-store-testkit`](../adam-store-testkit/README.md). Nothing in `adam-core` or `adam-runtime`
names it; [`adam-service`](../adam-service/README.md) picks it when `DATABASE_URL` is
`sqlite://<path>`.

## One file, one process

```rust
use std::sync::Arc;
use adam_core::{DynStore, Store};

let store = adam_store_sqlite::SqliteStore::open("/var/lib/adam/adam.sqlite")?;
store.migrate().await?;
let store: DynStore = Arc::new(store);
```

`open` takes an exclusive advisory lock on `<path>.lock` (an exclusive `flock`, named after the
file's real path so a symlink and its target share it), so a second process on the same file is
refused with `StoreError::InvalidInput` and a message that names the lock. The operating system
releases the lock when the process exits, **including a `SIGKILL`**; `migrate` then clears every
lease it finds, because the lock proves no other process is alive. A killed process's runs are
therefore claimable at once, without waiting for a lease to expire.

`sqlite://` with `ROLE=worker` or `ROLE=control-plane` is refused by `adam-service` (exit 78): a
split deployment needs a store several processes share. Use `ROLE=all`, Postgres, MongoDB or the
store service.

`SqliteStore::open(":memory:")` is supported for a test that wants no file; it has no lock and no
WAL, and is not durable.

## API at a glance

* `SqliteStore::open(path)`: one file, WAL, `foreign_keys=ON`, a 5 s busy timeout, and the process
  lock. `:memory:` is an in-memory database.
* `with_table_prefix(prefix)`: table names prefixed (default `adam_`, only `[a-z0-9_]`, at most 40
  characters), so two stores can share a file (they still share the one process lock).
* `path()`: the file. `ping()`: `SELECT 1`. `SCHEMA_VERSION` (1).
* The `Store` implementation: `create_run` and `commit_run` return the record with `state` set to
  `null`; `load_run`, `load_head`, `claim_due`, `open_run_for_conversation` and the listings return
  the same document — the head when `head_version == version` (or when the state is the tombstone),
  the whole state otherwise; `journal_put` returns the stored payload. `run_header` is one projected
  read.
* `keeps_blobs()` and `keeps_bodies()` are `true`: the five blob methods and the three body
  methods are implemented, and the crate passes `blob_conformance!` and `body_conformance!`.

## Concurrency

Every call runs on a blocking thread (`tokio::task::spawn_blocking`) and takes the one connection
behind a mutex, so the async runtime never waits on the file. Each change is one `BEGIN IMMEDIATE`
transaction. A commit that writes a head, body rows or blob references does the version check, the
rows and the compare-and-swap in that transaction. SQLite has no `FOR UPDATE SKIP LOCKED`; one
writer process does not need it.

A body row another attempt wrote is adopted when its message is the one this attempt would write,
collected and retried once when it is older than the five-minute orphan grace, and a live
different row is a `Conflict`. A superseded body generation is collected an hour after the commit
that replaced it; `gc_body` and `purge_finished` collect the rest.

## Schema and versions

The tables are the PostgreSQL ones with SQLite types: `<prefix>runs` (`state` and `head` are JSON
text, timestamps are whole milliseconds since the epoch), `<prefix>journal`, `<prefix>push`,
`<prefix>blob`, `<prefix>run_blob`, `<prefix>body` and `<prefix>meta`. One open run per
conversation is a partial unique index. A listing is a keyset page on `(updated_at, id)`.

`SCHEMA_VERSION` is in `<prefix>meta`. `migrate` is idempotent and refuses a file stamped with a
**newer** version (`StoreError::InvalidInput`, exit 78) instead of reading a schema this binary
does not know. It never lowers the stamp.

Time is truncated to milliseconds with `adam_core::truncate_ms` before it is written, as every
store does; a value before 1970 is floored, never rounded toward zero. Every JSON value goes
through `adam_core::storable` (a NUL becomes U+FFFD).

## What it does not support

* **More than one process per file.** The lock refuses it; a deployment that needs replicas uses
  Postgres, MongoDB or the store service.
* **Signals across processes.** There is no `LISTEN`/`NOTIFY` equivalent: `adam-service` gives the
  runtime a `LocalNotifier`, and another process would have to poll. There is no other process.
* **`JSONB` SQL.** `state` is text: query it with SQLite's JSON functions, not PostgreSQL's.
* **Windows and macOS.** The lock is `flock`; the ticket scopes those platforms out.
* **A server's TLS, roles and pools.** There is no network, no credential and no pool.

## Errors

A driver error becomes `StoreError::Backend { class, source }` with the `rusqlite::Error` as its
source:

| Driver error | Class |
|---|---|
| `SQLITE_BUSY`, `SQLITE_LOCKED`, an interrupted operation, an I/O failure, a full disk, a schema change | `Transient` |
| `SQLITE_CORRUPT`, `SQLITE_NOTADB`, a type mismatch, a column that cannot be decoded | `Corrupt` |
| `SQLITE_READONLY`, `SQLITE_CANTOPEN`, a permission error | `Invalid` |
| a constraint this adapter did not map, a misuse, anything else | `Internal` |

The adapter's own refusals are `InvalidInput` (the lock, a newer schema, a bad prefix, a body index
that is not its position) and `Corrupt` (a file that is not a SQLite database, an unknown status, a
negative version, a row that vanished). A unique violation on a create is `AlreadyExists` or
`ConversationBusy`; a foreign-key violation is `NotFound`.

## Tests

* `tests/conformance.rs`: `store_conformance!`, `blob_conformance!` and `body_conformance!`, each
  case on its own file under `CARGO_TARGET_TMPDIR` (no database server and no environment
  variable; the suite never skips).
* `tests/process.rs`: a second process is refused, and a child killed with `SIGKILL` leaves a run
  this process can claim at once after `migrate`.
* `tests/schema.rs`: a file stamped with a newer schema is refused and left untouched, an older one
  upgrades, and a file that is not a SQLite database is `Corrupt`.

```sh
cargo test -p adam-store-sqlite
```

## See also

[`adam-store-postgres`](../adam-store-postgres/README.md) and
[`adam-store-mongodb`](../adam-store-mongodb/README.md), the shared adapters;
[`adam-store-testkit`](../adam-store-testkit/README.md); the [store adapters
reference](../../docs/reference/store-adapters.md).
