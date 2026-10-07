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
pub mod push;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use adam_error::{BoxError, Classify, ErrorClass};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use push::{NewPushConfig, PushProgress, PushRecord, PushState};

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

/// Whose runs a claim may take (see [`Store::claim_due`]).
///
/// The enum is closed on purpose: a new scope must fail to compile in every store and every
/// runtime that matches on it, so none of them treats it as a default.
///
/// A run has an *owner*: the worker that first claimed it with [`ClaimScope::Pinned`]. The
/// owner is store-side scheduling data, not part of the run's state, and the pure state machine
/// never sees it. See ADR 0002 (`docs/decisions/0002-workspace-placement.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum ClaimScope {
    /// Any due run, whoever owns it. The owner is neither read nor written. This is how claiming
    /// worked before owners existed, and what workers that share their workspace use.
    #[default]
    Any,
    /// Only runs without an owner, or owned by the claiming worker. A claimed run without an
    /// owner gets the claiming worker as its owner and keeps it until the run is deleted.
    Pinned,
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

/// What can go wrong talking to a [`Store`].
///
/// A variant says what happened; [`Classify::class`] says what to do about it. Callers decide
/// on the class (retry, fail the run, leave the lease), never on a variant. No driver type
/// appears here: an adapter boxes the foreign error as the `source` of [`Backend`](Self::Backend)
/// after choosing its class.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// A run with this id exists already.
    #[error("run {0} already exists")]
    AlreadyExists(RunId),
    /// No run has this id.
    #[error("run {0} not found")]
    NotFound(RunId),
    /// Another writer moved the run's version first.
    #[error("version conflict on run {run}: expected {expected}, found {actual}")]
    Conflict {
        /// The run.
        run: RunId,
        /// The version the caller expected.
        expected: u64,
        /// The version found in the store.
        actual: u64,
    },
    /// The conversation already has an open run.
    #[error("conversation {conversation_id:?} of agent {agent:?} already has an open run")]
    ConversationBusy {
        /// The agent that owns the conversation.
        agent: String,
        /// The conversation.
        conversation_id: String,
    },
    /// Replaying the journal met a step the code no longer asks for.
    #[error(
        "non-deterministic replay on run {run} at step {seq}: journal has {recorded:?}, code asked for {requested:?}"
    )]
    NonDeterminism {
        /// The run.
        run: RunId,
        /// The step's position in the journal.
        seq: u64,
        /// The step name the journal has.
        recorded: String,
        /// The step name the code asked for.
        requested: String,
    },
    /// The caller passed data the store cannot hold (a bad table prefix, a NUL in a JSONB
    /// string, a number beyond the column). The same input never succeeds.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Data read back from the store breaks an invariant (an unknown status, a negative
    /// version, an entry that vanished).
    #[error("corrupt stored data: {0}")]
    Corrupt(String),
    /// The database driver failed. The adapter that boxed `source` chose `class`.
    #[error("storage backend error")]
    Backend {
        /// What to do about it, chosen by the adapter that knows the driver.
        class: ErrorClass,
        /// The driver's own error.
        #[source]
        source: BoxError,
    },
}

impl StoreError {
    /// The backend is unreachable or busy: network, pool, restart, deadlock. Retryable.
    pub fn unavailable(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            class: ErrorClass::Transient,
            source: Box::new(source),
        }
    }

    /// The backend rejected a statement it should never have received: a bug. Not retryable.
    pub fn internal(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            class: ErrorClass::Internal,
            source: Box::new(source),
        }
    }

    /// The backend returned a row or document that cannot be decoded. Not retryable.
    pub fn corrupt_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            class: ErrorClass::Corrupt,
            source: Box::new(source),
        }
    }
}

