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
pub mod query;

use std::borrow::Cow;
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
pub use query::{ConversationScope, RunQuery};

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

/// The content address of a file artifact's bytes: a SHA-256 as 64 lower-case hexadecimal digits.
///
/// Opaque, and deliberately computed outside this crate: `adam-core` has no hash dependency, so
/// the digest is made where the bytes are (`adam_runtime::blob_id`) and a store trusts the id it
/// is given. A store keeps the bytes under the id ([`Store::put_blob`]) and answers them back
/// ([`Store::get_blob`]); a commit names the ids its state adds ([`RunUpdate::blobs`]) so the bytes
/// survive the later commits of that run, and are collected once no run references them
/// ([`Store::purge_blobs`]).
///
/// Serialized as its text. Deserialization validates it with [`from_hex`](Self::from_hex), so a
/// `BlobId` from JSON or BSON is always 64 lower-case hex.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BlobId(String);

impl BlobId {
    /// `hex` as an id if it is exactly 64 lower-case hexadecimal digits; `None` otherwise.
    pub fn from_hex(hex: &str) -> Option<Self> {
        (hex.len() == 64 && hex.bytes().all(is_lower_hex)).then(|| Self(hex.to_owned()))
    }

    /// The id of `digest`, the 32 bytes of a SHA-256, as 64 lower-case hex.
    ///
    /// For a process that just computed a digest and would only re-validate text it produced;
    /// [`from_hex`](Self::from_hex) is the checked constructor for an id from anywhere else.
    pub fn from_sha256(digest: &[u8; 32]) -> Self {
        use std::fmt::Write as _;
        let mut hex = String::with_capacity(64);
        for byte in digest {
            // Writing to a `String` cannot fail.
            let _ = write!(hex, "{byte:02x}");
        }
        Self(hex)
    }

    /// The id as its 64 lower-case hexadecimal digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BlobId {
    type Error = String;

    /// The checked constructor serde uses: `Ok` only for 64 lower-case hexadecimal digits.
    fn try_from(hex: String) -> Result<Self, Self::Error> {
        Self::from_hex(&hex).ok_or_else(|| format!("{hex:?} is not 64 lower-case hex digits"))
    }
}

impl From<BlobId> for String {
    fn from(id: BlobId) -> Self {
        id.0
    }
}

impl fmt::Display for BlobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
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

/// One message of a run's transcript: the row at `idx` of the body **generation** a head names.
///
/// A body row is `(run, generation, idx)`. A generation is a transcript that only grows: a normal
/// commit appends the rows after the head's `len`, and a **compaction** starts a new generation
/// with the whole transcript. See [`BodyWrite`] and [`Store::body_list`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BodyRow {
    /// Position in the transcript, oldest first.
    pub idx: u32,
    /// The message, as the agent's state holds it.
    pub message: Value,
}

/// The transcript rows one commit writes, and where they go.
///
/// A run's transcript is **append-only per generation**: a normal commit passes the full transcript
/// and `generation: None`, and the store inserts only the rows after the current head's `len` (the
/// runtime guarantees the rows start with the previous transcript). A **compaction** — the agent
/// shortened or replaced messages, so the new transcript does not start with the old one — passes
/// `generation: Some(new)` and the whole transcript; the store writes it under a new generation and
/// the old one is collected later. A run with no current head (a v4 run's first split) starts a
/// generation: `Some(new)` from the runtime, or `None` and the store picks one.
///
/// `writer` is this **attempt's** id, fixed across the CAS retries of one attempt: the store keys
/// the rows it inserts with it, so a commit that loses the CAS can delete exactly its own rows and
/// a retry of the same attempt is idempotent.
#[derive(Clone, Debug, PartialEq)]
pub struct BodyWrite {
    /// `Some(new)` starts a generation with all `rows` (a compaction, or a first split); `None`
    /// appends the rows after the current head's `len`, or starts a generation when there is no
    /// current head.
    pub generation: Option<Uuid>,
    /// The rows, oldest first, as the full transcript. Their `idx` values must be unique and
    /// contiguous from 0.
    pub rows: Vec<BodyRow>,
    /// This attempt's writer id: the store deletes the rows it inserted if the CAS loses.
    pub writer: Uuid,
}

