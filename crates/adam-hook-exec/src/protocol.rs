//! The `adam-hooks/1` wire format: one JSON object per line, on the program's stdin and stdout.
//!
//! The adapter writes a [`Hello`], then one [`Outgoing::Event`] per hook call and one
//! [`Outgoing::Shutdown`] at the end. The program answers the hello with a [`Hello`] of its own and
//! every event with a [`Reply`], in any order, matching the event's `id`. A member this build does
//! not know is ignored; a line that is not a JSON object is a broken program.
//!
//! The protocol page is `docs/reference/hooks-protocol.md`; this module is the reference
//! implementation of it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The protocol's name, as both sides say it in the hello.
pub const PROTOCOL: &str = "adam-hooks/1";

/// The version of [`PROTOCOL`], sent in the hello.
pub const VERSION: u32 = 1;

/// The longest line either side may write, in bytes, the terminator excluded. A longer line is a
/// broken program.
pub const LINE_MAX: usize = 256 * 1024;

/// Whether an event asks the program to decide or only to look (ADR 0050, decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The program is asked to decide: it answers a verdict.
    Decide,
    /// The program is told what happened: it answers `continue` and changes nothing.
    Observe,
}

/// The verdict word of a [`Reply`] (ADR 0050, decision 15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Word {
    /// Go on, with no change.
    Continue,
    /// Go on, with the rewrite the reply carries.
    Rewrite,
    /// Refuse the call, with the reason the reply carries.
    Deny,
    /// Ask a person first, with the question the reply carries.
    Ask,
    /// Stop the run, with the reason the reply carries.
    Stop,
}

impl Word {
    /// The word as the protocol writes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Word::Continue => "continue",
            Word::Rewrite => "rewrite",
            Word::Deny => "deny",
            Word::Ask => "ask",
            Word::Stop => "stop",
        }
    }
}

/// What the adapter writes to the program.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    /// The first line: the protocol, this side's name and version, and the points the program is
    /// asked at. The program answers with its own [`Hello`].
    Hello {
        /// [`PROTOCOL`].
        protocol: String,
        /// The plugin's name, as the configuration names it.
        name: String,
        /// [`VERSION`].
        version: u32,
        /// The points this program is asked at.
        points: WirePoints,
    },
    /// One hook call: `decide` at a deciding point, `observe` for an event.
    Event {
        /// Matches the reply. Unique for the life of the process.
        id: String,
        /// [`Mode::Decide`] or [`Mode::Observe`].
        mode: Mode,
        /// The request's key: `<plugin>/<point>/<id>`. Stable for one request, and a replay sends
        /// no request at all (the loop replays the recorded verdict).
        key: String,
        /// The point's name, e.g. `tool.before`.
        point: String,
        /// How long the program has to answer, in milliseconds.
        deadline_ms: u64,
        /// The point's payload, scrubbed.
        payload: Value,
    },
    /// The last line: the program may exit. A program that does not is killed.
    Shutdown,
}

/// The points a program is asked at, as the hello carries them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WirePoints {
    /// The deciding points it is asked at.
    pub decide: Vec<String>,
    /// The events it observes: exact names or `ns.*` patterns.
    pub observe: Vec<String>,
}

/// The program's hello, its answer to the adapter's.
#[derive(Debug, Clone, Deserialize)]
pub struct Hello {
    /// The protocol the program speaks; anything but [`PROTOCOL`] fails the start.
    pub protocol: String,
    /// The program's name, for a log line.
    #[serde(default)]
    pub name: Option<String>,
    /// The program's version, for a log line.
    #[serde(default)]
    pub version: Option<u32>,
}

/// The program's answer to one event.
///
/// Which rewrite member a point reads is the point's: `arguments` for `tool.before`, `text` for
/// `tool.after` and `message.received`, `context` for `compaction.before`, and the sampling members
/// for `model.request`. A rewrite without its point's member is refused.
#[derive(Debug, Clone, Deserialize)]
pub struct Reply {
    /// The id of the event this answers.
    pub id: String,
    /// The verdict.
    pub verdict: Word,
    /// Why, for `deny`, `ask` and `stop`; empty for the others.
    #[serde(default)]
    pub reason: Option<String>,
    /// The rewritten text (`tool.after`, `message.received`).
    #[serde(default)]
    pub text: Option<String>,
    /// The rewritten arguments (`tool.before`).
    #[serde(default)]
    pub arguments: Option<Value>,
    /// The added context (`compaction.before`).
    #[serde(default)]
    pub context: Option<String>,
    /// The rewritten sampling temperature (`model.request`).
    #[serde(default)]
    pub temperature: Option<f32>,
    /// The rewritten nucleus sampling mass (`model.request`).
    #[serde(default)]
    pub top_p: Option<f32>,
    /// The rewritten bound on generated tokens (`model.request`).
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// The rewritten metadata (`model.request`).
    #[serde(default)]
    pub metadata: Option<BTreeMap<String, String>>,
    /// The rewritten headers (`model.request`).
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// The rewritten extra body (`model.request`).
    #[serde(default)]
    pub extra_body: Option<Map<String, Value>>,
    /// Actions the program asks the loop to perform. No action is built yet (ADR 0050, slice
    /// H1d), so a non-empty list refuses the call with class `Unsupported` rather than being
    /// silently dropped.
    #[serde(default)]
    pub actions: Vec<Value>,
}

impl Reply {
    /// The reason, or the empty string: a verdict that carries one always does.
    #[must_use]
    pub fn reason(&self) -> String {
        self.reason.clone().unwrap_or_default()
    }
}
