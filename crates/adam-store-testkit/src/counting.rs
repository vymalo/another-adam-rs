//! [`CountingStore`]: a [`Store`] wrapper that counts calls and the bytes of the states that cross
//! it, so a test can assert what a read or a commit costs.
//!
//! [`FaultyStore`](crate::fault::FaultyStore) proves callers stay correct while the store fails;
//! this measures what they ask it to do while it works. The question it answers is the one a
//! transcript asks: how much does one more commit write, and how much does one read transfer? It
//! counts calls per [`Method`] and sums the `serde_json` bytes of the `state` of every
//! [`RunRecord`] the store returns, and of every document a successful commit writes: its `state`,
//! its head and the body rows it appends. A
//! record returned without its state (`create_run` and `commit_run` echo none) counts zero, and so
//! does a state that is `null` (the contract's tombstone). A store's own encoding (JSONB, BSON) may
//! differ by a few bytes; a bound is about the ratio, not the encoding.
//!
//! ```
//! # tokio_test_block(async {
//! use std::sync::Arc;
//! use adam_core::{MemoryStore, NewRun, RunStatus, RunUpdate, Store};
//! use adam_store_testkit::counting::CountingStore;
//! use adam_store_testkit::fault::Method;
//! use serde_json::json;
//!
//! let store = CountingStore::new(Arc::new(MemoryStore::new()));
//! let run = store.create_run(NewRun::new("a", json!({}))).await.unwrap();
//! store.reset();
//! store
//!     .commit_run(run.id, 1, RunUpdate::new(RunStatus::Done, json!({"n": 1})))
//!     .await
//!     .unwrap();
//! store.load_run(run.id).await.unwrap();
//! assert_eq!(store.calls(Method::CommitRun), 1);
//! assert_eq!(store.calls(Method::LoadRun), 1);
//! assert!(store.state_bytes_written() > 0);
//! assert!(store.state_bytes_read() > 0);
//! # });
//! # fn tokio_test_block(f: impl std::future::Future<Output = ()>) {
//! #     tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
//! # }
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use adam_core::time::Timestamp;
use adam_core::{
    BlobId, ClaimScope, DynStore, JournalEntry, Lease, NewPushConfig, NewRun, PushProgress,
    PushRecord, RunHeader, RunId, RunQuery, RunRecord, RunStatus, RunUpdate, Store, StoreResult,
};
use async_trait::async_trait;
use jiff::SignedDuration;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::fault::Method;

#[derive(Default)]
struct Counts {
    calls: HashMap<Method, u64>,
    state_bytes_read: u64,
    state_bytes_written: u64,
    commit_bytes: u64,
    max_commit_bytes: u64,
    /// The `body.len` of the last head each run's successful commit wrote: the rows a later append
    /// adds are the ones past it.
    body_len: HashMap<RunId, u32>,
}

/// Wraps a store and counts calls and state bytes; every call passes through untouched.
///
/// Cloning is not offered: share it as `Arc<CountingStore>` (which also coerces to a [`DynStore`])
/// so the test keeps a handle to read the counters while the code under test holds the same store.
pub struct CountingStore {
    inner: DynStore,
    counts: Mutex<Counts>,
}

impl CountingStore {
    /// Wrap `inner`; the counters start at zero.
    pub fn new(inner: DynStore) -> Self {
        Self {
            inner,
            counts: Mutex::default(),
        }
    }

    fn counts(&self) -> MutexGuard<'_, Counts> {
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many calls of `method` the wrapped store has seen since the last
    /// [`reset`](Self::reset). `migrate` is not counted: a store that cannot migrate never gets as
    /// far as running anything, the same rule as [`FaultyStore`](crate::fault::FaultyStore).
    pub fn calls(&self, method: Method) -> u64 {
        self.counts().calls.get(&method).copied().unwrap_or(0)
    }

