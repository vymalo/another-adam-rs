//! Child sessions, as far as the runtime sees them: a conversation of child runs ("rounds").
//!
//! A session is nothing stored of its own (ADR 0045): its rounds are runs of one agent that share
//! a conversation id derived from the parent's, and the session id is round 1's run id. This module
//! holds the derivation and what a listing reads back from the runs alone.

use adam_core::time::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::collections::HashMap;
use std::time::Duration;

use adam_core::{RunId, RunRecord, RunStatus};

use crate::child::ChildStatus;

/// The separator between the (escaped) parent conversation id and the session id.
const SESSION_SEGMENT: &str = "/s/";

/// The separator between the (escaped) parent conversation id and the id of a timer run.
const TIMER_SEGMENT: &str = "/t/";

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

/// The conversation a timer run of a session lives in: its parent's, made unique by the timer's id,
/// because a conversation holds one open run per agent and a timer stays parked until its session
/// may have expired. A run in it speaks for the parent conversation
/// ([`ChildStarter`](crate::ChildStarter)), and it is no session's conversation.
pub(crate) fn timer_conversation_id(parent_conversation: &str, timer: RunId) -> String {
    format!("{}{TIMER_SEGMENT}{timer}", escape(parent_conversation))
}

/// The parent conversation a timer conversation id names: the inverse of
/// [`timer_conversation_id`], `None` for anything it cannot have produced.
pub(crate) fn parse_timer_conversation_id(conversation_id: &str) -> Option<String> {
    let (escaped, rest) = conversation_id.split_once('/')?;
    let timer = rest.strip_prefix("t/")?;
    let parsed = timer.parse::<uuid::Uuid>().ok()?;
    (parsed.to_string() == timer).then_some(())?;
    unescape(escaped)
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

/// Who may write to a session after it started, chosen when it starts ([ADR 0089] decision 2).
/// Ordered from the narrowest to the widest, as ADR 0054 narrows an option: `OneShot` is narrower
/// than `Sealed`, which is narrower than `Open`.
///
/// The enum is closed on purpose (Rule 3): a new mode must fail to compile in every consumer.
///
/// [ADR 0089]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0089-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// The session closes when its first round has ended: nobody sends it a next round. The
    /// default, and what `keep_open: false` meant.
    #[default]
    OneShot,
    /// The session stays open after a round, and only the run of an agent sends to it: a person
    /// sees it and cannot write to it.
    Sealed,
    /// The session stays open after a round, and both the agent and the person send to it: what
    /// `keep_open: true` meant.
    Open,
}

impl SessionMode {
    /// Every mode, narrowest first: the words a refusal lists.
    pub const ALL: [Self; 3] = [Self::OneShot, Self::Sealed, Self::Open];

    /// The mode's word on the wire and in the tools: `one_shot`, `sealed` or `open`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneShot => "one_shot",
            Self::Sealed => "sealed",
            Self::Open => "open",
        }
    }

    /// The mode named by `word` (`one_shot`, `sealed`, `open`), exactly; `None` for anything else.
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == word)
    }

    /// The mode the `keep_open` of before means: `true` is [`Open`](Self::Open), `false`
    /// [`OneShot`](Self::OneShot). Read for one release (ADR 0089, amended 2026-11-27).
    pub fn of_keep_open(keep_open: bool) -> Self {
        if keep_open { Self::Open } else { Self::OneShot }
    }

    /// Whether the session waits for another message after a round: every mode but
    /// [`OneShot`](Self::OneShot). The `keep_open` a reader of before reads.
    pub fn keeps_open(self) -> bool {
        match self {
            Self::OneShot => false,
            Self::Sealed | Self::Open => true,
        }
    }

    /// Whether a person may write to a session an agent started: only to an [`Open`](Self::Open)
    /// one.
    pub fn takes_person_messages(self) -> bool {
        match self {
            Self::Open => true,
            Self::OneShot | Self::Sealed => false,
        }
    }
}

impl std::fmt::Display for SessionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a round of a session says about the session, in the state of the round's agent (ADR 0045).
///
/// **The truth is the latest round's header** (nothing else stores a session). A session whose
/// rounds carry no header, such as one an embedder made with `Runtime::start_child_continuing`, is
/// kept open: it closes only when nobody has written to it for the runtime's idle TTL
/// ([`Runtime::session_idle_ttl`](crate::Runtime::session_idle_ttl), ADR 0045 slice 4).
///
/// On the wire the header carries `mode` and, for one release, the `keep_open` a reader of before
/// needs ([`SessionMode::keeps_open`]); a header that has only `keep_open`, written before `mode`
/// existed, reads as [`SessionMode::of_keep_open`] says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "HeaderWire", into = "HeaderWire")]
pub struct SessionHeader {
    /// The session: round 1's run id.
    pub id: RunId,
    /// The registration name of the agent whose runs the rounds are.
    pub agent: String,
    /// What the session is for, in the words of whoever started it (a line, at most 80 characters).
    pub description: String,
    /// Who may write to the session after its first round ([ADR 0089] decision 2): it closes when
    /// its round has ended ([`SessionMode::OneShot`], the default of the tools), or it waits for a
    /// message of its agent only ([`SessionMode::Sealed`]) or of the person too
    /// ([`SessionMode::Open`]).
    ///
    /// [ADR 0089]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0089-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
    pub mode: SessionMode,
    /// The session takes no more messages: set by the round that closes it, or by a close request
    /// that a working round has read. Absent when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closed: bool,
    /// This round only closes the session: it calls no model and its report is not a report. Absent
    /// when `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closing: bool,
    /// The session was **deleted** ([ADR 0054] decision 10): it is gone from every listing and every
    /// tool refuses it with `no_such_session`, while its runs stay in the store. Written by the
    /// closing round a `subagent_close` with `delete: true` starts. Absent when `false`.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// The round's number, from 1: what orders the rounds of a session (timestamps tie, ids are
    /// hashes). `0` for a header written before it existed.
    #[serde(default)]
    pub round: u32,
    /// The run this round's model calls name ([ADR 0067], amended): the root run of the task that
    /// opened the session for round one, and of the task that sent it for a round a task sends,
    /// since that is the task the orchestrator bound for the round; a person's round keeps what the
    /// latest header says. `None` for a session a person opened directly (its rounds name their own
    /// task) and in a header written before it existed. Absent when `None`.
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<RunId>,
    /// The run whose workspace this session's rounds work in: the task a session a **task** opened
    /// reads (its root run), and for a session a **person** opened, the **most recently created**
    /// task of the parent context it was opened in, which the A2A front resolves from the store
    /// (`context_workspace`, over the whole context, newest created wins). Created, not updated: a
    /// task's `updated_at` moves with every commit, so it is not the task the person is in. A round
    /// a task sends keeps it, so every round of one session reads the same repository. `None` when
    /// the parent context holds no task at all, which is a round that works in words only — not an
    /// error — and what a header written before it existed reads. Absent when `None`.
    ///
    /// It is **not** [`root`](Self::root): that is the run a round's model calls name, which is the
    /// round's own task for a person's round. A tool that keys a workspace, an environment session
    /// or a scratch project reads this; a tool that keys the notes, the budgets and the gates of a
    /// task reads [`root`](Self::root) (ADR 0045 slice 7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<RunId>,
    /// A client that activated `child-sessions/v1` started or sent this session, so the person
    /// sees it and can answer a round of it: a round of a kept-open session may ask the person
    /// only then (ADR 0045 decision 9, [`SessionHeader::mode`]). The A2A front writes it for
    /// a round it starts, and the session tools write it for a session they start from a run whose
    /// client shows child sessions; a later round keeps it. Absent when `false`, which is what a
    /// header written before it existed reads.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub visible: bool,
    /// The configuration the session runs with ([ADR 0054], decision 3): what its starter asked,
    /// as [`SessionBounds::admit`] accepted it, so a replay reads it and never recomputes it. An
    /// absent option means the folder's. Absent when `None`, which is what a header written before
    /// it existed reads.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<SessionConfig>,
    /// The tokens the session has spent so far ([ADR 0054], decision 6): the earlier rounds' and
    /// this round's, input and output, as `usage/v1` counts them. Each round's agent writes it into
    /// the state it commits, so a listing reads it from the round's record like the rest of the
    /// header, and a send refuses a session that spent its
    /// [`config.token_budget`](SessionConfig::token_budget) without another store read. Absent when
    /// `0`, which is what a header written before it existed reads.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub spent: u64,
}

impl SessionHeader {
    /// The cap on the session's tokens, when it set one ([`SessionConfig::token_budget`]).
    pub fn token_budget(&self) -> Option<u64> {
        self.config.as_ref().and_then(|config| config.token_budget)
    }

    /// Whether the session has spent its token budget: a send is refused, and the next model call
    /// of a round is its closing turn. One rule for the tool, the A2A front and the listing.
    pub fn is_spent(&self) -> bool {
        self.token_budget()
            .is_some_and(|budget| self.spent >= budget)
    }
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

impl SessionHeader {
    /// Whether a person may write to the session ([ADR 0089] decision 2): a session an agent
    /// started takes the person's messages only when it is [`SessionMode::Open`]; a session a
    /// person started is theirs, whatever its mode. A person's session names no root run
    /// ([`root`](Self::root) is `None`), and neither does a header written before `root` existed,
    /// which therefore reads as a person's, as every session did before modes.
    ///
    /// [ADR 0089]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0089-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
    pub fn takes_person_messages(&self) -> bool {
        self.mode.takes_person_messages() || self.root.is_none()
    }
}

/// [`SessionHeader`] as it is stored: `mode`, and `keep_open` beside it for one release, so a
/// reader of before (which requires `keep_open`) still reads the header a round of now writes.
#[derive(Serialize, Deserialize)]
struct HeaderWire {
    id: RunId,
    agent: String,
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<SessionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keep_open: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    closed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    closing: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    deleted: bool,
    #[serde(default)]
    round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<RunId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<SessionConfig>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    spent: u64,
}

impl TryFrom<HeaderWire> for SessionHeader {
    type Error = &'static str;

    /// `mode` when the header has one, else what its `keep_open` meant. A header with neither is
    /// not one a round wrote: it says nothing of whether the session stays open.
    fn try_from(wire: HeaderWire) -> Result<Self, Self::Error> {
        let mode = wire
            .mode
            .or_else(|| wire.keep_open.map(SessionMode::of_keep_open))
            .ok_or("a session header has `mode` (or, written before it, `keep_open`)")?;
        Ok(Self {
            id: wire.id,
            agent: wire.agent,
            description: wire.description,
            mode,
            closed: wire.closed,
            closing: wire.closing,
            deleted: wire.deleted,
            round: wire.round,
            root: wire.root,
            workspace: wire.workspace,
            visible: wire.visible,
            config: wire.config,
            spent: wire.spent,
        })
    }
}

impl From<SessionHeader> for HeaderWire {
    fn from(header: SessionHeader) -> Self {
        Self {
            id: header.id,
            agent: header.agent,
            description: header.description,
            mode: Some(header.mode),
            keep_open: Some(header.mode.keeps_open()),
            closed: header.closed,
            closing: header.closing,
            deleted: header.deleted,
            round: header.round,
            root: header.root,
            workspace: header.workspace,
            visible: header.visible,
            config: header.config,
            spent: header.spent,
        }
    }
}

/// The header a round's agent state holds, if it holds one.
pub(crate) fn header_of(agent_state: &Value) -> Option<SessionHeader> {
    serde_json::from_value(agent_state.get(SESSION_STATE_KEY)?.clone()).ok()
}

/// What a listing reads from a round's record: what its state says about its session, and how the
/// round ended. The envelope is **not** built: its `agent` member is the agent's whole state (for
/// `LlmAgent`, the transcript), and a listing never needs it.
struct RoundFacts {
    /// The header the round's agent keeps under `agent.session`.
    header: Option<SessionHeader>,
    /// A close request waits in the round's inbox.
    close_asked: bool,
    /// How the round ended, as a parent reads it: `Failed` when its state cannot be read.
    status: RunStatus,
    /// Why it failed, when it did.
    error: Option<String>,
    /// The wake the runtime set for the round itself (it watches a run it started), and that
    /// `agent_wake` is the agent's own when it did, exactly as [`RunView`](crate::RunView) reads
    /// them: with `rec.wake_at` they decide whether a parked round waits for a person.
    hb: bool,
    agent_wake: Option<Timestamp>,
}

/// The members of a round's state a listing reads by name. Every other member — `agent` above all —
/// is skipped, so a round's transcript is never materialised.
#[derive(Deserialize)]
struct ListedState {
    #[serde(default)]
    inbox: Vec<ListedInbound>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    hb: bool,
    #[serde(default)]
    agent_wake: Option<Timestamp>,
}

/// The one member of an inbox message a listing reads. Required, as it is in [`Inbound`]: a message
/// without one is not a state the runtime wrote.
///
/// [`Inbound`]: crate::agent::Inbound
#[derive(Deserialize)]
struct ListedInbound {
    kind: String,
}

impl RoundFacts {
    fn of(rec: &RunRecord) -> Self {
        Self::read(rec).unwrap_or_else(|| Self {
            // A round whose state cannot be read is a session nobody can continue: closed, not open
            // for ever.
            header: None,
            close_asked: true,
            status: if rec.status.is_open() {
                rec.status
            } else {
                RunStatus::Failed
            },
            error: None,
            hb: false,
            agent_wake: None,
        })
    }

