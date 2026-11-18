//! The versioned JSON envelope stored in `RunRecord::state`.
//!
//! Its layout is private to the runtime. Current version: 1.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use adam_core::{RunId, StoreError};

use crate::agent::Inbound;
use crate::events::Artifact;
use crate::runtime::RuntimeError;

pub(crate) const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub deadline: Option<DateTime<Utc>>,
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
    pub agent_wake: Option<DateTime<Utc>>,
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
    pub fn with_limit(mut self, now: DateTime<Utc>, within: std::time::Duration) -> Self {
        let span = chrono::Duration::from_std(within).unwrap_or(chrono::Duration::MAX);
        self.deadline = Some(
            now.checked_add_signed(span)
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        );
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
        let now = Utc::now();
        let env = Envelope::new(serde_json::json!({}))
            .with_limit(now, std::time::Duration::from_secs(1800));
        let back = Envelope::decode(RunId::new(), &env.encode().unwrap()).unwrap();
        assert_eq!(back.deadline, Some(now + chrono::Duration::seconds(1800)));
        assert_eq!(back.limit_secs, Some(1800));
    }

    /// The state is decoded from a borrow and re-encoded as it was: every member, in the order the
    /// encoder writes them, with no key added or dropped.
    #[test]
    fn decode_then_encode_gives_the_same_bytes_back() {
        let now = Utc::now();
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
        assert_eq!(
            (decoded.deadline, decoded.limit_secs),
            (env.deadline, env.limit_secs)
        );
        assert_eq!(
            (decoded.remaining_secs, decoded.agent_wake, decoded.hb),
            (env.remaining_secs, env.agent_wake, env.hb)
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
