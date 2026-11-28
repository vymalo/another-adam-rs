//! The five tools that run a subagent in the background as a **session** ([ADR 0045]): start one,
//! send it a message, read its report, close it, list the open ones.
//!
//! A session is a conversation of child runs ("rounds") of one subagent, kept in the parent's own
//! conversation. The tools hold nothing: every call reads the sessions from the store, so a replay,
//! a restart and another worker see what the model saw. They are given to an agent that has local
//! subagents ([`SessionTools::tools`]), beside the one tool per subagent, which stays for a
//! synchronous ask. The agent that has them also waits, at the end of a turn, for the sessions it
//! started in the task ([`LlmAgentBuilder::session_agents`](crate::LlmAgentBuilder::session_agents)).
//!
//! The schemas are fixed for a run: the only enum is the subagent names, sorted, so the specs are
//! byte-stable. A session id is the run id of the session's first round.
//!
//! [ADR 0045]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md

use crate::access::ToolAccess;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use adam_core::time::Timestamp;

use adam_core::RunId;
use adam_model::ToolSpec;
use adam_runtime::{
    Inbound, MAX_SESSION_PAGE, RuntimeError, SESSION_CLOSE_KIND, SessionBounds, SessionConfig,
    SessionHeader, SessionLimits, SessionMode, SessionState, SessionSummary, SessionView,
};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::render_output;
use crate::conversation::MESSAGE_KIND;
use crate::paging::{
    PAGE_LINES, PageError, clip_item, end_line, escape_text, more_line, page_lines, window,
    window_args,
};
use crate::tool::{DynTool, RemotePoll, StepStyle, Tool, ToolCtx, ToolError, ToolOutput};
use adam_runtime::{StepIcon, StepKind};

/// The name of the tool that starts a session.
pub const SUBAGENT_START: &str = "subagent_start";
/// The name of the tool that sends a message to a session.
pub const SUBAGENT_SEND: &str = "subagent_send";
/// The name of the tool that reads the report of a session.
pub const SUBAGENT_READ: &str = "subagent_read";
/// The name of the tool that closes a session.
pub const SUBAGENT_CLOSE: &str = "subagent_close";
/// The name of the tool that lists the sessions.
pub const SUBAGENT_LIST: &str = "subagent_list";

/// The five tool names, in the order the model is offered them.
pub const SESSION_TOOLS: [&str; 5] = [
    SUBAGENT_START,
    SUBAGENT_SEND,
    SUBAGENT_READ,
    SUBAGENT_CLOSE,
    SUBAGENT_LIST,
];

/// The most characters of a session's description.
pub const MAX_DESCRIPTION_CHARS: usize = 80;

/// The most characters of a `prompt` or a `message`.
pub const MAX_TEXT_CHARS: usize = 20_000;

/// The default of [`SessionLimits::max_working`]: the most rounds of one conversation that may
/// work at once. A start, or a send that would start a round, past it is refused.
pub const MAX_WORKING_ROUNDS: usize = 4;

/// The default of [`SessionLimits::max_open`]: the most sessions of one conversation that may be
/// open at once (not closed). A start past it is refused.
pub const MAX_OPEN_SESSIONS: usize = 16;

/// The default of [`SessionLimits::max_rounds`]: a send past it is refused (a close never is, so a
/// session can have one more round, the closing one).
pub const MAX_SESSION_ROUNDS: u32 = 20;

/// The time limit each session tool declares ([`Tool::time_limit`]). None of them does long work:
/// a read that waits parks the run and returns at once.
pub const SESSION_TOOL_TIME_LIMIT: Duration = Duration::from_secs(60);

/// How long a subagent's description may be in the text of `subagent_start`.
const ROSTER_DESCRIPTION_CHARS: usize = 200;

/// One subagent a session may be started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAgent {
    /// The subagent's name, as the model writes it in `agent`.
    pub name: String,
    /// The name it is registered under on the runtime (`coder/reviewer`).
    pub agent: String,
    /// What the subagent is for: shown to the model in the description of `subagent_start`.
    pub description: String,
    /// The time limit of each of its rounds (`None`: none).
    pub within: Option<Duration>,
    /// Whether it runs on another system (an A2A agent). A session on it is started like any
    /// other: its rounds are runs of the agent registered as [`agent`](Self::agent), which talks
    /// to the remote. What differs is the answer to a message sent while a round works: such an
    /// agent says in its round state (`queues_messages`) whether the remote takes the message at
    /// once (`steered`) or only after its current task (`queued`).
    pub remote: bool,
    /// The most a session on it may ask of each option ([ADR 0054] decision 2): what the
    /// subagent's folder declares, narrowed by what the deployment allows, computed once by the
    /// assembly at startup. [`SessionBounds::admit`] holds a `subagent_start`'s `config` to them;
    /// a request above one is refused by name, never clamped.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    pub bounds: SessionBounds,
    /// What the subagent's file says of its sessions (`session:`, [ADR 0087] decision 4): the
    /// `mode` and `wait` of a start that names none, and the widest `mode` a start may name.
    /// [`SessionDefaults::default`] is a file without the block: `one_shot` up to `open`, no wait.
    ///
    /// [ADR 0087]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0087-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
    pub session: SessionDefaults,
}

/// A subagent file's `session:` block ([ADR 0087] decision 4), as `subagent_start` applies it.
///
/// * `mode` is the **default and the ceiling** at once: a start that names no mode gets it, and a
///   start may name it or a narrower one (`one_shot` is narrower than `sealed`, which is narrower
///   than `open`, as [`SessionMode`] orders them), never a wider one: that is refused naming both,
///   as ADR 0054 refuses an option above its bound. `None` is a file that says nothing: a start
///   gets `one_shot` and may ask up to `open`.
/// * `wait` is a default only: a start that says `wait` is never refused for it.
///
/// [ADR 0087]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0087-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionDefaults {
    /// The file's `session.mode`.
    pub mode: Option<SessionMode>,
    /// The file's `session.wait`.
    pub wait: Option<bool>,
}

impl SessionDefaults {
    /// The widest mode a start may ask: the file's `mode`, else [`SessionMode::Open`].
    #[must_use]
    pub fn ceiling(self) -> SessionMode {
        self.mode.unwrap_or(SessionMode::Open)
    }

    /// The mode of a start that names none: the file's `mode`, else [`SessionMode::OneShot`].
    #[must_use]
    pub fn default_mode(self) -> SessionMode {
        self.mode.unwrap_or_default()
    }

    /// Whether a start that does not say waits for the first report: the file's `wait`, else
    /// `false`.
    #[must_use]
    pub fn default_wait(self) -> bool {
        self.wait.unwrap_or(false)
    }

    /// What the roster of `subagent_start` says of these defaults, when the file sets one that is
    /// not the tool's own: `mode sealed by default, at most sealed`, `mode open by default`,
    /// `waits by default`. `None` when the file says nothing the tool's description does not.
    fn roster_note(self) -> Option<String> {
        let mode = self.mode.map(|mode| match mode {
            SessionMode::Open => "mode open by default".to_owned(),
            SessionMode::OneShot | SessionMode::Sealed => {
                format!("mode {mode} by default, at most {mode}")
            }
        });
        let wait = (self.wait == Some(true)).then(|| "waits by default".to_owned());
        let words: Vec<String> = mode.into_iter().chain(wait).collect();
        (!words.is_empty()).then(|| words.join("; "))
    }

    /// The mode a start of `agent` runs in: `asked`, held to the [`ceiling`](Self::ceiling), or the
    /// [`default_mode`](Self::default_mode) when the start names none.
    ///
    /// # Errors
    ///
    /// The sentence that names the mode asked, the ceiling, the subagent and the modes it takes
    /// (`` `mode` is open; at most sealed for reviewer, as its file's `session.mode` says: one of
    /// one_shot, sealed ``), when `asked` is wider than the ceiling.
    pub(crate) fn admit(
        self,
        agent: &str,
        asked: Option<AskedMode>,
    ) -> Result<SessionMode, String> {
        let Some(asked) = asked else {
            return Ok(self.default_mode());
        };
        let ceiling = self.ceiling();
        if asked.mode <= ceiling {
            return Ok(asked.mode);
        }
        let allowed = SessionMode::ALL
            .into_iter()
            .filter(|mode| *mode <= ceiling)
            .map(SessionMode::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!(
            "{asked}; at most {ceiling} for {agent}, as its file's `session.mode` says: one of \
             {allowed}"
        ))
    }
}