    fn read(rec: &RunRecord) -> Option<Self> {
        // The envelope's version gate, without the envelope: a state another version wrote is not
        // read here, exactly as `Envelope::decode` refuses it.
        //
        // The contract release: `load_run` and `list_runs` answer the head for a v5 run. The
        // session header, `close_asked` and the error are in the head (its `agent` is the agent's
        // head, which carries `session`), so both versions are read; nothing here needs the
        // transcript, and no body is read.
        let version = rec.state.get("v").and_then(Value::as_u64);
        if version != Some(u64::from(crate::envelope::VERSION))
            && version != Some(u64::from(crate::head::VERSION))
        {
            return None;
        }
        // `agent` is required, and `artifacts`, when present, must be the array the envelope says:
        // a missing one is the envelope's default and is read; a listing keeps neither.
        let agent = rec.state.get("agent")?;
        if rec.state.get("artifacts").is_some_and(|a| !a.is_array()) {
            return None;
        }
        let state = ListedState::deserialize(&rec.state).ok()?;
        Some(Self {
            header: header_of(agent),
            close_asked: state.inbox.iter().any(|i| i.kind == SESSION_CLOSE_KIND),
            status: rec.status,
            error: state.error,
            hb: state.hb,
            agent_wake: state.agent_wake,
        })
    }

    /// Whether the round waits for the person: it is parked with no timer, as
    /// [`RunView::waiting`](crate::RunView::waiting) says of a run. The runtime's own wake on a
    /// run it watches is not the agent's, so a round that waits for a child on a timer does not
    /// read as waiting (`RunView::new`).
    fn waits(&self, rec: &RunRecord) -> bool {
        if rec.status != RunStatus::Parked {
            return false;
        }
        let wake = if self.hb {
            self.agent_wake
        } else {
            rec.wake_at
        };
        wake.is_none()
    }
}

/// The rounds of a session in the order they ran: by the round number the header carries, then
/// by creation time and id. Each with what a listing reads of it.
fn ordered(runs: Vec<RunRecord>) -> Vec<(RunRecord, RoundFacts)> {
    let mut keyed: Vec<(RunRecord, RoundFacts)> = runs
        .into_iter()
        .map(|r| {
            let facts = RoundFacts::of(&r);
            (r, facts)
        })
        .collect();
    keyed.sort_by_key(|(r, f)| {
        (
            f.header.as_ref().map_or(0, |h| h.round),
            r.created_at,
            r.id.0,
        )
    });
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
    pub updated_at: Timestamp,
    /// What the session is for (empty for a session with no header).
    pub description: String,
    /// Who may write to the session after its first round ([`SessionHeader::mode`]);
    /// [`SessionMode::Open`] for a session with no header, which is kept open.
    pub mode: SessionMode,
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
    /// The configuration the session runs with, as its latest round's header says
    /// ([`SessionHeader::config`], ADR 0054); `None` when it has none.
    pub config: Option<SessionConfig>,
    /// The tokens the session has spent so far, as its latest round's header says
    /// ([`SessionHeader::spent`], ADR 0054 decision 6); `0` for a session with no header.
    pub spent: u64,
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
    pub updated_at: Timestamp,
}

impl ReportRound {
    fn of(run: &RunRecord) -> Self {
        Self::of_status(run, &ChildStatus::from_record(run))
    }

    /// The report of `run`, from a status already read of it: a caller that read the status for
    /// another reason builds no second envelope.
    fn of_status(run: &RunRecord, status: &ChildStatus) -> Self {
        Self {
            run: run.id,
            ended: !run.status.is_open(),
            cancelled: status
                .error
                .as_deref()
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
    /// Whether the latest round ends the session: a closing round of a session that is closed. A
    /// round that only cancels what the round before left running (`closing` with the session
    /// still open) does not.
    pub fn closes(&self) -> bool {
        self.closing && self.closed
    }

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

/// The least `max_output_tokens` a session may ask for ([ADR 0054], decision 4): below it an answer
/// is not worth the request, and `0` would leave the cap to the model, which widens.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const MIN_SESSION_MAX_OUTPUT_TOKENS: u32 = 256;

/// The most `max_output_tokens` a session may ask for when its folder leaves the cap to the model
/// (`limits.max_output_tokens: 0`, [ADR 0054] decision 4): the model's own ceiling is not known
/// here, so the decision gives one.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const MAX_SESSION_MAX_OUTPUT_TOKENS: u32 = 1_048_576;

/// What a session's `context_window` must leave for the prompt before compaction can start:
/// 20,000 tokens ([ADR 0054], decision 4).
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const CONTEXT_WINDOW_PROMPT_TOKENS: u32 = 20_000;

/// What a session's `context_window` must leave beside the prompt and the answer: 8,000 tokens
/// ([ADR 0054], decision 4). A smaller window would make the compaction budget zero or less, which
/// turns compaction **off**.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const CONTEXT_WINDOW_HEADROOM_TOKENS: u32 = 8_000;

/// The least `token_budget` a session may ask for ([ADR 0054], decision 4): below it a session is
/// not worth the tokens of the answer.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const MIN_SESSION_TOKEN_BUDGET: u64 = 1_000;

/// The most `token_budget` a session may ask for ([ADR 0054], decision 4): 2^53 − 1, the largest
/// whole number a JSON number carries without losing one.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const MAX_SESSION_TOKEN_BUDGET: u64 = (1 << 53) - 1;

/// The most entries of a session's `tools` or of its `deny_tools` ([ADR 0054], decision 4).
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const MAX_SESSION_TOOL_ENTRIES: usize = 32;

/// The most characters of one entry of a session's `tools` or `deny_tools` ([ADR 0054], decision 4).
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const MAX_SESSION_TOOL_ENTRY_CHARS: usize = 200;

/// The most tool names a refusal lists before it says `and N more`.
const REFUSAL_LIST_NAMES: usize = 50;

/// The most bytes of a session's `report_schema` as compact JSON: 16 KiB ([ADR 0054], decision 4).
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
pub const MAX_REPORT_SCHEMA_BYTES: usize = 16 * 1024;

/// The configuration a session runs with ([ADR 0054]): every option optional, and each one only
/// ever **narrows** what the subagent's folder and the deployment allow ([`SessionBounds::admit`]).
///
/// An absent option stays absent and means "the folder's": nothing is filled in. The type is
/// `#[non_exhaustive]` because the decision adds options slice by slice (`tools`, `model`, …), and
/// an option added must not break a builder. It serialises camelCase, which is how it crosses the
/// wire in `start.config` and how it is journaled in a [`SessionHeader`].
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionConfig {
    /// The most model turns of a round, 1 to the folder's `limits.max_turns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// The most tool calls of a round, 1 to the folder's `limits.max_tool_calls`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// The cap of one model call, [`MIN_SESSION_MAX_OUTPUT_TOKENS`] to the folder's
    /// `limits.max_output_tokens` (to [`MAX_SESSION_MAX_OUTPUT_TOKENS`] when the folder leaves it to
    /// the model). Never `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// The window compaction starts from, at most the model's own window when known, and no smaller
    /// than [`CONTEXT_WINDOW_PROMPT_TOKENS`] + the effective `max_output_tokens` +
    /// [`CONTEXT_WINDOW_HEADROOM_TOKENS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// The most tool calls of one model turn that run at the same time ([ADR 0049]), 1 to the
    /// agent's `max_parallel`.
    ///
    /// [ADR 0049]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_tool_calls: Option<u32>,
    /// The minutes one round may take, 1 to the folder's `limits.max_minutes` (any when the folder
    /// says `0`, none). Never `0`, which would mean no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_limit: Option<u32>,
    /// The most rounds the session may have, 1 to the root's effective `max_session_rounds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    /// A cap on the tokens of the whole session, input and output, as `usage/v1` counts them
    /// ([`MIN_SESSION_TOKEN_BUDGET`] to [`MAX_SESSION_TOKEN_BUDGET`]). Before each model call,
    /// `spent` reaching it ends the round with the closing turn and the words `token budget
    /// reached`, and a send to a session that spent it is refused. Nothing bounds it below that
    /// ceiling today: any value narrows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// An allow-list of the subagent's tools, in the folder's own syntax: exact names and `*`
    /// patterns (`github__*` selects an MCP server). The session is offered only the tools it
    /// matches, and `read_output` stays whatever it says. At most [`MAX_SESSION_TOOL_ENTRIES`]
    /// entries of at most [`MAX_SESSION_TOOL_ENTRY_CHARS`] characters, none blank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// A deny-list in the same syntax, applied after `tools`: a tool it matches is withheld even
    /// when `tools` matches it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_tools: Option<Vec<String>>,
    /// A JSON Schema (draft 2020-12) the round's final answer must match: one JSON value, checked
    /// by the loop at the final answer ([ADR 0054], slice 9). At most [`MAX_REPORT_SCHEMA_BYTES`],
    /// no `$ref` outside itself, and it must compile ([`SessionBounds::admit`]). Local subagents
    /// only.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_schema: Option<Value>,
    /// Text the parent agent (or the person) adds to this session, **after** the folder's own
    /// instructions and never replacing them: at most [`MAX_SESSION_INSTRUCTIONS_CHARS`] characters,
    /// rendered under a fixed heading that says where it comes from and that the text above wins
    /// ([ADR 0054], decision 4).
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The most characters a session's `instructions` may hold ([ADR 0054], decision 4).
pub const MAX_SESSION_INSTRUCTIONS_CHARS: usize = 4_000;

impl SessionConfig {
    /// Whether no option is asked for: the folder's own settings, exactly as no `config` at all.
    pub fn is_empty(&self) -> bool {
        self.max_turns.is_none()
            && self.max_tool_calls.is_none()
            && self.max_output_tokens.is_none()
            && self.context_window.is_none()
            && self.max_parallel_tool_calls.is_none()
            && self.time_limit.is_none()
            && self.max_rounds.is_none()
            && self.token_budget.is_none()
            && self.tools.is_none()
            && self.deny_tools.is_none()
            && self.report_schema.is_none()
            && self.instructions.is_none()
    }

    /// What a round's context carries of this config under `vymalo.session.tools`
    /// (`adam_llm_agent::SESSION_TOOLS_CONTEXT_KEY`, [ADR 0054] slice 3): `{"tools": [..],
    /// "denyTools": [..]}`, only the members the config sets, and `{}` when it sets neither, so a
    /// round never inherits another round's value. One function for the model's tools and the A2A
    /// front, so the two write the same bytes.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[must_use]
    pub fn tools_context(&self) -> Value {
        let mut entry = serde_json::Map::new();
        if let Some(tools) = &self.tools {
            entry.insert("tools".to_owned(), serde_json::json!(tools));
        }
        if let Some(deny) = &self.deny_tools {
            entry.insert("denyTools".to_owned(), serde_json::json!(deny));
        }
        Value::Object(entry)
    }

    /// Read a `config` object ([ADR 0054], decision 1). **An unknown member is refused**, unlike the
    /// members of `start` itself: an ignored narrowing fails open.
    ///
    /// # Errors
    ///
    /// A [`Refusal`] naming the member that is not an option, or the one whose value is not of its
    /// type: a number, or for `tools` and `deny_tools` a list of strings.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    pub fn from_value(value: Value) -> Result<Self, Refusal> {
        serde_json::from_value(value).map_err(|e| Refusal::new(None, e.to_string()))
    }
}

/// Why a [`SessionConfig`] was not admitted ([ADR 0054], decision 2).
///
/// The sentence names the option, the value asked and what is allowed, so a caller can say what to
/// ask instead: `config.max_turns is 500; at most 200`.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Refusal {
    option: Option<String>,
    message: String,
}

impl Refusal {
    fn new(option: Option<&str>, message: String) -> Self {
        Self {
            option: option.map(str::to_owned),
            message,
        }
    }

    /// The name of the option the refusal is about, when it is about one: `max_turns` for
    /// `config.max_turns is 500; at most 200`.
    pub fn option(&self) -> Option<&str> {
        self.option.as_deref()
    }

    /// The whole sentence.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The most a session of one subagent may ask of each option ([ADR 0054], decision 2): computed
/// once at startup by the assembly from the subagent's file and the deployment, and never raised by
/// a request. The starter's own grant (its tier) is added at the call.
///
/// `max_output_tokens: 0` and `time_limit: 0` mean what they mean in a folder: the model's cap and
/// no limit. A request is still held to the ADR's rule for them.
///
/// The fields are public and the type is exhaustive: the assembly builds one per subagent with a
/// literal, once, where the folder and the deployment meet. A slice that adds an option adds its
/// bound and a default in [`Default`], so code that builds one says what it wants and the break is
/// the literal's (the ADR's consequences say so).
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBounds {
    /// The folder's `limits.max_turns`.
    pub max_turns: u32,
    /// The folder's `limits.max_tool_calls`.
    pub max_tool_calls: u32,
    /// The folder's `limits.max_output_tokens`, `0` when it leaves the cap to the model.
    pub max_output_tokens: u32,
    /// The model's context window, when the deployment knows it.
    pub context_window: Option<u32>,
    /// The agent's `max_parallel`, the deployment's `MAX_PARALLEL_TOOL_CALLS` lowered by the file.
    pub max_parallel_tool_calls: u32,
    /// The folder's `limits.max_minutes`, `0` when it sets no limit.
    pub time_limit: u32,
    /// The root's effective `max_session_rounds`.
    pub max_rounds: u32,
    /// The folder's own cap on the session's tokens, when it ever sets one: **no folder does
    /// today** ([ADR 0054], decision 4, "none today: any value narrows"), so this is `None` and
    /// [`admit`](Self::admit) holds a request to the ADR's own ceiling.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    pub token_budget: Option<u64>,
    /// The names of the tools the folder and the deployment can offer the subagent (its own, an MCP
    /// server's as `server__tool`, and what each of its sources declares:
    /// `ToolSource::declared`), **as far as the caller knows them**: a worker fills them from the
    /// node it binds. Empty for a caller that knows none: a control plane binds no agent and cannot
    /// list an MCP server's tools without calling the server.
    pub tools: Vec<String>,
    /// Whether [`tools`](Self::tools) is **every** tool the subagent can have: `false` when a source
    /// cannot name its tools before a run calls it (`ToolSource::declared` answers
    /// `Declared::Unknown`: an endpoint whose listing is another system's and may change with the
    /// conversation), so no list made at startup holds them. A name here may be a **shape**: a
    /// declared name containing `*` (`*__*` for a relayed `<server>__<tool>`) stands for a family
    /// the source can offer, and the set stays complete (ADR 0054, amended 2026-11-28). `admit`
    /// uses the two so that a narrowing is never silently ignored (ADR 0054 decision 2):
    ///
    /// * an entry is admitted when it **selects** a name the set holds: a concrete declared name
    ///   that its pattern matches (`glob(entry, name)`), or a declared shape that matches it as a
    ///   string (`glob(name, entry)`). `*__*` therefore admits `github__*`, `github__search` and
    ///   `*` (the last through the concrete names beside it);
    /// * a `tools` entry that selects nothing is refused, listing the names, **only when the set is
    ///   complete**; otherwise the shape alone is checked (an allow entry that matches nothing
    ///   offers nothing, so it fails narrow);
    /// * a `deny_tools` entry **without a `*`** that selects nothing is refused **whenever any name
    ///   is known**, even when the set is incomplete: a concrete name nobody knows is far more often
    ///   a typo than a tool a source adds, and the refusal says to use a pattern for the latter;
    /// * a `deny_tools` entry with a `*` is checked only when the set is complete, and by the same
    ///   rule as an allow entry — a shape admits a pattern over the family it declares, and never an
    ///   entry that selects nothing. A deny that silently denies nothing is worse than a refusal:
    ///   the person would believe the tool is blocked while it is offered. A pattern over a family
    ///   the set declares but no source provides today (`deny_tools: ["github__*"]` with no server
    ///   attached) is admitted: it denies nothing now and denies that family if a server appears.
    ///
    /// With no name known (a control plane) only the shape is checked, so it admits what a worker
    /// will accept.
    pub tools_complete: bool,
}

impl Default for SessionBounds {
    /// The loop's own defaults: 50 turns, 200 tool calls, 4,096 output tokens, 10 calls at once,
    /// 30 minutes, 20 rounds, no token budget, no known window and no known tools (an incomplete,
    /// empty set).
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_tool_calls: 200,
            max_output_tokens: 4_096,
            context_window: None,
            max_parallel_tool_calls: 10,
            time_limit: 30,
            max_rounds: 20,
            token_budget: None,
            tools: Vec::new(),
            tools_complete: false,
        }
    }
}

impl SessionBounds {
    /// Admit `requested` against these bounds ([ADR 0054], decision 2): the same config when every
    /// option is within them, a [`Refusal`] otherwise. **It never raises a bound and it fills
    /// nothing in**: an option the request leaves out stays out and means the folder's.
    ///
    /// # Errors
    ///
    /// The sentence naming the first option outside its bound.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    pub fn admit(&self, requested: SessionConfig) -> Result<SessionConfig, Refusal> {
        if let Some(v) = requested.max_turns {
            within("max_turns", v, 1, self.max_turns)?;
        }
        if let Some(v) = requested.max_tool_calls {
            within("max_tool_calls", v, 1, self.max_tool_calls)?;
        }
        if let Some(v) = requested.max_output_tokens {
            // `0` would leave the cap to the model, which is wider than any folder that set one:
            // the ADR refuses it whatever the folder says.
            if v == 0 {
                return Err(Refusal::new(
                    Some("max_output_tokens"),
                    format!(
                        "config.max_output_tokens is 0; a session may not leave it to the model: \
                         at least {MIN_SESSION_MAX_OUTPUT_TOKENS}"
                    ),
                ));
            }
            let ceiling = if self.max_output_tokens == 0 {
                MAX_SESSION_MAX_OUTPUT_TOKENS
            } else {
                self.max_output_tokens
            };
            within(
                "max_output_tokens",
                v,
                MIN_SESSION_MAX_OUTPUT_TOKENS,
                ceiling,
            )?;
        }
        if let Some(window) = requested.context_window {
            // The cap the round will really send: the request's, or the folder's, which is what
            // compaction's budget subtracts. The request's was checked above; the folder's is a
            // deployment value, so the sum saturates rather than panicking on a hand-made one.
            let output = requested
                .max_output_tokens
                .unwrap_or(self.max_output_tokens);
            let floor = CONTEXT_WINDOW_PROMPT_TOKENS
                .saturating_add(output)
                .saturating_add(CONTEXT_WINDOW_HEADROOM_TOKENS);
            if window < floor {
                return Err(Refusal::new(
                    Some("context_window"),
                    format!(
                        "config.context_window is {window}; at least {floor}, to leave \
                         {CONTEXT_WINDOW_PROMPT_TOKENS} for the prompt, {output} for the answer and \
                         {CONTEXT_WINDOW_HEADROOM_TOKENS} of room (a smaller one turns compaction \
                         off)"
                    ),
                ));
            }
            if let Some(model) = self.context_window.filter(|model| window > *model) {
                return Err(Refusal::new(
                    Some("context_window"),
                    format!(
                        "config.context_window is {window}; at most {model}, the model's window"
                    ),
                ));
            }
        }
        if let Some(v) = requested.max_parallel_tool_calls {
            within(
                "max_parallel_tool_calls",
                v,
                1,
                self.max_parallel_tool_calls,
            )?;
        }
        if let Some(v) = requested.time_limit {
            if v == 0 {
                return Err(Refusal::new(
                    Some("time_limit"),
                    "config.time_limit is 0; a session may not ask for no limit: at least 1 minute"
                        .to_owned(),
                ));
            }
            // A folder that says `0` sets no limit, so nothing is above it.
            if self.time_limit != 0 && v > self.time_limit {
                return Err(Refusal::new(
                    Some("time_limit"),
                    format!(
                        "config.time_limit is {v}; at most {} minutes",
                        self.time_limit
                    ),
                ));
            }
        }
        if let Some(v) = requested.max_rounds {
            within("max_rounds", v, 1, self.max_rounds)?;
        }
        if let Some(v) = requested.token_budget {
            // The floor and the ceiling are the ADR's, not the folder's; a folder that ever sets a
            // cap of its own lowers the ceiling further and never raises it.
            if v < MIN_SESSION_TOKEN_BUDGET {
                return Err(Refusal::new(
                    Some("token_budget"),
                    format!("config.token_budget is {v}; at least {MIN_SESSION_TOKEN_BUDGET}"),
                ));
            }
            let ceiling = self.token_budget.unwrap_or(MAX_SESSION_TOKEN_BUDGET);
            if v > ceiling {
                return Err(Refusal::new(
                    Some("token_budget"),
                    format!("config.token_budget is {v}; at most {ceiling}"),
                ));
            }
        }
        if let Some(entries) = &requested.tools {
            self.admit_tool_entries("tools", entries)?;
        }
        if let Some(entries) = &requested.deny_tools {
            self.admit_tool_entries("deny_tools", entries)?;
        }
        if let Some(text) = &requested.instructions {
            let chars = text.chars().count();
            if chars > MAX_SESSION_INSTRUCTIONS_CHARS {
                return Err(Refusal::new(
                    Some("instructions"),
                    format!(
                        "config.instructions is {chars} characters; at most \
                         {MAX_SESSION_INSTRUCTIONS_CHARS}"
                    ),
                ));
            }
        }
        if let Some(schema) = &requested.report_schema {
            report_schema_admissible(schema)?;
        }
        Ok(requested)
    }

