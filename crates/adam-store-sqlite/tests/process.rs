//! One process per file: a second process is refused, and a killed one leaves claimable runs.
//!
//! The child is this test binary, re-executed with `--ignored --exact`: it opens the same file,
//! claims the run with an hour-long lease and waits to be killed. The parent refuses to open the
//! file while the child holds it, then `SIGKILL`s the child and proves the run is claimable at
//! once — the operating system released the lock, and `migrate` cleared the dead lease.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use adam_core::store::now;
use adam_core::{ClaimScope, NewRun, Store, StoreError};
use adam_store_sqlite::SqliteStore;
use jiff::SignedDuration;
use serde_json::json;
use uuid::Uuid;

fn unique(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).expect("the test target directory");
    dir.join(format!("{name}-{}.sqlite", Uuid::new_v4()))
}

fn lock_of(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    PathBuf::from(lock)
}

/// Kills and reaps the child on every path, including a panic before the explicit kill: a leaked
/// child would keep the test's stdout pipe open and hang the harness (and hold the database lock).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[test]
fn a_second_process_is_refused_and_a_killed_ones_leases_are_claimable() {
    let database = unique("process");
    let marker = unique("process-marker");
    let agent = format!("agent-{}", Uuid::new_v4());
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");

    // This process creates the run and closes the file, so the child can take the lock.
    let run = runtime.block_on(async {
        let store = SqliteStore::open(&database).expect("open");
        store.migrate().await.expect("migrate");
        store
            .create_run(NewRun::new(&agent, json!({"turn": 0})))
            .await
            .expect("create the run")
    });

    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "--exact",
                "a_child_process_holds_the_store",
                "--ignored",
                "--nocapture",
            ])
            .env("ADAM_SQLITE_CHILD_DB", &database)
            .env("ADAM_SQLITE_CHILD_MARKER", &marker)
            .env("ADAM_SQLITE_CHILD_AGENT", &agent)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the child"),
    );

    // Wait until the child has claimed the run: the marker is written after the claim.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !marker.exists() {
        if let Some(status) = child.0.try_wait().expect("poll the child") {
            panic!("the child exited before it claimed a run: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "the child never claimed a run within 30 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The lock is held by another process: fail closed, and say why.
    let refused = SqliteStore::open(&database).expect_err("a second process must be refused");
    assert!(
        matches!(refused, StoreError::InvalidInput(_)),
        "{refused:?}"
    );
    assert!(
        refused
            .to_string()
            .contains("already open by another process"),
        "{refused}"
    );

    // SIGKILL: the child cannot clean up, so only the operating system can release the lock.
    child.0.kill().expect("kill the child");
    let status = child.0.wait().expect("reap the child");
    #[cfg(unix)]
    assert_eq!(
        status.signal(),
        Some(9),
        "the child must die of SIGKILL, so the lock is released by the kernel and not by a guard"
    );

    // A restart opens at once, and the dead process's hour-long lease does not block the claim.
    runtime.block_on(async {
        let store = SqliteStore::open(&database).expect("the file is free after the kill");
        store.migrate().await.expect("migrate after the kill");
        let leases = store
            .claim_due(
                std::slice::from_ref(&agent),
                "parent",
                ClaimScope::Any,
                &[],
                now() + SignedDuration::from_secs(1),
                Duration::from_secs(30),
                10,
            )
            .await
            .expect("claim after the kill");
        assert_eq!(
            leases.len(),
            1,
            "the killed process's run is claimable at once"
        );
        assert_eq!(leases[0].run.id, run.id);
        assert_eq!(leases[0].worker, "parent");
    });

    let _ = std::fs::remove_file(&database);
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(lock_of(&database));
}

/// The child half of [`a_second_process_is_refused_and_a_killed_ones_leases_are_claimable`].
/// Ignored, so only the parent's re-execution runs it.
#[cfg(unix)]
#[test]
#[ignore = "spawned by a_second_process_is_refused_and_a_killed_ones_leases_are_claimable"]
fn a_child_process_holds_the_store() {
    let database = std::env::var("ADAM_SQLITE_CHILD_DB").expect("the child database");
    let marker = std::env::var("ADAM_SQLITE_CHILD_MARKER").expect("the child marker");
    let agent = std::env::var("ADAM_SQLITE_CHILD_AGENT").expect("the child agent");
    let runtime = tokio::runtime::Runtime::new().expect("a runtime");
    runtime.block_on(async {
        let store = SqliteStore::open(&database).expect("the child opens the file");
        store.migrate().await.expect("the child migrates");
        let leases = store
            .claim_due(
                std::slice::from_ref(&agent),
                "child",
                ClaimScope::Any,
                &[],
                now() + SignedDuration::from_secs(1),
                Duration::from_secs(3600),
                10,
            )
            .await
            .expect("the child claims the run");
        assert_eq!(leases.len(), 1, "the child claims the run");
        std::fs::write(&marker, b"claimed").expect("write the marker");
        // Wait to be killed: a clean exit would release the lock the test wants to observe.
        std::future::pending::<()>().await;
    });
}
