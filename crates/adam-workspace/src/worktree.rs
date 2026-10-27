//! A run's worktree (one repository of its workspace): inspect, commit, push.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::git::{GitCmd, GitPin};
use crate::repo::RepoRef;
use crate::safe_fs::{self, SlotDir};
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
    /// The slot's directory, opened where the slot was checked and held from then on.
    root: Arc<SlotDir>,
    /// The name the work ends up under: the run's own branch, or the pushed branch this worktree
    /// continues (which only [`Worktree::publish`] moves).
    branch: String,
    /// The local branch that is checked out, always the run's own.
    local: String,
    mirror: PathBuf,
    /// The worktree's directory in the mirror's `worktrees/`, from the coder's metadata.
    admin: Option<PathBuf>,
    /// Which making of `admin` this is (see [`Worktree::git_dir_generation`]).
    admin_gen: Option<String>,
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

impl std::fmt::Display for FileStatus {
    /// The change in words a person reads: `modified`, `type changed`, ...
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Copied => "copied",
            Self::TypeChanged => "type changed",
            Self::Untracked => "untracked",
            Self::Conflicted => "conflicted",
        })
    }
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
    /// The worktree that `meta` describes, in the slot `dir`, at `path`, whose directory is `root`.
    pub(crate) fn new(
        ws: Arc<Inner>,
        meta: &Meta,
        dir: String,
        root: Arc<SlotDir>,
        mirror: PathBuf,
    ) -> Self {
        Self {
            ws,
            run: meta.run.clone(),
            dir,
            repo: RepoRef::new(&meta.url, &meta.base_branch),
            path: root.path().to_path_buf(),
            root,
            branch: meta
                .remote_branch
                .clone()
                .unwrap_or_else(|| meta.branch.clone()),
            local: meta.branch.clone(),
            admin: meta
                .admin
                .as_deref()
                .filter(|name| is_admin_name(name))
                .map(|name| mirror.join("worktrees").join(name)),
            admin_gen: meta
                .admin
                .as_deref()
                .filter(|name| is_admin_name(name))
                .and(meta.admin_gen.as_deref())
                .filter(|token| crate::workspace::is_generation(token))
                .map(str::to_owned),
            mirror,
        }
    }

    /// The worktree directory: `<root>/workspaces/<run>/<dir>` for a slot of a
    /// [`RunWorkspace`](crate::RunWorkspace), `<root>/worktrees/<run>` for one made by
    /// [`Workspaces::prepare`](crate::Workspaces::prepare).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The worktree's directory, held open: what the coder's own file operations in the worktree
    /// are relative to, so that a command which renames the directory and puts a link at its name
    /// leads them nowhere ([`SlotDir`]).
    pub fn root(&self) -> &Arc<SlotDir> {
        &self.root
    }

    /// The shared bare mirror this worktree is linked to: `<root>/git/<host>/<owner>/<repo>.git`.
    /// The worktree's `.git` file points into it, so a process that is to run `git` in the
    /// worktree (an environment other than the caller's own) must be able to read it.
    pub fn mirror(&self) -> &Path {
        &self.mirror
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

    /// `git` in the worktree, pinned to the git directory the coder recorded for it ([`GitPin`]), or
    /// refusing to run when there is none: never to wherever the slot's `.git` file points, which a
    /// command of the run can rewrite.
    fn git(&self) -> GitCmd {
        let git = self.ws.git().cwd(&self.path);
        match self.git_pin() {
            Some(pin) => git.pinned(pin),
            None => git.refusing(format!(
                "cannot tell which directory of {}/worktrees is the git directory of {}: remove the run's workspace and add the repository again",
                self.mirror.display(),
                self.path.display()
            )),
        }
    }

    /// The worktree's git directory: its directory in the mirror's `worktrees/`, as the coder
    /// recorded it when it made the worktree (not read from the worktree's `.git` file, which a
    /// command of the run can write). `None` when it cannot be told: metadata from before it was
    /// recorded, and a mirror with no or several candidates.
    pub fn git_dir(&self) -> Option<&Path> {
        self.admin.as_deref()
    }

    /// A token that is new each time the coder makes the directory [`git_dir`](Self::git_dir) names.
    /// The name is the same when a slot is removed and added again, the directory is not, and a run
    /// pod's mounts follow the directory it was made with: a caller that records what a pod mounts
    /// records this with the path, and makes the pod again when it changes. `None` for a worktree
    /// made before it was recorded, and without a git directory.
    pub fn git_dir_generation(&self) -> Option<&str> {
        self.admin.as_ref().and(self.admin_gen.as_deref())
    }

    /// Where `git` works for this worktree: [`git_dir`](Self::git_dir) and the mirror, with the
    /// worktree's directory as the work tree. `None` without a git directory.
    pub fn git_pin(&self) -> Option<GitPin> {
        self.admin.as_deref().map(|admin| {
            GitPin::worktree(admin, &self.mirror, &self.path, Arc::clone(&self.root))
                .with_sandbox(self.ws.sandbox())
        })
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
    /// the real index is not modified. The copy is made in a directory of the coder's own, outside
    /// the volume, from the index read as a regular file and not through a link (the index is in a
    /// directory the run's commands write): see [`safe_fs`](crate::safe_fs).
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn diff_stat(&self) -> WorkspaceResult<String> {
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

        // `git()` has refused already if there is no git directory.
        let Some(admin) = self.git_dir() else {
            return Err(WorkspaceError::Corrupt(
                "the worktree's git directory is not known".to_owned(),
            ));
        };
        let copy = safe_fs::private_copy_in(
            self.ws.private_dir().as_deref(),
            &admin.join("index"),
            safe_fs::MAX_INDEX_COPY,
        )
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidInput => {
                WorkspaceError::Corrupt(format!("the worktree's index cannot be used: {e}"))
            }
            _ => WorkspaceError::io("cannot copy the index", e),
        })?;

        self.git()
            .env("GIT_INDEX_FILE", copy.path())
            .args(["add", "-A", "--intent-to-add"])
            .run()
            .await?;
        self.git()
            .env("GIT_INDEX_FILE", copy.path())
            .args([
                "diff",
                "--stat",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
            ])
            .arg(&base)
            .arg("--")
            .run()
            .await
            .map(|out| out.stdout_text())
    }

    /// Stage everything (`git add -A`) and commit as `author`. Returns the new
    /// commit's sha, or `None` if there was nothing to commit.
    ///
    /// The commit goes onto the run's own branch ([`local_branch`](Self::local_branch)), which is
    /// named and not followed from `HEAD`: `HEAD` is a file in the worktree's directory in the mirror
    /// that a command of the run can write. The branch moves only if its tip is still the one the
    /// commit was made on.
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
        commit_all_in(|| self.git(), message, author, Some(&self.local)).await
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
        // The mirror lock is held from the check of the configuration to the end of the push and
        // of the upstream bookkeeping: what carries the token runs on a configuration that was
        // just checked, and the branch config (the mirror's shared file, which concurrent pushes
        // of different runs would otherwise race on) is written by one at a time. The push is made
        // on the bare mirror: the branch is there, and it needs nothing of the worktree.
        let _guard = self.ws.lock_mirror(&self.mirror).await?;
        let url = loc.remote_url(&self.repo.url);
        self.ws
            .run_network(&self.mirror, url, auth, |git| {
                git.args(["push", "--quiet"])
                    .arg(url)
                    .arg(format!("refs/heads/{0}:refs/heads/{0}", self.local))
            })
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
        let url = loc.remote_url(&self.repo.url);
        let pushed = self
            .ws
            .run_network(&self.mirror, url, auth, |git| {
                git.args(["push", "--quiet"]).arg(url).arg(format!(
                    "refs/heads/{}:refs/heads/{}",
                    self.local, self.branch
                ))
            })
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

