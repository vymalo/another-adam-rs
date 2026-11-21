//! `child-sessions/v1`: the sessions of an agent's helpers (ADR 0045) as A2A contexts and tasks.
//!
//! A session is a chain of runs of a helper agent that share one conversation
//! ([`adam_runtime::session_conversation_id`]). To a client that **activated** the extension, the
//! session is a **child context** and each of its rounds a task of it; the parent task carries the
//! list of its sessions. The contract is `docs/api/child-sessions-v1.md` of
//! `vymalo/another-agentic-system`.
//!
//! # Context ids
//!
//! | | Parent context `C` | Child context `K` of session `S` |
//! |---|---|---|
//! | the client says | `C` | `session_conversation_id(C, S)`, `<C escaped>/s/<S>` |
//! | the store keeps | `encode_conversation(subject, C)` | `session_conversation_id(encode_conversation(subject, C), S)` |
//!
//! `K` is reserved: a message to a root context that has that shape is refused whether or not the
//! extension is active, and a session cannot hold a session (`C` itself must not have the shape).
//! The caller owns `K` when the subject in the stored parent part is its own.
//!
//! # Requests
//!
//! | Request | Answer |
//! |---|---|
//! | a message to `C` with `start` | round 1 of a new session; no turn of the parent |
//! | a message to `K`, no `taskId` | to a working session: delivered into its round; to an idle one: the next round; to a closed one: `UnsupportedOperation` |
//! | a message to `K` with `close` | an idle session closes by a round of its own, a working one when its round ends (`cancel` first when asked) |
//! | `taskId` of a round | the follow-up rules of any task, in context `K` |
//! | `GetTask`, `SubscribeToTask`, `CancelTask` of a round | the round as a task; cancel is `Runtime::cancel` |
//! | `ListTasks {contextId: K}` | the rounds of the session; an unknown or foreign `K` is an empty page |
//!
//! Without activation every request about `K` or a round is `InvalidParams` (`TaskNotFound` for a
//! round of another caller), before anything is written, and a task carries nothing of this page.
//!
//! # A person's round is its own parent
//!
//! A round a person starts has no run of an agent behind it, so its parent is the round itself
//! ([`SessionView::started_outside_a_run`](adam_runtime::SessionView::started_outside_a_run)): no
//! run's end cancels it and no run waits for it. `by` is `person` for exactly those rounds.

use std::collections::HashMap;
use std::time::Duration;

use a2a::{Message, Task, TaskState};
use adam_a2a::{BackendError, CHILD_SESSIONS_EXTENSION, Caller, ChildSessionInfo, ExtensionConfig};
use adam_core::{RunId, RunRecord};
use adam_runtime::{
    Inbound, MAX_SESSION_PAGE, RoundOutcome, RunView, RuntimeError, SESSION_CLOSE_KIND,
    SESSION_STATE_KEY, SessionHeader, SessionLimits, SessionState, SessionSummary, SessionView,
    closing_round_payload, parse_session_conversation_id, session_conversation_id,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::backend::{RuntimeTaskBackend, map_err};
use crate::convert::{decode_conversation, encode_conversation};
use crate::ids::task_id_for;
use crate::task_state;

/// The most characters of a session's description (the contract's bound).
pub const MAX_DESCRIPTION_CHARS: usize = 80;

/// The most rounds a session may have before a message to it is refused. A close is always
/// allowed, even at the limit, so a person can always end a session.
pub const MAX_SESSION_ROUNDS: u32 = 20;

/// The most sessions the parent task's object lists once the open ones are in: closed sessions,
/// most recently updated first.
const CLOSED_LISTED: usize = 16;

/// How many times a request that finds its session changing under it looks again.
const ATTEMPTS: usize = 3;

/// One helper a person may start a session with.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildSessionAgent {
    /// The name the card lists and a `start` names.
    pub name: String,
    /// The name the helper's runs are registered under on the runtime (`coder/reviewer`).
    pub agent: String,
    /// What the helper does, for the card.
    pub description: String,
    /// The time limit of each of its rounds.
    pub within: Option<Duration>,
    /// Whether a round of it works on a task of another agent, which a cancel of the round or an
    /// idle expiry does not stop by itself: the process that serves the front holds no endpoint, so
    /// a reader of such a session starts a closing round, which a worker steps and which cancels it.
    pub remote: bool,
}

impl ChildSessionAgent {
    /// A helper listed as `name` whose runs are registered as `agent`, with no time limit.
    pub fn new(
        name: impl Into<String>,
        agent: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            agent: agent.into(),
            description: description.into(),
            within: None,
            remote: false,
        }
    }

    /// Mark the helper as one whose rounds work on a task of another agent.
    #[must_use]
    pub fn remote(mut self) -> Self {
        self.remote = true;
        self
    }

    /// The time limit of each of its rounds.
    #[must_use]
    pub fn within(mut self, within: Duration) -> Self {
        self.within = Some(within);
        self
    }
}

/// The helpers an agent serves `child-sessions/v1` for, and its limits.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChildSessions {
    agents: Vec<ChildSessionAgent>,
    max_open: u32,
    max_working: u32,
    max_rounds: u32,
    listing_page: usize,
}

impl ChildSessions {
    /// The helpers (each name once, none blank, each registered name once) and the most sessions
    /// open in one parent context (1 or more).
    ///
    /// # Errors
    ///
    /// The sentence that says what is wrong: no helper, a blank or repeated name, a repeated
    /// registered name, or `max_open` of 0.
    pub fn new(
        agents: impl IntoIterator<Item = ChildSessionAgent>,
        max_open: u32,
    ) -> Result<Self, String> {
        let agents: Vec<_> = agents.into_iter().collect();
        if agents.is_empty() {
            return Err("child-sessions/v1 needs at least one helper".into());
        }
        if max_open == 0 {
            return Err("child-sessions/v1 needs maxOpen of at least 1".into());
        }
        for (at, a) in agents.iter().enumerate() {
            if a.name.trim().is_empty() || a.agent.is_empty() {
                return Err("a helper needs a name and a registered agent".into());
            }
            if agents[..at]
                .iter()
                .any(|b| b.name == a.name || b.agent == a.agent)
            {
                return Err(format!("the helper `{}` is listed twice", a.name));
            }
        }
        Ok(Self {
            agents,
            max_open,
            // The tools' default until `with_limits`, so the two agree; never more than may be open.
            max_working: max_open.min(SessionLimits::default().max_working()),
            max_rounds: MAX_SESSION_ROUNDS,
            listing_page: MAX_SESSION_PAGE,
        })
    }

