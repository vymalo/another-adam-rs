//! Running the devcontainer CLI and Podman's client: one environment, one timeout, one reader.
//!
//! Every process this crate starts gets an **empty environment plus an allow-list** (`PATH`, a
//! `HOME` of its own, `CONTAINER_HOST`, `LANG`, `TMPDIR`): the coder's `GITHUB_TOKEN`,
//! `DATABASE_URL` and `A2A_BEARER_TOKENS` are never in it, and `${localEnv:NAME}` in a
//! repository's file, which the CLI resolves from its own environment, finds nothing but these.
//! A process runs in a process group of its own, which is killed when the call times out or the
//! future is dropped (a cancelled run), because the CLI's children do not die with it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::error::clip;

/// What a process's standard output may hold before the rest is dropped.
const MAX_STDOUT: usize = 4 << 20;
/// What is kept of a process's standard error, for the build log and for errors.
pub(crate) const LOG_TAIL_BYTES: usize = 64 << 10;
/// Longest line of standard error that is passed on whole.
const MAX_LINE: usize = 16 << 10;

/// The environment a process of this crate starts with.
#[derive(Debug, Clone)]
pub(crate) struct CleanEnv {
    path: OsString,
    lang: OsString,
    tmpdir: OsString,
    home: PathBuf,
    container_host: String,
}

impl CleanEnv {
    /// The allow-list, from this process's own `PATH`, `LANG` and `TMPDIR`.
    pub(crate) fn new(home: PathBuf, container_host: String) -> Self {
        Self::with(
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
            std::env::var_os("LANG").unwrap_or_else(|| "C.UTF-8".into()),
            std::env::var_os("TMPDIR").unwrap_or_else(|| "/tmp".into()),
            home,
            container_host,
        )
    }

    fn with(
        path: OsString,
        lang: OsString,
        tmpdir: OsString,
        home: PathBuf,
        container_host: String,
    ) -> Self {
        Self {
            path,
            lang,
            tmpdir,
            home,
            container_host,
        }
    }

    /// `HOME` of the processes.
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    /// The variables, as a process gets them and as `PreparedCommand::env` says them.
    pub(crate) fn vars(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("PATH", self.path.clone()),
            ("HOME", self.home.clone().into_os_string()),
            ("CONTAINER_HOST", OsString::from(&self.container_host)),
            ("LANG", self.lang.clone()),
            ("TMPDIR", self.tmpdir.clone()),
        ]
    }

    /// A command that starts from nothing and gets the allow-list.
    pub(crate) fn command<I, S>(&self, program: &Path, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(program);
        cmd.args(args).env_clear().envs(self.vars());
        cmd
    }
}

/// The end of what a process wrote to standard error, as lines.
#[derive(Debug, Default, Clone)]
pub(crate) struct LogTail {
    text: String,
}

impl LogTail {
    /// Add a line; the oldest lines go when it is over [`LOG_TAIL_BYTES`].
    pub(crate) fn push(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        if self.text.len() > LOG_TAIL_BYTES {
            let mut cut = self.text.len() - LOG_TAIL_BYTES;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            // Start at a line.
            if let Some(nl) = self.text[cut..].find('\n') {
                cut += nl + 1;
            }
            self.text.drain(..cut);
        }
    }

