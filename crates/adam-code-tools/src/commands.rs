//! The port the command tools work through: [`CommandWorkspace`] ([ADR 0047], decision 7), and the
//! plumbing that `run_command` and `run` share: running a command for a tool ([`run`]) and the tool
//! errors of a command that did not run ([`environment_error`], [`run_error`]).
//!
//! `CommandWorkspace` extends [`CodeWorkspace`]: a command finds its place with the same
//! [`place`](CodeWorkspace::place) as a file tool. What it adds is where a command runs (a
//! [session](CommandWorkspace::session)), its [limits](CommandWorkspace::limits), where a long
//! output is [kept](CommandWorkspace::kept) ([`output::settle`](crate::output::settle)), and the
//! [guard](CommandWorkspace::guard) of a command: what it changed and, where the workspace owns the
//! place, its undo. The person's directory owns nothing, so its guard watches nothing
//! ([`Unwatched`]) and undoes nothing.
//!
//! `guard` and [`after_run`](CommandWorkspace::after_run) are **required** methods (step 5c-1): a
//! breaking change for every implementation of the trait (the coder's and the testkit's
//! `DirWorkspace`, the only ones then).
//!
//! [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use adam_error::{Classify as _, report};
use adam_llm_agent::{KeptOutputs, ToolCtx, ToolError};
use adam_workspace::{EnvError, EnvSession, ExecSpec};
use async_trait::async_trait;

use crate::governor::Governor;
use crate::output::OutputDefaults;
use crate::redact::Redactor;
use crate::sandbox::Meter;
use crate::shell::{self, Governed, MissingTool, RunError, ShellOutcome};
use crate::workspace::{CodeWorkspace, Place};

/// The bounds of one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandLimits {
    /// The time limit of a command that asks for none, and the most one may ask for.
    pub timeout: Duration,
    /// The bytes of output a result shows: a longer output is shown as its head and tail and kept.
    pub output_tail: usize,
    /// What a result shows of the output when the call gives no `output`, per tool
    /// ([`OutputDefaults::TOOLS`] unless the workspace was given others; ADR 0088 slice T3).
    pub output: OutputDefaults,
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

    /// The process pool and the OS limits the commands of this workspace run under (ADR 0065):
    /// `None` (the default) is a workspace whose commands take no seat and are not confined. A new
    /// method with a default, so no implementation breaks.
    fn governor(&self) -> Option<&Governor> {
        None
    }

    /// Where a long output is kept, for `read_output`.
    ///
    /// The keeper scrubs what it keeps: an output longer than what is captured reaches it as it was
    /// printed ([`output::settle`](crate::output::settle)), so a keeper that does not scrub stores
    /// secrets. Run `adam_llm_agent::kept_outputs_conformance` over it; the command cases here
    /// do not reach that path.
    fn kept(&self) -> &dyn KeptOutputs;

    /// The guard of one command in `place`, taken before it runs and [settled](CommandGuard::settle)
    /// after it: what the command changed, held to `guarding`.
    ///
    /// **Required.** `Err` is the refusal the model is told, and nothing runs: the place cannot be
    /// watched, so what a command changes there could not be undone. The tool appends
    /// [`refusal_note`](Self::refusal_note) to it. A workspace that watches nothing returns
    /// [`Unwatched`].
    async fn guard(
        &self,
        ctx: &ToolCtx,
        place: &Place,
        guarding: Guarding,
    ) -> Result<Box<dyn CommandGuard>, String>;

    /// After a command that makes things (`run`) ran in `place`, before its guard is settled: what
    /// the workspace does. Returns text the tool appends to its result when the command changed
    /// files: empty, or starting with '\n'. `run_command`, which only looks, never calls it.
    ///
    /// **Required**, as [`after_write`](CodeWorkspace::after_write) is. The coder moves its run's
    /// check epoch (a check may read a file the command made, ignored ones included) and, in a
    /// scratch project that was published, says that the change does not reach the repository.
    ///
    /// # Errors
    ///
    /// A [`ToolError`] when what follows cannot be done or recorded: the tool returns it, once the
    /// guard is settled ([`make::run`](crate::make::run)), so a command that changed git is undone
    /// all the same (for the coder, its files with it); one that only changed files keeps them.
    async fn after_run(&self, ctx: &ToolCtx, place: &Place) -> Result<String, ToolError>;

    /// A note the tool appends to a refusal of its guard (one from [`guard`](Self::guard), or a
    /// command [undone](Settled::Undone)): empty (the default), or starting with '\n'. The coder
    /// names the commands that keep running in the slot, which write in it and are not undone.
    fn refusal_note(&self, _ctx: &ToolCtx, _place: &Place) -> String {
        String::new()
    }

    /// What the model is told of a command the shell could not find (`missing`), run in the first of
    /// `dirs` within `place` (the second is the place's root), in `session`. `None` (the default):
    /// the tool answers as for any other command, with what it printed.
    ///
    /// # Errors
    ///
    /// A [`ToolError`] when the answer cannot be made or recorded.
    async fn missing_tool_answer(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        _missing: &MissingTool,
        _dirs: &[&Path],
        _session: &dyn EnvSession,
    ) -> Result<Option<String>, ToolError> {
        Ok(None)
    }
}

