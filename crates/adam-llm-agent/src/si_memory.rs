//! `si_recall` and `si_record`: the run's self-improvement memory as two ordinary tool steps
//! (ADR 0081 decision 6).
//!
//! Both are [`Tool`]s like any other, so the loop journals them: a replay serves the recorded
//! result and asks the store nothing, and a retried step writes one attempt (the attempt id is the
//! tool call's id, and [`SiMemory::record`] treats a repeat of one it holds as a no-op).
//!
//! They are offered only when the run has a memory: [`AgentDef::si_memory`] resolved `On` **and**
//! the deployment supplied a handle. A run with `si_memory: off` or `SI_MEMORY=off` gets neither
//! tool nor loader, so its tool list is what it was.
//!
//! [`AgentDef::si_memory`]: https://docs.rs/adam-assembly

use std::sync::Arc;

use adam_memory::{AttemptDraft, AttemptId, MemoryError, Problem, Provenance, Scope, SiMemory};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use crate::toolset::ToolSet;

/// The tool that reads the run's memory.
pub const SI_RECALL: &str = "si_recall";

/// The tool that writes to it.
pub const SI_RECORD: &str = "si_record";

/// The lazy group the two tools load together under (ADR 0066): `load__group__memory`.
pub const SI_MEMORY_GROUP: &str = "memory";

/// What the two tools read: the store, and the scope this run writes and reads in.
///
/// One handle per run: the scope names the deployment, the caller and the agent, and no entry
/// crosses a caller (ADR 0081 decision 4). A deployment that serves several agents gives each its
/// own.
#[derive(Clone)]
pub struct SiMemoryHandle {
    memory: Arc<dyn SiMemory>,
    scope: Scope,
}

impl std::fmt::Debug for SiMemoryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiMemoryHandle")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl SiMemoryHandle {
    /// A handle over `memory` for the run that works in `scope`.
    #[must_use]
    pub fn new(memory: Arc<dyn SiMemory>, scope: Scope) -> Self {
        Self { memory, scope }
    }

    /// The scope this handle reads and writes.
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The two tools, ready for a [`ToolSet`] and a group named [`SI_MEMORY_GROUP`].
    #[must_use]
    pub fn tools(&self) -> ToolSet {
        ToolSet::new()
            .tool(SiRecall {
                handle: self.clone(),
            })
            .tool(SiRecord {
                handle: self.clone(),
            })
    }
}

/// `si_recall`: the scope's problems, freshest first, with what was tried and what was seen.
pub struct SiRecall {
    handle: SiMemoryHandle,
}

#[async_trait]
impl Tool for SiRecall {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SI_RECALL.to_owned(),
            description: "Read what this agent has already learnt here: the problems it met, the \
                          remedies it tried, what it predicted and what it saw. Call it before \
                          solving something that looks familiar; it asks no one and costs one \
                          step. `si_record` writes what you learn."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        match self.handle.memory.recall(&self.handle.scope) {
            Ok(problems) => Ok(ToolOutput::text(render(&problems))),
            Err(error) => Err(refuse(error)),
        }
    }
}

/// `si_record`: open a problem by its root cause, or add the attempt you are making to one.
pub struct SiRecord {
    handle: SiMemoryHandle,
}

