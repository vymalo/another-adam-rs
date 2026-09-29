# adam-store-postgres

`adam_core::Store` on PostgreSQL 12+ through `sqlx`.

## Where it sits

An **adapter** of the store port in [`adam-core`](../adam-core/README.md),
checked by [`adam-store-testkit`](../adam-store-testkit/README.md). Nothing in
`adam-core` or `adam-runtime` names it; a binary chooses it at composition
time (for example [`adam-coder`](../adam-coder/README.md)).

## API at a glance

* `PgStore::connect(url)`: default pool (16 connections).
* `PgStore::from_pool(pool)`: reuse your application's `PgPool`.
* `PgStore::with_table_prefix(prefix)`: table-name prefix (default `adam_`,
  only `[a-z0-9_]`, at most 40 characters).
* `PgStore::pool()`, `SCHEMA_VERSION`.
* The `Store` implementation: call `migrate()` once at boot (idempotent, safe
  from every replica).

```rust
use std::sync::Arc;
use adam_core::{DynStore, Store};

let store = adam_store_postgres::PgStore::connect(&url).await?;
store.migrate().await?;
let store: DynStore = Arc::new(store);
```

Tables are `<prefix>runs` (state as `JSONB`), `<prefix>journal` (primary key
`(run_id, seq)`, `ON DELETE CASCADE`) and `<prefix>meta`. Claiming is
`FOR UPDATE SKIP LOCKED`; one open run per conversation is a partial unique
index. No transaction is held open while agent code runs. `JSONB` cannot hold
`\u0000`: such state is rejected with `StoreError::InvalidInput`. The guarantee
table is in the [root README](../../README.md#how-each-adapter-guarantees-the-contract).

## Errors

Failures are `adam_core::StoreError` (see [`adam-core`](../adam-core/README.md#errors)
and [`adam-error`](../adam-error/README.md)). A driver error becomes
`StoreError::Backend { class, source }`, with the `sqlx` error as the source
and this adapter's choice of class:

| Driver error | Class |
|---|---|
| I/O, TLS, protocol, pool timed out or closed, worker crashed | `Transient` |
| database error with SQLSTATE class `08` or `53`, or `40001`, `40P01`, `57P01`, `57P02`, `57P03` | `Transient` |
| a value or column that cannot be decoded, a missing column | `Corrupt` |
| bad connection configuration | `Invalid` |
| any other database error (a rejected statement) and anything else | `Internal` |

A poisoned row is therefore `Corrupt`, not retryable and alerting, so the
runtime fails that run instead of re-leasing it for ever. The adapter's own
checks are `InvalidInput` (a NUL in `JSONB`, an integer above `i64::MAX`, a bad
table prefix) and `Corrupt` (an unknown status, a negative version or `seq`, a
journal entry that vanished). *Unverified:* the SQLSTATE codes are from the
PostgreSQL manual's error-code appendix, recalled from memory, not re-checked.

## Features

| Feature | Default | Effect |
|---|---|---|
| `tls-rustls` | yes | `sqlx/tls-rustls` |
| `tls-native-tls` | no | `sqlx/tls-native-tls` |

## Tests

`tests/conformance.rs` runs the shared suite against a real server.
`tests/errors.rs` checks the classes: an undecodable row is `Corrupt` and not
retryable (needs the server), and an unreachable server is `Transient` (offline).
Unit tests in `src/lib.rs` cover the driver-error table, and one of them
(`sqlstates_are_classified_against_a_real_server`) provokes real SQLSTATEs
when the server variable is set.

| Variable | Meaning |
|---|---|
| `ADAM_TEST_POSTGRES_URL` | server to test against; unset means the suite is skipped |
| `ADAM_TEST_TABLE_PREFIX` | table prefix of the test store (default `adam_test_`) |
| `ADAM_TEST_REQUIRE_DB` | `1`: an unset URL fails instead of skipping (CI sets it) |

```sh
docker compose up -d   # from the repository root
ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
  cargo test -p adam-store-postgres
```

CI runs it against PostgreSQL 12 and 17 (`conformance` job in
`.github/workflows/ci.yml`).

## See also

[`adam-store-mongodb`](../adam-store-mongodb/README.md), the other adapter;
[`adam-error`](../adam-error/README.md).
