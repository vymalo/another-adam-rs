//! The versioned JSON envelope stored in `RunRecord::state`.
//!
//! Its layout is private to the runtime. Current version: 1.

use adam_core::time::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use adam_core::{RunId, StoreError};

use crate::agent::Inbound;
use crate::events::Artifact;
use crate::retry::RetryRecord;
use crate::runtime::RuntimeError;

pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Envelope {
    /// Layout version.
    pub v: u32,
    /// The agent's own state.
    ///
    /// `transition` takes this by move before stepping, leaving `Null` in the envelope it still
    /// holds. That envelope is never encoded (a commit builds a fresh one from `Next`), and a
    /// reader of a stepped envelope must not treat `Null` as the agent's state.
    pub agent: Value,
    /// Delivered, not yet consumed messages, oldest first.
    #[serde(default)]
    pub inbox: Vec<Inbound>,
    /// Next journal seq at the last commit.
    #[serde(default)]
    pub seq: u64,
    /// Consecutive failed tries of the current transition.
    #[serde(default)]
    pub attempt: u32,
    /// What a failing run recorded of its retries: the failures per key and each key's policy
    /// (ADR 0061). Cleared by a transition that succeeds; absent from state written before it
    /// existed, and not written while `None`; one that does not decode reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::retry::lenient_record"
    )]
    pub retry: Option<RetryRecord>,
    /// Bumped by every commit a worker makes (never by `deliver`/`cancel`), so
    /// a worker can tell "only messages arrived" from "someone else advanced".
    #[serde(default)]
    pub rev: u64,
    /// Result of a `Done` run (`null` otherwise).
    #[serde(default)]
    pub output: Value,
    /// Reason of a `Failed` run.
    #[serde(default)]
    pub error: Option<String>,
    /// Artifacts emitted so far, committed with their transitions.
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    /// When the run must have finished, set once from the runtime's own clock when a child is
    /// started with a time limit ([`Runtime::start_child_within`](crate::Runtime::start_child_within)).
    /// Past it the worker fails the run (`cancelled: time limit: ...`), before a step and while one
    /// runs, and a parked run is woken at it. Absent from state written before it existed, and
    /// not written while `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<Timestamp>,
    /// The limit [`deadline`](Self::deadline) was set from, in seconds: what the failure says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_secs: Option<u64>,
    /// What is left of the time limit while the run waits for a person (it parked with no timer):
    /// the deadline is cleared and this is kept, and the next step that follows a message sets the
    /// deadline again from it. Time spent on a timer or on a child still counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
    /// When the agent itself asked to be woken, as it parked (`None`: it waits for a message). The
    /// stored `wake_at` of a watched run can be earlier, so that the runtime looks at the run's
    /// parent and deadline in between.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_wake: Option<Timestamp>,
    /// The runtime set the wake of this parked run (it works for a parent or has a deadline): a claim
    /// that is not the agent's own wake only looks at the parent and the deadline, and parks again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hb: bool,
}

impl Envelope {
    pub fn new(agent: Value) -> Self {
        Self {
            v: VERSION,
            agent,
            inbox: Vec::new(),
            seq: 0,
            attempt: 0,
            retry: None,
            rev: 0,
            output: Value::Null,
            error: None,
            artifacts: Vec::new(),
            deadline: None,
            limit_secs: None,
            remaining_secs: None,
            agent_wake: None,
            hb: false,
        }
    }

    /// Give the run a time limit of `within`, counted from `now`.
    pub fn with_limit(mut self, now: Timestamp, within: std::time::Duration) -> Self {
        self.deadline = Some(now + adam_core::time::span_of(within));
        self.limit_secs = Some(within.as_secs());
        self
    }

    pub fn decode(run: RunId, state: &Value) -> Result<Self, RuntimeError> {
        let corrupt = |reason: String| RuntimeError::Corrupt {
            run,
            reason,
            source: None,
        };
        match state.get("v").and_then(Value::as_u64) {
            Some(v) if v == u64::from(VERSION) => {}
            Some(v) => return Err(corrupt(format!("unsupported envelope version {v}"))),
            None => return Err(corrupt("missing envelope version".into())),
        }
        // `&Value` is a deserialiser: no copy of the state is made before it is read.
        let decoded = Self::deserialize(state).map_err(|e| RuntimeError::Corrupt {
            run,
            reason: "the envelope does not decode".into(),
            source: Some(Box::new(e)),
        });
        #[cfg(test)]
        if decoded.is_ok() {
            decodes::bump();
        }
        decoded
    }

