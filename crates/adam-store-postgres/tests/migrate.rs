//! Migrating a schema version 1 database (no `owner` column, no `push` table) to the current one. Gated on
//! `ADAM_TEST_POSTGRES_URL` like the conformance suite.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use std::time::Duration;

use adam_core::store::now;
use adam_core::{ClaimScope, ConversationScope, NewRun, RunQuery, RunStatus, RunUpdate, Store};
use adam_store_postgres::{PgStore, SCHEMA_VERSION};
use deadpool_postgres::Pool;
use serde_json::json;

/// The tables exactly as schema version 1 made them: no `owner` column, version row `1`.
async fn create_v1_schema(pool: &Pool, p: &str) {
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
        support::exec(pool, &stmt).await;
    }
}

async fn drop_tables(pool: &Pool, p: &str) {
    for table in ["run_blob", "blob", "push", "journal", "runs", "meta"] {
        support::exec(pool, &format!("DROP TABLE IF EXISTS {p}{table} CASCADE")).await;
    }
}

async fn version(pool: &Pool, p: &str) -> String {
    support::scalar(
        pool,
        &format!("SELECT value FROM {p}meta WHERE key = 'schema_version'"),
        &[],
    )
    .await
}

async fn has_owner_column(pool: &Pool, p: &str) -> bool {
    support::scalar(
        pool,
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                         WHERE table_name = $1 AND column_name = 'owner')",
        &[&format!("{p}runs")],
    )
    .await
}

#[tokio::test]
async fn a_version_1_schema_migrates_to_the_current_one_and_keeps_its_runs() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 4);
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    create_v1_schema(&pool, &p).await;
    assert!(!has_owner_column(&pool, &p).await);

    // A run a version 1 process left behind, with a lease that has long expired.
    let id = uuid::Uuid::now_v7();
    let old = now() - jiff::SignedDuration::from_hours(1);
    let client = pool.get().await.unwrap();
    client
        .execute(
            &format!(
                "INSERT INTO {p}runs (id, agent, status, state, sched_at, version, lease_owner, lease_until,
                                      created_at, updated_at)
                 VALUES ($1, 'legacy', 'runnable', '{{\"turn\": 3}}', $2, 7, 'dead-worker', $2, $2, $2)"
            ),
            &[&id, &old.0],
        )
        .await
        .unwrap();

    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    assert!(has_owner_column(&pool, &p).await, "the column was added");
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());
    assert_eq!(SCHEMA_VERSION, 6);
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
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());
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
            let at = now() + jiff::SignedDuration::from_secs(1);
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
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    // A future release wrote version 9.
    support::exec(
        &pool,
        &format!("UPDATE {p}meta SET value = '9' WHERE key = 'schema_version'"),
    )
    .await;
    store.migrate().await.unwrap();
    assert_eq!(version(&pool, &p).await, "9");
    drop_tables(&pool, &p).await;
}

/// Schema version 2: version 1 plus the `owner` column, still no `push` table.
async fn create_v2_schema(pool: &Pool, p: &str) {
    create_v1_schema(pool, p).await;
    for stmt in [
        format!("ALTER TABLE {p}runs ADD COLUMN owner TEXT"),
        format!("UPDATE {p}meta SET value = '2' WHERE key = 'schema_version'"),
    ] {
        support::exec(pool, &stmt).await;
    }
}

async fn has_table(pool: &Pool, name: &str) -> bool {
    support::scalar(pool, "SELECT to_regclass($1) IS NOT NULL", &[&name]).await
}

#[tokio::test]
async fn a_version_2_schema_migrates_to_the_current_one_and_gets_the_push_table() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
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
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());
    assert!(has_table(&pool, &format!("{p}push")).await);

    drop_tables(&pool, &p).await;
}

async fn index_definition(pool: &Pool, name: &str) -> Option<String> {
    support::optional(
        pool,
        "SELECT indexdef FROM pg_indexes
          WHERE schemaname = current_schema() AND indexname = $1",
        &[&name],
    )
    .await
}

/// `Some(true)` for a valid index, `Some(false)` for one a failed build left INVALID, `None` for
/// no such index.
async fn index_valid(pool: &Pool, name: &str) -> Option<bool> {
    support::optional(
        pool,
        "SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)",
        &[&name],
    )
    .await
}

