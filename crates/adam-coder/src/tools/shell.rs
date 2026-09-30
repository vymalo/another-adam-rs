//! Running a shell command for `run_checks` and `run_command`: cwd confined to the worktree, a
//! timeout that kills the whole process group, and an output cap that keeps the tail. The shell
//! is a login shell (`bash -lc`, or `sh -lc` where there is no bash), and a command the shell
//! cannot find is recognised ([`missing_tool`]) so that it is reported as a missing toolchain
//! and not as a failing check.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
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

/// The shell commands run in: `bash` when the image has one, else `sh`.
///
/// Models write bash (`${PIPESTATUS[0]}`, `[[ ]]`, arrays, `<(...)`), and where `sh` is dash
/// they fail with "Bad substitution" for reasons that have nothing to do with the project. Both
/// are run as login shells (`-l`), which is what keeps the toolchain's `PATH` from
/// `/etc/profile.d`, since Debian's `/etc/profile` resets it. Found once, on `PATH`.
pub fn login_shell() -> &'static str {
    static SHELL: OnceLock<&'static str> = OnceLock::new();
    SHELL.get_or_init(|| if on_path("bash") { "bash" } else { "sh" })
}

/// Whether an executable file called `name` is in a directory of `PATH`.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// A command the shell could not find, as a run's output says it: the workspace lacks a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingTool {
    /// The command word, when the output names it (`mvn`); `None` when the shell only exited 127.
    pub name: Option<String>,
}

/// Whether `outcome` is the shell saying it could not find a command.
///
/// Two shapes: `sh: 1: mvn: not found` (dash) and `bash: line 1: mvn: command not found` (also
/// `bash: mvn: command not found`), and the exit code 127 they come with. A line of that shape
/// counts when the run failed and it starts with a shell's name, or whatever it starts with when
/// the exit code is 127 (a script that calls a missing command names itself first). A run that
/// only exited 127 is a missing command too, with no name. So `cat: CLAUDE.md: No such file or
/// directory` (a missing file), a test that prints "resource not found" and exits 1, and a
/// timeout are not.
pub fn missing_tool(outcome: &ShellOutcome) -> Option<MissingTool> {
    let code = outcome.exit_code.filter(|c| *c != 0)?;
    if outcome.timed_out {
        return None;
    }
    let found = outcome.tail.lines().find_map(|line| {
        let line = line.trim();
        let before = line
            .strip_suffix(": command not found")
            .or_else(|| line.strip_suffix(": not found"))?;
        let word = before.rsplit(": ").next()?.trim();
        // The first thing such a line names is the shell that printed it (`sh`, `/bin/sh`).
        let shell = before.split(": ").next().unwrap_or_default();
        let shell_said_it = matches!(
            shell.rsplit('/').next(),
            Some("sh" | "bash" | "dash" | "zsh" | "ash")
        );
        let plausible =
            !word.is_empty() && word.len() <= 128 && !word.contains(char::is_whitespace);
        (plausible && (code == 127 || shell_said_it)).then(|| word.to_owned())
    });
    match (found, code) {
        (Some(name), _) => Some(MissingTool { name: Some(name) }),
        (None, 127) => Some(MissingTool { name: None }),
        _ => None,
    }
}

