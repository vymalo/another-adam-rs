//! The engine's own types: what a host says, and what it is told about a run.

use std::fmt;

use a2a::{Task, TaskState};
use adam_core::RunId;
use uuid::Uuid;

use crate::EngineError;

/// A file the agent handed over in a run (a file artifact), without its bytes: what
/// [`Engine::files`](crate::Engine::files) lists. Read the bytes with
/// [`Engine::file`](crate::Engine::file) and its `sha256`, which is also what an event that
/// withholds the file says ([`WithheldFile`](crate::WithheldFile)).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunFile {
    /// The artifact's name.
    pub name: String,
    /// Its media type, if the agent gave one.
    pub mime_type: Option<String>,
    /// The name the file is saved under.
    pub filename: String,
    /// How many bytes it has.
    pub len: u64,
    /// The SHA-256 of its bytes, as 64 lower-case hexadecimal digits.
    pub sha256: String,
}

/// A conversation: the runs that one person's thread is made of, each continuing the one before.
/// Opaque; the engine makes it on the first [`send`](crate::Engine::send) of a thread, and the host
/// keeps it to send the next message.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConversationId(String);

impl ConversationId {
    /// A conversation id the host kept, as [`as_str`](Self::as_str) gave it.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text, to keep.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the person says: a message to send, steer with or answer with.
///
/// `id` makes a send idempotent: the same `Say` sent again to the same conversation (a retry after
/// a crash) reaches the run its first attempt started, and the agent reads it once.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Say {
    /// The message's id, unique per message.
    pub id: String,
    /// The text.
    pub text: String,
    /// The model alias this message asks its run to be answered by, when it is not the agent's own
    /// ([`with_model`](Self::with_model)). `None`: the agent's own model, or the one an earlier
    /// message of the run named.
    pub model: Option<String>,
}

impl Say {
    /// `text`, with a new id.
    pub fn new(text: impl Into<String>) -> Self {
        Self::with_id(Uuid::new_v4().to_string(), text)
    }

    /// `text` with the id the host gives it (the id of the message in the host's own store).
    pub fn with_id(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            model: None,
        }
    }

    /// Ask for the run to go on with the model `alias`, with its profile's output cap and reasoning
    /// level (ADR 0064, amended 2026-11-23). The alias must be the root agent's own or a model it
    /// can be bound to (a profile in [`EngineBuilder::models`](crate::EngineBuilder::models) that
    /// resolves for it): the engine refuses any other with [`EngineError::Invalid`], and writes
    /// nothing. The choice holds for the run until a later message names another. A run that
    /// switched to its fallback because the endpoint locked its model stays on the fallback: a
    /// later message that names a model is refused with [`EngineError::Refused`] (never ignored),
    /// and the conversation's next run starts on the agent's own model again.
    #[must_use]
    pub fn with_model(mut self, alias: impl Into<String>) -> Self {
        self.model = Some(alias.into());
        self
    }

    pub(crate) fn check(&self) -> Result<(), EngineError> {
        if self.id.trim().is_empty() {
            return Err(EngineError::Invalid("a message needs an id".to_owned()));
        }
        if self.text.trim().is_empty() {
            return Err(EngineError::Invalid("the message has no text".to_owned()));
        }
        if self
            .model
            .as_deref()
            .is_some_and(|alias| alias.trim().is_empty())
        {
            return Err(EngineError::Invalid(
                "the message names an empty model alias".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Where a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunState {
    /// Started; no worker has taken it yet.
    Submitted,
    /// A worker steps it, or it waits on a timer or a child run.
    Working,
    /// It asked the person a question ([`RunInfo::question`]) and waits for
    /// [`answer`](crate::Engine::answer).
    Waiting,
    /// It finished with its answer ([`RunInfo::output`]).
    Done,
    /// It failed ([`RunInfo::error`]).
    Failed,
    /// It was cancelled ([`RunInfo::error`] is the reason).
    Cancelled,
}

impl RunState {
    /// Whether the run has ended: nothing more happens to it.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }

    fn of(state: &TaskState) -> Self {
        match state {
            TaskState::Unspecified | TaskState::Submitted => Self::Submitted,
            TaskState::Working => Self::Working,
            TaskState::InputRequired | TaskState::AuthRequired => Self::Waiting,
            TaskState::Completed => Self::Done,
            TaskState::Failed | TaskState::Rejected => Self::Failed,
            TaskState::Canceled => Self::Cancelled,
        }
    }
}

/// A run as the store has it now.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunInfo {
    /// The run.
    pub run: RunId,
    /// Its conversation.
    pub conversation: ConversationId,
    /// Its state.
    pub state: RunState,
    /// The question, while it is [`Waiting`](RunState::Waiting).
    pub question: Option<String>,
    /// The answer, once [`Done`](RunState::Done).
    pub output: Option<String>,
    /// Why it failed, or the reason it was cancelled.
    pub error: Option<String>,
}

impl RunInfo {
    pub(crate) fn of(task: &Task) -> Result<Self, EngineError> {
        let run = Uuid::parse_str(&task.id)
            .map(RunId)
            .map_err(|_| EngineError::internal("the runtime answered a run id that is no UUID"))?;
        let state = RunState::of(&task.status.state);
        let said = task
            .status
            .message
            .as_ref()
            .and_then(|m| m.text())
            .map(str::to_owned);
        let (question, output, error) = match state {
            RunState::Waiting => (said, None, None),
            RunState::Done => (None, said, None),
            RunState::Failed | RunState::Cancelled => (None, None, said),
            RunState::Submitted | RunState::Working => (None, None, None),
        };
        Ok(Self {
            run,
            conversation: ConversationId(task.context_id.clone()),
            state,
            question,
            output,
            error,
        })
    }
}

/// What [`send`](crate::Engine::send) did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Turn {
    /// The conversation: the one given, or the new one a first message started.
    pub conversation: ConversationId,
    /// The run that has the message.
    pub run: RunId,
    /// What the message did.
    pub delivered: Delivered,
    /// The run as it was when the message was accepted.
    pub info: RunInfo,
}

/// What a [`send`](crate::Engine::send) did with its message. A closed enum: a new way of
/// delivering a message breaks a `match` on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Delivered {
    /// It started the first run of a new conversation.
    Started,
    /// It started a new run that continues the conversation's latest, which had ended.
    Continued,
    /// It went to the conversation's open run, which was working when the engine looked just
    /// before (or a run started meanwhile): the run reads it at its next step.
    Steered,
    /// It went to the conversation's open run, which was waiting for an answer when the engine
    /// looked just before: it is that answer.
    Answered,
    /// The same `Say` (its id) had already started this run: nothing was written. A `Say` that
    /// went to an open run is delivered again instead, and the agent reads it once.
    Repeated,
}

/// Which runs [`list`](crate::Engine::list) answers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListQuery {
    /// Only the runs of this conversation.
    pub conversation: Option<ConversationId>,
    /// At most this many runs, 1 to [`MAX_PAGE_SIZE`]; `0` is [`DEFAULT_PAGE_SIZE`].
    pub page_size: usize,
    /// Continue after the page that gave this [`Page::next_page_token`].
    pub page_token: Option<String>,
}