/// The listing index answers the status filter from the index (`INCLUDE (status)`), so a page does
/// not fetch the rows it filters out. Schema version 4 gives it a new name, `runs_list_status`: the
/// upgrade builds it beside the version 3 `runs_list` and drops that one only afterwards, so a
/// database made by version 3 ends with the new index, valid, and without the old one.
#[tokio::test]
async fn the_listing_index_answers_the_status_filter_and_replaces_the_version_3_one() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    let new_index = format!("{p}runs_list_status");
    let old_index = format!("{p}runs_list");

    store.migrate().await.unwrap();
    let fresh = index_definition(&pool, &new_index)
        .await
        .expect("the listing index");
    assert!(
        fresh.contains("INCLUDE (status)"),
        "the listing index covers the status filter: {fresh}"
    );
    assert!(fresh.contains("updated_at DESC"), "{fresh}");
    assert_eq!(
        index_definition(&pool, &old_index).await,
        None,
        "a fresh database never gets the version 3 name"
    );
    assert_eq!(index_valid(&pool, &new_index).await, Some(true));

    // Runs the listing must still find after the upgrade.
    let agent = format!(
        "listing-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let first = store
        .create_run(NewRun::new(&agent, json!({"n": 1})).conversation("c:1"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(3)).await;
    let second = store
        .create_run(NewRun::new(&agent, json!({"n": 2})).conversation("c:2"))
        .await
        .unwrap();

    // A version 3 database: the same index under its old name, without the included column, and
    // the version row.
    support::exec(&pool, &format!("DROP INDEX {new_index}")).await;
    support::exec(
        &pool,
        &format!(
            "CREATE INDEX {old_index} ON {p}runs (agent, (conversation_id COLLATE \"C\"),
                                                updated_at DESC, id DESC)
             WHERE conversation_id IS NOT NULL"
        ),
    )
    .await;
    support::exec(
        &pool,
        &format!("UPDATE {p}meta SET value = '3' WHERE key = 'schema_version'"),
    )
    .await;
    assert!(index_definition(&pool, &new_index).await.is_none());
    assert!(
        !index_definition(&pool, &old_index)
            .await
            .unwrap()
            .contains("INCLUDE")
    );

    store.migrate().await.unwrap();
    let migrated = index_definition(&pool, &new_index)
        .await
        .expect("the new listing index");
    assert!(
        migrated.contains("INCLUDE (status)"),
        "the version 3 index was replaced: {migrated}"
    );
    assert_eq!(
        index_valid(&pool, &new_index).await,
        Some(true),
        "the index built beside the old one is valid"
    );
    assert_eq!(
        index_definition(&pool, &old_index).await,
        None,
        "the version 3 index was dropped after the new one was valid"
    );
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());

    // The listing still reads the runs, newest first, through the new index.
    let query = RunQuery::new(&agent, ConversationScope::Prefix("c:".into()), 10);
    let listed = store.list_runs(&query).await.unwrap();
    assert_eq!(
        listed.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![second.id, first.id]
    );

    drop_tables(&pool, &p).await;
}

/// A concurrent build that fails leaves an INVALID index under the name the migration looks for.
/// `CREATE INDEX IF NOT EXISTS` would keep it for ever; the upgrade drops and rebuilds it, and the
/// end state is a valid index with the included status.
#[tokio::test]
async fn an_invalid_listing_index_is_dropped_and_rebuilt() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    let new_index = format!("{p}runs_list_status");
    store.migrate().await.unwrap();

    // Two runs of one agent: a unique index on `agent` cannot be built over them, and a concurrent
    // build that fails leaves the index INVALID rather than removing it.
    let agent = format!(
        "invalid-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    store
        .create_run(NewRun::new(&agent, json!({"n": 1})))
        .await
        .unwrap();
    store
        .create_run(NewRun::new(&agent, json!({"n": 2})))
        .await
        .unwrap();
    support::exec(&pool, &format!("DROP INDEX {new_index}")).await;
    let client = pool.get().await.unwrap();
    let failed = client
        .batch_execute(&format!(
            "CREATE UNIQUE INDEX CONCURRENTLY {new_index} ON {p}runs (agent)"
        ))
        .await;
    assert!(
        failed.is_err(),
        "a unique index over duplicate agents cannot be built"
    );
    assert_eq!(
        index_valid(&pool, &new_index).await,
        Some(false),
        "a failed concurrent build leaves an INVALID index"
    );

    // Back to version 3, then migrate: the invalid index is dropped and rebuilt.
    support::exec(
        &pool,
        &format!("UPDATE {p}meta SET value = '3' WHERE key = 'schema_version'"),
    )
    .await;
    store.migrate().await.unwrap();
    let definition = index_definition(&pool, &new_index)
        .await
        .expect("the rebuilt index");
    assert!(
        definition.contains("INCLUDE (status)"),
        "the rebuilt index is the listing index, not the failed one: {definition}"
    );
    assert_eq!(index_valid(&pool, &new_index).await, Some(true));
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());

    drop_tables(&pool, &p).await;
}

