//! [`LlmAgent`]: the durable model <-> tools loop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_core::{RunId, RunStatus};
use adam_error::report;
use adam_model::{
    Classify, DynModel, FinishReason, Message, ModelError, ModelRequest, ModelResponse, ToolCall,
    ToolChoice, ToolSpec,
};
use adam_runtime::{
    AGENT_TEXT_KIND, Agent, AgentError, AgentStarter, CHILD_REPORTED_KIND, ChildStatus, Ctx,
    Emitter, Inbound, MAX_RUN_FILE_BYTES, MAX_SESSION_PAGE, RUN_FINISHED_KIND, RunEvent,
    SESSION_CLOSE_KIND, SandboxUsage, SessionState, StepState, Transition, UsageEvent, UsageTotals,
    child_run_id,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::conversation::{
    ArtifactRef, Conversation, LimitHit, LimitKind, PendingQuestion, PendingRemote, PendingRun,
    PendingSession, PendingWait, Repeats, asks_to_close, parse_context, parse_root_run,
    parse_root_step, parse_session, parse_user_text,
};
use crate::history::fit_history;
use crate::kept::{KeptOutput, MAX_RUN_KEPT_BYTES};
use crate::paging::backstop;
use crate::plan::Plan;
use crate::sessions::{SUBAGENT_CLOSE, SUBAGENT_READ, SUBAGENT_SEND, SUBAGENT_START};

/// Whether the history holds a call that started a session or sent to one.
fn has_started_session(state: &Conversation) -> bool {
    state.messages.iter().any(|m| {
        m.tool_calls()
            .iter()
            .any(|c| c.name == SUBAGENT_START || c.name == SUBAGENT_SEND)
    })
}

/// Whether a call of `tool` acts on a session (an effect outside the run's own state).
fn has_effect(tool: &str) -> bool {
    matches!(tool, SUBAGENT_START | SUBAGENT_SEND | SUBAGENT_CLOSE)
}
use crate::source::{
    DynToolSource, SourceCtx, ToolNote, ToolSource, instructed, offered, refined, withheld,
};
use crate::state::Extensions;
use crate::step_io::StepIo;
use crate::text_stream;
use crate::tool::{
    DEFAULT_TOOL_TIME_LIMIT, DynTool, RemotePoll, StepStyle, TOOL_STOP_GRACE, Tool, ToolCtx,
    ToolError, ToolOutput,
};
use crate::toolset::ToolSet;

/// What the run state says to the call it is about to run.
#[derive(Clone)]
struct CallFacts {
    /// Bytes of files the run has already handed over.
    files_used: u64,
    /// The turn that asked for the call stopped at the output limit.
    output_cut: bool,
    /// The outputs the run keeps in its own state, and how many bytes it may still keep there.
    kept: Arc<BTreeMap<String, String>>,
    kept_left: usize,
}

/// What the model is asked in the closing turn, the last model call of a run that reached
/// `max_turns` or `max_tool_calls`. Sent after the history, as a user message that is not kept in it.
pub const CLOSING_TURN: &str = "You have used all the steps this run may take. Do not call any \
tool. Reply in plain words: what you did, what is finished, and what is still left to do.";

/// Bounds on one run. A run that reaches `max_turns` or `max_tool_calls` gets one closing model
/// call in which it may call no tool, and ends with that answer marked `limited` (the output's
/// `limited` and `limit`); if the closing call says nothing the run fails with a message naming the
/// limit (`RunView::error`). The history limit shortens old tool outputs instead (see
/// [`max_history_tokens`](Self::max_history_tokens)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most model calls that may be followed by tools in one run. The call after them is the
    /// closing turn: one more, with the tools offered but none callable.
    pub max_turns: u32,
    /// Most tool calls in one run, counted when the model requests them. A
    /// model turn whose calls would go over the limit has none of them run (each is answered
    /// "not run"), and the next call is the closing turn.
    pub max_tool_calls: u32,
    /// Passed to the model as `max_output_tokens`; `0` leaves it to the model.
    pub max_output_tokens: u32,
    /// Budget for the history sent to the model, estimated as characters / 4.
    /// Old tool outputs are truncated (with an explicit marker) to fit; the
    /// results the model has not seen yet are never touched, and the stored
    /// history stays complete. `0` disables truncation.
    pub max_history_tokens: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_tool_calls: 200,
            max_output_tokens: 4096,
            max_history_tokens: 100_000,
        }
    }
}

/// What a turn that a cancel ended says. The run is `Failed` with the reason the canceller gave
/// (`cancelled: <reason>`, written by `Runtime::cancel`); this text is only ever dropped with the
/// commit that carries it, and shows up in logs.
const CANCELLED: &str = "cancelled: the run was cancelled while it worked";

/// A model failure in journal form: [`ModelError`] is not serializable, but
/// whether it is worth retrying must survive being recorded. The journal is a
/// persistence boundary, so the error chain is flattened here, once, with
/// [`report`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelFailure {
    retryable: bool,
    message: String,
    /// The provider's `Retry-After`, in milliseconds. Absent in journals
    /// written before hints were honoured, which decode as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
    /// The error's [`ErrorClass`], for whoever reads the journal. Absent in
    /// journals written before classes existed, which decode as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    class: Option<String>,
}

impl ModelFailure {
    /// The call was dropped because the run was cancelled. Not retryable: nobody is waiting for it.
    fn cancelled() -> Self {
        Self {
            retryable: false,
            message: CANCELLED.to_owned(),
            retry_after_ms: None,
            class: None,
        }
    }
}

impl From<text_stream::StreamStop> for ModelFailure {
    fn from(stop: text_stream::StreamStop) -> Self {
        match stop {
            text_stream::StreamStop::Failed(e) => e.into(),
            text_stream::StreamStop::Cancelled => Self::cancelled(),
        }
    }
}

impl From<ModelError> for ModelFailure {
    fn from(e: ModelError) -> Self {
        Self {
            retryable: e.is_retryable(),
            message: report(&e),
            retry_after_ms: e
                .retry_after()
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            class: Some(format!("{:?}", e.class())),
        }
    }
}

/// A model call as the journal records it: the response, and the stream its words were sent as, if
/// they were (see [`LlmAgentBuilder::stream_text`]).
///
/// The response's own members are written at the top level, as a journal written before streaming
/// existed holds them (a bare [`ModelResponse`]), which therefore decodes as one with no stream.
#[derive(Debug, Serialize, Deserialize)]
struct Recorded {
    #[serde(flatten)]
    response: ModelResponse,
    /// Absent when the words were not streamed, and in journals written before they could be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream: Option<String>,
    /// What the tool sources said about the tools they offered for this call ([`ToolNote`]); absent
    /// when they said nothing and in journals written before the notes existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notes: Vec<ToolNote>,
    /// How many messages the history held when the call was made. A turn that is replayed in a later
    /// transition (the commit of the first was lost) sees a longer history if a message arrived in
    /// between, and then knows its answer did not read it. Absent in journals written before, which
    /// read as "the answer is current".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seen: Option<usize>,
    /// The call's id for its usage report ([`call_id`]), made inside the step: a replay reports the
    /// call under the same id, a call made again (its first try never recorded) under another.
    /// Absent in journals written before reports existed, which report under [`old_call_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call: Option<String>,
}

/// The id of the stream of the words of turn `turn` of `run`: unique within the task, because the
/// run and the turn are, and because a retry of the turn (the failed try's journal is abandoned and
/// the step runs again) is another stream, which the suffix tells from the one that was abandoned.
fn stream_id(run: RunId, turn: u32) -> String {
    let mut suffix = uuid::Uuid::new_v4().simple().to_string();
    suffix.truncate(8);
    format!("{run}-m{turn}-{suffix}")
}

/// The id of model call `turn` of `run` in its usage report: unique within the task (the run id is
/// in it, a child run's too), at most 57 bytes. Made inside the journaled step and recorded with the
/// answer, like [`stream_id`]: a replay finds it there, and a call that runs again because its first
/// try was never recorded (the process died before the journal write) is a new call with a new id.
fn call_id(run: RunId, turn: u32) -> String {
    let mut suffix = uuid::Uuid::new_v4().simple().to_string();
    suffix.truncate(8);
    format!("{run}-c{turn}-{suffix}")
}

/// The id of a call recorded by a build that made none: the run and the turn, which a replay of the
/// same journal entry always gives again.
fn old_call_id(run: RunId, turn: u32) -> String {
    format!("{run}-c{turn}")
}

/// The id of the stream the reasoning of a model turn is sent as: apart from [`stream_id`]'s, which
/// is the id of the message the words become.
fn reasoning_stream_id(run: RunId, turn: u32) -> String {
    let mut suffix = uuid::Uuid::new_v4().simple().to_string();
    suffix.truncate(8);
    format!("{run}-r{turn}-{suffix}")
}

/// What a turn does with the reasoning its model wrote: counts it (a DEBUG line with how many
/// characters, never the text), says it as a stream of its own when it was not already sent while it
/// was written (`streamed`), and **drops it from the response**, so it is in no journal, no output
/// and no step. The reasoning a client echoes back lives in the message, which is not touched.
async fn unthought(
    emitter: &Emitter,
    run: RunId,
    turn: u32,
    streamed: bool,
    mut response: ModelResponse,
) -> ModelResponse {
    if let Some(reasoning) = response.reasoning.take() {
        tracing::debug!(
            run = %run,
            turn,
            reasoning_chars = reasoning.chars().count(),
            streamed,
            "the model reasoned before it answered"
        );
        if !streamed {
            text_stream::say_reasoning(emitter, &reasoning, move || reasoning_stream_id(run, turn))
                .await;
        }
    }
    response
}

/// Configures an [`LlmAgent`]. Start with [`LlmAgent::builder`].
pub struct LlmAgentBuilder {
    name: String,
    model: DynModel,
    model_alias: String,
    instructions: Option<String>,
    tools: Vec<DynTool>,
    sources: Vec<DynToolSource>,
    limits: Limits,
    extensions: Extensions,
    wait_poll: Duration,
    stream_text: bool,
    step_io: StepIo,
    session_agents: Vec<String>,
}

/// How long a run waiting for a child run sleeps before it looks at the child itself, unless
/// [`LlmAgentBuilder::wait_poll`] says otherwise.
pub const DEFAULT_WAIT_POLL: Duration = Duration::from_secs(60);

/// Why [`LlmAgentBuilder::try_build`] refused to build an agent.
///
/// A mistake in how the agent is put together, found at startup.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// A tool needs shared state the agent was not given.
    #[error("tool `{tool}` needs shared state `{state}`: register it with LlmAgentBuilder::state")]
    MissingState {
        /// The tool that declared the need ([`Tool::required_state`]).
        tool: String,
        /// The type of the missing state.
        state: String,
    },
    /// Two tools have the same name, and the model could not tell them apart.
    #[error("two tools are called `{name}`")]
    DuplicateTool {
        /// The repeated name.
        name: String,
    },
}

impl Classify for BuildError {
    fn class(&self) -> adam_model::ErrorClass {
        adam_model::ErrorClass::Invalid
    }
}

impl LlmAgentBuilder {
    /// The system prompt.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Register a tool. A second tool with the same name replaces the first.
    pub fn tool(self, tool: impl Tool) -> Self {
        self.dyn_tool(Arc::new(tool))
    }

    /// Register an already shared tool.
    pub fn dyn_tool(mut self, tool: DynTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Register every tool of a [`ToolSet`], in order.
    pub fn tools(self, tools: ToolSet) -> Self {
        tools
            .into_iter()
            .fold(self, |builder, tool| builder.dyn_tool(tool))
    }

    /// Add a [`ToolSource`]: tools the agent learns about while it runs, offered to the model on
    /// every turn after the agent's own tools. Sources are asked in the order they were added.
    pub fn tool_source(self, source: impl ToolSource) -> Self {
        self.dyn_tool_source(Arc::new(source))
    }

    /// Add an already shared [`ToolSource`].
    pub fn dyn_tool_source(mut self, source: DynToolSource) -> Self {
        self.sources.push(source);
        self
    }

    /// Share `value` with the tools: they read it with
    /// [`ToolCtx::state`] (as `State<T>`), and
    /// [`try_build`](Self::try_build) checks it exists for every tool that
    /// declares it in [`Tool::required_state`]. One value per type; a second
    /// call with the same `T` replaces the first.
    pub fn state<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        self.extensions.insert(value);
        self
    }

    /// Replace the [`Limits`] (default: [`Limits::default`]).
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// How long a run that waits for a child run (a tool returned [`ToolError::AwaitRun`]) sleeps
    /// before it reads the child itself (default [`DEFAULT_WAIT_POLL`], 60 s).
    ///
    /// The child normally tells the parent when it finishes, and the parent wakes at once. The
    /// timer is the fallback for a message that never arrived (the child's process died between
    /// its commit and the message), so it bounds how late the parent can be, not how often it
    /// runs: each wake without the message is one store read. A zero interval is raised to one
    /// millisecond.
    ///
    /// A run that waits for a task on another system ([`ToolError::AwaitRemote`]) is told nothing
    /// when the task ends, so for it this is how often the task is looked at, and the longest a
    /// finished task waits to be noticed.
    pub fn wait_poll(mut self, every: Duration) -> Self {
        self.wait_poll = every.max(Duration::from_millis(1));
        self
    }

    /// Whether a model turn is asked for as a stream (the default) and its words are sent as they are
    /// written ([`RunEvent::TextDelta`]), or as one answer ([`ModelClient::complete`](adam_model::ModelClient::complete))
    /// that is told when it is whole, as it was before.
    ///
    /// Streaming changes what observers see, not what the run does: the turn is still one journaled
    /// step and the answer is the assembled response, so the history, the tools and the outcome are
    /// the same. A client of the model that cannot stream, or a provider that answers a stream
    /// request with an error, is a reason to turn it off. See "Streamed text" on [`LlmAgent`].
    pub fn stream_text(mut self, on: bool) -> Self {
        self.stream_text = on;
        self
    }

