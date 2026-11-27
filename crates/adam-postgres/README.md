# adam-postgres

The `DATABASE_URL` parsing and TLS setup shared by adam-rs's two PostgreSQL adapters
([`adam-store-postgres`](../adam-store-postgres/README.md) and
[`adam-notify-postgres`](../adam-notify-postgres/README.md)). It is not a `Store` and not a
`Notifier`: it is the piece both need, in one place, so a URL means the same thing to the store and
to the listener.

## Why it exists

`tokio-postgres` understands `sslmode=disable|prefer|require` and rejects every parameter it does
not know. `DATABASE_URL` in the wild carries `verify-ca`, `verify-full`, `sslrootcert`, `sslcert`
and `sslkey`, and `sqlx` also accepted aliases (`ssl-mode`, `ssl-root-cert`, `ssl-ca`, `ssl-cert`,
`ssl-key`), `options[key]` and `statement-cache-capacity`. `PgConfig::parse` strips the TLS keys,
applies them to a `rustls` connector, folds `options[key]` into `options` and drops the rest with a
warning, so **every `DATABASE_URL` that worked with `sqlx` keeps working**
([ADR 0053](../../docs/decisions/0053-sqlite-a-store-service-a-database-per-agent-and-a-static-coder.md),
decision 1).

## API

| Item | What |
|---|---|
| `PgConfig::parse(url)` | the driver's configuration and the `rustls` connector. Reads `sslrootcert`, `sslcert` and `sslkey` here, so a missing file fails at startup (`Invalid`), not at the first connection |
| `.pool(max_size)` | a `deadpool-postgres` pool; no connection is opened until the first query |
| `.connect()` | one standalone connection, its driver spawned (a `SELECT 1`, a statement) |
| `.connect_raw()` | one standalone connection **and** its driver, for `LISTEN`/`NOTIFY`: `tokio-postgres`'s `Connection` future ignores asynchronous messages, so a listener polls `MessageDriver::poll_message` itself |
| `.ping()` | open a connection and run `SELECT 1` |
| `.with_application_name(name)` | name the process in `pg_stat_activity` |
| `.tls_mode()` | the mode the URL asked for, for logs and tests |
| `classify(err)` | a `tokio_postgres::Error` as `adam_core::StoreError`, with its class |
| `classify_pool(err)` | the same for `deadpool_postgres::PoolError` |
| `AdamSqlState`, `ADAM_SQLSTATE_CLASS` | adam's own SQLSTATEs (`AD001` to `AD004`) and their classes: [below](#adams-own-sqlstates) |

The pool's timeouts are explicit, because deadpool's default is none: a query waits at most 30 s for
a connection (`DEFAULT_WAIT_TIMEOUT`) and opening one takes at most 30 s
(`DEFAULT_CREATE_TIMEOUT`). A returned connection is recycled with a liveness check
(`RecyclingMethod::Fast`).

## TLS

| `sslmode` | driver | certificate check |
|---|---|---|
| `disable`, `allow` | no TLS | — |
| `prefer` (the default) | TLS if the server offers it | none |
| `require` | TLS required | none |
| `verify-ca` | TLS required | the chain, against `sslrootcert` or the Mozilla roots |
| `verify-full` | TLS required | the chain and the host name |

`verify-ca` deliberately skips the host-name check, as libpq does; `verify-full` uses `rustls`'s
default verifier. `sslcert` and `sslkey` (both, together) authenticate with a client certificate.
The provider is always `aws-lc-rs`, named explicitly: the tree also compiles `ring` (through
`mongodb`), so `rustls` refuses to pick a process default.

## Errors

`StoreError::Backend { class, source }`, with the driver's error as the source:

| Driver error | Class |
|---|---|
| I/O, TLS, a closed connection, a pool wait timeout or a closed pool | `Transient` |
| a database error with SQLSTATE class `08` or `53`, or `40001`, `40P01`, `57P01`, `57P02`, `57P03` | `Transient` |
| one of adam's own SQLSTATEs, `AD001` to `AD004` | the class of its row [below](#adams-own-sqlstates) |
| any other database error (a rejected statement) | `Internal` |
| a value or column that cannot be decoded | `Corrupt` (chosen by the adapter at `try_get`) |
| a bad connection string, a missing certificate file | `Invalid` |

The one documented change from the `sqlx` mapping: `sqlx`'s `Protocol` and `WorkerCrashed` were
`Transient`, and `tokio-postgres` has no equivalent, so what is not a database error, a closed
connection or an I/O/TLS failure is `Internal`.

## adam's own SQLSTATEs

The store's database functions
([ADR 0082](../../docs/decisions/0082-the-sql-adapters-keep-their-rules-in-the-database.md),
decision 4) refuse with SQLSTATEs of the class `AD`, which PostgreSQL does not use; none ends in
`000`, which marks a category code. `AdamSqlState` is the one table of them, beside the mapping of
PostgreSQL's own codes in `classify`; `adam-store-postgres` builds each `StoreError` variant from it.

| SQLSTATE | `AdamSqlState` | Raised when | `StoreError` | Class |
|---|---|---|---|---|
| `AD001` | `InvalidInput` | a body row's index is not its position, an append starts past the rows, a head has no body | `InvalidInput` | `Invalid` |
| `AD002` | `NonDeterminism` | the journal holds another step at this sequence number; the recorded name is the error's `DETAIL` | `NonDeterminism` | `Corrupt` |
| `AD003` | `Corrupt` | stored data breaks an invariant (a collected body slot is still occupied) | `Corrupt` | `Corrupt` |
| `AD004` | `Conflict` | a live attempt wrote a different body row; raised after rows were written, so the commit rolls back | `Conflict` | `Conflict` |

The enum is closed on purpose: a new code is a new variant, and every `match` over it fails to
compile until it says what the code means. An `AD` code this release does not know is `Internal`.

## Tests

`cargo test -p adam-postgres` runs offline: the `AD` table (each code's class, its format, that no
other SQLSTATE reads as one), every `DATABASE_URL` form checked in to `docs/`,
`deploy/`, `dev/` and `compose.yaml`, the `sslmode` table, the client certificate, and the
`verify-ca`/`verify-full` verifiers against checked-in test certificates
(`tests/fixtures/tls/`: a CA, a server certificate whose SAN is `db.example.internal`, a client
certificate, and a CA that did not sign them). `verify-full` rejects a name mismatch and a wrong
CA; `verify-ca` accepts the name mismatch but still rejects the wrong CA; `require` accepts both.