/// The mode a `subagent_start` call names, and whether it named it as the `keep_open` of before,
/// so that a refusal repeats what the call wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AskedMode {
    /// The mode asked.
    pub(crate) mode: SessionMode,
    /// `true` when the call wrote `keep_open` and no `mode` (read for one release).
    pub(crate) by_keep_open: bool,
}

impl std::fmt::Display for AskedMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.by_keep_open {
            write!(
                f,
                "`keep_open` {} is mode {}",
                self.mode.keeps_open(),
                self.mode
            )
        } else {
            write!(f, "`mode` is {}", self.mode)
        }
    }
}

/// The five session tools, for the subagents of one agent.
#[derive(Debug, Clone)]
pub struct SessionTools {
    roster: Arc<Roster>,
}

#[derive(Debug)]
struct Roster {
    /// Sorted by name.
    agents: Vec<SessionAgent>,
    limits: SessionLimits,
}

impl Roster {
    fn names(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.name.as_str()).collect()
    }

    fn registered(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.agent.as_str()).collect()
    }

    fn by_registered(&self, agent: &str) -> Option<&SessionAgent> {
        self.agents.iter().find(|a| a.agent == agent)
    }

    /// The name the model knows the subagent by.
    fn name_of<'a>(&'a self, agent: &'a str) -> &'a str {
        self.by_registered(agent).map_or(agent, |a| a.name.as_str())
    }
}

impl SessionTools {
    /// The tools for `agents`, whose names must be distinct. They are sorted by name, so the specs
    /// do not depend on the order the folder listed them in.
    pub fn new(agents: impl IntoIterator<Item = SessionAgent>) -> Self {
        let mut agents: Vec<SessionAgent> = agents.into_iter().collect();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            roster: Arc::new(Roster {
                agents,
                limits: SessionLimits::default(),
            }),
        }
    }

    /// These tools under `limits` instead of the defaults (16 open, 4 working, 20 rounds). The
    /// specs do not mention the limits, so they are the same bytes under any.
    #[must_use]
    pub fn with_limits(self, limits: SessionLimits) -> Self {
        Self {
            roster: Arc::new(Roster {
                agents: self.roster.agents.clone(),
                limits,
            }),
        }
    }

    /// The five tools, in [`SESSION_TOOLS`] order.
    pub fn tools(&self) -> Vec<DynTool> {
        let roster = &self.roster;
        vec![
            Arc::new(Start {
                roster: roster.clone(),
            }),
            Arc::new(Send {
                roster: roster.clone(),
            }),
            Arc::new(Read {
                roster: roster.clone(),
            }),
            Arc::new(Close {
                roster: roster.clone(),
            }),
            Arc::new(List {
                roster: roster.clone(),
            }),
        ]
    }

    /// The registration names of the subagents a session can be started on:
    /// what [`LlmAgentBuilder::session_agents`](crate::LlmAgentBuilder::session_agents) takes.
    pub fn session_agents(&self) -> Vec<String> {
        self.roster
            .registered()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// The bounds of each subagent, by the name the model writes in `agent` ([ADR 0054] decision
    /// 2), in the roster's order: what [`SessionBounds::admit`] holds a start's `config` to. The
    /// assembly computes them once, at startup, from the subagent's folder and the deployment, and
    /// hands them here; a reader of the card lists them beside each helper (decision 8).
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    pub fn bounds(&self) -> Vec<(String, SessionBounds)> {
        self.roster
            .agents
            .iter()
            .map(|agent| (agent.name.clone(), agent.bounds.clone()))
            .collect()
    }

    /// The mode a `subagent_start` naming none gives each subagent ([`SessionDefaults::default_mode`],
    /// [ADR 0087] slice C3), by the name the model writes in `agent`, in the roster's order: what
    /// [`LlmAgentBuilder::session_default_modes`](crate::LlmAgentBuilder::session_default_modes)
    /// takes, so the `session.started` of a start that waits names the mode it started in.
    ///
    /// [ADR 0087]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0087-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
    pub fn default_modes(&self) -> Vec<(String, SessionMode)> {
        self.roster
            .agents
            .iter()
            .map(|agent| (agent.name.clone(), agent.session.default_mode()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The options of `config` as the model writes them (snake_case, like the other arguments), and
/// the camelCase the wire and the header use for the same names.
const CONFIG_OPTIONS: [(&str, &str); 7] = [
    ("max_turns", "maxTurns"),
    ("max_tool_calls", "maxToolCalls"),
    ("max_output_tokens", "maxOutputTokens"),
    ("context_window", "contextWindow"),
    ("max_parallel_tool_calls", "maxParallelToolCalls"),
    ("time_limit", "timeLimit"),
    ("max_rounds", "maxRounds"),
];

/// A session's config as a list and a read say it: `max_turns 20, time_limit 10`, the options that
/// are set, in the model's own names. `None` when none is, so nothing is shown (ADR 0054
/// decision 3).
fn config_words(config: &SessionConfig) -> Option<String> {
    let set = serde_json::to_value(config).ok()?;
    let parts: Vec<String> = CONFIG_OPTIONS
        .iter()
        .filter_map(|(own, wire)| {
            set.get(*wire)
                .and_then(Value::as_u64)
                .map(|n| format!("{own} {n}"))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The options a remote subagent refuses: every one but `time_limit` and `max_rounds` (ADR 0054
/// decision 4).
const REMOTE_REFUSED: [&str; 5] = [
    "max_turns",
    "max_tool_calls",
    "max_output_tokens",
    "context_window",
    "max_parallel_tool_calls",
];

/// The [`SessionConfig`] a `subagent_start` call asked for, or the sentence that says what is wrong
/// with it. **An unknown option is refused** (ADR 0054 decision 1): an ignored narrowing fails open.
fn config_of(value: &Value) -> Result<SessionConfig, String> {
    let Some(object) = value.as_object() else {
        return Err("`config` must be an object of options".to_owned());
    };
    let mut wire = serde_json::Map::new();
    for (key, value) in object {
        let Some((_, name)) = CONFIG_OPTIONS.iter().find(|(own, _)| own == key) else {
            let options: Vec<&str> = CONFIG_OPTIONS.iter().map(|(own, _)| *own).collect();
            return Err(format!(
                "config.{} is not an option; the options are: {}",
                escape_text(key.chars().take(80).collect::<String>().as_bytes()),
                options.join(", ")
            ));
        };
        // Every option is a whole number of 32 bits. Say so by name here, so the refusal is ours:
        // serde's names no option and echoes what the model wrote, without a bound. A `0` passes
        // to `admit`, which refuses it with the words of each option.
        match value {
            Value::Null => continue,
            Value::Number(n) if n.as_u64().is_some_and(|n| u32::try_from(n).is_ok()) => {}
            _ => {
                return Err(format!(
                    "config.{key} must be a positive integer (a whole number, at most {})",
                    u32::MAX
                ));
            }
        }
        wire.insert((*name).to_owned(), value.clone());
    }
    SessionConfig::from_value(Value::Object(wire)).map_err(|why| why.message().to_owned())
}

/// The time limit of a round of `agent`: the session's own `time_limit` (minutes, already held to
/// the bounds), or the folder's when it asks none; the smaller when both are set, so a session's
/// limit never lifts the folder's.
fn round_within(agent: &SessionAgent, config: Option<&SessionConfig>) -> Option<Duration> {
    let own = config
        .and_then(|c| c.time_limit)
        .filter(|minutes| *minutes > 0)
        .map(|minutes| Duration::from_secs(u64::from(minutes) * 60));
    match (own, agent.within) {
        (Some(own), Some(folder)) => Some(own.min(folder)),
        (own, folder) => own.or(folder),
    }
}

fn refuse(text: impl Into<String>) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput::error(text))
}

fn runtime_error(e: &RuntimeError) -> ToolError {
    ToolError::from_classified(e)
}

/// The text of an argument that must be a non-blank string.
fn text_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// A boolean argument that defaults to `default` when absent; `Err` for another type.
fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("`{key}` must be true or false")),
    }
}

/// The [`SessionMode`] a `subagent_start` call asks for ([ADR 0087] slice C2): its `mode`, else
/// the `keep_open` of before (read for one release: `true` is `open`, `false` is `one_shot`), else
/// `None`, which the subagent's [`SessionDefaults`] fill (slice C3). Both given and disagreeing is
/// the sentence that names both, as is a mode that is none of the three.
///
/// [ADR 0087]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0087-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
pub(crate) fn mode_arg(args: &Value) -> Result<Option<AskedMode>, String> {
    let asked = match args.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(word)) => Some(SessionMode::parse(word).ok_or_else(|| {
            format!(
                "`mode` is one of: {}; not `{}`",
                mode_words(),
                escape_text(word.chars().take(80).collect::<String>().as_bytes())
            )
        })?),
        Some(_) => return Err(format!("`mode` must be one of: {}", mode_words())),
    };
    let kept = match args.get("keep_open") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(keep)) => Some(*keep),
        Some(_) => return Err("`keep_open` must be true or false".to_owned()),
    };
    match (asked, kept) {
        (Some(mode), Some(keep)) if mode != SessionMode::of_keep_open(keep) => Err(format!(
            "`keep_open` ({keep}) and `mode` ({mode}) disagree: `keep_open` {keep} is mode {}; \
             give `mode` alone",
            SessionMode::of_keep_open(keep)
        )),
        (Some(mode), _) => Ok(Some(AskedMode {
            mode,
            by_keep_open: false,
        })),
        (None, kept) => Ok(kept.map(|keep| AskedMode {
            mode: SessionMode::of_keep_open(keep),
            by_keep_open: true,
        })),
    }
}

