//! A run's workspace: a directory of slots, each a repository's worktree or a scratch project.
//!
//! The coder used to hold one worktree per run. Its tasks now start in a scratch project when no
//! repository is named, and may take in more than one repository, so a run's workspace is a
//! directory `<root>/workspaces/<run>/` of **slots**:
//!
//! * a **repository slot** is a [`Worktree`] of one repository, on the run's own branch
//!   `agent/<run-short-id>` (every operation of a worktree is as it was);
//! * a **scratch slot** is a local git repository with an empty root commit, which no remote has:
//!   a place to start building before anyone has named a repository. What it holds reaches a
//!   repository by [`copy_into`], and its history stays here.
//!
//! The layout, the locks and the lifecycle are in the [`Workspaces`] documentation and in the
//! crate README; the decision is [ADR 0008](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0008-a-workspace-holds-several-repositories.md).

use std::collections::HashSet;
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::git::{GitCmd, GitPin};
use crate::repo::RepoRef;
use crate::safe_fs;
use crate::workspace::{
    Inner, Meta, SLOT_META_VERSION, ScratchMeta, SlotMeta, Workspaces, exists,
    remove_dir_if_exists, remove_file_if_exists, slot_name,
};
use crate::worktree::{ChangedFile, GitIdentity, Worktree, commit_all_in, parse_porcelain};

/// Longest directory name of a scratch slot.
const MAX_SCRATCH_DIR: usize = 64;

/// The workspace of one run: a directory of slots (see [`Workspaces`] for the layout, the locks and
/// the lifecycle). Cheap to clone.
///
/// Made by [`Workspaces::run`]; making it touches nothing. Changing the set of slots (adding
/// one, removing them all) is done by one task or process at a time per run, under the run's lock
/// (see *Concurrency* in the [`Workspaces`] documentation), and every operation that changes a
/// mirror takes the mirror's lock, as a [`Worktree`]'s do.
#[derive(Clone)]
pub struct RunWorkspace {
    ws: Workspaces,
    run: String,
    path: PathBuf,
}

impl fmt::Debug for RunWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunWorkspace")
            .field("run", &self.run)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// One place of a [`RunWorkspace`]: its directory name, its path, and what it is.
#[derive(Debug, Clone)]
pub struct Slot {
    dir: String,
    path: PathBuf,
    seq: u32,
    kind: SlotKind,
}

/// What a [`Slot`] is.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SlotKind {
    /// A worktree of a repository, on the run's own branch.
    Repository(Worktree),
    /// A local project that no remote has yet.
    Scratch(Scratch),
}

impl Slot {
    /// The slot's directory name in the workspace: a repository's name, lowercased (`<name>-<owner>`
    /// when another repository of the run has the name), or the name a scratch project was given.
    /// What the model says to choose a slot.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// The slot's directory: `<root>/workspaces/<run>/<dir>` (`<root>/worktrees/<run>` for the legacy
    /// worktree of a run).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The order the slot joined the run, from 1; the legacy worktree of a run, which is older than
    /// any slot, is 0.
    pub fn seq(&self) -> u32 {
        self.seq
    }

    /// What the slot is.
    pub fn kind(&self) -> &SlotKind {
        &self.kind
    }

    /// The worktree, for a repository slot.
    pub fn worktree(&self) -> Option<&Worktree> {
        match &self.kind {
            SlotKind::Repository(wt) => Some(wt),
            SlotKind::Scratch(_) => None,
        }
    }

    /// The scratch project, for a scratch slot.
    pub fn scratch(&self) -> Option<&Scratch> {
        match &self.kind {
            SlotKind::Scratch(s) => Some(s),
            SlotKind::Repository(_) => None,
        }
    }

    /// Where `git` works for this slot, from the coder's own metadata ([`GitPin`]): a worktree's
    /// directory in its mirror and the mirror, or a scratch project's own `.git`. `None` for a
    /// worktree whose directory in the mirror cannot be told (then no `git` is run on it).
    pub fn git_pin(&self) -> Option<GitPin> {
        match &self.kind {
            SlotKind::Repository(wt) => wt.git_pin(),
            SlotKind::Scratch(s) => Some(s.git_pin()),
        }
    }
}

/// A scratch project: a local git repository (branch `main`, an empty root commit) in a slot of a
/// run's workspace. Nothing pushes it anywhere; its files reach a repository through
/// [`copy_into`]. Cheap to clone; clones refer to the same directory.
#[derive(Clone)]
pub struct Scratch {
    ws: Arc<Inner>,
    run: String,
    dir: String,
    path: PathBuf,
    published_to: Option<String>,
    conversation: Option<String>,
}