/// Input for [`Store::commit_run`]: the next state of the run.
///
/// The returned record does not echo `state` ([`Store::commit_run`]): the caller wrote it, and a
/// commit that transferred the whole state back would pay for the largest member twice.
/// `blobs` names the file bytes this commit **adds**, which the store records beside the run (see
/// [`Store::put_blob`]).
#[derive(Clone, Debug, PartialEq)]
pub struct RunUpdate {
    pub status: RunStatus,
    pub state: Value,
    /// The v5 head this commit writes, or `None` to leave the stored head (and its version) alone.
    ///
    /// A step that splits its agent state writes the head beside the full `state`; a delivery, a
    /// cancel or a park re-encodes what it read and passes `None`, so it keeps the format it read
    /// and a stale head is not made current by a commit that did not rebuild it.
    pub head: Option<Value>,
    /// The transcript rows this commit writes, or `None` to write no rows.
    ///
    /// The rows go to the store **before** the version CAS: a commit that loses deletes the rows
    /// its `writer` inserted, so a loser cannot poison the next append. The head this commit writes
    /// names the generation the store resolved and the transcript's length; see [`BodyWrite`].
    pub body: Option<BodyWrite>,
    /// The content addresses of the file bytes this commit **adds** (see [`BlobId`]).
    ///
    /// A delta, not the run's whole set: the store unions it with the ids already recorded, so a
    /// commit that names none keeps the earlier ones. The set only grows — a file artifact is never
    /// replaced (ADR 0012) — and the bytes survive the later commits of that run, to be collected
    /// once no run references them ([`Store::purge_blobs`]). Empty by default, so a caller that
    /// stores no blobs behaves exactly as before.
    pub blobs: Vec<BlobId>,
    pub wake_at: Option<DateTime<Utc>>,
}

impl RunUpdate {
    pub fn new(status: RunStatus, state: Value) -> Self {
        Self {
            status,
            state,
            head: None,
            body: None,
            blobs: Vec::new(),
            wake_at: None,
        }
    }

    /// Write `head` with this commit (see [`head`](Self::head)).
    #[must_use]
    pub fn head(mut self, head: Value) -> Self {
        self.head = Some(head);
        self
    }

    /// Write `body` with this commit (see [`body`](Self::body)).
    #[must_use]
    pub fn body(mut self, body: BodyWrite) -> Self {
        self.body = Some(body);
        self
    }

