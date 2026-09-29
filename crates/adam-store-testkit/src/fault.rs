//! [`FaultyStore`]: a [`Store`] wrapper that fails on demand, to test what
//! callers do when the database misbehaves.
//!
//! The conformance cases prove a store is correct while it works. This proves
//! its callers stay correct while it does not: a claim that errors, a commit
//! whose acknowledgement is lost, a lease renewal that never succeeds. Faults
//! are scripted per method and counted, so a test states exactly "the next 5
//! claims fail" and asserts afterwards that they did.
//!
//! ```
//! # tokio_test_block(async {
//! use std::sync::Arc;
//! use adam_core::{MemoryStore, NewRun, Store};
//! use adam_store_testkit::fault::{FaultyStore, Method};
//!
//! let store = FaultyStore::new(Arc::new(MemoryStore::new()));
//! store.fail(Method::CreateRun, 1);
//! assert!(store.create_run(NewRun::new("a", serde_json::json!({}))).await.is_err());
//! assert!(store.create_run(NewRun::new("a", serde_json::json!({}))).await.is_ok());
//! assert_eq!(store.injected(Method::CreateRun), 1);
//! # });
//! # fn tokio_test_block(f: impl std::future::Future<Output = ()>) {
//! #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
//! # }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use adam_core::{
    DynStore, JournalEntry, Lease, NewRun, RunId, RunRecord, RunUpdate, Store, StoreError,
    StoreResult,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// A [`Store`] method a fault can target. `migrate` is not injectable: a
/// store that cannot migrate never gets as far as running anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    /// [`Store::create_run`].
    CreateRun,
    /// [`Store::load_run`].
    LoadRun,
    /// [`Store::commit_run`].
    CommitRun,
    /// [`Store::open_run_for_conversation`].
    OpenRunForConversation,
    /// [`Store::journal_get`].
    JournalGet,
    /// [`Store::journal_put`].
    JournalPut,
    /// [`Store::journal_list`].
    JournalList,
    /// [`Store::claim_due`].
    ClaimDue,
    /// [`Store::renew_lease`].
    RenewLease,
    /// [`Store::release_lease`].
    ReleaseLease,
    /// [`Store::purge_finished`].
    PurgeFinished,
}

/// When, relative to the real operation, a fault strikes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The operation is not performed; the caller sees the error. The
    /// database is unreachable, or rejected the statement.
    Before,
    /// The operation *is* performed, then the caller sees an error anyway: a
    /// connection dropped after the write but before the acknowledgement. The
    /// nastiest case for a caller, which cannot know whether it happened.
    After,
}

#[derive(Clone, Copy, Debug)]
struct Rule {
    /// Faults left; `None` fails every call until healed.
    remaining: Option<u64>,
    mode: Mode,
}

#[derive(Default)]
struct Plan {
    rules: HashMap<Method, Rule>,
    calls: HashMap<Method, u64>,
    injected: HashMap<Method, u64>,
}

/// The error a fault produces: a [`StoreError::Backend`] whose message starts
/// with `injected store fault`.
#[derive(Debug)]
struct Injected(Method);

impl fmt::Display for Injected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "injected store fault in {:?}", self.0)
    }
}

impl std::error::Error for Injected {}

/// Whether `err` was produced by a [`FaultyStore`] fault (and not by the
/// wrapped store).
pub fn is_injected(err: &StoreError) -> bool {
    matches!(err, StoreError::Backend(e) if e.is::<Injected>())
}

/// Wraps a store and fails scripted calls with [`StoreError::Backend`]; every
/// other call passes through untouched.
///
/// Cloning is not offered: share it as `Arc<FaultyStore>` (which also coerces
/// to a [`DynStore`]) so the test keeps a handle to script faults while the
/// code under test holds the same store.
pub struct FaultyStore {
    inner: DynStore,
    plan: Mutex<Plan>,
}

impl FaultyStore {
    /// Wrap `inner`; no faults are scripted yet.
    pub fn new(inner: DynStore) -> Self {
        Self {
            inner,
            plan: Mutex::default(),
        }
    }

