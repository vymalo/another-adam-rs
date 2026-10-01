//! Mirrors and per-run worktrees.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::credentials::DynGitCredentials;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::git::{Auth, GitCmd};
use crate::repo::{RepoLocation, RepoRef, fnv1a};
use crate::run_workspace::RunWorkspace;
use crate::worktree::{GitIdentity, Worktree};

pub(crate) const REMOTE_TRACKING_PREFIX: &str = "refs/remotes/origin/";
/// The namespace of the branches worktrees are checked out on, and the only one that can be
/// continued ([`Workspaces::prepare_continuing`]).
const AGENT_BRANCH_PREFIX: &str = "agent/";
/// How many of the remote's branches the error about a missing branch names.
const MAX_BRANCHES_LISTED: usize = 30;
/// The metadata of the legacy layout: one worktree per run.
const META_VERSION: u32 = 1;
/// The metadata of a slot of a [`RunWorkspace`].
pub(crate) const SLOT_META_VERSION: u32 = 2;
const MAX_RUN_ID: usize = 128;
/// The empty tree of git (`git hash-object -t tree /dev/null`): what the first commit of an empty
/// repository points at.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Owns the workspace root: shared mirrors under `<root>/git/`, a workspace per run under
/// `<root>/workspaces/` (a directory of slots, each a repository's worktree or a scratch project:
/// see [`RunWorkspace`]), and small per-run metadata under `<root>/meta/` so a workspace can be
/// found again after a restart.
///
/// # Layout
///
/// ```text
/// <root>/git/<host>/<owner>/<repo>.git   bare mirror, shared by every run
/// <root>/workspaces/<run>/<dir>/         a slot: a worktree on agent/<run-short-id>, or a scratch project
/// <root>/workspaces/<run>.lock           the run's lock, beside its directory (see Concurrency)
/// <root>/meta/<run>/<dir>.json           the slot's metadata (version 2; no secrets)
/// <root>/worktrees/<run>                 legacy: the run's one working tree (see below)
/// <root>/meta/<run>.json                 legacy: repo url, base branch, branch (no secrets)
/// ```
///
/// A run's workspace is [`Workspaces::run`]. The **legacy** layout, one worktree per run, is what
/// [`Workspaces::prepare`] and [`Workspaces::prepare_continuing`] still make (single-repository
/// helpers, and what a workspace made before slots existed holds): a [`RunWorkspace`] reads it as a
/// slot named after the repository and removes it with the rest, but never makes one.
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
/// few mirror-config writes) within this process. Under it, an exclusive
/// advisory file lock on `<mirror>.lock` (`std::fs::File::lock`, `flock(2)` on
/// Linux) serialises them across processes, so several workers can share one
/// root (the `shared` workspace placement) without git's own lock files making
/// one of them fail with "could not lock". The lock file sits next to the
/// mirror directory, never inside it, and the lock goes away with the file
/// handle, so a crashed process frees it. Both locks are held for the same
/// stretch: the async one first, then the file lock (taken on a blocking
/// thread), released in the opposite order.
///
/// The same pair of locks, on `<root>/workspaces/<run>.lock`, makes **one run's workspace** change
/// by one at a time: adding a slot (which picks its directory and its place in the order slots
/// joined), and removing the workspace. The run's lock is taken before the mirror's, never the
/// other way round, and never while holding another run's.
///
/// A root used by one process pays for one uncontended `flock` per operation.
/// The volume must honour file locks. *Unverified* for NFS and for Longhorn RWX:
/// test two workers against one repository before relying on it. A volume that
/// refuses the lock makes `prepare`, `remove` and `push` fail with
/// [`WorkspaceError::Io`] instead of running unlocked.
///
/// # Lifecycle
///
/// [`RunWorkspace::remove`] (and [`Workspaces::remove`], which does it) deletes the worktrees,
/// the scratch projects and the metadata, but deliberately keeps the
/// local `agent/*` branches: they are the only copy of any unpushed commits. A branch
/// stays owned by its run (`branch.<b>.adam-run` in the mirror's
/// config), so preparing the same run again re-attaches it.
///
/// Background `gc` is disabled for every command; nothing here needs it, and a
/// concurrent repack must not race with worktree operations.
#[derive(Clone)]
pub struct Workspaces {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    root: PathBuf,
    pub(crate) creds: DynGitCredentials,
    policy: Policy,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

/// Held while a mirror is being changed: the in-process lock, then the file lock. Fields drop
/// in order, so the file lock is released first.
#[must_use = "the mirror is only locked while the guard is alive"]
pub(crate) struct MirrorGuard {
    _file: std::fs::File,
    _local: tokio::sync::OwnedMutexGuard<()>,
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

/// Persisted next to (not inside) the worktree; contains no credentials. Version 1 is the legacy
/// worktree of a run (`<root>/meta/<run>.json`); version 2 is a slot of a [`RunWorkspace`]
/// (`<root>/meta/<run>/<dir>.json`) and adds its `dir`, its `seq` and `kind: "repo"`.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Meta {
    pub(crate) version: u32,
    pub(crate) run: String,
    pub(crate) url: String,
    pub(crate) base_branch: String,
    /// The local branch the worktree has checked out (`agent/<run-short-id>`).
    pub(crate) branch: String,
    /// The pushed branch this run continues, if it does: the name its commits are published
    /// under. Absent for a run on a branch of its own (and in metadata written before it existed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote_branch: Option<String>,
    /// Version 2: the slot's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dir: Option<String>,
    /// Version 2: the order slots joined the run, from 1 (the legacy worktree is 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seq: Option<u32>,
    /// Version 2: `"repo"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
}

/// The metadata of a scratch slot (version 2).
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct ScratchMeta {
    pub(crate) version: u32,
    pub(crate) run: String,
    pub(crate) dir: String,
    pub(crate) seq: u32,
    /// `"scratch"`.
    pub(crate) kind: String,
    /// The repository whose slot the project's files were last copied into (its url as the
    /// caller wrote it, never a credential). Absent for a project that was never published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) published_to: Option<String>,
}

/// What one metadata file of a slot says.
#[derive(Clone)]
pub(crate) enum SlotMeta {
    Repo(Meta),
    Scratch(ScratchMeta),
}

impl SlotMeta {
    pub(crate) fn dir(&self) -> &str {
        match self {
            Self::Repo(m) => m.dir.as_deref().unwrap_or_default(),
            Self::Scratch(m) => &m.dir,
        }
    }