    /// The bytes of the `state` of every [`RunRecord`] returned since the last reset: what a
    /// reader transferred, not what it kept. A record a store returns without its state (a
    /// `create_run` or `commit_run` echo) adds nothing, and so does a `null` state.
    ///
    /// A header is the blind spot: [`Store::run_header`] is forwarded, and an adapter that
    /// overrides it answers in one small read with no state. One whose `run_header` is the trait
    /// default reads the record *inside* the adapter, which this wrapper cannot see, so it counts
    /// neither that `load_run` nor its state. Measure reads with `load_run`/`load_head`, never
    /// `run_header`.
    pub fn state_bytes_read(&self) -> u64 {
        self.counts().state_bytes_read
    }

    /// The bytes of [`RunUpdate::state`] written by the commits that **succeeded** since the last
    /// reset. A commit that failed (a stale version, a lost connection) wrote no state and is not
    /// counted.
    ///
    /// The contract release writes the tombstone for a store that keeps bodies, so this is `0` for
    /// those commits; [`commit_bytes`](Self::commit_bytes) is what they wrote.
    pub fn state_bytes_written(&self) -> u64 {
        self.counts().state_bytes_written
    }

    /// The bytes of the documents a **succeeded** commit handed the store since the last reset: its
    /// `state`, its head, and the body rows it appends (the rows after the last head's `len`, or
    /// all of them for a new generation). What an adapter persists for one commit, in serialized
    /// `serde_json` bytes.
    pub fn commit_bytes(&self) -> u64 {
        self.counts().commit_bytes
    }

    /// The largest commit (as [`commit_bytes`](Self::commit_bytes) counts one) since the last
    /// reset: the last state of a transcript, when every commit carries the whole thing. It is the
    /// serialized `serde_json` size, not the bytes an adapter wrote to disk.
    pub fn max_commit_bytes(&self) -> u64 {
        self.counts().max_commit_bytes
    }

    /// Zero every counter, keeping the wrapped store. Reset only between measurements: a counter
    /// read while a call is in flight sees a partial count, and a reset then loses that call's
    /// bytes.
    pub fn reset(&self) {
        *self.counts() = Counts::default();
    }

    fn call(&self, method: Method) {
        *self.counts().calls.entry(method).or_default() += 1;
    }

    fn read(&self, record: &RunRecord) {
        self.counts().state_bytes_read += state_bytes(&record.state);
    }

    fn reads<'a>(&self, records: impl IntoIterator<Item = &'a RunRecord>) {
        let bytes: u64 = records
            .into_iter()
            .map(|record| state_bytes(&record.state))
            .sum();
        self.counts().state_bytes_read += bytes;
    }

    fn wrote(&self, state: u64, total: u64) {
        let mut counts = self.counts();
        counts.state_bytes_written += state;
        counts.commit_bytes += total;
        counts.max_commit_bytes = counts.max_commit_bytes.max(total);
    }
}

fn state_bytes(state: &Value) -> u64 {
    // A record a store returns without its state carries `null` (the create and commit echo):
    // that is no state transferred, not the four bytes of the word.
    if state.is_null() {
        return 0;
    }
    serde_json::to_vec(state)
        .expect("a serde_json::Value always serializes")
        .len() as u64
}

/// The `body.len` a v5 head names, whether or not the store has resolved its generation yet (the
/// runtime leaves `generation` `null` and the store fills it).
fn head_len(head: &Value) -> Option<u32> {
    u32::try_from(head.get("body")?.get("len")?.as_u64()?).ok()
}

/// What one [`measure_transcript_write`] run measured, in serialized `serde_json` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    /// How many commits succeeded (50 when the workload ran to the end).
    pub commits: usize,
    /// The sum of every successful commit's persisted documents: its state, its head and the body
    /// rows it appended ([`CountingStore::commit_bytes`]).
    pub total_written: u64,
    /// The largest commit, counted as [`total_written`](Self::total_written) counts one.
    pub max_commit: u64,
    /// What the `load_run` after the first commit transferred: the head for a store that keeps
    /// bodies (the state is the tombstone).
    pub first_read: u64,
    /// What the `load_run` after the last commit transferred.
    pub final_read: u64,
}

impl Measurement {
    /// How many commits [`measure_transcript_write`] makes: the workload's one place for the
    /// number, so a test's `commits` assertion cannot drift from it.
    pub const COMMITS: usize = 50;

