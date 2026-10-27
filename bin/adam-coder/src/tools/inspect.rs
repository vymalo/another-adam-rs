//! `run_command { command, cwd?, repo? }`: look around in the worktree. (`run`, in [`make`](super::make),
//! is its sibling for making things: it keeps what the command changes.)
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
//!   branch, the tree of the files as `commit_and_push` would commit them, and the refs and local
//!   git configuration a command could change without touching a file), and a command after
//!   which any of them differs is undone and refused. Changes are made by the file tools, by
//!   `delegate_to_opencode` and, for a command that makes something (a build, an export, a
//!   generated file), by `run` ([`run`](super::make)), which keeps the files it changes and is held
//!   to the same guard for everything else.
//!
//! A command the shell cannot find is reported as a missing toolchain, as for `run_checks` (see
//! [`missing_tool`]).

use adam::prelude::*;

use super::checks::missing_tool_answer;
use super::gitcli::{
    GitAt, RepoState, git_stdout, head_ref, head_sha, point_head, repo_state, restore_repo_state,
    restore_worktree, status_text, working_tree_id,
};
use super::shell::{ShellOutcome, missing_tool, resolve_cwd, run_in, shell_spec};
use super::{Outcome, ToolEnv, non_empty, run_error};

/// Look around in your worktree with a shell command: `git branch -r`, `git log --oneline`,
/// `ls`, `cat README.md`, `grep -rn name src`. It runs in the workspace's environment (the
/// repository's own devcontainer when it has one, so its tools are there). You get the exit code and
/// the tail of the output.
/// It costs no check cycle and reports no checks, and it is for looking: changes it makes to
/// HEAD, the branch and the working tree are undone and refused (to make a file or change the
/// worktree with a command, use run; to change code, write_file, edit_file, apply_patch or
/// delegate_to_opencode). Use it to look; use run_checks only for the project's real checks.
#[tool(label = "Run a command", icon = "execute")]
pub async fn run_command(
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
    // A repository's worktree or a scratch project: both are looked at the same way.
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

    // What the worktree is before the command: nothing it does may change it.
    let Some(before) = Snapshot::of_slot(&slot).await else {
        return Ok(ToolOutput::error(
            "Cannot read the state of the worktree, so nothing was run (a command that might \
             change it cannot be undone). Try again; if it persists, tell the person.",
        ));
    };
    if let Some(refusal) = before.refusal() {
        return Ok(ToolOutput::error(refusal));
    }

    let redactor = &env.redactor;
    let shown = redactor.scrub(command).into_owned();
    ctx.emit_progress(format!("running: {shown}")).await;
    let mut outcome = run_in(
        &*environment,
        shell_spec(&dir, command),
        env.settings.check_timeout,
        env.settings.check_output_tail,
        &ctx.cancel_token(),
    )
    .await
    .map_err(|e| run_error(redactor, &e))?;
    outcome.tail = redactor.scrub_string(std::mem::take(&mut outcome.tail));

    // Did it change anything it should not have?
    let after = Snapshot::of_slot(&slot).await;
    if after.as_ref() != Some(&before) {
        let restored = before.restore(&slot).await;
        ctx.emit_progress(format!("undid a change made by: {shown}"))
            .await;
        return Ok(ToolOutput::error(changed_the_worktree(
            &shown, &outcome, restored,
        )));
    }

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
    Ok(ToolOutput::text(render(&shown, &outcome)))
}

