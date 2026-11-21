//! The frontmatter of a schedule: `schedules/<name>.md`, the body is the prompt.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// The frontmatter of a schedule file: exactly one of `cron`, `at` or `after` (ADR 0060).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ScheduleFrontmatter {
    /// A five-field cron expression: the schedule repeats.
    pub cron: Option<String>,
    /// One time, RFC 3339 or a civil date and time in `timezone`: the schedule fires once.
    pub at: Option<String>,
    /// One duration (`30m`, `1h30m`) from when the schedule starts: it fires once.
    pub after: Option<String>,
    /// An IANA time zone name. Default: `UTC`.
    pub timezone: Option<String>,
    /// The agent to run, in a multi-agent package.
    pub agent: Option<String>,
    /// Keys this schema does not know.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
