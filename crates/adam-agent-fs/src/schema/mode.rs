//! The frontmatter of a mode: `modes/<name>.md`, the body is the mode's prompt (ADR 0039,
//! decision 3).
//!
//! A mode narrows the agent: its `tools:` is the allowlist of the tools the model is shown while
//! it is in force, and its body is added to the agent's instructions. `permission:` is refused
//! until the rules of ADR 0039 decision 4 are built (see [`crate::Mode`]).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::agent::ToolList;

/// The frontmatter of a mode file (`modes/<name>.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ModeFrontmatter {
    /// What a client shows: at most [`MODE_TITLE_MAX`](crate::MODE_TITLE_MAX) characters.
    /// Default: the mode's name.
    pub title: Option<String>,
    /// One line on what the mode does, at most
    /// [`MODE_DESCRIPTION_MAX`](crate::MODE_DESCRIPTION_MAX) characters. Required: it is what a
    /// client shows beside the name.
    pub description: Option<String>,
    /// The tools the model is shown while the mode is in force: a list or a comma-separated
    /// string, as an agent's `tools:`. Absent: the agent's own list, unchanged. A plain name the
    /// agent does not have is left out with a warning (a deployment's setting decides some tools);
    /// a pattern that matches nothing is a bind error.
    pub tools: Option<ToolList>,
    /// `true` on the one mode the agent works in when a message names none. Exactly one mode of a
    /// folder says it.
    pub default: Option<bool>,
    /// Refused until the rules of ADR 0039 decision 4 are built: a mode's rules could only
    /// narrow, and nothing reads them yet.
    pub permission: Option<Value>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
