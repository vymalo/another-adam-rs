//! The server and the client in one process: what a request may do, what is refused, how a command
//! ends. (The processes are in `relay.rs`.)
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_git_sandbox::{
    Client, ClientError, Config, Request, SandboxError, Server, protocol::RefusalReason,
};
use common::{alive, git, pid_in, real_git, repository, script, wait_until};
use tokio::sync::oneshot;

/// A server on a socket in a temporary directory, serving in the background.
struct Running {
    tmp: tempfile::TempDir,
    root: PathBuf,
    client: Client,
    stop: Option<oneshot::Sender<()>>,
    done: Option<tokio::task::JoinHandle<()>>,
}

impl Running {
    /// With the real git.
    async fn start() -> Self {
        Self::with(real_git(), 64).await
    }

    async fn with(git: PathBuf, max_commands: usize) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        let socket = tmp.path().join("git.sock");
        let config = Config {
            listen: socket.clone(),
            root: root.clone(),
            git,
            max_commands,
        };
        let server = Server::bind(&config).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let done = tokio::spawn(server.serve(async move {
            let _ = stopped.await;
        }));
        Self {
            tmp,
            root,
            client: Client::new(socket),
            stop: Some(stop),
            done: Some(done),
        }
    }

    /// A request for `git <args>` in `cwd` with the environment git needs to be hermetic.
    fn request(&self, cwd: &Path, args: &[&str]) -> Request {
        Request::new(
            args.iter().copied(),
            cwd,
            [
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("LC_ALL", "C"),
            ],
        )
    }

    async fn git(&self, cwd: &Path, args: &[&str]) -> adam_git_sandbox::Output {
        self.client
            .output(self.request(cwd, args), &[])
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(done) = self.done.take() {
            done.await.unwrap();
        }
    }
}