    pub(crate) fn seq(&self) -> u32 {
        match self {
            Self::Repo(m) => m.seq.unwrap_or(0),
            Self::Scratch(m) => m.seq,
        }
    }
}

/// Where a worktree is made, and the metadata file that records it.
#[derive(Clone)]
pub(crate) struct Target {
    /// The worktree directory.
    pub(crate) path: PathBuf,
    /// The metadata file.
    pub(crate) meta_path: PathBuf,
    /// The slot's directory name.
    pub(crate) dir: String,
    /// `Some` for a slot of a [`RunWorkspace`] (metadata version 2): its place in the order slots
    /// joined. `None` for the legacy worktree of a run.
    pub(crate) seq: Option<u32>,
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

    /// Whether the policy ([`allow_hosts`](Self::allow_hosts), [`allow_local`](Self::allow_local))
    /// accepts `repo`: the check every operation that takes a repository makes first, for a caller
    /// that wants to refuse (or to ask a person about) a repository before anything else happens.
    /// Nothing is spawned, requested or written.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] when the repository's address cannot be read or its host or kind
    /// is not allowed, with the reason.
    pub fn check_repository(&self, repo: &RepoRef) -> WorkspaceResult<()> {
        self.check_repo(repo).map(|_| ())
    }

    /// `repo`'s location, once the policy has accepted it: a refused repository is an error
    /// before any process is spawned or any credential is requested.
    pub(crate) fn check_repo(&self, repo: &RepoRef) -> WorkspaceResult<RepoLocation> {
        let loc = repo.locate()?;
        self.inner.policy.check(&loc)?;
        Ok(loc)
    }

    /// The **single-repository helper**: the run's one worktree, in the legacy layout. For a
    /// workspace of several repositories and scratch projects use [`Workspaces::run`].
    ///
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
    ///   [`Classify::is_retryable`](adam_error::Classify::is_retryable)).
    #[tracing::instrument(skip(self, repo), fields(repo = %repo.url, run = %run))]
    pub async fn prepare(&self, repo: &RepoRef, run: &str) -> WorkspaceResult<Worktree> {
        self.prepare_on(repo, run, None).await
    }

    /// Like [`prepare`](Self::prepare), but the worktree **continues a branch that was pushed
    /// before** (by an earlier run, say): it starts from `origin/<existing>` instead of
    /// `origin/<base_branch>`. [`Worktree::push`] publishes its commits to the run's own branch,
    /// and only [`Worktree::publish`] moves `existing` to them (a pull request from `existing` is
    /// updated by that, when the caller decides the work may be published there).
    /// [`Worktree::branch`] is `existing`.
    ///
    /// The run still has a local branch of its own (`agent/<run-short-id>`), which is what is
    /// checked out and pushed, so the worktree never collides with the one of the run that pushed
    /// `existing`, and `publish` stays a fast-forward (never forced): if `existing` has moved on
    /// the remote in a way that is not a fast-forward of this run's work, `publish` refuses
    /// ([`WorkspaceError::Conflict`]) and `existing` is untouched.
    ///
    /// `existing` must be one of this crate's own branches, `agent/<...>`, so that a caller that
    /// takes the name from somebody else can never publish onto `main` or onto a person's branch,
    /// and it must exist on the remote (it is whatever the fetch found).
    ///
    /// Idempotent like `prepare`; a run that is already bound to a different branch (or to a
    /// branch of its own) is a [`WorkspaceError::Conflict`].
    ///
    /// # Errors
    ///
    /// As [`prepare`](Self::prepare), and: [`WorkspaceError::Invalid`] when `existing` is not an
    /// `agent/*` branch name or is the base branch, [`WorkspaceError::NotFound`] when it does not
    /// exist on the remote, [`WorkspaceError::Conflict`] as above.
    #[tracing::instrument(skip(self, repo), fields(repo = %repo.url, run = %run, existing = %existing))]
    pub async fn prepare_continuing(
        &self,
        repo: &RepoRef,
        run: &str,
        existing: &str,
    ) -> WorkspaceResult<Worktree> {
        self.prepare_on(repo, run, Some(existing)).await
    }

    async fn prepare_on(
        &self,
        repo: &RepoRef,
        run: &str,
        existing: Option<&str>,
    ) -> WorkspaceResult<Worktree> {
        validate_run(run)?;
        let loc = repo.locate()?;
        let target = self.inner.legacy_target(run, &loc);
        self.prepare_at(repo, run, existing, &target).await
    }