#[tokio::test]
async fn a_version_4_schema_gets_the_blob_tables_and_keeps_its_runs() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    let agent = format!("v4-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let run = store
        .create_run(NewRun::new(&agent, json!({"turn": 1})))
        .await
        .unwrap();

    // Back to version 4: no blob tables, the version 4 listing index still there.
    support::exec(&pool, &format!("DROP TABLE {p}run_blob, {p}blob")).await;
    support::exec(
        &pool,
        &format!("UPDATE {p}meta SET value = '4' WHERE key = 'schema_version'"),
    )
    .await;
    assert!(!has_table(&pool, &format!("{p}blob")).await);
    assert!(!has_table(&pool, &format!("{p}run_blob")).await);

    store.migrate().await.unwrap();
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());
    assert!(has_table(&pool, &format!("{p}blob")).await);
    assert!(has_table(&pool, &format!("{p}run_blob")).await);
    assert!(
        has_table(&pool, &format!("{p}body")).await,
        "the body table"
    );
    assert_eq!(
        index_valid(&pool, &format!("{p}runs_list_status")).await,
        Some(true),
        "a version 4 start does not touch the listing index"
    );
    // The run survived, and the new tables answer.
    assert_eq!(
        store
            .load_run(run.id)
            .await
            .unwrap()
            .expect("the run")
            .state,
        json!({"turn": 1})
    );
    assert_eq!(
        store
            .purge_blobs(now() + jiff::SignedDuration::from_hours(1))
            .await
            .unwrap(),
        0,
        "the blob sweep runs against an empty table"
    );

    drop_tables(&pool, &p).await;
}

/// Schema version 5 has the blob tables and the whole state, but no head and no body: the upgrade
/// to 6 adds the two columns and the table, and the run survives.
#[tokio::test]
async fn a_version_5_schema_gets_the_head_and_the_body_and_keeps_its_runs() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 2);
    let p = format!(
        "adam_mig_{}_",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&p)
        .unwrap();
    store.migrate().await.unwrap();
    let agent = format!("v5-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    let run = store
        .create_run(NewRun::new(&agent, json!({"turn": 1})))
        .await
        .unwrap();

    // Back to version 5: no head columns, no body table.
    support::exec(&pool, &format!("DROP TABLE {p}body")).await;
    support::exec(
        &pool,
        &format!("ALTER TABLE {p}runs DROP COLUMN head, DROP COLUMN head_version"),
    )
    .await;
    support::exec(
        &pool,
        &format!("UPDATE {p}meta SET value = '5' WHERE key = 'schema_version'"),
    )
    .await;
    assert!(!has_table(&pool, &format!("{p}body")).await);

    store.migrate().await.unwrap();
    assert_eq!(version(&pool, &p).await, SCHEMA_VERSION.to_string());
    assert!(has_table(&pool, &format!("{p}body")).await);
    assert!(
        support::scalar::<bool>(
            &pool,
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns
                             WHERE table_name = $1 AND column_name = 'head')",
            &[&format!("{p}runs")],
        )
        .await,
        "the head column"
    );
    // The run survived, and its whole state is still served (no head yet).
    let loaded = store.load_run(run.id).await.unwrap().expect("the run");
    assert_eq!(loaded.state, json!({"turn": 1}));
    assert_eq!(
        store.load_head(run.id).await.unwrap().unwrap().state,
        json!({"turn": 1})
    );

    drop_tables(&pool, &p).await;
}

/// A start on a current schema runs no DDL, so it takes no table lock and cannot deadlock with a
/// process that is working on the same tables (a `push_put` locks push then runs, a purge locks
/// runs then push; the migration's `ALTER TABLE` took `AccessExclusiveLock` on runs even as a
/// no-op).
#[tokio::test]
async fn a_start_on_a_current_schema_takes_no_table_lock() {
    let Some(url) = support::test_url() else {
        return;
    };
    let pool = support::pool(&url, 4);
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
    let mut holder = pool.get().await.unwrap();
    let tx = holder.transaction().await.unwrap();
    for table in ["runs", "push"] {
        tx.batch_execute(&format!("LOCK TABLE {p}{table} IN ROW EXCLUSIVE MODE"))
            .await
            .unwrap();
    }

    // Another process starts: its own pool, the same prefix.
    let other = support::pool(&url, 1);
    let store = PgStore::from_pool(other.clone())
        .with_table_prefix(&p)
        .unwrap();
    let started = tokio::time::timeout(Duration::from_secs(5), store.migrate()).await;

    tx.rollback().await.unwrap();
    drop_tables(&pool, &p).await;
    started
        .expect("a start on a current schema waited for a table lock")
        .unwrap();
}
