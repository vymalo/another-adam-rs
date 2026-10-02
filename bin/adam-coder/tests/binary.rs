//! The `adam-coder` binary as a process: configuration errors, the roles, an unreachable
//! Postgres, serving and SIGTERM. Offline, except that the cases which need a
//! database use `ADAM_TEST_POSTGRES_URL` (and skip without it).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use a2a::{Message, Part, Role, SendMessageRequest, StreamResponse, TaskState};
use adam_core::{RunId, RunStatus};
use common::pg::TestDb;
// A folder here is the shipped agent without its `mcp.json` (the shipped one names the GitHub
// server, a local process: see `common::plain_folder`); the tests of `mcp.json` write their own.
use common::{chat_response, edit_instructions, plain_folder as folder, text_reply, tool_reply};
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

/// One folder for every test of this file that needs the shipped agent's files and no server of
/// its `mcp.json`: made once, kept until the process ends.
fn plain_agent_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(folder).path()
}

/// The environment of a valid configuration, over an empty environment (plus
/// `PATH` and `HOME`, which git and the shell need), so nothing of the
/// developer's shell leaks in.
///
/// The server binds port 0 (the operating system picks a free one, so parallel
/// tests never collide) and logs the address it got; [`Proc::ready`] reads it.
///
/// The agent files are a folder: the shipped `agent/` without its `mcp.json` ([`plain_agent_dir`]),
/// because the shipped file starts the GitHub MCP server, a local process that needs
/// `MCP_ALLOW_STDIO` and the `github-mcp-server` binary. A test of the embedded copy removes
/// `ADAM_AGENT_DIR`; [`the_embedded_agent_connects_the_real_github_mcp_server`] does, with both.
fn valid_env(database_url: &str, workspace: &Path) -> Vec<(String, String)> {
    let env = |k: &str, v: String| (k.to_owned(), v);
    vec![
        env(
            "ADAM_AGENT_DIR",
            plain_agent_dir().to_string_lossy().into_owned(),
        ),
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

/// `valid_env` for `role`, with only the variables the role reads: without the front's variables
/// when the role serves no A2A, and without the model, GitHub and workspace variables when it runs
/// no workers, so a test proves that the process starts without them.
fn role_env(role: &str, database_url: &str, workspace: &Path) -> Vec<(String, String)> {
    let mut env = valid_env(database_url, workspace);
    env.push(("ROLE".to_owned(), role.to_owned()));
    if role == "worker" {
        env.retain(|(k, _)| k != "A2A_BEARER_TOKENS" && k != "PUBLIC_URL");
    }
    if role == "control-plane" {
        env.retain(|(k, _)| {
            !matches!(
                k.as_str(),
                "MODEL_BASE_URL" | "MODEL_API_KEY" | "MODEL" | "GITHUB_TOKEN" | "WORKSPACE_ROOT"
            )
        });
    }
    env
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

    /// Wait until the process logged that `LISTEN` is active, so it hears other processes.
    async fn notifying(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.stdout().contains("listening for notifications") {
            assert!(self.still_running(), "exited early\n{}", self.logs());
            assert!(
                Instant::now() < deadline,
                "never listened for notifications\n{}",
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

/// The failure the process logged, as one structured line: the `fields` of the
/// `adam-coder failed` event on stdout (the JSON logger's stream). A failure is
/// exactly one such line, and nothing goes to stderr.
fn failure(p: &Proc) -> Value {
    let out = p.stdout();
    let lines: Vec<Value> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["fields"]["message"] == "adam-coder failed")
        .collect();
    assert_eq!(lines.len(), 1, "exactly one failure line:\n{}", p.logs());
    assert_eq!(lines[0]["level"], "ERROR", "{}", p.logs());
    assert!(
        p.stderr().trim().is_empty(),
        "no Debug dump outside the logger:\n{}",
        p.stderr()
    );
    lines[0]["fields"].clone()
}

/// A misconfigured deployment must be fixed in one round trip: every problem
/// is listed, the exit code is 78 (`EX_CONFIG`, so a supervisor does not
/// restart it), and nothing is started.
#[tokio::test]
async fn missing_and_bad_variables_are_reported_together_and_exit_78() {
    // Nothing set.
    let mut p = Proc::spawn(&[]);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let fields = failure(&p);
    assert_eq!(fields["code"], 78);
    let err = fields["error"].as_str().unwrap().to_owned();
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
    env.push(("WORKSPACE_SWEEP_SECS".into(), "often".into()));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    for name in [
        "ALLOWED_REPO_HOSTS",
        "ALLOW_LOCAL_REPOS",
        "GITHUB_API_URL",
        "WORKERS",
        "WORKSPACE_SWEEP_SECS",
    ] {
        assert!(err.contains(name), "{name} missing from:\n{err}");
    }
    assert!(
        !err.contains("connecting to Postgres"),
        "a bad configuration must stop before anything connects:\n{err}"
    );
    assert!(!err.contains(GITHUB_TOKEN), "{err}");
}

/// `ROLE` is one of `all`, `control-plane` and `worker`. Anything else is a configuration
/// error (exit 78) that names the variable and the accepted values, and nothing is started.
#[tokio::test]
async fn an_unknown_role_is_a_configuration_error_naming_the_accepted_values() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
    env.push(("ROLE".into(), "boss".into()));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let fields = failure(&p);
    assert_eq!(fields["code"], 78);
    let err = fields["error"].as_str().unwrap().to_owned();
    assert!(err.contains("ROLE"), "{err}");
    assert!(err.contains("\"boss\""), "{err}");
    for accepted in ["all", "control-plane", "worker"] {
        assert!(err.contains(accepted), "{accepted} missing from:\n{err}");
    }
    assert!(
        !err.contains("connecting to Postgres"),
        "a bad role must stop before anything connects:\n{err}"
    );
}

/// `WORKSPACE_PLACEMENT` and `WORKER_ID`: a placement that pins runs needs a stable worker id,
/// the coder's tools need a workspace, and every such mistake is exit 78 naming the variable,
/// with nothing connected or started.
#[tokio::test]
async fn a_bad_placement_is_a_configuration_error_naming_the_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let cases: [(&str, Option<&str>, &str); 4] = [
        ("affinity", None, "WORKER_ID is required"),
        ("isolated", None, "WORKER_ID is required"),
        ("a2a-only", Some("coder-0"), "a2a-only"),
        ("pinned", None, "\"pinned\""),
    ];
    for (placement, worker_id, wants) in cases {
        let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
        env.push(("WORKSPACE_PLACEMENT".into(), placement.into()));
        if let Some(id) = worker_id {
            env.push(("WORKER_ID".into(), id.into()));
        }
        let mut p = Proc::spawn(&env);
        let status = p.exit_within(Duration::from_secs(30)).await;
        assert_eq!(status.code(), Some(78), "{placement}: {}", p.logs());
        let fields = failure(&p);
        assert_eq!(fields["code"], 78);
        let err = fields["error"].as_str().unwrap().to_owned();
        assert!(
            err.contains("WORKSPACE_PLACEMENT") || err.contains("WORKER_ID"),
            "{err}"
        );
        assert!(
            err.contains(wants),
            "{placement}: {wants:?} missing from:\n{err}"
        );
        assert!(
            !err.contains("connecting to Postgres"),
            "a bad placement must stop before anything connects:\n{err}"
        );
    }
}

/// What each role requires. The model and GitHub variables belong to the roles that run workers
/// (`all`, `worker`); a control plane starts without them (`a_control_plane_serves_a2a_...`). A
/// missing front variable is one only for the roles that serve A2A. (That a worker *starts*
/// without them is `a_worker_serves_only_healthz_...`.)
#[tokio::test]
async fn each_role_reports_the_variables_it_is_missing_and_exits_78() {
    let tmp = tempfile::tempdir().unwrap();
    for (role, remove, reported) in [
        ("control-plane", "A2A_BEARER_TOKENS", "A2A_BEARER_TOKENS"),
        ("control-plane", "PUBLIC_URL", "PUBLIC_URL"),
        ("control-plane", "DATABASE_URL", "DATABASE_URL"),
        ("all", "A2A_BEARER_TOKENS", "A2A_BEARER_TOKENS"),
        ("all", "GITHUB_TOKEN", "GITHUB_TOKEN"),
        ("worker", "GITHUB_TOKEN", "GITHUB_TOKEN"),
        ("worker", "MODEL", "MODEL"),
        ("worker", "MODEL_API_KEY", "MODEL_API_KEY"),
    ] {
        let mut env = role_env(role, "postgres://u:p@127.0.0.1:1/x", tmp.path());
        env.retain(|(k, _)| k != remove);
        let mut p = Proc::spawn(&env);
        let status = p.exit_within(Duration::from_secs(30)).await;
        assert_eq!(
            status.code(),
            Some(78),
            "{role} without {remove}\n{}",
            p.logs()
        );
        let err = failure(&p)["error"].as_str().unwrap().to_owned();
        assert!(
            err.contains(reported),
            "{role}: {reported} missing from:\n{err}"
        );
        assert!(!err.contains("connecting to Postgres"), "{role}: {err}");
    }
}

/// Postgres unreachable at boot: a clear error, exit code 69 (`EX_UNAVAILABLE`, so
/// a supervisor retries later), no panic, no password in the output, and no
/// waiting around.
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
    assert_eq!(
        status.code(),
        Some(69),
        "an unreachable dependency, not a signal, a panic or a generic failure:\n{}",
        p.logs()
    );
    let fields = failure(&p);
    assert_eq!(fields["code"], 69);
    let (out, err) = (p.stdout(), p.stderr());
    let chain = fields["error"].as_str().unwrap();
    assert!(chain.starts_with("connecting to Postgres: "), "{chain}");
    assert!(
        !out.contains("panicked") && !err.contains("panicked"),
        "{out}"
    );
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
    p.notifying().await;

    let (status, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200, "{card}");
    assert!(card.contains(PUBLIC_URL), "{card}");
    assert_eq!(common::json_of(&card)["name"], "Coder", "{card}");
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

/// A worker serves no A2A: its listener answers `/healthz` (so probes work) and nothing else, it
/// starts without `A2A_BEARER_TOKENS` and `PUBLIC_URL`, it creates its workspace root, and it
/// stops on SIGTERM with exit code 0.
#[tokio::test]
async fn a_worker_serves_only_healthz_needs_no_front_variables_and_stops_on_sigterm() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("work");
    let mut p = Proc::spawn(&role_env("worker", &db.url(), &workspace));
    let addr = p.ready().await;

    for (method, path) in [
        ("GET", "/.well-known/agent-card.json"),
        ("POST", "/"),
        ("GET", "/"),
    ] {
        let (status, body) = common::raw(addr, method, path, None).await;
        assert_eq!(status, 404, "a worker has no A2A: {method} {path}: {body}");
    }
    assert!(workspace.is_dir(), "a worker creates its workspace root");
    let out = p.stdout();
    assert!(
        out.contains("\"role\":\"worker\"") || out.contains("role=worker"),
        "the role is logged:\n{out}"
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

/// The `affinity` placement makes the worker keep its files in `WORKSPACE_ROOT/<WORKER_ID>`,
/// and it logs the placement and the worker id it runs with.
#[tokio::test]
async fn an_affinity_worker_works_in_a_folder_named_after_its_id() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("work");
    let mut env = role_env("worker", &db.url(), &workspace);
    env.push(("WORKSPACE_PLACEMENT".into(), "affinity".into()));
    env.push(("WORKER_ID".into(), "coder-7".into()));
    let mut p = Proc::spawn(&env);
    let _addr = p.ready().await;

    assert!(
        workspace.join("coder-7").is_dir(),
        "the worker's own folder"
    );
    let out = p.stdout();
    assert!(out.contains("affinity") && out.contains("coder-7"), "{out}");

    p.sigterm().await;
    let status = p.exit_within(Duration::from_secs(15)).await;
    assert_eq!(status.code(), Some(0), "{}", p.logs());
    db.finish().await;
}

// ------------------------------------------------------------------- agent folders

/// `ADAM_AGENT_DIR` names a folder that must exist: a missing one is a configuration error (exit
/// 78) that names the variable, and nothing is started (no database is needed to see it).
#[tokio::test]
async fn a_missing_agent_folder_is_a_configuration_error_naming_the_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
    env.push(("ADAM_AGENT_DIR".into(), "/nonexistent/agent".into()));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("ADAM_AGENT_DIR"), "{err}");
    assert!(err.contains("/nonexistent/agent"), "{err}");
    assert!(!err.contains("connecting to Postgres"), "{err}");
}

/// A folder with mistakes in its files stops every role before anything connects, with exit 78
/// and every finding as `path:line: error: ...` in the one failure line, so one round trip fixes
/// them all.
#[tokio::test]
async fn a_folder_with_errors_exits_78_with_every_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = folder();
    // Two subagents whose frontmatter is not YAML: both are found in one load.
    std::fs::create_dir_all(agent.path().join("agent/subagents")).unwrap();
    for name in ["broken", "worse"] {
        std::fs::write(
            agent.path().join(format!("agent/subagents/{name}.md")),
            "---\n: : [\n---\nSub.\n",
        )
        .unwrap();
    }
    for role in ["all", "control-plane", "worker"] {
        let mut env = role_env(role, "postgres://u:p@127.0.0.1:1/x", tmp.path());
        env.push((
            "ADAM_AGENT_DIR".into(),
            agent.path().to_string_lossy().into_owned(),
        ));
        let mut p = Proc::spawn(&env);
        let status = p.exit_within(Duration::from_secs(30)).await;
        assert_eq!(status.code(), Some(78), "{role}: {}", p.logs());
        let fields = failure(&p);
        assert_eq!(fields["code"], 78);
        let err = fields["error"].as_str().unwrap().to_owned();
        assert!(err.contains("reading the agent files"), "{role}: {err}");
        for file in ["broken", "worse"] {
            let at = format!("agent/subagents/{file}.md:");
            assert!(err.contains(&at), "{role}: {at} missing from {err}");
        }
        assert!(err.contains(": error: "), "{role}: {err}");
        assert!(
            !err.contains("connecting to Postgres"),
            "files are read before anything connects: {err}"
        );
    }
}

