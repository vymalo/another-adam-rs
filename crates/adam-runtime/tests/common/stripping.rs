//! A store that answers journal entries with their signature removed, as a store that dropped the
//! column — or a writer that nulled one — would: one `seq` alone, or every entry. Everything else
//! forwards.
//!
//! It is what a replay sees when an attacker strips a signature to make a signed entry look like
//! one written before the feature (ADR 0068 decisions 5 and 11). The runtime must refuse an entry
//! whose run holds a signed one, and adopt a journal whose entries are **all** unsigned.

#![allow(dead_code)] // one test binary includes this module

use std::time::Duration;

use adam_core::time::Timestamp;
use adam_core::{
    BlobId, BodyRow, ClaimScope, DynStore, JournalEntry, Lease, NewPushConfig, NewRun,
    PushProgress, PushRecord, RunHeader, RunId, RunQuery, RunRecord, RunUpdate, Store, StoreResult,
};
use async_trait::async_trait;
use uuid::Uuid;

/// See the module docs.
pub struct Stripping {
    inner: DynStore,
    /// The seq whose signature is removed; `None` removes every one.
    seq: Option<u64>,
    /// Remove the signature from what `journal_put` answers too, as a store without the column
    /// would: the write path must refuse it rather than keep an entry a replay would adopt.
    on_write: bool,
}

impl Stripping {
    /// Remove the signature of `seq`, or of every entry when `seq` is `None`.
    pub fn new(inner: DynStore, seq: Option<u64>) -> Self {
        Self {
            inner,
            seq,
            on_write: false,
        }
    }

    /// Also strip what `journal_put` answers.
    pub fn on_write(mut self) -> Self {
        self.on_write = true;
        self
    }

    fn strip(&self, entry: JournalEntry) -> JournalEntry {
        if self.seq.is_none_or(|seq| seq == entry.seq) {
            JournalEntry {
                signature: None,
                ..entry
            }
        } else {
            entry
        }
    }
}

#[async_trait]
impl Store for Stripping {
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
        Ok(self
            .inner
            .journal_get(run, seq)
            .await?
            .map(|entry| self.strip(entry)))
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        let stored = self.inner.journal_put(run, entry).await?;
        Ok(if self.on_write {
            self.strip(stored)
        } else {
            stored
        })
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        Ok(self
            .inner
            .journal_list(run)
            .await?
            .into_iter()
            .map(|entry| self.strip(entry))
            .collect())
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

    async fn release_worker_leases(&self, worker: &str) -> StoreResult<()> {
        self.inner.release_worker_leases(worker).await
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

    async fn activity(
        &self,
        agents: &[String],
        now: adam_core::Timestamp,
    ) -> StoreResult<adam_core::StoreActivity> {
        self.inner.activity(agents, now).await
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
        self.inner.get_blob(id).await
    }

    async fn purge_blobs(&self, before: Timestamp) -> StoreResult<u64> {
        self.inner.purge_blobs(before).await
    }
}
