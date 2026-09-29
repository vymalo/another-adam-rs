//! The [`Agent`] trait and the values that flow through it.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use adam_core::StoreError;

use crate::ctx::Ctx;

/// Something delivered to a run from the outside: a user message, an A2A
/// message, an approval, a webhook...
///
/// The runtime treats `id` and `kind` as opaque and does not deduplicate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Inbound {
    /// Caller-chosen identifier (for example an A2A `messageId`).
    pub id: String,
    /// What this is (`"message"`, `"approval"`, ...). Interpreted by the agent.
    pub kind: String,
    /// Arbitrary content.
    pub payload: Value,
    /// When the message entered the system.
    pub received_at: DateTime<Utc>,
}

impl Inbound {
    /// An inbound item with a fresh random id, stamped with the current time.
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.into(),
            payload,
            received_at: adam_core::store::now(),
        }
    }

    /// Replace the id (e.g. with the sender's message id).
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }
}

/// The outcome of one [`Agent::step`]: what the runtime commits next.
#[derive(Clone, Debug, PartialEq)]
pub enum Transition<S> {
    /// Commit `S`, stay runnable, and step again.
    Continue(S),
    /// Commit `state` and wait for a timer (`wake_at`) or an inbound message,
    /// whichever comes first. With `wake_at: None` only a message (or a
    /// cancel) ends the wait.
    Park {
        /// State to commit.
        state: S,
        /// Optional timer.
        wake_at: Option<DateTime<Utc>>,
    },
    /// Finish successfully.
    Done {
        /// Final state.
        state: S,
        /// Result, stored durably and shown by `Runtime::view`.
        output: Value,
    },
    /// Finish with an error the agent decided on itself.
    Fail {
        /// Final state.
        state: S,
        /// Reason, stored durably and shown by `Runtime::view`.
        error: String,
    },
}

/// Why an [`Agent`] or [`Ctx`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// Worth retrying: the runtime re-runs the transition after an
    /// exponential backoff, up to `RetryPolicy::max_attempts`.
    #[error("transient error: {0}")]
    Transient(String),
    /// Like [`AgentError::Transient`], with a minimum wait before the retry:
    /// a rate limit's `Retry-After`, a maintenance window, a job that reports
    /// when it will be ready.
    ///
    /// The runtime schedules the retry at `max(backoff, at_least)` from now:
    /// the hint can lengthen the wait but never shorten the policy's own
    /// backoff. It counts as one failed attempt like any transient error, so
    /// `RetryPolicy::max_attempts` still bounds the retries. Hints longer
    /// than [`MAX_RETRY_AFTER`](crate::MAX_RETRY_AFTER) are capped to it.
    #[error("transient error (retry in at least {1:?}): {0}")]
    TransientAfter(String, Duration),
    /// Not worth retrying: the run becomes `Failed`.
    #[error("permanent error: {0}")]
    Permanent(String),
    /// Replay asked for a different step than the journal recorded (or the
    /// recorded result no longer decodes). The run becomes `Failed`.
    #[error("non-deterministic replay: {0}")]
    NonDeterminism(String),
    /// The store failed. Invalid data fails the run; anything else leaves the
    /// run to be retried when its lease expires.
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

impl AgentError {
    /// Shorthand for [`AgentError::Transient`].
    pub fn transient(msg: impl std::fmt::Display) -> Self {
        Self::Transient(msg.to_string())
    }

    /// Shorthand for [`AgentError::TransientAfter`].
    pub fn transient_after(msg: impl std::fmt::Display, at_least: Duration) -> Self {
        Self::TransientAfter(msg.to_string(), at_least)
    }

    /// Shorthand for [`AgentError::Permanent`].
    pub fn permanent(msg: impl std::fmt::Display) -> Self {
        Self::Permanent(msg.to_string())
    }

    /// Whether the transition that returned this will be tried again.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transient(_) | Self::TransientAfter(..) => true,
            Self::Store(e) => !matches!(
                e,
                StoreError::InvalidInput(_)
                    | StoreError::Corrupt(_)
                    | StoreError::NonDeterminism { .. }
            ),
            Self::Permanent(_) | Self::NonDeterminism(_) => false,
        }
    }
}

/// A durable agent: any loop that can be written as "advance by one
/// transition" becomes crash-safe by implementing this trait.
///
/// The runtime persists `State` after every transition, so `step` may be
/// invoked more than once with the same state (after a crash, a lost lease, or
/// a retry). Side effects must therefore go through [`Ctx::step`].
#[async_trait]
pub trait Agent: Send + Sync + 'static {
    /// The agent's own state machine, stored as JSON.
    type State: Serialize + DeserializeOwned + Send + Sync;

    /// Stable name, stored as `RunRecord::agent`.
    fn name(&self) -> &str;

    /// Initial state for a new run.
    fn init(&self, input: Inbound) -> Result<Self::State, AgentError>;

    /// Advance by one transition. Side effects go through `ctx.step`; `step`
    /// may be re-invoked after a crash.
    async fn step(
        &self,
        ctx: &mut Ctx,
        state: Self::State,
    ) -> Result<Transition<Self::State>, AgentError>;
}
