//! The conformance suite of [`adam_budget::Admission`], and the [`Plan`] a test asks with.
//!
//! [`admission_conformance!`] writes the cases of ADR 0065 decision 3 into a module of the
//! implementing crate:
//!
//! * never over a limit under many concurrent asks;
//! * a dropped holder gives its share back;
//! * a cancelled ask gives its share back;
//! * one conversation never holds more than its share of a pool, while another waits;
//! * `snapshot` adds up;
//! * a refusal that may pass names a `retry_after`.
//!
//! Each case is a function here, so the suite is compiled once and the macro only names it.
//!
//! ```text
//! use std::sync::Arc;
//!
//! use adam_admission_testkit::Plan;
//! use adam_budget::DynAdmission;
//!
//! fn make(plan: &Plan) -> DynAdmission {
//!     Arc::new(LocalAdmission::new(plan.budget())) // LocalAdmission from adam-admission-local
//! }
//!
//! adam_admission_testkit::admission_conformance!(crate::make);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use adam_budget::{Admission, Ask, Budget, DynAdmission, Permit, Pool, Ratio, Refused, Usage};
use adam_error::Classify;

/// How many asks the concurrency case launches at once.
pub const ASKS: usize = 100;

/// How long a case waits for a task's effect to show in a snapshot, at most.
const SETTLE: Duration = Duration::from_secs(5);

/// What one conformance run asks with: which pool, its ceiling, one conversation's share of it and
/// whether the tokens of a model call are metered.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    /// The pool the cases admit from.
    pub pool: Pool,
    /// Its ceiling, in the pool's unit. At most [`ASKS`], so the concurrency case can fill it.
    pub limit: u32,
    /// The most one conversation holds.
    pub share: Ratio,
    /// Tokens per minute of a model call, when the implementation meters them.
    pub tokens_per_minute: Option<u32>,
}

impl Plan {
    /// A plan for `limit` of `pool`, half of it to one conversation, no token rate.
    ///
    /// # Panics
    ///
    /// When `limit` is more than [`ASKS`]: the concurrency case could not fill it.
    #[must_use]
    pub fn new(pool: Pool, limit: u32) -> Self {
        assert!(
            limit <= ASKS as u32,
            "a conformance plan's limit is at most {ASKS}"
        );
        assert!(limit > 0, "a conformance plan's limit is at least one");
        Self {
            pool,
            limit,
            share: Ratio::HALF,
            tokens_per_minute: None,
        }
    }

    /// The most one conversation may hold.
    #[must_use]
    pub fn share(mut self, share: Ratio) -> Self {
        self.share = share;
        self
    }

    /// Meter `tokens` per minute.
    #[must_use]
    pub fn tokens_per_minute(mut self, tokens: u32) -> Self {
        self.tokens_per_minute = Some(tokens);
        self
    }

    /// The budget these cases count against: `pool` at `limit`, one conversation at `share`, and
    /// the token rate when the plan names one. Every other pool is [`Budget::UNLIMITED`].
    #[must_use]
    pub fn budget(&self) -> Budget {
        let budget = Budget::builder()
            .pool_limit(self.pool, self.limit)
            .per_conversation_share(self.share);
        match self.tokens_per_minute {
            Some(tokens) => budget.tokens_per_minute(tokens).build(),
            None => budget.build(),
        }
    }

    fn ask(&self) -> Ask {
        Ask::new(self.pool, ConversationId::new("conformance"))
    }
}

// `Pool` is re-exported for the macro's expansions, which name it in the implementing crate.
pub use adam_budget::ConversationId;

/// Fill the pool, holding every permit: `plan.limit` asks of one unit at once, each from its own
/// conversation so that the share does not bind.
///
/// # Panics
///
/// When the implementation refuses one of the `limit` asks, or admits more than `limit`.
pub async fn fill(admission: &DynAdmission, plan: &Plan) -> Vec<Permit> {
    let mut permits = Vec::new();
    for i in 0..plan.limit {
        let ask = Ask::new(plan.pool, ConversationId::new(format!("fill-{i}")));
        permits.push(
            admission
                .admit(ask)
                .await
                .unwrap_or_else(|e| panic!("{} of {} admitted: {e}", i, plan.limit)),
        );
    }
    permits
}

