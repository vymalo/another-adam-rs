//! A run's worktree: inspect, commit, push.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::git::GitCmd;
use crate::repo::RepoRef;
use crate::workspace::{Inner, REMOTE_TRACKING_PREFIX};

/// A run's isolated working tree on its own branch.
///
/// Cheap to clone; clones refer to the same directory. Operations on one
/// worktree are not serialised against each other: drive a run from one task
/// at a time.
#[derive(Clone)]
pub struct Worktree {
    ws: Arc<Inner>,
    run: String,
    repo: RepoRef,
    path: PathBuf,
    branch: String,
    mirror: PathBuf,
}

impl fmt::Debug for Worktree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worktree")
            .field("run", &self.run)
            .field("path", &self.path)
            .field("branch", &self.branch)
            .finish_non_exhaustive()
    }
}

/// Author and committer of the commits [`Worktree::commit_all`] makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitIdentity {
    /// `user.name`.
    pub name: String,
    /// `user.email`.
    pub email: String,
}

impl GitIdentity {
    /// Build an identity.
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
        }
    }

    fn validate(&self) -> WorkspaceResult<()> {
        let bad = |s: &str| s.trim().is_empty() || s.contains(['\n', '\r', '<', '>', '\0']);
        if bad(&self.name) || bad(&self.email) {
            return Err(WorkspaceError::Invalid(
                "git identity needs a non-empty name and email without newlines or angle brackets"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// What happened to a file, as reported by `git status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileStatus {
    /// New and staged.
    Added,
    /// Content changed.
    Modified,
    /// Removed.
    Deleted,
    /// Renamed (the entry carries the new path).
    Renamed,
    /// Copied (the entry carries the new path).
    Copied,
    /// File type changed (e.g. file to symlink).
    TypeChanged,
    /// Not tracked by git.
    Untracked,
    /// Unmerged paths.
    Conflicted,
}

/// One entry of [`Worktree::status`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    /// Path relative to the worktree root.
    pub path: String,
    /// The change.
    pub status: FileStatus,
}

impl Worktree {
    pub(crate) fn new(
        ws: Arc<Inner>,
        run: String,
        repo: RepoRef,
        path: PathBuf,
        branch: String,
        mirror: PathBuf,
    ) -> Self {
        Self {
            ws,
            run,
            repo,
            path,
            branch,
            mirror,
        }
    }

    /// The worktree directory: `<root>/worktrees/<run>`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The branch checked out here: `agent/<run-short-id>`.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The run this worktree belongs to.
    pub fn run(&self) -> &str {
        &self.run
    }

    /// The repository, with the base branch the worktree was created from.
    pub fn repo(&self) -> &RepoRef {
        &self.repo
    }

    fn git(&self) -> GitCmd {
        self.ws.git().cwd(&self.path)
    }

    /// Changed files: staged, unstaged and untracked (each untracked file
    /// listed individually), from `git status --porcelain=v1 -z`.
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn status(&self) -> WorkspaceResult<Vec<ChangedFile>> {
        let out = self
            .git()
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .run()
            .await?;
        Ok(parse_porcelain(&out.stdout))
    }

    /// `git diff --stat` from the merge-base with `origin/<base_branch>` to the
    /// working tree: committed, staged, unstaged and untracked changes alike.
    ///
    /// Untracked files are counted through a temporary copy of the index, so
    /// the real index is not modified.
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn diff_stat(&self) -> WorkspaceResult<String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let base_ref = format!("{REMOTE_TRACKING_PREFIX}{}", self.repo.base_branch);
        let merge_base = self
            .git()
            .args(["merge-base", "HEAD"])
            .arg(&base_ref)
            .run_status()
            .await?;
        // Unrelated histories have no merge-base; compare against the tip.
        let base = if merge_base.success {
            merge_base.stdout_text()
        } else {
            base_ref
        };

        let index = self
            .git()
            .args(["rev-parse", "--git-path", "index"])
            .run()
            .await?
            .stdout_text();
        let index = self.path.join(index);
        let tmp = PathBuf::from(format!(
            "{}.adam-diff-{}-{}",
            index.display(),
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::copy(&index, &tmp)
            .await
            .map_err(|e| WorkspaceError::io("cannot copy the index", e))?;

        let result = async {
            self.git()
                .env("GIT_INDEX_FILE", &tmp)
                .args(["add", "-A", "--intent-to-add"])
                .run()
                .await?;
            self.git()
                .env("GIT_INDEX_FILE", &tmp)
                .args(["diff", "--stat", "--no-color", "--no-ext-diff"])
                .arg(&base)
                .arg("--")
                .run()
                .await
                .map(|out| out.stdout_text())
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        result
    }

    /// Stage everything (`git add -A`) and commit as `author`. Returns the new
    /// commit's sha, or `None` if there was nothing to commit.
    ///
    /// Hooks and commit signing are disabled.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for an unusable identity or empty message,
    /// or a git failure.
    #[tracing::instrument(skip(self, message, author), fields(run = %self.run))]
    pub async fn commit_all(
        &self,
        message: &str,
        author: &GitIdentity,
    ) -> WorkspaceResult<Option<String>> {
        author.validate()?;
        if message.trim().is_empty() {
            return Err(WorkspaceError::Invalid(
                "commit message is empty".to_owned(),
            ));
        }
        self.git().args(["add", "-A"]).run().await?;
        let staged = self
            .git()
            .args(["diff", "--cached", "--quiet"])
            .run_status()
            .await?;
        match staged.code {
            Some(0) => return Ok(None),
            Some(1) => {}
            _ => {
                return Err(WorkspaceError::Corrupt(
                    "cannot tell whether anything is staged".to_owned(),
                ));
            }
        }
        self.git()
            .config("user.name", &author.name)
            .config("user.email", &author.email)
            .args(["commit", "--quiet", "-m"])
            .arg(message)
            .run()
            .await?;
        let sha = self.git().args(["rev-parse", "HEAD"]).run().await?;
        Ok(Some(sha.stdout_text()))
    }

    /// Push the branch to `origin` (`refs/heads/<branch>`) and set it as the
    /// branch's upstream.
    ///
    /// Never forces: pushing a commit the remote already has is a no-op, so
    /// retrying after a lost response is safe. A remote that has diverged is
    /// [`WorkspaceError::Invalid`].
    #[tracing::instrument(skip(self), fields(run = %self.run, branch = %self.branch))]
    pub async fn push(&self) -> WorkspaceResult<()> {
        let loc = self.repo.locate()?;
        let auth = self.ws.authorize(&self.repo, &loc).await?;
        self.git()
            .args(["push", "--quiet", "origin"])
            .arg(format!("refs/heads/{0}:refs/heads/{0}", self.branch))
            .maybe_auth(auth)
            .run()
            .await?;

        // Equivalent of `--set-upstream`. Done by hand and under the repo lock
        // because the branch config lives in the mirror's shared config file,
        // which concurrent pushes of different runs would otherwise race on.
        let lock = self.ws.lock_for(&self.mirror);
        let _guard = lock.lock().await;
        let key = |k: &str| format!("branch.{}.{k}", self.branch);
        self.ws
            .mirror_git(&self.mirror)
            .args(["config", &key("remote"), "origin"])
            .run()
            .await?;
        self.ws
            .mirror_git(&self.mirror)
            .args([
                "config",
                &key("merge"),
                &format!("refs/heads/{}", self.branch),
            ])
            .run()
            .await?;
        Ok(())
    }
}

/// Parse `git status --porcelain=v1 -z`: `XY <path>\0`, plus an extra
/// `<orig-path>\0` entry after renames and copies.
fn parse_porcelain(bytes: &[u8]) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut entries = bytes.split(|b| *b == 0).filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (x, y) = (entry[0], entry[1]);
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            entries.next(); // original path
        }
        let status = match (x, y) {
            (b'!', b'!') => continue,
            (b'?', b'?') => FileStatus::Untracked,
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => FileStatus::Conflicted,
            (b'R', _) | (_, b'R') => FileStatus::Renamed,
            (b'C', _) | (_, b'C') => FileStatus::Copied,
            (b'A', _) => FileStatus::Added,
            (b'D', _) | (_, b'D') => FileStatus::Deleted,
            (b'T', _) | (_, b'T') => FileStatus::TypeChanged,
            _ => FileStatus::Modified,
        };
        files.push(ChangedFile { path, status });
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parsing() {
        let raw = b" M src/lib.rs\0A  new.rs\0?? untracked dir/f.txt\0R  renamed.rs\0old.rs\0 D gone.rs\0UU merge.rs\0!! ignored\0";
        let got = parse_porcelain(raw);
        let want = [
            ("src/lib.rs", FileStatus::Modified),
            ("new.rs", FileStatus::Added),
            ("untracked dir/f.txt", FileStatus::Untracked),
            ("renamed.rs", FileStatus::Renamed),
            ("gone.rs", FileStatus::Deleted),
            ("merge.rs", FileStatus::Conflicted),
        ];
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (g, (path, status)) in got.iter().zip(want) {
            assert_eq!((g.path.as_str(), g.status), (path, status));
        }
    }

    #[test]
    fn identity_is_validated() {
        assert!(
            GitIdentity::new("Ada", "ada@example.com")
                .validate()
                .is_ok()
        );
        assert!(GitIdentity::new("", "a@b").validate().is_err());
        assert!(GitIdentity::new("A <x>", "a@b").validate().is_err());
    }
}
