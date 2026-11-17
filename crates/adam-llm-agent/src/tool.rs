//! The [`Tool`] trait and the values that flow through it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_core::RunId;
use adam_error::{Classify, ErrorClass};
use adam_model::{ImageRef, ToolSpec};
use adam_runtime::{
    Artifact, CancelToken, ChildStarter, DynEventSink, Emitter, Inbound, MAX_RUN_FILE_BYTES,
    RunEvent, SandboxUsage, StepEvent, StepIcon, StepKind, StepState,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::access::ToolAccess;
use crate::conversation::{PROTECTED_CONTEXT_KEYS, ROOT_RUN_KEY, ROOT_STEP_KEY, user_message};
use crate::images::{ImageGate, ImageOutcome, ImagePolicy, ImageStore, KeptImage};
use crate::kept::KeptOutput;
use crate::plan::Plan;
use crate::source::ToolNote;
use crate::state::{Extensions, State, StateKey};

/// The longest a call of a tool that declares no [`Tool::time_limit`] may run: 300 s.
pub const DEFAULT_TOOL_TIME_LIMIT: Duration = Duration::from_secs(300);

/// How long a call that is past its time limit has to stop by itself, once its
/// [`ToolCtx::cancelled`] token has fired, before its future is dropped.
pub const TOOL_STOP_GRACE: Duration = Duration::from_secs(5);

/// A capability the model may call.
///
/// The agent runs every call inside a journaled step: once a call has
/// returned, its result is recorded and never recomputed, even across a crash
/// or a change of worker. A call that dies *before* it is recorded (a crash
/// mid-call, or [`ToolError::Transient`]) runs again, and so does every call
/// of the same model turn that started in the failed attempt: the calls of a
/// turn run at the same time, up to the agent's limit
/// ([`LlmAgentBuilder::max_parallel`](crate::LlmAgentBuilder::max_parallel)), except where
/// [`access`](Self::access) says they may not. A tool with side effects must
/// therefore be safe to retry: idempotent, or guarded by a key derived from
/// [`ToolCtx::call_id`].
#[async_trait]
pub trait Tool: Send + Sync + 'static {
    /// Name, description and JSON Schema of the arguments, as shown to the
    /// model. The name must be unique among the agent's tools and stable.
    fn spec(&self) -> ToolSpec;

    /// Run the tool. `args` is the model's argument object (already parsed as
    /// JSON, not validated against the schema: validate it and report bad
    /// input as [`ToolOutput::error`] so the model can correct itself).
    ///
    /// Runs inside a journaled step; must be safe to retry if it fails before
    /// being recorded.
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError>;

    /// The shared state this tool reads with [`ToolCtx::state`], by type.
    ///
    /// [`LlmAgentBuilder::try_build`](crate::LlmAgentBuilder::try_build)
    /// checks that the agent was given each one (with
    /// [`LlmAgentBuilder::state`](crate::LlmAgentBuilder::state)), so a
    /// missing dependency fails at startup and not in the middle of a run.
    /// The default is none, which is what a tool written before this method
    /// existed declares.
    fn required_state(&self) -> Vec<StateKey> {
        Vec::new()
    }

    /// Whether this tool can end a call with [`ToolError::NeedsInput`]: it asks the person who
    /// started the run a question, and the run parks until someone answers.
    ///
    /// Declare it (`true`) on every tool that can. A run nobody can answer would wait forever, and
    /// the composition layer uses the declaration to keep such a tool away from those runs:
    /// `adam-assembly` refuses to bind a subagent, which runs as a child of another run and has no
    /// user to ask, with a tool that says `true`. The default is `false`, which is what a tool
    /// written before this method existed declares. `#[tool(asks_user)]` and
    /// [`FnTool::asking_user`](crate::FnTool::asking_user) set it.
    ///
    /// It is a declaration, not a guard: nothing stops a tool that says `false` from returning
    /// `NeedsInput` anyway, and the run would park. A tool that wraps another (see
    /// [`ToolSet::wrap`](crate::ToolSet::wrap)) must forward it, as it forwards
    /// [`required_state`](Self::required_state).
    fn asks_user(&self) -> bool {
        false
    }

    /// Whether this tool's results are pages in the [paging convention](crate::paging): the tool
    /// takes `offset` and `limit` and ends every result with one final line that says where the
    /// next page starts. The loop's cap on a result (64 KiB) then tells the model to call again
    /// with a later `offset`. The default is `false`, and it is **a declaration of the tool**, never
    /// inferred from its schema: a tool that merely has an `offset` argument (an MCP tool, say) is
    /// not paged. `#[tool(paged)]` sets it. A tool that wraps another must forward it.
    fn paged(&self) -> bool {
        false
    }

    /// The longest one call of this tool may run. Past it the agent fires the call's
    /// [`ToolCtx::cancelled`] token, gives the tool [`TOOL_STOP_GRACE`] to stop, drops it, and
    /// answers the model with an error result that says the call did not finish (the result is
    /// journaled, so a replay never waits). `None`, the default, is [`DEFAULT_TOOL_TIME_LIMIT`].
    ///
    /// Declare it when a call can reasonably take longer (a build, a deployment's command limit) or
    /// must be quicker. A tool that waits on something else, a question or a child run, returns at
    /// once and is not affected. A tool that wraps another (see
    /// [`ToolSet::wrap`](crate::ToolSet::wrap)) must forward it, as it forwards
    /// [`required_state`](Self::required_state).
    fn time_limit(&self) -> Option<Duration> {
        None
    }

    /// How a call of this tool is drawn as a step ([`RunEvent::Step`]): its kind, the label (the
    /// tool's name when it says none) and an icon. The default is a plain tool: kind `tool`, the
    /// name as the label, no icon. A tool that hands its work to another agent says
    /// [`StepKind::Subagent`] here, so that the steps it reports nest under it
    /// (`#[tool(step = "subagent", label = "Helper", icon = "agent")]` writes it for a function).
    /// A tool that wraps another (see [`ToolSet::wrap`](crate::ToolSet::wrap)) must forward it, as
    /// it forwards [`required_state`](Self::required_state).
    fn step_style(&self) -> StepStyle {
        StepStyle::default()
    }

    /// What a call with these `args` shares with the other calls of its model turn: the loop runs
    /// the calls of a turn at the same time and keeps the model's order only between calls that
    /// conflict ([`ToolAccess`]). `args` are the call's arguments as the model made them, so the
    /// answer is a function of the recorded turn and the same on every replay.
    ///
    /// The default is [`ToolAccess::Alone`]: the call runs by itself, after every call before it,
    /// which is what a tool that declares nothing needs, because adam cannot know what it shares.
    /// Declare what the tool really uses (`#[tool(parallel = "read", path = "path")]` writes it
    /// for a function, [`FnTool::with_access`](crate::FnTool::with_access) for a closure). A tool
    /// whose [`asks_user`](Self::asks_user) is `true`, the session tools, and a tool that can park
    /// the run on a child or a remote task run alone whatever they say here.
    ///
    /// A tool that wraps another (see [`ToolSet::wrap`](crate::ToolSet::wrap)) must forward it, as
    /// it forwards [`required_state`](Self::required_state): one that does not makes the tool
    /// `Alone`, which is safe.
    fn access(&self, args: &Value) -> ToolAccess {
        let _ = args;
        ToolAccess::Alone
    }

    /// Whether the answer to this tool's own question may be acted on: the loop asks before it
    /// hands the answer to the model.
    ///
    /// The run is parked on a [`ToolError::NeedsInput`] of this tool and a message has arrived.
    /// `call_id` and `args` are the call that asked, as the model made it (the history already
    /// holds them, so the question needs no field of its own to be rebuilt), `ui` is the interface
    /// that went with the question, and `payload` is the inbound message's payload: when the person
    /// answered a form, its `form` member holds the form's `surface` and the action's `context`
    /// (`adam_a2a_runtime::PAYLOAD_FORM`). Words alone, and a question that went out as text, carry
    /// no `form`.
    ///
    /// [`AnswerCheck::Accept`], the default, hands the answer over as it came. [`AnswerCheck::Read`]
    /// hands over the words the tool read it as. [`AnswerCheck::Refuse`] keeps the run parked on a
    /// new question (the refusal's words and interface) and gives the model nothing: it never sees
    /// the refused answer, and no result is recorded for the call. [`AnswerCheck::End`] stops
    /// asking: the call ends with an error result and the model carries on.
    /// The check is a pure function of its arguments, so a replay refuses what it refused.
    /// A tool that wraps another (see [`ToolSet::wrap`](crate::ToolSet::wrap)) must forward it.
    fn read_answer(
        &self,
        call_id: &str,
        args: &Value,
        ui: Option<&Value>,
        payload: &Value,
    ) -> AnswerCheck {
        let _ = (call_id, args, ui, payload);
        AnswerCheck::Accept
    }

    /// How a task this tool started on another system stands. Only a tool that returns
    /// [`ToolError::AwaitRemote`] implements it.
    ///
    /// The agent calls it, in a journaled step, each time the timer of a run parked on
    /// [`AwaitRemote`](ToolError::AwaitRemote) fires, with the `task` the tool returned. The tool
    /// looks (a `GetTask` to an A2A agent) and answers [`RemotePoll::Ready`] with the tool result,
    /// or [`RemotePoll::Working`] to be asked again after the next interval. An `Err` is handled
    /// as for [`call`](Self::call): [`Transient`](ToolError::Transient) retries the step,
    /// [`Permanent`](ToolError::Permanent) becomes an error result for the model, and the waiting
    /// variants are refused as an error result.
    ///
    /// It has no arguments of the call: what it needs to look must be in `task`. The default
    /// refuses, which is right for a tool that never waits on a remote task. A tool that wraps
    /// another (see [`ToolSet::wrap`](crate::ToolSet::wrap)) must forward it.
    async fn poll_remote(&self, ctx: &ToolCtx, task: &str) -> Result<RemotePoll, ToolError> {
        let _ = (ctx, task);
        Err(ToolError::Permanent(format!(
            "tool `{}` does not wait on remote tasks",
            self.spec().name
        )))
    }
}

