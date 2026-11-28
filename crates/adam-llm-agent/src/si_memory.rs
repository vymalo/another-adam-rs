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

use adam_memory::{
    AttemptDraft, AttemptId, Audience, Lesson, Memory, MemoryError, Problem, Provenance, Scope,
    SiMemory,
};
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

/// What the two tools read: the store, the scope this run writes and reads in, and how the
/// **caller** is named (ADR 0081 decision 4: no L1 entry crosses a caller).
///
/// The caller is a property of the message, not of the process: `adam-agent` serves several, so a
/// handle built once cannot hold one fixed caller. It holds the deployment, the agent and the kind,
/// and a resolver that names the caller of the call being made — `adam-service` supplies one that
/// reads the A2A subject, and a process with no callers passes
/// [`one_caller`](Self::one_caller).
#[derive(Clone)]
pub struct SiMemoryHandle {
    memory: Arc<dyn SiMemory>,
    scope: Scope,
    caller: CallerResolver,
    /// The deployment's L2, when it configured one (`MEMORY_URL`), and the audience this agent's
    /// runs read. `None` is L1 alone, which is a working deployment (ADR 0081 decision 11).
    l2: Option<Arc<dyn Memory>>,
    audience: Option<Audience>,
}

/// How a handle names the caller of one call.
type CallerResolver = Arc<dyn Fn(&ToolCtx) -> String + Send + Sync>;

impl std::fmt::Debug for SiMemoryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SiMemoryHandle")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl SiMemoryHandle {
    /// A handle over `memory` for the run that works in `scope`, with `caller` naming the caller of
    /// every call.
    #[must_use]
    pub fn new(
        memory: Arc<dyn SiMemory>,
        scope: Scope,
        caller: impl Fn(&ToolCtx) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            memory,
            scope,
            caller: Arc::new(caller),
            l2: None,
            audience: None,
        }
    }

    /// The same handle reading the deployment's L2 as well: the lessons of `audience` come back
    /// from `si_recall` after the L1 problems (ADR 0081 decision 6).
    #[must_use]
    pub fn with_l2(mut self, memory: Arc<dyn Memory>, audience: Audience) -> Self {
        self.l2 = Some(memory);
        self.audience = Some(audience);
        self
    }

    /// A handle whose every call is the same caller: a process that serves one, or a test.
    #[must_use]
    pub fn one_caller(memory: Arc<dyn SiMemory>, scope: Scope, caller: impl Into<String>) -> Self {
        let caller = caller.into();
        Self::new(memory, scope, move |_| caller.clone())
    }

    /// The scope of one call: the handle's deployment, agent and kind, and the caller the resolver
    /// names for `ctx`.
    fn scope_for(&self, ctx: &ToolCtx) -> Scope {
        self.scope.clone().with_caller((self.caller)(ctx))
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

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let problems = match self.handle.memory.recall(&self.handle.scope_for(ctx)) {
            Ok(problems) => problems,
            Err(error) => return Err(refuse(error)),
        };
        let mut text = render(&problems);
        if let (Some(l2), Some(audience)) = (&self.handle.l2, &self.handle.audience) {
            match l2.recall(audience) {
                Ok(lessons) => {
                    text.push_str("\n\n");
                    text.push_str(&render_lessons(&lessons));
                }
                // An unreachable L2 never fails the run (decision 12): the L1 half still answers,
                // and the model is told the lessons could not be read.
                Err(error) => {
                    text.push_str("\n\nthe shared lessons could not be read: ");
                    text.push_str(&error.to_string());
                }
            }
        }
        Ok(ToolOutput::text(text))
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
        match self
            .handle
            .memory
            .record(&self.handle.scope_for(ctx), draft)
        {
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

/// The L2 lessons as text, after the L1 problems: what another run already learnt and a reviewer
/// approved (ADR 0081 decisions 9 and 10).
fn render_lessons(lessons: &[Lesson]) -> String {
    if lessons.is_empty() {
        return "no approved lesson for this audience yet".to_owned();
    }
    let mut out = format!("{} approved lesson(s), freshest first:", lessons.len());
    for lesson in lessons {
        out.push_str("\n\n- ");
        out.push_str(&lesson.root_cause);
        out.push_str("\n  remedy: ");
        out.push_str(&lesson.remedy);
        if !lesson.rationale.is_empty() {
            out.push_str("\n  why: ");
            out.push_str(&lesson.rationale);
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
    use adam_memory::{
        Audience, InProcessMemory, InProcessSiMemory, LessonDraft, LessonProvenance,
        LessonSettings, MemoryScope, REDACTION_MARKER, Scrubber, Settings,
    };
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
        SiMemoryHandle::one_caller(memory, Scope::agent("dep", "-", "adam-coder"), "caller")
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

    /// `si_recall` appends the audience's approved lessons from the L2 store the handle carries,
    /// after the L1 problems (ADR 0081 decisions 4 and 11): one tool answers for both tiers.
    #[tokio::test]
    async fn recall_appends_the_audiences_lessons_from_l2() {
        let l2 = Arc::new(InProcessMemory::new(
            LessonSettings::new(Scrubber::new(|text: &str| text.to_owned())),
            MemoryScope::new("release-1"),
        ));
        l2.record(
            LessonDraft::new(
                Audience::new("adam-coder"),
                "the lockfile was stale",
                LessonProvenance::new(["scope-a"], "the reviewer"),
            )
            .remedy("regenerate the lockfile"),
        )
        .expect("a valid lesson");
        let handle = handle(memory()).with_l2(l2, Audience::new("adam-coder"));
        let read = SiRecall { handle }
            .call(&ctx("call-1"), json!({}))
            .await
            .expect("a readable memory");
        assert!(
            read.content.contains("1 approved lesson(s)"),
            "{}",
            read.content
        );
        assert!(
            read.content.contains("regenerate the lockfile"),
            "{}",
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
