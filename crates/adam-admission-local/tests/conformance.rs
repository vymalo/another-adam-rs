//! `LocalAdmission` against the conformance suite, plus the cases of the card: twenty runs under a
//! limit of twenty, a wait that frees in time, a wait that runs out, and the token bucket.

use std::sync::Arc;
use std::time::Duration;

use adam_admission_local::LocalAdmission;
use adam_admission_testkit::Plan;
use adam_budget::{Admission, Ask, DynAdmission, Pool, Ratio, Refused};
use adam_error::Classify;

/// The admission `plan` describes.
fn make(plan: &Plan) -> DynAdmission {
    Arc::new(LocalAdmission::new(plan.budget()))
}

adam_admission_testkit::admission_conformance!(crate::make);

#[tokio::test]
async fn twenty_runs_fill_a_limit_of_twenty_and_never_a_twenty_first() {
    let plan = Plan::new(Pool::Runs, 20);
    let admission = make(&plan);
    let mut held = Vec::new();
    for i in 0..20 {
        held.push(
            admission
                .admit(Ask::new(Pool::Runs, format!("talk-{i}")))
                .await
                .expect("one of the twenty"),
        );
    }
    let refused = admission
        .admit(Ask::new(Pool::Runs, "stranger"))
        .await
        .expect_err("the twenty-first is not admitted");
    match &refused {
        Refused::Busy { pool, retry_after } => {
            assert_eq!(*pool, Pool::Runs);
            assert!(retry_after.is_some(), "a refusal says when to ask again");
        }
        other => panic!("a full pool says Busy, not {other}"),
    }
    assert_eq!(refused.class(), adam_error::ErrorClass::RateLimited);
    // One run ends: the next is admitted.
    held.pop();
    let _last = admission
        .admit(Ask::new(Pool::Runs, "stranger"))
        .await
        .expect("a freed run is a room for one more");
    assert_eq!(
        admission.snapshot().pool(Pool::Runs).map(|p| p.in_use()),
        Some(20)
    );
}

#[tokio::test]
async fn an_ask_that_waits_is_admitted_when_room_frees_in_time() {
    let plan = Plan::new(Pool::ModelCalls, 1);
    let admission = make(&plan);
    let held = admission
        .admit(Ask::new(Pool::ModelCalls, "first"))
        .await
        .expect("an empty pool admits one");
    let waiter = {
        let admission = Arc::clone(&admission);
        let ask = Ask::new(Pool::ModelCalls, "second").wait(Duration::from_secs(5));
        tokio::spawn(async move { admission.admit(ask).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        admission
            .snapshot()
            .pool(Pool::ModelCalls)
            .map(|p| p.waiting()),
        Some(1),
        "the ask waits"
    );
    drop(held);
    let permit = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("a waiting ask is woken")
        .expect("the task does not panic")
        .expect("room freed in time");
    assert_eq!(permit.pool(), Pool::ModelCalls);
}

#[tokio::test]
async fn an_ask_over_the_pool_limit_can_never_fit() {
    let plan = Plan::new(Pool::ToolCalls, 2);
    let admission = make(&plan);
    let refused = admission
        .admit(Ask::new(Pool::ToolCalls, "big").units(3))
        .await
        .expect_err("three of a pool of two never fits, however long it waits");
    assert!(
        matches!(
            refused,
            Refused::TooLarge {
                asked: 3,
                limit: 2,
                ..
            }
        ),
        "{refused}"
    );
    assert_eq!(refused.class(), adam_error::ErrorClass::Invalid);
    assert!(!refused.is_retryable());
}

#[tokio::test]
async fn one_conversation_takes_half_of_a_pool_of_twenty() {
    let plan = Plan::new(Pool::ModelCalls, 20).share(Ratio::HALF);
    let admission = make(&plan);
    let mut mine = Vec::new();
    for i in 0..10 {
        mine.push(
            admission
                .admit(Ask::new(Pool::ModelCalls, "mine"))
                .await
                .unwrap_or_else(|e| panic!("seat {i}: {e}")),
        );
    }
    assert!(
        admission
            .admit(Ask::new(Pool::ModelCalls, "mine"))
            .await
            .is_err(),
        "a conversation takes no more than its share, though the pool has room"
    );
    let other = admission
        .admit(Ask::new(Pool::ModelCalls, "theirs"))
        .await
        .expect("the other conversation's share is free");
    assert_eq!(other.units(), 1);
}

#[tokio::test]
async fn the_token_bucket_meters_a_model_call() {
    let plan = Plan::new(Pool::ModelCalls, 2).tokens_per_minute(1_000);
    let admission = make(&plan);
    assert!(
        adam_admission_testkit::the_token_bucket_meters_what_it_cannot_refill(
            Arc::clone(&admission),
            plan
        )
        .await,
        "the plan meters tokens"
    );
    let usage = admission.snapshot();
    let tokens = usage.tokens().expect("a metered budget reports its bucket");
    assert_eq!(tokens.limit(), 1_000);
    assert!(tokens.available() < 1_000);
}

#[tokio::test]
async fn a_budget_without_a_token_rate_reports_no_bucket() {
    let admission = make(&Plan::new(Pool::ModelCalls, 2));
    assert!(admission.snapshot().tokens().is_none());
    // A call that names tokens is admitted anyway: nothing meters them.
    let permit = admission
        .admit(Ask::new(Pool::ModelCalls, "free").tokens(1_000_000))
        .await
        .expect("no bucket, nothing to charge");
    drop(permit);
}