    /// Make the agent wait, at the end of a turn, for the sessions it started in this task
    /// ([ADR 0045]): `agents` are the registration names of the subagents the session tools
    /// ([`SessionTools`](crate::SessionTools)) can start. A model answer with no tool call, while a
    /// session of one of them that this run started still works, does not end the run: the run
    /// parks on the session as if the model had called `subagent_read` with `wait: true`, and the
    /// report reaches the model before the run ends. The agent needs the tools of
    /// [`SessionTools::tools`](crate::SessionTools::tools) registered, or the wait has nothing to
    /// read the report with and is skipped. The default is none, which waits for nothing.
    ///
    /// [ADR 0045]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md
    #[must_use]
    pub fn session_agents(mut self, agents: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.session_agents = agents.into_iter().map(Into::into).collect();
        self
    }

    /// How the step of a tool call reports what the tool was given and what it answered: the
    /// arguments on the report that starts the step, the result (or the error) on the one that ends
    /// it, each scrubbed by [`StepIo::redact`] and then cut to the contract's bounds (4 KiB in, 8 KiB
    /// out). The default sends both, with no redactor; [`StepIo::off`] sends neither. See "Events"
    /// on [`LlmAgent`] and ADR 0011.
    pub fn step_io(mut self, step_io: StepIo) -> Self {
        self.step_io = step_io;
        self
    }

    /// Build the agent, or say what is wrong with how it was put together:
    /// a tool whose [`Tool::required_state`] was not registered with
    /// [`state`](Self::state), or two tools with one name.
    ///
    /// [`build`](Self::build) keeps accepting both (the last duplicate wins,
    /// with a warning; a missing state shows up when the tool runs), for
    /// compatibility. Prefer this one.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use adam_llm_agent::{BuildError, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, StateKey};
    /// use adam_model::{MockModel, ToolSpec};
    /// use async_trait::async_trait;
    /// use serde_json::{Value, json};
    ///
    /// struct Db;
    /// struct Lookup;
    ///
    /// #[async_trait]
    /// impl Tool for Lookup {
    ///     fn spec(&self) -> ToolSpec {
    ///         ToolSpec { name: "lookup".into(), description: "Look up.".into(),
    ///                    parameters: json!({"type": "object", "properties": {}}) }
    ///     }
    ///     fn required_state(&self) -> Vec<StateKey> { vec![StateKey::of::<Db>()] }
    ///     async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
    ///         let _db = ctx.require_state::<Db>()?;
    ///         Ok(ToolOutput::text("found"))
    ///     }
    /// }
    ///
    /// let model = Arc::new(MockModel::new());
    /// let missing = LlmAgent::builder("a", model.clone(), "m").tool(Lookup).try_build();
    /// assert!(matches!(missing, Err(BuildError::MissingState { .. })));
    /// let ok = LlmAgent::builder("a", model, "m").state(Arc::new(Db)).tool(Lookup).try_build();
    /// assert!(ok.is_ok());
    /// ```
    pub fn try_build(self) -> Result<LlmAgent, BuildError> {
        let mut names = HashSet::new();
        for tool in &self.tools {
            let name = tool.spec().name;
            for key in tool.required_state() {
                if !self.extensions.contains(key) {
                    return Err(BuildError::MissingState {
                        tool: name,
                        state: key.to_string(),
                    });
                }
            }
            if !names.insert(name.clone()) {
                return Err(BuildError::DuplicateTool { name });
            }
        }
        Ok(self.build())
    }

    /// Build the agent.
    pub fn build(self) -> LlmAgent {
        let mut specs: Vec<ToolSpec> = Vec::new();
        let mut tools: Vec<DynTool> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for tool in self.tools {
            let spec = tool.spec();
            match index.get(&spec.name) {
                Some(&i) => {
                    tracing::warn!(tool = %spec.name, "tool registered twice, keeping the last");
                    specs[i] = spec;
                    tools[i] = tool;
                }
                None => {
                    index.insert(spec.name.clone(), tools.len());
                    specs.push(spec);
                    tools.push(tool);
                }
            }
        }
        LlmAgent {
            name: self.name,
            model: self.model,
            model_alias: self.model_alias,
            instructions: self.instructions,
            tools,
            sources: self.sources,
            index,
            specs,
            limits: self.limits,
            extensions: Arc::new(self.extensions),
            wait_poll: self.wait_poll,
            stream_text: self.stream_text,
            step_io: self.step_io,
            session_agents: self.session_agents.into(),
        }
    }
}

/// A durable LLM tool-calling agent: instructions + a model + a toolset.
///
/// Implements [`adam_runtime::Agent`] with [`Conversation`] as its state.
/// Register it on a `Runtime` and start runs with a user message (see
/// [`user_message`](crate::user_message)).
///
/// # One step is one model turn
///
/// 1. Inbox messages are appended to the history (or, when the run was parked
///    on a question, the first one answers it). A run parked on a child run
///    looks at the child instead (see "Child runs" below).
/// 2. The model is called in a journaled step `model:<turn>` with the history
///    and tool specs.
/// 3. Each requested tool runs as its own journaled step `tool:<call id>` and
///    its result is appended. Then the step returns `Continue`, so every
///    turn is committed before the next begins. A turn without tool calls
///    returns `Done` with `{"text": <final text>, "artifacts": [{name,
///    mime_type}...]}`. When a tool announced the answer
///    ([`ToolOutput::announcing`](crate::ToolOutput::announcing)) in this turn of the conversation, the
///    text is the last announcement and not the model's closing words (see "Announced answers").
///
/// Because steps are journaled, a restarted worker replays recorded model and
/// tool results instead of repeating them.
///
/// # Events
///
/// All best effort, through `Ctx::emit`:
/// * `TextDelta` while a model turn that streams ([`LlmAgentBuilder::stream_text`], on by default) writes: the
///   pieces of its words, in a stream whose id is made inside the journaled step and recorded with the answer
///   (see "Streamed text" below);
/// * `Custom { kind: "agent_text", payload: {text, turn} }` for each model turn with text, with `stream` (the
///   id of the stream the words were sent as) for a turn that goes on to call tools;
/// * `Step` for each tool call: a [`StepEvent`](adam_runtime::StepEvent) with the id `tool:<call id>`, the
///   kind, label and icon of [`Tool::step_style`] (the tool's name by default), `running` before
///   the tool runs and, after it, `completed` (a result), `failed` (an error result, or a failure
///   the run retries), or `waiting` (the tool asked the person a question, or the run parked on a
///   child run or a remote task: the end comes when the answer does). The `running` report carries
///   the call's arguments (`input`) and the report that ends the step the result, or the error it
///   ended with (`output`), scrubbed and cut by the agent's [`StepIo`](crate::StepIo) (ADR 0011);
/// * `Step` again from [`ToolCtx::emit_progress`] (an update of the call's own step, the text in its
///   detail) and from [`ToolCtx::report_step`] (steps that run under the call's: a command, a tool
///   of the agent the call drives);
/// * `Usage` for each model call that completed (the provider answered, with or without usage): its
///   tokens ([`Usage::accounted`](adam_model::Usage::accounted)), the alias, the provider and the
///   context window the model client says, and an id made inside the journaled step and recorded with
///   the answer, so a replay reports the call under the same id. It is an event of the **root** run
///   ([`ToolCtx::root_run_id`]): a child run's report names the root's step it works under
///   (`Conversation::root_step`), so the task's client counts a subagent apart. The tokens are also
///   counted in [`Conversation::usage_totals`], and a child's totals are added to its parent's when it
///   answers (ADR 0032);
/// * `Artifact` for each artifact a tool returns (also durable);
/// * `Custom { kind: "input_required", payload: {question, call_id} }` before parking;
/// * `Custom { kind: "awaiting_run", payload: {call_id, run} }` before parking on a child run,
///   followed by the call's step `waiting` (not final), and later by the final `completed` or
///   `failed` when the child's outcome becomes the result;
/// * `Custom { kind: "awaiting_remote", payload: {call_id, task} }` before parking on a remote
///   task, with the same step sequence.
///
/// `Step` replaces the `Custom` events `tool_start` and `tool_end` that earlier versions emitted
/// (`tool_end`'s `status` words: `ok` is `completed`, `error` and `transient_error` are `failed`,
/// `needs_input` and `waiting` are `waiting`), and `Progress` from `emit_progress`.
///
/// # Streamed text
///
/// A model turn is asked for as a stream ([`ModelClient::stream`](adam_model::ModelClient::stream)) unless
/// [`LlmAgentBuilder::stream_text`] says otherwise, and what the model writes is sent as it arrives
/// ([`RunEvent::TextDelta`]): a piece when 200 bytes have gathered or 100 ms have passed since the last, at most
/// [`MAX_TEXT_DELTA_BYTES`](adam_runtime::MAX_TEXT_DELTA_BYTES) at a time, in a stream that opens on the first word
/// that is not blank and ends with a piece marked `last` (`abandoned` when the model failed). The turn is still one
/// journaled step whose outcome is the assembled response, recorded with the stream's id (a journal written before
/// reads as one with no stream): a replay calls no model and sends no piece, but names the same stream, and a
/// failure, in the middle of the answer too, is the same failure as a failed
/// [`complete`](adam_model::ModelClient::complete). The answer that ends the run names its stream in the run's output
/// (`stream`), as `agent_text` does for the words before a tool call.
///
/// # Announced answers
///
/// A tool can say "this is my answer" before the model is done: [`ToolOutput::announcing`]. The words
/// are kept in [`Conversation::announced`] (journaled with the call's result, so a replay announces the
/// same ones), a later announcement replaces them, and an error result announces nothing. When the run
/// finishes, they are the output's `text`, and so the text of the A2A `completed` status, whatever the
/// model closes with; the closing words are still said, as `agent_text` with their `stream`, and the
/// output names no stream. A message that reaches the run ends the turn and clears the announcement.
///
/// A tool can also **end the turn with its announcement** ([`ToolOutput::final_answer`]): once the
/// calls of that model turn are answered, the run finishes with the announced text and the model is
/// not called again, so there are no closing words to say after an answer that was already handed
/// over ([`Conversation::announced_final`]). A message that reached the run meanwhile is read first.
///
/// # Child runs
///
/// A tool that returns [`ToolError::AwaitRun`] has started a child run (with
/// `Runtime::start_child`, under [`ToolCtx::child_run_id`]) whose outcome is
/// the answer. The run parks with a timer ([`LlmAgentBuilder::wait_poll`],
/// 60 s by default) and records the wait in [`Conversation::pending_wait`].
/// When the child finishes, the runtime delivers an `adam.run.finished`
/// message and the parent wakes at once; if that message is lost, the timer
/// wakes it and it reads the child (`Ctx::child_status`), so a lost message
/// costs at most one interval. The outcome becomes the tool result: the
/// child's `text` (or its output as JSON), or, for a child that failed or was
/// cancelled, an error result saying so. The message is matched to the wait by
/// the child's run id, and one that matches nothing (a copy of a message
/// already used, or a stray one) is dropped. User messages that arrive
/// meanwhile queue behind the owed result. Cancelling the parent does not
/// cancel the child.
///
/// # Remote tasks
///
/// A tool that starts a task on another system (an A2A agent) returns
/// [`ToolError::AwaitRemote`]. The run records a [`PendingWait::Remote`] and parks with the same
/// timer as for a child run, but nothing tells it when the task is over, so every time the timer
/// fires it asks the tool ([`Tool::poll_remote`]) in a journaled step named `poll:<call id>`, and
/// parks again until the answer is [`RemotePoll::Ready`]. A replay after a crash sees the recorded
/// answers, so the tool is not asked twice for one wake, and the call that started the task is
/// not repeated. If the tool gave a timeout, the call is answered with an error result once it has
/// passed (measured by the journaled clock). Cancelling the run does not cancel the remote task.
///
/// # Failure handling
///
/// * Retryable model error (`ModelError::is_retryable`) or
///   [`ToolError::Transient`]: `AgentError::Transient`, retried by the runtime.
///   A `ModelError::RateLimited` that carries a `retry_after` becomes an
///   `AgentError::Transient` with that hint (`with_retry_after`), so the
///   retry waits at least that long
///   (the runtime's backoff still applies when it is longer).
/// * Other model error: the run fails (`model call failed: ...`).
/// * [`ToolError::Permanent`], `ToolOutput { is_error: true }` and unknown
///   tool names: an error tool result for the model; the run goes on. So does
///   a child run that failed, was cancelled or no longer exists.
/// * A call whose arguments were not JSON ([`ToolCall::invalid_arguments`]): an error tool result
///   quoting what was sent; nothing runs; the run goes on.
/// * A tripped `max_turns` or `max_tool_calls`: one closing model call in which no tool may be
///   called ([`CLOSING_TURN`]), and the run ends `Done` with its answer marked `limited`; the run
///   fails only if that answer has no words.
#[derive(Clone)]
pub struct LlmAgent {
    name: String,
    model: DynModel,
    model_alias: String,
    instructions: Option<String>,
    tools: Vec<DynTool>,
    sources: Vec<DynToolSource>,
    index: HashMap<String, usize>,
    specs: Vec<ToolSpec>,
    limits: Limits,
    extensions: Arc<Extensions>,
    wait_poll: Duration,
    stream_text: bool,
    step_io: StepIo,
    /// The registered names of the subagents whose sessions the turn-end wait looks at.
    session_agents: Arc<[String]>,
}

