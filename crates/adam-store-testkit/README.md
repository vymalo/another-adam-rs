# adam-store-testkit

The conformance suite every `adam_core::Store` implementation must pass, plus
two `Store` wrappers for tests: one injects faults, the other counts calls and
the state bytes that cross it.

## Where it sits

The **testkit** of the store port defined in
[`adam-core`](../adam-core/README.md). Adapter crates list it as a
dev-dependency and run the same cases, so "passes the testkit" means "behaves
like every other store". It is a library used from tests, not a runtime
dependency.

## API at a glance

* `store_conformance!(make)` generates one `#[tokio::test]` per case (49
  cases: create/load, the millisecond precision of every timestamp (`.120` survives, finer truncates),
  `run_header` against `load_run` + `lease_until`, state round trip, CAS
  conflicts, concurrent commits, journal ordering and first-writer-wins, claim rules and
  exclusivity, busy runs that a claim leaves alone, lease expiry/renew/release, what `lease_until` reports, pinned claims and the
  run owner, one open run per conversation, purge, the listing of a caller's runs (scoped, ordered, keyset pages, status and time filters, literal prefixes including ones that hold or end in `/`, `count_runs`; two creates with different ids in one conversation leave one open run), and the push-notification configs: put/replace, list, delete, claim rules, exclusive claims under concurrency, commit, stale versions, removal with the run).
  `make` is a path to `async fn() -> Option<DynStore>`; `None` skips the suite.
* `blob_conformance!(make)` generates one `#[tokio::test]` per blob case (9 cases: put/get round
  trip, a 4 MiB round trip (a file artifact's cap), an idempotent put whose first bytes win, an
  unknown id answers `None`, empty bytes are a blob, a run's reference keeps a blob alive (including
after a commit that names a different one), a shared
  digest survives until the last referencing run is purged, `purge_blobs` respects its cutoff, and
  `purge_finished` removes the run's references and the unreferenced blob). Every case asserts `keeps_blobs()`: a store that keeps none is not
  given this suite. Blobs are global, so the cases take turns inside one test binary (the
  `BLOB_SUITE` lock, which the cases that purge take too); under nextest, where each test is its own
  process, `.config/nextest.toml` puts the same cases in one single-thread group. Runs are still
  isolated by agent name. `make` is the same `async fn() -> Option<DynStore>` as above.
* `transcript_measurement!(make)` generates the `measure_the_transcript_write` test: it runs
  `counting::measure_transcript_write` through `make`, prints its `p2-measure:` line and asserts
  only that the workload ran. `make` is the same `async fn() -> Option<DynStore>` as above; `None`
  skips it. S3 rewrites the workload to commit a head and a body, then adds the `< 50 × 20 KB` and
  `max < 20 KB` assertions here.
* `cases::*`: the cases as plain async functions taking a `DynStore`, for
  harnesses that do not use the macro.
* `counting::CountingStore`: wraps a `DynStore` and counts calls per `fault::Method` and the
  `state` bytes that cross it — every record read (`load_run`, `list_runs`, `claim_due`,
  `open_run_for_conversation`) and every successful `commit_run` write — through `calls(method)`,
  `state_bytes_read()`, `state_bytes_written()`, `max_commit_bytes()` and `reset()`. A commit that
  failed wrote nothing and is not counted; `migrate` is not counted, and neither is a record
  returned without its state (a `create_run` or `commit_run` echo). `run_header` is forwarded: an
  adapter that overrides it answers in one small read with no state; one that does not reads the
  record inside the adapter, which the wrapper cannot see — never measure reads with `run_header`.
* `counting::measure_transcript_write`: a **measurement helper, not a conformance case**. One run,
  `Measurement::COMMITS` commits of a 10 KB message, the last one `Done`, then purged; returns
  `Measurement` (`commits`, `total_written`, `max_commit`, `first_read`, `final_read`) and
  `report()`, the `p2-measure:` line `dev/p2-measure.sh` parses. It asserts nothing: the
  `transcript_measurement!` test prints the line and asserts only that the workload ran. The
  measured baseline is in [ADR 0055](../../docs/decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md).
* `fault::FaultyStore`: wraps a `DynStore` and fails scripted calls
  (`fail`, `fail_always`, `fail_after_apply`, `heal`, `calls`, `injected`, and `claimed(run)`, how many times
  `claim_due` handed a run to its caller; and `fail_run` /
  `fail_run_after_apply`, which strike only the calls about one run, to break one link of a chain such as
  the message a finished child sends its parent), with
  `fault::Method` and `fault::Mode` selecting the method and whether the
  operation is applied before the error is returned. `fault::Method` is **not** `#[non_exhaustive]`:
  a new variant breaks an exhaustive `match` on it (schema version 5 part 1 added `PutBlob`,
  `GetBlob` and `PurgeBlobs`). `fault::is_injected`
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

* `tests/memory.rs`: runs the store suite and the blob suite against `MemoryStore` (always on).
* `claim_skips_busy_runs` (both scopes): runs named as `busy` are not claimed, with a live lease or an expired one,
  take no slot of the limit, and are not leased by the call that skipped them.
* `lease_until_reports_the_lease`: nothing before a claim or for an unknown run, the end of the claim, of a renewal, still the same after a commit and after a stranger's release, nothing after the holder's release, and an expired lease reported as it was.
* Pinned-claim cases (`ClaimScope::Pinned`): an owned run is never given to another worker, not
  after a release, a commit or an expired lease; the first pinned claim sets the owner and an
  `Any` claim neither reads nor sets it; 4 workers racing on 48 runs split them exactly once and
  each gets back exactly its own.
* The `push_*` cases: a put creates then replaces (version, state and cursor reset, `created_at` kept), needs its run, lists per run in id order, round-trips configs with unusual keys, deletes idempotently; a claim takes only active, due configs of the asked agents, honours the limit and the lease, and 8 workers racing on 40 configs split them exactly once; a commit advances the version, drops the lease and fails with `Conflict` on a stale or replaced version and `NotFound` once deleted; configs go with their purged run.
* `src/fault.rs`: unit tests and a doctest of `FaultyStore`.
* `src/counting.rs`: unit tests of the counters (calls per method, reads through `load_run`,
  `list_runs` and `claim_due`, the write total and its high-water mark, a failed commit that
  counts nothing, a forwarded header whose default read inside the adapter the wrapper cannot
  see) and a doctest of `CountingStore`.
* `tests/memory.rs` also runs `transcript_measurement!(make_store)`; the Postgres and MongoDB
  adapters run it in `tests/conformance.rs`. It is a measurement, not a case: the test asserts only
  that the workload ran.

The Postgres and MongoDB adapters run the suite from their own
`tests/conformance.rs`.

## See also

[`adam-core`](../adam-core/README.md),
[`adam-store-postgres`](../adam-store-postgres/README.md),
[`adam-store-mongodb`](../adam-store-mongodb/README.md), and
"To add a backend" in the [testing guide](../../docs/guides/testing.md#adding-a-backend).