/// A folder of another agent is refused naming `name`: the runs of the coder are stored under it.
#[tokio::test]
async fn a_folder_for_another_agent_exits_78_naming_the_field() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = folder();
    edit_instructions(&agent, |text| {
        text.replacen("name: coder", "name: other", 1)
    });
    let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
    env.push((
        "ADAM_AGENT_DIR".into(),
        agent.path().to_string_lossy().into_owned(),
    ));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(
        err.contains("name: coder") && err.contains("`other`"),
        "{err}"
    );
}

/// The control plane serves the card of the folder (no model variables needed), and logs which
/// files it runs: the source, the path, the digest and the agent, then a warning for what the
/// loader found that does not stop it.
#[tokio::test]
async fn a_control_plane_serves_the_card_of_its_agent_folder() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let agent = folder();
    edit_instructions(&agent, |text| {
        text.replacen("  name: Coder", "  name: Cody", 1).replacen(
            "limits:",
            "favourite_colour: green\nlimits:",
            1,
        )
    });
    let mut env = role_env("control-plane", &db.url(), tmp.path());
    env.push((
        "ADAM_AGENT_DIR".into(),
        agent.path().to_string_lossy().into_owned(),
    ));
    let mut p = Proc::spawn(&env);
    let addr = p.ready().await;

    let (status, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200, "{card}");
    assert_eq!(common::json_of(&card)["name"], "Cody", "{card}");
    assert!(card.contains(PUBLIC_URL), "{card}");

    let line = p
        .stdout()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["fields"]["message"] == "agent files")
        .unwrap_or_else(|| panic!("no `agent files` line:\n{}", p.logs()));
    let fields = &line["fields"];
    assert_eq!(fields["source"], "folder", "{line}");
    assert_eq!(fields["agent"], "coder", "{line}");
    assert_eq!(fields["warnings"], 1, "{line}");
    assert!(
        fields["path"]
            .as_str()
            .unwrap()
            .contains(agent.path().to_str().unwrap()),
        "{line}"
    );
    assert!(
        fields["digest"].as_str().unwrap().starts_with("sha256:"),
        "{line}"
    );
    let out = p.stdout();
    assert!(
        out.contains("instructions.md") && out.contains("favourite_colour"),
        "the warning is logged as path and message:\n{out}"
    );

    p.sigterm().await;
    let status = p.exit_within(Duration::from_secs(15)).await;
    assert_eq!(status.code(), Some(0), "{}", p.logs());
    db.finish().await;
}

