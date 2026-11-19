---
name: adam-store-adapter
description: "Implement or update an adam_core::Store (the durable run store) or an adam_runtime::Notifier for another backend, and pass the conformance testkits (store_conformance!, notifier_conformance!). Use for 'new store backend', 'SQLite/Redis adapter for adam', or when a bump of adam-rs added a required Store method (for example lease_until) and your implementation stopped compiling."
---

# Implement or update an adam store or notifier

The run store is the one thing adam agents persist: runs, their journal and their leases. `Store`
is a trait in `adam-core`; each backend is a separate crate that must pass the shared conformance
suite. A `Notifier` (cross-process wake-ups) is the same pattern with its own testkit.

Every adam-rs path below is in `vymalo/another-adam-rs` at the revision you pin (replace `main`
by that revision). Entry point:
https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-core/src/store/mod.rs. Read the
trait at your rev: its doc comments are the contract.

## When to use

* You write a backend (SQLite, Redis, FoundationDB, ...) in your own crate or repository.
* You bumped adam-rs and your `impl Store for ...` no longer compiles (a method was added).
* You wrap a store (metrics, retries) and must forward every method.
* Not for choosing between the shipped Postgres and MongoDB adapters: use one, with
  `adam_core::DynStore` (`crates/adam-store-postgres/README.md`, `crates/adam-store-mongodb/README.md`).

## Procedure

1. **Read the trait**: `crates/adam-core/src/store/mod.rs`, `pub trait Store`: `migrate`,
   `create_run`, `load_run`, `run_header` (a run without its state; **defaulted** to
   `lease_until` + `load_run`, so overriding it is optional — override it with a projection that
   never reads the state, as both shipped adapters do, and pass `run_header_matches_the_record`),
   `commit_run` (compare-and-swap on `version`), `open_run_for_conversation`, `journal_get`,
   `journal_put` (first writer wins), `journal_list`, `claim_due`, `renew_lease`, `release_lease`,
   `lease_until`, `purge_finished`, then the listing (`list_runs` and `count_runs`: a caller's runs,
   newest update first, by keyset), the five push-notification methods (`push_put`, `push_list`,
   `push_delete`, `push_claim_due`, `push_commit`; schema version 3) and the **defaulted** methods:
   the five blob ones (`keeps_blobs`, `blob_stored`, `put_blob`, `get_blob`, `purge_blobs`; schema
   version 5, part 1: file bytes out of the run, content-addressed by `BlobId`) and the three body ones
   (`load_head`, which defaults to `load_run` and serves the v5 head only when
   `head_version == version`; `keeps_bodies`; `body_list`; `gc_body`; schema version 5, part 2: an
   append-only transcript per generation beside the head). A defaulted method left out of your
   implementation answers for your store with the trait's "I keep none" answer.
   Errors are `StoreError` (`crates/adam-core/README.md`, "Errors"): build a backend failure with
   `StoreError::unavailable(e)` (transient), `StoreError::internal(e)` or
   `StoreError::corrupt_source(e)`; callers decide from the error class, never from the variant.
2. **Match what the existing adapters promise**: the table "How each adapter keeps the
   contract" in `docs/reference/store-adapters.md` lists, per guarantee, how Postgres and MongoDB do it
   (CAS commit, first-writer-wins journal, exclusive claiming, `busy` runs never claimed,
   pinned claims and the run `owner`, one open run per conversation, journal deleted with its
   run, blob put/get and the blob GC, schema version 5 part 1, and the head and body of schema
   version 5 part 2). Time is truncated to milliseconds
   in every store; lease expiry uses the `now` the caller passes.
3. **Implement `Store`** in a crate of its own, depending on `adam-core` only (no driver type in
   a trait signature). Reference implementations: `crates/adam-core/src/store/memory.rs`
   (`MemoryStore`), `crates/adam-store-postgres/src/lib.rs`, `crates/adam-store-mongodb/src/lib.rs`.
