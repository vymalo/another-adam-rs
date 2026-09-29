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
use adam_core::{DynStore, JournalEntry, NewRun, RunId, RunStatus, RunUpdate, StoreError};
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
            claim_respects_due_rules, claim_filters_agents_and_limit,
            claim_is_exclusive_under_concurrency, lease_expiry_allows_takeover,
            renew_and_release_lease, conversation_single_open_run,
            conversation_race_single_winner, purge_finished_runs,
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
            .claim_due(std::slice::from_ref(&a), "w", now, ttl, 3)
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
            .claim_due(&[a.clone(), b.clone()], "w", now, ttl, 100)
            .await
            .unwrap();
        assert_eq!(rest.len(), 2 + 5);
        assert!(
            store
                .claim_due(&[], "w", now, ttl, 100)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .claim_due(std::slice::from_ref(&a), "w", now, ttl, 0)
                .await
                .unwrap()
                .is_empty()
        );
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

    async fn claim_ids(
        store: &DynStore,
        agent: &str,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> Vec<RunId> {
        store
            .claim_due(&[agent.to_owned()], worker, now, ttl, limit)
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
