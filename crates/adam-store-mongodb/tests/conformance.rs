//! Runs the shared conformance suite against a real MongoDB.
//!
//! ```sh
//! ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 cargo test -p adam-store-mongodb
//! ```
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it fails).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_store_mongodb::MongoStore;

async fn make_store() -> Option<adam_core::DynStore> {
    let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
    let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let store = MongoStore::connect(&uri, &db).await.expect("connect");
    Some(Arc::new(store))
}

adam_store_testkit::store_conformance!(make_store);

/// A run document written by schema version 1 has no `owner` field. A pinned claim treats that
/// as "no owner", takes the run, and keeps it from other workers afterwards.
#[tokio::test]
async fn a_document_without_an_owner_field_is_unowned() {
    use std::time::Duration;

    use adam_core::store::now;
    use adam_core::{ClaimScope, RunId, Store};
    use mongodb::bson::{Bson, doc};

    let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") else {
        adam_core::testing::skipped("a_document_without_an_owner_field_is_unowned: not configured");
        return;
    };
    let db_name = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let store = MongoStore::connect(&uri, &db_name).await.expect("connect");
    store.migrate().await.expect("migrate");

    let agent = adam_store_testkit::agent();
    let id = RunId::new();
    let t = now();
    let date = |at: chrono::DateTime<chrono::Utc>| {
        Bson::DateTime(mongodb::bson::DateTime::from_millis(at.timestamp_millis()))
    };
    // Exactly the fields version 1 wrote: no `owner`.
    let legacy = doc! {
        "_id": Bson::from(mongodb::bson::Uuid::from_bytes(id.0.into_bytes())),
        "agent": &agent,
        "conversation_id": Bson::Null,
        "parent_id": Bson::Null,
        "status": "runnable",
        "state": { "turn": 1_i64 },
        "wake_at": Bson::Null,
        "sched_at": date(t),
        "version": 1_i64,
        "open_key": format!("~{id}"),
        "lease_owner": Bson::Null,
        "lease_until": Bson::Null,
        "lease_token": Bson::Null,
        "created_at": date(t),
        "updated_at": date(t),
    };
    store
        .database()
        .collection::<mongodb::bson::Document>("adam_runs")
        .insert_one(&legacy)
        .await
        .expect("insert a version 1 document");

    let agents = [agent.clone()];
    let ttl = Duration::from_secs(30);
    let at = t + chrono::Duration::seconds(1);
    let first = store
        .claim_due(&agents, "w1", ClaimScope::Pinned, at, ttl, 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 1, "a missing owner field counts as no owner");
    store.release_lease(id, "w1").await.unwrap();
    let other = store
        .claim_due(&agents, "w2", ClaimScope::Pinned, at, ttl, 10)
        .await
        .unwrap();
    assert!(other.is_empty(), "w1 owns the run now");
}