4. **Add the conformance suite** as an integration test (`tests/conformance.rs`), with
   `adam-store-testkit` as a dev-dependency:

   ```rust
   async fn make_store() -> Option<adam_core::DynStore> {
       let url = adam_core::testing::test_env("MYSTORE_TEST_URL")?;
       Some(std::sync::Arc::new(MyStore::connect(&url).await.unwrap()))
   }
   adam_store_testkit::store_conformance!(make_store);
   adam_store_testkit::blob_conformance!(make_store); // a store that keeps blobs
   adam_store_testkit::body_conformance!(make_store); // a store that keeps bodies
   ```

   `store_conformance!` generates one `#[tokio::test]` per case (48 at the time of writing) and
   calls `migrate` first. `blob_conformance!` adds the nine blob cases (put/get, a 4 MiB round trip,
   an idempotent put, an unknown id, empty bytes, a run's reference keeping a blob alive (including
   after a commit that names a different one), a shared digest surviving until the last run is
   purged, the cutoff, and `purge_finished`'s sweep); every
   case asserts `keeps_blobs()`, so do not give it to a store that keeps none. `body_conformance!`
   adds the six body cases (a set round-tripping in `idx` order with its head, an idempotent retry,
   a superseded set that stays readable, a headless commit leaving the head stale, sets per run, and
   the purge); every case asserts `keeps_bodies()`. `None` skips the
   suites. Cases isolate themselves with a unique agent name, so they run in parallel on one shared
   database — **except the blob cases and the cases that purge**, which sweep every unreferenced
   blob and therefore must not overlap: keep them in one nextest test group (`.config/nextest.toml`,
   `[test-groups.store-global-sweeps]`) and take the suite's `BLOB_SUITE` lock under libtest. A test
   that calls `purge_finished` or `purge_blobs` must carry `purge` in its name so the group's filter
   catches it.
   `counting::measure_transcript_write` is a measurement helper, not a conformance case: it runs one
   50-commit transcript and prints the bytes, and it asserts nothing.
5. **In CI set `ADAM_TEST_REQUIRE_DB=1`**: a suite whose database variable is unset then fails
   instead of skipping (`adam_core::testing::test_env`), so a typo cannot turn CI green.
6. **After a trait change in adam-rs** (a method added or a signature changed): implement it in
   every store you own, add it to every wrapper, and run the new suite. The change that added
   `Store::lease_until` (commit `7e5dcc3` in adam-rs) touched: the trait, `MemoryStore`, both
   adapters, a testkit case (`lease_until_reports_the_lease`) and `fault::Method::LeaseUntil` of
   `FaultyStore`. Its semantics: the end of the lease on a run, `None` if never claimed, released
   or unknown; a lease that ran out and was not released is **still reported**, because whether it
   counts is the caller's to say against its own clock (a claim treats `until <= now` as free).
7. **A `Notifier`** (`crates/adam-runtime/src/notify.rs`: `publish(Signal)` never fails,
   `subscribe()` is a stream of `Delivery`, where `Delivery::Resync` replaces what a slow
   subscriber missed): implement it, then
   `adam_notify_testkit::notifier_conformance!(make_pair)` with a `make_pair` returning
   `Option<adam_notify_testkit::Pair>` (two sides that reach each other, both already listening;
   `crates/adam-notify-testkit/README.md`). A notification is a latency optimisation, never the
   truth: correctness must not depend on it, polling stays on.
8. **Test callers against failures** with `adam_store_testkit::fault::FaultyStore` (it wraps a
   `DynStore` and fails scripted calls: `fail`, `fail_always`, `fail_after_apply`, `heal`).

## Verify

* `cargo test -p <your-store-crate>` with your database variable set, and
  `ADAM_TEST_REQUIRE_DB=1` to prove nothing skipped.
