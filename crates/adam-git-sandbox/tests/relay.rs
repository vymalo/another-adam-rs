//! The two binaries as processes: the relay in place of git, the server's exit codes and its log.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use common::{alive, git, pid_in, repository, script, wait_until};

const RELAY: &str = env!("CARGO_BIN_EXE_adam-git");
const SERVER: &str = env!("CARGO_BIN_EXE_adam-git-sandbox");

/// The server as a process, its log in a file, and a work volume.
struct Sandbox {
    tmp: tempfile::TempDir,
    root: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    server: Child,
}

impl Sandbox {
    fn start() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        let socket = tmp.path().join("git.sock");
        let log = tmp.path().join("server.log");
        let server = Command::new(SERVER)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_SANDBOX_LISTEN", &socket)
            .env("GIT_SANDBOX_ROOT", &root)
            // The server's own environment: what a filter could read of it.
            .env("SANDBOX_SIDE", "the-sandboxs-own-variable")
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(10), || socket.exists()),
            "the server made no socket"
        );
        Self {
            tmp,
            root,
            socket,
            log,
            server,
        }
    }

    /// The relay, run in `cwd`, with a hermetic git environment.
    fn relay(&self, cwd: &Path) -> Command {
        let mut command = Command::new(RELAY);
        command
            .current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_SANDBOX_SOCKET", &self.socket)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        command
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn stop(&mut self) -> std::process::ExitStatus {
        let _ = Command::new("kill")
            .args(["-TERM", &self.server.id().to_string()])
            .status();
        self.server.wait().unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// `git <args>` run directly in `cwd` with the same hermetic environment.
fn direct(cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    run(
        Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C"),
        stdin,
    )
}

fn run(command: &mut Command, stdin: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let bytes = stdin.to_vec();
    // From a thread of its own: a command that answers before it has read everything must not
    // leave the test blocked on the write.
    let feeder = std::thread::spawn(move || {
        let _ = input.write_all(&bytes);
    });
    let output = child.wait_with_output().unwrap();
    feeder.join().unwrap();
    output
}

fn through(sandbox: &Sandbox, cwd: &Path, args: &[&str], stdin: &[u8]) -> Output {
    run(sandbox.relay(cwd).args(args), stdin)
}

#[test]
fn the_relay_gives_the_stdout_stderr_and_status_git_gives() {
    let sandbox = Sandbox::start();
    let repo = repository(&sandbox.root, "repo");
    std::fs::write(repo.join("new.txt"), "untracked\n").unwrap();
    for args in [
        vec!["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        vec!["log", "--format=%H %s"],
        vec!["rev-parse", "HEAD"],
        // A failure: the status and the message are git's.
        vec!["rev-parse", "--verify", "no-such-ref"],
        vec!["checkout", "no-such-branch"],
        // An argument with spaces, quotes and a newline crosses as it is.
        vec![
            "-c",
            "alias.say=!printf '%s|' \"$@\"",
            "say",
            "a b",
            "c\"d",
            "e\nf",
            "",
        ],
    ] {
        let (via, own) = (
            through(&sandbox, &repo, &args, b""),
            direct(&repo, &args, b""),
        );
        assert_eq!(via.status.code(), own.status.code(), "{args:?}");
        assert_eq!(via.stdout, own.stdout, "{args:?}");
        assert_eq!(via.stderr, own.stderr, "{args:?}");
    }
    assert_eq!(
        through(
            &sandbox,
            &repo,
            &["rev-parse", "--verify", "no-such-ref"],
            b""
        )
        .status
        .code(),
        Some(128)
    );
}

#[test]
fn a_large_output_and_a_large_input_are_the_same_through_the_relay() {
    let sandbox = Sandbox::start();
    let repo = repository(&sandbox.root, "repo");
    let blob: Vec<u8> = (0..5_000_000u32)
        .map(|i| (i.wrapping_mul(2_246_822_519) >> 11) as u8)
        .collect();
    std::fs::write(repo.join("big.bin"), &blob).unwrap();
    git(&repo, &["add", "big.bin"]);
    git(&repo, &["commit", "--quiet", "-m", "big"]);

    let via = through(&sandbox, &repo, &["cat-file", "blob", "HEAD:big.bin"], b"");
    assert!(via.status.success());
    assert!(via.stdout == blob, "the output differs");

    // The input, and a patch on the input: what `git apply` of the coder does.
    let via = through(&sandbox, &repo, &["hash-object", "--stdin"], &blob);
    let own = direct(&repo, &["hash-object", "--stdin"], &blob);
    assert_eq!(via.stdout, own.stdout);

    std::fs::write(repo.join("README.md"), "hello\nmore\n").unwrap();
    let patch = direct(&repo, &["diff"], b"").stdout;
    git(&repo, &["checkout", "--", "README.md"]);
    let applied = through(&sandbox, &repo, &["apply", "-"], &patch);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).unwrap(),
        "hello\nmore\n"
    );
}

#[test]
fn without_a_socket_the_relay_says_so_and_runs_no_git() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("git-ran");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A git on the relay's own PATH: the relay must never start it.
    script(&bin.join("git"), &format!("touch {}", marker.display()));
    let cases: [(&str, Option<PathBuf>); 3] = [
        ("not set", None),
        ("nobody there", Some(tmp.path().join("nobody.sock"))),
        ("relative", Some(PathBuf::from("git.sock"))),
    ];
    for (what, socket) in cases {
        let mut relay = Command::new(RELAY);
        relay.current_dir(tmp.path()).env_clear().env("PATH", &bin);
        if let Some(socket) = &socket {
            relay.env("GIT_SANDBOX_SOCKET", socket);
        }
        let out = run(relay.arg("status"), b"");
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let want = match what {
            "nobody there" => 69,
            _ => 78,
        };
        assert_eq!(out.status.code(), Some(want), "{what}: {stderr}");
        assert!(stderr.starts_with("adam-git: "), "{what}: {stderr}");
        if want == 78 {
            assert!(stderr.contains("invalid configuration"), "{what}: {stderr}");
            assert!(stderr.contains("GIT_SANDBOX_SOCKET"), "{what}: {stderr}");
        } else {
            assert!(
                stderr.contains("cannot reach the git sandbox"),
                "{what}: {stderr}"
            );
        }
        assert!(out.stdout.is_empty(), "{what}");
        assert!(!marker.exists(), "{what}: the relay ran git itself");
    }
}

