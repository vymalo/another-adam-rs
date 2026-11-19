//! Conformance suite for [`adam_core::Store`] implementations.
//!
//! Every adapter runs the same cases, so "passes the testkit" means "behaves
//! like every other adam-rs store". Use the [`store_conformance!`] macro in an
//! integration test:
//!
//! ```ignore
//! async fn make_store() -> Option<adam_core::DynStore> {
//!     // skip if unset (panics instead when ADAM_TEST_REQUIRE_DB=1)
//!     let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
//!     let store = PgStore::connect(&url).await.unwrap();
//!     Some(std::sync::Arc::new(store))
//! }
//! adam_store_testkit::store_conformance!(make_store);
//! ```
//!
//! Cases isolate themselves with a unique agent name, so they run in parallel
//! against one shared database without cleaning it between tests.

// A conformance suite is test code: a failed unwrap is a failed assertion.
#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod counting;
pub mod fault;

pub use adam_core::testing::skipped;

use std::collections::HashSet;
use std::time::Duration;

use adam_core::store::{now, truncate_ms};
use adam_core::{
    BlobId, BodyRow, BodyWrite, ClaimScope, ConversationScope, DynStore, JournalEntry,
    NewPushConfig, NewRun, PushProgress, PushState, RunId, RunQuery, RunStatus, RunUpdate,
    StoreError,
};
use chrono::{DateTime, Utc};
use futures::future::join_all;
use serde_json::{Value, json};
use uuid::Uuid;

/// Generate one `#[tokio::test]` per conformance case. `$make` is a path to an
/// `async fn() -> Option<DynStore>`; returning `None` skips the suite (for
/// example when the database URL env var is not set), unless
/// `ADAM_TEST_REQUIRE_DB=1` is set: then every case fails instead (see
/// `adam_core::testing`).
#[macro_export]
macro_rules! store_conformance {
    ($make:path) => {
        $crate::store_conformance!(@cases $make;
            create_and_load, state_roundtrip, nul_characters_are_stored_as_the_replacement_character,
            a_write_does_not_echo_the_state,
            create_duplicate_id, load_missing,
            run_header_matches_the_record,
            commit_advances_version, commit_stale_version_conflicts, commit_missing_run,
            concurrent_commits_single_winner, journal_roundtrip_and_order,
            journal_first_writer_wins, journal_concurrent_writers_agree,
            journal_detects_nondeterminism, journal_requires_run,
            claim_respects_due_rules, claim_filters_agents_and_limit, claim_skips_busy_runs,
            claim_is_exclusive_under_concurrency, lease_expiry_allows_takeover,
            renew_and_release_lease, lease_until_reports_the_lease,
            pinned_claim_never_gives_a_run_to_another_worker,
            pinned_claim_sets_the_owner_on_first_claim,
            any_claim_ignores_and_never_sets_the_owner,
            pinned_claims_are_exclusive_and_stable_under_concurrency,
            conversation_single_open_run,
            conversation_race_single_winner, purge_finished_runs,
            push_put_creates_and_replaces, push_put_requires_the_run, push_list_is_per_run_and_ordered,
            push_config_and_cursor_roundtrip, push_delete_is_idempotent,
            push_claim_respects_due_rules, push_claim_filters_agents_and_limit,
            push_claim_is_exclusive_under_concurrency, push_commit_advances_and_releases,
            push_commit_detects_stale_versions_and_missing_configs,
            push_configs_go_with_their_run,
            list_runs_is_scoped_ordered_and_pages_by_keyset, list_runs_filters_by_status_and_time,
            list_runs_prefix_status_filter_is_ordered_and_paged,
            list_runs_scopes_match_exactly, count_runs_ignores_the_page,
            list_runs_prefixes_with_slashes_match_literally,
            two_creates_with_different_ids_in_one_conversation_leave_one_open_run,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                let Some(store) = $make().await else {
                    // Panics when ADAM_TEST_REQUIRE_DB=1 (CI), so an
                    // unconfigured store never passes silently.
                    $crate::skipped(&format!(
                        "{}: store not configured",
                        stringify!($case)
                    ));
                    return;
                };
                store.migrate().await.expect("migrate");
                $crate::cases::$case(store).await;
            }
        )*
    };
}

/// Generate the `measure_the_transcript_write` test: run
/// [`counting::measure_transcript_write`] through `$make`, print its line, and assert that the
/// workload ran.
///
/// `$make` is a path to an `async fn() -> Option<DynStore>`, like [`store_conformance!`]: `None`
/// skips the test (and fails instead under `ADAM_TEST_REQUIRE_DB=1`, see `adam_core::testing`).
///
/// This is a measurement, not a conformance case: it asserts only that the workload ran. S3
/// rewrites the workload to commit a head and a body, then adds the `< 50 × 20 KB` and
/// `max < 20 KB` assertions here.
#[macro_export]
macro_rules! transcript_measurement {
    ($make:path) => {
        #[tokio::test]
        async fn measure_the_transcript_write() {
            let Some(store) = $make().await else {
                $crate::skipped("measure_the_transcript_write: store not configured");
                return;
            };
            store.migrate().await.expect("migrate");
            let measured = $crate::counting::measure_transcript_write(store).await;
            println!("{measured}");
            assert_eq!(
                measured.commits,
                $crate::counting::Measurement::COMMITS,
                "the workload ran to the end"
            );
            assert!(measured.total_written > 0, "the commits wrote state");
        }
    };
}

/// Generate one `#[tokio::test]` per blob conformance case. `$make` is a path to an
/// `async fn() -> Option<DynStore>`, like [`store_conformance!`]: `None` skips the suite (and
/// fails instead under `ADAM_TEST_REQUIRE_DB=1`, see `adam_core::testing`).
///
/// Every case asserts that the store keeps blobs ([`Store::keeps_blobs`](adam_core::Store::keeps_blobs)):
/// a store that does not is not given this suite, rather than passing it by answering the
/// defaulted methods. Blobs are global, so the cases take turns inside one test binary (see
/// `BLOB_SUITE`).
#[macro_export]
macro_rules! blob_conformance {
    ($make:path) => {
        $crate::blob_conformance!(@cases $make;
            blob_round_trip, blob_four_mib_round_trip, blob_put_is_idempotent, blob_unknown_id_is_none,
            blob_empty_bytes, blob_a_run_reference_keeps_the_blob_alive,
            blob_a_shared_digest_survives_until_the_last_run_is_purged,
            blob_purge_respects_before, blob_purge_finished_removes_the_refs_and_the_blob,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                let Some(store) = $make().await else {
                    $crate::skipped(&format!(
                        "{}: store not configured",
                        stringify!($case)
                    ));
                    return;
                };
                store.migrate().await.expect("migrate");
                $crate::cases::$case(store).await;
            }
        )*
    };
}

/// Generate one `#[tokio::test]` per body conformance case. `$make` is a path to an
/// `async fn() -> Option<DynStore>`, like [`store_conformance!`]: `None` skips the suite (and
/// fails instead under `ADAM_TEST_REQUIRE_DB=1`, see `adam_core::testing`).
///
/// Every case asserts that the store keeps bodies ([`Store::keeps_bodies`](adam_core::Store::keeps_bodies)):
/// a store that does not is not given this suite, rather than passing it by answering the
/// defaulted methods.
#[macro_export]
macro_rules! body_conformance {
    ($make:path) => {
        $crate::body_conformance!(@cases $make;
            body_an_append_writes_only_the_delta, body_the_generation_is_stable_across_appends,
            body_a_compaction_starts_a_new_generation_and_the_gc_collects_the_old,
            body_a_lost_cas_leaves_only_the_winners_rows,
            body_a_first_split_writes_the_whole_transcript,
            body_the_same_transcript_committed_again_adds_no_row, body_purge_removes_the_rows,
            body_a_non_contiguous_index_is_refused,
            body_an_unknown_generation_is_empty,
            body_a_nul_is_stored_as_the_replacement_character,
        );
    };
    (@cases $make:path; $($case:ident),* $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                let Some(store) = $make().await else {
                    $crate::skipped(&format!(
                        "{}: store not configured",
                        stringify!($case)
                    ));
                    return;
                };
                store.migrate().await.expect("migrate");
                $crate::cases::$case(store).await;
            }
        )*
    };
}

/// Generate the `measure_the_headless_transcript` test: run
/// [`counting::measure_headless_transcript`] through `$make`, print its line, and assert that the
/// workload ran.
///
/// `$make` is a path to an `async fn() -> Option<DynStore>`, like [`store_conformance!`]: `None`
/// skips the test (and fails instead under `ADAM_TEST_REQUIRE_DB=1`). A store that keeps no bodies
/// skips it too.
#[macro_export]
macro_rules! headless_measurement {
    ($make:path) => {
        #[tokio::test]
        async fn measure_the_headless_transcript() {
            let Some(store) = $make().await else {
                $crate::skipped("measure_the_headless_transcript: store not configured");
                return;
            };
            store.migrate().await.expect("migrate");
            if !store.keeps_bodies() {
                $crate::skipped("measure_the_headless_transcript: store keeps no bodies");
                return;
            }
            let measured = $crate::counting::measure_headless_transcript(store).await;
            println!("{measured}");
            assert_eq!(
                measured.steps,
                $crate::counting::HEADLESS_STEPS,
                "the workload ran to the end"
            );
        }
    };
}

