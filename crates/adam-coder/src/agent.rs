//! The coder as a durable agent: an [`LlmAgent`](adam_llm_agent::LlmAgent) with the coder's instructions
//! and tools, plus one rule the tools cannot express alone.

use std::sync::Arc;

use adam::{AgentDef, Assembly, AssemblyError};
use adam_error::report;
use adam_llm_agent::{Conversation, DynTool, LlmStarter, ToolSet};
use adam_model::{DynModel, Message, ToolCall};
use adam_runtime::{
    Agent, AgentError, AgentStarter, Ctx, Inbound, RUN_FINISHED_KIND, RunEvent, Transition,
};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::redact::Redactor;
use crate::tools::named::named_in;
use crate::tools::notes::RunNotes;
use crate::tools::{ToolEnv, ask, coder_tools};

/// The agent's name, as stored in `RunRecord::agent`. It is the `name` in `agent/instructions.md`
/// (the two are checked against each other by a unit test).
pub const AGENT_NAME: &str = "coder";

/// The agent `build.rs` embedded from `agent/`: its prompt, limits and A2A card.
mod embedded {
    // The generated module also has `AGENTS` and `PACKAGE`, which nothing here uses.
    #![allow(dead_code)]

    adam::include_agent!();
}

pub(crate) use embedded::AGENT;

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

/// [`LlmAgent`](adam_llm_agent::LlmAgent) + the coder's completion policy.
///
/// The `LlmAgent` is assembled from `agent/instructions.md` (the prompt, the limits, the
/// `max_check_cycles` var) and the [`coder_tools`]; this type adds the one thing files cannot say,
/// the policy below (see the README, "Where the prompt and the card live").
///
/// When the model stops (a turn without tool calls) the run completes only if it
/// delivered: it opened a pull request. Otherwise:
///
/// * A run that ends with the last check run red and no pull request fails, with
///   the findings as the error. That is what "at most N check/fix cycles, then
///   report the findings and stop" turns into: the model reports, the run is
///   `failed`, and nothing was opened.
/// * The same goes for a run whose credentials were rejected (GitHub or git
///   answered 401/403): the model cannot fix a bad token, so ending without a
///   pull request is a failure that names the token, not a completed task.
/// * Anything else is a question, not a completion: a model that answers "Hi! I
///   need a repository and a task" in plain text asked something, and the run
///   parks exactly as if it had called `ask_user` (see `stop_as_question`). The
///   person's answer resumes the run.
///
/// Before every step the agent also records which repositories the person named
/// (the task and every answer) in the run notes, because `prepare_workspace`
/// works on no other (see [`tools::prepare`](crate::tools::prepare)).
pub struct CoderAgent {
    assembly: Assembly,
    env: Arc<ToolEnv>,
}

impl CoderAgent {
    /// The coder over `model` (gateway alias `model_alias`) with the standard
    /// tools.
    ///
    /// # Panics
    ///
    /// When the agent cannot be assembled: see [`try_new`](Self::try_new). The embedded files are
    /// fixed at build time and a unit test binds them, so only a `model_alias` that is empty or
    /// has whitespace in it can cause this; a process that takes the alias from its environment
    /// should call `try_new` and report the error.
    pub fn new(model: DynModel, model_alias: impl Into<String>, env: Arc<ToolEnv>) -> Self {
        expect_assembled(Self::try_new(model, model_alias, env))
    }

    /// Like [`new`](Self::new) with an explicit toolset, for tests that wrap
    /// the standard tools.
    ///
    /// # Panics
    ///
    /// When the agent cannot be assembled: see [`try_with_tools`](Self::try_with_tools).
    pub fn with_tools(
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
        tools: impl IntoIterator<Item = DynTool>,
    ) -> Self {
        expect_assembled(Self::try_with_tools(model, model_alias, env, tools))
    }

    /// [`new`](Self::new), returning the error instead of panicking.
    ///
    /// # Errors
    ///
    /// [`AssemblyError`] (boxed: it is large) when the agent cannot be assembled, as the assembly reports it: a
    /// `model_alias` that is empty or has whitespace in it.
    pub fn try_new(
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
    ) -> Result<Self, Box<AssemblyError>> {
        let tools = coder_tools(&env);
        Self::try_with_tools(model, model_alias, env, tools)
    }

    /// [`with_tools`](Self::with_tools), returning the error instead of panicking.
    ///
    /// The steps are the ones any agent written as files takes: the embedded definition, the value
    /// of the `max_check_cycles` var (the prompt tells the model the limit the tools enforce), the
    /// tools, the state they read and the model.
    ///
    /// # Errors
    ///
    /// [`AssemblyError`] (boxed: it is large) when the agent cannot be assembled, as the assembly reports it: a
    /// `model_alias` that is empty or has whitespace in it.
    pub fn try_with_tools(
        model: DynModel,
        model_alias: impl Into<String>,
        env: Arc<ToolEnv>,
        tools: impl IntoIterator<Item = DynTool>,
    ) -> Result<Self, Box<AssemblyError>> {
        let assembly = AgentDef::from_manifest(AGENT)?
            .var("max_check_cycles", env.settings.max_check_cycles)
            .bind(tools.into_iter().collect::<ToolSet>())?
            .state(env.clone())
            .model(model, model_alias)?;
        Ok(Self { assembly, env })
    }

