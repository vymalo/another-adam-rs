//! A database built by the previous (`sqlx`) release opens with no migration.
//!
//! `tests/fixtures/schema-v5.sql` is the schema exactly as that release emitted it. This applies
//! it with `batch_execute` into a database of its own, then opens the store **without calling
//! `migrate`** and takes a create/load round trip: the tables, the statements and the version row
//! are the ones the new driver reads.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use adam_core::{NewRun, Store};
use adam_postgres::PgConfig;
use adam_store_postgres::{PgStore, SCHEMA_VERSION};
use serde_json::json;

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
async fn a_sqlx_migrated_database_opens_and_round_trips_without_a_migration() {
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
    assert_eq!(version, SCHEMA_VERSION.to_string());
    drop(client);

    // No `migrate`: the fixture is what the old release left behind.
    let store = PgStore::from_pool(PgConfig::parse(&url).expect("URL").pool(2).expect("pool"));
    let run = store
        .create_run(NewRun::new("fixture-agent", json!({"turn": 1})).conversation("c:1"))
        .await
        .expect("create a run in the old schema");
    assert_eq!(run.version, 1);
    let loaded = store
        .load_run(run.id)
        .await
        .expect("load the run")
        .expect("the run exists");
    assert_eq!(loaded.state, json!({"turn": 1}));
    assert_eq!(loaded.conversation_id.as_deref(), Some("c:1"));

    // And the migration the new build would run is a no-op on it.
    store.migrate().await.expect("migrate an up-to-date schema");

    store.pool().close();
    let cleaner = admin(&server).await;
    cleaner
        .batch_execute(&format!("DROP DATABASE IF EXISTS {name}"))
        .await
        .expect("drop the fixture database");
}