    fn plan(&self) -> MutexGuard<'_, Plan> {
        self.plan.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fail the next `n` calls of `method` before they reach the store
    /// ([`Mode::Before`]). Replaces any earlier script for that method.
    pub fn fail(&self, method: Method, n: u64) {
        self.script(method, Some(n), Mode::Before);
    }

    /// Fail every call of `method` until [`heal`](Self::heal).
    pub fn fail_always(&self, method: Method) {
        self.script(method, None, Mode::Before);
    }

    /// Perform the next `n` calls of `method`, then report each as failed
    /// ([`Mode::After`]): the write happened, the acknowledgement was lost.
    pub fn fail_after_apply(&self, method: Method, n: u64) {
        self.script(method, Some(n), Mode::After);
    }

    fn script(&self, method: Method, remaining: Option<u64>, mode: Mode) {
        self.plan().rules.insert(method, Rule { remaining, mode });
    }

    /// Stop failing `method`.
    pub fn heal(&self, method: Method) {
        self.plan().rules.remove(&method);
    }

    /// Stop failing every method. Counters are kept.
    pub fn heal_all(&self) {
        self.plan().rules.clear();
    }

    /// How many calls of `method` have been made (failed ones included).
    pub fn calls(&self, method: Method) -> u64 {
        self.plan().calls.get(&method).copied().unwrap_or(0)
    }

    /// How many calls of `method` were failed by a fault.
    pub fn injected(&self, method: Method) -> u64 {
        self.plan().injected.get(&method).copied().unwrap_or(0)
    }

    /// Count the call and decide whether it fails, and how.
    fn strike(&self, method: Method) -> Option<Mode> {
        let mut plan = self.plan();
        *plan.calls.entry(method).or_default() += 1;
        let rule = plan.rules.get_mut(&method)?;
        let mode = rule.mode;
        match &mut rule.remaining {
            Some(0) => {
                plan.rules.remove(&method);
                return None;
            }
            Some(n) => *n -= 1,
            None => {}
        }
        *plan.injected.entry(method).or_default() += 1;
        Some(mode)
    }

    /// Run `op` on the inner store under the script for `method`.
    async fn run<T>(
        &self,
        method: Method,
        op: impl std::future::Future<Output = StoreResult<T>>,
    ) -> StoreResult<T> {
        match self.strike(method) {
            None => op.await,
            Some(Mode::Before) => Err(StoreError::backend(Injected(method))),
            Some(Mode::After) => {
                let _ = op.await;
                Err(StoreError::backend(Injected(method)))
            }
        }
    }
}

impl fmt::Debug for FaultyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultyStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl Store for FaultyStore {
    async fn migrate(&self) -> StoreResult<()> {
        self.inner.migrate().await
    }

    async fn create_run(&self, run: NewRun) -> StoreResult<RunRecord> {
        self.run(Method::CreateRun, self.inner.create_run(run))
            .await
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.run(Method::LoadRun, self.inner.load_run(id)).await
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected_version: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        self.run(
            Method::CommitRun,
            self.inner.commit_run(id, expected_version, update),
        )
        .await
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        self.run(
            Method::OpenRunForConversation,
            self.inner.open_run_for_conversation(agent, conversation_id),
        )
        .await
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        self.run(Method::JournalGet, self.inner.journal_get(run, seq))
            .await
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        self.run(Method::JournalPut, self.inner.journal_put(run, entry))
            .await
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        self.run(Method::JournalList, self.inner.journal_list(run))
            .await
    }

