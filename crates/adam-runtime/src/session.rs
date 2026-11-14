//! Child sessions, as far as the runtime sees them: a conversation of child runs ("rounds").
//!
//! A session is nothing stored of its own (ADR 0045): its rounds are runs of one agent that share
//! a conversation id derived from the parent's, and the session id is round 1's run id. This module
//! holds the derivation and what a listing reads back from the runs alone.

use chrono::{DateTime, Utc};

use std::collections::HashMap;

use adam_core::{RunId, RunRecord, RunStatus};

use crate::child::ChildStatus;

/// The separator between the (escaped) parent conversation id and the session id.
const SESSION_SEGMENT: &str = "/s/";

/// The longest session conversation id, in bytes, that [`Runtime::start_child_continuing`]
/// accepts. The conversation id is an indexed column (PostgreSQL: a b-tree on
/// `(agent, conversation_id)`, whose entries are limited to about a third of a page, 2704 bytes
/// at the default page size; MongoDB: an indexed string, with a unique `open_key` that embeds it),
/// so the bound is far below both. *Unverified* against those products' current documentation;
/// the figure is a deliberate margin, not their limit.
///
/// [`Runtime::start_child_continuing`]: crate::Runtime::start_child_continuing
pub const MAX_SESSION_CONVERSATION_BYTES: usize = 1024;

/// Escape the parent part: `%` becomes `%25` and `/` becomes `%2F`, so the escaped text holds no
/// `/` and the first `/` of a session conversation id is its separator.
fn escape(parent: &str) -> String {
    let mut out = String::with_capacity(parent.len());
    for c in parent.chars() {
        match c {
            '%' => out.push_str("%25"),
            '/' => out.push_str("%2F"),
            c => out.push(c),
        }
    }
    out
}

/// The exact inverse of [`escape`]: `None` for text that `escape` cannot have produced.
fn unescape(escaped: &str) -> Option<String> {
    let mut out = String::with_capacity(escaped.len());
    let mut rest = escaped;
    while let Some(i) = rest.find(['%', '/']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(t) = tail.strip_prefix("%25") {
            out.push('%');
            rest = t;
        } else if let Some(t) = tail.strip_prefix("%2F") {
            out.push('/');
            rest = t;
        } else {
            return None;
        }
    }
    out.push_str(rest);
    Some(out)
}

/// The conversation id shared by every round of `session`, a session of the run whose conversation
/// is `parent_conversation` (the parent's [`Ctx::conversation_id`](crate::Ctx::conversation_id)).
///
/// It is the parent's id with `%` written `%25` and `/` written `%2F`, then `/s/` and the session
/// id. A client chooses its A2A context id freely, so without the escape a root conversation
/// `alice:x/s/<uuid>` would be indistinguishable from a session of `alice:x`; with it the first
/// `/` is always the separator and [`parse_session_conversation_id`] is the exact inverse. The
/// cost: a parent id that holds `%` or `/` is no longer a literal prefix of its sessions'
/// conversation ids (an owner prefix such as `subject:` still is, unless the subject holds those
/// characters).
///
/// ```
/// use adam_core::RunId;
/// use adam_runtime::{parse_session_conversation_id, session_conversation_id};
///
/// let session = RunId::new();
/// let id = session_conversation_id("alice:ctx1", session);
/// assert_eq!(id, format!("alice:ctx1/s/{session}"));
/// assert_eq!(
///     parse_session_conversation_id(&id),
///     Some(("alice:ctx1".to_owned(), session))
/// );
/// ```
pub fn session_conversation_id(parent_conversation: &str, session: RunId) -> String {
    format!("{}{SESSION_SEGMENT}{session}", escape(parent_conversation))
}

/// The conversation-id prefix that covers the sessions of `parent_conversation` and nothing else:
/// it ends in the separator, so `alice:ctx1` does not cover the sessions of `alice:ctx10`.
pub fn session_conversation_prefix(parent_conversation: &str) -> String {
    format!("{}{SESSION_SEGMENT}", escape(parent_conversation))
}

/// The parent conversation and the session a session conversation id names: the inverse of
/// [`session_conversation_id`]. `None` for anything it cannot have produced.
pub fn parse_session_conversation_id(conversation_id: &str) -> Option<(String, RunId)> {
    let (escaped, rest) = conversation_id.split_once('/')?;
    let session = rest.strip_prefix("s/")?;
    let parsed = session.parse::<uuid::Uuid>().ok()?;
    // Only the canonical spelling: the id is compared as text by the store.
    (parsed.to_string() == session).then_some(())?;
    Some((unescape(escaped)?, RunId(parsed)))
}

/// The session a conversation id names, if it is a session conversation of `parent_conversation`
/// itself (not of a session's own children).
pub(crate) fn session_of(parent_conversation: &str, conversation_id: &str) -> Option<RunId> {
    let (parent, session) = parse_session_conversation_id(conversation_id)?;
    (parent == parent_conversation).then_some(session)
}

