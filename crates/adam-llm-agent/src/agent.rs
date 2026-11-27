//! [`LlmAgent`]: the durable model <-> tools loop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_budget::{Admission, Ask, ConversationId, DynAdmission, Permit, Pool, Refused};
use adam_core::time::Timestamp;
use adam_core::turn::DynTurnKeys;
use adam_core::{RunId, RunStatus};
use adam_error::report;
use adam_model::{
    Classify, DynModel, FinishReason, Message, ModelError, ModelRequest, ModelResponse,
    PromptStatement, ReasoningEffort, ToolCall, ToolChoice, ToolSpec,
};
use adam_plugin::{
    DeclarationError, EventSpec, HookSet, HookedCall, HookedCompaction, HookedEnd, HookedMessage,
    HookedModelRequest, HookedText, MessageFile, ModelCallKind, Plugin, Verdict,
};
use adam_runtime::{
    AGENT_TEXT_KIND, Agent, AgentError, AgentStarter, CHILD_PARKED_KIND, CHILD_REPORTED_KIND,
    CancelToken, ChildLook, ChildStarter, ChildStatus, Ctx, Emitter, Inbound, MAX_RUN_FILE_BYTES,
    MAX_SESSION_PAGE, RUN_FINISHED_KIND, ReservedStep, RetryPolicy, RunEvent, SESSION_CLOSE_KIND,
    SandboxUsage, SessionState, SessionSummary, StepEvent, StepIcon, StepKind, StepState,
    Transition, UsageEvent, UsageTotals, child_run_id,
};
use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::access::ToolAccess;
use crate::compaction::{
    self, COMPACTION_TURN, CompactionReason, MAX_COMPACTIONS, OVERFLOW_OUTPUT_CHARS, PromptSize,
    RETRY_AFTER_TURNS,
};
use crate::conversation::{
    ArtifactRef, Conversation, LOCALE_CONTEXT_KEY, LimitHit, LimitKind, Loaded, Loader,
    MODEL_CONTEXT_KEY, ModelSwitch, PendingQuestion, PendingRemote, PendingRun, PendingSession,
    PendingWait, REASONING_EFFORT_CONTEXT_KEY, Repeats, asks_to_close, parse_context,
    parse_model_run, parse_prompt_statement, parse_root_run, parse_root_step, parse_session,
    parse_user_text,
};
use crate::events::EventRegistry;
use crate::history::{RunChars, record_shortenings_from};
use crate::images::{
    ImageCache, ImageGate, ImageStore, InboundImage, Item, KeptImage,
    MAX_RUN_JOURNALED_IMAGE_BYTES, admit_inbound, answer_with, described, for_sending, image_data,
    image_scope, image_step, parse_inbound_images, parts_of, state_bytes,
};
use crate::kept::{KeptOutput, MAX_RUN_KEPT_BYTES};
use crate::model_wait::{
    MODEL_RESUME_KIND, ModelPark, ModelParking, ModelWait, RecordedPark, backoff, detail_of,
    reason_of,
};
use crate::order;
use crate::paging::backstop;
use crate::plan::Plan;
use crate::schedule::{self, Entry, Ordered, Queue, Segment};
use crate::session_tools::{SessionNarrowing, narrowing_of};
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
    narrowing: &SessionNarrowing,
    request: &mut ModelRequest,
    inject: bool,
) -> Sourced {
    let (notes, offered, carried) = match source_ctx {
        Some(source_ctx) => offered_by(sources, source_ctx, narrowing, request).await,
        None => {
            // No sources, but a session's `tools` and `deny_tools` hold all the same.
            narrowing.retain(&mut request.tools);
            (Vec::new(), Vec::new(), Vec::new())
        }
    };
    let own = order::prepare(&mut request.tools, inject);
    Sourced {
        notes,
        own_order: own,
        offered,
        carried,
    }
}

/// What [`apply_sources`] read of the request: the notes of the sources, the tools whose schema has
/// `after_previous`, and the loaded tools the request carried **before** and **after** a source
/// withheld any. The two sets differ when the access tier hides a loaded tool on this message:
/// that is not a removal, so only the final set decides a growth, and only the first decides what
/// the run no longer holds.
struct Sourced {
    notes: Vec<ToolNote>,
    own_order: Vec<String>,
    offered: Vec<String>,
    carried: Vec<String>,
}

/// The loaded tools `request` carries, in the run's load order.
fn carried_loaded(source_ctx: &SourceCtx, request: &ModelRequest) -> Vec<String> {
    source_ctx
        .loaded()
        .iter()
        .flat_map(|one| one.tools.iter())
        .filter(|tool| request.tools.iter().any(|spec| spec.name == tool.as_str()))
        .cloned()
        .collect()
}

/// The heading a session's `instructions` are rendered under (ADR 0054, decision 4): they follow the
/// folder's own text, and the text above wins — which is what the heading says.
const SESSION_INSTRUCTIONS_HEADING: &str = "## Instructions from the parent agent or the person\n\n\
They are added to the instructions above, which take precedence over them.\n\n";

/// `system` with the session's `instructions` after it, when the round's header carries any: a
/// session's text never replaces the folder's, it follows it under the heading.
fn with_session_instructions(
    system: Option<String>,
    config: Option<&adam_runtime::SessionConfig>,
) -> Option<String> {
    let Some(text) = config
        .and_then(|c| c.instructions.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return system;
    };
    Some(match system {
        Some(own) if !own.trim().is_empty() => {
            format!("{own}\n\n{SESSION_INSTRUCTIONS_HEADING}{text}")
        }
        _ => format!("{SESSION_INSTRUCTIONS_HEADING}{text}"),
    })
}

/// The sources' part of [`apply_sources`], and what a session's narrowing holds back.
async fn offered_by(
    sources: &[DynToolSource],
    source_ctx: &SourceCtx,
    narrowing: &SessionNarrowing,
    request: &mut ModelRequest,
) -> (Vec<ToolNote>, Vec<String>, Vec<String>) {
    let (more, notes) = offered(sources, source_ctx, &request.tools).await;
    request.tools.extend(more);
    // What a source has already given the run during itself goes at the end, after everything:
    // a load appends to the tools array and never inserts into it, so the request before is a byte
    // prefix of this one (ADR 0044, decision 1). The copy it had in place is taken out.
    let appended = appended_by(sources, source_ctx).await;
    if !appended.is_empty() {
        let names: Vec<&str> = appended.iter().map(|spec| spec.name.as_str()).collect();
        request
            .tools
            .retain(|spec| !names.contains(&spec.name.as_str()));
        request.tools.extend(appended);
    }
    refined(sources, source_ctx, &mut request.tools).await;
    // What the sources still hold, before the tier holds anything back: a tool it hides on this
    // message is not one the run lost, while a tool a group no longer holds is. A source that does
    // not track its loaded tools falls back to what the request carried before the withhold.
    let offered = match crate::source::loaded_offered(sources, source_ctx).await {
        Some(tools) => tools,
        None => carried_loaded(source_ctx, request),
    };
    // What a source holds back is not shown, the agent's own tools included.
    let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
    let held = withheld(narrowing, sources, source_ctx, &names).await;
    if !held.is_empty() {
        request
            .tools
            .retain(|t| !held.iter().any(|h| h.tool == t.name));
    }
    let carried = carried_loaded(source_ctx, request);
    // What the sources add to the instructions (the agents a person mentioned).
    if let Some(more) = instructed(sources, source_ctx).await {
        request.system = Some(match request.system.take() {
            Some(own) if !own.trim().is_empty() => format!("{own}\n\n{more}"),
            _ => more,
        });
    }
    (notes, offered, carried)
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
    /// Why a `compaction.before` hook refused the summary (ADR 0050, slice H9): the history is
    /// shortened instead, and the reason is what the step says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vetoed: Option<String>,
}

/// The id of the summary call `n` of `run` in its usage report: made inside the journaled step, like
/// [`call_id`], and apart from every model turn's.
fn compaction_call_id(run: RunId, n: usize) -> String {
    let mut suffix = uuid::Uuid::new_v4().simple().to_string();
    suffix.truncate(8);
    format!("{run}-k{n}-{suffix}")
}

/// The id of the resume a run forwards to the child `child` when the inbound `waking` woke it
/// ([ADR 0067] decision 5): derived from both, so a step that runs again writes the same id, and a
/// child that receives the copy drops it (its `read_ids`) and makes no call.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
fn forwarded_resume_id(waking: &str, child: RunId) -> String {
    format!("{}:{waking}:{child}", crate::MODEL_RESUME_KIND)
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
                extra_content,
            } => {
                let len = content.chars().count();
                let short = (len > OVERFLOW_OUTPUT_CHARS)
                    .then(|| {
                        crate::history::shorten_output(content, len - OVERFLOW_OUTPUT_CHARS, None)
                    })
                    .flatten();
                Message::Tool {
                    extra_content: extra_content.clone(),
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

/// Whether a model turn recomputes the loaders: every run's first turn (so a dev reload's new
/// catalogue replaces a stale one), and any turn whose state has none (a journal written before
/// they existed).
fn loaders_needed(turns: u32, loaders: &[Loader]) -> bool {
    turns == 0 || loaders.is_empty()
}

/// Whether a call of `tool` acts on a session (an effect outside the run's own state).
fn has_effect(tool: &str) -> bool {
    matches!(tool, SUBAGENT_START | SUBAGENT_SEND | SUBAGENT_CLOSE)
}

/// Whether a round of the session is doing work somebody will want to read. A round that only
/// closes the session is not: it calls no model, ends by itself and reports nothing new, so a run
/// neither waits for it at the end of a turn nor cancels it when it ends.
fn does_work(session: &SessionSummary) -> bool {
    session.does_work()
}
use crate::source::{
    DynToolSource, SourceCtx, ToolNote, ToolSource, appended_by, instructed, offered, refined,
    withheld,
};
use crate::state::Extensions;
use crate::step_io::StepIo;
use crate::text_stream;
use crate::tool::{
    AnswerCheck, DEFAULT_TOOL_TIME_LIMIT, DynTool, HookDecision, HookedOutput, MessageDecision,
    Refusal, RemotePoll, StepStyle, TOOL_STOP_GRACE, Tool, ToolCtx, ToolError, ToolOutput,
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

/// The result a tool that asks the person gets in a run nobody answers, a firing of a schedule (a
/// conversation under `adam.schedule:`, ADR 0060): the run cannot wait for an answer, so the model
/// is told to go on without one.
pub const UNATTENDED: &str = "nobody answers this run: a schedule started it, not a person, so nothing \
can be asked. Do what you can without asking, and say in your final answer what is still needed";

/// The result a tool that asks the person gets in a child run that may not ask (ADR 0045 decision
/// 9): only a round of a kept-open session that a client shows (`child-sessions/v1`) has somebody
/// to answer it, so the model is told to go on and say what it needs in its report.
pub const NOT_ASKABLE: &str = "nobody can answer this run: only a kept-open subagent session whose \
client shows child sessions may ask the person. Do what you can without asking, and say in your \
final answer what is still needed";

/// The result of a call a `before_tool` hook asked a person to approve, in a run nobody can approve
/// it in: a child run that may not ask the person (ADR 0045 decision 9) or a firing of a schedule.
/// Such a run reads `Ask` as `Deny` (ADR 0050, decision 12, slice H8), and the step records a `deny`.
pub const NOT_APPROVABLE: &str = "a hook asks a person to approve this call, and nobody can approve \
it in this run: the call is not run. Do what you can without it, and say in your final answer what \
is still needed";

/// The result of a call a hook asked a person to approve while it ran beside other calls of its
/// turn: the run cannot wait on one of several, so the model is told to call the tool alone.
const APPROVAL_NOT_ALONGSIDE: &str = "a hook asks a person to approve this call, which cannot wait \
for an answer beside other calls: the call is not run. Call the tool alone to have it approved";

/// The journaled step that runs a call a person approved (ADR 0050, decision 12, slice H8):
/// `tool:<call id>:approved`. It is taken only after a recorded `Ask` and the person's yes, so its
/// seq never depends on which plugins are configured; a replay that finds it recorded runs nothing.
#[must_use]
pub fn approval_step(call_id: &str) -> String {
    format!("tool:{call_id}:approved")
}

/// Whether a person's answer to an approval is a yes: one of `yes`, `y`, `approve`, `approved`,
/// `allow`, `ok`, `okay`, without regard to case, surrounding space or a final `.` or `!`. Anything
/// else is a no (ADR 0050, decision 12): a call runs only on a clear yes, and the words of any other
/// answer are the reason the call is refused with.
#[must_use]
pub fn approves(answer: &str) -> bool {
    let word = answer
        .trim()
        .trim_end_matches(['.', '!'])
        .trim()
        .to_lowercase();
    matches!(
        word.as_str(),
        "yes" | "y" | "approve" | "approved" | "allow" | "ok" | "okay"
    )
}

/// The question a person reads for an approval: the hook's own words, then what to answer.
fn approval_question(tool: &str, question: &str) -> String {
    format!(
        "{question}\n\nApprove the call to `{tool}`? Answer yes to run it; any other answer \
         refuses it."
    )
}

/// What the model reads for a call the person did not approve: the refusal, with their words.
fn not_approved(answer: &str) -> String {
    let answer = answer.trim();
    if answer.is_empty() {
        "not run: the person did not approve the call".to_owned()
    } else {
        format!("not run: the person did not approve the call. They answered: {answer}")
    }
}

/// Whether a person can approve a call of the run in `state` (ADR 0050, decision 12, slice H8): it
/// may ask the person (ADR 0045 decision 9) and somebody answers it (it is no firing of a schedule).
fn approvable(ctx: &Ctx, state: &Conversation) -> bool {
    state.may_ask_person(ctx.conversation_id())
        && !is_unattended(ctx.conversation_id(), Some(&state.context))
}

/// Whether nobody answers a run: a firing of a schedule (its conversation is under
/// `adam.schedule:`), or a child of one (its context carries `adam_runtime::UNATTENDED_CONTEXT_KEY`,
/// which the scheduler sets and children inherit).
pub fn is_unattended(conversation_id: Option<&str>, context: Option<&Map<String, Value>>) -> bool {
    conversation_id.is_some_and(|c| c.starts_with(adam_runtime::SCHEDULE_CONVERSATION_PREFIX))
        || context.is_some_and(|c| {
            c.get(adam_runtime::UNATTENDED_CONTEXT_KEY) == Some(&Value::Bool(true))
        })
}

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
    /// What the run's session narrows (ADR 0054 slice 3), checked **before** the sources' holds and
    /// whether or not the agent has sources.
    narrowing: Arc<SessionNarrowing>,
    unjournaled: Arc<Mutex<SandboxUsage>>,
    /// A person can approve a call of this run ([`CallFacts::approvable`]).
    approvable: bool,
}

/// What a tool call's context is made from: the `Ctx` it is taken from does not exist inside
/// [`Ctx::step`], where a call that starts while the model writes is prepared.
#[derive(Clone)]
struct CtxFacts {
    run: RunId,
    conversation_id: Option<String>,
    attempt: u32,
    emitter: Emitter,
    cancel: CancelToken,
    child_starter: ChildStarter,
}

impl CtxFacts {
    /// The six things a call's context needs from the transition's `Ctx`.
    fn of(ctx: &Ctx) -> Self {
        Self {
            run: ctx.run_id(),
            conversation_id: ctx.conversation_id().map(str::to_owned),
            attempt: ctx.attempt(),
            emitter: ctx.emitter(),
            cancel: ctx.cancel_token(),
            child_starter: ctx.child_starter(),
        }
    }
}

/// How a call of a segment ended.
enum Finished {
    /// The call took no step (bad arguments, a tool nobody has): what the model is told.
    Said(String),
    /// The recorded outcome of the call's step.
    Ran {
        outcome: Box<Result<HookedOutput, ToolError>>,
        unjournaled: Arc<Mutex<SandboxUsage>>,
        live: Live,
    },
}

/// One call of a segment, as the drive loop takes it.
enum Slot {
    /// The call takes no step: the model is told why.
    Shortcut,
    /// The call runs in this transition's task.
    Fresh(Box<Prepared>, ReservedStep),
    /// The call started while the model wrote: its work is done (or still running), and its step
    /// records the outcome it gave.
    Started(Box<Finished>, ReservedStep),
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
    /// The words the run was started with (`Conversation::task`), for a source that decides from
    /// them: the same text at every turn and at every call.
    task: Option<Arc<str>>,
    /// What the run has loaded so far (`Conversation::loaded`): what a loader reads to refuse a
    /// second load, and what the source that withholds a group's tools reads.
    loaded: Arc<[Loaded]>,
    /// The loaded tools a request did not carry (`Conversation::removed_tools`).
    removed_tools: Arc<[String]>,
    /// The tools the run's session holds back ([`SessionNarrowing`]).
    narrowing: Arc<SessionNarrowing>,
    /// How many custom events the transition's calls have emitted: one counter for all of them.
    event_budget: Arc<AtomicUsize>,
    /// Whether a person can approve a call a hook asks about (ADR 0050, slice H8): the run may ask
    /// the person ([`Conversation::may_ask_person`]) and somebody answers it (not
    /// [`is_unattended`]). When not, an `Ask` is recorded as a `deny`.
    approvable: bool,
}

/// The runs a call reads, owned: the root run, the step of it a child works under, the plan, the run
/// the call's model names ([`model_run_of`]) and the run whose workspace it reads
/// ([`workspace_run_of`]). What [`EarlyFacts`] takes while the model is still writing.
type OwnedRootFacts = (
    Option<RunId>,
    Option<String>,
    Option<Plan>,
    Option<RunId>,
    Option<RunId>,
    Option<PromptStatement>,
);

/// [`OwnedRootFacts`], borrowed from it for one call.
type RootFacts<'a> = (
    Option<RunId>,
    Option<&'a str>,
    Option<&'a Plan>,
    Option<RunId>,
    Option<RunId>,
    Option<&'a PromptStatement>,
);

/// What the calls of a turn that starts while the model writes need from the run, owned: taken
/// from the state and the `Ctx` before [`Ctx::step`], which is where they start.
struct EarlyFacts {
    ctx: CtxFacts,
    /// The step's one shared copy of the run's context: every call of the turn reads the same map.
    context: Option<Arc<Map<String, Value>>>,
    /// The runs the calls read: the root, the step under it, the plan, the run the model calls
    /// name, and the run whose workspace they read ([`workspace_run_of`]).
    root: OwnedRootFacts,
    kept: Arc<BTreeMap<String, String>>,
    kept_left: usize,
    files_used: u64,
    images: ImageGate,
    output_cut: bool,
    task: Option<Arc<str>>,
    /// What the run has loaded as the turn starts, for the calls that read it.
    loaded: Arc<[Loaded]>,
    /// The loaded tools a request did not carry, as the turn starts.
    removed_tools: Arc<[String]>,
    /// The tools the run's session holds back, as the turn starts.
    narrowing: Arc<SessionNarrowing>,
    /// The transition's counter of custom events ([`CallFacts::event_budget`]).
    event_budget: Arc<AtomicUsize>,
    /// Whether a person can approve a call of the turn ([`CallFacts::approvable`]).
    approvable: bool,
    turn: crate::early::Turn,
    /// The committed `repeats` of the run, for the doom-loop decision of the first call.
    repeats: Option<Repeats>,
}

/// The calls of a turn that started while the model was still writing (ADR 0049, amended
/// 2026-11-18): their work runs beside the stream, and the turn hands it to their journaled steps
/// once it is recorded.
///
/// A started call's `running` step event is said when it starts and its end when it finishes, both
/// while the stream goes on; its result is held here until the turn's step is written. Nothing is
/// journaled before the turn: a crash loses the results, and the replay asks the model again and
/// runs the calls again (at-least-once, ADR 0049 decision 8).
pub(crate) struct Early<'a> {
    agent: &'a LlmAgent,
    facts: EarlyFacts,
    /// What the sources said of the tools they offered for this turn.
    notes: Vec<ToolNote>,
    /// The tools whose own schema has an `after_previous`: the loop does not read one for them.
    own_order: Vec<String>,
    /// The calls that started, in the order they started, with their position in the turn.
    started: Vec<(usize, ToolCall)>,
    /// What each started call shares, while it runs: `None` once it has finished.
    accesses: Vec<Option<ToolAccess>>,
    /// What each call the gate refused shares: it runs at the turn (when it runs at all), after
    /// every early call, so a later call that conflicts with it cannot start early either.
    waiting: Vec<ToolAccess>,
    /// Their work, by position in `started`.
    running: FuturesUnordered<BoxFuture<'static, (usize, Finished)>>,
    /// The results that came back while the stream went on, by position in `started`.
    done: Vec<Option<Finished>>,
    /// The context of the first started call: the budgets of a segment are one for all its calls.
    head: Option<ToolCtx>,
    /// The gate of the turn.
    gate: crate::early::Gate,
    /// How many calls of the turn the client announced.
    seen: usize,
    /// A started call failed transiently: no call starts after it (ADR 0049 decision 9).
    failed: bool,
}

impl<'a> Early<'a> {
    fn new(
        agent: &'a LlmAgent,
        facts: EarlyFacts,
        notes: Vec<ToolNote>,
        own_order: Vec<String>,
    ) -> Self {
        let gate = crate::early::Gate::new(
            facts.turn,
            facts.repeats.as_ref().map(|repeats| repeats.key.clone()),
        );
        Self {
            agent,
            facts,
            notes,
            own_order,
            started: Vec::new(),
            accesses: Vec::new(),
            waiting: Vec::new(),
            running: FuturesUnordered::new(),
            done: Vec::new(),
            head: None,
            gate,
            seen: 0,
            failed: false,
        }
    }

