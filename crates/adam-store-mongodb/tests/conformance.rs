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

adam_store_testkit::blob_conformance!(make_store);

adam_store_testkit::body_conformance!(make_store);

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

/// A body set is `(run, commit, idx)`: two rows of one set go in, the head names the set, and
/// `load_head` serves it at the version it was written at.
///
/// The adapter-local half of the body suite: the conformance cases cannot make an insert fail, and
/// cannot see the stored `head_version` a commit that writes no head leaves alone.
#[tokio::test]
async fn a_head_is_stored_with_its_version_and_a_headless_commit_leaves_it_alone() {
    use adam_core::{BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate, Store};
    use serde_json::json;

    let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") else {
        adam_core::testing::skipped(
            "a_head_is_stored_with_its_version_and_a_headless_commit_leaves_it_alone: not configured",
        );
        return;
    };
    let db_name = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let prefix = format!(
        "s2a_head_{}_",
        &uuid::Uuid::now_v7().simple().to_string()[..8]
    );
    let store = MongoStore::connect(&uri, &db_name)
        .await
        .expect("connect")
        .with_collection_prefix(&prefix)
        .expect("prefix");
    store.migrate().await.expect("migrate");
    let agent = adam_store_testkit::agent();
    let run = store
        .create_run(NewRun::new(&agent, json!({"v": 1, "turn": 0})))
        .await
        .expect("create");
    let generation = uuid::Uuid::new_v4();
    let head = json!({"v": 5, "agent": {"turn": 1}, "body": {"generation": null, "len": 1}});
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"v": 1, "turn": 1}))
                .head(head)
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: vec![BodyRow {
                        idx: 0,
                        message: json!("one"),
                    }],
                    writer: uuid::Uuid::new_v4(),
                }),
        )
        .await
        .expect("commit");

    // The document carries the head and the version it was written at.
    let runs = store
        .database()
        .collection::<mongodb::bson::Document>(&format!("{prefix}runs"));
    let doc = runs
        .find_one(mongodb::bson::doc! { "_id": mongodb::bson::Bson::from(
            mongodb::bson::Uuid::from_bytes(run.id.0.into_bytes())
        ) })
        .await
        .expect("find")
        .expect("the run");
    assert_eq!(
        doc.get_i64("head_version").expect("head_version"),
        2,
        "the head was written by the commit that moved version 1 to 2"
    );
    let served = store
        .load_head(run.id)
        .await
        .expect("load")
        .expect("the run")
        .state;
    assert_eq!(
        served["body"]["generation"],
        json!(generation.to_string()),
        "the store resolves and patches the generation"
    );
    assert_eq!(served["body"]["len"], 1);

    // A commit with no head (the shape of `deliver`) must leave `head` and `head_version` alone.
    store
        .commit_run(
            run.id,
            run.version + 1,
            RunUpdate::new(
                RunStatus::Runnable,
                json!({"v": 1, "turn": 1, "late": true}),
            ),
        )
        .await
        .expect("commit");
    let doc = runs
        .find_one(mongodb::bson::doc! { "_id": mongodb::bson::Bson::from(
            mongodb::bson::Uuid::from_bytes(run.id.0.into_bytes())
        ) })
        .await
        .expect("find")
        .expect("the run");
    assert_eq!(
        doc.get_i64("head_version").expect("head_version"),
        2,
        "a headless commit does not bump head_version"
    );
    assert_eq!(
        store
            .load_head(run.id)
            .await
            .expect("load")
            .expect("the run")
            .state,
        json!({"v": 1, "turn": 1, "late": true}),
        "the stale head is not served"
    );

    // A v4 document written before part 2 has neither field, and `load_head` serves its state.
    let legacy = NewRun::new(&agent, json!({"v": 1, "legacy": true}));
    let legacy_id = legacy.id;
    store.create_run(legacy).await.expect("create");
    let doc = runs
        .find_one(mongodb::bson::doc! { "_id": mongodb::bson::Bson::from(
            mongodb::bson::Uuid::from_bytes(legacy_id.0.into_bytes())
        ) })
        .await
        .expect("find")
        .expect("the run");
    assert!(doc.get("head").is_none(), "a v4 document has no head");
    assert!(
        doc.get("head_version").is_none(),
        "a v4 document has no head_version"
    );
    assert_eq!(
        store
            .load_head(legacy_id)
            .await
            .expect("load")
            .expect("the run")
            .state,
        json!({"v": 1, "legacy": true})
    );

    // Drop what this test made: a unique prefix per run of the suite.
    for name in [
        "runs", "journal", "push", "blob", "run_blob", "body", "meta",
    ] {
        let _ = store
            .database()
            .collection::<mongodb::bson::Document>(&format!("{prefix}{name}"))
            .drop()
            .await;
    }
}