/// A temporary detached checkout of the base branch next to a run's worktree, for running a check
/// on the code the run started from ([`Worktree::add_base_checkout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseCheckout {
    path: PathBuf,
    commit: String,
    /// The symlinks made to the worktree's ignored files and directories.
    links: Vec<PathBuf>,
}

impl BaseCheckout {
    /// The directory of the checkout: `<run's workspace>/.adam-base/<slot>`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The commit it holds: the base branch's, `origin/<base_branch>`, as it was when it was made.
    pub fn commit(&self) -> &str {
        &self.commit
    }
}

/// Where a run's [`BaseCheckout`]s are, in its workspace; not a slot (slots are listed from metadata).
const BASE_CHECKOUTS_DIR: &str = ".adam-base";

impl Worktree {
    /// The commit `origin/<base_branch>` is at in the mirror, `None` without such a ref.
    ///
    /// # Errors
    ///
    /// A git failure other than a missing ref.
    pub async fn base_commit(&self) -> WorkspaceResult<Option<String>> {
        let rev = format!(
            "{REMOTE_TRACKING_PREFIX}{}^{{commit}}",
            self.repo.base_branch
        );
        let out = self
            .git()
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(&rev)
            .run_status()
            .await?;
        let sha = out.stdout_text();
        Ok((out.success && !sha.is_empty()).then_some(sha))
    }