/// The modes, as a refusal lists them: `one_shot, sealed, open`.
fn mode_words() -> String {
    SessionMode::ALL.map(SessionMode::as_str).join(", ")
}

/// The integer a model wrote for `key`, if it wrote one.
fn int_arg(args: &Value, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be an integer")),
    }
}

/// The session a call names, or the words that say what to do instead.
fn session_arg(args: &Value, tool: &str) -> Result<RunId, String> {
    let Some(text) = text_arg(args, "session") else {
        return Err(format!(
            "`{tool}` needs `session`, the id `{SUBAGENT_START}` returned"
        ));
    };
    text.trim().parse::<uuid::Uuid>().map(RunId).map_err(|_| {
        format!(
            "`{}` is not a session id: use the `session` that `{SUBAGENT_START}` returned \
                 (`{SUBAGENT_LIST}` shows them)",
            escape_text(text.chars().take(80).collect::<String>().as_bytes())
        )
    })
}

fn no_such_session(session: RunId) -> String {
    format!(
        "no session `{session}` in this conversation; `{SUBAGENT_LIST}` shows the sessions it has"
    )
}

fn closed_words(session: RunId, idle_expired: bool) -> String {
    let why = if idle_expired {
        " (it sat idle for longer than this deployment keeps a session open)"
    } else {
        ""
    };
    format!(
        "session `{session}` is closed and takes no more messages{why} (a closed session is never \
         reopened). Start a new one with `{SUBAGENT_START}`, and put what it needs in `prompt`: it \
         does not see the closed session's conversation."
    )
}

/// The state a model is told: what the session is doing and whether it will take a message.
fn state_word(view: &SessionView) -> &'static str {
    match (view.state, view.closed) {
        (SessionState::Working, true) => "working (closes when its round ends)",
        (SessionState::Working, false) if view.waiting => "blocked on the person",
        (SessionState::Working, false) => "working",
        (SessionState::Idle, _) => "idle",
        (SessionState::Closed, _) => "closed",
    }
}

fn working_words(limits: SessionLimits) -> String {
    format!(
        "{} subagent rounds are already working: read a report with `{SUBAGENT_READ}` \
         (wait: true) before starting another",
        limits.max_working()
    )
}

fn description_of(view: &SessionView) -> String {
    view.header
        .as_ref()
        .map(|h| h.description.clone())
        .unwrap_or_default()
}

/// A model's description, cut to one line of printable characters.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The id of the call the loop makes for the report of `round`: `report_` and its id's 32 hex
/// digits, which every provider accepts and which no model call of ours ever has.
pub(crate) fn report_call_id(round: RunId) -> String {
    format!("report_{}", round.0.simple())
}

/// What decides whether a report is due, besides the session ([`is_due`]).
pub(crate) struct DueFacts<'a> {
    /// The run the model belongs to.
    pub me: RunId,
    /// The rounds already reported ([`Conversation::reported_rounds`](crate::Conversation)).
    pub reported: &'a [RunId],
    /// The ids of the report calls the history already holds: a round the cap gave up is still
    /// not due while its call is there, or the history would hold the id twice.
    pub in_history: &'a HashSet<String>,
    /// When the chain of tasks began: a person's round that ended before it is not due.
    pub since: Option<Timestamp>,
}

/// Whether the report of `session` is **due** to the model of the run `facts.me` ([ADR 0045],
/// slice 3). The report is of the latest round that worked (a closing round has none, so one that
/// follows does not hide it): it has ended, it was not given before, and either this run started
/// it or a person did after the chain of tasks began. A round an earlier task's run started is not
/// due: that run waited for it or cancelled it. A person's round that ends **in the same instant as
/// the chain starts is told** (`ended >= chain_started`): telling an old round once to a fresh task,
/// which has no history, is harmless, and losing a new one is not. A round that a later round of the same session
/// replaced before the parent looked is not reported on its own, and a round cancelled by the close
/// that follows it is not either.
///
/// [ADR 0045]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md
pub(crate) fn is_due(session: &SessionSummary, facts: &DueFacts) -> bool {
    let round = &session.report;
    // A session of nothing but a closing round has nothing to report.
    if session.closing && round.run == session.latest_run {
        return false;
    }
    // A round that was cancelled and then closed was cancelled by whoever closed the session, who
    // knows it.
    if round.cancelled && session.closing && round.run != session.latest_run {
        return false;
    }
    round.ended
        && !facts.reported.contains(&round.run)
        && !facts.in_history.contains(&report_call_id(round.run))
        && (round.started_by == Some(facts.me)
            || (round.started_outside_a_run() && facts.since.is_none_or(|t| round.updated_at >= t)))
}

fn header_json(header: &SessionHeader) -> Value {
    serde_json::to_value(header).unwrap_or(Value::Null)
}

/// The message that starts or continues a round: the child message of this run with the session's
/// header in it.
fn round_message(ctx: &ToolCtx, text: &str, header: &SessionHeader) -> Inbound {
    let mut message = ctx.child_message(text);
    if let Some(payload) = message.payload.as_object_mut() {
        payload.insert(
            adam_runtime::SESSION_STATE_KEY.to_owned(),
            header_json(header),
        );
    }
    message
}

