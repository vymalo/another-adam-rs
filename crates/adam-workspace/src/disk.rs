//! The disk budget of a [`Workspaces`](crate::Workspaces) root (ADR 0065 decision 7).
//!
//! File-system quotas need an administrator, so adam counts for itself: the bytes its own root
//! holds (mirrors and worktrees), and the free space the disk has left. The count is measured, not
//! kept in a counter: a counter drifts when a command deletes files, when a process dies, or when
//! two processes share a root, and a budget that drifts refuses work it has room for.

use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use adam_budget::{Budget, DiskReserve};

use crate::error::{WorkspaceError, WorkspaceResult};

/// What a disk has now, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskSpace {
    /// Free for a process without special rights.
    pub free: u64,
    /// The size of the file system.
    pub total: u64,
}

/// Where the free space of the disk comes from. A trait so that a test sets it, and a host with
/// another notion of "the disk" (a quota it knows of) can say so.
pub trait FreeSpace: Send + Sync + 'static {
    /// The space of the file system `path` is on. `path` exists.
    ///
    /// # Errors
    ///
    /// Whatever the system says when it cannot be read.
    fn probe(&self, path: &Path) -> io::Result<DiskSpace>;
}

/// The file system's own answer (`statvfs`).
#[derive(Clone, Copy, Debug, Default)]
pub struct HostDisk;

impl FreeSpace for HostDisk {
    fn probe(&self, path: &Path) -> io::Result<DiskSpace> {
        let stat = rustix::fs::statvfs(path).map_err(io::Error::from)?;
        Ok(DiskSpace {
            free: stat.f_bavail.saturating_mul(stat.f_frsize),
            total: stat.f_blocks.saturating_mul(stat.f_frsize),
        })
    }
}

/// How much disk workspaces and the cache may take, and how much of the disk stays free.
///
/// Checked by [`Workspaces`](crate::Workspaces) before it makes anything new on disk (a mirror, a
/// worktree). A refusal is [`WorkspaceError::DiskFull`], class `Rejected`: the person is asked, and
/// nothing is cloned.
#[derive(Clone)]
pub struct DiskBudget {
    limit: u64,
    reserve: DiskReserve,
    space: Arc<dyn FreeSpace>,
}

impl fmt::Debug for DiskBudget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskBudget")
            .field("limit", &self.limit)
            .field("reserve", &self.reserve)
            .finish_non_exhaustive()
    }
}

impl DiskBudget {
    /// At most `limit` bytes in the root, and `reserve` always free on the disk.
    pub fn new(limit: u64, reserve: DiskReserve) -> Self {
        Self {
            limit,
            reserve,
            space: Arc::new(HostDisk),
        }
    }

    /// The disk fields of a [`Budget`] (`disk`, `disk_reserve`).
    pub fn from_budget(budget: &Budget) -> Self {
        Self::new(budget.disk, budget.disk_reserve)
    }

    /// Read the free space from `space` instead of the host's file system.
    #[must_use]
    pub fn with_free_space(mut self, space: Arc<dyn FreeSpace>) -> Self {
        self.space = space;
        self
    }

    /// Whether the root at `root` has room to start something new.
    ///
    /// The size of what is about to be written is not known before a clone, so this asks for
    /// *headroom*, not for a fit: the root is under its budget, and the disk has at least the
    /// reserve free. A request that overshoots is caught by the next one.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::DiskFull`] when either does not hold; [`WorkspaceError::Io`] when the
    /// free space cannot be read, because a budget that cannot be checked is not a pass.
    pub(crate) async fn admit(&self, root: &Path) -> WorkspaceResult<()> {
        let (tree, space) = (root.to_path_buf(), Arc::clone(&self.space));
        let (used, space) = tokio::task::spawn_blocking(move || {
            let used = tree_size(&tree);
            (used, space.probe(&nearest_existing(&tree)))
        })
        .await
        .map_err(|e| WorkspaceError::io("the disk count was interrupted", io::Error::other(e)))?;
        let space = space
            .map_err(|e| WorkspaceError::io("cannot read the free space of the disk", e))?;
        let reserve = self.reserve.of_disk(space.total);
        if used >= self.limit || space.free < reserve {
            return Err(WorkspaceError::DiskFull {
                used,
                limit: self.limit,
                free: space.free,
                reserve,
            });
        }
        Ok(())
    }
}

/// The nearest ancestor of `path` that exists: a root that is not made yet is on its parent's disk.
fn nearest_existing(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new("/"))
        .to_path_buf()
}

/// The bytes `root` takes on disk (allocated blocks, so a sparse file counts what it holds). A link
/// is counted as itself and never followed; a hard link is counted once per name, which can only
/// overcount, the safe side for a budget. Entries that vanish or cannot be read meanwhile (another
/// run is working) are skipped.
fn tree_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                dirs.push(entry.path());
            } else {
                total = total.saturating_add(meta.blocks().saturating_mul(512));
            }
        }
    }
    total
}

/// What [`Workspaces::sweep_finished`](crate::Workspaces::sweep_finished) did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Runs whose workspace was removed.
    pub removed: Vec<String>,
    /// Runs whose workspace was kept: it holds unpushed work, or it could not be checked.
    pub kept: Vec<String>,
}