#[async_trait]
impl Tool for SiRecord {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SI_RECORD.to_owned(),
            description: "Record a problem and the attempt you are making at it, so the next run \
                          starts from what this one learnt. `root_cause` is the short reason it \
                          happened and is the problem's identity: the same root cause extends the \
                          problem instead of opening another. Write `predicted` before you see the \
                          outcome. A text that still holds a secret is refused and nothing is \
                          stored; the run goes on."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "root_cause": {
                        "type": "string",
                        "description": "The short root cause: why it happened, not what broke.",
                    },
                    "remedy": {
                        "type": "string",
                        "description": "What you are doing about it.",
                    },
                    "rationale": {
                        "type": "string",
                        "description": "Why you expect that to work.",
                    },
                    "predicted": {
                        "type": "string",
                        "description": "What you expect to see if it does; written before the outcome.",
                    },
                },
                "required": ["root_cause"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let root_cause = match text_arg(&args, "root_cause") {
            Ok(text) => text,
            Err(why) => return Ok(ToolOutput::error(why)),
        };
        let draft = AttemptDraft::new(
            root_cause,
            AttemptId::new(ctx.call_id()),
            Provenance::new(ctx.run_id(), ctx.conversation_id().unwrap_or("-")),
        );
        let draft = match optional_text(&args, "remedy") {
            Ok(Some(remedy)) => draft.remedy(remedy),
            Ok(None) => draft,
            Err(why) => return Ok(ToolOutput::error(why)),
        };
        let draft = match optional_text(&args, "rationale") {
            Ok(Some(rationale)) => draft.rationale(rationale),
            Ok(None) => draft,
            Err(why) => return Ok(ToolOutput::error(why)),
        };
        let draft = match optional_text(&args, "predicted") {
            Ok(Some(predicted)) => draft.predicted(predicted),
            Ok(None) => draft,
            Err(why) => return Ok(ToolOutput::error(why)),
        };
        match self.handle.memory.record(&self.handle.scope, draft) {
            Ok(problem) => Ok(ToolOutput::text(format!(
                "recorded: {} now holds {} attempt(s)",
                problem.root_cause,
                problem.attempts.len()
            ))),
            Err(error) => Err(refuse(error)),
        }
    }
}

/// A [`MemoryError`] as the loop sees it: a refusal the model reads and corrects, or a backend
/// failure worth retrying (ADR 0081 decision 12: an unreachable memory never stops a run, but the
/// step may be retried).
fn refuse(error: MemoryError) -> ToolError {
    match adam_error::Classify::class(&error) {
        adam_error::ErrorClass::Invalid | adam_error::ErrorClass::Rejected => {
            ToolError::Permanent(error.to_string())
        }
        _ => ToolError::from_classified(&error),
    }
}

/// The problems as text: bounded by the store's own limits, so a page is never unbounded.
fn render(problems: &[Problem]) -> String {
    if problems.is_empty() {
        return "no problem recorded in this scope yet".to_owned();
    }
    let mut out = format!(
        "{} problem(s) in this scope, freshest first:",
        problems.len()
    );
    for problem in problems {
        out.push_str("\n\n- ");
        out.push_str(&problem.root_cause);
        for attempt in &problem.attempts {
            out.push_str("\n  - ");
            out.push_str(if attempt.remedy.is_empty() {
                "(no remedy written)"
            } else {
                &attempt.remedy
            });
            if !attempt.predicted.is_empty() {
                out.push_str(" | predicted: ");
                out.push_str(&attempt.predicted);
            }
            match &attempt.observed {
                Some(observation) => {
                    out.push_str(" | observed: ");
                    out.push_str(&observation.detail);
                }
                None => out.push_str(" | not observed yet"),
            }
        }
    }
    out
}

/// A required non-empty text argument.
fn text_arg(args: &Value, name: &str) -> Result<String, String> {
    match args.get(name) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
        Some(Value::String(_)) => Err(format!("`{name}` is empty")),
        Some(_) => Err(format!("`{name}` must be a string")),
        None => Err(format!("`{name}` is required")),
    }
}

