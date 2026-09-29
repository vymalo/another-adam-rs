//! `ask_user { question }`.

use adam::prelude::*;

use super::{Outcome, non_empty};

// Parks the run until the user answers.
//
// `ToolError::NeedsInput` makes the agent emit the question and park with no
// timer, which the A2A backend reports as `input-required`; the next message
// delivered to the task becomes this call's result.

/// Ask the person who gave you the task a question and wait for the answer.
/// Use it only when you cannot proceed without it, or to get explicit consent
/// (for example to open a pull request with failing checks). Be specific.
#[tool]
pub async fn ask_user(
    /// What you need to know
    question: String,
) -> Outcome {
    match non_empty(&question) {
        Some(question) => Err(ToolError::NeedsInput {
            question: question.to_owned(),
        }),
        None => Ok(ToolOutput::error("question is required")),
    }
}