/// What a session is doing, as the runs alone tell.
///
/// The enum is closed on purpose (Rule 3): whether a session is closed is written in its header
/// (ADR 0045, slice 2), which a listing over runs does not read, so a `Closed` state arrives with
/// slice 2 as a deliberate breaking addition that fails to compile in every consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Its latest round is open (runnable or parked).
    Working,
    /// Its latest round is finished, so a message starts the next round.
    Idle,
}

/// How a round ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundOutcome {
    /// It finished with a result.
    Done,
    /// It failed, or its state could not be read.
    Failed,
    /// It was cancelled (`Runtime::cancel`).
    Cancelled,
}

/// One session in a listing: its latest round.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    /// The session: round 1's run id.
    pub id: RunId,
    /// The agent whose runs the rounds are.
    pub agent: String,
    /// The conversation id its rounds share.
    pub conversation_id: String,
    /// The latest round's run.
    pub latest_run: RunId,
    /// The latest round's number, from 1: the count of rounds still in the store, so a round
    /// purged by `purge_finished` is not counted.
    pub round: u32,
    /// Whether the latest round is open.
    pub state: SessionState,
    /// How the latest round ended; `None` while it is open.
    pub outcome: Option<RoundOutcome>,
    /// When the latest round was last updated.
    pub updated_at: DateTime<Utc>,
}

/// One page of [`Runtime::list_sessions`](crate::Runtime::list_sessions).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionList {
    /// The sessions of this page: working first, then the most recently updated, then by id.
    pub sessions: Vec<SessionSummary>,
    /// How many sessions the parent has in all.
    pub total: usize,
}

/// The largest page [`Runtime::list_sessions`](crate::Runtime::list_sessions) returns.
pub const MAX_SESSION_PAGE: usize = 100;

/// The most rounds [`Runtime::list_sessions`](crate::Runtime::list_sessions) reads for one
/// listing; more is [`RuntimeError::TooManyRounds`](crate::RuntimeError::TooManyRounds).
pub const MAX_SESSION_ROUNDS_SCANNED: usize = 5000;

