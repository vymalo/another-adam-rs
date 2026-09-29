//! Running a shell command for `run_checks`: cwd confined to the worktree, a
//! timeout that kills the whole process group, and an output cap that keeps the
//! tail.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Secrets of this process that a project's checks must not see. A check runs
/// repository code (build scripts, tests), which is untrusted.
const HIDDEN_FROM_CHECKS: &[&str] = &[
    "GITHUB_TOKEN",
    "DATABASE_URL",
    "A2A_BEARER_TOKENS",
    "MODEL_API_KEY",
];

/// How long to wait for the output pipes after the process is gone. A
/// grandchild that outlives its parent may hold them open forever.
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// What a command did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    /// Exit code; `None` when killed by a signal (also on timeout).
    pub exit_code: Option<i32>,
    /// The time limit was hit and the process group was killed.
    pub timed_out: bool,
    /// The last bytes of stdout and stderr, interleaved as they arrived.
    pub tail: String,
    /// Older output was dropped to fit the cap.
    pub truncated: bool,
}

impl ShellOutcome {
    /// Exit code 0 and no timeout.
    pub fn passed(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Resolve `relative` (a sub-directory of the worktree, `None` = its root) to
/// an existing directory that is inside `root`.
///
/// Rejects absolute paths and `..` outright, and canonicalises the result so a
/// symlink inside the worktree cannot lead out of it either.
///
/// # Errors
///
/// A message for the model when the path escapes or is not a directory.
pub fn resolve_cwd(root: &Path, relative: Option<&str>) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("cannot resolve the worktree: {e}"))?;
    let Some(relative) = relative
        .map(str::trim)
        .filter(|r| !r.is_empty() && *r != ".")
    else {
        return Ok(root);
    };
    let rel = Path::new(relative);
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(format!(
            "cwd `{relative}` must be a relative path inside the worktree (no absolute paths, no `..`)"
        ));
    }
    let joined = root.join(rel);
    let resolved = joined
        .canonicalize()
        .map_err(|e| format!("cwd `{relative}` does not exist in the worktree: {e}"))?;
    if !resolved.starts_with(&root) {
        return Err(format!("cwd `{relative}` resolves outside the worktree"));
    }
    if !resolved.is_dir() {
        return Err(format!("cwd `{relative}` is not a directory"));
    }
    Ok(resolved)
}

/// The last `cap` bytes of everything pushed.
struct Tail {
    buf: Vec<u8>,
    cap: usize,
    total: u64,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        self.buf.extend_from_slice(chunk);
        // Compact lazily so a firehose does not copy on every chunk.
        if self.buf.len() > self.cap * 2 {
            let drop = self.buf.len() - self.cap;
            self.buf.drain(..drop);
        }
    }

    fn finish(&self) -> (String, bool) {
        let start = self.buf.len().saturating_sub(self.cap);
        let text = String::from_utf8_lossy(&self.buf[start..]).into_owned();
        (text, self.total > self.cap as u64)
    }
}

async fn pump(mut reader: impl AsyncRead + Unpin, tail: Arc<Mutex<Tail>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => tail
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(&chunk[..n]),
        }
    }
}

/// Run `sh -lc <command>` in `dir` (a login shell: agent tool `PATH`s are set
/// up in `/etc/profile.d`).
///
/// Stdin is closed. After `timeout` the whole process group is killed. The
/// returned tail holds at most `tail_cap` bytes.
///
/// # Errors
///
/// Only when the shell cannot be started.
#[tracing::instrument(skip(command), fields(dir = %dir.display()))]
pub async fn run_shell(
    dir: &Path,
    command: &str,
    timeout: Duration,
    tail_cap: usize,
) -> io::Result<ShellOutcome> {
    run_shell_with(dir, command, timeout, tail_cap, &[]).await
}

/// [`run_shell`] with extra environment, applied *before* the secrets are
/// hidden (so the hiding is testable without touching this process's
/// environment).
async fn run_shell_with(
    dir: &Path,
    command: &str,
    timeout: Duration,
    tail_cap: usize,
    extra_env: &[(&str, &str)],
) -> io::Result<ShellOutcome> {
    let mut cmd = Command::new("sh");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    for name in HIDDEN_FROM_CHECKS {
        cmd.env_remove(name);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();

    let tail = Arc::new(Mutex::new(Tail {
        buf: Vec::new(),
        cap: tail_cap.max(1),
        total: 0,
    }));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(pump(out, tail.clone())));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(pump(err, tail.clone())));
    }

    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (status?.code(), false),
        Err(_) => {
            kill_group(pid).await;
            let _ = child.kill().await;
            (None, true)
        }
    };

    let drained = tokio::time::timeout(PIPE_GRACE, async {
        for reader in &mut readers {
            let _ = reader.await;
        }
    })
    .await;
    if drained.is_err() {
        for reader in &readers {
            reader.abort();
        }
    }

    let (tail, truncated) = tail.lock().unwrap_or_else(PoisonError::into_inner).finish();
    Ok(ShellOutcome {
        exit_code,
        timed_out,
        tail,
        truncated,
    })
}