    /// How many sessions one read of the listing asks for (default 100, the most the runtime
    /// gives). Smaller only to test paging.
    #[doc(hidden)]
    #[must_use]
    pub fn with_listing_page(mut self, page: usize) -> Self {
        self.listing_page = page.clamp(1, MAX_SESSION_PAGE);
        self
    }

    /// The most rounds a session may have (default [`MAX_SESSION_ROUNDS`]), at least 2.
    #[must_use]
    pub fn with_max_rounds(mut self, max_rounds: u32) -> Self {
        self.max_rounds = max_rounds.max(2);
        self
    }

    /// The deployment's limits ([`SessionLimits`]): the card's `maxOpen`, the most rounds working
    /// at once and the most rounds a session may have. A person's start, or a message that would
    /// start a round, past `max_working` is refused with `UnsupportedOperation`. Without it the
    /// front uses the tools' defaults (16 open, 4 working, 20 rounds), with `max_working` at most
    /// `max_open`.
    ///
    /// Pass the root's *effective* limits (`Assembly::session_limits()` or
    /// `AgentDef::effective_session_limits()`), which the file may have lowered, so that the card
    /// and the tools agree, and not the deployment's `SessionSettings::limits`:
    /// `AgentDef::child_sessions` in `adam-assembly` does it.
    #[must_use]
    pub fn with_limits(mut self, limits: SessionLimits) -> Self {
        self.max_open = limits.max_open();
        self.max_working = limits.max_working();
        self.max_rounds = limits.max_rounds();
        self
    }

    /// The helpers, in the order the card lists them.
    pub fn agents(&self) -> &[ChildSessionAgent] {
        &self.agents
    }

    /// The card entry, `None` only when the contract's bounds are not met.
    pub fn card_extension(&self) -> Option<ExtensionConfig> {
        let listed: Vec<_> = self
            .agents
            .iter()
            .map(|a| ChildSessionInfo::new(a.name.clone(), a.description.clone()))
            .collect();
        ExtensionConfig::child_sessions(&listed, self.max_open)
    }

    fn by_name(&self, name: &str) -> Option<&ChildSessionAgent> {
        self.agents.iter().find(|a| a.name == name)
    }

    fn by_registered(&self, agent: &str) -> Option<&ChildSessionAgent> {
        self.agents.iter().find(|a| a.agent == agent)
    }

    fn registered(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.agent.as_str()).collect()
    }

    pub(crate) fn names_exist_in(&self, registered: &[String]) -> Result<(), String> {
        for a in &self.agents {
            if !registered.contains(&a.agent) {
                return Err(a.agent.clone());
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Context ids
// ---------------------------------------------------------------------------

/// What a context id the client sent is.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContextKind {
    /// An ordinary context.
    Root,
    /// The child context of session `.1` of parent context `.0`.
    Child(String, RunId),
    /// A child context of a child context, which no session can have.
    TooDeep,
}

pub(crate) fn kind_of(context: &str) -> ContextKind {
    let Some((parent, session)) = parse_session_conversation_id(context) else {
        return ContextKind::Root;
    };
    if parse_session_conversation_id(&parent).is_some() {
        return ContextKind::TooDeep;
    }
    ContextKind::Child(parent, session)
}

/// The stored conversation id of the parent context `context` of `subject`.
fn parent_conversation(subject: &str, context: &str) -> String {
    encode_conversation(subject, context)
}

/// The child context and session a stored session conversation id names, when `subject` owns it:
/// `(parent context, session, child context)`.
fn owned_session_conversation(
    subject: &str,
    conversation: &str,
) -> Option<(String, RunId, String)> {
    let (parent, session) = parse_session_conversation_id(conversation)?;
    let (owner, context) = decode_conversation(&parent)?;
    if owner != subject || parse_session_conversation_id(&context).is_some() {
        return None;
    }
    let child = session_conversation_id(&context, session);
    Some((context, session, child))
}

// ---------------------------------------------------------------------------
// What a message asks
// ---------------------------------------------------------------------------

/// A `start`, read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Start {
    pub agent: String,
    pub description: String,
    pub keep_open: bool,
}

/// What a message asks of the child-sessions extension.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Asked {
    Nothing,
    Start(Start),
    Close { cancel: bool },
}

fn members(message: &Message) -> Result<Option<&Value>, String> {
    let Some(entry) = message
        .metadata
        .as_ref()
        .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
    else {
        return Ok(None);
    };
    if !entry.is_object() {
        return Err(format!(
            "the metadata under {CHILD_SESSIONS_EXTENSION} is not an object"
        ));
    }
    Ok(Some(entry))
}

/// What `message` asks. Unknown members are ignored; `start` and `close` are objects of the
/// contract's members, and a message cannot ask both.
pub(crate) fn asked(message: &Message, sessions: &ChildSessions) -> Result<Asked, String> {
    let Some(entry) = members(message)? else {
        return Ok(Asked::Nothing);
    };
    let (start, close) = (entry.get("start"), entry.get("close"));
    match (start, close) {
        (None, None) => Ok(Asked::Nothing),
        (Some(_), Some(_)) => Err("a message starts a session or closes one, not both".into()),
        (Some(start), None) => parse_start(start, sessions).map(Asked::Start),
        (None, Some(close)) => parse_close(close).map(|cancel| Asked::Close { cancel }),
    }
}

fn parse_start(start: &Value, sessions: &ChildSessions) -> Result<Start, String> {
    let object = start
        .as_object()
        .ok_or("`start` is not an object: {agent, description, keepOpen}")?;
    let agent = object
        .get("agent")
        .and_then(Value::as_str)
        .ok_or("`start.agent` is missing: it is a string")?;
    if sessions.by_name(agent).is_none() {
        let names: Vec<&str> = sessions.agents.iter().map(|a| a.name.as_str()).collect();
        return Err(format!(
            "no helper `{}`; `start.agent` is one of: {}",
            agent.chars().take(80).collect::<String>().escape_debug(),
            names.join(", ")
        ));
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .ok_or("`start.description` is missing: it is a string")?;
    if description.trim().is_empty() {
        return Err("`start.description` is blank".into());
    }
    if description.chars().any(char::is_control) {
        return Err("`start.description` is one line without control characters".into());
    }
    let length = description.chars().count();
    if length > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "`start.description` is {length} characters; at most {MAX_DESCRIPTION_CHARS}"
        ));
    }
    let keep_open = match object.get("keepOpen") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(keep)) => *keep,
        Some(_) => return Err("`start.keepOpen` is true or false".into()),
    };
    Ok(Start {
        agent: agent.to_owned(),
        description: description.trim().to_owned(),
        keep_open,
    })
}

