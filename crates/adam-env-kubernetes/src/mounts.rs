//! What a run pod mounts of the workspace volume: its own run, and nothing of the others.
//!
//! The coder's volume holds every run's files (`workspaces/<run>/`), the shared bare mirrors
//! (`git/<host>/<owner>/<repo>.git`) and the runs' metadata (`meta/<run>/`). A run pod gets only
//! these, each **at the path the coder has it at** (a path means the same in both):
//!
//! | Mount | Mode | Why |
//! |---|---|---|
//! | `workspaces/<run>` | read-write | the run's slots: its worktrees and scratch projects |
//! | `git/<host>/<owner>/<repo>.git` for each worktree slot | **read-only** | a worktree's `.git` file points into its mirror: git reads objects, refs and configuration there |
//! | `git/<host>/<owner>/<repo>.git/worktrees/<name>` for each worktree slot | read-write, on top | the worktree's own `HEAD`, `index` and logs, which `git status` refreshes |
//!
//! Nothing of `meta/`, of another run, or of another repository's mirror is in the pod, and the mirror is
//! never writable from it: its `config`, `hooks/` and refs are read by the coder's own `git` in every run's
//! worktree, and a run that could write them would run code in the coder.
//!
//! The worktree's directory in the mirror is taken from **the coder's own metadata** (`Worktree::git_dir`),
//! never from the slot's `.git` file: the run's commands can write that file, and a name read from it could
//! be another run's directory, which would then be mounted read-write into this run's pod.
//!
//! The mounts of a pod are fixed when it is made, so the set it was made with is an annotation
//! ([`MOUNTS_ANNOTATION`](crate::MOUNTS_ANNOTATION)) and [`ensure`](adam_workspace::Environment::ensure)
//! compares it with the run's workspace each time: a worktree slot added since needs mounts the pod does
//! not have.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use adam_workspace::{EnvError, RunWorkspace};

/// One mount of the workspace volume in a run pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    path: PathBuf,
    sub_path: String,
    read_only: bool,
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

    /// `rw <path>` or `ro <path>`: a line of the annotation.
    fn line(&self) -> String {
        format!(
            "{} {}",
            if self.read_only { "ro" } else { "rw" },
            self.path.display()
        )
    }
}

/// The git directories a worktree slot needs in the pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeDirs {
    /// The bare mirror: `<root>/git/<host>/<owner>/<repo>.git`.
    pub mirror: PathBuf,
    /// The worktree's own directory in it: `<mirror>/worktrees/<name>`.
    pub admin: PathBuf,
}

/// The mounts of the workspace volume a run pod gets, for one run's workspace as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMounts {
    /// Sorted by path, so that a directory comes before what is mounted inside it.
    mounts: Vec<Mount>,
}

impl WorkspaceMounts {
    /// The mounts for `run_dir` (read-write) and the git directories of its worktree slots. `base` is where
    /// the template mounts the volume: every path is inside it, and a mount's sub path is relative to it.
    ///
    /// # Errors
    ///
    /// [`EnvError::Refused`] for a path that is not absolute and plain text, is not below `base`, or a
    /// worktree directory that is not inside its mirror's `worktrees/`.
    pub fn plan(base: &Path, run_dir: &Path, worktrees: &[WorktreeDirs]) -> Result<Self, EnvError> {
        let mut wanted: BTreeMap<PathBuf, bool> = BTreeMap::new();
        wanted.insert(run_dir.to_owned(), false);
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
            wanted.entry(dirs.mirror.clone()).or_insert(true);
            wanted.entry(dirs.admin.clone()).or_insert(false);
        }
        let mut mounts = Vec::with_capacity(wanted.len());
        for (path, read_only) in wanted {
            let sub_path = sub_path(base, &path)?;
            mounts.push(Mount {
                path,
                sub_path,
                read_only,
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
    /// by the kubelet as root). Only a worktree slot inside the run's directory gets git directories: the
    /// legacy worktree of a run (`<root>/worktrees/<run>`) is outside it, where no command may run.
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
        let mut dirs = Vec::new();
        for slot in &slots {
            let Some(worktree) = slot.worktree() else {
                continue;
            };
            if !slot.path().starts_with(run_dir) {
                continue;
            }
            match worktree.git_dir() {
                Some(admin) => dirs.push(WorktreeDirs {
                    mirror: worktree.mirror().to_owned(),
                    admin: admin.to_owned(),
                }),
                None => tracing::warn!(
                    slot = slot.dir(),
                    "git will not work in this slot of the run's pod: which directory of the mirror is its worktree's is not known"
                ),
            }
        }
        Self::plan(base, run_dir, &dirs)
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
        }
    }

    fn plan(worktrees: &[WorktreeDirs]) -> WorkspaceMounts {
        WorkspaceMounts::plan(Path::new(BASE), Path::new(RUN), worktrees).unwrap()
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
        // By path: a mirror, then the worktree directory inside it, then the run's directory.
        assert_eq!(
            lines(&mounts),
            [
                "ro /work/git/github.com/o/r.git",
                "rw /work/git/github.com/o/r.git/worktrees/r1",
                "rw /work/workspaces/r1",
            ]
        );
        let subs: Vec<(&str, bool)> = mounts
            .iter()
            .map(|m| (m.sub_path(), m.read_only()))
            .collect();
        assert!(subs.contains(&("git/github.com/o/r.git", true)));
        assert!(subs.contains(&("git/github.com/o/r.git/worktrees/r1", false)));
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
            all.iter().filter(|l| l.starts_with("ro ")).count(),
            2,
            "{all:?}"
        );
        assert!(
            at("ro /work/git/github.com/o/a.git")
                < at("rw /work/git/github.com/o/a.git/worktrees/a")
        );
        assert!(
            at("ro /work/git/github.com/o/b.git")
                < at("rw /work/git/github.com/o/b.git/worktrees/b")
        );
        assert_eq!(all.len(), 1 + 2 + 3, "{all:?}");
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
    fn a_path_that_cannot_be_a_mount_is_refused() {
        let refused = |run: &str, worktrees: &[WorktreeDirs]| match WorkspaceMounts::plan(
            Path::new(BASE),
            Path::new(run),
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
        };
        assert!(refused(RUN, &[stray]).contains("not a worktree directory of the mirror"));
        let deep = WorktreeDirs {
            mirror: PathBuf::from("/work/git/github.com/o/a.git"),
            admin: PathBuf::from("/work/git/github.com/o/a.git/worktrees/a/b"),
        };
        assert!(refused(RUN, &[deep]).contains("not a worktree directory"));
    }
}
