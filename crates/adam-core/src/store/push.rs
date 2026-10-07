//! Push-notification configurations and their delivery progress, kept beside the runs.
//!
//! A2A lets a client register a webhook for a task and says the server must keep the
//! configuration until it is deleted and attempt delivery at least once. A restart or another
//! replica must not lose a notification, so the configuration **and** how far its delivery got
//! live in the store, next to the run they belong to.
//!
//! The store knows nothing of A2A: `config` and `cursor` are opaque JSON the caller defines. What
//! the store does own is the scheduling data, the same way it owns it for runs: `next_attempt_at`
//! says when a config is due, a lease keeps two replicas from delivering the same config at once,
//! and `version` makes every progress write a compare-and-swap. A lease is an efficiency
//! mechanism: the version CAS is what keeps the cursor from going backwards.
//!
//! **The store holds what a client gave it**, including the credentials of the webhook
//! (`config`). It does not encrypt them: protect the database like the runs it holds.

use std::fmt;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::RunId;

/// Where a push configuration is in its life. Closed on purpose: a new state must fail to
/// compile in every store and every deliverer that matches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PushState {
    /// Delivery goes on: the config is claimed when `next_attempt_at` has passed.
    Active,
    /// The task reached a terminal state and everything up to it was delivered. Nothing more
    /// will be sent; the configuration stays readable until it is deleted or the run is purged.
    Done,
    /// Delivery was abandoned (the webhook kept failing, or its address is refused). Nothing more
    /// will be sent; `last_error` says why.
    GaveUp,
}

impl PushState {
    /// Every state.
    pub const ALL: [PushState; 3] = [Self::Active, Self::Done, Self::GaveUp];

    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Done => "done",
            Self::GaveUp => "gave_up",
        }
    }

    /// The state with this stored name.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

impl fmt::Display for PushState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Input for [`Store::push_put`](super::Store::push_put).
#[derive(Clone, Debug, PartialEq)]
pub struct NewPushConfig {
    /// The run (the A2A task) the config belongs to. It must exist.
    pub run: RunId,
    /// The config's id, unique within the run. Putting an id again replaces the config.
    pub id: String,
    /// The agent of the run: what a deliverer claims by.
    pub agent: String,
    /// Who owns the config: the authenticated subject that created it.
    pub owner: String,
    /// The configuration, opaque to the store.
    pub config: Value,
    /// Where delivery starts, opaque to the store.
    pub cursor: Value,
}

/// A stored push configuration with its delivery progress.
#[derive(Clone, Debug, PartialEq)]
pub struct PushRecord {
    /// The run the config belongs to.
    pub run: RunId,
    /// The config's id, unique within the run.
    pub id: String,
    /// The agent of the run.
    pub agent: String,
    /// The subject that owns the config.
    pub owner: String,
    /// The configuration, as given.
    pub config: Value,
    /// How far delivery got, as the deliverer last wrote it.
    pub cursor: Value,
    /// Where the config is in its life.
    pub state: PushState,
    /// Failed attempts at the event being delivered (`0` when healthy).
    pub attempts: u32,
    /// Why the last attempt failed, short and free of credentials; `None` when it did not.
    pub last_error: Option<String>,
    /// When the config is due: a claim takes it once this has passed (and no lease holds it).
    pub next_attempt_at: DateTime<Utc>,
    /// Starts at 1 and increases by exactly 1 per progress write or replacement.
    pub version: u64,
    /// When the config was first created (kept when it is replaced).
    pub created_at: DateTime<Utc>,
    /// The last write.
    pub updated_at: DateTime<Utc>,
}

/// Input for [`Store::push_commit`](super::Store::push_commit): the next progress of a config.
#[derive(Clone, Debug, PartialEq)]
pub struct PushProgress {
    /// The next state.
    pub state: PushState,
    /// The next cursor.
    pub cursor: Value,
    /// Failed attempts so far at the event being delivered.
    pub attempts: u32,
    /// Why the last attempt failed, if it did.
    pub last_error: Option<String>,
    /// When the config is due next.
    pub next_attempt_at: DateTime<Utc>,
}
