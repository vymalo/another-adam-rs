//! The [`Tool`] trait and the values that flow through it.

use std::sync::Arc;

use adam_core::RunId;
use adam_error::{Classify, ErrorClass};
use adam_model::ToolSpec;
use adam_runtime::{Artifact, CancelToken, DynEventSink, Emitter, RunEvent};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

impl ToolOutput {
    /// A successful result with no artifacts.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            artifacts: Vec::new(),
        }
    }

    /// A failed result the model should see and react to (for example invalid
    /// arguments). It does not fail the run.
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            artifacts: Vec::new(),
        }
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
/// (the model asked for something that cannot work, and is told so), and `NeedsInput` is
/// [`ErrorClass::Rejected`] (valid, but it needs the user first).
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
    },
}

impl Classify for ToolError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Transient(_) => ErrorClass::Transient,
            Self::Permanent(_) => ErrorClass::Invalid,
            Self::NeedsInput { .. } => ErrorClass::Rejected,
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
}

impl ToolCtx {
    pub(crate) fn new(
        conversation_id: Option<String>,
        attempt: u32,
        call_id: String,
        tool_name: String,
        emitter: Emitter,
        cancel: CancelToken,
    ) -> Self {
        Self {
            run_id: emitter.run_id(),
            conversation_id,
            attempt,
            call_id,
            tool_name,
            emitter,
            cancel,
        }
    }

    /// A context detached from any run, for unit-testing a tool: a fresh run
    /// id, attempt 0, no conversation, and events sent to `sink`.
    pub fn detached(
        tool_name: impl Into<String>,
        call_id: impl Into<String>,
        sink: DynEventSink,
    ) -> Self {
        let tool_name = tool_name.into();
        let emitter = Emitter::new(RunId::new(), "detached", sink);
        Self::new(
            None,
            0,
            call_id.into(),
            tool_name,
            emitter,
            CancelToken::new(),
        )
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

    /// Report progress to observers as [`RunEvent::Progress`]. Best effort and
    /// not durable, like every event.
    pub async fn emit_progress(&self, message: impl Into<String>) {
        self.emitter
            .emit(RunEvent::Progress {
                message: message.into(),
            })
            .await;
    }
}