/// What a command is held to by its [guard](CommandWorkspace::guard).
///
/// Closed on purpose (rule 3): a new kind of command must be thought through by every workspace,
/// so it fails to compile in each until it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guarding {
    /// It looks (`run_command`): it may change nothing, files included.
    Look,
    /// It makes things (`run`): it may change files, and nothing of git.
    Make,
}

/// What a [guard](CommandGuard) found once the command ended.
///
/// `#[non_exhaustive]`, unlike [`Guarding`]: a workspace may come to report more (a guard that
/// records what changed in a person's directory without undoing it, slice 7), and the tools give
/// any verdict they do not know the answer of [`Unwatched`](Settled::Unwatched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Settled {
    /// Nothing the guard watches changed.
    Unchanged,
    /// Files changed, as a [`Make`](Guarding::Make) may, and they stay.
    FilesChanged,
    /// The command changed what it was held to leave alone, and it was undone; `restored`: the
    /// place is exactly as it was.
    Undone {
        /// The place is exactly as it was.
        restored: bool,
    },
    /// The place could not be read after the command, so whether it changed is not known: it was
    /// undone, to be safe; `restored`: the place is exactly as it was.
    Unreadable {
        /// The place is exactly as it was.
        restored: bool,
    },
    /// The guard watches nothing ([`Unwatched`]): nothing is known, nothing was undone.
    Unwatched,
}

/// The guard of one command: taken before it runs, settled once after.
///
/// A guard dropped without being settled **undoes nothing**: a tool that returns between the two
/// (the command did not run, or a hook failed) leaves the place as the command left it.
#[async_trait]
pub trait CommandGuard: Send {
    /// Compare the place with what it was, and undo what the command was not allowed to change.
    async fn settle(self: Box<Self>) -> Settled;
}

/// A guard that watches nothing: what a workspace that never undoes gives (the person's own
/// directory, the testkit's `DirWorkspace`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Unwatched;

#[async_trait]
impl CommandGuard for Unwatched {
    async fn settle(self: Box<Self>) -> Settled {
        Settled::Unwatched
    }
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
    let root = ctx.root_run_id().to_string();
    let governed = ws.governor().map(|governor| Governed {
        governor,
        conversation: &root,
    });
    let (outcome, ran) = if keep {
        shell::run_captured_governed(session, spec, timeout, tail_cap, margin, &cancel, governed)
            .await
    } else {
        shell::run_timed_governed(session, spec, timeout, tail_cap, margin, &cancel, governed).await
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
        RunError::Busy(refused) => ToolError::from_classified(refused),
        RunError::Confine(e) => ToolError::from_classified(e),
    }
}