impl fmt::Debug for Scratch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scratch")
            .field("run", &self.run)
            .field("dir", &self.dir)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Scratch {
    /// The project's directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The slot's directory name.
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// The conversation of the run that made the project (see [`RunWorkspace::add_scratch_for`]), as
    /// it was named then; `None` for a project made without one.
    pub fn conversation(&self) -> Option<&str> {
        self.conversation.as_deref()
    }

    /// The repository the project's files were last copied into
    /// ([`set_published_to`](Self::set_published_to)), as its url was written then: `None` for a
    /// project that was never published. As the slot was listed: a later call changes the next
    /// listing, not this value.
    pub fn published_to(&self) -> Option<&str> {
        self.published_to.as_deref()
    }

    /// Record that the project's files were copied into the repository `url` (see [`copy_into`]):
    /// what a caller says to the model that goes on editing the project afterwards, whose changes
    /// no longer reach that repository. Kept in the slot's metadata, so it survives a restart, and
    /// replaced by a later call (a project may be copied into more than one repository). Takes the
    /// run's lock, as every change to the run's slots does.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotFound`] when the project is no longer a slot of its run (the workspace
    /// was removed), and I/O failures.
    #[tracing::instrument(skip(self, url), fields(run = %self.run, dir = %self.dir))]
    pub async fn set_published_to(&self, url: &str) -> WorkspaceResult<()> {
        let inner = &self.ws;
        let _run = inner.lock_path(&inner.run_dir(&self.run)).await?;
        let path = inner.slot_meta_path(&self.run, &self.dir);
        let Some(SlotMeta::Scratch(mut meta)) = inner
            .read_slot_metas(&self.run)
            .await?
            .into_iter()
            .find(|m| m.dir() == self.dir && matches!(m, SlotMeta::Scratch(_)))
        else {
            return Err(WorkspaceError::NotFound(format!(
                "{} is not a scratch project of this workspace any more",
                self.dir
            )));
        };
        meta.published_to = Some(url.to_owned());
        inner.write_meta_at(&path, &meta).await
    }

    /// `git` in the project, pinned to its own `.git` directory ([`GitPin`]): a `.git` that a
    /// command replaced by a file or a link, to send the coder's `git` into another run's worktree
    /// or a mirror, is refused.
    fn git(&self) -> GitCmd {
        self.ws.git().cwd(&self.path).pinned(self.git_pin())
    }

    /// Where `git` works for this project: its own `.git` directory, for the git directory and the
    /// common directory alike, and its directory for the work tree. A caller that starts `git`
    /// itself applies it ([`GitPin::apply`]) and checks it ([`GitPin::check`]).
    pub fn git_pin(&self) -> GitPin {
        GitPin::own_repository(&self.path)
    }

    /// The files that differ from the last commit: staged, unstaged and untracked ones (each
    /// listed), from `git status --porcelain=v1 -z`. What [`Worktree::status`] says for a
    /// repository's slot.
    ///
    /// # Errors
    ///
    /// A git failure.
    #[tracing::instrument(skip(self), fields(run = %self.run, dir = %self.dir))]
    pub async fn status(&self) -> WorkspaceResult<Vec<ChangedFile>> {
        let out = self
            .git()
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .run()
            .await?;
        Ok(parse_porcelain(&out.stdout))
    }

    /// Stage everything (`git add -A`) and commit as `author`, locally: the sha of the new commit,
    /// or `None` when there was nothing to commit. Hooks and commit signing are off.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for an unusable identity or an empty message, or a git failure.
    #[tracing::instrument(skip(self, message, author), fields(run = %self.run, dir = %self.dir))]
    pub async fn commit_all(
        &self,
        message: &str,
        author: &GitIdentity,
    ) -> WorkspaceResult<Option<String>> {
        commit_all_in(|| self.git(), message, author, None).await
    }

    /// The files of the project: tracked ones and untracked ones that `.gitignore` does not
    /// exclude, as paths relative to its root, sorted. A file that was deleted and not yet
    /// committed is not listed, and neither is a directory (a nested repository).
    ///
    /// # Errors
    ///
    /// A git failure.
    #[tracing::instrument(skip(self), fields(run = %self.run, dir = %self.dir))]
    pub async fn files(&self) -> WorkspaceResult<Vec<PathBuf>> {
        let listed = self
            .git()
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .run()
            .await?;
        let mut files: Vec<PathBuf> = listed
            .stdout
            .split(|b| *b == 0)
            .filter(|name| !name.is_empty())
            .map(|name| PathBuf::from(String::from_utf8_lossy(name).into_owned()))
            .filter(|rel| {
                std::fs::symlink_metadata(self.path.join(rel)).is_ok_and(|meta| !meta.is_dir())
            })
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }
}

impl RunWorkspace {
    pub(crate) fn new(ws: Workspaces, run: &str) -> Self {
        let path = ws.inner.run_dir(run);
        Self {
            ws,
            run: run.to_owned(),
            path,
        }
    }

    /// The run.
    pub fn run(&self) -> &str {
        &self.run
    }

