//! What a run pod mounts of the workspace volume: its own run, and nothing of the others.
//!
//! The coder's volume holds every run's files (`workspaces/<run>/`), the shared bare mirrors
//! (`git/<host>/<owner>/<repo>.git`) and the runs' metadata (`meta/<run>/`). A run pod gets only
//! these, each **at the path the coder has it at** (a path means the same in both):
//!
//! | Mount | Mode | Why |
//! |---|---|---|
//! | `workspaces/<run>` | read-write | the run's slots: its worktrees and scratch projects |
//! | `workspaces/<run>/<slot>` for each slot, worktree or scratch project | read-write, on top | so that **the slot's directory is a mount point**: a name the pod cannot rename, move, remove or exchange (`EBUSY`), which is what the coder's `git`, that finds its work tree by name, relies on |
//! | `git/<host>/<owner>/<repo>.git` for each worktree slot | **read-only** | a worktree's `.git` file points into its mirror: git reads objects, refs and configuration there |
//! | `git/<host>/<owner>/<repo>.git/worktrees/<name>` for each worktree slot | read-write, on top | the worktree's own `HEAD` and `index`, which `git status` refreshes |
//! | `.../worktrees/<name>/HEAD`, `commondir`, `gitdir` and `logs` | **read-only**, on top of that | the branch the coder's `git` resets and commits (`HEAD`), what it follows into the mirror (`commondir`), what ties the directory to its slot (`gitdir`), and the reflog it appends to (`logs/HEAD`) |
//!
//! Nothing of `meta/`, of another run, or of another repository's mirror is in the pod, and the mirror is
//! never writable from it: its `config`, `hooks/` and refs are read by the coder's own `git` in every run's
//! worktree, and a run that could write them would run code in the coder.
//!
//! The four read-only entries are the files of the worktree's directory that the **coder's** `git` follows
//! outside it or acts on, and so the ones a run must not be able to replace: `HEAD` names the branch that
//! `reset` and `commit` move (a run that wrote another run's branch there would have the coder reset that
//! branch), `commondir` is read by git's reference store even when `GIT_COMMON_DIR` is set (a run that
//! rewrote it would move the refs the coder reads and writes), a link at `logs/HEAD` is appended to by `git`
//! (*verified* with git 2.43.0), and a `gitdir` that a run could write is no proof of whose directory it is.
//! A read-only bind mount of a file or of a directory cannot be written, renamed over, replaced or removed
//! from the pod (`EROFS`, `EBUSY`). A pod's own `git` never needs to write them: it cannot move a ref (the
//! mirror is read-only), so `HEAD` never changes, and a bare mirror keeps no reflog but the one `git worktree
//! add` made. A worktree whose entries are not what git wrote (a link, a missing file) gets no git mounts at
//! all: the coder refuses git on it too.
//!
//! A bind mount follows a **directory**, and not the name it had: a slot removed and added again has the
//! same name and a new directory, which the pod's mounts would not follow. So the line of a slot and the
//! lines of a worktree's directory in the mirror carry the *generation* of the directory
//! (`Slot::generation` and `Worktree::git_dir_generation`, new each time the coder makes it) after the path,
//! and a pod made for the earlier directory is made again. A slot made before generations were recorded
//! has none, and its line is what it always was, so a pod made for it is not made again for nothing.
//!
//! What the slot mounts protect is the slot's **directory**: a command of the run cannot rename it and put a
//! link at its name between the coder's look at it and `git`'s read of it. They do not protect what is
//! below it (the coder's file tools open those relative to the directory they hold, `SlotDir`), the
//! creation of a slot (the run's directory is the run's to write until the slot is there),
//! `.adam-base/`, or the entries of a worktree's directory in the mirror that are not sealed (the index).
//!
//! The worktree's directory in the mirror is taken from **the coder's own metadata** (`Worktree::git_dir`),
//! never from the slot's `.git` file: the run's commands can write that file, and a name read from it could
//! be another run's directory, which would then be mounted read-write into this run's pod.
//!
//! The mounts of a pod are fixed when it is made, so the set it was made with is an annotation
//! ([`MOUNTS_ANNOTATION`](crate::MOUNTS_ANNOTATION)) and [`ensure`](adam_workspace::Environment::ensure)
//! compares it with the run's workspace each time: a slot added since needs mounts the pod does not have.
//! The coder's file and git tools do not call `ensure`, so it calls
//! [`seal`](adam_workspace::Environment::seal) right after it adds a slot, which makes the same comparison
//! and deletes a stale pod when nothing runs in it.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use adam_workspace::{EnvError, RunWorkspace, safe_fs};

