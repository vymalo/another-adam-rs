//! What the model is shown of a command's output, and the `timeout` a call may ask for:
//! [`adam_code_tools::output`] over the coder's [`ToolEnv`].

use std::time::Duration;

use adam::prelude::*;

use super::ToolEnv;
use super::shell::ShellOutcome;
use adam_code_tools::CommandWorkspace as _;

pub(super) use adam_code_tools::output::{escaped, head_of, section, tail_of};

/// [`adam_code_tools::output::timeout_of`] under the deployment's limit (`CHECK_TIMEOUT_SECS`).
pub(super) fn timeout_of(env: &ToolEnv, asked: Option<i64>) -> Result<Duration, String> {
    adam_code_tools::output::timeout_of(&env.limits(), asked)
}

/// [`adam_code_tools::output::settle`]: the output kept in the coder's files ([`ToolEnv::kept`]).
pub(super) async fn settle(env: &ToolEnv, ctx: &ToolCtx, outcome: &mut ShellOutcome) {
    adam_code_tools::output::settle(env, ctx, outcome).await;
}
