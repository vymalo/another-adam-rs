//! The port push notifications keep their state behind: the configurations a client registered,
//! and how far each one's delivery got.
//!
//! [`PushStore`] is infrastructure like [`TaskBackend`](crate::TaskBackend): the durable
//! implementation is `adam-a2a-runtime`'s, over the run store (so a restart or another replica
//! loses nothing, and the configs go with their run), and [`InMemoryPushStore`] (feature
//! `test-util`) is the reference for what the methods mean.
//!
//! **The store holds what a client gave it**, credentials included. It does not encrypt them.

use std::time::Duration;

use a2a::TaskPushNotificationConfig;
use adam_error::{BoxError, Classify, ErrorClass};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::cursor::PushCursor;

/// Where a config is in its life. Closed on purpose: a new state must fail to compile in every
/// store and deliverer that matches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PushState {
    /// Delivery goes on.
    Active,
    /// The task is terminal and everything up to it was delivered. Nothing more is sent.
    Done,
    /// Delivery was abandoned: the webhook kept failing for the whole bound, or its address is
    /// not allowed. Nothing more is sent; [`PushRecord::last_error`] says why.
    GaveUp,
}

/// What [`PushStore::put`] creates or replaces.
#[derive(Clone, Debug)]
pub struct NewPushConfig {
    /// The task the config belongs to.
    pub task_id: String,
    /// The config's id, unique within the task.
    pub id: String,
    /// The authenticated subject that owns the config (and the task).
    pub owner: String,
    /// The webhook, as the client gave it.
    pub config: TaskPushNotificationConfig,
    /// Where delivery starts: what the webhook already knows.
    pub cursor: PushCursor,
}

/// A stored config with its delivery progress.
#[derive(Clone, Debug)]
pub struct PushRecord {
    /// The task the config belongs to.
    pub task_id: String,
    /// The config's id.
    pub id: String,
    /// The subject that owns it.
    pub owner: String,
    /// The webhook, as the client gave it (credentials included).
    pub config: TaskPushNotificationConfig,
    /// How far delivery got.
    pub cursor: PushCursor,
    /// Where the config is in its life.
    pub state: PushState,
    /// Failed attempts at the event being delivered.
    pub attempts: u32,
    /// Why the last attempt failed: short, with no URL and no credential.
    pub last_error: Option<String>,
    /// When the config is due.
    pub next_attempt_at: DateTime<Utc>,
    /// The version to pass back to [`PushStore::commit`].
    pub version: u64,
}

/// What [`PushStore::commit`] records.
#[derive(Clone, Debug)]
pub struct PushProgress {
    /// The next state.
    pub state: PushState,
    /// The next cursor.
    pub cursor: PushCursor,
    /// Failed attempts so far at the event being delivered.
    pub attempts: u32,
    /// Why the last attempt failed.
    pub last_error: Option<String>,
    /// When the config is due next.
    pub next_attempt_at: DateTime<Utc>,
}

/// Why a [`PushStore`] call failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PushStoreError {
    /// The task, or the config, does not exist (any more).
    #[error("not found")]
    NotFound,
    /// Someone else wrote the config first (a replacement, or another replica's commit).
    #[error("the config changed under this write")]
    Conflict,
    /// The store is temporarily unavailable. Retryable.
    #[error("push store unavailable")]
    Unavailable(#[source] BoxError),
    /// Anything else.
    #[error("push store failed")]
    Internal(#[source] BoxError),
}

impl Classify for PushStoreError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::NotFound => ErrorClass::NotFound,
            Self::Conflict => ErrorClass::Conflict,
            Self::Unavailable(_) => ErrorClass::Transient,
            Self::Internal(_) => ErrorClass::Internal,
        }
    }
}

/// The durable state of push notifications.
///
/// # Semantics every implementation must honour
///
/// * **Ownership is the caller's business.** The store does not check who may read a config: the
///   handler asks the task backend first, so another caller's task is `TaskNotFound` before the
///   store is reached.
/// * **`put`** creates or replaces `(task_id, id)`: a new config is [`PushState::Active`], at
///   version 1, with no attempts and due at once; replacing resets all that, drops any lease and
///   makes the version the old one plus 1, so a deliverer holding the old version loses its
///   next `commit`. The task must exist ([`PushStoreError::NotFound`]).
/// * **`claim_due`** leases up to `limit` active configs that are due and have no live lease, to
///   `worker` until `now + ttl`, earliest first. Two callers never get the same config while its
///   lease lives.
/// * **`commit`** is a compare-and-swap on the version; it drops the lease.
/// * **A config goes with its task.**
#[async_trait]
pub trait PushStore: Send + Sync + 'static {
    /// Create or replace a config.
    async fn put(&self, new: NewPushConfig) -> Result<PushRecord, PushStoreError>;

    /// The configs of a task, ordered by id.
    async fn list(&self, task_id: &str) -> Result<Vec<PushRecord>, PushStoreError>;

    /// Delete a config; whether it existed (deleting twice is not an error).
    async fn delete(&self, task_id: &str, id: &str) -> Result<bool, PushStoreError>;

    /// Lease due configs for delivery.
    async fn claim_due(
        &self,
        worker: &str,
        now: DateTime<Utc>,
        ttl: Duration,
        limit: usize,
    ) -> Result<Vec<PushRecord>, PushStoreError>;

    /// Record progress if the version is still `expected_version`.
    async fn commit(
        &self,
        task_id: &str,
        id: &str,
        expected_version: u64,
        progress: PushProgress,
    ) -> Result<PushRecord, PushStoreError>;
}

