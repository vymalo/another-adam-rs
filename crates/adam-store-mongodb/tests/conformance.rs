//! Runs the shared conformance suite against a real MongoDB.
//!
//! The two bespoke tests below (`the_listing_index_covers_the_status_filter` and
//! `a_document_without_an_owner_field_is_unowned`) build their own store with the default `adam_`
//! collection prefix and name `adam_runs` directly: they do not follow
//! `ADAM_TEST_MONGO_COLLECTION_PREFIX`.
//!
//! ```sh
//! ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 cargo test -p adam-store-mongodb
//! ```
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it fails).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;

use adam_core::Store;
use adam_store_mongodb::MongoStore;

async fn make_store() -> Option<adam_core::DynStore> {
    let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
    let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let prefix =
        std::env::var("ADAM_TEST_MONGO_COLLECTION_PREFIX").unwrap_or_else(|_| "adam_".into());
    let store = MongoStore::connect(&uri, &db)
        .await
        .expect("connect")
        .with_collection_prefix(&prefix)
        .expect("prefix");
    Some(Arc::new(store))
}

adam_store_testkit::store_conformance!(make_store);

/// The listing `ListTasks` sends — a conversation prefix, a status filter, newest first — must be
/// answered from an index that holds `status`: with `purging` in the filter and `status` in no
/// index, MongoDB fetched every candidate document (state included) to test both.
#[tokio::test]
async fn the_listing_index_covers_the_status_filter() {
    use futures::TryStreamExt;
    use mongodb::bson::{Document, doc};

    let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") else {
        adam_core::testing::skipped("the_listing_index_covers_the_status_filter: not configured");
        return;
    };
    let db_name = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let store = MongoStore::connect(&uri, &db_name).await.expect("connect");
    store.migrate().await.expect("migrate");
    let runs = store.database().collection::<Document>("adam_runs");

    let names = runs.list_index_names().await.expect("index names");
    assert!(names.iter().any(|n| n == "adam_list"), "{names:?}");
    assert!(
        names.iter().any(|n| n == "adam_list_status"),
        "the status-filtered listing index exists: {names:?}"
    );

    let models: Vec<mongodb::IndexModel> = runs
        .list_indexes()
        .await
        .expect("indexes")
        .try_collect()
        .await
        .expect("indexes");
    let found = models
        .iter()
        .find(|m| m.options.as_ref().and_then(|o| o.name.as_deref()) == Some("adam_list_status"))
        .expect("adam_list_status");
    assert_eq!(
        found.keys,
        doc! { "agent": 1, "status": 1, "conversation_id": 1, "updated_at": -1, "_id": -1 },
        "the status is an equality bound before the prefix range and the sort keys"
    );
}

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
    let date = |at: adam_core::time::Timestamp| {
        Bson::DateTime(mongodb::bson::DateTime::from_millis(at.as_millisecond()))
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
    let at = t + jiff::SignedDuration::from_secs(1);
    let first = store
        .claim_due(&agents, "w1", ClaimScope::Pinned, &[], at, ttl, 10)
        .await
        .unwrap();
    assert_eq!(first.len(), 1, "a missing owner field counts as no owner");
    store.release_lease(id, "w1").await.unwrap();
    let other = store
        .claim_due(&agents, "w2", ClaimScope::Pinned, &[], at, ttl, 10)
        .await
        .unwrap();
    assert!(other.is_empty(), "w1 owns the run now");
}

adam_store_testkit::transcript_measurement!(make_store);