/// A partial body insert aborts the commit **before** the CAS: the version does not move, no head
/// is written, and the rows that landed are the orphans a later GC collects.
#[tokio::test]
async fn a_partial_body_insert_aborts_before_the_cas() {
    use adam_core::{BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate, Store};
    use mongodb::bson::{Document, doc};
    use serde_json::json;

    let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") else {
        adam_core::testing::skipped("a_partial_body_insert_aborts_before_the_cas: not configured");
        return;
    };
    let db_name = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let prefix = format!(
        "s2a_partial_{}_",
        &uuid::Uuid::now_v7().simple().to_string()[..8]
    );
    let store = MongoStore::connect(&uri, &db_name)
        .await
        .expect("connect")
        .with_collection_prefix(&prefix)
        .expect("prefix");
    store.migrate().await.expect("migrate");
    // Make one row impossible to insert, so `insert_many(ordered: false)` lands the other and
    // reports a write error. A validator is the only way to fail one document of a set the store
    // builds itself.
    store
        .database()
        .run_command(doc! {
            "collMod": format!("{prefix}body"),
            "validator": { "message": { "$ne": "poison" } },
            "validationLevel": "strict",
        })
        .await
        .expect("a validator on adam_body");
    let agent = adam_store_testkit::agent();
    let run = store
        .create_run(NewRun::new(&agent, json!({"v": 1, "turn": 0})))
        .await
        .expect("create");
    let generation = uuid::Uuid::new_v4();
    let err = store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"v": 1, "turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: vec![
                        BodyRow {
                            idx: 0,
                            message: json!("fine"),
                        },
                        BodyRow {
                            idx: 1,
                            message: json!("poison"),
                        },
                    ],
                    writer: uuid::Uuid::new_v4(),
                }),
        )
        .await
        .expect_err("a partial set must not be committed");
    assert!(
        matches!(
            adam_error::Classify::class(&err),
            adam_error::ErrorClass::Internal
        ),
        "the store reports the backend's refusal: {err:?}"
    );
    let loaded = store
        .load_run(run.id)
        .await
        .expect("load")
        .expect("the run");
    assert_eq!(loaded.version, 1, "the CAS did not run");
    assert_eq!(loaded.state, json!({"v": 1, "turn": 0}), "no state moved");
    assert_eq!(
        store
            .load_head(run.id)
            .await
            .expect("load")
            .expect("the run")
            .state,
        json!({"v": 1, "turn": 0}),
        "no head names a set that is missing a row"
    );
    let landed = store.body_list(run.id, generation).await.expect("list");
    assert_eq!(
        landed,
        vec![BodyRow {
            idx: 0,
            message: json!("fine")
        }],
        "the row that landed is an orphan, and it is readable"
    );

    // Drop the validator and everything this test made.
    let _ = store
        .database()
        .run_command(doc! {
            "collMod": format!("{prefix}body"),
            "validator": {},
            "validationLevel": "off",
        })
        .await;
    for name in [
        "runs", "journal", "push", "blob", "run_blob", "body", "meta",
    ] {
        let _ = store
            .database()
            .collection::<Document>(&format!("{prefix}{name}"))
            .drop()
            .await;
    }
}

