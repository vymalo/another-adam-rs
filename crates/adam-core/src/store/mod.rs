//! The durable-state seam of adam-rs.
//!
//! # Model
//!
//! A **run** is one execution of an agent: the framework-owned agent loop
//! state machine, serialized as JSON in [`RunRecord::state`]. The runtime
//! advances a run one transition at a time and commits each transition with
//! [`Store::commit_run`], which is a compare-and-swap on [`RunRecord::version`].
//! If a worker dies mid-run, another worker picks the run up from the last
//! committed state.
//!
//! The **journal** records the result of every side effect a tool performs
//! through `ctx.step(..)`, keyed by `(run, seq)`. On replay the recorded result
//! is returned instead of re-executing the side effect. The first writer of a
//! given `(run, seq)` wins; later writers get the winning entry back.
//!
//! **Leases** keep two workers from advancing the same run at the same time.
//! They are an efficiency mechanism, not the correctness mechanism: the
//! version CAS guarantees that at most one commit succeeds per version even if
//! a lease expires while its holder is still working.
//!
//! # Scheduling
//!
//! Every run carries a derived `sched_at` timestamp ([`sched_at`]); a run is
//! *due* when `sched_at <= now`. Runnable runs are due immediately (or at
//! `wake_at` if set, which gives retries a backoff). Parked runs are due only
//! when they have a `wake_at` (timers, `ctx.sleep`). Parked runs without
//! `wake_at` wait for an external event (approval, inbound message) and are
//! resumed by committing them back to [`RunStatus::Runnable`].
//!
//! # Conversations
//!
//! At most one *open* (runnable or parked) run may exist per
//! `(agent, conversation_id)`. Stores enforce this atomically, so two inbound
//! messages racing on the same conversation cannot start two runs.
//!
//! # Time
//!
//! Timestamps are truncated to millisecond precision ([`truncate_ms`]) because
//! BSON dates are millisecond-precision; every store normalizes to it so they
//! all behave the same.

pub mod memory;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Identifier of a run. UUIDv7 by default, so ids sort roughly by creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub Uuid);

impl RunId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Uuid> for RunId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

/// Lifecycle status of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Ready to be advanced by a worker (possibly delayed by `wake_at`).
    Runnable,
    /// Waiting: on a timer if `wake_at` is set, otherwise on an external event.
    Parked,
    /// Finished successfully.
    Done,
    /// Finished with an error.
    Failed,
}

impl RunStatus {
    pub const ALL: [RunStatus; 4] = [Self::Runnable, Self::Parked, Self::Done, Self::Failed];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runnable => "runnable",
            Self::Parked => "parked",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// Runnable or parked: the run still has work to do.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Runnable | Self::Parked)
    }

    pub fn is_terminal(self) -> bool {
        !self.is_open()
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Input for [`Store::create_run`].
#[derive(Clone, Debug, PartialEq)]
pub struct NewRun {
    pub id: RunId,
    pub agent: String,
    pub conversation_id: Option<String>,
    pub parent_id: Option<RunId>,
    pub status: RunStatus,
    pub state: Value,
    pub wake_at: Option<DateTime<Utc>>,
}

impl NewRun {
    /// A runnable run with a fresh id and the given initial state.
    pub fn new(agent: impl Into<String>, state: Value) -> Self {
        Self {
            id: RunId::new(),
            agent: agent.into(),
            conversation_id: None,
            parent_id: None,
            status: RunStatus::Runnable,
            state,
            wake_at: None,
        }
    }

    pub fn with_id(mut self, id: RunId) -> Self {
        self.id = id;
        self
    }

    pub fn conversation(mut self, id: impl Into<String>) -> Self {
        self.conversation_id = Some(id.into());
        self
    }

    pub fn parent(mut self, parent: RunId) -> Self {
        self.parent_id = Some(parent);
        self
    }

    pub fn status(mut self, status: RunStatus) -> Self {
        self.status = status;
        self
    }

    pub fn wake_at(mut self, at: DateTime<Utc>) -> Self {
        self.wake_at = Some(truncate_ms(at));
        self
    }
}

/// Input for [`Store::commit_run`]: the next state of the run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunUpdate {
    pub status: RunStatus,
    pub state: Value,
    pub wake_at: Option<DateTime<Utc>>,
}

impl RunUpdate {
    pub fn new(status: RunStatus, state: Value) -> Self {
        Self {
            status,
            state,
            wake_at: None,
        }
    }

    pub fn wake_at(mut self, at: DateTime<Utc>) -> Self {
        self.wake_at = Some(truncate_ms(at));
        self
    }
}

/// A persisted run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRecord {
    pub id: RunId,
    pub agent: String,
    pub conversation_id: Option<String>,
    pub parent_id: Option<RunId>,
    pub status: RunStatus,
    pub state: Value,
    pub wake_at: Option<DateTime<Utc>>,
    /// Starts at 1 on creation and increases by exactly 1 per successful commit.
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RunRecord {
    pub fn sched_at(&self) -> Option<DateTime<Utc>> {
        sched_at(self.status, self.wake_at, self.updated_at)
    }
}

/// A run claimed by a worker until `until`.
#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub run: RunRecord,
    pub worker: String,
    pub until: DateTime<Utc>,
}

/// The recorded outcome of one `ctx.step(..)`.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalEntry {
    pub seq: u64,
    /// Step name; replay checks it to detect non-deterministic tool code.
    pub name: String,
    /// Whether the step succeeded. On replay, `ok = false` re-raises the error.
    pub ok: bool,
    /// Serialized step output (or error).
    pub payload: Value,
    pub recorded_at: DateTime<Utc>,
}