/// Serialises the cases that sweep blobs inside one test binary: a blob is global and
/// content-addressed, so a sweep (`purge_blobs`, and `purge_finished` in the same call) with a
/// future cutoff deletes every unreferenced blob in the database, including one a case running at
/// the same time is about to read. Runs are isolated by agent name; blobs cannot be, so every case
/// that can sweep takes this lock. nextest runs each test in its own process, where a process-local
/// mutex excludes nothing: `.config/nextest.toml` puts the same cases in one single-thread group.
pub(crate) static BLOB_SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub mod cases {
    use super::*;

    /// The digest of `bytes`, computed here so the suite pins the convention: the same SHA-256 the
    /// runtime's `blob_id` makes, as 64 lower-case hex. A store trusts the id it is given.
    fn blob_digest(bytes: &[u8]) -> BlobId {
        use sha2::Digest as _;
        let mut digest = [0_u8; 32];
        digest.copy_from_slice(&sha2::Sha256::digest(bytes));
        BlobId::from_sha256(&digest)
    }

    /// A store the blob suite runs against keeps blobs: a store that does not is not given
    /// `blob_conformance!`, rather than passing it by defaulting every answer.
    fn keeps_blobs(store: &DynStore) {
        assert!(
            store.keeps_blobs(),
            "the blob suite needs a store that keeps blobs"
        );
    }

    /// A store the body suite runs against keeps bodies: a store that does not is not given
    /// `body_conformance!`, rather than passing it by defaulting every answer.
    fn keeps_bodies(store: &DynStore) {
        assert!(
            store.keeps_bodies(),
            "the body suite needs a store that keeps bodies"
        );
    }

    /// The smallest document a head can be for a store test: the runtime leaves the body's
    /// `generation` null and the store fills it. The store patches `len` to the rows it wrote.
    fn head_doc(len: usize) -> Value {
        json!({
            "v": 5,
            "agent": {},
            "body": { "generation": null, "len": len },
        })
    }

    /// A head whose agent carries `agent`, for the NUL case.
    fn head_doc_with_agent(agent: Value, len: usize) -> Value {
        json!({
            "v": 5,
            "agent": agent,
            "body": { "generation": null, "len": len },
        })
    }

    fn rows(generation: Option<Uuid>, writer: Uuid, messages: &[&str]) -> BodyWrite {
        BodyWrite {
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
        }
    }

    /// The generation a served head names.
    fn generation_of(head: &Value) -> Uuid {
        Uuid::parse_str(
            head["body"]["generation"]
                .as_str()
                .expect("the store resolved a generation"),
        )
        .expect("a uuid")
    }

    fn messages(rows: &[BodyRow]) -> Vec<Value> {
        rows.iter().map(|row| row.message.clone()).collect()
    }

    pub async fn blob_round_trip(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let bytes = b"a run's file, byte for byte: \x00\xff\x01".to_vec();
        let id = blob_digest(&bytes);
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            None,
            "nothing is stored under the digest yet"
        );
        store.put_blob(&id, &bytes).await.unwrap();
        assert_eq!(store.get_blob(&id).await.unwrap(), Some(bytes));
    }

    pub async fn blob_four_mib_round_trip(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        // A file artifact's cap (`MAX_ARTIFACT_FILE_BYTES`), which is the largest blob a run can
        // make — not a comparison with any older limit. The bytes are varied, so a store that
        // compresses or truncates cannot pass by accident.
        let bytes: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let id = blob_digest(&bytes);
        store.put_blob(&id, &bytes).await.unwrap();
        assert_eq!(
            store.get_blob(&id).await.unwrap().as_deref(),
            Some(bytes.as_slice()),
            "four mebibytes round-trip byte for byte"
        );
        // Leave no four-mebibyte row behind in a suite that runs repeatedly.
        store
            .purge_blobs(now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(store.get_blob(&id).await.unwrap(), None);
    }

    pub async fn blob_put_is_idempotent(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let id = blob_digest(b"first");
        store.put_blob(&id, b"first").await.unwrap();
        store.put_blob(&id, b"first").await.unwrap();
        // The bytes behind an id never change: a put under a taken id is a no-op, whatever it
        // carries.
        store.put_blob(&id, b"second").await.unwrap();
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            Some(b"first".to_vec()),
            "the first bytes of a digest win"
        );
    }

    pub async fn blob_unknown_id_is_none(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let id = BlobId::from_hex(&"0".repeat(64)).expect("64 lower-case hex digits");
        assert_eq!(store.get_blob(&id).await.unwrap(), None);
    }

    pub async fn blob_empty_bytes(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let id = blob_digest(b"");
        store.put_blob(&id, b"").await.unwrap();
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            Some(Vec::new()),
            "an empty file is a blob, not a missing one"
        );
    }

    pub async fn blob_a_run_reference_keeps_the_blob_alive(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let agent = agent();
        let bytes = b"kept by a live run's reference".to_vec();
        let id = blob_digest(&bytes);
        store.put_blob(&id, &bytes).await.unwrap();
        let run = store
            .create_run(NewRun::new(&agent, json!({"turn": 0})))
            .await
            .unwrap();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1})).blobs(vec![id.clone()]),
            )
            .await
            .unwrap();
        // A cutoff past every blob: only the reference keeps this one. (The sweep may also collect
        // an earlier case's unreferenced blob, so the count is not the assertion.)
        let cutoff = now() + chrono::Duration::hours(1);
        store.purge_blobs(cutoff).await.unwrap();
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            Some(bytes.clone()),
            "a referenced blob is never collected, however old"
        );

        // A second commit names a **different** digest and not the first: a store unions each
        // commit's delta with the references it already has, so both must survive the sweep. A
        // store that replaced the set with the delta fails here.
        let other_bytes = b"a second file the same run names".to_vec();
        let other = blob_digest(&other_bytes);
        store.put_blob(&other, &other_bytes).await.unwrap();
        store
            .commit_run(
                run.id,
                run.version + 1,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 2})).blobs(vec![other.clone()]),
            )
            .await
            .unwrap();
        store.purge_blobs(cutoff).await.unwrap();
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            Some(bytes.clone()),
            "an earlier reference survives a commit that names a different digest"
        );
        assert_eq!(
            store.get_blob(&other).await.unwrap(),
            Some(other_bytes.clone()),
            "the new digest is recorded"
        );

        // Clean up: a finished run that is purged takes its references, and then the bytes. The
        // last commit names no digest at all — the union keeps both.
        store
            .commit_run(
                run.id,
                run.version + 2,
                RunUpdate::new(RunStatus::Done, json!({"turn": 3})),
            )
            .await
            .unwrap();
        store.purge_blobs(cutoff).await.unwrap();
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            Some(bytes.clone()),
            "a commit that names no blob keeps the earlier references"
        );
        assert_eq!(
            store.get_blob(&other).await.unwrap(),
            Some(other_bytes.clone()),
            "and the one the previous commit added"
        );
        assert_eq!(store.purge_finished(&agent, cutoff).await.unwrap(), 1);
        assert_eq!(store.get_blob(&id).await.unwrap(), None);
        assert_eq!(store.get_blob(&other).await.unwrap(), None);
    }

    pub async fn blob_a_shared_digest_survives_until_the_last_run_is_purged(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let bytes = b"shared by two runs".to_vec();
        let id = blob_digest(&bytes);
        store.put_blob(&id, &bytes).await.unwrap();
        // Two agents, so each purge is one run's.
        let mut runs = Vec::new();
        for _ in 0..2 {
            let agent = agent();
            let run = store
                .create_run(NewRun::new(&agent, json!({})))
                .await
                .unwrap();
            store
                .commit_run(
                    run.id,
                    run.version,
                    RunUpdate::new(RunStatus::Done, json!({"done": true})).blobs(vec![id.clone()]),
                )
                .await
                .unwrap();
            runs.push((agent, run.id));
        }
        let cutoff = now() + chrono::Duration::hours(1);
        assert_eq!(store.purge_finished(&runs[0].0, cutoff).await.unwrap(), 1);
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            Some(bytes.clone()),
            "the other run still names the digest"
        );
        assert_eq!(store.purge_finished(&runs[1].0, cutoff).await.unwrap(), 1);
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            None,
            "the last reference went with its run"
        );
    }

    pub async fn blob_purge_respects_before(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let bytes = b"written now".to_vec();
        let id = blob_digest(&bytes);
        // A cutoff a minute ago: nothing this suite wrote is older than it, whatever the
        // millisecond rounding of the clock.
        let before = now() - chrono::Duration::minutes(1);
        store.put_blob(&id, &bytes).await.unwrap();
        store.purge_blobs(before).await.unwrap();
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            Some(bytes),
            "a blob written after the cutoff is kept"
        );
        // At least this one: another case's unreferenced blob may go too, which is the contract.
        assert!(
            store
                .purge_blobs(now() + chrono::Duration::hours(1))
                .await
                .unwrap()
                >= 1
        );
        assert_eq!(store.get_blob(&id).await.unwrap(), None);
    }

    pub async fn blob_purge_finished_removes_the_refs_and_the_blob(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_blobs(&store);
        let agent = agent();
        let bytes = b"collected with its run".to_vec();
        let id = blob_digest(&bytes);
        store.put_blob(&id, &bytes).await.unwrap();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Done, json!({"done": true})).blobs(vec![id.clone()]),
            )
            .await
            .unwrap();
        let cutoff = now() + chrono::Duration::hours(1);
        assert_eq!(store.purge_finished(&agent, cutoff).await.unwrap(), 1);
        assert_eq!(
            store.get_blob(&id).await.unwrap(),
            None,
            "the run's refs went with it, and nothing else named the digest"
        );
        assert!(
            store.load_run(run.id).await.unwrap().is_none(),
            "the run itself is gone"
        );
    }

    /// A normal commit appends only the rows after the head's `len`: the generation holds the
    /// transcript once, not once per commit.
    pub async fn body_an_append_writes_only_the_delta(store: DynStore) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({"turn": 0})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        let writer = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"v": 1, "turn": 1}))
                    .head(head_doc(2))
                    .body(rows(Some(generation), writer, &["a", "b"])),
            )
            .await
            .unwrap();
        assert_eq!(
            messages(&store.body_list(run.id, generation).await.unwrap()),
            vec![json!("a"), json!("b")],
            "the first split writes the whole transcript under its generation"
        );

        store
            .commit_run(
                run.id,
                run.version + 1,
                RunUpdate::new(RunStatus::Runnable, json!({"v": 1, "turn": 2}))
                    .head(head_doc(4))
                    .body(rows(None, Uuid::new_v4(), &["a", "b", "c", "d"])),
            )
            .await
            .unwrap();
        let rows = store.body_list(run.id, generation).await.unwrap();
        assert_eq!(
            messages(&rows),
            vec![json!("a"), json!("b"), json!("c"), json!("d")],
            "an append writes the two new rows, not the four of the transcript"
        );
        assert_eq!(
            rows.len(),
            4,
            "the generation holds the transcript once, not once per commit"
        );
        let head = store.load_head(run.id).await.unwrap().unwrap().state;
        assert_eq!(generation_of(&head), generation);
        assert_eq!(head["body"]["len"], 4, "the head counts the transcript");
    }

    /// Every append keeps the generation the first split chose; the head's `len` grows with the
    /// transcript.
    pub async fn body_the_generation_is_stable_across_appends(store: DynStore) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        let transcripts: [&[&str]; 3] = [&["one"], &["one", "two"], &["one", "two", "three"]];
        for (step, transcript) in transcripts.iter().enumerate() {
            store
                .commit_run(
                    run.id,
                    run.version + u64::try_from(step).unwrap(),
                    RunUpdate::new(RunStatus::Runnable, json!({"turn": step + 1}))
                        .head(head_doc(transcript.len()))
                        .body(rows(
                            (step == 0).then_some(generation),
                            Uuid::new_v4(),
                            transcript,
                        )),
                )
                .await
                .unwrap();
            let head = store.load_head(run.id).await.unwrap().unwrap().state;
            assert_eq!(
                generation_of(&head),
                generation,
                "an append never moves the generation"
            );
            assert_eq!(head["body"]["len"], transcript.len());
        }
        assert_eq!(store.body_list(run.id, generation).await.unwrap().len(), 3);
    }

    /// A compaction starts a new generation with the whole transcript; the old one stays readable
    /// until the GC's cutoff, which collects it and never the current one.
    pub async fn body_a_compaction_starts_a_new_generation_and_the_gc_collects_the_old(
        store: DynStore,
    ) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head_doc(2))
                    .body(rows(Some(first), Uuid::new_v4(), &["old", "older"])),
            )
            .await
            .unwrap();
        store
            .commit_run(
                run.id,
                run.version + 1,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                    .head(head_doc(3))
                    .body(rows(
                        Some(second),
                        Uuid::new_v4(),
                        &["new", "newer", "newest"],
                    )),
            )
            .await
            .unwrap();
        assert_eq!(store.body_list(run.id, first).await.unwrap().len(), 2);
        assert_eq!(
            messages(&store.body_list(run.id, second).await.unwrap()),
            vec![json!("new"), json!("newer"), json!("newest")],
            "a compaction writes the whole transcript under the new generation"
        );
        let head = store.load_head(run.id).await.unwrap().unwrap().state;
        assert_eq!(generation_of(&head), second);
        assert_eq!(head["body"]["len"], 3);

        let cutoff = now() + chrono::Duration::hours(1);
        assert!(
            store.gc_body(run.id, second, cutoff).await.unwrap() >= 2,
            "the superseded generation is collected"
        );
        assert!(
            store.body_list(run.id, first).await.unwrap().is_empty(),
            "the old generation is gone"
        );
        assert_eq!(
            store.body_list(run.id, second).await.unwrap().len(),
            3,
            "the current generation is never collected"
        );
        assert_eq!(
            store
                .gc_body(run.id, second, now() + chrono::Duration::hours(24))
                .await
                .unwrap(),
            0,
            "a second sweep collects nothing"
        );
    }

    /// A commit at a stale version writes no row: the generation holds the winner's transcript,
    /// not the loser's.
    pub async fn body_a_lost_cas_leaves_only_the_winners_rows(store: DynStore) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head_doc(2))
                    .body(rows(Some(generation), Uuid::new_v4(), &["winner", "kept"])),
            )
            .await
            .unwrap();
        // The same expected version, with a longer transcript: the version moved, so it loses.
        let err = store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1, "loser": true}))
                    .head(head_doc(4))
                    .body(rows(
                        None,
                        Uuid::new_v4(),
                        &["loser", "lost", "more", "rows"],
                    )),
            )
            .await
            .expect_err("a stale version loses the CAS");
        assert!(
            matches!(err, StoreError::Conflict { .. }),
            "the loser is a conflict: {err:?}"
        );
        assert_eq!(
            messages(&store.body_list(run.id, generation).await.unwrap()),
            vec![json!("winner"), json!("kept")],
            "the loser wrote no row into the winner's generation"
        );
        let head = store.load_head(run.id).await.unwrap().unwrap().state;
        assert_eq!(generation_of(&head), generation);
        assert_eq!(head["body"]["len"], 2, "the winner's head did not move");
    }

    /// A v4 run has no current head: its first split writes the whole transcript, whether the
    /// runtime names the generation or leaves it to the store.
    pub async fn body_a_first_split_writes_the_whole_transcript(store: DynStore) {
        keeps_bodies(&store);
        for named in [true, false] {
            let agent = agent();
            let run = store
                .create_run(NewRun::new(&agent, json!({})))
                .await
                .unwrap();
            let generation = Uuid::new_v4();
            store
                .commit_run(
                    run.id,
                    run.version,
                    RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                        .head(head_doc(3))
                        .body(rows(
                            named.then_some(generation),
                            Uuid::new_v4(),
                            &["one", "two", "three"],
                        )),
                )
                .await
                .unwrap();
            let head = store.load_head(run.id).await.unwrap().unwrap().state;
            let served = generation_of(&head);
            if named {
                assert_eq!(served, generation);
            }
            assert_eq!(head["body"]["len"], 3);
            assert_eq!(
                messages(&store.body_list(run.id, served).await.unwrap()),
                vec![json!("one"), json!("two"), json!("three")],
                "the first split writes the whole transcript under one generation"
            );
        }
    }

    /// The same transcript committed again (a lost acknowledgement, or an append that adds
    /// nothing) adds no row and keeps the first bytes. (A true CAS retry of one attempt is the
    /// runtime's writer test: the store cannot see the attempt.)
    pub async fn body_the_same_transcript_committed_again_adds_no_row(store: DynStore) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        let writer = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head_doc(2))
                    .body(rows(Some(generation), writer, &["first", "second"])),
            )
            .await
            .unwrap();
        store
            .commit_run(
                run.id,
                run.version + 1,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                    .head(head_doc(4))
                    .body(rows(None, writer, &["first", "second", "third", "fourth"])),
            )
            .await
            .unwrap();
        // The same commit again (an acknowledgement was lost): the same writer and transcript.
        store
            .commit_run(
                run.id,
                run.version + 2,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 2}))
                    .head(head_doc(4))
                    .body(rows(None, writer, &["first", "second", "third", "fourth"])),
            )
            .await
            .unwrap();
        assert_eq!(
            messages(&store.body_list(run.id, generation).await.unwrap()),
            vec![
                json!("first"),
                json!("second"),
                json!("third"),
                json!("fourth")
            ],
            "the retry added no row and rewrote none"
        );
    }

    /// A body row's `idx` is its position in the transcript: a gap or a repeated index is refused
    /// before anything is written, because the head's `len` would still match.
    pub async fn body_a_non_contiguous_index_is_refused(store: DynStore) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        let err = store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head_doc(2))
                    .body(BodyWrite {
                        generation: Some(generation),
                        rows: vec![
                            BodyRow {
                                idx: 0,
                                message: json!("a"),
                            },
                            BodyRow {
                                idx: 5,
                                message: json!("b"),
                            },
                        ],
                        writer: Uuid::new_v4(),
                    }),
            )
            .await
            .expect_err("a gap is not a transcript");
        assert!(
            matches!(err, StoreError::InvalidInput(_)),
            "the index is refused: {err:?}"
        );
        let loaded = store.load_run(run.id).await.unwrap().expect("the run");
        assert_eq!(loaded.version, 1, "the CAS did not run");
        assert!(
            store
                .body_list(run.id, generation)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Purging a run removes its body rows with its journal and references.
    pub async fn body_purge_removes_the_rows(store: DynStore) {
        let _turn = BLOB_SUITE.lock().await;
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Done, json!({"done": true}))
                    .head(head_doc(2))
                    .body(rows(Some(generation), Uuid::new_v4(), &["one", "two"])),
            )
            .await
            .unwrap();
        assert_eq!(store.body_list(run.id, generation).await.unwrap().len(), 2);
        let cutoff = now() + chrono::Duration::hours(1);
        assert_eq!(store.purge_finished(&agent, cutoff).await.unwrap(), 1);
        assert!(
            store
                .body_list(run.id, generation)
                .await
                .unwrap()
                .is_empty(),
            "the rows went with the run"
        );
        assert!(store.load_head(run.id).await.unwrap().is_none());
    }

    /// An unknown generation is empty: a generation is an identity, not a filter over all rows.
    pub async fn body_an_unknown_generation_is_empty(store: DynStore) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head_doc(1))
                    .body(rows(Some(generation), Uuid::new_v4(), &["only"])),
            )
            .await
            .unwrap();
        assert!(
            store
                .body_list(run.id, Uuid::new_v4())
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A NUL in a head and in a body row is stored as U+FFFD, as every JSON value is (ADR 0051).
    pub async fn body_a_nul_is_stored_as_the_replacement_character(store: DynStore) {
        keeps_bodies(&store);
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let generation = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}))
                    .head(head_doc_with_agent(json!({"text": "a\u{0}b"}), 1))
                    .body(rows(Some(generation), Uuid::new_v4(), &["x\u{0}y"])),
            )
            .await
            .unwrap();
        let head = store.load_head(run.id).await.unwrap().unwrap().state;
        assert_eq!(head["agent"]["text"], json!("a\u{FFFD}b"));
        let rows = store.body_list(run.id, generation).await.unwrap();
        assert_eq!(rows[0].message, json!("x\u{FFFD}y"));
    }

    pub async fn create_and_load(store: DynStore) {
        let agent = agent();
        let parent = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let wake = now() + chrono::Duration::minutes(5);
        let new = NewRun::new(&agent, json!({"turn": 0}))
            .conversation("conv-1")
            .parent(parent.id)
            .status(RunStatus::Parked)
            .wake_at(wake);
        let before = now();
        let created = store.create_run(new.clone()).await.unwrap();
        assert_eq!(created.id, new.id);
        assert_eq!(created.agent, agent);
        assert_eq!(created.conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(created.parent_id, Some(parent.id));
        assert_eq!(created.status, RunStatus::Parked);
        assert_eq!(created.wake_at, Some(truncate_ms(wake)));
        assert_eq!(created.version, 1);
        assert!(created.created_at >= before - chrono::Duration::seconds(5));
        assert_eq!(created.created_at, created.updated_at);
        assert!(
            created.state.is_null(),
            "a write does not echo the state: the caller has it"
        );

        let loaded = store
            .load_run(created.id)
            .await
            .unwrap()
            .expect("run exists");
        assert_eq!(
            loaded.state, new.state,
            "the stored state is the one written"
        );
        assert_eq!(
            loaded,
            adam_core::RunRecord {
                state: loaded.state.clone(),
                ..created
            },
            "every other field of the returned record is the stored one"
        );
    }

    /// `create_run` and `commit_run` return the record without its state: the caller wrote it, and
    /// echoing it back would transfer the largest member of the record on every commit.
    /// `journal_put` is the exception: the loop decodes the step's outcome from what it returns.
    pub async fn a_write_does_not_echo_the_state(store: DynStore) {
        let state = json!({"turn": 0, "transcript": ["hello", "world"]});
        let created = store
            .create_run(NewRun::new(agent(), state.clone()))
            .await
            .unwrap();
        assert!(created.state.is_null(), "create_run returns no state");
        assert_eq!(
            store.load_run(created.id).await.unwrap().unwrap().state,
            state
        );

        let next = json!({"turn": 1});
        let committed = store
            .commit_run(
                created.id,
                created.version,
                RunUpdate::new(RunStatus::Runnable, next.clone()),
            )
            .await
            .unwrap();
        assert!(committed.state.is_null(), "commit_run returns no state");
        assert_eq!(
            store.load_run(created.id).await.unwrap().unwrap().state,
            next
        );

        let entry = store
            .journal_put(
                created.id,
                JournalEntry::ok(0, "tool", json!({"out": "a\u{0}b"})),
            )
            .await
            .unwrap();
        assert_eq!(
            entry.payload,
            json!({"out": "a\u{FFFD}b"}),
            "journal_put returns the payload it stored"
        );
    }

    pub async fn state_roundtrip(store: DynStore) {
        let state = json!({
            "messages": [
                {"role": "user", "content": "Bonjour, ça va ? 你好 👋"},
                {"role": "assistant", "tool_calls": [{"id": "c1", "args": {"city": "Douala"}}]}
            ],
            "ints": [0, -1, 42, 9007199254740993_i64, i64::MIN, i64::MAX],
            "floats": [0.5, -1.25, 1e-9, 12345.678],
            "flags": [true, false, null],
            "empty_obj": {},
            "empty_arr": [],
            "nested": {"a": {"b": {"c": {"d": [1, [2, [3]]]}}}},
            "weird keys": {"with space": 1, "ünïcödé": 2, "": 3},
            // JSON Schema and OpenAPI fragments in tool definitions use keys that
            // document databases treat specially.
            "schema": {"$schema": "https://json-schema.org/draft/2020-12/schema", "$ref": "#/$defs/City",
                       "$defs": {"City": {"type": "string"}}},
            "dotted": {"a.b": {"c.d": 1}, ".": 2, "$": 3, "%": 4, "%24ref": 5, "trailing.": 6}
        });
        let run = store
            .create_run(NewRun::new(agent(), state.clone()))
            .await
            .unwrap();
        assert!(run.state.is_null(), "a write does not echo the state");
        let loaded = store.load_run(run.id).await.unwrap().unwrap();
        assert_eq!(loaded.state, state, "state must roundtrip exactly");

        let next = json!({"scalar_state": "just a string"});
        let committed = store
            .commit_run(run.id, 1, RunUpdate::new(RunStatus::Runnable, next.clone()))
            .await
            .unwrap();
        assert!(committed.state.is_null());
        assert_eq!(store.load_run(run.id).await.unwrap().unwrap().state, next);
        let array_state = json!([1, "two", {"three": 3}]);
        store
            .commit_run(
                run.id,
                2,
                RunUpdate::new(RunStatus::Runnable, array_state.clone()),
            )
            .await
            .unwrap();
        assert_eq!(
            store.load_run(run.id).await.unwrap().unwrap().state,
            array_state
        );
    }

    /// A NUL cannot be stored by every backend (PostgreSQL's `JSONB` refuses it), so every store
    /// replaces it with U+FFFD in strings and keys, before it writes: the value a store returns is
    /// the value every other store would hold, and a run that printed a NUL does not fail on the
    /// store it happens to run on (ADR 0051).
    pub async fn nul_characters_are_stored_as_the_replacement_character(store: DynStore) {
        for (state, want) in [
            (json!({"text": "a\u{0}b"}), json!({"text": "a\u{FFFD}b"})),
            (json!({"k\u{0}ey": 1}), json!({"k\u{FFFD}ey": 1})),
            (
                json!({"nested": [{"deep": "x\u{0}\u{0}y"}]}),
                json!({"nested": [{"deep": "x\u{FFFD}\u{FFFD}y"}]}),
            ),
        ] {
            let run = store.create_run(NewRun::new(agent(), state)).await.unwrap();
            assert!(run.state.is_null(), "create_run returns no state");
            assert_eq!(
                store.load_run(run.id).await.unwrap().unwrap().state,
                want,
                "the state read back is the one stored"
            );
        }
        // A journal payload too: a tool's output is where a NUL comes from.
        let run = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        let entry = store
            .journal_put(
                run.id,
                JournalEntry::ok(0, "tool", json!({"content": "before\u{0}after"})),
            )
            .await
            .unwrap();
        assert_eq!(
            entry.payload,
            json!({"content": "before\u{FFFD}after"}),
            "journal_put returns what is stored"
        );
        assert_eq!(
            store.journal_get(run.id, 0).await.unwrap().unwrap().payload,
            entry.payload
        );
        // A commit, which is the state a run keeps between transitions.
        let committed = store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"text": "a\u{0}b"})),
            )
            .await
            .unwrap();
        assert!(committed.state.is_null());
        assert_eq!(
            store.load_run(run.id).await.unwrap().unwrap().state,
            json!({"text": "a\u{FFFD}b"})
        );
        // A push configuration and its cursor.
        let mut new = new_push(run.id, &agent(), "c1");
        new.config = json!({"url": "https://hooks.example/\u{0}"});
        new.cursor = json!({"status": "\u{0}"});
        let push = store.push_put(new).await.unwrap();
        assert_eq!(
            push.config,
            json!({"url": "https://hooks.example/\u{FFFD}"})
        );
        assert_eq!(push.cursor, json!({"status": "\u{FFFD}"}));
        let committed_push = store
            .push_commit(
                run.id,
                "c1",
                push.version,
                PushProgress {
                    state: PushState::Active,
                    cursor: json!({"status": "a\u{0}b"}),
                    attempts: 1,
                    last_error: Some("status 503".into()),
                    next_attempt_at: now(),
                },
            )
            .await
            .unwrap();
        assert_eq!(committed_push.cursor, json!({"status": "a\u{FFFD}b"}));
        // `last_error` is a text column, not JSON: a NUL there is the caller's, not a tool's output,
        // and PostgreSQL refuses it (ADR 0051 says what this decision covers).
        assert_eq!(committed_push.last_error.as_deref(), Some("status 503"));
    }

    pub async fn create_duplicate_id(store: DynStore) {
        let new = NewRun::new(agent(), json!({}));
        store.create_run(new.clone()).await.unwrap();
        let err = store.create_run(new.clone()).await.unwrap_err();
        assert!(
            matches!(err, StoreError::AlreadyExists(id) if id == new.id),
            "{err:?}"
        );
    }

    pub async fn load_missing(store: DynStore) {
        assert_eq!(store.load_run(RunId::new()).await.unwrap(), None);
    }

    /// `run_header` says what `load_run` and `lease_until` say together: the same run without its
    /// state, and the lease that makes a worker's run `claimed` once it is taken. A finished run
    /// is not `claimed` whatever lease it still carries, and an unknown id is `None`.
    pub async fn run_header_matches_the_record(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({"turn": 0})).conversation("c"))
            .await
            .unwrap();
        assert_eq!(store.run_header(RunId::new()).await.unwrap(), None);

        let header = store.run_header(run.id).await.unwrap().expect("a header");
        let rec = store.load_run(run.id).await.unwrap().expect("the record");
        assert_eq!(
            (
                header.id,
                &header.agent,
                &header.conversation_id,
                header.parent_id,
                header.status,
                header.wake_at,
                header.version,
                header.created_at,
                header.updated_at,
            ),
            (
                rec.id,
                &rec.agent,
                &rec.conversation_id,
                rec.parent_id,
                rec.status,
                rec.wake_at,
                rec.version,
                rec.created_at,
                rec.updated_at,
            ),
            "the header is the record without its state"
        );
        assert_eq!(header.lease_until, None, "never claimed");
        assert!(!header.claimed(now()));

        let t = truncate_ms(now() + chrono::Duration::seconds(1));
        let ttl = Duration::from_secs(10);
        assert_eq!(
            claim_ids(&store, &agent, "w1", t, ttl, 1).await,
            vec![run.id]
        );
        let until = store.lease_until(run.id).await.unwrap().expect("a lease");
        let header = store.run_header(run.id).await.unwrap().expect("a header");
        assert_eq!(header.lease_until, Some(until));
        assert!(header.claimed(t + chrono::Duration::seconds(1)));
        assert!(!header.claimed(until), "the lease ends at `until`");

        // A commit is visible in the header, and a finished run is never `claimed`, whatever
        // lease it still carries (a commit does not touch the lease).
        let rec = store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Done, json!({"turn": 1})),
            )
            .await
            .unwrap();
        let header = store.run_header(run.id).await.unwrap().expect("a header");
        assert_eq!(
            (header.status, header.version),
            (RunStatus::Done, rec.version)
        );
        assert_eq!(header.lease_until, Some(until), "the commit kept the lease");
        assert!(!header.claimed(t + chrono::Duration::seconds(1)));
    }

    pub async fn commit_advances_version(store: DynStore) {
        let run = store
            .create_run(NewRun::new(agent(), json!({"turn": 0})))
            .await
            .unwrap();
        let wake = now() + chrono::Duration::hours(1);
        let v2 = store
            .commit_run(
                run.id,
                1,
                RunUpdate::new(RunStatus::Parked, json!({"turn": 1})).wake_at(wake),
            )
            .await
            .unwrap();
        assert_eq!(v2.version, 2);
        assert_eq!(v2.status, RunStatus::Parked);
        assert!(v2.state.is_null(), "a commit does not echo the state");
        assert_eq!(v2.wake_at, Some(truncate_ms(wake)));
        assert_eq!(v2.created_at, run.created_at);
        assert!(v2.updated_at >= run.updated_at);
        let stored = store.load_run(run.id).await.unwrap().unwrap();
        assert_eq!(stored.state, json!({"turn": 1}));
        assert_eq!(
            stored,
            adam_core::RunRecord {
                state: stored.state.clone(),
                ..v2
            },
            "every other field of the returned record is the stored one"
        );

        let v3 = store
            .commit_run(
                run.id,
                2,
                RunUpdate::new(RunStatus::Done, json!({"turn": 2})),
            )
            .await
            .unwrap();
        assert_eq!(v3.version, 3);
        assert_eq!(v3.wake_at, None, "wake_at is replaced, not merged");
    }

    pub async fn commit_stale_version_conflicts(store: DynStore) {
        let run = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        store
            .commit_run(run.id, 1, RunUpdate::new(RunStatus::Runnable, json!(1)))
            .await
            .unwrap();
        let err = store
            .commit_run(run.id, 1, RunUpdate::new(RunStatus::Runnable, json!(2)))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict { run: id, expected: 1, actual: 2 } if id == run.id),
            "{err:?}"
        );
        let err = store
            .commit_run(run.id, 7, RunUpdate::new(RunStatus::Runnable, json!(2)))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Conflict {
                    expected: 7,
                    actual: 2,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            store.load_run(run.id).await.unwrap().unwrap().state,
            json!(1)
        );
    }

    pub async fn commit_missing_run(store: DynStore) {
        let id = RunId::new();
        let err = store
            .commit_run(id, 1, RunUpdate::new(RunStatus::Runnable, json!({})))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(x) if x == id), "{err:?}");
    }

    pub async fn concurrent_commits_single_winner(store: DynStore) {
        let run = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        let attempts = (0..16).map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .commit_run(
                        run.id,
                        1,
                        RunUpdate::new(RunStatus::Runnable, json!({"writer": i})),
                    )
                    .await
            })
        });
        let results: Vec<_> = join_all(attempts)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let winners: Vec<_> = results
            .iter()
            .enumerate()
            .filter_map(|(writer, r)| r.as_ref().ok().map(|rec| (writer, rec)))
            .collect();
        assert_eq!(
            winners.len(),
            1,
            "exactly one commit per version may succeed"
        );
        for r in &results {
            if let Err(e) = r {
                assert!(
                    matches!(e, StoreError::Conflict { .. }),
                    "losers must see Conflict, got {e:?}"
                );
            }
        }
        assert!(winners[0].1.state.is_null(), "a commit echoes no state");
        let stored = store.load_run(run.id).await.unwrap().unwrap();
        assert_eq!(stored.version, 2);
        assert_eq!(stored.state, json!({"writer": winners[0].0}));
    }

    pub async fn journal_roundtrip_and_order(store: DynStore) {
        let run = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        assert_eq!(store.journal_get(run.id, 0).await.unwrap(), None);
        assert!(store.journal_list(run.id).await.unwrap().is_empty());
        for seq in [2_u64, 0, 1, 10] {
            let entry = if seq == 1 {
                JournalEntry::err(seq, format!("step-{seq}"), json!({"error": "boom"}))
            } else {
                JournalEntry::ok(
                    seq,
                    format!("step-{seq}"),
                    json!({"n": seq, "big": u32::MAX, "$ref": "#/x", "a.b": [1]}),
                )
            };
            let stored = store.journal_put(run.id, entry.clone()).await.unwrap();
            assert_eq!(stored, entry);
            assert_eq!(store.journal_get(run.id, seq).await.unwrap(), Some(entry));
        }
        let list = store.journal_list(run.id).await.unwrap();
        assert_eq!(
            list.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2, 10]
        );
        assert!(!list[1].ok);
        assert_eq!(list[1].payload, json!({"error": "boom"}));

        let other = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        assert!(
            store.journal_list(other.id).await.unwrap().is_empty(),
            "journals are per run"
        );
    }

    pub async fn journal_first_writer_wins(store: DynStore) {
        let run = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        let first = JournalEntry::ok(0, "charge", json!({"receipt": "r-1"}));
        store.journal_put(run.id, first.clone()).await.unwrap();
        let replay = JournalEntry::ok(0, "charge", json!({"receipt": "r-2"}));
        let got = store.journal_put(run.id, replay).await.unwrap();
        assert_eq!(
            got, first,
            "a replayed step must get the recorded result back"
        );
        assert_eq!(store.journal_get(run.id, 0).await.unwrap(), Some(first));
    }

    pub async fn journal_concurrent_writers_agree(store: DynStore) {
        let run = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        let writers = (0..16).map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .journal_put(run.id, JournalEntry::ok(3, "fetch", json!({"writer": i})))
                    .await
            })
        });
        let results: Vec<JournalEntry> = join_all(writers)
            .await
            .into_iter()
            .map(|r| r.unwrap().unwrap())
            .collect();
        let first = &results[0];
        assert!(
            results.iter().all(|r| r == first),
            "all writers must observe the same winner"
        );
        assert_eq!(
            store.journal_get(run.id, 3).await.unwrap().as_ref(),
            Some(first)
        );
        assert_eq!(store.journal_list(run.id).await.unwrap().len(), 1);
    }

    pub async fn journal_detects_nondeterminism(store: DynStore) {
        let run = store
            .create_run(NewRun::new(agent(), json!({})))
            .await
            .unwrap();
        store
            .journal_put(run.id, JournalEntry::ok(0, "charge", json!(1)))
            .await
            .unwrap();
        let err = store
            .journal_put(run.id, JournalEntry::ok(0, "refund", json!(1)))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::NonDeterminism { seq: 0, recorded, requested, .. }
                if recorded == "charge" && requested == "refund"),
            "{err:?}"
        );
    }

    pub async fn journal_requires_run(store: DynStore) {
        let id = RunId::new();
        let err = store
            .journal_put(id, JournalEntry::ok(0, "x", json!(null)))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(x) if x == id), "{err:?}");
    }

    pub async fn claim_respects_due_rules(store: DynStore) {
        let agent = agent();
        let t0 = now();
        let soon = t0 + chrono::Duration::seconds(30);
        let runnable = store
            .create_run(NewRun::new(&agent, json!("runnable")))
            .await
            .unwrap();
        let delayed = store
            .create_run(NewRun::new(&agent, json!("delayed")).wake_at(soon))
            .await
            .unwrap();
        let timer = store
            .create_run(
                NewRun::new(&agent, json!("timer"))
                    .status(RunStatus::Parked)
                    .wake_at(soon),
            )
            .await
            .unwrap();
        let waiting = store
            .create_run(NewRun::new(&agent, json!("waiting")).status(RunStatus::Parked))
            .await
            .unwrap();
        let done = store
            .create_run(NewRun::new(&agent, json!("done")).status(RunStatus::Done))
            .await
            .unwrap();

        let ttl = Duration::from_secs(60);
        let first = claim_ids(
            &store,
            &agent,
            "w1",
            t0 + chrono::Duration::seconds(1),
            ttl,
            10,
        )
        .await;
        assert_eq!(
            first,
            vec![runnable.id],
            "only the plain runnable run is due now"
        );

        let later = soon + chrono::Duration::seconds(1);
        let mut second = claim_ids(&store, &agent, "w1", later, ttl, 10).await;
        second.sort();
        let mut expected = vec![delayed.id, timer.id];
        expected.sort();
        assert_eq!(
            second, expected,
            "the delayed runnable run and the timer become due; the first lease is still held"
        );

        let far = t0 + chrono::Duration::days(365);
        let third = claim_ids(&store, &agent, "w2", far, ttl, 10).await;
        let third: HashSet<_> = third.into_iter().collect();
        assert!(
            !third.contains(&waiting.id),
            "parked without wake_at is never due"
        );
        assert!(!third.contains(&done.id), "finished runs are never due");
        assert!(
            third.contains(&runnable.id),
            "expired leases are claimable again"
        );

        // Committing the waiting run back to runnable makes it due.
        store
            .commit_run(
                waiting.id,
                1,
                RunUpdate::new(RunStatus::Runnable, json!("resumed")),
            )
            .await
            .unwrap();
        let resumed = claim_ids(
            &store,
            &agent,
            "w3",
            far + chrono::Duration::days(1),
            ttl,
            10,
        )
        .await;
        assert!(resumed.contains(&waiting.id));
    }

    pub async fn claim_filters_agents_and_limit(store: DynStore) {
        let (a, b) = (agent(), agent());
        let mut a_ids = Vec::new();
        for i in 0..5 {
            a_ids.push(
                store
                    .create_run(NewRun::new(&a, json!(i)))
                    .await
                    .unwrap()
                    .id,
            );
            store.create_run(NewRun::new(&b, json!(i))).await.unwrap();
        }
        let now = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(60);
        let leases = store
            .claim_due(
                std::slice::from_ref(&a),
                "w",
                ClaimScope::Any,
                &[],
                now,
                ttl,
                3,
            )
            .await
            .unwrap();
        assert_eq!(leases.len(), 3, "limit is honoured");
        for l in &leases {
            assert_eq!(l.run.agent, a, "only requested agents are claimed");
            assert_eq!(l.worker, "w");
            assert_eq!(l.until, truncate_ms(now) + chrono::Duration::seconds(60));
        }
        let first_three: Vec<_> = leases.iter().map(|l| l.run.id).collect();
        assert_eq!(first_three, a_ids[..3].to_vec(), "earliest sched_at first");

        let rest = store
            .claim_due(
                &[a.clone(), b.clone()],
                "w",
                ClaimScope::Any,
                &[],
                now,
                ttl,
                100,
            )
            .await
            .unwrap();
        assert_eq!(rest.len(), 2 + 5);
        assert!(
            store
                .claim_due(&[], "w", ClaimScope::Any, &[], now, ttl, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .claim_due(
                    std::slice::from_ref(&a),
                    "w",
                    ClaimScope::Any,
                    &[],
                    now,
                    ttl,
                    0
                )
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// The runs a caller names as busy are never claimed, with a live lease, an expired one or
    /// none, and they take no slot of `limit`. A worker that still steps a run whose lease ran
    /// out must not be leased the run again: the snapshot of the claim would be older than what
    /// its own step is about to commit. A busy run is left as it is, for any other worker whose
    /// claim finds it expired.
    pub async fn claim_skips_busy_runs(store: DynStore) {
        for scope in [ClaimScope::Any, ClaimScope::Pinned] {
            let agent = agent();
            let mut runs = Vec::new();
            for i in 0..3 {
                runs.push(
                    store
                        .create_run(NewRun::new(&agent, json!(i)))
                        .await
                        .unwrap()
                        .id,
                );
            }
            let [first, second, third] = [runs[0], runs[1], runs[2]];
            let t = now() + chrono::Duration::seconds(1);
            let ttl = Duration::from_secs(30);

            let got = claim_ids_busy(&store, scope, &agent, &[first], t, ttl, 2).await;
            assert_eq!(
                got.into_iter().collect::<HashSet<_>>(),
                HashSet::from([second, third]),
                "{scope:?}: the busy run takes no slot of the limit"
            );
            assert_eq!(
                claim_ids_busy(&store, scope, &agent, &[], t, ttl, 10).await,
                vec![first],
                "{scope:?}: the busy run was not leased by the call that skipped it"
            );

            // All three leases have run out. Two runs are busy, so only the third is leased.
            let later = t + chrono::Duration::seconds(31);
            assert_eq!(
                claim_ids_busy(&store, scope, &agent, &[first, second], later, ttl, 10).await,
                vec![third],
                "{scope:?}: a busy run is skipped although its lease expired"
            );
            let left: HashSet<_> = claim_ids_busy(&store, scope, &agent, &[], later, ttl, 10)
                .await
                .into_iter()
                .collect();
            assert_eq!(
                left,
                HashSet::from([first, second]),
                "{scope:?}: and left expired, not leased to the caller behind its back"
            );
        }
    }

    pub async fn claim_is_exclusive_under_concurrency(store: DynStore) {
        let agent = agent();
        let total = 60;
        for i in 0..total {
            store
                .create_run(NewRun::new(&agent, json!(i)))
                .await
                .unwrap();
        }
        let now = now() + chrono::Duration::seconds(1);
        let workers = (0..8).map(|w| {
            let store = store.clone();
            let agent = agent.clone();
            tokio::spawn(async move {
                let mut mine = Vec::new();
                loop {
                    let got = store
                        .claim_due(
                            std::slice::from_ref(&agent),
                            &format!("w{w}"),
                            ClaimScope::Any,
                            &[],
                            now,
                            Duration::from_secs(60),
                            4,
                        )
                        .await
                        .unwrap();
                    if got.is_empty() {
                        return mine;
                    }
                    mine.extend(got.into_iter().map(|l| l.run.id));
                }
            })
        });
        let all: Vec<RunId> = join_all(workers)
            .await
            .into_iter()
            .flat_map(|r| r.unwrap())
            .collect();
        let unique: HashSet<_> = all.iter().copied().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "a run was leased to two workers at once"
        );
        assert_eq!(unique.len(), total, "every due run is claimed exactly once");
    }

    pub async fn lease_expiry_allows_takeover(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let t = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(30);
        assert_eq!(
            claim_ids(&store, &agent, "w1", t, ttl, 1).await,
            vec![run.id]
        );
        assert!(claim_ids(&store, &agent, "w2", t, ttl, 1).await.is_empty());
        let just_before = t + chrono::Duration::seconds(29);
        assert!(
            claim_ids(&store, &agent, "w2", just_before, ttl, 1)
                .await
                .is_empty()
        );
        let after = t + chrono::Duration::seconds(30);
        assert_eq!(
            claim_ids(&store, &agent, "w2", after, ttl, 1).await,
            vec![run.id]
        );
        // The old holder can no longer renew.
        assert!(!store.renew_lease(run.id, "w1", after, ttl).await.unwrap());
        // Versions, not leases, protect state: the stale worker's commit still races fairly.
        store
            .commit_run(run.id, 1, RunUpdate::new(RunStatus::Runnable, json!("w2")))
            .await
            .unwrap();
        let err = store
            .commit_run(run.id, 1, RunUpdate::new(RunStatus::Runnable, json!("w1")))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Conflict { .. }));
    }

    pub async fn renew_and_release_lease(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let t = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(10);
        claim_ids(&store, &agent, "w1", t, ttl, 1).await;

        assert!(!store.renew_lease(run.id, "intruder", t, ttl).await.unwrap());
        let t_renew = t + chrono::Duration::seconds(8);
        assert!(store.renew_lease(run.id, "w1", t_renew, ttl).await.unwrap());
        // Would have expired at t+10 without the renewal; now valid until t+18.
        assert!(
            claim_ids(
                &store,
                &agent,
                "w2",
                t + chrono::Duration::seconds(15),
                ttl,
                1
            )
            .await
            .is_empty()
        );
        assert!(!store.renew_lease(RunId::new(), "w1", t, ttl).await.unwrap());

        store.release_lease(run.id, "intruder").await.unwrap();
        assert!(
            claim_ids(
                &store,
                &agent,
                "w2",
                t + chrono::Duration::seconds(15),
                ttl,
                1
            )
            .await
            .is_empty(),
            "a non-owner cannot release"
        );
        store.release_lease(run.id, "w1").await.unwrap();
        assert_eq!(
            claim_ids(
                &store,
                &agent,
                "w2",
                t + chrono::Duration::seconds(15),
                ttl,
                1
            )
            .await,
            vec![run.id],
            "released runs are claimable at once"
        );
        assert!(
            !store
                .renew_lease(run.id, "w1", t + chrono::Duration::seconds(16), ttl)
                .await
                .unwrap()
        );
        store.release_lease(RunId::new(), "w1").await.unwrap();
    }

    /// `lease_until` says when the lease on a run ends: nothing before a claim, the claim's end
    /// after it, the renewed end after a renewal, nothing after a release, and nothing for a run
    /// that does not exist. A commit does not touch the lease. An expired lease is still reported:
    /// the caller compares with its own clock.
    pub async fn lease_until_reports_the_lease(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        assert_eq!(store.lease_until(run.id).await.unwrap(), None, "unclaimed");
        assert_eq!(store.lease_until(RunId::new()).await.unwrap(), None);

        let t = truncate_ms(now() + chrono::Duration::seconds(1));
        let ttl = Duration::from_secs(10);
        assert_eq!(
            claim_ids(&store, &agent, "w1", t, ttl, 1).await,
            vec![run.id]
        );
        let first_end = t + chrono::Duration::seconds(10);
        assert_eq!(store.lease_until(run.id).await.unwrap(), Some(first_end));

        let t_renew = t + chrono::Duration::seconds(8);
        assert!(store.renew_lease(run.id, "w1", t_renew, ttl).await.unwrap());
        let renewed_end = t_renew + chrono::Duration::seconds(10);
        assert_eq!(store.lease_until(run.id).await.unwrap(), Some(renewed_end));

        // Not the lease's business: a commit leaves it as it is.
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1})),
            )
            .await
            .unwrap();
        assert_eq!(store.lease_until(run.id).await.unwrap(), Some(renewed_end));

        // Only the holder's release clears it.
        store.release_lease(run.id, "intruder").await.unwrap();
        assert_eq!(store.lease_until(run.id).await.unwrap(), Some(renewed_end));
        store.release_lease(run.id, "w1").await.unwrap();
        assert_eq!(store.lease_until(run.id).await.unwrap(), None, "released");

        // A lease that ran out is reported as it was; whether it still counts is the caller's call.
        let t2 = renewed_end + chrono::Duration::seconds(1);
        assert_eq!(
            claim_ids(&store, &agent, "w2", t2, ttl, 1).await,
            vec![run.id]
        );
        assert_eq!(
            store.lease_until(run.id).await.unwrap(),
            Some(t2 + chrono::Duration::seconds(10))
        );
    }

    /// Once a run has an owner, a pinned claim by another worker never gets it: not while it
    /// is released, not after the owner's lease expired, not after a commit.
    pub async fn pinned_claim_never_gives_a_run_to_another_worker(store: DynStore) {
        let agent = agent();
        let mut mine = Vec::new();
        for i in 0..3 {
            mine.push(
                store
                    .create_run(NewRun::new(&agent, json!(i)))
                    .await
                    .unwrap()
                    .id,
            );
        }
        mine.sort();
        let t = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(30);
        let pinned = ClaimScope::Pinned;

        // Order is not the point here (a commit moves a run to the back), membership is.
        let claim = |worker: &'static str, at: DateTime<Utc>| {
            let (store, agent) = (store.clone(), agent.clone());
            async move {
                let mut ids = claim_ids_as(&store, pinned, &agent, worker, at, ttl, 10).await;
                ids.sort();
                ids
            }
        };

        assert_eq!(claim("w1", t).await, mine, "w1 takes every unowned run");
        for id in &mine {
            store.release_lease(*id, "w1").await.unwrap();
        }
        assert!(
            claim("w2", t).await.is_empty(),
            "w2 must not take w1's runs"
        );
        // A step commits in between; the owner survives a commit.
        let record = store.load_run(mine[0]).await.unwrap().unwrap();
        store
            .commit_run(
                record.id,
                record.version,
                RunUpdate::new(RunStatus::Runnable, json!("stepped")),
            )
            .await
            .unwrap();
        assert!(
            claim("w2", t).await.is_empty(),
            "a commit does not free the owner"
        );
        assert_eq!(claim("w1", t).await, mine, "w1 gets them all back");
        // The owner's lease runs out (a crashed worker): the run is stranded, not adopted.
        let after = t + chrono::Duration::seconds(31);
        assert!(
            claim("w2", after).await.is_empty(),
            "an expired lease does not hand a pinned run to another worker"
        );
        assert_eq!(
            claim("w1", after).await,
            mine,
            "the owner can take its runs after the lease expired"
        );
        for id in &mine {
            store.release_lease(*id, "w1").await.unwrap();
        }
        // A run created later has no owner yet: the first pinned claimant gets it for good.
        let late = store
            .create_run(NewRun::new(&agent, json!("late")))
            .await
            .unwrap();
        let later = after + chrono::Duration::seconds(1);
        assert_eq!(claim("w2", later).await, vec![late.id]);
        store.release_lease(late.id, "w2").await.unwrap();
        assert!(
            !claim("w1", later).await.contains(&late.id),
            "w1 must not take w2's run"
        );
        assert_eq!(claim("w2", later).await, vec![late.id]);
    }

    /// The first pinned claim sets the owner; release, commit and unpinned claims leave it.
    pub async fn pinned_claim_sets_the_owner_on_first_claim(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let t = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(30);
        assert_eq!(
            claim_ids_as(&store, ClaimScope::Pinned, &agent, "w1", t, ttl, 1).await,
            vec![run.id]
        );
        store.release_lease(run.id, "w1").await.unwrap();
        // An unpinned claim still takes the run (the owner is ignored) ...
        assert_eq!(
            claim_ids_as(&store, ClaimScope::Any, &agent, "w2", t, ttl, 1).await,
            vec![run.id]
        );
        store.release_lease(run.id, "w2").await.unwrap();
        // ... and it did not change the owner.
        assert!(
            claim_ids_as(&store, ClaimScope::Pinned, &agent, "w2", t, ttl, 1)
                .await
                .is_empty(),
            "w1 is still the owner"
        );
        assert_eq!(
            claim_ids_as(&store, ClaimScope::Pinned, &agent, "w1", t, ttl, 1).await,
            vec![run.id]
        );
    }

    /// `ClaimScope::Any` behaves as before owners existed: it never sets an owner.
    pub async fn any_claim_ignores_and_never_sets_the_owner(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let t = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(30);
        for worker in ["w1", "w2", "w1", "w3"] {
            assert_eq!(
                claim_ids_as(&store, ClaimScope::Any, &agent, worker, t, ttl, 1).await,
                vec![run.id],
                "{worker} takes the run"
            );
            store.release_lease(run.id, worker).await.unwrap();
        }
        // No owner was set, so the first pinned claimant wins it.
        assert_eq!(
            claim_ids_as(&store, ClaimScope::Pinned, &agent, "w4", t, ttl, 1).await,
            vec![run.id]
        );
        store.release_lease(run.id, "w4").await.unwrap();
        assert!(
            claim_ids_as(&store, ClaimScope::Pinned, &agent, "w1", t, ttl, 1)
                .await
                .is_empty()
        );
    }

    /// Workers racing on pinned claims split the runs exactly once, and each worker then gets
    /// back exactly the runs it first took.
    pub async fn pinned_claims_are_exclusive_and_stable_under_concurrency(store: DynStore) {
        let agent = agent();
        let total = 48;
        for i in 0..total {
            store
                .create_run(NewRun::new(&agent, json!(i)))
                .await
                .unwrap();
        }
        let t = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(60);
        let workers = (0..4).map(|w| {
            let store = store.clone();
            let agent = agent.clone();
            tokio::spawn(async move {
                let name = format!("w{w}");
                let mut mine = Vec::new();
                loop {
                    let got =
                        claim_ids_as(&store, ClaimScope::Pinned, &agent, &name, t, ttl, 3).await;
                    if got.is_empty() {
                        return (name, mine);
                    }
                    mine.extend(got);
                }
            })
        });
        let split: Vec<(String, Vec<RunId>)> = join_all(workers)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let all: Vec<RunId> = split.iter().flat_map(|(_, ids)| ids.clone()).collect();
        let unique: HashSet<_> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "a run was claimed twice");
        assert_eq!(unique.len(), total, "every run is claimed exactly once");

        for (name, ids) in &split {
            for id in ids {
                store.release_lease(*id, name).await.unwrap();
            }
        }
        // A second round: each worker sees exactly its own runs, in any amount.
        for (name, ids) in &split {
            let mut again =
                claim_ids_as(&store, ClaimScope::Pinned, &agent, name, t, ttl, 100).await;
            let mut want = ids.clone();
            again.sort();
            want.sort();
            assert_eq!(again, want, "{name} gets back its own runs and no others");
        }
    }

    pub async fn conversation_single_open_run(store: DynStore) {
        let agent = agent();
        assert_eq!(
            store.open_run_for_conversation(&agent, "c").await.unwrap(),
            None
        );

        let first = store
            .create_run(NewRun::new(&agent, json!(1)).conversation("c"))
            .await
            .unwrap();
        assert_eq!(
            store
                .open_run_for_conversation(&agent, "c")
                .await
                .unwrap()
                .map(|run| run.id),
            Some(first.id)
        );

        let err = store
            .create_run(
                NewRun::new(&agent, json!(2))
                    .conversation("c")
                    .status(RunStatus::Parked),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, StoreError::ConversationBusy { agent: a, conversation_id } if *a == agent && conversation_id == "c"),
            "{err:?}"
        );
        // Other conversations, other agents, and finished runs are unaffected.
        store
            .create_run(NewRun::new(&agent, json!(3)).conversation("other"))
            .await
            .unwrap();
        store
            .create_run(NewRun::new(super::agent(), json!(4)).conversation("c"))
            .await
            .unwrap();
        store
            .create_run(
                NewRun::new(&agent, json!(5))
                    .conversation("c")
                    .status(RunStatus::Done),
            )
            .await
            .unwrap();
        // Prefix collisions must not count as the same conversation.
        let (a1, a2) = (format!("{agent}x"), agent.clone());
        store
            .create_run(NewRun::new(&a1, json!(6)).conversation("y"))
            .await
            .unwrap();
        store
            .create_run(NewRun::new(&a2, json!(7)).conversation("xy"))
            .await
            .unwrap();

        // Parking keeps the conversation busy; finishing frees it.
        store
            .commit_run(
                first.id,
                1,
                RunUpdate::new(RunStatus::Parked, json!("waiting")),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .open_run_for_conversation(&agent, "c")
                .await
                .unwrap()
                .map(|run| (run.id, run.version, run.status)),
            Some((first.id, 2, RunStatus::Parked))
        );
        store
            .commit_run(first.id, 2, RunUpdate::new(RunStatus::Done, json!("bye")))
            .await
            .unwrap();
        assert_eq!(
            store.open_run_for_conversation(&agent, "c").await.unwrap(),
            None
        );

        let second = store
            .create_run(NewRun::new(&agent, json!(8)).conversation("c"))
            .await
            .unwrap();
        // Re-opening the finished run would create two open runs.
        let err = store
            .commit_run(
                first.id,
                3,
                RunUpdate::new(RunStatus::Runnable, json!("again")),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::ConversationBusy { .. }),
            "{err:?}"
        );
        assert_eq!(store.load_run(first.id).await.unwrap().unwrap().version, 3);
        assert_eq!(
            store
                .open_run_for_conversation(&agent, "c")
                .await
                .unwrap()
                .map(|r| r.id),
            Some(second.id)
        );
    }

    pub async fn conversation_race_single_winner(store: DynStore) {
        let agent = agent();
        let attempts = (0..16).map(|i| {
            let store = store.clone();
            let agent = agent.clone();
            tokio::spawn(async move {
                store
                    .create_run(NewRun::new(agent, json!({"msg": i})).conversation("hot"))
                    .await
            })
        });
        let results: Vec<_> = join_all(attempts)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(ok, 1, "two inbound messages must not start two runs");
        for r in results.iter().filter_map(|r| r.as_ref().err()) {
            assert!(matches!(r, StoreError::ConversationBusy { .. }), "{r:?}");
        }
    }

    pub async fn purge_finished_runs(store: DynStore) {
        // It sweeps unreferenced blobs with its cutoff (see `BLOB_SUITE`).
        let _turn = BLOB_SUITE.lock().await;
        let agent = agent();
        let open = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let done = store
            .create_run(NewRun::new(&agent, json!({})).status(RunStatus::Done))
            .await
            .unwrap();
        let failed = store
            .create_run(NewRun::new(&agent, json!({})).status(RunStatus::Failed))
            .await
            .unwrap();
        let other_agent = store
            .create_run(NewRun::new(super::agent(), json!({})).status(RunStatus::Done))
            .await
            .unwrap();
        store
            .journal_put(done.id, JournalEntry::ok(0, "s", json!(1)))
            .await
            .unwrap();
        store
            .journal_put(open.id, JournalEntry::ok(0, "s", json!(1)))
            .await
            .unwrap();

        let cutoff_before = done.updated_at - chrono::Duration::seconds(1);
        assert_eq!(
            store.purge_finished(&agent, cutoff_before).await.unwrap(),
            0
        );

        let cutoff = now() + chrono::Duration::seconds(1);
        assert_eq!(store.purge_finished(&agent, cutoff).await.unwrap(), 2);
        assert_eq!(store.load_run(done.id).await.unwrap(), None);
        assert_eq!(store.load_run(failed.id).await.unwrap(), None);
        assert!(
            store.journal_list(done.id).await.unwrap().is_empty(),
            "journal goes with the run"
        );
        assert!(store.load_run(open.id).await.unwrap().is_some());
        assert_eq!(store.journal_list(open.id).await.unwrap().len(), 1);
        assert!(store.load_run(other_agent.id).await.unwrap().is_some());
    }

    fn new_push(run: RunId, agent: &str, id: &str) -> NewPushConfig {
        NewPushConfig {
            run,
            id: id.to_owned(),
            agent: agent.to_owned(),
            owner: "owner-1".to_owned(),
            config: json!({"url": "https://hooks.example/a2a", "token": "t"}),
            cursor: json!({"status": null}),
        }
    }

    fn progress(state: PushState, at: DateTime<Utc>) -> PushProgress {
        PushProgress {
            state,
            cursor: json!({"status": "working"}),
            attempts: 0,
            last_error: None,
            next_attempt_at: at,
        }
    }

    pub async fn push_put_creates_and_replaces(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let before = now();
        let created = store
            .push_put(new_push(run.id, &agent, "c1"))
            .await
            .unwrap();
        assert_eq!(created.run, run.id);
        assert_eq!(created.id, "c1");
        assert_eq!(created.agent, agent);
        assert_eq!(created.owner, "owner-1");
        assert_eq!(created.state, PushState::Active);
        assert_eq!(created.attempts, 0);
        assert_eq!(created.last_error, None);
        assert_eq!(created.version, 1);
        assert!(
            created.next_attempt_at >= before - chrono::Duration::seconds(5),
            "a new config is due at once"
        );
        assert_eq!(created.created_at, created.updated_at);

        // Progress, then a replacement: the config starts over, the version still only grows.
        let committed = store
            .push_commit(
                run.id,
                "c1",
                1,
                PushProgress {
                    attempts: 3,
                    last_error: Some("status 500".into()),
                    ..progress(PushState::GaveUp, now())
                },
            )
            .await
            .unwrap();
        assert_eq!(committed.version, 2);
        let mut again = new_push(run.id, &agent, "c1");
        again.config = json!({"url": "https://other.example/hook"});
        again.cursor = json!({"status": "fresh"});
        let replaced = store.push_put(again).await.unwrap();
        assert_eq!(replaced.version, 3, "a replacement bumps the version");
        assert_eq!(replaced.state, PushState::Active);
        assert_eq!(replaced.attempts, 0);
        assert_eq!(replaced.last_error, None);
        assert_eq!(
            replaced.config,
            json!({"url": "https://other.example/hook"})
        );
        assert_eq!(replaced.cursor, json!({"status": "fresh"}));
        assert_eq!(
            replaced.created_at, created.created_at,
            "created_at is kept"
        );
        let listed = store.push_list(run.id).await.unwrap();
        assert_eq!(listed, vec![replaced]);
    }

    pub async fn push_put_requires_the_run(store: DynStore) {
        let agent = agent();
        let ghost = RunId::new();
        let err = store
            .push_put(new_push(ghost, &agent, "c1"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::NotFound(id) if id == ghost),
            "{err:?}"
        );
        assert!(store.push_list(ghost).await.unwrap().is_empty());
    }

    pub async fn push_list_is_per_run_and_ordered(store: DynStore) {
        let agent = agent();
        let a = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let b = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        for id in ["c", "a", "b"] {
            store.push_put(new_push(a.id, &agent, id)).await.unwrap();
        }
        store.push_put(new_push(b.id, &agent, "z")).await.unwrap();
        let ids = |records: Vec<adam_core::PushRecord>| -> Vec<String> {
            records.into_iter().map(|r| r.id).collect()
        };
        assert_eq!(ids(store.push_list(a.id).await.unwrap()), ["a", "b", "c"]);
        assert_eq!(ids(store.push_list(b.id).await.unwrap()), ["z"]);
        assert!(store.push_list(RunId::new()).await.unwrap().is_empty());
    }

    pub async fn push_config_and_cursor_roundtrip(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let config = json!({
            "url": "https://hooks.example/ü/你好?x=1&y=2",
            "token": "tok-€",
            "authentication": {"scheme": "Bearer", "credentials": "s3cr3t"},
            "$ref": 1, "dotted.key": {"a.b": [1, 2, 3]},
        });
        let cursor = json!({"status": "TASK_STATE_WORKING|m1", "artifacts": ["x", "y"], "pending": null,
                            "nested": {"$schema": "z"}});
        let mut new = new_push(run.id, &agent, "c1");
        new.config = config.clone();
        new.cursor = cursor.clone();
        let created = store.push_put(new).await.unwrap();
        assert_eq!(created.config, config);
        assert_eq!(created.cursor, cursor);
        let listed = store.push_list(run.id).await.unwrap();
        assert_eq!(listed[0].config, config);
        assert_eq!(listed[0].cursor, cursor);
    }

    pub async fn push_delete_is_idempotent(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        store
            .push_put(new_push(run.id, &agent, "c1"))
            .await
            .unwrap();
        store
            .push_put(new_push(run.id, &agent, "c2"))
            .await
            .unwrap();
        assert!(store.push_delete(run.id, "c1").await.unwrap());
        assert!(
            !store.push_delete(run.id, "c1").await.unwrap(),
            "second time"
        );
        assert!(!store.push_delete(run.id, "nope").await.unwrap());
        assert!(!store.push_delete(RunId::new(), "c2").await.unwrap());
        let left: Vec<_> = store
            .push_list(run.id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(left, ["c2"]);
    }

    pub async fn push_claim_respects_due_rules(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        for id in ["due", "later", "done", "gave-up"] {
            store.push_put(new_push(run.id, &agent, id)).await.unwrap();
        }
        let t = now();
        let soon = t + chrono::Duration::seconds(30);
        let version_of = |id: &str| {
            let store = store.clone();
            let id = id.to_owned();
            async move {
                store
                    .push_list(run.id)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|r| r.id == id)
                    .unwrap()
                    .version
            }
        };
        store
            .push_commit(
                run.id,
                "later",
                version_of("later").await,
                progress(PushState::Active, soon),
            )
            .await
            .unwrap();
        store
            .push_commit(
                run.id,
                "done",
                version_of("done").await,
                progress(PushState::Done, t),
            )
            .await
            .unwrap();
        store
            .push_commit(
                run.id,
                "gave-up",
                version_of("gave-up").await,
                progress(PushState::GaveUp, t),
            )
            .await
            .unwrap();

        let ttl = Duration::from_secs(60);
        let at = t + chrono::Duration::seconds(1);
        let agents = [agent.clone()];
        let first = store
            .push_claim_due(&agents, "w1", at, ttl, 10)
            .await
            .unwrap();
        assert_eq!(
            first.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["due"],
            "only an active, due config"
        );
        // The lease holds it.
        assert!(
            store
                .push_claim_due(&agents, "w2", at, ttl, 10)
                .await
                .unwrap()
                .is_empty()
        );
        // Once the lease has run out it is claimable again; a later config becomes due in time.
        let after = at + chrono::Duration::seconds(61);
        let again = store
            .push_claim_due(&agents, "w2", after, ttl, 10)
            .await
            .unwrap();
        let mut ids: Vec<_> = again.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["due", "later"]);
    }

    pub async fn push_claim_filters_agents_and_limit(store: DynStore) {
        let agent = agent();
        let other = super::agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let foreign = store
            .create_run(NewRun::new(&other, json!({})))
            .await
            .unwrap();
        for id in ["a", "b", "c"] {
            store.push_put(new_push(run.id, &agent, id)).await.unwrap();
        }
        store
            .push_put(new_push(foreign.id, &other, "x"))
            .await
            .unwrap();
        let at = now() + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(60);
        assert!(
            store
                .push_claim_due(&[], "w", at, ttl, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .push_claim_due(std::slice::from_ref(&agent), "w", at, ttl, 0)
                .await
                .unwrap()
                .is_empty()
        );
        let two = store
            .push_claim_due(std::slice::from_ref(&agent), "w", at, ttl, 2)
            .await
            .unwrap();
        assert_eq!(two.len(), 2);
        assert!(two.iter().all(|r| r.agent == agent));
        let rest = store
            .push_claim_due(std::slice::from_ref(&agent), "w", at, ttl, 10)
            .await
            .unwrap();
        assert_eq!(
            rest.len(),
            1,
            "the third; the foreign agent's is never taken"
        );
        let theirs = store
            .push_claim_due(std::slice::from_ref(&other), "w", at, ttl, 10)
            .await
            .unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].id, "x");
    }

    pub async fn push_claim_is_exclusive_under_concurrency(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        let total = 40;
        for i in 0..total {
            store
                .push_put(new_push(run.id, &agent, &format!("c{i:03}")))
                .await
                .unwrap();
        }
        let at = now() + chrono::Duration::seconds(1);
        let workers = (0..8).map(|w| {
            let store = store.clone();
            let agent = agent.clone();
            tokio::spawn(async move {
                let mut mine = Vec::new();
                loop {
                    let got = store
                        .push_claim_due(
                            std::slice::from_ref(&agent),
                            &format!("w{w}"),
                            at,
                            Duration::from_secs(60),
                            3,
                        )
                        .await
                        .unwrap();
                    if got.is_empty() {
                        return mine;
                    }
                    mine.extend(got.into_iter().map(|r| r.id));
                }
            })
        });
        let all: Vec<String> = join_all(workers)
            .await
            .into_iter()
            .flat_map(|r| r.unwrap())
            .collect();
        let unique: HashSet<_> = all.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "a config was leased to two workers at once"
        );
        assert_eq!(
            unique.len(),
            total,
            "every due config is claimed exactly once"
        );
    }

    pub async fn push_commit_advances_and_releases(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        store
            .push_put(new_push(run.id, &agent, "c1"))
            .await
            .unwrap();
        let t = now();
        let at = t + chrono::Duration::seconds(1);
        let ttl = Duration::from_secs(60);
        let agents = [agent.clone()];
        let claimed = store
            .push_claim_due(&agents, "w", at, ttl, 1)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(claimed.version, 1);
        let next = t + chrono::Duration::seconds(10);
        let committed = store
            .push_commit(
                run.id,
                "c1",
                claimed.version,
                PushProgress {
                    state: PushState::Active,
                    cursor: json!({"status": "working", "artifacts": ["a"]}),
                    attempts: 2,
                    last_error: Some("status 503".into()),
                    next_attempt_at: next,
                },
            )
            .await
            .unwrap();
        assert_eq!(committed.version, 2);
        assert_eq!(committed.attempts, 2);
        assert_eq!(committed.last_error.as_deref(), Some("status 503"));
        assert_eq!(committed.next_attempt_at, truncate_ms(next));
        assert_eq!(
            committed.cursor,
            json!({"status": "working", "artifacts": ["a"]})
        );
        assert!(committed.updated_at >= committed.created_at);
        // The commit released the lease: due again from `next`, not before.
        assert!(
            store
                .push_claim_due(&agents, "w", at, ttl, 1)
                .await
                .unwrap()
                .is_empty()
        );
        let later = next + chrono::Duration::seconds(1);
        let again = store
            .push_claim_due(&agents, "w2", later, ttl, 1)
            .await
            .unwrap();
        assert_eq!(
            again.len(),
            1,
            "released by the commit, due at next_attempt_at"
        );
        assert_eq!(again[0].version, 2);
        assert_eq!(again[0].cursor, committed.cursor);
    }

    pub async fn push_commit_detects_stale_versions_and_missing_configs(store: DynStore) {
        let agent = agent();
        let run = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        store
            .push_put(new_push(run.id, &agent, "c1"))
            .await
            .unwrap();
        let t = now();
        // A stale version conflicts and changes nothing.
        let err = store
            .push_commit(run.id, "c1", 7, progress(PushState::Done, t))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict { run: r, expected: 7, actual: 1 } if r == run.id),
            "{err:?}"
        );
        assert_eq!(
            store.push_list(run.id).await.unwrap()[0].state,
            PushState::Active
        );
        // A replacement beats a deliverer still holding the old version.
        store
            .push_put(new_push(run.id, &agent, "c1"))
            .await
            .unwrap();
        let err = store
            .push_commit(run.id, "c1", 1, progress(PushState::Done, t))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Conflict {
                    expected: 1,
                    actual: 2,
                    ..
                }
            ),
            "{err:?}"
        );
        // A deleted config, and one that never existed, are not found.
        store.push_delete(run.id, "c1").await.unwrap();
        let err = store
            .push_commit(run.id, "c1", 2, progress(PushState::Done, t))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::NotFound(id) if id == run.id),
            "{err:?}"
        );
        let err = store
            .push_commit(RunId::new(), "zz", 1, progress(PushState::Done, t))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)), "{err:?}");
    }

    pub async fn push_configs_go_with_their_run(store: DynStore) {
        // Its `purge_finished` sweeps unreferenced blobs (see `BLOB_SUITE`).
        let _turn = BLOB_SUITE.lock().await;
        let agent = agent();
        let finished = store
            .create_run(NewRun::new(&agent, json!({})).status(RunStatus::Done))
            .await
            .unwrap();
        let open = store
            .create_run(NewRun::new(&agent, json!({})))
            .await
            .unwrap();
        store
            .push_put(new_push(finished.id, &agent, "c1"))
            .await
            .unwrap();
        store
            .push_put(new_push(open.id, &agent, "c1"))
            .await
            .unwrap();
        let cutoff = now() + chrono::Duration::seconds(1);
        assert_eq!(store.purge_finished(&agent, cutoff).await.unwrap(), 1);
        assert!(
            store.push_list(finished.id).await.unwrap().is_empty(),
            "push configs go with the run"
        );
        assert_eq!(store.push_list(open.id).await.unwrap().len(), 1);
        // Nothing of the purged run is claimable either.
        let at = now() + chrono::Duration::seconds(1);
        let claimed = store
            .push_claim_due(
                std::slice::from_ref(&agent),
                "w",
                at,
                Duration::from_secs(60),
                10,
            )
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].run, open.id);
    }

    /// Runs in two owners' conversations (`a:` and `b:`, with a decoy whose prefix differs only by
    /// a wildcard character), each committed at a distinct time, plus a foreign agent's run.
    async fn listing_fixture(store: &DynStore, agent: &str) -> Vec<adam_core::RunRecord> {
        let mut made = Vec::new();
        for (i, conversation) in ["a:1", "b:1", "a:2", "a:3", "b:2", "a:4"]
            .into_iter()
            .enumerate()
        {
            let status = if i % 2 == 0 {
                RunStatus::Parked
            } else {
                RunStatus::Runnable
            };
            let run = store
                .create_run(
                    NewRun::new(agent, json!(i))
                        .conversation(conversation)
                        .status(status),
                )
                .await
                .unwrap();
            // Distinct, increasing update times (milliseconds are the store's resolution).
            tokio::time::sleep(Duration::from_millis(3)).await;
            // A create returns the record without its state; the listing comparisons want the
            // stored one.
            made.push(
                store
                    .load_run(run.id)
                    .await
                    .unwrap()
                    .expect("the created run"),
            );
        }
        // Commits move `updated_at`: the first run becomes the most recently updated. The commit
        // returns the record without its state, so read it back for the listing comparisons.
        let first = made[0].clone();
        store
            .commit_run(
                first.id,
                first.version,
                RunUpdate::new(RunStatus::Done, json!("done")),
            )
            .await
            .unwrap();
        made[0] = store
            .load_run(first.id)
            .await
            .unwrap()
            .expect("the committed run");
        made
    }

    pub async fn list_runs_is_scoped_ordered_and_pages_by_keyset(store: DynStore) {
        let agent = agent();
        let made = listing_fixture(&store, &agent).await;
        // A foreign agent's run in the same conversation namespace is never listed.
        store
            .create_run(NewRun::new(super::agent(), json!(0)).conversation("a:9"))
            .await
            .unwrap();
        let query = |limit| RunQuery::new(&agent, ConversationScope::Prefix("a:".into()), limit);
        let all = store.list_runs(&query(100)).await.unwrap();
        let mut want: Vec<_> = made
            .iter()
            .filter(|r| {
                r.conversation_id
                    .as_deref()
                    .is_some_and(|c| c.starts_with("a:"))
            })
            .cloned()
            .collect();
        want.sort_by_key(|x| std::cmp::Reverse((x.updated_at, x.id)));
        assert_eq!(
            all, want,
            "the owner's runs, newest update first, id breaking ties"
        );
        assert_eq!(all[0].id, made[0].id, "the run committed last is first");
        assert!(all.iter().all(|r| r.agent == agent));

        // Keyset pages of 3 cover the same list, once each, and the last page is short.
        let mut seen: Vec<RunId> = Vec::new();
        let mut after = None;
        loop {
            let mut q = query(3);
            q.after = after;
            let page = store.list_runs(&q).await.unwrap();
            assert!(page.len() <= 3);
            seen.extend(page.iter().map(|r| r.id));
            match page.last() {
                Some(last) if page.len() == 3 => after = Some((last.updated_at, last.id)),
                _ => break,
            }
        }
        assert_eq!(seen, want.iter().map(|r| r.id).collect::<Vec<_>>());
        // A position past the end is empty; a limit of zero returns nothing.
        let mut q = query(10);
        let oldest = want.last().unwrap();
        q.after = Some((oldest.updated_at, oldest.id));
        assert!(store.list_runs(&q).await.unwrap().is_empty());
        assert!(store.list_runs(&query(0)).await.unwrap().is_empty());
        // Another owner's namespace is its own list.
        let b = store
            .list_runs(&RunQuery::new(
                &agent,
                ConversationScope::Prefix("b:".into()),
                100,
            ))
            .await
            .unwrap();
        assert_eq!(b.len(), 2);
        assert!(
            b.iter()
                .all(|r| r.conversation_id.as_deref().unwrap().starts_with("b:"))
        );
    }

    pub async fn list_runs_filters_by_status_and_time(store: DynStore) {
        let agent = agent();
        let made = listing_fixture(&store, &agent).await;
        let scope = || ConversationScope::Prefix("a:".into());
        let mut q = RunQuery::new(&agent, scope(), 100);
        q.statuses = Some(vec![RunStatus::Done]);
        let done = store.list_runs(&q).await.unwrap();
        assert_eq!(
            done.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![made[0].id]
        );
        q.statuses = Some(vec![RunStatus::Runnable, RunStatus::Parked]);
        let open = store.list_runs(&q).await.unwrap();
        assert_eq!(open.len(), 3, "a:2 a:3 a:4 are open; a:1 is done");
        q.statuses = Some(vec![]);
        assert!(
            store.list_runs(&q).await.unwrap().is_empty(),
            "no status at all matches nothing"
        );
        // Updated at or after an instant: the boundary run is included.
        let mut q = RunQuery::new(&agent, scope(), 100);
        let all = store.list_runs(&q).await.unwrap();
        let cut = all[1].updated_at;
        q.updated_since = Some(cut);
        let since = store.list_runs(&q).await.unwrap();
        assert!(since.iter().all(|r| r.updated_at >= cut));
        assert!(since.iter().any(|r| r.id == all[1].id), "at or after");
        q.updated_since = Some(all[0].updated_at + chrono::Duration::seconds(1));
        assert!(store.list_runs(&q).await.unwrap().is_empty());
    }

    /// A prefix listing with a status filter returns the same rows in the same order as the
    /// unfiltered page restricted to that status, and the keyset pages over it the same way.
    /// This is the shape `ListTasks` sends, and the one the adapters index
    /// (`runs_list INCLUDE (status)`, `adam_list_status`).
    pub async fn list_runs_prefix_status_filter_is_ordered_and_paged(store: DynStore) {
        let agent = agent();
        let made = listing_fixture(&store, &agent).await;
        // `made[2]` (`a:2`) and `made[5]` (`a:4`) become the two parked runs of the prefix, in
        // that order, so the filtered page is not creation order: `a:4` is updated last.
        let mut parked = Vec::new();
        for at in [2, 5] {
            parked.push(
                store
                    .commit_run(
                        made[at].id,
                        made[at].version,
                        RunUpdate::new(RunStatus::Parked, json!("still waiting")),
                    )
                    .await
                    .unwrap(),
            );
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        let fields = |r: &adam_core::RunRecord| (r.id, r.status, r.updated_at, r.version);
        let mut want: Vec<_> = parked.iter().map(fields).collect();
        want.sort_by_key(|(_, _, at, _)| std::cmp::Reverse(*at));
        assert_eq!(want.len(), 2, "a:2 and a:4 are parked");

        let mut query = RunQuery::new(&agent, ConversationScope::Prefix("a:".into()), 100);
        query.statuses = Some(vec![RunStatus::Parked]);
        let page = store.list_runs(&query).await.unwrap();
        assert_eq!(
            page.iter().map(fields).collect::<Vec<_>>(),
            want,
            "the status filter keeps the page's order"
        );
        assert_eq!(store.count_runs(&query).await.unwrap(), 2);

        // Keyset pages of one cover the filtered list once each, in order.
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let mut q = query.clone();
            q.limit = 1;
            q.after = after;
            let page = store.list_runs(&q).await.unwrap();
            assert!(page.len() <= 1);
            seen.extend(page.iter().map(fields));
            match page.last() {
                Some(last) if !page.is_empty() && seen.len() < want.len() => {
                    after = Some((last.updated_at, last.id));
                }
                _ => break,
            }
        }
        assert_eq!(seen, want);
    }

    pub async fn list_runs_scopes_match_exactly(store: DynStore) {
        let agent = agent();
        // Prefixes with characters that are special in a pattern or a regular expression must
        // match literally, and an exact scope is one conversation only.
        for conversation in [
            "a.b*c:1", "aXbbc:1", "a.b*c:2", "100%_x:1", "100Zzx:1", "(x|y):1", "xy:1", "ü:1",
            "u:1",
        ] {
            store
                .create_run(NewRun::new(&agent, json!({})).conversation(conversation))
                .await
                .unwrap();
        }
        let count = |scope: ConversationScope| {
            let store = store.clone();
            let agent = agent.clone();
            async move {
                store
                    .list_runs(&RunQuery::new(&agent, scope, 100))
                    .await
                    .unwrap()
                    .len()
            }
        };
        assert_eq!(count(ConversationScope::Prefix("a.b*c:".into())).await, 2);
        assert_eq!(count(ConversationScope::Prefix("100%_x:".into())).await, 1);
        assert_eq!(count(ConversationScope::Prefix("(x|y):".into())).await, 1);
        assert_eq!(count(ConversationScope::Prefix("ü:".into())).await, 1);
        assert_eq!(
            count(ConversationScope::Prefix("".into())).await,
            9,
            "the empty prefix is everything of the agent"
        );
        assert_eq!(count(ConversationScope::Exact("a.b*c:1".into())).await, 1);
        assert_eq!(count(ConversationScope::Exact("a.b*c".into())).await, 0);
        assert_eq!(count(ConversationScope::Exact("nope".into())).await, 0);
    }

    /// A prefix that holds `/` or ends in `/` matches literally: `x:a/s/` covers the conversations
    /// below it and not `x:a/sx`, `x:a%2Fs/` or `x:ab/s/`.
    pub async fn list_runs_prefixes_with_slashes_match_literally(store: DynStore) {
        let agent = agent();
        for conversation in [
            "x:a/s/1",
            "x:a/s/2",
            "x:a/sx",
            "x:a%2Fs/1",
            "x:ab/s/1",
            "x:a/",
            "x:a/s",
            "x:a//s/1",
        ] {
            store
                .create_run(NewRun::new(&agent, json!({})).conversation(conversation))
                .await
                .unwrap();
        }
        let count = |prefix: &'static str| {
            let store = store.clone();
            let agent = agent.clone();
            async move {
                store
                    .list_runs(&RunQuery::new(
                        &agent,
                        ConversationScope::Prefix(prefix.into()),
                        100,
                    ))
                    .await
                    .unwrap()
                    .len()
            }
        };
        assert_eq!(count("x:a/s/").await, 2);
        assert_eq!(count("x:a/").await, 6, "a prefix that ends in a slash");
        assert_eq!(count("x:a%2Fs/").await, 1);
        assert_eq!(count("x:a/s").await, 4);
        assert_eq!(count("x:a//").await, 1);
    }

    /// Two creates with different ids in one agent and conversation leave one open run: the
    /// second is refused with `ConversationBusy`, whichever ids they carry.
    pub async fn two_creates_with_different_ids_in_one_conversation_leave_one_open_run(
        store: DynStore,
    ) {
        let agent = agent();
        let conversation = "x:one/s/open";
        let first = RunId::new();
        store
            .create_run(
                NewRun::new(&agent, json!({}))
                    .with_id(first)
                    .conversation(conversation),
            )
            .await
            .unwrap();
        let second = store
            .create_run(
                NewRun::new(&agent, json!({}))
                    .with_id(RunId::new())
                    .conversation(conversation),
            )
            .await;
        assert!(
            matches!(second, Err(StoreError::ConversationBusy { .. })),
            "{second:?}"
        );
        let open = store
            .open_run_for_conversation(&agent, conversation)
            .await
            .unwrap()
            .expect("an open run");
        assert_eq!(open.id, first);
        let mut query = RunQuery::new(&agent, ConversationScope::Exact(conversation.into()), 10);
        assert_eq!(store.count_runs(&query).await.unwrap(), 1);
        query.statuses = Some(vec![RunStatus::Runnable, RunStatus::Parked]);
        assert_eq!(store.count_runs(&query).await.unwrap(), 1);
    }

    pub async fn count_runs_ignores_the_page(store: DynStore) {
        let agent = agent();
        let made = listing_fixture(&store, &agent).await;
        let mut q = RunQuery::new(&agent, ConversationScope::Prefix("a:".into()), 1);
        assert_eq!(
            store.count_runs(&q).await.unwrap(),
            4,
            "the limit is not the count"
        );
        q.after = Some((made[0].updated_at, made[0].id));
        assert_eq!(
            store.count_runs(&q).await.unwrap(),
            4,
            "neither is the position"
        );
        q.statuses = Some(vec![RunStatus::Parked]);
        let listed = store
            .list_runs(&RunQuery {
                limit: 100,
                after: None,
                ..q.clone()
            })
            .await
            .unwrap();
        assert_eq!(store.count_runs(&q).await.unwrap(), listed.len() as u64);
        assert_eq!(
            store
                .count_runs(&RunQuery::new(
                    &agent,
                    ConversationScope::Exact("b:1".into()),
                    1
                ))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .count_runs(&RunQuery::new(
                    super::agent(),
                    ConversationScope::Prefix("a:".into()),
                    1
                ))
                .await
                .unwrap(),
            0
        );
    }

    async fn claim_ids(
        store: &DynStore,
        agent: &str,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> Vec<RunId> {
        claim_ids_as(store, ClaimScope::Any, agent, worker, now, ttl, limit).await
    }

    async fn claim_ids_as(
        store: &DynStore,
        scope: ClaimScope,
        agent: &str,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> Vec<RunId> {
        store
            .claim_due(&[agent.to_owned()], worker, scope, &[], now, ttl, limit)
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.run.id)
            .collect()
    }

    /// A claim by worker `w` that offers all runs but `busy`.
    async fn claim_ids_busy(
        store: &DynStore,
        scope: ClaimScope,
        agent: &str,
        busy: &[RunId],
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> Vec<RunId> {
        store
            .claim_due(&[agent.to_owned()], "w", scope, busy, now, ttl, limit)
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.run.id)
            .collect()
    }
}

/// A unique agent name, so concurrent test cases never see each other's runs.
pub fn agent() -> String {
    format!("testkit-{}", uuid::Uuid::now_v7().simple())
}
