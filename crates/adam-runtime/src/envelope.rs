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
        serde_json::from_value(state.clone()).map_err(|e| RuntimeError::Corrupt {
            run,
            reason: "the envelope does not decode".into(),
            source: Some(Box::new(e)),
        })
    }

    pub fn encode(&self) -> Result<Value, StoreError> {
        serde_json::to_value(self)
            .map_err(|e| StoreError::InvalidInput(format!("the envelope does not serialize: {e}")))
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
}