/// The pool is never over its limit, under [`ASKS`] concurrent asks.
///
/// # Panics
///
/// When more than `plan.limit` asks are admitted, when fewer are, or when the snapshot of what is
/// held disagrees.
pub async fn never_over_the_limit(admission: DynAdmission, plan: Plan) {
    let barrier = Arc::new(tokio::sync::Barrier::new(ASKS));
    let mut tasks = Vec::with_capacity(ASKS);
    for i in 0..ASKS {
        let admission = Arc::clone(&admission);
        let barrier = Arc::clone(&barrier);
        let pool = plan.pool;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            admission
                .admit(Ask::new(pool, ConversationId::new(format!("ask-{i}"))))
                .await
        }));
    }
    let mut held = Vec::new();
    let mut refusals = 0;
    for task in tasks {
        match task.await.expect("no ask panics") {
            Ok(permit) => held.push(permit),
            Err(e) => {
                refusals += 1;
                assert!(
                    matches!(e, Refused::Busy { .. } | Refused::TooLarge { .. }),
                    "an ask over a full pool waits or is refused: {e}"
                );
            }
        }
        assert!(
            held.len() as u32 <= plan.limit,
            "{} held of a limit of {}",
            held.len(),
            plan.limit
        );
    }
    assert_eq!(held.len() as u32, plan.limit, "the pool fills exactly");
    assert_eq!(refusals, ASKS - plan.limit as usize);
    assert_eq!(
        held_in_use(admission.snapshot(), plan.pool),
        u64::from(plan.limit),
        "the snapshot of a full pool"
    );
    drop(held);
    assert_eq!(
        held_in_use(admission.snapshot(), plan.pool),
        0,
        "a dropped permit is given back"
    );
}

/// A dropped permit gives its share back at once.
///
/// # Panics
///
/// When a permit is not released by dropping it.
pub async fn a_dropped_holder_frees_its_share(admission: DynAdmission, plan: Plan) {
    let mut held = fill(&admission, &plan).await;
    assert!(
        admission.admit(plan.ask()).await.is_err(),
        "a full pool refuses without waiting"
    );
    held.pop().expect("a permit to drop");
    let permit = admission
        .admit(plan.ask())
        .await
        .expect("a dropped permit frees its share");
    assert_eq!(
        held_in_use(admission.snapshot(), plan.pool),
        u64::from(plan.limit)
    );
    drop(permit);
}

/// A task cancelled while it holds a permit gives its share back.
///
/// # Panics
///
/// When the share of a cancelled holder stays held.
pub async fn a_cancelled_holder_frees_its_share(admission: DynAdmission, plan: Plan) {
    let mut held = fill(&admission, &plan).await;
    held.pop().expect("a seat for the cancelled holder");
    let (said, heard) = tokio::sync::oneshot::channel();
    let holder = {
        let admission = Arc::clone(&admission);
        let ask = plan.ask();
        tokio::spawn(async move {
            let _permit = admission.admit(ask).await.expect("the freed seat is taken");
            let _ = said.send(());
            // Held until the task is aborted, as a run cut short holds its permits.
            std::future::pending::<()>().await;
        })
    };
    heard.await.expect("the holder took the seat");
    holder.abort();
    let _ = holder.await;
    let permit = tokio::time::timeout(SETTLE, async { admission.admit(plan.ask()).await })
        .await
        .expect("a cancelled holder frees its share within the settle time")
        .expect("the cancelled holder's share is free");
    drop(permit);
}

