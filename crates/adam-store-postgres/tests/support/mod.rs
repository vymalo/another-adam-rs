//! Shared helpers for the store tests that need a real PostgreSQL server.
#![allow(dead_code)] // each integration test binary uses a different subset

use adam_postgres::PgConfig;
use deadpool_postgres::Pool;
use tokio_postgres::types::{FromSql, ToSql};

/// The server the database tests run against, or `None` when they are not enabled. With
/// `ADAM_TEST_REQUIRE_DB=1` a missing URL is a failure instead (see `adam_core::testing`).
pub fn test_url() -> Option<String> {
    adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")
}

/// A pool of `max_size` connections on `url`.
pub fn pool(url: &str, max_size: usize) -> Pool {
    PgConfig::parse(url)
        .expect("DATABASE_URL")
        .pool(max_size)
        .expect("pool")
}

/// Run one or more statements with the simple query protocol (no binds).
pub async fn exec(pool: &Pool, sql: &str) {
    pool.get()
        .await
        .expect("connection")
        .batch_execute(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The first column of the single row `sql` returns.
pub async fn scalar<T>(pool: &Pool, sql: &str, params: &[&(dyn ToSql + Sync)]) -> T
where
    T: for<'a> FromSql<'a>,
{
    let client = pool.get().await.expect("connection");
    client
        .query_one(sql, params)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get(0)
}

/// The first column of the row `sql` returns, if it returns one.
pub async fn optional<T>(pool: &Pool, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Option<T>
where
    T: for<'a> FromSql<'a>,
{
    let client = pool.get().await.expect("connection");
    client
        .query_opt(sql, params)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .map(|row| row.get(0))
}