impl std::fmt::Debug for LlmAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmAgent")
            .field("name", &self.name)
            .field("model_alias", &self.model_alias)
            .field(
                "tools",
                &self
                    .specs
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Send a step report, unless `silent`: the system behind the tool's source reports its calls itself.
async fn say(ctx: &mut Ctx, silent: bool, event: RunEvent) {
    if !silent {
        ctx.emit(event).await;
    }
}

/// Whether the call of tool `name` goes unreported by the agent: the tool is a source's and its
/// listing said the system behind the source reports its calls.
fn silent_call(agent: &LlmAgent, state: &Conversation, name: &str) -> bool {
    agent
        .source_note(&state.source_notes, name)
        .is_some_and(|n| n.reports_step)
}

/// What a part of a step decided.
enum Flow {
    /// Nothing terminal happened; carry on.
    Next,
    /// Wait for the user (a tool asked a question).
    Park,
    /// Wait for a child run, with a timer.
    Wait,
    /// The model finished.
    Done(Value),
    /// Fail the run.
    Fail(String),
}

impl LlmAgent {
    /// Start building an agent called `name` that talks to `model` under the
    /// gateway alias `model_alias`.
    pub fn builder(
        name: impl Into<String>,
        model: DynModel,
        model_alias: impl Into<String>,
    ) -> LlmAgentBuilder {
        LlmAgentBuilder {
            name: name.into(),
            model,
            model_alias: model_alias.into(),
            instructions: None,
            tools: Vec::new(),
            sources: Vec::new(),
            limits: Limits::default(),
            extensions: Extensions::new(),
            wait_poll: DEFAULT_WAIT_POLL,
            stream_text: true,
            step_io: StepIo::default(),
            session_agents: Vec::new(),
        }
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn tool(&self, name: &str) -> Option<&DynTool> {
        self.index.get(name).map(|&i| &self.tools[i])
    }

    /// Whether the tool `name` pages its results: it says so ([`Tool::paged`]). Never inferred
    /// from its schema, which an unrelated tool may give an `offset` argument.
    fn pages(&self, name: &str) -> bool {
        self.tool(name).is_some_and(|t| t.paged())
    }

    /// Move inbound messages into the state; the first one answers a pending
    /// question. Returns `false` when the run must keep waiting for that answer.
    ///
    /// A wait on a child run is not touched here: user messages queue behind
    /// the owed tool result like any other.
    fn absorb_inbox(&self, state: &mut Conversation, inbox: Vec<Inbound>) -> bool {
        let mut texts = Vec::new();
        for inbound in inbox {
            // A message the run has read is not read again: the orchestration layer sends the same
            // one a second time after a lost lease (`steer/v1`: the `messageId` is the key).
            if state.has_read(&inbound.id) {
                tracing::debug!(inbound = %inbound.id, "ignoring a message the run has already read");
                continue;
            }
            match parse_user_text(&inbound.payload) {
                Ok(text) => {
                    state.note_read(&inbound.id);
                    // What the message says about its sender is read along with what it says.
                    if let Some(context) = parse_context(&inbound.payload) {
                        state.merge_context(context);
                    }
                    texts.push(text);
                }
                Err(reason) => {
                    tracing::warn!(inbound = %inbound.id, kind = %inbound.kind, %reason, "ignoring unreadable inbound message");
                }
            }
        }
        let mut texts = texts.into_iter();
        // A message that reaches the run starts a new turn: what a tool announced as the answer of
        // the one before is not the answer of this one.
        if !texts.as_slice().is_empty() {
            state.announced = None;
            state.announced_final = false;
        }
        if let Some(PendingWait::Question(q)) = &state.pending_wait {
            let Some(answer) = texts.next() else {
                return false;
            };
            let call_id = q.call_id.clone();
            state.pending_wait = None;
            if state.pending_calls.first().is_some_and(|c| c.id == call_id) {
                state.pending_calls.remove(0);
            }
            state.messages.push(Message::tool_result(call_id, answer));
        }
        for text in texts {
            let message = Message::user_text(text);
            if state.pending_calls.is_empty() {
                state.messages.push(message);
            } else {
                state.deferred.push(message);
            }
        }
        true
    }

    /// When a run that waits for a child looks at it next.
    fn next_poll(&self, ctx: &Ctx) -> DateTime<Utc> {
        let every = chrono::Duration::from_std(self.wait_poll).unwrap_or(chrono::Duration::MAX);
        ctx.now()
            .checked_add_signed(every)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    }

    /// `ms` milliseconds from now, by the journaled clock.
    async fn deadline_after(&self, ctx: &mut Ctx, ms: u64) -> Result<DateTime<Utc>, AgentError> {
        let now = ctx.now_journaled().await?;
        let every = i64::try_from(ms)
            .ok()
            .and_then(chrono::Duration::try_milliseconds)
            .unwrap_or(chrono::Duration::MAX);
        Ok(now
            .checked_add_signed(every)
            .unwrap_or(DateTime::<Utc>::MAX_UTC))
    }

    /// The child's outcome as the result of the call that waited for it. Returns the state the end of
    /// the call's step carries.
    fn answer_run(state: &mut Conversation, wait: &PendingRun, child: &ChildStatus) -> StepState {
        let (message, status) = match (&child.status, &child.error) {
            (RunStatus::Done, _) => (
                Message::tool_result(wait.call_id.clone(), render_output(child.output.as_ref())),
                StepState::Completed,
            ),
            (_, error) => (
                Message::tool_error(
                    wait.call_id.clone(),
                    backstop(
                        &format!(
                            "the run failed: {}",
                            error.as_deref().unwrap_or("no reason given")
                        ),
                        "",
                        false,
                    )
                    .into_owned(),
                ),
                StepState::Failed,
            ),
        };
        state.messages.push(message);
        if state
            .pending_calls
            .first()
            .is_some_and(|c| c.id == wait.call_id)
        {
            state.pending_calls.remove(0);
        }
        state.pending_wait = None;
        status
    }

    /// Where the child a run waits for stands: what its message said, or, without one, what the
    /// store says. `None`: still working.
    async fn settle(
        ctx: &Ctx,
        wait: &PendingRun,
        notices: &[(RunId, ChildStatus)],
    ) -> Result<Option<ChildStatus>, AgentError> {
        if let Some((_, status)) = notices.iter().find(|(run, _)| *run == wait.run) {
            return Ok(Some(status.clone()));
        }
        Ok(match ctx.child_status(wait.run).await? {
            Some(status) if status.is_finished() => Some(status),
            Some(_) => None,
            None => Some(ChildStatus::vanished()),
        })
    }

    /// One journaled model call, then the assistant message into the state.
    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), turn = state.turns))]
    async fn model_turn(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
    ) -> Result<Flow, AgentError> {
        if ctx.is_cancelled() {
            return Ok(Flow::Fail(CANCELLED.into()));
        }
        // The run was reopened after its closing answer. With no message since (the answer is still
        // the last one), it ends with exactly that output again and calls no model. A message that
        // arrived is read by one more closing turn: its answer replaces the old one.
        if let Some(hit) = &mut state.limit
            && hit.answered
        {
            if let Some(answer) = &hit.answer
                && matches!(state.messages.last(), Some(Message::Assistant { .. }))
            {
                return Ok(Flow::Done(answer.clone()));
            }
            hit.answered = false;
            hit.answer = None;
        }
        if state.limit.is_none() && state.turns >= self.limits.max_turns {
            state.limit = Some(LimitHit {
                limit: LimitKind::MaxTurns,
                answered: false,
                answer: None,
            });
        }
        let closing = state.limit.is_some();
        let mut request = ModelRequest::new(self.model_alias.clone());
        request.system = self.instructions.clone();
        request.messages = fit_history(&state.messages, self.limits.max_history_tokens);
        request.tools = self.specs.clone();
        if closing {
            // The tools stay, so the prefix of the request is the one the provider has cached and
            // the calls in the history still have their tools; only the choice changes.
            request.tool_choice = ToolChoice::None;
            request.messages.push(Message::user_text(CLOSING_TURN));
        }
        request.max_output_tokens =
            (self.limits.max_output_tokens > 0).then_some(self.limits.max_output_tokens);

        let model = self.model.clone();
        let sources = self.sources.clone();
        let source_ctx = (!sources.is_empty()).then(|| {
            SourceCtx::new(
                ctx.run_id(),
                state.root_run.unwrap_or_else(|| ctx.run_id()),
                ctx.conversation_id().map(str::to_owned),
                Arc::new(state.context.clone()),
            )
        });
        let turn = state.turns;
        let seen = state.messages.len();
        let run = ctx.run_id();
        let emitter = ctx.emitter();
        let stream_text = self.stream_text;
        let cancel = ctx.cancel_token();
        let recorded: Result<Recorded, ModelFailure> = ctx
            .step(&format!("model:{turn}"), move || async move {
                // The sources are read here, inside the step, so that a replay of a turn whose model
                // call is recorded does not read them again: only the answer is journaled, not the
                // tools it was given.
                let mut notes = Vec::new();
                if let Some(source_ctx) = &source_ctx {
                    let (more, said) = offered(&sources, source_ctx, &request.tools).await;
                    request.tools.extend(more);
                    notes = said;
                    refined(&sources, source_ctx, &mut request.tools).await;
                    // What a source holds back is not shown, the agent's own tools included.
                    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
                    let held = withheld(&sources, source_ctx, &names).await;
                    if !held.is_empty() {
                        request
                            .tools
                            .retain(|t| !held.iter().any(|h| h.tool == t.name));
                    }
                    // What the sources add to the instructions (the agents a person mentioned): read
                    // here with the tools, so a replay of a recorded turn reads nothing.
                    if let Some(more) = instructed(&sources, source_ctx).await {
                        request.system = Some(match request.system.take() {
                            Some(own) if !own.trim().is_empty() => format!("{own}\n\n{more}"),
                            _ => more,
                        });
                    }
                }
                if !stream_text {
                    // The request races the run's cancellation: a cancel drops it, and with it the
                    // connection, instead of letting the provider finish an answer nobody will read.
                    let answer = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(ModelFailure::cancelled()),
                        answer = model.complete(request) => answer.map_err(ModelFailure::from)?,
                    };
                    return Ok(Recorded {
                        response: unthought(&emitter, run, turn, false, answer).await,
                        stream: None,
                        notes,
                        seen: Some(seen),
                        call: Some(call_id(run, turn)),
                    });
                }
                // The words go out while they are written, under an id made here, inside the step, and
                // recorded with the answer: a replay calls no model and sends no pieces, but it knows
                // which stream the words were, so it says them whole under the same id.
                let streamed = text_stream::stream_response(
                    &model,
                    request,
                    &emitter,
                    &cancel,
                    move || stream_id(run, turn),
                    move || reasoning_stream_id(run, turn),
                )
                .await
                .map_err(ModelFailure::from)?;
                Ok(Recorded {
                    response: unthought(&emitter, run, turn, true, streamed.response).await,
                    stream: streamed.stream,
                    notes,
                    seen: Some(seen),
                    call: Some(call_id(run, turn)),
                })
            })
            .await?;
        // The call completed (the provider answered): it is reported, every time this step is
        // passed, under the id its record holds, so a replay says the same report again. A
        // cancelled turn's call is reported too: it was made, though the turn is not kept.
        let usage = match &recorded {
            Ok(r) => {
                let usage = r.response.usage.accounted();
                let call = r.call.clone().unwrap_or_else(|| old_call_id(run, turn));
                self.report_usage(ctx, state, call, usage).await;
                usage
            }
            Err(_) => adam_model::Usage::default(),
        };
        // A cancel ends the turn here: nothing the model said is acted on, said or kept. The run is
        // already `Failed` in the store (that is what told the token), so this result is dropped
        // by the worker's commit.
        if ctx.is_cancelled() {
            return Ok(Flow::Fail(CANCELLED.into()));
        }
        let answered_stale: bool;
        let (response, stream) = match recorded {
            Ok(Recorded {
                response,
                stream,
                notes,
                seen,
                call: _,
            }) => {
                // The calls this turn asks for are the ones its listing described.
                state.source_notes = notes;
                // An answer recorded before a message that this transition read did not read it.
                answered_stale = seen.is_some_and(|seen| seen < state.messages.len());
                (response, stream)
            }
            // A recorded error is replayed forever on crash-replay, but a
            // transient retry starts at a fresh seq, so it calls again.
            Err(f) if f.retryable => {
                let error = AgentError::transient(format!("model call failed: {}", f.message));
                return Err(match f.retry_after_ms {
                    // The provider said how long to wait: the runtime waits
                    // at least that long (see `AgentError::with_retry_after`).
                    Some(ms) => error.with_retry_after(Duration::from_millis(ms)),
                    None => error,
                });
            }
            Err(f) => return Ok(Flow::Fail(format!("model call failed: {}", f.message))),
        };

        let ModelResponse {
            mut message,
            finish,
            usage: _,
            reasoning: _,
        } = response;
        // The call's tokens count whatever its answer was: the provider took them.
        state.usage = state.usage.saturating_add(usage);
        state
            .usage_totals
            .add(self.model.provider(), &self.model_alias, usage);
        if !matches!(message, Message::Assistant { .. }) {
            return Ok(Flow::Fail(
                "model call failed: the response is not an assistant message".into(),
            ));
        }
        state.turns += 1;
        state.output_cut = finish == FinishReason::Length;

        let text = message.text();
        // The closing turn may call nothing: a call the model sends anyway is dropped, so the history
        // holds no call without a result.
        if closing && let Message::Assistant { tool_calls, .. } = &mut message {
            tool_calls.clear();
        }
        let mut calls: Vec<ToolCall> = message.tool_calls().to_vec();
        // An answer that was recorded before a message this transition has read (the commit of the
        // transition that recorded it was lost, and the message arrived before this one began) did
        // not read it: it is dropped and the model answers again, with the message in front of it.
        if calls.is_empty() && answered_stale {
            tracing::debug!(
                run = %ctx.run_id(),
                turn,
                "the recorded answer predates a message the run has read: the model answers again"
            );
            return Ok(Flow::Next);
        }
        // A message that reached the run while the model wrote its answer has not been read by it:
        // the answer is not the last word, and the run takes another turn instead of ending
        // (`steer/v1`: an accepted message is never lost). What arrives after this check is caught
        // by the commit (`Ctx::reopen_on_arrival`).
        let unread = calls.is_empty() && ctx.arrived().await? > 0;
        // The turn ends here, but a session this run started still works: its report must reach
        // the model before the run ends, so the turn goes on with a read that waits for it, as if
        // the model had asked for one (the words it said stay, and are said as words before a call).
        if calls.is_empty()
            && !unread
            && state.limit.is_none()
            && state.tool_calls < self.limits.max_tool_calls
            && let Some(session) = self.session_to_wait_for(ctx, state, turn).await?
        {
            let wait = ToolCall::new(
                format!("wait_{turn}"),
                SUBAGENT_READ,
                json!({ "session": session, "wait": true }),
            );
            if let Message::Assistant { tool_calls, .. } = &mut message {
                tool_calls.push(wait.clone());
            }
            calls.push(wait);
        }
        if !text.is_empty() {
            let mut payload = json!({ "text": text, "turn": turn });
            // The words before a tool call are said whole, under the stream they were sent as, because
            // nothing else will say them: the turn goes on to its tools. The answer that ends the run
            // is said by the run itself, whose output names its stream (below).
            // So are the closing words of a turn whose answer a tool announced: the answer is the
            // announced text, and what the model says after it is working text, stated here. And
            // so are the words of an answer that does not end the run because a message is unread.
            if (!calls.is_empty() || state.announced.is_some() || unread)
                && let Some(stream) = &stream
            {
                payload["stream"] = json!(stream);
            }
            ctx.emit(RunEvent::Custom {
                kind: AGENT_TEXT_KIND.into(),
                payload,
            })
            .await;
        }
        state.messages.push(message);

        if unread {
            tracing::debug!(
                run = %ctx.run_id(),
                turn,
                "a message arrived while the model answered: the run takes another turn"
            );
            return Ok(Flow::Next);
        }
        if let Some(limit) = state.limit.as_ref().map(|hit| hit.limit) {
            let said = state.announced.clone().unwrap_or(text);
            if said.trim().is_empty() {
                return Ok(Flow::Fail(self.limit_message(state, limit)));
            }
            let output = limited_output(state, limit, said, finish == FinishReason::Length, stream);
            if let Some(hit) = &mut state.limit {
                hit.answered = true;
                hit.answer = Some(output.clone());
            }
            return Ok(Flow::Done(output));
        }
        if calls.is_empty() {
            // What a tool announced as the answer is the run's answer (the last announcement wins);
            // without one, the words that end the turn are.
            if let Some(announced) = state.announced.clone() {
                return Ok(Flow::Done(json!({
                    "text": announced,
                    "artifacts": state.artifacts,
                })));
            }
            let mut output = json!({
                "text": text,
                "artifacts": state.artifacts,
            });
            if finish == FinishReason::Length {
                output["truncated"] = json!(true);
            }
            // The answer was streamed: its stream is its id, so whoever read the pieces knows this text
            // for what it is.
            if let Some(stream) = stream {
                output["stream"] = json!(stream);
            }
            return Ok(Flow::Done(output));
        }

        let requested = u32::try_from(calls.len()).unwrap_or(u32::MAX);
        if state.tool_calls.saturating_add(requested) > self.limits.max_tool_calls {
            // None of the turn's calls runs, and each is answered, so the history has no call without
            // a result; the next model call is the closing turn.
            let said = format!(
                "not run: {} tool calls were already made and this turn asked for {} more \
                 (max_tool_calls = {})",
                state.tool_calls, requested, self.limits.max_tool_calls
            );
            state.messages.extend(
                calls
                    .into_iter()
                    .map(|call| Message::tool_error(call.id, said.clone())),
            );
            state.limit = Some(LimitHit {
                limit: LimitKind::MaxToolCalls,
                answered: false,
                answer: None,
            });
            return Ok(Flow::Next);
        }
        state.tool_calls += requested;
        state.pending_calls = calls;
        Ok(Flow::Next)
    }

    /// Report a completed model call of this run (`usage/v1`): as an event of the **root** run, the
    /// task's, with the root's step this run works under when it is a child, so the task's client
    /// counts a subagent's tokens apart. The model is the alias the request named, the provider and
    /// the context window what the model client says of it.
    async fn report_usage(
        &self,
        ctx: &Ctx,
        state: &Conversation,
        call: String,
        usage: adam_model::Usage,
    ) {
        let mut event = UsageEvent::new(call, &self.model_alias, usage);
        if let Some(provider) = self.model.provider() {
            event = event.with_provider(provider);
        }
        if let Some(window) = self.model.context_window(&self.model_alias) {
            event = event.with_context_window(window);
        }
        if let Some(step) = &state.root_step {
            event = event.under(step);
        }
        let root = state.root_run.unwrap_or_else(|| ctx.run_id());
        ctx.emitter().emit_for(root, RunEvent::Usage(event)).await;
    }

    /// Count the model calls of the child run `child`, which has just answered, in this run's
    /// totals: the totals its state keeps (`usage_totals`, its own children's included), and the
    /// sandbox usage its state keeps (`sandbox_usage`, likewise). A child whose state says none
    /// (another kind of agent, or one purged) adds nothing.
    async fn count_child(
        ctx: &Ctx,
        state: &mut Conversation,
        child: RunId,
    ) -> Result<(), AgentError> {
        let Some(child_state) = ctx.child_state(child).await? else {
            return Ok(());
        };
        let totals = child_state
            .get("usage_totals")
            .cloned()
            .and_then(|totals| serde_json::from_value::<UsageTotals>(totals).ok());
        if let Some(totals) = totals {
            state.usage_totals.merge(&totals);
        }
        // The child's commands ran in the root run's sandbox: they are the task's.
        let sandbox = child_state
            .get("sandbox_usage")
            .cloned()
            .and_then(|usage| serde_json::from_value::<SandboxUsage>(usage).ok());
        if let Some(sandbox) = sandbox {
            state.sandbox_usage.merge(&sandbox);
        }
        Ok(())
    }

    /// The session, if any, that this run started in this task and whose round still works, read
    /// from the store in a journaled step so that a replay and a takeover decide the same: the
    /// model has answered, and the run may not end while this one works. `None` for an agent that
    /// has no session tools. A failure to list is a retry when the store says so, and no wait
    /// otherwise.
    async fn session_to_wait_for(
        &self,
        ctx: &mut Ctx,
        state: &Conversation,
        turn: u32,
    ) -> Result<Option<RunId>, AgentError> {
        if self.session_agents.is_empty() || self.tool(SUBAGENT_READ).is_none() {
            return Ok(None);
        }
        // A round this run started is the result of a call to one of these two, so a history
        // without such a call has nothing to wait for and the store is not asked: a run that never
        // uses sessions journals what it always did.
        if !has_started_session(state) {
            return Ok(None);
        }
        let starter = ctx.child_starter();
        let me = ctx.run_id();
        let agents = self.session_agents.clone();
        let found: Result<Option<RunId>, (bool, String)> = ctx
            .step(&format!("sessions:{turn}"), move || async move {
                let names: Vec<&str> = agents.iter().map(String::as_str).collect();
                let list = starter
                    .sessions(&names, 0, MAX_SESSION_PAGE)
                    .await
                    .map_err(|e| (e.is_retryable(), report(&e)))?;
                Ok(list
                    .sessions
                    .into_iter()
                    .find(|s| s.state == SessionState::Working && s.started_by == Some(me))
                    .map(|s| s.id))
            })
            .await?;
        match found {
            Ok(session) => Ok(session),
            Err((true, why)) => Err(AgentError::transient(format!(
                "listing the sessions failed: {why}"
            ))),
            Err((false, why)) => Err(AgentError::transient(format!(
                "listing the sessions failed: {why}"
            ))),
        }
    }

    /// The run is about to end: a round it started or sent to that still works is cancelled (the
    /// turn-end wait could not wait for it: a limit, the last turn), so that a done run never leaves
    /// a round working with nobody to read it. A kept-open session goes idle, another closes; the
    /// answer says so. A listing that fails fails the try.
    async fn end_sessions(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        output: &mut Value,
    ) -> Result<(), AgentError> {
        if self.session_agents.is_empty() || !has_started_session(state) {
            return Ok(());
        }
        let starter = ctx.child_starter();
        let me = ctx.run_id();
        let agents = self.session_agents.clone();
        let cancelled: Result<usize, String> = ctx
            .step(
                &format!("sessions_end:{}", state.turns),
                move || async move {
                    let names: Vec<&str> = agents.iter().map(String::as_str).collect();
                    let list = starter
                        .sessions(&names, 0, MAX_SESSION_PAGE)
                        .await
                        .map_err(|e| report(&e))?;
                    let mut n = 0;
                    for s in list
                        .sessions
                        .iter()
                        .filter(|s| s.state == SessionState::Working && s.started_by == Some(me))
                    {
                        starter
                            .cancel_listed(s, "the run that started it ended")
                            .await
                            .map_err(|e| report(&e))?;
                        n += 1;
                    }
                    Ok(n)
                },
            )
            .await?;
        match cancelled {
            Ok(0) => {}
            Ok(n) => {
                let note = format!(
                    "(The run ended while {n} subagent round(s) it started were still working: they were cancelled.)"
                );
                if let Some(Value::String(text)) = output.get_mut("text") {
                    text.push_str("\n\n");
                    text.push_str(&note);
                }
            }
            Err(why) => {
                return Err(AgentError::transient(format!(
                    "ending the sessions failed: {why}"
                )));
            }
        }
        // Every round this run started is in its totals before it ends, read or not.
        Self::count_rounds(ctx, state, true).await
    }

    /// Count the tokens of the rounds this run started ([`Conversation::rounds_started`]) that have
    /// ended, each once. The read of a round is the journaled step `usage:<call id>`, named by the
    /// call and not by the round's run id, so a replay (of this run, or of a copy of its journal)
    /// counts what the first try counted. A round that works is left owed, unless `ending` (the run
    /// is over: a round the cancel could not stop is given up). A store error on the read is
    /// retried, never swallowed; a round that cannot be read for another reason is warned about and
    /// given up.
    async fn count_rounds(
        ctx: &mut Ctx,
        state: &mut Conversation,
        ending: bool,
    ) -> Result<(), AgentError> {
        let me = ctx.run_id();
        for call in state.rounds_started.clone() {
            let round = child_run_id(me, &call);
            let starter = ctx.child_starter();
            let read: Result<RoundUsage, (bool, String)> = ctx
                .step(&format!("usage:{call}"), move || async move {
                    match starter.round_record(round).await {
                        Ok(None) => Ok(RoundUsage::Gone),
                        Ok(Some((status, _))) if !status.is_terminal() => Ok(RoundUsage::Working),
                        Ok(Some((_, child_state))) => Ok(RoundUsage::Counted {
                            totals: child_state
                                .get("usage_totals")
                                .cloned()
                                .and_then(|t| serde_json::from_value::<UsageTotals>(t).ok()),
                            sandbox: child_state
                                .get("sandbox_usage")
                                .cloned()
                                .and_then(|u| serde_json::from_value::<SandboxUsage>(u).ok()),
                        }),
                        Err(e) => Err((e.is_retryable(), report(&e))),
                    }
                })
                .await?;
            match read {
                Ok(RoundUsage::Working) if !ending => continue,
                Ok(RoundUsage::Working | RoundUsage::Gone) => {}
                Ok(RoundUsage::Counted { totals, sandbox }) => {
                    if let Some(totals) = totals {
                        state.usage_totals.merge(&totals);
                    }
                    if let Some(sandbox) = sandbox {
                        state.sandbox_usage.merge(&sandbox);
                    }
                }
                Err((true, why)) => {
                    return Err(AgentError::transient(format!(
                        "reading a round to count its tokens failed: {why}"
                    )));
                }
                Err((false, why)) => {
                    tracing::warn!(%round, error = %why, "a round's tokens cannot be counted");
                }
            }
            state.rounds_started.retain(|c| *c != call);
        }
        Ok(())
    }

    /// Run the owed tool calls in order, each as its own journaled step.
    ///
    /// `notices` are the finished-child messages this transition received: a call that starts a
    /// child which already finished (its message got here first) is answered at once.
    async fn run_pending(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        notices: &[(RunId, ChildStatus)],
    ) -> Result<Flow, AgentError> {
        while let Some(call) = state.pending_calls.first().cloned() {
            // A cancel stops the turn between its calls too: the ones still owed do not start.
            if ctx.is_cancelled() {
                return Ok(Flow::Fail(CANCELLED.into()));
            }
            // `doom_loop`: the third call in a row with the same tool, the same arguments and the
            // same result as the two before it does not run. Decided from the committed state alone.
            let key = call_key(&call);
            if let Some(repeats) = &mut state.repeats
                && repeats.key == key
                && (repeats.count >= DOOM_LOOP_AFTER
                    || repeats.failing >= DOOM_LOOP_AFTER
                    || repeats.marked >= DOOM_LOOP_AFTER)
            {
                repeats.refused += 1;
                let ending = repeats.refused >= DOOM_LOOP_REFUSALS;
                let said = repeated_call(&call.name);
                let silent = silent_call(self, state, &call.name);
                let style = self.style_of(&call.name, &state.source_notes);
                say(
                    ctx,
                    silent,
                    RunEvent::Step(style.event(&call.name, &call.id, StepState::Running)),
                )
                .await;
                say(
                    ctx,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((&said, true)),
                        &state.source_notes,
                    ),
                )
                .await;
                state
                    .messages
                    .push(Message::tool_error(call.id.clone(), said));
                state.pending_calls.remove(0);
                if ending {
                    // The calls the turn still owed are answered too, so that no call is left without
                    // a result; the next model call is the closing turn.
                    for rest in std::mem::take(&mut state.pending_calls) {
                        state.messages.push(Message::tool_error(
                            rest.id,
                            "not run: the run is ending because a call was repeated with the same result",
                        ));
                    }
                    state.limit = Some(LimitHit {
                        limit: LimitKind::RepeatedCall,
                        answered: false,
                        answer: None,
                    });
                    return Ok(Flow::Next);
                }
                continue;
            }
            // The round this call starts, if it starts one, is this run's to count.
            if matches!(call.name.as_str(), SUBAGENT_START | SUBAGENT_SEND)
                && !state.rounds_started.contains(&call.id)
            {
                state.rounds_started.push(call.id.clone());
            }
            let used = files_kept(&state.artifacts);
            let (result, reported) = self
                .run_tool(
                    ctx,
                    &state.context,
                    (
                        state.root_run,
                        state.root_step.as_deref(),
                        state.plan.as_ref(),
                    ),
                    &state.source_notes,
                    &call,
                    CallFacts {
                        files_used: used,
                        output_cut: state.output_cut,
                        kept: Arc::clone(&state.kept_outputs),
                        kept_left: MAX_RUN_KEPT_BYTES.saturating_sub(state.kept_bytes()),
                    },
                )
                .await?;
            state.sandbox_usage.merge(&reported);
            let (message, artifacts) = match result {
                ToolResult::Answered {
                    message,
                    artifacts,
                    answer,
                    repeat_key,
                    plan,
                    kept,
                    reported,
                } => {
                    if let Some(answer) = answer {
                        state.announce(answer);
                    }
                    record_answer(state, key, &message, repeat_key);
                    if let Some(plan) = plan {
                        state.plan = Some(*plan);
                    }
                    // What the call asked the run to keep, within what the run may keep: the call was
                    // told its room (`ToolCtx::kept_left`), so a result over it was never offered.
                    state.keep_output(kept);
                    if !reported.is_empty() {
                        Self::count_rounds(ctx, state, false).await?;
                    }
                    (message, artifacts)
                }
                ToolResult::NeedsInput { question, ui } => {
                    state.repeats = None;
                    ctx.emit(RunEvent::Custom {
                        kind: "input_required".into(),
                        payload: json!({ "question": question, "call_id": call.id }),
                    })
                    .await;
                    state.pending_wait = Some(PendingWait::Question(PendingQuestion {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        question,
                        ui,
                        stream: None,
                    }));
                    return Ok(Flow::Park);
                }
                ToolResult::AwaitRun(run) => {
                    state.repeats = None;
                    let wait = PendingRun {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        run,
                    };
                    let Some((_, child)) = notices.iter().find(|(r, _)| *r == run) else {
                        ctx.emit(RunEvent::Custom {
                            kind: "awaiting_run".into(),
                            payload: json!({ "call_id": call.id, "run": run }),
                        })
                        .await;
                        say(
                            ctx,
                            silent_call(self, state, &call.name),
                            self.step_event(
                                &call.name,
                                &call.id,
                                StepState::Waiting,
                                &state.source_notes,
                            ),
                        )
                        .await;
                        state.pending_wait = Some(PendingWait::Run(wait));
                        return Ok(Flow::Wait);
                    };
                    let outcome = Self::answer_run(state, &wait, child);
                    Self::count_child(ctx, state, run).await?;
                    say(
                        ctx,
                        silent_call(self, state, &call.name),
                        self.step_end_with_result(state, &call.name, &call.id, outcome),
                    )
                    .await;
                    continue;
                }
                ToolResult::AwaitSession(session) => {
                    state.repeats = None;
                    ctx.emit(RunEvent::Custom {
                        kind: "awaiting_session".into(),
                        payload: json!({ "call_id": call.id, "session": session }),
                    })
                    .await;
                    say(
                        ctx,
                        silent_call(self, state, &call.name),
                        self.step_event(
                            &call.name,
                            &call.id,
                            StepState::Waiting,
                            &state.source_notes,
                        ),
                    )
                    .await;
                    state.pending_wait = Some(PendingWait::Session(PendingSession {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        session,
                    }));
                    return Ok(Flow::Wait);
                }
                ToolResult::AwaitRemote { task, timeout_ms } => {
                    state.repeats = None;
                    // The deadline is fixed now, from the journaled clock, so that every replay
                    // of this transition parks with the same one.
                    let deadline = match timeout_ms {
                        Some(ms) => Some(self.deadline_after(ctx, ms).await?),
                        None => None,
                    };
                    ctx.emit(RunEvent::Custom {
                        kind: "awaiting_remote".into(),
                        payload: json!({ "call_id": call.id, "task": task }),
                    })
                    .await;
                    say(
                        ctx,
                        silent_call(self, state, &call.name),
                        self.step_event(
                            &call.name,
                            &call.id,
                            StepState::Waiting,
                            &state.source_notes,
                        ),
                    )
                    .await;
                    state.pending_wait = Some(PendingWait::Remote(PendingRemote {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        task,
                        deadline,
                    }));
                    return Ok(Flow::Wait);
                }
            };
            let effect = has_effect(&call.name);
            state.messages.push(message);
            add_artifact_refs(&mut state.artifacts, artifacts);
            state.pending_calls.remove(0);
            // Its result is committed before the next call runs (see `step`).
            if effect && !state.pending_calls.is_empty() {
                return Ok(Flow::Next);
            }
        }
        Ok(Flow::Next)
    }

    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), tool = %call.name, call_id = %call.id))]
    async fn run_tool(
        &self,
        ctx: &mut Ctx,
        context: &Map<String, Value>,
        root: (Option<RunId>, Option<&str>, Option<&Plan>),
        notes: &[ToolNote],
        call: &ToolCall,
        facts: CallFacts,
    ) -> Result<(ToolResult, SandboxUsage), AgentError> {
        let CallFacts {
            files_used,
            output_cut,
            kept,
            kept_left,
        } = facts;
        let end = |state: StepState| self.step_event(&call.name, &call.id, state, notes);
        // What the listing that offered this tool said about it. Only a source's tool has a note: the
        // agent's own tools win a name clash.
        let note = self.source_note(notes, &call.name);
        // The system behind the source reports each call itself: the agent reports no step of its own.
        let silent = note.is_some_and(|n| n.reports_step);
        // The report that starts the step says what the call was given (and, with a title the tool
        // has for itself, what to call it: `Tool::step_style`).
        let mut start =
            self.style_of(&call.name, notes)
                .event(&call.name, &call.id, StepState::Running);
        if let Some((input, max)) = self.step_io.input(&call.arguments) {
            start = start.with_input_within(input, max);
        }
        say(ctx, silent, RunEvent::Step(start)).await;

        // Arguments that were not JSON: nothing runs, and the model is told what it sent. The
        // answer comes from the recorded model turn alone, so a replay gives the same one.
        if let Some(raw) = &call.invalid_arguments {
            let said = invalid_arguments(&call.name, raw, output_cut);
            say(
                ctx,
                silent,
                self.step_end(
                    &call.name,
                    &call.id,
                    StepState::Failed,
                    Some((&said, true)),
                    notes,
                ),
            )
            .await;
            let answered = ToolResult::Answered {
                message: Message::tool_error(call.id.clone(), said),
                artifacts: Vec::new(),
                answer: None,
                repeat_key: None,
                plan: None,
                kept: None,
                reported: Vec::new(),
            };
            return Ok((answered, SandboxUsage::new()));
        }

        let tool_ctx = self
            .tool_ctx(ctx, context, root, notes, &call.id, &call.name)
            .with_note(note.cloned())
            .with_files_left((MAX_RUN_FILE_BYTES as u64).saturating_sub(files_used))
            .with_kept(kept_left, kept);
        let args = call.arguments.clone();
        // What a source holds back is refused here too, inside the step that records the answer:
        // the model was shown the tools of a turn that may be over, or named one it was not shown.
        let holding = (!self.sources.is_empty()).then(|| {
            (
                self.sources.clone(),
                SourceCtx::new(
                    ctx.run_id(),
                    root.0.unwrap_or_else(|| ctx.run_id()),
                    ctx.conversation_id().map(str::to_owned),
                    Arc::new(context.clone()),
                ),
                call.name.clone(),
            )
        });
        // What the call reported as sandbox usage rides in its result, which the step journals; a
        // call that ends in an error has no result to carry it, and leaves it here.
        let unjournaled = Arc::new(Mutex::new(SandboxUsage::new()));
        let outcome: Result<ToolOutput, ToolError> = match self.tool(&call.name).cloned() {
            Some(tool) => {
                let unjournaled = unjournaled.clone();
                let name = call.name.clone();
                ctx.step(&format!("tool:{}", call.id), move || async move {
                    if let Some((sources, source_ctx, name)) = &holding
                        && let Some(held) = withheld(sources, source_ctx, &[name.as_str()])
                            .await
                            .into_iter()
                            .next()
                    {
                        return Ok(ToolOutput::error(held.reason));
                    }
                    let limit = tool.time_limit().unwrap_or(DEFAULT_TOOL_TIME_LIMIT);
                    let called = within(&tool_ctx, &name, limit, |ctx| async move {
                        tool.call(&ctx, args).await
                    })
                    .await;
                    with_reported_sandbox(&tool_ctx, &unjournaled, called)
                })
                .await?
            }
            // A name that is none of the agent's own: an agent with no source says so at once,
            // one with sources asks them, inside the step that records the answer.
            // Consequence: this branch records NOTHING, so an agent with no source whose journal holds a
            // `tool:<id>` step for a tool it no longer has (the tool was removed between two versions)
            // would fail replay (a recorded step with no step asked for it: non-determinism). An agent
            // with sources answers an unknown tool inside the journaled step below, and replays. The
            // coder always installs sources, which is what makes ADR 0038's removal of a tool safe.
            None if self.sources.is_empty() => {
                let unknown = self.unknown_tool(&call.name);
                say(
                    ctx,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((&unknown, true)),
                        notes,
                    ),
                )
                .await;
                let answered = ToolResult::Answered {
                    message: Message::tool_error(call.id.clone(), unknown),
                    artifacts: Vec::new(),
                    answer: None,
                    repeat_key: None,
                    plan: None,
                    kept: None,
                    reported: Vec::new(),
                };
                return Ok((answered, SandboxUsage::new()));
            }
            None => {
                let sources = self.sources.clone();
                let name = call.name.clone();
                let unknown = self.unknown_tool(&call.name);
                let source_limit = note
                    .and_then(|n| n.timeout_ms)
                    .map_or(DEFAULT_TOOL_TIME_LIMIT, |ms| {
                        Duration::from_millis(ms) + SOURCE_LIMIT_MARGIN
                    });
                let unjournaled = unjournaled.clone();
                ctx.step(&format!("tool:{}", call.id), move || async move {
                    if let Some((sources, source_ctx, name)) = &holding
                        && let Some(held) = withheld(sources, source_ctx, &[name.as_str()])
                            .await
                            .into_iter()
                            .next()
                    {
                        return Ok(ToolOutput::error(held.reason));
                    }
                    let tool_name = name.clone();
                    let called = within(&tool_ctx, &name, source_limit, |ctx| async move {
                        for source in &sources {
                            if let Some(answer) = source.call(&ctx, &tool_name, args.clone()).await
                            {
                                return answer;
                            }
                        }
                        Ok(ToolOutput::error(unknown))
                    })
                    .await;
                    with_reported_sandbox(&tool_ctx, &unjournaled, called)
                })
                .await?
            }
        };

        let reported = match &outcome {
            Ok(output) => output.sandbox.clone(),
            // Tried again: only the try that is kept counts.
            Err(ToolError::Transient(_)) => SandboxUsage::new(),
            Err(_) => unjournaled
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        };
        let answered = match outcome {
            Ok(mut output) => {
                let repeat_key = output.repeat_key.as_deref().cloned();
                let reported = if output.is_error {
                    Vec::new()
                } else {
                    std::mem::take(&mut output.reported)
                };
                let (message, artifacts, answer, plan, outcome, kept) = output_message(
                    ctx,
                    &call.id,
                    &call.name,
                    self.pages(&call.name),
                    output,
                    files_used,
                )
                .await;
                // What the step says is what the model is told (the refusal of a file included).
                let said = match &message {
                    Message::Tool {
                        content, is_error, ..
                    } => Some((content.as_str(), *is_error)),
                    _ => None,
                };
                say(
                    ctx,
                    silent,
                    self.step_end(&call.name, &call.id, outcome, said, notes),
                )
                .await;
                ToolResult::Answered {
                    message,
                    artifacts,
                    answer,
                    repeat_key,
                    plan,
                    kept,
                    reported,
                }
            }
            Err(ToolError::Permanent(reason)) => {
                say(
                    ctx,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((&reason, true)),
                        notes,
                    ),
                )
                .await;
                ToolResult::Answered {
                    message: Message::tool_error(
                        call.id.clone(),
                        backstop(&reason, &call.name, false).into_owned(),
                    ),
                    artifacts: Vec::new(),
                    answer: None,
                    repeat_key: None,
                    plan: None,
                    kept: None,
                    reported: Vec::new(),
                }
            }
            Err(ToolError::Transient(reason)) => {
                say(
                    ctx,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((&reason, true)),
                        notes,
                    ),
                )
                .await;
                return Err(AgentError::transient(format!(
                    "tool `{}` failed: {reason}",
                    call.name
                )));
            }
            Err(ToolError::NeedsInput { question, ui }) => {
                say(ctx, silent, end(StepState::Waiting)).await;
                ToolResult::NeedsInput { question, ui }
            }
            // No end event yet: `run_pending` says "waiting", or the final state when the
            // child's message is already here.
            Err(ToolError::AwaitRun { run }) => ToolResult::AwaitRun(run),
            Err(ToolError::AwaitSession { session }) => ToolResult::AwaitSession(session),
            Err(ToolError::AwaitRemote { task, timeout_ms }) => {
                ToolResult::AwaitRemote { task, timeout_ms }
            }
        };
        Ok((answered, reported))
    }

    /// The note the listing that offered tool `name` made about it, if the tool is a source's (the
    /// agent's own tools win a name clash) and the listing said something.
    fn source_note<'n>(&self, notes: &'n [ToolNote], name: &str) -> Option<&'n ToolNote> {
        if self.tool(name).is_some() {
            return None;
        }
        notes.iter().find(|n| n.tool == name)
    }

    /// The context a tool sees for the call `call_id`, made from this transition's `Ctx` and the
    /// run's inbound `context`.
    fn tool_ctx(
        &self,
        ctx: &Ctx,
        context: &Map<String, Value>,
        (root_run, root_step, plan): (Option<RunId>, Option<&str>, Option<&Plan>),
        notes: &[ToolNote],
        call_id: &str,
        tool: &str,
    ) -> ToolCtx {
        ToolCtx::new(
            ctx.conversation_id().map(str::to_owned),
            ctx.attempt(),
            call_id.to_owned(),
            tool.to_owned(),
            ctx.emitter(),
            ctx.cancel_token(),
            Arc::clone(&self.extensions),
            Some(ctx.child_starter()),
            Arc::new(context.clone()),
            root_run,
            root_step.map(str::to_owned),
            self.style_of(tool, notes)
                .event(tool, call_id, StepState::Running),
        )
        .with_plan(plan.cloned())
    }

    /// How a call of the tool `name` is drawn as a step: the tool's own [`Tool::step_style`]; for a
    /// source's tool, the default with the label its listing gave (`notes`, [`ToolNote::label`]); for
    /// a name nobody has, the default.
    fn style_of(&self, name: &str, notes: &[ToolNote]) -> StepStyle {
        match self.tool(name) {
            Some(tool) => tool.step_style(),
            None => match self
                .source_note(notes, name)
                .and_then(|n| n.label.as_deref())
            {
                Some(label) => StepStyle::default().with_label(label),
                None => StepStyle::default(),
            },
        }
    }

    /// The report that the call `call_id` of the tool `tool` is in `state`.
    fn step_event(
        &self,
        tool: &str,
        call_id: &str,
        state: StepState,
        notes: &[ToolNote],
    ) -> RunEvent {
        RunEvent::Step(self.style_of(tool, notes).event(tool, call_id, state))
    }

    /// The report that ends the step of the call `call_id` of the tool `tool` in `state`, with what
    /// the call answered: `result` is the text of the answer and whether it is an error. The
    /// agent's [`StepIo`] scrubs and cuts it (or leaves it out).
    fn step_end(
        &self,
        tool: &str,
        call_id: &str,
        state: StepState,
        result: Option<(&str, bool)>,
        notes: &[ToolNote],
    ) -> RunEvent {
        let mut step = self.style_of(tool, notes).event(tool, call_id, state);
        if let Some((text, error)) = result
            && let Some(output) = self.step_io.output(text, error)
        {
            step = step.with_output(output);
        }
        RunEvent::Step(step)
    }

    /// The report that ends the step of the call `call_id` of the tool `tool` in `state`, with the
    /// result `state` has recorded for that call, when it has one (the answer of the person, a child
    /// run's outcome, a remote task's).
    fn step_end_with_result(
        &self,
        conversation: &Conversation,
        tool: &str,
        call_id: &str,
        state: StepState,
    ) -> RunEvent {
        let result = conversation.messages.iter().rev().find_map(|m| match m {
            Message::Tool {
                call_id: id,
                content,
                is_error,
            } if id == call_id => Some((content.as_str(), *is_error)),
            _ => None,
        });
        self.step_end(tool, call_id, state, result, &conversation.source_notes)
    }

    /// Why a run that reached `limit` fails when its closing turn says nothing.
    fn limit_message(&self, state: &Conversation, limit: LimitKind) -> String {
        match limit {
            LimitKind::MaxToolCalls => format!(
                "tool call limit exceeded: {} calls already made (max_tool_calls = {})",
                state.tool_calls, self.limits.max_tool_calls
            ),
            LimitKind::MaxTurns => format!(
                "turn limit exceeded: the model was called {} times (max_turns = {})",
                state.turns, self.limits.max_turns
            ),
            LimitKind::RepeatedCall => "the same tool call was repeated with the same result \
                 and the run was stopped (doom_loop)"
                .to_owned(),
        }
    }

    /// What the model is told when it calls a tool nobody has.
    fn unknown_tool(&self, name: &str) -> String {
        let known: Vec<&str> = self.specs.iter().map(|s| s.name.as_str()).collect();
        format!(
            "unknown tool `{name}`; available tools: {}",
            if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            }
        )
    }

    /// Where the remote task a run waits for stands. `Ok(None)`: still going, park again.
    /// `Ok(Some(outcome))`: the call is answered (the result is in the state) and `outcome` is the
    /// state the end of the call's step carries.
    ///
    /// The look is one journaled step (`poll:<call id>`), so a replay of this transition sees what
    /// the first run saw. When the wait has a deadline the clock is read through the journal too
    /// (`ctx.now`): a replay must take the same branch, or the steps after it would not line up.
    async fn poll_remote(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        wait: &PendingRemote,
    ) -> Result<Option<StepState>, AgentError> {
        let owed = |state: &mut Conversation, message: Message| {
            state.messages.push(message);
            if state
                .pending_calls
                .first()
                .is_some_and(|c| c.id == wait.call_id)
            {
                state.pending_calls.remove(0);
            }
            state.pending_wait = None;
        };
        if let Some(deadline) = wait.deadline
            && ctx.now_journaled().await? >= deadline
        {
            owed(
                state,
                Message::tool_error(
                    wait.call_id.clone(),
                    "the remote task did not finish in the time allowed; it may still be \
                     running there, and its result is lost to this call",
                ),
            );
            return Ok(Some(StepState::Failed));
        }
        let Some(tool) = self.tool(&wait.tool).cloned() else {
            owed(
                state,
                Message::tool_error(
                    wait.call_id.clone(),
                    format!("the tool `{}` that started this task is gone", wait.tool),
                ),
            );
            return Ok(Some(StepState::Failed));
        };
        let tool_ctx = self
            .tool_ctx(
                ctx,
                &state.context,
                (
                    state.root_run,
                    state.root_step.as_deref(),
                    state.plan.as_ref(),
                ),
                &state.source_notes,
                &wait.call_id,
                &wait.tool,
            )
            .with_files_left(
                (MAX_RUN_FILE_BYTES as u64).saturating_sub(files_kept(&state.artifacts)),
            )
            .with_kept(
                MAX_RUN_KEPT_BYTES.saturating_sub(state.kept_bytes()),
                Arc::clone(&state.kept_outputs),
            );
        let task = wait.task.clone();
        let polled: Result<RemotePoll, ToolError> = ctx
            .step(&format!("poll:{}", wait.call_id), move || async move {
                tool.poll_remote(&tool_ctx, &task).await
            })
            .await?;
        match polled {
            Ok(RemotePoll::Working) => Ok(None),
            Ok(RemotePoll::Ready(mut output)) => {
                let reported = if output.is_error {
                    Vec::new()
                } else {
                    std::mem::take(&mut output.reported)
                };
                if !reported.is_empty() {
                    Self::count_rounds(ctx, state, false).await?;
                }
                let used = files_kept(&state.artifacts);
                let (message, artifacts, answer, plan, outcome, kept) = output_message(
                    ctx,
                    &wait.call_id,
                    &wait.tool,
                    self.pages(&wait.tool),
                    output,
                    used,
                )
                .await;
                add_artifact_refs(&mut state.artifacts, artifacts);
                state.keep_output(kept);
                if let Some(answer) = answer {
                    state.announce(answer);
                }
                if let Some(plan) = plan {
                    state.plan = Some(*plan);
                }
                owed(state, message);
                Ok(Some(outcome))
            }
            Err(ToolError::Transient(reason)) => Err(AgentError::transient(format!(
                "tool `{}` failed while polling its remote task: {reason}",
                wait.tool
            ))),
            Err(ToolError::Permanent(reason)) => {
                owed(
                    state,
                    Message::tool_error(
                        wait.call_id.clone(),
                        backstop(&reason, &wait.tool, false).into_owned(),
                    ),
                );
                Ok(Some(StepState::Failed))
            }
            Err(other) => {
                owed(
                    state,
                    Message::tool_error(
                        wait.call_id.clone(),
                        format!("polling the remote task went wrong: {other}"),
                    ),
                );
                Ok(Some(StepState::Failed))
            }
        }
    }
}