fn refused(error: ClientError) -> RefusalReason {
    match error {
        ClientError::Refused(refusal) => refusal.reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_gives_what_git_gives() {
    let fx = Running::start().await;
    let repo = repository(&fx.root, "repo");
    let out = fx.git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    assert!(out.exit.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
    assert!(out.stderr.is_empty());

    // A failing command: its status and its message arrive, and it is not an error of the call.
    let out = fx.git(&repo, &["rev-parse", "--verify", "nope"]).await;
    assert_eq!(out.exit.code, Some(128));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Needed a single revision"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_output_and_a_large_input_cross_whole() {
    let fx = Running::start().await;
    let repo = repository(&fx.root, "repo");
    let blob: Vec<u8> = (0..6_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(repo.join("big.bin"), &blob).unwrap();
    git(&repo, &["add", "big.bin"]);
    git(&repo, &["commit", "--quiet", "-m", "big"]);

    let out = fx.git(&repo, &["cat-file", "blob", "HEAD:big.bin"]).await;
    assert!(out.exit.success());
    assert_eq!(out.stdout.len(), blob.len());
    assert!(out.stdout == blob, "the bytes differ");

    // The same bytes in: the object id is the one git computes itself.
    let hashed = fx
        .client
        .output(fx.request(&repo, &["hash-object", "--stdin"]), &blob)
        .await
        .unwrap();
    assert!(hashed.exit.success());
    assert_eq!(
        String::from_utf8_lossy(&hashed.stdout).trim(),
        git(&repo, &["rev-parse", "HEAD:big.bin"])
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_command_is_always_the_servers_git() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = tmp.path().join("stub-git");
    script(
        &stub,
        "echo \"program=$0\"; for a in \"$@\"; do echo \"arg=$a\"; done",
    );
    let fx = Running::with(stub.clone(), 4).await;

    // The arguments that look like a program, a shell, another git: all are arguments.
    let out = fx
        .git(
            &fx.root,
            &["/bin/sh", "-c", "touch /tmp/never", "--exec-path=/evil"],
        )
        .await;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains(&format!("program={}", stub.display())),
        "{text}"
    );
    for arg in [
        "arg=/bin/sh",
        "arg=-c",
        "arg=touch /tmp/never",
        "arg=--exec-path=/evil",
    ] {
        assert!(text.contains(arg), "{arg} in {text}");
    }
    assert!(!Path::new("/tmp/never").exists());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_directory_outside_the_root_or_not_plain_is_refused_and_nothing_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("ran");
    let stub = tmp.path().join("stub-git");
    script(&stub, &format!("touch {}", marker.display()));
    let fx = Running::with(stub, 4).await;
    let elsewhere = fx.tmp.path().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("inside")).unwrap();
    std::fs::create_dir_all(fx.root.join("slot")).unwrap();
    std::fs::write(fx.root.join("file"), "x").unwrap();
    std::os::unix::fs::symlink(&elsewhere, fx.root.join("link")).unwrap();
    std::os::unix::fs::symlink(fx.root.join("slot"), fx.root.join("alias")).unwrap();

    let cases: Vec<PathBuf> = vec![
        PathBuf::from("/"),
        PathBuf::from("/etc"),
        fx.tmp.path().to_owned(),
        fx.root.join(".."),
        fx.root.join("slot/../.."),
        fx.root.join("link"),
        fx.root.join("link/inside"),
        fx.root.join("alias"),
        fx.root.join("file"),
        fx.root.join("missing"),
        PathBuf::from("relative/dir"),
    ];
    for cwd in cases {
        let error = fx
            .client
            .output(fx.request(&cwd, &["status"]), &[])
            .await
            .unwrap_err();
        assert_eq!(
            error.class(),
            ErrorClass::Invalid,
            "{}: {error}",
            cwd.display()
        );
        assert_eq!(
            refused(error),
            RefusalReason::BadDirectory,
            "{}",
            cwd.display()
        );
    }
    assert!(!marker.exists(), "git was started for a refused request");

    // The root itself and a plain directory are fine.
    for cwd in [fx.root.clone(), fx.root.join("slot")] {
        fx.client
            .output(fx.request(&cwd, &["status"]), &[])
            .await
            .unwrap();
    }
    assert!(marker.exists());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_that_is_not_one_is_refused() {
    let fx = Running::start().await;
    let mut request = fx.request(&fx.root, &["status"]);
    request.version = 2;
    let error = fx.client.output(request, &[]).await.unwrap_err();
    assert_eq!(refused(error), RefusalReason::UnsupportedVersion);

    let request = fx.request(&fx.root, &[]);
    let error = fx.client.output(request, &[]).await.unwrap_err();
    assert_eq!(refused(error), RefusalReason::NoArguments);

    let request = fx.request(&fx.root, &["status\0--help"]);
    let error = fx.client.output(request, &[]).await.unwrap_err();
    assert_eq!(refused(error), RefusalReason::BadRequest);

    // The server is still there.
    assert!(fx.git(&fx.root, &["--version"]).await.exit.success());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_allowed_environment_reaches_the_command() {
    let fx = Running::start().await;
    let repo = repository(&fx.root, "repo");
    let mut request = fx.request(&repo, &["-c", "alias.showenv=!env", "showenv"]);
    for (name, value) in [
        ("MODEL_API_KEY", "sk-must-not-arrive"),
        ("DATABASE_URL", "postgres://user:pw@db/x"),
        ("GITHUB_APP_PRIVATE_KEY_PATH", "/secrets/key.pem"),
        ("GIT_ASKPASS", "/evil/askpass"),
        ("PATH", "/evil/bin"),
        ("GIT_AUTHOR_NAME", "Ada Lovelace"),
        ("TZ", "UTC"),
        ("HOME", "/home/somebody"),
    ] {
        request.env.push(adam_git_sandbox::protocol::EnvPair {
            name: name.to_owned(),
            value: std::ffi::OsString::from(value).into(),
        });
    }
    let out = fx.client.output(request, &[]).await.unwrap();
    assert!(
        out.exit.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = String::from_utf8_lossy(&out.stdout).into_owned();

    for gone in [
        "MODEL_API_KEY",
        "sk-must-not-arrive",
        "DATABASE_URL",
        "pw@db",
        "GITHUB_APP",
        "/secrets",
        "GIT_ASKPASS",
        "/evil",
    ] {
        assert!(!seen.contains(gone), "{gone} reached the command: {seen}");
    }
    for kept in [
        "GIT_AUTHOR_NAME=Ada Lovelace",
        "TZ=UTC",
        "HOME=/home/somebody",
    ] {
        assert!(seen.lines().any(|l| l == kept), "{kept} in {seen}");
    }
    assert!(
        seen.lines()
            .any(|l| l.starts_with("PATH=") && l.ends_with("/usr/local/bin:/usr/bin:/bin")),
        "the path is the server's: {seen}"
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_way_a_token_travels_is_dropped_on_the_way_in() {
    let fx = Running::start().await;
    let repo = repository(&fx.root, "repo");
    let mut request = fx.request(&repo, &["config", "--list"]);
    for (name, value) in [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader"),
        ("GIT_CONFIG_VALUE_0", "Authorization: Basic c2VjcmV0"),
    ] {
        request.env.push(adam_git_sandbox::protocol::EnvPair {
            name: name.to_owned(),
            value: std::ffi::OsString::from(value).into(),
        });
    }
    let out = fx.client.output(request, &[]).await.unwrap();
    assert!(out.exit.success());
    let seen = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(
        !seen.contains("extraheader") && !seen.contains("authorization"),
        "{seen}"
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn how_a_command_ends_is_told() {
    let tmp = tempfile::tempdir().unwrap();
    let stub = tmp.path().join("stub-git");
    script(
        &stub,
        "case \"$1\" in\n  code) echo out; echo err >&2; exit 7;;\n  kill) kill -9 $$;;\nesac",
    );
    let fx = Running::with(stub, 4).await;
    let out = fx.git(&fx.root, &["code"]).await;
    assert_eq!(out.exit.code, Some(7));
    assert_eq!(out.exit.shell_code(), 7);
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");

    let out = fx.git(&fx.root, &["kill"]).await;
    assert_eq!(out.exit.code, None);
    assert_eq!(out.exit.signal, Some(9));
    assert_eq!(out.exit.shell_code(), 137);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_a_command_leaves_running_does_not_outlive_it_or_hold_the_call() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("background.pid");
    let stub = tmp.path().join("stub-git");
    // The background process keeps the command's output open, as a daemonised filter would.
    script(
        &stub,
        &format!("sleep 600 &\necho $! > {}\necho done", pid_file.display()),
    );
    let fx = Running::with(stub, 4).await;
    let out = tokio::time::timeout(Duration::from_secs(20), fx.git(&fx.root, &["x"]))
        .await
        .expect("the call ends with the command, not with its leftovers");
    assert_eq!(out.stdout, b"done\n");
    let background = pid_in(&pid_file);
    assert!(
        wait_until(Duration::from_secs(5), || !alive(background)),
        "the process the command left is still running"
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_goes_away_takes_the_commands_group_with_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (leader, child) = (tmp.path().join("leader.pid"), tmp.path().join("child.pid"));
    let stub = tmp.path().join("stub-git");
    script(
        &stub,
        &format!(
            "echo $$ > {}\nsleep 600 &\necho $! > {}\nwait",
            leader.display(),
            child.display()
        ),
    );
    let fx = Running::with(stub, 4).await;
    let call = tokio::spawn({
        let client = fx.client.clone();
        let request = fx.request(&fx.root, &["x"]);
        async move { client.output(request, &[]).await }
    });
    let (leader, child) = (pid_in(&leader), pid_in(&child));
    assert!(alive(leader) && alive(child));

    // The call is dropped: its connection closes.
    call.abort();
    let _ = call.await;
    assert!(
        wait_until(Duration::from_secs(10), || !alive(leader) && !alive(child)),
        "the command (leader alive: {}, child alive: {}) outlived its client",
        alive(leader),
        alive(child)
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_kills_the_commands_that_run_and_removes_the_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("leader.pid");
    let stub = tmp.path().join("stub-git");
    script(
        &stub,
        &format!("echo $$ > {}\nexec sleep 600", pid_file.display()),
    );
    let fx = Running::with(stub, 4).await;
    let socket = fx.client.socket().to_owned();
    let call = tokio::spawn({
        let client = fx.client.clone();
        let request = fx.request(&fx.root, &["x"]);
        async move { client.output(request, &[]).await }
    });
    let pid = pid_in(&pid_file);
    fx.stop().await;
    assert!(
        wait_until(Duration::from_secs(5), || !alive(pid)),
        "the command still runs"
    );
    assert!(!socket.exists(), "the socket is removed");
    // The caller is told the connection ended, not that the command did.
    let ended = call.await.unwrap();
    assert!(matches!(ended, Err(ClientError::Disconnected)), "{ended:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn at_most_the_limit_of_commands_run_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (first_in, release, second_in) = (
        tmp.path().join("first-in"),
        tmp.path().join("release"),
        tmp.path().join("second-in"),
    );
    let stub = tmp.path().join("stub-git");
    script(
        &stub,
        &format!(
            "case \"$1\" in\n  first) touch {first_in}; while [ ! -e {release} ]; do sleep 0.05; done;;\n  second) touch {second_in};;\nesac",
            first_in = first_in.display(),
            release = release.display(),
            second_in = second_in.display()
        ),
    );
    let fx = Running::with(stub, 1).await;
    let run = |arg: &'static str| {
        let client = fx.client.clone();
        let request = fx.request(&fx.root, &[arg]);
        tokio::spawn(async move { client.output(request, &[]).await })
    };
    let first = run("first");
    assert!(wait_until(Duration::from_secs(10), || first_in.exists()));
    let second = run("second");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !second_in.exists(),
        "the second command started beside the first"
    );
    std::fs::write(&release, "").unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert!(second_in.exists());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nobody_on_the_socket_is_an_error_and_not_a_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let client = Client::new(tmp.path().join("nobody.sock"));
    let request = Request::new(["--version"], tmp.path(), Vec::<(String, String)>::new());
    let error = client.output(request, &[]).await.unwrap_err();
    assert!(
        matches!(error, ClientError::Unreachable { .. }),
        "{error:?}"
    );
    assert_eq!(error.class(), ErrorClass::Transient);
    assert_eq!(error.exit_code(), 69);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_socket_is_private_and_a_path_that_is_not_a_socket_is_not_replaced() {
    let fx = Running::start().await;
    let mode = std::fs::metadata(fx.client.socket())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");

    // A server already answers there.
    let again = Config {
        listen: fx.client.socket().to_owned(),
        root: fx.root.clone(),
        git: real_git(),
        max_commands: 4,
    };
    let error = Server::bind(&again).unwrap_err();
    assert!(
        matches!(&error, SandboxError::Bind { source, .. } if source.kind() == std::io::ErrorKind::AddrInUse),
        "{error:?}"
    );
    assert_eq!(adam_git_sandbox::exit_code(&error), 71);

    // A file, a link to a socket: not replaced.
    let file = fx.tmp.path().join("a-file");
    std::fs::write(&file, "mine").unwrap();
    let link = fx.tmp.path().join("a-link");
    std::os::unix::fs::symlink(fx.client.socket(), &link).unwrap();
    for taken in [&file, &link] {
        let config = Config {
            listen: taken.clone(),
            ..again.clone()
        };
        let error = Server::bind(&config).unwrap_err();
        assert!(
            matches!(&error, SandboxError::Bind { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists),
            "{error:?}"
        );
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_socket_nobody_answers_on_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    let socket = tmp.path().join("git.sock");
    let config = Config {
        listen: socket.clone(),
        root,
        git: real_git(),
        max_commands: 4,
    };
    // A server that died without removing its socket.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    assert!(socket.exists());
    let server = Server::bind(&config).unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let done = tokio::spawn(server.serve(async move {
        let _ = stopped.await;
    }));
    let out = Client::new(&socket)
        .output(
            Request::new(
                ["--version"],
                config.root.clone(),
                Vec::<(String, String)>::new(),
            ),
            &[],
        )
        .await
        .unwrap();
    assert!(out.exit.success());
    let _ = stop.send(());
    done.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_that_left_the_group_does_not_hold_the_call_for_ever() {
    let tmp = tempfile::tempdir().unwrap();
    let pid_file = tmp.path().join("escaped.pid");
    let stub = tmp.path().join("stub-git");
    // `setsid` puts the background process in a session and a group of its own: killing the
    // command's group does not reach it, and it keeps the output open.
    script(
        &stub,
        &format!(
            "setsid sh -c 'echo $$ > {}; exec sleep 600' &\nwhile [ ! -s {} ]; do sleep 0.05; done\necho done",
            pid_file.display(),
            pid_file.display()
        ),
    );
    let fx = Running::with(stub, 4).await;
    let started = std::time::Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(30), fx.git(&fx.root, &["x"]))
        .await
        .expect("the call ends");
    assert_eq!(out.stdout, b"done\n");
    assert!(out.exit.success());
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    // The one that left is not the sandbox's to kill; the test does.
    let escaped = pid_in(&pid_file);
    assert!(alive(escaped), "it was out of reach of the group kill");
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &escaped.to_string()])
        .status();
    fx.stop().await;
}
