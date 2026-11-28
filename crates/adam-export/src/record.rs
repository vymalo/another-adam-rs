//! The exported record: one run, in the chat shape trainers read (ADR 0085 decision 2).

use std::collections::{BTreeMap, BTreeSet};

use adam_model::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `schema` of every record this crate writes. A new field is not a new schema; a changed
/// meaning is.
pub const SCHEMA: &str = "adam.export/v1";

/// One run as a training record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// The run's pseudonym ([`Pseudonyms`](crate::Pseudonyms)), never its id.
    pub id: String,
    /// The pseudonym of the run this one worked for, when it is a child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The pseudonym of the run's conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    /// The label of the [`RunSource`](crate::RunSource) the run was read from, as the operator
    /// named it; absent when the export reads one unnamed source, so such an export writes the
    /// lines it wrote before sources had labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The agent's name.
    pub agent: String,
    /// `runnable`, `parked`, `done` or `failed`.
    pub status: String,
    /// RFC 3339.
    pub created_at: String,
    /// RFC 3339.
    pub updated_at: String,
    /// Model calls the run made.
    pub turns: u32,
    /// Tool calls the run made.
    pub tool_calls: u32,
    /// The tokens of the run's model calls.
    pub usage: Usage,
    /// The model aliases the run's assistant turns name, sorted, and the fallback it switched to.
    pub models: Vec<String>,
    /// Whether the history was shortened or compacted: then `messages` are not what the model saw
    /// at every turn.
    pub compacted: bool,
    /// What the run touched, derived from its transcript ([`Facets`]).
    pub facets: Facets,
    /// How the run ended.
    pub outcome: Outcome,
    /// Where `tools` and the system message come from.
    pub tools_source: ToolsSource,
    /// Which messages carry the loss: `assistant` (every assistant message: a whole run) or
    /// `last` (only the last message: a per-turn sample). A record written before the field
    /// existed reads as `assistant`.
    #[serde(default)]
    pub train_on: TrainOn,
    /// Where a per-turn sample stands in its run ([`Samples::Turn`](crate::Samples::Turn)); absent
    /// from a whole-run record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Sample>,
    /// The conversation: an optional `system` message, then the history.
    pub messages: Vec<ChatMessage>,
    /// The tools the model was offered; empty for a chat with none, or when they are unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
}

/// One message, tagged by `role`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    /// The system prompt.
    System {
        /// Its text.
        content: String,
    },
    /// What the person (or a parent run) wrote.
    User {
        /// Its text; an image part is the text `[image]`.
        content: String,
    },
    /// What the model wrote.
    Assistant {
        /// Its text, empty when it only called tools.
        content: String,
        /// The tools it called.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ChatToolCall>,
        /// Its reasoning, when the run kept it and the export asked for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },
    /// A tool's result.
    Tool {
        /// The call it answers.
        tool_call_id: String,
        /// What the tool returned, or its error text.
        content: String,
    },
}

/// A tool call: `{id, type: "function", function: {name, arguments}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatToolCall {
    /// The id its result names.
    pub id: String,
    /// Always `function`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The call.
    pub function: FunctionCall,
}

/// The function of a [`ChatToolCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The tool's name.
    pub name: String,
    /// The arguments as a JSON text (the model's own text when it was not JSON).
    pub arguments: String,
}

/// A tool definition: `{type: "function", function: {name, description, parameters}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// Always `function`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The tool.
    pub function: FunctionDef,
}

/// The function of a [`ToolDef`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    /// The tool's name.
    pub name: String,
    /// What it does, for the model.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: Value,
}

/// Which messages of a record a trainer computes the loss on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrainOn {
    /// Every assistant message: a whole run, whose earlier answers were made from shorter
    /// prefixes of the same messages.
    #[default]
    Assistant,
    /// Only the last message, the answer of the sample's turn: the messages before it are what
    /// the model was sent.
    Last,
}

/// Where a per-turn sample stands in its run (ADR 0085, slice E5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// The model call the answer is from: [`Conversation::turns`](adam_llm_agent::Conversation::turns)
    /// when it was made, from 0.
    pub turn: u32,
    /// This sample's place among the run's samples, from 0, oldest first.
    pub index: u32,
    /// How many samples the run has (before a token budget left any out).
    pub of: u32,
    /// Whether `messages` and `tools` are what the model was sent at that call, as far as the
    /// state can tell (up to scrubbing, and images written `[image]`); `false` when a later
    /// shortening rewrote a message of the prefix, the tools are not the ones recorded for that
    /// turn, or the turn could not be placed for sure. See the crate README.
    pub exact: bool,
}

/// Where a record's `tools` and system message come from (ADR 0085 decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolsSource {
    /// Unknown: the record has no system message and no tools.
    None,
    /// The agent's current prompt and tools, passed by the export; they may differ from the
    /// ones the run had.
    Current,
    /// The ones the run's last model call was sent, recorded by the run.
    Recorded,
}

/// How a run ended: what the store says (ADR 0085 decision 6), and what the run delivered, read
/// from the artifacts of the outcome contract (slice E4, [`pull_requests_of`](crate::pull_requests_of)
/// and [`checks_of`](crate::checks_of)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// The run's status.
    pub status: String,
    /// The reason, for a failed run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The artifacts the run's tools produced, by name and media type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<OutcomeArtifact>,
    /// The pull requests the run reported, one each, as last reported; left out when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_requests: Vec<PullRequestOutcome>,
    /// The verdicts of the run's checks; left out when it reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<ChecksOutcome>,
}

impl Outcome {
    /// Whether the run delivered verified work: at least one pull request, and the head of each
    /// was verified by the run's checks. What [`Filter::verified_only`](crate::Filter::verified_only)
    /// keeps.
    pub fn verified(&self) -> bool {
        !self.pull_requests.is_empty() && self.pull_requests.iter().all(|pr| pr.verified)
    }
}

/// A pull request of an [`Outcome`]. Its URL, its repository's name and its commit are never
/// written: they name a person or an organisation and a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestOutcome {
    /// Its number in its repository.
    pub number: u64,
    /// The pseudonym of its repository (kind `repository`), when the run named it: the records
    /// of one repository share it, which keeps a repository out of a training split.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The run said the code at the pull request's head passed the run's checks; `false` when it
    /// said it did not (red checks a person accepted, or a failure the base has too) or did not
    /// say (a run from before slice E4).
    pub verified: bool,
}

/// The verdicts of a run's checks, in an [`Outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChecksOutcome {
    /// The last verdict passed.
    pub passed: bool,
    /// How many verdicts the run reported.
    pub reports: u32,
    /// How many of them failed.
    pub failed: u32,
}

/// An artifact of an [`Outcome`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeArtifact {
    /// Its name (the coder's `pull_request`, a file's name).
    pub name: String,
    /// Its media type, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// What a run touched, by kind (`lang`, `dep`), derived from its transcript (ADR 0085 decision
/// 5). A heuristic, not a fact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Facets(pub BTreeMap<String, BTreeSet<String>>);

impl Facets {
    /// Add `value` under `kind`.
    pub fn insert(&mut self, kind: &str, value: impl Into<String>) {
        self.0
            .entry(kind.to_owned())
            .or_default()
            .insert(value.into());
    }

    /// Whether `value` is under `kind`.
    pub fn has(&self, kind: &str, value: &str) -> bool {
        self.0
            .get(kind)
            .is_some_and(|values| values.contains(value))
    }

    /// The values under `kind`, sorted.
    pub fn get(&self, kind: &str) -> impl Iterator<Item = &str> {
        self.0.get(kind).into_iter().flatten().map(String::as_str)
    }
}