    /// The root, root step and plan of the run, as [`prepare`](LlmAgent::prepare) takes them.
    fn root(&self) -> RootFacts<'_> {
        (
            self.facts.root.0,
            self.facts.root.1.as_deref(),
            self.facts.root.2.as_ref(),
            self.facts.root.3,
            self.facts.root.4,
            self.facts.root.5.as_ref(),
        )
    }

    /// Whether a started call is still running.
    pub(crate) fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    /// A call's arguments are whole: start it if the gate admits it, and say that it runs. A call
    /// the gate refuses waits for the turn, exactly as before.
    pub(crate) async fn announce(&mut self, mut call: ToolCall) {
        let index = self.seen;
        self.seen += 1;
        if self.facts.ctx.cancel.is_cancelled() {
            self.gate.cancel();
        }
        if self.failed {
            return;
        }
        // The loop takes `after_previous` out before anything else sees the call; the gate reads it
        // first (on the call as the model sent it) and decides.
        let own = self.own_order.contains(&call.name);
        let after_previous = if own {
            order::Order::Plain
        } else {
            order::take(&mut call.arguments)
        };
        let key = call_key(&call);
        let access = self.agent.access_of(&call);
        let shortcut = self.agent.shortcut(&call, self.facts.output_cut).is_some();
        let doom_loop = index == 0 && doom_loop_would_refuse(self.facts.repeats.as_ref(), &key);
        let conflicts = self
            .accesses
            .iter()
            .flatten()
            .chain(self.waiting.iter())
            .any(|running| running.conflicts(&access));
        let admitted = self.gate.admits(
            index,
            &crate::early::Candidate {
                after_previous,
                shortcut,
                access: &access,
                key: &key,
                doom_loop,
                running: self.running.len(),
                conflicts,
            },
        );
        if !admitted {
            // It runs at the turn, after every early call: a later call that conflicts with it
            // must wait for it too.
            self.waiting.push(access);
            return;
        }
        let silent = self
            .agent
            .source_note(&self.notes, &call.name)
            .is_some_and(|note| note.reports_step);
        say_with(
            &self.facts.ctx.emitter,
            silent,
            self.agent.start_event(&call, &self.notes),
        )
        .await;
        let facts = CallFacts {
            files_used: self.facts.files_used,
            output_cut: self.facts.output_cut,
            kept: Arc::clone(&self.facts.kept),
            kept_left: self.facts.kept_left,
            images: self.facts.images.clone(),
            task: self.facts.task.clone(),
            loaded: Arc::clone(&self.facts.loaded),
            removed_tools: Arc::clone(&self.facts.removed_tools),
            narrowing: Arc::clone(&self.facts.narrowing),
            event_budget: Arc::clone(&self.facts.event_budget),
            approvable: self.facts.approvable,
        };
        let mut prepared = self.agent.prepare(
            &self.facts.ctx,
            self.facts.context.as_ref(),
            self.root(),
            &self.notes,
            &call,
            facts,
        );
        match &self.head {
            Some(head) => prepared.tool_ctx = prepared.tool_ctx.sharing_budgets_of(head),
            None => self.head = Some(prepared.tool_ctx.clone()),
        }
        let i = self.started.len();
        let work = self.agent.early_work(
            i,
            call.clone(),
            self.notes.clone(),
            self.facts.ctx.emitter.clone(),
            prepared,
        );
        self.started.push((index, call));
        self.accesses.push(Some(access));
        self.done.push(None);
        self.running.push(work);
    }

    /// Poll the started calls, keeping the one that finished: the stream goes on while they run.
    pub(crate) async fn next(&mut self) {
        if let Some((i, finished)) = self.running.next().await {
            if let Finished::Ran { outcome, .. } = &finished {
                self.failed |= transient_failure(&self.started[i].1, outcome, None).is_some();
            }
            self.accesses[i] = None;
            self.done[i] = Some(finished);
        }
    }

    /// The results of the started calls, in the order of `calls`, and the context of the first of
    /// them (whose budgets the segment's other calls share); `None` when one of them is not the
    /// turn's call at that position, in which case the batch is dropped whole (it was computed from
    /// calls that no longer exist). Waits for the calls still running, within their own time limits.
    async fn take(
        &mut self,
        calls: &[ToolCall],
    ) -> Option<(Option<ToolCtx>, Vec<Option<Finished>>)> {
        self.finish_all().await;
        let matched = self
            .started
            .iter()
            .all(|(index, call)| calls.get(*index) == Some(call));
        let mut results: Vec<Option<Finished>> = (0..calls.len()).map(|_| None).collect();
        if matched {
            for (i, (index, _)) in self.started.iter().enumerate() {
                results[*index] = self.done[i].take();
            }
        }
        let head = self.head.take();
        self.started.clear();
        self.accesses.clear();
        self.waiting.clear();
        self.done.clear();
        matched.then_some((head, results))
    }

    /// The result of the single call of the turn's first segment, when the segment is one call
    /// (`calls[0]`); `None` when the batch is not exactly it. Either way the batch is forgotten:
    /// a segment of several is taken by [`take`](Self::take).
    async fn take_one(&mut self, call: &ToolCall) -> Option<Finished> {
        self.finish_all().await;
        let taken = match self.started.as_slice() {
            [(0, started)] if started == call => self.done[0].take(),
            _ => None,
        };
        self.started.clear();
        self.accesses.clear();
        self.waiting.clear();
        self.done.clear();
        self.head = None;
        taken
    }

    /// Wait for the calls still running, keeping their results. A cancel is the run's token, the
    /// parent of each call's: the loop waits [`TOOL_STOP_GRACE`] at most for them and then drops
    /// their futures (ADR 0049 decision 10).
    async fn finish_all(&mut self) {
        let cancel = self.facts.ctx.cancel.clone();
        let mut cancelled = cancel.is_cancelled();
        let mut grace: Option<Pin<Box<tokio::time::Sleep>>> =
            cancelled.then(|| Box::pin(tokio::time::sleep(TOOL_STOP_GRACE)) as Pin<Box<_>>);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    grace = Some(Box::pin(tokio::time::sleep(TOOL_STOP_GRACE)));
                }
                () = async {
                    match grace.as_mut() {
                        Some(sleep) => sleep.await,
                        None => std::future::pending().await,
                    }
                } => {
                    // The calls that were running did not stop in time: their futures are dropped.
                    self.running = FuturesUnordered::new();
                    break;
                }
                next = self.running.next() => match next {
                    Some((i, finished)) => {
                        if let Finished::Ran { outcome, .. } = &finished {
                            self.failed |= transient_failure(&self.started[i].1, outcome, None).is_some();
                        }
                        self.accesses[i] = None;
                        self.done[i] = Some(finished);
                    }
                    None => break,
                },
            }
        }
    }

    /// Wait for the calls still running and forget everything: their results are not used.
    async fn drain(&mut self) {
        self.finish_all().await;
        self.started.clear();
        self.accesses.clear();
        self.waiting.clear();
        self.done.clear();
        self.head = None;
    }

    /// The transition ends before the batch is used (a cancel, a stream error, a turn over its
    /// tool-call limit): wait for the calls still running and drop their results.
    async fn settle(&mut self) {
        self.drain().await;
    }
}

/// What the model is asked in the closing turn, the last model call of a run that reached
/// `max_turns` or `max_tool_calls`. Sent after the history, as a user message that is not kept in it.
pub const CLOSING_TURN: &str = "You have used all the steps this run may take. Do not call any \
tool. Reply in plain words: what you did, what is finished, and what is still left to do.";

/// What the model is told about the language the person's interface names (`locale/v1`, ADR 0069).
/// Sent before the closing turn, as a user message that is not kept in the history, and never in
/// `system`: the stable prefix of ADR 0044 keeps its bytes, so a provider's prompt cache still hits,
/// and the language belongs to the message that named it.
pub fn interface_language_turn(tag: &str) -> String {
    format!(
        "The person's interface language is {tag}: answer in it, and follow the person when they \
write in another language. Never translate code, paths or command output."
    )
}

/// The most bytes of a tag this module prints: `adam_a2a::Locale`'s bound, restated here because
/// this crate does not depend on the A2A layer.
pub const MAX_INTERFACE_LANGUAGE_BYTES: usize = 64;

/// The interface language a run's context names ([`LOCALE_CONTEXT_KEY`]), when it is one to print: a
/// string of at most [`MAX_INTERFACE_LANGUAGE_BYTES`] bytes of ASCII letters, digits or `-` (the
/// characters of a BCP 47 `langtag`). A value that is anything else is ignored and nothing is sent, so
/// nothing a tool wrote can become a line of the prompt.
pub fn interface_language(context: &Map<String, Value>) -> Option<&str> {
    context
        .get(LOCALE_CONTEXT_KEY)
        .and_then(Value::as_str)
        .filter(|tag| {
            !tag.is_empty()
                && tag.len() <= MAX_INTERFACE_LANGUAGE_BYTES
                && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// What the model is told after a turn that reached the output limit while it was still thinking:
/// no words and no call, only reasoning. Kept in the history after that turn, except when the
/// next turn is the closing turn.
pub const THINKING_CUT_TURN: &str = "Your last turn reached the output limit while you were still \
thinking, before you wrote anything. Think briefly this time, then call a tool or answer.";

/// How many turns in a row may end at the output limit with reasoning only before the run fails
/// (each is continued with [`THINKING_CUT_TURN`] and counts as a model call against `max_turns`).
/// A turn with words or a tool call starts the count again.
pub const MAX_THINKING_CUTS: u32 = 2;

/// How many times `run.ending` hooks may keep one run going, unless the deployment says otherwise.
pub const DEFAULT_RUN_CONTINUATIONS: u32 = 3;
/// The most a deployment may allow ([`LlmAgentBuilder::max_run_continuations`]).
pub const MAX_RUN_CONTINUATIONS: u32 = 20;

/// What the journaled step `end:<turn>` records: whether the run ended or a hook kept it going.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EndDecision {
    /// `allow`, `continue`, `capped` (a hook asked to go on past the cap), `stop` or `failed`.
    verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plugin: Option<String>,
}

/// Asks the `run.ending` hooks about the answer; a retryable failure fails the try.
fn ask_end(
    hooks: &HookSet,
    text: &str,
    continuations: u32,
    cap: u32,
) -> Result<EndDecision, String> {
    let end = HookedEnd {
        text,
        continuations,
    };
    let one = |verdict: &str, reason: Option<String>, plugin: &str| EndDecision {
        verdict: verdict.into(),
        reason,
        plugin: Some(plugin.into()),
    };
    match hooks.run_ending(&end) {
        Ok(None)
        | Ok(Some(adam_plugin::Decided {
            verdict: Verdict::Continue,
            ..
        })) => Ok(EndDecision {
            verdict: "allow".into(),
            reason: None,
            plugin: None,
        }),
        Ok(Some(d)) => Ok(match d.verdict {
            Verdict::Deny(reason) if continuations >= cap => one("capped", Some(reason), d.plugin),
            Verdict::Deny(reason) => one("continue", Some(reason), d.plugin),
            Verdict::Stop(reason) => one("stop", Some(reason), d.plugin),
            _ => one(
                "failed",
                Some("the hook gave a verdict run.ending does not have".into()),
                d.plugin,
            ),
        }),
        Err(error) if error.is_retryable() => Err(format!("{}: {}", error.plugin, error.reason)),
        Err(error) => Ok(one("failed", Some(error.reason), &error.plugin)),
    }
}

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
    /// Passed to the model as `max_output_tokens`; `0` leaves it to the model. The assembly
    /// resolves it per model from the agent's folder, its `model_preset`, the model's profile and
    /// the 128,000 fallback, then clamps it to the model's limit and window (ADR 0064); a host that
    /// builds an agent by hand gets this default, 4,096, and no resolution or clamp.
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

/// The effort the run's context names ([`REASONING_EFFORT_CONTEXT_KEY`]): the `effort` member of
/// `{"effort": "<level>"}`. A value of another shape, or a word that is not a level, is ignored:
/// the A2A front refuses it before the message is written (ADR 0063).
fn context_effort(context: Option<&Arc<Map<String, Value>>>) -> Option<ReasoningEffort> {
    context?
        .get(REASONING_EFFORT_CONTEXT_KEY)?
        .get("effort")?
        .as_str()?
        .parse()
        .ok()
}

/// The model a run's call may be bound to besides the agent's own, with what its profile resolves
/// for it: the alias, the output cap and the default reasoning level (ADR 0064, amended 2026-11-23).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelBinding {
    /// The gateway alias the request names.
    pub alias: String,
    /// The output cap of a request to it, resolved and clamped as the agent's own is
    /// (`Limits::max_output_tokens`; `0` leaves it to the model).
    pub max_output_tokens: u32,
    /// The reasoning level its requests ask for unless the run's context names one.
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl ModelBinding {
    /// A binding to `alias` with the output cap `max_output_tokens` and no reasoning level.
    pub fn new(alias: impl Into<String>, max_output_tokens: u32) -> Self {
        Self {
            alias: alias.into(),
            max_output_tokens,
            reasoning_effort: None,
        }
    }

    /// The same binding with the default reasoning level `effort`.
    #[must_use]
    pub fn reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }
}

/// The model alias the run's context names ([`MODEL_CONTEXT_KEY`]): the `model` member of
/// `{"model": "<alias>"}`. A value of another shape is ignored.
fn context_model(context: &Map<String, Value>) -> Option<&str> {
    context
        .get(MODEL_CONTEXT_KEY)?
        .get("model")?
        .as_str()
        .filter(|alias| !alias.is_empty())
}

/// The model a model call of a run is made to: its alias, output cap and default reasoning level.
struct LiveModel<'a> {
    alias: &'a str,
    max_output_tokens: u32,
    reasoning_effort: Option<ReasoningEffort>,
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
    /// The endpoint refused the call and its provider parks the run on it
    /// ([`ModelError::Refused`], ADR 0067): its status, slug and wait, and nothing else of the
    /// answer. Absent from journals written before, and for every other failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    park: Option<RecordedPark>,
    /// No connection to the endpoint could be made ([`ModelError::Unreachable`]): a run whose
    /// endpoint parks ([`ModelParking::when_unreachable`]) parks once the step's retries are
    /// spent. Absent from journals written before, which decode as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unreachable: bool,
    /// The endpoint does not allow the model the request named ([`ModelError::ModelLocked`], ADR
    /// 0067 decision 7): the model's alias. A run with a fallback continues on it, once. Absent from
    /// journals written before, and for every other failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    locked: Option<String>,
}

impl ModelFailure {
    /// The retryable class the provider's error had, as recorded (ADR 0061): `RateLimited` or
    /// `Conflict`, else `Transient` (a journal written before classes, or any other name).
    fn retry_class(&self) -> adam_error::ErrorClass {
        match self.class.as_deref() {
            Some("RateLimited") => adam_error::ErrorClass::RateLimited,
            Some("Conflict") => adam_error::ErrorClass::Conflict,
            _ => adam_error::ErrorClass::Transient,
        }
    }

    /// A `model.request` hook ended the call before it was made: `retryable` for a hook failure
    /// that is worth another try, else the run fails with the message.
    fn by_hook(message: String, retryable: bool) -> Self {
        Self {
            retryable,
            message,
            retry_after_ms: None,
            class: None,
            context_length: false,
            refused: false,
            park: None,
            unreachable: false,
            locked: None,
        }
    }

