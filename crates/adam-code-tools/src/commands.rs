//! The port the command tools work through: [`CommandWorkspace`] ([ADR 0047], decision 7), and the
//! plumbing that `run_command` and `run` share: running a command for a tool ([`run`]) and the tool
//! errors of a command that did not run ([`environment_error`], [`run_error`]).
//!
//! `CommandWorkspace` extends [`CodeWorkspace`]: a command finds its place with the same
//! [`place`](CodeWorkspace::place) as a file tool. What it adds is where a command runs (a
//! [session](CommandWorkspace::session)), its [limits](CommandWorkspace::limits) and where a long
//! output is [kept](CommandWorkspace::kept) ([`output::settle`](crate::output::settle)).
//!
//! The trait is **additive**: an implementation of `CodeWorkspace` alone is unchanged. Step 5c adds
//! the guard (what a command changed, and its undo, which the person's directory never does) and
//! moves the tools here; its methods grow then.
//!
//! [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md

use std::sync::Arc;
use std::time::Duration;

use adam_error::{Classify as _, report};
use adam_llm_agent::{KeptOutputs, ToolCtx, ToolError};
use adam_workspace::{EnvError, EnvSession, ExecSpec};
use async_trait::async_trait;

use crate::redact::Redactor;
use crate::sandbox::Meter;
use crate::shell::{self, RunError, ShellOutcome};
use crate::workspace::CodeWorkspace;

/// The bounds of one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandLimits {
    /// The time limit of a command that asks for none, and the most one may ask for.
    pub timeout: Duration,
    /// The bytes of output a result shows: a longer output is shown as its head and tail and kept.
    pub output_tail: usize,
}

/// The workspace of the command tools: a [`CodeWorkspace`] that can run a command.
#[async_trait]
pub trait CommandWorkspace: CodeWorkspace {
    /// Where a command runs: the session of the run's environment, made on the first need. The
    /// implementation reports the session to the call's usage itself ([`Meter::report_session`]).
    ///
    /// # Errors
    ///
    /// A [`ToolError`] when no session can be had (the environment cannot be made, the run was
    /// cancelled while it waited).
    async fn session(&self, ctx: &ToolCtx) -> Result<Arc<dyn EnvSession>, ToolError>;

    /// The bounds of a command.
    fn limits(&self) -> CommandLimits;

    /// Where a long output is kept, for `read_output`.
    ///
    /// The keeper scrubs what it keeps: an output longer than what is captured reaches it as it was
    /// printed ([`output::settle`](crate::output::settle)), so a keeper that does not scrub stores
    /// secrets. Run `adam_llm_agent::kept_outputs_conformance` over it; the command cases here
    /// do not reach that path.
    fn kept(&self) -> &dyn KeptOutputs;
}

/// A shared [`CommandWorkspace`].
pub type DynCommandWorkspace = Arc<dyn CommandWorkspace>;

/// [`run_in`](shell::run_in) for a tool (`keep`: the output is captured for
/// [`output::settle`](crate::output::settle), which the caller then calls): the command runs in
/// `session`, stopped when the run is cancelled, and in a pod the time it ran is reported as the
/// call's sandbox usage whatever its end was (it exited, or a timeout or a cancel stopped it):
/// ADR 0043.
///
/// # Errors
///
/// A [`RunError`] when the command did not run ([`run_error`] makes it the tool's error).
pub async fn run(
    ws: &dyn CommandWorkspace,
    ctx: &ToolCtx,
    session: &dyn EnvSession,
    spec: ExecSpec,
    timeout: Duration,
    tail_cap: usize,
    keep: bool,
) -> Result<ShellOutcome, RunError> {
    let cancel = ctx.cancel_token();
    let margin = ws.redactor().margin();
    let (outcome, ran) = if keep {
        shell::run_captured(session, spec, timeout, tail_cap, margin, &cancel).await
    } else {
        shell::run_timed(session, spec, timeout, tail_cap, margin, &cancel).await
    };
    // A call that keeps nothing shows the tail of a long output: scrubbed here, with the bytes
    // before it, so that a secret cut by the tail's start leaves no part of itself.
    let outcome = outcome.map(|mut outcome| {
        if !keep && !outcome.tail_raw.is_empty() {
            let bytes = std::mem::take(&mut outcome.tail_raw);
            let reaches_the_start = outcome.total <= bytes.len() as u64;
            let raw = String::from_utf8_lossy(&bytes).into_owned();
            let redactor = ws.redactor();
            let scrub = |t: &str| redactor.scrub(t).into_owned();
            // What was held reaches back to the start of the output: nothing was cut there,
            // so it is scrubbed as it is; else its start is an edge, cut back.
            let scrubbed = if reaches_the_start {
                scrub(&raw)
            } else {
                adam_llm_agent::scrub_open_start(&raw, scrub, margin)
            };
            let mut start = scrubbed.len().saturating_sub(tail_cap.max(1));
            while !scrubbed.is_char_boundary(start) {
                start += 1;
            }
            outcome.tail = scrubbed[start..].to_owned();
        }
        outcome
    });
    if let (Some(meter), Some(ran)) = (Meter::of(session), ran) {
        meter.report_command(ctx, ran);
    }
    outcome
}

/// What a tool fails with when the run was cancelled under (or before) it.
///
/// The run is already `Failed` (`cancelled: ...`) by then and whatever the call returns is
/// dropped, but the loop that called the tool goes on to the next call of the same model turn: the
/// tools with effects outside the worktree (the coder's `publish`) therefore check
/// [`ToolCtx::is_cancelled`] first and refuse with this.
pub fn cancelled(what: &str) -> ToolError {
    ToolError::Permanent(format!("cancelled: the run was cancelled; {what}"))
}

/// An environment failure as a tool error: worth retrying (it is not available now, too slow, lost),
/// or a report to the model (the repository's configuration is wrong, the build failed). The end of
/// a failed build's output is in the report.
pub fn environment_error(redactor: &Redactor, e: &EnvError) -> ToolError {
    // Journaled and shown to the model: a boundary, so the chain is flattened here, once.
    let mut text = format!("the work environment: {}", report(e));
    if let EnvError::Build { log_tail, .. } = e
        && !log_tail.trim().is_empty()
    {
        text.push('\n');
        text.push_str(log_tail.trim_end());
    }
    let text = redactor.scrub(&text).into_owned();
    if e.is_retryable() {
        ToolError::Transient(text)
    } else {
        ToolError::Permanent(text)
    }
}

/// A command that did not run, as a tool error.
pub fn run_error(redactor: &Redactor, e: &RunError) -> ToolError {
    match e {
        RunError::Prepare(e) => environment_error(redactor, e),
        RunError::Spawn(e) => ToolError::Transient(format!("cannot start the shell: {e}")),
        RunError::Cancelled => cancelled("the command was stopped"),
    }
}