/// Without `ADAM_AGENT_DIR` the embedded copy is used, and says so.
#[tokio::test]
async fn without_an_agent_folder_the_embedded_copy_is_served_and_logged() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let mut env = role_env("control-plane", &db.url(), tmp.path());
    env.retain(|(k, _)| k != "ADAM_AGENT_DIR");
    let mut p = Proc::spawn(&env);
    p.ready().await;
    let line = p
        .stdout()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["fields"]["message"] == "agent files")
        .unwrap_or_else(|| panic!("no `agent files` line:\n{}", p.logs()));
    assert_eq!(line["fields"]["source"], "embedded", "{line}");
    assert_eq!(line["fields"]["warnings"], 0, "{line}");
    p.sigterm().await;
    p.exit_within(Duration::from_secs(15)).await;
    db.finish().await;
}

/// A worker whose folder disagrees with the code (here: no `max_check_cycles` var, which the
/// process supplies) fails at startup, naming the var, with exit 78: not in the middle of a run.
#[tokio::test]
async fn a_worker_whose_folder_cannot_be_assembled_exits_78_naming_the_problem() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let agent = folder();
    edit_instructions(&agent, |text| {
        text.replacen("  max_check_cycles: 3\n", "", 1)
            .replace("{{max_check_cycles}}", "three")
    });
    let mut env = role_env("worker", &db.url(), tmp.path());
    env.push((
        "ADAM_AGENT_DIR".into(),
        agent.path().to_string_lossy().into_owned(),
    ));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("assembling the coder agent"), "{err}");
    assert!(err.contains("max_check_cycles"), "{err}");
    db.finish().await;
}

/// `mcp.json` in the folder: a worker connects the servers it names at startup (with the token
/// from the environment in the header), serves, and stops on SIGTERM with exit code 0. A control
/// plane steps no run, so it connects none of them (the server it names is down and it starts).
#[tokio::test]
async fn a_worker_connects_the_mcp_servers_of_its_folder_and_a_control_plane_does_not() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let server = adam_mcp_testkit::TestHttpServer::start(Some("mcp-secret-token")).await;
    let agent = folder();
    write_mcp_json(&agent, &server.url());

    let mut env = role_env("worker", &db.url(), tmp.path());
    env.push((
        "ADAM_AGENT_DIR".into(),
        agent.path().to_string_lossy().into_owned(),
    ));
    env.push(("TEST_MCP_TOKEN".into(), "mcp-secret-token".into()));
    let mut p = Proc::spawn(&env);
    p.ready().await;
    assert!(
        server.initializations() >= 1,
        "the worker connected the server before it served:\n{}",
        p.logs()
    );
    assert!(
        server
            .authorizations()
            .iter()
            .all(|a| a == "Bearer mcp-secret-token"),
        "{:?}",
        server.authorizations()
    );
    assert!(
        !p.logs().contains("mcp-secret-token"),
        "the token is not logged:\n{}",
        p.logs()
    );
    p.sigterm().await;
    let status = p.exit_within(Duration::from_secs(15)).await;
    assert_eq!(status.code(), Some(0), "{}", p.logs());

    // The same folder, the server down, a control plane: it needs no tools, so it starts.
    let down = folder();
    write_mcp_json(&down, "http://127.0.0.1:1/mcp");
    let mut env = role_env("control-plane", &db.url(), tmp.path());
    env.push((
        "ADAM_AGENT_DIR".into(),
        down.path().to_string_lossy().into_owned(),
    ));
    let mut p = Proc::spawn(&env);
    p.ready().await;
    p.sigterm().await;
    let status = p.exit_within(Duration::from_secs(15)).await;
    assert_eq!(status.code(), Some(0), "{}", p.logs());
    db.finish().await;
}

/// What a worker cannot connect stops it at startup, with the exit code of the cause: a server
/// that is down is 69 (a supervisor retries), a local process the deployment does not allow, a
/// variable nobody set and a `${VAR}` in a URL are 78 (the files and the deployment disagree). The
/// failure line names the file and which variables decide, and never the value of a variable.
#[tokio::test]
async fn an_mcp_server_that_cannot_be_connected_stops_the_worker_with_the_code_of_the_cause() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let cases: [(&str, &str, u8, &str); 4] = [
        (
            r#"{"mcpServers": {"down": {"type": "http", "url": "http://127.0.0.1:1/mcp"}}}"#,
            "",
            69,
            "down",
        ),
        (
            r#"{"mcpServers": {"local": {"command": "adam-mcp-test-server"}}}"#,
            "",
            78,
            "MCP_ALLOW_STDIO",
        ),
        (
            r#"{"mcpServers": {"x": {"type": "http", "url": "http://127.0.0.1:1/mcp",
                "headers": {"Authorization": "Bearer ${TEST_MCP_TOKEN_NOBODY_SET}"}}}}"#,
            "",
            78,
            "TEST_MCP_TOKEN_NOBODY_SET",
        ),
        (
            r#"{"mcpServers": {"x": {"type": "http", "url": "${TEST_MCP_URL}"}}}"#,
            "http://127.0.0.1:1/mcp",
            78,
            "TEST_MCP_URL",
        ),
    ];
    for (mcp_json, url, code, wants) in cases {
        let agent = folder();
        std::fs::write(agent.path().join("agent/mcp.json"), mcp_json).unwrap();
        let mut env = role_env("worker", &db.url(), tmp.path());
        env.push((
            "ADAM_AGENT_DIR".into(),
            agent.path().to_string_lossy().into_owned(),
        ));
        if !url.is_empty() {
            env.push(("TEST_MCP_URL".into(), url.into()));
        }
        let mut p = Proc::spawn(&env);
        let status = p.exit_within(Duration::from_secs(60)).await;
        assert_eq!(
            status.code(),
            Some(i32::from(code)),
            "{wants}: {}",
            p.logs()
        );
        let err = failure(&p)["error"].as_str().unwrap().to_owned();
        assert!(err.contains("connecting the MCP servers"), "{err}");
        assert!(err.contains(wants), "{wants} missing from:\n{err}");
        assert!(
            err.contains("mcp.json"),
            "the failure names the file:\n{err}"
        );
        assert!(
            url.is_empty() || !err.contains(url),
            "the value of a variable is not in the failure:\n{err}"
        );
    }
    db.finish().await;
}