    /// What the agent was assembled from: the prompt the model sees, the limits, the tools it is
    /// offered and the model alias (`assembly().info()[0]`), and the A2A card
    /// (`assembly().card(url, version)`).
    pub fn assembly(&self) -> &Assembly {
        &self.assembly
    }

    /// Note the repositories the person has named so far, for `prepare_workspace`.
    ///
    /// The person's words are the user messages of the conversation, the answers to `ask_user`
    /// (tool results of that tool) and what is waiting in the inbox, which the step takes next.
    /// What the model or a tool said is never read: a repository found in a README is not one the
    /// person asked for.
    async fn record_named_repos(
        &self,
        ctx: &Ctx,
        state: &Conversation,
        run: &str,
    ) -> Result<(), AgentError> {
        let notes_error = |e| AgentError::transient("cannot read the run notes").with_source(e);
        let named: Vec<String> = person_texts(state, ctx.peek_inbox())
            .iter()
            .flat_map(|text| named_in(text))
            .collect();
        let mut notes = self.env.notes.load(run).await.map_err(notes_error)?;
        if notes.name_repos(named) {
            self.env
                .notes
                .save(run, &notes)
                .await
                .map_err(|e| AgentError::transient("cannot write the run notes").with_source(e))?;
        }
        Ok(())
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

/// What the person said in `state` and `inbox`, oldest first: their messages, and their answers
/// to `ask_user` (which reach the model as that tool's results).
fn person_texts(state: &Conversation, inbox: &[Inbound]) -> Vec<String> {
    let asks: Vec<&str> = state
        .messages
        .iter()
        .flat_map(Message::tool_calls)
        .filter(|call| call.name == ask::TOOL_NAME)
        .map(|call| call.id.as_str())
        .collect();
    let mut texts: Vec<String> = state
        .messages
        .iter()
        .chain(&state.deferred)
        .filter_map(|message| match message {
            Message::User { .. } => Some(message.text()),
            Message::Tool { call_id, .. } if asks.contains(&call_id.as_str()) => {
                Some(message.text())
            }
            _ => None,
        })
        .collect();
    texts.extend(
        inbox
            .iter()
            .filter(|inbound| inbound.kind != RUN_FINISHED_KIND)
            .filter_map(|inbound| match &inbound.payload {
                Value::String(text) => Some(text.clone()),
                payload => payload
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }),
    );
    texts
}

/// What the run asks when the model stopped with nothing to say.
const EMPTY_STOP_QUESTION: &str = "I stopped without delivering anything. Which repository should I work on, and what should I do?";

/// A model that stopped without opening a pull request and without a failure to report asked
/// something (or has nothing to offer): park the run as `ask_user` would, with the model's text
/// as the question.
///
/// The conversation is made to say what happened: the model's last message gets an `ask_user`
/// call with its text as the question, which the parked run owes an answer to. The person's
/// answer is that call's result, exactly as for a real `ask_user`, so the history stays valid
/// for every provider (a result without a call, or two user turns in a row, is not) and the
/// model sees its own stop as the question it was. The A2A backend reads `input-required` and the
/// question from the same place as for `ask_user` (`pending_wait`), and the run waits with no
/// timer until a message is delivered.
///
/// When the last message is not a plain assistant reply (it cannot be, after a `Done`), the run
/// completes as the model left it.
async fn stop_as_question(
    ctx: &Ctx,
    mut state: Conversation,
    output: Value,
) -> Transition<Conversation> {
    let text = output
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let question = match text.trim() {
        "" => EMPTY_STOP_QUESTION.to_owned(),
        said => said.to_owned(),
    };
    let call = ToolCall {
        id: format!("stop-{}", state.turns),
        name: ask::TOOL_NAME.to_owned(),
        arguments: json!({ "question": question }),
    };
    match state.messages.last_mut() {
        Some(Message::Assistant { tool_calls, .. }) if tool_calls.is_empty() => {
            tool_calls.push(call.clone());
        }
        _ => return Transition::Done { state, output },
    }
    ctx.emit(RunEvent::Custom {
        kind: "input_required".into(),
        payload: json!({ "question": question, "call_id": call.id }),
    })
    .await;
    state.pending_wait = Some(adam_llm_agent::PendingWait::Question(
        adam_llm_agent::PendingQuestion {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            question,
        },
    ));
    state.pending_calls = vec![call];
    Transition::Park {
        state,
        wake_at: None,
    }
}

/// The panic of the constructors that do not return a `Result`.
#[allow(clippy::expect_used)] // documented under `# Panics` on the callers
fn expect_assembled(assembled: Result<CoderAgent, Box<AssemblyError>>) -> CoderAgent {
    assembled.expect("the coder's embedded agent must assemble")
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
        self.record_named_repos(ctx, &state, &run).await?;
        // Whatever leaves this step as a failure, a retry note or the final
        // answer may quote OpenCode's stderr, a check's output or a provider's
        // error body, so it passes through the redactor.
        let transition = match self.assembly.root().step(ctx, state).await {
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
                    None if notes.pull_request.is_some() => Transition::Done { state, output },
                    None => stop_as_question(ctx, state, output).await,
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

    /// The runs are stored under `AGENT_NAME` and the starter registers that name; the assembled
    /// agent registers the `name` of its file. They must not drift apart.
    #[test]
    fn the_embedded_agent_is_named_like_the_constant() {
        assert_eq!(AGENT.name, AGENT_NAME);
        assert_eq!(CoderStarter.name(), AGENT.name);
    }

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
