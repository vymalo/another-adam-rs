//! `ask_user { question }`.

use adam_llm_agent::{Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Outcome, str_arg};

/// Parks the run until the user answers.
///
/// `ToolError::NeedsInput` makes the agent emit the question and park with no
/// timer, which the A2A backend reports as `input-required`; the next message
/// delivered to the task becomes this call's result.
pub struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the person who gave you the task a question and wait for the answer. \
                          Use it only when you cannot proceed without it, or to get explicit consent \
                          (for example to open a pull request with failing checks). Be specific."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "What you need to know" }
                },
                "required": ["question"]
            }),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Outcome {
        match str_arg(&args, "question") {
            Some(question) => Err(ToolError::NeedsInput {
                question: question.to_owned(),
            }),
            None => Ok(ToolOutput::error("question is required")),
        }
    }
}
