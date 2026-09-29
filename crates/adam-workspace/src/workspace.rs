//! Mirrors and per-run worktrees.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::credentials::DynGitCredentials;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::git::{Auth, GitCmd};
use crate::repo::{RepoLocation, RepoRef, fnv1a};
use crate::worktree::Worktree;

pub(crate) const REMOTE_TRACKING_PREFIX: &str = "refs/remotes/origin/";
const META_VERSION: u32 = 1;
const MAX_RUN_ID: usize = 128;

/// Owns the workspace root: shared mirrors under `<root>/git/`, one worktree
/// per run under `<root>/worktrees/`, and small per-run metadata under
/// `<root>/meta/` so a worktree can be found again after a restart.
///
/// # Layout
///
/// ```text
/// <root>/git/<host>/<owner>/<repo>.git   bare mirror, shared by every run
/// <root>/worktrees/<run>                 the run's working tree
/// <root>/meta/<run>.json                 repo url, base branch, branch (no secrets)
/// ```
///
/// # The mirror
///
/// The mirror is a *bare repository with a remote-tracking refspec*
/// (`git init --bare` + `remote.origin.fetch = +refs/heads/*:refs/remotes/origin/*`),
/// not a `git clone --mirror`. A `--mirror` clone maps remote heads onto
/// `refs/heads/*`, and `fetch --prune` would then reset or delete the local
/// `agent/*` branches that worktrees are checked out on. With this layout the
/// remote's branches live under `refs/remotes/origin/*` (so `origin/<base>`
/// resolves), and agent branches live under `refs/heads/agent/*`, untouched
/// by fetches.
///
/// # Concurrency
///
/// A per-mirror async lock serialises `fetch` and worktree add/remove (and the
/// few mirror-config writes) within this process. That is the whole story:
/// **one process per root** is assumed (the coder runs as a one-replica
/// StatefulSet with the root on its PVC), so there is no file lock. A second
/// process would not corrupt anything, since git's own lock files make it
/// fail loudly, but it may see spurious "could not lock" errors.
///
/// # Lifecycle
///
/// `remove` deletes the worktree and its metadata but deliberately keeps the
/// local `agent/*` branch: it is the only copy of any unpushed commits. The
/// branch stays owned by its run (`branch.<b>.adam-run` in the mirror's
/// config), so preparing the same run again re-attaches it.
///
/// Background `gc` is disabled for every command; nothing here needs it, and a
/// concurrent repack must not race with worktree operations.
#[derive(Clone)]
pub struct Workspaces {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    root: PathBuf,
    pub(crate) creds: DynGitCredentials,
    policy: Policy,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

/// Which repositories a [`Workspaces`] accepts. The default accepts
/// everything (the library predates the policy); see
/// [`Workspaces::allow_hosts`].
#[derive(Clone, Debug)]
struct Policy {
    /// `None`: any host. `Some`: only these (`host` or `host:port`).
    hosts: Option<Vec<String>>,
    /// Filesystem remotes and plain `http` (dev and test setups).
    allow_local: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            hosts: None,
            allow_local: true,
        }
    }
}

impl Policy {
    fn check(&self, loc: &RepoLocation) -> WorkspaceResult<()> {
        if loc.is_local() {
            return if self.allow_local {
                Ok(())
            } else {
                Err(WorkspaceError::Invalid(
                    "local repository paths and file:// URLs are not allowed".to_owned(),
                ))
            };
        }
        if !loc.is_secure() && !self.allow_local {
            return Err(WorkspaceError::Invalid(
                "repositories must be reached over https, not plain http".to_owned(),
            ));
        }
        match &self.hosts {
            Some(hosts) if !hosts.iter().any(|h| loc.matches_host(h)) => {
                Err(WorkspaceError::Invalid(format!(
                    "repository host {} is not allowed; allowed hosts: {}",
                    loc.host.replace('_', ":"),
                    if hosts.is_empty() {
                        "(none)".to_owned()
                    } else {
                        hosts.join(", ")
                    }
                )))
            }
            _ => Ok(()),
        }
    }
}