/// What a command must leave as it found it: `HEAD` and the branch, the files of the worktree,
/// and the shared repository state a command could change without touching a file (see
/// [`RepoState`]).
///
/// The files are the **tree** as `commit_and_push` would commit it, which is what a restore can
/// write back. Where that cannot be computed (an embedded repository without a commit makes `git
/// add -A` fail), the snapshot falls back to what `git status` says: a change is still seen, and a
/// restore then only puts `HEAD` and the branch back and leaves the files alone, since nothing it
/// could write back would be exact.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Snapshot {
    /// The git directory of this worktree: a command that replaced the `.git` file would point
    /// every later git call somewhere else, and nothing may be restored through it.
    git_dir: String,
    /// What the `.git` file of the worktree says (it points at `git_dir`); a command that rewrote
    /// it is put right before anything else, since git would act on wherever it points now.
    dot_git: Option<String>,
    head: String,
    /// What `HEAD` names (`refs/heads/...`), `None` when it is detached. **Compared**, so that a
    /// command that moves `HEAD` is seen; for a worktree of a repository it is not what a restore
    /// moves ([`own_ref`](Self::own_ref) is).
    branch: Option<String>,
    /// For a worktree of a repository: the run's own branch (`agent/...`), from the coder's metadata
    /// and never from `HEAD`. `HEAD` is a file in the worktree's directory in the mirror, which the
    /// run's commands can write: one that names another run's branch must not make that branch what
    /// a restore resets. `None` for a scratch project, whose repository is its own.
    own_branch: Option<String>,
    /// `HEAD` was not on `refs/heads/<own_branch>` when the snapshot was taken, so what it holds
    /// (`head`, the tree) is not the run's branch's and nothing can be restored from it.
    head_is_foreign: bool,
    tree: Option<String>,
    /// `git status`, for the worktree that has no tree id.
    status: Option<String>,
    repo: RepoState,
}

impl Snapshot {
    /// The state of `slot`'s worktree, with the run's own branch from the coder's metadata.
    pub(super) async fn of_slot(slot: &adam_workspace::Slot) -> Option<Self> {
        Self::take(
            &GitAt::slot(slot),
            slot.worktree().map(adam_workspace::Worktree::local_branch),
        )
        .await
    }

    /// The state of the worktree `at`. `own` is the branch the run's metadata says the worktree is on
    /// (a repository's worktree; `None` for a scratch project).
    pub(super) async fn take(at: &GitAt, own: Option<&str>) -> Option<Self> {
        let git_dir = git_stdout(at, &["rev-parse", "--absolute-git-dir"]).await?;
        // The file is in the slot, which a command can have replaced: read as a regular file and
        // not through a link.
        let dot_git = adam_workspace::safe_fs::read_regular_text(
            &at.path().join(".git"),
            adam_workspace::safe_fs::MAX_SMALL_FILE,
        )
        .await
        .ok();
        let branch = head_ref(at).await;
        let head = head_sha(at).await?;
        let head_is_foreign =
            own.is_some_and(|own| branch.as_deref() != Some(format!("refs/heads/{own}").as_str()));
        let tree = working_tree_id(at).await;
        let status = match tree {
            Some(_) => None,
            None => Some(status_text(at).await?),
        };
        Some(Self {
            git_dir,
            dot_git,
            head,
            branch,
            own_branch: own.map(str::to_owned),
            head_is_foreign,
            tree,
            status,
            repo: repo_state(at).await?,
        })
    }

    /// Why nothing may be run in this worktree, when `HEAD` is not on the run's own branch: a
    /// command that changes git could not be undone, since the commit `HEAD` is at is not the run's
    /// (a restore would reset whatever branch `HEAD` names).
    pub(super) fn refusal(&self) -> Option<String> {
        if !self.head_is_foreign {
            return None;
        }
        let own = self.own_branch.as_deref().unwrap_or("its branch");
        let now = self
            .branch
            .as_deref()
            .map_or_else(|| "no branch (it is detached)".to_owned(), str::to_owned);
        Some(format!(
            "HEAD of this worktree is on {now}, not on the run's own branch {own}: something moved it \
             (it is a file that commands of the run can write). Nothing was run, because a command \
             that changes git could not be undone here. Do not try to move HEAD back with git; tell \
             the person."
        ))
    }