/// One conversation never holds more than its share of a pool, while another waits.
///
/// # Panics
///
/// When a conversation holds more than `plan.share` of the pool, or when another conversation
/// cannot take the rest.
pub async fn a_conversation_keeps_its_share(admission: DynAdmission, plan: Plan) {
    // A pool of at least four, so that half of it is a share a second conversation can be held
    // out of.
    let limit = plan.limit.max(4);
    let plan = Plan::new(plan.pool, limit).share(plan.share);
    let share = plan.share.apply_u32(limit).max(1);
    let mut first = Vec::new();
    for _ in 0..share {
        first.push(
            admission
                .admit(Ask::new(plan.pool, ConversationId::new("talkative")))
                .await
                .expect("the conversation's share is free"),
        );
    }
    let mut second = Vec::new();
    for _ in 0..share {
        second.push(
            admission
                .admit(Ask::new(plan.pool, ConversationId::new("patient")))
                .await
                .expect("the other conversation's share is free"),
        );
    }
    // The pool is full of two shares; the first conversation asks for one more and may not have it
    // while the other holds its own.
    let refused = admission
        .admit(Ask::new(plan.pool, ConversationId::new("talkative")))
        .await;
    assert!(
        refused.is_err(),
        "one conversation held more than its share while another waited"
    );
    assert_eq!(
        held_in_use(admission.snapshot(), plan.pool),
        u64::from(limit),
        "the pool is full"
    );
    drop(first);
    let again = admission
        .admit(Ask::new(plan.pool, ConversationId::new("talkative")))
        .await
        .expect("room freed by the same conversation is its own again");
    drop((again, second));
}

/// A snapshot adds up: what it says is held is what the permits hold, and a waiting ask is
/// counted while it waits.
///
/// # Panics
///
/// When the report disagrees with the permits.
pub async fn a_snapshot_adds_up(admission: DynAdmission, plan: Plan) {
    let held = fill(&admission, &plan).await;
    let usage = admission.snapshot();
    let line = usage
        .pool(plan.pool)
        .unwrap_or_else(|| panic!("the report lists {}", plan.pool));
    assert_eq!(line.in_use(), u64::from(plan.limit));
    assert_eq!(line.limit(), u64::from(plan.limit));
    // A second ask of the same conversation waits: the report counts it while it waits.
    let waiter = {
        let admission = Arc::clone(&admission);
        let ask = Ask::new(plan.pool, ConversationId::new("waiter")).wait(Duration::from_secs(30));
        tokio::spawn(async move { admission.admit(ask).await })
    };
    wait_until(&admission, plan.pool, 1).await;
    waiter.abort();
    let _ = waiter.await;
    drop(held);
    wait_until(&admission, plan.pool, 0).await;
    let usage: Usage = admission.snapshot();
    assert_eq!(held_in_use(usage, plan.pool), 0);
}

/// A refusal that may pass says when to ask again, and its class is `RateLimited`.
///
/// # Panics
///
/// When a full pool refuses without a `retry_after`, or with a class a caller does not park on.
pub async fn a_refusal_names_a_retry_after(admission: DynAdmission, plan: Plan) {
    let held = fill(&admission, &plan).await;
    let refused = admission
        .admit(plan.ask())
        .await
        .expect_err("a full pool refuses");
    assert_eq!(
        refused.class(),
        adam_error::ErrorClass::RateLimited,
        "{refused}"
    );
    assert!(
        refused
            .retry_after()
            .is_some_and(|wait| wait > Duration::ZERO),
        "{refused}"
    );
    assert!(refused.is_retryable());
    drop(held);
}

