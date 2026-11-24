//! The disk budget (ADR 0065 decision 7): a clone past it asks the person, the reserve stays
//! free, and cleanup never removes unpushed work. Needs `git` on the PATH.

#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use adam_budget::{DiskReserve, Ratio};
use adam_error::{Classify, ErrorClass};
use adam_workspace::{
    DiskBudget, DiskSpace, FreeSpace, GitIdentity, RepoRef, StaticToken, WorkspaceError, Workspaces,
};

/// A disk whose free space the test sets.
struct FakeDisk {
    free: AtomicU64,
    total: u64,
}

impl FakeDisk {
    fn new(free: u64, total: u64) -> Arc<Self> {
        Arc::new(Self {
            free: AtomicU64::new(free),
            total,
        })
    }
}

impl FreeSpace for FakeDisk {
    fn probe(&self, _path: &Path) -> io::Result<DiskSpace> {
        Ok(DiskSpace {
            free: self.free.load(Ordering::SeqCst),
            total: self.total,
        })
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A bare repository with one commit on `main`, as a local remote.
fn remote(dir: &Path) -> RepoRef {
    let work = dir.join("seed");
    std::fs::create_dir_all(&work).expect("seed dir");
    git(&work, &["init", "-q", "-b", "main"]);
    std::fs::write(work.join("README.md"), "hello\n").expect("seed file");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "seed"]);
    let bare: PathBuf = dir.join("remote.git");
    git(
        dir,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().expect("utf-8"),
            bare.to_str().expect("utf-8"),
        ],
    );
    RepoRef::new(bare.to_str().expect("utf-8"), "main")
}

fn workspaces(dir: &Path) -> Workspaces {
    Workspaces::new(dir.join("root"), Arc::new(StaticToken::new("unused")))
}

fn plenty() -> Arc<FakeDisk> {
    FakeDisk::new(u64::MAX / 2, u64::MAX / 2)
}

#[tokio::test]
async fn a_clone_past_the_disk_budget_asks_the_person() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = remote(dir.path());
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("earlier-work.bin"), vec![1u8; 64 * 1024]).expect("filler");

    let over = workspaces(dir.path())
        .with_disk_budget(DiskBudget::new(1024, DiskReserve::NONE).with_free_space(plenty()));
    let err = over.prepare(&repo, "run-over").await.expect_err("refused");

    assert!(
        matches!(&err, WorkspaceError::DiskFull { used, limit: 1024, .. } if *used >= 64 * 1024),
        "{err:?}"
    );
    assert_eq!(err.class(), ErrorClass::Rejected, "a question, not a crash");
    assert!(err.to_string().contains("Free some space"), "{err}");
    assert!(
        !root.join("git").exists() && !root.join("worktrees").exists(),
        "a refused clone leaves nothing behind"
    );

    // The same request with room succeeds: the refusal was the budget, not the repository.
    let roomy = workspaces(dir.path())
        .with_disk_budget(DiskBudget::new(1 << 30, DiskReserve::NONE).with_free_space(plenty()));
    roomy.prepare(&repo, "run-roomy").await.expect("room");
}

#[tokio::test]
async fn the_reserve_is_kept_free() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = remote(dir.path());
    let root = dir.path().join("root");
    let ws = |disk: &Arc<FakeDisk>, reserve| {
        workspaces(dir.path()).with_disk_budget(
            DiskBudget::new(1 << 30, reserve)
                .with_free_space(Arc::clone(disk) as Arc<dyn FreeSpace>),
        )
    };

    // A fixed reserve: 4 KiB free is under the 8 KiB kept.
    let disk = FakeDisk::new(4096, 1_000_000);
    let err = ws(&disk, DiskReserve::Bytes(8192))
        .prepare(&repo, "run-a")
        .await
        .expect_err("under the reserve");
    assert!(
        matches!(
            err,
            WorkspaceError::DiskFull {
                free: 4096,
                reserve: 8192,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.class(), ErrorClass::Rejected);
    assert!(!root.join("git").exists(), "nothing was cloned");

    // A ratio follows the disk: 10 % of 100 000 is 10 000.
    let tenth = DiskReserve::Ratio(Ratio::new(1_000).expect("10 %"));
    let disk = FakeDisk::new(9_999, 100_000);
    let err = ws(&disk, tenth)
        .prepare(&repo, "run-b")
        .await
        .expect_err("one byte under");
    assert!(
        matches!(
            err,
            WorkspaceError::DiskFull {
                reserve: 10_000,
                ..
            }
        ),
        "{err:?}"
    );

    // Exactly the reserve is enough: "at least this much stays free".
    disk.free.store(10_000, Ordering::SeqCst);
    ws(&disk, tenth)
        .prepare(&repo, "run-b")
        .await
        .expect("at the reserve");
}

#[tokio::test]
async fn unpushed_work_is_never_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = remote(dir.path());
    let ws = workspaces(dir.path());
    let me = GitIdentity::new("adam", "adam@example.com");
    let runs = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();

    let clean = ws.prepare(&repo, "run-clean").await.expect("clean");
    let work = ws.prepare(&repo, "run-work").await.expect("work");
    std::fs::write(work.path().join("new.txt"), "not committed\n").expect("edit");

    // An uncommitted change is kept; a worktree with nothing of its own is not.
    let swept = ws.sweep_finished(&runs(&["run-clean", "run-work"])).await;
    assert_eq!(swept.removed, ["run-clean"]);
    assert_eq!(swept.kept, ["run-work"]);
    assert!(!clean.path().exists(), "the clean workspace was freed");
    assert!(
        work.path().join("new.txt").exists(),
        "the edit is still there"
    );

    // Committed but never pushed: still kept.
    assert!(
        work.commit_all("add new.txt", &me)
            .await
            .expect("commit")
            .is_some()
    );
    let swept = ws.sweep_finished(&runs(&["run-work"])).await;
    assert_eq!(swept.kept, ["run-work"], "a local commit is the only copy");
    assert!(work.path().exists());

    // Pushed: nothing to lose, so it goes.
    work.push().await.expect("push");
    let swept = ws.sweep_finished(&runs(&["run-work"])).await;
    assert_eq!(swept.removed, ["run-work"]);
    assert!(!work.path().exists());
}
