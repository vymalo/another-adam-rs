//! The versioned JSON envelope stored in `RunRecord::state`.
//!
//! Its layout is private to the runtime. Current version: 1.

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
        }
    }

    pub fn decode(run: RunId, state: &Value) -> Result<Self, RuntimeError> {
        let corrupt = |reason: String| RuntimeError::Corrupt { run, reason };
        match state.get("v").and_then(Value::as_u64) {
            Some(v) if v == u64::from(VERSION) => {}
            Some(v) => return Err(corrupt(format!("unsupported envelope version {v}"))),
            None => return Err(corrupt("missing envelope version".into())),
        }
        serde_json::from_value(state.clone()).map_err(|e| corrupt(e.to_string()))
    }

    pub fn encode(&self) -> Result<Value, StoreError> {
        serde_json::to_value(self).map_err(|e| StoreError::InvalidInput(e.to_string()))
    }
}