/// What a tool says about an answer to its own question: see [`Tool::read_answer`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AnswerCheck {
    /// Hand the answer to the model as the result of the call.
    Accept,
    /// Hand the model these words as the result of the call, in place of the answer's own: what the
    /// tool read the answer as.
    Read(String),
    /// The answer is not used: ask again.
    Refuse(Refusal),
    /// The answer is not used and the question is not asked again: the call ends with
    /// `Refusal::question` as its error result, which the model reads, and the run carries on.
    /// `Refusal::ui` is not used. The refusal is journaled like any other.
    End(Refusal),
}

impl AnswerCheck {
    /// The same check with every text the person or the model will read passed through `text`, and
    /// every interface through `ui`: for a wrapper that hides secrets (a [`Refusal`] is made from the
    /// call's own arguments, which a scrub has not seen).
    #[must_use]
    pub fn scrubbed(
        self,
        mut text: impl FnMut(String) -> String,
        mut ui: impl FnMut(&mut Value),
    ) -> Self {
        let mut scrub_refusal = |refusal: Refusal, text: &mut dyn FnMut(String) -> String| {
            let Refusal {
                question,
                ui: mut interface,
                rule,
                subject,
            } = refusal;
            if let Some(interface) = &mut interface {
                ui(interface);
            }
            Refusal {
                question: text(question),
                ui: interface,
                rule,
                subject,
            }
        };
        match self {
            Self::Accept => Self::Accept,
            Self::Read(said) => Self::Read(text(said)),
            Self::Refuse(refusal) => Self::Refuse(scrub_refusal(refusal, &mut text)),
            Self::End(refusal) => Self::End(scrub_refusal(refusal, &mut text)),
        }
    }
}

/// Why an answer is not used, and the question that takes its place: see [`AnswerCheck::Refuse`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Refusal {
    /// The new question the person reads: what was wrong, then the question.
    pub question: String,
    /// The interface of the new question ([`ToolError::NeedsInput::ui`]).
    pub ui: Option<Value>,
    /// The rule that failed, short and stable: journaled as `answer_refused`.
    pub rule: String,
    /// What the rule was about (a question's id), when it was about one thing.
    pub subject: Option<String>,
}

impl Refusal {
    /// A refusal that asks `question` again, under the interface `ui`, because of `rule`.
    pub fn new(question: impl Into<String>, ui: Option<Value>, rule: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            ui,
            rule: rule.into(),
            subject: None,
        }
    }

    /// Say what the rule was about.
    #[must_use]
    pub fn about(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }
}

/// How a tool call is drawn as a step: see [`Tool::step_style`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StepStyle {
    /// What kind of work the call is.
    pub kind: StepKind,
    /// The label; `None` is the tool's name.
    pub label: Option<String>,
    /// The picture; `None` is none.
    pub icon: Option<StepIcon>,
}

impl StepStyle {
    /// A step of `kind`, labelled with the tool's name, with no icon.
    pub fn new(kind: StepKind) -> Self {
        Self {
            kind,
            label: None,
            icon: None,
        }
    }

    /// Label the step `label` instead of the tool's name.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Draw the step with `icon`.
    #[must_use]
    pub fn with_icon(mut self, icon: StepIcon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// The report that says the call `call_id` of the tool `tool` is in `state`: id `tool:<call id>`,
    /// at the top, with this style.
    pub(crate) fn event(&self, tool: &str, call_id: &str, state: StepState) -> StepEvent {
        let label = self.label.as_deref().unwrap_or(tool);
        let step = StepEvent::new(step_id(call_id), self.kind, label, state);
        match self.icon {
            Some(icon) => step.with_icon(icon),
            None => step,
        }
    }
}

/// The id of the step of the call `call_id`: `tool:<call id>`.
pub(crate) fn step_id(call_id: &str) -> String {
    format!("tool:{call_id}")
}

/// What [`Tool::poll_remote`] found. Journaled, hence serializable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemotePoll {
    /// The task is over: this is the tool result (an error result if it failed).
    Ready(ToolOutput),
    /// Still going. Ask again after the next interval.
    Working,
}

/// A shared, type-erased [`Tool`].
pub type DynTool = Arc<dyn Tool>;

