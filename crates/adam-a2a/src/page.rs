//! `ListTasks`: the query a backend answers, the page it returns, and the page token.
//!
//! The token is **a cursor, never an offset**: it carries the position of the last task of the
//! page in the order `ListTasks` uses (last update, newest first, the task id breaking ties), so
//! a page costs the same however deep it is and nothing in it counts another caller's tasks.
//! It is opaque to clients (base64url of a small JSON object) and bound to the caller and the
//! filters it was issued for: a token from another caller, or one presented with other filters,
//! is `InvalidParams`, and so is anything that does not decode. Binding is by a SHA-256 digest
//! of the caller's subject and the filters, so a token is not a secret and is not signed: forging
//! one can only move the position inside the forger's own tasks, because every query is scoped to
//! the caller before the position is looked at.

use a2a::{Task, TaskState};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backend::{BackendError, Caller};

/// The page size used when a request names none (specification §3.1.4).
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// The largest page served; a larger request is clamped to it (specification §3.1.4).
pub const MAX_PAGE_SIZE: usize = 100;

/// What a `ListTasks` request asks for, after the handler has resolved the defaults.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TaskQuery {
    /// Only tasks of this context.
    pub context_id: Option<String>,
    /// Only tasks in this state.
    pub status: Option<TaskState>,
    /// Only tasks whose status timestamp is at or after this instant.
    pub status_timestamp_after: Option<DateTime<Utc>>,
    /// The page size: at least 1 and at most [`MAX_PAGE_SIZE`].
    pub page_size: usize,
    /// The token of the page to continue from, as the client sent it.
    pub page_token: Option<String>,
    /// Whether the tasks carry their artifacts (the handler removes them when `false`; a backend
    /// may skip reading them).
    pub include_artifacts: bool,
}

impl TaskQuery {
    /// The first page of every task of the caller, [`DEFAULT_PAGE_SIZE`] at a time.
    pub fn new() -> Self {
        Self {
            context_id: None,
            status: None,
            status_timestamp_after: None,
            page_size: DEFAULT_PAGE_SIZE,
            page_token: None,
            include_artifacts: false,
        }
    }

    /// Resolve a request's page size: absent, zero and negative mean the default, and the result
    /// never exceeds [`MAX_PAGE_SIZE`].
    pub fn resolve_page_size(requested: Option<i32>) -> usize {
        match requested {
            Some(n) if n > 0 => usize::try_from(n).map_or(MAX_PAGE_SIZE, |n| n.min(MAX_PAGE_SIZE)),
            _ => DEFAULT_PAGE_SIZE,
        }
    }

    /// The digest a page token is bound to: the caller and every filter, not the page size or
    /// the position (a client may change the page size between pages).
    fn binding(&self, caller: &Caller) -> String {
        let mut hash = Sha256::new();
        let mut field = |tag: &str, value: &str| {
            hash.update((tag.len() as u64).to_be_bytes());
            hash.update(tag.as_bytes());
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        };
        field("subject", &caller.subject);
        field("context", self.context_id.as_deref().unwrap_or("\u{0}none"));
        field(
            "status",
            &self
                .status
                .as_ref()
                .and_then(|s| serde_json::to_value(s).ok())
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| "\u{0}none".to_owned()),
        );
        field(
            "after",
            &self.status_timestamp_after.map_or_else(
                || "\u{0}none".to_owned(),
                |t| t.timestamp_millis().to_string(),
            ),
        );
        hash.finalize()
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

impl Default for TaskQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// One page of `ListTasks`.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct TaskPage {
    /// The tasks of the page, most recently updated first.
    pub tasks: Vec<Task>,
    /// The token of the next page; `None` on the last one.
    pub next_page_token: Option<String>,
    /// How many tasks match the filters (before pagination). A backend that cannot count
    /// exactly says so in its docs.
    pub total_size: usize,
}

impl TaskPage {
    /// A page.
    pub fn new(tasks: Vec<Task>, next_page_token: Option<String>, total_size: usize) -> Self {
        Self {
            tasks,
            next_page_token,
            total_size,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u8,
    /// The binding to the caller and the filters.
    b: String,
    /// The last update of the last task, in milliseconds.
    t: i64,
    /// Its id.
    i: String,
}

/// A decoded page token: the position to continue after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageToken {
    /// The last update of the last task of the previous page.
    pub updated_at: DateTime<Utc>,
    /// Its id.
    pub id: String,
}

impl PageToken {
    /// The token that continues after the task `id`, last updated at `updated_at`, for this
    /// `caller` and `query`.
    pub fn encode(
        caller: &Caller,
        query: &TaskQuery,
        updated_at: DateTime<Utc>,
        id: &str,
    ) -> String {
        let wire = Wire {
            v: 1,
            b: query.binding(caller),
            t: updated_at.timestamp_millis(),
            i: id.to_owned(),
        };
        // A struct of a number and two strings always serialises.
        let json = serde_json::to_vec(&wire).unwrap_or_default();
        URL_SAFE_NO_PAD.encode(json)
    }