    /// The raw lines.
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    /// The lines as text: a JSON log line (`{"type":"text","text":"..."}`) is its `text`.
    pub(crate) fn as_plain(&self) -> String {
        self.text
            .lines()
            .map(log_text)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The text of one line of the CLI's log: its `text` when it is a JSON event, else the line.
pub(crate) fn log_text(line: &str) -> String {
    if line.starts_with('{')
        && let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line)
    {
        return event
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                event
                    .get("stepDetail")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();
    }
    line.to_owned()
}

/// What phase of making the environment a line of the CLI's log says it is in, in a few words.
pub(crate) fn phase_of(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("Running the ")
        && let Some((hook, _)) = rest.split_once(' ')
    {
        return Some(format!("running {hook}"));
    }
    if lower.contains("pull") {
        return Some("pulling the image".to_owned());
    }
    if lower.contains("buildx build")
        || lower.contains(" build ")
        || lower.starts_with("start: run: ") && lower.contains("build")
    {
        return Some("building the image".to_owned());
    }
    if lower.contains("features") && (lower.contains("install") || lower.contains("build")) {
        return Some("installing features".to_owned());
    }
    if lower.contains(" run --sig-proxy") || lower.contains(" create ") || lower.contains(" start ")
    {
        return Some("starting the container".to_owned());
    }
    None
}

/// What a finished process left.
#[derive(Debug)]
pub(crate) struct Finished {
    /// The exit code; `None` when a signal ended it.
    pub code: Option<i32>,
    /// Standard output (at most 4 MiB).
    pub stdout: String,
    /// The end of standard error.
    pub log: LogTail,
}

impl Finished {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why a process could not be run to its end.
#[derive(Debug)]
pub(crate) enum RunError {
    /// It could not be started (not installed, not executable).
    Spawn(std::io::Error),
    /// It did not end in time and was killed with its process group.
    Timeout,
    /// Reading its output failed.
    Io(std::io::Error),
}

/// Kills a process group when dropped (a cancelled `ensure`) unless disarmed.
struct GroupGuard(Option<u32>);

impl GroupGuard {
    fn kill(&mut self) {
        if let Some(pid) = self.0.take() {
            // `kill -s KILL -- -<pid>` through sh: no signal API without a new dependency.
            let _ = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("kill -s KILL -- -{pid} 2>/dev/null"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Run `cmd` to its end, or kill it after `timeout`. Standard input is closed; each line of
/// standard error goes to `on_line` as it comes (and into the [`LogTail`]).
///
/// # Errors
///
/// [`RunError`].
pub(crate) async fn run(
    mut cmd: Command,
    timeout: Duration,
    mut on_line: impl FnMut(&str) + Send,
) -> Result<Finished, RunError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let mut guard = GroupGuard(child.id());
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(RunError::Io(std::io::Error::other("no pipes")));
    };

    let read_stdout = async move {
        let mut kept = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut stdout = stdout;
        loop {
            let n = stdout.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            // Keep reading after the cap so that the process never blocks on a full pipe.
            if kept.len() < MAX_STDOUT {
                kept.extend_from_slice(&chunk[..n.min(MAX_STDOUT - kept.len())]);
            }
        }
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&kept).into_owned())
    };
    let read_stderr = async {
        let mut tail = LogTail::default();
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line).await?;
            if n == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end_matches(['\n', '\r']);
            let text = if text.len() > MAX_LINE {
                clip(text, MAX_LINE)
            } else {
                text.to_owned()
            };
            on_line(&text);
            tail.push(&text);
        }
        Ok::<_, std::io::Error>(tail)
    };

    let work = async { tokio::join!(read_stdout, read_stderr, child.wait()) };
    match tokio::time::timeout(timeout, work).await {
        Ok((out, err, status)) => {
            guard.disarm();
            let status = status.map_err(RunError::Io)?;
            Ok(Finished {
                code: status.code(),
                stdout: out.map_err(RunError::Io)?,
                log: err.map_err(RunError::Io)?,
            })
        }
        Err(_) => {
            guard.kill();
            let _ = child.kill().await;
            Err(RunError::Timeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> CleanEnv {
        CleanEnv::with(
            "/usr/bin:/bin".into(),
            "C.UTF-8".into(),
            "/tmp".into(),
            PathBuf::from("/work/environments/.cli-home"),
            "unix:///run/podman/podman.sock".to_owned(),
        )
    }

    #[tokio::test]
    async fn a_process_gets_the_allow_list_and_nothing_else() {
        // A name of this process's own environment that must not reach the child.
        let leaked = std::env::vars()
            .map(|(k, _)| k)
            .find(|k| k != "PATH" && k != "HOME" && k != "LANG" && k != "TMPDIR");
        let cmd = env().command(Path::new("/usr/bin/env"), Vec::<&str>::new());
        let done = run(cmd, Duration::from_secs(10), |_| {}).await.unwrap();
        assert!(done.success());
        let mut names: Vec<&str> = done
            .stdout
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["CONTAINER_HOST", "HOME", "LANG", "PATH", "TMPDIR"]);
        assert!(done.stdout.contains("HOME=/work/environments/.cli-home"));
        assert!(
            done.stdout
                .contains("CONTAINER_HOST=unix:///run/podman/podman.sock")
        );
        if let Some(name) = leaked {
            assert!(!done.stdout.contains(&format!("{name}=")), "{name} leaked");
        }
    }

    #[tokio::test]
    async fn the_lines_of_standard_error_are_passed_on_as_they_come_and_kept() {
        let cmd = env().command(
            Path::new("/bin/sh"),
            ["-c", "echo out; echo one >&2; echo two >&2; exit 3"],
        );
        let mut seen = Vec::new();
        let done = run(cmd, Duration::from_secs(10), |l| seen.push(l.to_owned()))
            .await
            .unwrap();
        assert_eq!(done.code, Some(3));
        assert_eq!(done.stdout, "out\n");
        assert_eq!(seen, ["one", "two"]);
        assert_eq!(done.log.as_str(), "one\ntwo\n");
    }

    #[tokio::test]
    async fn a_process_that_is_too_slow_is_killed_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("grandchild-alive");
        let script = format!("(sleep 1; touch {}) & sleep 30", marker.display());
        let cmd = env().command(Path::new("/bin/sh"), ["-c", script.as_str()]);
        let started = std::time::Instant::now();
        let err = run(cmd, Duration::from_millis(200), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Timeout), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "the process group was killed, so the grandchild never got to write"
        );
    }

