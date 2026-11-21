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
//!   `write_file`, `edit_file` and `apply_patch` and, for a command that makes something (a build, an export, a
//!   generated file), by `run` ([`run`](super::make)), which keeps the files it changes and is held
//!   to the same guard for everything else.
//!
//! It does **not** bump the check epoch (`ChecksNotes::epoch`): its guard compares the tree and
//! undoes what a look changed, but it does not cover ignored files (`target/`, `node_modules/`), and
//! it is documented as looking only. `run` keeps files, ignored ones included, and does bump it.
//!
//! A command the shell cannot find is reported as a missing toolchain, as for `run_checks` (see
//! [`missing_tool`](super::shell::missing_tool)).
//!
//! The coder declares the tool here and its logic is `adam_code_tools::inspect` (ADR 0047), over
//! the run's slots (`ToolEnv` as a `CommandWorkspace`): the guard is the coder's `SlotGuard`, over
//! the `Snapshot` that stays here.

use adam::prelude::*;

use super::gitcli::{
    GitAt, RepoState, SnapshotError, git_stdout, head_ref, point_head, repo_state,
    restore_configuration_only, restore_repo_state, restore_worktree, status_text, working_tree_id,
};
use super::{Outcome, ToolEnv};

/// Look around in your worktree with a shell command: `git branch -r`, `git log --oneline`,
/// `ls`, `cat README.md`, `grep -rn name src`. It runs in the workspace's environment (a run pod
/// of the run's own when the deployment runs them). You get the exit code and
/// the tail of the output.
/// It costs no check cycle and reports no checks, and it is for looking: changes it makes to
/// HEAD, the branch and the working tree are undone and refused (to make a file or change the
/// worktree with a command, use run; to change code, write_file, edit_file or
/// apply_patch). Use it to look; use run_checks only for the project's real checks. Long output is
/// kept: you get its head and tail and an id to read the rest with read_output.
#[tool(label = "Run a command", icon = "execute", access = super::access::runs_a_command)]
pub async fn run_command(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the worktree, in the workspace's environment
    command: String,
    /// Optional sub-directory of the worktree to run in (relative, inside the worktree)
    cwd: Option<String>,
    /// The slot to run in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
    /// Seconds before the command is killed; at most the deployment's limit, which is also the default. Example: {"command":"npm test","timeout":120}
    timeout: Option<i64>,
) -> Outcome {
    adam_code_tools::inspect::run_command(&*env, ctx, command, cwd, repo, timeout).await
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
    pub(super) async fn of_slot(slot: &adam_workspace::Slot) -> Result<Self, SnapshotError> {
        Self::take(
            &GitAt::slot(slot),
            slot.worktree().map(adam_workspace::Worktree::local_branch),
        )
        .await
    }

    /// The state of the worktree `at`. `own` is the branch the run's metadata says the worktree is on
    /// (a repository's worktree; `None` for a scratch project).
    pub(super) async fn take(at: &GitAt, own: Option<&str>) -> Result<Self, SnapshotError> {
        // Every question is an independent read of the worktree, and each one is a `git` process:
        // asked at once they cost one round of starts rather than six, and a command has already
        // ended by the time this is called. The git directory and `HEAD` come from one `rev-parse`,
        // which prints each on its own line. `status` is the fallback of the tree id, so it waits
        // for it.
        let (git_and_head, dot_git, branch, tree, repo) = tokio::join!(
            git_stdout(at, &["rev-parse", "--absolute-git-dir", "HEAD"]),
            at.dot_git(),
            head_ref(at),
            working_tree_id(at),
            repo_state(at),
        );
        let (git_dir, head) = split_rev_parse(git_and_head).ok_or(SnapshotError::Unreadable)?;
        let repo = repo?;
        let head_is_foreign =
            own.is_some_and(|own| branch.as_deref() != Some(format!("refs/heads/{own}").as_str()));
        let status = match tree {
            Some(_) => None,
            None => Some(status_text(at).await.ok_or(SnapshotError::Unreadable)?),
        };
        Ok(Self {
            git_dir,
            dot_git,
            head,
            branch,
            own_branch: own.map(str::to_owned),
            head_is_foreign,
            tree,
            status,
            repo,
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

    /// Whether the repository's configuration and ignore rules are as `other` has them.
    pub(super) fn same_configuration_as(&self, other: &Self) -> bool {
        self.repo.configuration_is_as(&other.repo)
    }

    /// Whether everything [`same_git_as`](Self::same_git_as) compares is as `other` has it, except
    /// the configuration: the git directory, the `.git` file, `HEAD`, the branch and the refs.
    pub(super) fn same_git_apart_from_configuration_as(&self, other: &Self) -> bool {
        self.git_dir == other.git_dir
            && self.dot_git == other.dot_git
            && self.head == other.head
            && self.branch == other.branch
            && self.repo.refs_are_as(&other.repo)
    }

    /// Put the configuration back as it was and leave `HEAD`, the refs and the files: a command that
    /// kept running may have made commits the run's own tools made too. `true` when it is back.
    pub(super) async fn restore_configuration(&self, slot: &adam_workspace::Slot) -> bool {
        if self.head_is_foreign {
            return false;
        }
        let at = GitAt::slot(slot);
        let _lock = match slot.checkout() {
            Some(wt) => match wt.lock_mirror().await {
                Ok(lock) => Some(lock),
                Err(_) => return false,
            },
            None => None,
        };
        restore_configuration_only(&at, &self.repo).await
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
        let at = GitAt::slot(slot);
        if let Some(text) = &self.dot_git
            && at.dot_git().await.as_deref() != Some(text)
        {
            adam_workspace::safe_fs::race::checkpoint(at.path(), "restore-git");
            // Written next to it and renamed over it, in the directory the coder opened as the
            // slot: a link or a pipe a command put at `.git` is replaced, never written through,
            // and a slot that was swapped for a link meanwhile is not followed.
            let _ = at.put_dot_git(text.clone()).await;
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
        let _lock = match slot.checkout() {
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
            && Self::take(&at, self.own_branch.as_deref())
                .await
                .ok()
                .as_ref()
                == Some(self)
    }
}

/// The git directory and `HEAD` from one `rev-parse --absolute-git-dir HEAD`: git prints each on
/// its own line. Splitting from the end keeps a git directory whose name holds a newline whole.
/// The directory is trimmed at its end, as `git_stdout` trims the one [`restore`](Snapshot::restore)
/// asks for, so the two compare like with like; a git directory whose name ends in whitespace is
/// not supported (a worktree's is `.git` or a directory the coder generated, so none does).
fn split_rev_parse(out: Option<String>) -> Option<(String, String)> {
    let out = out?;
    let (git_dir, head) = out.rsplit_once('\n')?;
    let git_dir = git_dir.trim_end();
    (!git_dir.is_empty() && !head.is_empty()).then(|| (git_dir.to_owned(), head.to_owned()))
}

/// What a tool says when the worktree cannot be snapshotted: why, then `not_done`.
pub(super) fn cannot_snapshot(error: SnapshotError, not_done: &str) -> String {
    let why = match error {
        SnapshotError::GitTooOld => {
            "the git of this workspace is too old: the refs are scanned with `git for-each-ref \
             --exclude`, which needs git 2.42 or later"
        }
        SnapshotError::Unreadable => "the state of the worktree cannot be read",
    };
    format!(
        "Cannot look at the worktree: {why}. {not_done} Try again; if it persists, tell the person."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::gitcli::head_sha;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// The git directory and `HEAD` of one `rev-parse --absolute-git-dir HEAD` are the two values
    /// the two commands gave, on a branch and detached; a HEAD that does not exist is still a
    /// failure (a snapshot with no head has nothing to restore).
    #[tokio::test]
    async fn the_git_directory_and_head_of_one_rev_parse_are_the_two_values() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        git(dir, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(dir.join("README.md"), "x\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "--quiet", "-m", "seed"]);
        let at = GitAt::plain(dir);

        let snap = Snapshot::take(&at, None).await.unwrap();
        assert_eq!(
            snap.git_dir,
            git_stdout(&at, &["rev-parse", "--absolute-git-dir"])
                .await
                .unwrap()
        );
        assert_eq!(snap.head, head_sha(&at).await.unwrap());

        git(dir, &["checkout", "--quiet", "--detach", "HEAD"]);
        let snap = Snapshot::take(&at, None).await.unwrap();
        assert_eq!(snap.head, head_sha(&at).await.unwrap());
        assert_eq!(snap.branch, None);

        // An unborn HEAD: `rev-parse HEAD` fails, and so does the snapshot.
        git(dir, &["checkout", "--quiet", "--orphan", "empty"]);
        assert_eq!(
            Snapshot::take(&at, None).await,
            Err(SnapshotError::Unreadable)
        );
    }

    /// A git directory whose name holds a newline is not split in two: the split is at the last
    /// line, which is `HEAD`.
    #[tokio::test]
    async fn a_git_directory_whose_name_holds_a_newline_is_split_whole() {
        let tmp = tempfile::Builder::new().prefix("a\nb").tempdir().unwrap();
        // git answers with the path it resolved: compare like with like.
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        git(&dir, &["init", "--quiet", "--initial-branch=main"]);
        std::fs::write(dir.join("README.md"), "x\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "--quiet", "-m", "seed"]);
        let at = GitAt::plain(&dir);
        let snap = Snapshot::take(&at, None).await.unwrap();
        assert_eq!(snap.git_dir, format!("{}/.git", dir.display()));
        assert_eq!(snap.head, head_sha(&at).await.unwrap());
    }

    /// The directory is trimmed at its end, as `git_stdout` trims the one `restore` compares it
    /// with: a path ending in whitespace is not supported, and this is what "not supported" means.
    #[test]
    fn the_git_directory_is_trimmed_at_its_end_as_the_restore_query_trims_it() {
        assert_eq!(
            split_rev_parse(Some("/a/gitdir \n0123abc".to_owned())),
            Some(("/a/gitdir".to_owned(), "0123abc".to_owned()))
        );
    }

    #[test]
    fn a_git_too_old_is_said_as_such_not_as_an_unreadable_worktree() {
        let said = cannot_snapshot(SnapshotError::GitTooOld, "Nothing was run.");
        assert!(said.contains("git 2.42"), "{said}");
        assert!(!said.contains("cannot be read"), "{said}");
        let plain = cannot_snapshot(SnapshotError::Unreadable, "Nothing was run.");
        assert!(plain.contains("cannot be read"), "{plain}");
        assert!(!plain.contains("2.42"), "{plain}");
    }
}
