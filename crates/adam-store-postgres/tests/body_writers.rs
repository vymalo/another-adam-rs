//! The foreign-writer check inside a commit's transaction: another attempt's row at a slot is
//! adopted when it holds the same message, collected when it is older than the 5-minute orphan
//! grace, and a `Conflict` when it is fresh and different.
//!
//! The conformance suite cannot force the interleaving ("a real race is the adapter's test"), so
//! these cases write the foreign row directly, as a dead attempt or a second writer would.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod support;

use adam_core::store::now;
use adam_core::{BodyRow, BodyWrite, NewRun, RunStatus, RunUpdate, Store};
use adam_store_postgres::PgStore;
use deadpool_postgres::Pool;
use serde_json::json;
use uuid::Uuid;

fn rows(messages: &[&str]) -> Vec<BodyRow> {
    messages
        .iter()
        .enumerate()
        .map(|(idx, message)| BodyRow {
            idx: u32::try_from(idx).unwrap(),
            message: json!(message),
        })
        .collect()
}

async fn setup(url: &str) -> (Pool, PgStore, String, adam_core::RunRecord) {
    let pool = support::pool(url, 4);
    let prefix = format!(
        "adam_writer_{}_",
        &Uuid::new_v4().simple().to_string()[..12]
    );
    let store = PgStore::from_pool(pool.clone())
        .with_table_prefix(&prefix)
        .unwrap();
    store.migrate().await.unwrap();
    let agent = format!("writer-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let run = store
        .create_run(NewRun::new(&agent, json!({})))
        .await
        .unwrap();
    (pool, store, prefix, run)
}

#[allow(clippy::too_many_arguments)] // a fixture builder, one argument per column
async fn insert_foreign(
    pool: &Pool,
    prefix: &str,
    run: adam_core::RunId,
    generation: Uuid,
    idx: i32,
    message: &str,
    writer: Uuid,
    age: jiff::SignedDuration,
) {
    let client = pool.get().await.unwrap();
    let at = (now() - age).0;
    client
        .execute(
            &format!(
                "INSERT INTO {prefix}body (run_id, generation, idx, message, writer, written_at)
                 VALUES ($1, $2, $3, $4, $5, $6)"
            ),
            &[&run.0, &generation, &idx, &json!(message), &writer, &at],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_identical_foreign_row_is_adopted() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (pool, store, prefix, run) = setup(&url).await;
    let generation = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    insert_foreign(
        &pool,
        &prefix,
        run.id,
        generation,
        0,
        "a",
        foreign,
        jiff::SignedDuration::from_secs(1),
    )
    .await;
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: rows(&["a", "b"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .expect("an identical foreign row is adopted");
    assert_eq!(store.body_list(run.id, generation).await.unwrap().len(), 2);
    // The adopted row kept its writer: the commit did not rewrite it.
    let client = pool.get().await.unwrap();
    let writer: Uuid = client
        .query_one(
            &format!("SELECT writer FROM {prefix}body WHERE run_id = $1 AND idx = 0"),
            &[&run.id.0],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        writer, foreign,
        "the identical row was adopted, not replaced"
    );
}

#[tokio::test]
async fn an_orphaned_foreign_row_is_collected_and_replaced() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (pool, store, prefix, run) = setup(&url).await;
    let generation = Uuid::new_v4();
    insert_foreign(
        &pool,
        &prefix,
        run.id,
        generation,
        0,
        "dead attempt",
        Uuid::new_v4(),
        jiff::SignedDuration::from_mins(6),
    )
    .await;
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: rows(&["a", "b"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .expect("an orphan is collected and the row written again");
    let listed = store.body_list(run.id, generation).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|row| row.message.clone())
            .collect::<Vec<_>>(),
        vec![json!("a"), json!("b")]
    );
}

#[tokio::test]
async fn a_live_foreign_row_with_a_different_message_is_a_conflict() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (pool, store, prefix, run) = setup(&url).await;
    let generation = Uuid::new_v4();
    insert_foreign(
        &pool,
        &prefix,
        run.id,
        generation,
        0,
        "a live attempt's message",
        Uuid::new_v4(),
        jiff::SignedDuration::from_secs(1),
    )
    .await;
    let err = store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: rows(&["a", "b"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .expect_err("a live foreign row is a conflict");
    assert!(
        matches!(err, adam_core::StoreError::Conflict { .. }),
        "{err:?}"
    );
    // The loser wrote nothing: the run is still at version 1 with no head, and only the foreign
    // row is in the generation.
    let loaded = store.load_run(run.id).await.unwrap().unwrap();
    assert_eq!(loaded.version, 1);
    assert!(store.load_head(run.id).await.unwrap().unwrap().state == json!({}));
    let listed = store.body_list(run.id, generation).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].message, json!("a live attempt's message"));
}

/// A commit that loses the version check writes no head and no row (the conformance case), and the
/// run stays readable. Here the row lock serializes the two commits.
#[tokio::test]
async fn a_stale_commit_leaves_the_winners_head_and_generation_alone() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (_, store, _, run) = setup(&url).await;
    let generation = Uuid::new_v4();
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 1}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: rows(&["winner"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .unwrap();
    let err = store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: None,
                    rows: rows(&["winner", "loser"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .expect_err("a stale version loses");
    assert!(
        matches!(err, adam_core::StoreError::Conflict { .. }),
        "{err:?}"
    );
    let head = store.load_head(run.id).await.unwrap().unwrap().state;
    assert_eq!(head["body"]["len"], 1);
    assert_eq!(store.body_list(run.id, generation).await.unwrap().len(), 1);
}

/// The orphan collector works at the head's prefix end: a dead attempt's row at `idx == the head's
/// `len` is collected by the next append (which starts there), and the prefix the live head names is
/// not touched. `gc_body` never collects the generation the live head names; a superseded one is
/// collected whole, leftovers included.
#[tokio::test]
async fn an_orphan_at_the_prefix_end_is_collected_and_the_live_prefix_survives() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (pool, store, prefix, run) = setup(&url).await;
    let generation = Uuid::new_v4();
    let first_writer = Uuid::new_v4();
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: rows(&["a", "b"]),
                    writer: first_writer,
                }),
        )
        .await
        .unwrap();
    // A dead attempt's row at the head's len, older than the 5-minute orphan grace.
    insert_foreign(
        &pool,
        &prefix,
        run.id,
        generation,
        2,
        "dead attempt",
        Uuid::new_v4(),
        jiff::SignedDuration::from_mins(6),
    )
    .await;
    // The next append starts at the head's len: it collects the orphan and writes its own row.
    store
        .commit_run(
            run.id,
            run.version + 1,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 3}}))
                .body(BodyWrite {
                    generation: None,
                    rows: rows(&["a", "b", "c"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .body_list(run.id, generation)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.message)
            .collect::<Vec<_>>(),
        vec![json!("a"), json!("b"), json!("c")]
    );
    // The prefix rows kept the writer the live head's commit gave them: the append did not touch
    // them.
    let client = pool.get().await.unwrap();
    let writers: Vec<Uuid> = client
        .query(
            &format!("SELECT writer FROM {prefix}body WHERE run_id = $1 AND idx < 2 ORDER BY idx"),
            &[&run.id.0],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(writers, vec![first_writer, first_writer]);

    // The generation the live head names is never collected, however old its rows are.
    let cutoff = now() + jiff::SignedDuration::from_hours(1);
    assert_eq!(store.gc_body(run.id, generation, cutoff).await.unwrap(), 0);
    assert_eq!(store.body_list(run.id, generation).await.unwrap().len(), 3);

    // A compaction supersedes it; the old generation is collected whole.
    let second = Uuid::new_v4();
    store
        .commit_run(
            run.id,
            run.version + 2,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 3}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 1}}))
                .body(BodyWrite {
                    generation: Some(second),
                    rows: rows(&["x"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .unwrap();
    assert_eq!(
        store.gc_body(run.id, second, cutoff).await.unwrap(),
        3,
        "the superseded generation, all three rows"
    );
    assert!(
        store
            .body_list(run.id, generation)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.body_list(run.id, second).await.unwrap().len(), 1);
}

/// A head a later headless commit did not rebuild is **stale**: `load_head` serves the whole state
/// (a mixed fleet reads it as before) and the body rows stay, unread by the view.
#[tokio::test]
async fn a_head_a_later_commit_did_not_rebuild_is_not_served() {
    let Some(url) = support::test_url() else {
        return;
    };
    let (_, store, _, run) = setup(&url).await;
    let generation = Uuid::new_v4();
    store
        .commit_run(
            run.id,
            run.version,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": 2}}))
                .body(BodyWrite {
                    generation: Some(generation),
                    rows: rows(&["a", "b"]),
                    writer: Uuid::new_v4(),
                }),
        )
        .await
        .unwrap();
    // A delivery: no head, no body, so the head stays at version 2 while the run moves to 3.
    store
        .commit_run(
            run.id,
            run.version + 1,
            RunUpdate::new(RunStatus::Runnable, json!({"turn": 2})),
        )
        .await
        .unwrap();
    let head = store.load_head(run.id).await.unwrap().unwrap();
    assert_eq!(head.version, 3);
    assert_eq!(
        head.state,
        json!({"turn": 2}),
        "a stale head is not served; the whole state is"
    );
    assert_eq!(
        store.body_list(run.id, generation).await.unwrap().len(),
        2,
        "the rows stay for the next split"
    );
}
