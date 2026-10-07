# adam-store-postgres

`adam_core::Store` on PostgreSQL 12+ through `sqlx`.

## Where it sits

An **adapter** of the store port in [`adam-core`](../adam-core/README.md),
checked by [`adam-store-testkit`](../adam-store-testkit/README.md). Nothing in
`adam-core` or `adam-runtime` names it; a binary chooses it at composition
time (for example [`adam-coder`](../../bin/adam-coder/README.md)).

## API at a glance

* `PgStore::connect(url)`: default pool (16 connections).
* `PgStore::from_pool(pool)`: reuse your application's `PgPool`.
* `PgStore::with_table_prefix(prefix)`: table-name prefix (default `adam_`,
  only `[a-z0-9_]`, at most 40 characters).
* `PgStore::pool()`, `SCHEMA_VERSION` (3).
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
`(run_id, seq)`, `ON DELETE CASCADE`), `<prefix>push` (the A2A push-notification configs, primary
key `(run_id, id)`, `ON DELETE CASCADE`, schema version 3; a partial index on
`(agent, next_attempt_at)` for active configs is the claim scan; `config` holds the webhook
credentials as the client gave them) and `<prefix>meta`. Claiming is
`FOR UPDATE SKIP LOCKED`, and it leaves out the runs the caller says it is stepping (`AND id <> ALL($busy)`,
see [`adam-core`](../adam-core/README.md#runs-the-caller-is-stepping)); one open run per conversation is a partial unique
index. No transaction is held open while agent code runs. `JSONB` cannot hold
`\u0000`: such state is rejected with `StoreError::InvalidInput`. The guarantee
table is in the [store adapters reference](../../docs/reference/store-adapters.md#how-each-adapter-keeps-the-contract).

## Schema version 3: the push table

`migrate()` creates `<prefix>push` and its index **before** the statements that lock `runs`
exclusively, so a migration takes its locks in the order a `push_put` does and the two cannot
deadlock (found by the conformance suite, whose cases all migrate). Claiming push configs is
`FOR UPDATE SKIP LOCKED` in one statement, progress is `UPDATE .. WHERE version = $expected`, and
putting an id again is `INSERT .. ON CONFLICT DO UPDATE` that bumps the version.

## Schema version and the owner column

`SCHEMA_VERSION` is 2. Version 2 adds `<prefix>runs.owner TEXT`, the worker a pinned claim
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
`tests/migrate.rs` builds a version 1 schema by hand with a legacy run, migrates it and checks the
column, the version row, that the run survived and that pinning works on it; it also checks that an
older release does not lower the version.
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