    /// The workspace's directory: `<root>/workspaces/<run>`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn inner(&self) -> &Arc<Inner> {
        &self.ws.inner
    }

    /// A slot for `meta`, `None` when its directory is gone (a lost volume: adding the repository
    /// again re-attaches the branch).
    async fn slot_of(&self, meta: &SlotMeta) -> WorkspaceResult<Option<Slot>> {
        let inner = self.inner();
        match meta {
            SlotMeta::Repo(m) => {
                let dir = m.dir.clone().unwrap_or_default();
                let path = inner.slot_path(&self.run, &dir);
                if !is_slot_name(&dir) || !looks_like_worktree(&path).await {
                    return Ok(None);
                }
                let loc = RepoRef::new(&m.url, &m.base_branch).locate()?;
                let mirror = inner.root().join(loc.mirror_relative());
                let meta_path = inner.slot_meta_path(&self.run, &dir);
                let worktree = inner
                    .open_worktree(m, &dir, path.clone(), mirror, &meta_path)
                    .await;
                Ok(Some(Slot {
                    seq: m.seq.unwrap_or(0),
                    kind: SlotKind::Repository(worktree),
                    dir,
                    path,
                }))
            }
            SlotMeta::Scratch(m) => {
                let path = inner.slot_path(&self.run, &m.dir);
                if !is_slot_name(&m.dir)
                    || !safe_fs::is_real_directory(&path).await
                    || !safe_fs::is_real_directory(&path.join(".git")).await
                {
                    return Ok(None);
                }
                Ok(Some(Slot {
                    dir: m.dir.clone(),
                    seq: m.seq,
                    kind: SlotKind::Scratch(Scratch {
                        ws: Arc::clone(inner),
                        run: self.run.clone(),
                        dir: m.dir.clone(),
                        path: path.clone(),
                        published_to: m.published_to.clone(),
                        conversation: m.conversation.clone(),
                    }),
                    path,
                }))
            }
        }
    }

    /// The legacy worktree of the run as a slot (number 0), if it is there.
    async fn legacy_slot(&self) -> WorkspaceResult<Option<Slot>> {
        let inner = self.inner();
        let Some(meta) = inner.read_meta(&self.run).await? else {
            return Ok(None);
        };
        let loc = RepoRef::new(&meta.url, &meta.base_branch).locate()?;
        let path = inner.legacy_target(&self.run, &loc).path;
        if !looks_like_worktree(&path).await {
            return Ok(None);
        }
        let dir = slot_name(&loc.name);
        let mirror = inner.root().join(loc.mirror_relative());
        let meta_path = inner.meta_path(&self.run);
        let worktree = inner
            .open_worktree(&meta, &dir, path.clone(), mirror, &meta_path)
            .await;
        Ok(Some(Slot {
            seq: 0,
            kind: SlotKind::Repository(worktree),
            dir,
            path,
        }))
    }

    /// The slots of the workspace that are there: the run's legacy worktree first (it is named
    /// after its repository), then the others sorted by directory name. A slot whose directory is
    /// gone is not listed.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Corrupt`] for unreadable metadata, I/O errors.
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn slots(&self) -> WorkspaceResult<Vec<Slot>> {
        let mut slots = Vec::new();
        slots.extend(self.legacy_slot().await?);
        for meta in self.inner().read_slot_metas(&self.run).await? {
            slots.extend(self.slot_of(&meta).await?);
        }
        Ok(slots)
    }

    /// [`slots`](Self::slots) in the order they joined the run (the legacy worktree, then by
    /// [`Slot::seq`]). The first of them is the workspace's "first repository" for anything that
    /// has to pick one.
    ///
    /// # Errors
    ///
    /// As [`slots`](Self::slots).
    pub async fn slots_in_join_order(&self) -> WorkspaceResult<Vec<Slot>> {
        let mut slots = self.slots().await?;
        slots.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.dir.cmp(&b.dir)));
        Ok(slots)
    }

    /// The slot called `dir`, if there is one.
    ///
    /// # Errors
    ///
    /// As [`slots`](Self::slots).
    pub async fn slot(&self, dir: &str) -> WorkspaceResult<Option<Slot>> {
        Ok(self.slots().await?.into_iter().find(|s| s.dir == dir))
    }

    /// The repository slot of `repo`, if the run has one. Two spellings of one repository (with
    /// or without `.git`, `file://` or a path) are the same repository.
    ///
    /// # Errors
    ///
    /// As [`slots`](Self::slots); [`WorkspaceError::Invalid`] for a url that cannot be read.
    pub async fn slot_for(&self, repo: &RepoRef) -> WorkspaceResult<Option<Slot>> {
        let wanted = repo.locate()?.mirror_relative();
        Ok(self.slots().await?.into_iter().find(|slot| {
            slot.worktree().is_some_and(|wt| {
                wt.repo()
                    .locate()
                    .is_ok_and(|loc| loc.mirror_relative() == wanted)
            })
        }))
    }

    /// Add the repository `repo` to the workspace: a worktree on a new branch
    /// `agent/<run-short-id>` from `origin/<base_branch>` (see [`Workspaces::prepare`], which
    /// does it), in the slot named after the repository. **Idempotent per repository**: a
    /// repository the run already has returns its slot, whatever the base branch or spelling of
    /// the second request, and a slot whose directory was lost is made again, on the same branch.
    ///
    /// The slot is called the repository's name, lowercased; if another repository of the run has
    /// that name, `<name>-<owner>`.
    ///
    /// # Errors
    ///
    /// As [`Workspaces::prepare`] (a refused repository costs no process and no credential).
    #[tracing::instrument(skip(self, repo), fields(run = %self.run, repo = %repo.url))]
    pub async fn add_repository(&self, repo: &RepoRef) -> WorkspaceResult<Slot> {
        self.add(repo, None).await
    }

    /// [`add_repository`](Self::add_repository) for a worktree that **continues** a branch pushed
    /// before, as [`Workspaces::prepare_continuing`] does.
    ///
    /// # Errors
    ///
    /// As [`Workspaces::prepare_continuing`]; and [`WorkspaceError::Conflict`] when the run already
    /// has this repository on another branch.
    #[tracing::instrument(skip(self, repo), fields(run = %self.run, repo = %repo.url, existing = %existing))]
    pub async fn add_repository_continuing(
        &self,
        repo: &RepoRef,
        existing: &str,
    ) -> WorkspaceResult<Slot> {
        self.add(repo, Some(existing)).await
    }

    async fn add(&self, repo: &RepoRef, existing: Option<&str>) -> WorkspaceResult<Slot> {
        let inner = self.inner();
        // A refused repository costs no process, no lock and no credential.
        let loc = self.ws.check_repo(repo)?;
        let _run = inner.lock_path(&self.path).await?;
        let metas = inner.read_slot_metas(&self.run).await?;
        let legacy = inner.read_meta(&self.run).await?;
        let wanted = loc.mirror_relative();
        let same = |url: &str| {
            RepoRef::new(url, "HEAD")
                .locate()
                .is_ok_and(|l| l.mirror_relative() == wanted)
        };

        // The repository the run already has: its own spelling, so the worktree finds its meta.
        if let Some(meta) = legacy.as_ref().filter(|m| same(&m.url)) {
            let known = RepoRef::new(&meta.url, repo.base_branch.clone());
            let target = inner.legacy_target(&self.run, &loc);
            let wt = self
                .ws
                .prepare_at(&known, &self.run, existing, &target)
                .await?;
            return Ok(repository_slot(wt, 0));
        }
        if let Some(meta) = metas.iter().find_map(|m| match m {
            SlotMeta::Repo(m) if same(&m.url) => Some(m),
            _ => None,
        }) {
            let known = RepoRef::new(&meta.url, repo.base_branch.clone());
            let dir = meta.dir.clone().unwrap_or_default();
            let seq = meta.seq.unwrap_or(0);
            let target = inner.slot_target(&self.run, &dir, seq);
            let wt = self
                .ws
                .prepare_at(&known, &self.run, existing, &target)
                .await?;
            return Ok(repository_slot(wt, seq));
        }

        // A new one: its directory and its place in the order.
        let taken = self.taken_names(&metas, legacy.as_ref()).await?;
        let dir = pick_slot_dir(&loc.name, &loc.owner, &taken);
        let seq = metas.iter().map(SlotMeta::seq).max().unwrap_or(0) + 1;
        let target = inner.slot_target(&self.run, &dir, seq);
        let wt = self
            .ws
            .prepare_at(repo, &self.run, existing, &target)
            .await?;
        Ok(repository_slot(wt, seq))
    }

    /// Every directory name that is in use: slots, the legacy worktree's name, and anything
    /// that is in the workspace's directory.
    async fn taken_names(
        &self,
        metas: &[SlotMeta],
        legacy: Option<&Meta>,
    ) -> WorkspaceResult<HashSet<String>> {
        let mut taken: HashSet<String> = metas.iter().map(|m| m.dir().to_owned()).collect();
        if let Some(legacy) = legacy
            && let Ok(loc) = RepoRef::new(&legacy.url, &legacy.base_branch).locate()
        {
            taken.insert(slot_name(&loc.name));
        }
        if let Ok(mut entries) = tokio::fs::read_dir(&self.path).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                taken.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
        Ok(taken)
    }

    /// Add a scratch project called `dir`: a local git repository (branch `main`) with an empty
    /// root commit by `identity`. **Idempotent**: a scratch project that is already there is
    /// returned. `dir` is `^[a-z0-9][a-z0-9._-]{0,63}$` and does not end with `.git`. The project
    /// has no conversation: it stays with this run ([`add_scratch_for`](Self::add_scratch_for)).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for a bad `dir` or identity; [`WorkspaceError::Conflict`] when
    /// a repository slot is already called `dir`; git and I/O failures.
    pub async fn add_scratch(&self, dir: &str, identity: &GitIdentity) -> WorkspaceResult<Slot> {
        self.add_scratch_for(dir, identity, None).await
    }

    /// [`add_scratch`](Self::add_scratch) for a run of `conversation`: the project remembers it, so
    /// that a later run of the same conversation takes the project over
    /// ([`adopt_scratch`](Self::adopt_scratch)) instead of finding nothing, and the janitor of the
    /// coder keeps it while the conversation has tasks. A project that is already there without a
    /// conversation is given this one.
    ///
    /// # Errors
    ///
    /// As [`add_scratch`](Self::add_scratch).
    #[tracing::instrument(skip(self, identity), fields(run = %self.run))]
    pub async fn add_scratch_for(
        &self,
        dir: &str,
        identity: &GitIdentity,
        conversation: Option<&str>,
    ) -> WorkspaceResult<Slot> {
        validate_scratch_dir(dir)?;
        identity.validate()?;
        let inner = self.inner();
        let _run = inner.lock_path(&self.path).await?;
        let metas = inner.read_slot_metas(&self.run).await?;
        let legacy = inner.read_meta(&self.run).await?;
        let known = metas.iter().find(|m| m.dir() == dir);
        let (seq, kept_conversation) = if let Some(known) = known {
            match known {
                SlotMeta::Scratch(m) => {
                    if m.conversation.is_none()
                        && let Some(conversation) = conversation
                    {
                        let mut meta = m.clone();
                        meta.conversation = Some(conversation.to_owned());
                        inner
                            .write_meta_at(&inner.slot_meta_path(&self.run, dir), &meta)
                            .await?;
                        (m.seq, Some(conversation.to_owned()))
                    } else {
                        (m.seq, m.conversation.clone())
                    }
                }
                SlotMeta::Repo(_) => {
                    return Err(WorkspaceError::Conflict(format!(
                        "a repository of this workspace is already called {dir}"
                    )));
                }
            }
        } else {
            if self
                .taken_names(&metas, legacy.as_ref())
                .await?
                .contains(dir)
            {
                return Err(WorkspaceError::Conflict(format!(
                    "{dir} is already in use in this workspace"
                )));
            }
            let seq = metas.iter().map(SlotMeta::seq).max().unwrap_or(0) + 1;
            let meta = ScratchMeta {
                version: SLOT_META_VERSION,
                run: self.run.clone(),
                dir: dir.to_owned(),
                seq,
                kind: "scratch".to_owned(),
                published_to: None,
                conversation: conversation.map(str::to_owned),
            };
            inner
                .write_meta_at(&inner.slot_meta_path(&self.run, dir), &meta)
                .await?;
            (seq, meta.conversation)
        };
        let path = inner.slot_path(&self.run, dir);
        let published_to = metas.iter().find_map(|m| match m {
            SlotMeta::Scratch(m) if m.dir == dir => m.published_to.clone(),
            _ => None,
        });
        let scratch = Scratch {
            ws: Arc::clone(inner),
            run: self.run.clone(),
            dir: dir.to_owned(),
            path: path.clone(),
            published_to,
            conversation: kept_conversation,
        };
        scratch.init(identity).await?;
        Ok(Slot {
            dir: dir.to_owned(),
            path,
            seq,
            kind: SlotKind::Scratch(scratch),
        })
    }

    /// Take over the scratch projects that other runs' workspaces hold for `conversation`: each is
    /// **moved** (its directory renamed, its metadata rewritten for this run) into this workspace,
    /// so a conversation's tasks go on with the files the earlier tasks made. A project whose name
    /// this workspace already has stays where it is. Returns the names taken, in the order they
    /// were found.
    ///
    /// The two runs' locks are held, in the order of their paths, while a project moves; no other
    /// operation holds more than one run's lock, so two calls cannot wait for each other. A run
    /// whose metadata cannot be read is skipped. Idempotent: a second call finds nothing to take.
    ///
    /// # Errors
    ///
    /// I/O failures (what was moved before stays moved).
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn adopt_scratch(&self, conversation: &str) -> WorkspaceResult<Vec<String>> {
        let inner = self.inner();
        let mut taken = Vec::new();
        for run in self.ws.runs().await? {
            if run == self.run {
                continue;
            }
            // A look without the lock: most runs hold nothing of this conversation.
            let holds = match inner.read_slot_metas(&run).await {
                Ok(metas) => metas.iter().any(|m| {
                    matches!(m, SlotMeta::Scratch(s) if s.conversation.as_deref() == Some(conversation))
                }),
                Err(_) => false,
            };
            if holds {
                taken.extend(self.take_scratch_from(&run, conversation).await?);
            }
        }
        Ok(taken)
    }

    /// Move the scratch projects of `conversation` from the workspace of `from` into this one.
    async fn take_scratch_from(
        &self,
        from: &str,
        conversation: &str,
    ) -> WorkspaceResult<Vec<String>> {
        let inner = self.inner();
        let from_dir = inner.run_dir(from);
        let (first, second) = if from_dir < self.path {
            (&from_dir, &self.path)
        } else {
            (&self.path, &from_dir)
        };
        let _first = inner.lock_path(first).await?;
        let _second = inner.lock_path(second).await?;
        let theirs = inner.read_slot_metas(from).await?;
        let mine = inner.read_slot_metas(&self.run).await?;
        let legacy = inner.read_meta(&self.run).await?;
        let mut names = self.taken_names(&mine, legacy.as_ref()).await?;
        let mut seq = mine.iter().map(SlotMeta::seq).max().unwrap_or(0);
        let mut moved = Vec::new();
        for meta in theirs {
            let SlotMeta::Scratch(m) = meta else {
                continue;
            };
            if m.conversation.as_deref() != Some(conversation) || names.contains(&m.dir) {
                continue;
            }
            let source = inner.slot_path(from, &m.dir);
            if !exists(&source.join(".git")).await? {
                continue;
            }
            seq += 1;
            let new_meta = ScratchMeta {
                run: self.run.clone(),
                seq,
                ..m.clone()
            };
            let new_meta_path = inner.slot_meta_path(&self.run, &m.dir);
            inner.write_meta_at(&new_meta_path, &new_meta).await?;
            let renamed = async {
                tokio::fs::create_dir_all(&self.path).await?;
                tokio::fs::rename(&source, inner.slot_path(&self.run, &m.dir)).await
            }
            .await;
            if let Err(e) = renamed {
                remove_file_if_exists(&new_meta_path).await?;
                return Err(WorkspaceError::io("cannot move a scratch project", e));
            }
            remove_file_if_exists(&inner.slot_meta_path(from, &m.dir)).await?;
            names.insert(m.dir.clone());
            moved.push(m.dir);
        }
        Ok(moved)
    }

    /// Delete the whole workspace: every slot (a worktree with its uncommitted changes, a scratch
    /// project), the legacy worktree of the run, the metadata and the directory, under the
    /// mirrors' locks. **Idempotent.** The run's `agent/*` branches stay in their mirrors: they
    /// are the only copy of any unpushed commit.
    ///
    /// # Errors
    ///
    /// I/O or git failures.
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn remove(&self) -> WorkspaceResult<()> {
        self.remove_slots(false).await.map(drop)
    }

    /// [`remove`](Self::remove) except for the scratch projects that have a conversation
    /// ([`add_scratch_for`](Self::add_scratch_for)): the worktrees, the legacy worktree, the projects
    /// without one and their metadata go; those projects (their directories and metadata) and the
    /// workspace that holds them stay, for a later run of the conversation to take over
    /// ([`adopt_scratch`](Self::adopt_scratch)). Returns how many projects stay; with none, this is
    /// `remove`. Metadata that cannot be read is no reason to keep anything: the workspace is
    /// removed whole, as `remove` does. **Idempotent.**
    ///
    /// What the coder's janitor calls for a finished run whose conversation may go on
    /// ([`adopt_scratch`](Self::adopt_scratch)).
    ///
    /// # Errors
    ///
    /// I/O or git failures.
    #[tracing::instrument(skip(self), fields(run = %self.run))]
    pub async fn remove_keeping_scratch(&self) -> WorkspaceResult<usize> {
        self.remove_slots(true).await
    }

    async fn remove_slots(&self, keep_scratch: bool) -> WorkspaceResult<usize> {
        let inner = self.inner();
        let _run = inner.lock_path(&self.path).await?;
        let mut kept = 0;
        match inner.read_slot_metas(&self.run).await {
            Ok(metas) => {
                for meta in &metas {
                    match meta {
                        SlotMeta::Repo(m) => {
                            let dir = m.dir.clone().unwrap_or_default();
                            let path = inner.slot_path(&self.run, &dir);
                            match RepoRef::new(&m.url, &m.base_branch).locate() {
                                Ok(loc) => {
                                    let mirror = inner.root().join(loc.mirror_relative());
                                    inner
                                        .remove_worktree_dir(
                                            &path,
                                            &mirror,
                                            &self.path,
                                            Some((&self.run, &dir)),
                                        )
                                        .await?;
                                }
                                Err(_) => remove_dir_if_exists(&path).await?,
                            }
                        }
                        // Only a project with a conversation can be taken over by a later run.
                        SlotMeta::Scratch(m) if keep_scratch && m.conversation.is_some() => {
                            kept += 1;
                            continue;
                        }
                        SlotMeta::Scratch(m) => {
                            remove_dir_if_exists(&inner.slot_path(&self.run, &m.dir)).await?;
                        }
                    }
                    remove_file_if_exists(&inner.slot_meta_path(&self.run, meta.dir())).await?;
                }
            }
            // Metadata that cannot be read is no reason to keep the files, and the run's directories
            // are found in the mirrors by their names, which carry the run.
            Err(WorkspaceError::Corrupt(why)) => {
                tracing::warn!(run = %self.run, %why, "removing a workspace whose metadata cannot be read");
                remove_dir_if_exists(&self.path).await?;
                inner.forget_run_admins(&self.run).await?;
            }
            Err(e) => return Err(e),
        }
        inner.remove_legacy(&self.run).await?;
        if kept > 0 {
            return Ok(kept);
        }
        remove_dir_if_exists(&inner.slot_meta_dir(&self.run)).await?;
        remove_dir_if_exists(&self.path).await?;
        // The run is over: its lock goes with it.
        let _ = tokio::fs::remove_file(crate::workspace::lock_file_of(&self.path)).await;
        inner.forget_lock(&self.path);
        Ok(0)
    }
}