/// The shipped agent names the GitHub MCP server, a local process, so a worker on the embedded
/// copy stops at startup unless the deployment allows local processes (`MCP_ALLOW_STDIO`, which
/// the coder's deployment sets, not the image) and the binary is there: 78 when it is not allowed, 69 when it is allowed
/// and is not on `PATH` (a supervisor may retry: the image may be mid-roll). Never in the middle of
/// a run, and never with a value of a variable in the message.
#[tokio::test]
async fn the_embedded_agent_needs_mcp_allow_stdio_and_the_github_server_on_path() {
    let tmp = tempfile::tempdir().unwrap();
    // Failing before anything connects: Postgres need not exist.
    let mut env = role_env("worker", "postgres://u:p@127.0.0.1:1/x", tmp.path());
    env.retain(|(k, _)| k != "ADAM_AGENT_DIR");
    let empty_path = tmp.path().join("no-binaries");
    std::fs::create_dir_all(&empty_path).unwrap();
    env.push(("PATH".into(), empty_path.to_string_lossy().into_owned()));

    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("connecting the MCP servers"), "{err}");
    assert!(err.contains("MCP_ALLOW_STDIO"), "{err}");
    assert!(err.contains("github"), "the server is named:\n{err}");
    assert!(!err.contains(GITHUB_TOKEN), "{err}");

    env.push(("MCP_ALLOW_STDIO".into(), "true".into()));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(69), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("github"), "{err}");
    assert!(
        err.contains("github-mcp-server"),
        "the binary is named:\n{err}"
    );
    assert!(!err.contains(GITHUB_TOKEN), "{err}");
}

/// The shipped agent against the **real** `github-mcp-server` (the one the coder image carries,
/// `ADAM_TEST_GITHUB_MCP_SERVER` = the path of that binary, for example copied out of the image
/// with `docker cp`; skipped without it): a worker on the embedded copy connects it as a child
/// process, the model is offered the twelve read tools and no write tool, a call reaches GitHub
/// (a mock, through `GITHUB_MCP_HOST`) with the credentials of the mode the coder runs in, and no
/// credential is in the logs. Two modes, in one test because they share the binary: a token (the
/// server reads it as `GITHUB_PERSONAL_ACCESS_TOKEN`, and the `GITHUB_APP_*` variables the file
/// passes are empty) and a GitHub App (the file passes an **empty** `GITHUB_PERSONAL_ACCESS_TOKEN`:
/// the server counts it as unset, signs a JWT with the key file, trades it at the installation's
/// token endpoint and calls with the token it gets).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_embedded_agent_connects_the_real_github_mcp_server() {
    let Some(binary) = std::env::var_os("ADAM_TEST_GITHUB_MCP_SERVER")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
    else {
        eprintln!("skipping: ADAM_TEST_GITHUB_MCP_SERVER is not the path of a github-mcp-server");
        return;
    };
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    // The binary, named as the shipped `mcp.json` names it, on the `PATH` of the coder only.
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(&binary, bin.join("github-mcp-server")).unwrap();
    let search_path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let key = Arc::new(adam_workspace::testing::TestAppKey::generate());

    for app in [false, true] {
        let mode = if app { "App" } else { "token" };
        let github = MockServer::start().await;
        // What the server asks a classic token's scopes of at startup (`ghp_`), and the call below.
        Mock::given(method("HEAD"))
            .and(path("/api/v3/"))
            .respond_with(ResponseTemplate::new(200).insert_header("X-OAuth-Scopes", "repo"))
            .mount(&github)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v3/user"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"login": "octocat", "id": 1, "type": "User"})),
            )
            .mount(&github)
            .await;
        let minted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/v3/app/installations/67890/access_tokens"))
            .respond_with(MintToken {
                key: key.clone(),
                minted: minted.clone(),
            })
            .mount(&github)
            .await;

        let model = MockServer::start().await;
        let asked = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(Script {
                replies: vec![
                    tool_reply("g1", "github__get_me", json!({})),
                    text_reply("You are octocat."),
                ],
                asked,
            })
            .mount(&model)
            .await;

        let work = tmp.path().join(format!("work-{mode}"));
        let mut env = if app {
            app_env(&db.url(), &work, &key, tmp.path())
        } else {
            valid_env(&db.url(), &work)
        };
        // The embedded copy, with the deployment's two settings: the coder's `MCP_ALLOW_STDIO`, and
        // the mock as the GitHub host, which is plain http to this machine.
        env.retain(|(k, _)| k != "ADAM_AGENT_DIR");
        env.extend([
            ("MODEL_BASE_URL".to_owned(), model.uri()),
            ("PATH".to_owned(), search_path.clone()),
            ("MCP_ALLOW_STDIO".to_owned(), "true".to_owned()),
            ("GITHUB_MCP_HOST".to_owned(), github.uri()),
        ]);
        let mut coder = Proc::spawn(&env);
        let addr = coder.ready().await;
        let client = common::a2a_client(addr, A2A_TOKEN).await;
        let mut stream = client
            .send_streaming_message(&SendMessageRequest {
                message: Message::new(Role::User, vec![Part::text("Who am I on GitHub?")]),
                configuration: None,
                metadata: None,
                tenant: None,
            })
            .await
            .unwrap();
        let mut last = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while last != Some(TaskState::InputRequired) {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(Ok(StreamResponse::StatusUpdate(u)))) => last = Some(u.status.state),
                Ok(Some(Ok(StreamResponse::Task(t)))) => last = Some(t.status.state),
                Ok(Some(Ok(_))) => {}
                other => panic!(
                    "{mode}: the task did not wait for the person: {other:?}\n{}",
                    coder.logs()
                ),
            }
        }
        drop(stream);

        // The model was offered the twelve tools after the coder's own, and nothing that writes.
        let requests = model.received_requests().await.unwrap();
        let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let offered: Vec<&str> = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        let github_tools: Vec<&str> = offered
            .iter()
            .copied()
            .filter(|t| t.starts_with("github__"))
            .collect();
        assert_eq!(
            github_tools,
            [
                "github__get_me",
                "github__search_repositories",
                "github__get_file_contents",
                "github__list_branches",
                "github__list_commits",
                "github__get_commit",
                "github__search_code",
                "github__list_issues",
                "github__issue_read",
                "github__search_issues",
                "github__list_pull_requests",
                "github__pull_request_read",
            ],
            "{mode}: {offered:?}"
        );
        assert_eq!(
            offered[0], "prepare_workspace",
            "the coder's own come first: {offered:?}"
        );
        // The call reached the real server, and the answer is what GitHub (the mock) said.
        let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
        let answer = second["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(answer.contains("octocat"), "{mode}: {answer}");

        // The credentials the server called GitHub with are the coder's own, in the mode it runs in.
        let seen = github.received_requests().await.unwrap();
        let user: Vec<_> = seen
            .iter()
            .filter(|r| r.url.path() == "/api/v3/user")
            .collect();
        assert_eq!(user.len(), 1, "{mode}: {seen:?}");
        let bearer = user[0]
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_else(|| panic!("{mode}: no bearer: {seen:?}\n{}", coder.logs()));
        if app {
            assert_eq!(
                bearer,
                format!("Bearer {APP_TOKEN}"),
                "the token it traded for"
            );
            assert_eq!(minted.load(std::sync::atomic::Ordering::SeqCst), 1);
        } else {
            assert_eq!(bearer, format!("Bearer {GITHUB_TOKEN}"));
            assert_eq!(
                minted.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "no App, no trade"
            );
            // A classic token's scopes are asked of GitHub once, at startup, as the server does.
            assert!(seen.iter().any(|r| r.method.as_str() == "HEAD"), "{seen:?}");
        }

        coder.sigterm().await;
        let status = coder.exit_within(Duration::from_secs(30)).await;
        assert_eq!(status.code(), Some(0), "{mode}: {}", coder.logs());
        let visible = format!("{}{answer}", coder.logs());
        for secret in [GITHUB_TOKEN, APP_TOKEN] {
            assert!(!visible.contains(secret), "{mode}: {secret} is visible");
        }
    }
    db.finish().await;
}

