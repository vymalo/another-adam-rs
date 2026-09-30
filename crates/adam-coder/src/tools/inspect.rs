//! `run_command { command, cwd? }`: look around in the worktree.
//!
//! The model explores a repository with commands (`git branch -r`, `ls`, `cat README.md`,
//! `git log`). Doing that through `run_checks` made every look around a check run: it reported a
//! `checks` artifact, a non-zero exit (a missing `CLAUDE.md`) used up a check cycle, and three of
//! them failed a run that had not checked anything. `run_command` is the tool for looking:
//!
//! * it runs like `run_checks` (the worktree, a `cwd` inside it, the login shell, the same timeout,
//!   the output tail capped the same way, secrets hidden from the child);
//! * it emits **no** `checks` artifact, uses **no** check cycle, and a non-zero exit is a result,
//!   not a failure;
//! * it is **not an editing path**: the worktree is snapshotted before the command (`HEAD`, the
//!   branch, and the tree of the files as `commit_and_push` would commit them), and a command after
//!   which any of them differs is undone and refused. Changes are made by `delegate_to_opencode`, so
//!   that they are the work of the tool that commits, checks and reports them.
//!
//! A command the shell cannot find is reported as a missing toolchain, as for `run_checks` (see
//! [`missing_tool`](super::shell::missing_tool)).

use adam::prelude::*;

use super::checks::missing_tool_text;
use super::gitcli::{head_ref, head_sha, restore_worktree, working_tree_id};
use super::shell::{ShellOutcome, missing_tool, resolve_cwd, run_shell};
use super::{Outcome, ToolEnv, non_empty};

/// Look around in your worktree with a shell command: `git branch -r`, `git log --oneline`,
/// `ls`, `cat README.md`, `grep -rn name src`. You get the exit code and the tail of the output.
/// It costs no check cycle and reports no checks, and it is read-only: a command that changes
/// the worktree or HEAD is undone and refused (make changes with delegate_to_opencode). Use it
/// to explore; use run_checks only for the project's real checks.
#[tool]
pub async fn run_command(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the worktree
    command: String,
    /// Optional sub-directory of the worktree to run in (relative, inside the worktree)
    cwd: Option<String>,
) -> Outcome {
    let Some(command) = non_empty(&command) else {
        return Ok(ToolOutput::error("command is required"));
    };
    let wt = match env.worktree(ctx).await {
        Ok(wt) => wt,
        Err(outcome) => return outcome,
    };
    let dir = match resolve_cwd(wt.path(), cwd.as_deref().and_then(non_empty)) {
        Ok(dir) => dir,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };

    // What the worktree is before the command: nothing it does may change it.
    let (head, branch, tree) = (
        head_sha(wt.path()).await,
        head_ref(wt.path()).await,
        working_tree_id(wt.path()).await,
    );
    let (Some(head), Some(tree)) = (head, tree) else {
        return Ok(ToolOutput::error(
            "Cannot read the state of the worktree, so nothing was run (a command that might \
             change it cannot be undone). Try again; if it persists, tell the person.",
        ));
    };

    let redactor = &env.redactor;
    let shown = redactor.scrub(command).into_owned();
    ctx.emit_progress(format!("running: {shown}")).await;
    let mut outcome = run_shell(
        &dir,
        command,
        env.settings.check_timeout,
        env.settings.check_output_tail,
    )
    .await
    .map_err(|e| ToolError::Transient(format!("cannot start the shell: {e}")))?;
    outcome.tail = redactor.scrub_string(std::mem::take(&mut outcome.tail));

    // Did it change anything it should not have?
    let now = (
        head_sha(wt.path()).await,
        head_ref(wt.path()).await,
        working_tree_id(wt.path()).await,
    );
    let unchanged = now.0.as_deref() == Some(head.as_str())
        && now.1 == branch
        && now.2.as_deref() == Some(tree.as_str());
    if !unchanged {
        let restored = restore_worktree(wt.path(), &head, branch.as_deref(), &tree).await
            && head_sha(wt.path()).await.as_deref() == Some(head.as_str())
            && working_tree_id(wt.path()).await.as_deref() == Some(tree.as_str());
        ctx.emit_progress(format!("undid a change made by: {shown}"))
            .await;
        return Ok(ToolOutput::error(changed_the_worktree(
            &shown, &outcome, restored,
        )));
    }

    if let Some(missing) = missing_tool(&outcome) {
        ctx.emit_progress("the workspace lacks a tool".to_owned())
            .await;
        return Ok(ToolOutput::error(missing_tool_text(&missing)));
    }
    Ok(ToolOutput::text(render(&shown, &outcome)))
}

/// What the model is told when its command changed the worktree.
fn changed_the_worktree(command: &str, outcome: &ShellOutcome, restored: bool) -> String {
    let what = if restored {
        "The change was undone: the worktree is exactly as it was."
    } else {
        "The change could not be fully undone: run `git status` with run_command to see the \
         worktree before you go on."
    };
    format!(
        "`{command}` changed the worktree (files, HEAD or the branch), and run_command is for \
         looking around only. {what} Changes go through delegate_to_opencode. Output of the \
         command, for what it is worth:\n{}",
        outcome.tail
    )
}

/// The command, how it ended, and the tail of its output. A non-zero exit is said plainly: for a
/// command that only looks, it is an answer ("no such file"), not a failure.
fn render(command: &str, outcome: &ShellOutcome) -> String {
    let how = if outcome.timed_out {
        "timed out (the command was killed)".to_owned()
    } else {
        match outcome.exit_code {
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_owned(),
        }
    };
    let mut text = format!("$ {command}\n{how}\n");
    if outcome.truncated {
        text.push_str(&format!(
            "(output truncated: the last {} bytes follow)\n",
            outcome.tail.len()
        ));
    }
    text.push_str("--- output ---\n");
    text.push_str(&outcome.tail);
    if !outcome.tail.ends_with('\n') {
        text.push('\n');
    }
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
            said.contains("exactly as it was") && said.contains("delegate_to_opencode"),
            "{said}"
        );
        let unsure = changed_the_worktree("touch a", &outcome(Some(0), "", false, false), false);
        assert!(unsure.contains("could not be fully undone"), "{unsure}");
    }
}
