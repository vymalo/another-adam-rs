//! `run_command { command, cwd?, repo?, timeout? }`: look around in a place with a shell command,
//! over the commands' port ([`CommandWorkspace`]).
//!
//! It runs like the project's checks (the place, a `cwd` inside it, the login shell, the
//! workspace's time limit, the output shown as its tail or as a head and a tail and kept, secrets
//! hidden from the child), but it is **for looking**: the workspace's [guard](CommandWorkspace::guard)
//! watches the place while the command runs ([`Guarding::Look`]), and a command after which what it
//! watches differs is undone and refused. A non-zero exit is a result, not a failure. A command the
//! shell cannot find is answered by the workspace ([`CommandWorkspace::missing_tool_answer`]), when
//! it has an answer.
//!
//! It never calls [`after_run`](CommandWorkspace::after_run): a look changes nothing that follows.
//! The guard of a workspace that does not undo ([`Settled::FilesChanged`], [`Settled::Unwatched`])
//! leaves what the command changed, and the result says so when the guard saw it.

use adam_llm_agent::{ToolCtx, ToolOutput};

use crate::commands::{self, CommandWorkspace, Guarding, Settled, run_error};
use crate::find::non_empty;
use crate::output;
use crate::shell::{ShellOutcome, missing_tool, resolve_cwd, shell_spec};
use crate::workspace::{Intent, Outcome, Place};

