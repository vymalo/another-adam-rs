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

use std::sync::Arc;
use std::time::Duration;

use adam_core::RunId;
use adam_model::ToolSpec;
use adam_runtime::{
    Inbound, MAX_SESSION_PAGE, RuntimeError, SESSION_CLOSE_KIND, SessionHeader, SessionState,
    SessionView,
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

/// The most rounds of one conversation that may work at once: a start or a send that would make
/// another is refused. ADR 0045 decision 5 calls it `max_working` (4); a constant until slice 4.
pub const MAX_WORKING_ROUNDS: usize = 4;

/// The most sessions of one conversation that may be open at once (not closed): a start past it is
/// refused. ADR 0045 decision 5 calls it `max_open`; it is a constant until its slice makes it a
/// setting.
pub const MAX_OPEN_SESSIONS: usize = 16;

/// The most rounds one session may have, a close included: a send past it is refused. ADR 0045
/// decision 5 calls it `max_rounds`; a constant until its slice makes it a setting.
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
    /// Whether it runs on another system (an A2A agent). A session cannot be started on one yet:
    /// the call is refused in words that point to the subagent's own tool.
    pub remote: bool,
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
}

impl Roster {
    fn names(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.name.as_str()).collect()
    }

    fn registered(&self) -> Vec<&str> {
        self.agents
            .iter()
            .filter(|a| !a.remote)
            .map(|a| a.agent.as_str())
            .collect()
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
            roster: Arc::new(Roster { agents }),
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

    /// The registration names of the subagents a session can be started on (the local ones):
    /// what [`LlmAgentBuilder::session_agents`](crate::LlmAgentBuilder::session_agents) takes.
    pub fn session_agents(&self) -> Vec<String> {
        self.roster
            .registered()
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn closed_words(session: RunId) -> String {
    format!(
        "session `{session}` is closed and takes no more messages (a closed session is never \
         reopened). Start a new one with `{SUBAGENT_START}`, and put what it needs in `prompt`: it \
         does not see the closed session's conversation."
    )
}

/// The state a model is told: what the session is doing and whether it will take a message.
fn state_word(view: &SessionView) -> &'static str {
    match (view.state, view.closed) {
        (SessionState::Working, true) => "working (closes when its round ends)",
        (SessionState::Working, false) => "working",
        (SessionState::Idle, _) => "idle",
        (SessionState::Closed, _) => "closed",
    }
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
    let head = format!(
        "session {} ({}, \"{}\"): round {}, {}",
        view.id,
        roster.name_of(&view.agent),
        escape_text(description_of(view).as_bytes()),
        view.round,
        state_word(view)
    );
    if view.state == SessionState::Working {
        return ToolOutput::text(format!(
            "{head}\nreport: none yet; the round is still working. Call {SUBAGENT_READ} with \
             wait: true to wait for it; do not poll.\n{}",
            end_line(0, "lines", &[])
        ));
    }
    let (status, body) = match (&view.latest.status, &view.latest.error) {
        (adam_core::RunStatus::Done, _) => ("done", render_output(view.latest.output.as_ref())),
        (_, error) => (
            "failed",
            error.as_deref().unwrap_or("no reason given").to_owned(),
        ),
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
    out.reported = vec![view.report_run];
    out
}

fn is_busy_or_finished(e: &RuntimeError) -> bool {
    e.is_busy() || matches!(e, RuntimeError::Finished { .. })
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
             it; never poll), and send it more with {SUBAGENT_SEND}. With keep_open false (the \
             default) the session closes when its round has ended; set keep_open true only when \
             another message will follow, and close what you kept open with {SUBAGENT_CLOSE}. Subagents:"
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
                    "keep_open": {
                        "type": "boolean",
                        "default": false,
                        "description": "Keep the session open after its first report, to send it \
                                        more. By default it closes when its round has ended.",
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
        if agent.remote {
            return refuse(format!(
                "`{}` runs on another system, and a session cannot be started on it yet: call the \
                 `{}` tool for a synchronous ask",
                agent.name, agent.name
            ));
        }
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
        let keep = match bool_arg(&args, "keep_open", false) {
            Ok(keep) => keep,
            Err(why) => return refuse(why),
        };

        let starter = ctx.child_starter()?;
        let session = ctx.child_run_id();
        let started = json!({
            "session": session,
            "agent": agent.name,
            "state": "working",
            "keep_open": keep,
        });
        // A repeat of this call (a retry after a lost record) finds the round it started.
        if starter
            .session(session)
            .await
            .map_err(|e| runtime_error(&e))?
            .is_some()
        {
            return Ok(ToolOutput::text(started.to_string()));
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
        let working = listed
            .sessions
            .iter()
            .filter(|s| s.state == SessionState::Working)
            .count();
        if working >= MAX_WORKING_ROUNDS {
            return refuse(format!(
                "{MAX_WORKING_ROUNDS} subagent rounds are already working: read a report with \
                 `{SUBAGENT_READ}` (wait: true) before starting another"
            ));
        }
        if open >= MAX_OPEN_SESSIONS
            || (listed.total > MAX_SESSION_PAGE && open >= MAX_SESSION_PAGE)
        {
            return refuse(format!(
                "this conversation already has {MAX_OPEN_SESSIONS} open sessions: close one with \
                 `{SUBAGENT_CLOSE}` (`{SUBAGENT_LIST}` shows them) before starting another"
            ));
        }
        let header = SessionHeader {
            id: session,
            agent: agent.agent.clone(),
            description,
            keep_open: keep,
            closed: false,
            closing: false,
            round: 1,
        };
        let input = round_message(ctx, prompt, &header);
        starter
            .start_round(session, session, &agent.agent, input, None, agent.within)
            .await
            .map_err(|e| runtime_error(&e))?;
        Ok(ToolOutput::text(started.to_string()))
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
                return Ok(sent(session, view.round, "steered"));
            }
            if view.closed {
                return refuse(closed_words(session));
            }
            let steered = || user_inbound(message, ctx.call_id());
            if view.state == SessionState::Working {
                match starter.steer(&view, steered()).await {
                    Ok(()) => return Ok(sent(session, view.round, "steered")),
                    Err(e) if is_busy_or_finished(&e) => continue,
                    Err(e) => return Err(runtime_error(&e)),
                }
            }
            if view.round >= MAX_SESSION_ROUNDS {
                return refuse(format!(
                    "session `{session}` has had {MAX_SESSION_ROUNDS} rounds, the most a session \
                     may have: close it and start a new one with `{SUBAGENT_START}`"
                ));
            }
            let mut header = view.header.clone().unwrap_or_else(|| SessionHeader {
                id: session,
                agent: view.agent.clone(),
                description: String::new(),
                keep_open: true,
                closed: false,
                closing: false,
                round: 0,
            });
            header.round = view.round + 1;
            let within = self
                .roster
                .by_registered(&view.agent)
                .and_then(|a| a.within);
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
        Ok(RemotePoll::Ready(read_output(
            &self.roster,
            &view,
            None,
            None,
        )))
    }
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
                keep_open: true,
                closed: false,
                closing: false,
                round: 0,
            });
            header.round = view.round + 1;
            header.closed = true;
            header.closing = true;
            let mut input = Inbound::new(MESSAGE_KIND, json!({ "close_session": true }));
            if let Some(payload) = input.payload.as_object_mut() {
                payload.insert(
                    adam_runtime::SESSION_STATE_KEY.to_owned(),
                    header_json(&header),
                );
            }
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
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SUBAGENT_LIST.to_owned(),
            description: format!(
                "List the sessions of this conversation, the open ones first: working, then idle, \
                 then closed. Paged: `offset` and `limit` count sessions. Each line is the session \
                 id, the subagent, the state, the round and whether it is kept open, then what it \
                 is for. Use an id with {SUBAGENT_READ}, {SUBAGENT_SEND} or {SUBAGENT_CLOSE}."
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
                "{} {} {} round {} keep_open={} \"{}\"",
                s.id,
                escape_text(name.as_bytes()),
                state,
                s.round,
                s.keep_open,
                escape_text(s.description.as_bytes()),
            );
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
