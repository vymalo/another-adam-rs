//! [`LlmAgent`]: the durable model <-> tools loop.

use std::collections::HashMap;
use std::time::Duration;

use adam_error::report;
use adam_model::{
    Classify, DynModel, FinishReason, Message, ModelError, ModelRequest, ModelResponse, ToolCall,
    ToolSpec,
};
use adam_runtime::{Agent, AgentError, AgentStarter, Ctx, Inbound, RunEvent, Transition};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::conversation::{ArtifactRef, Conversation, PendingQuestion, parse_user_text};
use crate::history::fit_history;
use crate::tool::{DynTool, Tool, ToolCtx, ToolError, ToolOutput};

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
}

impl LlmAgentBuilder {
    /// The system prompt.
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Register a tool. A second tool with the same name replaces the first.
    pub fn tool(self, tool: impl Tool) -> Self {
        self.dyn_tool(std::sync::Arc::new(tool))
    }

    /// Register an already shared tool.
    pub fn dyn_tool(mut self, tool: DynTool) -> Self {
        self.tools.push(tool);
        self
    }

    /// Replace the [`Limits`] (default: [`Limits::default`]).
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
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
///    on a question, the first one answers it).
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
/// * `Custom { kind: "input_required", payload: {question, call_id} }` before parking.
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
///   tool names: an error tool result for the model; the run goes on.
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
    /// question. Returns `false` when the run must keep waiting.
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
        if let Some(q) = state.pending_question.take() {
            let Some(answer) = texts.next() else {
                state.pending_question = Some(q);
                return false;
            };
            if state
                .pending_calls
                .first()
                .is_some_and(|c| c.id == q.call_id)
            {
                state.pending_calls.remove(0);
            }
            state.messages.push(Message::tool_result(q.call_id, answer));
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
    async fn run_pending(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
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
                    state.pending_question = Some(PendingQuestion {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        question,
                    });
                    return Ok(Flow::Park);
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
        let end = |status: &'static str| RunEvent::Custom {
            kind: "tool_end".into(),
            payload: json!({ "name": call.name, "call_id": call.id, "status": status }),
        };
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

        let tool_ctx = ToolCtx::new(
            ctx.conversation_id().map(str::to_owned),
            ctx.attempt(),
            call.id.clone(),
            call.name.clone(),
            ctx.emitter(),
            ctx.cancel_token(),
        );
        let args = call.arguments.clone();
        let outcome: Result<ToolOutput, ToolError> = ctx
            .step(&format!("tool:{}", call.id), move || async move {
                tool.call(&tool_ctx, args).await
            })
            .await?;

        match outcome {
            Ok(output) => {
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
                ctx.emit(end(status)).await;
                Ok(ToolResult::Answered {
                    message: Message::Tool {
                        call_id: call.id.clone(),
                        content: output.content,
                        is_error: output.is_error,
                    },
                    artifacts: refs,
                })
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
        }
    }
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
        let inbox = ctx.take_inbox();
        if !self.absorb_inbox(&mut state, inbox) {
            // Woken without an answer (nothing usable arrived): keep waiting.
            return Ok(Transition::Park {
                state,
                wake_at: None,
            });
        }

        // Owed tool results (the run was parked on a question) come first;
        // otherwise this step is a fresh model turn.
        let mut flow = Flow::Next;
        if state.pending_calls.is_empty() {
            state.messages.append(&mut state.deferred);
            flow = self.model_turn(ctx, &mut state).await?;
        }
        if matches!(flow, Flow::Next) {
            flow = self.run_pending(ctx, &mut state).await?;
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
            Flow::Done(output) => Transition::Done { state, output },
            Flow::Fail(error) => Transition::Fail { state, error },
        })
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
