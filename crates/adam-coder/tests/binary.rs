//! The `adam-coder` binary as a process: configuration errors, an unreachable
//! Postgres, serving and SIGTERM. Offline, except that the cases which need a
//! database use `ADAM_TEST_POSTGRES_URL` (and skip without it).

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use a2a::{Message, Part, Role, SendMessageRequest, StreamResponse};
use adam_core::{RunId, RunStatus};
use common::pg::TestDb;
use common::{text_reply, tool_reply};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const PUBLIC_URL: &str = "http://coder.test:8080/";
const A2A_TOKEN: &str = "binary-test-token";
const GITHUB_TOKEN: &str = common::GITHUB_TOKEN;

/// A running `adam-coder` and what it wrote.
struct Proc {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

/// The environment of a valid configuration, over an empty environment (plus
/// `PATH` and `HOME`, which git and the shell need), so nothing of the
/// developer's shell leaks in.
///
/// The server binds port 0 (the operating system picks a free one, so parallel
/// tests never collide) and logs the address it got; [`Proc::ready`] reads it.
fn valid_env(database_url: &str, workspace: &Path) -> Vec<(String, String)> {
    let env = |k: &str, v: String| (k.to_owned(), v);
    vec![
        env("DATABASE_URL", database_url.to_owned()),
        env("MODEL_BASE_URL", "http://127.0.0.1:9/v1".to_owned()),
        env("MODEL_API_KEY", String::new()),
        env("MODEL", "test-model".to_owned()),
        env("GITHUB_TOKEN", GITHUB_TOKEN.to_owned()),
        env("A2A_BEARER_TOKENS", A2A_TOKEN.to_owned()),
        env("PUBLIC_URL", PUBLIC_URL.to_owned()),
        env("LISTEN_ADDR", "127.0.0.1:0".to_owned()),
        env("WORKSPACE_ROOT", workspace.to_string_lossy().into_owned()),
    ]
}

impl Proc {
    fn spawn(env: &[(String, String)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_adam-coder"));
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("adam-coder starts");
        let mut readers = Vec::new();
        let mut capture = |stream: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>| {
            let text = Arc::new(Mutex::new(String::new()));
            let sink = text.clone();
            if let Some(stream) = stream {
                readers.push(tokio::spawn(async move {
                    let mut lines = BufReader::new(stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let mut text = sink.lock().unwrap();
                        text.push_str(&line);
                        text.push('\n');
                    }
                }));
            }
            text
        };
        let stdout = capture(child.stdout.take().map(|s| Box::new(s) as _));
        let stderr = capture(child.stderr.take().map(|s| Box::new(s) as _));
        Self {
            child,
            stdout,
            stderr,
            readers,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id().expect("the process is running")
    }

    fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Everything it printed, for failure messages.
    fn logs(&self) -> String {
        format!("stdout:\n{}\nstderr:\n{}", self.stdout(), self.stderr())
    }

    async fn sigterm(&self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.pid().to_string()])
            .status()
            .await
            .expect("kill runs");
        assert!(status.success());
    }

    /// Wait for the process to exit on its own.
    async fn exit_within(&mut self, limit: Duration) -> ExitStatus {
        let status = match tokio::time::timeout(limit, self.child.wait()).await {
            Ok(status) => status.expect("wait"),
            Err(_) => panic!("still running after {limit:?}\n{}", self.logs()),
        };
        // Everything it wrote is in the pipes now; let the readers drain them.
        for reader in self.readers.drain(..) {
            let _ = reader.await;
        }
        status
    }

    fn still_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The address from the `listening` log line, once there is one.
    fn bound_addr(&self) -> Option<SocketAddr> {
        self.stdout().lines().find_map(|line| {
            let log: Value = serde_json::from_str(line).ok()?;
            (log["fields"]["message"] == "listening")
                .then(|| log["fields"]["addr"].as_str()?.parse().ok())
                .flatten()
        })
    }

