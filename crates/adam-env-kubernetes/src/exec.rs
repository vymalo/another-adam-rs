//! Running a command in a pod: `pods/exec` over a WebSocket.
//!
//! Two uses, one transport. The session's [`kill`](adam_workspace::EnvSession::kill) and the idle
//! sweep **capture** a short command's output ([`PodExec`]); `adam-kube-exec` **streams** one
//! command's stdin, stdout and stderr between the caller and the pod ([`stream`]) and ends with the
//! command's exit code. The exit code is in the stream's last message, a `Status` ([`exit_status`]).
//!
//! Closing the client does not stop what it started in the pod (a known property of exec, not
//! specific to this crate): `adam-exec kill <id>` does, which is what `kill` asks for.

use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use kube::Api;
use kube::api::AttachParams;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The bytes kept in memory between the pod and the pipe, each way. The default (1 KiB) turns a
/// build's output into thousands of tiny writes.
const PIPE_BYTES: usize = 64 * 1024;

/// The most of a captured command's output kept (the commands captured here print a number or
/// nothing).
const CAPTURE_CAP: usize = 64 * 1024;

/// Why a command could not be run in the pod, or its end not seen.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The cluster would not start the command (the pod or the container is not there, not ready,
    /// or the account may not `exec`).
    #[error("cannot start the command in the pod: {0}")]
    Start(String),
    /// The connection ended without the command's end being reported (the pod was deleted, the
    /// container restarted, a network failure).
    #[error("the connection to the pod ended before the command did: {0}")]
    Lost(String),
    /// A captured command took longer than it may.
    #[error("the command did not end in {0:?}")]
    Timeout(Duration),
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    /// It ended with this exit code (`0` is success). A command killed by a signal has the code
    /// the container runtime reports for it (128 plus the signal).
    Code(i32),
    /// The cluster says it could not run the command: the message.
    Failed(String),
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Code(code) => write!(f, "exit code {code}"),
            Self::Failed(message) => write!(f, "failed: {message}"),
        }
    }
}

/// What the end of a command says, from the stream's last message.
///
/// A success is `0`; a failure with a cause of reason `ExitCode` carries the code in its message
/// (what the kubelet answers for a non-zero exit); any other failure is the cluster's message.
pub fn exit_status(status: &Status) -> ExitStatus {
    if status.status.as_deref() == Some("Success") {
        return ExitStatus::Code(0);
    }
    let code = status
        .details
        .iter()
        .flat_map(|d| d.causes.iter().flatten())
        .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
        .and_then(|cause| cause.message.as_deref()?.trim().parse::<i32>().ok());
    match code {
        Some(code) => ExitStatus::Code(code),
        None => ExitStatus::Failed(crate::cluster::clip(
            status.message.as_deref().unwrap_or("the command failed"),
        )),
    }
}

/// The output of a command run to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// How it ended.
    pub status: ExitStatus,
    /// What it wrote to stdout, cut at 64 KiB.
    pub stdout: String,
    /// What it wrote to stderr, cut at 64 KiB.
    pub stderr: String,
}

/// Runs a short command in a pod and returns its output: the seam the session and the sweep use,
/// so that tests do not need a cluster.
#[async_trait]
pub(crate) trait PodExec: Send + Sync {
    /// Run `argv` in the run container of `pod`, with no stdin, and wait for its end.
    async fn capture(
        &self,
        pod: &str,
        argv: Vec<String>,
        timeout: Duration,
    ) -> Result<Captured, ExecError>;
}

/// [`PodExec`] against the cluster.
pub(crate) struct ClusterExec {
    pub(crate) api: Api<Pod>,
    pub(crate) container: String,
}

#[async_trait]
impl PodExec for ClusterExec {
    async fn capture(
        &self,
        pod: &str,
        argv: Vec<String>,
        timeout: Duration,
    ) -> Result<Captured, ExecError> {
        let params = AttachParams::default()
            .container(self.container.clone())
            .stdin(false)
            .stdout(true)
            .stderr(true)
            .max_stdout_buf_size(PIPE_BYTES)
            .max_stderr_buf_size(PIPE_BYTES);
        let run = async {
            let mut process = self
                .api
                .exec(pod, argv, &params)
                .await
                .map_err(|e| ExecError::Start(crate::cluster::clip(&e.to_string())))?;
            let status = process.take_status();
            let mut out = process.stdout();
            let mut err = process.stderr();
            let (stdout, stderr) = tokio::join!(read_capped(out.take()), read_capped(err.take()));
            let status = match status {
                Some(status) => status.await,
                None => None,
            };
            let _ = process.join().await;
            let status = status
                .map(|s| exit_status(&s))
                .ok_or_else(|| ExecError::Lost("no status was reported".to_owned()))?;
            Ok(Captured {
                status,
                stdout,
                stderr,
            })
        };
        tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| ExecError::Timeout(timeout))?
    }
}

/// Read `reader` to its end, keeping the first [`CAPTURE_CAP`] bytes: the rest is read and dropped,
/// because a pipe nobody empties stops the stream, and with it the command's end.
async fn read_capped(reader: Option<impl AsyncRead + Unpin>) -> String {
    let mut kept = Vec::new();
    if let Some(mut reader) = reader {
        let mut chunk = vec![0u8; 8 * 1024];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = CAPTURE_CAP.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).into_owned()
}

