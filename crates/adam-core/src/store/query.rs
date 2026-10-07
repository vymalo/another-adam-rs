//! Listing runs: the one read that is not "this run by id".
//!
//! A2A's `ListTasks` returns a caller's own tasks, newest first, in pages. The store does the
//! scoping, the filtering it can express and the keyset pagination, so a page costs one indexed
//! read and never an offset.

use chrono::{DateTime, Utc};

use super::{RunId, RunStatus};

/// Whose conversations a listing covers. Required: there is no unscoped listing, so a listing
/// cannot leak the runs of another owner by leaving a filter out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationScope {
    /// Conversation ids that start with this text (an owner's whole namespace).
    Prefix(String),
    /// This conversation only.
    Exact(String),
}

impl ConversationScope {
    /// Whether `conversation_id` is in the scope.
    pub fn contains(&self, conversation_id: Option<&str>) -> bool {
        match (self, conversation_id) {
            (Self::Prefix(p), Some(c)) => c.starts_with(p.as_str()),
            (Self::Exact(e), Some(c)) => c == e,
            (_, None) => false,
        }
    }
}

/// What [`Store::list_runs`](super::Store::list_runs) and
/// [`Store::count_runs`](super::Store::count_runs) select.
///
/// Runs come back ordered by `updated_at` descending, then `id` descending: A2A's "most recently
/// updated first", made total by the id so a page boundary is unambiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunQuery {
    /// The agent whose runs are listed.
    pub agent: String,
    /// Whose conversations.
    pub scope: ConversationScope,
    /// Only runs in one of these statuses; `None` for every status.
    pub statuses: Option<Vec<RunStatus>>,
    /// Only runs last updated at or after this instant.
    pub updated_since: Option<DateTime<Utc>>,
    /// Only runs strictly after this position in the order: the `(updated_at, id)` of the last run
    /// of the previous page. Ignored by `count_runs`.
    pub after: Option<(DateTime<Utc>, RunId)>,
    /// The most runs to return. Ignored by `count_runs`.
    pub limit: usize,
}

impl RunQuery {
    /// Every run of `agent` in `scope`, up to `limit`.
    pub fn new(agent: impl Into<String>, scope: ConversationScope, limit: usize) -> Self {
        Self {
            agent: agent.into(),
            scope,
            statuses: None,
            updated_since: None,
            after: None,
            limit,
        }
    }

    /// Whether `run` satisfies the filters (not the page position): the reference the in-memory
    /// store lists by, and what every other store's query must agree with.
    pub fn matches(&self, run: &super::RunRecord) -> bool {
        run.agent == self.agent
            && self.scope.contains(run.conversation_id.as_deref())
            && self
                .statuses
                .as_ref()
                .is_none_or(|statuses| statuses.contains(&run.status))
            && self
                .updated_since
                .is_none_or(|since| run.updated_at >= since)
    }
}