/// `MCP_ALLOW_STDIO=true` is read by the worker roles (not validated by a control plane), and a
/// bad value is a configuration error naming the variable.
#[tokio::test]
async fn a_bad_mcp_flag_is_a_configuration_error_naming_the_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
    env.push(("MCP_ALLOW_STDIO".into(), "maybe".into()));
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(err.contains("MCP_ALLOW_STDIO"), "{err}");
    assert!(!err.contains("connecting to Postgres"), "{err}");
}

/// A control plane serves A2A (card, `/healthz`, 401 without a token) with no model, GitHub or
/// workspace configuration, and never touches the workspace root, even when one is set: it is not
/// created, because no run is stepped here.
#[tokio::test]
async fn a_control_plane_serves_a2a_and_does_not_create_the_workspace_root() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("never-created");
    let mut env = role_env("control-plane", &db.url(), &workspace);
    env.push((
        "WORKSPACE_ROOT".to_owned(),
        workspace.to_string_lossy().into_owned(),
    ));
    let mut p = Proc::spawn(&env);
    let addr = p.ready().await;
    let out = p.stdout();
    assert!(
        out.contains("\"worker\":None") || out.contains("worker: None"),
        "the control plane's configuration has no worker half:\n{out}"
    );

    let (status, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200, "{card}");
    assert!(card.contains(PUBLIC_URL), "{card}");
    let (status, _) = common::raw(addr, "POST", "/", None).await;
    assert_eq!(status, 401, "an unauthenticated call is refused");
    assert!(
        !workspace.exists(),
        "a control plane must not create the workspace root"
    );

    p.sigterm().await;
    let status = p.exit_within(Duration::from_secs(15)).await;
    assert_eq!(status.code(), Some(0), "{}", p.logs());
    assert!(!workspace.exists(), "still not created after the stop");
    db.finish().await;
}

/// `agent/mcp.json` of `agent`: one server `test` at `url`, which takes its token from
/// `${TEST_MCP_TOKEN}`.
fn write_mcp_json(agent: &tempfile::TempDir, url: &str) {
    std::fs::write(
        agent.path().join("agent/mcp.json"),
        format!(
            r#"{{"mcpServers": {{"test": {{"type": "http", "url": "{url}",
                "headers": {{"Authorization": "Bearer ${{TEST_MCP_TOKEN}}"}},
                "tools": ["echo"]}}}}}}"#
        ),
    )
    .unwrap();
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
            // The coder streams its model calls: a request that asks for a stream gets one.
            Some(reply) => chat_response(request, reply),
            None => ResponseTemplate::new(500).set_body_string("script exhausted"),
        }
    }
}

/// How many turns the model has been asked about so far.
fn turns(asked: &Mutex<Vec<usize>>) -> usize {
    asked.lock().unwrap().iter().max().map_or(0, |t| t + 1)
}

/// The slot of `https://github.com/octo/widgets` (the repository these tests name; the private git
/// config sends it to the local remote) in the workspace of `run`: `<root>/workspaces/<run>/widgets`.
fn widgets_slot(root: &Path, run: &str) -> PathBuf {
    root.join("workspaces").join(run).join("widgets")
}

/// A bare `remote.git` seeded with `main`, and a private `$HOME` whose git config points
/// `https://github.com/octo/widgets.git` at it (so the production repository policy applies while
/// the bytes stay local). Returns `(remote, home)`.
fn seed_remote(dir: &Path) -> (PathBuf, PathBuf) {
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
    (remote, home)
}

/// Mount the happy-path model on `model`: prepare, delegate, checks, commit and push, pull
/// request, done. Returns the turn of every request, in arrival order.
async fn mount_happy_model(model: &MockServer) -> Arc<Mutex<Vec<usize>>> {
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
        .mount(model)
        .await;
    asked
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
    let (remote, home) = seed_remote(dir);

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
    let asked = mount_happy_model(&model).await;
    let github = common::mock_github().await;

    let workspace = dir.join("work");
    let env_for = || {
        let mut env = valid_env(&db.url(), &workspace);
        env.extend([
            ("MODEL_BASE_URL".to_owned(), model.uri()),
            ("GITHUB_API_URL".to_owned(), github.uri()),
            ("HOME".to_owned(), home.to_string_lossy().into_owned()),
            // The operator's own global configuration (the coder ignores `$HOME/.gitconfig`).
            (
                "GIT_CONFIG_GLOBAL".to_owned(),
                home.join(".gitconfig").to_string_lossy().into_owned(),
            ),
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
    let worktree = widgets_slot(&workspace, &run.to_string());
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

/// The two halves as two processes over one database: a control plane (A2A, no `run_worker`) and
/// a worker (`run_worker`, `/healthz` only). The task is sent to the control plane **before** the
/// worker exists and waits, unclaimed. Then the worker starts, steps the run to a pull request,
/// and the control plane's stream reports it.
///
/// Both processes log `listening for notifications`. The worker's progress reaches the control
/// plane's stream **as events**, over Postgres `NOTIFY` (ADR 0001, "Cross-process events"): a
/// `working` update carrying the text of a `Progress` event exists only as a live event, so a
/// control plane that learned things by polling the run alone could never have streamed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_control_plane_and_a_worker_process_complete_a_task_over_one_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (remote, home) = seed_remote(dir);
    let model = MockServer::start().await;
    let asked = mount_happy_model(&model).await;
    let github = common::mock_github().await;

    let front_workspace = dir.join("front-work");
    let worker_workspace = dir.join("worker-work");
    let with_backends = |mut env: Vec<(String, String)>| {
        env.extend([
            ("MODEL_BASE_URL".to_owned(), model.uri()),
            ("GITHUB_API_URL".to_owned(), github.uri()),
            ("HOME".to_owned(), home.to_string_lossy().into_owned()),
            // The operator's own global configuration (the coder ignores `$HOME/.gitconfig`).
            (
                "GIT_CONFIG_GLOBAL".to_owned(),
                home.join(".gitconfig").to_string_lossy().into_owned(),
            ),
            (
                "OPENCODE_COMMAND".to_owned(),
                common::fake_agent().to_string_lossy().into_owned(),
            ),
            ("FAKE_ACP_SCENARIO".to_owned(), "write-file".to_owned()),
            ("FAKE_ACP_WRITE_PATH".to_owned(), "hello.txt".to_owned()),
            ("FAKE_ACP_WRITE_CONTENT".to_owned(), "hello\n".to_owned()),
        ]);
        env
    };

    // The control plane alone takes the task. It gets no model, GitHub or OpenCode configuration
    // at all (and a workspace root it must leave alone): starting a run needs none of it.
    let mut front_env = role_env("control-plane", &db.url(), &front_workspace);
    front_env.push((
        "WORKSPACE_ROOT".to_owned(),
        front_workspace.to_string_lossy().into_owned(),
    ));
    let mut front = Proc::spawn(&front_env);
    let front_addr = front.ready().await;
    front.notifying().await;
    let front_out = front.stdout();
    for absent in [
        "model_base_url",
        "github_api_url",
        &model.uri(),
        &github.uri(),
    ] {
        assert!(
            !front_out.contains(absent),
            "the control plane logged model or GitHub configuration ({absent}):\n{front_out}"
        );
    }
    let client = common::a2a_client(front_addr, A2A_TOKEN).await;
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
        panic!("the first event is the task\n{}", front.logs());
    };
    let run = RunId(task.id.parse().expect("task id is a run id"));

    // Nobody steps it: the control plane runs no worker.
    let store = db.store();
    let waiting = store.load_run(run).await.unwrap().expect("the run exists");
    assert_eq!(waiting.status, RunStatus::Runnable);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let still = store.load_run(run).await.unwrap().unwrap();
    assert_eq!(
        (still.status, still.version),
        (RunStatus::Runnable, waiting.version),
        "a control plane must not step runs: {still:?}\n{}",
        front.logs()
    );
    assert_eq!(turns(&asked), 0, "the model was not asked");

    // A worker joins, on its own workspace, without the front's variables.
    let mut worker = Proc::spawn(&with_backends(role_env(
        "worker",
        &db.url(),
        &worker_workspace,
    )));
    let worker_addr = worker.ready().await;
    worker.notifying().await;
    let (status, _) = common::raw(worker_addr, "GET", "/healthz", None).await;
    assert_eq!(status, 200);

    // The control plane's stream reports what the worker did.
    let mut labels: Vec<String> = Vec::new();
    let mut artifacts: Vec<String> = Vec::new();
    let mut progress: Vec<String> = Vec::new();
    let mut last = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while !last.as_ref().is_some_and(TaskState::is_terminal) {
        let item = match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(item))) => item,
            Ok(other) => panic!(
                "the stream ended before the task did: {other:?}, seen {labels:?}\n{}\n{}",
                front.logs(),
                worker.logs()
            ),
            Err(_) => panic!(
                "the task did not finish, seen {labels:?}\n{}\n{}",
                front.logs(),
                worker.logs()
            ),
        };
        match item {
            StreamResponse::StatusUpdate(u) => {
                labels.push(format!("status:{:?}", u.status.state));
                progress.extend(
                    u.status
                        .message
                        .as_ref()
                        .and_then(|m| m.text())
                        .map(str::to_owned),
                );
                last = Some(u.status.state);
            }
            StreamResponse::ArtifactUpdate(u) => {
                let name = u.artifact.name.unwrap_or_default();
                labels.push(format!("artifact:{name}"));
                artifacts.push(name);
            }
            StreamResponse::Task(t) => last = Some(t.status.state),
            StreamResponse::Message(_) => {}
        }
    }
    assert_eq!(last, Some(TaskState::Completed), "{labels:?} {progress:?}");
    assert!(
        progress
            .iter()
            .any(|m| m.contains("preparing a worktree of")),
        "the worker's progress events crossed processes: {progress:?}\n{}\n{}",
        front.logs(),
        worker.logs()
    );
    for name in ["checks", "branch", "pull_request"] {
        assert!(
            artifacts.iter().any(|a| a == name),
            "{name} missing from {labels:?}"
        );
    }
    drop(stream);
    drop(client);

    // The durable result, and who did the work.
    let done = store.load_run(run).await.unwrap().unwrap();
    assert_eq!(done.status, RunStatus::Done, "{done:?}");
    assert_eq!(turns(&asked), 6, "{:?}", asked.lock().unwrap());
    assert!(
        widgets_slot(&worker_workspace, &run.to_string())
            .join("hello.txt")
            .is_file(),
        "the worker's workspace holds the worktree"
    );
    assert!(
        !front_workspace.exists(),
        "the control plane never created a workspace root"
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
    let pulls = github
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .count();
    assert_eq!(pulls, 1, "exactly one pull request");

    // Both stop cleanly on SIGTERM.
    front.sigterm().await;
    worker.sigterm().await;
    for (name, proc) in [("control plane", &mut front), ("worker", &mut worker)] {
        let status = proc.exit_within(Duration::from_secs(30)).await;
        assert_eq!(status.code(), Some(0), "{name}\n{}", proc.logs());
        assert!(proc.stdout().contains("shutdown requested"), "{name}");
    }
    let all_logs = format!("{}{}", front.logs(), worker.logs());
    assert!(
        !all_logs.contains(GITHUB_TOKEN),
        "the token is never logged"
    );
    assert!(
        !all_logs.contains(A2A_TOKEN),
        "the bearer token is never logged"
    );
    db.finish().await;
}

