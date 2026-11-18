//! Child sessions, as far as the runtime sees them: a conversation of child runs ("rounds").
//!
//! A session is nothing stored of its own (ADR 0045): its rounds are runs of one agent that share
//! a conversation id derived from the parent's, and the session id is round 1's run id. This module
//! holds the derivation and what a listing reads back from the runs alone.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::collections::HashMap;
use std::time::Duration;

use adam_core::{RunId, RunRecord, RunStatus};

use crate::child::ChildStatus;
use crate::envelope::Envelope;

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
        } else {
            out.push('/');
            rest = tail.strip_prefix("%2F")?;
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

/// The key under which an agent keeps a round's [`SessionHeader`] in its own state, and under which
/// the first message of a round carries it. The runtime reads a round's header from there.
pub const SESSION_STATE_KEY: &str = "session";

/// [`Inbound::kind`](crate::Inbound) of the message that asks a working round to be the last of its
/// session: the round finishes its work, and the session then takes no more messages.
pub const SESSION_CLOSE_KIND: &str = "adam.session.close";

/// The key of a round's first message that asks for a round which only closes its session
/// (`close_session: true`): the round calls no model, and its header says `closing`.
pub const SESSION_CLOSE_ROUND_KEY: &str = "close_session";

/// The payload of the first message of a round that only closes the session `header` describes:
/// `{"close_session": true, "session": <header>}`. The header must already be the closing round's
/// (the next round number, `closed` and `closing`).
pub fn closing_round_payload(header: &SessionHeader) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert(SESSION_CLOSE_ROUND_KEY.to_owned(), Value::Bool(true));
    payload.insert(
        SESSION_STATE_KEY.to_owned(),
        serde_json::to_value(header).unwrap_or(Value::Null),
    );
    Value::Object(payload)
}

/// What a round of a session says about the session, in the state of the round's agent (ADR 0045).
///
/// **The truth is the latest round's header** (nothing else stores a session). A session whose
/// rounds carry no header, such as one an embedder made with `Runtime::start_child_continuing`, is
/// kept open: it closes only when nobody has written to it for the runtime's idle TTL
/// ([`Runtime::session_idle_ttl`](crate::Runtime::session_idle_ttl), ADR 0045 slice 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// The session: round 1's run id.
    pub id: RunId,
    /// The registration name of the agent whose runs the rounds are.
    pub agent: String,
    /// What the session is for, in the words of whoever started it (a line, at most 80 characters).
    pub description: String,
    /// Whether the session waits for another message after a round (`true`) or closes when its
    /// round has ended (`false`, the default of the tools).
    pub keep_open: bool,
    /// The session takes no more messages: set by the round that closes it, or by a close request
    /// that a working round has read. Absent when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closed: bool,
    /// This round only closes the session: it calls no model and its report is not a report. Absent
    /// when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closing: bool,
    /// The round's number, from 1: what orders the rounds of a session (timestamps tie, ids are
    /// hashes). `0` for a header written before it existed.
    #[serde(default)]
    pub round: u32,
}

/// The header a round's agent state holds, if it holds one.
pub(crate) fn header_of(agent_state: &Value) -> Option<SessionHeader> {
    serde_json::from_value(agent_state.get(SESSION_STATE_KEY)?.clone()).ok()
}

fn header_of_record(rec: &RunRecord) -> Option<SessionHeader> {
    Envelope::decode(rec.id, &rec.state)
        .ok()
        .and_then(|e| header_of(&e.agent))
}

/// The rounds of a session in the order they ran: by the round number the header carries, then
/// by creation time and id. Each with its header.
fn ordered(runs: Vec<RunRecord>) -> Vec<(RunRecord, Option<SessionHeader>)> {
    let mut keyed: Vec<(RunRecord, Option<SessionHeader>)> = runs
        .into_iter()
        .map(|r| {
            let h = header_of_record(&r);
            (r, h)
        })
        .collect();
    keyed.sort_by_key(|(r, h)| (h.as_ref().map_or(0, |h| h.round), r.created_at, r.id.0));
    keyed
}

