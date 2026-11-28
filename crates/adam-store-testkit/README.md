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

* `store_conformance!(make)` generates one `#[tokio::test]` per case (51
  cases: create/load, the millisecond precision of every timestamp (`.120` survives, finer truncates),
  `run_header` against `load_run` + `lease_until`, state round trip, a message whose state, commit and
  journal payload canonicalize (RFC 8785) to the bytes written, with numbers either side of 2^53, `i64`'s extremes, floats in
  exponent form, `-0` and names that sort differently by UTF-16 (what a turn signature of
  [ADR 0068](../../docs/decisions/0068-adam-signs-its-own-history.md) covers; a `u64` above `i64::MAX` is left out, MongoDB
  refuses it), CAS
  conflicts, concurrent commits, journal ordering and first-writer-wins, a journal entry's signature
  (`adam-step/v1`) stored and returned byte for byte with an unsigned one as `None`, an entry
  changed after it was signed refused by `verify_step`, claim rules and
  exclusivity, busy runs that a claim leaves alone, lease expiry/renew/release, what `lease_until` reports, pinned claims and the
  run owner, one open run per conversation, purge, the listing of a caller's runs (scoped, ordered, keyset pages, status and time filters, literal prefixes including ones that hold or end in `/`, `count_runs`; two creates with different ids in one conversation leave one open run), and the push-notification configs: put/replace, list, delete, claim rules, exclusive claims under concurrency, commit, stale versions, removal with the run).
  `make` is a path to `async fn() -> Option<DynStore>`; `None` skips the suite.
* `blob_conformance!(make)` generates one `#[tokio::test]` per blob case (10 cases: put/get round
  trip, a 4 MiB round trip (a file artifact's cap), an idempotent put whose first bytes win, an
  unknown id answers `None`, `blob_stored` is false before a put and true after (and false for an
  unwritten id), empty bytes are a blob, a run's reference keeps a blob alive (including
after a commit that names a different one), a shared
  digest survives until the last referencing run is purged, `purge_blobs` respects its cutoff, and
  `purge_finished` removes the run's references and the unreferenced blob). The cases that assert a purge deletes a digest write bytes unique to the run (a UUID in the content), so the suite passes twice in a row against the same database. Every case asserts `keeps_blobs()`: a store that keeps none is not
  given this suite. Blobs are global, so the cases take turns inside one test binary (the
  `BLOB_SUITE` lock, which the cases that purge take too); under nextest, where each test is its own
  process, `.config/nextest.toml` puts the same cases in one single-thread group. Runs are still
  isolated by agent name. `make` is the same `async fn() -> Option<DynStore>` as above.
* `body_conformance!(make)` generates one `#[tokio::test]` per body case (15 cases: a row that
  canonicalizes as it was written (the same message as above); an append writes
  only the rows after the head's `len`, so a generation holds the transcript once; the generation
  is stable across appends; a compaction starts a new generation and `gc_body` collects the old one
  with an explicit cutoff; a stale version is a `Conflict` (a real insert-then-lose race is the
  adapter's own test); a v4 first split writes the
  whole transcript under one generation (named by the runtime or by the store); the same transcript
  committed again adds no row; a non-contiguous index is refused; a purge removes the rows; an
  unknown generation is empty; a NUL in a head and in a row is stored as U+FFFD; every read serves
  the current head — `load_run`, `load_head`, `claim_due`, `open_run_for_conversation` and
  `list_runs`; a stale head serves the state; a tombstoned run whose head is behind the version
  serves the head (the repair of a run a pre-S3 process failed); and a head that is JSON `null` is
  not a head). Every case asserts `keeps_bodies()`: a store that keeps none is
  not given this suite. The purge and compaction cases take the `BLOB_SUITE` lock because
  `purge_finished` sweeps blobs in the same call. `make` is the same
  `async fn() -> Option<DynStore>` as above.
* `activity_conformance!(make)` generates one `#[tokio::test]` per `Store::activity` case (2 cases: a claimed run
  makes `busy` 1, a parked schedule due in an hour is `next_due_at`, an expired or released lease is not busy and
  another agent's runs are not counted; with no open run the answer is zero and no time). Give it only to a store
  that implements `activity`.
* `worker_leases_conformance!(make)` generates one `#[tokio::test]` per worker-lease case (3 cases:
  every run lease a worker holds goes, so its runs are claimable at once — by another worker's unpinned
  claim too — and the owner is not cleared, so a pinned run still comes back only to the same
  worker; another worker's lease is untouched: still reported by `lease_until`, unclaimable and
  renewable; and a lease the **owner does not hold** — w1 owns the run, w2 holds its lease — survives
  `release_worker_leases("w1")`, because the sweep keys on the lease holder, not the owner).
  `release_worker_leases` is **defaulted to a no-op**, so a store that does not implement it passes
  `store_conformance!`; give this suite only to a store that implements the
  release. `make` is the same `async fn() -> Option<DynStore>` as above.
* `headless_measurement!(make)` generates the `measure_the_headless_transcript` test: it runs
  `counting::measure_headless_transcript` through `make`, prints its `p2-headless:` line (a
  delivery-shaped commit between every split, so each split writes the whole transcript) and
  asserts only that the workload ran. `make` is the same as above; a store that keeps no bodies
  skips it.
* `transcript_measurement!(make)` generates the `measure_the_transcript_write` test: it runs
  `counting::measure_transcript_write` through `make`, prints its `p2-measure:` line and asserts the
  S0 bound — the 50 commits write under `50 × 20 KB` in all, and no commit writes over `20 KB`. The
  workload commits the contract shape (the tombstone, a head and the body rows; the store appends
  the delta). The bound is the **wrapper's** arithmetic — the bytes a commit hands the store — not
  the adapter's: the store's own bytes are `dev/p2-measure.sh`'s relation sizes and WAL deltas, which
  a PR quotes beside it. `make` is the same `async fn() -> Option<DynStore>` as above; `None`, or a
  store that keeps no bodies, skips it.
* `cases::*`: the cases as plain async functions taking a `DynStore`, for
  harnesses that do not use the macro.
* `counting::CountingStore`: wraps a `DynStore` and counts calls per `fault::Method` and the
  `state` bytes that cross it — every record read (`load_run`, `load_head`, `list_runs`,
  `claim_due`, `open_run_for_conversation`) and every successful `commit_run` write (its state, its
  head and the body rows it appends; the tombstone counts nothing) — through
  `calls(method)`, and forwards `gc_body`/`blob_stored`/`release_worker_leases` to the wrapped store
  (`GcBody`/`BlobStored`/`ReleaseWorkerLeases` calls),
  `state_bytes_read()`, `state_bytes_written()`, `commit_bytes()`, `max_commit_bytes()` and
  `reset()`. A commit that
  failed wrote nothing and is not counted; `migrate` is not counted, and neither is a record
  returned without its state (a `create_run` or `commit_run` echo). `run_header` is forwarded: an
  adapter that overrides it answers in one small read with no state; one that does not reads the
  record inside the adapter, which the wrapper cannot see — never measure reads with `run_header`.
  The read path of ADR 0055's S2b is what a measurement asserts: `Runtime::view` is one
  `LoadHead` and no `BodyList`, and `Runtime::state` is one `LoadHead` and one `BodyList` for a v5
  run (a v4 run answers from the record with no body read). A store
  whose `load_head` is the trait default answers the `LoadRun` the wrapper sees inside the adapter,
  which it cannot count: measure the head with `load_head`, as `adam-runtime`'s suite does.
* `counting::measure_transcript_write`: a **measurement helper, not a conformance case**. One run,
  `Measurement::COMMITS` commits of a 10 KB message in the contract shape (the tombstone, a head
  and the body rows), the last one `Done`, then purged; returns `Measurement` (`commits`,
  `total_written`, `max_commit`, `first_read`, `final_read`) and `report()`, the `p2-measure:` line
  `dev/p2-measure.sh` parses. It asserts nothing: the `transcript_measurement!` test prints the line
  and asserts the bound. The before/after numbers are in
  [ADR 0055](../../docs/decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md).
* `fault::FaultyStore`: wraps a `DynStore` and fails scripted calls
  (`fail`, `fail_always`, `fail_after_apply`, `heal`, `calls`, `injected`, and `claimed(run)`, how many times
  `claim_due` handed a run to its caller; and `fail_run` /
  `fail_run_after_apply`, which strike only the calls about one run, to break one link of a chain such as
  the message a finished child sends its parent), with
  `fault::Method` and `fault::Mode` selecting the method and whether the
  operation is applied before the error is returned. `fault::Method` is **not** `#[non_exhaustive]`:
  a new variant breaks an exhaustive `match` on it (`ReleaseWorkerLeases` is the worker-lease
  sweep; schema version 5 part 1 added `PutBlob`,
  `GetBlob` and `PurgeBlobs`; part 2 added `LoadHead`, `BodyList`, `GcBody` and `BlobStored`).
  `fault::is_injected`
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

* `tests/memory.rs`: runs the store suite, the blob suite, the body suite and the worker-lease
  suite against `MemoryStore` (always on).
* `claim_skips_busy_runs` (both scopes): runs named as `busy` are not claimed, with a live lease or an expired one,
  take no slot of the limit, and are not leased by the call that skipped them.
* `lease_until_reports_the_lease`: nothing before a claim or for an unknown run, the end of the claim, of a renewal, still the same after a commit and after a stranger's release, nothing after the holder's release, and an expired lease reported as it was.
* `worker_leases_*`: the sweep frees every run lease of the worker (the runs claimable at once, the owner kept), leaves another worker's lease alone, and leaves a lease the owner does not hold (the sweep keys on the holder, not the owner).
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
  adapters run it in `tests/conformance.rs`. It is a measurement, not a case: the test asserts the
  S0 bound (under `50 × 20 KB` in all, under `20 KB` per commit).

The Postgres and MongoDB adapters run the suite from their own
`tests/conformance.rs`.

## See also

[`adam-core`](../adam-core/README.md),
[`adam-store-postgres`](../adam-store-postgres/README.md),
[`adam-store-mongodb`](../adam-store-mongodb/README.md), and
"To add a backend" in the [testing guide](../../docs/guides/testing.md#adding-a-backend).
