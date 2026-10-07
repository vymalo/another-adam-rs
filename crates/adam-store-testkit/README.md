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

* `store_conformance!(make)` generates one `#[tokio::test]` per case (43
  cases: create/load, state round trip, CAS conflicts, concurrent commits,
  journal ordering and first-writer-wins, claim rules and exclusivity, busy runs
  that a claim leaves alone, lease expiry/renew/release, what `lease_until` reports, pinned claims and the
  run owner, one open run per conversation, purge, the listing of a caller's runs (scoped, ordered, keyset pages, status and time filters, literal prefixes, `count_runs`), and the push-notification configs: put/replace, list, delete, claim rules, exclusive claims under concurrency, commit, stale versions, removal with the run).
  `make` is a path to `async fn() -> Option<DynStore>`; `None` skips the suite.
* `cases::*`: the cases as plain async functions taking a `DynStore`, for
  harnesses that do not use the macro.
* `fault::FaultyStore`: wraps a `DynStore` and fails scripted calls
  (`fail`, `fail_always`, `fail_after_apply`, `heal`, `calls`, `injected`, and `claimed(run)`, how many times
  `claim_due` handed a run to its caller; and `fail_run` /
  `fail_run_after_apply`, which strike only the calls about one run, to break one link of a chain such as
  the message a finished child sends its parent), with
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
* `claim_skips_busy_runs` (both scopes): runs named as `busy` are not claimed, with a live lease or an expired one,
  take no slot of the limit, and are not leased by the call that skipped them.
* `lease_until_reports_the_lease`: nothing before a claim or for an unknown run, the end of the claim, of a renewal, still the same after a commit and after a stranger's release, nothing after the holder's release, and an expired lease reported as it was.
* Pinned-claim cases (`ClaimScope::Pinned`): an owned run is never given to another worker, not
  after a release, a commit or an expired lease; the first pinned claim sets the owner and an
  `Any` claim neither reads nor sets it; 4 workers racing on 48 runs split them exactly once and
  each gets back exactly its own.
* The `push_*` cases: a put creates then replaces (version, state and cursor reset, `created_at` kept), needs its run, lists per run in id order, round-trips configs with unusual keys, deletes idempotently; a claim takes only active, due configs of the asked agents, honours the limit and the lease, and 8 workers racing on 40 configs split them exactly once; a commit advances the version, drops the lease and fails with `Conflict` on a stale or replaced version and `NotFound` once deleted; configs go with their purged run.
* `src/fault.rs`: unit tests and a doctest of `FaultyStore`.

The Postgres and MongoDB adapters run the suite from their own
`tests/conformance.rs`.

## See also

[`adam-core`](../adam-core/README.md),
[`adam-store-postgres`](../adam-store-postgres/README.md),
[`adam-store-mongodb`](../adam-store-mongodb/README.md), and
"To add a backend" in the [testing guide](../../docs/guides/testing.md#adding-a-backend).