/// The page size of a [`ListQuery`] that names none.
pub const DEFAULT_PAGE_SIZE: usize = adam_a2a::DEFAULT_PAGE_SIZE;

/// The largest page; a larger [`ListQuery::page_size`] is cut to it.
pub const MAX_PAGE_SIZE: usize = adam_a2a::MAX_PAGE_SIZE;

impl ListQuery {
    /// The first page of every run of the engine, [`DEFAULT_PAGE_SIZE`] at a time.
    pub fn new() -> Self {
        Self::default()
    }

    /// Only the runs of `conversation`.
    #[must_use]
    pub fn conversation(mut self, conversation: ConversationId) -> Self {
        self.conversation = Some(conversation);
        self
    }

    /// At most `size` runs a page.
    #[must_use]
    pub fn page_size(mut self, size: usize) -> Self {
        self.page_size = size;
        self
    }

    /// The page after the one that gave `token`.
    #[must_use]
    pub fn after(mut self, token: impl Into<String>) -> Self {
        self.page_token = Some(token.into());
        self
    }

    pub(crate) fn resolved_page_size(&self) -> usize {
        match self.page_size {
            0 => DEFAULT_PAGE_SIZE,
            n => n.min(MAX_PAGE_SIZE),
        }
    }
}

/// Which firings [`firings`](crate::Engine::firings) answers: the runs the agent's `schedules/`
/// fired (ADR 0060), which are no caller's and so not in [`list`](crate::Engine::list).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct FiringQuery {
    /// Only the firings of the schedule of this name (the file's stem in `schedules/`).
    pub schedule: Option<String>,
    /// At most this many firings, 1 to [`MAX_PAGE_SIZE`]; `0` is [`DEFAULT_PAGE_SIZE`].
    pub page_size: usize,
    /// Continue after the page that gave this [`Page::next_page_token`].
    pub page_token: Option<String>,
}

impl FiringQuery {
    /// The first page of every firing of the agent, [`DEFAULT_PAGE_SIZE`] at a time.
    pub fn new() -> Self {
        Self::default()
    }