/// The slot of a worktree that was just made or found.
fn repository_slot(wt: Worktree, seq: u32) -> Slot {
    Slot {
        dir: wt.dir().to_owned(),
        path: wt.path().to_path_buf(),
        seq,
        kind: SlotKind::Repository(wt),
    }
}

/// `path` is a worktree's directory itself, not a link a run's commands put there (to send the
/// coder's `git add -A` into another run's files), with the `.git` file a worktree has.
async fn looks_like_worktree(path: &Path) -> bool {
    safe_fs::is_real_directory(path).await && safe_fs::is_real_file(&path.join(".git")).await
}

/// `dir` is one plain component: the name of a direct child of a run's workspace.
fn is_slot_name(dir: &str) -> bool {
    let mut parts = Path::new(dir).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(name)), None) if name == std::ffi::OsStr::new(dir)
    )
}

/// The directory name of a new repository slot: the repository's name; if that is taken,
/// `<name>-<owner>`; if that is too, a number is added.
fn pick_slot_dir(name: &str, owner: &str, taken: &HashSet<String>) -> String {
    let base = slot_name(name);
    if !taken.contains(&base) {
        return base;
    }
    let with_owner = format!("{base}-{}", slot_name(owner));
    if !taken.contains(&with_owner) {
        return with_owner;
    }
    (2u32..)
        .map(|n| format!("{with_owner}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(with_owner)
}

/// `dir` is a name a scratch project may have: `^[a-z0-9][a-z0-9._-]{0,63}$`, not ending `.git`.
fn validate_scratch_dir(dir: &str) -> WorkspaceResult<()> {
    let ok = !dir.is_empty()
        && dir.len() <= MAX_SCRATCH_DIR
        && dir
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && dir
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        && !dir.ends_with(".git");
    if ok {
        Ok(())
    } else {
        Err(WorkspaceError::Invalid(format!(
            "{dir:?} is not a name for a scratch project: lowercase letters, digits, `.`, `_` and \
             `-`, starting with a letter or a digit, at most {MAX_SCRATCH_DIR} characters, not \
             ending in .git"
        )))
    }
}

/// What is at `path`, without following a link; `None` when there is nothing.
async fn lstat_if_exists(path: &Path) -> WorkspaceResult<Option<std::fs::Metadata>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(WorkspaceError::io(
            format!("cannot stat {}", path.display()),
            e,
        )),
    }
}