/// The report of the session's latest round as a result: the header line, the report's lines as a
/// page (numbered, so no line of the report can pass for the final line) and the final line.
fn read_output(
    roster: &Roster,
    view: &SessionView,
    offset: Option<i64>,
    limit: Option<i64>,
) -> ToolOutput {
    let (offset, limit) = match window_args(offset, limit, PAGE_LINES) {
        Ok(window) => window,
        Err(why) => return ToolOutput::error(why),
    };
    // A person's round says so: the model reads a report of something it did not ask for. The
    // person's words are not in it.
    let by = if view.started_outside_a_run() {
        ", started by the person"
    } else {
        ""
    };
    let head = format!(
        "session {} ({}, \"{}\"): round {}{by}, {}",
        view.id,
        roster.name_of(&view.agent),
        escape_text(description_of(view).as_bytes()),
        view.round,
        state_word(view)
    );
    // What the session runs with, when it was started with a config: a line of its own after the
    // head, so a session with none reads as it always did.
    let head = match view
        .header
        .as_ref()
        .and_then(|h| h.config.as_ref())
        .and_then(config_words)
    {
        Some(words) => format!("{head}\nconfig: {words}"),
        None => head,
    };
    // A round parked with no timer is **blocked** on the person (ADR 0045 decision 9): it asked a
    // question, or a model refused it until woken (ADR 0067), and only an answer moves it. The
    // parent is told why; nothing is a report yet, so nothing is counted as read.
    let blocked = view.state == SessionState::Working && view.waiting;
    if view.state == SessionState::Working && !blocked {
        return ToolOutput::text(format!(
            "{head}\nreport: none yet; the round is still working. Call {SUBAGENT_READ} with \
             wait: true to wait for it; do not poll.\n{}",
            end_line(0, "lines", &[])
        ));
    }
    let (status, body) = if blocked {
        (
            "blocked",
            view.waiting_reason
                .as_deref()
                .unwrap_or("the round waits for the person's answer")
                .to_owned(),
        )
    } else {
        match (&view.latest.status, &view.latest.error) {
            (adam_core::RunStatus::Done, _) => ("done", render_output(view.latest.output.as_ref())),
            (_, error) => (
                "failed",
                error.as_deref().unwrap_or("no reason given").to_owned(),
            ),
        }
    };
    // A report ends in a newline, so the page does not say the text has none.
    let body = if body.ends_with('\n') {
        body
    } else {
        format!("{body}\n")
    };
    let page = match page_lines(body.as_bytes(), offset, limit, true) {
        Ok(page) => page,
        Err(PageError::PastEnd { offset, total }) => {
            return ToolOutput::error(format!(
                "the report has {total} lines: offset {offset} is past the end"
            ));
        }
        Err(e) => return ToolOutput::error(e.to_string()),
    };
    let mut out = ToolOutput::text(format!("{head}\nreport: {status}\n{}", page.render()));
    out.is_error = false;
    if !blocked {
        out.reported = vec![view.report_run];
    }
    out
}

fn is_busy_or_finished(e: &RuntimeError) -> bool {
    e.is_busy() || matches!(e, RuntimeError::Finished { .. })
}

/// The header of round 1 of `session`, on `agent`, started by the call of `ctx`: one-shot, not
/// closed, no config. `subagent_start` and a one-shot session ([`start_one_shot`]) build theirs
/// from it, so the two cannot disagree on what a session started by a run says.
fn first_header(ctx: &ToolCtx, session: RunId, agent: &str, description: String) -> SessionHeader {
    SessionHeader {
        id: session,
        agent: agent.to_owned(),
        description,
        mode: SessionMode::OneShot,
        closed: false,
        closing: false,
        round: 1,
        // The run this call's model calls name, which the orchestrator bound: the round's
        // calls name it too (ADR 0067).
        root: Some(ctx.model_run_id()),
        // The workspace the round works in: the one of the run that opens the session, so a
        // helper a task starts reads the repository of that task (ADR 0045 slice 7).
        workspace: Some(ctx.workspace_run_id()),
        // A session is one the person sees when their client shows child sessions: only then
        // may a round of it ask them (ADR 0045 decision 9).
        visible: ctx.context(crate::CHILD_SESSIONS_CONTEXT_KEY) == Some(&Value::Bool(true)),
        config: None,
    }
}

