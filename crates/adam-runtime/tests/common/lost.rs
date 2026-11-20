//! A store whose `body_list` answers a generation's real rows **plus** a fake row at the head's
//! `len`: the extra row a commit that lost its CAS leaves behind until its `delete_loser` runs (or
//! the orphan grace collects it). The head names a prefix, so a reader must ignore it.
//!
//! It can also drop one real row from the prefix, so a test can prove that a generation longer than
//! the head does not hide a hole in the prefix.

#![allow(dead_code)] // one test binary includes this module

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_core::time::Timestamp;
use adam_core::{
    BlobId, BodyRow, ClaimScope, DynStore, JournalEntry, Lease, NewPushConfig, NewRun,
    PushProgress, PushRecord, RunHeader, RunId, RunQuery, RunRecord, RunUpdate, Store, StoreResult,
};
use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

/// See the module docs.
pub struct LostRows {
    inner: DynStore,
    run: RunId,
    /// The generation the head names; other generations pass through untouched.
    generation: Uuid,
    /// The head's `len`: rows at or past it are a lost attempt's.
    len: u32,
    /// The message of the fake row written at `len`.
    message: Value,
    /// Drop this row (below `len`) from the answer: a hole in the head's prefix.
    missing: Option<u32>,
    lists: AtomicUsize,
}

impl LostRows {
    pub fn new(inner: DynStore, run: RunId, generation: Uuid, len: u32, message: Value) -> Self {
        Self {
            inner,
            run,
            generation,
            len,
            message,
            missing: None,
            lists: AtomicUsize::new(0),
        }
    }

    /// Also drop the row at `idx` (below the head's `len`) from every answer.
    pub fn with_missing(mut self, idx: u32) -> Self {
        self.missing = Some(idx);
        self
    }

    /// How many `body_list` calls the wrapper saw for its run (one, or two when the reader re-read
    /// a head that did not move).
    pub fn lists(&self) -> usize {
        self.lists.load(SeqCst)
    }
}

#[async_trait]
impl Store for LostRows {
    async fn migrate(&self) -> StoreResult<()> {
        self.inner.migrate().await
    }

    async fn create_run(&self, run: NewRun) -> StoreResult<RunRecord> {
        self.inner.create_run(run).await
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
        self.inner.commit_run(id, expected_version, update).await
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
        now: Timestamp,
        ttl: Duration,
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
        now: Timestamp,
        ttl: Duration,
    ) -> StoreResult<bool> {
        self.inner.renew_lease(id, worker, now, ttl).await
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        self.inner.release_lease(id, worker).await
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<Timestamp>> {
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
        now: Timestamp,
        ttl: Duration,
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

    async fn purge_finished(&self, agent: &str, before: Timestamp) -> StoreResult<u64> {
        self.inner.purge_finished(agent, before).await
    }

    fn keeps_bodies(&self) -> bool {
        self.inner.keeps_bodies()
    }

    async fn body_list(&self, run: RunId, generation: Uuid) -> StoreResult<Vec<BodyRow>> {
        let rows = self.inner.body_list(run, generation).await?;
        if run != self.run || generation != self.generation {
            return Ok(rows);
        }
        self.lists.fetch_add(1, SeqCst);
        let mut rows: Vec<BodyRow> = rows
            .into_iter()
            .filter(|row| Some(row.idx) != self.missing)
            .collect();
        rows.push(BodyRow {
            idx: self.len,
            message: self.message.clone(),
        });
        Ok(rows)
    }

    async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: Timestamp,
    ) -> StoreResult<u64> {
        self.inner.gc_body(run, keep_generation, before).await
    }

    fn keeps_blobs(&self) -> bool {
        self.inner.keeps_blobs()
    }

    async fn blob_stored(&self, id: &BlobId) -> StoreResult<bool> {
        self.inner.blob_stored(id).await
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        self.inner.put_blob(id, bytes).await
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        self.inner.get_blob(id).await
    }

    async fn purge_blobs(&self, before: Timestamp) -> StoreResult<u64> {
        self.inner.purge_blobs(before).await
    }
}

/// A store whose `get_blob` answers `None` for one digest and forwards the rest: an inherited
/// file's bytes that are gone, so a refused split's demotion must not write the state as
/// zero-byte files (it leaves the run as it is instead).
pub struct LostBlob {
    inner: DynStore,
    hidden: BlobId,
}

impl LostBlob {
    pub fn new(inner: DynStore, hidden: BlobId) -> Self {
        Self { inner, hidden }
    }
}

#[async_trait]
impl Store for LostBlob {
    async fn migrate(&self) -> StoreResult<()> {
        self.inner.migrate().await
    }

    async fn create_run(&self, run: NewRun) -> StoreResult<RunRecord> {
        self.inner.create_run(run).await
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
        self.inner.commit_run(id, expected_version, update).await
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
        now: Timestamp,
        ttl: Duration,
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
        now: Timestamp,
        ttl: Duration,
    ) -> StoreResult<bool> {
        self.inner.renew_lease(id, worker, now, ttl).await
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        self.inner.release_lease(id, worker).await
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<Timestamp>> {
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
        now: Timestamp,
        ttl: Duration,
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

    async fn purge_finished(&self, agent: &str, before: Timestamp) -> StoreResult<u64> {
        self.inner.purge_finished(agent, before).await
    }

    fn keeps_bodies(&self) -> bool {
        self.inner.keeps_bodies()
    }

    async fn body_list(&self, run: RunId, generation: Uuid) -> StoreResult<Vec<BodyRow>> {
        self.inner.body_list(run, generation).await
    }

    async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: Timestamp,
    ) -> StoreResult<u64> {
        self.inner.gc_body(run, keep_generation, before).await
    }

    fn keeps_blobs(&self) -> bool {
        self.inner.keeps_blobs()
    }

    async fn blob_stored(&self, id: &BlobId) -> StoreResult<bool> {
        self.inner.blob_stored(id).await
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        self.inner.put_blob(id, bytes).await
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        if id == &self.hidden {
            return Ok(None);
        }
        self.inner.get_blob(id).await
    }

    async fn purge_blobs(&self, before: Timestamp) -> StoreResult<u64> {
        self.inner.purge_blobs(before).await
    }
}
