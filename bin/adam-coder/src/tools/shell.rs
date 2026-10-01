//! Running a shell command for `run_checks` and `run_command`: cwd confined to the worktree, a
//! timeout that kills the whole process group, and an output cap that keeps the tail. The command is
//! prepared by the run's environment ([`EnvSession`]: the coder's own container unless a deployment
//! says otherwise), and the shell is a login shell (`bash -lc`, or `sh -lc` where there is no bash).
//! A command the shell cannot find is recognised ([`missing_tool`]) so that it is reported as a
//! missing toolchain and not as a failing check.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use adam_workspace::{EnvError, EnvSession, ExecSpec};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

pub use adam_workspace::login_shell;

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

/// A command the shell could not find, as a run's output says it: the workspace lacks a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingTool {
    /// The command word the shell could not find (`mvn`).
    pub name: String,
}

/// Whether `outcome` is the shell saying it could not find a command: the run **exited 127** and
/// says so in the way a shell does.
///
/// The exit code is 127 *and* a line names the command: `sh: 1: mvn: not found` (dash), `bash: line
/// 1: mvn: command not found` (also `bash: mvn: command not found`), printed by the shell that ran
/// the command, or by a script the command itself runs (`./check.sh: line 4: cargo: command not
/// found`: the line starts with a word that the command contains). Neither alone is enough. A
/// nested `sh: 1: gti: not found` in the output of a test run that exits 101 is the project's
/// business, not the workspace's, and a bare 127 (a `exit 127` of a script) names nothing the
/// person could install. A missing file (`cat: CLAUDE.md: No such file or directory`), a test
/// that prints "resource not found" and a timeout are not it either.
pub fn missing_tool(outcome: &ShellOutcome, command: &str) -> Option<MissingTool> {
    if outcome.timed_out || outcome.exit_code != Some(127) {
        return None;
    }
    outcome.tail.lines().find_map(|line| {
        let line = line.trim();
        let before = line
            .strip_suffix(": command not found")
            .or_else(|| line.strip_suffix(": not found"))?;
        // `<who>: [line ]<n>: <word>` or `<who>: <word>`.
        let mut parts = before.split(": ");
        let who = parts.next()?;
        let rest: Vec<&str> = parts.collect();
        let (position, word) = match rest.as_slice() {
            [word] => (None, *word),
            [position, word] => (Some(*position), *word),
            _ => return None,
        };
        if let Some(position) = position {
            let digits = position.strip_prefix("line ").unwrap_or(position);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
        }
        // Said by a shell (`sh`, `/bin/sh`, `bash`), or by a script the command runs.
        let shell = matches!(
            who.rsplit('/').next(),
            Some("sh" | "bash" | "dash" | "zsh" | "ash")
        );
        let own_script = !who.is_empty() && command_words(command).any(|word| word == who);
        let plausible =
            !word.is_empty() && word.len() <= 128 && !word.contains(char::is_whitespace);
        (plausible && (shell || own_script)).then(|| MissingTool {
            name: word.to_owned(),
        })
    })
}

/// The words of a shell command line, split at whitespace and at the characters that separate
/// commands and quote: `sh ./check.sh && echo 'x'` has `sh`, `./check.sh`, `echo` and `x`.
fn command_words(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|c: char| c.is_whitespace() || ";&|()<>`'\"".contains(c))
        .filter(|word| !word.is_empty())
}

/// What the workspace is missing, if the project brings it itself: the tool `name` is one the
/// project's own dependencies provide (`jest` under `node_modules/.bin`, `pytest` in a virtual
/// environment), which is not a system toolchain the workspace lacks but a dependency nobody
/// installed yet. The project's own install command is the way on. Looks in `dirs` (the directory
/// the command ran in, then the worktree root) for the files that say so.
pub fn project_dependency_hint(dirs: &[&Path], name: &str) -> Option<String> {
    const NODE_TOOLS: &[&str] = &[
        "jest",
        "vitest",
        "tsc",
        "eslint",
        "prettier",
        "webpack",
        "vite",
        "mocha",
        "ts-node",
        "next",
        "nx",
        "turbo",
        "playwright",
        "cypress",
        "rollup",
        "esbuild",
        "babel",
        "tsx",
        "biome",
        "karma",
        "ng",
    ];
    const PYTHON_TOOLS: &[&str] = &[
        "pytest", "tox", "flake8", "black", "mypy", "ruff", "nox", "isort",
    ];
    for dir in dirs {
        if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
            let declared = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .is_some_and(|json| {
                    ["dependencies", "devDependencies"]
                        .iter()
                        .any(|key| json.get(key).and_then(|d| d.get(name)).is_some())
                });
            if declared || NODE_TOOLS.contains(&name) {
                let install = if dir.join("pnpm-lock.yaml").exists() {
                    "pnpm install"
                } else if dir.join("yarn.lock").exists() {
                    "yarn install"
                } else if dir.join("package-lock.json").exists() {
                    "npm ci"
                } else {
                    "npm install"
                };
                return Some(install.to_owned());
            }
        }
        let python = ["pyproject.toml", "requirements.txt", "setup.py", "tox.ini"]
            .iter()
            .any(|f| dir.join(f).exists());
        if python && PYTHON_TOOLS.contains(&name) {
            let install = if dir.join("poetry.lock").exists() {
                "poetry install"
            } else if dir.join("requirements.txt").exists() {
                "pip install -r requirements.txt (in a virtual environment)"
            } else {
                "pip install -e . (in a virtual environment)"
            };
            return Some(install.to_owned());
        }
    }
    None
}

