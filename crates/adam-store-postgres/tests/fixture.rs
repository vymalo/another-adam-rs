//! A database built by the previous (`sqlx`) release opens with no migration, and the upgrade to
//! the current schema adds the head and the body (version 6) and the functions step (version 7).
//!
//! `tests/fixtures/schema-v5.sql` is the schema exactly as that release emitted it. This applies it
//! with `batch_execute` into a database of its own, opens the store **without calling `migrate`**
//! and takes a create/load round trip, then migrates to the current version and commits a head and a body.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use adam_core::{BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate, Store};
use adam_postgres::PgConfig;
use adam_store_postgres::{PgStore, SCHEMA_VERSION};
use serde_json::json;
use uuid::Uuid;

async fn admin(server: &str) -> tokio_postgres::Client {
    PgConfig::parse(server)
        .expect("DATABASE_URL")
        .connect()
        .await
        .expect("connect to the test Postgres")
}

fn with_database(server: &str, database: &str) -> String {
    let mut url = url::Url::parse(server).expect("a URL");
    url.set_path(&format!("/{database}"));
    url.to_string()
}

#[tokio::test]
async fn a_sqlx_migrated_database_opens_and_upgrades_to_the_head_and_body() {
    let Some(server) = support::test_url() else {
        return;
    };
    let name = format!(
        "adam_fixture_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let creator = admin(&server).await;
    creator
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .await
        .expect("create the fixture database (does the role have CREATEDB?)");
    drop(creator);

    let url = with_database(&server, &name);
    let client = PgConfig::parse(&url)
        .expect("DATABASE_URL")
        .connect()
        .await
        .expect("connect to the fixture database");
    client
        .batch_execute(include_str!("fixtures/schema-v5.sql"))
        .await
        .expect("apply the version 5 schema");
    let version: String = client
        .query_one(
            "SELECT value FROM adam_meta WHERE key = 'schema_version'",
            &[],
        )
        .await
        .expect("the version row")
        .get(0);
    assert_eq!(version, "5");
    drop(client);

    // No `migrate`: the fixture is what the old release left behind. A run can be created (the
    // insert names no v6 column), but a read is the contract's read and needs the head columns.
    let store = PgStore::from_pool(PgConfig::parse(&url).expect("URL").pool(2).expect("pool"));
    let run = store
        .create_run(NewRun::new("fixture-agent", json!({"turn": 1})).conversation("c:1"))
        .await
        .expect("create a run in the old schema");
    assert_eq!(run.version, 1);
    assert!(
        store.load_run(run.id).await.is_err(),
        "a version 5 database has no head columns: migrate before reading"
    );

    // The upgrade is the migration the new build runs: metadata only, the run survives.
    store.migrate().await.expect("migrate a version 5 schema");
    let version: String = {
        let client = store.pool().get().await.expect("a connection");
        client
            .query_one(
                "SELECT value FROM adam_meta WHERE key = 'schema_version'",
                &[],
            )
            .await
            .expect("the version row")
            .get(0)
    };
    assert_eq!(version, SCHEMA_VERSION.to_string());
    assert_eq!(version, "7");

    // The old run reads as it was written: no head, so the whole state is served.
    let loaded = store
        .load_run(run.id)
        .await
        .expect("load the run")
        .expect("the run exists");
    assert_eq!(loaded.state, json!({"turn": 1}));
    assert_eq!(loaded.conversation_id.as_deref(), Some("c:1"));

    // And the head and the body work on the upgraded database.
    let generation = Uuid::new_v4();
    let writer = Uuid::new_v4();
    let rows: Vec<BodyRow> = ["one", "two"]
        .iter()
        .enumerate()
        .map(|(idx, message)| BodyRow {
            idx: u32::try_from(idx).expect("a short transcript"),
            message: json!(message),
        })
        .collect();
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows,
                    writer,
                }),
        )
        .await
        .expect("commit a head and a body");
    let head = store
        .load_head(run.id)
        .await
        .expect("load the head")
        .expect("the run exists");
    assert_eq!(head.state["body"]["generation"], generation.to_string());
    assert_eq!(store.body_list(run.id, generation).await.unwrap().len(), 2);

    store.pool().close();
    let cleaner = admin(&server).await;
    cleaner
        .batch_execute(&format!("DROP DATABASE IF EXISTS {name}"))
        .await
        .expect("drop the fixture database");
}