/// What a one-shot session is for, as a list shows it: the first line of `message`, cut to
/// [`MAX_DESCRIPTION_CHARS`] (the call has no `description` of its own).
fn one_shot_description(message: &str) -> String {
    let first = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let line = one_line(first);
    if line.chars().count() <= MAX_DESCRIPTION_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(MAX_DESCRIPTION_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Start `agent` as a **one-shot session** of the run of `ctx` ([ADR 0087] slice C1): round 1 of a
/// session whose id is [`ToolCtx::child_run_id`], with `message` as its first user message, a
/// session's conversation id (`<parent escaped>/s/<session>`), and a header that says it is
/// [`SessionMode::OneShot`] and **closed from the start**: it closes when its round ends, and
/// nobody (the parent, the person) can send to it meanwhile, not even a steer. It is
/// `subagent_start` with `mode: one_shot` and `wait: true`, except that the caller waits on it
/// ([`ToolError::AwaitRun`] with the returned id) and no session limit applies, as none applied to
/// a subagent's own tool before.
///
/// Idempotent, keyed by the call: a replay finds the round it started. A child this call started
/// as a plain child run before C1 (no conversation) is found too: when the runtime refuses the round
/// as not a session's ([`RuntimeError::InvalidSession`]: a run with the id exists that is not this
/// round, or the conversation id would pass
/// [`MAX_SESSION_CONVERSATION_BYTES`](adam_runtime::MAX_SESSION_CONVERSATION_BYTES), as a deeply
/// nested child's can), the child is started as a plain child run, which finds the existing one.
///
/// [ADR 0087]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0087-every-child-is-a-session-and-the-one-shot-subagent-tool-goes.md
pub(crate) async fn start_one_shot(
    ctx: &ToolCtx,
    agent: &str,
    message: &str,
    within: Option<Duration>,
) -> Result<RunId, ToolError> {
    let starter = ctx.child_starter()?;
    let session = ctx.child_run_id();
    let mut header = first_header(ctx, session, agent, one_shot_description(message));
    header.closed = true;
    let input = round_message(ctx, message, &header);
    match starter
        .start_round(session, session, agent, input, None, within)
        .await
    {
        Ok(_) => Ok(session),
        Err(RuntimeError::InvalidSession { reason }) => {
            tracing::debug!(%session, %reason, "a one-shot session could not be started; starting a plain child run");
            starter
                .start_within(session, agent, ctx.child_message(message), within)
                .await
                .map_err(|e| runtime_error(&e))?;
            Ok(session)
        }
        Err(e) => Err(runtime_error(&e)),
    }
}

// ---------------------------------------------------------------------------
// subagent_start
// ---------------------------------------------------------------------------

struct Start {
    roster: Arc<Roster>,
}

#[async_trait]
impl Tool for Start {
    fn spec(&self) -> ToolSpec {
        let mut description = format!(
            "Start a subagent in the background and get its session id at once; you keep working \
             while it works. Use it instead of the subagent's own tool when you have other work to \
             do meanwhile or want to ask it again. The subagent does not see this conversation: put everything it \
             needs in `prompt`. Read its report with {SUBAGENT_READ} (pass wait: true to wait for \
             it; never poll), and send it more with {SUBAGENT_SEND}. `mode` says who may send the \
             session more after its first round: nobody with one_shot (the default: it closes when \
             its round has ended), only you with sealed, you and the person with open; keep it open \
             only when another message will follow, and close what you kept open with \
             {SUBAGENT_CLOSE}. With wait true the call waits for the first report and returns it, \
             as {SUBAGENT_READ} with wait: true does. Subagents:"
        );
        for agent in &self.roster.agents {
            let text = one_line(&agent.description);
            let text: String = text.chars().take(ROSTER_DESCRIPTION_CHARS).collect();
            description.push_str("\n- ");
            description.push_str(&agent.name);
            if !text.is_empty() {
                description.push_str(": ");
                description.push_str(&text);
            }
            // Only a file that sets `session:` adds words, so every other roster line, and the
            // prompt's stable prefix with it, is as it was before the block existed.
            if let Some(note) = agent.session.roster_note() {
                description.push_str(" (");
                description.push_str(&note);
                description.push(')');
            }
        }
        ToolSpec {
            name: SUBAGENT_START.to_owned(),
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": self.roster.names(),
                        "description": "Which subagent to start.",
                    },
                    "description": {
                        "type": "string",
                        "maxLength": MAX_DESCRIPTION_CHARS,
                        "description": "What the session is for, in a few words: it is how you \
                                        and the person find it again.",
                    },
                    "prompt": {
                        "type": "string",
                        "maxLength": MAX_TEXT_CHARS,
                        "description": "The task, with everything the subagent needs to do it.",
                    },
                    "mode": {
                        "type": "string",
                        "enum": SessionMode::ALL.map(SessionMode::as_str),
                        "default": SessionMode::OneShot.as_str(),
                        "description": "Who may send the session more after its first round: \
                                        nobody (one_shot: it closes), only you (sealed), or you \
                                        and the person (open).",
                    },
                    "wait": {
                        "type": "boolean",
                        "default": false,
                        "description": "Wait for the first report and return it as this call's \
                                        result, instead of the session id at once.",
                    },
                    "config": {
                        "type": "object",
                        "description": "Optional limits for this session, each at most what the \
                                        subagent's folder allows (a value above it is refused, \
                                        naming the option). An option you leave out is the \
                                        folder's own.",
                        "properties": {
                            "max_turns": { "type": "integer", "minimum": 1 },
                            "max_tool_calls": { "type": "integer", "minimum": 1 },
                            "max_output_tokens": { "type": "integer", "minimum": 1 },
                            "context_window": { "type": "integer", "minimum": 1 },
                            "max_parallel_tool_calls": { "type": "integer", "minimum": 1 },
                            "time_limit": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Minutes one round may take.",
                            },
                            "max_rounds": { "type": "integer", "minimum": 1 },
                        },
                        "additionalProperties": false,
                    },
                },
                "required": ["agent", "description", "prompt"],
                "additionalProperties": false,
            }),
        }
    }

    fn time_limit(&self) -> Option<Duration> {
        Some(SESSION_TOOL_TIME_LIMIT)
    }

    fn step_style(&self) -> StepStyle {
        StepStyle::new(StepKind::Subagent)
            .with_label("Start subagent")
            .with_icon(StepIcon::Agent)
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(name) = text_arg(&args, "agent") else {
            return refuse(format!(
                "`{SUBAGENT_START}` needs `agent`, one of: {}",
                self.roster.names().join(", ")
            ));
        };
        let Some(agent) = self.roster.agents.iter().find(|a| a.name == name) else {
            return refuse(format!(
                "no subagent `{}`; `agent` is one of: {}",
                escape_text(name.chars().take(80).collect::<String>().as_bytes()),
                self.roster.names().join(", ")
            ));
        };
        let description = match text_arg(&args, "description") {
            Some(text) => one_line(text),
            None => {
                return refuse(format!(
                    "`{SUBAGENT_START}` needs `description`, a few words on what the session is for"
                ));
            }
        };
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return refuse(format!(
                "`description` is {} characters; at most {MAX_DESCRIPTION_CHARS}: say it shorter \
                 and put the detail in `prompt`",
                description.chars().count()
            ));
        }
        let Some(prompt) = text_arg(&args, "prompt") else {
            return refuse(format!(
                "`{SUBAGENT_START}` needs `prompt`, a non-empty string with the whole task"
            ));
        };
        if prompt.chars().count() > MAX_TEXT_CHARS {
            return refuse(format!(
                "`prompt` is {} characters; at most {MAX_TEXT_CHARS}: put the long material in a file the subagent can read",
                prompt.chars().count()
            ));
        }
        // The subagent's file gives the defaults and the ceiling of `mode` (ADR 0087 decision 4):
        // a start may narrow it, never widen it, and nothing starts when it asks wider.
        let mode = match mode_arg(&args).and_then(|asked| agent.session.admit(&agent.name, asked)) {
            Ok(mode) => mode,
            Err(why) => return refuse(why),
        };
        let wait = match bool_arg(&args, "wait", agent.session.default_wait()) {
            Ok(wait) => wait,
            Err(why) => return refuse(why),
        };

        // The session's own settings, held to what the subagent's folder allows by the one function
        // the A2A front uses (ADR 0054 decision 2): a request above a bound starts nothing.
        let config = match args.get("config") {
            None | Some(Value::Null) => None,
            Some(value) => match config_of(value).and_then(|asked| {
                // A remote subagent's loop is another system's: it takes `time_limit` and
                // `max_rounds` and refuses the rest by name (ADR 0054 decision 4), never records
                // an option nothing would enforce.
                if agent.remote
                    && let Some(option) = REMOTE_REFUSED
                        .iter()
                        .find(|own| value.get(**own).is_some_and(|v| !v.is_null()))
                {
                    return Err(format!(
                        "config.{option} is not an option of a remote subagent; it takes only \
                         time_limit and max_rounds"
                    ));
                }
                agent
                    .bounds
                    .admit(asked)
                    .map_err(|why| why.message().to_owned())
            }) {
                Ok(config) if config.is_empty() => None,
                Ok(config) => Some(config),
                Err(why) => return refuse(why),
            },
        };
        let starter = ctx.child_starter()?;
        let session = ctx.child_run_id();
        let started = json!({
            "session": session,
            "agent": agent.name,
            "state": "working",
            "mode": mode.as_str(),
        });
        // A repeat of this call (a retry after a lost record) finds the round it started.
        if let Some(view) = starter
            .session(session)
            .await
            .map_err(|e| runtime_error(&e))?
        {
            if !wait {
                return Ok(ToolOutput::text(started.to_string()));
            }
            if view.state == SessionState::Working {
                return Err(ToolError::AwaitSession { session });
            }
            return Ok(read_output(&self.roster, &view, None, None));
        }
        let listed = match starter
            .sessions(&self.roster.registered(), 0, MAX_SESSION_PAGE)
            .await
        {
            Ok(listed) => listed,
            Err(RuntimeError::TooManyRounds { .. }) => {
                return refuse(
                    "this conversation has too many rounds to list: nothing more can be started \
                     in it; tell the person to continue in a new conversation",
                );
            }
            Err(e) => return Err(runtime_error(&e)),
        };
        // What costs something: sessions still open, and rounds that still work (a session asked
        // to close while its round works is closed and still working).
        let open = listed
            .sessions
            .iter()
            .filter(|s| !s.closed || s.state == SessionState::Working)
            .count();
        let working = listed.sessions.iter().filter(|s| s.does_work()).count();
        let limits = self.roster.limits;
        if working >= limits.max_working() as usize {
            return refuse(working_words(limits));
        }
        if open >= limits.max_open() as usize
            || (listed.total > MAX_SESSION_PAGE && open >= MAX_SESSION_PAGE)
        {
            return refuse(format!(
                "this conversation already has {} open sessions: close one with \
                 `{SUBAGENT_CLOSE}` (`{SUBAGENT_LIST}` shows them) before starting another",
                limits.max_open()
            ));
        }
        let mut header = first_header(ctx, session, &agent.agent, description);
        header.mode = mode;
        header.config = config;
        let input = round_message(ctx, prompt, &header);
        let within = round_within(agent, header.config.as_ref());
        starter
            .start_round(session, session, &agent.agent, input, None, within)
            .await
            .map_err(|e| runtime_error(&e))?;
        // A start that waits parks on its round as a read that waits does: the report is the
        // call's result, counted as read, so it is never told again as a report of the session.
        if wait {
            return Err(ToolError::AwaitSession { session });
        }
        Ok(ToolOutput::text(started.to_string()))
    }

    async fn poll_remote(&self, ctx: &ToolCtx, task: &str) -> Result<RemotePoll, ToolError> {
        poll_report(&self.roster, ctx, task).await
    }
}

// ---------------------------------------------------------------------------
// subagent_send
// ---------------------------------------------------------------------------

struct Send {
    roster: Arc<Roster>,
}

