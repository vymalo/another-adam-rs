//! A frozen copy of the v4 envelope: the shape every `state` written in the expand release must
//! still decode.
//!
//! It is deliberately a **copy**, not the runtime's `Envelope`: a member added to the real one
//! does not appear here, so this fixture says what a v4 process reads. The expand release wrote
//! this shape beside the v5 head; the contract release writes the tombstone (`state = null`) and
//! the head, so this fixture rejects what a new run stores, and a pre-contract run still decodes —
//! the compatibility boundary made executable.

#![allow(dead_code)] // one case includes this module; the other cases do not use it

use serde::Deserialize;
use serde_json::Value;

/// The v4 envelope, frozen.
#[derive(Debug, Deserialize)]
pub struct V4Envelope {
    /// Layout version: `1` is the v4 envelope, and this fixture accepts nothing else.
    pub v: u32,
    /// The agent's own state, whole: a v4 worker steps it.
    pub agent: Value,
    #[serde(default)]
    pub inbox: Vec<Value>,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<Value>,
    #[serde(default)]
    pub deadline: Option<Value>,
    #[serde(default)]
    pub limit_secs: Option<u64>,
    #[serde(default)]
    pub remaining_secs: Option<u64>,
    #[serde(default)]
    pub agent_wake: Option<Value>,
    #[serde(default)]
    pub hb: bool,
}

impl V4Envelope {
    /// Decode `state` as the v4 envelope. `Err` for a v5 head (its `v` is `5`) and for a document
    /// that does not have the v4 shape.
    pub fn decode(state: &Value) -> Result<Self, String> {
        let envelope: Self = serde_json::from_value(state.clone()).map_err(|e| e.to_string())?;
        if envelope.v != 1 {
            return Err(format!("not a v4 envelope: v = {}", envelope.v));
        }
        Ok(envelope)
    }
}