/// What a session is doing, as its latest round and header tell.
///
/// The enum is closed on purpose (Rule 3): a new state must fail to compile in every consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Its latest round is open (runnable or parked).
    Working,
    /// Its latest round is finished and the session is open, so a message starts the next round.
    Idle,
    /// Its latest round is finished and the session takes no more messages: it was closed
    /// (`closed` in the header), or it was not kept open and its round has ended.
    Closed,
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
    /// What the session is for (empty for a session with no header).
    pub description: String,
    /// Whether the session waits for another message after a round (`true` for a session with no
    /// header).
    pub keep_open: bool,
    /// Whether the session takes no more messages, even if its latest round still works (it was
    /// asked to close and is finishing).
    pub closed: bool,
    /// Whether the latest round only closes the session: it calls no model and reports nothing
    /// new, so nobody needs to wait for it.
    pub closing: bool,
    /// Whether the session reads closed only because its latest round ended at least the idle
    /// TTL ago ([`closed`](Self::closed) is then also `true`). Its report is still there.
    pub idle_expired: bool,
    /// The run that started the latest round: the parent run of that round.
    pub started_by: Option<RunId>,
    /// The round whose report the session has: the latest one that worked (a closing round has
    /// none), which is the latest round itself unless a closing round follows it.
    pub report: ReportRound,
}

/// The round a session's report is of ([`SessionSummary::report`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportRound {
    /// Its run: what a report names it by ([`SessionView::report_run`]).
    pub run: RunId,
    /// Whether it has ended.
    pub ended: bool,
    /// Whether it was cancelled (`Runtime::cancel`, or a close with cancel).
    pub cancelled: bool,
    /// The run that started it: its parent run.
    pub started_by: Option<RunId>,
    /// When it was last updated.
    pub updated_at: DateTime<Utc>,
}

impl ReportRound {
    fn of(run: &RunRecord) -> Self {
        Self {
            run: run.id,
            ended: !run.status.is_open(),
            cancelled: ChildStatus::from_record(run)
                .error
                .is_some_and(|e| e.starts_with("cancelled:")),
            started_by: run.parent_id,
            updated_at: run.updated_at,
        }
    }

    /// Whether a person started it and not a run of an agent: a round that is its own parent.
    pub fn started_outside_a_run(&self) -> bool {
        self.started_by == Some(self.run)
    }
}

impl SessionSummary {
    /// Whether the latest round is doing work somebody will want to read: it is open and not a
    /// round that only closes the session (that one calls no model and ends by itself). What the
    /// working limit counts.
    pub fn does_work(&self) -> bool {
        self.state == SessionState::Working && !self.closing
    }

    /// Whether the latest round was started by a person and not by a run of an agent: a round
    /// that is its own parent (ADR 0045, decision 1, as amended for a person's rounds).
    pub fn started_outside_a_run(&self) -> bool {
        self.started_by == Some(self.latest_run)
    }
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

/// How long a kept-open session may sit idle (its latest round ended, nobody wrote to it) before
/// it reads closed: [`Runtime`](crate::Runtime)'s default, seven days
/// ([`RuntimeBuilder::session_idle_ttl`](crate::RuntimeBuilder::session_idle_ttl)).
pub const DEFAULT_SESSION_IDLE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// The limits on the sessions of one conversation (ADR 0045 decision 5, slice 4): what the
/// session tools, the A2A front and the card's `maxOpen` share.
///
/// Each is checked before the thing it limits starts, so two requests that race may both pass and
/// exceed it by the number racing. The fields are private: every value is within its bounds
/// ([`new`](Self::new), [`lowered_by`](Self::lowered_by)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionLimits {
    max_open: u32,
    max_working: u32,
    max_rounds: u32,
}

impl Default for SessionLimits {
    /// 16 open, 4 working, 20 rounds.
    fn default() -> Self {
        Self {
            max_open: 16,
            max_working: 4,
            max_rounds: 20,
        }
    }
}

impl SessionLimits {
    /// The most sessions open at once (not closed; one that is closing and still works counts): a
    /// start past it is refused. 1 to [`MAX_SESSION_PAGE`].
    pub fn max_open(&self) -> u32 {
        self.max_open
    }

    /// The most rounds working at once (a round that only closes a session does not count): a
    /// start, or a send that would start a round, past it is refused. 1 to 32, and at most
    /// `max_open`.
    pub fn max_working(&self) -> u32 {
        self.max_working
    }

    /// The most rounds a send may reach: a send past it is refused, but a close is always
    /// allowed, so a session can have `max_rounds + 1` rounds counting the closing one. 2 to 200.
    pub fn max_rounds(&self) -> u32 {
        self.max_rounds
    }

