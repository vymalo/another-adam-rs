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
another process reaches a running step at its next poll. The
[`adam-notify-postgres`](../adam-notify-postgres/README.md) adapter needs a
PostgreSQL, so it fits when that is the store (or one on the side); otherwise
accept the poll latency.

## API at a glance

* `MongoStore::connect(uri, db)`: connect and use database `db`.
* `MongoStore::new(database)`: reuse your application's `Database` handle.
* `MongoStore::with_collection_prefix(prefix)`: collection-name prefix
  (default `adam_`; alphanumerics, `_` and `-`, at most 40 characters).
* `MongoStore::database()`, `SCHEMA_VERSION` (2).
* `codec::{json_to_bson, bson_to_json, encode_key, decode_key}`: the reversible key escaping
  (`$ref`, dotted, empty keys, NUL) applied to run state. Ordinary keys are
  stored verbatim, so `state.messages.0.role` is still a valid query path.
* The `Store` implementation: call `migrate()` once at boot.

```rust
use std::sync::Arc;
use adam_core::{DynStore, Store};

let store = adam_store_mongodb::MongoStore::connect(&uri, "myapp").await?;
store.migrate().await?;
let store: DynStore = Arc::new(store);
```

Every operation is a single-document atomic write, so a standalone `mongod`
is enough (no multi-document transactions). Integers above `i64::MAX` are
rejected with `StoreError::InvalidInput`. Time is truncated to milliseconds.
The guarantee table is in the [store adapters reference](../../docs/reference/store-adapters.md#how-each-adapter-keeps-the-contract).

*Unverified:* the "MongoDB 5.0+" floor is the oldest server CI runs against
(`conformance` job), not a documented driver guarantee. The `mongodb` driver
requirement is `3.9` (verified 2026-09-29, `Cargo.toml`).

## Schema version and the owner field

`SCHEMA_VERSION` is 2. Version 2 adds an `owner` field to run documents, set by the first pinned
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

`tests/conformance.rs` runs the shared suite and checks that a version 1 document (no `owner`
field) is claimable and then pinned; `src/codec.rs` has property
tests of the key escaping; `src/lib.rs` has unit tests of the error classes
(`driver_errors_are_classified_by_what_they_mean`,
`the_driver_error_is_kept_as_the_source`).

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