    /// Wait until the server logged its address and `/healthz` answers 200;
    /// returns the address.
    async fn ready(&mut self) -> SocketAddr {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(
                self.still_running(),
                "exited before serving\n{}",
                self.logs()
            );
            let Some(addr) = self.bound_addr() else {
                assert!(
                    Instant::now() < deadline,
                    "never started listening\n{}",
                    self.logs()
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let probe = tokio::time::timeout(
                Duration::from_secs(2),
                common::try_raw(addr, "GET", "/healthz", None),
            )
            .await;
            if matches!(probe, Ok(Ok((200, _)))) {
                return addr;
            }
            assert!(
                Instant::now() < deadline,
                "never became ready\n{}",
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

// ------------------------------------------------------------------- offline

/// A misconfigured deployment must be fixed in one round trip: every problem
/// is listed, the exit code is non-zero, and nothing is started.
#[tokio::test]
async fn missing_and_bad_variables_are_reported_together_and_exit_non_zero() {
    // Nothing set.
    let mut p = Proc::spawn(&[]);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert!(!status.success());
    let err = p.stderr();
    assert!(err.contains("reading the configuration"), "{err}");
    for name in [
        "DATABASE_URL",
        "MODEL_BASE_URL",
        "MODEL_API_KEY",
        "MODEL",
        "GITHUB_TOKEN",
        "PUBLIC_URL",
        "A2A_BEARER_TOKENS",
    ] {
        assert!(err.contains(name), "{name} missing from:\n{err}");
    }

    // Set, but wrong in the new variables too.
    let tmp = tempfile::tempdir().unwrap();
    let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
    env.push(("ALLOWED_REPO_HOSTS".into(), "https://github.com/x".into()));
    env.push(("ALLOW_LOCAL_REPOS".into(), "sure".into()));
    env.push(("GITHUB_API_URL".into(), "ftp://api.example".into()));
    env.push(("WORKERS".into(), "0".into()));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert!(!status.success());
    let err = p.stderr();
    for name in [
        "ALLOWED_REPO_HOSTS",
        "ALLOW_LOCAL_REPOS",
        "GITHUB_API_URL",
        "WORKERS",
    ] {
        assert!(err.contains(name), "{name} missing from:\n{err}");
    }
    assert!(
        !err.contains("connecting to Postgres"),
        "a bad configuration must stop before anything connects:\n{err}"
    );
    assert!(!err.contains(GITHUB_TOKEN), "{err}");
}

/// Postgres unreachable at boot: a clear error, a non-zero exit, no panic, no
/// password in the output, and no waiting around.
#[tokio::test]
async fn boot_fails_fast_when_postgres_is_unreachable() {
    let tmp = tempfile::tempdir().unwrap();
    let env = valid_env(
        "postgres://adam:s3cr3tpassw0rd@127.0.0.1:1/adam",
        tmp.path(),
    );
    let started = Instant::now();
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(45)).await;
    assert!(!status.success(), "{}", p.logs());
    assert_eq!(
        status.code(),
        Some(1),
        "an error exit, not a signal or panic"
    );
    let (out, err) = (p.stdout(), p.stderr());
    assert!(err.contains("connecting to Postgres"), "{err}");
    assert!(!err.contains("panicked"), "{err}");
    for text in [&out, &err] {
        assert!(!text.contains("s3cr3tpassw0rd"), "password leaked:\n{text}");
        assert!(!text.contains(GITHUB_TOKEN), "token leaked:\n{text}");
    }
    assert!(
        out.contains("starting adam-coder") && !out.contains("listening"),
        "it starts, logs its configuration, never serves:\n{out}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(45),
        "failed within the bound"
    );
}

// ------------------------------------------------------------ with a database

/// The process serves the card (URL from `PUBLIC_URL`) and `/healthz`, refuses
/// unauthenticated calls, and stops on SIGTERM with exit code 0.
#[tokio::test]
async fn serves_card_and_healthz_then_stops_cleanly_on_sigterm() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut p = Proc::spawn(&valid_env(&db.url(), tmp.path()));
    let addr = p.ready().await;

    let (status, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200, "{card}");
    assert!(card.contains(PUBLIC_URL), "{card}");
    assert!(card.contains("adam-coder"), "{card}");
    let (status, _) = common::raw(addr, "POST", "/", None).await;
    assert_eq!(status, 401, "an unauthenticated call is refused");
    assert!(
        p.stdout().contains("listening"),
        "the bound address is logged:\n{}",
        p.stdout()
    );

    p.sigterm().await;
    let status = p.exit_within(Duration::from_secs(15)).await;
    assert_eq!(status.code(), Some(0), "{}", p.logs());
    let out = p.stdout();
    assert!(
        out.contains("shutdown requested") && out.contains("stopped"),
        "{out}"
    );
    assert!(!p.stderr().contains("panicked"), "{}", p.logs());
    db.finish().await;
}

/// Answers `POST /chat/completions` by *turn*: the reply is the one after as
/// many tool results as the conversation already holds. A model that is asked
/// the same question twice (a retried request, a replayed step) gets the same
/// answer, so the script cannot drift out of step with the run. A turn beyond
/// the script is a 500.
struct Script {
    replies: Vec<Value>,
    /// The turn of every request, in arrival order.
    asked: Arc<Mutex<Vec<usize>>>,
}

impl Respond for Script {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let turn = body["messages"]
            .as_array()
            .map_or(0, |m| m.iter().filter(|m| m["role"] == "tool").count());
        self.asked.lock().unwrap().push(turn);
        match self.replies.get(turn) {
            Some(reply) => ResponseTemplate::new(200).set_body_json(reply),
            None => ResponseTemplate::new(500).set_body_string("script exhausted"),
        }
    }
}

/// How many turns the model has been asked about so far.
fn turns(asked: &Mutex<Vec<usize>>) -> usize {
    asked.lock().unwrap().iter().max().map_or(0, |t| t + 1)
}

/// SIGTERM while OpenCode is working: the process does not abandon the step. It
/// waits for OpenCode, commits the step (the run is not lost, not failed and
/// not repeated), and exits 0. A second process over the same database and
/// workspace finishes the run: one commit, one push, one pull request.
///
/// Also the wiring test of the whole binary: the model is a wiremock
/// `/chat/completions`, GitHub is a wiremock reached through `GITHUB_API_URL`,
/// and the repository is `https://github.com/octo/widgets` under the
/// **production** policy (`ALLOW_LOCAL_REPOS` unset): git's `insteadOf`
/// (in a private `$HOME`) points that URL at a local bare repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_mid_run_commits_the_in_flight_step() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // The remote, and the private git config that makes github.com point at it.
    let remote = dir.join("remote.git");
    let seed = dir.join("seed");
    std::fs::create_dir_all(&remote).unwrap();
    std::fs::create_dir_all(&seed).unwrap();
    common::git(
        &remote,
        &["init", "--bare", "--quiet", "--initial-branch=main"],
    );
    common::git(&remote, &["config", "core.logAllRefUpdates", "always"]);
    common::git(&seed, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(seed.join("README.md"), "widgets\n").unwrap();
    common::git(&seed, &["add", "-A"]);
    common::git(&seed, &["commit", "--quiet", "-m", "seed"]);
    common::git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    common::git(&seed, &["push", "--quiet", "origin", "main"]);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        format!(
            "[url \"{}\"]\n\tinsteadOf = https://github.com/octo/widgets.git\n",
            remote.display()
        ),
    )
    .unwrap();

    // OpenCode: a script that reports it started and waits for `go`.
    let (started, go, launches) = (
        dir.join("started"),
        dir.join("go"),
        dir.join("launches.log"),
    );
    let script = dir.join("opencode.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho launched >> \"$LAUNCHES\"\n: > \"$STARTED\"\ni=0\n\
         while [ ! -e \"$GO\" ]; do i=$((i+1)); [ $i -gt 1200 ] && exit 9; sleep 0.05; done\n\
         exec \"$AGENT\"\n",
    )
    .unwrap();

    // The model: prepare, delegate, checks, commit and push, PR, done.
    let model = MockServer::start().await;
    let asked = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Script {
            replies: vec![
                tool_reply(
                    "c1",
                    "prepare_workspace",
                    json!({"repo_url": "https://github.com/octo/widgets", "base_branch": "main"}),
                ),
                tool_reply(
                    "c2",
                    "delegate_to_opencode",
                    json!({"instructions": "add hello.txt containing hello"}),
                ),
                tool_reply(
                    "c3",
                    "run_checks",
                    json!({"command": "test -f hello.txt && cat hello.txt"}),
                ),
                tool_reply("c4", "commit_and_push", json!({"message": "feat: add hello.txt"})),
                tool_reply(
                    "c5",
                    "open_pull_request",
                    json!({"title": "feat: add hello.txt", "body": "Adds hello.txt.\n\n## Verification\n- passed"}),
                ),
                text_reply("Opened the pull request."),
            ],
            asked: asked.clone(),
        })
        .mount(&model)
        .await;
    let github = common::mock_github().await;