    /// The one line `dev/p2-measure.sh` parses and a test prints:
    /// `p2-measure: commits=50 total_written=13290322 max_commit=521202 first_read=10434 final_read=521202`.
    /// The contract release prints the same fields with the head-and-delta numbers (the pre-contract
    /// line above is S0's baseline).
    pub fn report(&self) -> String {
        format!(
            "p2-measure: commits={} total_written={} max_commit={} first_read={} final_read={}",
            self.commits, self.total_written, self.max_commit, self.first_read, self.final_read
        )
    }
}

impl fmt::Display for Measurement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report())
    }
}

/// The bytes of each message [`measure_transcript_write`] appends.
const MESSAGE_BYTES: usize = 10 * 1024;

/// Measure what a transcript-shaped run costs **in the contract release**: one run,
/// [`Measurement::COMMITS`] commits, each appending a 10 KB message, the last one leaving the run
/// [`RunStatus::Done`], and the run purged afterwards so repeated measurements do not accumulate.
///
/// Each commit writes the v5 shape the runtime writes for a store that keeps bodies: the tombstone
/// as its `state`, a small head, and the body rows — of which the store appends only the delta
/// (the rows after the current head's `len`), so the cost is linear, not quadratic. The counter
/// ([`CountingStore::commit_bytes`]) is what an adapter persists for one commit.
///
/// **This is a measurement, not a conformance case.** It asserts nothing; the
/// `transcript_measurement!` test prints [`Measurement::report`] and asserts the S0 bound
/// (`total < 50 × 20 KB`, `max < 20 KB`). The pre-contract baseline, on Memory, PostgreSQL and
/// MongoDB (identical state JSON): `p2-measure: commits=50 total_written=13290322
/// max_commit=521202 first_read=10434 final_read=521202` — 12× over the bound. The disk side is
/// `dev/p2-measure.sh`; both numbers are recorded in
/// [ADR 0055](../../../docs/decisions/0055-a-runs-state-is-a-head-and-an-append-only-transcript.md).
///
/// # Panics
///
/// When the store errors: a measurement that cannot run is a broken test, not a number.
pub async fn measure_transcript_write(store: DynStore) -> Measurement {
    // It purges the measured run at the end, and that call sweeps unreferenced blobs too: take the
    // suite's lock so a concurrent case's blob is not collected (see `crate::BLOB_SUITE`).
    let _turn = crate::BLOB_SUITE.lock().await;
    let store = CountingStore::new(store);
    let agent = crate::agent();
    let run = store
        .create_run(NewRun::new(&agent, json!({"messages": []})))
        .await
        .expect("create the measured run");

    let mut messages: Vec<Value> = Vec::with_capacity(Measurement::COMMITS);
    let mut version = run.version;
    let mut commits = 0;
    let mut first_read = 0;
    for turn in 0..Measurement::COMMITS {
        messages.push(json!({
            "role": "assistant",
            "text": message_text(MESSAGE_BYTES, turn as u64),
        }));
        let len = u32::try_from(messages.len()).expect("a short transcript");
        let status = if turn + 1 == Measurement::COMMITS {
            RunStatus::Done
        } else {
            RunStatus::Runnable
        };
        // The shape the runtime commits for a body store: the tombstone, a head naming the whole
        // transcript (`generation: null` lets the store resolve the current one), and the rows —
        // the store appends the delta after the head it has.
        let head = json!({
            "v": 5,
            "agent": {"turns": turn + 1},
            "body": {"generation": null, "len": len},
        });
        let rows: Vec<adam_core::BodyRow> = messages
            .iter()
            .enumerate()
            .map(|(idx, message)| adam_core::BodyRow {
                idx: u32::try_from(idx).expect("a short transcript"),
                message: message.clone(),
            })
            .collect();
        let update = RunUpdate::new(status, Value::Null)
            .head(head)
            .body(adam_core::BodyWrite {
                generation: None,
                rows,
                writer: Uuid::new_v4(),
            });
        version = store
            .commit_run(run.id, version, update)
            .await
            .expect("commit the measured state")
            .version;
        commits += 1;
        if turn == 0 {
            first_read = read_state_bytes(&store, run.id).await;
        }
    }
    let final_read = read_state_bytes(&store, run.id).await;
    let measured = Measurement {
        commits,
        total_written: store.commit_bytes(),
        max_commit: store.max_commit_bytes(),
        first_read,
        final_read,
    };

    // The last commit left the run `Done`: purge it now that the numbers are taken, so a suite that
    // runs the measurement repeatedly does not accumulate a 500 KB run per adapter per measurement.
    store
        .purge_finished(
            &agent,
            adam_core::store::now() + jiff::SignedDuration::from_mins(1),
        )
        .await
        .expect("purge the measured run");

    measured
}