    /// Check out [`base_commit`](Self::base_commit) detached in `<run's workspace>/.adam-base/<slot>`,
    /// where the run's environment can run a command as it does in the slot. What the slot has and
    /// git ignores (`node_modules`, `target`) is linked in. A leftover is replaced. `None` for a
    /// worktree of the old layout (`<root>/worktrees/<run>`) and without a base commit.
    ///
    /// # Errors
    ///
    /// A git or file system failure.
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn add_base_checkout(&self) -> WorkspaceResult<Option<BaseCheckout>> {
        let run_dir = self.ws.run_dir(&self.run);
        if self.path.parent() != Some(run_dir.as_path()) {
            return Ok(None);
        }
        let Some(commit) = self.base_commit().await? else {
            return Ok(None);
        };
        let base_dir = run_dir.join(BASE_CHECKOUTS_DIR);
        let path = base_dir.join(&self.dir);
        let leftover = BaseCheckout {
            path: path.clone(),
            commit: commit.clone(),
            links: Vec::new(),
        };
        self.remove_base_checkout(&leftover).await?;
        // The directory of the base checkouts is the run's to write: a link there, to a directory
        // anywhere, would have the checkout made (and later removed) where the link leads.
        make_plain_directory(&base_dir).await?;
        {
            let _guard = self.ws.lock_mirror(&self.mirror).await?;
            self.ws
                .add_linked(
                    &self.mirror,
                    &self.run,
                    &self.base_admin_name(),
                    &path,
                    |cmd, staging| cmd.arg("--detach").arg(staging).arg(&commit),
                )
                .await?;
        }
        let mut checkout = BaseCheckout {
            path,
            commit,
            links: Vec::new(),
        };
        match self.link_ignored(&mut checkout).await {
            Ok(()) => Ok(Some(checkout)),
            Err(e) => {
                let _ = self.remove_base_checkout(&checkout).await;
                Err(e)
            }
        }
    }

    /// Link what this worktree has and git ignores into `checkout`, where the base has nothing of
    /// its own at that path.
    async fn link_ignored(&self, checkout: &mut BaseCheckout) -> WorkspaceResult<()> {
        let listed = self
            .git()
            .args([
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ])
            .run()
            .await?;
        for entry in listed.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let Ok(entry) = std::str::from_utf8(entry) else {
                continue;
            };
            let relative = Path::new(entry.trim_end_matches('/'));
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                continue;
            }
            let (from, to) = (self.path.join(relative), checkout.path.join(relative));
            if tokio::fs::symlink_metadata(&to).await.is_ok() {
                continue;
            }
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    WorkspaceError::io(format!("cannot make {}", parent.display()), e)
                })?;
            }
            #[cfg(unix)]
            tokio::fs::symlink(&from, &to)
                .await
                .map_err(|e| WorkspaceError::io(format!("cannot link {}", to.display()), e))?;
            #[cfg(not(unix))]
            let _ = from;
            checkout.links.push(to);
        }
        Ok(())
    }

    /// The name of this worktree's base checkout in the mirror's `worktrees/`: the slot's name and
    /// `+base`, after the run (see `admin_name`), so that it is one of the run's and no other's.
    fn base_admin_name(&self) -> String {
        crate::workspace::admin_name(&self.run, &format!("{}+base", self.dir))
    }

    /// Remove a checkout, its links first, and forget it in the mirror: its directory is removed by
    /// name, and nothing else of the mirror is touched (no `git worktree prune`, which judges every
    /// run's directories). Idempotent.
    ///
    /// The checkout is in the run's workspace, where the run's commands can put a link in place of
    /// `.adam-base` or of the checkout. A link is removed and not followed: it is not asked to
    /// `git worktree remove`, which would delete the worktree the link leads to.
    ///
    /// # Errors
    ///
    /// A git or file system failure; the directory is removed whatever git says.
    #[tracing::instrument(skip(self, checkout), fields(run = %self.run))]
    pub async fn remove_base_checkout(&self, checkout: &BaseCheckout) -> WorkspaceResult<()> {
        let run_dir = self.ws.run_dir(&self.run);
        let base_dir = run_dir.join(BASE_CHECKOUTS_DIR);
        let _guard = self.ws.lock_mirror(&self.mirror).await?;
        let forget = || async {
            crate::workspace::remove_dir_if_exists(
                &self.mirror.join("worktrees").join(self.base_admin_name()),
            )
            .await
        };
        if let Ok(meta) = tokio::fs::symlink_metadata(&base_dir).await
            && !meta.is_dir()
        {
            tokio::fs::remove_file(&base_dir).await.map_err(|e| {
                WorkspaceError::io("cannot remove a link in the run's workspace", e)
            })?;
            return forget().await;
        }
        if let Ok(meta) = tokio::fs::symlink_metadata(&checkout.path).await
            && !meta.is_dir()
        {
            tokio::fs::remove_file(&checkout.path).await.map_err(|e| {
                WorkspaceError::io("cannot remove a link in the run's workspace", e)
            })?;
            return forget().await;
        }
        for link in &checkout.links {
            let _ = tokio::fs::remove_file(link).await;
        }
        if tokio::fs::symlink_metadata(&checkout.path).await.is_ok() {
            let _ = self
                .ws
                .mirror_git(&self.mirror)
                .args(["worktree", "remove", "--force"])
                .arg(&checkout.path)
                .run_status()
                .await?;
        }
        crate::workspace::remove_dir_if_exists(&checkout.path).await?;
        forget().await
    }
}