#[async_trait]
impl Tool for Send {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SUBAGENT_SEND.to_owned(),
            description: format!(
                "Send a message to a session. When its round has ended, the message starts the \
                 next round, which continues the session's conversation: the subagent remembers \
                 the rounds before. While a round is working, the message is steered into it. A \
                 closed session takes no message. Read the answer with {SUBAGENT_READ}."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "The session id.",
                    },
                    "message": {
                        "type": "string",
                        "maxLength": MAX_TEXT_CHARS,
                        "description": "What to tell the subagent.",
                    },
                },
                "required": ["session", "message"],
                "additionalProperties": false,
            }),
        }
    }

    fn time_limit(&self) -> Option<Duration> {
        Some(SESSION_TOOL_TIME_LIMIT)
    }

    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("Message subagent")
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let session = match session_arg(&args, SUBAGENT_SEND) {
            Ok(session) => session,
            Err(why) => return refuse(why),
        };
        let Some(message) = text_arg(&args, "message") else {
            return refuse(format!(
                "`{SUBAGENT_SEND}` needs `message`, a non-empty string"
            ));
        };
        if message.chars().count() > MAX_TEXT_CHARS {
            return refuse(format!(
                "`message` is {} characters; at most {MAX_TEXT_CHARS}",
                message.chars().count()
            ));
        }
        let starter = ctx.child_starter()?;
        let round_id = ctx.child_run_id();
        // A steer can find its round ended, and a new round can find another open: look again.
        for _ in 0..3 {
            let Some(view) = starter
                .session(session)
                .await
                .map_err(|e| runtime_error(&e))?
            else {
                return refuse(no_such_session(session));
            };
            // The round this call started on an earlier try.
            if view.latest_run == round_id {
                return Ok(sent(session, view.round, "new_round"));
            }
            // The message this call steered on an earlier try, even if the round has ended since:
            // the round remembers the ids of the messages it read.
            if let Ok(Some(state)) = starter.round_state(view.latest_run).await
                && state
                    .get("read_ids")
                    .and_then(Value::as_array)
                    .is_some_and(|ids| ids.iter().any(|id| id == ctx.call_id()))
            {
                return Ok(sent(session, view.round, delivery_of(&state)));
            }
            if view.closed {
                return refuse(closed_words(session, view.idle_expired));
            }
            let steered = || user_inbound(message, ctx.call_id());
            if view.state == SessionState::Working {
                match starter.steer(&view, steered()).await {
                    Ok(()) => {
                        let state = starter.round_state(view.latest_run).await;
                        let how = state
                            .ok()
                            .flatten()
                            .map_or("steered", |state| delivery_of(&state));
                        return Ok(sent(session, view.round, how));
                    }
                    Err(e) if is_busy_or_finished(&e) => continue,
                    Err(e) => return Err(runtime_error(&e)),
                }
            }
            let limits = self.roster.limits;
            // This round's config is the header's, clamped to the bounds of now (ADR 0054
            // decision 3), and it is the clamped copy the round limit reads: a session's own
            // `max_rounds` is held here, never above the deployment's.
            let agent = self.roster.by_registered(&view.agent);
            let config = view
                .header
                .as_ref()
                .and_then(|h| h.config.as_ref())
                .map(|asked| match agent {
                    Some(a) => a.bounds.clamp(asked),
                    // The agent left the roster: clamp to the loop's own defaults, which never
                    // raise what the session asked for beyond what any folder would allow.
                    None => SessionBounds::default().clamp(asked),
                });
            let max_rounds = config
                .as_ref()
                .and_then(|c| c.max_rounds)
                .map_or(limits.max_rounds(), |own| own.min(limits.max_rounds()));
            if view.countable_rounds() >= max_rounds {
                return refuse(format!(
                    "session `{session}` has had {} rounds, the most a session may have: close \
                     it and start a new one with `{SUBAGENT_START}`",
                    max_rounds
                ));
            }
            // A message that starts a round makes another work, like a start does.
            let listed = match starter
                .sessions(&self.roster.registered(), 0, MAX_SESSION_PAGE)
                .await
            {
                Ok(listed) => listed,
                Err(RuntimeError::TooManyRounds { .. }) => {
                    return refuse(
                        "this conversation has too many rounds to list: no round can be \
                         started in it; tell the person to continue in a new conversation",
                    );
                }
                Err(e) => return Err(runtime_error(&e)),
            };
            let working = listed.sessions.iter().filter(|s| s.does_work()).count();
            if working >= limits.max_working() as usize {
                return refuse(working_words(limits));
            }
            let mut header = view.header.clone().unwrap_or_else(|| SessionHeader {
                id: session,
                agent: view.agent.clone(),
                description: String::new(),
                mode: SessionMode::Open,
                closed: false,
                closing: false,
                round: 0,
                root: None,
                workspace: None,
                visible: false,
                config: None,
            });
            header.config = config;
            header.round = view.round + 1;
            // The round before may have been one that only cleaned up after a cancel.
            header.closing = false;
            // A round names the task that sent it, the one the orchestrator bound for it, not the
            // one that opened the session, whose lease may be over (ADR 0067, amended).
            header.root = Some(ctx.model_run_id());
            // A client that shows child sessions sees this session too, so a round of it may ask
            // the person (ADR 0045 decision 9). Once shown, a session stays shown: the header the
            // round before left says so.
            header.visible = header.visible
                || ctx.context(crate::CHILD_SESSIONS_CONTEXT_KEY) == Some(&Value::Bool(true));
            let within = agent.and_then(|a| round_within(a, header.config.as_ref()));
            let input = round_message(ctx, message, &header);
            match starter
                .start_round(
                    session,
                    round_id,
                    &view.agent,
                    input,
                    Some(view.latest_run),
                    within,
                )
                .await
            {
                Ok(_) => return Ok(sent(session, view.round + 1, "new_round")),
                Err(e) if is_busy_or_finished(&e) => {}
                Err(e) => return Err(runtime_error(&e)),
            }
        }
        Err(ToolError::Transient(format!(
            "session `{session}` kept changing while the message was sent"
        )))
    }
}

/// How a message that reached a working round was taken: `queued` when the round's agent says it
/// keeps messages until its remote can take them (`queues_messages` in its state), else `steered`.
fn delivery_of(round_state: &Value) -> &'static str {
    match round_state.get("queues_messages") {
        Some(Value::Bool(true)) => "queued",
        _ => "steered",
    }
}

fn user_inbound(text: &str, id: &str) -> Inbound {
    Inbound::new(MESSAGE_KIND, json!({ "text": text })).with_id(id)
}

fn sent(session: RunId, round: u32, how: &str) -> ToolOutput {
    ToolOutput::text(
        json!({
            "session": session,
            "round": round,
            "state": "working",
            "delivered": how,
        })
        .to_string(),
    )
}

// ---------------------------------------------------------------------------
// subagent_read
// ---------------------------------------------------------------------------

struct Read {
    roster: Arc<Roster>,
}

#[async_trait]
impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SUBAGENT_READ.to_owned(),
            description: format!(
                "Read the report of a session's latest round: its state, its round number and, \
                 once the round has ended, the subagent's answer (paged: `offset` and `limit` are \
                 lines). While the round works the result says so. To wait for it, call again with \
                 wait: true, which returns when the round has ended: never poll by calling again \
                 and again, which gets the same answer and is refused after two. A wait returns \
                 the first page. Closing is {SUBAGENT_CLOSE}."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "The session id.",
                    },
                    "wait": {
                        "type": "boolean",
                        "default": false,
                        "description": "Wait for a working round to end instead of answering at once.",
                    },
                    "offset": {
                        "type": "integer",
                        "description": "The report line to start at, from 1; negative counts from the end.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "The most report lines to return.",
                    },
                },
                "required": ["session"],
                "additionalProperties": false,
            }),
        }
    }

    fn paged(&self) -> bool {
        true
    }

    fn time_limit(&self) -> Option<Duration> {
        Some(SESSION_TOOL_TIME_LIMIT)
    }

    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("Read subagent report")
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let session = match session_arg(&args, SUBAGENT_READ) {
            Ok(session) => session,
            Err(why) => return refuse(why),
        };
        let wait = match bool_arg(&args, "wait", false) {
            Ok(wait) => wait,
            Err(why) => return refuse(why),
        };
        let (offset, limit) = match (int_arg(&args, "offset"), int_arg(&args, "limit")) {
            (Ok(offset), Ok(limit)) => (offset, limit),
            (Err(why), _) | (_, Err(why)) => return refuse(why),
        };
        let starter = ctx.child_starter()?;
        let Some(view) = starter
            .session(session)
            .await
            .map_err(|e| runtime_error(&e))?
        else {
            return refuse(no_such_session(session));
        };
        if wait && view.state == SessionState::Working {
            return Err(ToolError::AwaitSession { session });
        }
        Ok(read_output(&self.roster, &view, offset, limit))
    }

    async fn poll_remote(&self, ctx: &ToolCtx, task: &str) -> Result<RemotePoll, ToolError> {
        poll_report(&self.roster, ctx, task).await
    }
}