/// Whether the local stdin is `/dev/null`: a caller that closed stdin (the coder does for every
/// command it does not talk to) gets a command with no stdin, which ends a `cat` at once, and the
/// stream of the WebSocket has nothing to close.
pub fn stdin_is_null() -> bool {
    use std::os::fd::AsFd as _;
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let stdin = std::io::stdin();
    let Ok(owned) = stdin.as_fd().try_clone_to_owned() else {
        return false;
    };
    let Ok(here) = std::fs::File::from(owned).metadata() else {
        return false;
    };
    let Ok(null) = std::fs::metadata("/dev/null") else {
        return false;
    };
    here.file_type().is_char_device() && here.rdev() == null.rdev()
}

/// One command to run in a pod.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    /// The pod.
    pub pod: &'a str,
    /// The container; `None`: the pod's default.
    pub container: Option<&'a str>,
    /// The command and its arguments.
    pub argv: Vec<String>,
    /// Whether the command gets a stdin (the caller's, until its EOF). Without, it has none.
    pub stdin: bool,
}

/// Run `target` with its streams joined to `stdin`, `stdout` and `stderr`, until the command ends.
///
/// With `target.stdin`, `stdin` is copied to the command until its EOF, which ends the command's
/// stdin (a Kubernetes that speaks `v5.channel.k8s.io`, 1.30 and later: *unverified* here; an older
/// one closes the whole stream instead).
///
/// # Errors
///
/// [`ExecError::Start`] when the cluster will not start it, [`ExecError::Lost`] when the stream
/// ended without the command's end.
pub async fn stream<I, O, E>(
    api: &Api<Pod>,
    target: Target<'_>,
    stdin: I,
    mut stdout: O,
    mut stderr: E,
) -> Result<ExitStatus, ExecError>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
{
    let mut params = AttachParams::default()
        .stdin(target.stdin)
        .stdout(true)
        .stderr(true)
        .max_stdin_buf_size(PIPE_BYTES)
        .max_stdout_buf_size(PIPE_BYTES)
        .max_stderr_buf_size(PIPE_BYTES);
    if let Some(container) = target.container {
        params = params.container(container);
    }
    let mut process = api
        .exec(target.pod, target.argv, &params)
        .await
        .map_err(|e| ExecError::Start(crate::cluster::clip(&e.to_string())))?;
    let status = process.take_status();
    let mut remote_out = process.stdout();
    let mut remote_err = process.stderr();
    // The copy of stdin ends with its EOF (the writer is shut down, which ends the remote stdin), or
    // is abandoned when the command ends: a stdin that never ends must not keep this alive.
    let pump = process
        .stdin()
        .filter(|_| target.stdin)
        .map(|mut remote_in| {
            let mut local = stdin;
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut local, &mut remote_in).await;
                let _ = remote_in.shutdown().await;
            })
        });
    let out = async {
        if let Some(remote) = remote_out.as_mut() {
            let _ = tokio::io::copy(remote, &mut stdout).await;
        }
        let _ = stdout.flush().await;
    };
    let err = async {
        if let Some(remote) = remote_err.as_mut() {
            let _ = tokio::io::copy(remote, &mut stderr).await;
        }
        let _ = stderr.flush().await;
    };
    tokio::join!(out, err);
    let status = match status {
        Some(status) => status.await,
        None => None,
    };
    if let Some(pump) = pump {
        pump.abort();
    }
    let _ = process.join().await;
    status
        .map(|s| exit_status(&s))
        .ok_or_else(|| ExecError::Lost("no status was reported".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{StatusCause, StatusDetails};

    fn failure(message: &str, reason: &str, exit_code: Option<&str>) -> Status {
        Status {
            status: Some("Failure".to_owned()),
            message: Some(message.to_owned()),
            reason: Some(reason.to_owned()),
            details: exit_code.map(|code| StatusDetails {
                causes: Some(vec![StatusCause {
                    reason: Some("ExitCode".to_owned()),
                    message: Some(code.to_owned()),
                    ..StatusCause::default()
                }]),
                ..StatusDetails::default()
            }),
            ..Status::default()
        }
    }

    #[test]
    fn an_exit_status_is_said_in_words() {
        assert_eq!(ExitStatus::Code(2).to_string(), "exit code 2");
        assert_eq!(
            ExitStatus::Failed("no pod".into()).to_string(),
            "failed: no pod"
        );
    }

    #[test]
    fn success_is_exit_code_zero() {
        let status = Status {
            status: Some("Success".to_owned()),
            ..Status::default()
        };
        assert_eq!(exit_status(&status), ExitStatus::Code(0));
    }

    #[test]
    fn a_non_zero_exit_carries_its_code_in_the_exit_code_cause() {
        let status = failure(
            "command terminated with non-zero exit code: command terminated with exit code 3",
            "NonZeroExitCode",
            Some("3"),
        );
        assert_eq!(exit_status(&status), ExitStatus::Code(3));
        // Killed by a signal: the runtime reports 128 plus the signal.
        assert_eq!(
            exit_status(&failure("x", "NonZeroExitCode", Some("137"))),
            ExitStatus::Code(137)
        );
    }

    #[test]
    fn any_other_failure_is_the_clusters_message() {
        let status = failure("container not found (\"run\")", "BadRequest", None);
        assert_eq!(
            exit_status(&status),
            ExitStatus::Failed("container not found (\"run\")".to_owned())
        );
        // A cause that is not a number is not an exit code.
        assert_eq!(
            exit_status(&failure("odd", "NonZeroExitCode", Some("many"))),
            ExitStatus::Failed("odd".to_owned())
        );
    }
}
