//! The coder as a durable agent: an [`LlmAgent`] with the coder's instructions
//! and tools, plus one rule the tools cannot express alone.

use std::sync::Arc;

use adam_llm_agent::{Conversation, DynTool, Limits, LlmAgent};
use adam_model::DynModel;
use adam_runtime::{Agent, AgentError, Ctx, Inbound, Transition};
use async_trait::async_trait;

use crate::instructions::instructions;
use crate::tools::notes::RunNotes;
use crate::tools::{ToolEnv, coder_tools};

/// The agent's name, as stored in `RunRecord::agent`.
pub const AGENT_NAME: &str = "coder";

/// Bounds of one coding run. A real task takes far more turns than the
/// `LlmAgent` defaults allow (every delegation, check and commit is a turn).
pub fn coder_limits() -> Limits {
    Limits {
        max_turns: 200,
        max_tool_calls: 400,
        max_output_tokens: 8192,
        max_history_tokens: 100_000,
    }
}

/// [`LlmAgent`] + the coder's completion policy.
///
/// When the model stops (a turn without tool calls) the run normally
/// completes. But a run that ends with the last check run red and no pull
/// request has not delivered: it fails, with the findings as the error. That is
/// what "at most N check/fix cycles, then report the findings and stop" turns
/// into: the model reports, the run is `failed`, and nothing was opened.
///
/// The same goes for a run whose credentials were rejected (GitHub or git
/// answered 401/403): the model cannot fix a bad token, so ending without a
/// pull request is a failure that names the token, not a completed task.
pub struct CoderAgent {
    inner: LlmAgent,
    env: Arc<ToolEnv>,
}

impl CoderAgent {
    /// The coder over `model` (gateway alias `model_alias`) with the standard
    /// tools.
    pub fn new(model: DynModel, model_alias: impl Into<String>, env: Arc<ToolEnv>) -> Self {
        let tools = coder_tools(&env);
        Self::with_tools(model, model_alias, env, tools)
    }

    /// Like [`new`](Self::new) with an explicit toolset, for tests that wrap
    /// the standard tools.
    pub fn with_tools(
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
        tools: Vec<DynTool>,
    ) -> Self {
        let mut builder = LlmAgent::builder(AGENT_NAME, model, model_alias)
            .instructions(instructions(env.settings.max_check_cycles))
            .limits(coder_limits());
        for tool in tools {
            builder = builder.dyn_tool(tool);
        }
        Self {
            inner: builder.build(),
            env,
        }
    }

    /// Why the run must fail instead of completing, if it must.
    fn verdict(&self, notes: &RunNotes) -> Option<String> {
        if notes.pull_request.is_some() {
            return None;
        }
        if let Some(blocker) = &notes.blocker {
            return Some(format!("no pull request was opened: {blocker}"));
        }
        if !notes.last_check_failed() {
            return None;
        }
        let last = notes.checks.last.as_ref()?;
        let max = self.env.settings.max_check_cycles;
        Some(format!(
            "checks are failing and no pull request was opened ({} of {max} check cycles used). \
             Findings from `{}` (exit code {:?}):\n{}",
            notes.checks.failures, last.command, last.exit_code, last.tail
        ))
    }
}

#[async_trait]
impl Agent for CoderAgent {
    type State = Conversation;

    fn name(&self) -> &str {
        AGENT_NAME
    }

    fn init(&self, input: Inbound) -> Result<Conversation, AgentError> {
        self.inner.init(input)
    }

    async fn step(
        &self,
        ctx: &mut Ctx,
        state: Conversation,
    ) -> Result<Transition<Conversation>, AgentError> {
        let run = ctx.run_id().to_string();
        let redactor = &self.env.redactor;
        // Whatever leaves this step as a failure, a retry note or the final
        // answer may quote OpenCode's stderr, a check's output or a provider's
        // error body, so it passes through the redactor.
        let transition = match self.inner.step(ctx, state).await {
            Ok(t) => t,
            Err(AgentError::Transient {
                message,
                retry_after,
                source,
            }) => {
                return Err(AgentError::Transient {
                    message: redactor.scrub_string(message),
                    retry_after,
                    source,
                });
            }
            Err(AgentError::Permanent { message, source }) => {
                return Err(AgentError::Permanent {
                    message: redactor.scrub_string(message),
                    source,
                });
            }
            Err(e) => return Err(e),
        };
        match transition {
            Transition::Fail { state, error } => Ok(Transition::Fail {
                state,
                error: redactor.scrub_string(error),
            }),
            Transition::Done { state, mut output } => {
                redactor.scrub_value(&mut output);
                let notes = self.env.notes.load(&run).await.map_err(|e| {
                    AgentError::transient(format!("cannot read the run notes: {e}"))
                })?;
                Ok(match self.verdict(&notes) {
                    Some(error) => Transition::Fail {
                        state,
                        error: redactor.scrub_string(error),
                    },
                    None => Transition::Done { state, output },
                })
            }
            other => Ok(other),
        }
    }
}