/// The body GC keeps the set the commit names and collects a superseded one once the explicit
/// cutoff has passed, so the one-hour grace is observable.
#[tokio::test]
async fn the_body_gc_respects_an_explicit_cutoff() {
    use adam_core::store::now;
    use adam_core::{BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate, Store};
    use mongodb::bson::{Document, doc};
    use serde_json::json;

    let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") else {
        adam_core::testing::skipped("the_body_gc_respects_an_explicit_cutoff: not configured");
        return;
    };
    let db_name = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let prefix = format!(
        "s2a_gc_{}_",
        &uuid::Uuid::now_v7().simple().to_string()[..8]
    );
    let store = MongoStore::connect(&uri, &db_name)
        .await
        .expect("connect")
        .with_collection_prefix(&prefix)
        .expect("prefix");
    store.migrate().await.expect("migrate");
    let agent = adam_store_testkit::agent();
    let run = store
        .create_run(NewRun::new(&agent, json!({})))
        .await
        .expect("create");
    let old = uuid::Uuid::new_v4();
    let current = uuid::Uuid::new_v4();
    let set = |generation, text: &str| BodyWrite {
        generation: Some(generation),
        rows: vec![BodyRow {
            idx: 0,
            message: json!(text),
        }],
        writer: uuid::Uuid::new_v4(),
    };
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 1}}))
                .body(set(old, "old")),
        )
        .await
        .expect("commit");
    store
        .commit_run(
            run.id,
            run.version + 1,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 1}}))
                .body(set(current, "current")),
        )
        .await
        .expect("commit");
    // Age the superseded set past a cutoff the test picks.
    let body = store
        .database()
        .collection::<Document>(&format!("{prefix}body"));
    body.update_many(
        doc! { "generation": mongodb::bson::Bson::from(
            mongodb::bson::Uuid::from_bytes(old.into_bytes())
        ) },
        doc! { "$set": { "written_at": mongodb::bson::DateTime::from_millis(
            (now() - chrono::Duration::hours(2)).timestamp_millis()
        ) } },
    )
    .await
    .expect("age the old set");
    let cutoff = now() - chrono::Duration::hours(1);
    assert_eq!(
        store.gc_body(run.id, current, cutoff).await.expect("gc"),
        1,
        "the superseded set is collected"
    );
    assert!(
        store.body_list(run.id, old).await.expect("list").is_empty(),
        "the old set is gone"
    );
    assert_eq!(
        store.body_list(run.id, current).await.expect("list").len(),
        1,
        "the current set is kept"
    );
    // However old it is, the set a commit names is never collected.
    assert_eq!(
        store
            .gc_body(run.id, current, now() + chrono::Duration::hours(24))
            .await
            .expect("gc"),
        0
    );
    assert_eq!(
        store.body_list(run.id, current).await.expect("list").len(),
        1
    );

    for name in [
        "runs", "journal", "push", "blob", "run_blob", "body", "meta",
    ] {
        let _ = store
            .database()
            .collection::<Document>(&format!("{prefix}{name}"))
            .drop()
            .await;
    }
}

/// A rejected insert command is not a completeness answer: the commit aborts before the CAS and
/// no head names the rows. (The write-concern answer itself is pinned by
/// `body_insert_counts_aborts_on_a_write_concern_error`, which needs no server.)
#[tokio::test]
async fn a_rejected_insert_command_aborts_before_the_cas() {
    use adam_core::{BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate, Store};
    use mongodb::Client;
    use mongodb::options::{Acknowledgment, DatabaseOptions, WriteConcern};
    use serde_json::json;

    let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") else {
        adam_core::testing::skipped(
            "a_rejected_insert_command_aborts_before_the_cas: not configured",
        );
        return;
    };
    let db_name = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let prefix = format!(
        "s2a_wc_{}_",
        &uuid::Uuid::now_v7().simple().to_string()[..8]
    );
    let store = MongoStore::connect(&uri, &db_name)
        .await
        .expect("connect")
        .with_collection_prefix(&prefix)
        .expect("prefix");
    store.migrate().await.expect("migrate");
    let agent = adam_store_testkit::agent();
    let run = store
        .create_run(NewRun::new(&agent, json!({})))
        .await
        .expect("create");

    // A handle on the same collections whose writes need a second node: a standalone cannot
    // acknowledge it, so `insert_many` reports a write-concern error.
    let client = Client::with_uri_str(&uri).await.expect("client");
    let concern = WriteConcern::builder()
        .w(Acknowledgment::Nodes(2))
        .w_timeout(std::time::Duration::from_millis(250))
        .build();
    let db = client.database_with_options(
        &db_name,
        DatabaseOptions::builder().write_concern(concern).build(),
    );
    let strict = MongoStore::new(db)
        .with_collection_prefix(&prefix)
        .expect("prefix");
    let generation = uuid::Uuid::new_v4();
    // One row is already there, so the insert reports a duplicate **and** the write-concern
    // failure: a check that only counted duplicates would call the set complete.
    let body = store
        .database()
        .collection::<mongodb::bson::Document>(&format!("{prefix}body"));
    body.insert_one(mongodb::bson::doc! {
        "_id": format!("{}:{}:0", run.id, generation),
        "run_id": mongodb::bson::Bson::from(
            mongodb::bson::Uuid::from_bytes(run.id.0.into_bytes())
        ),
        "generation": mongodb::bson::Bson::from(
            mongodb::bson::Uuid::from_bytes(generation.into_bytes())
        ),
        "writer": mongodb::bson::Bson::from(
            mongodb::bson::Uuid::from_bytes(uuid::Uuid::new_v4().into_bytes())
        ),
        "idx": 0_i32,
        "message": "already there",
        "written_at": mongodb::bson::DateTime::now(),
    })
    .await
    .expect("the duplicate row");
    let err = strict
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: vec![
                        BodyRow {
                            idx: 0,
                            message: json!("one"),
                        },
                        BodyRow {
                            idx: 1,
                            message: json!("two"),
                        },
                    ],
                    writer: uuid::Uuid::new_v4(),
                }),
        )
        .await
        .expect_err("a rejected insert must abort the commit");
    assert!(
        !matches!(err, adam_core::StoreError::Conflict { .. }),
        "the store reports the backend's failure: {err:?}"
    );
    let loaded = store
        .load_run(run.id)
        .await
        .expect("load")
        .expect("the run");
    assert_eq!(loaded.version, 1, "the CAS did not run");
    assert_eq!(
        store
            .load_head(run.id)
            .await
            .expect("load")
            .expect("the run")
            .state,
        json!({}),
        "no head names a generation the store could not confirm"
    );

    for name in [
        "runs", "journal", "push", "blob", "run_blob", "body", "meta",
    ] {
        let _ = store
            .database()
            .collection::<mongodb::bson::Document>(&format!("{prefix}{name}"))
            .drop()
            .await;
    }
}