/// One mount of the workspace volume in a run pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    path: PathBuf,
    sub_path: String,
    read_only: bool,
    /// Which making of the directory this is, for a mount of a slot or of a worktree's directory in the mirror.
    generation: Option<String>,
}

impl Mount {
    /// Where it is, in the pod and in the coder: an absolute path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory of the volume it shows: `path` relative to where the template mounts the volume.
    pub fn sub_path(&self) -> &str {
        &self.sub_path
    }

    /// Whether the pod may not write it.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The generation of the directory the mount shows ([`SlotMount::generation`],
    /// [`WorktreeDirs::generation`]); `None` for a mount of neither, and for a directory made before it
    /// was recorded.
    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }

    /// `rw <path>` or `ro <path>`, and ` @<generation>` when it has one: a line of the annotation.
    fn line(&self) -> String {
        format!(
            "{} {}{}",
            if self.read_only { "ro" } else { "rw" },
            self.path.display(),
            self.generation
                .as_ref()
                .map(|generation| format!(" @{generation}"))
                .unwrap_or_default()
        )
    }
}

/// The entries of a worktree's directory in the mirror that are mounted read-only on top of it.
pub(crate) const SEALED: [&str; 4] = ["HEAD", "commondir", "gitdir", "logs"];

/// The git directories a worktree slot needs in the pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeDirs {
    /// The bare mirror: `<root>/git/<host>/<owner>/<repo>.git`.
    pub mirror: PathBuf,
    /// The worktree's own directory in it: `<mirror>/worktrees/<name>`.
    pub admin: PathBuf,
    /// Which making of that directory this is (`Worktree::git_dir_generation`): recorded with the
    /// mounts of a pod, so that a directory made again under the same name makes the pod again.
    /// `None` for a worktree made before it was recorded.
    pub generation: Option<String>,
}

/// A slot's directory, which the pod mounts on its own on top of the run's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotMount {
    /// The slot's directory: `<run_dir>/<slot>`.
    pub path: PathBuf,
    /// Which making of that directory this is (`Slot::generation`): recorded with the mounts of a
    /// pod, so that a slot removed and added again under the same name makes the pod again.
    /// `None` for a slot made before it was recorded.
    pub generation: Option<String>,
}

/// The mounts of the workspace volume a run pod gets, for one run's workspace as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMounts {
    /// Sorted by path, so that a directory comes before what is mounted inside it.
    mounts: Vec<Mount>,
}

