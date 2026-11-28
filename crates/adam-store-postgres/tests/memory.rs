//! [`adam_memory_testkit::memory_conformance!`] against the PostgreSQL adapter (feature `memory`),
//! and the isolation of two configured scopes on one database.
//!
//! ```sh
//! ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
//!   cargo nextest run -p adam-store-postgres --features memory
//! ```
//!
//! The URL is read through [`test_env`]: with `ADAM_TEST_REQUIRE_DB=1` a missing URL is a failure.
//! Without the flag `test_env` notes the skip, but `memory_conformance!` has no skip hook (`make`
//! must return a `DynMemory`), so a feature-enabled run without a database fails loudly rather
//! than passing silently. The suite is behind the `memory` feature anyway, so `cargo test
//! --workspace` on a bare machine compiles none of it.
//!
//! Every run is clean whatever a previous one left behind: the table is the suite's own, created
//! idempotently, and the scope the kit hands `make` gains a per-process suffix (so the kit's two
//! scopes still differ, and two processes never read each other's rows). The suite leaves its
//! lessons in that table, scoped by the suffix; a development database keeps a few hundred small
//! rows per run, and `TRUNCATE <prefix>memory_lesson` clears them.
#![cfg(feature = "memory")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, OnceLock};

use adam_core::testing::test_env;
use adam_memory::{DynMemory, Memory, MemoryScope};
use adam_memory_testkit::{LessonPlan, audience, lesson_draft, memory_scope};
use adam_postgres::PgConfig;
use adam_store_postgres::PgMemory;
use deadpool_postgres::Pool;

/// One identity per test process (the pid, plus a random suffix so a recycled pid on a re-run is
/// still a fresh set of scopes): the tail of every scope name.
fn identity() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        format!(
            "{}_{}",
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        )
    })
}

/// The suite's table prefix: one table shared by every process, created idempotently under the
/// migration lock, so a run leaves one table behind and not one schema per test process.
const TABLE_PREFIX: &str = "adam_mem_test_";

fn url() -> String {
    test_env("ADAM_TEST_POSTGRES_URL").expect(
        "ADAM_TEST_POSTGRES_URL: the memory suite needs a server (ADAM_TEST_REQUIRE_DB=1 makes a \
         missing URL this failure; without it test_env only notes the skip and the suite cannot \
         skip from inside make)",
    )
}

/// A small pool per adapter: a [`PgMemory`] owns its runtime, and a connection's driver task lives
/// on the runtime that opened it, so two adapters never share a pool across two runtimes. Two
/// connections are enough for the suite, and the two-scope case gets two pools on one database.
fn pool() -> Pool {
    PgConfig::parse(&url())
        .expect("ADAM_TEST_POSTGRES_URL")
        .pool(2)
        .expect("a lazy pool")
}

/// Create the memory table once per process, through the adapter's own door (`PgMemory::migrate`,
/// the memory DDL alone; the run store's migration creates the same table at schema version 10).
fn migrate_once() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let plan = LessonPlan::default();
        PgMemory::from_pool(pool(), plan.settings.clone(), MemoryScope::new("migrate"))
            .expect("a PgMemory")
            .with_table_prefix(TABLE_PREFIX)
            .expect("prefix")
            .migrate()
            .expect("the memory DDL, once per process");
    });
}

/// The scope the kit hands `make`, with this process's suffix, so a re-run is clean and the kit's
/// two scopes still differ from each other.
///
/// An **empty** scope is handed on empty: the suffix would turn it into a valid one, and the kit's
/// `an_empty_configured_scope_is_refused` case exists precisely to see the adapter refuse it.
fn scoped(scope: &MemoryScope) -> MemoryScope {
    if scope.as_str().is_empty() {
        return scope.clone();
    }
    MemoryScope::new(format!("{}-{}", scope.as_str(), identity()))
}

fn make(plan: &LessonPlan, scope: &MemoryScope) -> DynMemory {
    migrate_once();
    Arc::new(
        PgMemory::from_pool(pool(), plan.settings.clone(), scoped(scope))
            .expect("a PgMemory over its own pool")
            .with_table_prefix(TABLE_PREFIX)
            .expect("prefix"),
    )
}

adam_memory_testkit::memory_conformance!(crate::make);

/// The kit's decisive case, spelled out on its own: two `PgMemory` values with two scopes over one
/// database never see each other's lessons, though both share the table.
#[test]
fn two_scopes_on_one_database_never_see_each_other() {
    let plan = LessonPlan::default();
    // Each adapter has its own pool, on purpose: they share the database and the table, not a
    // pool. A scope-blind adapter over this pair fails the kit's `two_scopes_never_see_each_other`.
    let first = PgMemory::from_pool(
        pool(),
        plan.settings.clone(),
        scoped(&memory_scope("explicit-a")),
    )
    .expect("the first memory")
    .with_table_prefix(TABLE_PREFIX)
    .expect("prefix");
    let second = PgMemory::from_pool(
        pool(),
        plan.settings.clone(),
        scoped(&memory_scope("explicit-b")),
    )
    .expect("the second memory")
    .with_table_prefix(TABLE_PREFIX)
    .expect("prefix");
    // The memory's own door: `PgMemory::migrate` runs the memory DDL alone (idempotent — the kit
    // cases migrate it once per process; this proves it works on its own).
    first.migrate().expect("the memory DDL");
    let audience = audience("explicit-isolation");
    first
        .record(lesson_draft(&audience, "the lockfile is stale").remedy("regenerate the lockfile"))
        .expect("the first scope records");
    assert!(
        second
            .recall(&audience)
            .expect("a readable memory")
            .is_empty(),
        "the second scope saw the first scope's lesson"
    );
    second
        .record(lesson_draft(&audience, "the lockfile is stale").remedy("delete the lockfile"))
        .expect("the second scope records its own");
    assert_eq!(
        first.recall(&audience).expect("a readable memory")[0].remedy,
        "regenerate the lockfile",
        "the second scope's record overwrote the first scope's lesson"
    );
    second
        .retire(&audience, "the lockfile is stale")
        .expect("the second scope retires its own lesson");
    assert_eq!(
        first.recall(&audience).expect("a readable memory").len(),
        1,
        "retiring in one scope retired the other scope's lesson"
    );
    assert!(
        second
            .recall(&audience)
            .expect("a readable memory")
            .is_empty()
    );
}
