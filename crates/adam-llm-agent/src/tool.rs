//! The [`Tool`] trait and the values that flow through it.

use std::sync::Arc;

use adam_core::RunId;
use adam_error::{Classify, ErrorClass};
use adam_model::ToolSpec;
use adam_runtime::{
    Artifact, CancelToken, ChildStarter, DynEventSink, Emitter, RunEvent, StepEvent, StepIcon,
    StepKind, StepState,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::conversation::{ROOT_RUN_KEY, ROOT_STEP_KEY, user_message};
use crate::source::ToolNote;
use crate::state::{Extensions, State, StateKey};

/// A capability the model may call.
///
/// The agent runs every call inside a journaled step: once a call has
/// returned, its result is recorded and never recomputed, even across a crash
/// or a change of worker. A call that dies *before* it is recorded (a crash
/// mid-call, or [`ToolError::Transient`]) runs again, and so does every tool
/// of the same model turn that ran before it in the failed attempt. A tool
/// with side effects must therefore be safe to retry: idempotent, or guarded
/// by a key derived from [`ToolCtx::call_id`].
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

    /// How a call of this tool is drawn as a step ([`RunEvent::Step`]): its kind, the label (the
    /// tool's name when it says none) and an icon. The default is a plain tool: kind `tool`, the
    /// name as the label, no icon. A tool that hands its work to another agent says
    /// [`StepKind::Subagent`] here, so that the steps it reports nest under it
    /// (`#[tool(step = "subagent", label = "OpenCode", icon = "agent")]` writes it for a function).
    /// A tool that wraps another (see [`ToolSet::wrap`](crate::ToolSet::wrap)) must forward it, as
    /// it forwards [`required_state`](Self::required_state).
    fn step_style(&self) -> StepStyle {
        StepStyle::default()
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
        }
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
            Self::NeedsInput { .. } | Self::AwaitRun { .. } | Self::AwaitRemote { .. } => {
                ErrorClass::Rejected
            }
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
        let Some(children) = &self.children else {
            return Err(ToolError::Permanent(
                "this tool context belongs to no runtime, so it cannot start a child run".into(),
            ));
        };
        let child = self.child_run_id();
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
        }
        children
            .start(child, agent, first)
            .await
            .map_err(|e| ToolError::from_classified(&e))?;
        Ok(child)
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
