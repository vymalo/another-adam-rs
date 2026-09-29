//! [`LlmAgent`]: the durable model <-> tools loop.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use adam_core::{RunId, RunStatus};
use adam_error::report;
use adam_model::{
    Classify, DynModel, FinishReason, Message, ModelError, ModelRequest, ModelResponse, ToolCall,
    ToolSpec,
};
use adam_runtime::{
    Agent, AgentError, AgentStarter, ChildStatus, Ctx, Inbound, RUN_FINISHED_KIND, RunEvent,
    Transition,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::conversation::{
    ArtifactRef, Conversation, PendingQuestion, PendingRemote, PendingRun, PendingWait,
    parse_user_text,
};
use crate::history::fit_history;
use crate::state::Extensions;
use crate::tool::{DynTool, RemotePoll, Tool, ToolCtx, ToolError, ToolOutput};
use crate::toolset::ToolSet;

/// Bounds on one run. When a limit trips the run fails with a message naming
/// it (`RunView::error`); a limit never silently degrades the run, except the
/// history limit, which shortens old tool outputs (see
/// [`max_history_tokens`](Self::max_history_tokens)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most model calls in one run. A run that needs one more fails.
    pub max_turns: u32,
    /// Most tool calls in one run, counted when the model requests them. A
    /// model turn whose calls would go over the limit fails the run before
    /// any of its tools runs.
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

/// Configures an [`LlmAgent`]. Start with [`LlmAgent::builder`].
pub struct LlmAgentBuilder {
    name: String,
    model: DynModel,
    model_alias: String,
    instructions: Option<String>,
    tools: Vec<DynTool>,
    limits: Limits,
    extensions: Extensions,
    wait_poll: Duration,
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
            index,
            specs,
            limits: self.limits,
            extensions: Arc::new(self.extensions),
            wait_poll: self.wait_poll,
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
///    mime_type}...]}`.
///
/// Because steps are journaled, a restarted worker replays recorded model and
/// tool results instead of repeating them.
///
/// # Events
///
/// All best effort, through `Ctx::emit`:
/// * `Custom { kind: "agent_text", payload: {text, turn} }` for each model turn with text;
/// * `Custom { kind: "tool_start", payload: {name, call_id, status: "running"} }` and
///   `Custom { kind: "tool_end", payload: {name, call_id, status} }` where `status` is
///   `ok`, `error`, `needs_input` or `transient_error`;
/// * `Progress` from [`ToolCtx::emit_progress`];
/// * `Artifact` for each artifact a tool returns (also durable);
/// * `Custom { kind: "input_required", payload: {question, call_id} }` before parking;
/// * `Custom { kind: "awaiting_run", payload: {call_id, run} }` before parking on a child run,
///   followed by a `tool_end` with status `waiting` (not final), and later by the final
///   `tool_end` with `ok` or `error` when the child's outcome becomes the result;
/// * `Custom { kind: "awaiting_remote", payload: {call_id, task} }` before parking on a remote
///   task, with the same `tool_end` sequence.
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
/// * A tripped [`Limits`] entry: the run fails.
#[derive(Clone)]
pub struct LlmAgent {
    name: String,
    model: DynModel,
    model_alias: String,
    instructions: Option<String>,
    tools: Vec<DynTool>,
    index: HashMap<String, usize>,
    specs: Vec<ToolSpec>,
    limits: Limits,
    extensions: Arc<Extensions>,
    wait_poll: Duration,
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
            limits: Limits::default(),
            extensions: Extensions::new(),
            wait_poll: DEFAULT_WAIT_POLL,
        }
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn tool(&self, name: &str) -> Option<&DynTool> {
        self.index.get(name).map(|&i| &self.tools[i])
    }

    /// Move inbound messages into the state; the first one answers a pending
    /// question. Returns `false` when the run must keep waiting for that answer.
    ///
    /// A wait on a child run is not touched here: user messages queue behind
    /// the owed tool result like any other.
    fn absorb_inbox(&self, state: &mut Conversation, inbox: Vec<Inbound>) -> bool {
        let mut texts = Vec::new();
        for inbound in inbox {
            match parse_user_text(&inbound.payload) {
                Ok(text) => texts.push(text),
                Err(reason) => {
                    tracing::warn!(inbound = %inbound.id, kind = %inbound.kind, %reason, "ignoring unreadable inbound message");
                }
            }
        }
        let mut texts = texts.into_iter();
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

    /// The child's outcome as the result of the call that waited for it. Returns the status word
    /// the `tool_end` event carries.
    fn answer_run(
        state: &mut Conversation,
        wait: &PendingRun,
        child: &ChildStatus,
    ) -> &'static str {
        let (message, status) = match (&child.status, &child.error) {
            (RunStatus::Done, _) => (
                Message::tool_result(wait.call_id.clone(), render_output(child.output.as_ref())),
                "ok",
            ),
            (_, error) => (
                Message::tool_error(
                    wait.call_id.clone(),
                    format!(
                        "the run failed: {}",
                        error.as_deref().unwrap_or("no reason given")
                    ),
                ),
                "error",
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
        if state.turns >= self.limits.max_turns {
            return Ok(Flow::Fail(format!(
                "turn limit exceeded: the model was called {} times (max_turns = {})",
                state.turns, self.limits.max_turns
            )));
        }
        let mut request = ModelRequest::new(self.model_alias.clone());
        request.system = self.instructions.clone();
        request.messages = fit_history(&state.messages, self.limits.max_history_tokens);
        request.tools = self.specs.clone();
        request.max_output_tokens =
            (self.limits.max_output_tokens > 0).then_some(self.limits.max_output_tokens);

        let model = self.model.clone();
        let recorded: Result<ModelResponse, ModelFailure> = ctx
            .step(&format!("model:{}", state.turns), move || async move {
                model.complete(request).await.map_err(ModelFailure::from)
            })
            .await?;
        let response = match recorded {
            Ok(response) => response,
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
            message,
            finish,
            usage,
        } = response;
        if !matches!(message, Message::Assistant { .. }) {
            return Ok(Flow::Fail(
                "model call failed: the response is not an assistant message".into(),
            ));
        }
        state.usage.input_tokens = state.usage.input_tokens.saturating_add(usage.input_tokens);
        state.usage.output_tokens = state
            .usage
            .output_tokens
            .saturating_add(usage.output_tokens);
        let turn = state.turns;
        state.turns += 1;

        let text = message.text();
        if !text.is_empty() {
            ctx.emit(RunEvent::Custom {
                kind: "agent_text".into(),
                payload: json!({ "text": text, "turn": turn }),
            })
            .await;
        }
        let calls: Vec<ToolCall> = message.tool_calls().to_vec();
        state.messages.push(message);

        if calls.is_empty() {
            let mut output = json!({
                "text": text,
                "artifacts": state.artifacts,
            });
            if finish == FinishReason::Length {
                output["truncated"] = json!(true);
            }
            return Ok(Flow::Done(output));
        }

        let requested = u32::try_from(calls.len()).unwrap_or(u32::MAX);
        if state.tool_calls.saturating_add(requested) > self.limits.max_tool_calls {
            return Ok(Flow::Fail(format!(
                "tool call limit exceeded: {} calls already made, the model asked for {} more (max_tool_calls = {})",
                state.tool_calls, requested, self.limits.max_tool_calls
            )));
        }
        state.tool_calls += requested;
        state.pending_calls = calls;
        Ok(Flow::Next)
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
            let (message, artifacts) = match self.run_tool(ctx, &call).await? {
                ToolResult::Answered { message, artifacts } => (message, artifacts),
                ToolResult::NeedsInput(question) => {
                    ctx.emit(RunEvent::Custom {
                        kind: "input_required".into(),
                        payload: json!({ "question": question, "call_id": call.id }),
                    })
                    .await;
                    state.pending_wait = Some(PendingWait::Question(PendingQuestion {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        question,
                    }));
                    return Ok(Flow::Park);
                }
                ToolResult::AwaitRun(run) => {
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
                        ctx.emit(tool_end(&call.name, &call.id, "waiting")).await;
                        state.pending_wait = Some(PendingWait::Run(wait));
                        return Ok(Flow::Wait);
                    };
                    let status = Self::answer_run(state, &wait, child);
                    ctx.emit(tool_end(&call.name, &call.id, status)).await;
                    continue;
                }
                ToolResult::AwaitRemote { task, timeout_ms } => {
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
                    ctx.emit(tool_end(&call.name, &call.id, "waiting")).await;
                    state.pending_wait = Some(PendingWait::Remote(PendingRemote {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        task,
                        deadline,
                    }));
                    return Ok(Flow::Wait);
                }
            };
            state.messages.push(message);
            state.artifacts.extend(artifacts);
            state.pending_calls.remove(0);
        }
        Ok(Flow::Next)
    }

    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), tool = %call.name, call_id = %call.id))]
    async fn run_tool(&self, ctx: &mut Ctx, call: &ToolCall) -> Result<ToolResult, AgentError> {
        let start = |status: &'static str| RunEvent::Custom {
            kind: "tool_start".into(),
            payload: json!({ "name": call.name, "call_id": call.id, "status": status }),
        };
        let end = |status: &'static str| tool_end(&call.name, &call.id, status);
        ctx.emit(start("running")).await;

        let Some(tool) = self.tool(&call.name).cloned() else {
            let known: Vec<&str> = self.specs.iter().map(|s| s.name.as_str()).collect();
            ctx.emit(end("error")).await;
            return Ok(ToolResult::Answered {
                message: Message::tool_error(
                    call.id.clone(),
                    format!(
                        "unknown tool `{}`; available tools: {}",
                        call.name,
                        if known.is_empty() {
                            "none".to_owned()
                        } else {
                            known.join(", ")
                        }
                    ),
                ),
                artifacts: Vec::new(),
            });
        };

        let tool_ctx = self.tool_ctx(ctx, &call.id, &call.name);
        let args = call.arguments.clone();
        let outcome: Result<ToolOutput, ToolError> = ctx
            .step(&format!("tool:{}", call.id), move || async move {
                tool.call(&tool_ctx, args).await
            })
            .await?;

        match outcome {
            Ok(output) => {
                let (message, artifacts, status) = output_message(ctx, &call.id, output).await;
                ctx.emit(end(status)).await;
                Ok(ToolResult::Answered { message, artifacts })
            }
            Err(ToolError::Permanent(reason)) => {
                ctx.emit(end("error")).await;
                Ok(ToolResult::Answered {
                    message: Message::tool_error(call.id.clone(), reason),
                    artifacts: Vec::new(),
                })
            }
            Err(ToolError::Transient(reason)) => {
                ctx.emit(end("transient_error")).await;
                Err(AgentError::transient(format!(
                    "tool `{}` failed: {reason}",
                    call.name
                )))
            }
            Err(ToolError::NeedsInput { question }) => {
                ctx.emit(end("needs_input")).await;
                Ok(ToolResult::NeedsInput(question))
            }
            // No end event yet: `run_pending` says "waiting", or the final status when the
            // child's message is already here.
            Err(ToolError::AwaitRun { run }) => Ok(ToolResult::AwaitRun(run)),
            Err(ToolError::AwaitRemote { task, timeout_ms }) => {
                Ok(ToolResult::AwaitRemote { task, timeout_ms })
            }
        }
    }

    /// The context a tool sees for the call `call_id`, made from this transition's `Ctx`.
    fn tool_ctx(&self, ctx: &Ctx, call_id: &str, tool: &str) -> ToolCtx {
        ToolCtx::new(
            ctx.conversation_id().map(str::to_owned),
            ctx.attempt(),
            call_id.to_owned(),
            tool.to_owned(),
            ctx.emitter(),
            ctx.cancel_token(),
            Arc::clone(&self.extensions),
            Some(ctx.child_starter()),
        )
    }

    /// Where the remote task a run waits for stands. `Ok(None)`: still going, park again.
    /// `Ok(Some(status))`: the call is answered (the result is in the state) and `status` is the
    /// word the final `tool_end` carries.
    ///
    /// The look is one journaled step (`poll:<call id>`), so a replay of this transition sees what
    /// the first run saw. When the wait has a deadline the clock is read through the journal too
    /// (`ctx.now`): a replay must take the same branch, or the steps after it would not line up.
    async fn poll_remote(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        wait: &PendingRemote,
    ) -> Result<Option<&'static str>, AgentError> {
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
            return Ok(Some("error"));
        }
        let Some(tool) = self.tool(&wait.tool).cloned() else {
            owed(
                state,
                Message::tool_error(
                    wait.call_id.clone(),
                    format!("the tool `{}` that started this task is gone", wait.tool),
                ),
            );
            return Ok(Some("error"));
        };
        let tool_ctx = self.tool_ctx(ctx, &wait.call_id, &wait.tool);
        let task = wait.task.clone();
        let polled: Result<RemotePoll, ToolError> = ctx
            .step(&format!("poll:{}", wait.call_id), move || async move {
                tool.poll_remote(&tool_ctx, &task).await
            })
            .await?;
        match polled {
            Ok(RemotePoll::Working) => Ok(None),
            Ok(RemotePoll::Ready(output)) => {
                let (message, artifacts, status) = output_message(ctx, &wait.call_id, output).await;
                state.artifacts.extend(artifacts);
                owed(state, message);
                Ok(Some(status))
            }
            Err(ToolError::Transient(reason)) => Err(AgentError::transient(format!(
                "tool `{}` failed while polling its remote task: {reason}",
                wait.tool
            ))),
            Err(ToolError::Permanent(reason)) => {
                owed(state, Message::tool_error(wait.call_id.clone(), reason));
                Ok(Some("error"))
            }
            Err(other) => {
                owed(
                    state,
                    Message::tool_error(
                        wait.call_id.clone(),
                        format!("polling the remote task went wrong: {other}"),
                    ),
                );
                Ok(Some("error"))
            }
        }
    }
}