/// The steps of [`measure_headless_transcript`]: 50, as S0's workload.
pub const HEADLESS_STEPS: usize = 50;

/// What one [`measure_headless_transcript`] run measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadlessMeasurement {
    /// How many steps ran (50 when the workload ran to the end).
    pub steps: usize,
    /// The body rows the store wrote, summed over the steps: the full transcript each step.
    pub rows: u64,
    /// The serialized bytes of those rows.
    pub bytes: u64,
    /// How many body generations the store created: one per step, because a headless commit makes
    /// the next split a compaction.
    pub generations: u64,
}

impl HeadlessMeasurement {
    /// The one line the test prints: `p2-headless: steps=50 rows=1275 bytes=13288347
    /// generations=50`.
    pub fn report(&self) -> String {
        format!(
            "p2-headless: steps={} rows={} bytes={} generations={}",
            self.steps, self.rows, self.bytes, self.generations
        )
    }
}

impl fmt::Display for HeadlessMeasurement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report())
    }
}

/// Measure the **headless** path: a delivery-shaped commit (no head) between every split, so each
/// split sees a stale head and starts a new body generation with the whole transcript.
///
/// This is the cost the one-line "resolve the generation from the head regardless" would hide:
/// with a headless commit between steps, the body is quadratic again until S3, where `deliver`,
/// `cancel` and `park_again` write the head they read. The workload is
/// [`HEADLESS_STEPS`] steps of a 10 KB message, the last one `Done`, and the run is purged.
///
/// **A measurement, not a conformance case.** It asserts that each split wrote the whole
/// transcript under a new generation (that is the path being measured) and prints
/// [`HeadlessMeasurement::report`]; the `headless_measurement!` test asserts only that the workload
/// ran. The contract release makes `deliver`, `cancel` and `park_again` write the head they read,
/// so the runtime no longer takes this path: the line is the expand release's cost, kept as the
/// comparison that says what S3 removed.
///
/// # Panics
///
/// When the store errors: a measurement that cannot run is a broken test, not a number.
pub async fn measure_headless_transcript(store: DynStore) -> HeadlessMeasurement {
    // It purges the measured run at the end, and that call sweeps unreferenced blobs too: take the
    // suite's lock so a concurrent case's blob is not collected (see `crate::BLOB_SUITE`).
    let _turn = crate::BLOB_SUITE.lock().await;
    let store = CountingStore::new(store);
    let agent = crate::agent();
    let run = store
        .create_run(NewRun::new(&agent, json!({"turn": 0})))
        .await
        .expect("create the measured run");
    let mut version = run.version;
    let mut messages: Vec<Value> = Vec::with_capacity(HEADLESS_STEPS);
    let mut measured = HeadlessMeasurement {
        steps: 0,
        rows: 0,
        bytes: 0,
        generations: 0,
    };
    for turn in 0..HEADLESS_STEPS {
        // A delivery's shape: no head, no body, just the version. The head becomes stale.
        version = store
            .commit_run(
                run.id,
                version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": turn, "late": true})),
            )
            .await
            .expect("the headless commit")
            .version;
        messages.push(json!({
            "role": "assistant",
            "text": message_text(MESSAGE_BYTES, turn as u64),
        }));
        let rows: Vec<adam_core::BodyRow> = messages
            .iter()
            .enumerate()
            .map(|(idx, message)| adam_core::BodyRow {
                idx: u32::try_from(idx).expect("a short transcript"),
                message: message.clone(),
            })
            .collect();
        let len = u32::try_from(rows.len()).expect("a short transcript");
        let status = if turn + 1 == HEADLESS_STEPS {
            RunStatus::Done
        } else {
            RunStatus::Runnable
        };
        let update = RunUpdate::new(status, json!({"messages": messages}))
            .head(json!({"v": 5, "agent": {}, "body": {"generation": null, "len": len}}))
            .body(adam_core::BodyWrite {
                generation: None,
                rows,
                writer: Uuid::new_v4(),
            });
        version = store
            .commit_run(run.id, version, update)
            .await
            .expect("the split after a headless commit")
            .version;
        let head = store
            .load_head(run.id)
            .await
            .expect("load the head")
            .expect("the measured run")
            .state;
        let generation = head["body"]["generation"]
            .as_str()
            .expect("the store resolved a generation");
        let stored = store
            .body_list(run.id, Uuid::parse_str(generation).expect("a uuid"))
            .await
            .expect("the body rows");
        assert_eq!(
            stored.len(),
            messages.len(),
            "a headless commit makes the next split write the whole transcript"
        );
        measured.steps += 1;
        measured.rows += u64::try_from(stored.len()).unwrap_or(u64::MAX);
        measured.bytes += stored
            .iter()
            .map(|row| {
                serde_json::to_vec(&row.message)
                    .map(|bytes| bytes.len() as u64)
                    .unwrap_or(0)
            })
            .sum::<u64>();
        measured.generations += 1;
    }
    store
        .purge_finished(
            &agent,
            adam_core::store::now() + SignedDuration::from_mins(1),
        )
        .await
        .expect("purge the measured run");
    measured
}