/// `new` added to `refs`: after the others, except one with an id that one of them has, which
/// takes its place where it stands (the run holds one artifact for an id, see
/// [`Artifact::id`](adam_runtime::Artifact::id)). A reference to a file (it has `bytes`) is never
/// replaced and never replaces: the runtime keeps both, so the bytes count once each.
fn add_artifact_refs(refs: &mut Vec<ArtifactRef>, new: Vec<ArtifactRef>) {
    for mut one in new {
        if one.bytes.is_some() {
            one.id = None;
        }
        match one.id.as_ref().and_then(|id| {
            refs.iter_mut()
                .find(|r| r.bytes.is_none() && r.id.as_ref() == Some(id))
        }) {
            Some(held) => *held = one,
            None => refs.push(one),
        }
    }
}

/// The bytes of the files the run has shared so far, by the references the state keeps.
fn files_kept(artifacts: &[ArtifactRef]) -> u64 {
    artifacts.iter().filter_map(|a| a.bytes).sum()
}

/// A tool's output as the message that answers `call_id`, with the references to its artifacts, the
/// words it announced as the run's answer (only from a result that is not an error, the refusal of a
/// file included) and the state the end of the call's step carries. The artifacts are emitted here (and recorded with the
/// transition's commit).
///
/// **Files are bounded per run.** The files of a run are journaled with it, so the run keeps at most
/// [`MAX_RUN_FILE_BYTES`] of them: `files_used` is what it kept so far, and a file that would go over is
/// not emitted. The model is told so in the tool's result (marked as an error), which is also where it
/// learns what was shared: the result says nothing of a file's bytes, so they are never in the history.
async fn output_message(
    ctx: &Ctx,
    call_id: &str,
    tool: &str,
    paged: bool,
    mut output: ToolOutput,
    files_used: u64,
) -> (
    Message,
    Vec<ArtifactRef>,
    Option<Announcement>,
    Option<Box<Plan>>,
    StepState,
    Option<Box<KeptOutput>>,
) {
    let mut used = files_used;
    let mut refused: Vec<String> = Vec::new();
    output.artifacts.retain(|a| {
        let len = a.file_len() as u64;
        if len == 0 {
            return true;
        }
        if used + len > MAX_RUN_FILE_BYTES as u64 {
            refused.push(a.name.clone());
            return false;
        }
        used += len;
        true
    });
    if !refused.is_empty() {
        output.is_error = true;
        output.content.push_str(&format!(
            "\nNot shared: {}. A run may share at most {} MiB of files in all, and it has shared \
             {} KiB so far.",
            refused.join(", "),
            MAX_RUN_FILE_BYTES / (1024 * 1024),
            files_used / 1024
        ));
    }
    let outcome = if output.is_error {
        StepState::Failed
    } else {
        StepState::Completed
    };
    let refs: Vec<ArtifactRef> = output
        .artifacts
        .iter()
        .map(|a| ArtifactRef {
            id: a.id.clone().filter(|_| a.file.is_none()),
            name: a.name.clone(),
            mime_type: a.mime_type.clone(),
            bytes: a.file.as_ref().map(|f| f.bytes.len() as u64),
        })
        .collect();
    for artifact in output.artifacts {
        ctx.emit(RunEvent::from(artifact)).await;
    }
    let ends_turn = output.ends_turn;
    let kept = output.kept.take().filter(|_| !output.is_error);
    let answer = output
        .answer
        .take()
        .filter(|_| !output.is_error)
        .map(|text| Announcement { text, ends_turn });
    let plan = output.plan.take().filter(|_| !output.is_error);
    let message = Message::Tool {
        call_id: call_id.to_owned(),
        // After the journal: the journal holds the whole output, and the cut (which the history, the
        // model and the step event carry) is a pure function of it.
        content: backstop(&output.content, tool, paged).into_owned(),
        is_error: output.is_error,
    };
    (message, refs, answer, plan, outcome, kept)
}