    /// Whether everything but the files is as `other` has it: the git directory, the `.git` file,
    /// `HEAD`, the branch, and the refs, configuration and ignore rules of the repository. What
    /// `run` holds a command to: it may change files, and nothing of git.
    pub(super) fn same_git_as(&self, other: &Self) -> bool {
        self.git_dir == other.git_dir
            && self.dot_git == other.dot_git
            && self.head == other.head
            && self.branch == other.branch
            && self.repo == other.repo
    }

    /// Whether the files differ from `other`'s (a worktree whose tree id cannot be computed has
    /// only `git status`, which is compared as it is).
    pub(super) fn files_differ_from(&self, other: &Self) -> bool {
        match (&self.tree, &other.tree) {
            (Some(a), Some(b)) => a != b,
            _ => self.status != other.status,
        }
    }

    /// Put it all back; `true` when the worktree is exactly as it was.
    pub(super) async fn restore(&self, slot: &adam_workspace::Slot) -> bool {
        // `HEAD` was on another branch when this was taken: `head` is that branch's commit, and
        // `reset` follows `HEAD`. Nothing is written, so no branch of any run is moved.
        if self.head_is_foreign {
            tracing::warn!(
                slot = slot.dir(),
                head = ?self.branch,
                own = ?self.own_branch,
                "not restoring: HEAD was not on the run's own branch"
            );
            return false;
        }
        let dir = slot.path();
        let at = GitAt::slot(slot);
        if let Some(text) = &self.dot_git
            && adam_workspace::safe_fs::read_regular_text(
                &dir.join(".git"),
                adam_workspace::safe_fs::MAX_SMALL_FILE,
            )
            .await
            .ok()
            .as_deref()
                != Some(text)
            && at.is_plain_directory().await
        {
            adam_workspace::safe_fs::race::checkpoint(dir, "restore-git");
            // Written next to it and renamed over it: a link or a pipe a command put at `.git` is
            // replaced, never written through.
            let _ = adam_workspace::safe_fs::replace_file(&dir.join(".git"), text.as_bytes()).await;
        }
        // Still this run's git directory? Otherwise git would act on another repository.
        if git_stdout(&at, &["rev-parse", "--absolute-git-dir"])
            .await
            .as_deref()
            != Some(self.git_dir.as_str())
        {
            return false;
        }
        // The configuration and the refs are the mirror's, shared by every run: written by one at
        // a time. A scratch project's are its own: nobody shares them, so there is nothing to lock.
        let _lock = match slot.worktree() {
            Some(wt) => match wt.lock_mirror().await {
                Ok(lock) => Some(lock),
                Err(_) => return false,
            },
            None => None,
        };
        let repo = restore_repo_state(&at, &self.repo).await;
        // The branch to put `HEAD` on: the run's own, from the metadata, for a worktree of a
        // repository; what it was on, for a scratch project.
        let own_ref = self.own_branch.as_ref().map(|b| format!("refs/heads/{b}"));
        let branch = own_ref.as_deref().or(self.branch.as_deref());
        let Some(tree) = &self.tree else {
            // No tree to write back: point `HEAD` where it was and keep the files as they are.
            // (Whether these work or not, the files are not known to be as they were.) `reset`
            // follows `HEAD`, so it is only run once `HEAD` is where it should be.
            if point_head(&at, branch, &self.head).await {
                let _ = git_stdout(&at, &["reset", "--soft", "--quiet", &self.head]).await;
            }
            let _ = repo;
            return false;
        };
        repo && restore_worktree(&at, &self.head, branch, tree).await
            && Self::take(&at, self.own_branch.as_deref()).await.as_ref() == Some(self)
    }
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
         configuration), and run_command is for looking around only. {what} To make something with a command use `run`; to change code use write_file, edit_file, apply_patch or delegate_to_opencode. Output of the \
         command, for what it is worth:\n{}",
        outcome.tail
    )
}

/// The command, how it ended, and the tail of its output. A non-zero exit is said plainly: for a
/// command that only looks, it is an answer ("no such file"), not a failure.
pub(super) fn render(command: &str, outcome: &ShellOutcome) -> String {
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