/// SIGKILL the process group led by `pid` (it was started with
/// `process_group(0)`, so its group id is its pid). Uses the shell's `kill`
/// builtin: this crate forbids `unsafe`, and `kill(1)` may be absent from slim
/// images.
async fn kill_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let killed = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -s KILL -- -{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if !matches!(killed, Ok(s) if s.success()) {
        tracing::warn!(
            pid,
            "could not kill the process group; killing the shell only"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn captures_exit_code_and_both_streams() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_shell(dir.path(), "echo out; echo err >&2; exit 3", LONG, 1024)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.passed() && !out.timed_out && !out.truncated);
        assert!(
            out.tail.contains("out") && out.tail.contains("err"),
            "{:?}",
            out.tail
        );

        let ok = run_shell(dir.path(), "true", LONG, 1024).await.unwrap();
        assert!(ok.passed());
    }

    #[tokio::test]
    async fn runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let out = run_shell(dir.path(), "ls", LONG, 1024).await.unwrap();
        assert!(out.tail.contains("marker.txt"), "{:?}", out.tail);
    }

    #[tokio::test]
    async fn keeps_only_the_tail_of_large_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_shell(
            dir.path(),
            "i=0; while [ $i -lt 2000 ]; do echo line-$i; i=$((i+1)); done",
            LONG,
            200,
        )
        .await
        .unwrap();
        assert!(out.truncated);
        assert!(out.tail.len() <= 200, "{}", out.tail.len());
        assert!(out.tail.contains("line-1999"), "{:?}", out.tail);
        assert!(!out.tail.contains("line-0\n"), "{:?}", out.tail);
    }

    #[tokio::test]
    async fn a_timeout_kills_the_process_group_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survivor");
        // A background grandchild that would create a file if it survived.
        let script = format!(
            "(sleep 3; touch {}) & echo started; sleep 60",
            marker.display()
        );
        let started = std::time::Instant::now();
        let out = run_shell(dir.path(), &script, Duration::from_millis(400), 1024)
            .await
            .unwrap();
        assert!(out.timed_out, "{out:?}");
        assert!(!out.passed());
        assert!(out.tail.contains("started"), "{:?}", out.tail);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned promptly, not after the sleeps"
        );
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "the grandchild was killed with the group");
    }

    #[tokio::test]
    async fn secrets_of_this_process_are_not_visible_to_checks() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_shell_with(
            dir.path(),
            "env | grep -c -E '^(GITHUB_TOKEN|DATABASE_URL|A2A_BEARER_TOKENS|MODEL_API_KEY)=' || true; echo kept=$KEPT",
            LONG,
            1024,
            &[
                ("GITHUB_TOKEN", "a"),
                ("DATABASE_URL", "b"),
                ("A2A_BEARER_TOKENS", "c"),
                ("MODEL_API_KEY", "d"),
                ("KEPT", "yes"),
            ],
        )
        .await
        .unwrap();
        // (a login shell may print profile noise before the count)
        assert!(out.tail.lines().any(|l| l.trim() == "0"), "{:?}", out.tail);
        assert!(
            out.tail.contains("kept=yes"),
            "other variables still pass: {:?}",
            out.tail
        );
    }

    #[test]
    fn cwd_escapes_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub/dir")).unwrap();
        std::fs::write(root.path().join("file.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();

        let canonical = root.path().canonicalize().unwrap();
        assert_eq!(resolve_cwd(root.path(), None).unwrap(), canonical);
        assert_eq!(resolve_cwd(root.path(), Some(".")).unwrap(), canonical);
        assert_eq!(
            resolve_cwd(root.path(), Some("sub/dir")).unwrap(),
            canonical.join("sub/dir")
        );

        for bad in ["..", "../x", "sub/../..", "/etc", "file.txt", "missing"] {
            assert!(
                resolve_cwd(root.path(), Some(bad)).is_err(),
                "{bad} must be rejected"
            );
        }
        #[cfg(unix)]
        assert!(
            resolve_cwd(root.path(), Some("link")).is_err(),
            "symlink escape"
        );
    }
}
