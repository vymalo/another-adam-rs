//! A store wrapper that records every state the runtime writes and every successful commit's
//! [`RunUpdate`], so a test can assert what the shadow wrote without reading it back.
//!
//! Only successful writes are recorded: a commit that lost the CAS wrote nothing, and recording it
//! would make "every state written" a lie.

#![allow(dead_code)] // one test binary includes this module

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use adam_core::{
    BlobId, BodyRow, ClaimScope, DynStore, JournalEntry, Lease, NewPushConfig, NewRun,
    PushProgress, PushRecord, RunHeader, RunId, RunQuery, RunRecord, RunUpdate, Store, StoreResult,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// One write the runtime made, in order.
#[derive(Clone, Debug)]
pub enum Written {
    /// A run's initial state ([`Store::create_run`]).
    Created(Value),
    /// A successful commit's whole update: the state, the head and the body.
    Committed(RunUpdate),
}

/// Wraps a store and keeps what the runtime wrote through it.
pub struct RecordingStore {
    inner: DynStore,
    written: Mutex<Vec<Written>>,
    /// Every `commit_run` update, successful or not: a CAS retry shows the same attempt twice.
    attempts: Mutex<Vec<RunUpdate>>,
    /// When set, the next `commit_run` answers `Conflict` without writing (a forced CAS retry).
    conflict_next: AtomicBool,
    /// Whether the wrapper reports that it keeps blobs (the inner store still does).
    keeps_blobs: bool,
}

impl RecordingStore {
    pub fn new(inner: DynStore) -> Self {
        Self {
            inner,
            written: Mutex::new(Vec::new()),
            attempts: Mutex::new(Vec::new()),
            conflict_next: AtomicBool::new(false),
            keeps_blobs: true,
        }
    }

    /// Report that this store keeps no blobs, as a store without the blob methods would.
    pub fn without_blobs(mut self) -> Self {
        self.keeps_blobs = false;
        self
    }

    /// Everything written since the last [`reset`](Self::reset), in order.
    pub fn written(&self) -> Vec<Written> {
        self.written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The successful commits' updates, in order.
    pub fn commits(&self) -> Vec<RunUpdate> {
        self.written()
            .into_iter()
            .filter_map(|written| match written {
                Written::Committed(update) => Some(update),
                Written::Created(_) => None,
            })
            .collect()
    }

    /// Every `state` written: a create's initial state and every successful commit's state.
    pub fn states(&self) -> Vec<Value> {
        self.written()
            .into_iter()
            .map(|written| match written {
                Written::Created(state) => state,
                Written::Committed(update) => update.state,
            })
            .collect()
    }

    /// Every commit attempt's update, in order, including the ones that lost.
    pub fn attempts(&self) -> Vec<RunUpdate> {
        self.attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Make the next `commit_run` answer `Conflict` without writing: the runtime retries the same
    /// attempt, which must reuse its body writer.
    pub fn conflict_next_commit(&self) {
        self.conflict_next.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn record(&self, written: Written) {
        self.written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(written);
    }
}

#[async_trait]
impl Store for RecordingStore {
    async fn migrate(&self) -> StoreResult<()> {
        self.inner.migrate().await
    }

    async fn create_run(&self, run: NewRun) -> StoreResult<RunRecord> {
        let state = run.state.clone();
        let record = self.inner.create_run(run).await?;
        self.record(Written::Created(state));
        Ok(record)
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.inner.load_run(id).await
    }

    async fn load_head(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.inner.load_head(id).await
    }

    async fn run_header(&self, id: RunId) -> StoreResult<Option<RunHeader>> {
        self.inner.run_header(id).await
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected_version: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        self.attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(update.clone());
        if self.conflict_next.swap(false, Ordering::SeqCst) {
            return Err(adam_core::StoreError::Conflict {
                run: id,
                expected: expected_version,
                actual: expected_version,
            });
        }
        let recorded = update.clone();
        let record = self.inner.commit_run(id, expected_version, update).await?;
        self.record(Written::Committed(recorded));
        Ok(record)
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        self.inner
            .open_run_for_conversation(agent, conversation_id)
            .await
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        self.inner.journal_get(run, seq).await
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        self.inner.journal_put(run, entry).await
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        self.inner.journal_list(run).await
    }

    async fn claim_due(
        &self,
        agents: &[String],
        worker: &str,
        scope: ClaimScope,
        busy: &[RunId],
        now: DateTime<Utc>,
        ttl: std::time::Duration,
        limit: usize,
    ) -> StoreResult<Vec<Lease>> {
        self.inner
            .claim_due(agents, worker, scope, busy, now, ttl, limit)
            .await
    }

    async fn renew_lease(
        &self,
        id: RunId,
        worker: &str,
        now: DateTime<Utc>,
        ttl: std::time::Duration,
    ) -> StoreResult<bool> {
        self.inner.renew_lease(id, worker, now, ttl).await
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        self.inner.release_lease(id, worker).await
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<DateTime<Utc>>> {
        self.inner.lease_until(id).await
    }

    async fn list_runs(&self, query: &RunQuery) -> StoreResult<Vec<RunRecord>> {
        self.inner.list_runs(query).await
    }

    async fn count_runs(&self, query: &RunQuery) -> StoreResult<u64> {
        self.inner.count_runs(query).await
    }

    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord> {
        self.inner.push_put(new).await
    }

    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>> {
        self.inner.push_list(run).await
    }

    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool> {
        self.inner.push_delete(run, id).await
    }

    async fn push_claim_due(
        &self,
        agents: &[String],
        worker: &str,
        now: DateTime<Utc>,
        ttl: std::time::Duration,
        limit: usize,
    ) -> StoreResult<Vec<PushRecord>> {
        self.inner
            .push_claim_due(agents, worker, now, ttl, limit)
            .await
    }

    async fn push_commit(
        &self,
        run: RunId,
        id: &str,
        expected_version: u64,
        progress: PushProgress,
    ) -> StoreResult<PushRecord> {
        self.inner
            .push_commit(run, id, expected_version, progress)
            .await
    }

    async fn purge_finished(&self, agent: &str, before: DateTime<Utc>) -> StoreResult<u64> {
        self.inner.purge_finished(agent, before).await
    }

    fn keeps_bodies(&self) -> bool {
        self.inner.keeps_bodies()
    }

    async fn body_list(&self, run: RunId, generation: Uuid) -> StoreResult<Vec<BodyRow>> {
        self.inner.body_list(run, generation).await
    }

    fn keeps_blobs(&self) -> bool {
        self.keeps_blobs
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        self.inner.put_blob(id, bytes).await
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        self.inner.get_blob(id).await
    }

    async fn purge_blobs(&self, before: DateTime<Utc>) -> StoreResult<u64> {
        self.inner.purge_blobs(before).await
    }
}