* adam-rs's own adapters show the pattern: `ADAM_TEST_POSTGRES_URL=... cargo test -p adam-store-postgres`
  and `ADAM_TEST_MONGODB_URI=... cargo test -p adam-store-mongodb`. Its CI runs the suites twice
  against a database that already has the schema, to exercise `migrate()` on an existing one
  (`.github/workflows/ci.yml`, job `conformance`).
* `crates/adam-store-testkit/tests/memory.rs` runs the suite against `MemoryStore` without any
  database: a quick check that a harness of yours is wired.

## Pitfalls

* `migrate` must be idempotent and safe to call from every replica at once.
* A run's state is JSON. Apply `adam_core::storable` (or `storable_owned`) to every JSON value
  before you write it — a NUL in a string or a key becomes U+FFFD — and return what you stored, so
  a tool that printed a NUL does not fail the run and every backend holds the same value
  (`docs/reference/store-adapters.md`, "Data caveats"; ADR 0051). Other data a backend cannot hold
  (an integer above `i64::MAX` in BSON) is `InvalidInput`, not a driver error.
* Claiming must be exclusive under concurrency (the suite races workers); a read-then-write claim
  without a conditional write fails it.
* `release_lease` never clears the run's owner (pinned claims).
* Implementing the trait with default or `unimplemented!()` bodies to get a bump compiling: the
  suite exists to catch exactly that.
* Blobs: `put_blob` is idempotent and **the bytes behind an id never change** (a second put under a
  taken digest writes nothing), the id is the SHA-256 of the bytes as 64 lower-case hex, and a
  commit's `RunUpdate.blobs` is a **delta** the store unions with what it already recorded — a
  commit that names none keeps the earlier references. `blob_stored(id)` is one indexed existence
  check (the runtime checks it before it re-sends a file's bytes; the default `false` means "put
  it"). A blob a surviving run references is never collected; `purge_blobs(before)` and
  `purge_finished` delete only unreferenced blobs older than the cutoff.
* Bodies: a generation is `(run, generation, idx)` and is **append-only**. `RunUpdate.body` is
  `{generation, rows, writer}`: `generation: None` appends `rows[old_len..]` to the current head's
  generation (or starts one when there is no head), `Some(new)` is a compaction and writes all rows
  under `new`. Validate `rows[i].idx == i`, resolve the target, patch the head's `body` to
  `{generation, len}` (`adam_core::set_head_body`; `head_body` reads it), write the rows **before**
  the CAS with the attempt's `writer`, and check that all of them landed. A duplicate key is this
  attempt's retry when the row's `writer` is this attempt. A row another attempt wrote is resolved
  by content and age: **adopt** it when its message equals the one this attempt would write, delete
  it and retry the insert once when it is older than the **orphan grace** (5 minutes, longer than
  any lease the runtime grants), and answer `Conflict` when a live attempt holds the slot with a
  different message. A live conflict is not a wedge: the runtime drops the result and the next
  claim steps the run again; an attempt that died before its CAS leaves rows that are adopted or
  collected. A **write-concern** error is not a completeness answer. A commit that loses the CAS deletes the
  rows and refs its `writer` added, but never a row or digest the winner's head names — and a commit
  with no body still cleans the refs it inserted. A commit with `head: None` leaves the stored head **and its `head_version`**
  alone; `load_head` serves the head only when `head_version == version`. Collect superseded
  generations after a grace period (`gc_body`, and `purge_finished` removes them with the run).
  **Forward `gc_body` and `blob_stored` in every wrapper**: the trait defaults answer `0`/`false`,
  so a wrapper that omits them collects nothing and re-puts every blob.
* A transient backend error must be classified transient so the runtime retries it.

## See also

* `crates/adam-core/README.md`, `crates/adam-store-testkit/README.md`,
  `crates/adam-notify-testkit/README.md`, "Adding a backend" in `docs/reference/store-adapters.md`
  and `docs/guides/testing.md`.
* `adam-upgrade` (finding the trait change in the first place), `adam-embed`.
* https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-store-testkit/README.md
