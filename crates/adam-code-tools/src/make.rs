//! `run { command, cwd?, repo?, timeout? }`: make something with a shell command, and keep it, over
//! the commands' port ([`CommandWorkspace`]).
//!
//! It runs exactly as [`run_command`](crate::inspect::run_command) does (the place, a `cwd` inside
//! it, the login shell, the workspace's time limit, the output shown as its tail or as a head and a
//! tail and kept, secrets hidden from the child), but **the files are the command's to change**: it
//! takes a place for a write, and its guard holds it to [`Guarding::Make`] (files may change, git may
//! not). A command after which the guard finds git changed, or cannot read it, is undone entirely and
//! refused. After the command ran, and **before** the guard is settled, the workspace's
//! [`after_run`](CommandWorkspace::after_run) follows (the coder moves its check epoch, even for a
//! command its guard then undoes); its text is appended to a result whose files changed. It is never
//! a check. When `after_run` fails, the guard is settled first, so a change the command made to
//! git is undone, and then its error is the tool's.

use std::fmt::Write as _;

use adam_llm_agent::{ToolCtx, ToolOutput};

use crate::commands::{self, CommandWorkspace, Guarding, Settled, run_error};
use crate::find::non_empty;
use crate::git::status_text;
use crate::inspect::{noted, render};
use crate::output;
use crate::shell::{ShellOutcome, missing_tool, resolve_cwd, shell_spec};
use crate::workspace::{Intent, Outcome};

/// Most changed files the result lists.
pub const MAX_LISTED: usize = 20;

