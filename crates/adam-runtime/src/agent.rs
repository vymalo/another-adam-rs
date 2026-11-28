//! The [`Agent`] trait and the values that flow through it.

use std::time::Duration;

use adam_core::time::Timestamp;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use adam_core::{RunId, StoreError};
use adam_error::{BoxError, Classify, ErrorClass};

use crate::ctx::Ctx;
use crate::retry::{RetryPolicy, StepRetry};

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
    pub received_at: Timestamp,
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
        wake_at: Option<Timestamp>,
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
/// | `Transient`, no `retry_after` | its `class` (`Transient` unless built by [`from_classified`](AgentError::from_classified)) | retried after the policy's backoff, when the policy retries the class |
/// | `Transient` of class `Transient` with `retry_after` | `RateLimited` | retried after `max(backoff, retry_after)`, when the policy retries rate limits |
/// | `Transient` of class `RateLimited` or `Conflict` with `retry_after` | its `class` | retried after `max(backoff, retry_after)` |
/// | `Permanent` | `Invalid` | `Failed` |
/// | `Rejected` | `Rejected` | `Failed`, with the policy's reason |
/// | `NonDeterminism` | `Corrupt` | `Failed` |
/// | `Store(e)` | `e.class()` | `Failed` for `Corrupt` and `Invalid`, otherwise left to its lease |
///
/// The policy is the error's own [`StepRetry`] when it carries one ([`with_retry`](AgentError::with_retry)),
/// else the runtime's ([ADR 0061](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0061-a-step-names-its-retry-policy.md)).
///
/// A message describes this layer only; the lower error is the
/// [`source`](std::error::Error::source) and [`adam_error::report`] prints the chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    /// Worth retrying: the runtime re-runs the transition after an
    /// exponential backoff, up to `RetryPolicy::max_attempts` of the error's policy (its `retry`,
    /// else the runtime's), when that policy retries the error's `class`.
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
        /// The retryable class of the failure: `Transient`, `RateLimited` or `Conflict`.
        class: ErrorClass,
        /// The step's own policy and the key its failures count under; `None`: the runtime's,
        /// under [`RUNTIME_RETRY_KEY`](crate::RUNTIME_RETRY_KEY).
        retry: Option<Box<StepRetry>>,
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
    /// Valid input that a policy refuses: the run becomes `Failed`, and the message is the reason a
    /// person reads ([`ErrorClass::Rejected`]). The deciding hooks of a message use it (ADR 0050,
    /// slice H1b); a front that reports the failure shows the reason as it is.
    #[error("refused: {message}")]
    Rejected {
        /// Why the input was refused, as a person reads it.
        message: String,
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
            class: ErrorClass::Transient,
            retry: None,
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

    /// A valid input a policy refuses: the run fails and the reason is what a person reads
    /// ([`Rejected`](Self::Rejected), [`ErrorClass::Rejected`]).
    pub fn rejected(msg: impl std::fmt::Display) -> Self {
        Self::Rejected {
            message: msg.to_string(),
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

    /// Count a [`Transient`](Self::Transient)'s failures under `key` with `policy`, the step's own
    /// (ADR 0061): the run retries while that key's failures are under the policy's
    /// `max_attempts` and the policy retries the error's class. The policy is recorded at the key's
    /// first failure, so a later deploy that changes it does not change a run already failing.
    /// Other variants are returned as they are.
    #[must_use]
    pub fn with_retry(mut self, key: impl Into<String>, policy: RetryPolicy) -> Self {
        if let Self::Transient { retry, .. } = &mut self {
            *retry = Some(Box::new(StepRetry::new(key, policy)));
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
    /// [`Transient`](Self::Transient) (carrying its class and its [`retry_after`](Classify::retry_after)),
    /// anything else [`Permanent`](Self::Permanent). `context` is this layer's message and
    /// `err` its source, so the chain prints once.
    pub fn from_classified<E>(context: impl std::fmt::Display, err: E) -> Self
    where
        E: Classify + Send + Sync + 'static,
    {
        let class = err.class();
        let retry_after = err.retry_after();
        let out = if class.is_retryable() {
            let mut out = Self::transient(context);
            if let Self::Transient { class: c, .. } = &mut out {
                *c = class;
            }
            out
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
                class: ErrorClass::Transient,
                ..
            } => ErrorClass::RateLimited,
            Self::Transient { class, .. } => *class,
            Self::Permanent { .. } => ErrorClass::Invalid,
            Self::Rejected { .. } => ErrorClass::Rejected,
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

    /// The journal epoch this agent's step list is written against ([ADR 0076]): the number a run
    /// it starts records, and the newest epoch a worker of this agent may replay.
    ///
    /// `0` by default, so an agent that never changes its step list never bumps it and every run
    /// it made stays replayable. An author bumps it when a step's name, its order or its result
    /// changes, so a run started under the older step list is never replayed against the newer
    /// one: a worker whose epoch is **older** than the run's leaves the run untouched, for a
    /// worker of its own epoch to finish ([`Agent::min_replay_epoch`] is the other end).
    ///
    /// [ADR 0076]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0076-a-run-records-the-journal-epoch-it-started-on.md
    fn replay_epoch(&self) -> u32 {
        0
    }

    /// The oldest journal epoch this agent can still replay ([ADR 0076]): a run started **below**
    /// it ends `Failed`, naming both epochs, instead of being replayed against a step list that no
    /// longer exists. `0` by default, so nothing is ever too old.
    ///
    /// [ADR 0076]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0076-a-run-records-the-journal-epoch-it-started-on.md
    fn min_replay_epoch(&self) -> u32 {
        0
    }

    /// Initial state for a new run.
    fn init(&self, input: Inbound) -> Result<Self::State, AgentError>;

    /// Initial state for a new run that **continues** run `prior_run`, whose last committed state
    /// is `prior`.
    ///
    /// This is how a new task in the same conversation remembers the one before it: the runtime
    /// calls it (`Runtime::start_continuing`, `Runtime::start_with_id_continuing`) instead of
    /// [`init`](Self::init) when the caller names a run to continue. The new run is an ordinary
    /// run with its own id, journal and limits; only its starting state is derived from `prior`.
    ///
    /// The default ignores `prior` and returns `self.init(input)`, so an agent that has nothing to
    /// carry over keeps working unchanged. An agent that overrides it must return the same kind of
    /// state `init` does, and a wrapper that delegates `init` to another agent must delegate this
    /// as well, or the continuation silently stops at the wrapper.
    ///
    /// `prior` is the state as the prior run last committed it, whatever status that run has: a
    /// run that failed mid-turn may hold a half-done step, and an override decides what to keep.
    /// The runtime does not check that the caller may read the prior run (ownership is the
    /// caller's rule, for A2A `adam-a2a-runtime`'s), only that it is a run of this agent whose
    /// state decodes; one that does not decode falls back to `init` with a warning.
    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Self::State,
        prior_run: RunId,
    ) -> Result<Self::State, AgentError> {
        let _ = (prior, prior_run);
        self.init(input)
    }

    /// Advance by one transition. Side effects go through `ctx.step`; `step`
    /// may be re-invoked after a crash.
    async fn step(
        &self,
        ctx: &mut Ctx,
        state: Self::State,
    ) -> Result<Transition<Self::State>, AgentError>;

    /// Split `state` into the small part a run's head keeps and its transcript, oldest first.
    ///
    /// `None` (the default) keeps the whole state in the head: today's behaviour, unoptimised. The
    /// runtime checks on every commit that [`join`](Self::join)`(split(s).0, split(s).1)`
    /// re-encodes to `s`; a split that does not is refused, and the run stays in the v4 format
    /// rather than gaining a head that does not reconstruct it. **A v5 run must split**: a
    /// successful step that returns `None` on a run whose head is current is a refused split, so
    /// the commit demotes the run to the whole v4 state (it does not write the transcript into the
    /// head, which the next join would discard).
    ///
    /// **A wrapper that delegates [`init`](Self::init) must delegate this too** (and
    /// [`join`](Self::join), and [`init_continuing`](Self::init_continuing)): a new run of a wrapper
    /// that does not silently stays in the v4 format, and a run that is already v5 is demoted to it.
    fn split(&self, state: &Self::State) -> Option<(Self::State, Vec<Value>)> {
        let _ = state;
        None
    }

    /// `head` with `rows` as its transcript: the inverse of [`split`](Self::split).
    ///
    /// The invariant is `join(split(s).0, split(s).1)` re-encodes to the same JSON as `s`, for
    /// every state the agent can produce. The default returns `head` and ignores `rows`, which is
    /// right exactly when [`split`](Self::split) is the default.
    fn join(&self, head: Self::State, rows: Vec<Value>) -> Result<Self::State, AgentError> {
        let _ = rows;
        Ok(head)
    }

    /// Called once after a cancel has committed `run` as `Failed` (`Runtime::cancel`, a parent
    /// that ended, a time limit, a cancel from the A2A front), with the agent state that commit
    /// holds as JSON, so an agent can tell a system it started elsewhere (a remote task) to stop.
    ///
    /// Best effort: it runs on a spawned task in the process that committed the cancel, only where
    /// the full agent is registered (never for a start-only registration), it is not retried, and
    /// the process may end before it finishes. A step that was running when the cancel landed has
    /// committed nothing, so `state` does not hold what that step did. The default does nothing.
    async fn on_cancelled(&self, run: RunId, state: serde_json::Value) {
        let _ = (run, state);
    }
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
/// JSON). The same goes for [`init_continuing`](Self::init_continuing) and
/// [`Agent::init_continuing`]. A run of a name that only a starter is registered
/// for is never claimed by this runtime's workers.
pub trait AgentStarter: Send + Sync + 'static {
    /// The state a new run starts with, stored as JSON. It is the same type as
    /// the [`Agent::State`] of the agent that steps the run, and so it decodes
    /// too: a run that continues another starts from the prior run's stored
    /// state.
    type State: Serialize + DeserializeOwned + Send + Sync;

    /// The journal epoch this starter's agent writes into a run it starts ([ADR 0076]).
    ///
    /// The starter side of [`Agent::replay_epoch`]: a front that only starts runs (an A2A control
    /// plane) has no `Agent`, and the run it starts must still record the epoch. `0` by default.
    ///
    /// [ADR 0076]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0076-a-run-records-the-journal-epoch-it-started-on.md
    fn replay_epoch(&self) -> u32 {
        0
    }

    /// Stable name, stored as `RunRecord::agent`. Equal to the name of the
    /// [`Agent`] that steps these runs.
    fn name(&self) -> &str;

    /// Initial state for a new run.
    fn init(&self, input: Inbound) -> Result<Self::State, AgentError>;

    /// Initial state for a new run that continues run `prior_run`: the start-only half of
    /// [`Agent::init_continuing`], and it must return what that does. The default ignores
    /// `prior` and returns `self.init(input)`.
    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Self::State,
        prior_run: RunId,
    ) -> Result<Self::State, AgentError> {
        let _ = (prior, prior_run);
        self.init(input)
    }

    /// The start-only half of [`Agent::split`]: it must return what the agent of the same name
    /// does, or a run it starts is never split. The default is [`Agent::split`]'s: no split.
    ///
    /// **A wrapper that delegates [`init`](Self::init) must delegate this too** (and
    /// [`join`](Self::join), and [`init_continuing`](Self::init_continuing)): a new run of a wrapper
    /// that does not silently stays in the v4 format, and a run that is already v5 is demoted to it.
    fn split(&self, state: &Self::State) -> Option<(Self::State, Vec<Value>)> {
        let _ = state;
        None
    }

    /// The start-only half of [`Agent::join`]: the inverse of [`split`](Self::split). The default
    /// returns `head`, as [`Agent::join`]'s does.
    fn join(&self, head: Self::State, rows: Vec<Value>) -> Result<Self::State, AgentError> {
        let _ = rows;
        Ok(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_core::RunId;

    #[derive(Debug, thiserror::Error)]
    #[error("lower")]
    struct Lower;

    /// Implements `init` only: `init_continuing` is the default.
    struct Echo;

    #[async_trait]
    impl Agent for Echo {
        type State = Value;

        fn name(&self) -> &str {
            "echo"
        }

        fn init(&self, input: Inbound) -> Result<Value, AgentError> {
            Ok(input.payload)
        }

        async fn step(&self, _: &mut Ctx, s: Value) -> Result<Transition<Value>, AgentError> {
            Ok(Transition::Continue(s))
        }
    }

    impl AgentStarter for Echo {
        type State = Value;

        fn name(&self) -> &str {
            "echo"
        }

        fn init(&self, input: Inbound) -> Result<Value, AgentError> {
            Ok(input.payload)
        }
    }

    #[test]
    fn the_default_continuation_is_init_and_ignores_the_prior_state() {
        let input = Inbound::new("message", serde_json::json!({"text": "second"}));
        let prior = serde_json::json!({"text": "first"});
        let run = RunId::new();
        assert_eq!(
            Agent::init_continuing(&Echo, input.clone(), &prior, run).unwrap(),
            Agent::init(&Echo, input.clone()).unwrap()
        );
        assert_eq!(
            AgentStarter::init_continuing(&Echo, input.clone(), &prior, run).unwrap(),
            AgentStarter::init(&Echo, input).unwrap()
        );
    }

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
                retry_after: Some(_),
                class: ErrorClass::Transient,
                ..
            } => ErrorClass::RateLimited,
            AgentError::Transient { class, .. } => *class,
            AgentError::Permanent { .. } => ErrorClass::Invalid,
            AgentError::Rejected { .. } => ErrorClass::Rejected,
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
            AgentError::rejected("x"),
        ];
        for e in &samples {
            assert_eq!(e.class(), expected(e), "{e}");
        }
        let retryable: Vec<bool> = samples.iter().map(Classify::is_retryable).collect();
        assert_eq!(
            retryable,
            [true, true, false, false, false, true, false, false, false]
        );
        assert_eq!(samples[1].retry_after(), Some(Duration::from_secs(5)));
        assert_eq!(samples[0].retry_after(), None);
    }

    /// A refusal is its own class: a person's message a policy refused is not a bug (`Invalid`) and
    /// not something a retry fixes, so a caller can tell the two apart with `Classify` alone.
    #[test]
    fn a_refusal_is_classified_rejected_and_a_permanent_failure_is_not() {
        let refusal = AgentError::rejected("not this task's to do");
        assert_eq!(refusal.class(), ErrorClass::Rejected);
        assert!(!refusal.is_retryable());
        assert_ne!(refusal.class(), AgentError::permanent("x").class());
        assert_eq!(
            adam_error::report(&refusal),
            "refused: not this task's to do",
            "the reason a person reads is the error's own text"
        );
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

        let e = AgentError::from_classified("lost a race", Classified(ErrorClass::Conflict, None));
        assert_eq!(
            e.class(),
            ErrorClass::Conflict,
            "the class is kept, not only retryability"
        );
        let e = AgentError::from_classified(
            "lost a race",
            Classified(ErrorClass::Conflict, Some(Duration::from_secs(1))),
        );
        assert_eq!(
            e.class(),
            ErrorClass::Conflict,
            "a hint does not make a conflict a rate limit"
        );
        let e = AgentError::from_classified("down", Classified(ErrorClass::Transient, None))
            .with_retry("fetch", RetryPolicy::new(2));
        let AgentError::Transient {
            retry: Some(retry),
            class,
            ..
        } = &e
        else {
            panic!("{e:?}");
        };
        assert_eq!(
            (retry.key.as_str(), retry.policy.max_attempts, *class),
            ("fetch", 2, ErrorClass::Transient)
        );
        assert!(matches!(
            AgentError::permanent("x").with_retry("fetch", RetryPolicy::new(2)),
            AgentError::Permanent { .. }
        ));

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