impl Classify for StoreError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::AlreadyExists(_) | Self::ConversationBusy { .. } => ErrorClass::Rejected,
            Self::NotFound(_) => ErrorClass::NotFound,
            Self::Conflict { .. } => ErrorClass::Conflict,
            Self::NonDeterminism { .. } | Self::Corrupt(_) => ErrorClass::Corrupt,
            Self::InvalidInput(_) => ErrorClass::Invalid,
            Self::Backend { class, .. } => *class,
        }
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
    ///
    /// `scope` decides whose runs are claimable (see [`ClaimScope`]): with
    /// [`ClaimScope::Pinned`] a run that another worker owns is skipped, and
    /// the first claim of a run without an owner makes `worker` its owner.
    /// [`Store::release_lease`] never clears the owner.
    ///
    /// `busy` names runs the caller is stepping right now. They are never claimed, whatever
    /// their lease says, and do not count against `limit`. A step can outlive its lease (a
    /// renewal that failed, a clock that jumped), and claiming such a run again would lease
    /// it a second time to the worker that holds it: the caller would get back a snapshot of
    /// the run that its own step is about to make stale, or clear the new lease with the
    /// release at the end of the step. Another worker is not affected: it claims the run
    /// once the lease has expired, as always.
    #[allow(clippy::too_many_arguments)] // one claim, described by its parts; every store matches on all of them
    async fn claim_due(
        &self,
        agents: &[String],
        worker: &str,
        scope: ClaimScope,
        busy: &[RunId],
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

    /// When the lease on a run ends, or `None` if the run has no lease (never claimed, or released)
    /// or does not exist.
    ///
    /// A lease that has run out and was not released is still reported: whether it still counts is
    /// for the caller to say against its own clock (a claim treats `until <= now` as free). This is
    /// how a reader that is not a worker (the A2A server, which may run in another process) learns
    /// that a worker is stepping a run: see `RunView::claimed` in `adam-runtime`.
    async fn lease_until(&self, id: RunId) -> StoreResult<Option<DateTime<Utc>>>;

    /// Create or replace the push configuration `(new.run, new.id)` and return it.
    ///
    /// The run must exist ([`StoreError::NotFound`] otherwise). A new config is
    /// [`PushState::Active`], at version 1, with no attempts and due at once. Putting an id that
    /// exists **replaces** the config: `config` and `cursor` are the new ones, the state is
    /// `Active` again, attempts and the last error are cleared, the lease is dropped and the
    /// version is the old one plus 1, so a deliverer still holding the old version loses its
    /// next [`push_commit`](Self::push_commit). `created_at` is kept.
    async fn push_put(&self, new: NewPushConfig) -> StoreResult<PushRecord>;

    /// The push configurations of a run, ordered by id. Empty for a run that has none or does not
    /// exist.
    async fn push_list(&self, run: RunId) -> StoreResult<Vec<PushRecord>>;

    /// Delete a push configuration. Returns whether it existed (deleting twice is not an error).
    async fn push_delete(&self, run: RunId, id: &str) -> StoreResult<bool>;

    /// Lease up to `limit` due push configurations of the given agents to `worker` until
    /// `now + ttl`, earliest `next_attempt_at` first. A config is claimable when it is
    /// [`PushState::Active`], due (`next_attempt_at <= now`) and has no unexpired lease. Concurrent
    /// callers never receive the same config while its lease is valid. The records returned carry
    /// the version to pass to [`push_commit`](Self::push_commit).
    async fn push_claim_due(
        &self,
        agents: &[String],
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> StoreResult<Vec<PushRecord>>;

    /// Atomically record the progress of a push configuration if its version is still
    /// `expected_version`, drop its lease, and return the new record (version + 1).
    ///
    /// Errors with [`StoreError::Conflict`] on a stale version (the config was replaced or
    /// committed by someone else) and [`StoreError::NotFound`] if the config no longer exists (it
    /// was deleted, or its run purged).
    async fn push_commit(
        &self,
        run: RunId,
        id: &str,
        expected_version: u64,
        progress: PushProgress,
    ) -> StoreResult<PushRecord>;

    /// Delete finished (done or failed) runs of `agent` last updated before
    /// `before`, with their journals and push configurations. Returns the number of runs deleted.
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
#[allow(
    clippy::expect_used,
    reason = "a millisecond count taken from a DateTime is always representable"
)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunId {
        RunId(Uuid::nil())
    }

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &StoreError) -> ErrorClass {
        match e {
            StoreError::AlreadyExists(_) => ErrorClass::Rejected,
            StoreError::NotFound(_) => ErrorClass::NotFound,
            StoreError::Conflict { .. } => ErrorClass::Conflict,
            StoreError::ConversationBusy { .. } => ErrorClass::Rejected,
            StoreError::NonDeterminism { .. } => ErrorClass::Corrupt,
            StoreError::InvalidInput(_) => ErrorClass::Invalid,
            StoreError::Corrupt(_) => ErrorClass::Corrupt,
            StoreError::Backend { class, .. } => *class,
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("driver said no")]
    struct Driver;

    fn samples() -> Vec<StoreError> {
        vec![
            StoreError::AlreadyExists(run()),
            StoreError::NotFound(run()),
            StoreError::Conflict {
                run: run(),
                expected: 1,
                actual: 2,
            },
            StoreError::ConversationBusy {
                agent: "a".into(),
                conversation_id: "c".into(),
            },
            StoreError::NonDeterminism {
                run: run(),
                seq: 0,
                recorded: "x".into(),
                requested: "y".into(),
            },
            StoreError::InvalidInput("nul".into()),
            StoreError::Corrupt("negative version".into()),
            StoreError::unavailable(Driver),
            StoreError::internal(Driver),
            StoreError::corrupt_source(Driver),
        ]
    }

    #[test]
    fn class_table() {
        for e in samples() {
            assert_eq!(e.class(), expected(&e), "{e}");
        }
        assert_eq!(
            StoreError::unavailable(Driver).class(),
            ErrorClass::Transient
        );
        assert_eq!(StoreError::internal(Driver).class(), ErrorClass::Internal);
        assert_eq!(
            StoreError::corrupt_source(Driver).class(),
            ErrorClass::Corrupt
        );
    }

    #[test]
    fn retryable_is_derived_from_the_class() {
        let retryable: Vec<bool> = samples().iter().map(Classify::is_retryable).collect();
        // Conflict and an unavailable backend retry; nothing else does.
        assert_eq!(
            retryable,
            [
                false, false, true, false, false, false, false, true, false, false
            ]
        );
    }

    #[test]
    fn backend_display_does_not_repeat_its_source() {
        let e = StoreError::unavailable(Driver);
        let source = std::error::Error::source(&e).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("driver said no"));
        assert!(!e.to_string().contains("driver said no"));
        assert_eq!(
            adam_error::report(&e),
            "storage backend error: driver said no"
        );
    }
}