/// How the session a call waits on ([`ToolError::AwaitSession`], its id the `task`) stands: still
/// working, or the first page of its report, as `subagent_read` gives it. What a read and a start
/// that wait both answer.
async fn poll_report(roster: &Roster, ctx: &ToolCtx, task: &str) -> Result<RemotePoll, ToolError> {
    let session = task
        .parse::<uuid::Uuid>()
        .map(RunId)
        .map_err(|_| ToolError::Permanent(format!("`{task}` is not a session id")))?;
    let Some(view) = ctx
        .child_starter()?
        .session(session)
        .await
        .map_err(|e| runtime_error(&e))?
    else {
        return Err(ToolError::Permanent(no_such_session(session)));
    };
    if view.state == SessionState::Working {
        return Ok(RemotePoll::Working);
    }
    Ok(RemotePoll::Ready(read_output(roster, &view, None, None)))
}

// ---------------------------------------------------------------------------
// subagent_close
// ---------------------------------------------------------------------------

struct Close {
    roster: Arc<Roster>,
}

#[async_trait]
impl Tool for Close {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SUBAGENT_CLOSE.to_owned(),
            description: format!(
                "Close a session: it takes no more messages, and is never reopened. A round that \
                 is working is left to finish (read its report with {SUBAGENT_READ}), unless you \
                 pass cancel: true, which stops it. Closing a closed session does nothing."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "session": {
                        "type": "string",
                        "description": "The session id.",
                    },
                    "cancel": {
                        "type": "boolean",
                        "default": false,
                        "description": "Stop a working round instead of letting it finish.",
                    },
                },
                "required": ["session"],
                "additionalProperties": false,
            }),
        }
    }

    fn time_limit(&self) -> Option<Duration> {
        Some(SESSION_TOOL_TIME_LIMIT)
    }

    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("Close subagent")
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let session = match session_arg(&args, SUBAGENT_CLOSE) {
            Ok(session) => session,
            Err(why) => return refuse(why),
        };
        let cancel = match bool_arg(&args, "cancel", false) {
            Ok(cancel) => cancel,
            Err(why) => return refuse(why),
        };
        let starter = ctx.child_starter()?;
        let round_id = ctx.child_run_id();
        let mut note = None;
        for _ in 0..3 {
            let Some(mut view) = starter
                .session(session)
                .await
                .map_err(|e| runtime_error(&e))?
            else {
                return refuse(no_such_session(session));
            };
            if view.latest_run == round_id || (view.closed && view.state != SessionState::Working) {
                return Ok(closed(session, note));
            }
            if view.state == SessionState::Working {
                if cancel {
                    starter
                        .cancel_round(&view, "the session was closed with cancel")
                        .await
                        .map_err(|e| runtime_error(&e))?;
                    match starter
                        .session(session)
                        .await
                        .map_err(|e| runtime_error(&e))?
                    {
                        Some(now) => view = now,
                        None => return refuse(no_such_session(session)),
                    }
                    if view.closed {
                        return Ok(closed(session, None));
                    }
                } else if view.closed {
                    return Ok(closed(session, Some(STILL_WORKING)));
                } else {
                    let ask = Inbound::new(SESSION_CLOSE_KIND, json!({})).with_id(ctx.call_id());
                    match starter.steer(&view, ask).await {
                        Ok(()) => return Ok(closed(session, Some(STILL_WORKING))),
                        // It ended meanwhile: close it as an idle session.
                        Err(e) if is_busy_or_finished(&e) => {
                            note = None;
                            continue;
                        }
                        Err(e) => return Err(runtime_error(&e)),
                    }
                }
            }
            // The round has ended and the session is open: a round that only closes it.
            let mut header = view.header.clone().unwrap_or_else(|| SessionHeader {
                id: session,
                agent: view.agent.clone(),
                description: String::new(),
                mode: SessionMode::Open,
                closed: false,
                closing: false,
                round: 0,
                root: None,
                workspace: None,
                visible: false,
                config: None,
            });
            header.round = view.round + 1;
            header.closed = true;
            header.closing = true;
            let input = Inbound::new(MESSAGE_KIND, adam_runtime::closing_round_payload(&header));
            match starter
                .start_round(
                    session,
                    round_id,
                    &view.agent,
                    input,
                    Some(view.latest_run),
                    self.roster
                        .by_registered(&view.agent)
                        .and_then(|a| a.within),
                )
                .await
            {
                Ok(_) => return Ok(closed(session, None)),
                Err(e) if is_busy_or_finished(&e) => note = None,
                Err(e) => return Err(runtime_error(&e)),
            }
        }
        Err(ToolError::Transient(format!(
            "session `{session}` kept changing while it was closed"
        )))
    }
}

const STILL_WORKING: &str =
    "its round is still working and is left to finish; read its report with subagent_read";

fn closed(session: RunId, note: Option<&str>) -> ToolOutput {
    let mut body = json!({ "session": session, "state": "closed" });
    if let (Some(note), Some(map)) = (note, body.as_object_mut()) {
        map.insert("note".to_owned(), json!(note));
    }
    ToolOutput::text(body.to_string())
}

// ---------------------------------------------------------------------------
// subagent_list
// ---------------------------------------------------------------------------

struct List {
    roster: Arc<Roster>,
}