/// The tool `run` over `ws`. Its description is the caller's own declaration (the coder's
/// `tools::make`).
///
/// In order: the command is required, the `timeout` within the limits, the place found for a write,
/// the `cwd` inside it, the session made (it may take a while, so before the place is watched), the
/// guard taken for a [`Make`](Guarding::Make) (its refusal is the answer, and nothing runs), then the
/// command runs, its output is settled, [`after_run`](CommandWorkspace::after_run) follows, and the
/// guard is settled: a command it undid, or after which git could not be read, is refused; a missing
/// tool is the workspace's answer; otherwise the result, with what `git status` lists when files
/// changed and the text of `after_run`, and its repeat key.
pub async fn run(
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
    // A repository's worktree or a scratch project: the command works on either.
    let place = match ws.place(ctx, repo.as_deref(), Intent::Write).await {
        Ok(place) => place,
        Err(outcome) => return outcome,
    };
    let dir = match resolve_cwd(place.path(), cwd.as_deref().and_then(non_empty)) {
        Ok(dir) => dir,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };

    // Where it runs; made before the place is looked at, since it may take a while.
    let session = ws.session(ctx).await?;

    // What git is before the command: nothing it does may change that.
    let guard = match ws.guard(ctx, &place, Guarding::Make).await {
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

    // The command ran: what follows it is done now, before git is checked, so it is done for a
    // command the guard then undoes too. On an error the guard is settled all the same, so git is
    // never left as the command changed it.
    let published = match ws.after_run(ctx, &place).await {
        Ok(text) => text,
        Err(e) => {
            let _ = guard.settle().await;
            return Err(e);
        }
    };

    // Did it touch git? The files are its to change; HEAD, the branch, the refs, the
    // configuration and `.git` are not. A place whose git cannot be read after the command is not
    // known to be untouched, so it is undone as one that changed git would be; the model is told
    // which of the two it was.
    let changed = match guard.settle().await {
        Settled::Unreadable { restored } => {
            ctx.emit_progress(format!("could not check git after: {shown}; undid it"))
                .await;
            let said = unverified(&shown, &outcome, restored);
            return Ok(ToolOutput::error(noted(ws, ctx, &place, said)));
        }
        Settled::Undone { restored } => {
            ctx.emit_progress(format!("undid a change to git made by: {shown}"))
                .await;
            let said = touched_git(&shown, &outcome, restored);
            return Ok(ToolOutput::error(noted(ws, ctx, &place, said)));
        }
        Settled::FilesChanged => Some(true),
        Settled::Unchanged => Some(false),
        // Nothing was watched, so nothing is said of the files.
        Settled::Unwatched => None,
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
    match changed {
        Some(true) => {
            text.push_str(&changes_note(status_text(place.git()).await.as_deref()));
            text.push_str(&published);
        }
        Some(false) => text.push_str(
            "(git sees no change in the worktree; files the project ignores are kept too, and \
             are not listed)\n",
        ),
        None => {}
    }
    ctx.emit_progress(if changed == Some(true) {
        format!("changed files: {shown}")
    } else {
        format!("ran: {shown}")
    })
    .await;
    let mut output = ToolOutput::text(text);
    if let Some(key) = outcome.repeat_key() {
        output = output.with_repeat_key(key);
    }
    Ok(output)
}

/// What the model is told of the undoing: whether the worktree is as it was.
fn undone(restored: bool) -> &'static str {
    if restored {
        "Everything it did was undone, its files included: the worktree is exactly as it was."
    } else {
        "It could not be fully undone (HEAD and the branch are back, but the files may differ): \
         run `git status` with run_command to see the worktree before you go on."
    }
}

/// What the model is told when the state of git could not be read after its command, so that
/// nobody can say whether it changed git (and everything was put back, to be safe).
fn unverified(command: &str, outcome: &ShellOutcome, restored: bool) -> String {
    format!(
        "After `{command}` the state of the worktree's git could not be read, so it is not known \
         whether the command changed git; to be safe it was treated as if it had, since `run` \
         may change files and nothing of git. {} If the command touches git (HEAD, the branch, \
         a ref, `.git` or the git configuration), commit with commit_and_push and run it again \
         without the git part; if it does not, try it again, and if this persists, tell the \
         person. Output of the command, for what it is worth:\n{}",
        undone(restored),
        outcome.tail
    )
}

/// What the model is told when its command changed git (and everything was put back).
fn touched_git(command: &str, outcome: &ShellOutcome, restored: bool) -> String {
    let what = undone(restored);
    format!(
        "`{command}` changed git (HEAD, the branch, a ref, `.git` or the git configuration), and \
         `run` may change files and nothing of git. {what} Commit with commit_and_push, and \
         run the command again without the git part. Output of the command, for what it is \
         worth:\n{}",
        outcome.tail
    )
}

/// The files `git status` (`--porcelain=v1 -z`, every untracked file) lists, as a note for the
/// model: the first [`MAX_LISTED`], then how many more. Files the project ignores are kept too, and
/// are not listed.
///
/// Changed files with nothing listed are ignored ones, or ones the command put back as `HEAD` has
/// them (an edit of the run's undone): the note names both.
fn changes_note(status: Option<&str>) -> String {
    let mut files = Vec::new();
    let mut entries = status.unwrap_or_default().split('\0');
    while let Some(entry) = entries.next() {
        if entry.len() < 3 {
            continue;
        }
        files.push(entry);
        // A rename or a copy is followed by the path it came from.
        if matches!(entry.as_bytes()[0], b'R' | b'C') {
            entries.next();
        }
    }
    let mut note = String::from(
        "--- the worktree now differs from HEAD in (git status; files the project ignores are kept \
         too) ---\n",
    );
    if files.is_empty() {
        note.push_str(
            "(nothing that git lists: the files it changed are ignored by the project, or are \
             back as HEAD has them)\n",
        );
    }
    for entry in files.iter().take(MAX_LISTED) {
        let _ = writeln!(note, "{entry}");
    }
    if files.len() > MAX_LISTED {
        let _ = writeln!(note, "and {} more", files.len() - MAX_LISTED);
    }
    note.push_str("The files changed, so run the project's checks again before you commit.\n");
    note
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_changes_are_listed_from_git_status_with_renames_counted_once() {
        let status = "?? out/chart.svg\0 M src/a.rs\0R  new.rs\0old.rs\0";
        let note = changes_note(Some(status));
        assert!(note.contains("?? out/chart.svg"), "{note}");
        assert!(note.contains(" M src/a.rs"), "{note}");
        assert!(note.contains("R  new.rs"), "{note}");
        assert!(
            !note.contains("old.rs\n"),
            "the source of a rename is not a line of its own: {note}"
        );
        assert!(note.contains("run the project's checks again"), "{note}");
    }

    #[test]
    fn many_changes_are_cut_and_counted_and_no_listing_is_said() {
        let status: String = (0..25).map(|i| format!("?? f{i}\0")).collect();
        let note = changes_note(Some(&status));
        assert!(
            note.contains("?? f19\n") && !note.contains("?? f20\n"),
            "{note}"
        );
        assert!(note.contains("and 5 more"), "{note}");
        let ignored = changes_note(Some(""));
        assert!(ignored.contains("ignored by the project"), "{ignored}");
    }

    #[test]
    fn a_refusal_says_everything_was_undone_and_where_commits_go() {
        let outcome = ShellOutcome {
            exit_code: Some(0),
            timed_out: false,
            tail: "out".into(),
            truncated: false,
            ..ShellOutcome::default()
        };
        let said = touched_git("git commit -am x", &outcome, true);
        assert!(
            said.contains("files included") && said.contains("commit_and_push"),
            "{said}"
        );
        let unsure = touched_git("git commit -am x", &outcome, false);
        assert!(unsure.contains("could not be fully undone"), "{unsure}");
    }

    #[test]
    fn a_worktree_that_cannot_be_read_after_is_not_said_to_have_changed_git() {
        let outcome = ShellOutcome {
            exit_code: Some(0),
            timed_out: false,
            tail: "out".into(),
            truncated: false,
            ..ShellOutcome::default()
        };
        let said = unverified("npm run render", &outcome, true);
        assert!(
            said.contains("could not be read")
                && said.contains("not known")
                && said.contains("files included")
                && !said.contains("changed git ("),
            "{said}"
        );
        let unsure = unverified("npm run render", &outcome, false);
        assert!(unsure.contains("could not be fully undone"), "{unsure}");
    }
}