/// The first user message of a run, as a fresh [`Conversation`]. Shared by
/// [`LlmAgent::init`] and [`LlmStarter::init`], so they cannot drift apart.
fn start_conversation(input: Inbound) -> Result<Conversation, AgentError> {
    let text = start_text(&input)?;
    let mut conversation = Conversation::new(text);
    conversation.root_run = parse_root_run(&input.payload);
    conversation.root_step = parse_root_step(&input.payload);
    conversation.session = parse_session(&input.payload);
    if let Some(context) = parse_context(&input.payload) {
        conversation.merge_context(context);
    }
    Ok(conversation)
}

/// The user message of a new run, continuing or not: one rule for what is accepted and how a
/// refusal reads.
fn start_text(input: &Inbound) -> Result<String, AgentError> {
    parse_user_text(&input.payload)
        .map_err(|reason| AgentError::permanent(format!("unusable start message: {reason}")))
}

/// The conversation of a run that continues `prior_run` (whose last state is `prior`) with the
/// user message of `input`: [`Conversation::continued`]. Shared by [`LlmAgent::init_continuing`]
/// and [`LlmStarter::init_continuing`], so the front that starts the run and the worker that
/// would have cannot disagree on what a continuation is.
fn continue_conversation(
    input: &Inbound,
    prior: &Conversation,
    prior_run: RunId,
) -> Result<Conversation, AgentError> {
    // A round that only closes its session has no message: it carries the conversation as it is.
    // It keeps only the last answer, which it ends with again, so that a report read after the
    // close is still the last one.
    if asks_to_close(&input.payload) {
        let last = prior
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m, Message::Assistant { .. }))
            .cloned();
        return Ok(Conversation {
            messages: last.into_iter().collect(),
            continued_from: Some(prior_run),
            context: prior.context.clone(),
            plan: prior.plan.clone(),
            session: parse_session(&input.payload).or_else(|| prior.session.clone()),
            ..Conversation::default()
        });
    }
    let text = start_text(input)?;
    let mut conversation = prior.continued(text, prior_run);
    // A round of a session works for the run that started it, as the first one did, and says what
    // the session is.
    conversation.root_run = parse_root_run(&input.payload);
    conversation.root_step = parse_root_step(&input.payload);
    if let Some(header) = parse_session(&input.payload) {
        conversation.session = Some(header);
    }
    // The context of the run before comes along; what this message says about its sender is
    // newer, and replaces it key by key.
    if let Some(context) = parse_context(&input.payload) {
        conversation.merge_context(context);
    }
    Ok(conversation)
}