    /// The entries of `tools` or `deny_tools`: each non-blank and short, not too many, and, as far
    /// as the bound tool names are known (see [`tools_complete`](Self::tools_complete)), each
    /// selecting at least one of them or one of the shapes they declare.
    fn admit_tool_entries(&self, option: &str, entries: &[String]) -> Result<(), Refusal> {
        if entries.len() > MAX_SESSION_TOOL_ENTRIES {
            return Err(Refusal::new(
                Some(option),
                format!(
                    "config.{option} has {} entries; at most {MAX_SESSION_TOOL_ENTRIES}",
                    entries.len()
                ),
            ));
        }
        for entry in entries {
            if entry.trim().is_empty() {
                return Err(Refusal::new(
                    Some(option),
                    format!("config.{option} has a blank entry; name a tool or a pattern"),
                ));
            }
            let chars = entry.chars().count();
            if chars > MAX_SESSION_TOOL_ENTRY_CHARS {
                return Err(Refusal::new(
                    Some(option),
                    format!(
                        "config.{option} has an entry of {chars} characters; at most \
                         {MAX_SESSION_TOOL_ENTRY_CHARS}"
                    ),
                ));
            }
        }
        let names = &self.tools;
        for entry in entries {
            let pattern = entry.contains('*');
            // What is checked, and so refused: see `tools_complete`.
            let checked = if option == "tools" || pattern {
                self.tools_complete
            } else {
                self.tools_complete || !names.is_empty()
            };
            if !checked || selects(names, entry) {
                continue;
            }
            let listed: Vec<&str> = names
                .iter()
                .take(REFUSAL_LIST_NAMES)
                .map(String::as_str)
                .collect();
            let mut list = if listed.is_empty() {
                "none".to_owned()
            } else {
                listed.join(", ")
            };
            if names.len() > REFUSAL_LIST_NAMES {
                list.push_str(&format!(" and {} more", names.len() - REFUSAL_LIST_NAMES));
            }
            let hint = if self.tools_complete {
                String::new()
            } else {
                format!(
                    "; the agent may get more tools while it runs: a pattern such as \"{entry}*\" \
                     names those"
                )
            };
            return Err(Refusal::new(
                Some(option),
                format!(
                    "config.{option} has {entry:?}, which matches none of the names this agent declares: \
                     {list}{hint}"
                ),
            ));
        }
        Ok(())
    }
}

/// Whether `name` matches `pattern`, where `*` stands for any run of characters: the folder's own
/// syntax for a list of tools or skills (`github__*` selects an MCP server). Without a `*` the
/// two must be equal. A session's `tools` and `deny_tools` and the folder's `tools:` use this one
/// function, so a pattern means the same in both.
pub fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    // `split` yields at least one piece; without a `*` there is exactly one.
    let (first, rest) = (parts[0], &parts[1..]);
    let Some((last, middles)) = rest.split_last() else {
        return pattern == name;
    };
    let Some(mut tail) = name.strip_prefix(first) else {
        return false;
    };
    for middle in middles {
        let Some(at) = tail.find(middle) else {
            return false;
        };
        tail = &tail[at + middle.len()..];
    }
    tail.ends_with(last)
}

/// Whether `entry`, an entry of a session's `tools` or `deny_tools`, selects a tool the bound set
/// holds: a declared concrete name its pattern matches, or a declared **shape** — a declared name
/// containing `*` (ADR 0054, amended 2026-11-28) — that matches it as a string. `*__*`, the shape
/// of a relayed `<server>__<tool>`, therefore admits `github__*`, `github__search` and `*` (the
/// last through the concrete names beside it).
///
/// Both lists use it, so a `deny_tools` entry is held to the same rule as an allow one and never to
/// a weaker one: an entry that selects neither a name nor a shape is refused with the list. A deny
/// that silently denies nothing is worse than a refusal, because the person would believe the tool
/// is blocked while it is offered.
fn selects(names: &[String], entry: &str) -> bool {
    names.iter().any(|name| {
        if name.contains('*') {
            // `*` matches the empty run, so the shape alone would admit `github__` or `__`: a
            // relayed name has text on both sides of its `__`, and an entry without it selects
            // nothing at run time.
            glob(name, entry) && relayed_shape(entry)
        } else {
            glob(entry, name)
        }
    })
}