    /// The largest `max_working`.
    pub const MAX_WORKING_BOUND: u32 = 32;
    /// The smallest and largest `max_rounds`.
    pub const MAX_ROUNDS_BOUNDS: (u32, u32) = (2, 200);

    /// Limits within their bounds.
    ///
    /// # Errors
    ///
    /// The sentence that says which value is out of bounds.
    pub fn new(max_open: u32, max_working: u32, max_rounds: u32) -> Result<Self, String> {
        let page = u32::try_from(MAX_SESSION_PAGE).unwrap_or(100);
        if !(1..=page).contains(&max_open) {
            return Err(format!("max_open is {max_open}; it must be 1 to {page}"));
        }
        if !(1..=Self::MAX_WORKING_BOUND).contains(&max_working) {
            return Err(format!(
                "max_working is {max_working}; it must be 1 to {}",
                Self::MAX_WORKING_BOUND
            ));
        }
        if max_working > max_open {
            return Err(format!(
                "max_working is {max_working}, more than max_open ({max_open})"
            ));
        }
        let (low, high) = Self::MAX_ROUNDS_BOUNDS;
        if !(low..=high).contains(&max_rounds) {
            return Err(format!(
                "max_rounds is {max_rounds}; it must be {low} to {high}"
            ));
        }
        Ok(Self {
            max_open,
            max_working,
            max_rounds,
        })
    }

    /// These limits, each lowered to the value `open`, `working` or `rounds` gives (a larger one
    /// changes nothing: a file may only lower what the deployment set; a `0` is read as the
    /// smallest legal value, so no limit is ever below its bound). `max_working` stays at most
    /// `max_open`.
    #[must_use]
    pub fn lowered_by(self, open: Option<u32>, working: Option<u32>, rounds: Option<u32>) -> Self {
        let max_open = open.map_or(self.max_open, |v| v.clamp(1, self.max_open));
        let max_working = working
            .map_or(self.max_working, |v| v.clamp(1, self.max_working))
            .min(max_open);
        let max_rounds = rounds.map_or(self.max_rounds, |v| {
            v.clamp(
                Self::MAX_ROUNDS_BOUNDS.0.min(self.max_rounds),
                self.max_rounds,
            )
        });
        Self {
            max_open,
            max_working,
            max_rounds,
        }
    }
}

/// The newest round of a session, read: what both a listing and a single session's view say.
pub(crate) struct Latest {
    pub state: SessionState,
    pub outcome: Option<RoundOutcome>,
    pub header: Option<SessionHeader>,
    pub closed: bool,
    pub idle_expired: bool,
    pub status: ChildStatus,
}

/// Read `latest`, the newest round of a session, at `now`: a round that ended at least `idle_ttl`
/// ago in a session that would otherwise stay open reads closed (and `idle_expired`). A round that
/// works never expires, and the report of an expired session is the round's, unchanged.
pub(crate) fn read_latest(latest: &RunRecord, now: DateTime<Utc>, idle_ttl: Duration) -> Latest {
    let open = latest.status.is_open();
    let (header, close_asked) = match Envelope::decode(latest.id, &latest.state) {
        Ok(env) => {
            let asked = env.inbox.iter().any(|i| i.kind == SESSION_CLOSE_KIND);
            (header_of(&env.agent), asked)
        }
        // A round whose state cannot be read is a session nobody can continue: closed, not open for ever.
        Err(_) => (None, true),
    };
    let keep_open = header.as_ref().is_none_or(|h| h.keep_open);
    let closed_already =
        close_asked || header.as_ref().is_some_and(|h| h.closed) || (!open && !keep_open);
    let idle_expired = !open
        && !closed_already
        && chrono::Duration::from_std(idle_ttl).is_ok_and(|ttl| now - latest.updated_at >= ttl);
    let closed = closed_already || idle_expired;
    let status = ChildStatus::from_record(latest);
    let (state, outcome) = if open {
        (SessionState::Working, None)
    } else {
        let outcome = match (status.status, status.error.as_deref()) {
            (RunStatus::Done, _) => RoundOutcome::Done,
            (_, Some(e)) if e.starts_with("cancelled:") => RoundOutcome::Cancelled,
            _ => RoundOutcome::Failed,
        };
        let state = if closed {
            SessionState::Closed
        } else {
            SessionState::Idle
        };
        (state, Some(outcome))
    };
    Latest {
        state,
        outcome,
        header,
        closed,
        idle_expired,
        status,
    }
}