/// The spec of a command of the project's (a check, a look around): the shell command `command`
/// in `dir`, without the secrets of this process (`GITHUB_TOKEN`, `DATABASE_URL`, `A2A_BEARER_TOKENS`
/// and `MODEL_API_KEY`: a check runs repository code, which is untrusted).
pub fn shell_spec(dir: &Path, command: &str) -> ExecSpec {
    ExecSpec::shell(command, dir).hide(HIDDEN_FROM_CHECKS.iter().copied())
}

/// Why a command did not run.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The environment would not prepare it.
    #[error("the environment would not prepare the command")]
    Prepare(#[source] EnvError),
    /// The process could not be started.
    #[error("cannot start the process")]
    Spawn(#[source] io::Error),
}

/// Run `spec` in the environment of `session` (a login shell for a [`Program::Shell`](adam_workspace::Program),
/// see [`login_shell`]; agent tool `PATH`s are set up in `/etc/profile.d`).
///
/// The session prepares the command, and it is spawned here, in a process group of its own, with
/// stdin closed and `kill_on_drop`. After `timeout` the whole process group is killed **and**
/// [`EnvSession::kill`] is told, which is for what runs where this process cannot reach. The
/// returned tail holds at most `tail_cap` bytes.
///
/// # Errors
///
/// [`RunError::Prepare`] when the environment refuses the spec, [`RunError::Spawn`] when the process
/// cannot be started.
#[tracing::instrument(skip(session, spec), fields(dir = %spec.cwd.display()))]
pub async fn run_in(
    session: &dyn EnvSession,
    spec: ExecSpec,
    timeout: Duration,
    tail_cap: usize,
) -> Result<ShellOutcome, RunError> {
    let prepared = session.prepare(&spec).map_err(RunError::Prepare)?;
    let mut cmd = prepared.command();
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
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
        Ok(status) => (status.map_err(RunError::Spawn)?.code(), false),
        Err(_) => {
            kill_group(pid).await;
            let _ = child.kill().await;
            session.kill(&prepared.exec).await;
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
    use adam_workspace::{
        EnvDescription, EnvKind, ExecId, LocalSession, PreparedCommand, SecretRef,
    };
    use async_trait::async_trait;

    use super::*;

    const LONG: Duration = Duration::from_secs(30);

    /// A command of the project's in `dir`, in this container.
    async fn run_shell(
        dir: &Path,
        command: &str,
        timeout: Duration,
        tail_cap: usize,
    ) -> Result<ShellOutcome, RunError> {
        run_in(&LocalSession, shell_spec(dir, command), timeout, tail_cap).await
    }

    /// An environment that is this container and keeps a record of what it was asked.
    #[derive(Default)]
    struct Recording {
        prepared: Mutex<Vec<ExecId>>,
        killed: Mutex<Vec<ExecId>>,
        refuse: bool,
    }

    #[async_trait]
    impl EnvSession for Recording {
        fn describe(&self) -> EnvDescription {
            EnvDescription {
                kind: EnvKind::Local,
                summary: "a recording environment".to_owned(),
            }
        }

        fn prepare(&self, spec: &ExecSpec) -> Result<PreparedCommand, EnvError> {
            if self.refuse {
                return Err(EnvError::Refused("not here".to_owned()));
            }
            let prepared = LocalSession.prepare(spec)?;
            self.prepared.lock().unwrap().push(prepared.exec.clone());
            Ok(prepared)
        }

        async fn kill(&self, exec: &ExecId) {
            self.killed.lock().unwrap().push(exec.clone());
        }

        fn secret_ref(&self, _name: &str) -> Option<SecretRef> {
            None
        }
    }

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
        // The spec sets the names itself, so the hiding is testable without touching this
        // process's environment: what a spec hides is hidden even when the spec sets it.
        let spec = shell_spec(
            dir.path(),
            "env | grep -c -E '^(GITHUB_TOKEN|DATABASE_URL|A2A_BEARER_TOKENS|MODEL_API_KEY)=' || true; echo kept=$KEPT",
        )
        .env("GITHUB_TOKEN", "a")
        .env("DATABASE_URL", "b")
        .env("A2A_BEARER_TOKENS", "c")
        .env("MODEL_API_KEY", "d")
        .env("KEPT", "yes");
        let out = run_in(&LocalSession, spec, LONG, 1024).await.unwrap();
        // (a login shell may print profile noise before the count)
        assert!(out.tail.lines().any(|l| l.trim() == "0"), "{:?}", out.tail);
        assert!(
            out.tail.contains("kept=yes"),
            "other variables still pass: {:?}",
            out.tail
        );
    }

    #[tokio::test]
    async fn the_command_that_runs_is_the_one_the_session_prepared() {
        let dir = tempfile::tempdir().unwrap();
        let session = Recording::default();
        let out = run_in(&session, shell_spec(dir.path(), "echo hi"), LONG, 1024)
            .await
            .unwrap();
        assert!(out.passed() && out.tail.contains("hi"), "{out:?}");
        assert_eq!(session.prepared.lock().unwrap().len(), 1);
        assert!(
            session.killed.lock().unwrap().is_empty(),
            "a command that ends on its own is not killed"
        );
    }

    #[tokio::test]
    async fn a_timeout_kills_the_group_here_and_tells_the_session_which_command() {
        let dir = tempfile::tempdir().unwrap();
        let session = Recording::default();
        let out = run_in(
            &session,
            shell_spec(dir.path(), "sleep 60"),
            Duration::from_secs(1),
            1024,
        )
        .await
        .unwrap();
        assert!(out.timed_out, "{out:?}");
        let prepared = session.prepared.lock().unwrap().clone();
        let killed = session.killed.lock().unwrap().clone();
        assert_eq!(prepared.len(), 1);
        assert_eq!(
            killed, prepared,
            "the session is told which command timed out"
        );
    }

    #[tokio::test]
    async fn a_spec_the_environment_refuses_is_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let session = Recording {
            refuse: true,
            ..Recording::default()
        };
        let err = run_in(
            &session,
            shell_spec(dir.path(), &format!("touch {}", marker.display())),
            LONG,
            1024,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RunError::Prepare(EnvError::Refused(_))),
            "{err:?}"
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn a_program_that_cannot_start_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ExecSpec::argv(["/nonexistent/program"], dir.path());
        let err = run_in(&LocalSession, spec, LONG, 1024).await.unwrap_err();
        assert!(matches!(err, RunError::Spawn(_)), "{err:?}");
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

    fn failed(code: i32, tail: &str) -> ShellOutcome {
        ShellOutcome {
            exit_code: Some(code),
            timed_out: false,
            tail: tail.to_owned(),
            truncated: false,
        }
    }

    fn named(name: &str) -> Option<MissingTool> {
        Some(MissingTool {
            name: name.to_owned(),
        })
    }

    #[test]
    fn a_command_the_shell_cannot_find_is_a_missing_tool() {
        // What dash, bash as `bash -c`, and a script under bash print, each with exit 127.
        let m = |tail: &str, command: &str| missing_tool(&failed(127, tail), command);
        assert_eq!(m("sh: 1: mvn: not found\n", "mvn package"), named("mvn"));
        assert_eq!(
            m("bash: line 1: mvn: command not found\n", "mvn package"),
            named("mvn")
        );
        assert_eq!(
            m("bash: mvn: command not found", "mvn package"),
            named("mvn")
        );
        assert_eq!(
            m("/bin/sh: 1: ./gradlew: not found", "./gradlew build"),
            named("./gradlew")
        );
        // After output of its own.
        assert_eq!(
            m(
                "building\nsh: 1: mvn: not found\n",
                "echo building; mvn verify"
            ),
            named("mvn")
        );
        // A script the command runs names itself first.
        assert_eq!(
            m(
                "ls: fine\n./check.sh: line 4: cargo: command not found\n",
                "sh ./check.sh"
            ),
            named("cargo")
        );
        assert_eq!(
            m("check.sh: 3: mvn: not found", "sh check.sh"),
            named("mvn")
        );
        // The first one is the one that is missing.
        assert_eq!(
            m(
                "sh: 1: mvn: not found\nsh: 2: gradle: not found",
                "mvn; gradle"
            ),
            named("mvn")
        );
    }

    #[test]
    fn a_failure_that_is_not_a_missing_command_is_not_one() {
        let m = |code: i32, tail: &str, command: &str| missing_tool(&failed(code, tail), command);
        // A nested shell's "not found" in the output of a run that failed on its own terms (a test
        // suite, cargo's 101) is the project's business: only exit 127 says the command itself
        // could not be found.
        assert_eq!(m(101, "sh: 1: gti: not found", "cargo test"), None);
        assert_eq!(m(1, "sh: 1: mvn: not found", "mvn package || exit 1"), None);
        assert_eq!(
            m(2, "bash: line 3: jq: command not found", "make test"),
            None
        );
        // A bare 127 names nothing.
        assert_eq!(m(127, "something went wrong", "./run.sh"), None);
        // A line in another shape, from something else that happens to print "not found".
        assert_eq!(m(127, "FAILED: user 7: resource not found", "x"), None);
        assert_eq!(
            m(
                127,
                "cat: CLAUDE.md: No such file or directory",
                "cat CLAUDE.md"
            ),
            None
        );
        assert_eq!(m(127, "sh: 1: the thing you wanted: not found", "x"), None);
        // A name that is only part of a word of the command is not the command's script.
        assert_eq!(m(127, "check: 1: mvn: not found", "sh ./check.sh"), None);
        assert_eq!(
            m(
                127,
                "check.sh: 1: mvn: not found",
                "echo check.sh-is-fine; exit 127"
            ),
            None
        );
        // A line from a tool the command does not run (it could be anything in the output).
        assert_eq!(
            m(127, "other: 1: mvn: not found", "echo hi; exit 127"),
            None
        );
        // A pass, a timeout, a signal.
        assert_eq!(m(0, "sh: 1: mvn: not found", "true"), None);
        let mut timed_out = failed(127, "sh: 1: mvn: not found");
        timed_out.timed_out = true;
        assert_eq!(missing_tool(&timed_out, "mvn"), None);
        let mut signalled = failed(1, "");
        signalled.exit_code = None;
        assert_eq!(missing_tool(&signalled, "x"), None);
    }

    #[tokio::test]
    async fn a_real_missing_command_is_recognised_in_the_shell_that_runs_it() {
        let dir = tempfile::tempdir().unwrap();
        let command = "no-such-toolchain --version";
        let out = run_shell(dir.path(), command, LONG, 1024).await.unwrap();
        assert_eq!(out.exit_code, Some(127), "{out:?}");
        assert_eq!(
            missing_tool(&out, command),
            named("no-such-toolchain"),
            "{:?}",
            out.tail
        );
        // A command that exists and fails is not.
        let command = "ls /no/such/dir";
        let out = run_shell(dir.path(), command, LONG, 1024).await.unwrap();
        assert!(!out.passed());
        assert_eq!(missing_tool(&out, command), None, "{:?}", out.tail);
        // The nested case for real: a script that fails on its own after a nested shell said it.
        let command = "sh -c 'no-such-inner-tool' ; exit 101";
        let out = run_shell(dir.path(), command, LONG, 1024).await.unwrap();
        assert_eq!(out.exit_code, Some(101));
        assert_eq!(missing_tool(&out, command), None, "{:?}", out.tail);
    }

    #[test]
    fn a_tool_the_project_brings_is_a_dependency_to_install_not_a_toolchain_to_wait_for() {
        let dir = tempfile::tempdir().unwrap();
        let here = [dir.path()];
        // Nothing says the project has it.
        assert_eq!(project_dependency_hint(&here, "jest"), None);
        assert_eq!(project_dependency_hint(&here, "mvn"), None);
        // A package.json: a dev dependency (any name), or a well-known node tool.
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"devDependencies": {"my-lint": "1"}, "dependencies": {"left-pad": "1"}}"#,
        )
        .unwrap();
        assert_eq!(
            project_dependency_hint(&here, "jest").as_deref(),
            Some("npm install")
        );
        assert_eq!(
            project_dependency_hint(&here, "my-lint").as_deref(),
            Some("npm install")
        );
        assert_eq!(
            project_dependency_hint(&here, "left-pad").as_deref(),
            Some("npm install")
        );
        assert_eq!(
            project_dependency_hint(&here, "mvn"),
            None,
            "a system toolchain"
        );
        // The lock file picks the project's own installer.
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(
            project_dependency_hint(&here, "vitest").as_deref(),
            Some("pnpm install")
        );
        std::fs::remove_file(dir.path().join("pnpm-lock.yaml")).unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        assert_eq!(
            project_dependency_hint(&here, "tsc").as_deref(),
            Some("npm ci")
        );
        // Python, only with a file that says it is a Python project.
        let py = tempfile::tempdir().unwrap();
        assert_eq!(project_dependency_hint(&[py.path()], "pytest"), None);
        std::fs::write(py.path().join("requirements.txt"), "pytest\n").unwrap();
        assert!(
            project_dependency_hint(&[py.path()], "pytest")
                .unwrap()
                .contains("pip install -r requirements.txt")
        );
        // The directory the command ran in, then the worktree root.
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(
            project_dependency_hint(&[sub.as_path(), dir.path()], "eslint").as_deref(),
            Some("npm ci")
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