/// Whether `entry` could name a relayed `<server>__<tool>`: text on both sides of its first `__`,
/// which is what the narrowing itself requires of a tool it matches.
fn relayed_shape(entry: &str) -> bool {
    match entry.split_once("__") {
        Some((server, tool)) => !server.is_empty() && !tool.is_empty(),
        None => false,
    }
}

impl SessionBounds {
    /// `config` narrowed to these bounds **as they are now** ([ADR 0054], decision 3): what a later
    /// round copies, so a deployment that lowered a limit since the session started is obeyed and
    /// nothing is raised back. Unlike [`admit`](Self::admit) it refuses nothing: a value above its
    /// bound becomes the bound, and an absent option stays absent. A value below the least an option
    /// takes is raised to that least (`max_output_tokens` is never `0`, which would widen it;
    /// `time_limit` never `0`, which would mean no limit; `token_budget` never below
    /// [`MIN_SESSION_TOKEN_BUDGET`]; a `context_window` leaves a budget), but never above the bound
    /// itself.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[must_use]
    pub fn clamp(&self, config: &SessionConfig) -> SessionConfig {
        let max_output_tokens = config.max_output_tokens.map(|v| {
            let ceiling = if self.max_output_tokens == 0 {
                MAX_SESSION_MAX_OUTPUT_TOKENS
            } else {
                self.max_output_tokens
            };
            v.max(MIN_SESSION_MAX_OUTPUT_TOKENS).min(ceiling)
        });
        let output = max_output_tokens.unwrap_or(self.max_output_tokens);
        let floor = CONTEXT_WINDOW_PROMPT_TOKENS
            .saturating_add(output)
            .saturating_add(CONTEXT_WINDOW_HEADROOM_TOKENS);
        SessionConfig {
            max_turns: config.max_turns.map(|v| v.clamp(1, self.max_turns.max(1))),
            max_tool_calls: config
                .max_tool_calls
                .map(|v| v.clamp(1, self.max_tool_calls.max(1))),
            max_output_tokens,
            context_window: config.context_window.map(|v| {
                let v = v.max(floor);
                self.context_window.map_or(v, |model| v.min(model))
            }),
            max_parallel_tool_calls: config
                .max_parallel_tool_calls
                .map(|v| v.clamp(1, self.max_parallel_tool_calls.max(1))),
            time_limit: config.time_limit.map(|v| {
                let v = v.max(1);
                if self.time_limit == 0 {
                    v
                } else {
                    v.min(self.time_limit)
                }
            }),
            max_rounds: config
                .max_rounds
                .map(|v| v.clamp(1, self.max_rounds.max(1))),
            token_budget: config.token_budget.map(|v| {
                let ceiling = self.token_budget.unwrap_or(MAX_SESSION_TOKEN_BUDGET);
                v.max(MIN_SESSION_TOKEN_BUDGET).min(ceiling)
            }),
            // Entries narrow and are matched against the tools the round really has, so a
            // deployment that dropped a tool since changes nothing here: an entry that matches
            // nothing withholds nothing it should not.
            tools: config.tools.clone(),
            deny_tools: config.deny_tools.clone(),
            report_schema: config.report_schema.clone(),
            // Text has no bound to clamp to: `admit` already refused one that is too long, and a
            // later round keeps what the session was started with.
            instructions: config.instructions.clone(),
        }
    }
}

/// The first `$ref` or `$dynamicRef` of `schema` that does not point inside it (its text does not
/// start with `#`), at any depth.
fn outside_ref(schema: &Value) -> Option<&str> {
    match schema {
        Value::Object(members) => members.iter().find_map(|(key, value)| match value {
            Value::String(target)
                if (key == "$ref" || key == "$dynamicRef") && !target.starts_with('#') =>
            {
                Some(target.as_str())
            }
            _ => outside_ref(value),
        }),
        Value::Array(items) => items.iter().find_map(outside_ref),
        _ => None,
    }
}

/// Whether `schema` may be a session's `report_schema` ([ADR 0054], decision 4): a JSON object, at
/// most [`MAX_REPORT_SCHEMA_BYTES`], no reference outside itself, and a schema of draft 2020-12 that
/// compiles with nothing fetched.
///
/// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
fn report_schema_admissible(schema: &Value) -> Result<(), Refusal> {
    let refuse = |message: String| Refusal::new(Some("report_schema"), message);
    if !schema.is_object() {
        return Err(refuse(
            "config.report_schema must be a JSON Schema, an object".to_owned(),
        ));
    }
    let bytes = serde_json::to_vec(schema).map_or(usize::MAX, |b| b.len());
    if bytes > MAX_REPORT_SCHEMA_BYTES {
        return Err(refuse(format!(
            "config.report_schema is {bytes} bytes; at most {MAX_REPORT_SCHEMA_BYTES} (16 KiB)"
        )));
    }
    if let Some(target) = outside_ref(schema) {
        let shown: String = target.chars().take(80).collect();
        return Err(refuse(format!(
            "config.report_schema has a reference to {shown:?}; a report_schema may refer only to \
             itself (a `$ref` that starts with `#`), never to another document"
        )));
    }
    jsonschema::draft202012::options()
        .offline()
        .build(schema)
        .map(|_| ())
        .map_err(|why| {
            let why: String = why.to_string().chars().take(300).collect();
            refuse(format!(
                "config.report_schema is not a JSON Schema of draft 2020-12 that compiles: {why}"
            ))
        })
}

/// `value` between `min` and `max`, or the sentence that says the option and the range.
fn within(option: &str, value: u32, min: u32, max: u32) -> Result<(), Refusal> {
    if value < min {
        return Err(Refusal::new(
            Some(option),
            format!("config.{option} is {value}; at least {min}"),
        ));
    }
    if value > max {
        return Err(Refusal::new(
            Some(option),
            format!("config.{option} is {value}; at most {max}"),
        ));
    }
    Ok(())
}

/// The newest round of a session, read: what both a listing and a single session's view say.
pub(crate) struct Latest {
    pub state: SessionState,
    pub outcome: Option<RoundOutcome>,
    pub header: Option<SessionHeader>,
    pub closed: bool,
    pub idle_expired: bool,
    pub status: ChildStatus,
    /// The round is parked with no timer: it waits for the person (ADR 0045 decision 9).
    pub waiting: bool,
    /// What it waits for: the question a tool asked, or the reason a park on the model gave.
    pub waiting_reason: Option<String>,
}