/// Build one page from the rounds of each `(agent, conversation id)`.
pub(crate) fn page_of_sessions(
    parent_conversation: &str,
    rounds: HashMap<(String, String), Vec<RunRecord>>,
    offset: usize,
    limit: usize,
) -> SessionList {
    let mut sessions: Vec<SessionSummary> = rounds
        .into_iter()
        .filter_map(|((agent, conversation_id), mut runs)| {
            let id = session_of(parent_conversation, &conversation_id)?;
            // Round 1 is the run whose id is the session id, started as a child.
            runs.iter().find(|r| r.id == id && r.parent_id.is_some())?;
            runs.sort_by_key(|r| (r.created_at, r.id.0));
            let round = u32::try_from(runs.len()).unwrap_or(u32::MAX);
            let latest = runs.pop()?;
            let (state, outcome) = if latest.status.is_open() {
                (SessionState::Working, None)
            } else {
                let status = ChildStatus::from_record(&latest);
                let outcome = match (status.status, status.error.as_deref()) {
                    (RunStatus::Done, _) => RoundOutcome::Done,
                    (_, Some(e)) if e.starts_with("cancelled:") => RoundOutcome::Cancelled,
                    _ => RoundOutcome::Failed,
                };
                (SessionState::Idle, Some(outcome))
            };
            Some(SessionSummary {
                id,
                agent,
                conversation_id,
                latest_run: latest.id,
                round,
                state,
                outcome,
                updated_at: latest.updated_at,
            })
        })
        .collect();
    sessions.sort_by(|a, b| {
        (a.state != SessionState::Working)
            .cmp(&(b.state != SessionState::Working))
            .then(b.updated_at.cmp(&a.updated_at))
            .then(a.id.0.cmp(&b.id.0))
    });
    let total = sessions.len();
    let limit = limit.min(MAX_SESSION_PAGE);
    SessionList {
        sessions: sessions.into_iter().skip(offset).take(limit).collect(),
        total,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn build_and_parse_are_inverse(parent in ".{0,40}", bytes in any::<[u8; 16]>()) {
            let session = RunId(uuid::Uuid::from_bytes(bytes));
            let id = session_conversation_id(&parent, session);
            prop_assert_eq!(parse_session_conversation_id(&id), Some((parent.clone(), session)));
            prop_assert!(id.starts_with(&session_conversation_prefix(&parent)));
            prop_assert_eq!(session_of(&parent, &id), Some(session));
            // The escaped part holds no separator, so no other parent claims it.
            prop_assert_eq!(id.matches('/').count(), 2);
        }

        #[test]
        fn a_string_that_is_not_built_parses_to_none_or_round_trips(s in ".{0,60}") {
            if let Some((parent, session)) = parse_session_conversation_id(&s) {
                prop_assert_eq!(session_conversation_id(&parent, session), s);
            }
        }
    }

    #[test]
    fn awkward_parents_are_kept_apart() {
        let session = RunId::new();
        for parent in ["a:x/s/y", "%", "/", "%2F", "a/", "a:%25/s/", ""] {
            let id = session_conversation_id(parent, session);
            assert_eq!(
                parse_session_conversation_id(&id),
                Some((parent.to_owned(), session)),
                "{parent:?}"
            );
        }
        // A parent whose own id looks like a session conversation is a different session's parent.
        let nested = session_conversation_id("alice:x/s/y", session);
        assert_ne!(nested, session_conversation_id("alice:x", session));
        assert_eq!(session_of("alice:x", &nested), None);
        assert_eq!(session_of("alice:x/s/y", &nested), Some(session));
        assert_eq!(parse_session_conversation_id("a%2fb/s/x"), None);
    }

    fn record(
        id: RunId,
        conv: &str,
        parent: Option<RunId>,
        status: RunStatus,
        error: Option<&str>,
        at: i64,
    ) -> RunRecord {
        let mut env = crate::envelope::Envelope::new(serde_json::json!({}));
        env.error = error.map(str::to_owned);
        let t = DateTime::from_timestamp(1_700_000_000, 0).unwrap() + chrono::Duration::seconds(at);
        RunRecord {
            id,
            agent: "a".into(),
            conversation_id: Some(conv.into()),
            parent_id: parent,
            status,
            state: env.encode().unwrap(),
            wake_at: None,
            version: 2,
            created_at: t,
            updated_at: t,
        }
    }

    fn rounds(runs: Vec<RunRecord>) -> HashMap<(String, String), Vec<RunRecord>> {
        let mut map: HashMap<(String, String), Vec<RunRecord>> = HashMap::new();
        for r in runs {
            map.entry(("a".into(), r.conversation_id.clone().unwrap()))
                .or_default()
                .push(r);
        }
        map
    }

    #[test]
    fn sessions_updated_in_the_same_instant_are_ordered_by_id() {
        let parent = RunId::new();
        let mut ids: Vec<RunId> = (0..4).map(|_| RunId::new()).collect();
        let runs = ids
            .iter()
            .map(|s| {
                record(
                    *s,
                    &session_conversation_id("p", *s),
                    Some(parent),
                    RunStatus::Done,
                    None,
                    5,
                )
            })
            .collect();
        ids.sort_by_key(|i| i.0);
        let list = page_of_sessions("p", rounds(runs), 0, 10);
        assert_eq!(list.sessions.iter().map(|s| s.id).collect::<Vec<_>>(), ids);
    }

    #[test]
    fn working_comes_first_then_newest() {
        let parent = RunId::new();
        let (old_working, new_idle, newer_idle) = (RunId::new(), RunId::new(), RunId::new());
        let conv = |s| session_conversation_id("p", s);
        let list = page_of_sessions(
            "p",
            rounds(vec![
                record(
                    old_working,
                    &conv(old_working),
                    Some(parent),
                    RunStatus::Parked,
                    None,
                    1,
                ),
                record(
                    new_idle,
                    &conv(new_idle),
                    Some(parent),
                    RunStatus::Done,
                    None,
                    2,
                ),
                record(
                    newer_idle,
                    &conv(newer_idle),
                    Some(parent),
                    RunStatus::Done,
                    None,
                    3,
                ),
            ]),
            0,
            10,
        );
        assert_eq!(
            list.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![old_working, newer_idle, new_idle]
        );
    }

    #[test]
    fn a_row_needs_a_first_round_with_a_parent() {
        let parent = RunId::new();
        let (real, no_parent, wrong_id) = (RunId::new(), RunId::new(), RunId::new());
        let conv = |s| session_conversation_id("p", s);
        let list = page_of_sessions(
            "p",
            rounds(vec![
                record(real, &conv(real), Some(parent), RunStatus::Done, None, 1),
                // A top-level run that spells a session's conversation id.
                record(no_parent, &conv(no_parent), None, RunStatus::Done, None, 2),
                // A child whose id is not its session's.
                record(
                    RunId::new(),
                    &conv(wrong_id),
                    Some(parent),
                    RunStatus::Done,
                    None,
                    3,
                ),
            ]),
            0,
            10,
        );
        assert_eq!(
            list.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![real]
        );
    }

    #[test]
    fn the_outcome_is_how_the_latest_round_ended() {
        let parent = RunId::new();
        let conv = |s| session_conversation_id("p", s);
        let cases = [
            (RunStatus::Done, None, RoundOutcome::Done),
            (RunStatus::Failed, Some("boom"), RoundOutcome::Failed),
            (
                RunStatus::Failed,
                Some("cancelled: stop"),
                RoundOutcome::Cancelled,
            ),
        ];
        for (status, error, want) in cases {
            let s = RunId::new();
            let list = page_of_sessions(
                "p",
                rounds(vec![record(s, &conv(s), Some(parent), status, error, 1)]),
                0,
                10,
            );
            assert_eq!(list.sessions[0].outcome, Some(want));
            assert_eq!(list.sessions[0].state, SessionState::Idle);
        }
    }
}