    /// Make (or find again) the worktree of `repo` for `run` at `target`: what [`prepare`] does
    /// for the legacy worktree and a [`RunWorkspace`] does for a slot.
    ///
    /// [`prepare`]: Self::prepare
    pub(crate) async fn prepare_at(
        &self,
        repo: &RepoRef,
        run: &str,
        existing: Option<&str>,
        target: &Target,
    ) -> WorkspaceResult<Worktree> {
        validate_run(run)?;
        // The URL is checked before anything else runs: a refused repository
        // costs no process, no request and no credential.
        let loc = repo.locate()?;
        self.inner.policy.check(&loc)?;
        self.inner.validate_base(&repo.base_branch).await?;
        if let Some(existing) = existing {
            self.inner
                .validate_continued(existing, &repo.base_branch)
                .await?;
        }
        let mirror = self.inner.root.join(loc.mirror_relative());

        let _guard = self.inner.lock_mirror(&mirror).await?;

        let path = &target.path;
        let meta = self
            .inner
            .read_meta_at(&target.meta_path, run, target.seq.is_some())
            .await?;
        if let Some(m) = &meta {
            if m.url != repo.url {
                return Err(WorkspaceError::Conflict(format!(
                    "run {run} is already bound to {}, not {}",
                    m.url, repo.url
                )));
            }
            // Asking again for what the run has is fine; asking for another branch is not. A
            // plain `prepare` of a run that continues a branch just finds its worktree.
            if let Some(existing) = existing
                && m.remote_branch.as_deref() != Some(existing)
            {
                return Err(WorkspaceError::Conflict(match &m.remote_branch {
                    Some(bound) => format!("run {run} already continues {bound}, not {existing}"),
                    None => format!(
                        "run {run} already has a branch of its own ({}) and cannot continue {existing}",
                        m.branch
                    ),
                }));
            }
            if self.inner.is_valid_worktree(path, &mirror).await {
                return Ok(self.inner.worktree(m, &target.dir, path.clone(), mirror));
            }
        }
        if exists(path).await? {
            return Err(WorkspaceError::Corrupt(format!(
                "{} exists but is not a worktree of {}; remove the run first",
                path.display(),
                mirror.display()
            )));
        }

        self.inner.ensure_mirror(repo, &loc, &mirror).await?;
        let auth = self.inner.authorize(repo, &loc).await?;
        // The URL and the refspec are named, not read from the remote called `origin`: what
        // carries the credentials does not depend on the mirror's config.
        self.inner
            .mirror_git(&mirror)
            .args(["fetch", "--prune", "--quiet"])
            .arg(loc.remote_url(&repo.url))
            .arg(format!("+refs/heads/*:{REMOTE_TRACKING_PREFIX}*"))
            .maybe_auth(auth)
            .run()
            .await?;

        let base_ref = format!("{REMOTE_TRACKING_PREFIX}{}", repo.base_branch);
        if !self.inner.has_commit(&mirror, &base_ref).await? {
            return Err(WorkspaceError::NotFound(format!(
                "branch {} does not exist on {}. {}",
                repo.base_branch,
                repo.url,
                self.inner.branches_said(&mirror).await
            )));
        }
        // What the worktree starts from: the base, or the branch it continues.
        let start_ref = match existing {
            Some(existing) => {
                let continued = format!("{REMOTE_TRACKING_PREFIX}{existing}");
                if !self.inner.has_commit(&mirror, &continued).await? {
                    return Err(WorkspaceError::NotFound(format!(
                        "branch {existing} does not exist on {}: it was never pushed there",
                        repo.url
                    )));
                }
                continued
            }
            None => base_ref,
        };

        let (meta, fresh) = match meta {
            Some(m) => (m, false),
            None => {
                let branch = self.inner.pick_branch(&mirror, run).await?;
                (
                    Meta {
                        version: if target.seq.is_some() {
                            SLOT_META_VERSION
                        } else {
                            META_VERSION
                        },
                        run: run.to_owned(),
                        url: repo.url.clone(),
                        base_branch: repo.base_branch.clone(),
                        branch,
                        remote_branch: existing.map(str::to_owned),
                        dir: target.seq.map(|_| target.dir.clone()),
                        seq: target.seq,
                        kind: target.seq.map(|_| "repo".to_owned()),
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
        self.inner.write_meta_at(&target.meta_path, &meta).await?;

        let added = self
            .inner
            .add_worktree(&mirror, path, &meta, &start_ref)
            .await;
        if let Err(e) = added {
            if fresh {
                let _ = tokio::fs::remove_file(&target.meta_path).await;
            }
            return Err(e);
        }
        Ok(self
            .inner
            .worktree(&meta, &target.dir, path.clone(), mirror))
    }

    /// The default branch of the repository at `repo_url`: what the remote's `HEAD` points at
    /// (`git ls-remote --symref origin HEAD`), for a caller that was not told which branch to
    /// start from. Nothing is checked out, and the mirror is created on first use like
    /// [`prepare`](Self::prepare) does.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for a url the policy refuses,
    /// [`WorkspaceError::NotFound`] when the remote has no `HEAD` that names a branch (an empty
    /// repository), and the network and auth failures of `git ls-remote`.
    #[tracing::instrument(skip(self))]
    pub async fn default_branch(&self, repo_url: &str) -> WorkspaceResult<String> {
        // Only the url matters here; the base branch is what the caller is asking for.
        let repo = RepoRef::new(repo_url, "HEAD");
        let loc = repo.locate()?;
        self.inner.policy.check(&loc)?;
        let mirror = self.inner.root.join(loc.mirror_relative());
        let _guard = self.inner.lock_mirror(&mirror).await?;
        self.inner.ensure_mirror(&repo, &loc, &mirror).await?;
        let auth = self.inner.authorize(&repo, &loc).await?;
        let out = self
            .inner
            .mirror_git(&mirror)
            .args(["ls-remote", "--symref"])
            .arg(loc.remote_url(&repo.url))
            .arg("HEAD")
            .maybe_auth(auth)
            .run()
            .await?;
        parse_symref_head(&out.stdout_text()).ok_or_else(|| {
            WorkspaceError::NotFound(format!(
                "{repo_url} has no default branch (is the repository empty?)"
            ))
        })
    }

    /// The legacy worktree of `run` (the one [`prepare`](Self::prepare) makes), if it exists on
    /// disk: how a restarted process finds its way back to a run's workspace. A single-repository
    /// helper: the slots of a [`RunWorkspace`] are [`RunWorkspace::slots`].
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
        self.inner.open_legacy(run).await
    }

    /// Delete everything of the run's workspace: every slot (a worktree with its uncommitted
    /// changes, a scratch project), the legacy worktree and the metadata, and prune the mirrors'
    /// bookkeeping. Idempotent. The run's `agent/*` branches are kept; see the type documentation.
    /// This is [`RunWorkspace::remove`].
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for a bad run id, I/O or git failures.
    #[tracing::instrument(skip(self))]
    pub async fn remove(&self, run: &str) -> WorkspaceResult<()> {
        self.run(run)?.remove().await
    }

    /// The workspace of `run`: a directory of slots, each a repository's worktree or a scratch
    /// project. Nothing is created or read until a method of the [`RunWorkspace`] is called.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for a bad run id (1-128 characters of `[A-Za-z0-9._-]`, not
    /// starting with `.`).
    pub fn run(&self, run: &str) -> WorkspaceResult<RunWorkspace> {
        validate_run(run)?;
        Ok(RunWorkspace::new(self.clone(), run))
    }

    /// The runs that have a workspace on disk, in either layout (slots, or the legacy worktree),
    /// sorted: what a sweep of finished runs walks. A run whose workspace is partly gone is
    /// listed too, so that [`RunWorkspace::remove`] can finish the job.
    ///
    /// # Errors
    ///
    /// I/O errors listing the root.
    pub async fn runs(&self) -> WorkspaceResult<Vec<String>> {
        let mut runs = std::collections::BTreeSet::new();
        let root = &self.inner.root;
        // A directory per run in each of these, and the legacy `<run>.json` files in `meta/`.
        for (dir, files) in [("workspaces", false), ("meta", true), ("worktrees", false)] {
            let mut entries = match tokio::fs::read_dir(root.join(dir)).await {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(WorkspaceError::io(format!("cannot list {dir}"), e)),
            };
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| WorkspaceError::io(format!("cannot list {dir}"), e))?
            {
                let Ok(kind) = entry.file_type().await else {
                    continue;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let name = if kind.is_dir() {
                    name
                } else if files && kind.is_file() {
                    match name.strip_suffix(".json") {
                        Some(run) => run.to_owned(),
                        None => continue,
                    }
                } else {
                    continue;
                };
                if validate_run(&name).is_ok() {
                    runs.insert(name);
                }
            }
        }
        Ok(runs.into_iter().collect())
    }

    /// The `git ls-remote` of the repository at `repo_url`: what the remote advertises, one `<sha>
    /// <ref>` line each. Nothing is checked out; the mirror is created on first use like
    /// [`default_branch`](Self::default_branch) does.
    async fn remote_refs(&self, repo: &RepoRef) -> WorkspaceResult<Vec<String>> {
        let loc = repo.locate()?;
        self.inner.policy.check(&loc)?;
        let mirror = self.inner.root.join(loc.mirror_relative());
        let _guard = self.inner.lock_mirror(&mirror).await?;
        self.inner.ensure_mirror(repo, &loc, &mirror).await?;
        let auth = self.inner.authorize(repo, &loc).await?;
        let out = self
            .inner
            .mirror_git(&mirror)
            .arg("ls-remote")
            .arg(loc.remote_url(&repo.url))
            .maybe_auth(auth)
            .run()
            .await?;
        Ok(out.stdout_text().lines().map(str::to_owned).collect())
    }

    /// Whether the remote at `repo_url` has no refs at all (`git ls-remote` prints nothing): a
    /// repository that was just created is empty, and has no branch to start a worktree from.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for a url the policy refuses, [`WorkspaceError::NotFound`] when
    /// the repository does not exist, and the network and auth failures of `git ls-remote`.
    #[tracing::instrument(skip(self))]
    pub async fn remote_is_empty(&self, repo_url: &str) -> WorkspaceResult<bool> {
        Ok(self
            .remote_refs(&RepoRef::new(repo_url, "HEAD"))
            .await?
            .is_empty())
    }

    /// Give an **empty** remote its first commit: a commit of the empty tree (message `Initial
    /// commit`, by `identity`), pushed as `repo.base_branch`, never forced. It is the only
    /// thing this crate pushes outside `agent/*`, and it lets a worktree start from the base of
    /// a repository that had none. Returns the commit's sha.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Conflict`] when the remote has any ref (it is not empty, or somebody
    /// pushed meanwhile): nothing is pushed. [`WorkspaceError::Invalid`] for a url the policy
    /// refuses, a bad base branch or identity. The network and auth failures of `git`.
    #[tracing::instrument(skip(self, repo, identity), fields(repo = %repo.url))]
    pub async fn initialize_empty(
        &self,
        repo: &RepoRef,
        identity: &GitIdentity,
    ) -> WorkspaceResult<String> {
        identity.validate()?;
        let loc = repo.locate()?;
        self.inner.policy.check(&loc)?;
        self.inner.validate_base(&repo.base_branch).await?;
        let mirror = self.inner.root.join(loc.mirror_relative());
        let _guard = self.inner.lock_mirror(&mirror).await?;
        self.inner.ensure_mirror(repo, &loc, &mirror).await?;
        let url = loc.remote_url(&repo.url);
        let auth = self.inner.authorize(repo, &loc).await?;
        let refs = self
            .inner
            .mirror_git(&mirror)
            .arg("ls-remote")
            .arg(url)
            .maybe_auth(auth)
            .run()
            .await?
            .stdout_text();
        if !refs.is_empty() {
            return Err(WorkspaceError::Conflict(format!(
                "{} is not empty: it has refs, so it is not given a first commit",
                repo.url
            )));
        }
        // `mktree` of nothing writes the empty tree into the mirror, so the commit can point at it.
        let tree = self
            .inner
            .mirror_git(&mirror)
            .arg("mktree")
            .run()
            .await?
            .stdout_text();
        if tree != EMPTY_TREE {
            return Err(WorkspaceError::Corrupt(format!(
                "git made the tree {tree} from nothing, not the empty tree"
            )));
        }
        let sha = self
            .inner
            .mirror_git(&mirror)
            .env("GIT_AUTHOR_NAME", &identity.name)
            .env("GIT_AUTHOR_EMAIL", &identity.email)
            .env("GIT_COMMITTER_NAME", &identity.name)
            .env("GIT_COMMITTER_EMAIL", &identity.email)
            .args(["commit-tree", EMPTY_TREE, "-m", "Initial commit"])
            .run()
            .await?
            .stdout_text();
        let auth = self.inner.authorize(repo, &loc).await?;
        self.inner.sanitize_mirror_config(&mirror, url).await?;
        let pushed = self
            .inner
            .mirror_git(&mirror)
            .args(["push", "--quiet"])
            .arg(url)
            .arg(format!("{sha}:refs/heads/{}", repo.base_branch))
            .maybe_auth(auth)
            .run()
            .await;
        match pushed {
            Ok(_) => {}
            // Somebody gave it a ref between the listing and the push: it is not empty any more.
            Err(WorkspaceError::Invalid(message))
                if message.contains("rejected")
                    || message.contains("non-fast-forward")
                    || message.contains("fetch first")
                    || message.contains("already exists") =>
            {
                return Err(WorkspaceError::Conflict(format!(
                    "{} was given a ref while its first commit was being pushed: nothing was overwritten",
                    repo.url
                )));
            }
            Err(e) => return Err(e),
        }
        // As `git push origin` would have: the next worktree starts from `origin/<base>`.
        let noted = self
            .inner
            .mirror_git(&mirror)
            .args(["update-ref", "--no-deref"])
            .arg(format!("{REMOTE_TRACKING_PREFIX}{}", repo.base_branch))
            .arg(&sha)
            .run()
            .await;
        if let Err(e) = noted {
            tracing::warn!(error = %e, "cannot record the first commit in the mirror");
        }
        Ok(sha)
    }

    /// Wait until the remote at `repo_url` answers `git ls-remote` (a repository that was just
    /// created may take a moment to be visible), for at most `within`: the first success returns.
    /// A repository that is not found yet, a network failure and a rate limit are tried again,
    /// with a backoff from 250 ms to 2 s; anything else (a refused URL, bad credentials) fails at
    /// once.
    ///
    /// # Errors
    ///
    /// The last error of `git ls-remote` when `within` has passed.
    #[tracing::instrument(skip(self))]
    pub async fn wait_reachable(&self, repo_url: &str, within: Duration) -> WorkspaceResult<()> {
        let repo = RepoRef::new(repo_url, "HEAD");
        let deadline = tokio::time::Instant::now() + within;
        let mut delay = Duration::from_millis(250);
        loop {
            let e = match self.remote_refs(&repo).await {
                Ok(_) => return Ok(()),
                Err(e) => e,
            };
            let again = matches!(
                e,
                WorkspaceError::NotFound(_)
                    | WorkspaceError::Transient { .. }
                    | WorkspaceError::RateLimited { .. }
            );
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if !again || left.is_zero() {
                return Err(e);
            }
            // The last wait ends at the deadline, and the try that follows is the last one.
            tokio::time::sleep(delay.min(left)).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }
}

/// Whether the (lowercased-section) configuration `key` is one the mirror must not carry into a
/// credentialed command (see [`Inner::sanitize_mirror_config`]).
fn is_unwanted_config(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if key == "remote.origin.url" || key == "remote.origin.fetch" {
        return false;
    }
    const PREFIXES: &[&str] = &[
        "url.",
        "remote.",
        "include.",
        "includeif.",
        "http.",
        "credential.",
        "core.sshcommand",
        "core.gitproxy",
        "core.fsmonitor",
        "core.hookspath",
        "core.askpass",
    ];
    PREFIXES.iter().any(|p| key.starts_with(p))
}

/// The branch in the first line of `git ls-remote --symref origin HEAD`:
/// `ref: refs/heads/<branch>\tHEAD`.
fn parse_symref_head(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("ref: refs/heads/"))
        .and_then(|rest| rest.split_whitespace().next())
        .filter(|b| !b.is_empty())
        .map(str::to_owned)
}

impl Inner {
    /// The workspace root.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The legacy worktree of `run`.
    fn worktree_path(&self, run: &str) -> PathBuf {
        self.root.join("worktrees").join(run)
    }

    /// The metadata of the legacy worktree of `run`.
    fn meta_path(&self, run: &str) -> PathBuf {
        self.root.join("meta").join(format!("{run}.json"))
    }

    /// The directory of `run`'s workspace: `<root>/workspaces/<run>`.
    pub(crate) fn run_dir(&self, run: &str) -> PathBuf {
        self.root.join("workspaces").join(run)
    }

    /// The directory of a slot: `<root>/workspaces/<run>/<dir>`.
    pub(crate) fn slot_path(&self, run: &str, dir: &str) -> PathBuf {
        self.run_dir(run).join(dir)
    }

    /// The directory of the metadata of `run`'s slots: `<root>/meta/<run>`.
    pub(crate) fn slot_meta_dir(&self, run: &str) -> PathBuf {
        self.root.join("meta").join(run)
    }

    /// The metadata file of a slot: `<root>/meta/<run>/<dir>.json`.
    pub(crate) fn slot_meta_path(&self, run: &str, dir: &str) -> PathBuf {
        self.slot_meta_dir(run).join(format!("{dir}.json"))
    }

    /// The target of the legacy worktree of `run` for the repository at `loc`.
    pub(crate) fn legacy_target(&self, run: &str, loc: &RepoLocation) -> Target {
        Target {
            path: self.worktree_path(run),
            meta_path: self.meta_path(run),
            dir: slot_name(&loc.name),
            seq: None,
        }
    }

    /// The target of the slot `dir` of `run`, which joined as number `seq`.
    pub(crate) fn slot_target(&self, run: &str, dir: &str, seq: u32) -> Target {
        Target {
            path: self.slot_path(run, dir),
            meta_path: self.slot_meta_path(run, dir),
            dir: dir.to_owned(),
            seq: Some(seq),
        }
    }

    /// The legacy worktree of `run` as a [`Worktree`], if it is there.
    pub(crate) async fn open_legacy(
        self: &Arc<Self>,
        run: &str,
    ) -> WorkspaceResult<Option<Worktree>> {
        let Some(meta) = self.read_meta(run).await? else {
            return Ok(None);
        };
        let loc = RepoRef::new(&meta.url, &meta.base_branch).locate()?;
        let mirror = self.root.join(loc.mirror_relative());
        let path = self.worktree_path(run);
        if self.is_valid_worktree(&path, &mirror).await {
            Ok(Some(self.worktree(
                &meta,
                &slot_name(&loc.name),
                path,
                mirror,
            )))
        } else {
            Ok(None)
        }
    }

    /// Lock `mirror` against this process's other tasks and against other processes on the same
    /// root. See the *Concurrency* section of [`Workspaces`].
    pub(crate) async fn lock_mirror(&self, mirror: &Path) -> WorkspaceResult<MirrorGuard> {
        self.lock_path(mirror).await
    }

    /// The lock of `target`, a directory (a mirror, a run's workspace): the in-process lock, then
    /// an exclusive lock on the file `<target>.lock` beside it.
    pub(crate) async fn lock_path(&self, mirror: &Path) -> WorkspaceResult<MirrorGuard> {
        let local = self.lock_for(mirror).lock_owned().await;
        let path = mirror_lock_path(mirror);
        let file = tokio::task::spawn_blocking(move || -> io::Result<std::fs::File> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)?;
            file.lock()?;
            Ok(file)
        })
        .await
        .map_err(|e| WorkspaceError::io("the mirror lock task failed", io::Error::other(e)))?
        .map_err(|e| {
            WorkspaceError::io(
                "cannot lock the mirror (does the volume support file locks?)",
                e,
            )
        })?;
        Ok(MirrorGuard {
            _file: file,
            _local: local,
        })
    }

    /// Forget the in-process lock of `target`, for a directory that is gone for good (a removed
    /// run's workspace): the locks of runs would otherwise be kept as long as the process lives.
    /// Whoever holds the lock keeps it.
    pub(crate) fn forget_lock(&self, target: &Path) {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(target);
    }

    fn lock_for(&self, mirror: &Path) -> Arc<tokio::sync::Mutex<()>> {
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

    pub(crate) fn worktree(
        self: &Arc<Self>,
        meta: &Meta,
        dir: &str,
        path: PathBuf,
        mirror: PathBuf,
    ) -> Worktree {
        Worktree::new(Arc::clone(self), meta, dir.to_owned(), path, mirror)
    }

    /// The remote's branches as the last fetch saw them (`origin/*` of the mirror), sorted, the
    /// symbolic `HEAD` left out.
    async fn remote_branches(&self, mirror: &Path) -> WorkspaceResult<Vec<String>> {
        let out = self
            .mirror_git(mirror)
            .args([
                "for-each-ref",
                "--format=%(refname)",
                REMOTE_TRACKING_PREFIX,
            ])
            .run()
            .await?;
        Ok(out
            .stdout_text()
            .lines()
            .filter_map(|r| r.strip_prefix(REMOTE_TRACKING_PREFIX))
            .filter(|b| *b != "HEAD")
            .map(str::to_owned)
            .collect())
    }

    /// A sentence that lists the remote's branches (the first [`MAX_BRANCHES_LISTED`]), for the
    /// error about a branch that is not there, so that a caller can pick one or ask.
    async fn branches_said(&self, mirror: &Path) -> String {
        match self.remote_branches(mirror).await {
            Ok(branches) if branches.is_empty() => {
                "The repository has no branches (is it empty?).".to_owned()
            }
            Ok(branches) => {
                let more = branches.len().saturating_sub(MAX_BRANCHES_LISTED);
                let mut said = format!(
                    "Its branches: {}",
                    branches
                        .iter()
                        .take(MAX_BRANCHES_LISTED)
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if more > 0 {
                    said.push_str(&format!(" (and {more} more)"));
                }
                said.push('.');
                said
            }
            Err(_) => String::new(),
        }
    }

    /// `rev` names a commit in `mirror`.
    async fn has_commit(&self, mirror: &Path, rev: &str) -> WorkspaceResult<bool> {
        Ok(self
            .mirror_git(mirror)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{rev}^{{commit}}"))
            .run_status()
            .await?
            .success)
    }

    /// `existing` may be continued: one of this crate's own `agent/*` branches, well formed, and
    /// not the base.
    async fn validate_continued(&self, existing: &str, base: &str) -> WorkspaceResult<()> {
        let own = existing
            .strip_prefix(AGENT_BRANCH_PREFIX)
            .is_some_and(|rest| !rest.is_empty());
        let well_formed = own
            && existing != base
            && self
                .git()
                .args(["check-ref-format"])
                .arg(format!("refs/heads/{existing}"))
                .run_status()
                .await?
                .success;
        if well_formed {
            Ok(())
        } else {
            Err(WorkspaceError::Invalid(format!(
                "{existing:?} is not a branch that can be continued: only branches named \
                 {AGENT_BRANCH_PREFIX}<...> that an agent pushed are"
            )))
        }
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
        self.sanitize_mirror_config(mirror, url).await
    }

    /// Remove from the mirror's configuration whatever is not what [`ensure_mirror`](Self::ensure_mirror)
    /// writes and could change where a credentialed command goes or how it connects: any `url.*`
    /// rewrite (`insteadOf` and `pushInsteadOf` rewrite the URLs given on the command line too), any
    /// `remote.*` key but the two `ensure_mirror` writes, `include`s, `http.*`, `credential.*`, `core.sshCommand`,
    /// `core.gitProxy`, `core.fsmonitor`, `core.hooksPath` and `core.askPass`; and puts the two
    /// `remote.origin` keys back to what was approved (`url`: the URL the credentials are for).
    ///
    /// Call it **under the mirror lock, before every command that carries a token** (fetch,
    /// ls-remote, push, publish): the configuration is shared by every run and written by more than
    /// this crate (a model's command, OpenCode, a repository's own scripts run in a worktree), so
    /// the guard is at the credentialed call and not at whoever wrote the key. Keys the crate
    /// itself writes (`branch.*`, the two `remote.origin.*`) are left.
    pub(crate) async fn sanitize_mirror_config(
        &self,
        mirror: &Path,
        url: &str,
    ) -> WorkspaceResult<()> {
        let listed = self
            .mirror_git(mirror)
            .args(["config", "--local", "--list", "-z"])
            .run()
            .await?;
        let mut keys: Vec<String> = listed
            .stdout_text()
            .split('\0')
            .filter_map(|entry| entry.split('\n').next())
            .filter(|key| is_unwanted_config(key))
            .map(str::to_owned)
            .collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            tracing::warn!(%key, "removing a git configuration key that was not written by the workspace");
            self.mirror_git(mirror)
                .args(["config", "--local", "--unset-all", &key])
                .run_status()
                .await?;
        }
        // The two keys that stay are what the workspace approved, whatever they say now.
        let set = |key: &str, value: &str| {
            self.mirror_git(mirror)
                .args(["config", "--local", key, value])
                .run()
        };
        set("remote.origin.url", url).await?;
        set(
            "remote.origin.fetch",
            &format!("+refs/heads/*:{REMOTE_TRACKING_PREFIX}*"),
        )
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
        start_ref: &str,
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
                .arg(start_ref)
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

    /// The metadata of the legacy worktree of `run`.
    pub(crate) async fn read_meta(&self, run: &str) -> WorkspaceResult<Option<Meta>> {
        self.read_meta_at(&self.meta_path(run), run, false).await
    }

    /// The repository metadata in `path`: version 1 (`slot` false: the legacy file) or version 2
    /// (`slot` true), for `run`.
    pub(crate) async fn read_meta_at(
        &self,
        path: &Path,
        run: &str,
        slot: bool,
    ) -> WorkspaceResult<Option<Meta>> {
        let Some(bytes) = read_if_exists(path).await? else {
            return Ok(None);
        };
        let meta: Meta = serde_json::from_slice(&bytes).map_err(|e| {
            WorkspaceError::Corrupt(format!("{} is not valid metadata: {e}", path.display()))
        })?;
        let version = if slot {
            SLOT_META_VERSION
        } else {
            META_VERSION
        };
        if meta.version != version || meta.run != run {
            return Err(WorkspaceError::Corrupt(format!(
                "{} has unexpected version or run",
                path.display()
            )));
        }
        Ok(Some(meta))
    }

    /// The metadata of every slot of `run`, by directory name.
    pub(crate) async fn read_slot_metas(&self, run: &str) -> WorkspaceResult<Vec<SlotMeta>> {
        let dir = self.slot_meta_dir(run);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(WorkspaceError::io("cannot list the slots", e)),
        };
        let mut metas = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| WorkspaceError::io("cannot list the slots", e))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(bytes) = read_if_exists(&path).await? else {
                continue;
            };
            let corrupt = |why: String| {
                WorkspaceError::Corrupt(format!("{} is not valid metadata: {why}", path.display()))
            };
            let probe: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| corrupt(e.to_string()))?;
            let meta = if probe.get("kind").and_then(|k| k.as_str()) == Some("scratch") {
                SlotMeta::Scratch(
                    serde_json::from_value(probe).map_err(|e| corrupt(e.to_string()))?,
                )
            } else {
                SlotMeta::Repo(serde_json::from_value(probe).map_err(|e| corrupt(e.to_string()))?)
            };
            let (version, run_of, dir_of) = match &meta {
                SlotMeta::Repo(m) => (m.version, m.run.as_str(), m.dir.as_deref().unwrap_or("")),
                SlotMeta::Scratch(m) => (m.version, m.run.as_str(), m.dir.as_str()),
            };
            let stem = path.file_stem().and_then(|n| n.to_str()).unwrap_or("");
            if version != SLOT_META_VERSION || run_of != run || dir_of != stem {
                return Err(WorkspaceError::Corrupt(format!(
                    "{} has unexpected version, run or directory",
                    path.display()
                )));
            }
            metas.push(meta);
        }
        metas.sort_by(|a, b| a.dir().cmp(b.dir()));
        Ok(metas)
    }