impl JournalEntry {
    pub fn ok(seq: u64, name: impl Into<String>, payload: Value) -> Self {
        Self {
            seq,
            name: name.into(),
            ok: true,
            payload,
            recorded_at: now(),
        }
    }

    pub fn err(seq: u64, name: impl Into<String>, payload: Value) -> Self {
        Self {
            ok: false,
            ..Self::ok(seq, name, payload)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("run {0} already exists")]
    AlreadyExists(RunId),
    #[error("run {0} not found")]
    NotFound(RunId),
    #[error("version conflict on run {run}: expected {expected}, found {actual}")]
    Conflict {
        run: RunId,
        expected: u64,
        actual: u64,
    },
    #[error("conversation {conversation_id:?} of agent {agent:?} already has an open run")]
    ConversationBusy {
        agent: String,
        conversation_id: String,
    },
    #[error(
        "non-deterministic replay on run {run} at step {seq}: journal has {recorded:?}, code asked for {requested:?}"
    )]
    NonDeterminism {
        run: RunId,
        seq: u64,
        recorded: String,
        requested: String,
    },
    #[error("invalid data: {0}")]
    InvalidData(String),
    #[error("storage backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl StoreError {
    pub fn backend(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend(Box::new(err))
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// Durable storage for runs, the step journal, and scheduling leases.
///
/// Implementations must be safe to share between many workers, processes and
/// machines pointing at the same database. The `adam-store-testkit` crate
/// provides a conformance suite every implementation should pass.
#[async_trait]
pub trait Store: Send + Sync + 'static {
    /// Create tables/collections and indexes. Idempotent and safe to call
    /// concurrently from several processes.
    async fn migrate(&self) -> StoreResult<()>;

    /// Insert a new run at version 1.
    ///
    /// Errors with [`StoreError::AlreadyExists`] if the id is taken, and with
    /// [`StoreError::ConversationBusy`] if the run is open and its
    /// conversation already has an open run. A deterministic id makes this
    /// an idempotent "fire once" (e.g. one run per schedule tick).
    async fn create_run(&self, run: NewRun) -> StoreResult<RunRecord>;

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>>;

    /// Atomically replace the run's status/state/wake_at if its version is
    /// still `expected_version`, and return the new record (version + 1).
    ///
    /// Errors with [`StoreError::Conflict`] on a stale version,
    /// [`StoreError::NotFound`] if the run doesn't exist, and
    /// [`StoreError::ConversationBusy`] if re-opening a finished run would give
    /// its conversation two open runs. Does not touch the lease.
    async fn commit_run(
        &self,
        id: RunId,
        expected_version: u64,
        update: RunUpdate,
    ) -> StoreResult<RunRecord>;

    /// The open (runnable or parked) run of a conversation, if any.
    async fn open_run_for_conversation(
        &self,
        agent: &str,
        conversation_id: &str,
    ) -> StoreResult<Option<RunRecord>>;

    async fn journal_get(&self, run: RunId, seq: u64) -> StoreResult<Option<JournalEntry>>;

    /// Record a step outcome if `(run, seq)` is not recorded yet, and return
    /// the entry that is recorded afterwards: `entry` if this call won, or the
    /// earlier winner. Errors with [`StoreError::NonDeterminism`] if the
    /// recorded entry has a different step name.
    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry>;

    /// All journal entries of a run, ordered by `seq`.
    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>>;

    /// Lease up to `limit` due runs of the given agents to `worker` until
    /// `now + ttl`, earliest `sched_at` first. A run is claimable when it is
    /// due (`sched_at <= now`) and has no unexpired lease. Concurrent callers
    /// never receive the same run while its lease is valid.
    async fn claim_due(
        &self,
        agents: &[String],
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> StoreResult<Vec<Lease>>;

    /// Extend a lease held by `worker`. Returns `false` if the worker no
    /// longer holds it (expired and taken over, or released).
    async fn renew_lease(
        &self,
        id: RunId,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> StoreResult<bool>;

    /// Drop the lease if `worker` holds it, so the run is claimable at once.
    async fn release_lease(&self, id: RunId, worker: &str) -> StoreResult<()>;

    /// Delete finished (done or failed) runs of `agent` last updated before
    /// `before`, with their journals. Returns the number of runs deleted.
    async fn purge_finished(&self, agent: &str, before: DateTime<Utc>) -> StoreResult<u64>;
}

/// Shared, type-erased store handle used by the runtime.
pub type DynStore = Arc<dyn Store>;

/// When a run becomes due, or `None` if it is never due on its own.
///
/// Every store persists this value so claiming is one indexed range scan.
pub fn sched_at(
    status: RunStatus,
    wake_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match status {
        RunStatus::Runnable => Some(wake_at.unwrap_or(updated_at)),
        RunStatus::Parked => wake_at,
        RunStatus::Done | RunStatus::Failed => None,
    }
}

/// Truncate to millisecond precision (see the module docs on time).
pub fn truncate_ms(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(t.timestamp_millis())
        .single()
        .expect("millisecond timestamp in range")
}

/// Current time, truncated to milliseconds.
pub fn now() -> DateTime<Utc> {
    truncate_ms(Utc::now())
}

/// `now + ttl`, truncated to milliseconds.
pub fn add_ttl(now: DateTime<Utc>, ttl: Duration) -> DateTime<Utc> {
    let ttl = chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX);
    truncate_ms(
        now.checked_add_signed(ttl)
            .unwrap_or(DateTime::<Utc>::MAX_UTC),
    )
}

/// A single-field key for stores that enforce "one open run per conversation"
/// with a unique index on one field (the MongoDB store). The agent name is
/// length-prefixed so no two `(agent, conversation)` pairs share a key.
pub fn open_conversation_key(agent: &str, conversation_id: &str) -> String {
    format!("{}:{agent}{conversation_id}", agent.len())
}