/// The start-only half of an [`LlmAgent`]: it turns the first user message
/// into the [`Conversation`] a run starts with, and needs neither a model nor
/// tools.
///
/// Register it with `RuntimeBuilder::starter` on a process that only accepts
/// requests, under the name of the [`LlmAgent`] that steps the runs on a
/// worker. `init` accepts exactly what [`LlmAgent::init`] accepts (payload
/// `{"text": "..."}` or a bare JSON string) and rejects what it rejects. So does
/// `init_continuing`, which carries the prior run's conversation over exactly as
/// [`LlmAgent::init_continuing`] does ([`Conversation::continued`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmStarter {
    name: String,
}

impl LlmStarter {
    /// A starter for the agent called `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl AgentStarter for LlmStarter {
    type State = Conversation;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Conversation, AgentError> {
        start_conversation(input)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Conversation,
        prior_run: RunId,
    ) -> Result<Conversation, AgentError> {
        continue_conversation(&input, prior, prior_run)
    }
}

/// What a tool announced as the run's answer, and whether it ended the turn with it.
struct Announcement {
    text: String,
    ends_turn: bool,
}

impl Conversation {
    /// Keep an announcement: the last one wins, and it ends the turn only if it says so.
    fn announce(&mut self, announcement: Announcement) {
        self.announced = Some(announcement.text);
        self.announced_final = announcement.ends_turn;
    }
}