/// What a successful tool call returns.
///
/// Journaled, hence serializable.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Text handed back to the model as the tool result.
    pub content: String,
    /// Whether the tool reports failure. The model still sees `content` (the
    /// run does not fail); the tool message is marked `is_error`.
    #[serde(default)]
    pub is_error: bool,
    /// Outputs of the run, emitted as [`RunEvent::Artifact`] (durable, see
    /// `RunView::artifacts`).
    #[serde(default)]
    pub artifacts: Vec<Artifact>,
    /// Words the tool announces as **the run's answer**: when the run finishes, they are its
    /// output's `text` (the A2A `completed` status message), not the closing words of the model.
    /// See [`announcing`](Self::announcing). Never read from an error result.
    ///
    /// Journaled with the result, so a replay announces the same words; absent from a journal
    /// written before it existed, and not written while it is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// The announced [`answer`](Self::answer) is also **the end of the turn**: once the calls the
    /// model asked for in this turn are answered, the run finishes with it and the model is not
    /// called again. See [`final_answer`](Self::final_answer). Ignored without an answer, and never
    /// read from an error result.
    ///
    /// Absent from a journal written before it existed, and not written while it is `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ends_turn: bool,
    /// The time the call's commands ran in a sandbox, as the call reported it
    /// ([`ToolCtx::report_sandbox`]); the agent loop fills it in from what the call reported. Journaled
    /// with the result, so the run counts it once, and a replay counts the same.
    ///
    /// Absent from a journal written before it existed, and not written while empty.
    #[serde(default, skip_serializing_if = "SandboxUsage::is_empty")]
    pub sandbox: SandboxUsage,
    /// What the tool says makes two results of the same call **the same result for the
    /// repeated-call rule** (`doom_loop`): two answers of one call (same tool, same arguments) that
    /// carry the same key count as equal, whatever their text says. A command that fails the same way
    /// twice prints a different duration or path each time, so its text never repeats to the byte; the
    /// key (`exit:2`) does (boxed, so that a `Result<_, ToolOutput>` stays small). See [`with_repeat_key`](Self::with_repeat_key).
    ///
    /// Journaled with the result, so a replay decides the same; absent from a journal written before
    /// it existed, and not written while it is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_key: Option<Box<String>>,
    /// The conversation's plan as it is after this call ([`Plan`]): the whole plan, not a change.
    /// The loop makes it the run's plan when the result is not an error, in the same commit as the
    /// result; a replay takes it from the journal. Read the plan a call starts from with
    /// [`ToolCtx::plan`]. Never read from an error result.
    ///
    /// Absent from a journal written before it existed, and not written while it is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Box<Plan>>,
    /// A long text the tool wants the run to keep **in its own state**, for
    /// [`read_output`](crate::ReadOutput) to page through ([`keep_long`](crate::keep_long) makes it,
    /// and only when the agent has no [`KeptStore`](crate::KeptStore)). The loop moves it into
    /// [`Conversation::kept_outputs`](crate::Conversation::kept_outputs); it is journaled with the
    /// result, so a replay keeps the same text.
    ///
    /// Absent from a journal written before it existed, and not written while `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<Box<KeptOutput>>,
    /// The rounds of child sessions whose reports this result gives the model ([ADR 0045]), as run
    /// ids. A result that gives one is the moment the loop counts the tokens of the rounds **this
    /// run started** that have ended, in the run's totals, once each
    /// ([`Conversation::rounds_started`](crate::Conversation::rounds_started)); only whether the list
    /// is empty is read, because a run id differs between a run and a copy of its journal. Set by
    /// the session tools only. Never read from an error result.
    ///
    /// Absent from a journal written before it existed, and not written while empty.
    ///
    /// **Honoured only from a tool that runs alone.** The loop reads the rounds in a journaled step of
    /// its own right after the call; beside other calls (ADR 0049) that step has no place between
    /// them, so a call of a segment of several that reports rounds is answered with an error that
    /// tells the model to call the tool alone.
    ///
    /// [ADR 0045]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reported: Vec<adam_core::RunId>,
    /// The result is **pinned**: a compaction that drops it from the history copies it into the
    /// marker that stands in for what was dropped (up to 32 KiB, and 64 KiB for all the results that
    /// were pinned), so what the model must keep knowing (a workspace's path and branch, the
    /// instructions of a repository) outlives the summary. Set it only for a small result that is
    /// the tool's own and whose loss would leave the model not knowing where it is: it is copied as
    /// text into a user message. Never read from an error result.
    ///
    /// Absent from a journal written before it existed, and not written while it is `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// Images the result returns, by reference: [`keep_image`](crate::keep_image) admits and keeps
    /// them, [`with_image`](Self::with_image) adds one. The history holds them on the tool's message,
    /// and a model that sees is sent them after the run of tool messages; the references hold no
    /// byte. Never read from an error result.
    ///
    /// Absent from a journal written before it existed, and not written while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    /// The bytes of [`images`](Self::images), for the run to keep **in its own state**: only when
    /// the agent has no [`ImageStore`](crate::ImageStore) ([`keep_image`](crate::keep_image) says
    /// which). Journaled with the result, like [`kept`](Self::kept), so a replay keeps the same
    /// images.
    ///
    /// Absent from a journal written before it existed, and not written while empty.
    #[serde(default, skip_serializing_if = "no_kept_images")]
    pub kept_images: Box<[KeptImage]>,
}

/// `kept_images` is a boxed slice, which keeps a [`ToolOutput`] small: most results return none.
fn no_kept_images(kept: &[KeptImage]) -> bool {
    kept.is_empty()
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ToolOutput {
    /// A successful result with no artifacts.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            artifacts: Vec::new(),
            answer: None,
            ends_turn: false,
            sandbox: SandboxUsage::new(),
            repeat_key: None,
            plan: None,
            kept: None,
            reported: Vec::new(),
            pinned: false,
            images: Vec::new(),
            kept_images: Box::default(),
        }
    }

    /// A failed result the model should see and react to (for example invalid
    /// arguments). It does not fail the run.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            artifacts: Vec::new(),
            answer: None,
            ends_turn: false,
            sandbox: SandboxUsage::new(),
            repeat_key: None,
            plan: None,
            kept: None,
            reported: Vec::new(),
            pinned: false,
            images: Vec::new(),
            kept_images: Box::default(),
        }
    }

    /// Add the image `outcome` admitted ([`keep_image`](crate::keep_image)) to the result. An image
    /// that was not admitted adds nothing: its line is `ImageOutcome::stand_in`, for the text.
    #[must_use]
    pub fn with_image(mut self, outcome: ImageOutcome) -> Self {
        self.images.extend(outcome.image);
        if let Some(kept) = outcome.kept {
            let mut all = std::mem::take(&mut self.kept_images).into_vec();
            all.push(*kept);
            self.kept_images = all.into_boxed_slice();
        }
        self
    }

    /// Announce `text` as the run's answer: once the run finishes, `text` is what it answered, and
    /// what the model says in its last turn is not. A later call that announces replaces it (the
    /// last announcement wins), and one that fails announces nothing: an error result's `answer`
    /// is ignored. The announcement lasts until a new message reaches the run.
    ///
    /// For a tool that hands the final answer over by another route (the thread tool
    /// `turn_output` of `adam-ui` does), so that a client that reads only the run's output reads the
    /// same answer the person was shown.
    #[must_use]
    pub fn announcing(mut self, text: impl Into<String>) -> Self {
        self.answer = Some(text.into());
        self
    }

    /// Announce `text` as the run's answer **and end the turn with it**: like
    /// [`announcing`](Self::announcing), and once the calls of this model turn are answered the run
    /// finishes with `text` as its output, with no further model call, so the model has no closing
    /// line to write after an answer the person has already read. A message that reached the run
    /// meanwhile is read first: the model is called again with it, and the announcement ends there
    /// as a new turn's would. A call that fails ends nothing.
    ///
    /// For a tool whose call *is* the model's answer (`turn_output` of `adam-ui`).
    #[must_use]
    pub fn final_answer(mut self, text: impl Into<String>) -> Self {
        self.answer = Some(text.into());
        self.ends_turn = true;
        self
    }

    /// Say what makes this result the same as another of the same call for the repeated-call rule:
    /// three calls in a row with the same tool and arguments are refused when the two before carried
    /// the same key (or gave equal results, or both were errors). Set it only for a **failure** whose
    /// text varies and whose cause does not (`exit:2`), never on a success: a poll that succeeds with
    /// a changing text must keep running.
    #[must_use]
    pub fn with_repeat_key(mut self, key: impl Into<String>) -> Self {
        self.repeat_key = Some(Box::new(key.into()));
        self
    }

    /// Pin the result: see [`pinned`](Self::pinned).
    #[must_use]
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }

    /// Make `plan` the conversation's plan, when this result is not an error: see
    /// [`plan`](Self::plan).
    #[must_use]
    pub fn with_plan(mut self, plan: Plan) -> Self {
        self.plan = Some(Box::new(plan));
        self
    }

    /// Add an artifact.
    pub fn with_artifact(mut self, artifact: Artifact) -> Self {
        self.artifacts.push(artifact);
        self
    }
}

