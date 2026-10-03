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
   `create_run`, `load_run`, `commit_run` (compare-and-swap on `version`),
   `open_run_for_conversation`, `journal_get`, `journal_put` (first writer wins),
   `journal_list`, `claim_due`, `renew_lease`, `release_lease`, `lease_until`, `purge_finished`.
   Errors are `StoreError` (`crates/adam-core/README.md`, "Errors"): build a backend failure with
   `StoreError::unavailable(e)` (transient), `StoreError::internal(e)` or
   `StoreError::corrupt_source(e)`; callers decide from the error class, never from the variant.
2. **Match what the existing adapters promise**: the table "How each adapter guarantees the
   contract" in the root `README.md` lists, per guarantee, how Postgres and MongoDB do it
   (CAS commit, first-writer-wins journal, exclusive claiming, `busy` runs never claimed,
   pinned claims and the run `owner`, one open run per conversation, journal deleted with its
   run). Time is truncated to milliseconds in every store; lease expiry uses the `now` the caller
   passes.
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
   ```

   `store_conformance!` generates one `#[tokio::test]` per case (28 at the time of writing) and
   calls `migrate` first. `None` skips the suite. Cases isolate themselves with a unique agent
   name, so they run in parallel on one shared database.
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
* A run's state is JSON: a backend that cannot hold some values (a NUL in a Postgres `JSONB`
  string, an integer above `i64::MAX` in BSON) must reject them with `InvalidInput`, not a driver
  error (root `README.md`, "Data caveats").
* Claiming must be exclusive under concurrency (the suite races workers); a read-then-write claim
  without a conditional write fails it.
* `release_lease` never clears the run's owner (pinned claims).
* Implementing the trait with default or `unimplemented!()` bodies to get a bump compiling: the
  suite exists to catch exactly that.
* A transient backend error must be classified transient so the runtime retries it.

## See also

* `crates/adam-core/README.md`, `crates/adam-store-testkit/README.md`,
  `crates/adam-notify-testkit/README.md`, "To add a backend" in the root `README.md`
  ("Testing").
* `adam-upgrade` (finding the trait change in the first place), `adam-embed`.
* https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-store-testkit/README.md
