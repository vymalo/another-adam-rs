//! What the model is shown of a command's output, and the `timeout` and `output` a call may ask
//! for: [`adam_code_tools::output`] over the coder's [`ToolEnv`].

use std::time::Duration;

use adam::prelude::*;

use super::ToolEnv;
use super::shell::ShellOutcome;
use adam_code_tools::CommandWorkspace as _;

pub(super) use adam_code_tools::output::{
    OutputMode, escaped, head_of, section, section_as, tail_of,
};

/// [`adam_code_tools::output::timeout_of`] under the deployment's limit (`CHECK_TIMEOUT_SECS`).
pub(super) fn timeout_of(env: &ToolEnv, asked: Option<i64>) -> Result<Duration, String> {
    adam_code_tools::output::timeout_of(&env.limits(), asked)
}

/// [`adam_code_tools::output::settle_as`]: the output kept in the coder's files ([`ToolEnv::kept`]),
/// a short one too when `mode` leaves it out of the result.
pub(super) async fn settle_as(
    env: &ToolEnv,
    ctx: &ToolCtx,
    outcome: &mut ShellOutcome,
    mode: OutputMode,
) {
    adam_code_tools::output::settle_as(env, ctx, outcome, mode).await;
}