    /// Name `blobs` as the digests this commit adds (a delta the store unions).
    #[must_use]
    pub fn blobs(mut self, blobs: Vec<BlobId>) -> Self {
        self.blobs = blobs;
        self
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

/// A run without its state: what a reader that only needs the run's own bookkeeping reads — its
/// status, its version, its lease.
///
/// The state is the largest member of a run (for an `LlmAgent`, the transcript), and some readers
/// never look at it: a step's cancel watch, a parent check, a listing that builds its view from the
/// record it already has. [`Store::run_header`] answers this: an adapter that overrides it projects
/// the header and the lease, so it costs one small read where [`Store::load_run`] transfers the
/// whole run; the trait's default reads the record.
///
/// It does not answer an A2A task's state: `input-required` is `waiting`, which the runtime derives
/// from the envelope's wake bookkeeping, so a task needs the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunHeader {
    /// The run.
    pub id: RunId,
    /// Name of its agent.
    pub agent: String,
    /// Its conversation, if any.
    pub conversation_id: Option<String>,
    /// The run it works for, if it is a child.
    pub parent_id: Option<RunId>,
    /// Lifecycle status.
    pub status: RunStatus,
    /// Timer: when a parked run wakes, or a runnable run becomes due.
    pub wake_at: Option<DateTime<Utc>>,
    /// Starts at 1 on creation and increases by exactly 1 per successful commit.
    pub version: u64,
    /// When the run was created.
    pub created_at: DateTime<Utc>,
    /// Last commit time.
    pub updated_at: DateTime<Utc>,
    /// When the lease on the run ends, or `None` if it has none (never claimed, or released).
    /// A lease that ran out is still reported: whether it still counts is for the caller's clock
    /// (see [`claimed`](Self::claimed)).
    pub lease_until: Option<DateTime<Utc>>,
}

impl RunHeader {
    /// Whether a worker holds an unexpired lease on the run: it is being stepped right now,
    /// though nothing has been committed yet. [`claimed`] is the rule; this is it for a header.
    pub fn claimed(&self, now: DateTime<Utc>) -> bool {
        claimed(self.status, self.lease_until, now)
    }
}

/// Whether a worker holds an unexpired lease on a run: it is being stepped right now, though
/// nothing has been committed yet.
///
/// The one rule of [`RunHeader::claimed`], a runtime view's `claimed`, and a listing that builds a
/// view from a record it already has: only a runnable run is being stepped, whatever lease a parked
/// or finished one still carries, and a lease that ran out (the worker died) does not count.
pub fn claimed(status: RunStatus, lease_until: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    status == RunStatus::Runnable && lease_until.is_some_and(|until| until > now)
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
    /// The caller passed data the store cannot hold (a bad table prefix, a number beyond the
    /// column). The same input never succeeds. A NUL is not such data: every adapter replaces it
    /// with U+FFFD before it writes ([`storable`]).
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
///
/// Every JSON value a method writes is passed through [`storable`] first (a NUL in a string or a
/// key becomes U+FFFD), so the same run reads the same bytes on every backend and a tool that
/// printed a NUL does not fail it. A read returns the stored value. [`Store::journal_put`] returns
/// the payload it stored (the loop decodes the step's outcome from it) and the push methods return
/// the cleaned configuration and cursor; [`Store::create_run`] and [`Store::commit_run`] return the
/// record without its state ([`RunRecord::state`] is [`Value::Null`]), which the caller wrote and
/// does not need echoed back on every transition.
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
    ///
    /// The returned record carries no state ([`RunRecord::state`] is [`Value::Null`]): the caller
    /// supplied it, and a read returns the stored one (a store may have replaced a NUL in it).
    async fn create_run(&self, run: NewRun) -> StoreResult<RunRecord>;

    async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>>;

    /// The run as an **observer** reads it: its `state` is the v5 head when the store has a current
    /// one (`head_version == version`), and the whole state otherwise.
    ///
    /// Defaults to [`load_run`](Self::load_run), so a store that keeps no head answers exactly as it
    /// does today. A head that a later commit did not rebuild (a delivery, a cancel, a park) is
    /// stale and is **not** served: the whole state is, and the next step re-splits and rewrites the
    /// head. The expand release (S2a) writes and verifies the head but serves the full state: the
    /// read path (`Runtime::view`) does not call this method yet, and neither does anything that
    /// steps a run. Tests call it to prove the stored head round-trips.
    async fn load_head(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
        self.load_run(id).await
    }

    /// The run's header — everything about it but its state — or `None` if it does not exist.
    ///
    /// **The default implementation reads twice**: [`lease_until`](Self::lease_until) and then
    /// [`load_run`](Self::load_run), which it projects. An adapter that can answer both in one
    /// statement overrides it: PostgreSQL selects the header columns and the lease together,
    /// MongoDB projects them in one `find_one`, so neither reads the state.
    ///
    /// The lease is read **before** the record, as the runtime's `view` does: a claim that lands
    /// between the two reads then reads as not yet, and a step that committed and released is read
    /// with the version it committed. A reader that needs the state as well reads
    /// [`load_run`](Self::load_run) instead.
    async fn run_header(&self, id: RunId) -> StoreResult<Option<RunHeader>> {
        let lease_until = self.lease_until(id).await?;
        let Some(rec) = self.load_run(id).await? else {
            return Ok(None);
        };
        Ok(Some(RunHeader {
            id: rec.id,
            agent: rec.agent,
            conversation_id: rec.conversation_id,
            parent_id: rec.parent_id,
            status: rec.status,
            wake_at: rec.wake_at,
            version: rec.version,
            created_at: rec.created_at,
            updated_at: rec.updated_at,
            lease_until,
        }))
    }

    /// Atomically replace the run's status/state/wake_at if its version is
    /// still `expected_version`, and return the new record (version + 1).
    ///
    /// Errors with [`StoreError::Conflict`] on a stale version,
    /// [`StoreError::NotFound`] if the run doesn't exist, and
    /// [`StoreError::ConversationBusy`] if re-opening a finished run would give
    /// its conversation two open runs. Does not touch the lease.
    ///
    /// The returned record carries no state ([`RunRecord::state`] is [`Value::Null`]): the caller
    /// supplied `update.state`, and a read returns the stored one. Every other field is the stored
    /// one, so a caller that reports the commit (the version, the status, the parent notice) reads
    /// it here and does not decode the state it already has.
    ///
    /// `update.blobs` is recorded with the CAS: a store that keeps blobs unions the commit's delta
    /// ([`RunUpdate::blobs`]) with the references already recorded, in the same step as the version
    /// check, and a commit that loses the check records nothing.
    ///
    /// `update.head` and `update.body` are the expand release's shadow (ADR 0055): a step that
    /// splits its agent state writes the v5 head beside the full `state` and the rows the head
    /// names. `head: None` leaves the stored head and its version alone, so a delivery, a cancel or
    /// a park keeps the format it read. `body: None` writes no rows.
    ///
    /// A store that keeps bodies ([`keeps_bodies`](Self::keeps_bodies)) resolves the generation
    /// ([`BodyWrite`]), appends only the rows after the current head's `len` (or writes all of them
    /// for a new generation), writes them **before** the CAS with the attempt's `writer`, and
    /// patches the head's `body` to `{generation, len}`. A commit that loses deletes the rows its
    /// `writer` inserted: a loser must not poison the next append. It also checks that all the rows
    /// landed, so a head never names a generation that is missing a row.
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
    ///
    /// The entry returned is the stored one, payload included: the payload went through
    /// [`storable`], and the loop decodes the step's outcome from what is returned, so it must be
    /// the bytes a replay will read.
    async fn journal_put(&self, run: RunId, entry: JournalEntry) -> StoreResult<JournalEntry>;

    /// All journal entries of a run, ordered by `seq`.
    async fn journal_list(&self, run: RunId) -> StoreResult<Vec<JournalEntry>>;

    /// Lease up to `limit` due runs of the given agents to `worker` until
    /// `now + ttl`, earliest `sched_at` first. A run is claimable when it is
    /// due (`sched_at <= now`) and has no unexpired lease. Concurrent callers
    /// never receive the same run while its lease is valid.
    ///
    /// The record returned may be read after the lease was set, so it can be newer than what
    /// the claim matched, terminal included ([`Store::commit_run`] does not touch the lease).
    /// A caller must not step a returned run that is not open.
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

    /// The runs `query` selects, ordered by `updated_at` descending and then `id` descending, one
    /// page of at most `query.limit`: the keyset page that starts strictly after `query.after`.
    ///
    /// The scope is required, so a listing is always one owner's (or one conversation's) runs.
    /// Every adapter indexes it: a page is one indexed read, however deep.
    async fn list_runs(&self, query: &RunQuery) -> StoreResult<Vec<RunRecord>>;

    /// How many runs match `query`'s filters (its `after` and `limit` are ignored): the total
    /// before pagination.
    async fn count_runs(&self, query: &RunQuery) -> StoreResult<u64>;

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
    /// `before`, with their journals, body rows and push configurations. Returns the number of runs
    /// deleted.
    ///
    /// A run's blob references ([`RunUpdate::blobs`]) go with it; after the run deletions, in the
    /// same call, the blobs no surviving run references and that are older than `before` are
    /// deleted too. A digest shared with a run that survives is kept. The run count may be short
    /// when several sweepers run at once (each takes the rows it could lock, and the next call
    /// takes the rest).
    ///
    /// The sweep and a commit's reference insert are **not one transaction**: a reference another
    /// commit is writing at that moment is invisible to the sweep, which can therefore collect a
    /// blob that commit is about to name. The PostgreSQL follow-up (after #361) closes it with one
    /// data-modifying CTE; MongoDB inserts the references, checks their completeness, then CASes
    /// (ADR 0055, decision 3); until then the window is a blob whose bytes a run's state still
    /// carries in the expand release.
    ///
    /// **Retention must be longer than a minute.** A child run looks at its parent at least once a
    /// minute (`adam-runtime`), and fails when the parent is `Failed`; a parent that is missing is
    /// treated as unknown and cancels nothing, so a failed parent purged before its children saw it
    /// leaves them running to their own limits.
    async fn purge_finished(&self, agent: &str, before: DateTime<Utc>) -> StoreResult<u64>;

    /// Whether this store keeps a run's transcript beside its head ([`body_list`](Self::body_list),
    /// [`RunUpdate::body`]). `false` (the default) means `RunUpdate::state` is the whole run, as
    /// before: a store that opts out keeps exactly today's behaviour.
    fn keeps_bodies(&self) -> bool {
        false
    }

    /// The rows of generation `generation` of `run`, ordered by [`idx`](BodyRow::idx); empty when
    /// it does not exist (never written, or collected after a compaction).
    ///
    /// A store that keeps no bodies ([`keeps_bodies`](Self::keeps_bodies)) answers empty.
    async fn body_list(&self, run: RunId, generation: Uuid) -> StoreResult<Vec<BodyRow>> {
        let _ = (run, generation);
        Ok(Vec::new())
    }

    /// Delete the body generations of `run` other than `keep_generation` that were written before
    /// `before`, and return how many rows went. The generation a head names is never collected,
    /// however old it is; a superseded one is collected by the next commit of the run, with a
    /// grace period, or by an explicit call with its own cutoff.
    ///
    /// A store that keeps no bodies ([`keeps_bodies`](Self::keeps_bodies)) answers `0`.
    async fn gc_body(
        &self,
        run: RunId,
        keep_generation: Uuid,
        before: DateTime<Utc>,
    ) -> StoreResult<u64> {
        let _ = (run, keep_generation, before);
        Ok(0)
    }

    /// Whether this store keeps a run's file bytes out of its state
    /// ([`put_blob`](Self::put_blob), [`get_blob`](Self::get_blob)). `false` (the default) means a
    /// file artifact's bytes live in the run's state, which is how every store worked before blobs.
    fn keeps_blobs(&self) -> bool {
        false
    }

    /// Store `bytes` under `id`, their content address ([`BlobId`]).
    ///
    /// Idempotent: a second put of an id writes nothing, and the bytes behind an id never change,
    /// so a commit that is retried costs one no-op statement. A store that keeps no blobs
    /// ([`keeps_blobs`](Self::keeps_blobs)) answers [`StoreError::InvalidInput`].
    async fn put_blob(&self, id: &BlobId, bytes: &[u8]) -> StoreResult<()> {
        let _ = (id, bytes);
        Err(StoreError::InvalidInput("this store keeps no blobs".into()))
    }

    /// The bytes stored under `id`, or `None` when there are none: never written, or collected once
    /// no run referenced them any more (see [`purge_blobs`](Self::purge_blobs)). A store that keeps
    /// no blobs ([`keeps_blobs`](Self::keeps_blobs)) answers [`StoreError::InvalidInput`], as
    /// [`put_blob`](Self::put_blob) does.
    async fn get_blob(&self, id: &BlobId) -> StoreResult<Option<Vec<u8>>> {
        let _ = id;
        Err(StoreError::InvalidInput("this store keeps no blobs".into()))
    }

    /// Delete the blobs written before `before` that no run references, and return how many were
    /// deleted. A blob a surviving run references is kept, however old it is: a digest shared by
    /// several runs is deleted only with the last one.
    ///
    /// The explicit knob for a host that reclaims bytes without purging runs;
    /// [`purge_finished`](Self::purge_finished) sweeps the same set with its own cutoff.
    async fn purge_blobs(&self, before: DateTime<Utc>) -> StoreResult<u64> {
        let _ = before;
        Ok(0)
    }
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

/// The `(generation, len)` a v5 head's `body` names, or `None` when it has none yet: a head the
/// runtime built leaves `generation` `null` for the store to resolve ([`set_head_body`]).
pub fn head_body(head: &Value) -> Option<(Uuid, u32)> {
    let body = head.get("body")?;
    let generation = body
        .get("generation")?
        .as_str()
        .and_then(|text| Uuid::parse_str(text).ok())?;
    let len = u32::try_from(body.get("len")?.as_u64()?).ok()?;
    Some((generation, len))
}

/// Point a v5 head's `body` at `generation`, with `len` rows: what a store does when it resolves
/// the generation an append belongs to. The runtime leaves `generation` `null` in the head it
/// builds, because only the store can see the current head.
pub fn set_head_body(head: &mut Value, generation: Uuid, len: u32) -> StoreResult<()> {
    let body = head
        .get_mut("body")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| StoreError::InvalidInput("a v5 head has no body object".into()))?;
    body.insert("generation".into(), Value::String(generation.to_string()));
    body.insert("len".into(), Value::from(len));
    Ok(())
}

/// A JSON value as **every** store keeps it: the NUL character (`\u{0}`) in a string or an object
/// key becomes U+FFFD, the replacement character.
///
/// PostgreSQL's `JSONB` cannot hold a NUL at all, and a value that one backend kept while another
/// refused it would make a run's bytes depend on the store behind it. Every adapter applies this
/// one rule before it writes a state, a journal payload or a push configuration, and returns what
/// it stored, so a tool that printed a NUL (a command's output, an MCP result) neither fails the
/// run nor replays differently ([ADR 0051](../../../../docs/decisions/0051-a-nul-is-stored-as-the-replacement-character.md)).
/// Borrowed when there is nothing to replace, so the common path copies nothing.
pub fn storable(value: &Value) -> Cow<'_, Value> {
    if !holds_a_nul(value) {
        return Cow::Borrowed(value);
    }
    let mut owned = value.clone();
    replace_nul(&mut owned);
    Cow::Owned(owned)
}

