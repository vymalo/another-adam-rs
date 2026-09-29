//! The frontmatter of a schedule: `schedules/<name>.md`, the body is the prompt.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// The frontmatter of a schedule file (roadmap 5; parsed and validated here, run later).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ScheduleFrontmatter {
    /// A five-field cron expression.
    pub cron: Option<String>,
    /// An IANA time zone name. Default: `UTC`.
    pub timezone: Option<String>,
    /// The agent to run, in a multi-agent package.
    pub agent: Option<String>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
