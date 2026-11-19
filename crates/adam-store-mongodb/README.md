# adam-store-mongodb

`adam_core::Store` on MongoDB 5.0+ through the official driver.

## Where it sits

An **adapter** of the store port in [`adam-core`](../adam-core/README.md),
checked by [`adam-store-testkit`](../adam-store-testkit/README.md). It is the
alternative to [`adam-store-postgres`](../adam-store-postgres/README.md); a
binary picks one at composition time.

## Cross-process signals

There are none. A standalone `mongod` has no change streams (they need a replica
set), and this adapter promises to work on one, so workers of other processes
find new runs by polling `claim_due` (`poll_interval`), and a cancel issued by
another process reaches a running step at its next cancel poll
(`cancel_poll_interval`, 1 s by default). The
[`adam-notify-postgres`](../adam-notify-postgres/README.md) adapter needs a
PostgreSQL, so it fits when that is the store (or one on the side); otherwise
accept the poll latency.

## API at a glance

* `MongoStore::connect(uri, db)`: connect and use database `db`.
* `MongoStore::new(database)`: reuse your application's `Database` handle.
* `MongoStore::with_collection_prefix(prefix)`: collection-name prefix
  (default `adam_`; alphanumerics, `_` and `-`, at most 40 characters).
* `MongoStore::database()`, `SCHEMA_VERSION` (5).
* `create_run` and `commit_run` return the record with `state` set to `null`: the caller supplied the
  state, and returning it would convert the document back on every transition. `load_run`, the claim
  and the listings return the stored state; `journal_put` returns the stored payload (the loop
  decodes the step's outcome from it).
* `run_header` is one `find_one` projected to the header and `lease_until`: the state is never read,
  so a reader that only needs the run's own bookkeeping (a cancel watch, a parent check) pays for one
  small document.
* `codec::{json_to_bson, bson_to_json, encode_key, decode_key}`: the reversible key escaping
  (`$ref`, dotted, empty keys, NUL). Ordinary keys are stored verbatim, so
  `state.messages.0.role` is still a valid query path. The store replaces a NUL with U+FFFD before
  the codec (`adam_core::storable`), so the NUL escape is for data written before ADR 0051 and for
  direct users of the codec.
* The `Store` implementation: call `migrate()` once at boot. It replaces a NUL
  with U+FFFD before the codec (`adam_core::storable`), as every adapter does
  ([ADR 0051](../../docs/decisions/0051-a-nul-is-stored-as-the-replacement-character.md)).

A commit reads the current document projected to the metadata it needs (`agent`, `conversation_id`,
the version, the parent and the creation time): it never reads the state it is about to replace.
`journal_put` inserts the entry and, when the run still exists, returns it as it was written; a
duplicate key means an earlier writer won, and their entry is read back. A journaled step is two
round trips rather than the four of read, check, insert, check. `lease_until` reads one field.

```rust
use std::sync::Arc;
use adam_core::{DynStore, Store};

let store = adam_store_mongodb::MongoStore::connect(&uri, "myapp").await?;
store.migrate().await?;
let store: DynStore = Arc::new(store);
```

Every operation is a single-document atomic write, so a standalone `mongod`
is enough (no multi-document transactions). Integers above `i64::MAX` are
rejected with `StoreError::InvalidInput`. A NUL in a string or a key is stored
as U+FFFD (`adam_core::storable`), as in every adapter. Time is truncated to
milliseconds.
The guarantee table is in the [store adapters reference](../../docs/reference/store-adapters.md#how-each-adapter-keeps-the-contract).

*Unverified:* the "MongoDB 5.0+" floor is the oldest server CI runs against
(`conformance` job), not a documented driver guarantee. The `mongodb` driver
requirement is `3.9` (verified 2026-09-29, `Cargo.toml`).

## Schema version 3: the push collection

`<prefix>push` holds the A2A push-notification configs: `_id` is `"<run id>:<config id>"`, with
`run_id`, `agent`, `owner`, `config` (the webhook credentials as the client gave them) and
`cursor` as BSON, the scheduling fields (`state`, `next_attempt_at`, `lease_until`) and `version`.
Indexes: `(agent, state, next_attempt_at)` for claiming and `(run_id, id)` for listing. Runs get `adam_list`
(`agent, conversation_id, updated_at -1, _id -1`) for `list_runs`, which matches a conversation prefix with an anchored,
escaped regular expression: a listing of one conversation is read in index order, while a prefix is a range over the
index, so its page is a blocking sort of the matching runs. Claiming is
a loop of `findOneAndUpdate` (each atomic on its document, so two workers never lease one config),
progress is `findOneAndUpdate({_id, version: expected})`, and the purge deletes a purged run's
configs after its journal. Still single-document writes only, so a standalone `mongod` is enough.

## Schema version 4: the status-filtered listing index

`adam_list_status` (`agent, status, conversation_id, updated_at -1, _id -1`) is the same listing with a status filter.
`status` is an equality bound before `conversation_id`, so the index yields the runs of that status and the prefix is a
range inside it: a page fetches its own rows instead of every candidate the filter excludes (`explain` on 2 000 runs:
10 documents examined for a page of 10, against 500 fetched to test the filter without it). It serves the order of a
listing of **one conversation** (`agent`, `status` and `conversation_id` are equalities, so `updated_at, _id` is the
index's order and no sort runs); a **prefix** listing — the default `ListTasks` path — still sorts its matches, because
the range on `conversation_id` breaks the order. The index bounds that sort to the matching entries and keeps the state
of the ones it drops out of it. Neither listing index carries `purging`: a tombstoned run is left out of `commit_run`,
and a listing may show one until the sweep deletes it — up to a batch of deletes, or the next sweep after a crash.

`ensure_indexes` matches an existing index **by name only**, so a future change to `adam_list_status`'s keys needs a new
name: reusing the name would leave every existing database on the old keys, silently.

## Schema version 5, part 1: file bytes out of the run

`<prefix>blob` holds the bytes: `_id` is the digest (64 lower-case hex, a SHA-256), `bytes` is a
binary, `size` its length and `created_at` when it was first written; the GC index `adam_blob_gc`
is `{created_at: 1}`. `<prefix>run_blob` is the append-only set of digests one run references,
`_id = "<run>:<digest>"` with `run_id` and `digest`, indexed by `adam_run_blob_digest` (the GC's
"does any run name this digest") and `adam_run_blob_run` (a purge batch's "which digests does this
run name"). A `put_blob` is one `insertOne`; a duplicate key is a no-op, because the bytes behind a
digest never change.

A commit inserts its references **before** the CAS, in one unordered `insertMany` (duplicates
ignored): there is no cross-document transaction, so a commit that loses the CAS leaves rows nothing
names, and they are deleted with the run's purge. A commit whose run was purged between the refs and
the CAS removes them itself.

`purge_finished` deletes the purged runs' references and sweeps unreferenced blobs written before its
cutoff; `purge_blobs(before)` is the explicit knob. The sweep pages candidates by `_id`, so a batch
that is entirely referenced still moves on, and a digest a surviving run names is kept however old
it is. Nothing is rewritten by the migration: a version 4 run document has no blob field at all.

`RunUpdate` gained the public field `blobs` — a delta the store unions; a commit that names none
keeps the earlier ones — so **a hand-built `RunUpdate { .. }` literal stops compiling**: add
`blobs: Vec::new()` or use `RunUpdate::new(..)`/`.blobs(..)`. The four blob methods are
**defaulted** on `Store`, so a store of your own that keeps no blobs still compiles and behaves as
before.

## Schema version and the owner field

The owner field is schema version 2. Version 2 adds an `owner` field to run documents, set by the first pinned
claim (`ClaimScope::Pinned`, see
[`adam-core`](../adam-core/README.md#claim-scope-and-the-run-owner)). No data is rewritten: a
document without the field reads as `owner: null` (`{ owner: null }` matches a missing field), so
version 1 documents are unowned. `migrate()` only raises the `schema_version` document to 2
(`$lt` guard, never lowered). The pinned claim adds `$and: [{ $or: [{ owner: null }, { owner:
worker }] }]` to the candidate filter **and** to the `updateMany` filter, which MongoDB
re-evaluates per document under its write lock, and `$set`s `owner` in that same `updateMany`. So
two workers cannot both take an unowned run, and the owner is written atomically with the lease.
`release_lease` leaves `owner` alone.

The runs the caller says it is stepping (`busy`, see
[`adam-core`](../adam-core/README.md#runs-the-caller-is-stepping)) are left out of the candidate
filter with `_id: { $nin: busy }`; the `updateMany` then selects by the ids of the candidates, so it
cannot take one either.

## Errors

Failures are `adam_core::StoreError` (see [`adam-core`](../adam-core/README.md#errors)
and [`adam-error`](../adam-error/README.md)). A driver error becomes
`StoreError::Backend { class, source }`, with the `mongodb` error as the source
and this adapter's choice of class:

| Driver error | Class |
|---|---|
| labelled `TransientTransactionError` or `RetryableWriteError`; I/O, DNS, pool cleared, server selection, transaction | `Transient` |
| a reply that cannot be decoded (BSON deserialization, invalid response) | `Corrupt` |
| authentication failed | `Unauthenticated` |
| invalid argument, invalid TLS configuration | `Invalid` |
| any other rejected command | `Internal` |

The adapter's own checks are `InvalidInput` (an integer above `i64::MAX`, a bad
collection prefix) and `Corrupt` (a non-finite number in stored state, a
malformed escaped key, a journal entry that vanished).

## Features

None.

## Tests

`tests/conformance.rs` runs the shared suite and the blob suite, and checks that a version 1
document (no `owner` field) is claimable and then pinned; `src/codec.rs` has property
tests of the key escaping; `src/lib.rs` has unit tests of the error classes
(`driver_errors_are_classified_by_what_they_mean`,
`the_driver_error_is_kept_as_the_source`), of what a write returns
(`returned_value_tests`: the cleaned value it stored, without converting the stored document
back to JSON) and of the round trips a journal entry costs (`round_trip_tests`, counting the
driver's command events).

| Variable | Meaning |
|---|---|
| `ADAM_TEST_MONGODB_URI` | server to test against; unset means the suite is skipped |
| `ADAM_TEST_MONGODB_DB` | database name (default `adam_test`) |
| `ADAM_TEST_REQUIRE_DB` | `1`: an unset URI fails instead of skipping (CI sets it) |

```sh
docker compose up -d   # from the repository root
ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 cargo test -p adam-store-mongodb
```

CI runs it against MongoDB 5.0 and 8.0.

## See also

[`adam-store-postgres`](../adam-store-postgres/README.md), the other adapter;
[`adam-error`](../adam-error/README.md).