/// Run `<login shell> -lc <command>` in `dir` (see [`login_shell`]; agent tool `PATH`s are set
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
    let mut cmd = Command::new(login_shell());
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
        // The timeout leaves a login shell on a loaded machine time to start and
        // print (400 ms did not, once, in CI); the grandchild outlives it by far.
        let timeout = Duration::from_secs(2);
        let grandchild_delay = 4;
        // A background grandchild that would create a file if it survived.
        let script = format!(
            "(sleep {grandchild_delay}; touch {}) & echo started; sleep 60",
            marker.display()
        );
        let started = std::time::Instant::now();
        let out = run_shell(dir.path(), &script, timeout, 1024).await.unwrap();
        assert!(out.timed_out, "{out:?}");
        assert!(!out.passed());
        assert!(out.tail.contains("started"), "{:?}", out.tail);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned promptly, not after the sleeps"
        );
        // The grandchild started before the timeout, so a survivor would have
        // touched the marker by timeout + its delay; wait past that.
        let deadline = started + timeout + Duration::from_secs(grandchild_delay + 1);
        tokio::time::sleep_until(deadline.into()).await;
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

    #[tokio::test]
    async fn bash_constructs_work_when_there_is_a_bash() {
        if login_shell() != "bash" {
            eprintln!("skipping: no bash on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // The owner's case: `${PIPESTATUS[0]}` is "Bad substitution" in dash.
        let out = run_shell(
            dir.path(),
            "false | true; echo status=${PIPESTATUS[0]}; [[ a == a ]] && echo ok; arr=(x y); echo ${arr[1]}",
            LONG,
            1024,
        )
        .await
        .unwrap();
        assert!(out.passed(), "{out:?}");
        assert!(out.tail.contains("status=1"), "{:?}", out.tail);
        assert!(
            out.tail.contains("ok") && out.tail.contains('y'),
            "{:?}",
            out.tail
        );
    }

    #[test]
    fn the_shell_is_bash_where_there_is_one_and_sh_otherwise() {
        assert_eq!(login_shell() == "bash", on_path("bash"));
        assert!(on_path("sh"));
        assert!(!on_path("no-such-program-anywhere"));
    }

    fn failed(code: i32, tail: &str) -> ShellOutcome {
        ShellOutcome {
            exit_code: Some(code),
            timed_out: false,
            tail: tail.to_owned(),
            truncated: false,
        }
    }

    #[test]
    fn a_command_the_shell_cannot_find_is_a_missing_tool() {
        let named = |name: &str| {
            Some(MissingTool {
                name: Some(name.to_owned()),
            })
        };
        // What dash, bash as `bash -c`, and a script under bash print, each with exit 127.
        assert_eq!(
            missing_tool(&failed(127, "sh: 1: mvn: not found\n")),
            named("mvn")
        );
        assert_eq!(
            missing_tool(&failed(127, "bash: line 1: mvn: command not found\n")),
            named("mvn")
        );
        assert_eq!(
            missing_tool(&failed(127, "bash: mvn: command not found")),
            named("mvn")
        );
        assert_eq!(
            missing_tool(&failed(
                127,
                "ls: fine\n./check.sh: line 4: cargo: command not found\n"
            )),
            named("cargo"),
            "a script names itself first, the exit code is what says it"
        );
        assert_eq!(
            missing_tool(&failed(127, "/bin/sh: 1: ./gradlew: not found")),
            named("./gradlew")
        );
        // A shell line with another exit code (a pipeline, `|| exit 1`) still counts.
        assert_eq!(
            missing_tool(&failed(1, "sh: 1: mvn: not found")),
            named("mvn")
        );
        // Exit 127 and nothing it names.
        assert_eq!(
            missing_tool(&failed(127, "something went wrong")),
            Some(MissingTool { name: None })
        );
        // The first one is the one that is missing.
        assert_eq!(
            missing_tool(&failed(
                127,
                "sh: 1: mvn: not found\nsh: 2: gradle: not found"
            )),
            named("mvn")
        );
    }

    #[test]
    fn a_failure_that_is_not_a_missing_command_is_not_one() {
        // A missing file, a failing test that says "not found", a pass, a timeout, a signal.
        assert_eq!(
            missing_tool(&failed(1, "cat: CLAUDE.md: No such file or directory")),
            None
        );
        assert_eq!(
            missing_tool(&failed(1, "FAILED: user 7: resource not found")),
            None,
            "not a shell's line, and not exit 127"
        );
        assert_eq!(
            missing_tool(&failed(0, "sh: 1: mvn: not found")),
            None,
            "it passed"
        );
        let mut timed_out = failed(127, "sh: 1: mvn: not found");
        timed_out.timed_out = true;
        assert_eq!(missing_tool(&timed_out), None);
        let mut signalled = failed(1, "");
        signalled.exit_code = None;
        assert_eq!(missing_tool(&signalled), None);
        // A "word" that is a sentence is not a command.
        assert_eq!(
            missing_tool(&failed(1, "sh: 1: the thing you wanted: not found")),
            None
        );
    }

    #[tokio::test]
    async fn a_real_missing_command_is_recognised_in_the_shell_that_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_shell(dir.path(), "no-such-toolchain --version", LONG, 1024)
            .await
            .unwrap();
        assert_eq!(out.exit_code, Some(127), "{out:?}");
        assert_eq!(
            missing_tool(&out),
            Some(MissingTool {
                name: Some("no-such-toolchain".to_owned())
            }),
            "{:?}",
            out.tail
        );
        // A command that exists and fails is not.
        let out = run_shell(dir.path(), "ls /no/such/dir", LONG, 1024)
            .await
            .unwrap();
        assert!(!out.passed());
        assert_eq!(missing_tool(&out), None, "{:?}", out.tail);
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