impl Scratch {
    /// Make the repository if it is not there, and give it its root commit if it has none: what a
    /// crash in the middle leaves is finished by the next call.
    ///
    /// The project's directory is in the run's workspace, which the run's commands write: a link
    /// there (to another run's project, to a mirror) would have `create_dir_all` and `git init`
    /// make a repository where the link leads. So the directory and its `.git` are looked at
    /// without following a link first (`lstat`), and nothing is made through one.
    async fn init(&self, author: &GitIdentity) -> WorkspaceResult<()> {
        let refuse = |what: &Path, is: &str| {
            WorkspaceError::Corrupt(format!(
                "{} is {is}, not a directory of its own: the scratch project {} cannot be used; remove the run first",
                what.display(),
                self.dir
            ))
        };
        match lstat_if_exists(&self.path).await? {
            Some(meta) if !meta.is_dir() => return Err(refuse(&self.path, "a link or a file")),
            _ => {}
        }
        let dot_git = self.path.join(".git");
        match lstat_if_exists(&dot_git).await? {
            Some(meta) if !meta.is_dir() => return Err(refuse(&dot_git, "a link or a file")),
            Some(_) => {}
            None => {
                tokio::fs::create_dir_all(&self.path)
                    .await
                    .map_err(|e| WorkspaceError::io("cannot create the scratch directory", e))?;
                // `--template=`: none of the system's sample hooks.
                self.ws
                    .git()
                    .args(["init", "--quiet", "--initial-branch=main", "--template="])
                    .arg(&self.path)
                    .run()
                    .await?;
                // A link put there while the repository was being made: the repository is where
                // the link leads, and the project is not usable.
                if !safe_fs::is_plain_path(&self.ws.run_dir(&self.run), &self.path).await {
                    return Err(refuse(&self.path, "a link"));
                }
            }
        }
        let has_commit = self
            .git()
            .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .run_status()
            .await?
            .success;
        if !has_commit {
            self.git()
                .config("user.name", &author.name)
                .config("user.email", &author.email)
                .args([
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "Start of the scratch project",
                ])
                .run()
                .await?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------ copy_into

/// A file [`copy_into`] did not copy, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    /// The file, relative to the scratch project's root.
    pub path: PathBuf,
    /// What is in the way.
    pub reason: String,
}

/// What [`copy_into`] did. **All or nothing**: when `collisions` is not empty, nothing was
/// copied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// The files written: new ones, and (with `overwrite`) the ones that differed. Relative to
    /// the scratch project's root, sorted.
    pub copied: Vec<PathBuf>,
    /// The files that were already there with the same content, left alone.
    pub unchanged: Vec<PathBuf>,
    /// The files that stopped the copy.
    pub collisions: Vec<Collision>,
}

/// What to do with one file.
enum Step {
    Copy,
    Unchanged,
    /// There is a file or a link of another content: it is replaced when `overwrite` is on.
    Differs(String),
    /// Never, whatever `overwrite` says.
    Refuse(String),
}

/// Copy the files of the scratch project `from` into the worktree `to`, under the directory `path`
/// of it (`.` or empty for its root): what turns a scratch project into the change a pull request
/// carries.
///
/// * Regular files are copied with their mode (the executable bit stays), each written next to its
///   place and renamed over it. A symbolic link is copied only if its target is relative and stays
///   inside the project; any other is a collision.
/// * A file that is already there with the same content is `unchanged`. One that differs is a
///   collision, unless `overwrite`, which replaces it. A directory, a symbolic link, or
///   anything behind a symbolic link of the repository is in the way whatever `overwrite` says.
/// * Nothing is written inside `.git`, and `path` may not name it.
/// * **All or nothing**: if any file collides, nothing is copied and the report lists every
///   collision. Files that are only `unchanged` do not stop it.
///
/// # Errors
///
/// [`WorkspaceError::Invalid`] for a `path` that is absolute, goes up with `..` or names `.git`;
/// a git failure listing the project's files; I/O errors while copying.
#[tracing::instrument(skip(from, to), fields(from = %from.dir, to = %to.path().display()))]
pub async fn copy_into(
    from: &Scratch,
    to: &Worktree,
    path: &str,
    overwrite: bool,
) -> WorkspaceResult<CopyReport> {
    let base = destination_base(path)?;
    let files = from.files().await?;
    let source = from.path.clone();
    let dest = to.path().to_path_buf();
    tokio::task::spawn_blocking(move || copy_files(&source, &dest, &base, &files, overwrite))
        .await
        .map_err(|e| WorkspaceError::io("the copy task failed", io::Error::other(e)))?
}

/// The directory of the worktree the files go under, as a relative path (empty for the root).
fn destination_base(path: &str) -> WorkspaceResult<PathBuf> {
    let invalid = |why: &str| WorkspaceError::Invalid(format!("path {path:?} {why}"));
    let mut base = PathBuf::new();
    for component in Path::new(path.trim()).components() {
        match component {
            Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".git") => {
                return Err(invalid("is inside .git"));
            }
            Component::Normal(name) => base.push(name),
            Component::CurDir => {}
            Component::ParentDir => return Err(invalid("goes up with `..`")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid("is absolute: give a directory of the repository"));
            }
        }
    }
    Ok(base)
}