fn rank(state: SessionState) -> u8 {
    match state {
        SessionState::Working => 0,
        SessionState::Idle => 1,
        SessionState::Closed => 2,
    }
}

/// Build one page from the rounds of each `(agent, conversation id)`.
pub(crate) fn page_of_sessions(
    parent_conversation: &str,
    rounds: HashMap<(String, String), Vec<RunRecord>>,
    offset: usize,
    limit: usize,
    now: DateTime<Utc>,
    idle_ttl: Duration,
) -> SessionList {
    let mut sessions: Vec<SessionSummary> = rounds
        .into_iter()
        .filter_map(|((agent, conversation_id), runs)| {
            let id = session_of(parent_conversation, &conversation_id)?;
            // Round 1 is the run whose id is the session id, started as a child.
            runs.iter().find(|r| r.id == id && r.parent_id.is_some())?;
            let mut runs = ordered(runs);
            let round = u32::try_from(runs.len()).unwrap_or(u32::MAX);
            let (latest, _) = runs.pop()?;
            // The report is of the newest round that worked: all of them but a closing one.
            let worked = runs
                .iter()
                .rev()
                .find(|(_, h)| !h.as_ref().is_some_and(|h| h.closing))
                .map_or(&latest, |(r, _)| r);
            let read = read_latest(&latest, now, idle_ttl);
            let report = if read.header.as_ref().is_some_and(|h| h.closing) {
                ReportRound::of(worked)
            } else {
                ReportRound::of(&latest)
            };
            Some(SessionSummary {
                id,
                agent,
                conversation_id,
                latest_run: latest.id,
                round,
                state: read.state,
                outcome: read.outcome,
                updated_at: latest.updated_at,
                description: read
                    .header
                    .as_ref()
                    .map(|h| h.description.clone())
                    .unwrap_or_default(),
                keep_open: read.header.as_ref().is_none_or(|h| h.keep_open),
                closed: read.closed,
                closing: read.header.as_ref().is_some_and(|h| h.closing),
                idle_expired: read.idle_expired,
                started_by: latest.parent_id,
                report,
            })
        })
        .collect();
    sessions.sort_by(|a, b| {
        rank(a.state)
            .cmp(&rank(b.state))
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

/// One session, read by [`ChildStarter::session`](crate::ChildStarter::session): what a tool needs
/// to answer a read, a send or a close.
///
/// It cannot be built outside this crate, so holding one proves the session belongs to the
/// conversation it was read for.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SessionView {
    /// The session: round 1's run id.
    pub id: RunId,
    /// The agent whose runs the rounds are.
    pub agent: String,
    /// The conversation id its rounds share.
    pub conversation_id: String,
    /// The latest round's header; `None` for a session an embedder made with no header.
    pub header: Option<SessionHeader>,
    /// How many rounds the session has in the store.
    pub round: u32,
    /// The latest round's run.
    pub latest_run: RunId,
    /// What the session is doing.
    pub state: SessionState,
    /// How the latest round ended; `None` while it is open.
    pub outcome: Option<RoundOutcome>,
    /// Whether the session takes no more messages (it may still be finishing a round).
    pub closed: bool,
    /// Whether it reads closed only because its latest round ended at least the idle TTL ago
    /// ([`closed`](Self::closed) is then also `true`).
    pub idle_expired: bool,
    /// The report: the last round **that did work** (a round that only closed the session has none),
    /// as a parent reads a child: its status and output.
    pub latest: ChildStatus,
    /// The run of the round the report is of.
    pub report_run: RunId,
    /// The run that started the latest round.
    pub started_by: Option<RunId>,
}

impl SessionView {
    /// Whether the latest round was started by a person and not by a run of an agent: a round
    /// that is its own parent.
    pub fn started_outside_a_run(&self) -> bool {
        self.started_by == Some(self.latest_run)
    }

    pub(crate) fn from_rounds(
        session: RunId,
        runs: Vec<RunRecord>,
        now: DateTime<Utc>,
        idle_ttl: Duration,
    ) -> Option<Self> {
        let runs = ordered(runs);
        let round = u32::try_from(runs.len()).unwrap_or(u32::MAX);
        let (latest, _) = runs.last()?.clone();
        let worked = runs
            .iter()
            .rev()
            .find(|(_, h)| !h.as_ref().is_some_and(|h| h.closing))
            .map_or_else(|| latest.clone(), |(r, _)| r.clone());
        let mut read = read_latest(&latest, now, idle_ttl);
        read.status = ChildStatus::from_record(&worked);
        Some(Self {
            id: session,
            agent: latest.agent.clone(),
            conversation_id: latest.conversation_id.clone().unwrap_or_default(),
            header: read.header,
            round,
            latest_run: latest.id,
            state: read.state,
            outcome: read.outcome,
            closed: read.closed,
            idle_expired: read.idle_expired,
            latest: read.status,
            report_run: worked.id,
            started_by: latest.parent_id,
        })
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

    #[test]
    fn a_closing_round_says_close_and_carries_its_header() {
        let header = SessionHeader {
            id: RunId::new(),
            agent: "helper".into(),
            description: "d".into(),
            keep_open: true,
            closed: true,
            closing: true,
            round: 3,
        };
        let payload = closing_round_payload(&header);
        assert_eq!(payload[SESSION_CLOSE_ROUND_KEY], Value::Bool(true));
        assert_eq!(payload.as_object().map(|o| o.len()), Some(2));
        assert_eq!(header_of(&payload), Some(header));
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
        let list = page_of_sessions("p", rounds(runs), 0, 10, t0(), idle_ttl());
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
            t0(),
            idle_ttl(),
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
            t0(),
            idle_ttl(),
        );
        assert_eq!(
            list.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![real]
        );
    }

    fn with_header(
        id: RunId,
        header: Option<SessionHeader>,
        status: RunStatus,
        inbox: Vec<crate::agent::Inbound>,
    ) -> RunRecord {
        let agent = header.map_or(
            serde_json::json!({}),
            |h| serde_json::json!({ SESSION_STATE_KEY: h }),
        );
        let mut env = crate::envelope::Envelope::new(agent);
        env.inbox = inbox;
        let mut rec = record(
            id,
            &session_conversation_id("p", id),
            Some(RunId::new()),
            status,
            None,
            1,
        );
        rec.state = env.encode().unwrap();
        rec
    }

    fn header(id: RunId, keep_open: bool, closed: bool) -> SessionHeader {
        SessionHeader {
            id,
            agent: "a".into(),
            description: "look".into(),
            keep_open,
            closed,
            closing: false,
            round: 1,
        }
    }

    #[test]
    fn a_header_decides_whether_a_session_is_closed() {
        let state = |h: Option<SessionHeader>, status, inbox| {
            let id = h.as_ref().map_or_else(RunId::new, |h| h.id);
            let rec = with_header(id, h, status, inbox);
            let read = read_latest(&rec, rec.updated_at, idle_ttl());
            (read.state, read.closed)
        };
        let id = RunId::new();
        let close = crate::agent::Inbound::new(SESSION_CLOSE_KIND, serde_json::json!({}));
        // No header: kept open, never closes by itself.
        assert_eq!(
            state(None, RunStatus::Done, vec![]),
            (SessionState::Idle, false)
        );
        // Not kept open: closed once its round has ended, and not before.
        let once = || Some(header(id, false, false));
        assert_eq!(
            state(once(), RunStatus::Done, vec![]),
            (SessionState::Closed, true)
        );
        assert_eq!(
            state(once(), RunStatus::Failed, vec![]),
            (SessionState::Closed, true)
        );
        assert_eq!(
            state(once(), RunStatus::Parked, vec![]),
            (SessionState::Working, false)
        );
        // Kept open: idle, until its header says closed or a close request is waiting in the inbox
        // (a working round is then still working, and takes no more messages).
        let kept = || Some(header(id, true, false));
        assert_eq!(
            state(kept(), RunStatus::Done, vec![]),
            (SessionState::Idle, false)
        );
        assert_eq!(
            state(Some(header(id, true, true)), RunStatus::Done, vec![]),
            (SessionState::Closed, true)
        );
        assert_eq!(
            state(kept(), RunStatus::Runnable, vec![close.clone()]),
            (SessionState::Working, true)
        );
        assert_eq!(
            state(kept(), RunStatus::Done, vec![close]),
            (SessionState::Closed, true)
        );
    }

    #[test]
    fn rounds_are_ordered_by_their_number_when_timestamps_tie_and_a_close_has_no_report() {
        let first = RunId::new();
        let mut ids = [first, RunId::new(), RunId::new()];
        ids.sort_by_key(|i| std::cmp::Reverse(i.0));
        // Hashed ids in the reverse order of the rounds, all created in the same instant.
        let mk = |id: RunId, round: u32, closing: bool, status: RunStatus, err: Option<&str>| {
            let mut h = header(first, true, closing);
            h.round = round;
            h.closing = closing;
            let mut rec = with_header(id, Some(h), status, vec![]);
            rec.conversation_id = Some(session_conversation_id("p", first));
            let mut env = crate::envelope::Envelope::decode(id, &rec.state).unwrap();
            env.error = err.map(str::to_owned);
            rec.state = env.encode().unwrap();
            rec
        };
        let runs = vec![
            mk(ids[2], 3, true, RunStatus::Done, None),
            mk(ids[0], 1, false, RunStatus::Done, None),
            mk(ids[1], 2, false, RunStatus::Failed, Some("cancelled: stop")),
        ];
        let view = SessionView::from_rounds(first, runs, t0(), idle_ttl()).unwrap();
        assert_eq!(
            view.latest_run, ids[2],
            "the highest round number is the latest"
        );
        assert_eq!(
            view.report_run, ids[1],
            "the closing round has no report: the last that worked"
        );
        assert_eq!(view.latest.status, RunStatus::Failed);
        assert_eq!(view.round, 3);
        assert!(view.closed);
    }

    #[test]
    fn a_round_that_cannot_be_decoded_closes_its_session() {
        let id = RunId::new();
        let mut rec = with_header(id, Some(header(id, true, false)), RunStatus::Done, vec![]);
        rec.state = serde_json::json!("garbage");
        let read = read_latest(&rec, rec.updated_at, idle_ttl());
        assert!(read.closed);
        assert_eq!(read.state, SessionState::Closed);
    }

    #[test]
    fn open_sessions_come_before_closed_ones() {
        let (working, idle, closed) = (RunId::new(), RunId::new(), RunId::new());
        let mk = |id: RunId, h: SessionHeader, status| {
            let mut rec = with_header(id, Some(h), status, vec![]);
            // Closed sessions are the most recently updated: still last.
            rec.updated_at += chrono::Duration::seconds(if id == closed { 10 } else { 0 });
            rec
        };
        let list = page_of_sessions(
            "p",
            rounds(vec![
                mk(closed, header(closed, false, false), RunStatus::Done),
                mk(idle, header(idle, true, false), RunStatus::Done),
                mk(working, header(working, true, false), RunStatus::Runnable),
            ]),
            0,
            10,
            t0(),
            idle_ttl(),
        );
        assert_eq!(
            list.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![working, idle, closed]
        );
        assert_eq!(list.sessions[0].description, "look");
        assert!(list.sessions[0].keep_open && !list.sessions[2].keep_open);
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
                t0(),
                idle_ttl(),
            );
            assert_eq!(list.sessions[0].outcome, Some(want));
            assert_eq!(list.sessions[0].state, SessionState::Idle);
        }
    }

    fn idle_ttl() -> Duration {
        Duration::from_secs(3600)
    }

    /// The instant the test records are made at.
    fn t0() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    fn after(rec: &RunRecord, secs: i64) -> DateTime<Utc> {
        rec.updated_at + chrono::Duration::seconds(secs)
    }

    #[test]
    fn a_session_idle_for_the_ttl_reads_closed_and_not_a_moment_before() {
        let id = RunId::new();
        let rec = with_header(id, Some(header(id, true, false)), RunStatus::Done, vec![]);
        let before = read_latest(&rec, after(&rec, 3599), idle_ttl());
        assert_eq!((before.state, before.closed), (SessionState::Idle, false));
        assert!(!before.idle_expired);
        let at = read_latest(&rec, after(&rec, 3600), idle_ttl());
        assert_eq!((at.state, at.closed), (SessionState::Closed, true));
        assert!(at.idle_expired);
        assert_eq!(at.outcome, Some(RoundOutcome::Done));
    }

    #[test]
    fn a_session_with_no_header_expires_too_and_a_failed_round_is_idle_like_any() {
        let id = RunId::new();
        for status in [RunStatus::Done, RunStatus::Failed] {
            let rec = with_header(id, None, status, vec![]);
            let read = read_latest(&rec, after(&rec, 7200), idle_ttl());
            assert!(read.idle_expired && read.closed, "{status:?}");
        }
    }

    #[test]
    fn a_working_round_never_expires() {
        let id = RunId::new();
        for status in [RunStatus::Runnable, RunStatus::Parked] {
            let rec = with_header(id, Some(header(id, true, false)), status, vec![]);
            let read = read_latest(&rec, after(&rec, 10 * 24 * 3600), idle_ttl());
            assert_eq!((read.state, read.closed), (SessionState::Working, false));
            assert!(!read.idle_expired);
        }
    }

    #[test]
    fn a_session_closed_another_way_is_not_said_to_have_expired() {
        let id = RunId::new();
        let long = 30 * 24 * 3600;
        // Not kept open: closed by its round's end.
        let once = with_header(id, Some(header(id, false, false)), RunStatus::Done, vec![]);
        assert!(!read_latest(&once, after(&once, long), idle_ttl()).idle_expired);
        // Closed by its header.
        let closed = with_header(id, Some(header(id, true, true)), RunStatus::Done, vec![]);
        assert!(!read_latest(&closed, after(&closed, long), idle_ttl()).idle_expired);
        // A state nobody can read.
        let mut broken = with_header(id, None, RunStatus::Done, vec![]);
        broken.state = serde_json::json!("garbage");
        assert!(!read_latest(&broken, after(&broken, long), idle_ttl()).idle_expired);
    }

    #[test]
    fn a_closing_round_does_not_work_for_anyone() {
        let id = RunId::new();
        let mut closing = header(id, true, true);
        closing.closing = true;
        for (h, status, works) in [
            (Some(header(id, true, false)), RunStatus::Runnable, true),
            (Some(header(id, true, false)), RunStatus::Parked, true),
            (Some(closing.clone()), RunStatus::Runnable, false),
            (Some(header(id, true, false)), RunStatus::Done, false),
        ] {
            let rec = with_header(id, h, status, vec![]);
            let list = page_of_sessions("p", rounds(vec![rec]), 0, 10, t0(), idle_ttl());
            assert_eq!(list.sessions[0].does_work(), works, "{status:?}");
        }
    }

    #[test]
    fn the_defaults_are_the_documented_limits() {
        let d = SessionLimits::default();
        assert_eq!((d.max_open(), d.max_working(), d.max_rounds()), (16, 4, 20));
        assert_eq!(DEFAULT_SESSION_IDLE_TTL, Duration::from_secs(7 * 24 * 3600));
    }

    #[test]
    fn limits_are_checked_against_their_bounds() {
        assert!(SessionLimits::new(1, 1, 2).is_ok());
        assert!(SessionLimits::new(100, 32, 200).is_ok());
        for (open, working, rounds, word) in [
            (0, 1, 2, "max_open"),
            (101, 4, 20, "max_open"),
            (16, 0, 20, "max_working"),
            (64, 33, 20, "max_working"),
            (16, 4, 1, "max_rounds"),
            (16, 4, 201, "max_rounds"),
            (3, 4, 20, "max_working"),
        ] {
            let why = SessionLimits::new(open, working, rounds).expect_err(word);
            assert!(why.contains(word), "{why}");
        }
    }

    #[test]
    fn a_file_only_lowers_the_limits() {
        let d = SessionLimits::default();
        let lowered = d.lowered_by(Some(8), Some(2), Some(10));
        assert_eq!(
            (lowered.max_open, lowered.max_working, lowered.max_rounds),
            (8, 2, 10)
        );
        let raised = d.lowered_by(Some(50), Some(30), Some(100));
        assert_eq!(raised, d, "a larger value changes nothing");
        assert_eq!(d.lowered_by(None, None, None), d);
        // A zero is never a limit.
        let zero = d.lowered_by(Some(0), Some(0), Some(0));
        assert_eq!(
            (zero.max_open(), zero.max_working(), zero.max_rounds()),
            (1, 1, 2)
        );
        // Working never exceeds open, even when the file lowers open alone.
        let narrow = d.lowered_by(Some(2), None, None);
        assert_eq!((narrow.max_open(), narrow.max_working()), (2, 2));
    }
}
