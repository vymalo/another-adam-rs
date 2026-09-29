//! Runs the shared conformance suite against a real PostgreSQL.
//!
//! ```sh
//! ADAM_TEST_POSTGRES_URL=postgres://postgres@localhost:5432/adam_test cargo test -p adam-store-postgres
//! ```
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it fails).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_store_postgres::PgStore;
use sqlx::postgres::PgPoolOptions;

async fn make_store() -> Option<adam_core::DynStore> {
    let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
    // Each #[tokio::test] has its own runtime, so each gets its own small pool.
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect");
    let prefix = std::env::var("ADAM_TEST_TABLE_PREFIX").unwrap_or_else(|_| "adam_test_".into());
    Some(Arc::new(
        PgStore::from_pool(pool)
            .with_table_prefix(&prefix)
            .expect("prefix"),
    ))
}

adam_store_testkit::store_conformance!(make_store);