    /// Write-then-rename, so a crash never leaves half a file.
    pub(crate) async fn write_meta_at(
        &self,
        path: &Path,
        meta: &impl Serialize,
    ) -> WorkspaceResult<()> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
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
        tokio::fs::rename(&tmp, path)
            .await
            .map_err(|e| WorkspaceError::io("cannot install run metadata", e))
    }

    /// Remove a worktree (or what is left of one) at `path` of the repository mirrored at
    /// `mirror`, and prune the mirror's bookkeeping. Idempotent.
    pub(crate) async fn remove_worktree_dir(
        &self,
        path: &Path,
        mirror: &Path,
    ) -> WorkspaceResult<()> {
        let _guard = self.lock_mirror(mirror).await?;
        let have_mirror = exists(&mirror.join("HEAD")).await?;
        if have_mirror {
            // Failure is fine (e.g. not registered any more); the directory is
            // removed below regardless.
            let _ = self
                .mirror_git(mirror)
                .args(["worktree", "remove", "--force"])
                .arg(path)
                .run_status()
                .await?;
        }
        remove_dir_if_exists(path).await?;
        if have_mirror {
            self.mirror_git(mirror)
                .args(["worktree", "prune"])
                .run()
                .await?;
        }
        Ok(())
    }

    /// Remove the legacy worktree of `run` and its metadata. Idempotent.
    pub(crate) async fn remove_legacy(&self, run: &str) -> WorkspaceResult<()> {
        let path = self.worktree_path(run);
        let Some(meta) = self.read_meta(run).await? else {
            // Never prepared, or a crash left a bare directory: nothing to
            // unregister, just make sure the directory is gone.
            return remove_dir_if_exists(&path).await;
        };
        let loc = RepoRef::new(&meta.url, &meta.base_branch).locate()?;
        let mirror = self.root.join(loc.mirror_relative());
        self.remove_worktree_dir(&path, &mirror).await?;
        remove_file_if_exists(&self.meta_path(run)).await
    }
}