/// Why a tool call did not produce an output.
///
/// Journaled, hence serializable: the strings are the record and the serde shape is frozen, so
/// old journals stay replayable. That is why it carries no `source`: a tool flattens its own
/// cause into the message (with [`adam_error::report`]) before returning it.
///
/// Classes: `Transient` is [`ErrorClass::Transient`], `Permanent` is [`ErrorClass::Invalid`]
/// (the model asked for something that cannot work, and is told so), and `NeedsInput`, `AwaitRun`
/// and `AwaitRemote` are [`ErrorClass::Rejected`] (valid, but they need the user, a child run, or a
/// remote task, first).
///
/// The enum only grows: a variant added later decodes in journals written before it existed
/// (they never contain it), and a journal that contains a new variant is not readable by a
/// build that predates it, so the tool that returns `AwaitRun` or `AwaitRemote` ships with the
/// build that understands it.
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ToolError {
    /// Worth retrying (a timeout, a busy backend). The run's step fails with
    /// `AgentError::Transient` and the runtime retries it with backoff.
    #[error("transient tool error: {0}")]
    Transient(String),
    /// Not worth retrying. The run continues: the message becomes an error
    /// tool result for the model.
    #[error("permanent tool error: {0}")]
    Permanent(String),
    /// The tool cannot go on without an answer from the user. The agent emits
    /// the question and parks; the next inbound message becomes this call's
    /// result.
    #[error("tool needs input: {question}")]
    NeedsInput {
        /// What to ask the user.
        question: String,
        /// An interface the question comes with: an array of A2UI messages (a surface with the
        /// choices to draw), which the A2A server sends beside the question in the
        /// `input-required` status. `None` for a question that is only text. The agent does not
        /// read it: it keeps it with the question ([`PendingQuestion::ui`](crate::PendingQuestion::ui))
        /// and whoever serves the run draws it. Absent from journals written before it existed,
        /// which decode as `None`, and not written while it is `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ui: Option<Value>,
    },
    /// The answer is the result of a child run this tool started (or found: see below). The agent
    /// parks with a timer, and the child's outcome becomes this call's result, as text, or as an
    /// error result when the child failed.
    ///
    /// The tool starts the child with `Runtime::start_child`, under an id it derives from the
    /// call ([`ToolCtx::child_run_id`]), so that running the tool again, after a crash or a
    /// retry, finds the child it already started. The error is journaled like any other result,
    /// and the tool is not called again for this call: the wait is settled from the run's state.
    #[error("tool awaits run {run}")]
    AwaitRun {
        /// The child run whose outcome is the result.
        run: RunId,
    },
    /// The answer is the outcome of a task the tool started on another system (an A2A agent, a
    /// job queue). The agent parks with a timer
    /// ([`LlmAgentBuilder::wait_poll`](crate::LlmAgentBuilder::wait_poll)) and, each time it
    /// fires, asks the tool how the task
    /// stands with [`Tool::poll_remote`], as a journaled step. The tool's answer becomes this
    /// call's result.
    ///
    /// The tool starts the task in the same journaled step that returns this error, and must make
    /// the start idempotent, keyed by the call ([`ToolCtx::call_id`], [`ToolCtx::child_run_id`]),
    /// so that a step which ran but was not recorded does not start a second task. The error is
    /// journaled, and the tool is not called again for this call.
    #[error("tool awaits remote task {task}")]
    AwaitRemote {
        /// The tool's own name for the task, handed back to [`Tool::poll_remote`]. It is
        /// stored in the run's state and visible to whoever can read it: put no secret in it.
        task: String,
        /// Give up after this many milliseconds of waiting: the call is answered with an error
        /// result and the task is left where it is. `None`: wait for ever.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// The answer is the report of the round a child session is working on ([ADR 0045]). The agent
    /// parks with a timer, records a [`PendingWait::Session`](crate::PendingWait::Session), and each
    /// time it wakes (the round's end wakes it, the timer is the fallback) asks the tool how the
    /// session stands with [`Tool::poll_remote`], the session id as the `task`, in a journaled step,
    /// until the tool answers [`RemotePoll::Ready`] with the report.
    ///
    /// [ADR 0045]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0045-a-subagent-can-run-in-the-background-as-a-session.md
    #[error("tool awaits session {session}")]
    AwaitSession {
        /// The session whose round is awaited: the run id of its first round.
        session: RunId,
    },
}

impl ToolError {
    /// A question for the user that is only text: [`ToolError::NeedsInput`] with no interface.
    pub fn needs_input(question: impl Into<String>) -> Self {
        Self::NeedsInput {
            question: question.into(),
            ui: None,
        }
    }

    /// A question for the user that comes with an interface (`ui`, an array of A2UI messages):
    /// [`ToolError::NeedsInput`].
    pub fn needs_input_with_ui(question: impl Into<String>, ui: Value) -> Self {
        Self::NeedsInput {
            question: question.into(),
            ui: Some(ui),
        }
    }

    /// Turn a classified error into a tool error: a retryable one
    /// ([`ErrorClass::is_retryable`]) becomes [`Transient`](Self::Transient),
    /// anything else [`Permanent`](Self::Permanent). The message is the error
    /// and its whole source chain ([`adam_error::report`]), because a
    /// `ToolError` is journaled as text.
    ///
    /// ```
    /// use adam_error::{Classify, ErrorClass};
    /// use adam_llm_agent::ToolError;
    ///
    /// #[derive(Debug, thiserror::Error)]
    /// #[error("backend busy")]
    /// struct Busy;
    /// impl Classify for Busy {
    ///     fn class(&self) -> ErrorClass {
    ///         ErrorClass::Transient
    ///     }
    /// }
    ///
    /// assert_eq!(ToolError::from_classified(&Busy), ToolError::Transient("backend busy".into()));
    /// ```
    pub fn from_classified<E: Classify + 'static>(err: &E) -> Self {
        let message = adam_error::report(err);
        if err.is_retryable() {
            Self::Transient(message)
        } else {
            Self::Permanent(message)
        }
    }
}

impl Classify for ToolError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Transient(_) => ErrorClass::Transient,
            Self::Permanent(_) => ErrorClass::Invalid,
            Self::NeedsInput { .. }
            | Self::AwaitRun { .. }
            | Self::AwaitRemote { .. }
            | Self::AwaitSession { .. } => ErrorClass::Rejected,
        }
    }
}