    /// Only the firings of the schedule `name`.
    #[must_use]
    pub fn schedule(mut self, name: impl Into<String>) -> Self {
        self.schedule = Some(name.into());
        self
    }

    /// At most `size` firings a page.
    #[must_use]
    pub fn page_size(mut self, size: usize) -> Self {
        self.page_size = size;
        self
    }

    /// The page after the one that gave `token`.
    #[must_use]
    pub fn after(mut self, token: impl Into<String>) -> Self {
        self.page_token = Some(token.into());
        self
    }

    pub(crate) fn resolved_page_size(&self) -> usize {
        match self.page_size {
            0 => DEFAULT_PAGE_SIZE,
            n => n.min(MAX_PAGE_SIZE),
        }
    }
}

/// A run one of the agent's schedules fired: the run as [`get`](crate::Engine::get) answers it,
/// and what fired it. Its [`RunInfo::conversation`] is the firing's own, not one to
/// [`send`](crate::Engine::send) to: a message to it is [`EngineError::NotFound`](crate::EngineError::NotFound).
/// Not `adam_schedule::Firing`, the scheduler's record of a due time it fired.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Firing {
    /// The run.
    pub info: RunInfo,
    /// The schedule and the due time it fired for.
    pub trigger: adam_schedule::Trigger,
}

/// What starts a child session ([`start_session`](crate::Engine::start_session)): which helper,
/// what the session is for, and whether it stays open after its first round.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StartSession {
    /// The helper, by the name [`helpers`](crate::Engine::helpers) lists.
    pub helper: String,
    /// One line, at most 80 characters: what the session is for, as the host shows it.
    pub description: String,
    /// Whether the session stays open, idle, after a round ends; otherwise it closes when its
    /// first round ends.
    pub keep_open: bool,
}

impl StartSession {
    /// A session of `helper` for `description`, closed when its first round ends.
    pub fn new(helper: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            helper: helper.into(),
            description: description.into(),
            keep_open: false,
        }
    }

    /// Keep the session open after each round, until it is closed.
    #[must_use]
    pub fn keep_open(mut self) -> Self {
        self.keep_open = true;
        self
    }
}

/// A helper a person may start a session with: a local subagent of the root.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionHelper {
    /// Its name, as [`StartSession::helper`] names it.
    pub name: String,
    /// What it does, from its file.
    pub description: String,
}

/// Where a child session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionState {
    /// A round works.
    Working,
    /// Open, no round works: the next message starts one.
    Idle,
    /// Closed: it takes no more messages.
    Closed,
}

/// A child session of a conversation ([`sessions`](crate::Engine::sessions)).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionInfo {
    /// The session's id within its conversation.
    pub session: String,
    /// Its own conversation: where [`send`](crate::Engine::send) talks to it, and what
    /// [`close_session`](crate::Engine::close_session) and [`list`](crate::Engine::list) take.
    pub conversation: ConversationId,
    /// The helper that works it.
    pub helper: String,
    /// What it is for.
    pub description: String,
    /// Whether it stays open between rounds.
    pub keep_open: bool,
    /// Where it is.
    pub state: SessionState,
    /// How many rounds it has had.
    pub round: u32,
    /// Its latest round.
    pub latest: Option<RunId>,
}

impl SessionInfo {
    /// A session as the parent task's `child-sessions/v1` object lists it.
    pub(crate) fn of(entry: &serde_json::Value) -> Result<Self, EngineError> {
        let bad = |what: &str| EngineError::internal(format!("a session listed without {what}"));
        let text = |key: &str| {
            entry
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| bad(key))
        };
        let state = match entry.get("state").and_then(serde_json::Value::as_str) {
            Some("working") => SessionState::Working,
            Some("idle") => SessionState::Idle,
            Some("closed") => SessionState::Closed,
            _ => return Err(bad("a state")),
        };
        let round = entry.get("round");
        Ok(Self {
            session: text("session")?,
            conversation: ConversationId(text("contextId")?),
            helper: text("agent")?,
            description: text("description")?,
            keep_open: entry
                .get("keepOpen")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            state,
            round: round
                .and_then(|r| r.get("number"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            latest: round
                .and_then(|r| r.get("taskId"))
                .and_then(serde_json::Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
                .map(RunId),
        })
    }
}

/// One page of a listing, the most recently updated first.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Page<T> {
    /// The items of the page.
    pub items: Vec<T>,
    /// The token of the next page ([`ListQuery::after`]); `None` on the last one. It is bound to
    /// the query's filters: another filter refuses it.
    pub next_page_token: Option<String>,
    /// How many items match in all.
    pub total: usize,
}
