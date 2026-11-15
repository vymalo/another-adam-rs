//! The frontmatter of a command: `commands/<name>.md`, the body is the template.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// The frontmatter of a command file (ADR 0044, decision 9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CommandFrontmatter {
    /// What the command does: at most 200 characters. Required.
    pub description: Option<String>,
    /// What the person types after the name (`<file>`), shown by a client. Optional.
    #[serde(rename = "argument-hint")]
    pub argument_hint: Option<String>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