#[test]
fn a_directory_outside_the_root_is_refused_with_its_code_and_nothing_is_made() {
    let sandbox = Sandbox::start();
    let outside = sandbox.tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let out = through(&sandbox, &outside, &["init", "made-here"], b"");
    assert_eq!(
        out.status.code(),
        Some(77),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("refused"));
    assert!(!outside.join("made-here").exists());
    // Inside the root it works.
    let out = through(
        &sandbox,
        &sandbox.root,
        &["init", "--quiet", "made-here"],
        b"",
    );
    assert!(out.status.success());
    assert!(sandbox.root.join("made-here/.git").is_dir());
}

#[test]
fn a_relay_that_is_killed_takes_the_commands_group_with_it() {
    let sandbox = Sandbox::start();
    let (leader, child) = (
        sandbox.tmp.path().join("leader.pid"),
        sandbox.tmp.path().join("child.pid"),
    );
    // An alias that starts a shell and a sleeper, and waits.
    let alias = format!(
        "alias.hang=!echo $$ > {}; sleep 600 & echo $! > {}; wait",
        leader.display(),
        child.display()
    );
    let mut relay = sandbox
        .relay(&sandbox.root)
        .args(["-c", &alias, "hang"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (leader, child) = (pid_in(&leader), pid_in(&child));
    assert!(alive(leader) && alive(child));
    relay.kill().unwrap();
    relay.wait().unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || !alive(leader) && !alive(child)),
        "the command outlived its relay (shell alive: {}, sleeper alive: {})",
        alive(leader),
        alive(child)
    );
}

