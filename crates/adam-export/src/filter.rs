//! Which records an export writes.

use adam_core::store::RunStatus;
use adam_core::time::Timestamp;

use crate::{ChatMessage, Record};

/// Which runs an export writes. [`Filter::default`] is a training export's: finished runs that
/// were not compacted and have at least one assistant turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// Only runs in one of these statuses; empty for every status. `[Done]` by default.
    pub statuses: Vec<RunStatus>,
    /// Only runs last updated at or after this instant.
    pub since: Option<Timestamp>,
    /// Only runs last updated before this instant.
    pub until: Option<Timestamp>,
    /// Facets every record must have, as `(kind, value)`: `("lang", "rust")`, `("dep", "axum")`.
    pub require: Vec<(String, String)>,
    /// Keep runs whose history was compacted. `false` by default.
    pub include_compacted: bool,
    /// The fewest model calls a run must have made. `1` by default.
    pub min_turns: u32,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            statuses: vec![RunStatus::Done],
            since: None,
            until: None,
            require: Vec::new(),
            include_compacted: false,
            min_turns: 1,
        }
    }
}

impl Filter {
    /// Also require the facet `kind=value`.
    #[must_use]
    pub fn require(mut self, kind: impl Into<String>, value: impl Into<String>) -> Self {
        self.require.push((kind.into(), value.into()));
        self
    }

    /// Only runs in `statuses` (empty for every status).
    #[must_use]
    pub fn statuses(mut self, statuses: impl IntoIterator<Item = RunStatus>) -> Self {
        self.statuses = statuses.into_iter().collect();
        self
    }

    /// Parse a requirement written `kind=value` (`lang=rust`, `dep=cratestack`).
    ///
    /// # Errors
    ///
    /// The text, when it is not `kind=value` with both sides non-empty.
    pub fn parse_requirement(text: &str) -> Result<(String, String), String> {
        match text.split_once('=') {
            Some((kind, value)) if !kind.trim().is_empty() && !value.trim().is_empty() => {
                Ok((kind.trim().to_owned(), value.trim().to_ascii_lowercase()))
            }
            _ => Err(format!("{text:?} is not kind=value (lang=rust, dep=axum)")),
        }
    }

    /// Whether the run's status and time pass, before its state is read.
    pub fn admits(&self, status: RunStatus, updated_at: Timestamp) -> bool {
        within(&self.statuses, self.since, self.until, status, updated_at)
    }

    /// Whether `record` is written.
    pub fn accepts(&self, record: &Record) -> bool {
        let answered = record
            .messages
            .iter()
            .any(|message| matches!(message, ChatMessage::Assistant { .. }));
        answered
            && record.turns >= self.min_turns
            && (self.include_compacted || !record.compacted)
            && self
                .require
                .iter()
                .all(|(kind, value)| record.facets.has(kind, value))
    }
}

/// Whether `status` is one of `statuses` (any, when empty) and `updated_at` is in `[since, until)`:
/// the part of a [`Filter`] a [`SourceQuery`](crate::SourceQuery) carries.
pub(crate) fn within(
    statuses: &[RunStatus],
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    status: RunStatus,
    updated_at: Timestamp,
) -> bool {
    (statuses.is_empty() || statuses.contains(&status))
        && since.is_none_or(|since| updated_at >= since)
        && until.is_none_or(|until| updated_at < until)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_requirement_is_kind_equals_value() {
        assert_eq!(
            Filter::parse_requirement(" dep = CrateStack "),
            Ok(("dep".to_owned(), "cratestack".to_owned()))
        );
        assert!(Filter::parse_requirement("rust").is_err());
        assert!(Filter::parse_requirement("lang=").is_err());
    }

    #[test]
    fn the_default_keeps_finished_runs_only() {
        let now = Timestamp::now();
        let filter = Filter::default();
        assert!(filter.admits(RunStatus::Done, now));
        assert!(!filter.admits(RunStatus::Failed, now));
        assert!(
            Filter::default()
                .statuses([])
                .admits(RunStatus::Parked, now)
        );
        let later = Filter {
            since: Some(now),
            ..Filter::default()
        };
        let before = Timestamp::from_second(now.as_second() - 1).expect("a second ago");
        assert!(!later.admits(RunStatus::Done, before));
        assert!(later.admits(RunStatus::Done, now));
        let until = Filter {
            until: Some(now),
            ..Filter::default()
        };
        assert!(until.admits(RunStatus::Done, before));
        assert!(!until.admits(RunStatus::Done, now));
    }
}