/// Run ids become directory and branch names, so they are restricted.
/// `<mirror>.lock`, a sibling of the mirror directory (`.../repo.git` -> `.../repo.git.lock`).
/// A mirror is always named `<name>.git`, so no other mirror can have this name.
fn mirror_lock_path(mirror: &Path) -> PathBuf {
    lock_file_of(mirror)
}

/// `<target>.lock`, the file the lock of the directory `target` is taken on.
pub(crate) fn lock_file_of(mirror: &Path) -> PathBuf {
    let mut name = mirror.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

pub(crate) fn validate_run(run: &str) -> WorkspaceResult<()> {
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

/// A directory name for a repository called `name` in a run's workspace: lowercased, only
/// `[a-z0-9._-]`, no leading `.`, `-` or `_`, at most 40 characters, `repo` if nothing is left.
pub(crate) fn slot_name(name: &str) -> String {
    let lowered: String = name
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = lowered.trim_start_matches(['.', '-', '_']);
    let cut: String = trimmed.chars().take(40).collect();
    if cut.is_empty() {
        "repo".to_owned()
    } else {
        cut
    }
}

/// The bytes of the file at `path`, `None` when there is no such file.
async fn read_if_exists(path: &Path) -> WorkspaceResult<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WorkspaceError::io("cannot read run metadata", e)),
    }
}