#[test]
fn the_server_logs_the_subcommand_and_nothing_of_the_command_line_or_its_output() {
    let mut sandbox = Sandbox::start();
    let repo = repository(&sandbox.root, "repo");
    std::fs::write(repo.join("secret.txt"), "TOP-SECRET-FILE-CONTENT\n").unwrap();
    git(&repo, &["add", "secret.txt"]);
    git(&repo, &["commit", "--quiet", "-m", "secret"]);

    let mut relay = sandbox.relay(&repo);
    relay
        .env("MODEL_API_KEY", "sk-secret-in-the-relays-environment")
        .env("GIT_AUTHOR_NAME", "Author-Name-Value")
        .args([
            "-c",
            "user.name=CONFIG-VALUE-SECRET",
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "THE-MESSAGE-SECRET",
        ])
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "C")
        .env("GIT_COMMITTER_EMAIL", "c@example.com");
    assert!(run(&mut relay, b"").status.success());
    // Output and a path argument that must stay out of the log.
    let shown = through(&sandbox, &repo, &["show", "HEAD~1:secret.txt"], b"");
    assert_eq!(shown.stdout, b"TOP-SECRET-FILE-CONTENT\n");
    let failed = through(
        &sandbox,
        &repo,
        &["checkout", "--", "PATH-ARGUMENT-SECRET"],
        b"",
    );
    assert!(!failed.status.success());
    let outside = sandbox.tmp.path();
    let refused = through(&sandbox, outside, &["status", "ANOTHER-ARGUMENT"], b"");
    assert_eq!(refused.status.code(), Some(77));

    assert_eq!(sandbox.stop().code(), Some(0), "a clean shutdown");
    let log = sandbox.log();
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect();
    let commands: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["fields"]["message"] == "git command")
        .collect();
    let subcommands: Vec<&str> = commands
        .iter()
        .map(|l| l["fields"]["subcommand"].as_str().unwrap())
        .collect();
    assert_eq!(
        subcommands,
        ["commit", "show", "checkout", "status"],
        "{log}"
    );
    for line in &commands {
        let fields = &line["fields"];
        assert!(
            fields["cwd"].is_string()
                && fields["status"].is_string()
                && fields["duration_ms"].is_u64(),
            "{line}"
        );
    }
    assert_eq!(commands[0]["fields"]["cwd"], repo.to_str().unwrap());
    assert_eq!(commands[0]["fields"]["status"], "exit 0");
    assert_eq!(commands[2]["fields"]["status"], "exit 1");
    assert!(
        commands[3]["fields"]["status"]
            .as_str()
            .unwrap()
            .starts_with("refused"),
        "{}",
        commands[3]
    );
    for secret in [
        "CONFIG-VALUE-SECRET",
        "THE-MESSAGE-SECRET",
        "TOP-SECRET-FILE-CONTENT",
        "PATH-ARGUMENT-SECRET",
        "ANOTHER-ARGUMENT",
        "sk-secret-in-the-relays-environment",
        "Author-Name-Value",
        "the-sandboxs-own-variable",
        "user.name",
        "secret.txt",
    ] {
        assert!(!log.contains(secret), "{secret} is in the log:\n{log}");
    }
}

#[test]
fn the_server_ends_with_the_codes_it_documents() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("work");
    std::fs::create_dir_all(&root).unwrap();

    // No variables: the configuration, every problem at once.
    let out = Command::new(SERVER)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    let log = String::from_utf8_lossy(&out.stdout);
    for want in [
        "invalid configuration",
        "GIT_SANDBOX_LISTEN is required",
        "GIT_SANDBOX_ROOT is required",
    ] {
        assert!(log.contains(want), "{want} in {log}");
    }

    // A path that is a file: the socket cannot be made.
    let taken = tmp.path().join("taken");
    std::fs::write(&taken, "mine").unwrap();
    let out = Command::new(SERVER)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_SANDBOX_LISTEN", &taken)
        .env("GIT_SANDBOX_ROOT", &root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(71));
    assert!(String::from_utf8_lossy(&out.stdout).contains("GIT_SANDBOX_LISTEN"));
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "mine");
}

#[test]
fn sigterm_ends_the_server_cleanly_and_removes_its_socket() {
    let mut sandbox = Sandbox::start();
    assert!(sandbox.socket.exists());
    assert_eq!(sandbox.stop().code(), Some(0));
    assert!(!sandbox.socket.exists());
}