/// A tool's output as the message that answers `call_id`, with the references to its artifacts and
/// the status word for the `tool_end` event. The artifacts are emitted here (and recorded with the
/// transition's commit).
async fn output_message(
    ctx: &Ctx,
    call_id: &str,
    output: ToolOutput,
) -> (Message, Vec<ArtifactRef>, &'static str) {
    let status = if output.is_error { "error" } else { "ok" };
    let refs: Vec<ArtifactRef> = output
        .artifacts
        .iter()
        .map(|a| ArtifactRef {
            name: a.name.clone(),
            mime_type: a.mime_type.clone(),
        })
        .collect();
    for artifact in output.artifacts {
        ctx.emit(RunEvent::from(artifact)).await;
    }
    let message = Message::Tool {
        call_id: call_id.to_owned(),
        content: output.content,
        is_error: output.is_error,
    };
    (message, refs, status)
}

/// The first user message of a run, as a fresh [`Conversation`]. Shared by
/// [`LlmAgent::init`] and [`LlmStarter::init`], so they cannot drift apart.
fn start_conversation(input: Inbound) -> Result<Conversation, AgentError> {
    let text = parse_user_text(&input.payload)
        .map_err(|reason| AgentError::permanent(format!("unusable start message: {reason}")))?;
    Ok(Conversation::new(text))
}

