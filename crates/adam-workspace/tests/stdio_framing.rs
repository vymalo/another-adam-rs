//! The local environment keeps a framed conversation on stdin and stdout: a first message, a silence
//! with stdin held open, a 64 KiB message, then the end of input (the process ends by itself with
//! status 0) or a kill (nothing of it is left). The same sequence is asked of a devcontainer
//! (`adam-devcontainer/tests/podman.rs`) and of a run pod (`adam-env-kubernetes/tests/cluster.rs`);
//! see `adam_workspace::testing_stdio`.
//!
//! Always runs. The silence is 20 s (`ADAM_TEST_STDIO_SILENCE_SECS` changes it) for the case that
//! ends by EOF, and 1 s for the case that ends by a kill.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use adam_workspace::testing_stdio::{
    StdioProbe, Step, framed_stdio_until_eof, framed_stdio_until_kill,
    processes_carrying_until_found, processes_carrying_until_gone, silence,
};
use adam_workspace::{EnvSession, ExecSpec, LocalSession};

/// A marker unique to this test and this run: another test's process must never match it.
fn marker(what: &str) -> String {
    format!(
        "probe-test-{what}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    )
}

#[tokio::test]
async fn local_stdin_stays_open_through_a_silence_and_eof_ends_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let probe = StdioProbe::new(dir.path()).silence(silence());
    if let Err(e) = framed_stdio_until_eof(&LocalSession, &probe).await {
        panic!("local environment: {e}");
    }
}

#[tokio::test]
async fn local_kill_leaves_nothing_of_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let probe = StdioProbe::new(dir.path()).silence(Duration::from_secs(1));
    if let Err(e) = framed_stdio_until_kill(&LocalSession, &probe).await {
        panic!("local environment: {e}");
    }
}

#[tokio::test]
async fn the_probe_sees_a_live_local_process_and_stops_seeing_it_after_the_kill() {
    let dir = tempfile::tempdir().unwrap();
    let marker = marker("live");
    let spec = ExecSpec::shell("exec sleep 300", dir.path()).env("ADAM_STDIO_PROBE", &marker);
    let prepared = LocalSession.prepare(&spec).unwrap();
    let mut child = prepared.command().kill_on_drop(true).spawn().unwrap();
    let wait = Duration::from_secs(10);
    // `spawn` returns after the fork: the child may not have exec'd, so the marker is not in its
    // `/proc` entry yet. The polling form waits for it instead of scanning once.
    let seen = processes_carrying_until_found(&LocalSession, dir.path(), &marker, wait)
        .await
        .unwrap();
    assert!(!seen.is_empty(), "the live process is found");
    child.kill().await.unwrap();
    let after = processes_carrying_until_gone(&LocalSession, dir.path(), &marker, wait)
        .await
        .unwrap();
    assert!(
        after.is_empty(),
        "nothing is found after the kill: {after:?}"
    );
}

#[tokio::test]
async fn polling_finds_a_process_that_appears_after_a_delay() {
    let dir = tempfile::tempdir().unwrap();
    let marker = marker("delayed");
    // The shell does not carry the marker; the process it execs 200 ms later does, so a scan
    // taken right after the spawn cannot see it yet.
    let spec = ExecSpec::shell(
        format!("sleep 0.2; exec env ADAM_STDIO_PROBE={marker} sleep 300"),
        dir.path(),
    );
    let prepared = LocalSession.prepare(&spec).unwrap();
    let mut child = prepared.command().kill_on_drop(true).spawn().unwrap();
    let wait = Duration::from_secs(10);
    let seen = processes_carrying_until_found(&LocalSession, dir.path(), &marker, wait)
        .await
        .unwrap();
    assert!(!seen.is_empty(), "the process is found once it appears");
    child.kill().await.unwrap();
}

#[tokio::test]
async fn polling_until_gone_returns_as_soon_as_the_process_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let marker = marker("gone");
    let spec = ExecSpec::shell("exec sleep 300", dir.path()).env("ADAM_STDIO_PROBE", &marker);
    let prepared = LocalSession.prepare(&spec).unwrap();
    let mut child = prepared.command().kill_on_drop(true).spawn().unwrap();
    let wait = Duration::from_secs(10);
    let seen = processes_carrying_until_found(&LocalSession, dir.path(), &marker, wait)
        .await
        .unwrap();
    assert!(!seen.is_empty(), "the live process is found");
    child.kill().await.unwrap();
    let started = Instant::now();
    let left = processes_carrying_until_gone(&LocalSession, dir.path(), &marker, wait)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(left.is_empty(), "nothing is left: {left:?}");
    assert!(
        elapsed < wait / 2,
        "until_gone returned after {elapsed:?}, not the whole {wait:?} deadline"
    );
}

#[tokio::test]
async fn a_probe_that_cannot_see_the_live_process_is_the_probe_step() {
    let dir = tempfile::tempdir().unwrap();
    let mut probe = StdioProbe::new(dir.path()).silence(Duration::from_secs(1));
    // The polling form waits for a process that never appears: keep that wait short.
    probe.deadline = Duration::from_secs(5);
    probe.probe_marker = Some("not-the-processs-marker".to_owned());
    for result in [
        framed_stdio_until_kill(&LocalSession, &probe).await,
        framed_stdio_until_eof(&LocalSession, &probe).await,
    ] {
        let e = result.expect_err("a wrong marker must fail");
        assert_eq!(e.step, Step::Probe, "{e}");
        assert!(e.to_string().contains("would prove nothing"), "{e}");
    }
}