/// [`storable`]'s rule for a single string: a NUL becomes U+FFFD, borrowed when there is none.
pub fn storable_text(text: &str) -> Cow<'_, str> {
    if text.contains('\u{0}') {
        Cow::Owned(text.replace('\u{0}', "\u{FFFD}"))
    } else {
        Cow::Borrowed(text)
    }
}

/// [`storable`] for a value the caller already owns: the same rule, with no clone on either path
/// (a clean value is returned as it is; a dirty one is changed in place).
pub fn storable_owned(mut value: Value) -> Value {
    if holds_a_nul(&value) {
        replace_nul(&mut value);
    }
    value
}

/// Whether `value` holds a NUL in a string or a key, at any depth.
fn holds_a_nul(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains('\u{0}'),
        Value::Array(items) => items.iter().any(holds_a_nul),
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key.contains('\u{0}') || holds_a_nul(value)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Replace every NUL of `value` with U+FFFD, in strings and in keys.
fn replace_nul(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(clean) = storable_text(text) {
                *text = clean;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(replace_nul),
        Value::Object(map) => {
            if map.keys().any(|key| key.contains('\u{0}')) {
                let old = std::mem::take(map);
                let mut new = serde_json::Map::with_capacity(old.len());
                for (key, mut value) in old {
                    replace_nul(&mut value);
                    let key = if key.contains('\u{0}') {
                        key.replace('\u{0}', "\u{FFFD}")
                    } else {
                        key
                    };
                    new.insert(key, value);
                }
                *map = new;
            } else {
                map.values_mut().for_each(replace_nul);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
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
    use async_trait::async_trait;
    use serde_json::json;

    /// A store that overrides none of the new defaults: the reference for "today's behaviour".
    struct Bare {
        state: Value,
    }

    #[async_trait]
    impl Store for Bare {
        async fn migrate(&self) -> StoreResult<()> {
            Ok(())
        }

        async fn create_run(&self, _run: NewRun) -> StoreResult<RunRecord> {
            unimplemented!("the defaults under test never create a run")
        }

        async fn load_run(&self, id: RunId) -> StoreResult<Option<RunRecord>> {
            let t = now();
            Ok(Some(RunRecord {
                id,
                agent: "a".into(),
                conversation_id: None,
                parent_id: None,
                status: RunStatus::Runnable,
                state: self.state.clone(),
                wake_at: None,
                version: 1,
                created_at: t,
                updated_at: t,
            }))
        }

        async fn commit_run(
            &self,
            _id: RunId,
            _expected_version: u64,
            _update: RunUpdate,
        ) -> StoreResult<RunRecord> {
            unimplemented!("the defaults under test never commit")
        }

        async fn open_run_for_conversation(
            &self,
            _agent: &str,
            _conversation_id: &str,
        ) -> StoreResult<Option<RunRecord>> {
            unimplemented!("the defaults under test never open a conversation")
        }

        async fn journal_get(&self, _run: RunId, _seq: u64) -> StoreResult<Option<JournalEntry>> {
            unimplemented!("the defaults under test never read the journal")
        }

        async fn journal_put(
            &self,
            _run: RunId,
            _entry: JournalEntry,
        ) -> StoreResult<JournalEntry> {
            unimplemented!("the defaults under test never write the journal")
        }

        async fn journal_list(&self, _run: RunId) -> StoreResult<Vec<JournalEntry>> {
            unimplemented!("the defaults under test never list the journal")
        }

        async fn claim_due(
            &self,
            _agents: &[String],
            _worker: &str,
            _scope: ClaimScope,
            _busy: &[RunId],
            _now: DateTime<Utc>,
            _ttl: Duration,
            _limit: usize,
        ) -> StoreResult<Vec<Lease>> {
            unimplemented!("the defaults under test never claim")
        }

        async fn renew_lease(
            &self,
            _id: RunId,
            _worker: &str,
            _now: DateTime<Utc>,
            _ttl: Duration,
        ) -> StoreResult<bool> {
            unimplemented!("the defaults under test never renew a lease")
        }

        async fn release_lease(&self, _id: RunId, _worker: &str) -> StoreResult<()> {
            unimplemented!("the defaults under test never release a lease")
        }

        async fn lease_until(&self, _id: RunId) -> StoreResult<Option<DateTime<Utc>>> {
            unimplemented!("the defaults under test never read a lease")
        }

        async fn list_runs(&self, _query: &RunQuery) -> StoreResult<Vec<RunRecord>> {
            unimplemented!("the defaults under test never list")
        }

        async fn count_runs(&self, _query: &RunQuery) -> StoreResult<u64> {
            unimplemented!("the defaults under test never count")
        }

        async fn push_put(&self, _new: NewPushConfig) -> StoreResult<PushRecord> {
            unimplemented!("the defaults under test never write a push config")
        }

        async fn push_list(&self, _run: RunId) -> StoreResult<Vec<PushRecord>> {
            unimplemented!("the defaults under test never list push configs")
        }

        async fn push_delete(&self, _run: RunId, _id: &str) -> StoreResult<bool> {
            unimplemented!("the defaults under test never delete a push config")
        }

        async fn push_claim_due(
            &self,
            _agents: &[String],
            _worker: &str,
            _now: DateTime<Utc>,
            _ttl: Duration,
            _limit: usize,
        ) -> StoreResult<Vec<PushRecord>> {
            unimplemented!("the defaults under test never claim a push config")
        }

        async fn push_commit(
            &self,
            _run: RunId,
            _id: &str,
            _expected_version: u64,
            _progress: PushProgress,
        ) -> StoreResult<PushRecord> {
            unimplemented!("the defaults under test never commit a push config")
        }

        async fn purge_finished(&self, _agent: &str, _before: DateTime<Utc>) -> StoreResult<u64> {
            unimplemented!("the defaults under test never purge")
        }
    }

    #[test]
    fn a_new_run_update_writes_no_head_and_no_body() {
        let update = RunUpdate::new(RunStatus::Runnable, json!({"turn": 1}));
        assert_eq!(update.head, None);
        assert_eq!(update.body, None);
        assert!(update.blobs.is_empty());
        assert_eq!(update.wake_at, None);
    }

    #[tokio::test]
    async fn the_defaults_keep_no_bodies_and_serve_the_whole_state() {
        let state = json!({"v": 1, "agent": {"turn": 1}});
        let store = Bare {
            state: state.clone(),
        };
        let id = RunId::new();
        assert!(!store.keeps_bodies(), "a store opts in to bodies");
        assert!(
            store.body_list(id, Uuid::nil()).await.unwrap().is_empty(),
            "a store that keeps no bodies answers empty"
        );
        // `load_head` is `load_run`: the same record, and the whole state.
        let run = store.load_run(id).await.unwrap();
        let head = store.load_head(id).await.unwrap();
        assert_eq!(head, run);
        assert_eq!(head.unwrap().state, state);
    }

    #[test]
    fn a_body_row_is_its_index_and_its_message() {
        let row = BodyRow {
            idx: 7,
            message: json!({"role": "user", "text": "hi"}),
        };
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(
            json,
            json!({"idx": 7, "message": {"role": "user", "text": "hi"}})
        );
        assert_eq!(serde_json::from_value::<BodyRow>(json).unwrap(), row);
    }

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
    fn a_blob_id_is_64_lower_case_hex_or_nothing() {
        let id = BlobId::from_sha256(&[0xab; 32]);
        assert_eq!(id.as_str(), "ab".repeat(32));
        assert_eq!(BlobId::from_hex(id.as_str()), Some(id.clone()));
        assert_eq!(BlobId::from_hex(&"A".repeat(64)), None, "upper case");
        assert_eq!(BlobId::from_hex(&"g".repeat(64)), None, "not hex");
        assert_eq!(BlobId::from_hex(&"0".repeat(63)), None, "too short");
        assert_eq!(BlobId::from_hex(&"0".repeat(65)), None, "too long");
        // Its text in JSON, both ways, and deserialization validates like `from_hex` does.
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{id}\""));
        assert_eq!(
            serde_json::from_str::<BlobId>(&format!("\"{id}\"")).unwrap(),
            id
        );
        assert!(serde_json::from_str::<BlobId>(&format!("\"{}\"", "A".repeat(64))).is_err());
        assert!(serde_json::from_str::<BlobId>(&format!("\"{}\"", "0".repeat(63))).is_err());
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
    fn a_value_without_a_nul_is_borrowed_and_unchanged() {
        let value = json!({"text": "a\u{1}b", "nested": [1, true, null]});
        assert!(matches!(storable(&value), std::borrow::Cow::Borrowed(_)));
        assert_eq!(storable(&value).as_ref(), &value);
    }

    #[test]
    fn every_nul_in_a_string_a_key_and_at_any_depth_becomes_the_replacement_character() {
        let value = json!({
            "text": "a\u{0}b",
            "nested": [{"deep": "x\u{0}\u{0}y"}],
            "k\u{0}ey": 1,
            "\u{0}": "\u{0}",
            "number": 1,
        });
        let want = json!({
            "text": "a\u{FFFD}b",
            "nested": [{"deep": "x\u{FFFD}\u{FFFD}y"}],
            "k\u{FFFD}ey": 1,
            "\u{FFFD}": "\u{FFFD}",
            "number": 1,
        });
        assert_eq!(storable(&value).into_owned(), want);
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