/// What a tool sees of the run it executes in.
///
/// Owned and cheap to clone (the runtime's `Ctx` is borrowed mutably while a
/// step runs, so a tool cannot hold it).
#[derive(Debug, Clone)]
pub struct ToolCtx {
    run_id: RunId,
    conversation_id: Option<String>,
    attempt: u32,
    call_id: String,
    tool_name: String,
    emitter: Emitter,
    cancel: CancelToken,
    extensions: Arc<Extensions>,
    children: Option<ChildStarter>,
    context: Arc<Map<String, Value>>,
    root_run: RunId,
    /// The step of the root run this run works under, when it is a child (see
    /// `Conversation::root_step`); `None` for a run that is nobody's child.
    root_step: Option<String>,
    /// The step of this call (id, kind, label, icon): what [`emit_progress`](Self::emit_progress)
    /// updates and [`report_step`](Self::report_step) nests under.
    step: StepEvent,
    /// What the [`ToolSource`](crate::ToolSource) that offered this tool said about it when it listed
    /// it; `None` for the agent's own tools and for a listing that said nothing.
    note: Option<ToolNote>,
    /// The bytes of files the run may still share ([`files_left`](Self::files_left)).
    files_left: Arc<std::sync::atomic::AtomicU64>,
    /// What the call reported with [`report_sandbox`](Self::report_sandbox). Shared by the clones of
    /// the context, so a tool may hand one to a task it spawns.
    sandbox: Arc<Mutex<SandboxUsage>>,
    /// The bytes the run may still keep in its own state ([`kept_left`](Self::kept_left)). Shared by
    /// the clones of the context and spent by whatever the call keeps there, an image included.
    kept_left: Arc<std::sync::atomic::AtomicUsize>,
    /// The outputs the run keeps in its own state ([`kept_text`](Self::kept_text)).
    kept: Arc<BTreeMap<String, String>>,
    /// The conversation's plan as the call starts ([`plan`](Self::plan)).
    plan: Option<Arc<Plan>>,
    /// What scrubs secrets out of a text ([`scrub`](Self::scrub)); `None` scrubs nothing.
    scrub: Option<Scrubber>,
    /// What the call needs to admit images ([`keep_image`](crate::keep_image)); `None` for a
    /// detached context, which admits none.
    images: Option<Arc<ImagePolicy>>,
}

/// A function from a text to the same text with its secrets replaced.
#[derive(Clone)]
struct Scrubber(
    Arc<dyn Fn(&str) -> String + Send + Sync>,
    Option<Arc<dyn Fn() -> usize + Send + Sync>>,
);

impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scrubber")
    }
}

impl ToolCtx {
    #[allow(clippy::too_many_arguments)] // built in one place, from the pieces of a `Ctx`
    pub(crate) fn new(
        conversation_id: Option<String>,
        attempt: u32,
        call_id: String,
        tool_name: String,
        emitter: Emitter,
        cancel: CancelToken,
        extensions: Arc<Extensions>,
        children: Option<ChildStarter>,
        context: Arc<Map<String, Value>>,
        root_run: Option<RunId>,
        root_step: Option<String>,
        step: StepEvent,
    ) -> Self {
        let run_id = emitter.run_id();
        Self {
            run_id,
            conversation_id,
            attempt,
            call_id,
            tool_name,
            emitter,
            cancel,
            extensions,
            children,
            context,
            root_run: root_run.unwrap_or(run_id),
            root_step,
            step,
            note: None,
            files_left: Arc::new(std::sync::atomic::AtomicU64::new(MAX_RUN_FILE_BYTES as u64)),
            sandbox: Arc::default(),
            plan: None,
            scrub: None,
            kept_left: Arc::default(),
            kept: Arc::default(),
            images: None,
        }
    }

    /// Say how the run admits images: the gate for the model, the conversation's scope for the
    /// keeper, and the keeper in the agent's state. The agent loop does it for every call.
    #[must_use]
    pub(crate) fn with_images(mut self, gate: ImageGate, scope: String, root_run: String) -> Self {
        let store = self.state::<ImageStore>().map(|s| (*s).clone());
        self.images = Some(Arc::new(ImagePolicy {
            gate: std::sync::Mutex::new(gate),
            store,
            scope,
            root_run,
        }));
        self
    }

    /// Make this call spend the budgets of `first`, a call of the same segment: the room for kept
    /// outputs, the room for files and the image gate are one for all the calls that run together.
    #[must_use]
    pub(crate) fn sharing_budgets_of(mut self, first: &ToolCtx) -> Self {
        self.kept_left = Arc::clone(&first.kept_left);
        self.files_left = Arc::clone(&first.files_left);
        self.images.clone_from(&first.images);
        self
    }

    /// Say that the model of the call sees images within `limits`, with an empty history and no room
    /// to count in a run's state: for a context made outside a run (a test of a tool that offers
    /// images, [`keep_image`](crate::keep_image)). The agent loop does the same for every call, from
    /// the model's own word and what the history holds.
    #[must_use]
    pub fn with_image_limits(self, limits: adam_model::ImageLimits) -> Self {
        let root = self.root_run.to_string();
        let scope = crate::images::image_scope(None, &root);
        self.with_images(ImageGate::new(Some(limits), 0, None), scope, root)
    }

    /// Whether the model of this run can see images at all: a tool that would read or fetch an
    /// image only to have it replaced by a line may say so instead, and not read it.
    pub fn sees_images(&self) -> bool {
        self.images.as_ref().is_some_and(|p| {
            p.gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sees()
        })
    }

    /// Whether the model sees images that come from `origin`: it sees them and the deployment
    /// sends that source.
    pub fn sees_images_from(&self, origin: adam_model::ImageOrigin) -> bool {
        self.images.as_ref().is_some_and(|p| {
            p.gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sees_from(origin)
        })
    }

    pub(crate) fn image_policy(&self) -> Option<&ImagePolicy> {
        self.images.as_deref()
    }

    /// Give the call a function that scrubs secrets out of a text. A layer that scrubs the results
    /// of the tools it wraps (the coder's `Redacting`) sets it, so a tool can scrub **before** it
    /// cuts, truncates, hashes or splits a text it makes: a secret cut in half is no longer a secret
    /// the scrubber recognises, and the half that is left is durable. See [`scrub`](Self::scrub).
    #[must_use]
    pub fn with_scrub(mut self, scrub: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.scrub = Some(Scrubber(Arc::new(scrub), None));
        self
    }

    /// [`with_scrub`](Self::with_scrub), and a function that says how long the longest value the
    /// scrubber replaces is, in bytes. A tool that cuts a text it may only half have (the first part
    /// of a result) keeps back [`scrub_margin`](Self::scrub_margin) bytes of its end: a scrubber
    /// that holds a value longer than [`SCRUB_OVERLAP`](crate::SCRUB_OVERLAP) must say so here, and
    /// one that does not is taken to hold none.
    #[must_use]
    pub fn with_scrub_longest(
        mut self,
        scrub: impl Fn(&str) -> String + Send + Sync + 'static,
        longest: impl Fn() -> usize + Send + Sync + 'static,
    ) -> Self {
        self.scrub = Some(Scrubber(Arc::new(scrub), Some(Arc::new(longest))));
        self
    }

    /// How many bytes at the open end of a cut text may be the start of a secret:
    /// [`SCRUB_OVERLAP`](crate::SCRUB_OVERLAP), or the longest value the scrubber says it holds when
    /// that is more.
    pub fn scrub_margin(&self) -> usize {
        let longest = self
            .scrub
            .as_ref()
            .and_then(|s| s.1.as_ref())
            .map_or(0, |f| f());
        longest.max(crate::SCRUB_OVERLAP)
    }

    /// `text` with its secrets replaced, as the layer around this tool would; `text` itself when
    /// there is no such layer.
    ///
    /// **The rule for a tool that shortens a text it puts in its result** (a label cut to a
    /// limit, a line cut to a width): call this on the whole text first, then shorten what it
    /// returns. The layer scrubs the finished result as well, but only a secret still whole in it.
    pub fn scrub(&self, text: &str) -> String {
        match &self.scrub {
            Some(scrubber) => (scrubber.0)(text),
            None => text.to_owned(),
        }
    }

    /// Give the call the plan the conversation has when it starts: what [`plan`](Self::plan)
    /// answers. The agent does it for every call; a test of a plan tool does it to say what the
    /// plan was.
    #[must_use]
    pub fn with_plan(mut self, plan: impl Into<Option<Plan>>) -> Self {
        self.plan = plan.into().map(Arc::new);
        self
    }