/// The start-only half of an [`LlmAgent`]: it turns the first user message
/// into the [`Conversation`] a run starts with, and needs neither a model nor
/// tools.
///
/// Register it with `RuntimeBuilder::starter` on a process that only accepts
/// requests, under the name of the [`LlmAgent`] that steps the runs on a
/// worker. `init` accepts exactly what [`LlmAgent::init`] accepts (payload
/// `{"text": "..."}` or a bare JSON string) and rejects what it rejects.
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
}

enum ToolResult {
    Answered {
        message: Message,
        artifacts: Vec<ArtifactRef>,
    },
    NeedsInput(String),
    AwaitRun(RunId),
    AwaitRemote {
        task: String,
        timeout_ms: Option<u64>,
    },
}

fn tool_end(name: &str, call_id: &str, status: &'static str) -> RunEvent {
    RunEvent::Custom {
        kind: "tool_end".into(),
        payload: json!({ "name": name, "call_id": call_id, "status": status }),
    }
}

/// A child's output as the text of the tool result. An `LlmAgent` child finishes with
/// `{"text": ..., "artifacts": [...]}` and the parent's model gets the text (the same reading the
/// A2A adapter has of an output); another string is passed as it is, and anything else as JSON.
fn render_output(output: Option<&Value>) -> String {
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
        "(the run finished without output)".to_owned()
    } else {
        text
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

    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), agent = %self.name))]
    async fn step(
        &self,
        ctx: &mut Ctx,
        mut state: Conversation,
    ) -> Result<Transition<Conversation>, AgentError> {
        let (finished, inbox): (Vec<_>, Vec<_>) = ctx
            .take_inbox()
            .into_iter()
            .partition(|i| i.kind == RUN_FINISHED_KIND);
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
        if !self.absorb_inbox(&mut state, inbox) {
            // Woken without an answer (nothing usable arrived): keep waiting.
            return Ok(Transition::Park {
                state,
                wake_at: None,
            });
        }

        // A wait on a child ends with the child's message or, when woken without one (the timer,
        // or a user message), with what the store says. A message that matches no wait (a
        // duplicate of one already used, or one for a call the run is not on) is dropped here:
        // the wait it was for has been settled, or will be, from the store.
        if let Some(PendingWait::Run(wait)) = state.pending_wait.clone() {
            match Self::settle(ctx, &wait, &notices).await? {
                Some(child) => {
                    let status = Self::answer_run(&mut state, &wait, &child);
                    ctx.emit(tool_end(&wait.tool, &wait.call_id, status)).await;
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
                Some(status) => {
                    ctx.emit(tool_end(&wait.tool, &wait.call_id, status)).await;
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
        }
        if matches!(flow, Flow::Next) {
            flow = self.run_pending(ctx, &mut state, &notices).await?;
        }
        Ok(match flow {
            Flow::Next => {
                if state.pending_calls.is_empty() {
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
            Flow::Done(output) => Transition::Done { state, output },
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
