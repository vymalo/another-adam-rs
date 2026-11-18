//! Child sessions, as far as the runtime sees them: a conversation of child runs ("rounds").
//!
//! A session is nothing stored of its own (ADR 0045): its rounds are runs of one agent that share
//! a conversation id derived from the parent's, and the session id is round 1's run id. This module
//! holds the derivation and what a listing reads back from the runs alone.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::collections::HashMap;

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
/// kept open and never closes by itself.
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
    /// The run that started the latest round: the parent run of that round.
    pub started_by: Option<RunId>,
}

impl SessionSummary {
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

/// The newest round of a session, read: what both a listing and a single session's view say.
pub(crate) struct Latest {
    pub state: SessionState,
    pub outcome: Option<RoundOutcome>,
    pub header: Option<SessionHeader>,
    pub closed: bool,
    pub status: ChildStatus,
}

/// Read `latest`, the newest round of a session.
pub(crate) fn read_latest(latest: &RunRecord) -> Latest {
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
    let closed = close_asked || header.as_ref().is_some_and(|h| h.closed) || (!open && !keep_open);
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
) -> SessionList {
    let mut sessions: Vec<SessionSummary> = rounds
        .into_iter()
        .filter_map(|((agent, conversation_id), runs)| {
            let id = session_of(parent_conversation, &conversation_id)?;
            // Round 1 is the run whose id is the session id, started as a child.
            runs.iter().find(|r| r.id == id && r.parent_id.is_some())?;
            let mut runs: Vec<RunRecord> = ordered(runs).into_iter().map(|(r, _)| r).collect();
            let round = u32::try_from(runs.len()).unwrap_or(u32::MAX);
            let latest = runs.pop()?;
            let read = read_latest(&latest);
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
                started_by: latest.parent_id,
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

    pub(crate) fn from_rounds(session: RunId, runs: Vec<RunRecord>) -> Option<Self> {
        let runs = ordered(runs);
        let round = u32::try_from(runs.len()).unwrap_or(u32::MAX);
        let (latest, _) = runs.last()?.clone();
        let worked = runs
            .iter()
            .rev()
            .find(|(_, h)| !h.as_ref().is_some_and(|h| h.closing))
            .map_or_else(|| latest.clone(), |(r, _)| r.clone());
        let mut read = read_latest(&latest);
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
            let read = read_latest(&rec);
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
        let view = SessionView::from_rounds(first, runs).unwrap();
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
        let read = read_latest(&rec);
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
            );
            assert_eq!(list.sessions[0].outcome, Some(want));
            assert_eq!(list.sessions[0].state, SessionState::Idle);
        }
    }
}
