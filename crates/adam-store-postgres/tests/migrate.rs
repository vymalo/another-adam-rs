//! Migrating a schema version 1 database (no `owner` column, no `push` table) to the current one. Gated on
//! `ADAM_TEST_POSTGRES_URL` like the conformance suite.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::time::Duration;

use adam_core::store::now;
use adam_core::{ClaimScope, RunStatus, RunUpdate, Store};
use adam_store_postgres::{PgStore, SCHEMA_VERSION};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};

/// The tables exactly as schema version 1 made them: no `owner` column, version row `1`.
async fn create_v1_schema(pool: &PgPool, p: &str) {
    for stmt in [
        format!("CREATE TABLE {p}meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"),
        format!(
            "CREATE TABLE {p}runs (
                id              UUID        NOT NULL,
                agent           TEXT        NOT NULL,
                conversation_id TEXT,
                parent_id       UUID,
                status          TEXT        NOT NULL CHECK (status IN ('runnable', 'parked', 'done', 'failed')),
                state           JSONB       NOT NULL,
                wake_at         TIMESTAMPTZ,
                sched_at        TIMESTAMPTZ,
                version         BIGINT      NOT NULL CHECK (version > 0),
                lease_owner     TEXT,
                lease_until     TIMESTAMPTZ,
                created_at      TIMESTAMPTZ NOT NULL,
                updated_at      TIMESTAMPTZ NOT NULL,
                CONSTRAINT {p}runs_pkey PRIMARY KEY (id)
            )"
        ),
        format!(
            "CREATE TABLE {p}journal (
                run_id      UUID        NOT NULL REFERENCES {p}runs (id) ON DELETE CASCADE,
                seq         BIGINT      NOT NULL CHECK (seq >= 0),
                name        TEXT        NOT NULL,
                ok          BOOLEAN     NOT NULL,
                payload     JSONB       NOT NULL,
                recorded_at TIMESTAMPTZ NOT NULL,
                PRIMARY KEY (run_id, seq)
            )"
        ),
        format!("INSERT INTO {p}meta (key, value) VALUES ('schema_version', '1')"),
    ] {
        sqlx::query(AssertSqlSafe(stmt)).execute(pool).await.unwrap();
    }
}

async fn drop_tables(pool: &PgPool, p: &str) {
    for table in ["push", "journal", "runs", "meta"] {
        let _ = sqlx::query(AssertSqlSafe(format!(
            "DROP TABLE IF EXISTS {p}{table} CASCADE"
        )))
        .execute(pool)
        .await;
    }
}

async fn version(pool: &PgPool, p: &str) -> String {
    sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT value FROM {p}meta WHERE key = 'schema_version'"
    )))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn has_owner_column(pool: &PgPool, p: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                         WHERE table_name = $1 AND column_name = 'owner')",
    )
    .bind(format!("{p}runs"))
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_version_1_schema_migrates_to_the_current_one_and_keeps_its_runs() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    create_v1_schema(&pool, &p).await;
    assert!(!has_owner_column(&pool, &p).await);

    // A run a version 1 process left behind, with a lease that has long expired.
    let id = uuid::Uuid::now_v7();
    let old = now() - chrono::Duration::hours(1);
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {p}runs (id, agent, status, state, sched_at, version, lease_owner, lease_until,
                              created_at, updated_at)
         VALUES ($1, 'legacy', 'runnable', '{{\"turn\": 3}}', $2, 7, 'dead-worker', $2, $2, $2)"
    )))
    .bind(id)
    .bind(old)
    .execute(&pool)
    .await
    .unwrap();

    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    assert!(has_owner_column(&pool, &p).await, "the column was added");
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());
    assert_eq!(SCHEMA_VERSION, 3);
    // The push table exists now (a missing table would be a backend error).
    assert!(
        store
            .push_list(adam_core::RunId(id))
            .await
            .unwrap()
            .is_empty()
    );

    // Nothing was lost, and migrating again changes nothing.
    store.migrate().await.unwrap();
    assert_eq!(version(&pool, &p).await, "3");
    let run = store
        .load_run(adam_core::RunId(id))
        .await
        .unwrap()
        .expect("the legacy run survived");
    assert_eq!(run.version, 7);
    assert_eq!(run.state, json!({"turn": 3}));

    // The legacy run has no owner: the first pinned claimant gets it, and keeps it.
    let ttl = Duration::from_secs(30);
    let agents = ["legacy".to_owned()];
    let claim = |worker: &'static str, scope: ClaimScope| {
        let (store, agents) = (store.clone(), agents.clone());
        async move {
            // A commit makes a run due at the time of the commit, so look a moment ahead.
            let at = now() + chrono::Duration::seconds(1);
            store
                .claim_due(&agents, worker, scope, &[], at, ttl, 10)
                .await
                .unwrap()
        }
    };
    assert_eq!(claim("w1", ClaimScope::Pinned).await.len(), 1);
    store.release_lease(run.id, "w1").await.unwrap();
    assert!(claim("w2", ClaimScope::Pinned).await.is_empty());
    let run = store
        .commit_run(
            run.id,
            7,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 4})),
        )
        .await
        .unwrap();
    assert_eq!(run.version, 8);
    assert!(claim("w2", ClaimScope::Pinned).await.is_empty());
    assert_eq!(claim("w1", ClaimScope::Pinned).await.len(), 1);

    drop_tables(&pool, &p).await;
}

