//! The pool of git processes (`MAX_GIT_PROCESSES`, ADR 0065): a git past the pool's size waits for a
//! seat, then is refused as busy, and the seat is given back when the process is gone.
//!
//! The gate is one per process, so this file is its own test binary and sets it once.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use adam_budget::{Budget, Refused};
use adam_workspace::{LocalGit, govern_git};

#[tokio::test]
async fn a_git_past_the_pool_waits_then_is_busy() {
    let budget = Budget::builder().git(1).build();
    assert!(govern_git(
        Arc::new(adam_admission_local::LocalAdmission::new(budget)),
        Duration::from_millis(200),
    ));
    let dir = tempfile::tempdir().unwrap();

    // A git that runs for a while: an alias that sleeps.
    let slow = LocalGit::new(dir.path(), None).args(["-c", "alias.slow=!sleep 3", "slow"]);
    let running = tokio::spawn(slow.output());
    tokio::time::sleep(Duration::from_millis(500)).await;

    let started = std::time::Instant::now();
    let err = LocalGit::new(dir.path(), None)
        .arg("--version")
        .output()
        .await
        .expect_err("the only seat is taken");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "it waited for a seat first"
    );
    let refused = err
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Refused>())
        .expect("busy is a Refused");
    assert!(matches!(refused, Refused::Busy { .. }), "{refused:?}");

    // The seat comes back with the process.
    running.await.unwrap().unwrap();
    let out = LocalGit::new(dir.path(), None)
        .arg("--version")
        .output()
        .await
        .unwrap();
    assert!(out.success);
}
