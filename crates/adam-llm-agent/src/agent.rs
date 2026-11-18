//! [`LlmAgent`]: the durable model <-> tools loop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_core::{RunId, RunStatus};
use adam_error::report;
use adam_model::{
    Classify, DynModel, FinishReason, Message, ModelError, ModelRequest, ModelResponse, ToolCall,
    ToolChoice, ToolSpec,
};
use adam_runtime::{
    AGENT_TEXT_KIND, Agent, AgentError, AgentStarter, CHILD_REPORTED_KIND, ChildStarter,
    ChildStatus, Ctx, Emitter, Inbound, MAX_RUN_FILE_BYTES, MAX_SESSION_PAGE, RUN_FINISHED_KIND,
    ReservedStep, RunEvent, SESSION_CLOSE_KIND, SandboxUsage, SessionState, SessionSummary,
    StepEvent, StepIcon, StepKind, StepState, Transition, UsageEvent, UsageTotals, child_run_id,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::access::ToolAccess;
use crate::compaction::{
    self, COMPACTION_TURN, CompactionReason, MAX_COMPACTIONS, OVERFLOW_OUTPUT_CHARS, PromptSize,
    RETRY_AFTER_TURNS, SUMMARY_OUTPUT_TOKENS,
};
use crate::conversation::{
    ArtifactRef, Conversation, LimitHit, LimitKind, PendingQuestion, PendingRemote, PendingRun,
    PendingSession, PendingWait, Repeats, asks_to_close, parse_context, parse_root_run,
    parse_root_step, parse_session, parse_user_text,
};
use crate::history::record_shortenings;
use crate::images::{
    ImageGate, ImageStore, InboundImage, Item, KeptImage, MAX_RUN_JOURNALED_IMAGE_BYTES,
    admit_inbound, answer_with, described, for_sending, image_data, image_scope, image_step,
    parse_inbound_images, parts_of, state_bytes,
};
use crate::kept::{KeptOutput, MAX_RUN_KEPT_BYTES};
use crate::order;
use crate::paging::backstop;
use crate::plan::Plan;
use crate::schedule::{self, Entry, Ordered, Queue, Segment};
use crate::sessions::{
    DueFacts, SUBAGENT_CLOSE, SUBAGENT_READ, SUBAGENT_SEND, SUBAGENT_START, is_due, report_call_id,
};

/// Whether the history holds a call that started a session or sent to one.
fn has_started_session(state: &Conversation) -> bool {
    // A compaction that cut the call away says so: the run still waits for, and cancels, the
    // sessions it started.
    state.compactions.iter().any(|c| c.sessions)
        || state.messages.iter().any(|m| {
            m.tool_calls()
                .iter()
                .any(|c| c.name == SUBAGENT_START || c.name == SUBAGENT_SEND)
        })
}

/// What the tool sources add to a request: their tools (refined, minus what a source holds back) and
/// their instructions. The one place both the model turn and the summary request read them, so the
/// head of the two is the same bytes. Returns what the sources said of the tools they offered.
///
/// Last, with `inject`, every tool's schema gets the optional `after_previous` (see [`order`]); the names
/// of the tools whose own schema has one are returned beside the notes, and the turn records them.
async fn apply_sources(
    sources: &[DynToolSource],
    source_ctx: Option<&SourceCtx>,
    request: &mut ModelRequest,
    inject: bool,
) -> (Vec<ToolNote>, Vec<String>) {
    let notes = match source_ctx {
        Some(source_ctx) => offered_by(sources, source_ctx, request).await,
        None => Vec::new(),
    };
    let own = order::prepare(&mut request.tools, inject);
    (notes, own)
}

/// The sources' part of [`apply_sources`].
async fn offered_by(
    sources: &[DynToolSource],
    source_ctx: &SourceCtx,
    request: &mut ModelRequest,
) -> Vec<ToolNote> {
    let (more, notes) = offered(sources, source_ctx, &request.tools).await;
    request.tools.extend(more);
    refined(sources, source_ctx, &mut request.tools).await;
    // What a source holds back is not shown, the agent's own tools included.
    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    let held = withheld(sources, source_ctx, &names).await;
    if !held.is_empty() {
        request
            .tools
            .retain(|t| !held.iter().any(|h| h.tool == t.name));
    }
    // What the sources add to the instructions (the agents a person mentioned).
    if let Some(more) = instructed(sources, source_ctx).await {
        request.system = Some(match request.system.take() {
            Some(own) if !own.trim().is_empty() => format!("{own}\n\n{more}"),
            _ => more,
        });
    }
    notes
}

/// The summary call of a compaction as the journal records it: the summary (none when the model said
/// nothing), the cut it was made for, and the call's tokens and id for its usage report.
#[derive(Debug, Serialize, Deserialize)]
struct Summarised {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    cut: usize,
    usage: adam_model::Usage,
    call: String,
}

/// The id of the summary call `n` of `run` in its usage report: made inside the journaled step, like
/// [`call_id`], and apart from every model turn's.
fn compaction_call_id(run: RunId, n: usize) -> String {
    let mut suffix = uuid::Uuid::new_v4().simple().to_string();
    suffix.truncate(8);
    format!("{run}-k{n}-{suffix}")
}

/// `messages` with every tool output over [`OVERFLOW_OUTPUT_CHARS`] shortened, for the summary
/// request that follows a rejection: the history was too long for the provider once already.
fn shortened_for_overflow(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|m| match m {
            Message::Tool {
                call_id,
                content,
                is_error,
                images,
            } => {
                let len = content.chars().count();
                let short = (len > OVERFLOW_OUTPUT_CHARS)
                    .then(|| {
                        crate::history::shorten_output(content, len - OVERFLOW_OUTPUT_CHARS, None)
                    })
                    .flatten();
                Message::Tool {
                    call_id: call_id.clone(),
                    content: short.unwrap_or_else(|| content.clone()),
                    is_error: *is_error,
                    images: images.clone(),
                }
            }
            other => other.clone(),
        })
        .collect()
}

/// Whether a call of `tool` acts on a session (an effect outside the run's own state).
fn has_effect(tool: &str) -> bool {
    matches!(tool, SUBAGENT_START | SUBAGENT_SEND | SUBAGENT_CLOSE)
}

/// Whether a round of the session is doing work somebody will want to read. A round that only
/// closes the session is not: it calls no model, ends by itself and reports nothing new, so a run
/// neither waits for it at the end of a turn nor cancels it when it ends.
fn does_work(session: &SessionSummary) -> bool {
    session.state == SessionState::Working && !session.closing
}
use crate::source::{
    DynToolSource, SourceCtx, ToolNote, ToolSource, instructed, offered, refined, withheld,
};
use crate::state::Extensions;
use crate::step_io::StepIo;
use crate::text_stream;
use crate::tool::{
    AnswerCheck, DEFAULT_TOOL_TIME_LIMIT, DynTool, Refusal, RemotePoll, StepStyle, TOOL_STOP_GRACE,
    Tool, ToolCtx, ToolError, ToolOutput,
};
use crate::toolset::ToolSet;

/// The most tool calls of one turn that run at the same time unless
/// [`LlmAgentBuilder::max_parallel`] says otherwise.
pub const DEFAULT_MAX_PARALLEL: usize = 10;

/// The most [`LlmAgentBuilder::max_parallel`] accepts: a larger value is this.
pub const MAX_PARALLEL_LIMIT: usize = 32;

/// What the model is told when a tool that asks the person, or waits for a child run or a remote
/// task, was called beside other calls: it has nowhere to park there.
const NOT_ALONGSIDE: &str = "this tool cannot run alongside others: call it alone";

/// Whether a result reports the rounds of sessions: only a call that runs alone may.
fn reports_rounds(output: &ToolOutput) -> bool {
    !output.is_error && !output.reported.is_empty()
}

/// Whether `name` is one of the session tools, which always run alone.
fn is_session_tool(name: &str) -> bool {
    matches!(
        name,
        SUBAGENT_START | SUBAGENT_SEND | SUBAGENT_READ | SUBAGENT_CLOSE
    )
}

/// What a call needs to run its step, made before the call starts.
struct Prepared {
    tool_ctx: ToolCtx,
    /// The sources and what they see of the run, to check what a source holds back.
    holding: Option<(Vec<DynToolSource>, SourceCtx)>,
    unjournaled: Arc<Mutex<SandboxUsage>>,
}

/// How a call of a segment ended.
enum Finished {
    /// The call took no step (bad arguments, a tool nobody has): what the model is told.
    Said(String),
    /// The recorded outcome of the call's step.
    Ran {
        outcome: Box<Result<ToolOutput, ToolError>>,
        unjournaled: Arc<Mutex<SandboxUsage>>,
        live: Live,
    },
}

/// What was said about the end of a call's step as it happened, so that applying the result says it
/// again only when it changes.
struct Live {
    /// The state and the text of an end that applying the result may change; `None` for an end
    /// that it cannot.
    said: Option<(StepState, String)>,
}

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
    /// What the run admits as images from now on: the gate for this model and what the history holds.
    images: ImageGate,
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
    /// Over it, old tool outputs are truncated **in the stored history**, with an explicit marker,
    /// down to three quarters of the budget (so that it happens rarely and each request starts with
    /// the one before: [`Conversation::shortenings`] says when); the results the model has not seen
    /// yet are never touched, and the journal still holds the original outputs. `0` disables
    /// truncation.
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