/// A repository whose configuration runs a spy as a clean filter: the spy writes its ancestors (the
/// processes it runs under, up to the first) and what it can read of their environment to `marker`.
fn spied_repository(sandbox: &Sandbox, marker: &Path) -> PathBuf {
    let repo = repository(&sandbox.root, "repo");
    let spy = sandbox.tmp.path().join("spy.sh");
    // Up the tree from the filter: each process and what is readable of its environment. The walk
    // ends at the sandbox's server (its parent here is the test, in a pod another container) and
    // after six levels otherwise.
    script(
        &spy,
        &format!(
            "pid=$$; depth=0\nwhile [ \"$pid\" -gt 1 ] && [ $depth -lt 6 ]; do\n  comm=$(cat /proc/$pid/comm)\n  echo \"ancestor $pid $comm\" >> {m}\n  tr '\\0' '\\n' < /proc/$pid/environ | sed 's/^/environ /' >> {m} 2>/dev/null\n  case \"$comm\" in adam-git-sandb*) break;; esac\n  pid=$(sed 's/^[^)]*) . \\([0-9]*\\) .*/\\1/' /proc/$pid/stat)\n  depth=$((depth + 1))\ndone\ncat",
            m = marker.display()
        ),
    );
    git(
        &repo,
        &["config", "filter.spy.clean", spy.to_str().unwrap()],
    );
    std::fs::write(repo.join(".gitattributes"), "*.txt filter=spy\n").unwrap();
    std::fs::write(repo.join("a.txt"), "content\n").unwrap();
    repo
}

/// The `ancestor` lines of a marker.
fn ancestors(marker: &str) -> impl Iterator<Item = &str> {
    marker.lines().filter(|l| l.starts_with("ancestor "))
}

/// `sh -c '<program> add -A'` in `repo`: a caller that holds a secret in its environment and starts
/// `program` under it.
fn caller_running(program: &str, repo: &Path, sandbox: &Sandbox) -> Output {
    let mut caller = Command::new("sh");
    caller
        .current_dir(repo)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("CALLERS_SECRET", "the-callers-secret-value")
        .env("GIT_SANDBOX_SOCKET", &sandbox.socket)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // Not `exec`: the caller stays an ancestor of what it starts, as the coder is.
        .args(["-c", &format!("{program} add -A; exit $?")]);
    run(&mut caller, b"")
}

#[test]
fn a_filter_runs_in_the_servers_tree_and_not_in_the_callers() {
    let sandbox = Sandbox::start();
    let marker = sandbox.tmp.path().join("marker");
    let repo = spied_repository(&sandbox, &marker);

    // The control: git run by the caller itself. The filter runs under the caller and can read its
    // environment, secret included. This is what the sandbox is for.
    assert!(caller_running("git", &repo, &sandbox).status.success());
    let local = std::fs::read_to_string(&marker).expect("the filter ran");
    assert!(
        local.contains("environ CALLERS_SECRET=the-callers-secret-value"),
        "the control does not show the vector: {local}"
    );
    assert!(
        !ancestors(&local).any(|a| a.contains("adam-git-sandb")),
        "{local}"
    );
    std::fs::remove_file(&marker).unwrap();
    std::fs::remove_file(repo.join(".git/index")).unwrap();

    // Through the sandbox: the same filter runs again, under the server. Its ancestors are the
    // server's, its environment is the server's, and nothing of the caller's is among them.
    let through = caller_running(RELAY, &repo, &sandbox);
    assert!(
        through.status.success(),
        "{}",
        String::from_utf8_lossy(&through.stderr)
    );
    let sandboxed = std::fs::read_to_string(&marker).expect("the filter ran again, in the sandbox");
    assert!(
        ancestors(&sandboxed)
            .any(|a| a.starts_with(&format!("ancestor {} adam-git-sandb", sandbox.server.id()))),
        "the server is an ancestor: {sandboxed}"
    );
    assert!(
        sandboxed.contains("environ SANDBOX_SIDE=the-sandboxs-own-variable"),
        "what the filter reads is the server's environment: {sandboxed}"
    );
    assert!(
        !sandboxed.contains("the-callers-secret-value"),
        "{sandboxed}"
    );
    assert!(!sandboxed.contains("CALLERS_SECRET"), "{sandboxed}");
    assert!(
        !ancestors(&sandboxed).any(|a| a.ends_with(" adam-git") || a.ends_with(" sh")),
        "neither the relay nor the caller is an ancestor: {sandboxed}"
    );
}