/// The token bucket refuses what it cannot refill in the ask's wait, and says when it can.
///
/// Returns whether the plan meters tokens: with none there is nothing to prove, and the case is a
/// no-op.
///
/// # Panics
///
/// When a metered bucket admits more than its rate, or refuses without saying when to come back.
pub async fn the_token_bucket_meters_what_it_cannot_refill(
    admission: DynAdmission,
    plan: Plan,
) -> bool {
    let Some(tokens) = plan.tokens_per_minute else {
        return false;
    };
    // A model call, whatever pool the plan names: the bucket is the process's one.
    let ask = Ask::new(Pool::ModelCalls, ConversationId::new("reader")).tokens(tokens);
    let permit = admission
        .admit(ask.clone())
        .await
        .expect("a full bucket admits one minute's tokens");
    drop(permit);
    let refused = admission
        .admit(ask)
        .await
        .expect_err("an empty bucket refuses at once");
    match refused {
        Refused::TokensPerMinute { retry_after } => {
            assert!(retry_after > Duration::ZERO, "the wait is named");
        }
        other => panic!("an empty bucket says TokensPerMinute, not {other}"),
    }
    assert_eq!(
        refused.class(),
        adam_error::ErrorClass::RateLimited,
        "{refused}"
    );
    assert!(refused.retry_after().is_some_and(|d| d > Duration::ZERO));
    true
}

/// What the report says is held in `pool`.
fn held_in_use(usage: Usage, pool: Pool) -> u64 {
    usage
        .pool(pool)
        .map_or(u64::MAX, adam_budget::PoolUsage::in_use)
}

/// Wait until the report says `target` asks of `pool` are waiting, at most [`SETTLE`].
async fn wait_until(admission: &DynAdmission, pool: Pool, target: u32) {
    let started = std::time::Instant::now();
    loop {
        let waiting = admission
            .snapshot()
            .pool(pool)
            .map_or(0, adam_budget::PoolUsage::waiting);
        if waiting == target {
            return;
        }
        assert!(
            started.elapsed() < SETTLE,
            "the report said {waiting} waiting for {pool}, not {target}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Writes the conformance cases of ADR 0065 decision 3 into a `mod admission_conformance` of the
/// crate that calls it, for the admission `$make` builds from a [`Plan`].
///
/// `$make` is a path to a `fn(&Plan) -> DynAdmission`; in a test file of a crate, that is
/// `crate::make_admission`. The calling crate needs `tokio` with `macros`, `rt-multi-thread` and
/// `time`, and the tests run on the multi-threaded runtime the macro names.
///
/// The token case runs only when the plan meters tokens; an implementation that meters them calls
/// it with one (`the_token_bucket_meters_what_it_cannot_refill`) or passes a plan that does.
#[macro_export]
macro_rules! admission_conformance {
    ($make:path) => {
        #[allow(missing_docs)]
        mod admission_conformance {
            use super::*;
            use $crate::Plan;

            fn make(plan: &Plan) -> $crate::DynAdmission {
                $make(plan)
            }

            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn never_over_the_limit() {
                $crate::never_over_the_limit(make(&Plan::default()), Plan::default()).await;
            }

            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_dropped_holder_frees_its_share() {
                $crate::a_dropped_holder_frees_its_share(make(&Plan::default()), Plan::default())
                    .await;
            }

            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_cancelled_holder_frees_its_share() {
                $crate::a_cancelled_holder_frees_its_share(make(&Plan::default()), Plan::default())
                    .await;
            }

            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_conversation_keeps_its_share() {
                $crate::a_conversation_keeps_its_share(make(&Plan::default()), Plan::default())
                    .await;
            }

            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_snapshot_adds_up() {
                $crate::a_snapshot_adds_up(make(&Plan::default()), Plan::default()).await;
            }

            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_refusal_names_a_retry_after() {
                $crate::a_refusal_names_a_retry_after(make(&Plan::default()), Plan::default())
                    .await;
            }

            #[::tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn the_token_bucket_meters_what_it_cannot_refill() {
                let plan = Plan::default();
                let metered = plan.tokens_per_minute.is_some();
                let proved = $crate::the_token_bucket_meters_what_it_cannot_refill(
                    make(&plan),
                    plan.clone(),
                )
                .await;
                assert_eq!(
                    proved, metered,
                    "the token case runs exactly when the plan meters one"
                );
            }
        }
    };
}

impl Default for Plan {
    /// The pool of tool calls, four at once, half of them to one conversation, no token rate.
    fn default() -> Self {
        Self::new(Pool::ToolCalls, 4)
    }
}