    let workspace = dir.join("work");
    let env_for = || {
        let mut env = valid_env(&db.url(), &workspace);
        env.extend([
            ("MODEL_BASE_URL".to_owned(), model.uri()),
            ("GITHUB_API_URL".to_owned(), github.uri()),
            ("HOME".to_owned(), home.to_string_lossy().into_owned()),
            (
                "OPENCODE_COMMAND".to_owned(),
                format!("/bin/sh {}", script.display()),
            ),
            (
                "AGENT".to_owned(),
                common::fake_agent().to_string_lossy().into_owned(),
            ),
            ("STARTED".to_owned(), started.to_string_lossy().into_owned()),
            ("GO".to_owned(), go.to_string_lossy().into_owned()),
            (
                "LAUNCHES".to_owned(),
                launches.to_string_lossy().into_owned(),
            ),
            ("FAKE_ACP_SCENARIO".to_owned(), "write-file".to_owned()),
            ("FAKE_ACP_WRITE_PATH".to_owned(), "hello.txt".to_owned()),
            ("FAKE_ACP_WRITE_CONTENT".to_owned(), "hello\n".to_owned()),
        ]);
        env
    };

    // Process A takes the task and gets into OpenCode's turn.
    let mut a = Proc::spawn(&env_for());
    let addr_a = a.ready().await;
    let client = common::a2a_client(addr_a, A2A_TOKEN).await;
    let mut stream = client
        .send_streaming_message(&SendMessageRequest {
            message: Message::new(
                Role::User,
                vec![Part::text(
                    "In https://github.com/octo/widgets (base main) add hello.txt containing hello",
                )],
            ),
            configuration: None,
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    let Some(Ok(StreamResponse::Task(task))) = stream.next().await else {
        panic!("the first event is the task\n{}", a.logs());
    };
    let run = RunId(task.id.parse().expect("task id is a run id"));
    drop(stream);
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(
            Instant::now() < deadline,
            "OpenCode never started\n{}",
            a.logs()
        );
        assert!(a.still_running(), "{}", a.logs());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(turns(&asked), 2, "prepare and delegate turns");

    // SIGTERM now: the step is in flight, so the process must not exit yet.
    a.sigterm().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        a.still_running(),
        "the process left while OpenCode was working\n{}",
        a.logs()
    );
    let store = db.store();
    let mid = store.load_run(run).await.unwrap().expect("the run exists");
    assert_eq!(mid.status, RunStatus::Runnable, "still mid-step");

    // OpenCode finishes; the step commits; the process exits 0.
    std::fs::write(&go, "").unwrap();
    let status = a.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(0), "{}", a.logs());
    assert!(a.stdout().contains("shutdown requested"), "{}", a.logs());
    let after = store.load_run(run).await.unwrap().unwrap();
    assert_eq!(
        after.status,
        RunStatus::Runnable,
        "not lost, not failed: {after:?}"
    );
    assert!(
        after.version > mid.version,
        "the in-flight step was committed ({} -> {})",
        mid.version,
        after.version
    );
    assert!(
        after.state.to_string().contains("OpenCode finished"),
        "OpenCode's result is in the run: {}",
        after.state
    );
    assert_eq!(turns(&asked), 2, "no new turn after SIGTERM");
    assert!(
        github.received_requests().await.unwrap().is_empty(),
        "nothing reached GitHub yet"
    );
    let worktree = workspace.join("worktrees").join(run.to_string());
    assert_eq!(
        std::fs::read_to_string(worktree.join("hello.txt")).unwrap(),
        "hello\n"
    );

