//! [`SubagentTool`]: the tool a parent calls to run a subagent as a durable child run.

use adam_llm_agent::{StepIcon, StepKind, StepStyle, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};

/// What every subagent tool says after the subagent's own description: the child starts with an
/// empty history.
const NOT_SEEN: &str =
    "The agent does not see this conversation; put everything it needs in `message`.";

/// The spec every subagent tool has, local or remote: `{ "message": string }`, described by the
/// subagent's own `description` and then the note that it starts with no history.
pub(crate) fn subagent_spec(tool_name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: tool_name.to_owned(),
        description: match description.trim() {
            "" => NOT_SEEN.to_owned(),
            text => format!("{text} {NOT_SEEN}"),
        },
        parameters: json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The task, with everything the agent needs to do it: \
                                    it starts with no history and cannot ask you anything.",
                }
            },
            "required": ["message"],
            "additionalProperties": false,
        }),
    }
}

/// A subagent's tool name as the label of its step: the name with its separators as spaces and the
/// first letter capitalised (`explorer` is "Explorer", `code_reviewer` is "Code reviewer"). The
/// description is free text for the model, so its first words make a poor title.
pub(crate) fn title_of(tool_name: &str) -> String {
    let spaced = tool_name.replace(['_', '-'], " ");
    let mut chars = spaced.trim().chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => tool_name.to_owned(),
    }
}

/// The tool that runs a subagent, one per subagent, named after it.
///
/// `AgentDef::bind` adds one to the parent of every local subagent, so a user of this crate
/// never builds one; it is public so that code which assembles agents by hand can give an
/// agent the same tool for any agent registered on its runtime.
///
/// The tool takes `{ "message": string }` and its description is the subagent's `description`
/// followed by "The agent does not see this conversation; put everything it needs in `message`."
/// A call starts the subagent as a child run of the parent's run (`ToolCtx::start_child`, under
/// the id derived from the call, so a replay finds the child it started), with `message` as its
/// first user message, and returns [`ToolError::AwaitRun`]: the parent parks, and the child's final
/// text becomes the tool result (an error result when the child failed). See "Child runs" in
/// `docs/architecture.md`.
///
/// What that means for the model and for the author:
///
/// * The child sees only `message`, never the parent's history.
/// * Only the child's final **text** comes back. Its artifacts stay on the child's run.
/// * The calls of one model turn run one after another, so two subagent calls in a turn run one
///   after the other and not side by side.
/// * A `message` that is missing, not a string, or blank is an error result for the model to fix; no
///   child is started.
#[derive(Debug, Clone)]
pub struct SubagentTool {
    /// The name the child is registered under on the runtime: `coder/reviewer`.
    agent: String,
    spec: ToolSpec,
}

impl SubagentTool {
    /// A tool called `tool_name` that runs `agent`, the name it is registered under on the
    /// runtime (`coder/reviewer`), and describes it with `description`.
    pub fn new(tool_name: impl Into<String>, agent: impl Into<String>, description: &str) -> Self {
        Self {
            agent: agent.into(),
            spec: subagent_spec(&tool_name.into(), description),
        }
    }

    /// The registration name of the agent this tool runs.
    pub fn agent(&self) -> &str {
        &self.agent
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    /// A call is an agent working for this one: a `subagent` step, drawn as an agent and called by
    /// the tool's name as a person reads it (`explorer` is "Explorer").
    fn step_style(&self) -> StepStyle {
        StepStyle::new(StepKind::Subagent)
            .with_label(title_of(&self.spec.name))
            .with_icon(StepIcon::Agent)
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let message = match args.get("message") {
            Some(Value::String(text)) if !text.trim().is_empty() => text,
            _ => {
                return Ok(ToolOutput::error(format!(
                    "`{}` needs `message`, a non-empty string with the whole task",
                    self.spec.name
                )));
            }
        };
        let run = ctx.start_child(&self.agent, message).await?;
        Err(ToolError::AwaitRun { run })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam_runtime::NoopSink;

    use super::*;

    fn tool() -> SubagentTool {
        SubagentTool::new("reviewer", "coder/reviewer", "Reviews a diff.  ")
    }

    #[test]
    fn the_spec_is_the_description_and_a_message() {
        let spec = tool().spec();
        assert_eq!(spec.name, "reviewer");
        assert_eq!(
            spec.description,
            "Reviews a diff. The agent does not see this conversation; put everything it needs in `message`."
        );
        assert_eq!(spec.parameters["required"], json!(["message"]));
        assert_eq!(spec.parameters["additionalProperties"], json!(false));
        assert_eq!(spec.parameters["properties"]["message"]["type"], "string");
        assert_eq!(tool().agent(), "coder/reviewer");
        // A hand-built manifest may have no description: the note stands alone.
        assert_eq!(
            SubagentTool::new("x", "a/x", " ").spec().description,
            NOT_SEEN
        );
    }

    #[test]
    fn a_call_is_a_subagent_step_drawn_as_an_agent() {
        let style = tool().step_style();
        assert_eq!(style.kind, StepKind::Subagent);
        assert_eq!(style.icon, Some(StepIcon::Agent));
        assert_eq!(style.label.as_deref(), Some("Reviewer"));
    }

    #[test]
    fn a_title_is_the_tool_name_as_a_person_reads_it() {
        assert_eq!(title_of("explorer"), "Explorer");
        assert_eq!(title_of("code_reviewer"), "Code reviewer");
        assert_eq!(title_of("billing-agent"), "Billing agent");
        assert_eq!(title_of("x"), "X");
    }

    #[tokio::test]
    async fn a_missing_or_blank_message_is_an_error_result_and_starts_nothing() {
        let ctx = ToolCtx::detached("reviewer", "c1", Arc::new(NoopSink));
        for args in [
            json!({}),
            json!({"message": ""}),
            json!({"message": "  \n"}),
            json!({"message": 7}),
            json!("go"),
        ] {
            let out = tool().call(&ctx, args.clone()).await.unwrap();
            assert!(out.is_error, "{args}");
            assert!(out.content.contains("`reviewer` needs `message`"), "{args}");
        }
    }

    #[tokio::test]
    async fn without_a_runtime_the_call_is_refused_for_good() {
        // A detached context belongs to no run, so nothing can be started from it.
        let ctx = ToolCtx::detached("reviewer", "c1", Arc::new(NoopSink));
        let error = tool()
            .call(&ctx, json!({"message": "review it"}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::Permanent(_)), "{error:?}");
    }
}