/// The result of a call with what the call reported as sandbox usage: in the result of a call that
/// returned one, which the step journals, and else in `unjournaled`.
fn with_reported_sandbox(
    tool_ctx: &ToolCtx,
    unjournaled: &Mutex<SandboxUsage>,
    result: Result<ToolOutput, ToolError>,
) -> Result<ToolOutput, ToolError> {
    let reported = tool_ctx.reported_sandbox();
    match result {
        Ok(mut output) => {
            output.sandbox.merge(&reported);
            Ok(output)
        }
        Err(error) => {
            unjournaled
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .merge(&reported);
            Err(error)
        }
    }
}

/// What a journaled step read of a round this run started, to count its tokens.
#[derive(Serialize, Deserialize)]
enum RoundUsage {
    /// The round still works: nothing to count yet.
    Working,
    /// The round is not there (a call that started none, or one purged): nothing to count.
    Gone,
    /// The round has ended: what its state kept.
    Counted {
        totals: Option<UsageTotals>,
        sandbox: Option<SandboxUsage>,
    },
}

enum ToolResult {
    Answered {
        message: Message,
        artifacts: Vec<ArtifactRef>,
        /// What the tool announced as the run's answer, from a result that is not an error.
        answer: Option<Announcement>,
        /// What the tool said makes this answer equal to another of the same call (`doom_loop`).
        repeat_key: Option<String>,
        /// The plan the tool returned, from a result that is not an error.
        plan: Option<Box<Plan>>,
        /// A long output the tool asks the run to keep in its state.
        kept: Option<Box<KeptOutput>>,
        /// The rounds of sessions whose reports the result gives, from a result that is not an
        /// error.
        reported: Vec<RunId>,
    },
    NeedsInput {
        question: String,
        ui: Option<Value>,
    },
    AwaitRun(RunId),
    AwaitSession(RunId),
    AwaitRemote {
        task: String,
        timeout_ms: Option<u64>,
    },
}

/// What a source's call may run beyond the `timeout_ms` its listing gave the tool: the time the
/// source itself needs to answer after its own limit.
const SOURCE_LIMIT_MARGIN: Duration = Duration::from_secs(10);

/// What the model is told when a call ran past its time limit.
fn timed_out(tool: &str, limit: Duration) -> String {
    format!(
        "`{tool}` did not finish within {} and was stopped. What it had done is not undone: look \
         at the state again before relying on it, and try a smaller step.",
        if limit.subsec_nanos() == 0 {
            format!("{} s", limit.as_secs())
        } else {
            format!("{} ms", limit.as_millis())
        }
    )
}

/// Run a call of a tool within `limit`. The call gets a token of its own, a child of the run's: when
/// the limit passes the token fires, the tool has [`TOOL_STOP_GRACE`] to stop, and then its future
/// is dropped. The answer is an error result, unless the tool finished with a real (not an error)
/// result before that, at the instant of the limit or while it was told to stop. It runs inside the
/// journaled step, so a replay reads the recorded answer and never waits, and no clock is read in a
/// transition.
async fn within<F, Fut>(
    tool_ctx: &ToolCtx,
    name: &str,
    limit: Duration,
    call: F,
) -> Result<ToolOutput, ToolError>
where
    F: FnOnce(ToolCtx) -> Fut,
    Fut: std::future::Future<Output = Result<ToolOutput, ToolError>>,
{
    let token = tool_ctx.cancel_token().child();
    let fut = call(tool_ctx.clone().with_cancel_token(token.clone()));
    tokio::pin!(fut);
    tokio::select! {
        // Biased toward the tool: a call that finishes at the very instant the timer fires has
        // finished, and its result is the one that counts.
        biased;
        out = &mut fut => out,
        () = tokio::time::sleep(limit) => {
            token.cancel();
            match tokio::time::timeout(TOOL_STOP_GRACE, &mut fut).await {
                // It finished by itself while it was being told to stop, with a result and not an
                // error: that result is what happened (a pull request that was opened must not be
                // reported as stopped, or the model may open another).
                Ok(Ok(output)) if !output.is_error => Ok(output),
                _ => Ok(ToolOutput::error(timed_out(name, limit))),
            }
        }
    }
}

/// The call that is the third in a row with the same result is refused after this many that ran.
const DOOM_LOOP_AFTER: u32 = 2;

/// The refusals after which a run is ended with the closing turn (`limit: "repeated_call"`).
const DOOM_LOOP_REFUSALS: u32 = 2;

/// What the model is told when it makes the third identical call in a row.
fn repeated_call(tool: &str) -> String {
    format!(
        "Not run: this is the third call in a row of `{tool}` with the same arguments, and the last \
         two gave the same result. Calling it again will not change it. Change your approach: other \
         arguments, another tool, or answer with what you know."
    )
}

/// FNV-1a over the parts, each followed by a zero byte: stable across processes and versions
/// (`DefaultHasher` is not), which a hash kept in the durable state must be.
fn fnv_hex(parts: &[&str]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// `value` as text with the keys of every object in order, whatever order the map keeps them in.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The tool name and the canonical arguments of a call, hashed (the raw text for arguments that
/// were not JSON).
fn call_key(call: &ToolCall) -> String {
    match &call.invalid_arguments {
        Some(raw) => fnv_hex(&[&call.name, "raw", raw]),
        None => {
            let mut text = String::new();
            canonical(&call.arguments, &mut text);
            fnv_hex(&[&call.name, "json", &text])
        }
    }
}

/// Count the answer of a call that ran: one more in a row when its key and its result are those of
/// the call before, a new run of one otherwise.
fn record_answer(
    state: &mut Conversation,
    key: String,
    message: &Message,
    repeat_key: Option<String>,
) {
    let Message::Tool {
        content, is_error, ..
    } = message
    else {
        state.repeats = None;
        return;
    };
    let answer = fnv_hex(&[if *is_error { "error" } else { "ok" }, content]);
    let failed = u32::from(*is_error);
    match &mut state.repeats {
        Some(repeats) if repeats.key == key => {
            // Same call again: the same answer counts up; a different one starts the count over. A
            // call that errs again counts as a failure in a row whatever the error says (a result
            // that carries a duration or a path never repeats to the byte).
            if repeats.answer == answer {
                repeats.count += 1;
            } else {
                repeats.count = 1;
                repeats.answer = answer;
            }
            repeats.failing = if *is_error { repeats.failing + 1 } else { 0 };
            // The same repeat key again counts up whatever the text says.
            repeats.marked = match (&repeats.marked_with, &repeat_key) {
                (Some(before), Some(now)) if before == now => repeats.marked + 1,
                (_, Some(_)) => 1,
                (_, None) => 0,
            };
            repeats.marked_with = repeat_key;
        }
        slot => {
            *slot = Some(Repeats {
                key,
                answer,
                count: 1,
                refused: 0,
                failing: failed,
                marked: u32::from(repeat_key.is_some()),
                marked_with: repeat_key,
            });
        }
    }
}

/// The output of a run that ended at a limit, with the answer of its closing turn.
fn limited_output(
    state: &Conversation,
    limit: LimitKind,
    text: String,
    truncated: bool,
    stream: Option<String>,
) -> Value {
    let mut output = json!({
        "text": text,
        "artifacts": state.artifacts,
        "limited": true,
        "limit": limit.as_str(),
    });
    if truncated {
        output["truncated"] = json!(true);
    }
    if let Some(stream) = stream {
        output["stream"] = json!(stream);
    }
    output
}

/// The longest part of the text of an invalid call that its error quotes.
const INVALID_ARGUMENTS_QUOTE: usize = 2048;

/// What the model is told when the arguments of its call were not JSON.
fn invalid_arguments(name: &str, raw: &str, output_cut: bool) -> String {
    let why = serde_json::from_str::<Value>(raw)
        .err()
        .map_or_else(String::new, |e| format!(": {e}"));
    let mut end = raw.len().min(INVALID_ARGUMENTS_QUOTE);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    let more = if end < raw.len() { " ..." } else { "" };
    let cut = if output_cut {
        " The reply was cut at the output limit, so the arguments are probably incomplete: send less in one call."
    } else {
        ""
    };
    format!(
        "the arguments of `{name}` are not valid JSON{why}. The call did not run. \
         What you sent: {}{more}{cut}",
        &raw[..end]
    )
}

/// A child's output as the text of the tool result. An `LlmAgent` child finishes with
/// `{"text": ..., "artifacts": [...]}` and the parent's model gets the text (the same reading the
/// A2A adapter has of an output); another string is passed as it is, and anything else as JSON.
pub(crate) fn render_output(output: Option<&Value>) -> String {
    let text = match output {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) if matches!(o.get("text"), Some(Value::String(_))) => o
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some(other) => other.to_string(),
    };
    if text.trim().is_empty() {
        return "(the run finished without output)".to_owned();
    }
    let text = backstop(&text, "", false).into_owned();
    // A child that ran out of turns or tool calls still answered, from its closing turn.
    let limited = output.and_then(|o| {
        (o.get("limited") == Some(&Value::Bool(true)))
            .then(|| o.get("limit").and_then(Value::as_str))
            .flatten()
    });
    match limited {
        Some(limit) => format!(
            "{text}\n\n(the run stopped at its `{limit}` limit; this answer may be incomplete)"
        ),
        None => text,
    }
}

#[async_trait]
impl Agent for LlmAgent {
    type State = Conversation;

    fn name(&self) -> &str {
        &self.name
    }

    /// The input is the first user message: payload `{"text": "..."}` or a bare
    /// JSON string (see [`user_message`](crate::user_message)); its `kind` is
    /// not inspected. The same as [`LlmStarter::init`].
    fn init(&self, input: Inbound) -> Result<Conversation, AgentError> {
        start_conversation(input)
    }

