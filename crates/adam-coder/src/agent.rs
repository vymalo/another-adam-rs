//! The coder as a durable agent: an [`LlmAgent`] with the coder's instructions
//! and tools, plus one rule the tools cannot express alone.

use std::sync::Arc;

use adam_error::report;
use adam_llm_agent::{Conversation, DynTool, Limits, LlmAgent, LlmStarter};
use adam_model::DynModel;
use adam_runtime::{Agent, AgentError, AgentStarter, Ctx, Inbound, Transition};
use async_trait::async_trait;

use crate::instructions::instructions;
use crate::redact::Redactor;
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

/// The start-only half of the [`CoderAgent`]: its name and its `init`, with no model, tools or
/// credentials.
///
/// A process that only accepts tasks registers this (`RuntimeBuilder::starter`, or
/// [`Coder::control_plane`](crate::Coder::control_plane)) and a worker with the [`CoderAgent`]
/// steps the runs. [`CoderAgent::init`] delegates here, so the two cannot disagree on the state a
/// run starts with.
#[derive(Debug, Clone, Default)]
pub struct CoderStarter;

impl AgentStarter for CoderStarter {
    type State = Conversation;

    fn name(&self) -> &str {
        AGENT_NAME
    }

    fn init(&self, input: Inbound) -> Result<Conversation, AgentError> {
        LlmStarter::new(AGENT_NAME).init(input)
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
        CoderStarter.init(input)
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
            Err(e) => return Err(boundary_error(e, redactor)),
        };
        match transition {
            Transition::Fail { state, error } => Ok(Transition::Fail {
                state,
                error: redactor.failure_text(error),
            }),
            Transition::Done { state, mut output } => {
                redactor.scrub_value(&mut output);
                let notes = self.env.notes.load(&run).await.map_err(|e| {
                    AgentError::transient("cannot read the run notes").with_source(e)
                })?;
                Ok(match self.verdict(&notes) {
                    Some(error) => Transition::Fail {
                        state,
                        error: redactor.failure_text(error),
                    },
                    None => Transition::Done { state, output },
                })
            }
            other => Ok(other),
        }
    }
}

/// The boundary a failed step crosses on its way to the run's failure text and the retry note:
/// the whole error chain is flattened into the message, scrubbed of the process's secrets, and
/// bounded ([`Redactor::failure_text`]). The retry hint survives; the source does not, because a
/// source is exactly where a secret hides once nothing scrubs it. A `Store` error carries no model
/// or tool text and passes through; a variant added later becomes a scrubbed permanent error.
fn boundary_error(e: AgentError, redactor: &Redactor) -> AgentError {
    let clean = |message: String,
                 source: Option<&(dyn std::error::Error + Send + Sync + 'static)>| {
        let text = match source {
            Some(cause) => format!("{message}: {}", report(cause)),
            None => message,
        };
        redactor.failure_text(text)
    };
    match e {
        AgentError::Transient {
            message,
            retry_after,
            source,
        } => {
            let out = AgentError::transient(clean(message, source.as_deref()));
            match retry_after {
                Some(after) => out.with_retry_after(after),
                None => out,
            }
        }
        AgentError::Permanent { message, source } => {
            AgentError::permanent(clean(message, source.as_deref()))
        }
        AgentError::NonDeterminism { message, source } => {
            AgentError::non_determinism(clean(message, source.as_deref()))
        }
        store @ AgentError::Store(_) => store,
        other => AgentError::permanent(redactor.failure_text(report(&other))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_error::Classify;
    use std::time::Duration;

    const KEY: &str = "sk-live-0123456789abcdef";

    fn redactor() -> Redactor {
        Redactor::new([KEY])
    }

    fn text(e: &AgentError) -> String {
        report(e)
    }

    /// Regression for A6: a failure's text embeds what the peer said (an upstream body, OpenCode's
    /// stderr tail), which can echo the model key. Every variant is scrubbed, including a cause
    /// that used to be printed after the message without passing the redactor, and the retry hint
    /// is kept.
    #[test]
    fn failure_text_never_carries_the_key_whatever_the_variant() {
        let cause = || std::io::Error::other(format!("upstream echoed Bearer {KEY} back"));
        for e in [
            AgentError::transient(format!("model call failed: {KEY}")),
            AgentError::transient("model call failed").with_source(cause()),
            AgentError::transient("slow down")
                .with_source(cause())
                .with_retry_after(Duration::from_secs(30)),
            AgentError::permanent(format!("bad request: {KEY}")),
            AgentError::permanent("bad request").with_source(cause()),
            AgentError::non_determinism("step differs").with_source(cause()),
        ] {
            let before = text(&e);
            let out = boundary_error(e, &redactor());
            let after = text(&out);
            assert!(!after.contains(KEY), "{after}");
            assert!(
                after.contains(crate::redact::REDACTED) || !before.contains(KEY),
                "{after}"
            );
            assert!(std::error::Error::source(&out).is_none(), "{after}");
        }
    }

    #[test]
    fn the_class_and_the_retry_hint_survive_the_boundary() {
        let out = boundary_error(
            AgentError::transient("rate limited")
                .with_source(std::io::Error::other("429"))
                .with_retry_after(Duration::from_secs(30)),
            &redactor(),
        );
        assert_eq!(out.retry_after(), Some(Duration::from_secs(30)));
        assert!(out.is_retryable());
        assert_eq!(text(&out), "transient error: rate limited: 429");

        let out = boundary_error(AgentError::permanent("no"), &redactor());
        assert!(!out.is_retryable());
        let out = boundary_error(AgentError::non_determinism("no"), &redactor());
        assert!(matches!(out, AgentError::NonDeterminism { .. }));
    }

    /// Store errors keep their variant: the worker decides them by class.
    #[test]
    fn a_store_error_passes_through() {
        let e = AgentError::Store(adam_core::StoreError::InvalidInput("x".into()));
        assert!(matches!(
            boundary_error(e, &redactor()),
            AgentError::Store(adam_core::StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn failure_text_is_bounded_and_scrubbed_before_it_is_cut() {
        let r = redactor();
        // The key straddles the cut: cutting first would leave its front half visible.
        let padding = "x".repeat(crate::redact::MAX_FAILURE_TEXT - KEY.len() / 2);
        let out = r.failure_text(format!("{padding}{KEY}{}", "y".repeat(5000)));
        assert!(
            out.len() <= crate::redact::MAX_FAILURE_TEXT + " [truncated]".len(),
            "{}",
            out.len()
        );
        assert!(out.ends_with(" [truncated]"));
        assert!(!out.contains(&KEY[..8]), "the front of the key leaked");
        // A multi-byte character at the cut is not split.
        let out = r.failure_text("é".repeat(3000));
        assert!(out.ends_with(" [truncated]"));
        assert!(out.is_char_boundary(out.len() - " [truncated]".len()));
        // Short text is left alone.
        assert_eq!(r.failure_text("short".into()), "short");
    }

    #[test]
    fn the_starter_carries_the_agents_name_and_reads_the_same_start_message() {
        use adam_llm_agent::user_message;

        assert_eq!(CoderStarter.name(), AGENT_NAME);
        let state = CoderStarter.init(user_message("fix it")).unwrap();
        assert_eq!(state, Conversation::new("fix it"));
        // A start message the agent would reject is rejected here, before a run exists.
        let err = CoderStarter
            .init(Inbound::new("message", serde_json::json!({"text": 7})))
            .unwrap_err();
        assert!(matches!(err, AgentError::Permanent { .. }), "{err:?}");
    }
}
