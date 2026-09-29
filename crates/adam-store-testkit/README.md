# adam-store-testkit

The conformance suite every `adam_core::Store` implementation must pass, plus
a fault-injecting `Store` wrapper for testing callers.

## Where it sits

The **testkit** of the store port defined in
[`adam-core`](../adam-core/README.md). Adapter crates list it as a
dev-dependency and run the same cases, so "passes the testkit" means "behaves
like every other store". It is a library used from tests, not a runtime
dependency.

## API at a glance

* `store_conformance!(make)` generates one `#[tokio::test]` per case (22
  cases: create/load, state round trip, CAS conflicts, concurrent commits,
  journal ordering and first-writer-wins, claim rules and exclusivity, lease
  expiry/renew/release, one open run per conversation, purge).
  `make` is a path to `async fn() -> Option<DynStore>`; `None` skips the suite.
* `cases::*`: the cases as plain async functions taking a `DynStore`, for
  harnesses that do not use the macro.
* `fault::FaultyStore`: wraps a `DynStore` and fails scripted calls
  (`fail`, `fail_always`, `fail_after_apply`, `heal`, `calls`, `injected`), with
  `fault::Method` and `fault::Mode` selecting the method and whether the
  operation is applied before the error is returned. `fault::is_injected`
  tells an injected error from a real one.
* `skipped`: re-export of `adam_core::testing::skipped`.

```rust
async fn make_store() -> Option<adam_core::DynStore> {
    let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
    Some(std::sync::Arc::new(PgStore::connect(&url).await.unwrap()))
}
adam_store_testkit::store_conformance!(make_store);
```

Cases isolate themselves with a unique agent name, so they run in parallel
against one shared database with no cleanup.

## Features and environment

No Cargo features. `ADAM_TEST_REQUIRE_DB=1` turns a skipped suite into a
failure (see `adam_core::testing`).

## Tests

* `tests/memory.rs`: runs the suite against `MemoryStore` (always on).
* `src/fault.rs`: unit tests and a doctest of `FaultyStore`.

The Postgres and MongoDB adapters run the suite from their own
`tests/conformance.rs`.

## See also

[`adam-core`](../adam-core/README.md),
[`adam-store-postgres`](../adam-store-postgres/README.md),
[`adam-store-mongodb`](../adam-store-mongodb/README.md), and
"To add a backend" in the [root README](../../README.md#testing).