    /// The prior run's conversation with the input as its newest user message, see
    /// [`Conversation::continued`] for what is carried, dropped and reset. The same as
    /// [`LlmStarter::init_continuing`].
    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Conversation,
        prior_run: RunId,
    ) -> Result<Conversation, AgentError> {
        continue_conversation(&input, prior, prior_run)
    }

    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), agent = %self.name))]
    async fn step(
        &self,
        ctx: &mut Ctx,
        mut state: Conversation,
    ) -> Result<Transition<Conversation>, AgentError> {
        // A message that arrives while this step finishes the run is read, not dropped: see
        // `Ctx::reopen_on_arrival`.
        ctx.reopen_on_arrival();
        // A round that only closes its session calls no model: it ends with the last answer again.
        if state.session.as_ref().is_some_and(|h| h.closing) {
            let text = state
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m, Message::Assistant { .. }))
                .map(Message::text)
                .unwrap_or_default();
            let output = json!({ "text": text, "artifacts": [], "closed": true });
            return Ok(Transition::Done { state, output });
        }
        let (finished, inbox): (Vec<_>, Vec<_>) = ctx
            .take_inbox()
            .into_iter()
            // A session round's report (ADR 0045) is a wake-up, not a person's words: it is
            // dropped, never shown to the model, and the report is read from the store.
            .filter(|i| i.kind != CHILD_REPORTED_KIND)
            .partition(|i| i.kind == RUN_FINISHED_KIND);
        // A request to close the session this round belongs to: the round goes on and finishes its
        // work, and the session takes no more messages after it.
        let (closes, inbox): (Vec<_>, Vec<_>) = inbox
            .into_iter()
            .partition(|i| i.kind == SESSION_CLOSE_KIND);
        if !closes.is_empty()
            && let Some(header) = &mut state.session
        {
            header.closed = true;
        }
        let notices: Vec<(RunId, ChildStatus)> = finished
            .iter()
            .filter_map(|i| {
                let notice = ChildStatus::from_notice(i);
                if notice.is_none() {
                    tracing::warn!(inbound = %i.id, "ignoring an unreadable run.finished message");
                }
                notice
            })
            .collect();
        let asked = match &state.pending_wait {
            Some(PendingWait::Question(q)) => Some((q.tool.clone(), q.call_id.clone())),
            _ => None,
        };
        if !self.absorb_inbox(&mut state, inbox) {
            // Woken without an answer (nothing usable arrived): keep waiting.
            return Ok(Transition::Park {
                state,
                wake_at: None,
            });
        }
        // The person answered: the step of the call that asked, which has been `waiting` since, ends.
        if let Some((tool, call_id)) = asked
            && state.pending_wait.is_none()
        {
            ctx.emit(self.step_end_with_result(&state, &tool, &call_id, StepState::Completed))
                .await;
        }
        // A credential in the context does not outlive its expiry in the store.
        let expired = state.drop_expired_context(ctx.now());
        if expired > 0 {
            tracing::debug!(expired, "expired entries left the run's context");
        }

        // A wait on a child ends with the child's message or, when woken without one (the timer,
        // or a user message), with what the store says. A message that matches no wait (a
        // duplicate of one already used, or one for a call the run is not on) is dropped here:
        // the wait it was for has been settled, or will be, from the store.
        if let Some(PendingWait::Run(wait)) = state.pending_wait.clone() {
            match Self::settle(ctx, &wait, &notices).await? {
                Some(child) => {
                    let outcome = Self::answer_run(&mut state, &wait, &child);
                    Self::count_child(ctx, &mut state, wait.run).await?;
                    ctx.emit(self.step_end_with_result(&state, &wait.tool, &wait.call_id, outcome))
                        .await;
                }
                None => {
                    let wake_at = Some(self.next_poll(ctx));
                    return Ok(Transition::Park { state, wake_at });
                }
            }
        }

        // A wait on a remote task ends when the tool says the task is over. Woken by the timer
        // (or by a user message, which queues behind the result), the run looks once and either
        // answers the call or parks again.
        if let Some(PendingWait::Remote(wait)) = state.pending_wait.clone() {
            match self.poll_remote(ctx, &mut state, &wait).await? {
                Some(outcome) => {
                    ctx.emit(self.step_end_with_result(&state, &wait.tool, &wait.call_id, outcome))
                        .await;
                }
                None => {
                    let wake_at = Some(self.next_poll(ctx));
                    return Ok(Transition::Park { state, wake_at });
                }
            }
        }

        // A wait on a session ends when its round has: the round's report woke the run, or the timer
        // did, and either way the tool says whether the report is there (the same look as a remote
        // task's, with the session as the task).
        if let Some(PendingWait::Session(wait)) = state.pending_wait.clone() {
            let looking = PendingRemote {
                call_id: wait.call_id,
                tool: wait.tool,
                task: wait.session.to_string(),
                deadline: None,
            };
            match self.poll_remote(ctx, &mut state, &looking).await? {
                Some(outcome) => {
                    ctx.emit(self.step_end_with_result(
                        &state,
                        &looking.tool,
                        &looking.call_id,
                        outcome,
                    ))
                    .await;
                }
                None => {
                    let wake_at = Some(self.next_poll(ctx));
                    return Ok(Transition::Park { state, wake_at });
                }
            }
        }

        // Owed tool results (the run was parked on a question) come first;
        // otherwise this step is a fresh model turn.
        let mut flow = Flow::Next;
        if state.pending_calls.is_empty() {
            state.messages.append(&mut state.deferred);
            flow = self.model_turn(ctx, &mut state).await?;
            // A call that starts, sends to or closes a session has an effect no failed try can undo:
            // the turn and its calls are committed first, so a retry never asks the model again
            // (new call ids, so a second session) and the call runs under the id the history holds.
            if matches!(flow, Flow::Next) && state.pending_calls.iter().any(|c| has_effect(&c.name))
            {
                return Ok(Transition::Continue(state));
            }
        }
        if matches!(flow, Flow::Next) {
            flow = self.run_pending(ctx, &mut state, &notices).await?;
        }
        Ok(match flow {
            Flow::Next => {
                if state.pending_calls.is_empty() {
                    // A tool handed over the answer and ended the turn with it
                    // (`ToolOutput::final_answer`): the calls it was asked with are answered, so
                    // the run is done, with no model call to say a closing line after it. A
                    // message that reached the run meanwhile is read first, by the model.
                    if let Some(text) = state.announced.clone()
                        && state.announced_final
                        && state.deferred.is_empty()
                        && ctx.arrived().await? == 0
                    {
                        let mut output = json!({ "text": text, "artifacts": state.artifacts });
                        self.end_sessions(ctx, &mut state, &mut output).await?;
                        return Ok(Transition::Done { state, output });
                    }
                    state.announced_final = false;
                    state.messages.append(&mut state.deferred);
                }
                Transition::Continue(state)
            }
            Flow::Park => Transition::Park {
                state,
                wake_at: None,
            },
            Flow::Wait => Transition::Park {
                wake_at: Some(self.next_poll(ctx)),
                state,
            },
            Flow::Done(mut output) => {
                self.end_sessions(ctx, &mut state, &mut output).await?;
                Transition::Done { state, output }
            }
            Flow::Fail(error) => Transition::Fail { state, error },
        })
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn a_child_answers_with_its_text() {
        assert_eq!(
            render_output(Some(&json!({"text": "approved", "artifacts": []}))),
            "approved"
        );
        assert_eq!(render_output(Some(&json!("plain"))), "plain");
        // Anything else is the model's to read as it is: its JSON.
        let other = json!({"verdict": "ok", "n": 2});
        let rendered = render_output(Some(&other));
        assert_eq!(serde_json::from_str::<Value>(&rendered).unwrap(), other);
        assert_eq!(render_output(Some(&json!([1, 2]))), "[1,2]");
        // A `text` that is not a string is not a text.
        assert_eq!(render_output(Some(&json!({"text": 3}))), r#"{"text":3}"#);
    }

    #[test]
    fn no_output_is_said_not_left_empty() {
        for none in [
            None,
            Some(&Value::Null),
            Some(&json!("")),
            Some(&json!({"text": "  "})),
        ] {
            assert_eq!(render_output(none), "(the run finished without output)");
        }
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("connection reset")]
    struct Reset;

    #[test]
    fn a_journaled_failure_keeps_the_class_the_hint_and_the_whole_chain() {
        let limited = ModelFailure::from(ModelError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        });
        assert!(limited.retryable);
        assert_eq!(limited.retry_after_ms, Some(30_000));
        assert_eq!(limited.class.as_deref(), Some("RateLimited"));

        // The journal is a boundary: the cause is flattened into the message, once.
        let transient =
            ModelFailure::from(ModelError::transient("connection failed").with_source(Reset));
        assert!(transient.retryable);
        assert_eq!(transient.retry_after_ms, None);
        assert_eq!(
            transient.message,
            "transient model error: connection failed: connection reset"
        );

        let auth = ModelFailure::from(ModelError::Auth("bad key".into()));
        assert!(!auth.retryable);
        assert_eq!(auth.class.as_deref(), Some("Unauthenticated"));
    }

    /// A model call recorded before words could be streamed is a bare response: it reads as a call
    /// whose words were not streamed; one recorded now carries the stream, and says nothing of it when
    /// there is none.
    #[test]
    fn a_recorded_model_call_reads_a_journal_from_before_streaming() {
        let bare = serde_json::to_value(ModelResponse::text("hi")).unwrap();
        let old: Recorded = serde_json::from_value(bare.clone()).unwrap();
        assert_eq!(old.response, ModelResponse::text("hi"));
        assert_eq!(old.stream, None);

        let streamed = Recorded {
            response: ModelResponse::text("hi"),
            stream: Some("run-m0-a1b2c3d4".into()),
            notes: Vec::new(),
            seen: None,
            call: None,
        };
        let json = serde_json::to_value(&streamed).unwrap();
        assert_eq!(json["stream"], "run-m0-a1b2c3d4");
        assert_eq!(
            json["message"], bare["message"],
            "the response's members, at the top"
        );
        let back: Recorded = serde_json::from_value(json).unwrap();
        assert_eq!(back.stream.as_deref(), Some("run-m0-a1b2c3d4"));
        assert_eq!(back.response, ModelResponse::text("hi"));

        // Not streamed: the record is the bare response, byte for byte, as a build without streaming
        // would have written it (a rolling deploy reads it either way).
        let plain = Recorded {
            response: ModelResponse::text("hi"),
            stream: None,
            notes: Vec::new(),
            seen: None,
            call: None,
        };
        assert_eq!(serde_json::to_value(&plain).unwrap(), bare);

        // With notes the record has them beside the response; an older reader of the record ignores
        // the member, and a record without it reads as no notes.
        let noted = Recorded {
            response: ModelResponse::text("hi"),
            stream: None,
            notes: vec![ToolNote::new("relay__search").reporting_steps()],
            seen: None,
            call: None,
        };
        let json = serde_json::to_value(&noted).unwrap();
        assert_eq!(
            json["notes"],
            json!([{"tool": "relay__search", "reports_step": true}])
        );
        let back: Recorded = serde_json::from_value(json).unwrap();
        assert_eq!(back.notes, noted.notes);
        assert!(old.notes.is_empty());

        // The call's id for its usage report is beside the response, and absent from an old record.
        assert_eq!(old.call, None);
        let reported = Recorded {
            response: ModelResponse::text("hi"),
            stream: None,
            notes: Vec::new(),
            seen: None,
            call: Some("run-c0-a1b2c3d4".into()),
        };
        let json = serde_json::to_value(&reported).unwrap();
        assert_eq!(json["call"], "run-c0-a1b2c3d4");
        let back: Recorded = serde_json::from_value(json).unwrap();
        assert_eq!(back.call.as_deref(), Some("run-c0-a1b2c3d4"));
    }

    #[test]
    fn a_call_id_names_the_run_the_turn_and_the_try() {
        let run = RunId::new();
        let (first, again) = (call_id(run, 3), call_id(run, 3));
        assert!(first.starts_with(&format!("{run}-c3-")), "{first}");
        assert_ne!(first, again, "a call made again is another call");
        assert!(first.len() <= adam_runtime::MAX_USAGE_CALL_BYTES);
        assert_eq!(old_call_id(run, 3), old_call_id(run, 3));
    }

    #[test]
    fn a_stream_id_names_the_run_the_turn_and_the_try() {
        let run = RunId::new();
        let (first, again) = (stream_id(run, 3), stream_id(run, 3));
        assert!(first.starts_with(&format!("{run}-m3-")), "{first}");
        assert_ne!(first, again, "a retry of the turn is another stream");
        assert!(first.len() <= adam_runtime::MAX_STREAM_ID_BYTES);
        assert!(first.is_ascii());
    }

    /// Journals written before hints and classes existed still decode.
    #[test]
    fn old_journal_records_decode() {
        let old: ModelFailure =
            serde_json::from_value(json!({"retryable": true, "message": "boom"})).unwrap();
        assert!(old.retryable);
        assert_eq!(old.retry_after_ms, None);
        assert_eq!(old.class, None);
        // And a new record round-trips.
        let new = ModelFailure::from(ModelError::transient("x"));
        let back: ModelFailure =
            serde_json::from_value(serde_json::to_value(&new).unwrap()).unwrap();
        assert_eq!(back.class, new.class);
    }
}

#[cfg(test)]
mod artifact_ref_tests {
    use super::*;

    fn reference(name: &str, id: Option<&str>, bytes: Option<u64>) -> ArtifactRef {
        ArtifactRef {
            id: id.map(str::to_owned),
            name: name.to_owned(),
            mime_type: None,
            bytes,
        }
    }

    fn names(refs: &[ArtifactRef]) -> Vec<&str> {
        refs.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn a_replaced_reference_keeps_its_position() {
        let mut refs = vec![
            reference("x", None, None),
            reference("plan", Some("p"), None),
            reference("y", None, None),
        ];
        add_artifact_refs(&mut refs, vec![reference("plan2", Some("p"), None)]);
        assert_eq!(names(&refs), ["x", "plan2", "y"]);
    }

    #[test]
    fn two_of_one_id_in_one_call_are_one_entry_with_the_second() {
        let mut refs = vec![reference("x", None, None)];
        add_artifact_refs(
            &mut refs,
            vec![
                reference("a", Some("p"), None),
                reference("b", Some("p"), None),
            ],
        );
        assert_eq!(names(&refs), ["x", "b"]);
    }

    #[test]
    fn a_file_reference_never_replaces_and_is_never_replaced() {
        let mut refs = vec![reference("plan", Some("p"), None)];
        add_artifact_refs(&mut refs, vec![reference("logo", Some("p"), Some(10))]);
        assert_eq!(names(&refs), ["plan", "logo"], "both kept");
        assert_eq!(refs[1].id, None);
        assert_eq!(files_kept(&refs), 10, "the bytes count once");
        add_artifact_refs(&mut refs, vec![reference("plan2", Some("p"), None)]);
        assert_eq!(names(&refs), ["plan2", "logo"]);
    }
}