/// The key the loop gives a model client to route the requests of one conversation to the same cache:
/// the first 32 hex digits of the SHA-256 of the conversation id. The id itself never leaves, and the
/// length is bounded whatever the id is.
fn prompt_cache_key(conversation_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(conversation_id.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
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
    /// The provider rejected the prompt as too long ([`ModelError::is_context_overflow`]): the run
    /// compacts its history once and asks again. Absent from journals written before, which decode
    /// as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    context_length: bool,
    /// The provider refused the request itself, not for its length, nor its credentials, nor a
    /// limit ([`ErrorClass::Invalid`]): the same request never succeeds. When the request carried
    /// images no earlier request carried, the run replaces them with lines and asks again. Absent
    /// from journals written before, which decode as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    refused: bool,
}

impl ModelFailure {
    /// The call was dropped because the run was cancelled. Not retryable: nobody is waiting for it.
    fn cancelled() -> Self {
        Self {
            retryable: false,
            message: CANCELLED.to_owned(),
            retry_after_ms: None,
            class: None,
            context_length: false,
            refused: false,
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
            context_length: e.is_context_overflow(),
            refused: e.class() == adam_error::ErrorClass::Invalid && !e.is_context_overflow(),
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
    /// The tools whose own schema has an `after_previous`: their calls keep the argument as it is
    /// ([`order`]). Absent when none, and in journals written before it existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    own_order: Vec<String>,
    /// The reports of sessions the request was given before the call ([`RoundReport`], ADR 0045
    /// slice 3), in the order they were appended to it. Absent when there were none, and in
    /// journals written before they existed, which read as none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reports: Vec<RoundReport>,
    /// What the listing made for the reports saw: whether a session of the conversation is not
    /// closed. `None` when the call made no listing (or it failed), and in journals written before
    /// it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sessions_open: Option<bool>,
}

/// The report of one round, read for a model call that did not ask for it: the result of a
/// `subagent_read` the loop made itself. Journaled in the record of the call, so a replay tells
/// the model the same words.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RoundReport {
    /// The session.
    session: RunId,
    /// The round the report is of: the id of the call that carries it is made from it.
    round: RunId,
    /// What the read of the session answered.
    output: ToolOutput,
}

/// What a request is given for `reports`: one assistant message that calls `subagent_read` once
/// for each, then the results, in the same order. Pure: the request and the history are given the
/// same bytes. `paged` is the tool's ([`Tool::paged`]).
fn report_messages(reports: &[RoundReport], paged: bool) -> Vec<Message> {
    if reports.is_empty() {
        return Vec::new();
    }
    let calls: Vec<ToolCall> = reports
        .iter()
        .map(|r| {
            ToolCall::new(
                report_call_id(r.round),
                SUBAGENT_READ,
                json!({ "session": r.session.to_string() }),
            )
        })
        .collect();
    let mut messages = vec![Message::Assistant {
        content: Vec::new(),
        tool_calls: calls.clone(),
        reasoning: None,
    }];
    messages.extend(reports.iter().zip(&calls).map(|(r, call)| Message::Tool {
        call_id: call.id.clone(),
        content: backstop(&r.output.content, SUBAGENT_READ, paged).into_owned(),
        is_error: false,
        images: Vec::new(),
    }));
    messages
}

/// What a model call needs to tell the model the reports of sessions that ended: read inside the
/// step of the call, from the store.
struct ReportPlan {
    /// The `subagent_read` tool: its `poll_remote` is the read of a report that has ended.
    tool: DynTool,
    tool_ctx: ToolCtx,
    paged: bool,
    starter: ChildStarter,
    me: RunId,
    agents: Arc<[String]>,
    /// The rounds already reported ([`Conversation::reported_rounds`]).
    reported: Vec<RunId>,
    /// The ids of the report calls the history holds.
    in_history: HashSet<String>,
    /// When the chain of tasks began ([`Conversation::chain_started`]).
    since: Option<DateTime<Utc>>,
}

impl ReportPlan {
    /// List the sessions, read the report of each round that is due ([`is_due`]) and append them
    /// to `request` as the calls and results of reads. A listing or a read that fails is skipped,
    /// with a warning: the next call lists again. Returns the reports, and whether a session is
    /// still open (`None` when the listing failed).
    async fn tell(self, request: &mut ModelRequest) -> (Vec<RoundReport>, Option<bool>) {
        let names: Vec<&str> = self.agents.iter().map(String::as_str).collect();
        let list = match self.starter.sessions(&names, 0, MAX_SESSION_PAGE).await {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(error = %report(&e), "listing the sessions to report failed; the next call lists again");
                return (Vec::new(), None);
            }
        };
        let mut open = list
            .sessions
            .iter()
            .any(|s| s.state != SessionState::Closed);
        let facts = DueFacts {
            me: self.me,
            reported: &self.reported,
            in_history: &self.in_history,
            since: self.since,
        };
        let mut reports = Vec::new();
        for session in list.sessions.iter().filter(|s| is_due(s, &facts)) {
            match self
                .tool
                .poll_remote(&self.tool_ctx, &session.id.to_string())
                .await
            {
                Ok(RemotePoll::Ready(output)) if !output.is_error => {
                    if let Some(round) = output.reported.first().copied() {
                        reports.push(RoundReport {
                            session: session.id,
                            round,
                            output,
                        });
                        continue;
                    }
                }
                Ok(RemotePoll::Ready(_)) => {
                    tracing::warn!(session = %session.id, "the read of a report answered an error; the next call looks again");
                }
                // The round the report is of has not ended as far as the read can tell (a closing
                // round still works): it is due, and looked at again.
                Ok(RemotePoll::Working) => {}
                Err(e) => {
                    tracing::warn!(session = %session.id, error = %e, "reading a report failed; the next call looks again");
                }
            }
            // A due report that was not appended keeps the listing on.
            open = true;
        }
        request
            .messages
            .extend(report_messages(&reports, self.paged));
        (reports, Some(open))
    }
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
    session_reports: bool,
    max_parallel: usize,
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
    /// A tool's own schema (a compiled tool's: not one whose [`Tool::schema_is_foreign`], such as an
    /// MCP server's) has a property called `after_previous`, which the loop reserves for the
    /// order of the calls of a turn ([`AFTER_PREVIOUS`](crate::AFTER_PREVIOUS)) whenever more than
    /// one call may run at once. Rename the property, or build the agent with `max_parallel(1)`.
    #[error(
        "tool `{tool}` has a property called `{argument}`, which the agent reserves for the order of \
         parallel tool calls: rename it, or use max_parallel(1)"
    )]
    ReservedArgument {
        /// The tool.
        tool: String,
        /// The reserved name.
        argument: String,
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

    /// Build the agent as one that predates the reports a model is told during a turn: it never
    /// lists the sessions before a model call, so a journal it writes has none of their records.
    /// For the test that proves such a journal still replays.
    #[doc(hidden)]
    #[must_use]
    pub fn without_session_reports(mut self) -> Self {
        self.session_reports = false;
        self
    }

    /// The most tool calls of one model turn that run at the same time: `10` by default, and
    /// `n` clamped to `1..=32`. The calls of a turn run together as far as their
    /// [`Tool::access`] allows (a tool that declares nothing runs alone), the ones over the limit wait
    /// in a queue and start in call order, and the results go back to the model in call order. `1`
    /// runs the calls one after another. It is not a field of [`Limits`]: those are built by literals.
    /// See [ADR 0049](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0049-the-tool-calls-of-a-turn-run-at-the-same-time.md).
    #[must_use]
    pub fn max_parallel(mut self, n: usize) -> Self {
        self.max_parallel = n.clamp(1, MAX_PARALLEL_LIMIT);
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
            if self.max_parallel > 1 && !tool.schema_is_foreign() && order::has_own(&tool.spec()) {
                return Err(BuildError::ReservedArgument {
                    tool: name,
                    argument: order::AFTER_PREVIOUS.to_owned(),
                });
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
            // Above a limit of one the model is told what `after_previous` means.
            instructions: if self.max_parallel > 1 {
                Some(match self.instructions {
                    Some(own) if !own.trim().is_empty() => {
                        format!("{own}\n\n{}", order::INSTRUCTION)
                    }
                    _ => order::INSTRUCTION.to_owned(),
                })
            } else {
                self.instructions
            },
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
            session_reports: self.session_reports,
            max_parallel: self.max_parallel,
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
///    its result is appended, in the order of the calls. The calls of the turn run at the same
///    time, up to [`LlmAgentBuilder::max_parallel`], except where [`Tool::access`] says two
///    conflict: a tool that declares nothing runs alone, after the calls before it (ADR 0049). Then
///    the step returns `Continue`, so every
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
/// * `Custom { kind: "answer_refused", payload: {call_id, question, rule} }` when the tool that asked read the
///   answer and refused it ([`Tool::read_answer`]): the run stays parked on the question the tool asked in its
///   place, and the model hears nothing (`question` is what the rule was about, `*` for the whole answer);
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
    /// Whether a report of a session that ended reaches the model without a read of its own.
    session_reports: bool,
    /// The most tool calls of a turn that run at the same time.
    max_parallel: usize,
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
async fn say(ctx: &Ctx, silent: bool, event: RunEvent) {
    if !silent {
        ctx.emit(event).await;
    }
}

/// [`say`] through an [`Emitter`], for the calls of a segment, which run while the `Ctx` is not at hand.
async fn say_with(emitter: &Emitter, silent: bool, event: RunEvent) {
    if !silent {
        emitter.emit(event).await;
    }
}

/// Whether the call of tool `name` goes unreported by the agent: the tool is a source's and its
/// listing said the system behind the source reports its calls.
fn silent_call(agent: &LlmAgent, state: &Conversation, name: &str) -> bool {
    agent
        .source_note(&state.source_notes, name)
        .is_some_and(|n| n.reports_step)
}

/// What the run made of the messages that arrived: see [`LlmAgent::absorb_inbox`].
struct Absorbed {
    /// The run is parked on a question and nothing answered it.
    waiting: bool,
    /// The answers a tool refused (`Tool::read_answer`), each with the call it was for.
    refusals: Vec<(String, Refusal)>,
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
            session_reports: true,
            max_parallel: DEFAULT_MAX_PARALLEL,
        }
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The most tool calls of a turn that run at the same time
    /// ([`LlmAgentBuilder::max_parallel`]).
    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    fn tool(&self, name: &str) -> Option<&DynTool> {
        self.index.get(name).map(|&i| &self.tools[i])
    }

    /// Whether the tool `name` pages its results: it says so ([`Tool::paged`]). Never inferred
    /// from its schema, which an unrelated tool may give an `offset` argument.
    fn pages(&self, name: &str) -> bool {
        self.tool(name).is_some_and(|t| t.paged())
    }

    /// Move inbound messages into the state; the first one answers a pending question.
    /// [`Absorbed::waiting`] is `true` when the run must keep waiting for that answer.
    ///
    /// A wait on a child run is not touched here: user messages queue behind
    /// the owed tool result like any other.
    ///
    /// The answer is read by the tool that asked ([`Tool::read_answer`]) before the model sees it. A
    /// refused one is not used: the run stays parked on the question the tool made in its place, and
    /// the refusal is returned for the caller to journal. A tool may also end the question: the call
    /// then gets an error result.
    fn absorb_inbox(
        &self,
        state: &mut Conversation,
        inbox: Vec<Inbound>,
        admitted: &HashMap<String, Vec<Item>>,
    ) -> Absorbed {
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
                    let items = admitted.get(&inbound.id).cloned().unwrap_or_default();
                    texts.push((text, inbound.payload, items));
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
        let mut refusals = Vec::new();
        // The first message that the tool accepts answers the question; one it refuses leaves the
        // run parked on the question the tool asked in its place, and the next is read against that.
        while let Some(PendingWait::Question(q)) = &state.pending_wait {
            let Some((answer, payload, items)) = texts.next() else {
                return Absorbed {
                    waiting: true,
                    refusals,
                };
            };
            let (call_id, tool, ui) = (q.call_id.clone(), q.tool.clone(), q.ui.clone());
            let given = |state: &mut Conversation, result: Message| {
                state.pending_wait = None;
                if state.pending_calls.first().is_some_and(|c| c.id == call_id) {
                    state.pending_calls.remove(0);
                }
                state.messages.push(result);
            };
            match self.read_answer(state, &tool, &call_id, ui.as_ref(), &payload) {
                AnswerCheck::Refuse(refusal) => {
                    // The new question is other words than the model's, so it has no stream.
                    state.pending_wait = Some(PendingWait::Question(PendingQuestion {
                        call_id: call_id.clone(),
                        tool,
                        question: refusal.question.clone(),
                        ui: refusal.ui.clone(),
                        stream: None,
                    }));
                    refusals.push((call_id, refusal));
                }
                AnswerCheck::End(refusal) => {
                    given(
                        state,
                        Message::tool_error(call_id.clone(), refusal.question.clone()),
                    );
                    refusals.push((call_id, refusal));
                }
                // What the person attached to the answer is the tool result's images.
                AnswerCheck::Read(said) => {
                    let (said, images) = answer_with(said, &items);
                    given(
                        state,
                        Message::tool_result_with_images(call_id.clone(), said, images),
                    );
                }
                AnswerCheck::Accept => {
                    let (answer, images) = answer_with(answer, &items);
                    given(
                        state,
                        Message::tool_result_with_images(call_id.clone(), answer, images),
                    );
                }
            }
        }
        for (text, _, items) in texts {
            let message = if items.is_empty() {
                Message::user_text(text)
            } else {
                Message::User {
                    content: parts_of(&text, &items),
                }
            };
            if state.pending_calls.is_empty() {
                state.messages.push(message);
            } else {
                state.deferred.push(message);
            }
        }
        Absorbed {
            waiting: false,
            refusals,
        }
    }

    /// What the run admits as images from now on: the gate for this model, and what the history
    /// holds, so that a call admits the images the run has room for and no more.
    fn image_gate(&self, state: &Conversation) -> ImageGate {
        let limits = self.model.input_modalities(&self.model_alias).images;
        let keeps_in_state = self.extensions.get::<ImageStore>().is_none();
        let room = keeps_in_state.then(|| MAX_RUN_KEPT_BYTES.saturating_sub(state.kept_bytes()));
        let journal_left = keeps_in_state
            .then(|| MAX_RUN_JOURNALED_IMAGE_BYTES.saturating_sub(state.journaled_image_bytes));
        ImageGate::new(limits, state.image_count(), room).with_journal_left(journal_left)
    }

    /// Admit and keep the images of the start message (`state.pending_images`) and of each message of
    /// `inbox`, each in a journaled step of its own that records what became of every image: the
    /// references of the ones kept and the line that stands in the place of the others. A replay
    /// reads the same records. The images of the start message are attached to it; the others are
    /// returned by the id of their message, for [`absorb_inbox`](Self::absorb_inbox).
    async fn admit_images(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        inbox: &[Inbound],
    ) -> Result<HashMap<String, Vec<Item>>, AgentError> {
        let mut admitted = HashMap::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let waiting = inbox
            .iter()
            .any(|i| !state.has_read(&i.id) && !parse_inbound_images(&i.payload).is_empty());
        if state.pending_images.is_empty() && !waiting {
            return Ok(admitted);
        }
        // The images of a message are journaled as references: the journal's bound is not theirs.
        let mut gate = self.image_gate(state).without_journal_bound();
        if !state.pending_images.is_empty() {
            let pending = std::mem::take(&mut state.pending_images);
            let items = self
                .admit_step(ctx, state, "image:start", &pending, &mut gate)
                .await?;
            if let Some(Message::User { content }) = state
                .messages
                .iter_mut()
                .rev()
                .find(|m| matches!(m, Message::User { .. }))
            {
                // The words of the message come first and an empty text part is not kept: a message
                // that is only an image has no words.
                content.retain(
                    |p| !matches!(p, adam_model::ContentPart::Text { text } if text.is_empty()),
                );
                content.extend(parts_of("", &items));
            }
        }
        for inbound in inbox {
            if state.has_read(&inbound.id) || !seen.insert(inbound.id.as_str()) {
                continue;
            }
            let images = parse_inbound_images(&inbound.payload);
            if images.is_empty() {
                continue;
            }
            let name = image_step(&inbound.id);
            let items = self
                .admit_step(ctx, state, &name, &images, &mut gate)
                .await?;
            admitted.insert(inbound.id.clone(), items);
        }
        Ok(admitted)
    }

    /// One journaled step that admits `images`: what it records is the decision for each image, never
    /// a byte. The bytes of an admitted image are kept by the store the agent has, inside the step,
    /// or, with none, in the run's own state, from the message that still holds them.
    async fn admit_step(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        name: &str,
        images: &[InboundImage],
        gate: &mut ImageGate,
    ) -> Result<Vec<Item>, AgentError> {
        let store = self.extensions.get::<ImageStore>().map(|s| (*s).clone());
        let root = state.root_run.unwrap_or_else(|| ctx.run_id()).to_string();
        let scope = image_scope(ctx.conversation_id(), &root);
        if let Some(store) = &store
            && let Err(error) = store.0.note_run(&scope, &root).await
        {
            tracing::warn!(%error, "the run could not be noted for the images it keeps");
        }
        let (mut inside, offered, keeper, scope_in) =
            (gate.clone(), images.to_vec(), store.clone(), scope);
        let recorded: Result<Vec<Item>, String> = ctx
            .step(name, move || async move {
                Ok(admit_inbound(&offered, &mut inside, keeper.as_ref(), &scope_in).await)
            })
            .await?;
        let items = recorded.unwrap_or_default();
        for item in &items {
            if let Item::Image(image) = item {
                gate.took(image);
            }
        }
        if store.is_none() {
            state.keep_images(state_bytes(images, &items));
        }
        Ok(items)
    }

    /// What the tool that asked says about `payload`, the message that answers its question: the
    /// call is the one the run holds in `pending_calls`. A tool or a call that is no longer there
    /// accepts, so the answer reaches the model as before.
    fn read_answer(
        &self,
        state: &Conversation,
        tool: &str,
        call_id: &str,
        ui: Option<&Value>,
        payload: &Value,
    ) -> AnswerCheck {
        let call = state.pending_calls.iter().find(|c| c.id == call_id);
        match (self.tool(tool), call) {
            (Some(tool), Some(call)) => tool.read_answer(call_id, &call.arguments, ui, payload),
            _ => AnswerCheck::Accept,
        }
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
        // What is sent about an image is decided here, from the reference and the deployment's
        // settings now, and recorded nowhere: the same settings give the same lines every turn.
        let image_limits = self.model.input_modalities(&self.model_alias).images;
        request.messages = for_sending(&state.messages, image_limits.as_ref());
        request.tools = self.specs.clone();
        request.prompt_cache_key = ctx.conversation_id().map(prompt_cache_key);
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
        let ordered = self.max_parallel > 1;
        let cancel = ctx.cancel_token();
        // The bytes of the images of the history are read here, inside the step, for this call only:
        // they are never in the journal and never in the history. A model that cannot see is given
        // none, whatever the history holds.
        let named: Vec<adam_model::ImageRef> = request
            .messages
            .iter()
            .flat_map(Message::images)
            .cloned()
            .collect();
        let carried: Vec<String> = named.iter().map(|i| i.sha256.clone()).collect();
        let sent = carried.clone();
        let images_store = self.extensions.get::<ImageStore>().map(|s| (*s).clone());
        let images_scope = image_scope(
            ctx.conversation_id(),
            &state.root_run.unwrap_or_else(|| ctx.run_id()).to_string(),
        );
        let images_state = Arc::clone(&state.images);
        // A report is told to the model before its first call of a run, and before every call
        // while a session of the conversation may still end.
        if turn == 0 && state.chain_started.is_none() {
            state.chain_started = Some(ctx.now());
        }
        let report_plan = (self.session_reports && !closing && (turn == 0 || state.sessions_open))
            .then(|| self.report_plan(ctx, state))
            .flatten();
        let recorded: Result<Recorded, ModelFailure> = ctx
            .step(&format!("model:{turn}"), move || async move {
                if !named.is_empty() {
                    request.image_data = image_data(
                        named.iter().collect(),
                        images_store.as_ref(),
                        &images_scope,
                        &images_state,
                    )
                    .await;
                }
                // The sources are read here, inside the step, so that a replay of a turn whose model
                // call is recorded does not read them again: only the answer is journaled, not the
                // tools it was given.
                let (notes, own_order) =
                    apply_sources(&sources, source_ctx.as_ref(), &mut request, ordered).await;
                // The reports are appended last: after the history, which is what the request
                // before this one started with.
                let (reports, sessions_open) = match report_plan {
                    Some(plan) => plan.tell(&mut request).await,
                    None => (Vec::new(), None),
                };
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
                        own_order,
                        reports,
                        sessions_open,
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
                    own_order,
                    reports,
                    sessions_open,
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
        let mut seen_at: usize;
        let own_order: Vec<String>;
        let (response, stream) = match recorded {
            Ok(Recorded {
                response,
                stream,
                notes,
                seen,
                call: _,
                own_order: own,
                reports,
                sessions_open,
            }) => {
                seen_at = seen.unwrap_or(state.messages.len());
                own_order = own;
                if let Some(open) = sessions_open {
                    state.sessions_open = open;
                }
                // The provider took every image this request carried.
                state.note_images_sent(sent);
                // The calls this turn asks for are the ones its listing described.
                state.source_notes = notes;
                // An answer recorded before a message that this transition read did not read it.
                answered_stale = seen.is_some_and(|seen| seen < state.messages.len());
                // What the model was told goes into the history in front of its answer. An answer
                // that did not read a message that arrived is not the last word, and nor is its
                // listing: the reports are left for the next call, which finds them again.
                // The steps taken do not depend on it (a replay that finds the answer stale must take
                // the ones the first try took): the rounds are counted either way, and only the
                // history waits.
                seen_at += self.told(ctx, state, &reports, !answered_stale).await?;
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
            // The provider rejected the prompt as too long: the history is compacted once, in the
            // next transition, and the turn is tried again.
            Err(f) if f.context_length => return Ok(self.rejected_as_too_long(state, &f.message)),
            // The provider refused the request itself: if images no earlier request carried are in
            // it, they may be why.
            Err(f) if f.refused => {
                return Ok(Self::refused_with_images(state, &carried, &f.message));
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
        // What the provider counted of the prompt this call sent: the next estimate starts from it.
        state.last_prompt = (usage.input_tokens > 0).then_some(PromptSize {
            input_tokens: usage.input_tokens,
            messages: seen_at,
        });
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
        let (calls, ordered_calls) = ordered_by_the_model(calls, &own_order);
        state.pending_calls = calls;
        state.ordered_calls = ordered_calls;
        Ok(Flow::Next)
    }

    /// The budget of the history, in tokens (see [`compaction::budget`]).
    fn history_budget(&self) -> Option<usize> {
        compaction::budget(
            self.model.context_window(&self.model_alias),
            self.limits.max_output_tokens,
            self.limits.max_history_tokens,
        )
    }

    /// Characters a request holds besides the history: the instructions and the tools.
    fn fixed_chars(&self) -> usize {
        self.instructions
            .as_deref()
            .map_or(0, |s| s.chars().count())
            + serde_json::to_string(&self.shown_specs()).map_or(0, |s| s.chars().count())
    }

    /// The agent's own specs as a request shows them: with `after_previous` above a limit of one.
    fn shown_specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.specs.clone();
        order::prepare(&mut specs, self.max_parallel > 1);
        specs
    }

    /// The estimate of the next request, in tokens.
    fn estimated_tokens(&self, state: &Conversation) -> usize {
        compaction::estimate(
            &state.messages,
            state.last_prompt.as_ref(),
            self.fixed_chars(),
        )
    }

    /// Where a compaction of `state` would cut, if it could: fewer than two turns to drop, or
    /// [`MAX_COMPACTIONS`] already made, is none (the history is shortened instead).
    fn cut_for(&self, state: &Conversation, reason: CompactionReason) -> Option<compaction::Cut> {
        if state.compactions.iter().filter(|c| !c.carried).count() >= MAX_COMPACTIONS {
            return None;
        }
        let estimate = self.estimated_tokens(state);
        let limit = match (reason, self.history_budget()) {
            (CompactionReason::Budget, budget) => budget.unwrap_or(estimate),
            (CompactionReason::Overflow, budget) => budget.unwrap_or(estimate).min(estimate),
        };
        compaction::cut_point(&state.messages, limit)
    }

    /// Whether the committed state is due for a compaction: the estimate has reached the budget, and
    /// nothing says otherwise (a compaction at this very turn, a summary that failed a few turns ago).
    fn compaction_due(&self, state: &Conversation) -> bool {
        let Some(budget) = self.history_budget() else {
            return false;
        };
        state
            .compactions
            .last()
            .is_none_or(|c| c.carried || c.turn != state.turns)
            && state
                .compact_failed
                .is_none_or(|at| state.turns >= at.saturating_add(RETRY_AFTER_TURNS))
            && self.estimated_tokens(state) >= budget
            // A compaction that leaves the request over the budget even with no summary would be
            // made again at once: the history is shortened instead.
            && self
                .cut_for(state, CompactionReason::Budget)
                .is_some_and(|cut| self.projected_tokens(state, cut.index) < budget)
    }

    /// The least the request holds after a cut at `at`: the instructions, the tools, the task and
    /// the turns that stay. The marker, whatever it holds, comes on top.
    fn projected_tokens(&self, state: &Conversation, at: usize) -> usize {
        compaction::projected(&state.messages, at, self.fixed_chars())
    }

    /// What a model call that the provider refused for itself does. The images admitted since the last
    /// call that succeeded are recorded as lines, once, and the turn is tried again; images an earlier
    /// request carried successfully are never touched. With no such image the refusal is the failure.
    fn refused_with_images(state: &mut Conversation, carried: &[String], message: &str) -> Flow {
        if state.replace_unsent_images(carried, &|image: &adam_model::ImageRef| {
            adam_runtime::SystemLine::NotShown.line(&format!(
                "{}: the model's service refused the request that carried it",
                image.label()
            ))
        }) {
            return Flow::Next;
        }
        Flow::Fail(format!("model call failed: {message}"))
    }

    /// What a model call whose prompt the provider rejected as too long does: ask for one compaction
    /// and a second try, the first time at this turn, and fail the run the second.
    fn rejected_as_too_long(&self, state: &mut Conversation, message: &str) -> Flow {
        if state.overflow_turn == Some(state.turns) {
            let first = state
                .compactions
                .iter()
                .rev()
                .find(|c| c.reason == CompactionReason::Overflow)
                .and_then(|c| c.rejected.clone())
                .unwrap_or_default();
            return Flow::Fail(format!(
                "model call failed: context length exceeded after compacting the history: \
                 {message} (the first rejection of this turn was: {first})"
            ));
        }
        if self.cut_for(state, CompactionReason::Overflow).is_none() {
            return Flow::Fail(format!("model call failed: {message}"));
        }
        state.overflow_turn = Some(state.turns);
        state.overflow_message = Some(message.chars().take(1000).collect());
        state.compact_next = Some(CompactionReason::Overflow);
        Flow::Next
    }

    /// Compact the history of the committed `state` when it is due (or when a rejected prompt asked
    /// for it), and say whether it did: the transition then ends there, and the model is asked in the
    /// next one. Only with no call and no wait open. A summary that cannot be made leaves the history
    /// to be shortened (`false`), except after a rejection, which drops the turns without one.
    async fn compact_if_due(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        output_id_of: &(dyn Fn(&str) -> Option<String> + Sync),
    ) -> Result<bool, AgentError> {
        let reason = match state.compact_next.take() {
            Some(reason) => reason,
            None => {
                if !state.pending_calls.is_empty()
                    || state.pending_wait.is_some()
                    || state.limit.is_some()
                    || !self.compaction_due(state)
                {
                    return Ok(false);
                }
                CompactionReason::Budget
            }
        };
        let Some(cut) = self.cut_for(state, reason) else {
            return Ok(false);
        };
        let n = state.compactions.len();
        let before_tokens = self.estimated_tokens(state);
        // One id for each attempt: a summary that failed is tried again at a later turn.
        let id = format!("compact:{n}:{}", state.turns);
        let step = |state_of: StepState| {
            StepEvent::new(&id, StepKind::Tool, "Summarise the conversation", state_of)
                .with_icon(StepIcon::Think)
        };
        ctx.emit(RunEvent::Step(step(StepState::Running))).await;

        let mut request = ModelRequest::new(self.model_alias.clone());
        request.system = self.instructions.clone();
        // The summary request is the request before it and one more message, so a provider's cache
        // serves it: its images are the ones the last request sent. A history that a provider
        // rejected as too long is sent shorter, with each image said in a few words.
        let image_limits = self.model.input_modalities(&self.model_alias).images;
        request.messages = match reason {
            CompactionReason::Budget => for_sending(&state.messages, image_limits.as_ref()),
            CompactionReason::Overflow => {
                described(&shortened_for_overflow(&state.messages[..cut.index]))
            }
        };
        request.messages.push(Message::user_text(COMPACTION_TURN));
        request.tools = self.specs.clone();
        request.tool_choice = ToolChoice::None;
        request.prompt_cache_key = ctx.conversation_id().map(prompt_cache_key);
        request.max_output_tokens = Some(SUMMARY_OUTPUT_TOKENS);
        let model = self.model.clone();
        let ordered = self.max_parallel > 1;
        let sources = self.sources.clone();
        let source_ctx = (!sources.is_empty()).then(|| {
            SourceCtx::new(
                ctx.run_id(),
                state.root_run.unwrap_or_else(|| ctx.run_id()),
                ctx.conversation_id().map(str::to_owned),
                Arc::new(state.context.clone()),
            )
        });
        let run = ctx.run_id();
        let cancel = ctx.cancel_token();
        let at = cut.index;
        let named: Vec<adam_model::ImageRef> = request
            .messages
            .iter()
            .flat_map(Message::images)
            .cloned()
            .collect();
        let images_store = self.extensions.get::<ImageStore>().map(|s| (*s).clone());
        let images_scope = image_scope(
            ctx.conversation_id(),
            &state.root_run.unwrap_or_else(|| ctx.run_id()).to_string(),
        );
        let images_state = Arc::clone(&state.images);
        let recorded: Result<Summarised, ModelFailure> = ctx
            .step(&id, move || async move {
                if !named.is_empty() {
                    request.image_data = image_data(
                        named.iter().collect(),
                        images_store.as_ref(),
                        &images_scope,
                        &images_state,
                    )
                    .await;
                }
                apply_sources(&sources, source_ctx.as_ref(), &mut request, ordered).await;
                let answer = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(ModelFailure::cancelled()),
                    answer = model.complete(request) => answer.map_err(ModelFailure::from)?,
                };
                let text = answer.message.text();
                Ok(Summarised {
                    summary: (!text.trim().is_empty()).then_some(text),
                    cut: at,
                    usage: answer.usage.accounted(),
                    call: compaction_call_id(run, n),
                })
            })
            .await?;
        let (summary, cut_at, rejected) = match recorded {
            Ok(done) => {
                // The call was made and counts, whatever its summary is worth.
                self.report_usage(ctx, state, done.call.clone(), done.usage)
                    .await;
                state.usage = state.usage.saturating_add(done.usage);
                state
                    .usage_totals
                    .add(self.model.provider(), &self.model_alias, done.usage);
                (done.summary, done.cut, state.overflow_message.take())
            }
            Err(f) if f.retryable => {
                let error = AgentError::transient(format!("summary call failed: {}", f.message));
                return Err(match f.retry_after_ms {
                    Some(ms) => error.with_retry_after(Duration::from_millis(ms)),
                    None => error,
                });
            }
            // The summary could not be made. A prompt that was rejected still has to shrink, so its
            // turns go without one when the summary request was itself too long; a history that only
            // neared its budget is shortened instead, and this is not tried again for a few turns.
            Err(f) if reason == CompactionReason::Overflow && f.context_length => {
                (None, cut.index, state.overflow_message.take())
            }
            // Any other failure would fail the next call too, with the history gone: the run fails
            // with the summary call's own class and message instead.
            Err(f) if reason == CompactionReason::Overflow => {
                let message = format!(
                    "model call failed: the summary after a prompt rejected as too long failed \
                     ({}): {}",
                    f.class.as_deref().unwrap_or("unclassified"),
                    f.message
                );
                ctx.emit(RunEvent::Step(
                    step(StepState::Failed).with_detail(message.clone()),
                ))
                .await;
                return Err(AgentError::permanent(message));
            }
            Err(f) => {
                tracing::warn!(run = %run, "the summary call failed: the history is shortened instead");
                state.compact_failed = Some(state.turns);
                ctx.emit(RunEvent::Step(step(StepState::Failed).with_detail(
                    format!(
                        "no summary could be made ({}); the history is shortened instead",
                        f.message
                    ),
                )))
                .await;
                return Ok(false);
            }
        };
        // The recorded cut is the one to apply; it must still be a turn start of this history.
        let valid = cut_at >= 1
            && cut_at < state.messages.len()
            && matches!(state.messages[cut_at], Message::Assistant { .. });
        if !valid {
            state.compact_failed = Some(state.turns);
            return Ok(false);
        }
        compaction::apply(
            state,
            cut_at,
            summary.as_deref(),
            reason,
            rejected,
            output_id_of,
        );
        // The images of the turns that were summarised away are not kept for anything any more.
        state.drop_unnamed_images();
        let dropped = state.compactions.last().map_or(0, |c| c.dropped_turns);
        ctx.emit(RunEvent::Step(step(StepState::Completed).with_detail(
            format!(
                "Summarised {dropped} earlier turns (the history was about {before_tokens} tokens)"
            ),
        )))
        .await;
        Ok(true)
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

    /// What the model call needs to be told the reports of ended rounds, if this agent has session
    /// tools to read them with.
    fn report_plan(&self, ctx: &Ctx, state: &Conversation) -> Option<ReportPlan> {
        if self.session_agents.is_empty() {
            return None;
        }
        let tool = self.tool(SUBAGENT_READ)?.clone();
        let tool_ctx = self.tool_ctx(
            ctx,
            &state.context,
            (
                state.root_run,
                state.root_step.as_deref(),
                state.plan.as_ref(),
            ),
            &state.source_notes,
            "report",
            SUBAGENT_READ,
        );
        let in_history = state
            .messages
            .iter()
            .flat_map(|m| m.tool_calls())
            .filter(|c| c.name == SUBAGENT_READ && c.id.starts_with("report_"))
            .map(|c| c.id.clone())
            .collect();
        Some(ReportPlan {
            paged: tool.paged(),
            tool,
            tool_ctx,
            starter: ctx.child_starter(),
            me: ctx.run_id(),
            agents: Arc::clone(&self.session_agents),
            reported: state.reported_rounds.clone(),
            in_history,
            since: state.chain_started,
        })
    }

    /// Put the reports a model call was given into the history, in front of the answer: the call
    /// and result of each read, as a request carried them, said as steps. The model has been told
    /// them, so they are not told again, and the tokens of their rounds are counted as a read
    /// counts them. Returns how many messages it added.
    async fn told(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        reports: &[RoundReport],
        append: bool,
    ) -> Result<usize, AgentError> {
        if reports.is_empty() {
            return Ok(0);
        }
        // The tokens of the rounds are counted whether or not the history takes the reports, so
        // that the steps of a replay are the steps of the first try.
        Self::count_rounds(ctx, state, false).await?;
        if !append {
            // Left for the next call, which finds them again: its listing stays on.
            state.sessions_open = true;
            return Ok(0);
        }
        let messages = report_messages(reports, self.pages(SUBAGENT_READ));
        let added = messages.len();
        for report in reports {
            let id = report_call_id(report.round);
            ctx.emit(self.step_event(SUBAGENT_READ, &id, StepState::Running, &[]))
                .await;
            let content = backstop(
                &report.output.content,
                SUBAGENT_READ,
                self.pages(SUBAGENT_READ),
            );
            ctx.emit(self.step_end(
                SUBAGENT_READ,
                &id,
                StepState::Completed,
                Some((&content, false)),
                &[],
            ))
            .await;
        }
        state.messages.extend(messages);
        let rounds: Vec<RunId> = reports.iter().map(|r| r.round).collect();
        state.note_reported(&rounds);
        Ok(added)
    }

    /// The session, if any, that this run started in this task and whose round still works, or
    /// whose round ended and was not reported (a round a person started included), read
    /// from the store in a journaled step so that a replay and a takeover decide the same: the
    /// model has answered, and the run may not end while this one works, nor before its report
    /// is read. `None` for an agent that has no session tools. A failure to list is a retry when the store says so, and no wait
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
        // uses sessions journals what it always did. A listing that saw a session open
        // ([`Conversation::sessions_open`]) is a reason to ask too: a round may end that a person
        // started.
        if !has_started_session(state) && !state.sessions_open {
            return Ok(None);
        }
        let starter = ctx.child_starter();
        let me = ctx.run_id();
        let agents = self.session_agents.clone();
        let reports = self.session_reports;
        let reported = state.reported_rounds.clone();
        let since = state.chain_started;
        let in_history: HashSet<String> = state
            .messages
            .iter()
            .flat_map(|m| m.tool_calls())
            .filter(|c| c.name == SUBAGENT_READ && c.id.starts_with("report_"))
            .map(|c| c.id.clone())
            .collect();
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
                    .find(|s| {
                        (does_work(s) && s.started_by == Some(me))
                            || (reports
                                && is_due(
                                    s,
                                    &DueFacts {
                                        me,
                                        reported: &reported,
                                        in_history: &in_history,
                                        since,
                                    },
                                ))
                    })
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
                        .filter(|s| does_work(s) && s.started_by == Some(me))
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

    /// Run the owed tool calls, in segments (ADR 0049): a call that runs alone, one the model asked to
    /// follow the ones before it or that repeats the call before it, is a segment of its own and runs
    /// as one journaled step `tool:<call id>`; the calls between two of those run at the same time,
    /// as far as their [`Tool::access`] allows and up to [`max_parallel`](LlmAgentBuilder::max_parallel),
    /// each as its own journaled step at a seq reserved in call order. Results are applied in call order
    /// either way.
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
            // The calls that run beside this one: none unless the schedule puts several together.
            let together = self.together(state);
            if together >= 2 {
                match self.run_segment(ctx, state, together).await? {
                    Some(flow) => return Ok(flow),
                    None => continue,
                }
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
            // A round may now end and be reported to the model.
            if self.session_reports && matches!(call.name.as_str(), SUBAGENT_START | SUBAGENT_SEND)
            {
                state.sessions_open = true;
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
                        images: self.image_gate(state),
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
                    pinned,
                    kept_images,
                } => {
                    if pinned {
                        state.pinned_calls.push(call.id.clone());
                    }
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
                    state.keep_tool_images(kept_images);
                    if !reported.is_empty() {
                        state.note_reported(&reported);
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

    /// What the call shares with the other calls of its turn: the tool's own declaration
    /// ([`Tool::access`]), a source's ([`ToolSource::access`]), and [`ToolAccess::Alone`] for what the
    /// loop forces: a tool that asks the person, the session tools, and (through the default) any
    /// tool that does not declare. A call that takes no step (bad arguments, a tool nobody has) shares
    /// nothing.
    fn access_of(&self, call: &ToolCall) -> ToolAccess {
        if call.invalid_arguments.is_some() {
            return ToolAccess::Independent;
        }
        match self.tool(&call.name) {
            Some(tool) => {
                if tool.asks_user() || is_session_tool(&call.name) {
                    ToolAccess::Alone
                } else {
                    tool.access(&call.arguments)
                }
            }
            None if self.sources.is_empty() => ToolAccess::Independent,
            None => self
                .sources
                .iter()
                .map(|source| source.access(&call.name, &call.arguments))
                .find(|access| !access.is_alone())
                .unwrap_or(ToolAccess::Alone),
        }
    }

    /// How many of the owed calls, from the first, run together as one segment: `1` when the first
    /// runs by itself (the common case: a tool that declares nothing runs alone).
    fn together(&self, state: &Conversation) -> usize {
        let calls = &state.pending_calls;
        if self.max_parallel <= 1 || calls.len() < 2 {
            return 1;
        }
        let mut before = state.repeats.as_ref().map(|r| r.key.clone());
        let entries: Vec<Entry> = calls
            .iter()
            .map(|call| {
                let key = call_key(call);
                // The doom-loop count of a call needs the result of the one before it.
                let barrier = before.as_deref() == Some(key.as_str())
                    || state.ordered_calls.contains(&call.id);
                before = Some(key);
                Entry {
                    access: self.access_of(call),
                    barrier,
                }
            })
            .collect();
        match schedule::segments(&entries, self.max_parallel).first() {
            Some(Segment::Together(range)) if range.start == 0 => range.end,
            _ => 1,
        }
    }

    /// Run the first `n` owed calls at the same time ([`schedule`]): reserve their journal steps in
    /// call order, drive them in this task, and apply the results in call order. `Some(flow)` ends the
    /// transition (a cancel); `None` means the segment is applied and the loop goes on.
    async fn run_segment(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        n: usize,
    ) -> Result<Option<Flow>, AgentError> {
        let calls: Vec<ToolCall> = state.pending_calls[..n].to_vec();
        let notes = state.source_notes.clone();
        let accesses: Vec<ToolAccess> = calls.iter().map(|c| self.access_of(c)).collect();
        let shortcuts: Vec<Option<String>> = calls
            .iter()
            .map(|c| self.shortcut(c, state.output_cut))
            .collect();

        // One journal step for each call that runs a tool, at seqs in call order: the ones a
        // sequential loop would take.
        let takes_step: Vec<bool> = shortcuts.iter().map(Option::is_none).collect();
        let names: Vec<String> = calls
            .iter()
            .zip(&takes_step)
            .filter(|(_, takes)| **takes)
            .map(|(c, _)| format!("tool:{}", c.id))
            .collect();
        let mut steps = ctx.reserve_steps(&names).await?.into_iter();
        let base = steps.as_slice().first().map_or(0, ReservedStep::seq);
        let seqs = schedule::step_seqs(base, &takes_step);

        // What each call is given, built from the state as the segment begins: the budgets of the
        // segment (room for kept outputs, the image gate) are one, shared by its calls.
        let used = files_kept(&state.artifacts);
        let root = (
            state.root_run,
            state.root_step.as_deref(),
            state.plan.as_ref(),
        );
        let mut first: Option<ToolCtx> = None;
        let mut slots: Vec<Option<(Option<Prepared>, Option<ReservedStep>)>> =
            Vec::with_capacity(n);
        for (i, call) in calls.iter().enumerate() {
            if shortcuts[i].is_some() {
                slots.push(Some((None, None)));
                continue;
            }
            let facts = CallFacts {
                files_used: used,
                output_cut: state.output_cut,
                kept: Arc::clone(&state.kept_outputs),
                kept_left: MAX_RUN_KEPT_BYTES.saturating_sub(state.kept_bytes()),
                images: self.image_gate(state),
            };
            let mut one = self.prepare(ctx, &state.context, root, &notes, call, facts);
            match &first {
                Some(head) => one.tool_ctx = one.tool_ctx.sharing_budgets_of(head),
                None => first = Some(one.tool_ctx.clone()),
            }
            let step = steps.next().ok_or_else(|| {
                AgentError::permanent("a call of a segment has no reserved step".to_owned())
            })?;
            debug_assert_eq!(step.name(), format!("tool:{}", call.id));
            debug_assert_eq!(
                Some(step.seq()),
                seqs[i],
                "the seq a sequential loop gives call {i}"
            );
            slots.push(Some((Some(one), Some(step))));
        }

        let emitter = ctx.emitter();
        let cancel = ctx.cancel_token();
        let mut queue = Queue::new(&accesses, self.max_parallel);
        let mut running = FuturesUnordered::new();
        let mut finished: Ordered<Finished> = Ordered::new(n);
        let mut failure: Option<AgentError> = None;
        let mut cancelled = false;
        let mut grace: Option<Pin<Box<tokio::time::Sleep>>> = None;
        loop {
            if !cancelled && cancel.is_cancelled() {
                cancelled = true;
                queue.stop();
                grace = Some(Box::pin(tokio::time::sleep(TOOL_STOP_GRACE)));
            }
            for i in queue.ready() {
                let Some((prepared, step)) = slots[i].take() else {
                    continue;
                };
                running.push(
                    self.drive(
                        i,
                        &calls[i],
                        &notes,
                        &emitter,
                        shortcuts[i].clone(),
                        prepared,
                        step,
                    )
                    .boxed(),
                );
            }
            if running.is_empty() {
                break;
            }
            tokio::select! {
                biased;
                () = cancel.cancelled(), if !cancelled => {}
                () = async {
                    match grace.as_mut() {
                        Some(sleep) => sleep.await,
                        None => std::future::pending().await,
                    }
                } => {
                    // The calls that were running did not stop in time: their futures are dropped.
                    return Ok(Some(Flow::Fail(CANCELLED.into())));
                }
                Some((i, done)) = running.next() => {
                    queue.finish(i);
                    match done {
                        Ok(one) => finished.put(i, one),
                        Err(error) => {
                            // No call starts after a failure; the running ones are waited for.
                            // A permanent error outranks a transient one, whichever came first:
                            // retrying would only repeat it.
                            let outranks = matches!(&failure, Some(AgentError::Transient { .. }))
                                && !matches!(&error, AgentError::Transient { .. });
                            if failure.is_none() || outranks {
                                failure = Some(error);
                            }
                            queue.stop();
                        }
                    }
                }
            }
        }
        if cancelled {
            return Ok(Some(Flow::Fail(CANCELLED.into())));
        }
        if let Some(error) = failure {
            return Err(error);
        }
        let results = finished.into_ordered().ok_or_else(|| {
            AgentError::permanent("a call of a segment did not finish".to_owned())
        })?;

        // The results are applied in the order of the calls, whatever order they finished in.
        for (call, done) in calls.iter().zip(results) {
            let key = call_key(call);
            let (result, reported) = match done {
                Finished::Said(said) => (ToolResult::error(&call.id, said), SandboxUsage::new()),
                Finished::Ran {
                    outcome,
                    unjournaled,
                    live,
                } => {
                    self.conclude(
                        ctx,
                        &notes,
                        call,
                        *outcome,
                        &unjournaled,
                        files_kept(&state.artifacts),
                        Some(live),
                    )
                    .await?
                }
            };
            state.sandbox_usage.merge(&reported);
            let ToolResult::Answered {
                message,
                artifacts,
                answer,
                repeat_key,
                plan,
                kept,
                reported,
                pinned,
                kept_images,
            } = result
            else {
                return Err(AgentError::permanent(
                    "a call of a segment of several was left unanswered".to_owned(),
                ));
            };
            if pinned {
                state.pinned_calls.push(call.id.clone());
            }
            if let Some(answer) = answer {
                state.announce(answer);
            }
            record_answer(state, key, &message, repeat_key);
            if let Some(plan) = plan {
                state.plan = Some(*plan);
            }
            state.keep_output(kept);
            state.keep_tool_images(kept_images);
            if !reported.is_empty() {
                state.note_reported(&reported);
                Self::count_rounds(ctx, state, false).await?;
            }
            state.messages.push(message);
            add_artifact_refs(&mut state.artifacts, artifacts);
            state.pending_calls.remove(0);
        }
        Ok(None)
    }

    /// One call of a segment, from its start to its finish: say that it runs, run its step, and say
    /// how it ended, **as it happens**; applying the result to the run is left to the caller, in call
    /// order. An `Err` is a failure of the transition (a transient tool error, the journal).
    #[allow(clippy::too_many_arguments)] // one call's pieces, handed over once
    async fn drive(
        &self,
        i: usize,
        call: &ToolCall,
        notes: &[ToolNote],
        emitter: &Emitter,
        shortcut: Option<String>,
        prepared: Option<Prepared>,
        step: Option<ReservedStep>,
    ) -> (usize, Result<Finished, AgentError>) {
        (
            i,
            self.drive_one(call, notes, emitter, shortcut, prepared, step)
                .await,
        )
    }

    #[tracing::instrument(skip_all, fields(run = %emitter.run_id(), tool = %call.name, call_id = %call.id))]
    async fn drive_one(
        &self,
        call: &ToolCall,
        notes: &[ToolNote],
        emitter: &Emitter,
        shortcut: Option<String>,
        prepared: Option<Prepared>,
        step: Option<ReservedStep>,
    ) -> Result<Finished, AgentError> {
        let silent = self
            .source_note(notes, &call.name)
            .is_some_and(|n| n.reports_step);
        say_with(emitter, silent, self.start_event(call, notes)).await;
        if let Some(said) = shortcut {
            say_with(
                emitter,
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
            return Ok(Finished::Said(said));
        }
        let (Some(prepared), Some(step)) = (prepared, step) else {
            return Err(AgentError::permanent(
                "a call of a segment has no step to run".to_owned(),
            ));
        };
        let unjournaled = prepared.unjournaled.clone();
        let work = self.step_work(call, self.source_note(notes, &call.name), prepared);
        let outcome: Result<ToolOutput, ToolError> = step.run(move || work).await?;
        // How it ended, said now; `conclude` says it again only if applying the result changes it.
        let live = match &outcome {
            // Rounds are reported only by a call that runs alone (the loop reads them in a step of
            // its own, which a segment cannot place between the calls).
            Ok(output) if reports_rounds(output) => {
                say_with(
                    emitter,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((NOT_ALONGSIDE, true)),
                        notes,
                    ),
                )
                .await;
                Live { said: None }
            }
            Ok(output) => {
                let state = if output.is_error {
                    StepState::Failed
                } else {
                    StepState::Completed
                };
                let text =
                    backstop(&output.content, &call.name, self.pages(&call.name)).into_owned();
                say_with(
                    emitter,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        state,
                        Some((&text, output.is_error)),
                        notes,
                    ),
                )
                .await;
                Live {
                    said: Some((state, text)),
                }
            }
            Err(ToolError::Permanent(reason)) => {
                say_with(
                    emitter,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((reason, true)),
                        notes,
                    ),
                )
                .await;
                Live { said: None }
            }
            Err(ToolError::Transient(reason)) => {
                say_with(
                    emitter,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((reason, true)),
                        notes,
                    ),
                )
                .await;
                return Err(AgentError::transient(format!(
                    "tool `{}` failed: {reason}",
                    call.name
                )));
            }
            // A call that asks or waits cannot run beside others: the model is told to call it
            // alone (a tool that can do it must say `Alone`).
            Err(_) => {
                say_with(
                    emitter,
                    silent,
                    self.step_end(
                        &call.name,
                        &call.id,
                        StepState::Failed,
                        Some((NOT_ALONGSIDE, true)),
                        notes,
                    ),
                )
                .await;
                Live { said: None }
            }
        };
        Ok(Finished::Ran {
            outcome: Box::new(outcome),
            unjournaled,
            live,
        })
    }

    /// What the model is told for a call that takes no journal step: its arguments were not JSON, or
    /// nobody has the tool and the agent has no source to ask. `None` for a call that runs a tool.
    fn shortcut(&self, call: &ToolCall, output_cut: bool) -> Option<String> {
        if let Some(raw) = &call.invalid_arguments {
            if call.arguments.as_str() == Some(order::BAD_ORDER_MARKER) {
                return Some(order::bad_value(&call.name, raw));
            }
            return Some(invalid_arguments(&call.name, raw, output_cut));
        }
        // A name that is none of the agent's own: an agent with no source says so at once, one with
        // sources asks them, inside the step that records the answer.
        // Consequence: this records NOTHING, so an agent with no source whose journal holds a
        // `tool:<id>` step for a tool it no longer has (the tool was removed between two versions)
        // would fail replay (a recorded step with no step asked for it: non-determinism). An agent
        // with sources answers an unknown tool inside the journaled step, and replays. The coder
        // always installs sources, which is what makes ADR 0038's removal of a tool safe.
        (self.tool(&call.name).is_none() && self.sources.is_empty())
            .then(|| self.unknown_tool(&call.name))
    }

    /// The report that starts the step of `call`: what the call was given (and, with a title the tool
    /// has for itself, what to call it: `Tool::step_style`).
    fn start_event(&self, call: &ToolCall, notes: &[ToolNote]) -> RunEvent {
        let mut start =
            self.style_of(&call.name, notes)
                .event(&call.name, &call.id, StepState::Running);
        if let Some((input, max)) = self.step_io.input(&call.arguments) {
            start = start.with_input_within(input, max);
        }
        RunEvent::Step(start)
    }

    /// What a call needs to run, made from the `Ctx` while it can still be read.
    fn prepare(
        &self,
        ctx: &Ctx,
        context: &Map<String, Value>,
        root: (Option<RunId>, Option<&str>, Option<&Plan>),
        notes: &[ToolNote],
        call: &ToolCall,
        facts: CallFacts,
    ) -> Prepared {
        let CallFacts {
            files_used,
            kept,
            kept_left,
            images,
            ..
        } = facts;
        let note = self.source_note(notes, &call.name);
        let root_run = root.0.unwrap_or_else(|| ctx.run_id()).to_string();
        let tool_ctx = self
            .tool_ctx(ctx, context, root, notes, &call.id, &call.name)
            .with_note(note.cloned())
            .with_files_left((MAX_RUN_FILE_BYTES as u64).saturating_sub(files_used))
            .with_kept(kept_left, kept)
            .with_images(
                images,
                image_scope(ctx.conversation_id(), &root_run),
                root_run,
            );
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
            )
        });
        Prepared {
            tool_ctx,
            holding,
            // What the call reported as sandbox usage rides in its result, which the step journals; a
            // call that ends in an error has no result to carry it, and leaves it here.
            unjournaled: Arc::new(Mutex::new(SandboxUsage::new())),
        }
    }

    /// The work of the journaled step `tool:<call id>`, which runs only if nothing is recorded: the
    /// call to the agent's own tool or to a source's, within its time limit, after the check of what a
    /// source holds back.
    fn step_work(
        &self,
        call: &ToolCall,
        note: Option<&ToolNote>,
        prepared: Prepared,
    ) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
        let Prepared {
            tool_ctx,
            holding,
            unjournaled,
        } = prepared;
        let args = call.arguments.clone();
        let name = call.name.clone();
        let own = self.tool(&call.name).cloned();
        let sources = self.sources.clone();
        let unknown = self.unknown_tool(&call.name);
        let source_limit = note
            .and_then(|n| n.timeout_ms)
            .map_or(DEFAULT_TOOL_TIME_LIMIT, |ms| {
                Duration::from_millis(ms) + SOURCE_LIMIT_MARGIN
            });
        async move {
            if let Some((sources, source_ctx)) = &holding
                && let Some(held) = withheld(sources, source_ctx, &[name.as_str()])
                    .await
                    .into_iter()
                    .next()
            {
                return Ok(ToolOutput::error(held.reason));
            }
            let called = match own {
                Some(tool) => {
                    let limit = tool.time_limit().unwrap_or(DEFAULT_TOOL_TIME_LIMIT);
                    within(&tool_ctx, &name, limit, |ctx| async move {
                        tool.call(&ctx, args).await
                    })
                    .await
                }
                None => {
                    let tool_name = name.clone();
                    within(&tool_ctx, &name, source_limit, |ctx| async move {
                        for source in &sources {
                            if let Some(answer) = source.call(&ctx, &tool_name, args.clone()).await
                            {
                                return answer;
                            }
                        }
                        Ok(ToolOutput::error(unknown))
                    })
                    .await
                }
            };
            with_reported_sandbox(&tool_ctx, &unjournaled, called)
        }
        .boxed()
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
        let files_used = facts.files_used;
        // The system behind the source reports each call itself: the agent reports no step of its own.
        let silent = self
            .source_note(notes, &call.name)
            .is_some_and(|n| n.reports_step);
        say(ctx, silent, self.start_event(call, notes)).await;

        // Arguments that were not JSON, or a tool nobody has: nothing runs, and the model is told
        // why. The answer comes from the recorded model turn alone, so a replay gives the same one.
        if let Some(said) = self.shortcut(call, facts.output_cut) {
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
            return Ok((ToolResult::error(&call.id, said), SandboxUsage::new()));
        }
        let prepared = self.prepare(ctx, context, root, notes, call, facts);
        let unjournaled = prepared.unjournaled.clone();
        let work = self.step_work(call, self.source_note(notes, &call.name), prepared);
        let outcome: Result<ToolOutput, ToolError> =
            ctx.step(&format!("tool:{}", call.id), move || work).await?;
        self.conclude(ctx, notes, call, outcome, &unjournaled, files_used, None)
            .await
    }

    /// What the call's recorded outcome means for the run: the message that answers it, its artifacts
    /// and what it asks the run to keep, or a wait. The step's end is said here, unless `live` says
    /// the call's end was already said as it happened and applying the result did not change it.
    /// `live` is `Some` for a call of a segment of several, which cannot ask or wait.
    #[allow(clippy::too_many_arguments)] // the call's pieces, handed over once
    async fn conclude(
        &self,
        ctx: &Ctx,
        notes: &[ToolNote],
        call: &ToolCall,
        outcome: Result<ToolOutput, ToolError>,
        unjournaled: &Mutex<SandboxUsage>,
        files_used: u64,
        live: Option<Live>,
    ) -> Result<(ToolResult, SandboxUsage), AgentError> {
        let silent = self
            .source_note(notes, &call.name)
            .is_some_and(|n| n.reports_step);
        let said_live = |state: StepState, text: Option<&str>| match &live {
            Some(Live {
                said: Some((live_state, live_text)),
            }) => *live_state == state && Some(live_text.as_str()) == text,
            Some(Live { said: None }) => true,
            None => false,
        };
        let end = |state: StepState| self.step_event(&call.name, &call.id, state, notes);
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
            Ok(output) if live.is_some() && reports_rounds(&output) => {
                ToolResult::error(&call.id, NOT_ALONGSIDE.to_owned())
            }
            Ok(mut output) => {
                let repeat_key = output.repeat_key.as_deref().cloned();
                let reported = if output.is_error {
                    Vec::new()
                } else {
                    std::mem::take(&mut output.reported)
                };
                let pinned = output.pinned && !output.is_error;
                let (message, artifacts, answer, plan, outcome, kept, kept_images) =
                    output_message(
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
                if !said_live(outcome, said.map(|(text, _)| text)) {
                    say(
                        ctx,
                        silent,
                        self.step_end(&call.name, &call.id, outcome, said, notes),
                    )
                    .await;
                }
                ToolResult::Answered {
                    message,
                    artifacts,
                    answer,
                    repeat_key,
                    plan,
                    kept,
                    reported,
                    pinned,
                    kept_images,
                }
            }
            Err(ToolError::Permanent(reason)) => {
                if live.is_none() {
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
                }
                ToolResult::error(&call.id, backstop(&reason, &call.name, false).into_owned())
            }
            Err(ToolError::Transient(reason)) => {
                if live.is_none() {
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
                }
                return Err(AgentError::transient(format!(
                    "tool `{}` failed: {reason}",
                    call.name
                )));
            }
            // Beside other calls a question or a wait has nowhere to park: the model is told to call
            // the tool alone (its end was said as it happened).
            Err(
                ToolError::NeedsInput { .. }
                | ToolError::AwaitRun { .. }
                | ToolError::AwaitSession { .. }
                | ToolError::AwaitRemote { .. },
            ) if live.is_some() => ToolResult::error(&call.id, NOT_ALONGSIDE.to_owned()),
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
                ..
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
                if output.pinned && !output.is_error {
                    state.pinned_calls.push(wait.call_id.clone());
                }
                if !reported.is_empty() {
                    state.note_reported(&reported);
                    Self::count_rounds(ctx, state, false).await?;
                }
                let used = files_kept(&state.artifacts);
                let (message, artifacts, answer, plan, outcome, kept, kept_images) =
                    output_message(
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
                state.keep_tool_images(kept_images);
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
    Vec<KeptImage>,
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
        output
            .content
            .push_str(&crate::tool::files_not_shared(&refused, files_used));
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
    // The images of a result that failed are not shown: the person's screen and the model read the
    // text, and an image nobody chose to send is not kept.
    let (images, kept_images) = if output.is_error {
        (Vec::new(), Vec::new())
    } else {
        (
            std::mem::take(&mut output.images),
            std::mem::take(&mut output.kept_images).into_vec(),
        )
    };
    let message = Message::Tool {
        call_id: call_id.to_owned(),
        // After the journal: the journal holds the whole output, and the cut (which the history, the
        // model and the step event carry) is a pure function of it.
        content: backstop(&output.content, tool, paged).into_owned(),
        is_error: output.is_error,
        images,
    };
    (message, refs, answer, plan, outcome, kept, kept_images)
}

/// The first user message of a run, as a fresh [`Conversation`]. Shared by
/// [`LlmAgent::init`] and [`LlmStarter::init`], so they cannot drift apart.
fn start_conversation(input: Inbound) -> Result<Conversation, AgentError> {
    let text = start_text(&input)?;
    let mut conversation = Conversation::new(text);
    conversation.pending_images = parse_inbound_images(&input.payload);
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
    conversation.pending_images = parse_inbound_images(&input.payload);
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

impl ToolResult {
    /// An answer that is an error result with `text`, and nothing else.
    fn error(call_id: &str, text: String) -> Self {
        Self::Answered {
            message: Message::tool_error(call_id.to_owned(), text),
            artifacts: Vec::new(),
            answer: None,
            repeat_key: None,
            plan: None,
            kept: None,
            reported: Vec::new(),
            pinned: false,
            kept_images: Vec::new(),
        }
    }
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
        /// The result is pinned ([`ToolOutput::pinned`]), from a result that is not an error.
        pinned: bool,
        /// The bytes of the images the result returns, for the run to keep in its own state (only
        /// when the agent has no image store), from a result that is not an error.
        kept_images: Vec<KeptImage>,
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

/// The calls of a turn as the loop runs them: `after_previous` taken out of each (the tool, its access, the
/// doom-loop key, the hooks and the step's input never see it), and the ids of the calls that said `true`.
/// A call whose value is not a boolean does not run: it becomes a call whose arguments are
/// [`order::BAD_ORDER_MARKER`] and whose `invalid_arguments` hold the value, which `shortcut` answers. A tool in `own` has its own `after_previous`: its calls are left as they
/// are. The history keeps what the model said.
fn ordered_by_the_model(calls: Vec<ToolCall>, own: &[String]) -> (Vec<ToolCall>, Vec<String>) {
    let mut barriers = Vec::new();
    let calls = calls
        .into_iter()
        .map(|mut call| {
            if call.invalid_arguments.is_some() || own.contains(&call.name) {
                return call;
            }
            match order::take(&mut call.arguments) {
                order::Order::Plain => {}
                order::Order::After => barriers.push(call.id.clone()),
                order::Order::Bad(value) => {
                    call.invalid_arguments = Some(value);
                    call.arguments = Value::String(order::BAD_ORDER_MARKER.into());
                }
            }
            call
        })
        .collect();
    (calls, barriers)
}

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
        // The history is shortened here, in the state, from the state as it was committed: before
        // this step appends a message that arrived since (an inbox message, a deferred one, the
        // answer to a question), so a replay after a crash shortens the same way whatever arrived
        // in between, and each request starts with the one before it. What arrives counts toward the
        // budget one turn later. `state.turns` is the turn of the next request.
        // A pointer to a kept output stays through a cut only for the id this very call kept
        // (`kept::output_id`: the same one `keep_long` and the coder's tools write).
        let root = state.root_run.unwrap_or_else(|| ctx.run_id());
        let run = ctx.run_id();
        let output_id_of = |call_id: &str| {
            let key = if run == root {
                call_id.to_owned()
            } else {
                format!("{run}/{call_id}")
            };
            Some(crate::kept::output_id(&root.to_string(), &key))
        };
        // A history that has reached the budget is summarised first, at a journaled step of its own;
        // the model is asked in the next transition, from the new history.
        if self.compact_if_due(ctx, &mut state, &output_id_of).await? {
            return Ok(Transition::Continue(state));
        }
        if record_shortenings(
            &mut state.messages,
            self.limits.max_history_tokens,
            &output_id_of,
        ) {
            state.shortenings.push(state.turns);
            state.last_prompt = None;
            // The images of a result that was shortened left the history with it.
            state.drop_unnamed_images();
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
        // The images that came with a message are admitted and kept in journaled steps of their
        // own, before the message is read: it is read from the references those steps recorded.
        let admitted = self.admit_images(ctx, &mut state, &inbox).await?;
        let absorbed = self.absorb_inbox(&mut state, inbox, &admitted);
        for (call_id, refusal) in &absorbed.refusals {
            ctx.emit(RunEvent::Custom {
                kind: "answer_refused".into(),
                payload: json!({
                    "call_id": call_id,
                    "question": refusal.subject.as_deref().unwrap_or("*"),
                    "rule": refusal.rule,
                }),
            })
            .await;
        }
        if absorbed.waiting {
            // Woken without an answer (nothing usable arrived), or the answer was refused and the
            // tool asked again: keep waiting.
            return Ok(Transition::Park {
                state,
                wake_at: None,
            });
        }
        // A copy of a message the run has read, delivered again while this step runs, is not a
        // reason for another turn.
        ctx.read_already(&state.read_ids);
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
        // The order the model asked for is the turn's: once its calls are answered it says nothing.
        if state.pending_calls.is_empty() {
            state.ordered_calls.clear();
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
            own_order: Vec::new(),
            reports: Vec::new(),
            sessions_open: None,
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
            own_order: Vec::new(),
            reports: Vec::new(),
            sessions_open: None,
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
            own_order: Vec::new(),
            reports: Vec::new(),
            sessions_open: None,
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
            own_order: Vec::new(),
            reports: Vec::new(),
            sessions_open: None,
        };
        let json = serde_json::to_value(&reported).unwrap();
        assert_eq!(json["call"], "run-c0-a1b2c3d4");
        let back: Recorded = serde_json::from_value(json).unwrap();
        assert_eq!(back.call.as_deref(), Some("run-c0-a1b2c3d4"));

        // The reports a request was given (ADR 0045, slice 3) are beside the response, and a record
        // without them, which every journal from before slice 3 is, reads as none.
        assert!(old.reports.is_empty());
        assert_eq!(old.sessions_open, None);
        assert!(!plain_json_has_reports(&reported));
        let round = RunId::new();
        let told = Recorded {
            response: ModelResponse::text("hi"),
            stream: None,
            notes: Vec::new(),
            seen: Some(4),
            call: None,
            own_order: Vec::new(),
            reports: vec![RoundReport {
                session: RunId::new(),
                round,
                output: ToolOutput::text("session s: round 1\nreport: done\n"),
            }],
            sessions_open: Some(true),
        };
        let json = serde_json::to_value(&told).unwrap();
        assert_eq!(json["sessions_open"], true);
        let back: Recorded = serde_json::from_value(json).unwrap();
        assert_eq!(back.reports.len(), 1);
        assert_eq!(back.reports[0].round, round);
        assert_eq!(back.sessions_open, Some(true));
    }

    fn plain_json_has_reports(record: &Recorded) -> bool {
        let json = serde_json::to_value(record).unwrap();
        json.get("reports").is_some() || json.get("sessions_open").is_some()
    }

    /// The report of a round is appended as a call and its result, the same bytes to the request
    /// and to the history, under an id made from the round.
    #[test]
    fn a_report_is_a_read_call_and_its_result() {
        let (session, round) = (RunId::new(), RunId::new());
        let reports = vec![RoundReport {
            session,
            round,
            output: ToolOutput::text("the words"),
        }];
        let messages = report_messages(&reports, true);
        assert_eq!(messages.len(), 2);
        let id = report_call_id(round);
        assert!(id.starts_with("report_") && id.len() == 7 + 32, "{id}");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        match &messages[0] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, SUBAGENT_READ);
                assert_eq!(tool_calls[0].id, id);
                assert_eq!(tool_calls[0].arguments["session"], session.to_string());
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            &messages[1],
            Message::Tool { call_id, content, is_error: false, .. }
                if *call_id == id && content == "the words"
        ));
        assert_eq!(
            serde_json::to_string(&messages).unwrap(),
            serde_json::to_string(&report_messages(&reports, true)).unwrap()
        );
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

#[cfg(test)]
mod compaction_gate_tests {
    use super::*;
    use crate::compaction::Compaction;
    use adam_model::MockModel;

    fn agent(max_history_tokens: u32) -> LlmAgent {
        let model: DynModel = Arc::new(MockModel::new());
        LlmAgent::builder("t", model, "m")
            .limits(Limits {
                max_history_tokens,
                ..Limits::default()
            })
            .build()
    }

    /// The budget of a request counts what the loop adds above a limit of one: the property in each
    /// schema and the sentence of the instructions.
    #[test]
    fn the_fixed_part_of_a_request_counts_the_injected_order() {
        let built = |limit: usize| {
            let model: DynModel = Arc::new(MockModel::new());
            LlmAgent::builder("t", model, "m")
                .instructions("be brief")
                .max_parallel(limit)
                .tool(crate::FnTool::raw(
                    "a",
                    "a",
                    json!({"type": "object", "properties": {}}),
                    |_ctx, _args| async { Ok::<_, crate::ToolError>(crate::ToolOutput::text("a")) },
                ))
                .build()
        };
        let one = built(1).fixed_chars();
        let ten = built(10).fixed_chars();
        let mut specs = built(10).specs.clone();
        order::prepare(&mut specs, true);
        let injected = serde_json::to_string(&specs).unwrap().chars().count()
            - serde_json::to_string(&built(10).specs)
                .unwrap()
                .chars()
                .count();
        assert!(injected > 0);
        assert_eq!(
            ten - one,
            injected + order::INSTRUCTION.chars().count() + 2,
            "the property and the sentence (after a blank line) are counted"
        );
    }

    /// A history of `turns` turns whose results are `size` letters, and `turns` model turns made.
    fn history(turns: usize, size: usize) -> Conversation {
        let mut state = Conversation::new("task");
        for n in 0..turns {
            state.messages.push(Message::Assistant {
                content: vec![],
                tool_calls: vec![adam_model::ToolCall {
                    id: format!("c{n}"),
                    name: "t".into(),
                    arguments: json!({}),
                    ..Default::default()
                }],
                reasoning: None,
            });
            state
                .messages
                .push(Message::tool_result(format!("c{n}"), "x".repeat(size)));
        }
        state.turns = u32::try_from(turns).unwrap();
        state
    }

    fn reported(state: &mut Conversation, input_tokens: u64) {
        state.last_prompt = Some(PromptSize {
            input_tokens,
            messages: state.messages.len(),
        });
    }

    fn made(turn: u32) -> Compaction {
        Compaction {
            turn,
            dropped_turns: 2,
            reason: CompactionReason::Budget,
            summarised: true,
            sessions: false,
            rejected: None,
            skills: vec![],
            pointers: vec![],
            pinned: vec![],
            carried: false,
        }
    }

    fn carried(turn: u32) -> Compaction {
        Compaction {
            carried: true,
            ..made(turn)
        }
    }

    #[test]
    fn a_history_that_has_reached_the_budget_exactly_is_due() {
        let agent = agent(2000);
        let mut state = history(12, 40);
        reported(&mut state, 1999);
        assert!(!agent.compaction_due(&state));
        reported(&mut state, 2000);
        assert!(agent.compaction_due(&state));
    }

    #[test]
    fn no_second_compaction_is_due_at_the_turn_of_the_first() {
        let agent = agent(2000);
        let mut state = history(12, 40);
        reported(&mut state, 5000);
        state.compactions = vec![made(state.turns)];
        assert!(!agent.compaction_due(&state));
        state.compactions = vec![made(state.turns - 1)];
        assert!(agent.compaction_due(&state));
        // What an earlier run compacted at a turn of that number says nothing about this one.
        state.compactions = vec![carried(state.turns)];
        assert!(agent.compaction_due(&state));
    }

    #[test]
    fn a_failed_summary_is_tried_again_after_eight_turns() {
        let agent = agent(2000);
        let mut state = history(12, 40);
        reported(&mut state, 5000);
        state.compact_failed = Some(state.turns - (RETRY_AFTER_TURNS - 1));
        assert!(!agent.compaction_due(&state));
        state.compact_failed = Some(state.turns - RETRY_AFTER_TURNS);
        assert!(agent.compaction_due(&state));
    }

    #[test]
    fn eight_compactions_are_the_most_a_run_makes() {
        let agent = agent(2000);
        let mut state = history(12, 40);
        reported(&mut state, 5000);
        state.compactions = vec![made(0); MAX_COMPACTIONS - 1];
        assert!(agent.cut_for(&state, CompactionReason::Budget).is_some());
        assert!(agent.cut_for(&state, CompactionReason::Overflow).is_some());
        state.compactions = vec![made(0); MAX_COMPACTIONS];
        assert!(agent.cut_for(&state, CompactionReason::Budget).is_none());
        assert!(agent.cut_for(&state, CompactionReason::Overflow).is_none());
        // The record a continued run carries is not one of its own eight.
        state.compactions = vec![made(0); MAX_COMPACTIONS - 1];
        state.compactions.insert(0, carried(0));
        assert!(agent.cut_for(&state, CompactionReason::Budget).is_some());
        assert!(agent.cut_for(&state, CompactionReason::Overflow).is_some());
        state.compactions.push(made(0));
        assert!(agent.cut_for(&state, CompactionReason::Budget).is_none());
        assert!(agent.cut_for(&state, CompactionReason::Overflow).is_none());
    }

    #[test]
    fn a_rejection_cuts_by_the_estimate_when_it_is_smaller_than_the_budget() {
        let agent = agent(100_000);
        let state = history(12, 400);
        let estimate = agent.estimated_tokens(&state);
        assert!(estimate < 25_000, "{estimate}");
        let cut = agent.cut_for(&state, CompactionReason::Overflow).unwrap();
        assert_eq!(
            Some(cut),
            compaction::cut_point(&state.messages, estimate),
            "a quarter of what the history is, not of what it may be"
        );
        let by_budget = agent.cut_for(&state, CompactionReason::Budget).unwrap();
        assert!(cut.dropped_turns > by_budget.dropped_turns);
    }

    #[test]
    fn a_compaction_that_would_leave_the_request_over_the_budget_is_not_due() {
        // Each turn is about a thousand tokens, and the newest one stays.
        let mut state = history(12, 4000);
        let over = agent(900);
        reported(&mut state, 20_000);
        assert!(over.estimated_tokens(&state) >= 900);
        assert!(over.cut_for(&state, CompactionReason::Budget).is_some());
        assert!(
            !over.compaction_due(&state),
            "the tail alone is over the budget"
        );
        let room = agent(1100);
        assert!(room.compaction_due(&state), "the tail fits");
    }
}