/// A normal commit sends only the rows after the head's `len`: the insert command's document count
/// is the delta, not the transcript. The data looks the same either way (duplicates are ignored),
/// so the driver's command is what proves the write win.
#[tokio::test]
async fn an_append_sends_only_the_delta() {
    use std::sync::{Arc, Mutex};

    use adam_core::{BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate, Store};
    use mongodb::Client;
    use mongodb::event::EventHandler;
    use mongodb::event::command::CommandEvent;
    use mongodb::options::ClientOptions;
    use serde_json::json;

    let Some(uri) = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI") else {
        adam_core::testing::skipped("an_append_sends_only_the_delta: not configured");
        return;
    };
    let db_name = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let prefix = format!(
        "s2a_delta_{}_",
        &uuid::Uuid::now_v7().simple().to_string()[..8]
    );
    let seen = Arc::new(Mutex::new(Vec::<(String, usize)>::new()));
    let sink = Arc::clone(&seen);
    let mut options = ClientOptions::parse(&uri).await.expect("options");
    options.command_event_handler = Some(EventHandler::callback(move |event: CommandEvent| {
        if let CommandEvent::Started(event) = event {
            let documents = event
                .command
                .get_array("documents")
                .map(Vec::len)
                .unwrap_or(0);
            sink.lock()
                .expect("lock")
                .push((event.command_name, documents));
        }
    }));
    let client = Client::with_options(options).expect("client");
    let store = MongoStore::new(client.database(&db_name))
        .with_collection_prefix(&prefix)
        .expect("prefix");
    store.migrate().await.expect("migrate");
    let agent = adam_store_testkit::agent();
    let run = store
        .create_run(NewRun::new(&agent, json!({})))
        .await
        .expect("create");
    let generation = uuid::Uuid::new_v4();
    let rows = |generation, writer, messages: &[&str]| BodyWrite {
        generation,
        writer,
        rows: messages
            .iter()
            .enumerate()
            .map(|(idx, message)| BodyRow {
                idx: u32::try_from(idx).expect("a short transcript"),
                message: json!(message),
            })
            .collect(),
    };
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(rows(Some(generation), uuid::Uuid::new_v4(), &["a", "b"])),
        )
        .await
        .expect("the first split");
    seen.lock().expect("lock").clear();
    store
        .commit_run(
            run.id,
            run.version + 1,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 4}}))
                .body(rows(None, uuid::Uuid::new_v4(), &["a", "b", "c", "d"])),
        )
        .await
        .expect("the append");
    let inserts: Vec<usize> = seen
        .lock()
        .expect("lock")
        .iter()
        .filter(|(name, _)| name == "insert")
        .map(|(_, documents)| *documents)
        .collect();
    assert_eq!(
        inserts,
        vec![2],
        "the append sent two rows, not the four of the transcript"
    );
    assert_eq!(
        store
            .body_list(run.id, generation)
            .await
            .expect("body")
            .len(),
        4
    );

    for name in [
        "runs", "journal", "push", "blob", "run_blob", "body", "meta",
    ] {
        let _ = store
            .database()
            .collection::<mongodb::bson::Document>(&format!("{prefix}{name}"))
            .drop()
            .await;
    }
}