/// An optional non-empty text argument.
fn optional_text(args: &Value, name: &str) -> Result<Option<String>, String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("`{name}` must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use adam_memory::{InProcessSiMemory, REDACTION_MARKER, Scrubber, Settings};
    use serde_json::json;

    use super::*;

    /// A memory whose scrubber redacts anything that looks like a key, the way a deployment's
    /// does: a text that still holds the marker after it is refused.
    fn memory() -> Arc<InProcessSiMemory> {
        Arc::new(InProcessSiMemory::new(Settings::new(Scrubber::new(
            |text: &str| {
                if text.contains("sk-live") {
                    format!("{text} {REDACTION_MARKER}")
                } else {
                    text.to_owned()
                }
            },
        ))))
    }

    fn handle(memory: Arc<InProcessSiMemory>) -> SiMemoryHandle {
        SiMemoryHandle::new(memory, Scope::agent("dep", "caller", "adam-coder"))
    }

    fn ctx(call: &str) -> ToolCtx {
        ToolCtx::detached(SI_RECORD, call, Arc::new(adam_runtime::NoopSink))
    }

    async fn record(
        handle: &SiMemoryHandle,
        call: &str,
        args: Value,
    ) -> Result<ToolOutput, ToolError> {
        SiRecord {
            handle: handle.clone(),
        }
        .call(&ctx(call), args)
        .await
    }

    /// `si_record` opens a problem, and `si_recall` reads it back with what was predicted.
    #[tokio::test]
    async fn a_recorded_attempt_is_recalled() {
        let handle = handle(memory());
        record(
            &handle,
            "call-1",
            json!({
                "root_cause": "the lockfile was stale",
                "remedy": "regenerate the lockfile",
                "predicted": "the build resolves its dependencies",
            }),
        )
        .await
        .expect("a valid record");

        let read = SiRecall {
            handle: handle.clone(),
        }
        .call(&ctx("call-2"), json!({}))
        .await
        .expect("a readable memory");
        let text = read.content;
        assert!(text.contains("the lockfile was stale"), "{text}");
        assert!(text.contains("regenerate the lockfile"), "{text}");
        assert!(
            text.contains("the build resolves its dependencies"),
            "{text}"
        );
        assert!(text.contains("not observed yet"), "{text}");
    }

    /// A text that still holds a secret is refused, nothing is stored, and the run goes on: the
    /// error is a permanent one the model reads, not a failure of the run.
    #[tokio::test]
    async fn a_secret_in_a_field_is_refused_and_nothing_is_stored() {
        let handle = handle(memory());
        let error = record(
            &handle,
            "call-1",
            json!({ "root_cause": "the token sk-live-123 was in the log" }),
        )
        .await
        .expect_err("a secret is refused");
        assert!(
            matches!(error, ToolError::Permanent(_)),
            "a refusal is permanent, not transient: {error:?}"
        );

        let read = SiRecall {
            handle: handle.clone(),
        }
        .call(&ctx("call-2"), json!({}))
        .await
        .expect("a readable memory");
        assert!(
            read.content.contains("no problem recorded"),
            "{}",
            read.content
        );
    }

    /// The attempt id is the call's: recording the same call twice writes one attempt, which is
    /// what makes a retried tool step land once (ADR 0081 decision 6).
    #[tokio::test]
    async fn the_same_call_records_one_attempt() {
        let handle = handle(memory());
        let args = json!({ "root_cause": "the lockfile was stale", "remedy": "regenerate it" });
        record(&handle, "call-1", args.clone())
            .await
            .expect("first");
        record(&handle, "call-1", args).await.expect("retry");
        let read = SiRecall {
            handle: handle.clone(),
        }
        .call(&ctx("call-2"), json!({}))
        .await
        .expect("a readable memory");
        assert!(
            read.content.contains("1 problem(s)"),
            "a retried call opens no second problem: {}",
            read.content
        );
        assert_eq!(
            read.content.matches("regenerate it").count(),
            1,
            "a retried call writes one attempt: {}",
            read.content
        );
    }

    /// The two tools are a group: their names, and nothing else, so a run with `si_memory: off`
    /// has neither.
    #[test]
    fn the_group_holds_the_two_tools() {
        let tools = handle(memory()).tools();
        let names: Vec<String> = tools.names();
        assert_eq!(names, vec![SI_RECALL.to_owned(), SI_RECORD.to_owned()]);
        assert_eq!(SI_MEMORY_GROUP, "memory");
    }
}