impl fmt::Debug for Workspaces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspaces")
            .field("root", &self.inner.root)
            .finish_non_exhaustive()
    }
}

/// Persisted next to (not inside) the worktree; contains no credentials.
#[derive(Serialize, Deserialize)]
struct Meta {
    version: u32,
    run: String,
    url: String,
    base_branch: String,
    branch: String,
}

impl Workspaces {
    /// Use `root` (created on demand) for mirrors and worktrees.
    ///
    /// `root` must be on persistent storage: uncommitted work in a worktree
    /// under `/tmp` did not survive restarts in earlier systems.
    pub fn new(root: PathBuf, creds: DynGitCredentials) -> Self {
        let root = std::path::absolute(&root).unwrap_or(root);
        Self {
            inner: Arc::new(Inner {
                root,
                creds,
                policy: Policy::default(),
                locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Accept only repositories on these hosts. An entry is a host name (any
    /// port) or `host:port`, compared case-insensitively; the list replaces an
    /// earlier one, and an empty list accepts nothing.
    ///
    /// **Call this whenever the repository URL comes from somebody else** (a
    /// model, a user, a webhook). The URL decides where `git` connects, and
    /// that is where the credentials are sent; without an allowlist, any host
    /// a caller names receives the token. A refused repository is
    /// [`WorkspaceError::Invalid`], reported before any process is spawned or
    /// any credential is requested. Pair it with a
    /// [`ScopedToken`](crate::ScopedToken) for defence in depth.
    ///
    /// Configure the value right after [`Workspaces::new`]; the per-mirror
    /// locks of an earlier handle are not shared with the returned one.
    #[must_use]
    pub fn allow_hosts<I, S>(self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let hosts = hosts
            .into_iter()
            .map(|h| h.as_ref().trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        self.with_policy(|p| p.hosts = Some(hosts))
    }

    /// Whether filesystem remotes (an absolute path or a `file://` URL) and
    /// plain `http://` remotes are accepted. Default: `true`, for
    /// backwards compatibility; production setups that take URLs from others
    /// pass `false`. Local remotes never receive credentials.
    ///
    /// Configure the value right after [`Workspaces::new`], like
    /// [`Workspaces::allow_hosts`].
    #[must_use]
    pub fn allow_local(self, allow: bool) -> Self {
        self.with_policy(|p| p.allow_local = allow)
    }

    fn with_policy(self, change: impl FnOnce(&mut Policy)) -> Self {
        let mut policy = self.inner.policy.clone();
        change(&mut policy);
        Self {
            inner: Arc::new(Inner {
                root: self.inner.root.clone(),
                creds: self.inner.creds.clone(),
                policy,
                locks: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Mirror at `<root>/git/<host>/<owner>/<repo>.git` (created or fetched),
    /// worktree at `<root>/worktrees/<run>`, on a new branch
    /// `agent/<run-short-id>` from `origin/<base_branch>`.
    ///
    /// Idempotent for the same run: if its worktree already exists (also after
    /// a process restart) it is returned as is, without fetching, and any
    /// uncommitted changes in it are untouched. If the worktree directory was
    /// lost but the run's branch survives in the mirror, the branch is
    /// re-attached, keeping its commits.
    ///
    /// `<run-short-id>` is the first 8 characters of `run` (restricted to
    /// `[A-Za-z0-9_-]`). If another run already owns that branch name (UUIDv7
    /// ids created within about a minute of each other share their first 8
    /// characters), the id is extended in steps of 4 characters until the name
    /// is free; the choice is recorded, so it is stable for the run.
    ///
    /// # Errors
    ///
    /// * [`WorkspaceError::Invalid`]: bad `run` (must be 1-128 characters of
    ///   `[A-Za-z0-9._-]`, not starting with `.`), bad url or base branch.
    /// * [`WorkspaceError::NotFound`]: the remote or `origin/<base_branch>`
    ///   does not exist.
    /// * [`WorkspaceError::Conflict`]: `run` is already bound to another repo.
    /// * [`WorkspaceError::Corrupt`]: the worktree path exists but is not a
    ///   worktree of this mirror; call [`Workspaces::remove`].
    /// * network and auth failures from `git fetch` (see
    ///   [`WorkspaceError::is_retryable`]).
    #[tracing::instrument(skip(self, repo), fields(repo = %repo.url, run = %run))]
    pub async fn prepare(&self, repo: &RepoRef, run: &str) -> WorkspaceResult<Worktree> {
        validate_run(run)?;
        // The URL is checked before anything else runs: a refused repository
        // costs no process, no request and no credential.
        let loc = repo.locate()?;
        self.inner.policy.check(&loc)?;
        self.inner.validate_base(&repo.base_branch).await?;
        let mirror = self.inner.root.join(loc.mirror_relative());

        let lock = self.inner.lock_for(&mirror);
        let _guard = lock.lock().await;

        let path = self.inner.worktree_path(run);
        let meta = self.inner.read_meta(run).await?;
        if let Some(m) = &meta {
            if m.url != repo.url {
                return Err(WorkspaceError::Conflict(format!(
                    "run {run} is already bound to {}, not {}",
                    m.url, repo.url
                )));
            }
            if self.inner.is_valid_worktree(&path, &mirror).await {
                return Ok(self.inner.worktree(m, path, mirror));
            }
        }
        if exists(&path).await? {
            return Err(WorkspaceError::Corrupt(format!(
                "{} exists but is not a worktree of {}; remove the run first",
                path.display(),
                mirror.display()
            )));
        }

        self.inner.ensure_mirror(repo, &loc, &mirror).await?;
        let auth = self.inner.authorize(repo, &loc).await?;
        self.inner
            .mirror_git(&mirror)
            .args(["fetch", "--prune", "--quiet", "origin"])
            .maybe_auth(auth)
            .run()
            .await?;

        let base_ref = format!("{REMOTE_TRACKING_PREFIX}{}", repo.base_branch);
        let base_exists = self
            .inner
            .mirror_git(&mirror)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{base_ref}^{{commit}}"))
            .run_status()
            .await?
            .success;
        if !base_exists {
            return Err(WorkspaceError::NotFound(format!(
                "branch {} does not exist on {}",
                repo.base_branch, repo.url
            )));
        }

        let (meta, fresh) = match meta {
            Some(m) => (m, false),
            None => {
                let branch = self.inner.pick_branch(&mirror, run).await?;
                (
                    Meta {
                        version: META_VERSION,
                        run: run.to_owned(),
                        url: repo.url.clone(),
                        base_branch: repo.base_branch.clone(),
                        branch,
                    },
                    true,
                )
            }
        };

        // Record ownership and intent before touching the worktree, so a crash
        // in between is repaired by the next `prepare` instead of leaking.
        self.inner
            .mirror_git(&mirror)
            .args(["config", &format!("branch.{}.adam-run", meta.branch), run])
            .run()
            .await?;
        self.inner.write_meta(&meta).await?;

        let added = self
            .inner
            .add_worktree(&mirror, &path, &meta, &base_ref)
            .await;
        if let Err(e) = added {
            if fresh {
                let _ = tokio::fs::remove_file(self.inner.meta_path(run)).await;
            }
            return Err(e);
        }
        Ok(self.inner.worktree(&meta, path, mirror))
    }

    /// The worktree of `run`, if it exists on disk: how a restarted process
    /// finds its way back to a run's workspace.
    ///
    /// Returns `Ok(None)` when the run was never prepared, was removed, or its
    /// directory is gone (call [`Workspaces::prepare`] to recreate it).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for a bad run id, [`WorkspaceError::Corrupt`]
    /// for unreadable metadata.
    #[tracing::instrument(skip(self))]
    pub async fn open_existing(&self, run: &str) -> WorkspaceResult<Option<Worktree>> {
        validate_run(run)?;
        let Some(meta) = self.inner.read_meta(run).await? else {
            return Ok(None);
        };
        let loc = RepoRef::new(&meta.url, &meta.base_branch).locate()?;
        let mirror = self.inner.root.join(loc.mirror_relative());
        let path = self.inner.worktree_path(run);
        if self.inner.is_valid_worktree(&path, &mirror).await {
            Ok(Some(self.inner.worktree(&meta, path, mirror)))
        } else {
            Ok(None)
        }
    }

    /// Delete the run's worktree (including uncommitted changes) and prune the
    /// mirror's bookkeeping. Idempotent. The run's branch is kept; see the
    /// type documentation.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for a bad run id, I/O or git failures.
    #[tracing::instrument(skip(self))]
    pub async fn remove(&self, run: &str) -> WorkspaceResult<()> {
        validate_run(run)?;
        let path = self.inner.worktree_path(run);
        let Some(meta) = self.inner.read_meta(run).await? else {
            // Never prepared, or a crash left a bare directory: nothing to
            // unregister, just make sure the directory is gone.
            return remove_dir_if_exists(&path).await;
        };
        let loc = RepoRef::new(&meta.url, &meta.base_branch).locate()?;
        let mirror = self.inner.root.join(loc.mirror_relative());

        let lock = self.inner.lock_for(&mirror);
        let _guard = lock.lock().await;

        let have_mirror = exists(&mirror.join("HEAD")).await?;
        if have_mirror {
            // Failure is fine (e.g. not registered any more); the directory is
            // removed below regardless.
            let _ = self
                .inner
                .mirror_git(&mirror)
                .args(["worktree", "remove", "--force"])
                .arg(&path)
                .run_status()
                .await?;
        }
        remove_dir_if_exists(&path).await?;
        if have_mirror {
            self.inner
                .mirror_git(&mirror)
                .args(["worktree", "prune"])
                .run()
                .await?;
        }
        match tokio::fs::remove_file(self.inner.meta_path(run)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(WorkspaceError::io("cannot remove run metadata", e)),
        }
    }
}

impl Inner {
    fn worktree_path(&self, run: &str) -> PathBuf {
        self.root.join("worktrees").join(run)
    }

    fn meta_path(&self, run: &str) -> PathBuf {
        self.root.join("meta").join(format!("{run}.json"))
    }

    pub(crate) fn lock_for(&self, mirror: &Path) -> Arc<tokio::sync::Mutex<()>> {
        // The std mutex is never held across an await.
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.entry(mirror.to_owned()).or_default().clone()
    }

    /// A command that is confined to the workspace root.
    pub(crate) fn git(&self) -> GitCmd {
        GitCmd::new().ceiling(&self.root)
    }

    /// A command on the bare mirror, addressed explicitly so that a damaged
    /// mirror can never make git operate on an enclosing repository.
    pub(crate) fn mirror_git(&self, mirror: &Path) -> GitCmd {
        self.git().git_dir(mirror)
    }

    /// The credentials for talking to `repo`'s remote, after the policy said
    /// the remote is acceptable. The only place a token is requested: nothing
    /// is asked of [`GitCredentials`](crate::GitCredentials) for a refused
    /// host, and filesystem remotes need (and get) none.
    pub(crate) async fn authorize(
        &self,
        repo: &RepoRef,
        loc: &RepoLocation,
    ) -> WorkspaceResult<Option<Auth>> {
        self.policy.check(loc)?;
        let Some(scope) = loc.http_scope() else {
            return Ok(None);
        };
        let token = self.creds.token_for(repo).await?;
        Ok(Some(Auth::new(token, Some(scope))))
    }

    fn worktree(self: &Arc<Self>, meta: &Meta, path: PathBuf, mirror: PathBuf) -> Worktree {
        Worktree::new(
            Arc::clone(self),
            meta.run.clone(),
            RepoRef::new(&meta.url, &meta.base_branch),
            path,
            meta.branch.clone(),
            mirror,
        )
    }

    async fn validate_base(&self, base: &str) -> WorkspaceResult<()> {
        let ok = !base.is_empty()
            && !base.starts_with('-')
            && self
                .git()
                .args(["check-ref-format"])
                .arg(format!("refs/heads/{base}"))
                .run_status()
                .await?
                .success;
        if ok {
            Ok(())
        } else {
            Err(WorkspaceError::Invalid(format!(
                "{base:?} is not a valid branch name"
            )))
        }
    }

    /// Create the bare mirror if needed and (re)write its remote config.
    async fn ensure_mirror(
        &self,
        repo: &RepoRef,
        loc: &RepoLocation,
        mirror: &Path,
    ) -> WorkspaceResult<()> {
        if !exists(&mirror.join("HEAD")).await? {
            if let Some(parent) = mirror.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| WorkspaceError::io("cannot create mirror directory", e))?;
            }
            self.git()
                .args(["init", "--bare", "--quiet"])
                .arg(mirror)
                .run()
                .await?;
        }
        let git = || self.mirror_git(mirror).arg("config");
        // For http(s), the URL rebuilt from the parsed parts: git connects to
        // exactly the host the policy approved, however the raw string parses.
        let url = loc.canonical_http_url().unwrap_or(&repo.url);
        git().args(["remote.origin.url", url]).run().await?;
        git()
            .args([
                "remote.origin.fetch",
                &format!("+refs/heads/*:{REMOTE_TRACKING_PREFIX}*"),
            ])
            .run()
            .await?;
        Ok(())
    }

    /// First free `agent/<short>` name for `run` (see [`Workspaces::prepare`]).
    async fn pick_branch(&self, mirror: &Path, run: &str) -> WorkspaceResult<String> {
        let filtered: String = run
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let mut candidates = Vec::new();
        let mut len = 8;
        while len < filtered.len() {
            candidates.push(format!("agent/{}", &filtered[..len]));
            len += 4;
        }
        candidates.push(format!("agent/{}", &filtered[..filtered.len().min(len)]));
        // Reachable only when distinct valid ids filter to the same string.
        candidates.push(format!(
            "agent/{filtered}-{:08x}",
            fnv1a(run.as_bytes()) as u32
        ));
        candidates.dedup();

        for candidate in candidates {
            if self.branch_is_available(mirror, &candidate, run).await? {
                return Ok(candidate);
            }
        }
        Err(WorkspaceError::Conflict(format!(
            "no free agent/* branch name for run {run}"
        )))
    }

    /// Free, or already owned by `run`.
    async fn branch_is_available(
        &self,
        mirror: &Path,
        branch: &str,
        run: &str,
    ) -> WorkspaceResult<bool> {
        let owner = self
            .mirror_git(mirror)
            .args(["config", "--get"])
            .arg(format!("branch.{branch}.adam-run"))
            .run_status()
            .await?;
        if owner.success {
            return Ok(owner.stdout_text() == run);
        }
        for prefix in ["refs/heads/", REMOTE_TRACKING_PREFIX] {
            let taken = self
                .mirror_git(mirror)
                .args(["show-ref", "--verify", "--quiet"])
                .arg(format!("{prefix}{branch}"))
                .run_status()
                .await?
                .success;
            if taken {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn add_worktree(
        &self,
        mirror: &Path,
        path: &Path,
        meta: &Meta,
        base_ref: &str,
    ) -> WorkspaceResult<()> {
        // Forget worktrees whose directories vanished, so their branches are
        // not considered checked out.
        self.mirror_git(mirror)
            .args(["worktree", "prune"])
            .run()
            .await?;
        let branch_ref = format!("refs/heads/{}", meta.branch);
        let branch_exists = self
            .mirror_git(mirror)
            .args(["show-ref", "--verify", "--quiet"])
            .arg(&branch_ref)
            .run_status()
            .await?
            .success;
        let mut cmd = self.mirror_git(mirror).args(["worktree", "add", "--quiet"]);
        cmd = if branch_exists {
            // Re-attach the run's surviving branch, keeping its commits.
            cmd.arg(path).arg(&meta.branch)
        } else {
            cmd.args(["--no-track", "-b", &meta.branch])
                .arg(path)
                .arg(base_ref)
        };
        cmd.run().await?;
        Ok(())
    }

    /// `path` is a checked-out worktree whose git dir belongs to `mirror`.
    async fn is_valid_worktree(&self, path: &Path, mirror: &Path) -> bool {
        if !matches!(tokio::fs::metadata(path.join(".git")).await, Ok(m) if m.is_file()) {
            return false;
        }
        let out = self
            .git()
            .cwd(path)
            .args(["rev-parse", "--is-inside-work-tree", "--git-common-dir"])
            .run_status()
            .await;
        let Ok(out) = out else { return false };
        if !out.success {
            return false;
        }
        let text = out.stdout_text();
        let mut lines = text.lines();
        if lines.next() != Some("true") {
            return false;
        }
        let Some(common) = lines.next() else {
            return false;
        };
        let common = path.join(common);
        match (
            tokio::fs::canonicalize(&common).await,
            tokio::fs::canonicalize(mirror).await,
        ) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }

    async fn read_meta(&self, run: &str) -> WorkspaceResult<Option<Meta>> {
        let path = self.meta_path(run);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(WorkspaceError::io("cannot read run metadata", e)),
        };
        let meta: Meta = serde_json::from_slice(&bytes).map_err(|e| {
            WorkspaceError::Corrupt(format!("{} is not valid metadata: {e}", path.display()))
        })?;
        if meta.version != META_VERSION || meta.run != run {
            return Err(WorkspaceError::Corrupt(format!(
                "{} has unexpected version or run",
                path.display()
            )));
        }
        Ok(Some(meta))
    }

    /// Write-then-rename, so a crash never leaves half a file.
    async fn write_meta(&self, meta: &Meta) -> WorkspaceResult<()> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = self.meta_path(&meta.run);
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| WorkspaceError::io("cannot create metadata directory", e))?;
        }
        let tmp = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(meta)
            .map_err(|e| WorkspaceError::Corrupt(format!("cannot serialise metadata: {e}")))?;
        tokio::fs::write(&tmp, bytes)
            .await
            .map_err(|e| WorkspaceError::io("cannot write run metadata", e))?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| WorkspaceError::io("cannot install run metadata", e))
    }
}

/// Run ids become directory and branch names, so they are restricted.
fn validate_run(run: &str) -> WorkspaceResult<()> {
    let ok = !run.is_empty()
        && run.len() <= MAX_RUN_ID
        && !run.starts_with('.')
        && run
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(WorkspaceError::Invalid(format!(
            "run id {run:?} must be 1-{MAX_RUN_ID} characters of [A-Za-z0-9._-] and not start with '.'"
        )))
    }
}

async fn exists(path: &Path) -> WorkspaceResult<bool> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|e| WorkspaceError::io(format!("cannot stat {}", path.display()), e))
}

async fn remove_dir_if_exists(path: &Path) -> WorkspaceResult<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WorkspaceError::io(
            format!("cannot remove {}", path.display()),
            e,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_are_validated() {
        for ok in ["018f3a2b-7c1d-7000-8000-000000000001", "run_1.a-b", "a"] {
            assert!(validate_run(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "..", "a/b", "a b", "a\nb", &"x".repeat(129)] {
            assert!(validate_run(bad).is_err(), "{bad:?}");
        }
    }
}