/// Whether the relative link `target` of the file `rel` stays inside the project, by its text.
fn link_stays_inside(rel: &Path, target: &Path) -> bool {
    let mut at: Vec<&std::ffi::OsStr> =
        rel.parent().map(|p| p.iter().collect()).unwrap_or_default();
    for component in target.components() {
        match component {
            Component::Normal(name) => at.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if at.pop().is_none() {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    !at.iter()
        .any(|name| name.to_string_lossy().eq_ignore_ascii_case(".git"))
}

/// Whether two files have the same bytes.
fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    Ok(ma.len() == mb.len() && std::fs::read(a)? == std::fs::read(b)?)
}

/// What to do with `rel` (a file of the project under `source`) in the worktree `dest` under
/// `base`.
fn plan_one(source: &Path, dest: &Path, base: &Path, rel: &Path) -> io::Result<Step> {
    if rel.components().any(
        |c| matches!(c, Component::Normal(n) if n.to_string_lossy().eq_ignore_ascii_case(".git")),
    ) {
        return Ok(Step::Refuse("nothing is copied into .git".to_owned()));
    }
    let from = source.join(rel);
    let from_meta = std::fs::symlink_metadata(&from)?;
    // Anything on the way that is a symbolic link or a file leads somewhere the copy must not.
    let mut at = dest.to_path_buf();
    let parent = base.join(rel.parent().unwrap_or(Path::new("")));
    for component in parent.components() {
        at.push(component);
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Ok(Step::Refuse(format!(
                    "the repository has a symbolic link at {}: nothing is written through it",
                    at.strip_prefix(dest).unwrap_or(&at).display()
                )));
            }
            Ok(meta) if !meta.is_dir() => {
                return Ok(Step::Refuse(format!(
                    "the repository has a file at {}, where a directory is needed",
                    at.strip_prefix(dest).unwrap_or(&at).display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(e),
        }
    }
    let to = dest.join(base).join(rel);
    let existing = match std::fs::symlink_metadata(&to) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let kind = from_meta.file_type();
    if kind.is_symlink() {
        let target = std::fs::read_link(&from)?;
        if !link_stays_inside(rel, &target) {
            return Ok(Step::Refuse(format!(
                "the symbolic link points to {}, which is absolute or outside the project",
                target.display()
            )));
        }
        return Ok(match existing {
            None => Step::Copy,
            Some(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&to)? == target {
                    Step::Unchanged
                } else {
                    Step::Differs(
                        "the repository has a symbolic link here with another target".to_owned(),
                    )
                }
            }
            Some(meta) if meta.is_dir() => {
                Step::Refuse("the repository has a directory here".to_owned())
            }
            Some(_) => Step::Differs("the repository has a file here".to_owned()),
        });
    }
    if !kind.is_file() {
        return Ok(Step::Refuse("it is not a regular file".to_owned()));
    }
    Ok(match existing {
        None => Step::Copy,
        Some(meta) if meta.is_dir() => {
            Step::Refuse("the repository has a directory here".to_owned())
        }
        Some(meta) if meta.file_type().is_symlink() => {
            Step::Refuse("the repository has a symbolic link here".to_owned())
        }
        Some(_) => {
            if same_content(&from, &to)? {
                Step::Unchanged
            } else {
                Step::Differs("the repository has a file here with other content".to_owned())
            }
        }
    })
}

fn copy_files(
    source: &Path,
    dest: &Path,
    base: &Path,
    files: &[PathBuf],
    overwrite: bool,
) -> WorkspaceResult<CopyReport> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let io_err = |what: &str, e: io::Error| WorkspaceError::io(what.to_owned(), e);
    let mut report = CopyReport::default();
    let mut to_write: Vec<&PathBuf> = Vec::new();
    for rel in files {
        match plan_one(source, dest, base, rel)
            .map_err(|e| io_err("cannot look at a file to copy", e))?
        {
            Step::Copy => to_write.push(rel),
            Step::Unchanged => report.unchanged.push(rel.clone()),
            Step::Differs(_) if overwrite => to_write.push(rel),
            Step::Differs(reason) | Step::Refuse(reason) => report.collisions.push(Collision {
                path: rel.clone(),
                reason,
            }),
        }
    }
    if !report.collisions.is_empty() {
        return Ok(report);
    }
    for rel in to_write {
        let from = source.join(rel);
        let to = dest.join(base).join(rel);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io_err("cannot create a directory of the repository", e))?;
        }
        let tmp = to.with_file_name(format!(
            ".adam-copy-{}-{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let made = (|| -> io::Result<()> {
            if std::fs::symlink_metadata(&from)?.file_type().is_symlink() {
                std::os::unix::fs::symlink(std::fs::read_link(&from)?, &tmp)?;
            } else {
                // `copy` keeps the permission bits: the executable bit stays.
                std::fs::copy(&from, &tmp)?;
            }
            std::fs::rename(&tmp, &to)
        })();
        if let Err(e) = made {
            let _ = std::fs::remove_file(&tmp);
            return Err(io_err("cannot copy a file", e));
        }
        report.copied.push(rel.clone());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_directory_is_the_name_then_the_name_and_the_owner_then_a_number() {
        let mut taken = HashSet::new();
        assert_eq!(pick_slot_dir("Lib", "acme", &taken), "lib");
        taken.insert("lib".to_owned());
        assert_eq!(pick_slot_dir("Lib", "Acme", &taken), "lib-acme");
        taken.insert("lib-acme".to_owned());
        assert_eq!(pick_slot_dir("lib", "acme", &taken), "lib-acme-2");
        taken.insert("lib-acme-2".to_owned());
        assert_eq!(pick_slot_dir("lib", "acme", &taken), "lib-acme-3");
    }

    #[test]
    fn a_scratch_name_is_a_plain_one() {
        for ok in ["scratch", "fib", "a", "x1.y_z-w", &"a".repeat(64)] {
            assert!(validate_scratch_dir(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Fib",
            ".hidden",
            "-x",
            "_x",
            "a b",
            "a/b",
            "x.git",
            "..",
            &"a".repeat(65),
        ] {
            assert!(validate_scratch_dir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_link_stays_inside_when_its_text_does() {
        let inside = |rel: &str, target: &str| link_stays_inside(Path::new(rel), Path::new(target));
        assert!(inside("a/link", "b.txt"));
        assert!(inside("a/link", "../b.txt"));
        assert!(inside("a/b/link", "../../c/d"));
        assert!(!inside("a/link", "../../b.txt"));
        assert!(!inside("link", "../x"));
        assert!(!inside("link", "/etc/passwd"));
        assert!(!inside("a/link", "../.git/config"));
    }

    #[test]
    fn a_destination_may_not_leave_the_repository_or_name_git() {
        assert_eq!(destination_base("").unwrap(), PathBuf::new());
        assert_eq!(destination_base(".").unwrap(), PathBuf::new());
        assert_eq!(
            destination_base("apps/fib").unwrap(),
            PathBuf::from("apps/fib")
        );
        assert_eq!(
            destination_base("./apps//fib/").unwrap(),
            PathBuf::from("apps/fib")
        );
        for bad in ["..", "a/../..", "/etc", ".git", "a/.GIT/b"] {
            assert!(destination_base(bad).is_err(), "{bad}");
        }
    }
}