#[async_trait]
impl Tool for List {
    /// Declared like the other session tools, which the loop runs alone.
    fn access(&self, _args: &serde_json::Value) -> ToolAccess {
        ToolAccess::Alone
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SUBAGENT_LIST.to_owned(),
            description: format!(
                "List the sessions of this conversation, the open ones first: working, then idle, \
                 then closed. Paged: `offset` and `limit` count sessions. Each line is the session \
                 id, the subagent, the state, the round and its mode (who may send it more), then \
                 what it is for. Use an id with {SUBAGENT_READ}, {SUBAGENT_SEND} or {SUBAGENT_CLOSE}."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "offset": {
                        "type": "integer",
                        "description": "The session to start at, from 1; negative counts from the end.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "The most sessions to return.",
                    },
                },
                "additionalProperties": false,
            }),
        }
    }

    fn paged(&self) -> bool {
        true
    }

    fn time_limit(&self) -> Option<Duration> {
        Some(SESSION_TOOL_TIME_LIMIT)
    }

    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("List subagents")
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let (offset, limit) = match (int_arg(&args, "offset"), int_arg(&args, "limit")) {
            (Ok(offset), Ok(limit)) => (offset, limit),
            (Err(why), _) | (_, Err(why)) => return refuse(why),
        };
        let page = match window_args(
            offset,
            limit,
            u32::try_from(MAX_SESSION_PAGE).unwrap_or(100),
        ) {
            Ok(page) => page,
            Err(why) => return refuse(why),
        };
        let starter = ctx.child_starter()?;
        let agents = self.roster.registered();
        let total = starter
            .sessions(&agents, 0, 0)
            .await
            .map_err(|e| runtime_error(&e))?
            .total;
        let range = match window(total, page.0, page.1) {
            Ok(range) => range,
            Err(PageError::PastEnd { offset, total }) => {
                return refuse(format!("{total} sessions: offset {offset} is past the end"));
            }
            Err(e) => return refuse(e.to_string()),
        };
        let shown = starter
            .sessions(&agents, range.start, range.end - range.start)
            .await
            .map_err(|e| runtime_error(&e))?;
        let mut out = String::new();
        if shown.total == 0 {
            out.push_str("no sessions\n");
        }
        for s in &shown.sessions {
            let state = match (s.state, s.closed) {
                (SessionState::Working, true) => "working (closes when its round ends)",
                (SessionState::Working, false) => "working",
                (SessionState::Idle, _) => "idle",
                (SessionState::Closed, _) => "closed",
            };
            let name = self.roster.name_of(&s.agent);
            let line = format!(
                "{} {} {} round {} mode={} \"{}\"",
                s.id,
                escape_text(name.as_bytes()),
                state,
                s.round,
                s.mode,
                escape_text(s.description.as_bytes()),
            );
            let line = match s.config.as_ref().and_then(config_words) {
                Some(words) => format!("{line} config: {words}"),
                None => line,
            };
            out.push_str(&clip_item(&line));
            out.push('\n');
        }
        let shown_end = range.start + shown.sessions.len();
        if shown_end < shown.total {
            out.push_str(&more_line(
                (shown.total - shown_end) as u64,
                "sessions",
                shown_end as u64 + 1,
            ));
        } else {
            out.push_str(&end_line(shown.total as u64, "sessions", &[]));
        }
        Ok(ToolOutput::text(out))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use adam_runtime::{ReportRound, RoundOutcome};

    /// ADR 0087 slice C3: a filed mode is the default of a start that names none and the widest
    /// one a start may name; no file is `one_shot` up to `open`.
    #[test]
    fn a_start_may_narrow_the_filed_mode_and_never_widen_it() {
        let filed = [
            None,
            Some(SessionMode::OneShot),
            Some(SessionMode::Sealed),
            Some(SessionMode::Open),
        ];
        for mode in filed {
            let defaults = SessionDefaults { mode, wait: None };
            assert_eq!(
                defaults.admit("helper", None),
                Ok(mode.unwrap_or(SessionMode::OneShot)),
                "{mode:?}"
            );
            for asked in SessionMode::ALL {
                for by_keep_open in [false, true] {
                    let got = defaults.admit(
                        "helper",
                        Some(AskedMode {
                            mode: asked,
                            by_keep_open,
                        }),
                    );
                    if asked <= mode.unwrap_or(SessionMode::Open) {
                        assert_eq!(got, Ok(asked), "{mode:?} {asked}");
                    } else {
                        let why = got.unwrap_err();
                        assert!(
                            why.contains(&format!("at most {} for helper", defaults.ceiling())),
                            "{why}"
                        );
                    }
                }
            }
        }
        let one_shot = SessionDefaults {
            mode: Some(SessionMode::OneShot),
            wait: None,
        };
        assert_eq!(
            one_shot.admit(
                "helper",
                Some(AskedMode {
                    mode: SessionMode::Sealed,
                    by_keep_open: false,
                }),
            ),
            Err(
                "`mode` is sealed; at most one_shot for helper, as its file's `session.mode` \
                 says: one of one_shot"
                    .to_owned()
            )
        );
        assert!(!SessionDefaults::default().default_wait());
        assert!(
            SessionDefaults {
                mode: None,
                wait: Some(true),
            }
            .default_wait()
        );
    }

    fn ended(me: RunId, by: Option<RunId>) -> SessionSummary {
        let latest = RunId::new();
        SessionSummary {
            id: RunId::new(),
            agent: "helper".into(),
            conversation_id: "c".into(),
            latest_run: latest,
            round: 1,
            state: SessionState::Idle,
            outcome: Some(RoundOutcome::Done),
            updated_at: Timestamp::now(),
            description: String::new(),
            mode: SessionMode::Open,
            closed: false,
            closing: false,
            idle_expired: false,
            started_by: by.or(Some(me)),
            report: ReportRound {
                run: latest,
                ended: true,
                cancelled: false,
                started_by: by.or(Some(me)),
                updated_at: Timestamp::now(),
            },
            config: None,
        }
    }

    fn due(session: &SessionSummary, me: RunId, reported: &[RunId]) -> bool {
        is_due(
            session,
            &DueFacts {
                me,
                reported,
                in_history: &HashSet::new(),
                since: None,
            },
        )
    }

    #[test]
    fn a_round_this_run_started_and_that_ended_is_due() {
        let me = RunId::new();
        let session = ended(me, None);
        assert!(due(&session, me, &[]));
        // A session that was not kept open is closed once its round ends, and is still due.
        let mut closed = ended(me, None);
        closed.state = SessionState::Closed;
        assert!(due(&closed, me, &[]));
    }

    #[test]
    fn a_session_that_expired_idle_still_owes_its_unreported_round() {
        let me = RunId::new();
        let mut session = ended(me, None);
        session.state = SessionState::Closed;
        session.closed = true;
        session.idle_expired = true;
        assert!(due(&session, me, &[]));
        assert!(!due(&session, me, &[session.report.run]), "told once");
    }

    #[test]
    fn a_round_that_works_is_not_due() {
        let me = RunId::new();
        let mut session = ended(me, None);
        session.state = SessionState::Working;
        session.outcome = None;
        session.report.ended = false;
        assert!(!due(&session, me, &[]));
    }

    #[test]
    fn a_closing_round_after_the_round_that_worked_does_not_hide_it() {
        let me = RunId::new();
        let mut session = ended(me, None);
        // The closing round is the latest and still works; the report is of the round before.
        session.latest_run = RunId::new();
        session.closing = true;
        session.state = SessionState::Working;
        assert!(due(&session, me, &[]));
        // A session whose only round is a closing one reports nothing.
        session.report.run = session.latest_run;
        assert!(!due(&session, me, &[]));
    }

    #[test]
    fn a_round_cancelled_by_the_close_that_follows_is_not_news() {
        let me = RunId::new();
        let mut session = ended(me, None);
        session.latest_run = RunId::new();
        session.closing = true;
        session.report.cancelled = true;
        assert!(!due(&session, me, &[]));
        // Cancelled with no close after it (a person stopped it): it is news.
        session.closing = false;
        session.latest_run = session.report.run;
        assert!(due(&session, me, &[]));
    }

    #[test]
    fn a_round_that_a_later_round_replaced_is_not_reported_on_its_own() {
        let me = RunId::new();
        let mut session = ended(me, None);
        // Round 2 works: the report is of round 2, which has not ended.
        session.latest_run = RunId::new();
        session.state = SessionState::Working;
        session.report = ReportRound {
            run: session.latest_run,
            ended: false,
            cancelled: false,
            started_by: Some(me),
            updated_at: Timestamp::now(),
        };
        assert!(!due(&session, me, &[]));
    }

    #[test]
    fn a_round_of_another_run_is_not_due() {
        let me = RunId::new();
        let session = ended(me, Some(RunId::new()));
        assert!(!due(&session, me, &[]));
    }

    #[test]
    fn a_round_a_person_started_is_due_only_if_it_ended_after_the_chain_began() {
        let me = RunId::new();
        let mut session = ended(me, None);
        session.started_by = Some(session.latest_run);
        session.report.started_by = Some(session.latest_run);
        assert!(session.report.started_outside_a_run());
        assert!(due(&session, me, &[]));
        let none = HashSet::new();
        let facts = |since| DueFacts {
            me,
            reported: &[],
            in_history: &none,
            since,
        };
        let at = session.report.updated_at;
        assert!(is_due(
            &session,
            &facts(Some(at - jiff::SignedDuration::from_secs(1)))
        ));
        assert!(is_due(&session, &facts(Some(at))));
        assert!(!is_due(
            &session,
            &facts(Some(at + jiff::SignedDuration::from_secs(1)))
        ));
    }

    #[test]
    fn a_round_already_reported_is_not_due_and_a_later_one_is() {
        let me = RunId::new();
        let session = ended(me, None);
        assert!(!due(&session, me, &[RunId::new(), session.report.run]));
        let mut next = session.clone();
        next.report.run = RunId::new();
        assert!(due(&next, me, &[session.report.run]));
    }

    #[test]
    fn a_round_given_up_by_the_cap_is_not_due_while_its_call_is_in_the_history() {
        let me = RunId::new();
        let session = ended(me, None);
        let in_history: HashSet<String> = [report_call_id(session.report.run)].into();
        let facts = |in_history| DueFacts {
            me,
            reported: &[],
            in_history,
            since: None,
        };
        assert!(!is_due(&session, &facts(&in_history)));
        assert!(is_due(&session, &facts(&HashSet::new())));
    }
}