    pub fn encode(&self) -> Result<Value, StoreError> {
        serde_json::to_value(self)
            .map_err(|e| StoreError::InvalidInput(format!("the envelope does not serialize: {e}")))
    }
}

/// How many envelopes a thread decoded, in tests: what a test asserts a listing does not do.
#[cfg(test)]
pub(crate) mod decodes {
    use std::cell::Cell;

    thread_local! {
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }

    /// Note one decode.
    pub(crate) fn bump() {
        COUNT.with(|count| count.set(count.get() + 1));
    }

    /// How many decodes this thread took, resetting the count.
    pub(crate) fn take() -> usize {
        COUNT.with(|count| count.replace(0))
    }

    /// How many decodes this thread took, leaving the count.
    pub(crate) fn count() -> usize {
        COUNT.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_envelope_written_before_deadlines_decodes_without_one() {
        let old = serde_json::json!({
            "v": 1, "agent": {"x": 1}, "inbox": [], "seq": 3, "attempt": 0, "rev": 2,
            "output": null, "error": null, "artifacts": []
        });
        let env = Envelope::decode(RunId::new(), &old).expect("decodes");
        assert_eq!((env.deadline, env.limit_secs), (None, None));
        // And one with no deadline is written without the members.
        let again = env.encode().expect("encodes");
        assert!(again.get("deadline").is_none() && again.get("limit_secs").is_none());
    }

    #[test]
    fn a_deadline_survives_the_round_trip() {
        let now = Timestamp::now();
        let env = Envelope::new(serde_json::json!({}))
            .with_limit(now, std::time::Duration::from_secs(1800));
        let back = Envelope::decode(RunId::new(), &env.encode().unwrap()).unwrap();
        assert_eq!(
            back.deadline,
            Some(now + jiff::SignedDuration::from_secs(1800))
        );
        assert_eq!(back.limit_secs, Some(1800));
    }

    /// A deadline at `.120` keeps chrono's three digits in the stored state: jiff's own serde
    /// would write `.12`, and a state written by either build must re-encode to itself.
    #[test]
    fn a_deadline_at_a_fraction_of_a_millisecond_keeps_its_bytes() {
        let at: Timestamp = "2026-01-02T03:04:05.120Z".parse().expect("a timestamp");
        let env =
            Envelope::new(serde_json::json!({})).with_limit(at, std::time::Duration::from_secs(60));
        let state = env.encode().expect("encodes");
        assert_eq!(state["deadline"], "2026-01-02T03:05:05.120Z");
        let back = Envelope::decode(RunId::new(), &state).expect("decodes");
        assert_eq!(
            back.deadline,
            Some(at + jiff::SignedDuration::from_secs(60))
        );
        assert_eq!(
            back.encode().expect("encodes again")["deadline"],
            "2026-01-02T03:05:05.120Z"
        );
    }

    /// A time limit too large for a signed duration saturates at the end of jiff's range instead
    /// of overflowing: the run still parks with a deadline far in the future.
    #[test]
    fn a_time_limit_too_large_for_jiff_parks_at_the_end_of_time() {
        let now = Timestamp::from_second(1_700_000_000).expect("in range");
        let env = Envelope::new(serde_json::json!({})).with_limit(now, std::time::Duration::MAX);
        assert_eq!(env.deadline, Some(Timestamp::MAX));
        assert_eq!(env.limit_secs, Some(u64::MAX));
        let back =
            Envelope::decode(RunId::new(), &env.encode().expect("encodes")).expect("decodes");
        assert_eq!(back.deadline, Some(Timestamp::MAX));
    }

    /// The state is decoded from a borrow and re-encoded as it was: every member, in the order the
    /// encoder writes them, with no key added or dropped.
    #[test]
    fn decode_then_encode_gives_the_same_bytes_back() {
        let now = Timestamp::now();
        let mut env = Envelope::new(serde_json::json!({
            "messages": [
                {"role": "user", "text": "hi"},
                {"role": "assistant", "text": "hello"},
            ],
            "artifacts": [],
        }))
        .with_limit(now, std::time::Duration::from_secs(90));
        env.inbox.push(
            crate::agent::Inbound::new("message", serde_json::json!({"text": "late"}))
                .with_id("m-1"),
        );
        env.seq = 7;
        env.attempt = 2;
        let mut record = RetryRecord::default();
        record.fail(
            "tool:search",
            &crate::RetryPolicy::new(3).with_multiplier(1.5),
        );
        record.fail("tool:search", &crate::RetryPolicy::new(9));
        env.retry = Some(record);
        env.rev = 3;
        env.output = serde_json::json!({"answer": 42});
        env.error = Some("not yet".into());
        env.artifacts.push(Artifact::new(
            "plan",
            None,
            serde_json::json!({"steps": [1, 2]}),
        ));
        env.remaining_secs = Some(60);
        env.agent_wake = Some(now);
        env.hb = true;
        let state = env.encode().expect("encodes");
        let bytes = serde_json::to_string(&state).expect("serialises");

        let decoded = Envelope::decode(RunId::new(), &state).expect("decodes");
        let again = decoded.encode().expect("encodes again");
        assert_eq!(
            serde_json::to_string(&again).expect("serialises"),
            bytes,
            "decode then encode is the identity on the bytes"
        );
        // And the content survived, member by member.
        assert_eq!(decoded.agent, env.agent);
        assert_eq!(decoded.inbox, env.inbox);
        assert_eq!(decoded.artifacts, env.artifacts);
        assert_eq!(decoded.output, env.output);
        assert_eq!(decoded.error, env.error);
        assert_eq!(decoded.retry, env.retry);
        assert_eq!(
            (decoded.deadline, decoded.limit_secs),
            (env.deadline, env.limit_secs)
        );
        assert_eq!(
            (decoded.remaining_secs, decoded.agent_wake, decoded.hb),
            (env.remaining_secs, env.agent_wake, env.hb)
        );
    }

    /// The retry record is stored plainly (ADR 0061, decision 7), and one that does not decode is
    /// ignored rather than failing the run: the next failure counts afresh.
    #[test]
    fn the_retry_record_is_plain_and_one_that_does_not_decode_is_ignored() {
        let mut env = Envelope::new(serde_json::json!({}));
        let mut record = RetryRecord::default();
        let (n, policy) = record.fail(
            "tool:pay",
            &crate::RetryPolicy::new(2)
                .with_backoff(
                    std::time::Duration::from_millis(1500),
                    std::time::Duration::from_secs(30),
                )
                .with_retry_on(crate::RetryClasses::of([
                    adam_error::ErrorClass::RateLimited,
                ])),
        );
        assert_eq!((n, policy.max_attempts), (1, 2));
        env.retry = Some(record);
        let state = env.encode().expect("encodes");
        assert_eq!(
            state["retry"],
            serde_json::json!({
                "failures": {"tool:pay": 1},
                "policies": {"tool:pay": {
                    "max_attempts": 2, "initial_backoff_ms": 1500, "max_backoff_ms": 30000,
                    "multiplier": 2.0, "retry_on": ["rate-limited"]
                }}
            })
        );

        let mut garbled = state.clone();
        garbled["retry"] = serde_json::json!({"failures": "many"});
        let back = Envelope::decode(RunId::new(), &garbled).expect("the run still decodes");
        assert_eq!(back.retry, None);

        // A class this build does not know is dropped: the policy retries less, never more.
        let mut newer = state;
        newer["retry"]["policies"]["tool:pay"]["retry_on"] =
            serde_json::json!(["rate-limited", "overloaded"]);
        let mut back = Envelope::decode(RunId::new(), &newer)
            .expect("decodes")
            .retry
            .expect("kept");
        let (n, policy) = back.fail("tool:pay", &crate::RetryPolicy::new(7));
        assert_eq!(
            (n, policy.max_attempts),
            (2, 2),
            "the recorded policy, not the one asked"
        );
        assert_eq!(policy.retry_on.names(), ["rate-limited"]);
        assert_eq!(
            policy.initial_backoff,
            std::time::Duration::from_millis(1500)
        );
    }

    /// A state written before the optional members existed keeps decoding and re-encodes to the same
    /// bytes: the version check accepts it and the defaults are left out again.
    #[test]
    fn an_old_state_re_encodes_to_itself() {
        let old = serde_json::json!({
            "v": 1, "agent": {"x": 1}, "inbox": [], "seq": 3, "attempt": 0, "rev": 2,
            "output": null, "error": null, "artifacts": []
        });
        let bytes = serde_json::to_string(&old).expect("serialises");
        let env = Envelope::decode(RunId::new(), &old).expect("decodes");
        assert_eq!(
            serde_json::to_string(&env.encode().expect("encodes")).expect("serialises"),
            bytes
        );
    }
}