    /// The conversation's plan as this call starts: the one a tool before it in the same turn left,
    /// since calls run one after the other. `None` while there is none. Return the new plan with
    /// [`ToolOutput::with_plan`]; this value is a copy, and changing nothing here changes nothing
    /// there.
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_deref()
    }

    /// Say how many bytes the run may still keep in its own state and what it keeps there:
    /// [`kept_left`](Self::kept_left) and [`kept_text`](Self::kept_text). The agent loop does it for
    /// every call.
    #[must_use]
    pub fn with_kept(mut self, left: usize, kept: Arc<BTreeMap<String, String>>) -> Self {
        self.kept_left = Arc::new(std::sync::atomic::AtomicUsize::new(left));
        self.kept = kept;
        self
    }

    /// The bytes a tool may still hand back in [`ToolOutput::kept`]: [`MAX_RUN_KEPT_BYTES`](crate::MAX_RUN_KEPT_BYTES)
    /// less what the run keeps already (none for a detached context).
    pub fn kept_left(&self) -> usize {
        self.kept_left.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `bytes` of the room were used: what a call keeps takes from what it may still keep.
    pub(crate) fn spend_kept(&self, bytes: usize) {
        use std::sync::atomic::Ordering::SeqCst;
        // A compare-and-swap loop: `fetch_update` is deprecated from Rust 1.99 and its
        // replacement is newer than the minimum version this workspace builds with.
        let mut left = self.kept_left.load(SeqCst);
        while let Err(now) =
            self.kept_left
                .compare_exchange_weak(left, left.saturating_sub(bytes), SeqCst, SeqCst)
        {
            left = now;
        }
    }

    /// The text the run keeps under the output id `id` in its own state.
    pub fn kept_text(&self, id: &str) -> Option<&str> {
        self.kept.get(id).map(String::as_str)
    }

    /// Say how many bytes of files the run may still share: what [`files_left`](Self::files_left)
    /// answers. The agent loop does it for every call.
    #[must_use]
    pub fn with_files_left(mut self, bytes: u64) -> Self {
        self.files_left = Arc::new(std::sync::atomic::AtomicU64::new(
            bytes.min(MAX_RUN_FILE_BYTES as u64),
        ));
        self
    }

    /// The bytes of files the run may still share: `MAX_RUN_FILE_BYTES` (6 MiB) less what it kept
    /// so far, as the turn's call began (the whole cap for a detached context). The loop refuses a
    /// file of a result that would go over, but only after the result is journaled; a tool that
    /// returns files it did not choose (an MCP server's images, a remote agent's file parts) keeps
    /// its result within this, so that one journal entry stays within the run's cap.
    pub fn files_left(&self) -> u64 {
        self.files_left.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `bytes` of the room for files are taken: call it when the call decides to return files of
    /// that size. The calls of one segment that run together share the room, so a call that starts
    /// after this one reads a smaller [`files_left`](Self::files_left). The loop still refuses, when
    /// the results are applied, a file that would go over the run's cap.
    pub fn spend_files(&self, bytes: u64) {
        use std::sync::atomic::Ordering::SeqCst;
        // A compare-and-swap loop: `fetch_update` is deprecated from Rust 1.99.
        let mut left = self.files_left.load(SeqCst);
        while let Err(now) =
            self.files_left
                .compare_exchange_weak(left, left.saturating_sub(bytes), SeqCst, SeqCst)
        {
            left = now;
        }
    }

    /// Give the call the note its source made about the tool when it listed it
    /// ([`ToolNote`]): what [`note`](Self::note) answers. The agent does it for the calls of a
    /// source's tools; a test of a source does it to say what a listing said.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<Option<ToolNote>>) -> Self {
        self.note = note.into();
        self
    }

    /// What the source that offered this tool said about it when it listed it: whether the system
    /// behind the source reports each call as a step itself, and how long a call may run. `None`
    /// for the agent's own tools and when the listing said nothing.
    pub fn note(&self) -> Option<&ToolNote> {
        self.note.as_ref()
    }

    /// Say that this call runs under the step `parent` (reported earlier): what
    /// [`parent_step_id`](Self::parent_step_id) answers. For a call made on behalf of a step the caller
    /// reported; a call of the model's own, from the agent loop, has none.
    #[must_use]
    pub fn under_step(mut self, parent: impl Into<String>) -> Self {
        self.step = self.step.under(parent);
        self
    }

    /// The step this call runs under, if one was reported for it ([`under_step`](Self::under_step)):
    /// `None` for a call the model asked for, which is at the top.
    pub fn parent_step_id(&self) -> Option<&str> {
        self.step.parent.as_deref()
    }

    /// A context detached from any run, for unit-testing a tool: a fresh run
    /// id, attempt 0, no conversation, and events sent to `sink`.
    pub fn detached(
        tool_name: impl Into<String>,
        call_id: impl Into<String>,
        sink: DynEventSink,
    ) -> Self {
        let tool_name = tool_name.into();
        let call_id = call_id.into();
        let emitter = Emitter::new(RunId::new(), "detached", sink);
        let step = StepStyle::default().event(&tool_name, &call_id, StepState::Running);
        Self::new(
            None,
            0,
            call_id,
            tool_name,
            emitter,
            CancelToken::new(),
            Arc::default(),
            None,
            Arc::default(),
            None,
            None,
            step,
        )
    }

    /// Make the context the one of a child run that works for `root`, as a run started with
    /// [`start_child`](Self::start_child) is.
    #[must_use]
    pub fn with_root_run(mut self, root: RunId) -> Self {
        self.root_run = root;
        self
    }

    /// Give the tool under test the inbound context of a run
    /// ([`Conversation::context`](crate::Conversation::context)), as it would read it with
    /// [`context`](Self::context) in a real one.
    pub fn with_context(mut self, context: Map<String, Value>) -> Self {
        self.context = Arc::new(context);
        self
    }

    /// Make `value` available to the tool under test through
    /// [`state`](Self::state), as
    /// [`LlmAgentBuilder::state`](crate::LlmAgentBuilder::state) does for a
    /// real run.
    pub fn with_state<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        Arc::make_mut(&mut self.extensions).insert(value);
        self
    }

    /// Replace the cancellation token, so a test can fire it
    /// ([`CancelToken::cancel`]) and check that the tool stops. Contexts
    /// built by the agent already carry the run's own token.
    pub fn with_cancel_token(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// The run this call belongs to.
    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    /// The run whose work this one serves: the top of the chain of parents when this run is a child
    /// started with [`start_child`](Self::start_child) (a subagent), this run itself when it is
    /// nobody's child. A tool that keeps something for the whole task, such as a workspace or a
    /// budget, keys it by this id, so that a subagent shares what its root run made.
    ///
    /// The id travels in the child's first message and its stored state, so it is the same after a
    /// restart and costs no store read.
    pub fn root_run_id(&self) -> RunId {
        self.root_run
    }

    /// The conversation the run belongs to, if any.
    pub fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.as_deref()
    }

    /// How many earlier tries of the current transition failed transiently
    /// (`0` on the first try). See `Ctx::attempt`.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The model's identifier for this call (`ToolCall::id`); stable across
    /// retries and replays, so usable as an idempotency key.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The id of the child run this call starts: [`adam_runtime::child_run_id`] of this run and
    /// [`call_id`](Self::call_id). The same on every replay of the call, so it is the id to pass to
    /// `Runtime::start_child` and to [`ToolError::AwaitRun`].
    pub fn child_run_id(&self) -> RunId {
        adam_runtime::child_run_id(self.run_id, &self.call_id)
    }

    /// Start `agent` as a child run of this run, with `message` as its first user message, under
    /// [`child_run_id`](Self::child_run_id), and return that id: what to put in
    /// [`ToolError::AwaitRun`]. The child starts on the runtime that steps this run, so the agent
    /// must be registered on it (as an agent or a starter).
    ///
    /// The first message also names the run and the step the child works under, the **root** run's
    /// ([`root_run_id`](Self::root_run_id), and this call's own step when this run is the root): the
    /// child reports its model calls to the root's task under that step (`usage/v1`), and hands both
    /// down to its own children.
    ///
    /// Idempotent: a call that runs again (a replay, a retry) finds the child it started. One child
    /// per call: starting a second one from the same call would find the first.
    ///
    /// # Errors
    ///
    /// [`ToolError::Permanent`] for an agent the runtime does not know, and for a context made by
    /// [`detached`](Self::detached), which belongs to no runtime; [`ToolError::Transient`] for a
    /// store failure worth retrying.
    pub async fn start_child(&self, agent: &str, message: &str) -> Result<RunId, ToolError> {
        self.start_child_within(agent, message, None).await
    }

    /// [`start_child`](Self::start_child) for a child that must finish within `within` (`None`: no
    /// limit). The runtime stores the deadline from its own clock; a child past it is failed with
    /// `cancelled: time limit: the run did not finish within N min`, which the parent reads as the
    /// child's error ("the run failed: ..."). See `Runtime::start_child_within`.
    ///
    /// # Errors
    ///
    /// As [`start_child`](Self::start_child).
    pub async fn start_child_within(
        &self,
        agent: &str,
        message: &str,
        within: Option<Duration>,
    ) -> Result<RunId, ToolError> {
        let children = self.child_starter()?;
        let child = self.child_run_id();
        let first = self.child_message(message);
        children
            .start_within(child, agent, first, within)
            .await
            .map_err(|e| ToolError::from_classified(&e))?;
        Ok(child)
    }

    /// The handle for starting children and reading sessions on the runtime that steps this run.
    pub(crate) fn child_starter(&self) -> Result<&ChildStarter, ToolError> {
        self.children.as_ref().ok_or_else(|| {
            ToolError::Permanent(
                "this tool context belongs to no runtime, so it cannot start a child run".into(),
            )
        })
    }

    /// The first message of a child of this run (or of a session's round): `message`, the run and
    /// step the child works under, and the protected context keys of this run.
    pub(crate) fn child_message(&self, message: &str) -> Inbound {
        let mut first = user_message(message);
        if let Some(payload) = first.payload.as_object_mut() {
            payload.insert(
                ROOT_RUN_KEY.to_owned(),
                Value::String(self.root_run.to_string()),
            );
            // The step of the root the child works under: this call's own when this run is the
            // root, the one this run works under when it is a child itself.
            let root_step = self.root_step.as_deref().unwrap_or(&self.step.id);
            payload.insert(
                ROOT_STEP_KEY.to_owned(),
                Value::String(root_step.to_owned()),
            );
            // A limit on this run (the access tier, who it was sent for) is the child's too, and its
            // own children's: the child's context starts with the protected keys of this one.
            let inherited: Map<String, Value> = PROTECTED_CONTEXT_KEYS
                .iter()
                .filter_map(|key| {
                    self.context
                        .get(*key)
                        .map(|value| ((*key).to_owned(), value.clone()))
                })
                .collect();
            if !inherited.is_empty() {
                payload.insert("context".to_owned(), Value::Object(inherited));
            }
        }
        first
    }

    /// A value of the run's inbound context ([`Conversation::context`](crate::Conversation::context)):
    /// what the caller said about itself in the messages it sent, under the key it used. The
    /// context is the same for every call of one model turn and changes only when a message
    /// arrives, so a tool that reads it is repeatable.
    ///
    /// Treat a value as input from whoever sent the message, and a secret in it (a token) as a
    /// secret: do not put it in a result the model reads.
    pub fn context(&self, key: &str) -> Option<&Value> {
        self.context.get(key)
    }

    /// The whole inbound context, for a tool that wants more than one key.
    pub fn context_map(&self) -> &Map<String, Value> {
        &self.context
    }

    /// The shared value of type `T` the agent was given, if any.
    pub fn state<T: Send + Sync + 'static>(&self) -> Option<State<T>> {
        self.extensions.get::<T>().map(State::from)
    }

    /// Like [`state`](Self::state), but a missing value is a
    /// [`ToolError::Permanent`] naming the type, for a tool that cannot go on
    /// without it. A tool that lists the type in
    /// [`Tool::required_state`] can only see this error when the agent was
    /// built with [`build`](crate::LlmAgentBuilder::build) instead of
    /// [`try_build`](crate::LlmAgentBuilder::try_build).
    pub fn require_state<T: Send + Sync + 'static>(&self) -> Result<State<T>, ToolError> {
        self.state::<T>().ok_or_else(|| {
            ToolError::Permanent(format!(
                "missing shared state `{}`: register it with LlmAgentBuilder::state",
                StateKey::of::<T>()
            ))
        })
    }

    /// The name this tool was called by.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Whether the run was cancelled (or finished by someone else) while this
    /// call runs. See [`ToolCtx::cancelled`].
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Resolves when the run is cancelled; never resolves for a run that is
    /// not. A tool with long-running work (a subprocess, a remote job, a
    /// stream) `select!`s on it to stop early:
    ///
    /// ```ignore
    /// tokio::select! {
    ///     out = child.wait() => out,
    ///     () = ctx.cancelled() => { child.kill().await?; Err(ToolError::Permanent("cancelled".into())) }
    /// }
    /// ```
    ///
    /// Cancelling commits the run as `Failed`, so whatever the tool returns
    /// afterwards is dropped: stop fast, clean up, and return anything (an
    /// `Err(ToolError::Permanent(..))` reads best in logs). Nothing here
    /// aborts the call; a tool that ignores the signal simply runs to its end.
    /// See [`adam_runtime::CancelToken`].
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await;
    }

    /// The token behind [`cancelled`](Self::cancelled), for handing to code
    /// that takes one (for example a spawned task).
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Say what the call is doing now: an update of its own step ([`step_id`](Self::step_id)),
    /// whose detail is `message`. Best effort and not durable, like every event. A client that
    /// asked for steps gets a bounded number of them (the orchestration layer keeps a few updates
    /// per step); one that did not gets `message` as a line of text, as it always did.
    pub async fn emit_progress(&self, message: impl Into<String>) {
        let update = self.step.clone().with_detail(message.into());
        self.emitter.emit(RunEvent::Step(update)).await;
    }

    /// Report that this call's commands ran in a sandbox (or that the run's environment was asked
    /// for): the usage is added to what the call has reported so far.
    ///
    /// The loop takes it when the call returns and adds it to the run's
    /// [`sandbox_usage`](crate::Conversation::sandbox_usage), in the same commit as the result: a
    /// result it journaled carries it ([`ToolOutput::sandbox`]), so it is counted once and kept
    /// across a restart. A call that returns [`ToolError::Permanent`] (a cancel, for one) is counted
    /// too; one that fails with [`ToolError::Transient`] is tried again and counts only the try that
    /// is kept.
    pub fn report_sandbox(&self, usage: &SandboxUsage) {
        self.sandbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .merge(usage);
    }

    /// What the call has reported with [`report_sandbox`](Self::report_sandbox) so far.
    pub fn reported_sandbox(&self) -> SandboxUsage {
        self.sandbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The id of this call's own step: `tool:<call id>`. The agent reports its start before the
    /// tool runs and its end after, so a tool that wants to say more about its work reports steps
    /// that run under it ([`report_step`](Self::report_step)).
    pub fn step_id(&self) -> &str {
        &self.step.id
    }

    /// Report a step of the work this call does: a command it ran, a tool of the agent it drove.
    /// It runs under this call's step unless `step` says it runs under another one
    /// ([`StepEvent::under`]) the call reported before. The first report of an id starts the step,
    /// the ones after update it, and a report whose state ends it ends it; a step the call leaves
    /// open when it returns is closed by whoever shows the tree.
    ///
    /// Ids must be unique within the run: put the call id in them (`acp:<call id>:<n>`). Best effort
    /// and not durable, like every event.
    pub async fn report_step(&self, step: StepEvent) {
        let step = if step.parent.is_some() {
            step
        } else {
            step.under(self.step.id.clone())
        };
        self.emitter.emit(RunEvent::Step(step)).await;
    }
}

