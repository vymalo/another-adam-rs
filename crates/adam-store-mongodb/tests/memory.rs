//! The L2 [`Memory`] port over MongoDB, proven by the conformance suite of ADR 0081 decisions 4, 5
//! and 10.
//!
//! ```sh
//! ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 cargo test -p adam-store-mongodb --features memory
//! ```
//!
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it fails), like the
//! run store's suite. The cases are written out rather than taken from `memory_conformance!`
//! because that macro expands `#[test]`s, and a `#[test]` cannot skip: each one here asks
//! [`memory`] for a store and returns when the variable is unset.
//!
//! Every case gets its own **collection** per process, shared by the two scopes of
//! `two_scopes_never_see_each_other` (they must sit on one store) and thrown away with the
//! process, so a second run starts from an empty store.
#![cfg(feature = "memory")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::OnceLock;

use adam_memory::MemoryScope;
use adam_memory_testkit::{DynMemory, LessonPlan};
use adam_store_mongodb::memory::MongoMemory;

/// The collection every case of this process shares.
fn collection() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_nanos();
        format!("adam_memory_test_{nanos}")
    })
}

/// A memory over the test database for `scope`, or `None` when no MongoDB is configured.
fn memory(plan: &LessonPlan, scope: &MemoryScope) -> Option<DynMemory> {
    let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
    let database = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let client = mongodb::sync::Client::with_uri_str(&uri).expect("a MongoDB client");
    let memory = MongoMemory::new(
        &client.database(&database),
        collection(),
        plan.settings.clone(),
        scope.clone(),
    )
    .expect("a collection");
    Some(std::sync::Arc::new(memory))
}

/// A scope by its key.
fn scope(key: &str) -> MemoryScope {
    MemoryScope::new(key)
}

#[test]
fn record_then_recall_round_trips() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("round-trip")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::record_then_recall_round_trips(m0, plan);
}

#[test]
fn a_record_lands_once_for_one_key() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("one-key")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_record_lands_once_for_one_key(m0, plan);
}

#[test]
fn recall_answers_only_its_audience() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("audiences")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::recall_answers_only_its_audience(m0, plan);
}

#[test]
fn an_audience_is_not_scrubbed() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("audience-key")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::an_audience_is_not_scrubbed(m0, plan);
}

#[test]
fn two_scopes_never_see_each_other() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("scope-a")) else {
        return;
    };
    let Some(m1) = memory(&plan, &scope("scope-b")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::two_scopes_never_see_each_other(m0, m1, plan);
}

#[test]
fn a_text_is_scrubbed_before_the_write() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("scrubbed")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_text_is_scrubbed_before_the_write(m0, plan);
}

#[test]
fn a_text_carrying_the_marker_is_refused() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("marker")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_text_carrying_the_marker_is_refused(m0, plan);
}

#[test]
fn a_field_over_the_bound_is_refused() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("field-bound")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_field_over_the_bound_is_refused(m0, plan);
}

#[test]
fn a_root_cause_over_the_bound_is_refused() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("root-cause-bound")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_root_cause_over_the_bound_is_refused(m0, plan);
}

#[test]
fn a_full_audience_refuses_another_lesson() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("audience-bound")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_full_audience_refuses_another_lesson(m0, plan);
}

#[test]
fn a_lesson_refuses_an_address() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("addresses")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_lesson_refuses_an_address(m0, plan);
}

#[test]
fn a_retired_lesson_is_kept_and_not_served() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("retired")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_retired_lesson_is_kept_and_not_served(m0, plan);
}

#[test]
fn a_full_audience_refuses_a_revived_lesson() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("revive-bound")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::a_full_audience_refuses_a_revived_lesson(m0, plan);
}

#[test]
fn retire_takes_the_stored_root_cause() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("retire-key")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::retire_takes_the_stored_root_cause(m0, plan);
}

#[test]
fn sixteen_writers_share_one_audience() {
    let plan = LessonPlan::default();
    let Some(m0) = memory(&plan, &scope("writers")) else {
        return;
    };
    adam_memory_testkit::lesson_cases::sixteen_writers_share_one_audience(m0, plan);
}
