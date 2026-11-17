//! The local environment keeps a framed conversation on stdin and stdout: a first message, a silence
//! with stdin held open, a 64 KiB message, then the end of input (the process ends by itself with
//! status 0) or a kill (nothing of it is left). The same sequence is asked of a devcontainer
//! (`adam-devcontainer/tests/podman.rs`) and of a run pod (`adam-env-kubernetes/tests/cluster.rs`);
//! see `adam_workspace::testing_stdio`.
//!
//! Always runs. The silence is 20 s (`ADAM_TEST_STDIO_SILENCE_SECS` changes it) for the case that
//! ends by EOF, and 1 s for the case that ends by a kill.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use adam_workspace::LocalSession;
use adam_workspace::testing_stdio::{
    StdioProbe, framed_stdio_until_eof, framed_stdio_until_kill, silence,
};

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
