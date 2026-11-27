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

/// How a run ended: what the store says (ADR 0085 decision 6).
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