/// `path` as a directory of its own: made when it is not there, and a link or a file in its place
/// is removed first.
async fn make_plain_directory(path: &Path) -> WorkspaceResult<()> {
    if let Ok(meta) = tokio::fs::symlink_metadata(path).await {
        if meta.is_dir() {
            return Ok(());
        }
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| WorkspaceError::io(format!("cannot remove {}", path.display()), e))?;
    }
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|e| WorkspaceError::io(format!("cannot make {}", path.display()), e))
}

/// Stage everything (`git add -A`) and commit as `author`, in the repository `git` runs in: the
/// sha, or `None` when there was nothing to commit. What [`Worktree::commit_all`] and
/// [`Scratch::commit_all`](crate::Scratch::commit_all) do.
///
/// With `branch` (a worktree's own branch) the commit is made with plumbing onto
/// `refs/heads/<branch>`, whatever `HEAD` says: the worktree's `HEAD` is a file in the worktree's
/// directory in the mirror, which the run's commands can write, and a `git commit` follows it. A
/// run that pointed it at another run's branch would have the coder commit its files there. The
/// new commit's parent is the branch's tip, and the branch moves only if it is still there
/// (`update-ref` with the old value).
pub(crate) async fn commit_all_in(
    git: impl Fn() -> GitCmd,
    message: &str,
    author: &GitIdentity,
    branch: Option<&str>,
) -> WorkspaceResult<Option<String>> {
    author.validate()?;
    if message.trim().is_empty() {
        return Err(WorkspaceError::Invalid(
            "commit message is empty".to_owned(),
        ));
    }
    git().args(["add", "-A"]).run().await?;
    if let Some(branch) = branch {
        return commit_staged_on(git, message, author, &format!("refs/heads/{branch}")).await;
    }
    let staged = git()
        .args([
            "diff",
            "--cached",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
        ])
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

/// What `git commit` does for the index, onto `reference`, named and not followed from `HEAD`:
/// the tree of the index, a commit on the reference's tip, and the reference moved to it.
async fn commit_staged_on(
    git: impl Fn() -> GitCmd,
    message: &str,
    author: &GitIdentity,
    reference: &str,
) -> WorkspaceResult<Option<String>> {
    let tip = git()
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{reference}^{{commit}}"))
        .run()
        .await?
        .stdout_text();
    let tree = git().arg("write-tree").run().await?.stdout_text();
    let tip_tree = git()
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{tip}^{{tree}}"))
        .run()
        .await?
        .stdout_text();
    if tree == tip_tree {
        return Ok(None);
    }
    let commit = git()
        .config("user.name", &author.name)
        .config("user.email", &author.email)
        .args(["commit-tree", &tree, "-p", &tip, "-m"])
        .arg(message)
        .run()
        .await?
        .stdout_text();
    let subject = message.lines().next().unwrap_or_default();
    git()
        .args(["update-ref", "-m"])
        .arg(format!("commit: {subject}"))
        .arg(reference)
        .arg(&commit)
        .arg(&tip)
        .run()
        .await?;
    Ok(Some(commit))
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

/// Whether `name` can be the name of a directory in the mirror's `worktrees/`: one plain path
/// component. The name is read from the coder's metadata, but a path is built from it.
pub(crate) fn is_admin_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
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
    fn a_status_is_said_in_words() {
        assert_eq!(FileStatus::Modified.to_string(), "modified");
        assert_eq!(FileStatus::TypeChanged.to_string(), "type changed");
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
