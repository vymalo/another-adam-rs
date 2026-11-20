//! A store that moves a run's head between the first `load_head` and the `body_list` that follows,
//! so a test can exercise [`Runtime::state`](adam_runtime::Runtime::state)'s re-read.
//!
//! The wrapper is armed with the run's current head record and the generation that head names. On
//! its first `load_head` of that run it either commits a **compaction** (a new generation with the
//! old rows, a new head, one version up) and collects the old generation, or it collects the old
//! generation alone; either way it returns the **old** record, so the `body_list` the caller makes
//! next finds the set short. The second `load_head` passes through to the inner store.
//!
//! [`MovingHead::appending`] is the other direction: it **appends** to the generation on each of
//! the first two `load_head` calls and answers the record from before the append, so the
//! `body_list` the caller makes next finds **more** rows than the head names — a later commit,
//! which must not be read as a corrupt head.

#![allow(dead_code)] // one test binary includes this module

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use adam_core::time::Timestamp;
use adam_core::{
    BlobId, BodyRow, BodyWrite, ClaimScope, DynStore, JournalEntry, Lease, NewPushConfig, NewRun,
    PushProgress, PushRecord, RunHeader, RunId, RunQuery, RunRecord, RunUpdate, Store, StoreResult,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;

/// See the module docs.
pub struct MovingHead {
    inner: DynStore,
    run: RunId,
    /// The record the first `load_head` answers, and the generation its head names.
    first: Mutex<Option<(RunRecord, Uuid)>>,
    /// Whether the first `load_head` commits a compaction (a new generation) before collecting the
    /// old one. `false`: it collects the old one and leaves the head where it was.
    compact: bool,
    /// `Some(n)`: append `n` rows to the current generation on each of the first two `load_head`
    /// calls, answering the record from before the append (see [`MovingHead::appending`]).
    grow: Option<usize>,
    loads: AtomicUsize,
}

impl MovingHead {
    pub fn new(
        inner: DynStore,
        run: RunId,
        first: RunRecord,
        generation: Uuid,
        compact: bool,
    ) -> Self {
        Self {
            inner,
            run,
            first: Mutex::new(Some((first, generation))),
            compact,
            grow: None,
            loads: AtomicUsize::new(0),
        }
    }

    /// A wrapper that **appends** `extra` rows to the run's current generation on each of its first
    /// two `load_head` calls (a new head each time, one version up), and answers the record from
    /// before that append. The `body_list` the caller makes after each `load_head` therefore finds
    /// more rows than the head names: a commit that landed between the two reads.
    pub fn appending(inner: DynStore, run: RunId, extra: usize) -> Self {
        Self {
            inner,
            run,
            first: Mutex::new(None),
            compact: false,
            grow: Some(extra),
            loads: AtomicUsize::new(0),
        }
    }

    /// How many `load_head` calls the wrapper saw for its run (the first and the re-read).
    pub fn loads(&self) -> usize {
        self.loads.load(SeqCst)
    }

    fn first(&self) -> Option<(RunRecord, Uuid)> {
        self.first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Append `extra` rows to the current generation, then answer the record from before it.
    async fn grow(&self, id: RunId, extra: usize) -> StoreResult<Option<RunRecord>> {
        let before = self.inner.load_head(id).await?.expect("the run");
        let generation = generation_of(&before);
        let mut rows = self.inner.body_list(id, generation).await?;
        let start = rows.len();
        for i in 0..extra {
            let idx = u32::try_from(start + i).expect("a short transcript");
            rows.push(BodyRow {
                idx,
                message: json!(format!("appended {idx}")),
            });
        }
        let len = u32::try_from(rows.len()).expect("a short transcript");
        let head = json!({
            "v": 5,
            "agent": before.state["agent"].clone(),
            "body": {"generation": null, "len": len},
        });
        let state = json!({
            "v": 1,
            "agent": before.state["agent"].clone(),
            "inbox": [], "seq": 0, "attempt": 0, "rev": 0,
            "output": null, "error": null, "artifacts": [],
        });
        self.inner
            .commit_run(
                id,
                before.version,
                RunUpdate::new(before.status, state)
                    .head(head)
                    .body(BodyWrite {
                        generation: None,
                        rows,
                        writer: Uuid::new_v4(),
                    }),
            )
            .await?;
        Ok(Some(before))
    }
}

#[async_trait]
impl Store for MovingHead {
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
        if id != self.run {
            return self.inner.load_head(id).await;
        }
        let call = self.loads.fetch_add(1, SeqCst);
        if let Some(extra) = self.grow {
            if call > 1 {
                return self.inner.load_head(id).await;
            }
            return self.grow(id, extra).await;
        }
        if call > 0 {
            return self.inner.load_head(id).await;
        }
        let Some((first, generation)) = self.first() else {
            return self.inner.load_head(id).await;
        };
        let cutoff = Timestamp::now() + jiff::SignedDuration::from_hours(1);
        if self.compact {
            let rec = self.inner.load_run(id).await?.expect("the run");
            let rows = self.inner.body_list(id, generation).await?;
            let head = json!({
                "v": 5,
                "agent": first.state["agent"].clone(),
                "body": {"generation": null, "len": rows.len()},
            });
            // The v4 state beside the head is not read by `Runtime::state`; a minimal envelope is
            // enough for the store, which only needs it to be JSON.
            let state = json!({
                "v": 1,
                "agent": first.state["agent"].clone(),
                "inbox": [], "seq": 0, "attempt": 0, "rev": 0,
                "output": null, "error": null, "artifacts": [],
            });
            self.inner
                .commit_run(
                    id,
                    rec.version,
                    RunUpdate::new(rec.status, state)
                        .head(head)
                        .body(BodyWrite {
                            generation: Some(Uuid::new_v4()),
                            rows,
                            writer: Uuid::new_v4(),
                        }),
                )
                .await?;
            // Keep the new generation and collect the old one, so the head this call answers
            // names a set that is gone.
            let current = self.inner.load_head(id).await?.expect("the run");
            let new_generation = Uuid::parse_str(
                current.state["body"]["generation"]
                    .as_str()
                    .expect("the store resolved the generation"),
            )
            .expect("a uuid");
            self.inner.gc_body(id, new_generation, cutoff).await?;
        } else {
            // Keep no generation: the head still names the old one, which is now gone.
            self.inner.gc_body(id, Uuid::nil(), cutoff).await?;
        }
        Ok(Some(first))
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

/// The generation a head record names.
pub fn generation_of(rec: &RunRecord) -> Uuid {
    Uuid::parse_str(
        rec.state["body"]["generation"]
            .as_str()
            .expect("the head names a generation"),
    )
    .expect("a uuid")
}

/// A state's messages, as the tests read them.
pub fn messages(state: &Value) -> Vec<Value> {
    state["messages"].as_array().cloned().unwrap_or_default()
}
