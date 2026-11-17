//! Does a process started in an environment keep a framed conversation on stdin and stdout?
//! (feature `test-util`)
//!
//! A language server is a long-running process that is written to on stdin for minutes and answers
//! on stdout, with JSON-RPC messages framed by `Content-Length`. Whether an environment can host
//! one is a fact about the environment: its `prepare` and the client it spawns (`adam-kube-exec`,
//! the devcontainer CLI's `exec`) must pass stdin through, keep it open while nothing is said, and
//! deliver the end of input. These functions ask exactly that of any [`EnvSession`], with `cat` as
//! the server (bytes in, the same bytes out), so that the three environments are held to one
//! sequence and a failure names the [`Step`] that broke:
//!
//! 1. [`Step::FirstMessage`]: one framed message is written and read back;
//! 2. [`Step::Silence`]: nothing is written for [`StdioProbe::silence`] with stdin held open, and the
//!    process must still be there;
//! 3. [`Step::LargeMessage`]: a framed message of 64 KiB (more than a pipe buffer) is written and
//!    read back;
//! 4. [`Step::Eof`] ([`framed_stdio_until_eof`]): stdin is closed and the process ends by itself with
//!    status 0; or [`Step::Kill`] ([`framed_stdio_until_kill`]): the process group is killed and so
//!    is the exec id in the environment, and no process that carries the run's marker is left in
//!    the environment.
//!
//! The process is started as a background command is (`ExecSpec::background`), with stdin and
//! stdout piped and a process group of its own.

use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::environment::{EnvSession, ExecId, ExecSpec};

/// How long stdin is held open without a byte when nothing else is said.
pub const DEFAULT_SILENCE: Duration = Duration::from_secs(20);

/// The variable that overrides [`DEFAULT_SILENCE`], in seconds: a CI step asks for longer with it.
pub const SILENCE_VAR: &str = "ADAM_TEST_STDIO_SILENCE_SECS";

/// The silence the environment asks for ([`SILENCE_VAR`]), else `default`.
pub fn silence_or(default: Duration) -> Duration {
    std::env::var(SILENCE_VAR)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .map_or(default, Duration::from_secs)
}

/// [`silence_or`] with [`DEFAULT_SILENCE`].
pub fn silence() -> Duration {
    silence_or(DEFAULT_SILENCE)
}

/// The size of the large message's body: not less than a Linux pipe buffer (64 KiB).
pub const LARGE_BODY: usize = 64 * 1024;

/// A step of the conversation, for the message of a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The process could not be prepared or spawned.
    Spawn,
    /// The first message, written and read back.
    FirstMessage,
    /// The silence with stdin held open.
    Silence,
    /// The 64 KiB message after the silence.
    LargeMessage,
    /// Closing stdin: the process must end by itself, with status 0.
    Eof,
    /// Killing the process: it and its process group must be gone from the environment.
    Kill,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Spawn => "spawn",
            Self::FirstMessage => "first message",
            Self::Silence => "silence with stdin open",
            Self::LargeMessage => "large message after the silence",
            Self::Eof => "end of input (EOF)",
            Self::Kill => "kill",
        })
    }
}

/// A step that failed, and what was seen.
#[derive(Debug)]
pub struct StdioFailure {
    /// Which step.
    pub step: Step,
    /// What was seen (including the tail of the process's stderr when there is one).
    pub detail: String,
}

impl fmt::Display for StdioFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "step `{}` failed: {}", self.step, self.detail)
    }
}

impl std::error::Error for StdioFailure {}

/// What to ask of an environment.
#[derive(Debug, Clone)]
pub struct StdioProbe {
    /// The working directory of the process: an absolute path that exists in the environment.
    pub cwd: PathBuf,
    /// How long stdin stays open without a byte ([`silence`]).
    pub silence: Duration,
    /// The longest each wait takes (a message back, the end of the process, the kill).
    pub deadline: Duration,
}

