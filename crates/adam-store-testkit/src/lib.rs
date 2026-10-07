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

pub mod fault;

pub use adam_core::testing::skipped;

use std::collections::HashSet;
use std::time::Duration;

use adam_core::store::{now, truncate_ms};
use adam_core::{
    ClaimScope, DynStore, JournalEntry, NewPushConfig, NewRun, PushProgress, PushState, RunId,
    RunStatus, RunUpdate, StoreError,
};
use chrono::{DateTime, Utc};
use futures::future::join_all;
use serde_json::json;

/// Generate one `#[tokio::test]` per conformance case. `$make` is a path to an
/// `async fn() -> Option<DynStore>`; returning `None` skips the suite (for
/// example when the database URL env var is not set), unless
/// `ADAM_TEST_REQUIRE_DB=1` is set: then every case fails instead (see
/// `adam_core::testing`).
#[macro_export]
macro_rules! store_conformance {
    ($make:path) => {
        $crate::store_conformance!(@cases $make;
            create_and_load, state_roundtrip, nul_characters_roundtrip_or_reject_cleanly,
            create_duplicate_id, load_missing,
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

pub mod cases {
    use super::*;

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

        let loaded = store
            .load_run(created.id)
            .await
            .unwrap()
            .expect("run exists");
        assert_eq!(loaded, created);
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
        assert_eq!(run.state, state);
        let loaded = store.load_run(run.id).await.unwrap().unwrap();
        assert_eq!(loaded.state, state, "state must roundtrip exactly");

        let next = json!({"scalar_state": "just a string"});
        let committed = store
            .commit_run(run.id, 1, RunUpdate::new(RunStatus::Runnable, next.clone()))
            .await
            .unwrap();
        assert_eq!(committed.state, next);
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

    /// Some backends (PostgreSQL JSONB) cannot store `\u0000`. They must say so
    /// with `InvalidInput`, not an opaque backend error, and must not corrupt it.
    pub async fn nul_characters_roundtrip_or_reject_cleanly(store: DynStore) {
        for state in [json!({"text": "a\u{0}b"}), json!({"k\u{0}ey": 1})] {
            match store.create_run(NewRun::new(agent(), state.clone())).await {
                Ok(run) => {
                    assert_eq!(run.state, state);
                    assert_eq!(store.load_run(run.id).await.unwrap().unwrap().state, state);
                }
                Err(StoreError::InvalidInput(_)) => {}
                Err(other) => panic!("expected a roundtrip or InvalidInput, got {other:?}"),
            }
        }
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
        assert_eq!(v2.state, json!({"turn": 1}));
        assert_eq!(v2.wake_at, Some(truncate_ms(wake)));
        assert_eq!(v2.created_at, run.created_at);
        assert!(v2.updated_at >= run.updated_at);
        assert_eq!(store.load_run(run.id).await.unwrap().unwrap(), v2);

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
        let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
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
        let stored = store.load_run(run.id).await.unwrap().unwrap();
        assert_eq!(stored.version, 2);
        assert_eq!(stored.state, winners[0].state);
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
            store.open_run_for_conversation(&agent, "c").await.unwrap(),
            Some(first.clone())
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
        let parked = store
            .commit_run(
                first.id,
                1,
                RunUpdate::new(RunStatus::Parked, json!("waiting")),
            )
            .await
            .unwrap();
        assert_eq!(
            store.open_run_for_conversation(&agent, "c").await.unwrap(),
            Some(parked)
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
                .push_claim_due(&[agent.clone()], "w", at, ttl, 0)
                .await
                .unwrap()
                .is_empty()
        );
        let two = store
            .push_claim_due(&[agent.clone()], "w", at, ttl, 2)
            .await
            .unwrap();
        assert_eq!(two.len(), 2);
        assert!(two.iter().all(|r| r.agent == agent));
        let rest = store
            .push_claim_due(&[agent.clone()], "w", at, ttl, 10)
            .await
            .unwrap();
        assert_eq!(
            rest.len(),
            1,
            "the third; the foreign agent's is never taken"
        );
        let theirs = store
            .push_claim_due(&[other.clone()], "w", at, ttl, 10)
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
            .push_claim_due(&[agent.clone()], "w", at, Duration::from_secs(60), 10)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].run, open.id);
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