/// What a run parked with no timer waits for, from the agent head of its state: the question a
/// tool asked, or the reason of a model park ([ADR 0067]). The same order the A2A front reads
/// (`adam_a2a_runtime::convert::default_prompt`).
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
fn waiting_reason(state: &Value) -> Option<String> {
    let agent = state.get("agent")?;
    agent
        .pointer("/pending_wait/question")
        .or_else(|| agent.pointer("/pending_question/question"))
        .or_else(|| agent.get("question"))
        .or_else(|| agent.pointer("/model_wait/reason"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Read `latest`, the newest round of a session, at `now`, from the facts a listing already read of
/// it: a round that ended at least `idle_ttl` ago in a session that would otherwise stay open reads
/// closed (and `idle_expired`). A round that works never expires, and the report of an expired
/// session is the round's, unchanged. The header and a close request come from `facts`, so the
/// envelope is not built for them.
fn read_latest(
    latest: &RunRecord,
    facts: &RoundFacts,
    now: Timestamp,
    idle_ttl: Duration,
) -> Latest {
    let open = latest.status.is_open();
    let keep_open = facts.header.as_ref().is_none_or(|h| h.mode.keeps_open());
    let closed_already = facts.close_asked
        || facts.header.as_ref().is_some_and(|h| h.closed)
        || (!open && !keep_open);
    let idle_expired =
        !open && !closed_already && now - latest.updated_at >= adam_core::time::span_of(idle_ttl);
    let closed = closed_already || idle_expired;
    let status = ChildStatus::from_record(latest);
    let (state, outcome) = if open {
        (SessionState::Working, None)
    } else {
        let outcome = match (facts.status, facts.error.as_deref()) {
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
    let waiting = open && facts.waits(latest);
    let waiting_reason = if waiting {
        waiting_reason(&latest.state)
    } else {
        None
    };
    Latest {
        state,
        outcome,
        header: facts.header.clone(),
        closed,
        idle_expired,
        status,
        waiting,
        waiting_reason,
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
    now: Timestamp,
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
            let (latest, facts) = runs.pop()?;
            // The report is of the newest round that worked: all of them but a closing one.
            let worked = runs
                .iter()
                .rev()
                .find(|(_, f)| !f.header.as_ref().is_some_and(|h| h.closing))
                .map_or(&latest, |(r, _)| r);
            let read = read_latest(&latest, &facts, now, idle_ttl);
            let report = if read.header.as_ref().is_some_and(|h| h.closing) {
                ReportRound::of(worked)
            } else {
                // The report is of `latest`, whose status `read_latest` already read: no second
                // envelope.
                ReportRound::of_status(&latest, &read.status)
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
                mode: read.header.as_ref().map_or(SessionMode::Open, |h| h.mode),
                closed: read.closed,
                closing: read.header.as_ref().is_some_and(|h| h.closing),
                idle_expired: read.idle_expired,
                started_by: latest.parent_id,
                report,
                config: read.header.as_ref().and_then(|h| h.config.clone()),
                spent: read.header.as_ref().map_or(0, |h| h.spent),
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
    /// Whether the latest round is parked with no timer: it waits for the person, on a question it
    /// asked (ADR 0045 decision 9) or on a model that refused it until woken (ADR 0067). Its
    /// [`state`](Self::state) is still [`Working`](SessionState::Working): the round's work is not
    /// done, and the person's answer is its next step.
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    pub waiting: bool,
    /// What the latest round waits for, when it does: the question it asked, or the reason of a
    /// model park. `None` for a round that does not wait, and for one whose state names no reason.
    pub waiting_reason: Option<String>,
    /// The run of the round the report is of.
    pub report_run: RunId,
    /// The run that started the latest round.
    pub started_by: Option<RunId>,
    /// How many of [`round`](Self::round) only cancel what the round before left running (a closing
    /// round in a session that stays open): ours, not the person's, so no limit counts them.
    pub cleanup_rounds: u32,
}

impl SessionView {
    /// The rounds the person's limit counts: [`round`](Self::round) without the cleanup rounds.
    pub fn countable_rounds(&self) -> u32 {
        self.round.saturating_sub(self.cleanup_rounds)
    }

    /// Whether the latest round was started by a person and not by a run of an agent: a round
    /// that is its own parent.
    pub fn started_outside_a_run(&self) -> bool {
        self.started_by == Some(self.latest_run)
    }

    pub(crate) fn from_rounds(
        session: RunId,
        runs: Vec<RunRecord>,
        now: Timestamp,
        idle_ttl: Duration,
    ) -> Option<Self> {
        let runs = ordered(runs);
        let round = u32::try_from(runs.len()).unwrap_or(u32::MAX);
        let last = runs.len().checked_sub(1)?;
        // The report is the last round that did work: a round that only closes the session has none.
        let worked = runs
            .iter()
            .rposition(|(_, f)| !f.header.as_ref().is_some_and(|h| h.closing))
            .unwrap_or(last);
        let (latest, facts) = &runs[last];
        // A deleted session is gone (ADR 0054 decision 10): not listed, and every tool's `view` is
        // `None`, so each refuses it with `no_such_session`. Its runs stay in the store.
        if facts.header.as_ref().is_some_and(|h| h.deleted) {
            return None;
        }
        let mut read = read_latest(latest, facts, now, idle_ttl);
        // The report is of another round: read its status, the one decode a closing round adds.
        if runs[worked].0.id != latest.id {
            read.status = ChildStatus::from_record(&runs[worked].0);
        }
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
            waiting: read.waiting,
            waiting_reason: read.waiting_reason,
            report_run: runs[worked].0.id,
            started_by: latest.parent_id,
            cleanup_rounds: u32::try_from(
                runs.iter()
                    .filter(|(_, f)| f.header.as_ref().is_some_and(|h| h.closing && !h.closed))
                    .count(),
            )
            .unwrap_or(u32::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use serde_json::json;

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
    fn a_timer_conversation_names_its_parent_and_is_no_session() {
        let timer = RunId::new();
        for parent in ["alice:ctx", "a:x/s/y", "%", "/", ""] {
            let id = timer_conversation_id(parent, timer);
            assert_eq!(parse_timer_conversation_id(&id), Some(parent.to_owned()));
            assert_eq!(parse_session_conversation_id(&id), None, "{parent:?}");
            assert!(!id.starts_with(&session_conversation_prefix(parent)));
        }
        let session = session_conversation_id("alice:ctx", timer);
        assert_eq!(parse_timer_conversation_id(&session), None);
        assert_ne!(
            timer_conversation_id("alice:ctx", timer),
            timer_conversation_id("alice:ctx", RunId::new()),
            "one conversation for each timer"
        );
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
    fn a_header_names_the_root_that_opened_it_and_an_old_one_still_decodes() {
        let id = RunId::new();
        let old = serde_json::json!({
            "id": id, "agent": "a", "description": "d", "keep_open": true, "round": 2
        });
        let decoded: SessionHeader = serde_json::from_value(old).unwrap();
        assert_eq!(decoded.root, None, "a header stored before `root`");
        assert_eq!(
            decoded.workspace, None,
            "a header stored before `workspace`"
        );
        assert_eq!(
            decoded.spent, 0,
            "a header stored before `spent` has spent nothing"
        );
        let encoded = serde_json::to_value(&decoded).unwrap();
        assert!(encoded.get("root").is_none(), "absent when None");
        assert!(encoded.get("workspace").is_none(), "absent when None");
        assert!(encoded.get("spent").is_none(), "absent when 0");
        let root = RunId::new();
        let header = SessionHeader {
            root: Some(root),
            ..decoded
        };
        let back: SessionHeader =
            serde_json::from_value(serde_json::to_value(&header).unwrap()).unwrap();
        assert_eq!(back.root, Some(root));
    }

    /// ADR 0089 slice C2: a header of before, with `keep_open` only, reads as the mode it meant; a
    /// header of now writes `mode` and, for one release, the `keep_open` a reader of before needs;
    /// and one with neither is no header.
    #[test]
    fn a_header_carries_its_mode_and_one_of_before_reads_as_the_mode_keep_open_meant() {
        let id = RunId::new();
        let of = |extra: serde_json::Value| {
            let mut header = serde_json::json!({"id": id, "agent": "a", "description": "d"});
            if let (Some(header), Some(extra)) = (header.as_object_mut(), extra.as_object()) {
                header.extend(extra.clone());
            }
            serde_json::from_value::<SessionHeader>(header)
        };
        let mode = |extra| of(extra).map(|h| h.mode).ok();
        assert_eq!(mode(json!({"keep_open": true})), Some(SessionMode::Open));
        assert_eq!(
            mode(json!({"keep_open": false})),
            Some(SessionMode::OneShot)
        );
        // A one-shot session closed from its start (ADR 0089 slice C1) reads the same way.
        let c1 = of(json!({"keep_open": false, "closed": true})).unwrap();
        assert!(c1.mode == SessionMode::OneShot && c1.closed);
        assert_eq!(mode(json!({"mode": "sealed"})), Some(SessionMode::Sealed));
        assert_eq!(
            mode(json!({"mode": "sealed", "keep_open": true})),
            Some(SessionMode::Sealed),
            "`mode` wins over `keep_open`"
        );
        assert!(
            of(json!({})).is_err(),
            "neither: not a header a round wrote"
        );
        assert!(of(json!({"mode": "ajar"})).is_err(), "an unknown mode");
        for mode in SessionMode::ALL {
            let header = SessionHeader {
                mode,
                ..of(json!({"keep_open": true})).unwrap()
            };
            let encoded = serde_json::to_value(&header).unwrap();
            assert_eq!(encoded["mode"], mode.as_str());
            assert_eq!(encoded["keep_open"], mode != SessionMode::OneShot, "{mode}");
            assert_eq!(
                serde_json::from_value::<SessionHeader>(encoded).unwrap(),
                header
            );
            assert_eq!(SessionMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(SessionMode::default(), SessionMode::OneShot);
        assert!(
            SessionMode::OneShot < SessionMode::Sealed && SessionMode::Sealed < SessionMode::Open
        );
    }

    /// Who may write a session: the person writes to an `open` session of an agent, and to a
    /// session of their own (it names no root run) whatever its mode.
    #[test]
    fn a_person_writes_to_an_open_session_or_their_own() {
        let mut h = header(RunId::new(), true, false);
        for (mode, root, person) in [
            (SessionMode::Open, Some(RunId::new()), true),
            (SessionMode::Sealed, Some(RunId::new()), false),
            (SessionMode::OneShot, Some(RunId::new()), false),
            (SessionMode::OneShot, None, true),
        ] {
            h.mode = mode;
            h.root = root;
            assert_eq!(h.takes_person_messages(), person, "{mode} {root:?}");
        }
    }

    /// The workspace a session's rounds read (ADR 0045 slice 7) is its own field, not `root`: it
    /// round-trips, it is absent when `None`, and a header without it decodes as one without.
    #[test]
    fn a_header_names_the_workspace_its_rounds_read() {
        let workspace = RunId::new();
        let root = RunId::new();
        let header = SessionHeader {
            id: RunId::new(),
            agent: "helper".into(),
            description: "d".into(),
            mode: SessionMode::Open,
            closed: false,
            closing: false,
            deleted: false,
            round: 1,
            root: Some(root),
            workspace: Some(workspace),
            visible: false,
            config: None,
            spent: 0,
        };
        let encoded = serde_json::to_value(&header).unwrap();
        assert_eq!(
            encoded["workspace"],
            serde_json::to_value(workspace).unwrap()
        );
        assert_eq!(encoded["root"], serde_json::to_value(root).unwrap());
        let back: SessionHeader = serde_json::from_value(encoded).unwrap();
        assert_eq!(back, header);
        let none = SessionHeader {
            workspace: None,
            ..header
        };
        assert!(
            serde_json::to_value(&none)
                .unwrap()
                .get("workspace")
                .is_none(),
            "absent when None"
        );
        assert_eq!(none.workspace, None);
    }

    #[test]
    fn a_closing_round_says_close_and_carries_its_header() {
        let header = SessionHeader {
            id: RunId::new(),
            agent: "helper".into(),
            description: "d".into(),
            mode: SessionMode::Open,
            closed: true,
            closing: true,
            deleted: false,
            round: 3,
            root: None,
            workspace: None,
            visible: false,
            config: None,
            spent: 0,
        };
        let payload = closing_round_payload(&header);
        assert_eq!(payload[SESSION_CLOSE_ROUND_KEY], Value::Bool(true));
        assert_eq!(payload.as_object().map(|o| o.len()), Some(2));
        assert_eq!(header_of(&payload), Some(header));
    }

    /// The tokens a session spent ([ADR 0054], decision 6) are a field of its header: they
    /// round-trip, they are absent at `0`, and whether a session is spent is the header's rule
    /// alone, so the tool, the A2A front and the listing cannot disagree.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[test]
    fn a_header_carries_the_tokens_the_session_spent() {
        let mut header = header(RunId::new(), true, false);
        header.config = Some(SessionConfig {
            token_budget: Some(MIN_SESSION_TOKEN_BUDGET),
            ..SessionConfig::default()
        });
        assert_eq!(header.token_budget(), Some(MIN_SESSION_TOKEN_BUDGET));
        assert!(!header.is_spent(), "nothing spent yet");
        header.spent = MIN_SESSION_TOKEN_BUDGET - 1;
        assert!(!header.is_spent(), "one token left");
        header.spent = MIN_SESSION_TOKEN_BUDGET;
        assert!(header.is_spent(), "the budget itself is spent");

        let encoded = serde_json::to_value(&header).unwrap();
        assert_eq!(
            encoded["spent"],
            serde_json::json!(MIN_SESSION_TOKEN_BUDGET),
            "written as a number"
        );
        let back: SessionHeader = serde_json::from_value(encoded).unwrap();
        assert_eq!(back.spent, MIN_SESSION_TOKEN_BUDGET);

        // No budget: any number of tokens is not a spent session.
        header.config = None;
        assert_eq!(header.token_budget(), None);
        assert!(!header.is_spent());
    }

    /// The listing reads `spent` from the latest round's header, like the rest of it: a session
    /// with no header reads `0`.
    #[test]
    fn a_listing_reads_the_spent_of_the_latest_round() {
        let session = RunId::new();
        let conversation = session_conversation_id("p", session);
        let mut first = header(session, true, false);
        first.spent = 700;
        let mut second = header(session, true, false);
        second.round = 2;
        second.spent = 1_800;
        let mut one = with_header(session, Some(first), RunStatus::Done, vec![]);
        one.conversation_id = Some(conversation.clone());
        let mut two = with_header(RunId::new(), Some(second), RunStatus::Done, vec![]);
        two.conversation_id = Some(conversation.clone());
        let list = page_of_sessions("p", rounds(vec![one, two]), 0, 10, t0(), idle_ttl());
        assert_eq!(list.sessions.len(), 1);
        assert_eq!(
            list.sessions[0].spent, 1_800,
            "the latest round's header says it"
        );

        // A session with no header has spent nothing to read.
        let bare = RunId::new();
        let list = page_of_sessions(
            "p",
            rounds(vec![with_header(bare, None, RunStatus::Done, vec![])]),
            0,
            10,
            t0(),
            idle_ttl(),
        );
        assert_eq!(list.sessions[0].spent, 0);
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
        let t =
            Timestamp::from_second(1_700_000_000).unwrap() + jiff::SignedDuration::from_secs(at);
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
            mode: SessionMode::of_keep_open(keep_open),
            closed,
            closing: false,
            deleted: false,
            round: 1,
            root: None,
            workspace: None,
            visible: false,
            config: None,
            spent: 0,
        }
    }

    #[test]
    fn a_header_decides_whether_a_session_is_closed() {
        let state = |h: Option<SessionHeader>, status, inbox| {
            let id = h.as_ref().map_or_else(RunId::new, |h| h.id);
            let rec = with_header(id, h, status, inbox);
            let read = read_latest(&rec, &RoundFacts::of(&rec), rec.updated_at, idle_ttl());
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
        let read = read_latest(&rec, &RoundFacts::of(&rec), rec.updated_at, idle_ttl());
        assert!(read.closed);
        assert_eq!(read.state, SessionState::Closed);
    }

    /// A round as an `LlmAgent` commits one: a valid envelope whose `agent` holds the round's header
    /// and a transcript of about `bytes` bytes under `messages`.
    fn round_with_a_transcript(id: RunId, header: SessionHeader, bytes: usize) -> RunRecord {
        let messages: Vec<Value> = (0..bytes / 128)
            .map(|n| {
                serde_json::json!({"role": "assistant", "text": format!("m{n} {}", "x".repeat(100))})
            })
            .collect();
        let mut agent = serde_json::Map::new();
        agent.insert(
            SESSION_STATE_KEY.to_owned(),
            serde_json::to_value(&header).expect("a header"),
        );
        agent.insert("messages".to_owned(), Value::Array(messages));
        let t = Timestamp::from_second(1_700_000_000).expect("a fixed instant");
        RunRecord {
            id,
            agent: "a".into(),
            conversation_id: Some(session_conversation_id("p", id)),
            parent_id: Some(RunId::new()),
            status: RunStatus::Done,
            state: crate::envelope::Envelope::new(Value::Object(agent))
                .encode()
                .expect("an envelope"),
            wake_at: None,
            version: 2,
            created_at: t,
            updated_at: t,
        }
    }

    /// The members the listing reads are validated to the envelope's shape — the version, `agent`,
    /// `artifacts` when present, and an inbox message's `kind` — so a state wrong in one of them
    /// closes its session, as it did when the listing built the envelope. Members the listing does
    /// not read (a malformed `seq`, an artifact element) are not checked here: the run's next step
    /// refuses them, and no reported value depends on them.
    #[test]
    fn a_state_the_envelope_would_refuse_closes_its_session() {
        let id = RunId::new();
        let unreadable = |mut state: Value, break_it: fn(&mut Value)| {
            break_it(&mut state);
            let mut rec = with_header(id, Some(header(id, true, false)), RunStatus::Done, vec![]);
            rec.state = state;
            let facts = RoundFacts::of(&rec);
            assert!(
                facts.header.is_none() && facts.close_asked,
                "the state is not read"
            );
            let read = read_latest(&rec, &facts, rec.updated_at, idle_ttl());
            assert!(read.closed);
            assert_eq!(read.state, SessionState::Closed);
        };
        let base = || with_header(id, Some(header(id, true, false)), RunStatus::Done, vec![]).state;
        // A positive control: the unmodified state is read, so a `read` that always refused could
        // not make the cases below pass.
        let control = with_header(id, Some(header(id, true, false)), RunStatus::Done, vec![]);
        assert!(RoundFacts::read(&control).is_some(), "the base is read");

        unreadable(base(), |state| state["v"] = serde_json::json!(2));
        unreadable(base(), |state| {
            state.as_object_mut().expect("an envelope").remove("v");
        });
        unreadable(base(), |state| state["artifacts"] = serde_json::json!("x"));
        unreadable(
            with_header(
                id,
                Some(header(id, true, false)),
                RunStatus::Done,
                vec![crate::agent::Inbound::new(
                    SESSION_CLOSE_KIND,
                    serde_json::json!({}),
                )],
            )
            .state,
            |state| {
                state["inbox"][0]
                    .as_object_mut()
                    .expect("a message")
                    .remove("kind");
            },
        );
    }

    /// A deleted session is not a session (ADR 0054 decision 10): `from_rounds` answers `None`, so
    /// the listing drops it and every tool's `view` is `None`, which they refuse as
    /// `no_such_session`. Its runs are still in the store — they are what was read here.
    #[test]
    fn a_deleted_session_is_not_a_session() {
        let session = RunId::new();
        let mut h = header(session, true, true);
        h.closing = true;
        h.deleted = true;
        let rec = round_with_a_transcript(session, h, 1024);
        assert!(
            SessionView::from_rounds(session, vec![rec], t0(), idle_ttl()).is_none(),
            "a deleted session is gone"
        );
    }

    /// A listing reads each round's header from the raw state and never builds the envelope for it,
    /// whose `agent` member is the whole transcript: 20 rounds of a 100 KiB transcript cost one
    /// decode, not twenty. The one decode is the report's: `ChildStatus::from_record` reads the
    /// round the report is of, and a listing reuses that status rather than building a second
    /// envelope for `ReportRound`. A view decodes at most twice — the latest round for its status
    /// and, when a closing round follows, the last round that worked for the report — and a listing
    /// at most once per session (twice when the latest round is closing, whose status a listing
    /// does not use).
    #[test]
    fn a_listing_reads_headers_without_building_the_envelope() {
        let parent = RunId::new();
        let session = RunId::new();
        let conversation = session_conversation_id("p", session);
        let mut runs = Vec::new();
        let mut last = session;
        for round in 1..=20 {
            let id = if round == 1 { session } else { RunId::new() };
            let mut h = header(session, true, false);
            h.round = round;
            let mut rec = round_with_a_transcript(id, h, 100 * 1024);
            rec.conversation_id = Some(conversation.clone());
            rec.parent_id = Some(parent);
            runs.push(rec);
            last = id;
        }
        // The transcripts really are large, and the states really are envelopes.
        let transcript = serde_json::to_vec(&runs[0].state["agent"]["messages"])
            .expect("a transcript")
            .len();
        assert!(transcript >= 100 * 1024, "{transcript}");
        assert!(crate::envelope::Envelope::decode(runs[0].id, &runs[0].state).is_ok());

        // A session view reads the latest round for its status, and no other: the report is of the
        // latest round too, so its status is the one decode, whatever the number of rounds.
        let _ = crate::envelope::decodes::take();
        let view =
            SessionView::from_rounds(session, runs.clone(), t0(), idle_ttl()).expect("a session");
        let decodes = crate::envelope::decodes::take();
        assert_eq!(
            decodes, 1,
            "the latest round's status is the one decode; the report is of that round too"
        );
        assert_eq!(view.round, 20);
        assert_eq!(view.header.as_ref().map(|h| h.round), Some(20));
        assert!(!view.closed);
        // The positive control: the status came out of the decode, so a `read` that decoded nothing
        // and defaulted could not make this pass.
        assert_eq!(view.latest.status, RunStatus::Done);
        assert_eq!(view.report_run, last);

        // A closing round after them: the view reads the closing round's status and the report's,
        // two decodes at most, and the report stays the round that worked.
        let mut closing_header = header(session, true, true);
        closing_header.round = 21;
        closing_header.closing = true;
        let mut closing = round_with_a_transcript(RunId::new(), closing_header, 0);
        closing.conversation_id = Some(conversation.clone());
        closing.parent_id = Some(parent);
        let mut with_closing = runs.clone();
        with_closing.push(closing);
        let _ = crate::envelope::decodes::take();
        let view =
            SessionView::from_rounds(session, with_closing, t0(), idle_ttl()).expect("a session");
        assert_eq!(
            crate::envelope::decodes::take(),
            2,
            "the latest round's status and the report's: the last round that worked"
        );
        assert_eq!(view.report_run, last);

        // A listing reads one report per session, so one decode, whatever the number of rounds;
        // every header came from the raw state.
        let _ = crate::envelope::decodes::take();
        let list = page_of_sessions("p", rounds(runs), 0, 10, t0(), idle_ttl());
        assert_eq!(
            crate::envelope::decodes::count(),
            1,
            "a listing decodes once per session, for the report"
        );
        assert_eq!(list.sessions.len(), 1);
        let summary = &list.sessions[0];
        assert_eq!(summary.round, 20);
        assert_eq!(summary.latest_run, last);
        assert_eq!(summary.description, "look", "the header was read");
        assert!(summary.mode == SessionMode::Open && !summary.closed);
        assert_eq!(summary.outcome, Some(RoundOutcome::Done));
        assert_eq!(summary.report.run, last);
    }

    #[test]
    fn open_sessions_come_before_closed_ones() {
        let (working, idle, closed) = (RunId::new(), RunId::new(), RunId::new());
        let mk = |id: RunId, h: SessionHeader, status| {
            let mut rec = with_header(id, Some(h), status, vec![]);
            // Closed sessions are the most recently updated: still last.
            rec.updated_at += jiff::SignedDuration::from_secs(if id == closed { 10 } else { 0 });
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
        assert_eq!(list.sessions[0].mode, SessionMode::Open);
        assert_eq!(list.sessions[2].mode, SessionMode::OneShot);
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
    fn t0() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    fn after(rec: &RunRecord, secs: i64) -> Timestamp {
        rec.updated_at + jiff::SignedDuration::from_secs(secs)
    }

    #[test]
    fn a_session_idle_for_the_ttl_reads_closed_and_not_a_moment_before() {
        let id = RunId::new();
        let rec = with_header(id, Some(header(id, true, false)), RunStatus::Done, vec![]);
        let before = read_latest(&rec, &RoundFacts::of(&rec), after(&rec, 3599), idle_ttl());
        assert_eq!((before.state, before.closed), (SessionState::Idle, false));
        assert!(!before.idle_expired);
        let at = read_latest(&rec, &RoundFacts::of(&rec), after(&rec, 3600), idle_ttl());
        assert_eq!((at.state, at.closed), (SessionState::Closed, true));
        assert!(at.idle_expired);
        assert_eq!(at.outcome, Some(RoundOutcome::Done));
    }

    #[test]
    fn a_session_with_no_header_expires_too_and_a_failed_round_is_idle_like_any() {
        let id = RunId::new();
        for status in [RunStatus::Done, RunStatus::Failed] {
            let rec = with_header(id, None, status, vec![]);
            let read = read_latest(&rec, &RoundFacts::of(&rec), after(&rec, 7200), idle_ttl());
            assert!(read.idle_expired && read.closed, "{status:?}");
        }
    }

    #[test]
    fn a_working_round_never_expires() {
        let id = RunId::new();
        for status in [RunStatus::Runnable, RunStatus::Parked] {
            let rec = with_header(id, Some(header(id, true, false)), status, vec![]);
            let read = read_latest(
                &rec,
                &RoundFacts::of(&rec),
                after(&rec, 10 * 24 * 3600),
                idle_ttl(),
            );
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
        assert!(
            !read_latest(
                &once,
                &RoundFacts::of(&once),
                after(&once, long),
                idle_ttl()
            )
            .idle_expired
        );
        // Closed by its header.
        let closed = with_header(id, Some(header(id, true, true)), RunStatus::Done, vec![]);
        assert!(
            !read_latest(
                &closed,
                &RoundFacts::of(&closed),
                after(&closed, long),
                idle_ttl()
            )
            .idle_expired
        );
        // A state nobody can read.
        let mut broken = with_header(id, None, RunStatus::Done, vec![]);
        broken.state = serde_json::json!("garbage");
        assert!(
            !read_latest(
                &broken,
                &RoundFacts::of(&broken),
                after(&broken, long),
                idle_ttl()
            )
            .idle_expired
        );
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

    // ── ADR 0054: the session configuration and its admission ────────────────────────────────

    /// Bounds as the assembly computes them for a `reviewer` whose folder allows more than any
    /// test asks for: 200 turns, 500 tool calls, 4,096 output tokens, a 200,000-token window, 10
    /// calls at once, 30 minutes, 12 rounds and no token budget of its own.
    fn bounds() -> SessionBounds {
        SessionBounds {
            max_turns: 200,
            max_tool_calls: 500,
            max_output_tokens: 4_096,
            context_window: Some(200_000),
            max_parallel_tool_calls: 10,
            time_limit: 30,
            max_rounds: 12,
            token_budget: None,
            tools: Vec::new(),
            tools_complete: false,
        }
    }

    /// A config that asks `option` for `value`, and nothing else.
    fn asking(option: &str, value: u32) -> SessionConfig {
        let mut config = SessionConfig::default();
        let slot = match option {
            "max_turns" => &mut config.max_turns,
            "max_tool_calls" => &mut config.max_tool_calls,
            "max_output_tokens" => &mut config.max_output_tokens,
            "context_window" => &mut config.context_window,
            "max_parallel_tool_calls" => &mut config.max_parallel_tool_calls,
            "time_limit" => &mut config.time_limit,
            "max_rounds" => &mut config.max_rounds,
            other => panic!("not an option of ADR 0054 slice 1: {other}"),
        };
        *slot = Some(value);
        config
    }

    #[test]
    fn max_turns_above_its_bound_is_refused_by_name() {
        let why = bounds()
            .admit(asking("max_turns", 500))
            .expect_err("above the bound");
        assert_eq!(why.option(), Some("max_turns"));
        assert!(
            why.message()
                .contains("config.max_turns is 500; at most 200"),
            "{why}"
        );
        // The positive control: the bound itself is admitted, so the refusal is the value.
        assert_eq!(
            bounds()
                .admit(asking("max_turns", 200))
                .expect("at the bound")
                .max_turns,
            Some(200)
        );
    }

    #[test]
    fn max_tool_calls_above_its_bound_is_refused_by_name() {
        let why = bounds()
            .admit(asking("max_tool_calls", 501))
            .expect_err("above the bound");
        assert_eq!(why.option(), Some("max_tool_calls"));
        assert!(
            why.message()
                .contains("config.max_tool_calls is 501; at most 500"),
            "{why}"
        );
        assert_eq!(
            bounds()
                .admit(asking("max_tool_calls", 500))
                .expect("at the bound")
                .max_tool_calls,
            Some(500)
        );
        // Never zero: a round with no turn would do nothing.
        let zero = bounds()
            .admit(asking("max_tool_calls", 0))
            .expect_err("zero");
        assert!(zero.message().contains("at least 1"), "{zero}");
    }

    #[test]
    fn max_output_tokens_above_its_bound_is_refused_by_name() {
        let why = bounds()
            .admit(asking("max_output_tokens", 4_097))
            .expect_err("above the bound");
        assert_eq!(why.option(), Some("max_output_tokens"));
        assert!(
            why.message()
                .contains("config.max_output_tokens is 4097; at most 4096"),
            "{why}"
        );
        assert_eq!(
            bounds()
                .admit(asking("max_output_tokens", 4_096))
                .expect("at the bound")
                .max_output_tokens,
            Some(4_096)
        );
    }

    #[test]
    fn context_window_above_the_models_window_is_refused_by_name() {
        let why = bounds()
            .admit(asking("context_window", 200_001))
            .expect_err("above the model's window");
        assert_eq!(why.option(), Some("context_window"));
        assert!(
            why.message()
                .contains("config.context_window is 200001; at most 200000"),
            "{why}"
        );
        // A deployment that does not know the model's window has no ceiling, only the floor.
        let unknown = SessionBounds {
            context_window: None,
            ..bounds()
        };
        assert!(
            unknown.admit(asking("context_window", 1_000_000)).is_ok(),
            "no window is known, so none is raised"
        );
    }

    #[test]
    fn max_parallel_tool_calls_above_its_bound_is_refused_by_name() {
        let why = bounds()
            .admit(asking("max_parallel_tool_calls", 11))
            .expect_err("above the bound");
        assert_eq!(why.option(), Some("max_parallel_tool_calls"));
        assert!(
            why.message()
                .contains("config.max_parallel_tool_calls is 11; at most 10"),
            "{why}"
        );
        assert_eq!(
            bounds()
                .admit(asking("max_parallel_tool_calls", 10))
                .expect("at the bound")
                .max_parallel_tool_calls,
            Some(10)
        );
    }

    #[test]
    fn time_limit_above_its_bound_is_refused_by_name() {
        let why = bounds()
            .admit(asking("time_limit", 31))
            .expect_err("above the bound");
        assert_eq!(why.option(), Some("time_limit"));
        assert!(
            why.message()
                .contains("config.time_limit is 31; at most 30 minutes"),
            "{why}"
        );
        assert_eq!(
            bounds()
                .admit(asking("time_limit", 30))
                .expect("at the bound")
                .time_limit,
            Some(30)
        );
        // A folder that sets no limit (`max_minutes: 0`) is above every value a session may ask.
        let none = SessionBounds {
            time_limit: 0,
            ..bounds()
        };
        assert!(none.admit(asking("time_limit", 1_440)).is_ok());
        // But no session may ask for *no* limit: that widens.
        let zero = bounds().admit(asking("time_limit", 0)).expect_err("zero");
        assert!(zero.message().contains("at least 1 minute"), "{zero}");
    }

    #[test]
    fn max_rounds_above_its_bound_is_refused_by_name() {
        let why = bounds()
            .admit(asking("max_rounds", 13))
            .expect_err("above the bound");
        assert_eq!(why.option(), Some("max_rounds"));
        assert!(
            why.message()
                .contains("config.max_rounds is 13; at most 12"),
            "{why}"
        );
        assert_eq!(
            bounds()
                .admit(asking("max_rounds", 12))
                .expect("at the bound")
                .max_rounds,
            Some(12)
        );
    }

    /// `token_budget` is bounded by the ADR alone ([ADR 0054], decision 4: "none today: any value
    /// narrows"): 1,000 to 2^53 − 1, with a folder's own cap, when it ever sets one, lowering the
    /// ceiling and never raising it.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    #[test]
    fn token_budget_has_a_floor_and_the_adrs_ceiling() {
        let ask = |v: u64| SessionConfig {
            token_budget: Some(v),
            ..SessionConfig::default()
        };
        let why = bounds().admit(ask(999)).expect_err("below the floor");
        assert_eq!(why.option(), Some("token_budget"));
        assert!(
            why.message()
                .contains("config.token_budget is 999; at least 1000"),
            "{why}"
        );
        assert_eq!(
            bounds()
                .admit(ask(MIN_SESSION_TOKEN_BUDGET))
                .expect("the floor itself")
                .token_budget,
            Some(MIN_SESSION_TOKEN_BUDGET)
        );
        assert_eq!(
            bounds()
                .admit(ask(MAX_SESSION_TOKEN_BUDGET))
                .expect("the ceiling itself")
                .token_budget,
            Some(MAX_SESSION_TOKEN_BUDGET)
        );
        let why = bounds()
            .admit(ask(MAX_SESSION_TOKEN_BUDGET + 1))
            .expect_err("above the ceiling");
        assert_eq!(why.option(), Some("token_budget"));
        assert!(
            why.message()
                .contains(&format!("at most {MAX_SESSION_TOKEN_BUDGET}")),
            "{why}"
        );

        // A folder's own cap, when there ever is one, lowers the ceiling and never raises it.
        let folder = SessionBounds {
            token_budget: Some(50_000),
            ..bounds()
        };
        assert!(folder.admit(ask(50_001)).is_err());
        assert_eq!(
            folder
                .admit(ask(50_000))
                .expect("the folder's cap itself")
                .token_budget,
            Some(50_000)
        );

        // Clamp raises a value below the floor to it, and brings one above the ceiling down to it.
        assert_eq!(
            bounds().clamp(&ask(1)).token_budget,
            Some(MIN_SESSION_TOKEN_BUDGET)
        );
        assert_eq!(folder.clamp(&ask(80_000)).token_budget, Some(50_000));
        // Absent stays absent: nothing is filled in.
        assert_eq!(bounds().clamp(&SessionConfig::default()).token_budget, None);
    }

    #[test]
    fn admit_takes_a_lower_value_for_every_option_and_fills_nothing_in() {
        let requested = SessionConfig {
            max_turns: Some(10),
            max_tool_calls: Some(20),
            max_output_tokens: Some(512),
            context_window: Some(40_000),
            max_parallel_tool_calls: Some(2),
            time_limit: Some(3),
            max_rounds: Some(4),
            token_budget: Some(5_000),
            report_schema: Some(serde_json::json!({"type": "object"})),
            ..SessionConfig::default()
        };
        let admitted = bounds().admit(requested.clone()).expect("all lowered");
        assert_eq!(
            admitted, requested,
            "each option as asked, none raised to its bound"
        );

        // Nothing is filled in: an absent option stays absent and means the folder's.
        let one = SessionConfig {
            max_turns: Some(5),
            ..SessionConfig::default()
        };
        let admitted = bounds().admit(one.clone()).expect("one option");
        assert_eq!(admitted, one);
        assert_eq!(admitted.max_tool_calls, None, "the rest is the folder's");
        assert!(bounds().admit(SessionConfig::default()).is_ok());
        assert!(SessionConfig::default().is_empty() && !admitted.is_empty());
    }

    #[test]
    fn admit_refuses_a_raised_value_for_every_option_and_never_clamps_it() {
        // A clamp would answer the bound; the ADR refuses, so nothing runs with a setting the
        // starter did not ask for.
        for (option, over, bound) in [
            ("max_turns", 201, 200),
            ("max_tool_calls", 501, 500),
            ("max_output_tokens", 4_097, 4_096),
            ("context_window", 200_001, 200_000),
            ("max_parallel_tool_calls", 11, 10),
            ("time_limit", 31, 30),
            ("max_rounds", 13, 12),
        ] {
            let why = bounds()
                .admit(asking(option, over))
                .expect_err("above its bound");
            assert_eq!(why.option(), Some(option), "{option}: {why}");
            assert!(
                why.message().contains(&format!("at most {bound}")),
                "{option}: {why}"
            );
        }
    }

    fn reporting(schema: Value) -> SessionConfig {
        SessionConfig {
            report_schema: Some(schema),
            ..SessionConfig::default()
        }
    }

    #[test]
    fn a_report_schema_of_draft_2020_12_is_admitted_as_asked() {
        let schema = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/node"}},
            "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
        });
        let asked = reporting(schema);
        assert_eq!(bounds().admit(asked.clone()).expect("admitted"), asked);
        assert!(!asked.is_empty());
    }

    #[test]
    fn a_report_schema_with_a_reference_outside_itself_is_refused_by_name() {
        for outside in [
            serde_json::json!({"$ref": "https://example.com/other.json"}),
            serde_json::json!({"$ref": "other.json"}),
            serde_json::json!({"properties": {"a": {"items": [{"$ref": "file:///etc/x.json"}]}}}),
            serde_json::json!({"$dynamicRef": "https://example.com/meta"}),
        ] {
            let why = bounds()
                .admit(reporting(outside.clone()))
                .expect_err("a reference to another document");
            assert_eq!(why.option(), Some("report_schema"), "{outside}");
            assert!(
                why.message().contains("config.report_schema")
                    && why.message().contains("refer only to itself"),
                "{}",
                why.message()
            );
        }
    }

    #[test]
    fn a_report_schema_over_16_kib_is_refused_and_one_at_the_limit_is_not() {
        let padded = |len: usize| serde_json::json!({"description": "x".repeat(len)});
        let overhead = serde_json::to_vec(&padded(0)).unwrap().len();
        let at = padded(MAX_REPORT_SCHEMA_BYTES - overhead);
        assert_eq!(
            serde_json::to_vec(&at).unwrap().len(),
            MAX_REPORT_SCHEMA_BYTES
        );
        bounds().admit(reporting(at)).expect("exactly 16 KiB");
        let why = bounds()
            .admit(reporting(padded(MAX_REPORT_SCHEMA_BYTES - overhead + 1)))
            .expect_err("one byte over");
        assert_eq!(why.option(), Some("report_schema"));
        assert!(
            why.message().contains("at most 16384 (16 KiB)"),
            "{}",
            why.message()
        );
    }

    #[test]
    fn a_report_schema_that_does_not_compile_or_is_not_an_object_is_refused() {
        let broken = bounds()
            .admit(reporting(serde_json::json!({"type": "no-such-type"})))
            .expect_err("not a schema");
        assert_eq!(broken.option(), Some("report_schema"));
        assert!(
            broken.message().contains("draft 2020-12 that compiles"),
            "{}",
            broken.message()
        );
        let scalar = bounds()
            .admit(reporting(serde_json::json!("string")))
            .expect_err("not an object");
        assert!(
            scalar.message().contains("an object"),
            "{}",
            scalar.message()
        );
    }

    #[test]
    fn a_report_schema_crosses_the_wire_camel_case_and_survives_a_clamp() {
        let config = SessionConfig::from_value(serde_json::json!({
            "reportSchema": {"type": "string"},
        }))
        .expect("an option");
        assert_eq!(
            config.report_schema,
            Some(serde_json::json!({"type": "string"}))
        );
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::json!({"reportSchema": {"type": "string"}})
        );
        assert_eq!(bounds().clamp(&config), config);
    }

    #[test]
    fn clamp_brings_every_option_down_to_the_bounds_of_now_and_never_raises_one() {
        let ask = SessionConfig {
            max_turns: Some(150),
            max_tool_calls: Some(400),
            max_output_tokens: Some(4_000),
            context_window: Some(150_000),
            max_parallel_tool_calls: Some(8),
            time_limit: Some(25),
            max_rounds: Some(10),
            token_budget: Some(8_000),
            report_schema: Some(serde_json::json!({"type": "object"})),
            ..SessionConfig::default()
        };
        // A deployment that lowered every limit since the session started.
        let lowered = SessionBounds {
            max_turns: 40,
            max_tool_calls: 90,
            max_output_tokens: 1_000,
            context_window: Some(60_000),
            max_parallel_tool_calls: 3,
            time_limit: 10,
            max_rounds: 5,
            token_budget: Some(4_000),
            tools: Vec::new(),
            tools_complete: false,
        };
        let clamped = lowered.clamp(&ask);
        assert_eq!(clamped.max_turns, Some(40));
        assert_eq!(clamped.max_tool_calls, Some(90));
        assert_eq!(clamped.max_output_tokens, Some(1_000));
        assert_eq!(clamped.context_window, Some(60_000));
        assert_eq!(clamped.max_parallel_tool_calls, Some(3));
        assert_eq!(clamped.time_limit, Some(10));
        assert_eq!(clamped.max_rounds, Some(5));
        assert_eq!(clamped.token_budget, Some(4_000));
        // Within the bounds: unchanged. Absent: stays absent.
        assert_eq!(bounds().clamp(&ask), ask);
        assert_eq!(
            bounds().clamp(&SessionConfig::default()),
            SessionConfig::default()
        );
    }

    #[test]
    fn clamp_never_produces_a_value_that_widens() {
        let zero = SessionConfig {
            max_output_tokens: Some(0),
            time_limit: Some(0),
            max_turns: Some(0),
            ..SessionConfig::default()
        };
        let clamped = bounds().clamp(&zero);
        assert_eq!(
            clamped.max_output_tokens,
            Some(MIN_SESSION_MAX_OUTPUT_TOKENS),
            "0 would leave the cap to the model"
        );
        assert_eq!(clamped.time_limit, Some(1), "0 would mean no limit");
        assert_eq!(clamped.max_turns, Some(1));
        // A window that leaves no budget is raised to the least that leaves one, with the cap it
        // really has, and a model's smaller window still wins.
        let small = SessionConfig {
            context_window: Some(1_000),
            max_output_tokens: Some(2_000),
            ..SessionConfig::default()
        };
        let floor = CONTEXT_WINDOW_PROMPT_TOKENS + 2_000 + CONTEXT_WINDOW_HEADROOM_TOKENS;
        assert_eq!(bounds().clamp(&small).context_window, Some(floor));
    }

    #[test]
    fn a_zero_output_cap_is_refused_whatever_the_folder_leaves_it_to_the_model() {
        for (bounds, what) in [
            (bounds(), "a folder with a cap"),
            (
                SessionBounds {
                    max_output_tokens: 0,
                    ..bounds()
                },
                "a folder that leaves it to the model",
            ),
        ] {
            let why = bounds
                .admit(asking("max_output_tokens", 0))
                .expect_err("zero widens");
            assert_eq!(why.option(), Some("max_output_tokens"), "{what}");
            assert!(why.message().contains("at least 256"), "{what}: {why}");
        }
        // When the folder leaves the cap to the model, the ADR's ceiling is the bound.
        let model = SessionBounds {
            max_output_tokens: 0,
            ..bounds()
        };
        assert_eq!(
            model
                .admit(asking("max_output_tokens", MAX_SESSION_MAX_OUTPUT_TOKENS))
                .expect("the ceiling itself")
                .max_output_tokens,
            Some(MAX_SESSION_MAX_OUTPUT_TOKENS)
        );
        assert!(
            model
                .admit(asking(
                    "max_output_tokens",
                    MAX_SESSION_MAX_OUTPUT_TOKENS + 1
                ))
                .is_err()
        );
    }

    #[test]
    fn a_window_that_leaves_no_budget_is_refused() {
        let floor = CONTEXT_WINDOW_PROMPT_TOKENS + 4_096 + CONTEXT_WINDOW_HEADROOM_TOKENS;
        let why = bounds()
            .admit(asking("context_window", floor - 1))
            .expect_err("no budget left");
        assert_eq!(why.option(), Some("context_window"));
        assert!(
            why.message().contains(&format!("at least {floor}")),
            "{why}"
        );
        assert!(
            why.message().contains("compaction off"),
            "the refusal says why: {why}"
        );
        assert!(
            bounds().admit(asking("context_window", floor)).is_ok(),
            "the floor leaves a budget"
        );

        // The floor follows the cap the round will really send: a session that asks for a larger
        // answer needs a larger window.
        let wide = SessionBounds {
            max_output_tokens: 200_000,
            ..bounds()
        };
        let request = SessionConfig {
            max_output_tokens: Some(100_000),
            context_window: Some(floor),
            ..SessionConfig::default()
        };
        let why = wide
            .admit(request)
            .expect_err("the window is below the new floor");
        let raised = CONTEXT_WINDOW_PROMPT_TOKENS + 100_000 + CONTEXT_WINDOW_HEADROOM_TOKENS;
        assert!(
            why.message().contains(&format!("at least {raised}")),
            "{why}"
        );
    }

    #[test]
    fn an_unknown_option_is_refused_and_named() {
        let why = SessionConfig::from_value(serde_json::json!({
            "maxTurns": 4,
            "maxParallelTools": 2,
        }))
        .expect_err("an unknown member fails open if ignored");
        assert!(
            why.message().contains("unknown field"),
            "serde says what kind: {why}"
        );
        assert!(
            why.message().contains("maxParallelTools"),
            "the member is named: {why}"
        );

        // The known members read, camelCase as `start.config` writes them.
        let config = SessionConfig::from_value(serde_json::json!({
            "maxTurns": 4,
            "contextWindow": 40_000,
            "timeLimit": 2,
        }))
        .expect("every member is an option");
        assert_eq!(config.max_turns, Some(4));
        assert_eq!(config.context_window, Some(40_000));
        assert_eq!(config.time_limit, Some(2));
        assert_eq!(config.max_rounds, None, "an absent member stays absent");
    }

    #[test]
    fn a_header_carries_the_config_and_round_trips_with_its_version() {
        let id = RunId::new();
        let mut h = header(id, true, false);
        h.config = Some(asking("max_turns", 7));

        // The wire form is camelCase, and an absent option is absent.
        let encoded = serde_json::to_value(&h).expect("a header");
        assert_eq!(encoded["config"]["maxTurns"], serde_json::json!(7));
        assert!(
            encoded["config"].get("maxRounds").is_none(),
            "an absent option is not written"
        );
        let back: SessionHeader = serde_json::from_value(encoded).expect("a header");
        assert_eq!(back, h);

        // A header journaled before `config` existed reads as the folder's settings.
        let old = serde_json::json!({
            "id": id, "agent": "a", "description": "d", "keep_open": true,
        });
        let decoded: SessionHeader = serde_json::from_value(old).expect("an old header");
        assert_eq!(decoded.config, None);

        // Through the state the runtime keeps, at either version its gate accepts: the header and
        // its config come back as they went in.
        for v in [crate::envelope::VERSION, crate::head::VERSION] {
            let state = serde_json::json!({
                "v": v,
                "agent": { SESSION_STATE_KEY: serde_json::to_value(&h).expect("a header") },
            });
            let mut rec = record(
                id,
                &session_conversation_id("p", id),
                Some(RunId::new()),
                RunStatus::Done,
                None,
                1,
            );
            rec.state = state;
            let read = RoundFacts::of(&rec);
            assert_eq!(
                read.header.as_ref().map(|h| h.config.clone()),
                Some(h.config.clone()),
                "version {v}"
            );
            assert_eq!(
                read.header.as_ref().map(|h| h.round),
                Some(1),
                "version {v}"
            );
        }
    }

    // ── ADR 0054 slice 3: `tools` and `deny_tools` ───────────────────────────────────────────

    /// Bounds of a worker that binds `reviewer` with these tools: two of its own and two of an MCP
    /// server.
    fn bound_with_tools() -> SessionBounds {
        SessionBounds {
            tools: [
                "read_file",
                "write_file",
                "linear__list_issues",
                "linear__get_issue",
            ]
            .map(str::to_owned)
            .to_vec(),
            tools_complete: true,
            ..bounds()
        }
    }

    /// The same, for an agent a source adds tools to while it runs: the names are known, but not
    /// all of them (a source that answers `Declared::Unknown`, ADR 0054 decision 4).
    fn bound_with_incomplete_tools() -> SessionBounds {
        SessionBounds {
            tools_complete: false,
            ..bound_with_tools()
        }
    }

    fn listing(tools: &[&str], deny: &[&str]) -> SessionConfig {
        let own = |names: &[&str]| {
            (!names.is_empty()).then(|| names.iter().map(|n| (*n).to_owned()).collect())
        };
        SessionConfig {
            tools: own(tools),
            deny_tools: own(deny),
            ..SessionConfig::default()
        }
    }

    #[test]
    fn a_star_matches_any_run() {
        assert!(glob("linear__*", "linear__list_issues"));
        assert!(glob("linear__*", "linear__"));
        assert!(!glob("linear__*", "github__list"));
        assert!(!glob("linear__*", "linear_"));
        assert!(glob("*_issues", "linear__list_issues"));
        assert!(glob("*", "anything"));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(glob("a*b*c", "abc"));
        assert!(!glob("a*b*c", "acb"));
        assert!(!glob("a*b*c", "aXbY"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
        // The pieces of a pattern cannot overlap: `ab*ba` needs four characters.
        assert!(!glob("ab*ba", "aba"));
    }

    #[test]
    fn a_tools_pattern_that_matches_no_bound_tool_is_refused_with_the_list() {
        for (option, asked) in [
            ("tools", listing(&["red_file"], &[])),
            ("deny_tools", listing(&[], &["red_file"])),
        ] {
            let why = bound_with_tools().admit(asked).expect_err("matches none");
            assert_eq!(why.option(), Some(option));
            assert!(
                why.message().contains(&format!(
                    "config.{option} has \"red_file\", which matches none of the names this agent declares: \
                     read_file, write_file, linear__list_issues, linear__get_issue"
                )),
                "{why}"
            );
        }
        // The positive control: names and patterns that match something are admitted as asked.
        let ok = listing(&["read_file", "linear__*"], &["write_*"]);
        assert_eq!(bound_with_tools().admit(ok.clone()).expect("matches"), ok);
    }

    #[test]
    fn an_incomplete_set_refuses_a_concrete_deny_entry_it_does_not_know_and_checks_no_pattern() {
        let bound = bound_with_incomplete_tools();
        // A typo of a concrete deny entry: refused, with the list and the way to name the rest.
        let why = bound
            .admit(listing(&[], &["red_file"]))
            .expect_err("a concrete name no tool has");
        assert_eq!(why.option(), Some("deny_tools"));
        assert!(
            why.message().contains(
                "config.deny_tools has \"red_file\", which matches none of the names this agent declares: read_file, \
                 write_file, linear__list_issues, linear__get_issue; the agent may get more tools \
                 while it runs: a pattern such as \"red_file*\" names those"
            ),
            "{why}"
        );
        // A pattern, and an allow entry, are not checked: a source may add what they select.
        let ok = listing(&["github__*", "later_tool"], &["github__*", "later_*"]);
        assert_eq!(bound.admit(ok.clone()).expect("not checked"), ok);
        // A known name is admitted.
        assert!(bound.admit(listing(&[], &["write_file"])).is_ok());
        // With no name known at all (a control plane) even a concrete deny entry is shape only.
        assert!(bounds().admit(listing(&[], &["red_file"])).is_ok());
    }

    #[test]
    fn a_complete_set_checks_patterns_and_allow_entries_too() {
        let bound = bound_with_tools();
        for asked in [
            listing(&["github__*"], &[]),
            listing(&[], &["github__*"]),
            listing(&["later_tool"], &[]),
        ] {
            assert!(bound.admit(asked.clone()).is_err(), "{asked:?}");
        }
        // No hint to name more tools by pattern: the set is every tool there is.
        let why = bound.admit(listing(&["x"], &[])).unwrap_err();
        assert!(!why.message().contains("while it runs"), "{why}");
        // A complete empty set (an agent with no tools) refuses with `none`.
        let bare = SessionBounds {
            tools: Vec::new(),
            tools_complete: true,
            ..bounds()
        };
        assert!(
            bare.admit(listing(&["x"], &[]))
                .unwrap_err()
                .message()
                .ends_with("matches none of the names this agent declares: none")
        );
    }

    #[test]
    fn a_server_pattern_selects_the_tools_of_an_mcp_server() {
        let admitted = bound_with_tools()
            .admit(listing(&["linear__*"], &[]))
            .expect("linear__ matches linear__list_issues");
        assert_eq!(admitted.tools, Some(vec!["linear__*".to_owned()]));
        assert!(glob("linear__*", "linear__list_issues"));
        // A server the agent has no tool of selects nothing.
        let why = bound_with_tools()
            .admit(listing(&["github__*"], &[]))
            .expect_err("no github tools");
        assert!(why.message().contains("\"github__*\""), "{why}");
    }

    /// ADR 0054 decision 4, amended 2026-11-28 (ANR-1390, the owner's answer to ANR-Q155): a
    /// declaration may name a **shape**. `*__*` stands for the `<server>__<tool>` tools the
    /// thread-tools source relays from the servers a person attached, so a session may name one by
    /// pattern, by name, or everything — and an entry that selects nothing is still refused.
    #[test]
    fn a_declared_shape_admits_a_relayed_servers_tools() {
        let bound = SessionBounds {
            tools: ["turn_output", "*__*"].map(str::to_owned).to_vec(),
            tools_complete: true,
            ..bounds()
        };
        for entry in ["github__*", "github__search", "*", "turn_output", "*__*"] {
            assert!(
                bound.admit(listing(&[entry], &[])).is_ok(),
                "{entry} selects a declared name or the shape"
            );
        }
        // A concrete name, or a pattern, that selects neither is refused with the list, and the
        // shape is listed as itself.
        for entry in ["a_typo", "github_", "*_issues"] {
            let why = bound
                .admit(listing(&[entry], &[]))
                .expect_err("selects nothing");
            assert_eq!(why.option(), Some("tools"), "{entry}");
            assert!(
                why.message()
                    .contains("matches none of the names this agent declares: turn_output, *__*"),
                "{entry}: {why}"
            );
            assert!(!why.message().contains("while it runs"), "{entry}: {why}");
        }
        // `deny_tools` is held to the same rule and never a weaker one: a relayed server's pattern
        // is a real deny, and a typo of a concrete name is still refused.
        assert!(
            bound.admit(listing(&[], &["github__*"])).is_ok(),
            "a deny over the declared family is a deny of something"
        );
        let why = bound
            .admit(listing(&[], &["write_fil"]))
            .expect_err("a typo of a concrete name");
        assert_eq!(why.option(), Some("deny_tools"));
        assert!(
            why.message()
                .contains("matches none of the names this agent declares: turn_output, *__*"),
            "{why}"
        );
    }

    #[test]
    fn a_long_list_of_tools_is_cut_at_fifty_names() {
        let many = SessionBounds {
            tools: (0..53).map(|n| format!("tool_{n:02}")).collect(),
            tools_complete: true,
            ..bounds()
        };
        let why = many.admit(listing(&["nope"], &[])).expect_err("none");
        assert!(why.message().contains("tool_49 and 3 more"), "{why}");
        assert!(!why.message().contains("tool_50"), "{why}");
    }

    #[test]
    fn without_known_tools_an_entry_is_admitted_by_its_shape_alone() {
        let unknown = bounds();
        assert!(unknown.tools.is_empty() && !unknown.tools_complete);
        let ask = listing(&["anything_at_all", "server__*"], &["x"]);
        assert_eq!(unknown.admit(ask.clone()).expect("shape only"), ask);
        // The shape is still held.
        for (asked, words) in [
            (listing(&["  "], &[]), "config.tools has a blank entry"),
            (listing(&[], &[""]), "config.deny_tools has a blank entry"),
            (
                listing(&[&"x".repeat(201)], &[]),
                "config.tools has an entry of 201 characters; at most 200",
            ),
            (
                listing(&[&"x".repeat(200)], &[&"y".repeat(201)]),
                "config.deny_tools has an entry of 201 characters; at most 200",
            ),
        ] {
            let why = unknown.admit(asked).expect_err("bad shape");
            assert!(why.message().contains(words), "{why}");
        }
        let names: Vec<String> = (0..33).map(|n| format!("t{n}")).collect();
        let too_many = SessionConfig {
            tools: Some(names.clone()),
            ..SessionConfig::default()
        };
        let why = unknown.admit(too_many).expect_err("33 entries");
        assert_eq!(why.message(), "config.tools has 33 entries; at most 32");
        let at_most = SessionConfig {
            deny_tools: Some(names[..32].to_vec()),
            ..SessionConfig::default()
        };
        assert!(unknown.admit(at_most).is_ok());
    }

    #[test]
    fn tools_and_deny_tools_count_as_set_and_survive_a_clamp() {
        assert!(!listing(&["a"], &[]).is_empty());
        assert!(!listing(&[], &["a"]).is_empty());
        let both = listing(&["a*"], &["ab"]);
        assert_eq!(bounds().clamp(&both), both);
        // The wire is camelCase and carries only what is set.
        assert_eq!(
            serde_json::to_value(&both).expect("serialises"),
            serde_json::json!({"tools": ["a*"], "denyTools": ["ab"]})
        );
    }
}