impl StdioProbe {
    /// A probe in `cwd` with [`silence`] and a deadline of 60 s.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            silence: silence(),
            deadline: Duration::from_secs(60),
        }
    }

    /// The same, with this silence.
    #[must_use]
    pub fn silence(mut self, silence: Duration) -> Self {
        self.silence = silence;
        self
    }
}

/// `Content-Length: N\r\n\r\n` and the body.
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

/// A JSON-RPC-looking body of exactly `size` bytes (at least 80), with text that varies along it so
/// that a dropped or repeated chunk cannot read back equal.
pub fn json_body(id: u32, size: usize) -> Vec<u8> {
    let head = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"probe","params":{{"text":""#);
    let tail = r#""}}"#;
    let fill = size.saturating_sub(head.len() + tail.len());
    let mut body = head.into_bytes();
    body.extend((0..fill).map(|i| b'a' + u8::try_from((i * 7 + i / 251) % 26).unwrap_or(0)));
    body.extend_from_slice(tail.as_bytes());
    body
}

struct Running {
    child: Child,
    pid: Option<u32>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    exec: ExecId,
    marker: String,
    stderr: Arc<Mutex<Vec<u8>>>,
}

fn fail(step: Step, detail: impl Into<String>) -> StdioFailure {
    StdioFailure {
        step,
        detail: detail.into(),
    }
}

impl Running {
    fn start(session: &dyn EnvSession, probe: &StdioProbe) -> Result<Self, StdioFailure> {
        let marker = format!(
            "adam-stdio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let spec = ExecSpec::shell("cat", &probe.cwd)
            .env("ADAM_STDIO_PROBE", &marker)
            .background();
        let prepared = session
            .prepare(&spec)
            .map_err(|e| fail(Step::Spawn, format!("prepare: {e}")))?;
        let mut cmd = prepared.command();
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| fail(Step::Spawn, format!("spawn {:?}: {e}", prepared.program)))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| fail(Step::Spawn, "no stdout pipe"))?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(mut err) = child.stderr.take() {
            let sink = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut buf = [0_u8; 4096];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut sink) = sink.lock() {
                        sink.extend_from_slice(&buf[..n]);
                    }
                }
            });
        }
        Ok(Self {
            child,
            pid,
            stdin,
            stdout: BufReader::new(stdout),
            exec: prepared.exec,
            marker,
            stderr,
        })
    }

    /// The tail of what the process wrote on stderr, for a failure's message.
    fn stderr_tail(&self) -> String {
        let bytes = self.stderr.lock().map(|b| b.clone()).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let chars: Vec<char> = text.chars().collect();
        let tail: String = chars[chars.len().saturating_sub(600)..].iter().collect();
        if tail.trim().is_empty() {
            "stderr: (nothing)".to_owned()
        } else {
            format!("stderr: {}", tail.trim())
        }
    }

    fn describe_end(&mut self) -> String {
        match self.child.try_wait() {
            Ok(Some(status)) => format!("the process had ended: {status}; {}", self.stderr_tail()),
            Ok(None) => format!("the process is still running; {}", self.stderr_tail()),
            Err(e) => format!("could not poll the process: {e}"),
        }
    }

    /// Read one framed message: its body.
    async fn read_message(&mut self, step: Step) -> Result<Vec<u8>, StdioFailure> {
        let mut length = None;
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .map_err(|e| fail(step, format!("reading stdout: {e}")))?;
            if n == 0 {
                return Err(fail(
                    step,
                    "stdout ended before a message header was complete",
                ));
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                length = Some(value.trim().parse::<usize>().map_err(|e| {
                    fail(
                        step,
                        format!("a Content-Length that is not a number ({line:?}): {e}"),
                    )
                })?);
            }
        }
        let length = length.ok_or_else(|| fail(step, "a header block with no Content-Length"))?;
        let mut body = vec![0_u8; length];
        self.stdout
            .read_exact(&mut body)
            .await
            .map_err(|e| fail(step, format!("reading {length} bytes of body: {e}")))?;
        Ok(body)
    }

    /// Write `body` framed and read exactly that back, within `deadline`. The write and the read go
    /// together: a large message fills the pipes in both directions.
    async fn round_trip(
        &mut self,
        step: Step,
        body: &[u8],
        deadline: Duration,
    ) -> Result<(), StdioFailure> {
        let framed = frame(body);
        let Some(mut stdin) = self.stdin.take() else {
            return Err(fail(step, "stdin is already closed"));
        };
        let writer = tokio::spawn(async move {
            let wrote = async {
                stdin.write_all(&framed).await?;
                stdin.flush().await
            }
            .await;
            (stdin, wrote)
        });
        let read = tokio::time::timeout(deadline, self.read_message(step)).await;
        let got = match read {
            Ok(Ok(got)) => got,
            Ok(Err(mut e)) => {
                e.detail = format!("{}; {}", e.detail, self.describe_end());
                writer.abort();
                return Err(e);
            }
            Err(_) => {
                writer.abort();
                return Err(fail(
                    step,
                    format!(
                        "no complete answer within {deadline:?} ({} bytes sent); {}",
                        body.len(),
                        self.describe_end()
                    ),
                ));
            }
        };
        match tokio::time::timeout(deadline, writer).await {
            Ok(Ok((stdin, Ok(())))) => self.stdin = Some(stdin),
            Ok(Ok((_, Err(e)))) => return Err(fail(step, format!("writing to stdin: {e}"))),
            _ => return Err(fail(step, "the write to stdin did not finish")),
        }
        if got != body {
            let at = got.iter().zip(body).position(|(a, b)| a != b);
            return Err(fail(
                step,
                format!(
                    "the answer differs from what was sent: {} bytes back, {} sent, first difference at {at:?}",
                    got.len(),
                    body.len()
                ),
            ));
        }
        Ok(())
    }

    /// Stdin open, nothing written, for `silence`; the process must stay.
    async fn stay_silent(&mut self, silence: Duration) -> Result<(), StdioFailure> {
        let started = Instant::now();
        while started.elapsed() < silence {
            let left = silence.saturating_sub(started.elapsed());
            tokio::time::sleep(left.min(Duration::from_secs(1))).await;
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(fail(
                    Step::Silence,
                    format!(
                        "the process ended after {:?} of {silence:?} with stdin open: {status}; {}",
                        started.elapsed(),
                        self.stderr_tail()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The conversation up to the point where stdin is still open and the second message was answered.
    async fn converse(&mut self, probe: &StdioProbe) -> Result<(), StdioFailure> {
        self.round_trip(Step::FirstMessage, &json_body(1, 120), probe.deadline)
            .await?;
        self.stay_silent(probe.silence).await?;
        self.round_trip(
            Step::LargeMessage,
            &json_body(2, LARGE_BODY),
            probe.deadline,
        )
        .await
    }

    async fn eof(&mut self, probe: &StdioProbe) -> Result<(), StdioFailure> {
        drop(self.stdin.take());
        let status = tokio::time::timeout(probe.deadline, self.child.wait())
            .await
            .map_err(|_| {
                fail(
                    Step::Eof,
                    format!(
                        "stdin was closed and the process did not end within {:?}: EOF did not reach it; {}",
                        probe.deadline,
                        self.stderr_tail()
                    ),
                )
            })?
            .map_err(|e| fail(Step::Eof, format!("waiting for the process: {e}")))?;
        if !status.success() {
            return Err(fail(
                Step::Eof,
                format!(
                    "the process ended with {status}, not 0; {}",
                    self.stderr_tail()
                ),
            ));
        }
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(probe.deadline, self.stdout.read_to_end(&mut rest)).await;
        if !rest.is_empty() {
            return Err(fail(
                Step::Eof,
                format!("{} bytes of output nobody asked for", rest.len()),
            ));
        }
        Ok(())
    }

    /// Kill as the coder does: the process group, then the environment's own `kill` of the exec id.
    async fn kill(&mut self, session: &dyn EnvSession, deadline: Duration) {
        kill_group(self.pid).await;
        let _ = tokio::time::timeout(deadline, session.kill(&self.exec)).await;
    }

    async fn killed(
        &mut self,
        session: &dyn EnvSession,
        probe: &StdioProbe,
    ) -> Result<(), StdioFailure> {
        self.kill(session, probe.deadline).await;
        let status = tokio::time::timeout(probe.deadline, self.child.wait())
            .await
            .map_err(|_| {
                fail(
                    Step::Kill,
                    format!(
                        "the process was still there {:?} after the kill",
                        probe.deadline
                    ),
                )
            })?
            .map_err(|e| fail(Step::Kill, format!("waiting for the process: {e}")))?;
        if status.success() {
            return Err(fail(
                Step::Kill,
                "the process ended with status 0: it was not killed",
            ));
        }
        // Nothing carrying the marker is left in the environment (it may take a moment to go).
        let started = Instant::now();
        loop {
            let left = self.leftovers(session, probe).await?;
            if left.is_empty() {
                return Ok(());
            }
            if started.elapsed() > probe.deadline {
                return Err(fail(
                    Step::Kill,
                    format!("after the kill, still carrying the run's marker: {left:?}"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// The `/proc` entries of processes of the environment whose environment holds this run's marker.
    async fn leftovers(
        &self,
        session: &dyn EnvSession,
        probe: &StdioProbe,
    ) -> Result<Vec<String>, StdioFailure> {
        let line = format!(
            "grep -l -a 'ADAM_STDIO_PROBE={}' /proc/[0-9]*/environ 2>/dev/null; true",
            self.marker
        );
        let prepared = session
            .prepare(&ExecSpec::shell(line, &probe.cwd))
            .map_err(|e| fail(Step::Kill, format!("prepare the leftover probe: {e}")))?;
        let mut cmd = prepared.command();
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(probe.deadline, cmd.output())
            .await
            .map_err(|_| fail(Step::Kill, "the leftover probe did not end"))?
            .map_err(|e| fail(Step::Kill, format!("the leftover probe: {e}")))?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect())
    }
}

/// SIGKILL the process group led by `pid` (the shell's `kill`: this crate forbids `unsafe`).
async fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let _ = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -s KILL -- -{pid} 2>/dev/null"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// First message, silence, a 64 KiB message, then **close stdin**: the process must end by itself
/// with status 0.
///
/// # Errors
///
/// The [`Step`] that failed, and what was seen. The process is killed when a step fails.
pub async fn framed_stdio_until_eof(
    session: &dyn EnvSession,
    probe: &StdioProbe,
) -> Result<(), StdioFailure> {
    let mut running = Running::start(session, probe)?;
    let result = match running.converse(probe).await {
        Ok(()) => running.eof(probe).await,
        Err(e) => Err(e),
    };
    if result.is_err() {
        running.kill(session, probe.deadline).await;
    }
    result
}

/// First message, silence, a 64 KiB message, then **kill** the process (its group and, in the
/// environment, its exec id): it must end, and nothing that carries its marker may be left.
///
/// # Errors
///
/// The [`Step`] that failed, and what was seen. The process is killed when a step fails.
pub async fn framed_stdio_until_kill(
    session: &dyn EnvSession,
    probe: &StdioProbe,
) -> Result<(), StdioFailure> {
    let mut running = Running::start(session, probe)?;
    let result = match running.converse(probe).await {
        Ok(()) => running.killed(session, probe).await,
        Err(e) => Err(e),
    };
    if result.is_err() {
        running.kill(session, probe.deadline).await;
    }
    result
}