impl WorkspaceMounts {
    /// The mounts for `run_dir` (read-write), each of its `slots` (read-write, on top: a slot is a direct
    /// child of `run_dir`) and the git directories of its worktree slots. `base` is where the template
    /// mounts the volume: every path is inside it, and a mount's sub path is relative to it.
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] for a path that is not absolute and plain text, is not below `base`, a slot
    /// that is not a direct child of `run_dir`, or a worktree directory that is not inside its mirror's
    /// `worktrees/`.
    pub fn plan(
        base: &Path,
        run_dir: &Path,
        slots: &[SlotMount],
        worktrees: &[WorktreeDirs],
    ) -> Result<Self, EnvError> {
        let mut wanted: BTreeMap<PathBuf, (bool, Option<String>)> = BTreeMap::new();
        wanted.insert(run_dir.to_owned(), (false, None));
        for slot in slots {
            let direct = slot.path.parent() == Some(run_dir) && slot.path.file_name().is_some();
            if !direct {
                return Err(EnvError::Refused(format!(
                    "{} is not a slot of {}",
                    slot.path.display(),
                    run_dir.display()
                )));
            }
            wanted
                .entry(slot.path.clone())
                .or_insert((false, slot.generation.clone()));
        }
        for dirs in worktrees {
            let inside = dirs
                .admin
                .strip_prefix(dirs.mirror.join("worktrees"))
                .is_ok_and(|rest| rest.components().count() == 1);
            if !inside {
                return Err(EnvError::Refused(format!(
                    "{} is not a worktree directory of the mirror {}",
                    dirs.admin.display(),
                    dirs.mirror.display()
                )));
            }
            wanted.entry(dirs.mirror.clone()).or_insert((true, None));
            wanted
                .entry(dirs.admin.clone())
                .or_insert((false, dirs.generation.clone()));
            for entry in SEALED {
                wanted
                    .entry(dirs.admin.join(entry))
                    .or_insert((true, dirs.generation.clone()));
            }
        }
        let mut mounts = Vec::with_capacity(wanted.len());
        for (path, (read_only, generation)) in wanted {
            let sub_path = sub_path(base, &path)?;
            mounts.push(Mount {
                path,
                sub_path,
                read_only,
                generation,
            });
        }
        Ok(Self { mounts })
    }

    /// The mounts, a directory before what is mounted inside it.
    pub fn iter(&self) -> impl Iterator<Item = &Mount> {
        self.mounts.iter()
    }

    /// The annotation's value: one `rw <path>` or `ro <path>` line for each mount.
    pub fn encode(&self) -> String {
        self.mounts
            .iter()
            .map(Mount::line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The first mount of these that a pod made with `recorded` (its annotation, `None` for a pod
    /// without one: made before mounts were recorded, so it mounts the whole volume) does not have.
    /// A mount the pod has and the workspace no longer needs is harmless and does not count.
    pub fn missing_from(&self, recorded: Option<&str>) -> Option<&Mount> {
        let has: HashSet<&str> = recorded.unwrap_or_default().lines().collect();
        self.mounts
            .iter()
            .find(|mount| !has.contains(mount.line().as_str()))
    }

    /// The mounts of `workspace` as it is on disk now, for a template that mounts the volume at `base`.
    ///
    /// The run's directory is made first (by the coder, which owns the volume: a missing directory is made
    /// by the kubelet as root). Every slot inside the run's directory is a mount of its own, and a worktree
    /// slot gets its git directories too: the legacy worktree of a run (`<root>/worktrees/<run>`) is outside
    /// the run's directory, where no command may run, and gets nothing.
    /// A worktree whose directory in the mirror the coder's metadata cannot tell (a slot made before it was
    /// recorded, with no or several candidates in the mirror) gets no git mounts and is logged: less access
    /// is safe, and the next `ensure` tries again.
    ///
    /// # Errors
    ///
    /// [`EnvError::Unavailable`] when the slots cannot be listed; [`EnvError::Refused`] as
    /// [`plan`](Self::plan).
    pub(crate) async fn of(base: &Path, workspace: &RunWorkspace) -> Result<Self, EnvError> {
        let run_dir = workspace.path();
        if let Err(e) = tokio::fs::create_dir_all(run_dir).await {
            tracing::debug!(dir = %run_dir.display(), error = %e, "cannot make the run's directory; the pod's volume mount makes it");
        }
        let slots = workspace.slots().await.map_err(|e| {
            EnvError::Unavailable(format!(
                "cannot list the run's slots to decide what its pod mounts: {}",
                adam_error::report(&e)
            ))
        })?;
        let mut slot_mounts = Vec::new();
        let mut dirs = Vec::new();
        for slot in &slots {
            // The legacy worktree of a run is outside its directory, where no command may run.
            if !slot.path().starts_with(run_dir) {
                continue;
            }
            slot_mounts.push(SlotMount {
                path: slot.path().to_owned(),
                generation: slot.generation().map(str::to_owned),
            });
            let Some(worktree) = slot.worktree() else {
                continue;
            };
            match worktree.git_dir() {
                Some(admin) if sealable(admin).await => dirs.push(WorktreeDirs {
                    mirror: worktree.mirror().to_owned(),
                    admin: admin.to_owned(),
                    generation: worktree.git_dir_generation().map(str::to_owned),
                }),
                Some(admin) => tracing::warn!(
                    slot = slot.dir(),
                    admin = %admin.display(),
                    "git will not work in this slot of the run's pod: HEAD, commondir, gitdir or logs of its worktree directory is not what git wrote (a link, a pipe, a missing file), so they cannot be mounted read-only"
                ),
                None => tracing::warn!(
                    slot = slot.dir(),
                    "git will not work in this slot of the run's pod: which directory of the mirror is its worktree's is not known"
                ),
            }
        }
        Self::plan(base, run_dir, &slot_mounts, &dirs)
    }
}

/// Whether the entries of the worktree directory `admin` that the pod gets read-only are there and
/// are what git wrote: `HEAD`, `commondir` and `gitdir` regular files (looked at, not followed),
/// `logs` a directory (made, if it is not there: the kubelet would make a missing one as root). A
/// link is not mounted: the mount follows it to a place the coder did not name.
async fn sealable(admin: &Path) -> bool {
    if !safe_fs::is_real_directory(admin).await {
        return false;
    }
    for file in ["HEAD", "commondir", "gitdir"] {
        if !safe_fs::is_real_file(&admin.join(file)).await {
            return false;
        }
    }
    let logs = admin.join("logs");
    match tokio::fs::symlink_metadata(&logs).await {
        Ok(meta) => meta.is_dir(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir(&logs).await.is_ok()
        }
        Err(_) => false,
    }
}

/// `path` relative to `base`, as a volume's sub path.
fn sub_path(base: &Path, path: &Path) -> Result<String, EnvError> {
    let refuse = |why: &str| {
        EnvError::Refused(format!(
            "{} cannot be a mount of the workspace volume: {why}",
            path.display()
        ))
    };
    let relative = path
        .strip_prefix(base)
        .map_err(|_| refuse(&format!("it is not below {}", base.display())))?;
    if relative.as_os_str().is_empty() {
        return Err(refuse("a run pod never mounts the whole volume"));
    }
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => match part.to_str() {
                Some(part) if !part.chars().any(char::is_control) => parts.push(part),
                _ => return Err(refuse("it is not plain text")),
            },
            _ => return Err(refuse("it is not a plain path")),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "/work";
    const RUN: &str = "/work/workspaces/r1";

    fn dirs(mirror: &str, name: &str) -> WorktreeDirs {
        WorktreeDirs {
            mirror: PathBuf::from(mirror),
            admin: PathBuf::from(format!("{mirror}/worktrees/{name}")),
            generation: None,
        }
    }

    fn plan(worktrees: &[WorktreeDirs]) -> WorkspaceMounts {
        plan_with(&[], worktrees)
    }

    fn plan_with(slots: &[SlotMount], worktrees: &[WorktreeDirs]) -> WorkspaceMounts {
        WorkspaceMounts::plan(Path::new(BASE), Path::new(RUN), slots, worktrees).unwrap()
    }

    fn slot(name: &str, generation: Option<&str>) -> SlotMount {
        SlotMount {
            path: PathBuf::from(format!("{RUN}/{name}")),
            generation: generation.map(str::to_owned),
        }
    }

    fn lines(mounts: &WorkspaceMounts) -> Vec<String> {
        mounts.encode().lines().map(str::to_owned).collect()
    }

    #[test]
    fn a_run_with_no_worktree_mounts_its_own_directory_and_nothing_else() {
        let mounts = plan(&[]);
        assert_eq!(lines(&mounts), ["rw /work/workspaces/r1"]);
        let mount = mounts.iter().next().unwrap();
        assert_eq!(mount.sub_path(), "workspaces/r1");
        assert!(!mount.read_only());
    }

    #[test]
    fn a_worktree_adds_its_mirror_read_only_and_its_own_directory_in_it_read_write() {
        let mounts = plan(&[dirs("/work/git/github.com/o/r.git", "r1")]);
        // By path: a mirror, then the worktree directory inside it and what is read-only inside
        // that, then the run's directory.
        assert_eq!(
            lines(&mounts),
            [
                "ro /work/git/github.com/o/r.git",
                "rw /work/git/github.com/o/r.git/worktrees/r1",
                "ro /work/git/github.com/o/r.git/worktrees/r1/HEAD",
                "ro /work/git/github.com/o/r.git/worktrees/r1/commondir",
                "ro /work/git/github.com/o/r.git/worktrees/r1/gitdir",
                "ro /work/git/github.com/o/r.git/worktrees/r1/logs",
                "rw /work/workspaces/r1",
            ]
        );
        let subs: Vec<(&str, bool)> = mounts
            .iter()
            .map(|m| (m.sub_path(), m.read_only()))
            .collect();
        assert!(subs.contains(&("git/github.com/o/r.git", true)));
        assert!(subs.contains(&("git/github.com/o/r.git/worktrees/r1", false)));
        assert!(subs.contains(&("git/github.com/o/r.git/worktrees/r1/HEAD", true)));
        assert!(subs.contains(&("git/github.com/o/r.git/worktrees/r1/commondir", true)));
        assert!(subs.contains(&("git/github.com/o/r.git/worktrees/r1/gitdir", true)));
        assert!(subs.contains(&("git/github.com/o/r.git/worktrees/r1/logs", true)));
        assert!(subs.contains(&("workspaces/r1", false)));
    }

    #[test]
    fn several_worktrees_are_each_mounted_once_and_a_mirror_comes_before_what_is_inside_it() {
        let mounts = plan(&[
            dirs("/work/git/github.com/o/b.git", "b"),
            dirs("/work/git/github.com/o/a.git", "a"),
            // The same mirror twice (two slots of one repository): one read-only mount.
            dirs("/work/git/github.com/o/a.git", "a2"),
        ]);
        let all = lines(&mounts);
        let at = |line: &str| all.iter().position(|l| l == line).unwrap();
        assert_eq!(
            all.iter()
                .filter(|l| l.starts_with("ro ") && l.ends_with(".git"))
                .count(),
            2,
            "{all:?}"
        );
        assert!(
            at("ro /work/git/github.com/o/a.git")
                < at("rw /work/git/github.com/o/a.git/worktrees/a")
        );
        assert!(
            at("rw /work/git/github.com/o/a.git/worktrees/a")
                < at("ro /work/git/github.com/o/a.git/worktrees/a/HEAD"),
            "a directory comes before what is mounted in it"
        );
        assert!(
            at("ro /work/git/github.com/o/b.git")
                < at("rw /work/git/github.com/o/b.git/worktrees/b")
        );
        // The run, two mirrors, three worktree directories with four read-only entries each.
        assert_eq!(all.len(), 1 + 2 + 3 * (1 + 4), "{all:?}");
    }

    #[test]
    fn what_the_pod_has_is_compared_with_what_the_workspace_needs() {
        let one = plan(&[dirs("/work/git/github.com/o/a.git", "a")]);
        let two = plan(&[
            dirs("/work/git/github.com/o/a.git", "a"),
            dirs("/work/git/github.com/o/b.git", "b"),
        ]);
        let recorded = one.encode();
        assert!(one.missing_from(Some(&recorded)).is_none());
        let missing = two.missing_from(Some(&recorded)).unwrap();
        assert_eq!(missing.path(), Path::new("/work/git/github.com/o/b.git"));
        // A slot that went away leaves the pod with a mount it does not need: not a reason to make it again.
        assert!(one.missing_from(Some(&two.encode())).is_none());
        // A pod made before mounts were recorded mounted the whole volume: it is made again.
        assert!(one.missing_from(None).is_some());
        // A mount that is there in the other mode is not the mount.
        assert!(
            one.missing_from(Some(&recorded.replace("ro /work/git", "rw /work/git")))
                .is_some()
        );
    }

    #[test]
    fn a_directory_made_again_under_the_same_name_is_not_the_one_the_pod_mounted() {
        let with = |generation: Option<&str>| {
            plan(&[WorktreeDirs {
                generation: generation.map(str::to_owned),
                ..dirs("/work/git/github.com/o/a.git", "a")
            }])
        };
        let (first, again) = (with(Some("1a-0")), with(Some("2b-1")));
        // The worktree's directory and what is read-only in it carry the generation; the mirror and the
        // run's own directory do not (they are not made again with a slot).
        assert_eq!(
            lines(&first),
            [
                "ro /work/git/github.com/o/a.git",
                "rw /work/git/github.com/o/a.git/worktrees/a @1a-0",
                "ro /work/git/github.com/o/a.git/worktrees/a/HEAD @1a-0",
                "ro /work/git/github.com/o/a.git/worktrees/a/commondir @1a-0",
                "ro /work/git/github.com/o/a.git/worktrees/a/gitdir @1a-0",
                "ro /work/git/github.com/o/a.git/worktrees/a/logs @1a-0",
                "rw /work/workspaces/r1",
            ]
        );
        assert!(first.missing_from(Some(&first.encode())).is_none());
        let missing = again.missing_from(Some(&first.encode())).unwrap();
        assert_eq!(
            missing.path(),
            Path::new("/work/git/github.com/o/a.git/worktrees/a")
        );
        assert_eq!(missing.generation(), Some("2b-1"));
        // The pod of the new directory is not made again for the old one going away: only a change
        // of what is mounted counts.
        assert!(first.missing_from(Some(&again.encode())).is_some());
        // A worktree with no generation (made before it was recorded) keeps the lines it always had,
        // so a pod made for it is not made again; one that has a generation now is.
        let none = with(None);
        assert!(
            none.encode()
                .contains("\nrw /work/git/github.com/o/a.git/worktrees/a\n")
        );
        assert!(none.missing_from(Some(&none.encode())).is_none());
        assert!(first.missing_from(Some(&none.encode())).is_some());
    }

    #[test]
    fn a_path_that_cannot_be_a_mount_is_refused() {
        let refused = |run: &str, worktrees: &[WorktreeDirs]| match WorkspaceMounts::plan(
            Path::new(BASE),
            Path::new(run),
            &[],
            worktrees,
        )
        .unwrap_err()
        {
            EnvError::Refused(why) => why,
            other => panic!("{other}"),
        };
        assert!(refused("/elsewhere/workspaces/r1", &[]).contains("not below /work"));
        assert!(refused("/work", &[]).contains("whole volume"));
        assert!(refused("/work/workspaces/../meta", &[]).contains("plain path"));
        assert!(refused("/work/workspaces/a\nb", &[]).contains("plain text"));
        let stray = WorktreeDirs {
            mirror: PathBuf::from("/work/git/github.com/o/a.git"),
            admin: PathBuf::from("/work/git/github.com/o/b.git/worktrees/a"),
            generation: None,
        };
        assert!(refused(RUN, &[stray]).contains("not a worktree directory of the mirror"));
        let deep = WorktreeDirs {
            mirror: PathBuf::from("/work/git/github.com/o/a.git"),
            admin: PathBuf::from("/work/git/github.com/o/a.git/worktrees/a/b"),
            generation: None,
        };
        assert!(refused(RUN, &[deep]).contains("not a worktree directory"));
    }

    #[test]
    fn every_slot_is_a_mount_of_its_own_on_top_of_the_runs_directory() {
        let mounts = plan_with(
            &[slot("notes", Some("2b-1")), slot("app", Some("1a-0"))],
            &[dirs("/work/git/github.com/o/r.git", "r1")],
        );
        let all = lines(&mounts);
        // By path: the mirror and what is in it, then the run's directory, then each slot in it.
        assert_eq!(
            all[all.len() - 3..],
            [
                "rw /work/workspaces/r1",
                "rw /work/workspaces/r1/app @1a-0",
                "rw /work/workspaces/r1/notes @2b-1",
            ]
        );
        let slots: Vec<_> = mounts
            .iter()
            .filter(|m| m.path() != Path::new(RUN) && m.path().starts_with(RUN))
            .map(|m| (m.sub_path(), m.read_only(), m.generation()))
            .collect();
        assert_eq!(
            slots,
            [
                ("workspaces/r1/app", false, Some("1a-0")),
                ("workspaces/r1/notes", false, Some("2b-1")),
            ],
            "read-write, relative to where the template mounts the volume"
        );
        assert_eq!(all.len(), 1 + (1 + 1 + 4) + 2, "{all:?}");
    }

    #[test]
    fn a_slot_comes_after_the_directory_it_is_mounted_in_whatever_order_it_is_given() {
        let mounts = plan_with(&[slot("zeta", None), slot("alpha", None)], &[]);
        assert_eq!(
            lines(&mounts),
            [
                "rw /work/workspaces/r1",
                "rw /work/workspaces/r1/alpha",
                "rw /work/workspaces/r1/zeta",
            ]
        );
    }

    #[test]
    fn a_slot_removed_and_added_again_under_the_same_name_is_missing_from_the_pod_of_the_earlier_one()
     {
        let first = plan_with(&[slot("notes", Some("1a-0"))], &[]);
        let again = plan_with(&[slot("notes", Some("2b-1"))], &[]);
        assert!(first.missing_from(Some(&first.encode())).is_none());
        let missing = again.missing_from(Some(&first.encode())).unwrap();
        assert_eq!(missing.path(), Path::new("/work/workspaces/r1/notes"));
        assert_eq!(missing.generation(), Some("2b-1"));
        // A slot that went away leaves the pod with a mount it does not need: not a reason to make it again.
        assert!(
            plan_with(&[], &[])
                .missing_from(Some(&first.encode()))
                .is_none()
        );
        // And a slot added since is one the pod lacks.
        let two = plan_with(
            &[slot("notes", Some("1a-0")), slot("site", Some("3c-2"))],
            &[],
        );
        let missing = two.missing_from(Some(&first.encode())).unwrap();
        assert_eq!(missing.path(), Path::new("/work/workspaces/r1/site"));
    }

    #[test]
    fn a_slot_made_before_generations_keeps_the_line_it_always_had() {
        let old = plan_with(&[slot("notes", None)], &[]);
        assert_eq!(
            lines(&old),
            ["rw /work/workspaces/r1", "rw /work/workspaces/r1/notes"]
        );
        // A pod made for it is not made again for nothing.
        assert!(old.missing_from(Some(&old.encode())).is_none());
        // One made again has a generation, and the pod made for the earlier one lacks it.
        let remade = plan_with(&[slot("notes", Some("2b-1"))], &[]);
        assert!(remade.missing_from(Some(&old.encode())).is_some());
    }

    #[test]
    fn a_pod_made_before_slots_were_mounted_lacks_every_slot_it_has() {
        let before = plan(&[dirs("/work/git/github.com/o/a.git", "a")]);
        let now = plan_with(
            &[slot("a", Some("1a-0")), slot("notes", None)],
            &[dirs("/work/git/github.com/o/a.git", "a")],
        );
        let missing = now.missing_from(Some(&before.encode())).unwrap();
        assert_eq!(missing.path(), Path::new("/work/workspaces/r1/a"));
    }

    #[test]
    fn a_slot_that_is_not_a_direct_child_of_the_runs_directory_is_refused() {
        let refused = |path: &str| {
            let slot = SlotMount {
                path: PathBuf::from(path),
                generation: None,
            };
            match WorkspaceMounts::plan(Path::new(BASE), Path::new(RUN), &[slot], &[]).unwrap_err()
            {
                EnvError::Refused(why) => why,
                other => panic!("{other}"),
            }
        };
        assert!(refused("/work/workspaces/r2/app").contains("not a slot of"));
        assert!(refused("/work/workspaces/r1/a/b").contains("not a slot of"));
        assert!(refused(RUN).contains("not a slot of"));
        assert!(refused("/work/workspaces/r1/..").contains("not a slot of"));
        assert!(refused("/work/workspaces/r1/a\nb").contains("plain text"));
    }
}