    // Process B, same database and workspace, finishes the run.
    let mut b = Proc::spawn(&env_for());
    let deadline = Instant::now() + Duration::from_secs(90);
    let done = loop {
        let rec = store.load_run(run).await.unwrap().unwrap();
        if rec.status.is_terminal() {
            break rec;
        }
        assert!(
            Instant::now() < deadline,
            "the run did not finish\n{}",
            b.logs()
        );
        assert!(b.still_running(), "{}", b.logs());
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(done.status, RunStatus::Done, "{done:?}\n{}", b.logs());
    b.sigterm().await;
    let status = b.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(0), "{}", b.logs());

    assert_eq!(
        common::launches(&launches),
        1,
        "OpenCode ran once, not again"
    );
    assert_eq!(
        turns(&asked),
        6,
        "the model was asked six turns in all: {:?}",
        asked.lock().unwrap()
    );
    let branches: Vec<String> = common::git(
        &remote,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/agent",
        ],
    )
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert_eq!(
        common::git(&remote, &["show", &format!("{}:hello.txt", branches[0])]),
        "hello"
    );
    assert_eq!(
        common::git(
            &remote,
            &["rev-list", "--count", &format!("main..{}", branches[0])]
        ),
        "1"
    );
    let ref_log = remote.join("logs/refs/heads").join(&branches[0]);
    assert_eq!(
        std::fs::read_to_string(ref_log).unwrap().lines().count(),
        1,
        "the branch was pushed once"
    );
    let pulls: Vec<Value> = github
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let tool_results: Vec<Value> = model
        .received_requests()
        .await
        .unwrap()
        .last()
        .and_then(|r| serde_json::from_slice::<Value>(&r.body).ok())
        .and_then(|b| b["messages"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(
        pulls.len(),
        1,
        "exactly one pull request: {pulls:?}; the model was asked (turn per request): {:?}; what it was told last: {tool_results:#?}\n{}\n{}",
        asked.lock().unwrap(),
        a.logs(),
        b.logs()
    );
    assert_eq!(pulls[0]["head"], branches[0].as_str());
    assert_eq!(pulls[0]["base"], "main");
    // The PR call carried the token GitHub is meant to see, and only there.
    let authorized = github.received_requests().await.unwrap().iter().all(|r| {
        r.headers.get("authorization").and_then(|v| v.to_str().ok())
            == Some(format!("Bearer {GITHUB_TOKEN}").as_str())
    });
    assert!(authorized, "GitHub calls carry the bearer token");
    let all_logs = format!("{}{}", a.logs(), b.logs());
    assert!(
        !all_logs.contains(GITHUB_TOKEN),
        "the token is never logged"
    );
    db.finish().await;
}