/// A shareable, type-erased [`PushStore`].
pub type DynPushStore = std::sync::Arc<dyn PushStore>;

#[cfg(feature = "test-util")]
pub use memory::InMemoryPushStore;

#[cfg(feature = "test-util")]
mod memory {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};

    use super::{NewPushConfig, PushProgress, PushRecord, PushState, PushStore, PushStoreError};

    /// A [`PushStore`] in process memory, for tests. Clone it to share one store between a
    /// server and a second server "after a restart".
    #[derive(Clone, Default)]
    pub struct InMemoryPushStore {
        inner: Arc<Mutex<BTreeMap<(String, String), Slot>>>,
    }

    struct Slot {
        record: PushRecord,
        lease_until: Option<DateTime<Utc>>,
    }

    impl std::fmt::Debug for InMemoryPushStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("InMemoryPushStore").finish_non_exhaustive()
        }
    }

    impl InMemoryPushStore {
        /// An empty store.
        pub fn new() -> Self {
            Self::default()
        }

        fn lock(&self) -> MutexGuard<'_, BTreeMap<(String, String), Slot>> {
            self.inner.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Every record, for a test to look at.
        pub fn records(&self) -> Vec<PushRecord> {
            self.lock().values().map(|s| s.record.clone()).collect()
        }
    }

    #[async_trait]
    impl PushStore for InMemoryPushStore {
        async fn put(&self, new: NewPushConfig) -> Result<PushRecord, PushStoreError> {
            let mut slots = self.lock();
            let key = (new.task_id.clone(), new.id.clone());
            let version = slots.get(&key).map_or(1, |s| s.record.version + 1);
            let record = PushRecord {
                task_id: new.task_id,
                id: new.id,
                owner: new.owner,
                config: new.config,
                cursor: new.cursor,
                state: PushState::Active,
                attempts: 0,
                last_error: None,
                next_attempt_at: Utc::now(),
                version,
            };
            slots.insert(
                key,
                Slot {
                    record: record.clone(),
                    lease_until: None,
                },
            );
            Ok(record)
        }

        async fn list(&self, task_id: &str) -> Result<Vec<PushRecord>, PushStoreError> {
            Ok(self
                .lock()
                .range((task_id.to_owned(), String::new())..)
                .take_while(|((t, _), _)| t == task_id)
                .map(|(_, s)| s.record.clone())
                .collect())
        }

        async fn delete(&self, task_id: &str, id: &str) -> Result<bool, PushStoreError> {
            Ok(self
                .lock()
                .remove(&(task_id.to_owned(), id.to_owned()))
                .is_some())
        }

        async fn claim_due(
            &self,
            _worker: &str,
            now: DateTime<Utc>,
            ttl: Duration,
            limit: usize,
        ) -> Result<Vec<PushRecord>, PushStoreError> {
            let until = now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX);
            let mut slots = self.lock();
            let mut due: Vec<_> = slots
                .iter()
                .filter(|(_, s)| {
                    s.record.state == PushState::Active
                        && s.record.next_attempt_at <= now
                        && s.lease_until.is_none_or(|l| l <= now)
                })
                .map(|(k, s)| (s.record.next_attempt_at, k.clone()))
                .collect();
            due.sort();
            due.truncate(limit);
            Ok(due
                .into_iter()
                .filter_map(|(_, key)| {
                    let slot = slots.get_mut(&key)?;
                    slot.lease_until = Some(until);
                    Some(slot.record.clone())
                })
                .collect())
        }

        async fn commit(
            &self,
            task_id: &str,
            id: &str,
            expected_version: u64,
            progress: PushProgress,
        ) -> Result<PushRecord, PushStoreError> {
            let mut slots = self.lock();
            let slot = slots
                .get_mut(&(task_id.to_owned(), id.to_owned()))
                .ok_or(PushStoreError::NotFound)?;
            if slot.record.version != expected_version {
                return Err(PushStoreError::Conflict);
            }
            let r = &mut slot.record;
            r.state = progress.state;
            r.cursor = progress.cursor;
            r.attempts = progress.attempts;
            r.last_error = progress.last_error;
            r.next_attempt_at = progress.next_attempt_at;
            r.version += 1;
            slot.lease_until = None;
            Ok(slot.record.clone())
        }
    }
}
