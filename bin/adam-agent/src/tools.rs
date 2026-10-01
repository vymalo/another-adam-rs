//! The one tool every agent folder gets: `ask_user`. Everything else an agent offers comes from its
//! folder: the tools of its MCP servers (`mcp.json`), the skills tools, and a tool per subagent.

use adam::prelude::*;

/// What a tool call returns.
type Outcome = Result<ToolOutput, ToolError>;

/// The tool's name.
pub const ASK_USER: &str = "ask_user";

// Parks the run until the person answers.
//
// `ToolError::NeedsInput` makes the agent emit the question and park with no timer, which the A2A
// backend reports as `input-required`; the next message delivered to the task becomes this call's
// result.

/// Ask the person you are talking to a question and wait for the answer. Use it only when you
/// cannot proceed without it, or to get explicit consent before something that cannot be undone.
/// Be specific.
#[tool(asks_user)]
pub async fn ask_user(
    /// What you need to know
    question: String,
) -> Outcome {
    match Some(question.trim()).filter(|q| !q.is_empty()) {
        Some(question) => Err(ToolError::needs_input(question)),
        None => Ok(ToolOutput::error("question is required")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use adam::{Tool, ToolCtx};
    use adam_runtime::CollectingSink;
    use serde_json::{Value, json};

    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
    }

    /// What the model reads: the name, a description that fits any agent (the coder's names pull
    /// requests, this one does not), and one required string.
    #[test]
    fn the_spec_is_what_the_model_reads() {
        let spec = AskUser.spec();
        assert_eq!(spec.name, ASK_USER);
        assert!(
            spec.description
                .starts_with("Ask the person you are talking to a question"),
            "{}",
            spec.description
        );
        assert!(
            !spec.description.contains("pull request"),
            "{}",
            spec.description
        );
        assert_eq!(spec.parameters["type"], "object");
        assert_eq!(spec.parameters["required"], json!(["question"]));
        assert_eq!(spec.parameters["properties"]["question"]["type"], "string");
        assert_eq!(
            spec.parameters["properties"]["question"]["description"],
            "What you need to know"
        );
    }

    /// The tool parks the run with the question (trimmed): `input-required` over A2A. It is
    /// declared as one that asks, so `adam-assembly` refuses to give it to a subagent.
    #[tokio::test]
    async fn the_question_parks_the_run_and_a_blank_one_is_the_models_mistake() {
        let out = AskUser
            .call(&ctx(), json!({"question": " Which city? "}))
            .await;
        assert_eq!(out, Err(ToolError::needs_input("Which city?")));
        for args in [json!({"question": "  "}), json!({}), Value::Null] {
            let out = AskUser.call(&ctx(), args.clone()).await;
            assert!(matches!(&out, Ok(o) if o.is_error), "{args}: {out:?}");
        }
        assert!(AskUser.asks_user());
    }
}