#[cfg(test)]
mod tests {
    use adam_error::{Classify, ErrorClass};

    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("outer")]
    struct Classed(ErrorClass, #[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("inner")]
    struct Inner;

    impl Classify for Classed {
        fn class(&self) -> ErrorClass {
            self.0
        }
    }

    /// Journal entries as older builds wrote them, by hand: every shape that existed before
    /// `AwaitRun` still decodes to the same value.
    #[test]
    fn an_output_journaled_before_repeat_keys_decodes_and_a_key_round_trips() {
        let old: ToolOutput =
            serde_json::from_value(serde_json::json!({"content": "x", "is_error": false})).unwrap();
        assert_eq!(old.repeat_key, None);
        assert!(
            serde_json::to_value(&old)
                .unwrap()
                .get("repeat_key")
                .is_none()
        );
        let keyed = ToolOutput::text("x").with_repeat_key("exit:2");
        let back: ToolOutput =
            serde_json::from_value(serde_json::to_value(&keyed).unwrap()).unwrap();
        assert_eq!(
            back.repeat_key.as_deref().map(String::as_str),
            Some("exit:2")
        );
    }

    #[test]
    fn journals_written_before_await_run_still_decode() {
        let old = [
            (
                r#"{"Transient":"timed out"}"#,
                ToolError::Transient("timed out".into()),
            ),
            (
                r#"{"Permanent":"bad input"}"#,
                ToolError::Permanent("bad input".into()),
            ),
            (
                r#"{"NeedsInput":{"question":"which one?"}}"#,
                ToolError::needs_input("which one?"),
            ),
        ];
        for (json, expected) in old {
            let decoded: ToolError = serde_json::from_str(json).unwrap();
            assert_eq!(decoded, expected, "{json}");
            assert_eq!(
                serde_json::to_string(&expected).unwrap(),
                json,
                "the shape is frozen"
            );
        }
        // And a successful output, as recorded before, is not touched either.
        let out: ToolOutput = serde_json::from_str(r#"{"content":"ok"}"#).unwrap();
        assert_eq!(out, ToolOutput::text("ok"));
    }

    #[test]
    fn await_run_has_a_frozen_shape_and_is_rejected_by_class() {
        let run: RunId = serde_json::from_str(r#""0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b""#).unwrap();
        let e = ToolError::AwaitRun { run };
        let json = r#"{"AwaitRun":{"run":"0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"}}"#;
        assert_eq!(serde_json::to_string(&e).unwrap(), json);
        assert_eq!(serde_json::from_str::<ToolError>(json).unwrap(), e);
        assert_eq!(e.class(), ErrorClass::Rejected);
        assert!(!e.is_retryable());
        assert_eq!(
            e.to_string(),
            "tool awaits run 0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
        );
    }

    #[test]
    fn await_remote_has_a_frozen_shape_and_is_rejected_by_class() {
        let e = ToolError::AwaitRemote {
            task: "t-9".into(),
            timeout_ms: None,
        };
        let json = r#"{"AwaitRemote":{"task":"t-9"}}"#;
        assert_eq!(serde_json::to_string(&e).unwrap(), json);
        assert_eq!(serde_json::from_str::<ToolError>(json).unwrap(), e);
        let limited = ToolError::AwaitRemote {
            task: "t-9".into(),
            timeout_ms: Some(1500),
        };
        let json = r#"{"AwaitRemote":{"task":"t-9","timeout_ms":1500}}"#;
        assert_eq!(serde_json::to_string(&limited).unwrap(), json);
        assert_eq!(serde_json::from_str::<ToolError>(json).unwrap(), limited);
        assert_eq!(e.class(), ErrorClass::Rejected);
        assert!(!e.is_retryable());
        assert_eq!(e.to_string(), "tool awaits remote task t-9");
    }

    #[test]
    fn a_poll_result_round_trips() {
        for poll in [
            RemotePoll::Working,
            RemotePoll::Ready(ToolOutput::error("it failed")),
        ] {
            let json = serde_json::to_value(&poll).unwrap();
            assert_eq!(serde_json::from_value::<RemotePoll>(json).unwrap(), poll);
        }
    }

    #[test]
    fn the_calls_of_a_segment_spend_one_room_for_kept_outputs_and_one_image_gate() {
        let sink: DynEventSink = Arc::new(adam_runtime::NoopSink);
        let limits = adam_model::ImageLimits {
            max_bytes: 1024,
            max_count: 4,
            max_side: 100,
            sources: std::collections::BTreeSet::from([adam_model::ImageOrigin::Mcp]),
        };
        let one = ToolCtx::detached("a", "c1", sink.clone())
            .with_kept(1000, Arc::default())
            .with_image_limits(limits);
        let two = ToolCtx::detached("b", "c2", sink.clone())
            .with_kept(1000, Arc::default())
            .sharing_budgets_of(&one);
        one.spend_kept(300);
        assert_eq!(
            two.kept_left(),
            700,
            "what one call keeps is gone for the other"
        );
        two.spend_kept(500);
        assert_eq!(one.kept_left(), 200);
        assert!(two.sees_images(), "the image gate is the first call's");
        let alone = ToolCtx::detached("c", "c3", sink).with_kept(1000, Arc::default());
        assert_eq!(
            alone.kept_left(),
            1000,
            "a call outside the segment has its own"
        );
    }

    #[test]
    fn a_call_starts_the_same_child_on_every_replay() {
        use adam_runtime::NoopSink;
        let sink: DynEventSink = Arc::new(NoopSink);
        let ctx = ToolCtx::detached("t", "call_1", sink.clone());
        assert_eq!(ctx.child_run_id(), ctx.child_run_id());
        assert_eq!(
            ctx.child_run_id(),
            adam_runtime::child_run_id(ctx.run_id(), "call_1")
        );
        let other = ToolCtx::detached("t", "call_2", sink);
        assert_ne!(ctx.child_run_id(), other.child_run_id());
    }

    #[tokio::test]
    async fn a_detached_context_cannot_start_a_child() {
        use adam_runtime::NoopSink;
        let ctx = ToolCtx::detached("t", "call_1", Arc::new(NoopSink));
        let error = ctx.start_child("child", "go").await.unwrap_err();
        assert!(
            matches!(&error, ToolError::Permanent(m) if m.contains("belongs to no runtime")),
            "{error:?}"
        );
    }

    #[test]
    fn from_classified_splits_on_retryability_and_keeps_the_chain() {
        for class in [
            ErrorClass::Transient,
            ErrorClass::RateLimited,
            ErrorClass::Conflict,
        ] {
            assert_eq!(
                ToolError::from_classified(&Classed(class, Inner)),
                ToolError::Transient("outer: inner".into()),
                "{class:?}"
            );
        }
        for class in [
            ErrorClass::Invalid,
            ErrorClass::NotFound,
            ErrorClass::Internal,
        ] {
            assert_eq!(
                ToolError::from_classified(&Classed(class, Inner)),
                ToolError::Permanent("outer: inner".into()),
                "{class:?}"
            );
        }
    }
}
