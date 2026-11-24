//! The conformance suite of [`adam_workspace::Confine`].
//!
//! [`confine_conformance!`] writes the cases into a module of the implementing crate:
//!
//! * a confined command still runs, in its directory and with its variables, and ends unstopped;
//! * a backend that enforces anything stops a file over its size limit, and says so;
//! * a backend that does not list `memory` among what it misses stops a memory hog, and the hog
//!   never finishes;
//! * a backend that enforces everything holds a flood of processes to `max_processes`.
//!
//! The hog and the flood only ever run in a backend that claims to cap them: a backend that does not
//! is not asked to, so the suite never starts one on a machine that nothing protects.
//!
//! ```text
//! fn make() -> adam_workspace::DynConfine {
//!     std::sync::Arc::new(adam_confine::Rlimits::new())
//! }
//!
//! adam_confine_testkit::confine_conformance!(crate::make);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // a test kit asserts by unwrapping

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use adam_workspace::{Confine, Confined, Ended, Enforcement, ProcessLimits, Stopped};
use tokio::process::Command;

/// How long a case waits for a command, at most.
pub const WAIT: Duration = Duration::from_secs(60);

/// What a confined command did.
#[derive(Debug)]
pub struct Ran {
    /// Its standard output.
    pub stdout: String,
    /// How it ended.
    pub ended: Ended,
    /// Whether a limit stopped it, as the backend says.
    pub stopped: Option<Stopped>,
}

/// Run `script` with `sh -c` in `dir`, confined by `confine` to `limits`, and wait for it.
pub async fn run(
    confine: &dyn Confine,
    limits: &ProcessLimits,
    dir: &Path,
    script: &str,
) -> (Ran, Confined) {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(script)
        .current_dir(dir)
        .env("ADAM_CONFINE_CASE", "yes");
    let confined = confine.confine(&mut cmd, limits).expect("confine");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = cmd.spawn().expect("spawn");
    let out = match tokio::time::timeout(WAIT, child.wait_with_output()).await {
        Ok(out) => out.expect("wait"),
        Err(_) => {
            confined.stop().await;
            panic!("the command did not end within {WAIT:?}");
        }
    };
    let ended = Ended::from(out.status);
    let stopped = confined.finish(ended).await;
    (
        Ran {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            ended,
            stopped,
        },
        confined,
    )
}

/// A confined command still runs, in its directory, with its variables.
pub async fn a_confined_command_still_runs(confine: &dyn Confine) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
    let limits = ProcessLimits::new()
        .with_memory(512 * 1024 * 1024)
        .with_file_size(64 * 1024 * 1024);
    let (ran, _) = run(
        confine,
        &limits,
        dir.path(),
        "ls; echo case=$ADAM_CONFINE_CASE",
    )
    .await;
    assert_eq!(ran.ended.code, Some(0), "{ran:?}");
    assert!(ran.stdout.contains("marker.txt"), "{ran:?}");
    assert!(ran.stdout.contains("case=yes"), "{ran:?}");
    assert_eq!(ran.stopped, None, "a command that passed was not stopped");
}

/// A file over the size limit is stopped, and the backend says it was the limit.
pub async fn a_file_over_the_size_limit_is_stopped(confine: &dyn Confine) {
    if confine.describe() == Enforcement::None {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let limit = 1024 * 1024;
    let limits = ProcessLimits::new().with_file_size(limit);
    let (ran, _) = run(
        confine,
        &limits,
        dir.path(),
        "exec head -c 8388608 /dev/zero > big.bin",
    )
    .await;
    let size = std::fs::metadata(dir.path().join("big.bin")).unwrap().len();
    assert!(size <= limit, "the file grew to {size} bytes: {ran:?}");
    assert!(
        !ran.stdout.contains("finished") || ran.ended.code != Some(0),
        "{ran:?}"
    );
    assert_eq!(ran.stopped, Some(Stopped::FileSize { limit }), "{ran:?}");
}

/// A memory hog is stopped before it finishes. Only asked of a backend that does not list `memory`
/// among what it misses.
pub async fn a_memory_hog_is_stopped(confine: &dyn Confine) {
    let enforced = match confine.describe() {
        Enforcement::Full => true,
        Enforcement::Partial(missing) => !missing.iter().any(|m| m.starts_with("memory")),
        _ => false,
    };
    if !enforced {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let limit = 256 * 1024 * 1024;
    let limits = ProcessLimits::new().with_memory(limit);
    let (ran, _) = run(
        confine,
        &limits,
        dir.path(),
        "x=$(head -c 700000000 /dev/zero | tr '\\0' a); echo hog-finished",
    )
    .await;
    assert!(
        !ran.stdout.contains("hog-finished"),
        "the hog was not stopped: {ran:?}"
    );
    assert_ne!(ran.ended.code, Some(0), "{ran:?}");
    if let Some(stopped) = ran.stopped {
        assert_eq!(stopped, Stopped::Memory { limit }, "{ran:?}");
    }
}

/// A flood of processes is held to `max_processes`. Only asked of a backend that enforces
/// everything: the flood is bounded by it alone.
pub async fn a_process_flood_is_held_to_its_limit(confine: &dyn Confine) {
    if confine.describe() != Enforcement::Full {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let max = 32;
    let limits = ProcessLimits::new().with_max_processes(max);
    // Each child that starts writes its pid and sleeps; at most 300 are tried.
    let script = "n=0; while [ $n -lt 300 ]; do n=$((n+1)); \
                  (echo $$ >> started; exec sleep 20) 2>/dev/null & done; sleep 3; wait";
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script).current_dir(dir.path());
    let confined = confine.confine(&mut cmd, &limits).expect("confine");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn");
    tokio::time::sleep(Duration::from_secs(6)).await;
    let started = std::fs::read_to_string(dir.path().join("started"))
        .unwrap_or_default()
        .lines()
        .count();
    confined.stop().await;
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let _ = confined
        .finish(Ended {
            code: None,
            signal: Some(9),
        })
        .await;
    assert!(started >= 1, "the flood did not start");
    assert!(
        started <= usize::try_from(max).unwrap(),
        "{started} processes started under a limit of {max}"
    );
}

/// Write the conformance cases for `$make`, a function that returns the [`adam_workspace::DynConfine`]
/// to test.
#[macro_export]
macro_rules! confine_conformance {
    ($make:path) => {
        mod confine_conformance {
            #[tokio::test(flavor = "multi_thread")]
            async fn a_confined_command_still_runs() {
                $crate::a_confined_command_still_runs(&*$make()).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_file_over_the_size_limit_is_stopped() {
                $crate::a_file_over_the_size_limit_is_stopped(&*$make()).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_memory_hog_is_stopped() {
                $crate::a_memory_hog_is_stopped(&*$make()).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn a_process_flood_is_held_to_its_limit() {
                $crate::a_process_flood_is_held_to_its_limit(&*$make()).await;
            }
        }
    };
}