    /// The position `token` stands for, if it was issued to this `caller` for these filters.
    ///
    /// # Errors
    ///
    /// [`BackendError::InvalidParams`] ("invalid page token") for anything else: not base64, not
    /// ours, another version, another caller's, other filters. One message for all, so the
    /// client learns nothing about why.
    pub fn decode(token: &str, caller: &Caller, query: &TaskQuery) -> Result<Self, BackendError> {
        let invalid = || BackendError::InvalidParams("invalid page token".to_owned());
        let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
        let wire: Wire = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if wire.v != 1 || wire.b != query.binding(caller) {
            return Err(invalid());
        }
        let updated_at = DateTime::<Utc>::from_timestamp_millis(wire.t).ok_or_else(invalid)?;
        Ok(Self {
            updated_at,
            id: wire.i,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(subject: &str) -> Caller {
        Caller::new(subject)
    }

    #[test]
    fn page_sizes_are_resolved_as_the_specification_says() {
        assert_eq!(TaskQuery::resolve_page_size(None), 50);
        assert_eq!(TaskQuery::resolve_page_size(Some(0)), 50);
        assert_eq!(TaskQuery::resolve_page_size(Some(-3)), 50);
        assert_eq!(TaskQuery::resolve_page_size(Some(1)), 1);
        assert_eq!(TaskQuery::resolve_page_size(Some(100)), 100);
        assert_eq!(TaskQuery::resolve_page_size(Some(101)), 100);
        assert_eq!(TaskQuery::resolve_page_size(Some(i32::MAX)), 100);
    }

    #[test]
    fn a_token_round_trips_for_its_caller_and_filters() {
        let q = TaskQuery::new();
        let at = DateTime::<Utc>::from_timestamp_millis(1_760_000_123_456).unwrap();
        let token = PageToken::encode(&caller("a"), &q, at, "task-9");
        let back = PageToken::decode(&token, &caller("a"), &q).unwrap();
        assert_eq!(
            back,
            PageToken {
                updated_at: at,
                id: "task-9".into()
            }
        );
        // The page size is not part of the binding.
        let mut bigger = q.clone();
        bigger.page_size = 7;
        assert!(PageToken::decode(&token, &caller("a"), &bigger).is_ok());
    }

    #[test]
    fn a_token_is_refused_for_another_caller_other_filters_or_garbage() {
        let q = TaskQuery::new();
        let at = DateTime::<Utc>::from_timestamp_millis(1_760_000_000_000).unwrap();
        let token = PageToken::encode(&caller("a"), &q, at, "t");
        let refuse = |token: &str, c: &Caller, q: &TaskQuery| matches!(PageToken::decode(token, c, q), Err(BackendError::InvalidParams(m)) if m == "invalid page token");
        assert!(refuse(&token, &caller("b"), &q), "another caller");
        let mut other = q.clone();
        other.context_id = Some("ctx".into());
        assert!(refuse(&token, &caller("a"), &other), "other filters");
        other = q.clone();
        other.status = Some(TaskState::Completed);
        assert!(refuse(&token, &caller("a"), &other));
        for junk in [
            "",
            "!!!",
            "e30",
            "bm90LWpzb24",
            "eyJ2IjoyLCJiIjoiIiwidCI6MCwiaSI6IiJ9",
        ] {
            assert!(refuse(junk, &caller("a"), &q), "{junk:?}");
        }
        // A tampered position inside a valid envelope still decodes (it can only move within the
        // caller's own tasks) but a flipped binding does not.
        let mut bytes = URL_SAFE_NO_PAD.decode(&token).unwrap();
        let text = String::from_utf8(bytes.clone())
            .unwrap()
            .replace("\"b\":\"", "\"b\":\"0");
        bytes = text.into_bytes();
        assert!(refuse(&URL_SAFE_NO_PAD.encode(bytes), &caller("a"), &q));
    }
}