pub(crate) async fn remove_file_if_exists(path: &Path) -> WorkspaceResult<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(WorkspaceError::io("cannot remove run metadata", e)),
    }
}

pub(crate) async fn exists(path: &Path) -> WorkspaceResult<bool> {
    tokio::fs::try_exists(path)
        .await
        .map_err(|e| WorkspaceError::io(format!("cannot stat {}", path.display()), e))
}

pub(crate) async fn remove_dir_if_exists(path: &Path) -> WorkspaceResult<()> {
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

    #[test]
    fn the_keys_that_can_redirect_a_credentialed_command_are_unwanted() {
        for key in [
            "url.https://evil.example/.insteadof",
            "url.file:///evil.git.pushinsteadof",
            "remote.origin.pushurl",
            "remote.evil.url",
            "remote.origin.proxy",
            "include.path",
            "includeif.gitdir:/x.path",
            "http.proxy",
            "http.https://github.com/.extraheader",
            "credential.helper",
            "core.sshcommand",
            "core.fsmonitor",
            "core.hookspath",
        ] {
            assert!(is_unwanted_config(key), "{key}");
        }
        for key in [
            "remote.origin.url",
            "remote.origin.fetch",
            "core.bare",
            "core.repositoryformatversion",
            "branch.agent/abc.adam-run",
            "branch.agent/abc.remote",
            "user.name",
        ] {
            assert!(!is_unwanted_config(key), "{key}");
        }
    }

    #[test]
    fn the_default_branch_is_read_from_the_symref_line() {
        assert_eq!(
            parse_symref_head("ref: refs/heads/master\tHEAD\n0123abcd\tHEAD").as_deref(),
            Some("master")
        );
        assert_eq!(
            parse_symref_head("ref: refs/heads/feature/x\tHEAD").as_deref(),
            Some("feature/x")
        );
        // An empty repository, or a detached HEAD, has no symref line.
        assert_eq!(parse_symref_head(""), None);
        assert_eq!(parse_symref_head("0123abcd\tHEAD"), None);
    }
}