    async fn claim_due(
        &self,
        agents: &[String],
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> StoreResult<Vec<Lease>> {
        self.run(
            Method::ClaimDue,
            self.inner.claim_due(agents, worker, now, ttl, limit),
        )
        .await
    }

    async fn renew_lease(
        &self,
        id: RunId,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> StoreResult<bool> {
        self.run(
            Method::RenewLease,
            self.inner.renew_lease(id, worker, now, ttl),
        )
        .await
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        self.run(Method::ReleaseLease, self.inner.release_lease(id, worker))
            .await
    }

    async fn purge_finished(&self, agent: &str, before: DateTime<Utc>) -> StoreResult<u64> {
        self.run(
            Method::PurgeFinished,
            self.inner.purge_finished(agent, before),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_core::{MemoryStore, RunStatus};
    use serde_json::json;

    use super::*;

    fn faulty() -> Arc<FaultyStore> {
        Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())))
    }

    async fn create(store: &FaultyStore) -> StoreResult<RunRecord> {
        store.create_run(NewRun::new("fault-test", json!({}))).await
    }

    #[tokio::test]
    async fn fails_exactly_n_calls_then_passes_through() {
        let store = faulty();
        store.fail(Method::CreateRun, 2);
        for _ in 0..2 {
            let err = create(&store).await.expect_err("scripted fault");
            assert!(is_injected(&err), "{err}");
            assert!(err.to_string().contains("injected store fault"));
        }
        assert!(create(&store).await.is_ok());
        assert!(create(&store).await.is_ok());
        assert_eq!(store.calls(Method::CreateRun), 4);
        assert_eq!(store.injected(Method::CreateRun), 2);
    }

    #[tokio::test]
    async fn faults_are_per_method() {
        let store = faulty();
        store.fail_always(Method::LoadRun);
        let run = create(&store).await.expect("create is untouched");
        assert!(store.load_run(run.id).await.is_err());
        assert!(store.load_run(run.id).await.is_err());
        store.heal(Method::LoadRun);
        assert!(store.load_run(run.id).await.expect("healed").is_some());
        assert_eq!(store.injected(Method::CreateRun), 0);
        assert_eq!(store.injected(Method::LoadRun), 2);
    }

    #[tokio::test]
    async fn a_failed_call_before_is_not_applied() {
        let store = faulty();
        let run = create(&store).await.expect("create");
        store.fail(Method::CommitRun, 1);
        let update = RunUpdate::new(RunStatus::Done, json!({"n": 1}));
        assert!(store.commit_run(run.id, 1, update.clone()).await.is_err());
        let loaded = store.load_run(run.id).await.expect("load").expect("run");
        assert_eq!(loaded.version, 1, "the commit never reached the store");
        assert_eq!(loaded.status, RunStatus::Runnable);
        // The same commit works afterwards.
        store.commit_run(run.id, 1, update).await.expect("commit");
    }

    #[tokio::test]
    async fn a_lost_acknowledgement_is_applied_but_reported_as_failed() {
        let store = faulty();
        let run = create(&store).await.expect("create");
        store.fail_after_apply(Method::CommitRun, 1);
        let update = RunUpdate::new(RunStatus::Done, json!({"n": 1}));
        let err = store
            .commit_run(run.id, 1, update.clone())
            .await
            .expect_err("reported as failed");
        assert!(is_injected(&err));
        let loaded = store.load_run(run.id).await.expect("load").expect("run");
        assert_eq!(loaded.version, 2, "the write happened");
        assert_eq!(loaded.status, RunStatus::Done);
        // Retrying with the old version now conflicts, like a real lost ack.
        assert!(matches!(
            store.commit_run(run.id, 1, update).await,
            Err(StoreError::Conflict { .. })
        ));
    }

    #[tokio::test]
    async fn heal_all_stops_every_fault_and_keeps_the_counters() {
        let store = faulty();
        store.fail_always(Method::CreateRun);
        store.fail_always(Method::ClaimDue);
        assert!(create(&store).await.is_err());
        store.heal_all();
        assert!(create(&store).await.is_ok());
        let claimed = store
            .claim_due(
                &["fault-test".to_owned()],
                "w",
                adam_core::store::now(),
                Duration::from_secs(5),
                1,
            )
            .await
            .expect("claim passes through");
        assert_eq!(claimed.len(), 1);
        assert_eq!(store.injected(Method::CreateRun), 1);
    }

    #[tokio::test]
    async fn injected_errors_are_distinguishable_from_real_ones() {
        let store = faulty();
        let real = store.load_run(RunId::new()).await;
        assert!(matches!(real, Ok(None)));
        let err = store
            .commit_run(RunId::new(), 1, RunUpdate::new(RunStatus::Done, json!({})))
            .await
            .expect_err("no such run");
        assert!(!is_injected(&err), "{err}");
    }
}