fn parse_close(close: &Value) -> Result<bool, String> {
    let object = close
        .as_object()
        .ok_or("`close` is not an object: {cancel}")?;
    match object.get("cancel") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(cancel)) => Ok(*cancel),
        Some(_) => Err("`close.cancel` is true or false".into()),
    }
}

/// How a session reads on the wire: `working`, `idle` or `closed`.
///
/// A session is closed when it is, or when its latest round is the one that closes it; a session
/// that was asked to close while its round works is still `working` (and its rounds say `closing`).
pub(crate) fn wire_state(state: SessionState, latest_closes: bool) -> &'static str {
    match state {
        SessionState::Closed => "closed",
        _ if latest_closes => "closed",
        SessionState::Idle => "idle",
        SessionState::Working => "working",
    }
}

/// The sessions a parent object lists: those not closed on the wire (working, idle, or a round
/// still ending after a close), then the most recent [`CLOSED_LISTED`] closed ones. The input is
/// in the runtime's order, so closed ones are already newest first.
fn select_listed(listed: Vec<SessionSummary>) -> Vec<SessionSummary> {
    let (mut open, closed): (Vec<_>, Vec<_>) = listed
        .into_iter()
        .partition(|s| wire_state(s.state, s.closes()) != "closed");
    open.extend(closed.into_iter().take(CLOSED_LISTED));
    open
}

