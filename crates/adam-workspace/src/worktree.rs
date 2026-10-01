//! A run's worktree (one repository of its workspace): inspect, commit, push.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::git::GitCmd;
use crate::repo::RepoRef;
use crate::workspace::{Inner, Meta, REMOTE_TRACKING_PREFIX};

/// A run's isolated working tree on its own branch.
///
/// Cheap to clone; clones refer to the same directory. Operations on one
/// worktree are not serialised against each other: drive a run from one task
/// at a time.
#[derive(Clone)]
pub struct Worktree {
    ws: Arc<Inner>,
    run: String,
    /// The slot's directory name in the run's workspace.
    dir: String,
    repo: RepoRef,
    path: PathBuf,
    /// The name the work ends up under: the run's own branch, or the pushed branch this worktree
    /// continues (which only [`Worktree::publish`] moves).
    branch: String,
    /// The local branch that is checked out, always the run's own.
    local: String,
    mirror: PathBuf,
}

impl fmt::Debug for Worktree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worktree")
            .field("run", &self.run)
            .field("dir", &self.dir)
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

    pub(crate) fn validate(&self) -> WorkspaceResult<()> {
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
    /// The worktree that `meta` describes, in the slot `dir`, at `path`.
    pub(crate) fn new(
        ws: Arc<Inner>,
        meta: &Meta,
        dir: String,
        path: PathBuf,
        mirror: PathBuf,
    ) -> Self {
        Self {
            ws,
            run: meta.run.clone(),
            dir,
            repo: RepoRef::new(&meta.url, &meta.base_branch),
            path,
            branch: meta
                .remote_branch
                .clone()
                .unwrap_or_else(|| meta.branch.clone()),
            local: meta.branch.clone(),
            mirror,
        }
    }

    /// The worktree directory: `<root>/workspaces/<run>/<dir>` for a slot of a
    /// [`RunWorkspace`](crate::RunWorkspace), `<root>/worktrees/<run>` for one made by
    /// [`Workspaces::prepare`](crate::Workspaces::prepare).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The name of the worktree's slot in the run's workspace: the repository's name, lowercased
    /// (see [`RunWorkspace::slots`](crate::RunWorkspace::slots)); the same for a worktree made by
    /// [`Workspaces::prepare`](crate::Workspaces::prepare), which is a slot of the legacy layout.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// The branch this worktree's work ends up on, which is what a pull request is opened from:
    /// `agent/<run-short-id>`, or, for a worktree made by
    /// [`Workspaces::prepare_continuing`](crate::Workspaces::prepare_continuing), the pushed
    /// branch it continues. It is **not** where [`push`](Self::push) sends the commits: that is
    /// [`local_branch`](Self::local_branch), and the continued branch only receives them through
    /// [`publish`](Self::publish).
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The run's own branch, `agent/<run-short-id>`: what is checked out, and what
    /// [`push`](Self::push) publishes.
    pub fn local_branch(&self) -> &str {
        &self.local
    }

    /// The pushed branch this worktree continues, if it continues one
    /// ([`Workspaces::prepare_continuing`](crate::Workspaces::prepare_continuing)): the branch
    /// that [`publish`](Self::publish) moves forward.
    pub fn continues(&self) -> Option<&str> {
        (self.branch != self.local).then_some(self.branch.as_str())
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
        // The copy keeps the index's modification time: git re-reads an entry that is as new as
        // the index file ("racily clean"), and a copy stamped with the time it was made would
        // make every entry look old, so a file rewritten with the same size right after the
        // checkout would be counted with its old content.
        let keep = async {
            let modified = tokio::fs::metadata(&index).await?.modified()?;
            let copy = tmp.clone();
            tokio::task::spawn_blocking(move || {
                std::fs::File::options()
                    .write(true)
                    .open(copy)?
                    .set_modified(modified)
            })
            .await
            .map_err(std::io::Error::other)?
        }
        .await;
        if let Err(e) = keep {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(WorkspaceError::io("cannot copy the index's timestamp", e));
        }

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
        commit_all_in(|| self.git(), message, author).await
    }

    /// Push the run's own branch ([`local_branch`](Self::local_branch), `agent/<run-short-id>`) to
    /// `origin` under the same name and set it as the branch's upstream.
    ///
    /// This is the publication of the run's commits, and it is the same for a worktree that
    /// continues a pushed branch: the run's work goes to its own branch first, and the branch it
    /// continues is only moved by [`publish`](Self::publish), when the caller has decided that work
    /// may be published there (a pull request that is open for that branch must not carry commits
    /// nobody has verified).
    ///
    /// Never forces: pushing a commit the remote already has is a no-op, so
    /// retrying after a lost response is safe. A remote that has diverged is
    /// [`WorkspaceError::Invalid`].
    #[tracing::instrument(skip(self), fields(run = %self.run, branch = %self.local))]
    pub async fn push(&self) -> WorkspaceResult<()> {
        let loc = self.repo.locate()?;
        let auth = self.ws.authorize(&self.repo, &loc).await?;
        // The mirror lock is held from the clean-up of the configuration to the end of the
        // push and of the upstream bookkeeping: what carries the token runs on a configuration
        // that was just made safe, and the branch config (the mirror's shared file, which
        // concurrent pushes of different runs would otherwise race on) is written by one at a time.
        let _guard = self.ws.lock_mirror(&self.mirror).await?;
        self.ws
            .sanitize_mirror_config(&self.mirror, loc.remote_url(&self.repo.url))
            .await?;
        self.git()
            .args(["push", "--quiet"])
            .arg(loc.remote_url(&self.repo.url))
            .arg(format!("refs/heads/{0}:refs/heads/{0}", self.local))
            .maybe_auth(auth)
            .run()
            .await?;
        self.record_pushed(&self.local).await;

        // Equivalent of `--set-upstream`, done by hand.
        let key = |k: &str| format!("branch.{}.{k}", self.local);
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
                &format!("refs/heads/{}", self.local),
            ])
            .run()
            .await?;
        Ok(())
    }

    /// Move the branch this worktree continues ([`branch`](Self::branch)) forward to the run's
    /// branch: `git push origin <local>:<branch>`, **never forced**, so the branch only ever gets
    /// this run's commits on top of what it had. A worktree that continues nothing has nothing to
    /// do and returns at once.
    ///
    /// Call it after [`push`](Self::push) (the commits are then on the remote already), once the
    /// work has earned its place on the branch. Repeating it is a no-op.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Conflict`] when the branch has moved on the remote since this worktree
    /// started from it (someone pushed to it: the run's commits are no longer a fast-forward of
    /// it), and it was not touched. Any other push failure as for [`push`](Self::push).
    #[tracing::instrument(skip(self), fields(run = %self.run, branch = %self.branch))]
    pub async fn publish(&self) -> WorkspaceResult<()> {
        if self.continues().is_none() {
            return Ok(());
        }
        let loc = self.repo.locate()?;
        let auth = self.ws.authorize(&self.repo, &loc).await?;
        let _guard = self.ws.lock_mirror(&self.mirror).await?;
        self.ws
            .sanitize_mirror_config(&self.mirror, loc.remote_url(&self.repo.url))
            .await?;
        let pushed = self
            .git()
            .args(["push", "--quiet"])
            .arg(loc.remote_url(&self.repo.url))
            .arg(format!(
                "refs/heads/{}:refs/heads/{}",
                self.local, self.branch
            ))
            .maybe_auth(auth)
            .run()
            .await;
        match pushed {
            Err(WorkspaceError::Invalid(message))
                if message.contains("non-fast-forward") || message.contains("fetch first") =>
            {
                Err(WorkspaceError::Conflict(format!(
                    "the branch {} moved on the remote since this worktree was started from it, \
                     so this run's commits cannot be added to it without overwriting what was \
                     pushed there; it was not changed",
                    self.branch
                )))
            }
            Err(e) => Err(e),
            Ok(_) => {
                self.record_pushed(&self.branch).await;
                Ok(())
            }
        }
    }

    /// Note in the mirror that `origin/<branch>` is now the run's branch tip, as `git push origin`
    /// would have: the pushes name the URL and not the remote called `origin`, which does not
    /// update the remote-tracking ref by itself, and `@{upstream}` and the next `prepare` read it.
    ///
    /// Best effort: the push has succeeded, and a failure here must not turn it into an error
    /// (the next fetch writes the same ref).
    async fn record_pushed(&self, branch: &str) {
        let noted = async {
            let tip = self
                .git()
                .args(["rev-parse", "--verify", "--quiet"])
                .arg(format!("refs/heads/{}^{{commit}}", self.local))
                .run()
                .await?
                .stdout_text();
            self.ws
                .mirror_git(&self.mirror)
                .args(["update-ref", "--no-deref"])
                .arg(format!("{REMOTE_TRACKING_PREFIX}{branch}"))
                .arg(tip)
                .run()
                .await
        }
        .await;
        if let Err(e) = noted {
            tracing::warn!(error = %e, branch, "cannot record the pushed tip in the mirror");
        }
    }

    /// Hold the lock of this worktree's mirror: the repository state that every run shares (refs,
    /// configuration) is changed by one at a time while it is held. For a caller that writes that
    /// state itself, such as undoing a change a command made to it.
    ///
    /// # Errors
    ///
    /// When the lock cannot be taken (see *Sharing a root between processes* in the README).
    pub async fn lock_mirror(&self) -> WorkspaceResult<MirrorLock> {
        Ok(MirrorLock {
            _guard: self.ws.lock_mirror(&self.mirror).await?,
        })
    }
}