    #[tokio::test]
    async fn a_program_that_is_not_there_is_a_spawn_error() {
        let cmd = env().command(Path::new("/no/such/devcontainer"), ["up"]);
        let err = run(cmd, Duration::from_secs(5), |_| {}).await.unwrap_err();
        assert!(matches!(err, RunError::Spawn(e) if e.kind() == std::io::ErrorKind::NotFound));
    }

    #[tokio::test]
    async fn output_over_the_cap_is_dropped_without_blocking_the_process() {
        let cmd = env().command(
            Path::new("/bin/sh"),
            ["-c", "head -c 6000000 /dev/zero | tr '\\0' x"],
        );
        let done = run(cmd, Duration::from_secs(20), |_| {}).await.unwrap();
        assert!(done.success());
        assert_eq!(done.stdout.len(), MAX_STDOUT);
    }

    #[test]
    fn the_tail_keeps_the_end_in_whole_lines() {
        let mut tail = LogTail::default();
        let line = "x".repeat(1000);
        for n in 0..200 {
            tail.push(&format!("{n} {line}"));
        }
        assert!(tail.as_str().len() <= LOG_TAIL_BYTES);
        assert!(tail.as_str().ends_with(&format!("199 {line}\n")));
        assert!(
            tail.as_str().starts_with(|c: char| c.is_ascii_digit()),
            "starts at a line"
        );
    }

    #[test]
    fn a_json_log_event_is_its_text_and_anything_else_is_itself() {
        assert_eq!(
            log_text(
                r#"{"type":"text","level":2,"timestamp":1,"text":"Start: Run: docker build"}"#
            ),
            "Start: Run: docker build"
        );
        assert_eq!(
            log_text(r#"{"type":"progress","name":"x","status":"running","stepDetail":"step 3"}"#),
            "step 3"
        );
        assert_eq!(log_text("plain line"), "plain line");
        assert_eq!(log_text("{ not json"), "{ not json");
    }

    #[test]
    fn phases_are_read_from_what_the_cli_says() {
        assert_eq!(
            phase_of("\u{1b}[1mRunning the postCreateCommand from devcontainer.json...").as_deref(),
            None,
            "escapes are the caller's to remove"
        );
        assert_eq!(
            phase_of("Running the postCreateCommand from devcontainer.json...").as_deref(),
            Some("running postCreateCommand")
        );
        assert_eq!(
            phase_of("Start: Run: podman pull mcr.microsoft.com/devcontainers/base:2.2.1")
                .as_deref(),
            Some("pulling the image")
        );
        assert_eq!(
            phase_of("Start: Run: podman buildx build --load --build-arg x").as_deref(),
            Some("building the image")
        );
        assert_eq!(
            phase_of("Start: Run: podman run --sig-proxy=false -a STDOUT -a STDERR --mount x")
                .as_deref(),
            Some("starting the container")
        );
        assert_eq!(phase_of("something unrelated"), None);
    }
}