#[tokio::test]
async fn an_older_release_migrating_does_not_lower_the_version() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    // A future release wrote version 9.
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {p}meta SET value = '9' WHERE key = 'schema_version'"
    )))
    .execute(&pool)
    .await
    .unwrap();
    store.migrate().await.unwrap();
    assert_eq!(version(&pool, &p).await, "9");
    drop_tables(&pool, &p).await;
}

/// Schema version 2: version 1 plus the `owner` column, still no `push` table.
async fn create_v2_schema(pool: &PgPool, p: &str) {
    create_v1_schema(pool, p).await;
    for stmt in [
        format!("ALTER TABLE {p}runs ADD COLUMN owner TEXT"),
        format!("UPDATE {p}meta SET value = '2' WHERE key = 'schema_version'"),
    ] {
        sqlx::query(AssertSqlSafe(stmt))
            .execute(pool)
            .await
            .unwrap();
    }
}

async fn has_table(pool: &PgPool, name: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_version_2_schema_migrates_to_3_and_gets_the_push_table() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    create_v2_schema(&pool, &p).await;
    assert!(!has_table(&pool, &format!("{p}push")).await);

    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    assert_eq!(version(&pool, &p).await, "3");
    assert!(has_table(&pool, &format!("{p}push")).await);

    drop_tables(&pool, &p).await;
}

/// A start on a current schema runs no DDL, so it takes no table lock and cannot deadlock with a
/// process that is working on the same tables (a `push_put` locks push then runs, a purge locks
/// runs then push; the migration's `ALTER TABLE` took `AccessExclusiveLock` on runs even as a
/// no-op).
#[tokio::test]
async fn a_start_on_a_current_schema_takes_no_table_lock() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap()
        .migrate()
        .await
        .unwrap();

    // A process in the middle of its work: row locks on both tables, held open.
    let mut holder = pool.begin().await.unwrap();
    for table in ["runs", "push"] {
        sqlx::query(AssertSqlSafe(format!(
            "LOCK TABLE {p}{table} IN ROW EXCLUSIVE MODE"
        )))
        .execute(&mut *holder)
        .await
        .unwrap();
    }

    // Another process starts: its own pool, the same prefix.
    let other = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let store = PgStore::from_pool(other.clone())
        .with_table_prefix(&p)
        .unwrap();
    let started = tokio::time::timeout(Duration::from_secs(5), store.migrate()).await;

    holder.rollback().await.unwrap();
    drop_tables(&pool, &p).await;
    started
        .expect("a start on a current schema waited for a table lock")
        .unwrap();
}