pub(crate) fn a2a_state_word(state: &TaskState) -> Value {
    serde_json::to_value(state).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// A round of a session of the caller, located.
pub(crate) struct Round {
    pub run: RunId,
    pub view: RunView,
    /// The child context.
    pub context: String,
    pub parent_context: String,
    pub session: RunId,
    pub by_person: bool,
}

impl RuntimeTaskBackend {
    fn sessions(&self) -> Result<&ChildSessions, BackendError> {
        self.child_sessions
            .as_ref()
            .ok_or_else(|| BackendError::InvalidParams("this agent has no child sessions".into()))
    }

    /// Whether `caller` activated the extension and this agent serves it.
    pub(crate) fn sessions_activated(&self, caller: &Caller) -> bool {
        self.child_sessions.is_some() && caller.has_extension(CHILD_SESSIONS_EXTENSION)
    }

    /// The round behind `task_id`, if it is one of a session of a helper of this agent and of the
    /// caller. Judged from the raw record first: a record that is not the caller's is never
    /// decoded.
    pub(crate) async fn round_of(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> Result<Option<Round>, BackendError> {
        let Some(sessions) = &self.child_sessions else {
            return Ok(None);
        };
        let Ok(uuid) = Uuid::parse_str(task_id) else {
            return Ok(None);
        };
        let run = RunId(uuid);
        let Some(rec) = self
            .runtime
            .store()
            .load_run(run)
            .await
            .map_err(|e| map_err(e.into()))?
        else {
            return Ok(None);
        };
        if sessions.by_registered(&rec.agent).is_none() {
            return Ok(None);
        }
        let Some((parent_context, session, context)) = rec
            .conversation_id
            .as_deref()
            .and_then(|c| owned_session_conversation(&caller.subject, c))
        else {
            return Ok(None);
        };
        let Some(view) = self.runtime.view(run).await.map_err(map_err)? else {
            return Ok(None);
        };
        Ok(Some(Round {
            run,
            view,
            context,
            parent_context,
            session,
            by_person: rec.parent_id == Some(run),
        }))
    }

    /// The session `session` of parent context `context`, if it is the caller's and a helper's.
    pub(crate) async fn session_of(
        &self,
        caller: &Caller,
        context: &str,
        session: RunId,
    ) -> Result<Option<SessionView>, BackendError> {
        let sessions = self.sessions()?;
        let view = self
            .runtime
            .session_view(&parent_conversation(&caller.subject, context), session)
            .await
            .map_err(map_err)?;
        let Some(view) = view.filter(|v| sessions.by_registered(&v.agent).is_some()) else {
            return Ok(None);
        };
        // Whoever reads such a session starts what ends the remote task it left running. A failure
        // is tried again by the next read: the session itself is as it was.
        let helper = sessions.by_registered(&view.agent);
        if needs_cleanup(&view, helper)
            && let Err(error) = self
                .start_cleanup(caller, context, &view, helper.and_then(|h| h.within))
                .await
        {
            tracing::warn!(session = %view.id, %error, "cancelling the remote task of a session failed");
        }
        Ok(Some(view))
    }

    /// The round `run` as a task of child context `context`, and what it says about its session.
    pub(crate) async fn round_task(
        &self,
        caller: &Caller,
        session: &SessionView,
        round: &Round,
    ) -> Task {
        let task = self.task_of(&round.view, &round.context).await;
        let mut task = self.said(caller, &round.view, task);
        task.metadata.get_or_insert_with(HashMap::new).insert(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            self.round_object(session, round),
        );
        task
    }

    fn round_object(&self, session: &SessionView, round: &Round) -> Value {
        let header = header_of_state(&round.view.state);
        let sessions = self.child_sessions.as_ref();
        let name = sessions
            .and_then(|s| s.by_registered(&session.agent))
            .map_or(session.agent.as_str(), |a| a.name.as_str());
        let latest = session.header.as_ref();
        let number = header
            .as_ref()
            .map(|h| h.round)
            .filter(|n| *n > 0)
            .unwrap_or(session.round);
        json!({
            "session": session.id,
            "parentContextId": round.parent_context,
            "agent": name,
            "description": latest.map_or("", |h| h.description.as_str()),
            "keepOpen": latest.is_some_and(|h| h.keep_open),
            "round": number,
            "by": if round.by_person { "person" } else { "agent" },
            "state": wire_state(session.state, latest.is_some_and(|h| h.closing && h.closed)),
            "closing": session.closed && session.state == SessionState::Working,
        })
    }

    /// `task`, a task of `context`, with what this extension says to `caller`: the sessions of a
    /// parent context, or the session a round belongs to. Nothing for a caller that did not
    /// activate it.
    pub(crate) async fn with_sessions(
        &self,
        caller: &Caller,
        mut task: Task,
    ) -> Result<Task, BackendError> {
        if !self.sessions_activated(caller) {
            return Ok(task);
        }
        let entry = match kind_of(&task.context_id) {
            ContextKind::Root => match self.sessions_object(caller, &task.context_id).await {
                Some(entry) => entry,
                None => return Ok(task),
            },
            ContextKind::Child(parent, session) => {
                let Some(round) = self.round_of(caller, &task.id).await? else {
                    return Ok(task);
                };
                let Some(view) = self.session_of(caller, &parent, session).await? else {
                    return Ok(task);
                };
                self.round_object(&view, &round)
            }
            ContextKind::TooDeep => return Ok(task),
        };
        task.metadata
            .get_or_insert_with(HashMap::new)
            .insert(CHILD_SESSIONS_EXTENSION.to_owned(), entry);
        Ok(task)
    }

    /// The sessions of the parent context `context`, open first, then the most recently closed.
    /// `None` when they could not be listed: a task then says nothing, never "no sessions".
    pub(crate) async fn sessions_object(&self, caller: &Caller, context: &str) -> Option<Value> {
        let listed = match self.listed_sessions(caller, context).await {
            Ok(listed) => listed,
            Err(e) => {
                tracing::warn!(error = %e, "the sessions of a context could not be listed");
                return None;
            }
        };
        let mut out = Vec::new();
        for s in &listed {
            out.push(self.summary_object(context, s).await);
        }
        Some(json!({ "sessions": out }))
    }

    /// The sessions a parent object lists: open first, then the most recently closed.
    pub(crate) async fn listed_sessions(
        &self,
        caller: &Caller,
        context: &str,
    ) -> Result<Vec<SessionSummary>, RuntimeError> {
        // The listing ranks working, then idle, then closed sessions (newest first within each),
        // so reading pages until 16 closed ones are in hand has every open session and the 16
        // most recently closed: only closed sessions are ever left unread.
        let enough = |all: &[SessionSummary]| {
            all.iter()
                .filter(|s| s.state == SessionState::Closed)
                .count()
                >= CLOSED_LISTED
        };
        let listed = self.list_pages(caller, context, enough).await?;
        Ok(select_listed(listed))
    }

    /// The name a client knows the helper `agent` by (its card name).
    pub(crate) fn session_agent_name(&self, agent: &str) -> String {
        self.child_sessions
            .as_ref()
            .and_then(|c| c.by_registered(agent))
            .map_or_else(|| agent.to_owned(), |a| a.name.clone())
    }

    /// What the session of `round` says now, as on a task of it. `None` when it cannot be read.
    pub(crate) async fn round_status(&self, caller: &Caller, round: &Round) -> Option<Value> {
        let view = self
            .session_of(caller, &round.parent_context, round.session)
            .await
            .ok()??;
        Some(self.round_object(&view, round))
    }

    /// The sessions of `context` page by page, until `enough` says what is read will do or none
    /// are left.
    async fn list_pages(
        &self,
        caller: &Caller,
        context: &str,
        enough: impl Fn(&[SessionSummary]) -> bool,
    ) -> Result<Vec<SessionSummary>, RuntimeError> {
        let Some(sessions) = &self.child_sessions else {
            return Ok(Vec::new());
        };
        let parent = parent_conversation(&caller.subject, context);
        let names = sessions.registered();
        let mut all: Vec<SessionSummary> = Vec::new();
        loop {
            let page = self
                .runtime
                .list_sessions(&parent, &names, all.len(), sessions.listing_page)
                .await?;
            let empty = page.sessions.is_empty();
            // A session on a remote helper that expired idle: reading it starts its cleanup.
            for expired in page.sessions.iter().filter(|s| {
                s.idle_expired
                    && !s.closing
                    && sessions.by_registered(&s.agent).is_some_and(|a| a.remote)
            }) {
                let _ = self.session_of(caller, context, expired.id).await;
            }
            all.extend(page.sessions);
            if empty || all.len() >= page.total || enough(&all) {
                return Ok(all);
            }
        }
    }

    async fn summary_object(&self, context: &str, s: &SessionSummary) -> Value {
        let sessions = self.child_sessions.as_ref();
        let name = sessions
            .and_then(|c| c.by_registered(&s.agent))
            .map_or(s.agent.as_str(), |a| a.name.as_str());
        let round_state = match self.runtime.view(s.latest_run).await {
            Ok(Some(view)) => a2a_state_word(&task_state(&view)),
            _ => Value::Null,
        };
        json!({
            "session": s.id,
            "contextId": session_conversation_id(context, s.id),
            "agent": name,
            "description": s.description,
            "keepOpen": s.keep_open,
            "state": wire_state(s.state, s.closes()),
            "round": {"number": s.round, "taskId": s.latest_run, "state": round_state},
        })
    }

    /// The task behind `task_id` for a request that reads or stops it: a root task of this agent,
    /// or a round of a session of the caller (`InvalidParams` when the request did not activate
    /// the extension). A task of another caller, or that does not exist, is `None`.
    pub(crate) async fn locate(
        &self,
        caller: &Caller,
        task_id: &str,
    ) -> Result<Option<(RunId, RunView, String)>, BackendError> {
        if let Some(found) = self.owned(caller, task_id).await? {
            return Ok(Some(found));
        }
        let Some(round) = self.round_of(caller, task_id).await? else {
            return Ok(None);
        };
        if !caller.has_extension(CHILD_SESSIONS_EXTENSION) {
            return Err(not_activated());
        }
        Ok(Some((round.run, round.view, round.context)))
    }

    /// A message that is about a session: a `start` to a parent context, anything to a child
    /// context, or a message naming a round. `None` for every other message, which the root path
    /// answers.
    pub(crate) async fn submit_child(
        &self,
        caller: &Caller,
        message: &Message,
        task_id: Option<&str>,
        context_id: Option<&str>,
    ) -> Result<Option<Task>, BackendError> {
        let sessions = self.sessions()?;
        let activated = caller.has_extension(CHILD_SESSIONS_EXTENSION);
        let round = match task_id {
            Some(task) => self.round_of(caller, task).await?,
            None => None,
        };
        let kind = context_id.map(kind_of);
        let addressed =
            round.is_some() || matches!(kind, Some(ContextKind::Child(..) | ContextKind::TooDeep));
        if !addressed {
            return self
                .start_if_asked(caller, message, task_id, context_id)
                .await;
        }
        if !activated {
            return Err(not_activated());
        }
        let asked = asked(message, sessions).map_err(BackendError::InvalidParams)?;
        let task = match (task_id, round, kind) {
            (_, _, Some(ContextKind::TooDeep)) => {
                return Err(BackendError::InvalidParams(
                    "a session cannot hold a session".into(),
                ));
            }
            (Some(task), Some(round), _) => {
                if asked != Asked::Nothing {
                    return Err(BackendError::InvalidParams(
                        "a start or a close names no taskId".into(),
                    ));
                }
                let mut inbound = (self.inbound)(message).map_err(BackendError::InvalidParams)?;
                self.named_tier(caller, message, &mut inbound, false)?;
                let located = (round.run, round.view, round.context);
                let context = context_id.map(str::to_owned);
                self.follow_up(caller, located, inbound, task, context)
                    .await?
            }
            (Some(task), None, _) => return Err(BackendError::TaskNotFound(task.to_owned())),
            (None, _, Some(ContextKind::Child(parent, session))) => {
                let inbound = match asked {
                    Asked::Start(_) => {
                        return Err(BackendError::InvalidParams(
                            "a start goes to the parent context".into(),
                        ));
                    }
                    Asked::Close { .. } => None,
                    Asked::Nothing => {
                        let mut inbound =
                            (self.inbound)(message).map_err(BackendError::InvalidParams)?;
                        self.named_tier(caller, message, &mut inbound, true)?;
                        Some(inbound)
                    }
                };
                let close = match asked {
                    Asked::Close { cancel } => Some(cancel),
                    _ => None,
                };
                self.message_to_session(caller, message, inbound, &parent, session, close)
                    .await?
            }
            (None, _, _) => return Ok(None),
        };
        Ok(Some(self.with_sessions(caller, task).await?))
    }

    /// A `start` in a message to a root context, when the request activated the extension and the
    /// message carries one. `None` for any other message.
    async fn start_if_asked(
        &self,
        caller: &Caller,
        message: &Message,
        task_id: Option<&str>,
        context_id: Option<&str>,
    ) -> Result<Option<Task>, BackendError> {
        if !caller.has_extension(CHILD_SESSIONS_EXTENSION) {
            return Ok(None);
        }
        match asked(message, self.sessions()?).map_err(BackendError::InvalidParams)? {
            Asked::Nothing => Ok(None),
            Asked::Close { .. } => Err(BackendError::InvalidParams(
                "a close goes to the child context of a session".into(),
            )),
            Asked::Start(start) => {
                if task_id.is_some() {
                    return Err(BackendError::InvalidParams(
                        "a start names no taskId".into(),
                    ));
                }
                let mut inbound = (self.inbound)(message).map_err(BackendError::InvalidParams)?;
                self.named_tier(caller, message, &mut inbound, true)?;
                let task = self
                    .start_session(
                        caller,
                        message,
                        inbound,
                        start,
                        context_id.map(str::to_owned),
                    )
                    .await?;
                Ok(Some(self.with_sessions(caller, task).await?))
            }
        }
    }

    /// The access tier of a message to a child context. A request that activated `access-tier/v1`
    /// names one, as for any message; with `always` a message that names none still gets the
    /// agent's default, so a round a person starts never runs under a tier nobody chose.
    fn named_tier(
        &self,
        caller: &Caller,
        message: &Message,
        inbound: &mut Inbound,
        always: bool,
    ) -> Result<(), BackendError> {
        let Some(tiers) = &self.access_tiers else {
            return Ok(());
        };
        let tier = if caller.has_extension(adam_a2a::ACCESS_TIER_EXTENSION) {
            crate::tier::requested(message, tiers).map_err(BackendError::InvalidParams)?
        } else if always {
            tiers.default_tier()
        } else {
            return Ok(());
        };
        crate::tier::put(inbound, tier).map_err(BackendError::InvalidParams)
    }

    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    /// A message to the parent context `context` that starts a session.
    pub(crate) async fn start_session(
        &self,
        caller: &Caller,
        message: &Message,
        mut inbound: Inbound,
        start: Start,
        context: Option<String>,
    ) -> Result<Task, BackendError> {
        let sessions = self.sessions()?;
        let helper = sessions
            .by_name(&start.agent)
            .ok_or_else(|| BackendError::InvalidParams("no such helper".into()))?;
        let context = context.unwrap_or_else(a2a::new_context_id);
        let session = if message.message_id.is_empty() {
            RunId::new()
        } else {
            task_id_for(
                &helper.agent,
                &caller.subject,
                Some(&context),
                &message.message_id,
            )
        };
        let parent = parent_conversation(&caller.subject, &context);
        let conversation = session_conversation_id(&parent, session);
        // A repeat of the request finds the round it started.
        if let Some(existing) = self.session_of(caller, &context, session).await? {
            return self.answer_round(caller, &existing.latest_run).await;
        }
        // Every open session counts, not the first page's: the listing puts the open ones first,
        // so reading until a closed one shows (or `maxOpen` is reached) sees them all. Checked
        // before the start, so two concurrent starts may both pass and exceed `maxOpen` by the
        // number racing, as the model's start tool does.
        let max_open = sessions.max_open as usize;
        let is_open = |s: &SessionSummary| !s.closed || s.state == SessionState::Working;
        let max_working = sessions.max_working as usize;
        let enough = |all: &[SessionSummary]| {
            all.iter().filter(|s| is_open(s)).count() >= max_open
                || all.iter().any(|s| s.state == SessionState::Closed)
                || working_in(all) >= max_working
        };
        let listed = self
            .list_pages(caller, &context, enough)
            .await
            .map_err(listing_error)?;
        if listed.iter().filter(|s| is_open(s)).count() >= max_open {
            return Err(BackendError::UnsupportedOperation(format!(
                "this context already has {max_open} sessions open; close one first"
            )));
        }
        if working_in(&listed) >= max_working {
            return Err(BackendError::UnsupportedOperation(working_words(
                max_working,
            )));
        }
        let header = SessionHeader {
            id: session,
            agent: helper.agent.clone(),
            description: start.description,
            keep_open: start.keep_open,
            closed: false,
            closing: false,
            round: 1,
            // A person's session is its own parent: no run opened it, so each round's model calls
            // name the round's own task, which the orchestrator binds as any task it creates
            // (ADR 0067).
            root: None,
        };
        put_header(&mut inbound, &header).map_err(BackendError::InvalidParams)?;
        self.runtime
            .start_child_continuing_within(
                session,
                session,
                &helper.agent,
                inbound,
                &conversation,
                None,
                helper.within,
            )
            .await
            .map_err(map_err)?;
        self.answer_round(caller, &session).await
    }

    /// A message to child context `K` of session `session` in parent context `context`, no `taskId`.
    pub(crate) async fn message_to_session(
        &self,
        caller: &Caller,
        message: &Message,
        inbound: Option<Inbound>,
        context: &str,
        session: RunId,
        close: Option<bool>,
    ) -> Result<Task, BackendError> {
        let sessions = self.sessions()?;
        let child = session_conversation_id(context, session);
        let mut cancel = close.unwrap_or(false);
        for _ in 0..ATTEMPTS {
            let Some(view) = self.session_of(caller, context, session).await? else {
                return Err(BackendError::InvalidParams(format!(
                    "no session in context {child}"
                )));
            };
            let next = round_id(&view, caller, &child, &message.message_id, close.is_some());
            // The round this message started on an earlier try.
            if view.latest_run == next && view.round > 1 {
                return self.answer_round(caller, &view.latest_run).await;
            }
            let working = view.state == SessionState::Working;
            let helper = sessions.by_registered(&view.agent);
            let within = helper.and_then(|h| h.within);
            if close.is_some() {
                if view.closed && !working && !needs_cleanup(&view, helper) {
                    return self.answer_round(caller, &view.latest_run).await;
                }
                if working {
                    if cancel {
                        self.runtime
                            .cancel(view.latest_run, "the session was closed with cancel")
                            .await
                            .map_err(map_err)?;
                        cancel = false;
                        continue;
                    }
                    if view.closed {
                        return self.answer_round(caller, &view.latest_run).await;
                    }
                    let ask = Inbound::new(SESSION_CLOSE_KIND, json!({}))
                        .with_id(message.message_id.clone());
                    match self.runtime.deliver(view.latest_run, ask).await {
                        Ok(()) => return self.answer_round(caller, &view.latest_run).await,
                        Err(RuntimeError::Finished { .. } | RuntimeError::NotFound(_)) => continue,
                        Err(e) => return Err(map_err(e)),
                    }
                }
                let mut header = next_header(&view, session);
                header.closed = true;
                header.closing = true;
                let input = Inbound::new("message", closing_round_payload(&header))
                    .with_id(message.message_id.clone());
                match self.next_round(&view, next, input, within).await? {
                    Next::Started => return self.answer_round(caller, &next).await,
                    Next::Busy => continue,
                }
            }
            if view.closed {
                let why = if view.idle_expired {
                    " (it sat idle for longer than this deployment keeps a session open)"
                } else {
                    ""
                };
                return Err(BackendError::UnsupportedOperation(format!(
                    "session {session} is closed and takes no more messages{why}"
                )));
            }
            let mut inbound = inbound
                .clone()
                .ok_or_else(|| BackendError::InvalidParams("the message has no text".into()))?;
            if working {
                match self.runtime.deliver(view.latest_run, inbound).await {
                    Ok(()) => return self.answer_round(caller, &view.latest_run).await,
                    Err(RuntimeError::Finished { .. } | RuntimeError::NotFound(_)) => continue,
                    Err(e) => return Err(map_err(e)),
                }
            }
            if view.countable_rounds() >= sessions.max_rounds {
                return Err(BackendError::UnsupportedOperation(format!(
                    "session {session} has had {} rounds, the most it may have",
                    sessions.max_rounds
                )));
            }
            // A message that starts a round makes another work, like a start does.
            let max_working = sessions.max_working as usize;
            let working_now = |all: &[SessionSummary]| {
                all.iter().any(|s| s.state != SessionState::Working)
                    || working_in(all) >= max_working
            };
            let listed = self
                .list_pages(caller, context, working_now)
                .await
                .map_err(listing_error)?;
            if working_in(&listed) >= max_working {
                return Err(BackendError::UnsupportedOperation(working_words(
                    max_working,
                )));
            }
            put_header(&mut inbound, &next_header(&view, session))
                .map_err(BackendError::InvalidParams)?;
            match self.next_round(&view, next, inbound, within).await? {
                Next::Started => return self.answer_round(caller, &next).await,
                Next::Busy => {}
            }
        }
        Err(BackendError::unavailable(
            "the session kept changing while the message was sent",
        ))
    }

    /// After a cancel of round `run` of a person's session: the session is read, and
    /// [`start_cleanup`](Self::start_cleanup) runs for it when it needs it.
    pub(crate) async fn clean_up_after_cancel(
        &self,
        caller: &Caller,
        round: &Round,
    ) -> Result<(), BackendError> {
        self.session_of(caller, &round.parent_context, round.session)
            .await
            .map(drop)
    }

    /// Start the round that cancels the remote task a remote helper's session left running: a
    /// closing round continuing the latest one, which a worker steps (`carry_round` puts the task
    /// in `prior_open_task`, `cancel_prior` sends `CancelTask`). The process that serves the front
    /// holds no endpoint and no credential, so only a worker can send it. The session closes with
    /// it when it was closed already (not kept open, or expired idle) and otherwise stays open: the
    /// round only cancels. Its id derives from the latest run, so a repeat finds it.
    async fn start_cleanup(
        &self,
        caller: &Caller,
        context: &str,
        view: &SessionView,
        within: Option<Duration>,
    ) -> Result<(), BackendError> {
        let mut header = next_header(view, view.id);
        header.closed = view.closed;
        header.closing = true;
        let message_id = format!("cleanup-after-{}", view.latest_run);
        let child = session_conversation_id(context, view.id);
        let next = task_id_for(&view.agent, &caller.subject, Some(&child), &message_id);
        let input = Inbound::new("message", closing_round_payload(&header)).with_id(message_id);
        // Busy: another request already started a round after it; that one cleans up as well.
        self.next_round(view, next, input, within).await?;
        Ok(())
    }

    /// Start round `id` of `view`'s session, continuing its latest round. The round is its own
    /// parent: nothing but its own cancel ends it.
    async fn next_round(
        &self,
        view: &SessionView,
        id: RunId,
        input: Inbound,
        within: Option<Duration>,
    ) -> Result<Next, BackendError> {
        match self
            .runtime
            .start_child_continuing_within(
                id,
                id,
                &view.agent,
                input,
                &view.conversation_id,
                Some(view.latest_run),
                within,
            )
            .await
        {
            Ok(_) => Ok(Next::Started),
            Err(RuntimeError::ConversationBusy { .. }) => Ok(Next::Busy),
            Err(e) => Err(map_err(e)),
        }
    }

    /// The round `run` of a session of the caller as the answer to a request.
    pub(crate) async fn answer_round(
        &self,
        caller: &Caller,
        run: &RunId,
    ) -> Result<Task, BackendError> {
        let round = self
            .round_of(caller, &run.to_string())
            .await?
            .ok_or_else(|| BackendError::TaskNotFound(run.to_string()))?;
        let session = self
            .session_of(caller, &round.parent_context, round.session)
            .await?
            .ok_or_else(|| BackendError::TaskNotFound(run.to_string()))?;
        Ok(self.round_task(caller, &session, &round).await)
    }

    /// A listed record as a round task: the session's words on it, no tier (a listing never says
    /// one).
    pub(crate) async fn listed_round(
        &self,
        session: &SessionView,
        context: &str,
        parent_context: &str,
        rec: &RunRecord,
        view: RunView,
    ) -> Task {
        let round = Round {
            run: rec.id,
            context: context.to_owned(),
            parent_context: parent_context.to_owned(),
            session: session.id,
            by_person: rec.parent_id == Some(rec.id),
            view,
        };
        let mut task = self.task_of(&round.view, context).await;
        task.metadata.get_or_insert_with(HashMap::new).insert(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            self.round_object(session, &round),
        );
        task
    }
}

/// The rounds that work: a round that only closes a session does not count.
fn working_in(all: &[SessionSummary]) -> usize {
    all.iter().filter(|s| s.does_work()).count()
}

/// A listing that read too many rounds is a refusal to start another, in words; any other error
/// is the usual mapping.
fn listing_error(e: RuntimeError) -> BackendError {
    match e {
        RuntimeError::TooManyRounds { .. } => BackendError::UnsupportedOperation(
            "this context has too many rounds to take another session or round".into(),
        ),
        other => map_err(other),
    }
}

fn working_words(max_working: usize) -> String {
    format!("{max_working} rounds are already working in this context; wait for one to finish")
}

fn not_activated() -> BackendError {
    BackendError::InvalidParams(format!(
        "a child context or one of its rounds is reached with {CHILD_SESSIONS_EXTENSION} activated"
    ))
}

enum Next {
    Started,
    Busy,
}

/// Whether a remote task of `view`'s session may still be running with nobody to cancel it: the
/// helper works on another agent's task, and the latest round was cancelled or the session
/// expired idle, and no closing round has been started since. A closing round sent
/// `CancelTask` for what the round before left (`carry_round`).
fn needs_cleanup(view: &SessionView, helper: Option<&ChildSessionAgent>) -> bool {
    helper.is_some_and(|h| h.remote)
        && view.state != SessionState::Working
        && (view.outcome == Some(RoundOutcome::Cancelled) || view.idle_expired)
        && view.header.as_ref().is_some_and(|h| !h.closing)
}

/// The id of the round a message to `view`'s session would start: derived from the message, so a
/// repeat finds it; a message with no id starts a round that cannot be recognised.
fn round_id(
    view: &SessionView,
    caller: &Caller,
    child_context: &str,
    message_id: &str,
    _closing: bool,
) -> RunId {
    if message_id.is_empty() {
        return RunId::new();
    }
    task_id_for(
        &view.agent,
        &caller.subject,
        Some(child_context),
        message_id,
    )
}

/// The header of the next round of `view`'s session.
fn next_header(view: &SessionView, session: RunId) -> SessionHeader {
    let mut header = view.header.clone().unwrap_or_else(|| SessionHeader {
        id: session,
        agent: view.agent.clone(),
        description: String::new(),
        keep_open: false,
        closed: false,
        closing: false,
        round: 0,
        root: None,
    });
    header.round = view.round + 1;
    // The round before may have been one that only cleaned up after a cancel.
    header.closing = false;
    header
}

fn header_of_state(state: &Value) -> Option<SessionHeader> {
    serde_json::from_value(state.get(SESSION_STATE_KEY)?.clone()).ok()
}

/// Put `header` into the payload of `inbound`, under the key the helper's agent reads it from.
fn put_header(inbound: &mut Inbound, header: &SessionHeader) -> Result<(), String> {
    if let Value::String(text) = &inbound.payload {
        inbound.payload = json!({ "text": text });
    }
    let payload: &mut Map<String, Value> = inbound
        .payload
        .as_object_mut()
        .ok_or("the message cannot carry a session header: its payload is not text")?;
    payload.insert(
        SESSION_STATE_KEY.to_owned(),
        serde_json::to_value(header).map_err(|e| e.to_string())?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use a2a::{Part, Role};
    use proptest::prelude::*;

    use super::*;

    fn sessions() -> ChildSessions {
        ChildSessions::new(
            [
                ChildSessionAgent {
                    name: "planner".into(),
                    agent: "root/planner".into(),
                    description: "Plans".into(),
                    within: None,
                    remote: false,
                },
                ChildSessionAgent {
                    name: "researcher".into(),
                    agent: "root/researcher".into(),
                    description: "Looks things up".into(),
                    within: Some(Duration::from_secs(60)),
                    remote: false,
                },
            ],
            16,
        )
        .unwrap()
    }

    fn message(entry: Option<Value>) -> Message {
        let mut m = Message::new(Role::User, vec![Part::text("hi")]);
        m.metadata = entry.map(|e| HashMap::from([(CHILD_SESSIONS_EXTENSION.to_owned(), e)]));
        m
    }

    #[test]
    fn the_roster_is_checked_and_makes_the_card_entry() {
        let one = |name: &str, agent: &str| ChildSessionAgent {
            name: name.into(),
            agent: agent.into(),
            description: String::new(),
            within: None,
            remote: false,
        };
        assert!(ChildSessions::new([], 4).is_err());
        assert!(ChildSessions::new([one("a", "x/a")], 0).is_err());
        assert!(ChildSessions::new([one(" ", "x/a")], 4).is_err());
        assert!(ChildSessions::new([one("a", "x/a"), one("a", "x/b")], 4).is_err());
        assert!(ChildSessions::new([one("a", "x/a"), one("b", "x/a")], 4).is_err());
        let card = sessions().card_extension().unwrap();
        assert_eq!(card.uri, CHILD_SESSIONS_EXTENSION);
        assert_eq!(card.params["maxOpen"], 16);
        assert_eq!(card.params["agents"][1]["name"], "researcher");
        assert_eq!(sessions().with_max_rounds(0).max_rounds, 2);
    }

    #[test]
    fn a_closing_round_is_not_counted_as_working() {
        let mut closing = summary(SessionState::Working, true, true, 1);
        closing.closing = true;
        let working = summary(SessionState::Working, false, false, 1);
        let idle = summary(SessionState::Idle, false, false, 1);
        assert_eq!(working_in(&[closing, working, idle]), 1);
    }

    #[test]
    fn the_default_max_working_is_the_tools_default_capped_at_max_open() {
        let one = || vec![ChildSessionAgent::new("a", "x/a", "A.")];
        let working = |open| ChildSessions::new(one(), open).unwrap().max_working;
        assert_eq!(working(16), 4, "the tools' default, not max_open");
        assert_eq!(working(100), 4);
        assert_eq!(working(3), 3, "capped at max_open");
        assert_eq!(working(1), 1);
    }

    #[test]
    fn a_child_context_is_reserved_and_never_nests() {
        let parent = "ctx/with%odd:chars";
        let session = RunId::new();
        let child = session_conversation_id(parent, session);
        assert_eq!(kind_of(&child), ContextKind::Child(parent.into(), session));
        assert_eq!(kind_of(parent), ContextKind::Root);
        assert_eq!(kind_of("a/s/not-a-uuid"), ContextKind::Root);
        let deeper = session_conversation_id(&child, RunId::new());
        assert_eq!(kind_of(&deeper), ContextKind::TooDeep);
    }

    proptest! {
        #[test]
        fn the_owner_of_a_session_conversation_is_found_and_no_other(
            subject in ".{0,12}", other in ".{0,12}", context in "[^/]{0,20}", bytes in any::<[u8; 16]>()
        ) {
            let session = RunId(Uuid::from_bytes(bytes));
            let stored = session_conversation_id(&parent_conversation(&subject, &context), session);
            let owned = owned_session_conversation(&subject, &stored);
            prop_assert_eq!(
                owned,
                Some((context.clone(), session, session_conversation_id(&context, session)))
            );
            if other != subject {
                prop_assert_eq!(owned_session_conversation(&other, &stored), None);
            }
        }
    }

    #[test]
    fn a_start_is_read_strictly() {
        let s = sessions();
        let ok = asked(
            &message(Some(json!({"start": {"agent": "planner", "description": " Plan it ", "keepOpen": true, "x": 1}}))),
            &s,
        )
        .unwrap();
        assert_eq!(
            ok,
            Asked::Start(Start {
                agent: "planner".into(),
                description: "Plan it".into(),
                keep_open: true
            })
        );
        let default_close = asked(
            &message(Some(
                json!({"start": {"agent": "planner", "description": "d"}}),
            )),
            &s,
        )
        .unwrap();
        assert!(matches!(
            default_close,
            Asked::Start(Start {
                keep_open: false,
                ..
            })
        ));
        let long = "x".repeat(81);
        for entry in [
            json!({"start": 3}),
            json!({"start": {}}),
            json!({"start": {"agent": "nobody", "description": "d"}}),
            json!({"start": {"agent": "planner"}}),
            json!({"start": {"agent": "planner", "description": "  "}}),
            json!({"start": {"agent": "planner", "description": "a\nb"}}),
            json!({"start": {"agent": "planner", "description": "a\u{7}b"}}),
            json!({"start": {"agent": "planner", "description": "a\tb"}}),
            json!({"start": {"agent": "planner", "description": long}}),
            json!({"start": {"agent": "planner", "description": "d", "keepOpen": "yes"}}),
            json!({"start": {"agent": "planner", "description": "d"}, "close": {}}),
            json!("start"),
        ] {
            assert!(asked(&message(Some(entry.clone())), &s).is_err(), "{entry}");
        }
        assert_eq!(asked(&message(None), &s), Ok(Asked::Nothing));
        assert_eq!(
            asked(&message(Some(json!({"other": 1}))), &s),
            Ok(Asked::Nothing)
        );
        let exact = "x".repeat(80);
        assert!(
            asked(
                &message(Some(
                    json!({"start": {"agent": "planner", "description": exact}})
                )),
                &s
            )
            .is_ok()
        );
    }

    #[test]
    fn a_close_is_read_strictly() {
        let s = sessions();
        assert_eq!(
            asked(&message(Some(json!({"close": {}}))), &s),
            Ok(Asked::Close { cancel: false })
        );
        assert_eq!(
            asked(&message(Some(json!({"close": {"cancel": true}}))), &s),
            Ok(Asked::Close { cancel: true })
        );
        for entry in [json!({"close": true}), json!({"close": {"cancel": "yes"}})] {
            assert!(asked(&message(Some(entry)), &s).is_err());
        }
    }

    fn summary(state: SessionState, closed: bool, closing: bool, age: i64) -> SessionSummary {
        SessionSummary {
            id: RunId::new(),
            agent: "root/planner".into(),
            conversation_id: String::new(),
            latest_run: RunId::new(),
            round: 1,
            state,
            outcome: None,
            updated_at: adam_core::store::now() - jiff::SignedDuration::from_secs(age),
            description: String::new(),
            keep_open: true,
            closed,
            closing,
            idle_expired: false,
            started_by: None,
            report: adam_runtime::ReportRound {
                run: RunId::new(),
                ended: state != SessionState::Working,
                cancelled: false,
                started_by: None,
                updated_at: adam_core::store::now(),
            },
        }
    }

    #[test]
    fn a_working_session_with_a_close_pending_is_never_cut_with_the_closed_ones() {
        // Runtime order: working, then idle, then closed newest first.
        let pending = summary(SessionState::Working, true, false, 1);
        let mut listed = vec![
            pending.clone(),
            summary(SessionState::Idle, false, false, 2),
        ];
        listed.extend((0..20).map(|n| summary(SessionState::Closed, true, false, 10 + n)));
        let kept = select_listed(listed);
        assert_eq!(kept.len(), 2 + CLOSED_LISTED);
        assert!(kept.iter().any(|s| s.id == pending.id));
        assert_eq!(wire_state(kept[0].state, kept[0].closes()), "working");
        assert!(kept[2..].iter().all(|s| s.state == SessionState::Closed));
    }

    #[test]
    fn the_wire_state_of_a_session() {
        use SessionState::{Closed, Idle, Working};
        for (state, closes, wanted) in [
            (Working, false, "working"),
            (Working, true, "closed"),
            (Idle, false, "idle"),
            (Idle, true, "closed"),
            (Closed, false, "closed"),
            (Closed, true, "closed"),
        ] {
            assert_eq!(wire_state(state, closes), wanted, "{state:?} {closes}");
        }
    }

    #[test]
    fn the_header_goes_into_the_payload_whatever_it_is() {
        let header = SessionHeader {
            id: RunId::new(),
            agent: "a".into(),
            description: "d".into(),
            keep_open: true,
            closed: false,
            closing: false,
            round: 1,
            root: None,
        };
        let mut text = Inbound::new("message", json!("hi"));
        put_header(&mut text, &header).unwrap();
        assert_eq!(text.payload["text"], "hi");
        assert_eq!(header_of_state(&text.payload), Some(header.clone()));
        let mut bad = Inbound::new("message", json!(3));
        assert!(put_header(&mut bad, &header).is_err());
    }
}