/// Stage everything (`git add -A`) and commit as `author`, in the repository `git` runs in: the
/// sha, or `None` when there was nothing to commit. What [`Worktree::commit_all`] and
/// [`Scratch::commit_all`](crate::Scratch::commit_all) do.
pub(crate) async fn commit_all_in(
    git: impl Fn() -> GitCmd,
    message: &str,
    author: &GitIdentity,
) -> WorkspaceResult<Option<String>> {
    author.validate()?;
    if message.trim().is_empty() {
        return Err(WorkspaceError::Invalid(
            "commit message is empty".to_owned(),
        ));
    }
    git().args(["add", "-A"]).run().await?;
    let staged = git()
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
    git()
        .config("user.name", &author.name)
        .config("user.email", &author.email)
        .args(["commit", "--quiet", "-m"])
        .arg(message)
        .run()
        .await?;
    let sha = git().args(["rev-parse", "HEAD"]).run().await?;
    Ok(Some(sha.stdout_text()))
}

/// The lock of a mirror, held by [`Worktree::lock_mirror`] until it is dropped.
#[must_use = "the lock is released when this is dropped"]
pub struct MirrorLock {
    _guard: crate::workspace::MirrorGuard,
}

/// Parse `git status --porcelain=v1 -z`: `XY <path>\0`, plus an extra
/// `<orig-path>\0` entry after renames and copies.
pub(crate) fn parse_porcelain(bytes: &[u8]) -> Vec<ChangedFile> {
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