/// One `load_run`, and the state bytes it transferred (a delta, not the cumulative counter).
async fn read_state_bytes(store: &CountingStore, run: RunId) -> u64 {
    let before = store.state_bytes_read();
    store
        .load_run(run)
        .await
        .expect("load the measured run")
        .expect("the measured run exists");
    store.state_bytes_read() - before
}

/// `len` bytes of text that does not compress away: a real transcript is tokens, not one repeated
/// byte, and a store that compresses a large state (PostgreSQL TOASTs a big JSONB value) must be
/// measured on content it cannot shrink to nothing.
fn message_text(len: usize, seed: u64) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,\n";
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ALPHABET[(state >> 33) as usize % ALPHABET.len()] as char
        })
        .collect()
}

impl fmt::Debug for CountingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingStore").finish_non_exhaustive()
    }
}

// Every `Store` method is implemented here, `run_header` included: forwarding it keeps the measured
// store's own query (`PgStore` and `MongoStore` project the header; `MemoryStore` answers with the
// trait default). When the contract grows a **defaulted** method — S2 adds `keeps_bodies` and
// `body_list`, and `load_head` — forward it explicitly too: a default left to the wrapper answers
// for the wrapper, not for the store it wraps (`keeps_bodies` would say `false` for a store that
// keeps bodies).
#[async_trait]
impl Store for CountingStore {
    async fn migrate(&self) -> StoreResult<()> {
        self.inner.migrate().await
    }

