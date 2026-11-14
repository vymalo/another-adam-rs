//! Child sessions, as far as the runtime sees them: a conversation of child runs ("rounds").
//!
//! A session is nothing stored of its own (ADR 0045): its rounds are runs of one agent that share
//! a conversation id derived from the parent's, and the session id is round 1's run id. This module
//! holds the derivation and what a listing reads back from the runs alone.

use chrono::{DateTime, Utc};

use adam_core::RunId;

/// The separator between a parent's conversation id and a session id.
const SESSION_SEGMENT: &str = "/s/";

/// The conversation id shared by every round of `session`, a session of the run whose conversation
/// is `parent_conversation` (the parent's [`Ctx::conversation_id`](crate::Ctx::conversation_id)).
///
/// It is the parent's id followed by `/s/` and the session id, which is what
/// `encode_conversation(subject, "<context>/s/<session>")` makes of the same context, so the
/// owner's prefix still covers it and the parent's own id is a prefix of it.
///
/// ```
/// use adam_core::RunId;
/// use adam_runtime::session_conversation_id;
///
/// let session = RunId::new();
/// assert_eq!(
///     session_conversation_id("alice:ctx1", session),
///     format!("alice:ctx1/s/{session}")
/// );
/// ```
pub fn session_conversation_id(parent_conversation: &str, session: RunId) -> String {
    format!("{parent_conversation}{SESSION_SEGMENT}{session}")
}

/// The conversation-id prefix that covers the sessions of `parent_conversation` and nothing else:
/// it ends in the separator, so `alice:ctx1` does not cover the sessions of `alice:ctx10`.
pub fn session_conversation_prefix(parent_conversation: &str) -> String {
    format!("{parent_conversation}{SESSION_SEGMENT}")
}

/// The session a conversation id names, if it is a session conversation of `parent_conversation`
/// itself (not of a session's own children).
pub(crate) fn session_of(parent_conversation: &str, conversation_id: &str) -> Option<RunId> {
    let rest = conversation_id.strip_prefix(&session_conversation_prefix(parent_conversation))?;
    rest.parse::<uuid::Uuid>().ok().map(RunId)
}

/// What a session is doing, as the runs alone tell.
///
/// The enum is `non_exhaustive`: whether a session is closed is written in its header (ADR 0045,
/// slice 2), which a listing over runs does not read, so a `Closed` state arrives with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionState {
    /// Its latest round is open (runnable or parked).
    Working,
    /// Its latest round is finished, so a message starts the next round.
    Idle,
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