    /// The call was dropped because the run was cancelled. Not retryable: nobody is waiting for it.
    fn cancelled() -> Self {
        Self {
            retryable: false,
            message: CANCELLED.to_owned(),
            retry_after_ms: None,
            class: None,
            context_length: false,
            refused: false,
            park: None,
            unreachable: false,
            locked: None,
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
            park: RecordedPark::of(&e),
            unreachable: matches!(e, ModelError::Unreachable { .. }),
            locked: match &e {
                ModelError::ModelLocked { model } => Some(model.clone()),
                _ => None,
            },
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
    /// The loaded tools the request actually carried **before a source withheld any**, in the order
    /// the run loaded them: the turn transition journals every loaded tool the sources no longer
    /// hold as removed ([`Conversation::removed_tools`](crate::Conversation::removed_tools)), so a
    /// replay decides the same. Absent when nothing was loaded, and in journals written before it
    /// existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    offered: Vec<String>,
    /// The loaded tools the **final** request carried: compared with the previous request's set to
    /// decide whether this turn's request grew (a load, not a tier hide). Absent when nothing was
    /// loaded, and in journals written before it existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    carried: Vec<String>,
    /// The loaders the sources still offered on the turn the request was made: recomputed on every
    /// run's first turn and recorded in [`Conversation::loaders`](crate::Conversation::loaders), so
    /// a dev reload's new catalogue replaces a stale one. `None` on the other turns (nothing to
    /// record), and in journals written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    loaders: Option<Vec<Loader>>,
    /// The groups the **task** loaded, journaled on the run's first turn so a compaction's recap
    /// names them. Absent when there were none, after the first turn, and in journals written
    /// before it existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    task_loaded: Vec<Loaded>,
    /// The final request grew by a loaded group's tools (the tier did not hide all of them): the
    /// turn transition records it in [`Conversation::tools_appended`](crate::Conversation::tools_appended).
    /// Absent when it did not, and in journals written before it existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    grew: bool,
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
        extra_content: None,
        content: Vec::new(),
        tool_calls: calls.clone(),
        reasoning: None,
        provider: None,
    }];
    messages.extend(reports.iter().zip(&calls).map(|(r, call)| Message::Tool {
        extra_content: None,
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
    since: Option<Timestamp>,
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
    tool_retry: BTreeMap<String, RetryPolicy>,
    reasoning_effort: Option<ReasoningEffort>,
    model_parking: ModelParking,
    fallback: Option<ModelBinding>,
    choices: Vec<ModelBinding>,
    admission: Option<DynAdmission>,
    hooks: HookSet,
    max_run_continuations: u32,
    turn_keys: Option<DynTurnKeys>,
    events: Vec<EventSpec>,
}

/// How long a run waiting for a child run sleeps before it looks at the child itself, unless
/// [`LlmAgentBuilder::wait_poll`] says otherwise.
pub const DEFAULT_WAIT_POLL: Duration = Duration::from_secs(60);

/// How long a tool call waits for the process's tool pool ([`LlmAgentBuilder::admission`]) before it
/// runs anyway, with a warning: a run never fails because a slot was busy (ADR 0065 decision 4).
pub const TOOL_ADMISSION_WAIT: Duration = Duration::from_secs(60);

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
    /// A custom event is declared wrongly (ADR 0050, decision 6): a name that is not `ns.name`, a
    /// name in a core namespace, two declarations of one name that differ, or a plugin subscription
    /// that can never match. A binary exits 78 on it.
    #[error("custom events: {0}")]
    Event(#[from] DeclarationError),
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

    /// The retry policy of the tool `name`'s retryable failures, which wins over the tool's own
    /// ([`Tool::retry_policy`]): what an agent folder's `retry:` sets ([ADR 0061]). A tool that
    /// another system offers (an MCP server's `<server>__<tool>`) has no policy of its own, so this
    /// is the only way to give it one. The failures count under the key `tool:<name>`
    /// ([`tool_retry_key`]). A policy for a name the agent never calls is never used.
    ///
    /// [ADR 0061]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0061-a-step-names-its-retry-policy.md
    #[must_use]
    pub fn tool_retry(mut self, name: impl Into<String>, policy: RetryPolicy) -> Self {
        self.tool_retry.insert(name.into(), policy);
        self
    }

    /// The process's pools ([`Admission`], [ADR 0065]): every model call takes a seat of
    /// [`Pool::ModelCalls`] and every tool call one of [`Pool::ToolCalls`], so four runs at once
    /// never start more calls than the deployment allows. `None` (the default) admits everything:
    /// a host that sets no ceiling is the ceiling.
    ///
    /// * A model call waits five seconds for its seat, then parks the run on the refusal's
    ///   `retry_after`, as it parks on a provider's `429` (ADR 0061, ADR 0067).
    /// * A tool call waits up to `TOOL_ADMISSION_WAIT` and then runs anyway, with a warning: a
    ///   run never fails because a slot was busy (ADR 0065 decision 4).
    ///
    /// [ADR 0061]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0061-a-step-names-its-retry-policy.md
    /// [ADR 0065]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    #[must_use]
    pub fn admission(mut self, admission: DynAdmission) -> Self {
        self.admission = Some(admission);
        self
    }

    /// Install a compiled hook set around every tool call ([ADR 0050], slice H1): `before_tool` runs
    /// inside the call's journaled step, after a source's `withhold` and before the tool, and
    /// `after_tool` runs there once the tool answered.
    ///
    /// A hook may rewrite the arguments or the text the model reads, deny the call, ask a person
    /// (until approval is built the call is not run, and its result says it awaits approval), or stop
    /// the run: the call's result is the reason and the run fails `stopped by a plugin: <reason>`.
    /// A hook that fails never lets a call through. The verdict is recorded in the step's record
    /// ([`HookedOutput`](crate::HookedOutput)), so a replay that finds the step recorded asks no hook
    /// again, and adding or removing a plugin never changes a step's name or seq.
    ///
    /// A `before_tool` rewrite that would change what the call touches (`Tool::access` of the new
    /// arguments differs from the one the schedule used, ADR 0049) is refused and the call denied. A
    /// call that takes no step (arguments that are not JSON, a tool nobody has) runs no hook: a
    /// decision is recorded in the step it would have changed, and there is none.
    ///
    /// [ADR 0050]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0050-hooks-around-the-agent-loop-compiled-first-and-programs-second.md
    #[must_use]
    pub fn hooks(mut self, hooks: HookSet) -> Self {
        self.hooks = hooks;
        self
    }

    /// The most times the `run.ending` hooks may keep one run going (ADR 0050, decision 10; default
    /// `DEFAULT_RUN_CONTINUATIONS`, at most `MAX_RUN_CONTINUATIONS`). Past it the run ends and
    /// the step `end:<turn>` records `capped`; `0` never lets a hook keep a run going.
    #[must_use]
    pub fn max_run_continuations(mut self, max: u32) -> Self {
        self.max_run_continuations = max;
        self
    }

    /// The keys that sign every assistant and tool turn this agent commits ([ADR 0068], slice
    /// T3a-1): each carries `extra_content.adam.turn_signature`, bound to its run, conversation,
    /// position and the chain of every message before it. A person's message is chained, never
    /// signed. Without keys (the default) nothing is signed and a run commits as it always did.
    /// Nothing is verified here (re-entry is slice T3a-3), and the signature is never sent to a
    /// model.
    ///
    /// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
    #[must_use]
    pub fn turn_keys(mut self, keys: DynTurnKeys) -> Self {
        self.turn_keys = Some(keys);
        self
    }

    /// Declare a custom event the agent's tools may emit, beside the ones each tool declares itself
    /// ([`Tool::events`]) (ADR 0050, decision 6). Every tool of the agent may emit it. A name in a core
    /// namespace, one that is not `ns.name`, or a second declaration of it that differs is a
    /// [`BuildError::Event`] from [`try_build`](Self::try_build).
    #[must_use]
    pub fn event(mut self, spec: EventSpec) -> Self {
        self.events.push(spec);
        self
    }

    /// Append one plugin to the hook set ([`LlmAgentBuilder::hooks`]); the order plugins are added in
    /// is the order their hooks run in.
    #[must_use]
    pub fn plugin(mut self, plugin: impl Plugin + 'static) -> Self {
        self.hooks = std::mem::take(&mut self.hooks).with(Arc::new(plugin));
        self
    }

    /// The reasoning effort every model call of this agent asks for unless the run's context names
    /// one ([`REASONING_EFFORT_CONTEXT_KEY`], ADR 0063): what an agent folder, its `model_preset`
    /// or its model profile resolves to. `None` (the default) sends nothing, which is the model's
    /// own default and may reason. The loop sets it on the request of each turn, never on the
    /// summary call.
    #[must_use]
    pub fn reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// The model the run continues on, once, when a model call of it is refused with
    /// [`ModelError::ModelLocked`] or over the person's limit ([`ModelError::Refused`] of status
    /// `402`, ADR 0064 amended 2026-11-23 and 2026-11-25): an agent folder's `model_fallback`,
    /// resolved by the assembly with its profile. The run says so as a step, reports the calls after
    /// it under this alias, and never switches back; a second `ModelLocked` fails the run naming
    /// both models, and a second `402` parks it as over limit. Without one, a locked model fails the
    /// run as before, and a 402 parks it.
    #[must_use]
    pub fn model_fallback(mut self, fallback: ModelBinding) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// The models a message may pick for its run ([`MODEL_CONTEXT_KEY`]), each with its resolved
    /// profile values. A context that names another alias, or an agent given none, is ignored: the
    /// agent's own model answers. The assembly gives them to an embedding engine only, never to a
    /// served agent, whose callers may not choose what a run costs.
    #[must_use]
    pub fn model_choices(mut self, choices: Vec<ModelBinding>) -> Self {
        self.choices = choices;
        self
    }

    /// How a run waits on a model that refused it or could not be reached ([ADR 0067]): how long
    /// parks on a timer in a row may last (24 hours by default), and whether an unreachable
    /// endpoint parks at all (no, by default). See "A run parked on the model" on [`LlmAgent`].
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    #[must_use]
    pub fn model_parking(mut self, parking: ModelParking) -> Self {
        self.model_parking = parking;
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
        // The custom events (ADR 0050, slice H5): every declaration is valid and agrees with the
        // others, and no plugin subscribes to a namespace nobody declares.
        EventRegistry::build(&self.tools, &self.events, &self.hooks.declared_events())?;
        let mut declared_here: Vec<EventSpec> =
            self.tools.iter().flat_map(|t| t.events()).collect();
        declared_here.extend(self.events.iter().cloned());
        self.hooks.check_events(&declared_here)?;
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
        // A declaration `build` cannot judge (`try_build` refuses it) leaves the agent with no custom
        // event at all: an event nobody checked is never sent.
        let events = EventRegistry::build(&tools, &self.events, &self.hooks.declared_events())
            .unwrap_or_else(|error| {
                tracing::error!(%error, "the custom events are refused: the agent emits none");
                EventRegistry::default()
            });
        // Above a limit of one the model is told what `after_previous` means.
        let instructions = if self.max_parallel > 1 {
            Some(match self.instructions {
                Some(own) if !own.trim().is_empty() => {
                    format!("{own}\n\n{}", order::INSTRUCTION)
                }
                _ => order::INSTRUCTION.to_owned(),
            })
        } else {
            self.instructions
        };
        // The toolset never changes after the build, so what a request holds besides the history
        // (the instructions and the specs as the request shows them) is counted once, here, and
        // never cloned or serialised again to answer `fixed_chars`.
        let mut shown = specs.clone();
        order::prepare(&mut shown, self.max_parallel > 1);
        let fixed_chars = instructions.as_deref().map_or(0, |s| s.chars().count())
            + serde_json::to_string(&shown).map_or(0, |s| s.chars().count());
        let model = crate::gate::admitted(self.model, self.admission.as_ref(), &self.name);
        LlmAgent {
            name: self.name,
            model,
            model_alias: self.model_alias,
            instructions,
            tools,
            sources: self.sources,
            index,
            specs,
            fixed_chars,
            limits: self.limits,
            extensions: Arc::new(self.extensions),
            wait_poll: self.wait_poll,
            stream_text: self.stream_text,
            step_io: self.step_io,
            session_agents: self.session_agents.into(),
            session_reports: self.session_reports,
            max_parallel: self.max_parallel,
            tool_retry: self.tool_retry,
            reasoning_effort: self.reasoning_effort,
            model_parking: self.model_parking,
            fallback: self.fallback,
            choices: self.choices.into(),
            images_cache: Arc::new(Mutex::new(ImageCache::new())),
            run_chars: Arc::new(Mutex::new(RunChars::default())),
            admission: self.admission,
            hooks: Arc::new(self.hooks),
            max_run_continuations: self.max_run_continuations.min(MAX_RUN_CONTINUATIONS),
            turn_keys: self.turn_keys,
            events: Arc::new(events),
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
/// # A run parked on the model
///
/// A model call the endpoint refused, when its provider parks the run on the refusal
/// ([`ModelCredentials::parks`](adam_model::ModelCredentials::parks), [ADR 0067]), does not fail the
/// run. The conversation is kept as it was before the call, [`Conversation::model_wait`] says why
/// ([`ModelWait`]: the reason, the status, the slug, how it wakes, and since when), and the run
/// parks. When it wakes, the **same call** is made again, in a new journaled step; the summary call
/// of a compaction parks and is made again the same way. The first call that is answered clears the
/// wait.
///
/// * **On a timer** (a lease not bound yet, a busy gateway, one that cannot be reached): the run
///   wakes on its own after 30 s, doubling at each park in a row up to [`MAX_MODEL_BACKOFF`](crate::MAX_MODEL_BACKOFF), and
///   never sooner than the provider asked (`Retry-After`). Parks on a timer in a row are bounded by
///   [`ModelParking::limit`] from the first of them, on the journaled clock: past it the run fails
///   with the last reason, and no wait goes past it. A timer counts against a child run's time
///   limit, as any timer does.
/// * **Until woken** (a usage limit reached): the run waits like a question, with no timer and its
///   deadline paused, and never fails on its own. A run nobody can wake fails instead: a schedule's
///   firing ([`is_unattended`]). A local subagent waits for the resume its parent forwards (ADR 0067
///   decision 5); a parent that is not an `LlmAgent` forwards none, so the child waits until a cancel.
///   A firing also fails on a refusal of status 401 or 403, since nothing binds a firing at the
///   gateway yet.
/// * **Waking.** A message wakes either kind: its text is the person's turn, added before the call. A
///   message of kind [`MODEL_RESUME_KIND`] wakes it and adds nothing (and one that lands while the
///   run works is no arrival). A run parked on its summary call takes its inbox, the words kept in
///   [`Conversation::deferred`] for after the summary. A cancel ends the park as it ends any wait.
///
/// The journal, the run's error and what a front shows hold the reason, the status and the slug,
/// never a header, a body or a credential.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
///
/// # Failure handling
///
/// * Retryable model error (`ModelError::is_retryable`), [`ToolError::Transient`] or
///   [`ToolError::Retryable`]: `AgentError::Transient` of the error's class, retried by the
///   runtime, under the tool's policy for a tool that has one ([`Tool::retry_policy`],
///   [`LlmAgentBuilder::tool_retry`], counted under [`tool_retry_key`]).
///   A `ModelError::RateLimited` that carries a `retry_after` becomes an
///   `AgentError::Transient` with that hint (`with_retry_after`), so the
///   retry waits at least that long
///   (the runtime's backoff still applies when it is longer).
/// * A refusal whose provider parks the run (`ModelError::Refused`), and, with
///   [`ModelParking::when_unreachable`], an endpoint that cannot be reached once the step's retries
///   are spent: the run parks on the model (below), and fails only past [`ModelParking::limit`], or
///   on a park until woken in a run nobody can wake (a schedule's firing). A refusal of status
///   `402` (the person's usage limit reached) first tries the fallback once, as a locked model
///   does, and parks only when no model is left.
/// * Other model error, `ModelError::ModelLocked` (naming the model) among them: the run fails
///   (`model call failed: ...`). So does a refusal under a provider that does not park
///   (`StaticBearer`'s `401` is `ModelError::Auth`).
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
    /// The characters of the instructions and the specs as a request shows them: what a request
    /// holds besides the history. Counted once at build; the toolset never changes after it.
    fixed_chars: usize,
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
    /// The retry policies set per tool by name ([`LlmAgentBuilder::tool_retry`]).
    tool_retry: BTreeMap<String, RetryPolicy>,
    /// The effort every turn asks for unless the run's context names one
    /// ([`LlmAgentBuilder::reasoning_effort`]).
    reasoning_effort: Option<ReasoningEffort>,
    /// How a run waits on a model that refused it ([`LlmAgentBuilder::model_parking`]).
    model_parking: ModelParking,
    /// The model a run continues on when its model is locked ([`LlmAgentBuilder::model_fallback`]).
    fallback: Option<ModelBinding>,
    /// The models a message may pick ([`LlmAgentBuilder::model_choices`]).
    choices: Arc<[ModelBinding]>,
    /// The bytes of the images already read and checked, by SHA-256, shared by every run of this
    /// agent: a turn reads an image file once, not once per model call.
    images_cache: Arc<Mutex<ImageCache>>,
    /// The characters of each run's history as this worker last counted it: a turn counts only the
    /// messages appended since, not the whole transcript again.
    run_chars: Arc<Mutex<RunChars>>,
    /// The process's pools, when the deployment built some: every model call takes a seat of
    /// [`Pool::ModelCalls`] and every tool call one of [`Pool::ToolCalls`] (ADR 0065).
    admission: Option<DynAdmission>,
    /// The compiled hooks around every tool call (ADR 0050, slice H1). Shared, because a run's calls
    /// of one segment read the same set from several futures, and a hook's plugin is written once.
    hooks: Arc<HookSet>,
    /// The most times `run.ending` hooks may keep one run going (ADR 0050, decision 10).
    max_run_continuations: u32,
    /// The keys that sign each assistant and tool turn at commit ([`LlmAgentBuilder::turn_keys`]).
    turn_keys: Option<DynTurnKeys>,
    /// The custom events the agent declares, and what each tool may emit (ADR 0050, slice H5).
    events: Arc<EventRegistry>,
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
    /// The id of the first message the run read, if it read any: what a resume it forwards to a
    /// child parked on the model derives its own id from (ADR 0067 decision 5).
    read: Option<String>,
    /// The approvals the person answered (ADR 0050, slice H8), in the order they were answered.
    approvals: Vec<ApprovalAnswer>,
}

/// A person's answer to an approval, as [`Absorbed`] reports it.
struct ApprovalAnswer {
    /// The call the approval was for, and its tool.
    call_id: String,
    tool: String,
    /// The answer was a yes: the call runs next.
    approved: bool,
    /// The message that answered: the key of its `approval.answered`.
    inbound: String,
}

/// What [`LlmAgent::compact_if_due`] did.
enum Compacted {
    /// Nothing: no compaction was due, or none could be made.
    No,
    /// The history was compacted: the model is asked in the next transition.
    Made,
    /// The summary call parked the run on the model (a [`Flow::Park`], [`Flow::Sleep`] or
    /// [`Flow::Fail`]).
    Parked(Flow),
}

/// What a part of a step decided.
enum Flow {
    /// Nothing terminal happened; carry on.
    Next,
    /// Wait for the user (a tool asked a question).
    Park,
    /// Wait for a child run, with a timer.
    Wait,
    /// Wait on the model until this time (a park on a timer, ADR 0067).
    Sleep(Timestamp),
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
            tool_retry: BTreeMap::new(),
            reasoning_effort: None,
            model_parking: ModelParking::default(),
            fallback: None,
            choices: Vec::new(),
            admission: None,
            hooks: HookSet::new(),
            max_run_continuations: DEFAULT_RUN_CONTINUATIONS,
            turn_keys: None,
            events: Vec::new(),
        }
    }

    /// How a run of this agent waits on a model that refused it ([`LlmAgentBuilder::model_parking`]).
    pub fn model_parking(&self) -> ModelParking {
        self.model_parking
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

    /// The retry policy of the tool `name`, first found: the one set by name
    /// ([`LlmAgentBuilder::tool_retry`]), the tool's own ([`Tool::retry_policy`]); `None`: the
    /// runtime's (ADR 0061, decision 3).
    fn tool_retry_policy(&self, name: &str) -> Option<RetryPolicy> {
        self.tool_retry
            .get(name)
            .copied()
            .or_else(|| self.tool(name).and_then(|tool| tool.retry_policy()))
    }

    /// The runtime's error for a retryable failure of the tool `tool`: `context` and the reason,
    /// of `class`, under the tool's policy when it has one (ADR 0061).
    fn tool_retry_error(
        &self,
        tool: &str,
        message: String,
        class: adam_error::ErrorClass,
    ) -> AgentError {
        tool_transient(
            message,
            class,
            self.tool_retry_policy(tool).map(|p| (tool, p)),
        )
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
        self.absorb_inbox_into(state, inbox, admitted, false)
    }

    /// [`absorb_inbox`](Self::absorb_inbox), with every message's words kept in
    /// [`Conversation::deferred`] when `defer`, whatever calls are owed: a run parked on its summary
    /// call reads them after the summary, which is asked for again from the history as it was.
    fn absorb_inbox_into(
        &self,
        state: &mut Conversation,
        inbox: Vec<Inbound>,
        admitted: &HashMap<String, Vec<Item>>,
        defer: bool,
    ) -> Absorbed {
        let mut texts = Vec::new();
        let mut read = None;
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
                    if read.is_none() {
                        read = Some(inbound.id.clone());
                    }
                    // What the message says about its sender is read along with what it says.
                    if let Some(context) = parse_context(&inbound.payload) {
                        state.merge_context(context);
                    }
                    // New values replace the run's; a message that brings none keeps them.
                    if let Some(prompt) = parse_prompt_statement(&inbound.payload) {
                        state.prompt_statement = Some(prompt);
                    }
                    let items = admitted.get(&inbound.id).cloned().unwrap_or_default();
                    texts.push((text, inbound.payload, items, inbound.id));
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
            state.thinking_cuts = 0;
            state.announced = None;
            state.announced_final = false;
        }
        let mut refusals = Vec::new();
        let mut approvals = Vec::new();
        // The first message that the tool accepts answers the question; one it refuses leaves the
        // run parked on the question the tool asked in its place, and the next is read against that.
        while let Some(PendingWait::Question(q)) = &state.pending_wait {
            let Some((answer, payload, items, inbound)) = texts.next() else {
                return Absorbed {
                    waiting: true,
                    refusals,
                    read,
                    approvals,
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
            // The question is a hook's approval (ADR 0050, slice H8): a clear yes leaves the call
            // owed, approved, and the loop runs it next; any other answer refuses it with the
            // person's words. The tool is not asked to read the answer: it asked nothing.
            if let Some(approval) = state
                .approval
                .as_mut()
                .filter(|approval| approval.call_id == call_id && !approval.approved)
            {
                let approved = approves(&answer);
                if approved {
                    approval.approved = true;
                    state.pending_wait = None;
                } else {
                    state.approval = None;
                    given(
                        state,
                        Message::tool_error(call_id.clone(), not_approved(&answer)),
                    );
                }
                approvals.push(ApprovalAnswer {
                    call_id: call_id.clone(),
                    tool,
                    approved,
                    inbound,
                });
                continue;
            }
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
        for (text, _, items, _) in texts {
            let message = if items.is_empty() {
                Message::user_text(text)
            } else {
                Message::User {
                    content: parts_of(&text, &items),
                }
            };
            if state.pending_calls.is_empty() && !defer {
                state.messages.push(message);
            } else {
                state.deferred.push(message);
            }
        }
        Absorbed {
            waiting: false,
            refusals,
            read,
            approvals,
        }
    }

    /// The inbox of a run that asks or parks on its summary call (ADR 0067), handed in by the step
    /// that took it before the summary (a resume is read and recorded there first): the wake-ups
    /// and notices dropped, a request to close its session noted, and each message's words kept in
    /// [`Conversation::deferred`], read once the summary is made.
    async fn absorb_while_parked(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        inbox: Vec<Inbound>,
    ) -> Result<(), AgentError> {
        let inbox: Vec<Inbound> = inbox
            .into_iter()
            .filter(|i| {
                !matches!(
                    i.kind.as_str(),
                    CHILD_PARKED_KIND | CHILD_REPORTED_KIND | RUN_FINISHED_KIND
                )
            })
            .collect();
        let (closes, inbox): (Vec<_>, Vec<_>) = inbox
            .into_iter()
            .partition(|i| i.kind == SESSION_CLOSE_KIND);
        if !closes.is_empty()
            && let Some(header) = &mut state.session
        {
            header.closed = true;
        }
        if inbox.is_empty() {
            return Ok(());
        }
        let admitted = self.admit_images(ctx, state, &inbox).await?;
        self.absorb_inbox_into(state, inbox, &admitted, true);
        Ok(())
    }

    /// What the run admits as images from now on: the gate for this model, and what the history
    /// holds, so that a call admits the images the run has room for and no more.
    fn image_gate(&self, state: &Conversation) -> ImageGate {
        let limits = self
            .model
            .input_modalities(self.live_model(state).alias)
            .images;
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
                // The message the run started with changed in place: the count of the history this
                // worker keeps no longer describes it, so the next turn counts in full.
                self.forget_chars(ctx.run_id());
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
    fn next_poll(&self, ctx: &Ctx) -> Timestamp {
        ctx.now() + adam_core::time::span_of(self.wait_poll)
    }

    /// `ms` milliseconds from now, by the journaled clock.
    async fn deadline_after(&self, ctx: &mut Ctx, ms: u64) -> Result<Timestamp, AgentError> {
        let now = ctx.now_journaled().await?;
        let every = i64::try_from(ms)
            .map(jiff::SignedDuration::from_millis)
            .unwrap_or(jiff::SignedDuration::MAX);
        Ok(now + every)
    }

    /// The model a model call of this run is made to now: after a switch, the fallback; else the
    /// one the run's context names when the agent was given a binding for it; else the agent's own.
    fn live_model(&self, state: &Conversation) -> LiveModel<'_> {
        let bound = if state.model_switch.is_some() {
            self.fallback.as_ref()
        } else {
            context_model(&state.context)
                .and_then(|alias| self.choices.iter().find(|b| b.alias == alias))
        };
        let mut live = match bound {
            Some(b) => LiveModel {
                alias: &b.alias,
                max_output_tokens: b.max_output_tokens,
                reasoning_effort: b.reasoning_effort,
            },
            None => LiveModel {
                alias: &self.model_alias,
                max_output_tokens: self.limits.max_output_tokens,
                reasoning_effort: self.reasoning_effort,
            },
        };
        // A session's own cap narrows the model's, never widens it (ADR 0054): with the model's cap
        // left to the model (`0`) the session's is the cap, and it is never `0`.
        if let Some(cap) = self
            .session_config(state)
            .and_then(|c| c.max_output_tokens)
            .filter(|cap| *cap > 0)
        {
            live.max_output_tokens = match live.max_output_tokens {
                0 => cap,
                own => own.min(cap),
            };
        }
        live
    }

    /// What this run's session asked for ([ADR 0054]): the `config` of the header of the round, an
    /// empty one for a run that is no round of a session.
    ///
    /// [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
    fn session_config<'a>(
        &self,
        state: &'a Conversation,
    ) -> Option<&'a adam_runtime::SessionConfig> {
        state.session.as_ref().and_then(|h| h.config.as_ref())
    }

    /// The agent's [`Limits`] narrowed by its session's `max_turns` and `max_tool_calls`: read at
    /// every step, so the next model call obeys them. A value of `0` is no narrowing (a header is
    /// never trusted to widen), and nothing here is ever raised.
    fn limits_of(&self, state: &Conversation) -> Limits {
        let mut limits = self.limits;
        if let Some(config) = self.session_config(state) {
            if let Some(n) = config.max_turns.filter(|n| *n > 0) {
                limits.max_turns = limits.max_turns.min(n);
            }
            if let Some(n) = config.max_tool_calls.filter(|n| *n > 0) {
                limits.max_tool_calls = limits.max_tool_calls.min(n);
            }
        }
        limits
    }

    /// The most tool calls of a turn that run at the same time in this run: the agent's, narrowed
    /// by its session's `max_parallel_tool_calls` (never below 1, never above the agent's).
    fn parallel_of(&self, state: &Conversation) -> usize {
        match self
            .session_config(state)
            .and_then(|c| c.max_parallel_tool_calls)
        {
            Some(n) => self.max_parallel.min((n as usize).max(1)),
            None => self.max_parallel,
        }
    }

    /// A 402, the person's usage limit reached for the model the call named, takes the switch of a
    /// `model_locked` refusal once (ADR 0064, amended 2026-11-25): the run asks another model, and
    /// parks as over limit only when none is left. `None` when the failure is no 402, or when the
    /// switch is spent, so the caller parks it as it always has.
    async fn switched_on_limit(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        failure: &ModelFailure,
    ) -> Result<Option<Flow>, AgentError> {
        if !failure.park.as_ref().is_some_and(|p| p.status == 402) {
            return Ok(None);
        }
        let from = self.live_model(state).alias.to_owned();
        match self
            .switched_on_lock(ctx, state, &from, &failure.message)
            .await
        {
            Flow::Next => {
                state.model_wait = None;
                Ok(Some(Flow::Next))
            }
            _ => Ok(None),
        }
    }

    /// What a model call the endpoint refused as locked does (ADR 0064, amended 2026-11-23): the
    /// first time, with a fallback, the run switches to it, says so as a step, and asks the turn
    /// again; a lock on the fallback, or with no fallback, fails the run, save for a 402, which
    /// parks it as over limit ([`switched_on_limit`](Self::switched_on_limit)).
    async fn switched_on_lock(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        locked: &str,
        message: &str,
    ) -> Flow {
        let Some(fallback) = &self.fallback else {
            return Flow::Fail(format!("model call failed: {message}"));
        };
        if let Some(switch) = &state.model_switch {
            return Flow::Fail(format!(
                "model call failed: {message}: `{locked}` is the fallback of `{}`, which the \
                 endpoint did not allow either, and a run switches once",
                switch.from
            ));
        }
        let from = self.live_model(state).alias.to_owned();
        let to = fallback.alias.clone();
        if from == to {
            return Flow::Fail(format!(
                "model call failed: {message}: `{locked}` is also the fallback of this agent, so \
                 there is no other model to continue on"
            ));
        }
        tracing::info!(run = %ctx.run_id(), %from, %to, "the endpoint does not allow the model: the run continues on its fallback");
        ctx.emit(RunEvent::Step(
            StepEvent::new(
                format!("model-switch:{}", state.turns),
                StepKind::Tool,
                "Switched model",
                StepState::Completed,
            )
            .with_icon(StepIcon::Think)
            .with_detail(format!(
                "The model `{from}` is not allowed; this run continues on `{to}`"
            )),
        ))
        .await;
        state.model_switch = Some(ModelSwitch {
            from,
            to,
            turn: state.turns,
        });
        Flow::Next
    }

    /// What a model call that failed with `failure` does when it parks the run ([ADR 0067]
    /// decisions 4, 6 and 8): `None` for a failure that does not park, which the caller handles as
    /// it always has.
    ///
    /// A refusal whose provider parks ([`ModelError::Refused`]) parks on a timer or until woken, as
    /// the provider said; an endpoint that could not be reached parks on a timer, when this agent
    /// parks on one ([`ModelParking::when_unreachable`]) and the step's retries are spent. The
    /// conversation is left as it was before the call, with [`Conversation::model_wait`] set, so the
    /// same call is made again when the run wakes. A park on a timer waits with a backoff, and parks
    /// on a timer in a row past [`ModelParking::limit`] fail the run; a run nobody can wake (a
    /// schedule's firing) fails on a park until woken. What is recorded is the reason, the status
    /// and the slug, never anything else of the answer.
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    async fn parked_on_model(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        failure: &ModelFailure,
    ) -> Result<Option<Flow>, AgentError> {
        let (status, slug, timer) = match &failure.park {
            Some(park) => (
                Some(park.status),
                park.slug.clone(),
                park.timer_ms.map(Duration::from_millis),
            ),
            None if failure.unreachable
                && self.model_parking.when_unreachable
                && ctx.retries_left() == 0 =>
            {
                (None, None, Some(Duration::ZERO))
            }
            None => return Ok(None),
        };
        let reason = reason_of(status, slug.as_deref());
        let detail = detail_of(reason, status, slug.as_deref());
        let previous = state.model_wait.take();
        let run = ctx.run_id();
        let unattended = is_unattended(ctx.conversation_id(), Some(&state.context));
        // A schedule's firing is bound at no gateway yet (ADR 0067 decision 6): its refusals of
        // status 401 or 403 cannot be cured by waiting, and fail it.
        if unattended && matches!(status, Some(401 | 403)) {
            tracing::info!(%run, %detail, "the model refused a firing's call as unauthenticated: it fails");
            return Ok(Some(Flow::Fail(format!(
                "model call failed: {detail}; a schedule started this run, and nothing binds a firing at the model gateway"
            ))));
        }
        let Some(at_least) = timer else {
            if unattended {
                tracing::info!(%run, %detail, "the model refused the call, and nobody can wake this run: it fails");
                return Ok(Some(Flow::Fail(format!(
                    "model call failed: {detail}; a schedule started this run, so nobody can wake it"
                ))));
            }
            let now = ctx.now_journaled().await?;
            let parked_since = match &previous {
                Some(w) if w.park == ModelPark::UntilWoken => w.parked_since,
                _ => now,
            };
            tracing::info!(%run, %detail, "the model refused the call: the run waits until it is woken");
            state.model_wait = Some(ModelWait {
                reason: reason.to_owned(),
                status,
                slug,
                park: ModelPark::UntilWoken,
                parked_since,
                parks: 0,
                wake_at: None,
            });
            return Ok(Some(Flow::Park));
        };
        let now = ctx.now_journaled().await?;
        let (parked_since, parks) = match &previous {
            Some(w) if w.park == ModelPark::Timer => (w.parked_since, w.parks.saturating_add(1)),
            _ => (now, 1),
        };
        let parked_for = Duration::try_from(now - parked_since).unwrap_or(Duration::ZERO);
        if parked_for >= self.model_parking.limit {
            tracing::info!(%run, %detail, parks, "parked on the model past the limit: the run fails");
            return Ok(Some(Flow::Fail(format!(
                "model call failed: {detail}; the run waited on the model for {}, past its limit of \
                 {} (MODEL_PARK_LIMIT)",
                words_of(parked_for),
                words_of(self.model_parking.limit)
            ))));
        }
        let wait = backoff(parks, at_least);
        // Never past the limit: a run asked to wait longer than it may wakes at the limit, and fails
        // then if it is refused again.
        let wake_at = (now + adam_core::time::span_of(wait))
            .min(parked_since + adam_core::time::span_of(self.model_parking.limit));
        tracing::info!(%run, %detail, parks, wait_secs = wait.as_secs(), "the model refused the call: the run waits on a timer");
        state.model_wait = Some(ModelWait {
            reason: reason.to_owned(),
            status,
            slug,
            park: ModelPark::Timer,
            parked_since,
            parks,
            wake_at: Some(wake_at),
        });
        Ok(Some(Flow::Sleep(wake_at)))
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

    /// The model wait the head of a child's state says it has (ADR 0067 decision 5): its own, or
    /// the copy it keeps of its own child's — a park is shown through each parent, one level at a
    /// time. `None` for a child that works or waits on something else, and for a wait this build
    /// cannot read.
    fn model_wait_of(head: Option<&Value>) -> Option<ModelWait> {
        let head = head?;
        let wait = head
            .get("model_wait")
            .or_else(|| head.pointer("/pending_wait/model_wait"))?;
        serde_json::from_value(wait.clone()).ok()
    }

    /// Whether a cut turn has nothing the model would see: no reasoning text, no raw reasoning
    /// block and no opaque members. A message with a carrier is not empty on the wire, so it stays.
    fn cut_turn_has_nothing_to_send(message: &Message) -> bool {
        match message {
            Message::Assistant {
                reasoning: None,
                provider,
                ..
            } => provider
                .as_ref()
                .is_none_or(|turn| turn.reasoning.is_none() && turn.members.is_empty()),
            _ => false,
        }
    }

    /// One journaled model call, then the assistant message into the state.
    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), turn = state.turns))]
    async fn model_turn(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        context: Option<&Arc<Map<String, Value>>>,
    ) -> Result<(Flow, Option<Early<'_>>), AgentError> {
        if ctx.is_cancelled() {
            return Ok((Flow::Fail(CANCELLED.into()), None));
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
                return Ok((Flow::Done(answer.clone()), None));
            }
            hit.answered = false;
            hit.answer = None;
        }
        if state.limit.is_none() && state.turns >= self.limits_of(state).max_turns {
            state.limit = Some(LimitHit {
                limit: LimitKind::MaxTurns,
                answered: false,
                answer: None,
            });
        }
        let closing = state.limit.is_some();
        let live = self.live_model(state);
        let live_alias = live.alias.to_owned();
        let mut request = ModelRequest::new(live_alias.clone());
        request.system = with_session_instructions(
            self.instructions.clone(),
            state.session.as_ref().and_then(|h| h.config.as_ref()),
        );
        // What is sent about an image is decided here, from the reference and the deployment's
        // settings now, and recorded nowhere: the same settings give the same lines every turn.
        let image_limits = self.model.input_modalities(&live_alias).images;
        request.messages = for_sending(&state.messages, image_limits.as_ref());
        // The language the person's interface names (ADR 0069) is a turn of its own, after the
        // history and never in `system`: the stable prefix of ADR 0044 is the same bytes on every
        // turn of a conversation, and a language belongs to the message that named it.
        if let Some(tag) = context.and_then(|c| interface_language(c)) {
            request
                .messages
                .push(Message::user_text(interface_language_turn(tag)));
        }
        request.tools = self.specs.clone();
        request.prompt_cache_key = ctx.conversation_id().map(prompt_cache_key);
        request.run = Some(model_run(state, ctx).to_string());
        request.prompt_statement = state.prompt_statement.clone();
        if closing {
            // The tools stay, so the prefix of the request is the one the provider has cached and
            // the calls in the history still have their tools; only the choice changes.
            request.tool_choice = ToolChoice::None;
            request.messages.push(Message::user_text(CLOSING_TURN));
        }
        request.max_output_tokens = (live.max_output_tokens > 0).then_some(live.max_output_tokens);
        // What the newest message activated wins over the agent's resolved default (ADR 0063).
        request.reasoning_effort = context_effort(context).or(live.reasoning_effort);
        // Kept for the report: a call names the level it asked for, and the request is moved into
        // the model call before the call is reported (ADR 0063 decision 7).
        let call_effort = request.reasoning_effort;

        let model = self.model.clone();
        let sources = self.sources.clone();
        let narrowing = Arc::new(narrowing_of(state, context.map(|c| &**c)));
        let turn = state.turns;
        // The step start seeded a journal written before `loaded` existed; a fresh run (or a
        // continuation) has no seed, and its task-loaded groups are journaled here.
        let need_task_loaded = state.loaded.is_empty();
        let source_ctx = (!sources.is_empty()).then(|| {
            SourceCtx::new(
                ctx.run_id(),
                state.root_run.unwrap_or_else(|| ctx.run_id()),
                ctx.conversation_id().map(str::to_owned),
                context.cloned().unwrap_or_default(),
            )
            .with_task(state.task())
            .with_workspace_run(
                workspace_run_of(state)
                    .unwrap_or_else(|| state.root_run.unwrap_or_else(|| ctx.run_id())),
            )
            .with_loaded(Arc::from(state.loaded.clone()))
            .with_removed_tools(Arc::from(state.removed_tools.clone()))
        });
        let carried_before: Arc<[String]> = Arc::from(state.carried_tools.clone());
        let need_loaders = loaders_needed(state.turns, &state.loaders);
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
        let images_cache = Arc::clone(&self.images_cache);
        // A report is told to the model before its first call of a run, and before every call
        // while a session of the conversation may still end.
        if turn == 0 && state.chain_started.is_none() {
            state.chain_started = Some(ctx.now());
        }
        // What a call of this turn is made from, taken once: the report's tool context and the calls
        // that may start while the model writes read the same snapshot.
        let ctx_facts = CtxFacts::of(ctx);
        let report_plan = (self.session_reports && !closing && (turn == 0 || state.sessions_open))
            .then(|| self.report_plan(&ctx_facts, state, context))
            .flatten();
        // The calls of this turn may start while the model is still writing (ADR 0049, amended):
        // what they need is taken from the state and the `Ctx` now, because neither is at hand
        // inside `Ctx::step`. The batch is handed back through the slot however the step ends.
        let early_facts = EarlyFacts {
            ctx: ctx_facts,
            // The step's one shared copy, not a clone of the map: a started call reads the same
            // context as the calls that run at the turn.
            context: context.cloned(),
            root: (
                state.root_run,
                state.root_step.clone(),
                state.plan.clone(),
                model_run_of(state),
                workspace_run_of(state),
                state.prompt_statement.clone(),
            ),
            kept: Arc::clone(&state.kept_outputs),
            kept_left: MAX_RUN_KEPT_BYTES.saturating_sub(state.kept_bytes()),
            files_used: files_kept(&state.artifacts),
            images: self.image_gate(state),
            output_cut: state.output_cut,
            task: state.task().map(Arc::from),
            loaded: Arc::from(state.loaded.clone()),
            removed_tools: Arc::from(state.removed_tools.clone()),
            narrowing: Arc::new(narrowing_of(state, context.map(|c| &**c))),
            event_budget: Arc::clone(&state.event_budget.0),
            approvable: approvable(ctx, state),
            turn: crate::early::Turn {
                limit: self.parallel_of(state),
                closing,
                cancelled: false,
                tool_calls: state.tool_calls,
                max_tool_calls: self.limits_of(state).max_tool_calls,
            },
            repeats: state.repeats.clone(),
        };
        let early_slot: Arc<Mutex<Option<Early<'_>>>> = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&early_slot);
        let agent = self;
        let recorded: Result<Recorded, ModelFailure> = ctx
            .step(&format!("model:{turn}"), move || async move {
                if !named.is_empty() {
                    request.image_data = image_data(
                        named.iter().collect(),
                        images_store.as_ref(),
                        &images_scope,
                        &images_state,
                        &images_cache,
                    )
                    .await;
                }
                // The sources are read here, inside the step, so that a replay of a turn whose model
                // call is recorded does not read them again: only the answer is journaled, not the
                // tools it was given.
                let Sourced {
                    notes,
                    own_order,
                    offered,
                    mut carried,
                } = apply_sources(
                    &sources,
                    source_ctx.as_ref(),
                    &narrowing,
                    &mut request,
                    ordered,
                )
                .await;
                let loaders = match source_ctx.as_ref() {
                    Some(ctx) if need_loaders => {
                        Some(crate::source::loaders_offered(&sources, ctx).await)
                    }
                    _ => None,
                };
                let task_loaded = match source_ctx.as_ref() {
                    Some(ctx) if need_task_loaded => {
                        crate::source::loaded_by_task(&sources, ctx).await
                    }
                    _ => Vec::new(),
                };
                // A task-loaded group is journaled only after this call, so the first request's
                // carried set would not name its tools: add them, or the next turn would record a
                // growth the first request already made.
                if turn == 0 {
                    for one in &task_loaded {
                        for tool in &one.tools {
                            if !carried.contains(tool)
                                && request.tools.iter().any(|spec| spec.name == *tool)
                            {
                                carried.push(tool.clone());
                            }
                        }
                    }
                }
                // The growth is judged on the final request, so a tier that hides every loaded
                // tool records none; the loaders and the task-loaded groups are journaled on the
                // first turn, with the answer, so a replay decides the same.
                let grew = turn > 0
                    && carried
                        .iter()
                        .any(|tool| !carried_before.iter().any(|before| before == tool));
                // The reports are appended last: after the history, which is what the request
                // before this one started with.
                let (reports, sessions_open) = match report_plan {
                    Some(plan) => plan.tell(&mut request).await,
                    None => (Vec::new(), None),
                };
                // The `model.request` hooks (ADR 0050, slice H3) run here, inside the step, on the
                // final request: a replay of a recorded turn asks none. They may rewrite only the
                // temperature and top-p, the bound on output tokens, the metadata, the headers and an extra body.
                if !agent.hooks.is_empty() {
                    let mut temperature = request.temperature;
                    let mut top_p = request.top_p;
                    let mut headers = request.headers.clone();
                    let mut extra_body = request.extra_body.clone();
                    let mut max_output_tokens = request.max_output_tokens;
                    let mut metadata = request.metadata.clone();
                    let verdict = agent.hooks.model_request(&mut HookedModelRequest {
                        kind: ModelCallKind::Turn,
                        temperature: &mut temperature,
                        top_p: &mut top_p,
                        headers: &mut headers,
                        extra_body: &mut extra_body,
                        max_output_tokens: &mut max_output_tokens,
                        metadata: &mut metadata,
                    });
                    match verdict {
                        Ok(Verdict::Continue) => {
                            apply_hooked(
                                &mut request,
                                temperature,
                                top_p,
                                headers,
                                extra_body,
                                max_output_tokens,
                                metadata,
                            )?;
                        }
                        Ok(Verdict::Stop(reason)) => {
                            return Err(ModelFailure::by_hook(
                                format!("stopped by a plugin: {reason}"),
                                false,
                            ));
                        }
                        Ok(Verdict::Deny(_) | Verdict::Ask(_)) => {
                            return Err(ModelFailure::by_hook(
                                "a hook may only continue or stop at model.request".to_owned(),
                                false,
                            ));
                        }
                        Err(error) => {
                            return Err(ModelFailure::by_hook(
                                format!("the hook `{}` failed: {}", error.plugin, error.reason),
                                error.is_retryable(),
                            ));
                        }
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
                        own_order,
                        reports,
                        sessions_open,
                        offered,
                        carried,
                        loaders,
                        task_loaded,
                        grew,
                    });
                }
                // The words go out while they are written, under an id made here, inside the step, and
                // recorded with the answer: a replay calls no model and sends no pieces, but it knows
                // which stream the words were, so it says them whole under the same id.
                // The notes are this turn's (a source's listing, read above): a source's call
                // reports and is labelled as the turn says, not as the turn before it did.
                let mut early = Early::new(agent, early_facts, notes.clone(), own_order.clone());
                let streamed = text_stream::stream_response(
                    &model,
                    request,
                    &emitter,
                    &cancel,
                    Some(&mut early),
                    move || stream_id(run, turn),
                    move || reasoning_stream_id(run, turn),
                )
                .await;
                // The batch goes back to the turn, whatever the stream did with it: the calls that
                // started are the turn's to settle (their steps are written only after it).
                *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(early);
                let streamed = streamed.map_err(ModelFailure::from)?;
                Ok(Recorded {
                    response: unthought(&emitter, run, turn, true, streamed.response).await,
                    stream: streamed.stream,
                    notes,
                    seen: Some(seen),
                    call: Some(call_id(run, turn)),
                    own_order,
                    reports,
                    sessions_open,
                    offered,
                    carried,
                    loaders,
                    task_loaded,
                    grew,
                })
            })
            .await?;
        let early = early_slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // The call completed (the provider answered): it is reported, every time this step is
        // passed, under the id its record holds, so a replay says the same report again. A
        // cancelled turn's call is reported too: it was made, though the turn is not kept.
        let usage = match &recorded {
            Ok(r) => {
                let usage = r.response.usage.accounted();
                let call = r.call.clone().unwrap_or_else(|| old_call_id(run, turn));
                self.report_usage(ctx, state, call, &live_alias, usage, call_effort)
                    .await;
                usage
            }
            Err(_) => adam_model::Usage::default(),
        };
        // A cancel ends the turn here: nothing the model said is acted on, said or kept. The run is
        // already `Failed` in the store (that is what told the token), so this result is dropped
        // by the worker's commit.
        if ctx.is_cancelled() {
            return Ok((Flow::Fail(CANCELLED.into()), early));
        }
        // A 402 is the person's usage limit reached for the model the call named: a run with another
        // model tries it once, and parks only when none is left (ADR 0064, amended 2026-11-25). The
        // calls that started end; their results go, as for a park.
        if let Err(f) = &recorded
            && let Some(flow) = self.switched_on_limit(ctx, state, f).await?
        {
            if let Some(mut early) = early {
                early.settle().await;
            }
            return Ok((flow, None));
        }
        // A refusal that parks the run (ADR 0067): the conversation stays as it was before the call,
        // which is made again when the run wakes. The calls that started end; their results go.
        if let Err(f) = &recorded
            && let Some(flow) = self.parked_on_model(ctx, state, f).await?
        {
            if let Some(mut early) = early {
                early.settle().await;
            }
            return Ok((flow, None));
        }
        // Anything else the endpoint answered ends a wait on it; a retry keeps it, since the state of
        // a failed transition is not kept.
        if !matches!(&recorded, Err(f) if f.retryable) {
            state.model_wait = None;
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
                offered,
                carried,
                loaders,
                task_loaded,
                grew,
            }) => {
                // What the request carried **before the tier withheld any** is the truth of the
                // loaded set: every loaded tool it did not carry is journaled as removed, and a
                // growth is recorded only when the final request actually grew.
                if let Some(loaders) = loaders {
                    state.fill_seeded(&loaders, &task_loaded);
                    state.loaders = loaders;
                }
                state.note_task_loaded(task_loaded);
                state.note_removed(&offered);
                let explicit: Vec<String> = state
                    .loaders
                    .iter()
                    .map(|loader| loader.name.clone())
                    .collect();
                state.note_tier_withheld(&explicit, &carried);
                state.note_growth(grew);
                state.note_carried(carried);
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
                // The started calls end within their limits; their results are not used, and the
                // retry asks the model again.
                if let Some(mut early) = early {
                    early.settle().await;
                }
                let error = tool_transient(
                    format!("model call failed: {}", f.message),
                    f.retry_class(),
                    None,
                );
                return Err(match f.retry_after_ms {
                    // The provider said how long to wait: the runtime waits
                    // at least that long (see `AgentError::with_retry_after`).
                    Some(ms) => error.with_retry_after(Duration::from_millis(ms)),
                    None => error,
                });
            }
            // The provider rejected the prompt as too long: the history is compacted once, in the
            // next transition, and the turn is tried again.
            Err(f) if f.context_length => {
                return Ok((self.rejected_as_too_long(state, &f.message), early));
            }
            // The provider refused the request itself: if images no earlier request carried are in
            // it, they may be why.
            Err(f) if f.refused => {
                let flow = Self::refused_with_images(state, &carried, &f.message);
                if matches!(flow, Flow::Next) {
                    // Images left the history in place: count it afresh next turn.
                    self.forget_chars(run);
                }
                return Ok((flow, early));
            }
            Err(f) => {
                let flow = match &f.locked {
                    Some(locked) => self.switched_on_lock(ctx, state, locked, &f.message).await,
                    None => Flow::Fail(format!("model call failed: {}", f.message)),
                };
                return Ok((flow, early));
            }
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
            .add(self.model.provider(), &live_alias, usage);
        // What the provider counted of the prompt this call sent: the next estimate starts from it.
        state.last_prompt = (usage.input_tokens > 0).then_some(PromptSize {
            input_tokens: usage.input_tokens,
            messages: seen_at,
        });
        if !matches!(message, Message::Assistant { .. }) {
            return Ok((
                Flow::Fail("model call failed: the response is not an assistant message".into()),
                early,
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
            return Ok((Flow::Next, early));
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
            && state.tool_calls < self.limits_of(state).max_tool_calls
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
        // Only a turn that is continued as a cut keeps the count: any other ends or moves the run on.
        let cut = finish == FinishReason::Length
            && text.trim().is_empty()
            && calls.is_empty()
            && !unread
            && state.limit.is_none()
            && state.announced.is_none();
        if !cut {
            state.thinking_cuts = 0;
        }
        if !text.trim().is_empty() {
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
            return Ok((Flow::Next, early));
        }
        if let Some(limit) = state.limit.as_ref().map(|hit| hit.limit) {
            let said = state.announced.clone().unwrap_or(text);
            if said.trim().is_empty() {
                return Ok((Flow::Fail(self.limit_message(state, limit)), early));
            }
            let output = limited_output(state, limit, said, finish == FinishReason::Length, stream);
            if let Some(hit) = &mut state.limit {
                hit.answered = true;
                hit.answer = Some(output.clone());
            }
            return Ok((Flow::Done(output), early));
        }
        if calls.is_empty() {
            // What a tool announced as the answer is the run's answer (the last announcement wins);
            // without one, the words that end the turn are.
            if let Some(announced) = state.announced.clone() {
                if self.run_ending(ctx, state, &announced).await? {
                    return Ok((Flow::Next, early));
                }
                return Ok((
                    Flow::Done(json!({
                        "text": announced,
                        "artifacts": state.artifacts,
                    })),
                    early,
                ));
            }
            // Reasoning that filled the output limit before any word is no answer: the model is
            // asked again, a few times, and then the run fails (never an empty answer, never a
            // question to the person).
            if cut {
                state.thinking_cuts += 1;
                if state.thinking_cuts > MAX_THINKING_CUTS {
                    return Ok((
                        Flow::Fail(format!(
                            "output limit reached while thinking: {} turns in a row ended at the \
                             output limit with no words and no tool call; raise max_output_tokens \
                             or lower the reasoning effort",
                            state.thinking_cuts
                        )),
                        early,
                    ));
                }
                // A cut turn with nothing the model will see (no reasoning echoed back, no raw
                // block, no carrier) would be sent as an empty assistant message, which some
                // servers refuse: it leaves the history.
                if state
                    .messages
                    .last()
                    .is_some_and(Self::cut_turn_has_nothing_to_send)
                {
                    state.messages.pop();
                }
                // The next turn is the closing turn, which has its own instruction.
                if state.turns < self.limits_of(state).max_turns {
                    state.messages.push(Message::user_text(THINKING_CUT_TURN));
                }
                return Ok((Flow::Next, early));
            }
            if self.run_ending(ctx, state, &text).await? {
                return Ok((Flow::Next, early));
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
            return Ok((Flow::Done(output), early));
        }

        let requested = u32::try_from(calls.len()).unwrap_or(u32::MAX);
        let max_tool_calls = self.limits_of(state).max_tool_calls;
        if state.tool_calls.saturating_add(requested) > max_tool_calls {
            // None of the turn's calls runs, and each is answered, so the history has no call without
            // a result; the next model call is the closing turn.
            let said = format!(
                "not run: {} tool calls were already made and this turn asked for {} more \
                 (max_tool_calls = {})",
                state.tool_calls, requested, max_tool_calls
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
            return Ok((Flow::Next, early));
        }
        state.tool_calls += requested;
        let (calls, ordered_calls) = ordered_by_the_model(calls, &own_order);
        state.pending_calls = calls;
        state.ordered_calls = ordered_calls;
        Ok((Flow::Next, early))
    }

    /// The budget of the history, in tokens (see [`compaction::budget`]).
    fn history_budget(&self, state: &Conversation) -> Option<usize> {
        let live = self.live_model(state);
        compaction::budget(
            compaction::effective_window(
                self.model.context_window(live.alias),
                self.session_config(state).and_then(|c| c.context_window),
                live.max_output_tokens,
            ),
            live.max_output_tokens,
            self.limits.max_history_tokens,
        )
    }

    /// Characters a request holds besides the history: the instructions and the tools.
    fn fixed_chars(&self) -> usize {
        self.fixed_chars
    }

    /// Forget the character count of a run that ends: nothing of its history is counted again.
    fn forget_chars(&self, run: RunId) {
        self.run_chars
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .forget(run);
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
        let limit = match (reason, self.history_budget(state)) {
            (CompactionReason::Budget, budget) => budget.unwrap_or(estimate),
            (CompactionReason::Overflow, budget) => budget.unwrap_or(estimate).min(estimate),
        };
        compaction::cut_point(&state.messages, limit)
    }

    /// Whether the committed state is due for a compaction: the estimate has reached the budget, and
    /// nothing says otherwise (a compaction at this very turn, a summary that failed a few turns ago).
    fn compaction_due(&self, state: &Conversation) -> bool {
        let Some(budget) = self.history_budget(state) else {
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
    ) -> Result<Compacted, AgentError> {
        let asked = state.compact_next.take();
        let reason = match asked {
            Some(reason) => reason,
            None => {
                if !state.pending_calls.is_empty()
                    || state.pending_wait.is_some()
                    || state.limit.is_some()
                    || !self.compaction_due(state)
                {
                    return Ok(Compacted::No);
                }
                CompactionReason::Budget
            }
        };
        let Some(cut) = self.cut_for(state, reason) else {
            return Ok(Compacted::No);
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

        let live = self.live_model(state);
        let live_alias = live.alias.to_owned();
        let mut request = ModelRequest::new(live_alias.clone());
        request.system = with_session_instructions(
            self.instructions.clone(),
            state.session.as_ref().and_then(|h| h.config.as_ref()),
        );
        // The summary request is the request before it and one more message, so a provider's cache
        // serves it: its images are the ones the last request sent. A history that a provider
        // rejected as too long is sent shorter, with each image said in a few words.
        let image_limits = self.model.input_modalities(&live_alias).images;
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
        request.run = Some(model_run(state, ctx).to_string());
        request.prompt_statement = state.prompt_statement.clone();
        request.max_output_tokens = Some(compaction::summary_output_tokens(live.max_output_tokens));
        // A summary is not a reasoning choice: it asks for no level, whatever the run or the agent
        // set (ADR 0063, decision 6).
        request.reasoning_effort = None;
        let model = self.model.clone();
        let ordered = self.max_parallel > 1;
        let sources = self.sources.clone();
        let narrowing = Arc::new(narrowing_of(state, Some(&state.context)));
        let source_ctx = (!sources.is_empty()).then(|| {
            SourceCtx::new(
                ctx.run_id(),
                state.root_run.unwrap_or_else(|| ctx.run_id()),
                ctx.conversation_id().map(str::to_owned),
                Arc::new(state.context.clone()),
            )
            .with_task(state.task())
            .with_workspace_run(
                workspace_run_of(state)
                    .unwrap_or_else(|| state.root_run.unwrap_or_else(|| ctx.run_id())),
            )
            .with_loaded(Arc::from(state.loaded.clone()))
            .with_removed_tools(Arc::from(state.removed_tools.clone()))
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
        let images_cache = Arc::clone(&self.images_cache);
        let hooks = &self.hooks;
        let recorded: Result<Summarised, ModelFailure> = ctx
            .step(&id, move || async move {
                if !named.is_empty() {
                    request.image_data = image_data(
                        named.iter().collect(),
                        images_store.as_ref(),
                        &images_scope,
                        &images_state,
                        &images_cache,
                    )
                    .await;
                }
                apply_sources(
                    &sources,
                    source_ctx.as_ref(),
                    &narrowing,
                    &mut request,
                    ordered,
                )
                .await;
                // The compaction hooks (ADR 0050, slice H9) run here, inside the journaled step:
                // a replay of a recorded summary asks none. A hook may add context to the request
                // or veto the summary, and the history is shortened instead; it never writes the
                // summary itself.
                if !hooks.is_empty() {
                    let mut context = String::new();
                    match hooks.compaction_before(&mut HookedCompaction {
                        context: &mut context,
                    }) {
                        Ok(Verdict::Continue) => {}
                        Ok(Verdict::Deny(reason)) => {
                            return Ok(Summarised {
                                summary: None,
                                cut: at,
                                usage: adam_model::Usage::default(),
                                call: compaction_call_id(run, n),
                                vetoed: Some(reason),
                            });
                        }
                        Ok(Verdict::Stop(reason)) => {
                            return Err(ModelFailure::by_hook(
                                format!("stopped by a plugin: {reason}"),
                                false,
                            ));
                        }
                        Ok(Verdict::Ask(_)) => {
                            return Err(ModelFailure::by_hook(
                                "a hook may only continue, deny or stop at compaction.before"
                                    .to_owned(),
                                false,
                            ));
                        }
                        Err(error) => {
                            return Err(ModelFailure::by_hook(
                                format!("the hook `{}` failed: {}", error.plugin, error.reason),
                                error.is_retryable(),
                            ));
                        }
                    }
                    if !context.is_empty() {
                        request.messages.push(Message::user_text(context));
                    }
                    // The summary is a model call too: `model.request` sees it, and knows which
                    // kind it is (ADR 0050, slice H9).
                    let mut temperature = request.temperature;
                    let mut top_p = request.top_p;
                    let mut headers = request.headers.clone();
                    let mut extra_body = request.extra_body.clone();
                    let mut max_output_tokens = request.max_output_tokens;
                    let mut metadata = request.metadata.clone();
                    match hooks.model_request(&mut HookedModelRequest {
                        kind: ModelCallKind::Compaction,
                        temperature: &mut temperature,
                        top_p: &mut top_p,
                        headers: &mut headers,
                        extra_body: &mut extra_body,
                        max_output_tokens: &mut max_output_tokens,
                        metadata: &mut metadata,
                    }) {
                        Ok(Verdict::Continue) => {
                            apply_hooked(
                                &mut request,
                                temperature,
                                top_p,
                                headers,
                                extra_body,
                                max_output_tokens,
                                metadata,
                            )?;
                        }
                        Ok(Verdict::Stop(reason)) => {
                            return Err(ModelFailure::by_hook(
                                format!("stopped by a plugin: {reason}"),
                                false,
                            ));
                        }
                        Ok(Verdict::Deny(_) | Verdict::Ask(_)) => {
                            return Err(ModelFailure::by_hook(
                                "a hook may only continue or stop at model.request".to_owned(),
                                false,
                            ));
                        }
                        Err(error) => {
                            return Err(ModelFailure::by_hook(
                                format!("the hook `{}` failed: {}", error.plugin, error.reason),
                                error.is_retryable(),
                            ));
                        }
                    }
                }
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
                    vetoed: None,
                })
            })
            .await?;
        // A 402 is the person's usage limit reached: the summary is asked of the fallback once, as
        // for a turn, and parks only when no model is left (ADR 0064, amended 2026-11-25).
        if let Err(f) = &recorded
            && let Some(flow) = self.switched_on_limit(ctx, state, f).await?
        {
            state.compact_next = asked;
            return Ok(Compacted::Parked(flow));
        }
        // A refusal that parks the run parks it here too (ADR 0067): the summary is asked for again
        // when the run wakes, from the history as it is.
        if let Err(f) = &recorded
            && let Some(flow) = self.parked_on_model(ctx, state, f).await?
        {
            state.compact_next = asked;
            if let Some(wait) = &state.model_wait {
                let reason = wait.reason.clone();
                ctx.emit(RunEvent::Step(step(StepState::Waiting).with_detail(reason)))
                    .await;
            }
            return Ok(Compacted::Parked(flow));
        }
        // A summary the endpoint refuses as locked switches the run as a turn does: the summary is
        // asked for again of the fallback in the next transition, from the history as it is.
        if let Err(f) = &recorded
            && let Some(locked) = &f.locked
        {
            let flow = self.switched_on_lock(ctx, state, locked, &f.message).await;
            if matches!(flow, Flow::Next) {
                state.compact_next = asked;
            }
            ctx.emit(RunEvent::Step(
                step(StepState::Failed).with_detail(format!("the model `{locked}` is not allowed")),
            ))
            .await;
            return Ok(Compacted::Parked(flow));
        }
        if !matches!(&recorded, Err(f) if f.retryable) {
            state.model_wait = None;
        }
        let (summary, cut_at, rejected) = match recorded {
            // A compaction hook vetoed the summary: the history is shortened instead (ADR 0050,
            // slice H9), as when no summary can be made, and the reason is what the step says.
            Ok(Summarised {
                vetoed: Some(reason),
                ..
            }) => {
                tracing::warn!(run = %run, "a compaction hook vetoed the summary: the history is shortened instead");
                state.compact_failed = Some(state.turns);
                ctx.emit(RunEvent::Step(step(StepState::Failed).with_detail(format!(
                    "no summary could be made (vetoed by a plugin: {reason}); the history is shortened instead"
                ))))
                .await;
                return Ok(Compacted::No);
            }
            Ok(done) => {
                // The call was made and counts, whatever its summary is worth.
                self.report_usage(ctx, state, done.call.clone(), &live_alias, done.usage, None)
                    .await;
                state.usage = state.usage.saturating_add(done.usage);
                state
                    .usage_totals
                    .add(self.model.provider(), &live_alias, done.usage);
                (done.summary, done.cut, state.overflow_message.take())
            }
            Err(f) if f.retryable => {
                let error = tool_transient(
                    format!("summary call failed: {}", f.message),
                    f.retry_class(),
                    None,
                );
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
                return Ok(Compacted::No);
            }
        };
        // The recorded cut is the one to apply; it must still be a turn start of this history.
        let valid = cut_at >= 1
            && cut_at < state.messages.len()
            && matches!(state.messages[cut_at], Message::Assistant { .. });
        if !valid {
            state.compact_failed = Some(state.turns);
            return Ok(Compacted::No);
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
        Ok(Compacted::Made)
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
        alias: &str,
        usage: adam_model::Usage,
        effort: Option<ReasoningEffort>,
    ) {
        let mut event = UsageEvent::new(call, alias, usage);
        if let Some(level) = effort {
            event = event.with_effort(level);
        }
        if let Some(provider) = self.model.provider() {
            event = event.with_provider(provider);
        }
        if let Some(window) = self.model.context_window(alias) {
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
    fn report_plan(
        &self,
        ctx: &CtxFacts,
        state: &Conversation,
        context: Option<&Arc<Map<String, Value>>>,
    ) -> Option<ReportPlan> {
        if self.session_agents.is_empty() {
            return None;
        }
        let tool = self.tool(SUBAGENT_READ)?.clone();
        let tool_ctx = self
            .tool_ctx(
                ctx,
                context,
                (
                    state.root_run,
                    state.root_step.as_deref(),
                    state.plan.as_ref(),
                    model_run_of(state),
                    workspace_run_of(state),
                    state.prompt_statement.as_ref(),
                ),
                &state.source_notes,
                "report",
                SUBAGENT_READ,
            )
            .with_task(state.task().map(Arc::from))
            .with_loaded(Arc::from(state.loaded.clone()))
            .with_removed_tools(Arc::from(state.removed_tools.clone()));
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
            starter: ctx.child_starter.clone(),
            me: ctx.run,
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

    /// The `run.ending` point (ADR 0050, decision 10): the run would end with `text`. The hooks are
    /// asked in the journaled step `end:<turn>`, so a replay asks none. Returns whether a hook kept
    /// the run going: the reason is then in the history as a message of the plugin's, and the loop
    /// asks the model again. No hook set takes no step.
    async fn run_ending(
        &self,
        ctx: &mut Ctx,
        state: &mut Conversation,
        text: &str,
    ) -> Result<bool, AgentError> {
        if self.hooks.is_empty() {
            return Ok(false);
        }
        let hooks = Arc::clone(&self.hooks);
        let owned = text.to_owned();
        let (continuations, cap) = (state.end_continuations, self.max_run_continuations);
        let decided: Result<EndDecision, String> = ctx
            .step(&format!("end:{}", state.turns), move || async move {
                ask_end(&hooks, &owned, continuations, cap)
            })
            .await?;
        let decision = decided.map_err(|why| {
            AgentError::transient(format!("a run.ending hook could not decide: {why}"))
        })?;
        if decision.verdict != "continue" {
            return Ok(false);
        }
        let plugin = decision.plugin.as_deref().unwrap_or("plugin");
        let reason = decision.reason.as_deref().unwrap_or("");
        state.end_continuations += 1;
        state.announced = None;
        state.announced_final = false;
        state.messages.push(Message::user_text(format!(
            "[A plugin ({plugin}) asked the run to go on instead of ending, not the person: {reason}]"
        )));
        Ok(true)
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
        context: Option<&Arc<Map<String, Value>>>,
        early: &mut Option<Early<'_>>,
    ) -> Result<Flow, AgentError> {
        while let Some(call) = state.pending_calls.first().cloned() {
            // A cancel stops the turn between its calls too: the ones still owed do not start.
            if ctx.is_cancelled() {
                return Ok(Flow::Fail(CANCELLED.into()));
            }
            // A call a person approved (ADR 0050, slice H8) runs now, alone, with the arguments the
            // step that asked recorded: it is not scheduled again, and the doom loop judged it before
            // it asked.
            let approved = state
                .approval
                .as_ref()
                .filter(|approval| approval.approved && approval.call_id == call.id)
                .map(|approval| approval.arguments.clone());
            // The calls that run beside this one: none unless the schedule puts several together.
            let together = if approved.is_some() {
                1
            } else {
                self.together(state)
            };
            if together >= 2 {
                match self
                    .run_segment(ctx, state, together, context, early)
                    .await?
                {
                    Some(flow) => return Ok(flow),
                    None => continue,
                }
            }
            // `doom_loop`: the third call in a row with the same tool, the same arguments and the
            // same result as the two before it does not run. Decided from the committed state alone.
            let key = call_key(&call);
            if approved.is_none()
                && doom_loop_would_refuse(state.repeats.as_ref(), &key)
                && let Some(repeats) = &mut state.repeats
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
            // A call that started while the model wrote: its work is done, and its step records it.
            let started = match early.as_mut() {
                Some(early) if approved.is_none() => early.take_one(&call).await,
                _ => None,
            };
            let may_ask = state.may_ask_person(ctx.conversation_id());
            // An approved call's `tool.started` was told before it asked (its key is the same).
            if approved.is_none() {
                self.observe_tool_started(ctx, state.root_run, &call);
            }
            let approving = approved.is_some();
            let (result, reported) = self
                .run_tool(
                    ctx,
                    context,
                    (
                        state.root_run,
                        state.root_step.as_deref(),
                        state.plan.as_ref(),
                        model_run_of(state),
                        workspace_run_of(state),
                        state.prompt_statement.as_ref(),
                    ),
                    &state.source_notes,
                    &call,
                    CallFacts {
                        files_used: used,
                        output_cut: state.output_cut,
                        kept: Arc::clone(&state.kept_outputs),
                        kept_left: MAX_RUN_KEPT_BYTES.saturating_sub(state.kept_bytes()),
                        images: self.image_gate(state),
                        task: state.task().map(Arc::from),
                        loaded: Arc::from(state.loaded.clone()),
                        removed_tools: Arc::from(state.removed_tools.clone()),
                        narrowing: Arc::new(narrowing_of(state, context.map(|c| &**c))),
                        event_budget: Arc::clone(&state.event_budget.0),
                        approvable: approvable(ctx, state),
                    },
                    started,
                    may_ask,
                    approved,
                )
                .await?;
            // The approved call has run: its approval is spent, in the state its step's result is
            // committed with.
            if approving {
                state.approval = None;
            }
            state.sandbox_usage.merge(&reported);
            let (message, artifacts) = match result {
                ToolResult::Answered {
                    message,
                    artifacts,
                    answer,
                    repeat_key,
                    plan,
                    loaded,
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
                    if !loaded.is_empty() {
                        state.load_groups(loaded);
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
                // A hook asked a person to approve the call (ADR 0050, decision 12, slice H8): the
                // run parks on a question, the one every question takes, and the call stays owed.
                // The step that asked is recorded, so a replay parks on the same question and asks
                // no hook again.
                ToolResult::NeedsApproval {
                    question,
                    arguments,
                } => {
                    state.repeats = None;
                    let asked = approval_question(&call.name, &question);
                    // The hook's words may name the call's arguments: the event says them through the
                    // agent's redactor, as the call's step says its input (ADR 0047 slice 5).
                    ctx.emit(RunEvent::Custom {
                        kind: "input_required".into(),
                        payload: json!({
                            "question": self.step_io.scrub(&asked),
                            "call_id": call.id,
                            "approval": true,
                        }),
                    })
                    .await;
                    self.observe_approval(
                        ctx,
                        state.root_run,
                        adam_plugin::EventKind::ApprovalAsked,
                        &format!("tool:{}", call.id),
                        (&call.name, &call.id),
                        |event, scrub| event.text("question", &question, scrub),
                    );
                    state.pending_wait = Some(PendingWait::Question(PendingQuestion {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        question: asked,
                        ui: None,
                        stream: None,
                    }));
                    state.approval = Some(crate::conversation::Approval {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        question,
                        arguments,
                        approved: false,
                    });
                    return Ok(Flow::Park);
                }
                ToolResult::AwaitRun(run) => {
                    state.repeats = None;
                    let wait = PendingRun {
                        call_id: call.id.clone(),
                        tool: call.name.clone(),
                        run,
                        model_wait: None,
                    };
                    self.observe_child(
                        ctx,
                        state.root_run,
                        adam_plugin::EventKind::ChildStarted,
                        (&call.name, &call.id),
                        (run, None),
                    );
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
                    self.observe_child(
                        ctx,
                        state.root_run,
                        adam_plugin::EventKind::ChildEnded,
                        (&call.name, &call.id),
                        (run, Some(outcome)),
                    );
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
        access_of(self.tool(&call.name), &self.sources, call)
    }

    /// How many of the owed calls, from the first, run together as one segment: `1` when the first
    /// runs by itself (the common case: a tool that declares nothing runs alone).
    fn together(&self, state: &Conversation) -> usize {
        let calls = &state.pending_calls;
        let parallel = self.parallel_of(state);
        if parallel <= 1 || calls.len() < 2 {
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
        match schedule::segments(&entries, parallel).first() {
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
        context: Option<&Arc<Map<String, Value>>>,
        early: &mut Option<Early<'_>>,
    ) -> Result<Option<Flow>, AgentError> {
        let calls: Vec<ToolCall> = state.pending_calls[..n].to_vec();
        let notes = state.source_notes.clone();
        // The calls that started while the model wrote: their work is done or still running. A
        // batch that is not this turn's calls is dropped whole, and every call runs here as today.
        let taken = match early.as_mut() {
            Some(early) => early.take(&calls).await,
            None => None,
        };
        let (mut first, mut started) = match taken {
            Some((head, results)) => (head, results),
            None => (None, (0..n).map(|_| None).collect()),
        };
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
            model_run_of(state),
            workspace_run_of(state),
            state.prompt_statement.as_ref(),
        );
        let facts_of_ctx = CtxFacts::of(ctx);
        let mut slots: Vec<Option<Slot>> = Vec::with_capacity(n);
        for (i, call) in calls.iter().enumerate() {
            if shortcuts[i].is_some() {
                slots.push(Some(Slot::Shortcut));
                continue;
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
            if let Some(finished) = started[i].take() {
                slots.push(Some(Slot::Started(Box::new(finished), step)));
                continue;
            }
            let facts = CallFacts {
                files_used: used,
                output_cut: state.output_cut,
                kept: Arc::clone(&state.kept_outputs),
                kept_left: MAX_RUN_KEPT_BYTES.saturating_sub(state.kept_bytes()),
                images: self.image_gate(state),
                task: state.task().map(Arc::from),
                loaded: Arc::from(state.loaded.clone()),
                removed_tools: Arc::from(state.removed_tools.clone()),
                narrowing: Arc::new(narrowing_of(state, context.map(|c| &**c))),
                event_budget: Arc::clone(&state.event_budget.0),
                approvable: approvable(ctx, state),
            };
            let mut one = self.prepare(&facts_of_ctx, context, root, &notes, call, facts);
            match &first {
                Some(head) => one.tool_ctx = one.tool_ctx.sharing_budgets_of(head),
                None => first = Some(one.tool_ctx.clone()),
            }
            slots.push(Some(Slot::Fresh(Box::new(one), step)));
        }

        let emitter = ctx.emitter();
        let cancel = ctx.cancel_token();
        let mut queue = Queue::new(&accesses, self.parallel_of(state));
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
                let Some(slot) = slots[i].take() else {
                    continue;
                };
                let work = match slot {
                    Slot::Shortcut => self
                        .drive(
                            i,
                            &calls[i],
                            &notes,
                            &emitter,
                            shortcuts[i].clone(),
                            None,
                            None,
                        )
                        .boxed(),
                    Slot::Fresh(prepared, step) => self
                        .drive(
                            i,
                            &calls[i],
                            &notes,
                            &emitter,
                            None,
                            Some(*prepared),
                            Some(step),
                        )
                        .boxed(),
                    Slot::Started(finished, step) => {
                        self.record_early(i, &calls[i], finished, step)
                    }
                };
                running.push(work);
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
                    // The calls of a segment ran together; their events are told in the order of the
                    // calls, whatever order they finished in (ADR 0050, decision 4).
                    self.observe_tool_started(ctx, state.root_run, call);
                    self.conclude(
                        ctx,
                        state.root_run,
                        &notes,
                        call,
                        *outcome,
                        &unjournaled,
                        files_kept(&state.artifacts),
                        Some(live),
                        is_unattended(ctx.conversation_id(), Some(&state.context)),
                        state.may_ask_person(ctx.conversation_id()),
                        false,
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
                loaded,
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
            if !loaded.is_empty() {
                state.load_groups(loaded);
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

    /// A seat of the process's tool pool for `conversation` ([`Pool::ToolCalls`], ADR 0065).
    ///
    /// `None` when the deployment built no admission, when the pool can never admit the call, or
    /// when waiting once more would pass `TOOL_ADMISSION_WAIT`: the call then runs ungated. A run
    /// never fails because a slot was busy, so the ceiling is a bulkhead and not a barrier.
    async fn seat_tool(&self, conversation: ConversationId) -> Option<Permit> {
        let admission = self.admission.as_ref()?;
        let deadline = std::time::Instant::now() + TOOL_ADMISSION_WAIT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match admission
                .admit(Ask::new(Pool::ToolCalls, conversation.clone()).wait(left))
                .await
            {
                Ok(permit) => return Some(permit),
                Err(Refused::Busy { retry_after, .. }) => {
                    let pause = retry_after.unwrap_or(Duration::from_millis(50));
                    if std::time::Instant::now() + pause >= deadline {
                        break;
                    }
                    tokio::time::sleep(pause).await;
                }
                Err(refused) => {
                    tracing::warn!(
                        pool = Pool::ToolCalls.as_str(),
                        %refused,
                        "the tool pool cannot admit this call: it runs nevertheless"
                    );
                    return None;
                }
            }
        }
        let limit = admission
            .snapshot()
            .pool(Pool::ToolCalls)
            .map_or(adam_budget::Limit::Unlimited, adam_budget::PoolUsage::limit);
        tracing::warn!(
            pool = Pool::ToolCalls.as_str(),
            %limit,
            waited_millis = u64::try_from(TOOL_ADMISSION_WAIT.as_millis()).unwrap_or(u64::MAX),
            "the tool pool stayed full: the call runs nevertheless, over its share"
        );
        None
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

    /// A call that started while the model wrote: its step records the outcome the call gave (its
    /// work is done, or still running), and a transient failure fails the transition, as a call the
    /// turn ran does.
    fn record_early(
        &self,
        i: usize,
        call: &ToolCall,
        finished: Box<Finished>,
        step: ReservedStep,
    ) -> BoxFuture<'static, (usize, Result<Finished, AgentError>)> {
        let call = call.clone();
        let policy = self.tool_retry_policy(&call.name);
        async move {
            let Finished::Ran {
                outcome,
                unjournaled,
                live,
            } = *finished
            else {
                return (
                    i,
                    Err(AgentError::permanent(
                        "a call that started early has no outcome".to_owned(),
                    )),
                );
            };
            let journaled: Result<HookedOutput, ToolError> =
                match step.run(move || async move { *outcome }).await {
                    Ok(outcome) => outcome,
                    Err(error) => return (i, Err(error)),
                };
            let failure = transient_failure(&call, &journaled, policy);
            let finished = Finished::Ran {
                outcome: Box::new(journaled),
                unjournaled,
                live,
            };
            match failure {
                Some(error) => (i, Err(error)),
                None => (i, Ok(finished)),
            }
        }
        .boxed()
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
        let conversation = prepared
            .tool_ctx
            .conversation_id()
            .map(ConversationId::from)
            .unwrap_or_else(|| ConversationId::new(emitter.run_id().to_string()));
        // Held until the call's work is done: the process's tool pool, one share per call.
        let _seat = self.seat_tool(conversation).await;
        let unjournaled = prepared.unjournaled.clone();
        let work = self.step_work(call, self.source_note(notes, &call.name), prepared, None);
        let outcome: Result<HookedOutput, ToolError> = step.run(move || work).await?;
        // How it ended, said now; `conclude` says it again only if applying the result changes it.
        let live = self.say_outcome(call, notes, emitter, &outcome).await;
        if let Some(error) = transient_failure(call, &outcome, self.tool_retry_policy(&call.name)) {
            return Err(error);
        }
        Ok(Finished::Ran {
            outcome: Box::new(outcome),
            unjournaled,
            live,
        })
    }

    /// What a call's outcome says about its step's end, said **now**, as it happens: the end event,
    /// and what applying the result may change (so that it is said again only then). A transient
    /// failure is said too, and the transition fails after it ([`transient_failure`]).
    async fn say_outcome(
        &self,
        call: &ToolCall,
        notes: &[ToolNote],
        emitter: &Emitter,
        outcome: &Result<HookedOutput, ToolError>,
    ) -> Live {
        let silent = self
            .source_note(notes, &call.name)
            .is_some_and(|n| n.reports_step);
        match outcome {
            // A call that waits for a person's approval (slice H8) has no end to say yet: `conclude`
            // says it waits, or, beside other calls, that it was not run.
            Ok(hooked) if hooked.awaits_approval().is_some() => Live { said: None },
            // Rounds are reported only by a call that runs alone (the loop reads them in a step of
            // its own, which a segment cannot place between the calls).
            Ok(hooked) if reports_rounds(&hooked.output) => {
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
            Ok(hooked) => {
                let output = &hooked.output;
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
            Err(
                ToolError::Transient(reason)
                | ToolError::Retryable {
                    message: reason, ..
                },
            ) => {
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
        }
    }

    /// The work of a call that started while the model was still writing: run it, and say how it
    /// ended as it happens. The turn's step records the outcome ([`Self::run_segment`]).
    fn early_work(
        &self,
        i: usize,
        call: ToolCall,
        notes: Vec<ToolNote>,
        emitter: Emitter,
        prepared: Prepared,
    ) -> BoxFuture<'static, (usize, Finished)> {
        let agent = self.clone();
        async move {
            let unjournaled = prepared.unjournaled.clone();
            let note = agent.source_note(&notes, &call.name).cloned();
            let work = agent.step_work(&call, note.as_ref(), prepared, None);
            let outcome = work.await;
            let live = agent.say_outcome(&call, &notes, &emitter, &outcome).await;
            (
                i,
                Finished::Ran {
                    outcome: Box::new(outcome),
                    unjournaled,
                    live,
                },
            )
        }
        .boxed()
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
        ctx: &CtxFacts,
        context: Option<&Arc<Map<String, Value>>>,
        root: RootFacts<'_>,
        notes: &[ToolNote],
        call: &ToolCall,
        facts: CallFacts,
    ) -> Prepared {
        let CallFacts {
            files_used,
            kept,
            kept_left,
            images,
            task,
            loaded,
            removed_tools,
            narrowing,
            event_budget,
            approvable,
            ..
        } = facts;
        let note = self.source_note(notes, &call.name);
        let root_run = root.0.unwrap_or(ctx.run).to_string();
        let tool_ctx = self
            .tool_ctx(ctx, context, root, notes, &call.id, &call.name)
            .with_note(note.cloned())
            .with_task(task.clone())
            .with_loaded(Arc::clone(&loaded))
            .with_removed_tools(Arc::clone(&removed_tools))
            .with_files_left((MAX_RUN_FILE_BYTES as u64).saturating_sub(files_used))
            .with_kept(kept_left, kept)
            .with_event_scope(self.events.scope(&call.name, event_budget))
            .with_images(
                images,
                image_scope(ctx.conversation_id.as_deref(), &root_run),
                root_run,
            );
        // What a source holds back is refused here too, inside the step that records the answer:
        // the model was shown the tools of a turn that may be over, or named one it was not shown.
        let holding = (!self.sources.is_empty()).then(|| {
            (
                self.sources.clone(),
                SourceCtx::new(
                    ctx.run,
                    root.0.unwrap_or(ctx.run),
                    ctx.conversation_id.clone(),
                    context.cloned().unwrap_or_default(),
                )
                .with_task(task.as_deref())
                .with_workspace_run(root.4.unwrap_or_else(|| root.0.unwrap_or(ctx.run)))
                .with_loaded(loaded)
                .with_removed_tools(removed_tools),
            )
        });
        Prepared {
            tool_ctx,
            holding,
            narrowing,
            // What the call reported as sandbox usage rides in its result, which the step journals; a
            // call that ends in an error has no result to carry it, and leaves it here.
            unjournaled: Arc::new(Mutex::new(SandboxUsage::new())),
            approvable,
        }
    }

    /// The work of the journaled step `tool:<call id>`, which runs only if nothing is recorded: the
    /// call to the agent's own tool or to a source's, within its time limit, after the check of what a
    /// source holds back and around the hooks (ADR 0050, slice H1).
    ///
    /// The hooks run **here**, inside the step: a `before_tool` rewrite changes the arguments the tool
    /// runs with, a `tool.after` rewrite changes the text the model reads, and the decisions are
    /// returned beside the output ([`HookedOutput`]) so the one journal write records both. A replay
    /// that finds the step recorded runs none of this, so no hook is asked twice.
    ///
    /// `approved` is the work of the step `tool:<call id>:approved` (slice H8): the call a person
    /// approved runs with those arguments, the ones the step that asked recorded, and no
    /// `before_tool` hook is asked again (the hooks decided, the person approved). What a source
    /// holds back is still refused, and `after_tool` still reads what the call answered.
    fn step_work(
        &self,
        call: &ToolCall,
        note: Option<&ToolNote>,
        prepared: Prepared,
        approved: Option<Value>,
    ) -> BoxFuture<'static, Result<HookedOutput, ToolError>> {
        let Prepared {
            tool_ctx,
            holding,
            narrowing,
            unjournaled,
            approvable,
        } = prepared;
        let call = call.clone();
        let name = call.name.clone();
        let hooks = Arc::clone(&self.hooks);
        let own = self.tool(&call.name).cloned();
        let sources = self.sources.clone();
        let unknown = self.unknown_tool(&call.name);
        let source_limit = note
            .and_then(|n| n.timeout_ms)
            .map_or(DEFAULT_TOOL_TIME_LIMIT, |ms| {
                Duration::from_millis(ms) + SOURCE_LIMIT_MARGIN
            });
        async move {
            // A session's own narrowing, first and with or without sources (ADR 0054 slice 3).
            if let Some(held) = narrowing.hold(&name) {
                return Ok(HookedOutput::plain(
                    ToolOutput::error(held.reason).sanitized(),
                ));
            }
            if let Some((sources, source_ctx)) = &holding
                && let Some(held) = withheld(&narrowing, sources, source_ctx, &[name.as_str()])
                    .await
                    .into_iter()
                    .next()
            {
                return Ok(HookedOutput::plain(
                    ToolOutput::error(held.reason).sanitized(),
                ));
            }
            let mut hooks_run: Vec<HookDecision> = Vec::new();
            let args = match approved {
                // Approved: the arguments the asking step recorded, and no hook asked again.
                Some(args) => args,
                None => {
                    let mut args = call.arguments.clone();
                    // `tool.before`: the arguments a rewrite made, or a verdict that ends the call
                    // here (an ask among them: the run parks, and the call runs once approved).
                    let refusal = decide_before(
                        &hooks,
                        &call,
                        &mut args,
                        (own.as_ref(), &sources),
                        approvable,
                        &mut hooks_run,
                    )?;
                    if let Some(refusal) = refusal {
                        return Ok(refusal);
                    }
                    args
                }
            };
            let rewritten = args != call.arguments;
            // The arguments the tool ran with, for the two `within` closures: each would otherwise
            // take `args` from this scope, and the after hook reads them again below.
            let ran = args.clone();
            let called = match own {
                Some(tool) => {
                    let limit = tool.time_limit().unwrap_or(DEFAULT_TOOL_TIME_LIMIT);
                    within(&tool_ctx, &name, limit, |ctx| async move {
                        tool.call(&ctx, ran).await
                    })
                    .await
                }
                None => {
                    let tool_name = name.clone();
                    within(&tool_ctx, &name, source_limit, |ctx| async move {
                        for source in &sources {
                            if let Some(answer) = source.call(&ctx, &tool_name, ran.clone()).await {
                                return answer;
                            }
                        }
                        Ok(ToolOutput::error(unknown))
                    })
                    .await
                }
            };
            // The one place a call's output becomes its journaled step and its model message: a NUL
            // a tool printed is stored and shown as U+FFFD, before either (ADR 0051).
            let answered =
                with_reported_sandbox(&tool_ctx, &unjournaled, called).map(ToolOutput::sanitized);
            let output = decide_after(&hooks, &name, &args, answered, &mut hooks_run);
            // What the call emitted is recorded with its result, so delivery reads the record.
            let events = tool_ctx.emitted_events();
            output.map(|output| HookedOutput {
                output,
                hooks: hooks_run,
                args: rewritten.then_some(args),
                events,
                approval: None,
            })
        }
        .boxed()
    }

    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), tool = %call.name, call_id = %call.id))]
    #[allow(clippy::too_many_arguments)] // one call's pieces, handed over once
    async fn run_tool(
        &self,
        ctx: &mut Ctx,
        context: Option<&Arc<Map<String, Value>>>,
        root: RootFacts<'_>,
        notes: &[ToolNote],
        call: &ToolCall,
        facts: CallFacts,
        started: Option<Finished>,
        may_ask: bool,
        approved: Option<Value>,
    ) -> Result<(ToolResult, SandboxUsage), AgentError> {
        let files_used = facts.files_used;
        // A call that started while the model wrote: its work is done, so the step records what it
        // gave, and its end was said as it happened.
        if let Some(finished) = started {
            let Finished::Ran {
                outcome,
                unjournaled,
                live,
            } = finished
            else {
                return Err(AgentError::permanent(
                    "a call that started early has no outcome".to_owned(),
                ));
            };
            let outcome: Result<HookedOutput, ToolError> = ctx
                .step(
                    &format!("tool:{}", call.id),
                    move || async move { *outcome },
                )
                .await?;
            return self
                .conclude(
                    ctx,
                    root.0,
                    notes,
                    call,
                    outcome,
                    &unjournaled,
                    files_used,
                    Some(live),
                    is_unattended(ctx.conversation_id(), context.map(|c| c.as_ref())),
                    may_ask,
                    true,
                )
                .await;
        }
        // The system behind the source reports each call itself: the agent reports no step of its own.
        let silent = self
            .source_note(notes, &call.name)
            .is_some_and(|n| n.reports_step);
        say(ctx, silent, self.start_event(call, notes)).await;

        // Arguments that were not JSON, or a tool nobody has: nothing runs, and the model is told
        // why. The answer comes from the recorded model turn alone, so a replay gives the same one.
        // A call a person approved took its first step already, so it has none.
        if approved.is_none()
            && let Some(said) = self.shortcut(call, facts.output_cut)
        {
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
        let prepared = self.prepare(&CtxFacts::of(ctx), context, root, notes, call, facts);
        let unjournaled = prepared.unjournaled.clone();
        // A call a person approved runs in a step of its own (ADR 0050, slice H8): the step that
        // asked is recorded, and this one is taken only after the yes, so a replay that finds it
        // recorded runs the call no second time.
        let step = match approved {
            Some(_) => approval_step(&call.id),
            None => format!("tool:{}", call.id),
        };
        let work = self.step_work(
            call,
            self.source_note(notes, &call.name),
            prepared,
            approved,
        );
        let outcome: Result<HookedOutput, ToolError> = ctx.step(&step, move || work).await?;
        let unattended = is_unattended(ctx.conversation_id(), context.map(|c| c.as_ref()));
        self.conclude(
            ctx,
            root.0,
            notes,
            call,
            outcome,
            &unjournaled,
            files_used,
            None,
            unattended,
            may_ask,
            true,
        )
        .await
    }

    /// What the call's recorded outcome means for the run: the message that answers it, its artifacts
    /// and what it asks the run to keep, or a wait. The step's end is said here, unless `live` says
    /// the call's end was already said as it happened and applying the result did not change it.
    /// `live` is `Some` for a call of a segment of several, which cannot ask or wait, and for a call
    /// that started while the model wrote. `alone` is `true` for a call the loop runs by itself (one
    /// that started early included): only such a call parks for a person's approval (slice H8).
    #[allow(clippy::too_many_arguments)] // the call's pieces, handed over once
    async fn conclude(
        &self,
        ctx: &Ctx,
        root: Option<RunId>,
        notes: &[ToolNote],
        call: &ToolCall,
        outcome: Result<HookedOutput, ToolError>,
        unjournaled: &Mutex<SandboxUsage>,
        files_used: u64,
        live: Option<Live>,
        unattended: bool,
        may_ask: bool,
        alone: bool,
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
        // A call that waits for a person's approval has no end yet (slice H8): its end is the step
        // that runs it, or the refusal, after the answer.
        let awaits_approval = matches!(&outcome, Ok(hooked) if hooked.awaits_approval().is_some());
        if !awaits_approval {
            self.observe_tool_ended(ctx, root, &call.name, &call.id, &outcome);
        }
        // The custom events the step recorded (ADR 0050, decision 6), after `tool.completed` and in
        // the order the call emitted them: read from the record, so a replay says them again.
        if let Ok(hooked) = &outcome {
            self.deliver_events(ctx, root, call, hooked, silent).await;
        }
        // A hook that stopped the run (ADR 0050, decision 8): the call's result is its reason, and the
        // run fails after the step that recorded it.
        if let Ok(hooked) = &outcome
            && let Some(reason) = hooked.stopped()
        {
            let reason = reason.to_owned();
            if !said_live(StepState::Failed, Some(&reason)) {
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
            return Err(AgentError::permanent(format!(
                "stopped by a plugin: {reason}"
            )));
        }
        let reported = match &outcome {
            Ok(hooked) => hooked.output.sandbox.clone(),
            // Tried again: only the try that is kept counts.
            Err(ToolError::Transient(_) | ToolError::Retryable { .. }) => SandboxUsage::new(),
            Err(_) => unjournaled
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        };
        let answered = match outcome {
            // A hook asked a person to approve the call (ADR 0050, decision 12, slice H8). A call
            // that ran beside others cannot wait: the model is told to call it alone. Alone, the run
            // parks on the question, and the call runs once the person says yes.
            Ok(hooked) if hooked.awaits_approval().is_some() => {
                if live.is_some() && !alone {
                    say(
                        ctx,
                        silent,
                        self.step_end(
                            &call.name,
                            &call.id,
                            StepState::Failed,
                            Some((APPROVAL_NOT_ALONGSIDE, true)),
                            notes,
                        ),
                    )
                    .await;
                    ToolResult::error(&call.id, APPROVAL_NOT_ALONGSIDE.to_owned())
                } else {
                    say(ctx, silent, end(StepState::Waiting)).await;
                    ToolResult::NeedsApproval {
                        question: hooked.approval.unwrap_or_default(),
                        arguments: hooked.args.unwrap_or_else(|| call.arguments.clone()),
                    }
                }
            }
            Ok(hooked) if live.is_some() && reports_rounds(&hooked.output) => {
                ToolResult::error(&call.id, NOT_ALONGSIDE.to_owned())
            }
            Ok(hooked) => {
                let mut output = hooked.output;
                let repeat_key = output.repeat_key.as_deref().cloned();
                let reported = if output.is_error {
                    Vec::new()
                } else {
                    std::mem::take(&mut output.reported)
                };
                let pinned = output.pinned && !output.is_error;
                let (message, artifacts, answer, plan, loaded, outcome, kept, kept_images) =
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
                    loaded,
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
            Err(error @ (ToolError::Transient(_) | ToolError::Retryable { .. })) => {
                let (reason, class) = error.retry_parts().map_or(
                    (String::new(), adam_error::ErrorClass::Transient),
                    |(r, c)| (r.to_owned(), c),
                );
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
                return Err(self.tool_retry_error(
                    &call.name,
                    format!("tool `{}` failed: {reason}", call.name),
                    class,
                ));
            }
            // Beside other calls a question or a wait has nowhere to park: the model is told to call
            // the tool alone (its end was said as it happened).
            Err(
                ToolError::NeedsInput { .. }
                | ToolError::AwaitRun { .. }
                | ToolError::AwaitSession { .. }
                | ToolError::AwaitRemote { .. },
            ) if live.is_some() => ToolResult::error(&call.id, NOT_ALONGSIDE.to_owned()),
            // Nobody answers a firing of a schedule: the question is refused, and the model goes on.
            Err(ToolError::NeedsInput { question, .. }) if unattended => {
                let reason = format!("{UNATTENDED}. The question was: {question}");
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
                ToolResult::error(&call.id, backstop(&reason, &call.name, false).into_owned())
            }
            // Only a round of a kept-open session a client shows may ask the person (ADR 0045
            // decision 9): a child that cannot would wait for an answer nobody can give.
            Err(ToolError::NeedsInput { question, .. }) if !may_ask => {
                let reason = format!("{NOT_ASKABLE}. The question was: {question}");
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
                ToolResult::error(&call.id, backstop(&reason, &call.name, false).into_owned())
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

    /// Delivers the custom events `hooked` records for `call`, in order (ADR 0050, slice H5).
    ///
    /// Each has the key `<run>/tool:<call id>/<name>/<index>`, which is the same on a replay. Every
    /// event goes to the observers that subscribe to it; one declared for clients also goes out as a
    /// `RunEvent::Custom` (`kind` the name, `payload` the version, the key and the data), which the A2A
    /// layer serves as a data part. One declared `Hooks` stays here. A name this build no longer
    /// declares is delivered to nobody: who may see an event is decided by the build that delivers it.
    async fn deliver_events(
        &self,
        ctx: &Ctx,
        root: Option<RunId>,
        call: &ToolCall,
        hooked: &HookedOutput,
        silent: bool,
    ) {
        use adam_plugin::Visibility;
        if hooked.events.is_empty() {
            return;
        }
        let scrub = |s: &str| self.step_io.scrub(s);
        let mut observed = Vec::new();
        for (index, event) in hooked.events.iter().enumerate() {
            let Some(spec) = self.events.spec(&event.name) else {
                tracing::warn!(
                    run = %ctx.run_id(),
                    event = %event.name,
                    "a recorded custom event is not declared by this build: not delivered"
                );
                continue;
            };
            let run = ctx.run_id().to_string();
            let key = format!("{run}/tool:{}/{}/{index}", call.id, event.name);
            if !self.hooks.is_empty() {
                let header = adam_plugin::EventHeader {
                    key: key.clone(),
                    root_run: root.map_or_else(|| run.clone(), |r| r.to_string()),
                    run,
                    agent: self.name.clone(),
                    at: jiff::Timestamp::now().to_string(),
                };
                observed.push(adam_plugin::HookEvent::custom(
                    &event.name,
                    event.version,
                    header,
                    &event.data,
                    &scrub,
                ));
            }
            if spec.visibility == Visibility::Clients {
                say(
                    ctx,
                    silent,
                    RunEvent::Custom {
                        kind: event.name.clone(),
                        payload: json!({
                            "version": event.version,
                            "key": key,
                            "data": event.data,
                        }),
                    },
                )
                .await;
            }
        }
        self.flush_events(observed);
    }

    /// The observers' flush for events of `call`'s step (ADR 0050, slice H2): the events are queued
    /// in the order they happened and offered to every plugin before the loop goes on, so they reach
    /// observers before the transition commits. An observer's failure is counted and dropped.
    fn flush_events(&self, events: Vec<adam_plugin::HookEvent>) {
        if self.hooks.is_empty() {
            return;
        }
        let mut queue = adam_plugin::EventQueue::new();
        for event in events {
            queue.push(event);
        }
        let report = queue.flush(&self.hooks);
        if report.failed > 0 || report.dropped > 0 {
            tracing::warn!(
                failed = report.failed,
                dropped = report.dropped,
                "observers did not take every event"
            );
        }
    }

    fn event_header(
        &self,
        ctx: &Ctx,
        root: Option<RunId>,
        step: &str,
        kind: adam_plugin::EventKind,
    ) -> adam_plugin::EventHeader {
        let run = ctx.run_id().to_string();
        adam_plugin::EventHeader {
            key: format!("{run}/{step}/{}/0", kind.name()),
            root_run: root.map_or_else(|| run.clone(), |r| r.to_string()),
            run,
            agent: self.name.clone(),
            at: jiff::Timestamp::now().to_string(),
        }
    }

    /// `tool.started`: a call's step is about to run. It is emitted before the step, so a replay
    /// emits it again with the same key.
    fn observe_tool_started(&self, ctx: &Ctx, root: Option<RunId>, call: &ToolCall) {
        if self.hooks.is_empty() {
            return;
        }
        let scrub = |s: &str| self.step_io.scrub(s);
        let kind = adam_plugin::EventKind::ToolStarted;
        let header = self.event_header(ctx, root, &format!("tool:{}", call.id), kind);
        let event = adam_plugin::HookEvent::new(kind, header)
            .text("tool", &call.name, &scrub)
            .text("call_id", &call.id, &scrub)
            .json("arguments", &call.arguments, &scrub);
        self.flush_events(vec![event]);
    }

    /// `tool.completed`, `tool.failed` or `tool.denied`, from the step's record. A call that is
    /// tried again, or parks on a question or a wait, has no end yet and says nothing.
    fn observe_tool_ended(
        &self,
        ctx: &Ctx,
        root: Option<RunId>,
        tool: &str,
        call_id: &str,
        outcome: &Result<HookedOutput, ToolError>,
    ) {
        use adam_plugin::EventKind;
        if self.hooks.is_empty() {
            return;
        }
        let scrub = |s: &str| self.step_io.scrub(s);
        let (kind, field, text): (EventKind, &str, String) = match outcome {
            Ok(hooked) => {
                if let Some(refusal) = hooked.hooks.iter().find(|d| d.verdict != "continue") {
                    (
                        EventKind::ToolDenied,
                        "reason",
                        refusal.reason.clone().unwrap_or_default(),
                    )
                } else if hooked.output.is_error {
                    (
                        EventKind::ToolFailed,
                        "output",
                        hooked.output.content.clone(),
                    )
                } else {
                    (
                        EventKind::ToolCompleted,
                        "output",
                        hooked.output.content.clone(),
                    )
                }
            }
            Err(ToolError::Permanent(reason)) => (EventKind::ToolFailed, "reason", reason.clone()),
            Err(_) => return,
        };
        // Whether a hook rewrote anything is part of the record, not of this event.
        let header = self.event_header(ctx, root, &format!("tool:{call_id}"), kind);
        let event = adam_plugin::HookEvent::new(kind, header)
            .text("tool", tool, &scrub)
            .text("call_id", call_id, &scrub)
            .text(field, &text, &scrub);
        let mut events = vec![event];
        if kind == EventKind::ToolCompleted
            && let Ok(hooked) = outcome
        {
            events.extend(self.session_event(
                ctx,
                root,
                tool,
                call_id,
                &hooked.output.content,
                &hooked.output.reported,
            ));
        }
        self.flush_events(events);
    }

    /// The session event of a session tool's answer (ADR 0050 decision 5, slice H7): what the tool
    /// told the model, read back, so an observer sees what the model saw. A refused call (an error
    /// answer) is a `tool.failed` and no session event.
    fn session_event(
        &self,
        ctx: &Ctx,
        root: Option<RunId>,
        tool: &str,
        call_id: &str,
        content: &str,
        reported: &[RunId],
    ) -> Option<adam_plugin::HookEvent> {
        use adam_plugin::EventKind;
        let kind = match tool {
            SUBAGENT_START => EventKind::SessionStarted,
            SUBAGENT_SEND => EventKind::SessionRoundStarted,
            SUBAGENT_READ if !reported.is_empty() => EventKind::SessionRoundEnded,
            SUBAGENT_CLOSE => EventKind::SessionClosed,
            _ => return None,
        };
        let scrub = |s: &str| self.step_io.scrub(s);
        let header = self.event_header(ctx, root, &format!("tool:{call_id}"), kind);
        let mut event = adam_plugin::HookEvent::new(kind, header).text("call_id", call_id, &scrub);
        match kind {
            EventKind::SessionRoundEnded => {
                for run in reported {
                    event = event.text("round_run", &run.to_string(), &scrub);
                }
            }
            _ => {
                let body: Value = serde_json::from_str(content).ok()?;
                if let Some(session) = body.get("session").and_then(Value::as_str) {
                    event = event.text("session", session, &scrub);
                }
                if let Some(agent) = body.get("agent").and_then(Value::as_str) {
                    event = event.text("agent", agent, &scrub);
                }
                if let Some(keep) = body.get("keep_open").and_then(Value::as_bool) {
                    event = event.plain("keep_open", keep);
                }
                if let Some(round) = body.get("round").and_then(Value::as_u64) {
                    event = event.plain("round", round);
                }
            }
        }
        Some(event)
    }

    /// `child.started` or `child.ended`, for the one-shot child run a tool waits for.
    fn observe_child(
        &self,
        ctx: &Ctx,
        root: Option<RunId>,
        kind: adam_plugin::EventKind,
        (tool, call_id): (&str, &str),
        (child, status): (RunId, Option<StepState>),
    ) {
        if self.hooks.is_empty() {
            return;
        }
        let scrub = |s: &str| self.step_io.scrub(s);
        let header = self.event_header(ctx, root, &format!("tool:{call_id}"), kind);
        let mut event = adam_plugin::HookEvent::new(kind, header)
            .text("tool", tool, &scrub)
            .text("call_id", call_id, &scrub)
            .text("child_run", &child.to_string(), &scrub);
        if let Some(status) = status {
            let word = if status == StepState::Completed {
                "done"
            } else {
                "failed"
            };
            event = event.text("status", word, &scrub);
        }
        self.flush_events(vec![event]);
    }

    /// `approval.asked` or `approval.answered` (ADR 0050, decision 5, slice H8) for the call
    /// `call_id` of `tool`, keyed under `step`: the asking call's step for the ask (so a replay of the
    /// transition that parked tells it again with the same key), and the id of the message that
    /// answered for the answer. `more` adds the event's own fields.
    fn observe_approval(
        &self,
        ctx: &Ctx,
        root: Option<RunId>,
        kind: adam_plugin::EventKind,
        step: &str,
        (tool, call_id): (&str, &str),
        more: impl FnOnce(adam_plugin::HookEvent, &dyn Fn(&str) -> String) -> adam_plugin::HookEvent,
    ) {
        if self.hooks.is_empty() {
            return;
        }
        let scrub = |s: &str| self.step_io.scrub(s);
        let header = self.event_header(ctx, root, step, kind);
        let event = adam_plugin::HookEvent::new(kind, header)
            .text("tool", tool, &scrub)
            .text("call_id", call_id, &scrub);
        self.flush_events(vec![more(event, &scrub)]);
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
        ctx: &CtxFacts,
        context: Option<&Arc<Map<String, Value>>>,
        (root_run, root_step, plan, model_run, workspace_run, prompt_statement): RootFacts<'_>,
        notes: &[ToolNote],
        call_id: &str,
        tool: &str,
    ) -> ToolCtx {
        ToolCtx::new(
            ctx.conversation_id.clone(),
            ctx.attempt,
            call_id.to_owned(),
            tool.to_owned(),
            ctx.emitter.clone(),
            ctx.cancel.clone(),
            Arc::clone(&self.extensions),
            Some(ctx.child_starter.clone()),
            context.cloned().unwrap_or_default(),
            root_run,
            root_step.map(str::to_owned),
            self.style_of(tool, notes)
                .event(tool, call_id, StepState::Running),
        )
        .with_model_run(model_run.or(root_run).unwrap_or(ctx.run))
        .with_workspace_run(workspace_run.or(root_run).unwrap_or(ctx.run))
        .with_plan(plan.cloned())
        .with_prompt_statement(prompt_statement.cloned())
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
                state.tool_calls,
                self.limits_of(state).max_tool_calls
            ),
            LimitKind::MaxTurns => format!(
                "turn limit exceeded: the model was called {} times (max_turns = {})",
                state.turns,
                self.limits_of(state).max_turns
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
        context: Option<&Arc<Map<String, Value>>>,
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
                &CtxFacts::of(ctx),
                context,
                (
                    state.root_run,
                    state.root_step.as_deref(),
                    state.plan.as_ref(),
                    model_run_of(state),
                    workspace_run_of(state),
                    state.prompt_statement.as_ref(),
                ),
                &state.source_notes,
                &wait.call_id,
                &wait.tool,
            )
            .with_task(state.task().map(Arc::from))
            .with_loaded(Arc::from(state.loaded.clone()))
            .with_removed_tools(Arc::from(state.removed_tools.clone()))
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
                    // A wait that settles here is the same read as one answered at once: the same
                    // event, under the same key.
                    let root = state.root_run;
                    let event = self.session_event(
                        ctx,
                        root,
                        &wait.tool,
                        &wait.call_id,
                        &output.content,
                        &reported,
                    );
                    self.flush_events(event.into_iter().collect());
                    state.note_reported(&reported);
                    Self::count_rounds(ctx, state, false).await?;
                }
                let used = files_kept(&state.artifacts);
                let (message, artifacts, answer, plan, loaded, outcome, kept, kept_images) =
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
                if !loaded.is_empty() {
                    state.load_groups(loaded);
                }
                owed(state, message);
                Ok(Some(outcome))
            }
            Err(error @ (ToolError::Transient(_) | ToolError::Retryable { .. })) => {
                let (reason, class) = error
                    .retry_parts()
                    .unwrap_or(("", adam_error::ErrorClass::Transient));
                Err(self.tool_retry_error(
                    &wait.tool,
                    format!(
                        "tool `{}` failed while polling its remote task: {reason}",
                        wait.tool
                    ),
                    class,
                ))
            }
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
    Vec<Loaded>,
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
    let loaded = if output.is_error {
        Vec::new()
    } else {
        std::mem::take(&mut output.loaded)
    };
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
        extra_content: None,
        call_id: call_id.to_owned(),
        // After the journal: the journal holds the whole output, and the cut (which the history, the
        // model and the step event carry) is a pure function of it.
        content: backstop(&output.content, tool, paged).into_owned(),
        is_error: output.is_error,
        images,
    };
    (
        message,
        refs,
        answer,
        plan,
        loaded,
        outcome,
        kept,
        kept_images,
    )
}

/// The run a model call of this run names ([`ModelRequest::run`], ADR 0067): the run its parent's
/// calls name, else the root its session's header names (the task that sent the round), else the
/// root run this one works for, else this run. From in-process state only, never from a message's
/// context, which a person can set.
fn model_run(state: &Conversation, ctx: &Ctx) -> RunId {
    model_run_of(state).unwrap_or_else(|| ctx.run_id())
}

/// [`model_run`] when the state says one: the run the run that started this one named
/// ([`Conversation::model_run`]), else the root its session's header names, else its root run.
fn model_run_of(state: &Conversation) -> Option<RunId> {
    state
        .model_run
        .or_else(|| state.session.as_ref().and_then(|header| header.root))
        .or(state.root_run)
}

/// The run whose workspace this run's calls read, when its session's header names one (ADR 0045
/// slice 7): the task a person's session was opened in, so its rounds read that repository. `None`
/// when the run is no round of a session, or its session names none; the calls then read the
/// workspace of [`ToolCtx::root_run_id`](crate::ToolCtx::root_run_id), as they always did.
fn workspace_run_of(state: &Conversation) -> Option<RunId> {
    state.session.as_ref().and_then(|header| header.workspace)
}

/// The first user message of a run, as a fresh [`Conversation`]. Shared by
/// [`LlmAgent::init`] and [`LlmStarter::init`], so they cannot drift apart.
fn start_conversation(input: Inbound) -> Result<Conversation, AgentError> {
    let text = start_text(&input)?;
    let mut conversation = Conversation::new(text);
    conversation.pending_images = parse_inbound_images(&input.payload);
    conversation.root_run = parse_root_run(&input.payload);
    conversation.model_run = parse_model_run(&input.payload);
    conversation.root_step = parse_root_step(&input.payload);
    conversation.session = parse_session(&input.payload);
    conversation.prompt_statement = parse_prompt_statement(&input.payload);
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
/// The name of the step that decides the message a run starts with (`message.received`, ADR 0050
/// slice H1b). It is taken only when a hook set is installed.
const START_MESSAGE_STEP: &str = "message:start";

/// The step that decides one message delivered to a running run: `message:<inbound id>`.
fn message_step(inbound: &Inbound) -> String {
    format!("message:{}", inbound.id)
}

/// Why a `message.received` hook could not decide: the step records it so a retryable failure
/// fails the transition, which the runtime tries again (ADR 0050, decision 8).
#[derive(Debug, Serialize, Deserialize)]
struct MessageStepError {
    plugin: String,
    reason: String,
}

/// Asks the `message.received` hooks about one message, inside the journaled step `name` (ADR 0050,
/// slice H1b). A recorded step is what a replay applies, so no hook is asked again. A plan that
/// cannot read the record is the runtime's `NonDeterminism`, as for any step.
async fn decide_message(
    agent_hooks: &Arc<HookSet>,
    ctx: &mut Ctx,
    name: &str,
    text: &str,
    files: Vec<MessageFile>,
) -> Result<MessageDecision, AgentError> {
    let hooks = Arc::clone(agent_hooks);
    let original = text.to_owned();
    let decided = ctx
        .step(
            name,
            move || async move { ask_message(&hooks, original, files) },
        )
        .await?;
    match decided {
        Ok(decision) => Ok(decision),
        Err(error) => Err(AgentError::transient(format!(
            "the hook {} could not decide a message: {}",
            error.plugin, error.reason
        ))),
    }
}

/// What the hooks say about one message: allowed as it was, rewritten, or refused. A hook that
/// cannot say `Continue` never lets the message through.
fn ask_message(
    hooks: &HookSet,
    mut text: String,
    files: Vec<MessageFile>,
) -> Result<MessageDecision, MessageStepError> {
    let original = text.clone();
    let decided = {
        let mut message = HookedMessage {
            text: &mut text,
            files: &files,
        };
        hooks.message_received(&mut message)
    };
    Ok(match decided {
        Ok(None) => MessageDecision {
            text: (text != original).then_some(text),
            hook: None,
        },
        Ok(Some(decided)) => {
            let plugin = decided.plugin;
            match decided.verdict {
                Verdict::Continue => MessageDecision {
                    text: (text != original).then_some(text),
                    hook: None,
                },
                Verdict::Deny(reason) => MessageDecision {
                    text: None,
                    hook: Some(
                        HookDecision::new(
                            HookDecision::MESSAGE_RECEIVED,
                            HookDecision::DENY,
                            Some(&reason),
                        )
                        .by(plugin),
                    ),
                },
                Verdict::Stop(reason) => MessageDecision {
                    text: None,
                    hook: Some(
                        HookDecision::new(
                            HookDecision::MESSAGE_RECEIVED,
                            HookDecision::STOP,
                            Some(&reason),
                        )
                        .by(plugin),
                    ),
                },
                // The point allows, rewrites or refuses: an ask is a verdict this point does not
                // have, so it is a failure of the hook and the message does not go through.
                Verdict::Ask(reason) => refuse_at_message(
                    plugin,
                    &format!(
                        "the hook asked at message.received, which only allows or refuses: {reason}"
                    ),
                ),
            }
        }
        Err(error) if error.is_retryable() => {
            return Err(MessageStepError {
                plugin: error.plugin,
                reason: error.reason,
            });
        }
        Err(error) => refuse_at_message(&error.plugin, &error.reason),
    })
}

/// A message refused because the hook could not decide a verdict of this point.
fn refuse_at_message(plugin: &str, reason: &str) -> MessageDecision {
    MessageDecision {
        text: None,
        hook: Some(
            HookDecision::new(
                HookDecision::MESSAGE_RECEIVED,
                HookDecision::FAILED,
                Some(reason),
            )
            .by(plugin),
        ),
    }
}

/// Decide the messages that arrived (`message.received`, ADR 0050 slice H1b) and return the ones the
/// run keeps: each in a journaled step of its own, so a replay asks no hook again. A refused message
/// is left out and its plugin and reason are said as progress; the task goes on. A message the run
/// already read, or one that is not a person's words, is returned as it is.
async fn decide_inbox(
    hooks: &Arc<HookSet>,
    ctx: &mut Ctx,
    state: &mut Conversation,
    inbox: Vec<Inbound>,
) -> Result<Vec<Inbound>, AgentError> {
    if hooks.is_empty() {
        return Ok(inbox);
    }
    let mut kept = Vec::with_capacity(inbox.len());
    let mut seen: HashSet<String> = HashSet::new();
    for mut inbound in inbox {
        if state.has_read(&inbound.id) || !seen.insert(inbound.id.clone()) {
            kept.push(inbound);
            continue;
        }
        let Ok(text) = parse_user_text(&inbound.payload) else {
            // Not a person's message (an unreadable payload): the loop says so when it reads it.
            kept.push(inbound);
            continue;
        };
        let files = crate::images::message_files(&parse_inbound_images(&inbound.payload));
        let step = message_step(&inbound);
        let decision = decide_message(hooks, ctx, &step, &text, files).await?;
        if let Some(refusal) = decision.refusal() {
            state.note_read(&inbound.id);
            let reason = refusal.reason.as_deref().unwrap_or("no reason was given");
            let plugin = refusal.plugin.as_deref().unwrap_or("a plugin");
            ctx.emit(RunEvent::Progress {
                message: format!(
                    "the message was refused by {plugin}: {reason}; the task goes on without it"
                ),
            })
            .await;
            continue;
        }
        if let Some(rewritten) = decision.text {
            rewrite_payload(&mut inbound.payload, &rewritten);
        }
        kept.push(inbound);
    }
    Ok(kept)
}

/// Put a rewritten text in a message's payload, keeping everything else it carries (its images, its
/// context, a session header).
fn rewrite_payload(payload: &mut Value, text: &str) {
    match payload {
        Value::String(_) => *payload = Value::String(text.to_owned()),
        Value::Object(object) => {
            object.insert("text".into(), Value::String(text.to_owned()));
        }
        other => *other = json!({ "text": text }),
    }
}

/// The failure a refused message that started the task leaves: `Rejected`, with the reason a person
/// reads (ADR 0050, decision 9).
fn refused_message(decision: &HookDecision) -> AgentError {
    let reason = decision.reason.as_deref().unwrap_or("no reason was given");
    match decision.plugin.as_deref() {
        Some(plugin) => {
            AgentError::rejected(format!("the message was refused by {plugin}: {reason}"))
        }
        None => AgentError::rejected(format!("the message was refused: {reason}")),
    }
}

/// The text of the last user message of the state, when there is one.
fn last_user_text(state: &Conversation) -> Option<String> {
    state
        .messages
        .iter()
        .rev()
        .find(|message| matches!(message, Message::User { .. }))
        .map(Message::text)
}

/// Replace the text of the last user message, keeping its images: what a rewritten start message
/// becomes in the history.
fn rewrite_last_user(state: &mut Conversation, text: &str) {
    if let Some(message) = state
        .messages
        .iter_mut()
        .rev()
        .find(|message| matches!(message, Message::User { .. }))
    {
        match message {
            Message::User { content } => {
                let images: Vec<adam_model::ContentPart> = content
                    .iter()
                    .filter(|part| !matches!(part, adam_model::ContentPart::Text { .. }))
                    .cloned()
                    .collect();
                let mut parts = parts_of(text, &[]);
                parts.extend(images);
                *content = parts;
            }
            other => *other = Message::user_text(text),
        }
    }
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
        let mut conversation = Conversation {
            messages: last.into_iter().collect(),
            continued_from: Some(prior_run),
            context: prior.context.clone(),
            plan: prior.plan.clone(),
            session: parse_session(&input.payload).or_else(|| prior.session.clone()),
            ..Conversation::default()
        };
        // The carried answer was signed under the run it came from; this run signs it again.
        crate::signing::unsign_from(&mut conversation.messages, 0);
        return Ok(conversation);
    }
    let text = start_text(input)?;
    let mut conversation = prior.continued(text, prior_run);
    // A carried row was signed under the run it came from, at its place in that run's chain. The
    // runtime verified it there before handing it over (`prior_state`); this run signs every row
    // again at its first commit, under its own id (ADR 0068 decision 5).
    crate::signing::unsign_from(&mut conversation.messages, 0);
    conversation.pending_images = parse_inbound_images(&input.payload);
    // A round of a session works for the run that started it, as the first one did, and says what
    // the session is.
    conversation.root_run = parse_root_run(&input.payload);
    conversation.model_run = parse_model_run(&input.payload);
    conversation.root_step = parse_root_step(&input.payload);
    // A later task brings its own; a round of a session, the run that sent it.
    conversation.prompt_statement = parse_prompt_statement(&input.payload);
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

    fn split(&self, state: &Conversation) -> Option<(Conversation, Vec<Value>)> {
        Some(state.split())
    }

    fn join(&self, head: Conversation, rows: Vec<Value>) -> Result<Conversation, AgentError> {
        Conversation::join(head, rows)
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

/// What a call with the arguments in `call` shares with the other calls of its turn, from the agent's
/// own tool and its sources: [`LlmAgent::access_of`] is this, for a call the loop is scheduling.
///
/// The loop asks it twice for a call a hook rewrote — the schedule was made from the model's
/// arguments, and the rewrite runs only if it changes nothing (ADR 0049, ADR 0050 decision 7).
fn access_of(own: Option<&DynTool>, sources: &[DynToolSource], call: &ToolCall) -> ToolAccess {
    if call.invalid_arguments.is_some() {
        return ToolAccess::Independent;
    }
    match own {
        Some(tool) => {
            if tool.asks_user() || is_session_tool(&call.name) {
                ToolAccess::Alone
            } else {
                tool.access(&call.arguments)
            }
        }
        None if sources.is_empty() => ToolAccess::Independent,
        None => sources
            .iter()
            .map(|source| source.access(&call.name, &call.arguments))
            .find(|access| !access.is_alone())
            .unwrap_or(ToolAccess::Alone),
    }
}

/// The hooks' `before_tool` for one call (ADR 0050, slice H1): they may rewrite the arguments in
/// `args`, and the first verdict other than `Continue` decides. `Some(output)` is the refusal the
/// call ends with — its result, and what the step records; `None` means the call runs.
///
/// A hook that fails never lets the call through: a retryable failure is the step's own
/// ([`ToolError::Transient`], so the transition is tried again and the call runs afresh), and any
/// other failure denies the call. A rewrite that would change what the call touches
/// ([`ToolAccess`]) is refused the same way, because the schedule of the turn was made from the
/// model's arguments (ADR 0049).
///
/// An `Ask` (slice H8) is recorded with its question in [`HookedOutput::approval`] and the arguments
/// the hooks left, and the call is not run in this step: the run parks for the person's approval,
/// and on yes the call runs in its own step. When `approvable` is `false` (a child run that may not
/// ask the person, or a firing of a schedule) the ask is recorded as a `deny` and answered with
/// [`NOT_APPROVABLE`]: nobody would answer it.
fn decide_before(
    hooks: &HookSet,
    call: &ToolCall,
    args: &mut Value,
    (own, sources): (Option<&DynTool>, &[DynToolSource]),
    approvable: bool,
    recorded: &mut Vec<HookDecision>,
) -> Result<Option<HookedOutput>, ToolError> {
    if hooks.is_empty() {
        return Ok(None);
    }
    let tool = call.name.as_str();
    let verdict = hooks.before_tool(&mut HookedCall {
        tool,
        arguments: &mut *args,
    });
    // The schedule of the turn was made from the model's arguments (ADR 0049): a rewrite that would
    // change what the call touches is refused, because two calls the loop started together were
    // ordered from the arguments it has. A call a person approves runs with the arguments the hooks
    // left, so the rule holds for an ask too.
    let changes_access = |args: &Value| {
        if *args == call.arguments {
            return false;
        }
        let mut rewritten = call.clone();
        rewritten.arguments = args.clone();
        access_of(own, sources, &rewritten) != access_of(own, sources, call)
    };
    let changed_access = || {
        format!(
            "a hook rewrote the arguments of `{tool}` so that the call would touch what the \
             schedule of the turn was not made from: the call is not run"
        )
    };
    let mut refuse = |verdict: &str, reason: String, plugin: Option<&str>| {
        let decision = match plugin {
            Some(plugin) => {
                HookDecision::new(HookDecision::TOOL_BEFORE, verdict, Some(&reason)).by(plugin)
            }
            None => HookDecision::new(HookDecision::TOOL_BEFORE, verdict, Some(&reason)),
        };
        recorded.push(decision);
        HookedOutput {
            output: ToolOutput::error(reason).sanitized(),
            hooks: recorded.clone(),
            args: None,
            events: Vec::new(),
            approval: None,
        }
    };
    match verdict {
        Ok(Verdict::Continue) => {
            if !changes_access(args) {
                return Ok(None);
            }
            Ok(Some(refuse(HookDecision::FAILED, changed_access(), None)))
        }
        Ok(Verdict::Deny(reason)) => Ok(Some(refuse(HookDecision::DENY, reason, None))),
        Ok(Verdict::Ask(question)) => {
            if changes_access(args) {
                return Ok(Some(refuse(HookDecision::FAILED, changed_access(), None)));
            }
            // Nobody can answer this run (ADR 0045 decision 9): the ask is read as a denial.
            if !approvable {
                return Ok(Some(refuse(
                    HookDecision::DENY,
                    format!("{NOT_APPROVABLE}. The question was: {question}"),
                    None,
                )));
            }
            // The run parks for the person's approval (slice H8). The output is what a build that
            // predates the slice reads: an error, so the call stays unrun there too.
            recorded.push(HookDecision::new(
                HookDecision::TOOL_BEFORE,
                HookDecision::ASK,
                Some(&question),
            ));
            Ok(Some(HookedOutput {
                output: ToolOutput::error(format!("awaiting approval: {question}")).sanitized(),
                hooks: recorded.clone(),
                args: (*args != call.arguments).then(|| args.clone()),
                events: Vec::new(),
                approval: Some(question),
            }))
        }
        Ok(Verdict::Stop(reason)) => Ok(Some(refuse(HookDecision::STOP, reason, None))),
        Err(error) if error.is_retryable() => Err(ToolError::Transient(error.to_string())),
        Err(error) => Ok(Some(refuse(
            HookDecision::FAILED,
            format!("the hook `{}` failed: {}", error.plugin, error.reason),
            Some(&error.plugin),
        ))),
    }
}

/// The hooks' `after_tool` for one call (ADR 0050, slice H1): they may rewrite the text the model
/// will read, and the first verdict other than `Continue` decides. The result is the output the step
/// records.
///
/// At this point ask and refuse are not verdicts the point allows (the call already ran): both are
/// recorded as a failure and the result becomes an error, so nothing a hook was meant to hold back
/// reaches the model. A hook that fails does the same, except that a retryable failure is the step's
/// own, and the call runs again (at-least-once, as for any step that is not yet recorded).
fn decide_after(
    hooks: &HookSet,
    tool: &str,
    args: &Value,
    answered: Result<ToolOutput, ToolError>,
    recorded: &mut Vec<HookDecision>,
) -> Result<ToolOutput, ToolError> {
    let mut output = answered?;
    if hooks.is_empty() {
        return Ok(output);
    }
    let mut text = std::mem::take(&mut output.content);
    // The arguments the call ran with, as a hook reads them. A rewrite here changes nothing: the call
    // already ran, and only the text it answered is still open to a plugin.
    let mut ran = args.clone();
    let verdict = hooks.after_tool(
        &HookedCall {
            tool,
            arguments: &mut ran,
        },
        &mut HookedText { text: &mut text },
    );
    // What a refusal leaves: the reason as an error result, and the time the call already ran still
    // counted, nothing else kept.
    let mut refuse = |output: ToolOutput, verdict: &str, reason: String, plugin: Option<&str>| {
        let decision = match plugin {
            Some(plugin) => {
                HookDecision::new(HookDecision::TOOL_AFTER, verdict, Some(&reason)).by(plugin)
            }
            None => HookDecision::new(HookDecision::TOOL_AFTER, verdict, Some(&reason)),
        };
        recorded.push(decision);
        let mut refused = ToolOutput::error(reason);
        refused.sandbox = output.sandbox;
        refused.sanitized()
    };
    match verdict {
        Ok(Verdict::Continue) => {
            output.content = text;
            Ok(output.sanitized())
        }
        Ok(Verdict::Deny(reason)) => Ok(refuse(output, HookDecision::DENY, reason, None)),
        Ok(Verdict::Ask(question)) => Ok(refuse(
            output,
            HookDecision::FAILED,
            format!("a hook may not ask after the call ran: {question}"),
            None,
        )),
        Ok(Verdict::Stop(reason)) => Ok(refuse(output, HookDecision::STOP, reason, None)),
        Err(error) if error.is_retryable() => Err(ToolError::Transient(error.to_string())),
        Err(error) => Ok(refuse(
            output,
            HookDecision::FAILED,
            format!("the hook `{}` failed: {}", error.plugin, error.reason),
            Some(&error.plugin),
        )),
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
            loaded: Vec::new(),
            kept: None,
            reported: Vec::new(),
            pinned: false,
            kept_images: Vec::new(),
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "a turn's message is the value; boxing it would allocate for every tool answer"
)]
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
        /// The groups and tools the run has loaded, from a result that is not an error: the whole
        /// list.
        loaded: Vec<Loaded>,
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
    /// A hook asked a person to approve the call (ADR 0050, slice H8): the run parks on the
    /// question, and on yes runs the call with `arguments`.
    NeedsApproval {
        question: String,
        arguments: Value,
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
/// A duration in words, to the minute past an hour: `24 h`, `1 h 30 min`, `5 min`, `45 s`.
fn words_of(d: Duration) -> String {
    let secs = d.as_secs();
    match (secs / 3600, (secs % 3600) / 60) {
        (0, 0) => format!("{secs} s"),
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

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
pub(crate) fn call_key(call: &ToolCall) -> String {
    match &call.invalid_arguments {
        Some(raw) => fnv_hex(&[&call.name, "raw", raw]),
        None => {
            let mut text = String::new();
            canonical(&call.arguments, &mut text);
            fnv_hex(&[&call.name, "json", &text])
        }
    }
}

/// The transition a transient tool failure fails: the step's record is abandoned with the try, and
/// the retry runs the turn's calls again (ADR 0049 decision 9). The end of the call was said as it
/// happened, by [`LlmAgent::say_outcome`].
fn transient_failure(
    call: &ToolCall,
    outcome: &Result<HookedOutput, ToolError>,
    policy: Option<RetryPolicy>,
) -> Option<AgentError> {
    let (reason, class) = outcome.as_ref().err()?.retry_parts()?;
    Some(tool_transient(
        format!("tool `{}` failed: {reason}", call.name),
        class,
        policy.map(|p| (call.name.as_str(), p)),
    ))
}

/// The key a tool's failures count under: `tool:<name>`, the name and never the call id, so a tool
/// called again in a later turn counts as the same step (ADR 0061, decision 3).
pub fn tool_retry_key(tool: &str) -> String {
    format!("tool:{tool}")
}

/// An `AgentError::Transient` of `class` with `message`, under `policy` (the tool's name and its
/// policy) when there is one: a tool's retryable failure, or the model's (with no policy).
fn tool_transient(
    message: String,
    class: adam_error::ErrorClass,
    policy: Option<(&str, RetryPolicy)>,
) -> AgentError {
    let error = AgentError::Transient {
        message,
        retry_after: None,
        class,
        retry: None,
        source: None,
    };
    match policy {
        Some((tool, policy)) => error.with_retry(tool_retry_key(tool), policy),
        None => error,
    }
}

/// Checks what a `model.request` hook left in a request and, if it is allowed, writes it back:
/// no header that looks like a credential, no body member the client owns, a finite top-p.
fn apply_hooked(
    request: &mut ModelRequest,
    temperature: Option<f32>,
    top_p: Option<f32>,
    headers: std::collections::BTreeMap<String, String>,
    extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    max_output_tokens: Option<u32>,
    metadata: std::collections::BTreeMap<String, String>,
) -> Result<(), ModelFailure> {
    if let Some(name) = headers
        .keys()
        .find(|name| adam_model::is_credential_header(name))
    {
        return Err(ModelFailure::by_hook(
            format!(
                "a hook set the header `{name}`, which looks like a credential: the model's key is the deployment's"
            ),
            false,
        ));
    }
    if let Some(key) = adam_model::RESERVED_BODY_KEYS
        .iter()
        .find(|key| extra_body.as_ref().is_some_and(|b| b.contains_key(**key)))
    {
        return Err(ModelFailure::by_hook(
            format!("a hook set the body member `{key}`, which the client owns"),
            false,
        ));
    }
    if top_p.is_some_and(|p| !p.is_finite()) {
        return Err(ModelFailure::by_hook(
            "a hook set a top_p that is not finite".to_owned(),
            false,
        ));
    }
    request.top_p = top_p;
    request.headers = headers;
    request.extra_body = extra_body;
    request.temperature = temperature;
    request.max_output_tokens = max_output_tokens;
    request.metadata = metadata;
    Ok(())
}

/// Whether `doom_loop` would refuse a call of `key` now, from the committed state alone: the third
/// call in a row with the same tool, the same arguments and the same result does not run.
fn doom_loop_would_refuse(repeats: Option<&Repeats>, key: &str) -> bool {
    repeats.is_some_and(|repeats| {
        repeats.key == key
            && (repeats.count >= DOOM_LOOP_AFTER
                || repeats.failing >= DOOM_LOOP_AFTER
                || repeats.marked >= DOOM_LOOP_AFTER)
    })
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

impl LlmAgent {
    /// `transition` with every assistant and tool message of its state signed
    /// (`signing::sign_messages`); unchanged when the agent has no [`turn_keys`](LlmAgentBuilder::turn_keys).
    async fn sign_transition(
        &self,
        ctx: &Ctx,
        mut transition: Transition<Conversation>,
    ) -> Result<Transition<Conversation>, AgentError> {
        let Some(keys) = &self.turn_keys else {
            return Ok(transition);
        };
        let (Transition::Continue(state)
        | Transition::Park { state, .. }
        | Transition::Done { state, .. }
        | Transition::Fail { state, .. }) = &mut transition;
        crate::signing::sign_messages(
            keys,
            ctx.run_id(),
            ctx.conversation_id(),
            &mut state.messages,
        )
        .await
        .map_err(|e| AgentError::from_classified("signing the turns of a step failed", e))?;
        Ok(transition)
    }

    /// The transition of one step, before its turns are signed ([`LlmAgent::sign_transition`]).
    async fn step_unsigned(
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
            self.forget_chars(ctx.run_id());
            return Ok(Transition::Done { state, output });
        }
        // A person's message is decided before anything sends it to a model (ADR 0050, slice H1b):
        // here for the message the run starts with, and below for the ones that arrived, in steps
        // of their own. The steps are taken only when a hook set is installed, so a run whose
        // journal was written without hooks replays unchanged.
        if !self.hooks.is_empty() && !state.start_decided {
            if let Some(text) = last_user_text(&state) {
                let files = crate::images::message_files(&state.pending_images);
                let decision =
                    decide_message(&self.hooks, ctx, START_MESSAGE_STEP, &text, files).await?;
                if let Some(refusal) = decision.refusal() {
                    let error = refused_message(refusal);
                    self.forget_chars(ctx.run_id());
                    return Err(error);
                }
                if let Some(rewritten) = decision.text {
                    rewrite_last_user(&mut state, &rewritten);
                }
            }
            state.start_decided = true;
        }
        // The inbox is taken once, here, before the summary: a resume that wakes a run parked on its
        // summary is read (and its id recorded) before `compact_if_due` asks the summary again, so
        // `absorb_while_parked` cannot consume it unrecorded and a replayed forward cannot wake the
        // run a second time.
        let inbox = ctx.take_inbox();
        // A resume (ADR 0067) says nothing to the run itself: it wakes a run parked on the model,
        // which then makes its call again, and a run that waits on a child parked on the model
        // forwards one to it (decision 5). It adds nothing to the conversation and answers no
        // question, so it never reaches the inbox below.
        //
        // The id is derived from the waking message and the child, so a step that runs again (a
        // lost commit) writes the same one: a resume whose id this run has read is a **copy**, and
        // a copy is no wake. The ones kept are noted in the commit of this step, which consumes
        // them; the copy of a later delivery is then recognised.
        let (resumes, inbox): (Vec<_>, Vec<_>) =
            inbox.into_iter().partition(|i| i.kind == MODEL_RESUME_KIND);
        let mut kept = Vec::with_capacity(resumes.len());
        let mut copies = 0_usize;
        for resume in resumes {
            if state.has_read(&resume.id) {
                copies += 1;
            } else {
                state.note_read(&resume.id);
                kept.push(resume);
            }
        }
        let resumes = kept;
        // Woken by nothing but copies of resumes this run has read: the step that sent them ran
        // again after a lost commit. The run waits on the model as it did, with no call — a copy is
        // not a wake, and it does not ask a summary again either. A timer that has come due is not
        // "nothing" (the runtime woke it for the timer), and anything else in the inbox (a message,
        // a notice, a close) is read below.
        let nothing_else = inbox
            .iter()
            .all(|i| i.kind == CHILD_PARKED_KIND || i.kind == CHILD_REPORTED_KIND);
        if copies > 0
            && resumes.is_empty()
            && nothing_else
            && let Some(wait) = &state.model_wait
            && !wait.wake_at.is_some_and(|at| at <= ctx.now())
        {
            let wake_at = wait.wake_at;
            return Ok(Transition::Park { state, wake_at });
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
        // A journal written before `loaded` existed is seeded and made whole **before** a
        // compaction can read the recap, so the first marker names the group; any turn whose loaded
        // set is empty seeds, not only the first, so a run resumed at turn 4 restores its tools.
        // The first request of the resumed run carries them: it is not a growth.
        if state.loaded.is_empty() && !state.loaded_servers.is_empty() {
            state.seed_loaded();
            let source_ctx = SourceCtx::new(
                ctx.run_id(),
                state.root_run.unwrap_or_else(|| ctx.run_id()),
                ctx.conversation_id().map(str::to_owned),
                Arc::new(state.context.clone()),
            )
            .with_task(state.task())
            .with_workspace_run(
                workspace_run_of(&state)
                    .unwrap_or_else(|| state.root_run.unwrap_or_else(|| ctx.run_id())),
            );
            let loaders = crate::source::loaders_offered(&self.sources, &source_ctx).await;
            let task_loaded = crate::source::loaded_by_task(&self.sources, &source_ctx).await;
            state.fill_seeded(&loaders, &task_loaded);
            // The task may load a group the old journal never named (a different `when` server):
            // journal it here too, or the recap would omit it.
            state.note_task_loaded(task_loaded);
            let carried: Vec<String> = state
                .loaded
                .iter()
                .flat_map(|one| one.tools.iter().cloned())
                .collect();
            state.note_carried(carried);
        }
        // A history that has reached the budget is summarised first, at a journaled step of its own;
        // the model is asked in the next transition, from the new history.
        match self.compact_if_due(ctx, &mut state, &output_id_of).await? {
            Compacted::Made => {
                // What arrived while the summary was asked is read now, after it: the step took
                // the inbox before the summary, so its words are kept for the next transition.
                self.absorb_while_parked(ctx, &mut state, inbox).await?;
                return Ok(Transition::Continue(state));
            }
            Compacted::Parked(flow) => {
                // What arrived is read now, its words kept for after the summary: a parked run with
                // messages left in its inbox would be stepped again by every look of the runtime
                // (a child's or a run with a deadline), making the summary call each time.
                if !matches!(flow, Flow::Fail(_)) {
                    self.absorb_while_parked(ctx, &mut state, inbox).await?;
                }
                return Ok(match flow {
                    // A switch to the fallback: the summary is asked of it in the next transition.
                    Flow::Next => Transition::Continue(state),
                    Flow::Sleep(wake_at) => Transition::Park {
                        state,
                        wake_at: Some(wake_at),
                    },
                    Flow::Fail(error) => {
                        self.forget_chars(ctx.run_id());
                        Transition::Fail { state, error }
                    }
                    _ => Transition::Park {
                        state,
                        wake_at: None,
                    },
                });
            }
            Compacted::No => {}
        }
        let mut counted = {
            let mut totals = self
                .run_chars
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            totals.begin(
                run,
                state.turns,
                state.shortenings.len(),
                state.compactions.len(),
            )
        };
        if let Some(first) = record_shortenings_from(
            &mut state.messages,
            self.limits.max_history_tokens,
            &output_id_of,
            &mut counted,
        ) {
            // A shortened row's bytes changed, and every later row's `prv` with it: their turn
            // signatures no longer hold, and the commit signs them again (ADR 0068).
            crate::signing::unsign_from(&mut state.messages, first);
            state.shortenings.push(state.turns);
            state.last_prompt = None;
            // The images of a result that was shortened left the history with it.
            state.drop_unnamed_images();
        }
        {
            let mut totals = self
                .run_chars
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            totals.end(
                run,
                counted,
                state.shortenings.len(),
                state.compactions.len(),
            );
        }
        let (finished, inbox): (Vec<_>, Vec<_>) = inbox
            .into_iter()
            // A session round's report (ADR 0045) is a wake-up, not a person's words: it is
            // dropped, never shown to the model, and the report is read from the store.
            .filter(|i| i.kind != CHILD_REPORTED_KIND)
            // A child's park (ADR 0067 decision 5) is a wake-up too: what the child waits on is
            // read from its record, below.
            .filter(|i| i.kind != CHILD_PARKED_KIND)
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
        // A message that arrived is decided before its images are admitted (ADR 0050, slice H1b):
        // a refused one is left out, its files are not kept, and the task goes on. Each decision is
        // a journaled step of its own; a replay reads it and asks no hook again.
        let inbox = decide_inbox(&self.hooks, ctx, &mut state, inbox).await?;
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
        // The person answered an approval (ADR 0050, slice H8): observers are told, keyed by the
        // message that answered, so a replay of this transition tells them again with the same key.
        for answer in &absorbed.approvals {
            self.observe_approval(
                ctx,
                state.root_run,
                adam_plugin::EventKind::ApprovalAnswered,
                &answer.inbound,
                (&answer.tool, &answer.call_id),
                |event, _| event.plain("approved", answer.approved),
            );
        }
        // The person answered: the step of the call that asked, which has been `waiting` since, ends.
        // An approved call's step ends when it has run; a refused one ends failed, with the refusal.
        if let Some((tool, call_id)) = asked
            && state.pending_wait.is_none()
        {
            match absorbed.approvals.iter().find(|a| a.call_id == call_id) {
                Some(answer) if answer.approved => {}
                Some(_) => {
                    ctx.emit(self.step_end_with_result(&state, &tool, &call_id, StepState::Failed))
                        .await;
                }
                None => {
                    ctx.emit(self.step_end_with_result(
                        &state,
                        &tool,
                        &call_id,
                        StepState::Completed,
                    ))
                    .await;
                }
            }
        }
        // A credential in the context does not outlive its expiry in the store.
        let expired = state.drop_expired_context(ctx.now());
        if expired > 0 {
            tracing::debug!(expired, "expired entries left the run's context");
        }
        // One shared copy of the run's context for this step: when there is a context, every tool
        // call and every source sees the same map, and no call clones it. Built after this step's messages merged their
        // context and the expired entries left, so it is the map the tools of this step read, and
        // only when there is something in it: a step with no context (a wake that only looks at a
        // wait) does not copy one.
        let context: Option<Arc<Map<String, Value>>> =
            (!state.context.is_empty()).then(|| Arc::new(state.context.clone()));

        // A wait on a child ends with the child's message or, when woken without one (the timer,
        // a message, a resume, or the child's park), with what the store says. A message that
        // matches no wait (a duplicate of one already used, or one for a call the run is not on)
        // is dropped here: the wait it was for has been settled, or will be, from the store.
        if let Some(PendingWait::Run(wait)) = state.pending_wait.clone() {
            // One read of the child, or the notice of its end the inbox already holds.
            let look = match notices.iter().find(|(run, _)| *run == wait.run) {
                Some((_, status)) => Some(ChildLook::finished(status.clone())),
                None => ctx.child_look(wait.run).await?,
            };
            let finished = match &look {
                None => Some(ChildStatus::vanished()),
                Some(look) if look.status.is_finished() => Some(look.status.clone()),
                Some(_) => None,
            };
            match finished {
                Some(child) => {
                    let outcome = Self::answer_run(&mut state, &wait, &child);
                    self.observe_child(
                        ctx,
                        state.root_run,
                        adam_plugin::EventKind::ChildEnded,
                        (&wait.tool, &wait.call_id),
                        (wait.run, Some(outcome)),
                    );
                    Self::count_child(ctx, &mut state, wait.run).await?;
                    ctx.emit(self.step_end_with_result(&state, &wait.tool, &wait.call_id, outcome))
                        .await;
                }
                None => {
                    // The child works or waits. What it waits on is read from the same head and
                    // kept with the pending child (ADR 0067 decision 5).
                    let copy = look
                        .as_ref()
                        .and_then(|look| Self::model_wait_of(look.head.as_ref()));
                    // A message or a resume to this run goes down to a child parked on the model
                    // as one resume, under an id a replay writes again. The words of a message
                    // stay here: they are deferred above, behind the call the child answers.
                    let waking = absorbed
                        .read
                        .as_deref()
                        .or_else(|| resumes.first().map(|i| i.id.as_str()));
                    let forwarded = match (&copy, waking) {
                        (Some(_), Some(waking)) => {
                            ctx.resume_child(wait.run, forwarded_resume_id(waking, wait.run))
                                .await?;
                            true
                        }
                        _ => false,
                    };
                    // A child parked until it is woken cannot finish on its own, so this run
                    // waits with no timer, as it waits on a question: its task says why. A child
                    // on a timer keeps the 60 s look, which is what a lost notice falls back on.
                    let until_woken = !forwarded
                        && copy
                            .as_ref()
                            .is_some_and(|wait| wait.park == ModelPark::UntilWoken);
                    if let Some(PendingWait::Run(stored)) = &mut state.pending_wait {
                        stored.model_wait = if forwarded { None } else { copy };
                    }
                    let wake_at = (!until_woken).then(|| self.next_poll(ctx));
                    return Ok(Transition::Park { state, wake_at });
                }
            }
        }

        // A wait on a remote task ends when the tool says the task is over. Woken by the timer
        // (or by a user message, which queues behind the result), the run looks once and either
        // answers the call or parks again.
        if let Some(PendingWait::Remote(wait)) = state.pending_wait.clone() {
            match self
                .poll_remote(ctx, &mut state, &wait, context.as_ref())
                .await?
            {
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
            match self
                .poll_remote(ctx, &mut state, &looking, context.as_ref())
                .await?
            {
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
        let mut early: Option<Early<'_>> = None;
        if state.pending_calls.is_empty() {
            state.messages.append(&mut state.deferred);
            let (next, batch) = self.model_turn(ctx, &mut state, context.as_ref()).await?;
            flow = next;
            early = batch;
            // A call that starts, sends to or closes a session has an effect no failed try can undo:
            // the turn and its calls are committed first, so a retry never asks the model again
            // (new call ids, so a second session) and the call runs under the id the history holds.
            if matches!(flow, Flow::Next) && state.pending_calls.iter().any(|c| has_effect(&c.name))
            {
                // The calls that started while the model wrote are not this transition's to keep.
                if let Some(mut batch) = early.take() {
                    batch.settle().await;
                }
                return Ok(Transition::Continue(state));
            }
        }
        if matches!(flow, Flow::Next) {
            flow = self
                .run_pending(ctx, &mut state, &notices, context.as_ref(), &mut early)
                .await?;
        }
        // Whatever the turn did not use (a limit answered its calls, a barrier came first, a stream
        // error, a cancel): the calls that started end within their limits, and their results are
        // dropped. On a cancel the run's token has fired, so the wait is TOOL_STOP_GRACE at most.
        if let Some(mut left) = early {
            left.settle().await;
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
                        if self.run_ending(ctx, &mut state, &text).await? {
                            return Ok(Transition::Continue(state));
                        }
                        let mut output = json!({ "text": text, "artifacts": state.artifacts });
                        self.end_sessions(ctx, &mut state, &mut output).await?;
                        self.forget_chars(ctx.run_id());
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
            Flow::Sleep(wake_at) => Transition::Park {
                wake_at: Some(wake_at),
                state,
            },
            Flow::Done(mut output) => {
                self.end_sessions(ctx, &mut state, &mut output).await?;
                self.forget_chars(ctx.run_id());
                Transition::Done { state, output }
            }
            Flow::Fail(error) => {
                self.forget_chars(ctx.run_id());
                Transition::Fail { state, error }
            }
        })
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

    fn split(&self, state: &Conversation) -> Option<(Conversation, Vec<Value>)> {
        Some(state.split())
    }

    fn join(&self, head: Conversation, rows: Vec<Value>) -> Result<Conversation, AgentError> {
        Conversation::join(head, rows)
    }

    #[tracing::instrument(skip_all, fields(run = %ctx.run_id(), agent = %self.name))]
    async fn step(
        &self,
        ctx: &mut Ctx,
        state: Conversation,
    ) -> Result<Transition<Conversation>, AgentError> {
        let transition = self.step_unsigned(ctx, state).await?;
        self.sign_transition(ctx, transition).await
    }
}

#[cfg(test)]
mod thinking_cut_tests {
    use super::*;
    use adam_model::MockModel;

    #[test]
    fn a_message_that_reaches_the_run_starts_the_count_again() {
        let model: DynModel = Arc::new(MockModel::new());
        let agent = LlmAgent::builder("t", model, "m").build();
        let mut state = Conversation::new("work");
        state.thinking_cuts = 2;
        let message = crate::user_message("and also this");
        agent.absorb_inbox(&mut state, vec![message], &HashMap::new());
        assert_eq!(state.thinking_cuts, 0);
    }

    #[test]
    fn an_unreadable_message_does_not() {
        let model: DynModel = Arc::new(MockModel::new());
        let agent = LlmAgent::builder("t", model, "m").build();
        let mut state = Conversation::new("work");
        state.thinking_cuts = 2;
        let junk = Inbound::new("message", json!({ "nothing": true }));
        agent.absorb_inbox(&mut state, vec![junk], &HashMap::new());
        assert_eq!(state.thinking_cuts, 2);
    }

    #[test]
    fn a_cut_turn_with_a_reasoning_block_or_a_carrier_stays() {
        let mut block = Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![],
            reasoning: None,
            provider: Some(adam_model::ProviderTurn::new("m", "e")),
        };
        let Message::Assistant {
            provider: Some(turn),
            ..
        } = &mut block
        else {
            unreachable!("a carrier")
        };
        turn.reasoning = Some(json!({"blocks": []}));
        assert!(
            !LlmAgent::cut_turn_has_nothing_to_send(&block),
            "a block-only turn must not be popped before it is echoed"
        );

        let Message::Assistant {
            provider: Some(turn),
            ..
        } = &mut block
        else {
            unreachable!("a carrier")
        };
        turn.reasoning = None;
        turn.members
            .insert("extra_content".into(), json!({"sig": 1}));
        assert!(!LlmAgent::cut_turn_has_nothing_to_send(&block));

        let empty = Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![],
            reasoning: None,
            provider: None,
        };
        assert!(LlmAgent::cut_turn_has_nothing_to_send(&empty));
        let echoed = Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![],
            reasoning: Some("r".into()),
            provider: None,
        };
        assert!(!LlmAgent::cut_turn_has_nothing_to_send(&echoed));
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

        // The journal is a boundary: the cause is flattened into the message, once. A connection
        // that could not be made is what the OpenAI client reports as `Unreachable` (ADR 0067).
        let transient = ModelFailure::from(ModelError::unreachable().with_source(Reset));
        assert!(transient.retryable);
        assert_eq!(transient.retry_after_ms, None);
        assert_eq!(transient.class.as_deref(), Some("Transient"));
        assert_eq!(
            transient.message,
            "the model endpoint could not be reached: connection reset"
        );

        let auth = ModelFailure::from(ModelError::Auth("bad key".into()));
        assert!(!auth.retryable);
        assert_eq!(auth.class.as_deref(), Some("Unauthenticated"));
    }

    #[tokio::test]
    async fn a_loaded_entry_from_a_source_that_does_not_track_is_not_removed_next_to_groups() {
        struct Tracks;
        #[async_trait]
        impl ToolSource for Tracks {
            async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
                Vec::new()
            }
            async fn loaded_tools(&self, _ctx: &SourceCtx) -> Option<Vec<String>> {
                Some(vec!["tracked".into()])
            }
            async fn call(
                &self,
                _ctx: &ToolCtx,
                _name: &str,
                _args: Value,
            ) -> Option<Result<ToolOutput, ToolError>> {
                None
            }
        }
        struct Untracked;
        #[async_trait]
        impl ToolSource for Untracked {
            async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
                Vec::new()
            }
            async fn call(
                &self,
                _ctx: &ToolCtx,
                _name: &str,
                _args: Value,
            ) -> Option<Result<ToolOutput, ToolError>> {
                None
            }
        }
        let ctx = SourceCtx::detached(Map::new()).with_loaded(vec![Loaded {
            loader: "custom".into(),
            tools: vec!["custom__x".into()],
            note: String::new(),
            note_is_server: false,
        }]);
        let sources: Vec<DynToolSource> = vec![Arc::new(Tracks), Arc::new(Untracked)];
        assert!(
            crate::source::loaded_offered(&sources, &ctx)
                .await
                .is_none(),
            "one source that does not track makes the set unknown: the loop falls back"
        );
    }

    #[test]
    fn a_run_resumed_at_a_later_turn_seeds_its_loaded_set() {
        let mut state: Conversation =
            serde_json::from_value(json!({"turns": 4, "loaded_servers": ["docs"]})).unwrap();
        assert!(state.seed_loaded());
        assert_eq!(state.loaded[0].loader, "load__group__docs");
        assert!(state.loaded[0].tools.is_empty());
    }

    #[test]
    fn the_loaders_are_recomputed_on_a_runs_first_turn() {
        let stale = Loader {
            name: "load__group__gone".into(),
            holds: Vec::new(),
            note: String::new(),
            note_is_server: false,
        };
        assert!(
            loaders_needed(0, std::slice::from_ref(&stale)),
            "a run's first turn recomputes, even over a carried list"
        );
        assert!(loaders_needed(4, &[]), "an old journal has none to keep");
        assert!(
            !loaders_needed(4, std::slice::from_ref(&stale)),
            "a run in flight keeps what its first turn recorded"
        );
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
            offered: Vec::new(),
            carried: Vec::new(),
            loaders: None,
            task_loaded: Vec::new(),
            grew: false,
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
            offered: Vec::new(),
            carried: Vec::new(),
            loaders: None,
            task_loaded: Vec::new(),
            grew: false,
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
            offered: Vec::new(),
            carried: Vec::new(),
            loaders: None,
            task_loaded: Vec::new(),
            grew: false,
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
            offered: Vec::new(),
            carried: Vec::new(),
            loaders: None,
            task_loaded: Vec::new(),
            grew: false,
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
            offered: Vec::new(),
            carried: Vec::new(),
            loaders: None,
            task_loaded: Vec::new(),
            grew: false,
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
mod admission_tests {
    use super::*;
    use adam_model::MockModel;

    /// An admission that admits everything and records each ask, so a test can see what the loop
    /// asked for and under which conversation.
    #[derive(Default)]
    struct Recorder {
        asks: Mutex<Vec<(Pool, String, u32)>>,
    }

    impl Recorder {
        fn asks(&self) -> Vec<(Pool, String, u32)> {
            self.asks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl Admission for Recorder {
        async fn admit(&self, ask: Ask) -> Result<Permit, Refused> {
            let conversation = ask.conversation.as_str().to_owned();
            self.asks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((ask.pool, conversation, ask.units));
            Ok(Permit::new(ask.pool, ask.conversation, ask.units, || {}))
        }

        fn snapshot(&self) -> adam_budget::Usage {
            adam_budget::Usage::default()
        }
    }

    fn agent(admission: Option<DynAdmission>) -> LlmAgent {
        let model: DynModel = Arc::new(MockModel::new());
        match admission {
            Some(admission) => LlmAgent::builder("t", model, "m")
                .admission(admission)
                .build(),
            None => LlmAgent::builder("t", model, "m").build(),
        }
    }

    /// A tool call asks the process's tool pool for one seat, under the run's conversation.
    #[tokio::test]
    async fn a_tool_call_takes_a_seat_of_the_tool_pool() {
        let recorder = Arc::new(Recorder::default());
        let admission: DynAdmission = Arc::clone(&recorder) as DynAdmission;
        let agent = agent(Some(admission));
        let seat = agent.seat_tool(ConversationId::new("talk")).await;
        assert!(seat.is_some(), "a pool with room admits the call");
        drop(seat);
        assert_eq!(
            recorder.asks(),
            [(Pool::ToolCalls, "talk".to_owned(), 1)].to_vec()
        );
    }

    /// An agent with no admission asks nobody: the host that sets no ceiling is the ceiling.
    #[tokio::test]
    async fn a_call_without_an_admission_takes_no_seat() {
        let agent = agent(None);
        assert!(
            agent.seat_tool(ConversationId::new("talk")).await.is_none(),
            "nothing is asked"
        );
    }

    /// Every model call asks the model pool first, under the root run the request names, so every
    /// turn and every child call of one conversation counts together.
    #[tokio::test]
    async fn a_model_call_takes_a_seat_of_the_model_pool() {
        let recorder = Arc::new(Recorder::default());
        let admission: DynAdmission = Arc::clone(&recorder) as DynAdmission;
        let model: DynModel = Arc::new(MockModel::new());
        let gate = crate::gate::admitted(model, Some(&admission), "t");
        let mut req = ModelRequest::new("m");
        req.run = Some("root-run".to_owned());
        // The mock's answer is the mock's business; the seat is what this proves.
        let _ = gate.complete(req).await;
        let asks = recorder.asks();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert_eq!(asks[0].0, Pool::ModelCalls);
        assert_eq!(asks[0].1, "root-run");
        assert_eq!(asks[0].2, 1);
        // A request that names no run is counted under the agent's name.
        let _ = gate.complete(ModelRequest::new("m")).await;
        assert_eq!(recorder.asks()[1].1, "t");
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

    /// An agent with one tool and a sentence of instructions, at a limit of `limit` calls.
    fn built(limit: usize) -> LlmAgent {
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
    }

    /// The budget of a request counts what the loop adds above a limit of one: the property in each
    /// schema and the sentence of the instructions.
    #[test]
    fn the_fixed_part_of_a_request_counts_the_injected_order() {
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
                extra_content: None,
                content: vec![],
                tool_calls: vec![adam_model::ToolCall {
                    id: format!("c{n}"),
                    name: "t".into(),
                    arguments: json!({}),
                    ..Default::default()
                }],
                reasoning: None,
                provider: None,
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

#[cfg(test)]
mod session_instructions_tests {
    use super::*;

    /// A session's `instructions` follow the folder's, under a heading that says where they come
    /// from and that the text above wins (ADR 0054, decision 4). Nothing is added without them.
    #[test]
    fn a_sessions_instructions_follow_the_folders_under_a_heading() {
        let config = adam_runtime::SessionConfig::from_value(serde_json::json!({
            "instructions": "Answer in one paragraph."
        }))
        .expect("a config");
        let out = with_session_instructions(Some("You are the coder.".into()), Some(&config))
            .expect("a system");
        assert!(out.starts_with("You are the coder."), "{out}");
        assert!(
            out.contains("Instructions from the parent agent or the person"),
            "{out}"
        );
        assert!(out.ends_with("Answer in one paragraph."), "{out}");

        // No session, and a session that asks for nothing: the folder's text alone, no heading.
        assert_eq!(
            with_session_instructions(Some("You are the coder.".into()), None),
            Some("You are the coder.".into())
        );
        let none =
            adam_runtime::SessionConfig::from_value(serde_json::json!({})).expect("a config");
        assert_eq!(
            with_session_instructions(Some("You are the coder.".into()), Some(&none)),
            Some("You are the coder.".into())
        );
    }
}