// ------------------------------------------------------------------- the janitor

/// A run of the coder in the store with `status`, and a workspace of one slot for it (and for
/// `ids` the store does not know) under `workspace`.
async fn run_with_workspace(
    store: &adam_core::DynStore,
    workspaces: &adam_workspace::Workspaces,
    remote: &Path,
    status: Option<RunStatus>,
) -> RunId {
    let id = RunId::new();
    if let Some(status) = status {
        store
            .create_run(adam_core::NewRun {
                id,
                agent: "coder".to_owned(),
                conversation_id: None,
                parent_id: None,
                status,
                state: json!({}),
                wake_at: None,
            })
            .await
            .unwrap();
    }
    let repo = adam_workspace::RepoRef::new(remote.to_str().unwrap(), "main");
    workspaces
        .run(&id.to_string())
        .unwrap()
        .add_repository(&repo)
        .await
        .unwrap();
    id
}

/// The coder sweeps the workspaces of runs that are over, on its own, while it serves: a run that
/// is done or failed, and one the store does not know, lose their workspace; a parked run keeps it,
/// whatever it holds; a directory that is not a run is left alone; the notes stay.
#[tokio::test]
async fn the_janitor_removes_the_workspace_of_a_finished_run_and_keeps_an_open_one() {
    use adam_coder::tools::{NotesStore, RunNotes};
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (remote, _home) = seed_remote(tmp.path());
    let workspace = tmp.path().join("work");
    let workspaces = adam_workspace::Workspaces::new(
        workspace.clone(),
        Arc::new(adam_workspace::StaticToken::new("t")),
    );
    let store = db.store();
    let done = run_with_workspace(&store, &workspaces, &remote, Some(RunStatus::Done)).await;
    let failed = run_with_workspace(&store, &workspaces, &remote, Some(RunStatus::Failed)).await;
    let parked = run_with_workspace(&store, &workspaces, &remote, Some(RunStatus::Parked)).await;
    let unknown = run_with_workspace(&store, &workspaces, &remote, None).await;
    let notes = NotesStore::new(&workspace);
    notes
        .save(&done.to_string(), &RunNotes::default())
        .await
        .unwrap();
    std::fs::create_dir_all(workspace.join("workspaces/not-a-run")).unwrap();
    let dir = |run: RunId| workspace.join("workspaces").join(run.to_string());
    assert!(dir(done).is_dir() && dir(parked).is_dir());

    let mut env = valid_env(&db.url(), &workspace);
    env.push(("WORKSPACE_SWEEP_SECS".into(), "1".into()));
    let mut p = Proc::spawn(&env);
    p.ready().await;
    let deadline = Instant::now() + Duration::from_secs(60);
    while [done, failed, unknown].iter().any(|run| dir(*run).exists()) {
        assert!(
            Instant::now() < deadline,
            "the janitor never swept\n{}",
            p.logs()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(dir(parked).is_dir(), "a parked run keeps its workspace");
    assert!(
        workspace.join("workspaces/not-a-run").is_dir(),
        "what is not a run is not the janitor's"
    );
    assert!(
        workspace
            .join("coder")
            .join(format!("{done}.json"))
            .is_file(),
        "the notes stay"
    );
    assert!(
        workspace.join("git").is_dir(),
        "the mirrors stay: the run's branches are in them"
    );
    assert!(
        p.stdout()
            .contains("removed the workspace of a finished run"),
        "{}",
        p.logs()
    );
    // And it goes on: a run that finishes later loses its workspace at the next sweep.
    let record = store.load_run(parked).await.unwrap().unwrap();
    store
        .commit_run(
            parked,
            record.version,
            adam_core::RunUpdate::new(RunStatus::Done, record.state),
        )
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while dir(parked).exists() {
        assert!(
            Instant::now() < deadline,
            "the run finished and its workspace stayed\n{}",
            p.logs()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    p.sigterm().await;
    let status = p.exit_within(Duration::from_secs(15)).await;
    assert_eq!(
        status.code(),
        Some(0),
        "the janitor stops with the workers\n{}",
        p.logs()
    );
    db.finish().await;
}

/// `WORKSPACE_SWEEP_SECS=0` turns the janitor off: it says so and removes nothing.
#[tokio::test]
async fn a_sweep_of_zero_seconds_turns_the_janitor_off() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (remote, _home) = seed_remote(tmp.path());
    let workspace = tmp.path().join("work");
    let workspaces = adam_workspace::Workspaces::new(
        workspace.clone(),
        Arc::new(adam_workspace::StaticToken::new("t")),
    );
    let store = db.store();
    let done = run_with_workspace(&store, &workspaces, &remote, Some(RunStatus::Done)).await;

    let mut env = valid_env(&db.url(), &workspace);
    env.push(("WORKSPACE_SWEEP_SECS".into(), "0".into()));
    let mut p = Proc::spawn(&env);
    p.ready().await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !p
        .stdout()
        .contains("the sweep of finished workspaces is off")
    {
        assert!(
            Instant::now() < deadline,
            "it never said it is off\n{}",
            p.logs()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        workspace.join("workspaces").join(done.to_string()).is_dir(),
        "nothing was swept"
    );
    p.sigterm().await;
    assert_eq!(
        p.exit_within(Duration::from_secs(15)).await.code(),
        Some(0),
        "{}",
        p.logs()
    );
    db.finish().await;
}

// ------------------------------------------------------------------------------ devcontainers

/// A file that is a native executable as far as the configuration can tell (the ELF magic, and
/// executable): what `OPENCODE_BINARY` must be when the runtime is `podman`.
fn native_stub(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("opencode-native");
    std::fs::write(&path, b"\x7fELF this is only a stub").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

/// The devcontainer variables are checked before anything connects: a value that is not one, a
/// runtime with no service address, an OpenCode that cannot be mounted. Exit 78, with the variable
/// named.
#[tokio::test]
async fn a_bad_devcontainer_configuration_exits_78_naming_the_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("opencode.js");
    std::fs::write(&script, "#!/usr/bin/env node\n").unwrap();
    let cases: [(&str, Vec<(&str, String)>); 5] = [
        (
            "DEVCONTAINER_RUNTIME must be off or podman",
            vec![("DEVCONTAINER_RUNTIME", "docker".into())],
        ),
        (
            "DEVCONTAINER_NETWORK must be inherit or none",
            vec![("DEVCONTAINER_NETWORK", "host".into())],
        ),
        (
            "CONTAINER_HOST is required with DEVCONTAINER_RUNTIME=podman",
            vec![
                ("DEVCONTAINER_RUNTIME", "podman".into()),
                ("OPENCODE_BINARY", native_stub(tmp.path())),
            ],
        ),
        (
            "is not a native executable",
            vec![
                ("DEVCONTAINER_RUNTIME", "podman".into()),
                ("CONTAINER_HOST", "unix:///run/podman/podman.sock".into()),
                ("OPENCODE_BINARY", script.to_string_lossy().into_owned()),
            ],
        ),
        (
            "DEVCONTAINER_UP_TIMEOUT_SECS is invalid",
            vec![("DEVCONTAINER_UP_TIMEOUT_SECS", "later".into())],
        ),
    ];
    for (wants, vars) in cases {
        let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
        env.extend(vars.into_iter().map(|(k, v)| (k.to_owned(), v)));
        let mut p = Proc::spawn(&env);
        let status = p.exit_within(Duration::from_secs(30)).await;
        assert_eq!(status.code(), Some(78), "{wants}: {}", p.logs());
        let err = failure(&p)["error"].as_str().unwrap().to_owned();
        assert!(err.contains(wants), "{wants:?} missing from:\n{err}");
        assert!(
            !err.contains("connecting to Postgres"),
            "a bad configuration must stop before anything connects:\n{err}"
        );
    }
}

/// With devcontainers on and the Podman service not answering, the worker starts anyway: it writes
/// the tools every devcontainer would mount, says that the service does not answer, and stops
/// cleanly on SIGTERM. (A run then goes on in the coder's own container, with a step: the
/// environment's own tests.) The service may start after the coder.
#[tokio::test]
async fn a_worker_with_devcontainers_on_starts_when_the_service_does_not_answer() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("work");
    // A client that says the service is down, as it would for a socket that is not there.
    let client = tmp.path().join("podman-remote");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(
            &client,
            "#!/bin/sh\necho 'Cannot connect to Podman: connection refused' >&2\nexit 125\n",
        )
        .unwrap();
        std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut env = valid_env(&db.url(), &workspace);
    env.extend(
        [
            ("DEVCONTAINER_RUNTIME", "podman".to_owned()),
            (
                "CONTAINER_HOST",
                "unix:///nonexistent/podman.sock".to_owned(),
            ),
            ("DEVCONTAINER_PODMAN", client.to_string_lossy().into_owned()),
            ("DEVCONTAINER_PREPULL", "false".to_owned()),
            ("OPENCODE_BINARY", native_stub(tmp.path())),
        ]
        .map(|(k, v)| (k.to_owned(), v)),
    );
    let mut p = Proc::spawn(&env);
    p.ready().await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !p.stdout().contains("the Podman service does not answer") {
        assert!(
            Instant::now() < deadline,
            "it never said the service does not answer\n{}",
            p.logs()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let out = p.stdout();
    assert!(out.contains("devcontainers are on"), "{out}");
    assert!(
        !out.contains(A2A_TOKEN) && !out.contains(GITHUB_TOKEN),
        "nothing secret is logged: {out}"
    );
    let tools = workspace.join("environments/.tools");
    let written: Vec<_> = std::fs::read_dir(&tools)
        .expect("the tools directory is written at startup")
        .flatten()
        .collect();
    assert_eq!(written.len(), 1, "{written:?}");
    let dir = written[0].path();
    assert!(dir.join("adam-exec").is_file(), "{dir:?}");
    assert!(
        dir.join("opencode").is_file(),
        "the coder's OpenCode is in it: {dir:?}"
    );
    p.sigterm().await;
    assert_eq!(
        p.exit_within(Duration::from_secs(15)).await.code(),
        Some(0),
        "{}",
        p.logs()
    );
    db.finish().await;
}

// ------------------------------------------------------ GitHub credentials, per installation

/// `valid_env` for a GitHub App installation instead of a token: `GITHUB_TOKEN` is gone, and the key
/// is a file under `dir` (what a deployment mounts).
fn app_env(
    database_url: &str,
    workspace: &Path,
    key: &adam_workspace::testing::TestAppKey,
    dir: &Path,
) -> Vec<(String, String)> {
    let file = dir.join("github-app.pem");
    std::fs::write(&file, &key.pkcs1_pem).unwrap();
    let mut env = valid_env(database_url, workspace);
    env.retain(|(k, _)| k != "GITHUB_TOKEN");
    env.extend([
        ("GITHUB_APP_ID".to_owned(), "12345".to_owned()),
        ("GITHUB_APP_INSTALLATION_ID".to_owned(), "67890".to_owned()),
        (
            "GITHUB_APP_PRIVATE_KEY_PATH".to_owned(),
            file.to_string_lossy().into_owned(),
        ),
    ]);
    env
}

/// A token or a GitHub App, never both and never a part of an App: every problem at once (exit 78,
/// before anything connects), each naming its variable, none carrying a token or a key; and a
/// complete App configuration is accepted (the process gets as far as Postgres).
#[tokio::test]
async fn a_github_app_configuration_is_checked_at_startup_and_exits_78_with_every_problem() {
    let key = adam_workspace::testing::TestAppKey::generate();
    let tmp = tempfile::tempdir().unwrap();
    let unreachable = "postgres://u:p@127.0.0.1:1/x";
    let failed = |env: Vec<(String, String)>| {
        let mut p = Proc::spawn(&env);
        async move {
            let status = p.exit_within(Duration::from_secs(45)).await;
            (status.code(), p)
        }
    };

    // A complete App is accepted: the key was read and parsed, and the process goes on to Postgres.
    let (code, p) = failed(app_env(unreachable, tmp.path(), &key, tmp.path())).await;
    assert_eq!(code, Some(69), "{}", p.logs());
    let chain = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(chain.starts_with("connecting to Postgres: "), "{chain}");

    // A token and an App: refused, naming the variables and not their values.
    let mut env = app_env(unreachable, tmp.path(), &key, tmp.path());
    env.push(("GITHUB_TOKEN".to_owned(), GITHUB_TOKEN.to_owned()));
    let (code, p) = failed(env).await;
    assert_eq!(code, Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(
        err.contains("GITHUB_TOKEN and the GITHUB_APP_* variables are both set"),
        "{err}"
    );
    assert!(
        !err.contains(GITHUB_TOKEN) && !err.contains("BEGIN"),
        "{err}"
    );

    // Neither: both ways are named.
    let mut env = valid_env(unreachable, tmp.path());
    env.retain(|(k, _)| k != "GITHUB_TOKEN");
    let (code, p) = failed(env).await;
    assert_eq!(code, Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(
        err.contains("GITHUB_TOKEN is required") && err.contains("GITHUB_APP_ID"),
        "{err}"
    );

    // A part of an App, a key that is not RSA, a bad installation: all in one report.
    let ec = tmp.path().join("ec.pem");
    std::fs::write(
        &ec,
        "-----BEGIN EC PRIVATE KEY-----\nTOPSECRETBODYOFANECKEY\n-----END EC PRIVATE KEY-----\n",
    )
    .unwrap();
    let mut env = valid_env(unreachable, tmp.path());
    env.retain(|(k, _)| k != "GITHUB_TOKEN");
    env.extend([
        ("GITHUB_APP_ID".to_owned(), "12345".to_owned()),
        ("GITHUB_APP_INSTALLATION_ID".to_owned(), "none".to_owned()),
        (
            "GITHUB_APP_PRIVATE_KEY_PATH".to_owned(),
            ec.to_string_lossy().into_owned(),
        ),
        ("GITHUB_API_URL".to_owned(), "ftp://api.example".to_owned()),
    ]);
    let (code, p) = failed(env).await;
    assert_eq!(code, Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    for name in [
        "GITHUB_APP_INSTALLATION_ID must be a positive integer",
        "GITHUB_APP_PRIVATE_KEY_PATH: the GitHub App's private key is not usable",
        "GITHUB_API_URL",
    ] {
        assert!(err.contains(name), "{name} missing from:\n{err}");
    }
    assert!(
        !p.logs().contains("TOPSECRET") && !p.logs().contains("connecting to Postgres"),
        "the key is not logged, and nothing connected:\n{}",
        p.logs()
    );

    // A key file that is not there.
    let mut env = app_env(unreachable, tmp.path(), &key, tmp.path());
    for (k, v) in &mut env {
        if k == "GITHUB_APP_PRIVATE_KEY_PATH" {
            *v = tmp
                .path()
                .join("missing.pem")
                .to_string_lossy()
                .into_owned();
        }
    }
    let (code, p) = failed(env).await;
    assert_eq!(code, Some(78), "{}", p.logs());
    let err = failure(&p)["error"].as_str().unwrap().to_owned();
    assert!(
        err.contains("GITHUB_APP_PRIVATE_KEY_PATH") && err.contains("cannot be read"),
        "{err}"
    );
}

/// What GitHub does with the trade of a JWT for an installation token: checks the signature against
/// the App's public key, and gives the same token (good for years, so that the whole run uses one).
struct MintToken {
    key: Arc<adam_workspace::testing::TestAppKey>,
    minted: Arc<std::sync::atomic::AtomicUsize>,
}

impl Respond for MintToken {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let jwt = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default();
        match self.key.verify_jwt(jwt) {
            // The App's id, as a number (adam's own JWT) or as a string (github-mcp-server's): GitHub
            // takes either.
            Ok(claims) if claims["iss"] == 12345 || claims["iss"] == "12345" => {
                self.minted
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(201).set_body_json(json!({
                    "token": APP_TOKEN,
                    "expires_at": "2099-01-01T00:00:00Z",
                }))
            }
            _ => ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
        }
    }
}

/// The installation token the mock GitHub gives.
const APP_TOKEN: &str = "ghs_binaryAppInstallationToken0123456789";

/// The whole binary as a GitHub App installation: the key is read at startup, the coder trades a
/// JWT signed with it for an installation token the first time it needs one, **that token (and not
/// the JWT) is what the pull request calls carry**, one token serves the whole run, and neither the
/// token, the JWT nor the key is in a log line or on the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_github_app_installation_gets_its_token_minted_and_opens_the_pull_request() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let key = Arc::new(adam_workspace::testing::TestAppKey::generate());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let (remote, home) = seed_remote(dir);
    let model = MockServer::start().await;
    let asked = mount_happy_model(&model).await;
    let github = common::mock_github().await;
    let minted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/app/installations/67890/access_tokens"))
        .respond_with(MintToken {
            key: key.clone(),
            minted: minted.clone(),
        })
        .mount(&github)
        .await;

    let mut env = app_env(&db.url(), &dir.join("work"), &key, dir);
    env.extend([
        ("MODEL_BASE_URL".to_owned(), model.uri()),
        ("GITHUB_API_URL".to_owned(), github.uri()),
        ("HOME".to_owned(), home.to_string_lossy().into_owned()),
        (
            "GIT_CONFIG_GLOBAL".to_owned(),
            home.join(".gitconfig").to_string_lossy().into_owned(),
        ),
        (
            "OPENCODE_COMMAND".to_owned(),
            common::fake_agent().to_string_lossy().into_owned(),
        ),
        ("FAKE_ACP_SCENARIO".to_owned(), "write-file".to_owned()),
        ("FAKE_ACP_WRITE_PATH".to_owned(), "hello.txt".to_owned()),
        ("FAKE_ACP_WRITE_CONTENT".to_owned(), "hello\n".to_owned()),
    ]);
    let mut coder = Proc::spawn(&env);
    let addr = coder.ready().await;
    let client = common::a2a_client(addr, A2A_TOKEN).await;
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
    let mut seen = String::new();
    let mut last = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while !last.as_ref().is_some_and(TaskState::is_terminal) {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(item))) => {
                seen.push_str(&format!("{item:?}\n"));
                match item {
                    StreamResponse::StatusUpdate(u) => last = Some(u.status.state),
                    StreamResponse::Task(t) => last = Some(t.status.state),
                    _ => {}
                }
            }
            other => panic!("the task did not finish: {other:?}\n{}", coder.logs()),
        }
    }
    assert_eq!(last, Some(TaskState::Completed), "{seen}\n{}", coder.logs());
    drop(stream);

    assert_eq!(turns(&asked), 6, "{:?}", asked.lock().unwrap());
    let branches = common::git(
        &remote,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/agent",
        ],
    );
    assert_eq!(branches.lines().count(), 1, "{branches}");

    // One trade, and the token it made is what GitHub's REST API was given, every time.
    assert_eq!(
        minted.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one installation token for the whole run"
    );
    let calls: Vec<_> = github
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().starts_with("/repos/"))
        .collect();
    assert!(
        calls.len() >= 2,
        "the probe and the pull request: {calls:?}"
    );
    for call in &calls {
        assert_eq!(
            call.headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {APP_TOKEN}").as_str()),
            "{} {}",
            call.method,
            call.url
        );
    }
    assert_eq!(
        calls.iter().filter(|r| r.method.as_str() == "POST").count(),
        1,
        "one pull request"
    );

    coder.sigterm().await;
    let status = coder.exit_within(Duration::from_secs(30)).await;
    assert_eq!(status.code(), Some(0), "{}", coder.logs());
    // Nothing secret anywhere the process shows: not the installation token, not the key, in the
    // logs or on the stream.
    let body: String = key
        .pkcs1_pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect();
    let visible = format!("{}{seen}", coder.logs());
    for secret in [
        APP_TOKEN,
        body.as_str(),
        &body[..60],
        "BEGIN RSA PRIVATE KEY",
    ] {
        assert!(!visible.contains(secret), "{secret}");
    }
    db.finish().await;
}