    async fn create_run(&self, run: NewRun) -> StoreResult<RunRecord> {
        self.call(Method::CreateRun);
        let record = self.inner.create_run(run).await?;
        self.read(&record);
        Ok(record)
    }

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.call(Method::LoadRun);
        let record = self.inner.load_run(id).await?;
        if let Some(record) = &record {
            self.read(record);
        }
        Ok(record)
    }

    async fn load_head(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.call(Method::LoadHead);
        let record = self.inner.load_head(id).await?;
        if let Some(record) = &record {
            self.read(record);
        }
        Ok(record)
    }

    async fn run_header(&self, id: RunId) -> StoreResult<Option<RunHeader>> {
        self.call(Method::RunHeader);
        self.inner.run_header(id).await
    }

    async fn commit_run(
        &self,
        id: RunId,
        expected_version: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord> {
        self.call(Method::CommitRun);
        // What this commit hands the store: the state, the head, and the body rows it appends (a
        // new generation writes all of them; an append writes the ones after the last head's len).
        let state = state_bytes(&update.state);
        let head = update.head.as_ref().map_or(0, state_bytes);
        let rows = update.body.as_ref().map_or(0, |body| {
            let start = match body.generation {
                Some(_) => 0,
                None => self.counts().body_len.get(&id).copied().unwrap_or(0) as usize,
            };
            body.rows
                .iter()
                .skip(start)
                .map(|row| state_bytes(&row.message))
                .sum()
        });
        let next_len = update.head.as_ref().and_then(head_len);
        let record = self.inner.commit_run(id, expected_version, update).await?;
        self.wrote(state, state + head + rows);
        if let Some(len) = next_len {
            self.counts().body_len.insert(id, len);
        }
        self.read(&record);
        Ok(record)
    }

    fn keeps_bodies(&self) -> bool {
        self.inner.keeps_bodies()
    }

    async fn body_list(
        &self,
        run: RunId,
        generation: Uuid,
    ) -> StoreResult<Vec<adam_core::BodyRow>> {
        self.call(Method::BodyList);
        self.inner.body_list(run, generation).await
    }

    async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: Timestamp,
    ) -> StoreResult<u64> {
        self.call(Method::GcBody);
        self.inner.gc_body(run, keep_generation, before).await
    }

    async fn blob_stored(&self, id: &BlobId) -> StoreResult<bool> {
        self.call(Method::BlobStored);
        self.inner.blob_stored(id).await
    }

    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>> {
        self.call(Method::OpenRunForConversation);
        let record = self
            .inner
            .open_run_for_conversation(agent, conversation_id)
            .await?;
        if let Some(record) = &record {
            self.read(record);
        }
        Ok(record)
    }

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>> {
        self.call(Method::JournalGet);
        self.inner.journal_get(run, seq).await
    }

    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry> {
        self.call(Method::JournalPut);
        self.inner.journal_put(run, entry).await
    }

    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>> {
        self.call(Method::JournalList);
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
        self.call(Method::ClaimDue);
        let leases = self
            .inner
            .claim_due(agents, worker, scope, busy, now, ttl, limit)
            .await?;
        self.reads(leases.iter().map(|lease| &lease.run));
        Ok(leases)
    }

    async fn renew_lease(
        &self,
        id: RunId,
        worker: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> StoreResult<bool> {
        self.call(Method::RenewLease);
        self.inner.renew_lease(id, worker, now, ttl).await
    }

    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()> {
        self.call(Method::ReleaseLease);
        self.inner.release_lease(id, worker).await
    }

    async fn release_worker_leases(&self, worker: &str) -> StoreResult<()> {
        self.call(Method::ReleaseWorkerLeases);
        self.inner.release_worker_leases(worker).await
    }

    async fn lease_until(&self, id: RunId) -> StoreResult<Option<Timestamp>> {
        self.call(Method::LeaseUntil);
        self.inner.lease_until(id).await
    }

    async fn list_runs(&self, query: &RunQuery) -> StoreResult<Vec<RunRecord>> {
        self.call(Method::ListRuns);
        let runs = self.inner.list_runs(query).await?;
        self.reads(&runs);
        Ok(runs)
    }

    async fn count_runs(&self, query: &RunQuery) -> StoreResult<u64> {
        self.call(Method::CountRuns);
        self.inner.count_runs(query).await
    }

    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord> {
        self.call(Method::PushPut);
        self.inner.push_put(new).await
    }

    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>> {
        self.call(Method::PushList);
        self.inner.push_list(run).await
    }

    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool> {
        self.call(Method::PushDelete);
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
        self.call(Method::PushClaimDue);
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
        self.call(Method::PushCommit);
        self.inner
            .push_commit(run, id, expected_version, progress)
            .await
    }

    async fn purge_finished(&self, agent: &str, before: Timestamp) -> StoreResult<u64> {
        self.call(Method::PurgeFinished);
        self.inner.purge_finished(agent, before).await
    }

    fn keeps_blobs(&self) -> bool {
        self.inner.keeps_blobs()
    }

    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        self.call(Method::PutBlob);
        self.inner.put_blob(id, bytes).await
    }

    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        self.call(Method::GetBlob);
        self.inner.get_blob(id).await
    }

    async fn purge_blobs(&self, before: Timestamp) -> StoreResult<u64> {
        self.call(Method::PurgeBlobs);
        self.inner.purge_blobs(before).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_core::{ConversationScope, MemoryStore, RunStatus};
    use serde_json::json;

    use super::*;

    const AGENT: &str = "counting-test";

    /// The wrapper must not swallow `gc_body` or `blob_stored`: a wrapped store that answers the
    /// trait defaults would collect nothing and re-put every blob.
    #[tokio::test]
    async fn the_wrapper_forwards_gc_body_and_blob_stored() {
        let store = counting();
        let run = create(&store).await;
        let generation = Uuid::new_v4();
        store
            .commit_run(
                run.id,
                run.version,
                RunUpdate::new(RunStatus::Runnable, json!({"turn": 1})).body(
                    adam_core::BodyWrite {
                        generation: Some(generation),
                        rows: vec![adam_core::BodyRow {
                            idx: 0,
                            message: json!("one"),
                        }],
                        writer: Uuid::new_v4(),
                    },
                ),
            )
            .await
            .unwrap();
        let cutoff = adam_core::store::now() + SignedDuration::from_hours(1);
        assert_eq!(
            store.gc_body(run.id, Uuid::new_v4(), cutoff).await.unwrap(),
            1,
            "the wrapper reaches the inner store's GC"
        );
        assert_eq!(store.calls(Method::GcBody), 1);
        let id = BlobId::from_hex(&"a".repeat(64)).unwrap();
        assert!(!store.blob_stored(&id).await.unwrap());
        store.put_blob(&id, b"x").await.unwrap();
        assert!(store.blob_stored(&id).await.unwrap());
        assert_eq!(store.calls(Method::BlobStored), 2);
    }

    fn counting() -> Arc<CountingStore> {
        Arc::new(CountingStore::new(Arc::new(MemoryStore::new())))
    }

    /// The wrapper must not swallow `release_worker_leases`: a wrapped store that answers the trait
    /// default releases nothing, so a restarted worker waits for its old leases to run out.
    #[tokio::test]
    async fn the_wrapper_forwards_release_worker_leases() {
        let store = counting();
        let run = create(&store).await;
        let claimed = store
            .claim_due(
                &[AGENT.to_owned()],
                "w",
                ClaimScope::Any,
                &[],
                adam_core::store::now() + SignedDuration::from_secs(1),
                Duration::from_secs(30),
                1,
            )
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 1);
        store.release_worker_leases("w").await.expect("sweep");
        assert_eq!(store.calls(Method::ReleaseWorkerLeases), 1);
        assert_eq!(
            store.lease_until(run.id).await.expect("lease"),
            None,
            "the wrapper reached the inner store's sweep"
        );
    }

    async fn create(store: &CountingStore) -> RunRecord {
        store
            .create_run(NewRun::new(AGENT, json!({"messages": []})).conversation("counting:1"))
            .await
            .expect("create")
    }

    #[tokio::test]
    async fn counts_calls_per_method_until_reset() {
        let store = counting();
        let run = create(&store).await;
        store.load_run(run.id).await.expect("load");
        store.load_run(run.id).await.expect("load");
        assert_eq!(store.calls(Method::CreateRun), 1);
        assert_eq!(store.calls(Method::LoadRun), 2);
        assert_eq!(store.calls(Method::CommitRun), 0);
        store.reset();
        assert_eq!(store.calls(Method::CreateRun), 0);
        assert_eq!(store.calls(Method::LoadRun), 0);
        assert_eq!(store.state_bytes_read(), 0);
    }

    #[tokio::test]
    async fn a_read_counts_the_state_of_every_record_returned() {
        let store = counting();
        let run = store
            .create_run(
                NewRun::new(AGENT, json!({"text": "x".repeat(1000)})).conversation("counting:1"),
            )
            .await
            .expect("create");
        // A create echoes no state: there is nothing to read back.
        assert_eq!(store.state_bytes_read(), 0);

        store.reset();
        let loaded = store.load_run(run.id).await.expect("load").expect("run");
        let state = state_bytes(&loaded.state);
        assert!(state > 1000);
        assert_eq!(store.state_bytes_read(), state);

        // A listing returns the states of the page, and each one counts.
        store.reset();
        let listed = store
            .list_runs(&RunQuery::new(
                AGENT,
                ConversationScope::Exact("counting:1".into()),
                10,
            ))
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(store.state_bytes_read(), state);

        // So does a claim, which returns the run it leased.
        store.reset();
        let claimed = store
            .claim_due(
                &[AGENT.to_owned()],
                "w",
                ClaimScope::Any,
                &[],
                adam_core::store::now(),
                Duration::from_secs(30),
                1,
            )
            .await
            .expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(store.state_bytes_read(), state);
    }

    #[tokio::test]
    async fn a_commit_counts_the_state_it_writes_and_the_largest_one() {
        let store = counting();
        let run = create(&store).await;
        store.reset();
        let small = json!({"n": 1});
        let small_bytes = state_bytes(&small);
        store
            .commit_run(run.id, 1, RunUpdate::new(RunStatus::Runnable, small))
            .await
            .expect("commit");
        assert_eq!(store.state_bytes_written(), small_bytes);
        assert_eq!(store.max_commit_bytes(), small_bytes);

        let large = json!({"n": 2, "text": "x".repeat(10_000)});
        let large_bytes = state_bytes(&large);
        store
            .commit_run(run.id, 2, RunUpdate::new(RunStatus::Runnable, large))
            .await
            .expect("commit");
        assert_eq!(store.state_bytes_written(), small_bytes + large_bytes);
        assert_eq!(store.max_commit_bytes(), large_bytes);

        // A commit that loses the CAS wrote nothing: neither counter moves.
        let before = store.state_bytes_written();
        assert!(
            store
                .commit_run(run.id, 1, RunUpdate::new(RunStatus::Done, json!({"n": 3})),)
                .await
                .is_err()
        );
        assert_eq!(store.state_bytes_written(), before);
        assert_eq!(store.max_commit_bytes(), large_bytes);
        assert_eq!(store.calls(Method::CommitRun), 3);
    }

    #[tokio::test]
    async fn a_commit_counts_the_head_and_only_the_body_delta() {
        let store = counting();
        let run = create(&store).await;
        store.reset();
        let head =
            |len: u32| json!({"v": 5, "agent": {}, "body": {"generation": null, "len": len}});
        let row = |idx: u32| adam_core::BodyRow {
            idx,
            message: json!({"text": "x".repeat(100)}),
        };
        let write = |len: u32, rows: Vec<adam_core::BodyRow>| {
            RunUpdate::new(RunStatus::Runnable, Value::Null)
                .head(head(len))
                .body(adam_core::BodyWrite {
                    generation: None,
                    rows,
                    writer: Uuid::new_v4(),
                })
        };
        // A first split writes the whole transcript.
        store
            .commit_run(run.id, 1, write(1, vec![row(0)]))
            .await
            .expect("the first split");
        let first = store.commit_bytes();
        assert_eq!(
            first,
            state_bytes(&head(1)) + state_bytes(&json!({"text": "x".repeat(100)})),
            "the tombstone writes nothing; the head and the row are the commit"
        );
        assert_eq!(store.state_bytes_written(), 0, "the state is the tombstone");

        // An append writes the head and the rows past the head's len, not the whole transcript.
        store
            .commit_run(run.id, 2, write(2, vec![row(0), row(1)]))
            .await
            .expect("the append");
        assert_eq!(
            store.commit_bytes() - first,
            state_bytes(&head(2)) + state_bytes(&json!({"text": "x".repeat(100)})),
            "the row the first split wrote is not written again"
        );
    }

    #[tokio::test]
    async fn a_forwarded_header_hides_the_default_read_from_the_wrapper() {
        let store = counting();
        let run = create(&store).await;
        store.reset();
        let header = store
            .run_header(run.id)
            .await
            .expect("header")
            .expect("run");
        assert_eq!(header.id, run.id);
        // `run_header` is forwarded: the wrapper counts the call. `MemoryStore` answers it with the
        // trait default, which reads the record *inside* the adapter — a `load_run` and its state
        // the wrapper cannot see, so it counts neither.
        assert_eq!(store.calls(Method::RunHeader), 1);
        assert_eq!(store.calls(Method::LoadRun), 0);
        assert_eq!(store.state_bytes_read(), 0);
    }
}
