//! How the Postgres adapter reports failures. The tests that need a server are
//! gated like the conformance suite; `an_unreachable_server_is_transient` runs
//! offline.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use adam_core::{NewRun, Store, StoreError};
use adam_error::{Classify, ErrorClass};
use adam_store_postgres::PgStore;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

/// Regression for A2: a row that cannot be decoded used to be a retryable `Backend` error, so a
/// poisoned row was retried for ever. It is `Corrupt` now, and the driver error is the source.
#[tokio::test]
async fn an_undecodable_row_is_corrupt_and_not_retryable() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let prefix = format!("adam_err_{}_", uuid_suffix());
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&prefix)
        .unwrap();
    store.migrate().await.unwrap();
    let run = store
        .create_run(NewRun::new("errors-agent".to_owned(), json!({})))
        .await
        .unwrap();

    // Hand-alter the row's shape: `state` is read as JSON.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE {prefix}runs ALTER COLUMN state TYPE text USING state::text"
    )))
    .execute(&pool)
    .await
    .unwrap();

    let err = store.load_run(run.id).await.unwrap_err();
    let (class, retryable) = (err.class(), err.is_retryable());
    let source_is_sqlx =
        std::error::Error::source(&err).is_some_and(|s| s.downcast_ref::<sqlx::Error>().is_some());

    for table in ["journal", "runs", "meta"] {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE IF EXISTS {prefix}{table} CASCADE"
        )))
        .execute(&pool)
        .await;
    }

    assert!(matches!(err, StoreError::Backend { .. }), "{err:?}");
    assert_eq!(class, ErrorClass::Corrupt);
    assert!(!retryable);
    assert!(source_is_sqlx);
}

/// An unreachable server is transient, and the error carries the driver's own error.
#[tokio::test]
async fn an_unreachable_server_is_transient() {
    let pool = PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .connect_lazy("postgres://postgres:postgres@127.0.0.1:1/none")
        .unwrap();
    let err = PgStore::from_pool(pool).migrate().await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Transient, "{err:?}");
    assert!(err.is_retryable());
    assert!(std::error::Error::source(&err).is_some_and(|s| s.is::<sqlx::Error>()));
}

fn uuid_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}
