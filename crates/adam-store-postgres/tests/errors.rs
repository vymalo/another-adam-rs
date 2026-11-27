//! How the Postgres adapter reports failures. The tests that need a server are
//! gated like the conformance suite; `an_unreachable_server_is_transient` runs
//! offline.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use adam_core::{NewRun, Store, StoreError};
use adam_error::{Classify, ErrorClass};
use adam_postgres::{PgConfig, classify};
use adam_store_postgres::PgStore;
use serde_json::json;

fn uuid_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

/// Regression for A2: a row that cannot be decoded used to be a retryable `Backend` error, so a
/// poisoned row was retried for ever. It is `Corrupt` now, and the driver error is the source.
#[tokio::test]
async fn an_undecodable_row_is_corrupt_and_not_retryable() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let prefix = format!("adam_err_{}_", uuid_suffix());
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&prefix)
        .unwrap();
    store.migrate().await.unwrap();
    let run = store
        .create_run(NewRun::new("errors-agent", json!({})))
        .await
        .unwrap();

    // Hand-alter the row's shape: `created_at` is read as a timestamp. (`state` is no longer the
    // column a read always decodes — the contract's `load_run` reads the head when it is current —
    // so a type change there would fail to plan, not to decode.)
    support::drop_claim_function(&pool, &prefix).await;
    support::exec(
        &pool,
        &format!(
            "ALTER TABLE {prefix}runs ALTER COLUMN created_at TYPE text USING created_at::text"
        ),
    )
    .await;

    let err = store.load_run(run.id).await.unwrap_err();
    let (class, retryable) = (err.class(), err.is_retryable());
    let source_is_driver = std::error::Error::source(&err)
        .is_some_and(|s| s.downcast_ref::<tokio_postgres::Error>().is_some());

    for table in ["journal", "runs", "meta"] {
        support::exec(
            &pool,
            &format!("DROP TABLE IF EXISTS {prefix}{table} CASCADE"),
        )
        .await;
    }

    assert!(matches!(err, StoreError::Backend { .. }), "{err:?}");
    assert_eq!(class, ErrorClass::Corrupt);
    assert!(!retryable);
    assert!(source_is_driver);
}

/// An unreachable server is transient, and the error carries the driver's own error.
#[tokio::test]
async fn an_unreachable_server_is_transient() {
    let config = PgConfig::parse("postgres://postgres:postgres@127.0.0.1:1/none").expect("a URL");
    let store = PgStore::from_config(config, 2).expect("a lazy store");
    let err = store.migrate().await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Transient, "{err:?}");
    assert!(err.is_retryable());
    assert!(
        std::error::Error::source(&err)
            .is_some_and(|s| s.downcast_ref::<tokio_postgres::Error>().is_some())
    );
}

/// A unique violation keeps the variant the store gives it (`AlreadyExists`), not a backend error.
#[tokio::test]
async fn a_unique_violation_stays_already_exists() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let prefix = format!("adam_err_{}_", uuid_suffix());
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&prefix)
        .unwrap();
    store.migrate().await.unwrap();
    let new = NewRun::new("errors-agent", json!({"n": 1}));
    let run = store.create_run(new.clone()).await.unwrap();
    let err = store.create_run(new).await.unwrap_err();
    assert!(
        matches!(err, StoreError::AlreadyExists(id) if id == run.id),
        "{err:?}"
    );

    for table in ["journal", "runs", "meta"] {
        support::exec(
            &pool,
            &format!("DROP TABLE IF EXISTS {prefix}{table} CASCADE"),
        )
        .await;
    }
}

/// A serialization failure and a deadlock are `Transient`; a syntax error and a violated CHECK are
/// `Internal`. Provoked on a real server, with the classes the store maps them to.
#[tokio::test]
async fn sqlstates_keep_their_class_against_a_real_server() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 1);
    let client = pool.get().await.unwrap();
    for (state, want) in [
        ("40001", ErrorClass::Transient), // serialization failure
        ("40P01", ErrorClass::Transient), // deadlock
        ("08006", ErrorClass::Transient), // connection failure
        ("53300", ErrorClass::Transient), // too many connections
        ("57P01", ErrorClass::Transient), // admin shutdown
        ("42601", ErrorClass::Internal),  // syntax error
        ("23514", ErrorClass::Internal),  // check violation
    ] {
        let stmt = format!("DO $$ BEGIN RAISE EXCEPTION 'boom' USING ERRCODE = '{state}'; END $$");
        let err = client.batch_execute(&stmt).await.expect_err("it raises");
        assert_eq!(classify(err).class(), want, "SQLSTATE {state}");
    }
    // A real syntax error on a raw query, not a raised one.
    let err = client
        .batch_execute("SELEC 1")
        .await
        .expect_err("syntax error");
    assert_eq!(classify(err).class(), ErrorClass::Internal);
}

/// A connection the server closed is `Transient`, and the error carries the driver's error.
#[tokio::test]
async fn a_lost_connection_is_transient() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 1);
    let client = pool.get().await.unwrap();
    let pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let admin = support::pool(&url, 1);
    let killed: bool = support::scalar(&admin, "SELECT pg_terminate_backend($1)", &[&pid]).await;
    assert!(killed, "the backend was terminated");
    let err = client.query_one("SELECT 1", &[]).await.unwrap_err();
    let mapped = classify(err);
    assert_eq!(mapped.class(), ErrorClass::Transient, "{mapped:?}");
    assert!(mapped.is_retryable());
}

/// A statement that outlasts `statement_timeout` is cancelled by the server as a transient error,
/// and the pool is usable afterwards. The option is the one `PgStore::from_config` sets from
/// `ADAM_PG_STATEMENT_TIMEOUT_MS` (the environment is not touched here: tests share a process).
#[tokio::test]
async fn a_statement_past_the_timeout_is_transient_and_the_pool_survives() {
    let Some(url) = support::test_url() else {
        return;
    };
    let config = PgConfig::parse(&url)
        .unwrap()
        .with_statement_timeout_ms(200);
    let pool = config.pool(2).unwrap();
    let client = pool.get().await.unwrap();
    let err = classify(
        client
            .batch_execute("SELECT pg_sleep(5)")
            .await
            .unwrap_err(),
    );
    drop(client);
    assert_eq!(err.class(), ErrorClass::Transient, "{err:?}");
    let client = pool.get().await.unwrap();
    client.batch_execute("SELECT 1").await.unwrap();
}
