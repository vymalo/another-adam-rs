//! The [`Agent`] trait and the values that flow through it.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use adam_core::StoreError;
use adam_error::{BoxError, Classify, ErrorClass};

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
///
/// The runtime decides from [`Classify::class`], not from the variant:
///
/// | Variant | Class | The run |
/// |---|---|---|
/// | `Transient`, no `retry_after` | `Transient` | retried after the policy's backoff |
/// | `Transient` with `retry_after` | `RateLimited` | retried after `max(backoff, retry_after)` |
/// | `Permanent` | `Invalid` | `Failed` |
/// | `NonDeterminism` | `Corrupt` | `Failed` |
/// | `Store(e)` | `e.class()` | `Failed` for `Corrupt` and `Invalid`, otherwise left to its lease |
///
/// A message describes this layer only; the lower error is the
/// [`source`](std::error::Error::source) and [`adam_error::report`] prints the chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    /// Worth retrying: the runtime re-runs the transition after an
    /// exponential backoff, up to `RetryPolicy::max_attempts`.
    ///
    /// With a `retry_after` (a rate limit's `Retry-After`, a maintenance
    /// window, a job that reports when it will be ready) the runtime schedules
    /// the retry at `max(backoff, retry_after)` from now: the hint can lengthen
    /// the wait but never shorten the policy's own backoff. It counts as one
    /// failed attempt like any transient error, so `RetryPolicy::max_attempts`
    /// still bounds the retries. Hints longer than
    /// [`MAX_RETRY_AFTER`](crate::MAX_RETRY_AFTER) are capped to it.
    #[error("transient error: {message}")]
    Transient {
        /// What happened, without the lower error's text.
        message: String,
        /// The minimum wait before the retry, when the peer said so.
        retry_after: Option<Duration>,
        /// The lower error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// Not worth retrying: the run becomes `Failed`.
    #[error("permanent error: {message}")]
    Permanent {
        /// What is wrong, without the lower error's text.
        message: String,
        /// The lower error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// Replay asked for a different step than the journal recorded (or the
    /// recorded result no longer decodes). The run becomes `Failed`.
    #[error("non-deterministic replay: {message}")]
    NonDeterminism {
        /// Which step, and what differed.
        message: String,
        /// The lower error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// The store failed. Corrupt or invalid data fails the run; anything else
    /// leaves the run to be retried when its lease expires.
    #[error("store error")]
    Store(#[from] StoreError),
}

impl AgentError {
    /// A failure worth retrying with the policy's backoff ([`Transient`](Self::Transient)).
    pub fn transient(msg: impl std::fmt::Display) -> Self {
        Self::Transient {
            message: msg.to_string(),
            retry_after: None,
            source: None,
        }
    }

    /// Like [`transient`](Self::transient), with a minimum wait before the retry: shorthand for
    /// `AgentError::transient(msg).with_retry_after(at_least)`.
    pub fn transient_after(msg: impl std::fmt::Display, at_least: Duration) -> Self {
        Self::transient(msg).with_retry_after(at_least)
    }

    /// A failure that fails the run ([`Permanent`](Self::Permanent)).
    pub fn permanent(msg: impl std::fmt::Display) -> Self {
        Self::Permanent {
            message: msg.to_string(),
            source: None,
        }
    }

    /// A replay that diverged from the journal ([`NonDeterminism`](Self::NonDeterminism)).
    pub fn non_determinism(msg: impl std::fmt::Display) -> Self {
        Self::NonDeterminism {
            message: msg.to_string(),
            source: None,
        }
    }

    /// Ask for a minimum wait before the retry. Only a [`Transient`](Self::Transient) has one;
    /// other variants are returned as they are.
    #[must_use]
    pub fn with_retry_after(mut self, at_least: Duration) -> Self {
        if let Self::Transient { retry_after, .. } = &mut self {
            *retry_after = Some(at_least);
        }
        self
    }

    /// Keep `err` as the source of a `Transient`, `Permanent` or `NonDeterminism`; a `Store` is
    /// returned as it is.
    #[must_use]
    pub fn with_source(mut self, err: impl Into<BoxError>) -> Self {
        if let Self::Transient { source, .. }
        | Self::Permanent { source, .. }
        | Self::NonDeterminism { source, .. } = &mut self
        {
            *source = Some(err.into());
        }
        self
    }

    /// Turn a classified error into the runtime's vocabulary: a retryable one becomes
    /// [`Transient`](Self::Transient) (carrying its [`retry_after`](Classify::retry_after)),
    /// anything else [`Permanent`](Self::Permanent). `context` is this layer's message and
    /// `err` its source, so the chain prints once.
    pub fn from_classified<E>(context: impl std::fmt::Display, err: E) -> Self
    where
        E: Classify + Send + Sync + 'static,
    {
        let retry = err.is_retryable();
        let retry_after = err.retry_after();
        let out = if retry {
            Self::transient(context)
        } else {
            Self::permanent(context)
        }
        .with_source(err);
        match retry_after {
            Some(d) => out.with_retry_after(d),
            None => out,
        }
    }
}

impl Classify for AgentError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Transient {
                retry_after: Some(_),
                ..
            } => ErrorClass::RateLimited,
            Self::Transient { .. } => ErrorClass::Transient,
            Self::Permanent { .. } => ErrorClass::Invalid,
            Self::NonDeterminism { .. } => ErrorClass::Corrupt,
            Self::Store(e) => e.class(),
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Transient { retry_after, .. } => *retry_after,
            Self::Store(e) => e.retry_after(),
            _ => None,
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

/// The start-only half of an [`Agent`]: a name and the initial state of a
/// run, without a way to step it.
///
/// Starting a run needs nothing but [`AgentStarter::init`]: it turns the first
/// inbound message into the state the run is created with. Advancing the run
/// is the [`Agent`]'s job, and an [`Agent`] usually needs a model, credentials
/// or a sandbox that a process which only accepts requests should not have to
/// hold. Register a starter with `RuntimeBuilder::starter` on such a process;
/// a worker that registers the full agent under the same name steps the run.
///
/// `init` must produce the same state the [`Agent`] of that name would, and
/// `State` must be the type that agent decodes (the runtime stores it as
/// JSON). A run of a name that only a starter is registered for is never
/// claimed by this runtime's workers.
pub trait AgentStarter: Send + Sync + 'static {
    /// The state a new run starts with, stored as JSON. It is the same type as
    /// the [`Agent::State`] of the agent that steps the run.
    type State: Serialize + Send + Sync;

    /// Stable name, stored as `RunRecord::agent`. Equal to the name of the
    /// [`Agent`] that steps these runs.
    fn name(&self) -> &str;

    /// Initial state for a new run.
    fn init(&self, input: Inbound) -> Result<Self::State, AgentError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_core::RunId;

    #[derive(Debug, thiserror::Error)]
    #[error("lower")]
    struct Lower;

    #[derive(Debug, thiserror::Error)]
    #[error("classified")]
    struct Classified(ErrorClass, Option<Duration>);

    impl Classify for Classified {
        fn class(&self) -> ErrorClass {
            self.0
        }
        fn retry_after(&self) -> Option<Duration> {
            self.1
        }
    }

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &AgentError) -> ErrorClass {
        match e {
            AgentError::Transient {
                retry_after: None, ..
            } => ErrorClass::Transient,
            AgentError::Transient {
                retry_after: Some(_),
                ..
            } => ErrorClass::RateLimited,
            AgentError::Permanent { .. } => ErrorClass::Invalid,
            AgentError::NonDeterminism { .. } => ErrorClass::Corrupt,
            AgentError::Store(e) => e.class(),
        }
    }

    #[test]
    fn class_table() {
        let samples = [
            AgentError::transient("x"),
            AgentError::transient_after("x", Duration::from_secs(5)),
            AgentError::permanent("x"),
            AgentError::non_determinism("x"),
            AgentError::Store(StoreError::NotFound(RunId::new())),
            AgentError::Store(StoreError::unavailable(Lower)),
            AgentError::Store(StoreError::corrupt_source(Lower)),
            AgentError::Store(StoreError::InvalidInput("x".into())),
        ];
        for e in &samples {
            assert_eq!(e.class(), expected(e), "{e}");
        }
        let retryable: Vec<bool> = samples.iter().map(Classify::is_retryable).collect();
        assert_eq!(
            retryable,
            [true, true, false, false, false, true, false, false]
        );
        assert_eq!(samples[1].retry_after(), Some(Duration::from_secs(5)));
        assert_eq!(samples[0].retry_after(), None);
    }

    #[test]
    fn from_classified_maps_retryable_to_transient_and_keeps_the_hint() {
        let e = AgentError::from_classified(
            "model call failed",
            Classified(ErrorClass::RateLimited, Some(Duration::from_secs(30))),
        );
        assert!(matches!(e, AgentError::Transient { .. }), "{e:?}");
        assert_eq!(e.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(e.class(), ErrorClass::RateLimited);
        assert_eq!(
            adam_error::report(&e),
            "transient error: model call failed: classified"
        );

        let e = AgentError::from_classified("bad request", Classified(ErrorClass::Invalid, None));
        assert!(matches!(e, AgentError::Permanent { .. }), "{e:?}");
        assert!(!e.is_retryable());
    }

    #[test]
    fn a_message_never_repeats_its_source() {
        let e = AgentError::permanent("agent state is not serializable").with_source(Lower);
        assert!(!e.to_string().contains("lower"));
        assert_eq!(
            adam_error::report(&e),
            "permanent error: agent state is not serializable: lower"
        );
        // `Store` prints its own layer only; the store error is the source.
        let e = AgentError::Store(StoreError::unavailable(Lower));
        assert_eq!(e.to_string(), "store error");
        assert_eq!(
            adam_error::report(&e),
            "store error: storage backend error: lower"
        );
    }
}
