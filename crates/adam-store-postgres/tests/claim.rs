//! `FOR UPDATE SKIP LOCKED`: a claim never waits for a row another transaction holds, and both
//! claims stay one statement.
//!
//! This is the PostgreSQL guarantee the shared conformance suite cannot state (MongoDB has no
//! row locks): a claim that finds a due row locked by another transaction takes the next one at
//! once instead of queueing behind it.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::time::Duration;

use adam_core::store::now;
use adam_core::{ClaimScope, NewPushConfig, NewRun, Store};
use adam_store_postgres::PgStore;
use serde_json::json;

/// Two due runs of one agent, the first of them locked by another transaction. A claim of one must
/// return the second, not wait for the first.
#[tokio::test]
async fn a_run_claim_skips_a_row_another_transaction_holds() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 4);
    let prefix = format!(
        "adam_lock_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&prefix)
        .unwrap();
    store.migrate().await.unwrap();
    let agent = format!(
        "lock-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let first = store
        .create_run(NewRun::new(&agent, json!({"n": 1})))
        .await
        .unwrap();
    // A later `sched_at`, so the claim order would pick `first` first.
    tokio::time::sleep(Duration::from_millis(3)).await;
    let second = store
        .create_run(NewRun::new(&agent, json!({"n": 2})))
        .await
        .unwrap();

    // Another transaction holds the first run's row lock.
    let mut holder = pool.get().await.unwrap();
    let tx = holder.transaction().await.unwrap();
    tx.execute(
        &format!("SELECT id FROM {prefix}runs WHERE id = $1 FOR UPDATE"),
        &[&first.id.0],
    )
    .await
    .unwrap();

    let claimed = tokio::time::timeout(
        Duration::from_secs(2),
        store.claim_due(
            std::slice::from_ref(&agent),
            "worker",
            ClaimScope::Any,
            &[],
            now() + jiff::SignedDuration::from_secs(1),
            Duration::from_secs(30),
            1,
        ),
    )
    .await
    .expect("a claim must not wait for a locked row")
    .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].run.id, second.id,
        "the locked run was skipped, not waited for"
    );

    tx.rollback().await.unwrap();
    support::exec(&pool, &format!("DROP TABLE {prefix}runs CASCADE")).await;
}

/// The same for a push-configuration claim: the locked config is skipped and the other claimed.
#[tokio::test]
async fn a_push_claim_skips_a_row_another_transaction_holds() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 4);
    let prefix = format!(
        "adam_lock_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&prefix)
        .unwrap();
    store.migrate().await.unwrap();
    let agent = format!(
        "lock-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let run = store
        .create_run(NewRun::new(&agent, json!({})))
        .await
        .unwrap();
    let config = |id: &str| NewPushConfig {
        run: run.id,
        id: id.to_owned(),
        agent: agent.clone(),
        owner: "owner".to_owned(),
        config: json!({"url": "https://example.invalid/hook"}),
        cursor: json!({}),
    };
    store.push_put(config("a")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    store.push_put(config("b")).await.unwrap();

    let mut holder = pool.get().await.unwrap();
    let tx = holder.transaction().await.unwrap();
    tx.execute(
        &format!("SELECT run_id FROM {prefix}push WHERE run_id = $1 AND id = 'a' FOR UPDATE"),
        &[&run.id.0],
    )
    .await
    .unwrap();

    let claimed = tokio::time::timeout(
        Duration::from_secs(2),
        store.push_claim_due(
            std::slice::from_ref(&agent),
            "worker",
            now() + jiff::SignedDuration::from_secs(1),
            Duration::from_secs(30),
            1,
        ),
    )
    .await
    .expect("a push claim must not wait for a locked row")
    .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, "b", "the locked config was skipped");

    tx.rollback().await.unwrap();
    support::exec(&pool, &format!("DROP TABLE {prefix}runs CASCADE")).await;
}