/// The tool `run_command` over `ws`. Its description is the caller's own declaration (the coder's
/// `tools::inspect`).
///
/// In order: the command is required, the `timeout` within the limits, the place found for a read,
/// the `cwd` inside it, the session made (it may take a while, so before the place is watched), the
/// guard taken (its refusal is the answer, and nothing runs), then the command runs, its output is
/// settled, and the guard is settled: a command it undid is refused; a missing tool is the
/// workspace's answer; otherwise the result, with its repeat key.
pub async fn run_command(
    ws: &dyn CommandWorkspace,
    ctx: &ToolCtx,
    // Shell command, run with `bash -lc` (`sh -lc` without bash) in the place, in the workspace's environment
    command: String,
    // Optional sub-directory of the place to run in (relative, inside it)
    cwd: Option<String>,
    // The place to run in: its name. Leave out when the workspace has one.
    repo: Option<String>,
    // Seconds before the command is killed; at most the workspace's limit, which is also the default
    timeout: Option<i64>,
) -> Outcome {
    let Some(command) = non_empty(&command) else {
        return Ok(ToolOutput::error("command is required"));
    };
    let limits = ws.limits();
    let timeout = match output::timeout_of(&limits, timeout) {
        Ok(timeout) => timeout,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let place = match ws.place(ctx, repo.as_deref(), Intent::Read).await {
        Ok(place) => place,
        Err(outcome) => return outcome,
    };
    let dir = match resolve_cwd(place.path(), cwd.as_deref().and_then(non_empty)) {
        Ok(dir) => dir,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };

    let session = ws.session(ctx).await?;

    let guard = match ws.guard(ctx, &place, Guarding::Look).await {
        Ok(guard) => guard,
        Err(refusal) => return Ok(ToolOutput::error(noted(ws, ctx, &place, refusal))),
    };

    let redactor = ws.redactor();
    let shown = redactor.scrub(command).into_owned();
    ctx.emit_progress(format!("running: {shown}")).await;
    let mut outcome = commands::run(
        ws,
        ctx,
        &*session,
        shell_spec(&dir, command),
        timeout,
        limits.output_tail,
        true,
    )
    .await
    .map_err(|e| run_error(redactor, &e))?;
    output::settle(ws, ctx, &mut outcome).await;

    let changed_files = match guard.settle().await {
        Settled::Undone { restored } | Settled::Unreadable { restored } => {
            ctx.emit_progress(format!("undid a change made by: {shown}"))
                .await;
            let said = changed_the_worktree(&shown, &outcome, restored);
            return Ok(ToolOutput::error(noted(ws, ctx, &place, said)));
        }
        Settled::FilesChanged => true,
        Settled::Unchanged | Settled::Unwatched => false,
    };

    if let Some(missing) = missing_tool(&outcome, command)
        && let Some(said) = ws
            .missing_tool_answer(
                ctx,
                &place,
                &missing,
                &[dir.as_path(), place.path()],
                &*session,
            )
            .await?
    {
        ctx.emit_progress("the workspace lacks a tool".to_owned())
            .await;
        return Ok(ToolOutput::error(said));
    }
    let mut text = render(&shown, &outcome);
    if changed_files {
        text.push_str(KEPT_BY_THE_WORKSPACE);
        ctx.emit_progress(format!("changed files: {shown}")).await;
    }
    let mut output = ToolOutput::text(text);
    if let Some(key) = outcome.repeat_key() {
        output = output.with_repeat_key(key);
    }
    Ok(output)
}

/// What a result says when the guard saw files change and did not undo them (a workspace that
/// never undoes, and watches).
const KEPT_BY_THE_WORKSPACE: &str =
    "(the command changed files here, and this workspace does not undo them: they stay)\n";

/// A refusal of the guard, with the workspace's note.
fn noted(ws: &dyn CommandWorkspace, ctx: &ToolCtx, place: &Place, mut text: String) -> String {
    text.push_str(&ws.refusal_note(ctx, place));
    text
}

/// What the model is told when its command changed the worktree.
fn changed_the_worktree(command: &str, outcome: &ShellOutcome, restored: bool) -> String {
    let what = if restored {
        "The change was undone: the worktree is exactly as it was."
    } else {
        "The change could not be fully undone (HEAD and the branch are back, but the files \
         may differ): run `git status` with run_command to see the worktree before you go on."
    };
    format!(
        "`{command}` changed the worktree (files, HEAD, the branch, a ref or the git \
         configuration), and run_command is for looking around only. {what} To make something with a command use `run`; to change code use write_file, edit_file or apply_patch. Output of the \
         command, for what it is worth:\n{}",
        outcome.tail
    )
}

/// The command, how it ended, and the tail of its output. A non-zero exit is said plainly: for a
/// command that only looks, it is an answer ("no such file"), not a failure.
pub fn render(command: &str, outcome: &ShellOutcome) -> String {
    let how = if outcome.timed_out {
        "timed out (the command was killed)".to_owned()
    } else {
        match outcome.exit_code {
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_owned(),
        }
    };
    let mut text = format!("$ {command}\n{how}\n");
    text.push_str(&output::section(outcome));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(code: Option<i32>, tail: &str, timed_out: bool, truncated: bool) -> ShellOutcome {
        ShellOutcome {
            exit_code: code,
            timed_out,
            tail: tail.to_owned(),
            truncated,
            ..ShellOutcome::default()
        }
    }

    #[test]
    fn a_result_says_how_the_command_ended_without_calling_it_a_failure() {
        let ok = render("ls", &outcome(Some(0), "a\nb\n", false, false));
        assert_eq!(ok, "$ ls\nexit code 0\n--- output ---\na\nb\n");
        let no = render(
            "cat x",
            &outcome(Some(1), "cat: x: No such file", false, false),
        );
        assert!(no.contains("exit code 1") && !no.contains("FAILED"), "{no}");
        assert!(no.ends_with("No such file\n"), "{no}");
        let cut = render("yes", &outcome(Some(0), "y\n", false, true));
        assert!(cut.contains("output truncated: the last 2 bytes"), "{cut}");
        let slow = render("sleep 9", &outcome(None, "", true, false));
        assert!(slow.contains("timed out"), "{slow}");
        let sig = render("x", &outcome(None, "", false, false));
        assert!(sig.contains("killed by a signal"), "{sig}");
    }

    #[test]
    fn the_refusal_says_what_happened_and_where_changes_go() {
        let said = changed_the_worktree("touch a", &outcome(Some(0), "out", false, false), true);
        assert!(
            said.contains("exactly as it was") && said.contains("apply_patch"),
            "{said}"
        );
        let unsure = changed_the_worktree("touch a", &outcome(Some(0), "", false, false), false);
        assert!(unsure.contains("could not be fully undone"), "{unsure}");
    }
}
