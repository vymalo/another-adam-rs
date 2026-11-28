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
//! Every run is clean whatever a previous one left behind: the table prefix is unique to the
//! process, and the scope the kit hands `make` gains the same per-process suffix (so the kit's two
//! scopes still differ).
#![cfg(feature = "memory")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, OnceLock};

use adam_core::Store;
use adam_core::testing::test_env;
use adam_memory::{DynMemory, Memory, MemoryScope};
use adam_memory_testkit::{LessonPlan, audience, lesson_draft, memory_scope};
use adam_postgres::PgConfig;
use adam_store_postgres::{PgMemory, PgStore};
use deadpool_postgres::Pool;

/// One identity per test process (the pid, plus a random suffix so a recycled pid on a re-run is
/// still a fresh database): the tail of the table prefix and of every scope name.
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

/// The table prefix of this process: `adam_mem_<pid>_<random>_`.
fn table_prefix() -> String {
    format!("adam_mem_{}_", identity())
}

fn url() -> String {
    test_env("ADAM_TEST_POSTGRES_URL").expect(
        "ADAM_TEST_POSTGRES_URL: the memory suite needs a server (ADAM_TEST_REQUIRE_DB=1 makes a \
         missing URL this failure; without it test_env only notes the skip and the suite cannot \
         skip from inside make)",
    )
}

/// One pool per process: the kit's two-scope case and the explicit one build both adapters over
/// the same pool and the same database.
fn pool() -> Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        PgConfig::parse(&url())
            .expect("ADAM_TEST_POSTGRES_URL")
            .pool(8)
            .expect("a lazy pool")
    })
    .clone()
}

/// Migrate the store schema (which holds the memory table at schema version 10) once per process.
fn migrate_once() {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        rt.block_on(async {
            PgStore::from_pool(pool())
                .with_table_prefix(&table_prefix())
                .expect("prefix")
                .migrate()
                .await
                .expect("the store schema, once per process");
        });
    });
}

/// The scope the kit hands `make`, with this process's suffix, so a re-run is clean and the kit's
/// two scopes still differ from each other.
fn scoped(scope: &MemoryScope) -> MemoryScope {
    MemoryScope::new(format!("{}-{}", scope.as_str(), identity()))
}

fn make(plan: &LessonPlan, scope: &MemoryScope) -> DynMemory {
    migrate_once();
    Arc::new(
        PgMemory::from_pool(pool(), plan.settings.clone(), scoped(scope))
            .expect("a PgMemory over the test pool")
            .with_table_prefix(&table_prefix())
            .expect("prefix"),
    )
}

adam_memory_testkit::memory_conformance!(crate::make);

/// The kit's decisive case, spelled out on its own: two `PgMemory` values with two scopes over one
/// pool and one database never see each other's lessons, though both share the table.
#[test]
fn two_scopes_on_one_database_never_see_each_other() {
    let plan = LessonPlan::default();
    // Both over the same pool **and the same table prefix**: two configured scopes on one table.
    // A scope-blind adapter over this pair fails the kit's `two_scopes_never_see_each_other`.
    let first: PgMemory = PgMemory::from_pool(
        pool(),
        plan.settings.clone(),
        scoped(&memory_scope("explicit-a")),
    )
    .expect("the first memory")
    .with_table_prefix(&table_prefix())
    .expect("prefix");
    let second: PgMemory = PgMemory::from_pool(
        pool(),
        plan.settings.clone(),
        scoped(&memory_scope("explicit-b")),
    )
    .expect("the second memory")
    .with_table_prefix(&table_prefix())
    .expect("prefix");
    // The memory's own door: `PgMemory::migrate` runs the memory DDL alone (idempotent — the store
    // schema holds the same table at version 10; the kit cases migrate it once per process).
    first.migrate().expect("the memory DDL");
    // Both are built from `pool()`: one pool, one database, one table — two scopes.
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
