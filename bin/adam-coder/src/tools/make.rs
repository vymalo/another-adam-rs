//! `run { command, cwd?, repo? }`: make something with a shell command, and keep it.
//!
//! The coder produces files with commands: a build, an export, a render, a generated chart. Until
//! this tool the only command that could was `run_checks`, so a model that needed `npm run render`
//! ran it as a *check*: the file appeared, and the job's gate recorded "npm run render passed" as
//! one of the project's checks. Three tools now share the shell, each for one thing:
//!
//! | Tool | For | Files the command changes | A check? |
//! |---|---|---|---|
//! | `run_command` | looking around | undone, and refused | no |
//! | `run` | making a file or changing the worktree with a command | **kept** | **no** |
//! | `run_checks` | the project's own checks | kept (they are the project's) | yes: a cycle, a `checks` artifact, the gate |
//!
//! `run` runs exactly as `run_command` does: the workspace's environment (the repository's
//! devcontainer when it has one), the login shell, a `cwd` inside the worktree, the same time limit
//! (`CHECK_TIMEOUT_SECS`) and output cap, the secrets of this process hidden from the child, and a
//! missing tool reported as a missing toolchain. It differs in what it is held to afterwards:
//!
//! * **The files are the command's to change.** What it wrote, edited or removed stays, ignored
//!   files (`dist/`, `node_modules/`) included. The result lists what `git status` shows.
//! * **Git is not.** The worktree is snapshotted before the command exactly as `run_command` does
//!   it (the git directory, the `.git` file, `HEAD`, the branch, the refs, the local configuration,
//!   the ignore rules), and a command after which any of that differs is undone **entirely**, its
//!   files included, and refused: a command that moved `HEAD` or rewrote `.git` did more than make
//!   a file, and what it made on the way cannot be told from what it broke. The model is told to use
//!   `commit_and_push` for commits.
//! * **It is never a check.** It spends no check cycle, emits no `checks` artifact and writes
//!   nothing to the run's notes, so the gate cannot see it. The files it changes change the
//!   worktree's tree, so a check that passed before no longer covers the code: `commit_and_push`
//!   and `open_pull_request` bind a verdict to a tree, and a changed tree is unchecked until
//!   `run_checks` has run on it (the same rule as for `write_file` and OpenCode's edits).
//!
//! A command that outlives its time limit is killed with its process group, and what it had
//! written by then stays.

use std::fmt::Write as _;

use adam::prelude::*;

use super::checks::missing_tool_answer;
use super::gitcli::status_text;
use super::inspect::{Snapshot, render};
use super::shell::{ShellOutcome, missing_tool, resolve_cwd, run_in, shell_spec};
use super::{Outcome, ToolEnv, non_empty, run_error};

/// Most changed files the result lists.
const MAX_LISTED: usize = 20;

/// Make something with a shell command in your worktree and keep what it changes: generate or
/// export a file (`npm run render`, `python chart.py`, `convert a.png b.jpg`), install a project's
/// dependencies, run a formatter, build an artifact. It runs in the workspace's environment (the
/// repository's own devcontainer when it has one, so its tools are there), with the same time limit
/// as the checks, and you get the exit code, the tail of the output and the files that changed.
/// Unlike run_command (for looking, it undoes changes) the files stay, and unlike run_checks it is
/// not a check: it costs no check cycle and the pull request gate never sees it. It cannot touch
/// git: a command that changes HEAD, the branch, a ref, .git or the git configuration is undone
/// completely and refused, so commit with commit_and_push. After it changes the files, run the
/// project's checks again before you commit. Show the person a file it made with share_file.
#[tool]
pub async fn run(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the worktree, in the workspace's environment
    command: String,
    /// Optional sub-directory of the worktree to run in (relative, inside the worktree)
    cwd: Option<String>,
    /// The slot to run in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    let Some(command) = non_empty(&command) else {
        return Ok(ToolOutput::error("command is required"));
    };
    // A repository's worktree or a scratch project: the command works on either.
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let dir = match resolve_cwd(slot.path(), cwd.as_deref().and_then(non_empty)) {
        Ok(dir) => dir,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };

    // Where it runs; made before the worktree is looked at, since it may take a while.
    let environment = env.session(ctx).await?;

    // What git is before the command: nothing it does may change that.
    let Some(before) = Snapshot::take(slot.path()).await else {
        return Ok(ToolOutput::error(
            "Cannot read the state of the worktree, so nothing was run (a command that might \
             change git cannot be undone). Try again; if it persists, tell the person.",
        ));
    };

    let redactor = &env.redactor;
    let shown = redactor.scrub(command).into_owned();
    ctx.emit_progress(format!("running: {shown}")).await;
    let mut outcome = run_in(
        &*environment,
        shell_spec(&dir, command),
        env.settings.check_timeout,
        env.settings.check_output_tail,
    )
    .await
    .map_err(|e| run_error(redactor, &e))?;
    outcome.tail = redactor.scrub_string(std::mem::take(&mut outcome.tail));

    // Did it touch git? The files are its to change; HEAD, the branch, the refs, the
    // configuration and `.git` are not.
    let after = Snapshot::take(slot.path()).await;
    let Some(after) = after.filter(|after| before.same_git_as(after)) else {
        let restored = before.restore(&slot).await;
        ctx.emit_progress(format!("undid a change to git made by: {shown}"))
            .await;
        return Ok(ToolOutput::error(touched_git(&shown, &outcome, restored)));
    };

    if let Some(missing) = missing_tool(&outcome, command) {
        ctx.emit_progress("the workspace lacks a tool".to_owned())
            .await;
        let said = missing_tool_answer(
            &env,
            ctx,
            &missing,
            &[dir.as_path(), slot.path()],
            &environment.describe().kind,
        )
        .await?;
        return Ok(ToolOutput::error(said));
    }

    let changed = after.files_differ_from(&before);
    let mut text = render(&shown, &outcome);
    if changed {
        text.push_str(&changes_note(status_text(slot.path()).await.as_deref()));
        super::files::push_published_note(&mut text, &slot);
    } else {
        text.push_str(
            "(git sees no change in the worktree; files the project ignores are kept too, and \
             are not listed)\n",
        );
    }
    ctx.emit_progress(if changed {
        format!("changed files: {shown}")
    } else {
        format!("ran: {shown}")
    })
    .await;
    Ok(ToolOutput::text(text))
}

/// What the model is told when its command changed git (and everything was put back).
fn touched_git(command: &str, outcome: &ShellOutcome, restored: bool) -> String {
    let what = if restored {
        "Everything it did was undone, its files included: the worktree is exactly as it was."
    } else {
        "It could not be fully undone (HEAD and the branch are back, but the files may differ): \
         run `git status` with run_command to see the worktree before you go on."
    };
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
            "(nothing that git lists: the files it changed are ignored by the project)\n",
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
        };
        let said = touched_git("git commit -am x", &outcome, true);
        assert!(
            said.contains("files included") && said.contains("commit_and_push"),
            "{said}"
        );
        let unsure = touched_git("git commit -am x", &outcome, false);
        assert!(unsure.contains("could not be fully undone"), "{unsure}");
    }
}
