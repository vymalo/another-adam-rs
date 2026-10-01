//! The `adam-agent` binary as a process: the folder it must be given, the roles, an unreachable
//! Postgres, serving a chat persona over A2A, MCP servers and SIGTERM. Offline, except that the cases
//! which need a database use `ADAM_TEST_POSTGRES_URL` (and skip without it).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::net::SocketAddr;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use a2a::{Message, Part, Role, SendMessageRequest, StreamResponse, TaskState, TaskStatus};
use common::pg::TestDb;
use common::{
    SearchServer, assistant, chat, edit_instructions, greeting_for, researcher, text_reply,
    tool_reply,
};
use futures::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const PUBLIC_URL: &str = "http://agent.test:8080/";
const A2A_TOKEN: &str = "agent-binary-test-token";
const MCP_TOKEN: &str = "mcp-secret-token-31c9";

/// A running `adam-agent` and what it wrote.
struct Proc {
    child: Child,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    readers: Vec<tokio::task::JoinHandle<()>>,
}

fn pair(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_owned(), v.into())
}

/// The environment of a valid configuration for the folder `agent_dir`, over an empty environment
/// (plus `PATH` and `HOME`), so nothing of the developer's shell leaks in. The server binds port 0
/// (the operating system picks a free one, so parallel tests never collide) and logs the address it
/// got; [`Proc::ready`] reads it.
fn valid_env(database_url: &str, agent_dir: &std::path::Path) -> Vec<(String, String)> {
    vec![
        pair("DATABASE_URL", database_url),
        pair("MODEL_BASE_URL", "http://127.0.0.1:9/v1"),
        pair("MODEL_API_KEY", ""),
        pair("MODEL", "test-model"),
        pair("A2A_BEARER_TOKENS", A2A_TOKEN),
        pair("PUBLIC_URL", PUBLIC_URL),
        pair("LISTEN_ADDR", "127.0.0.1:0"),
        pair("ADAM_AGENT_DIR", agent_dir.to_string_lossy()),
    ]
}

/// [`valid_env`] for `role`, with only the variables the role reads: without the front's variables
/// when the role serves no A2A, and without the model variables when it runs no workers, so a test
/// proves that the process starts without them.
fn role_env(role: &str, database_url: &str, agent_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut env = valid_env(database_url, agent_dir);
    env.push(pair("ROLE", role));
    if role == "worker" {
        env.retain(|(k, _)| k != "A2A_BEARER_TOKENS" && k != "PUBLIC_URL");
    }
    if role == "control-plane" {
        env.retain(|(k, _)| !matches!(k.as_str(), "MODEL_BASE_URL" | "MODEL_API_KEY" | "MODEL"));
    }
    env
}

impl Proc {
    fn spawn(env: &[(String, String)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_adam-agent"));
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("adam-agent starts");
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

    /// The structured log line `message`, if there is one.
    fn line(&self, message: &str) -> Option<Value> {
        self.stdout().lines().find_map(|line| {
            let log: Value = serde_json::from_str(line).ok()?;
            (log["fields"]["message"] == message).then(|| log["fields"].clone())
        })
    }

    /// The address from the `listening` log line, once there is one.
    fn bound_addr(&self) -> Option<SocketAddr> {
        self.line("listening")?["addr"].as_str()?.parse().ok()
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

    /// Wait until the server logged its address and `/healthz` answers 200; returns the address.
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

    /// SIGTERM, and a clean exit (code 0) within the drain.
    async fn stop_cleanly(&mut self) {
        self.sigterm().await;
        let status = self.exit_within(Duration::from_secs(15)).await;
        assert_eq!(status.code(), Some(0), "{}", self.logs());
        assert!(!self.stderr().contains("panicked"), "{}", self.logs());
    }
}

/// The failure the process logged, as one structured line: the `fields` of the `adam-agent failed`
/// event on stdout (the JSON logger's stream). A failure is exactly one such line, and nothing goes
/// to stderr.
fn failure(p: &Proc) -> Value {
    let out = p.stdout();
    let lines: Vec<Value> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["fields"]["message"] == "adam-agent failed")
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

/// The process exits with `code` on its own and says why in its one failure line, which is
/// returned (the whole cause chain as text).
async fn refused(env: &[(String, String)], code: u8) -> String {
    let mut p = Proc::spawn(env);
    let status = p.exit_within(Duration::from_secs(60)).await;
    assert_eq!(status.code(), Some(i32::from(code)), "{}", p.logs());
    let fields = failure(&p);
    assert_eq!(fields["code"], code);
    fields["error"].as_str().unwrap().to_owned()
}

// ------------------------------------------------------------------- offline

/// `ADAM_AGENT_DIR` has no default and the binary has no embedded agent: without it no role starts
/// (exit 78), the message says what to set, and every other problem is listed with it, so one round
/// trip fixes them all.
#[tokio::test]
async fn without_an_agent_folder_the_process_refuses_to_start() {
    // Nothing set: every problem at once.
    let err = refused(&[], 78).await;
    assert!(err.contains("reading the configuration"), "{err}");
    for name in [
        "DATABASE_URL",
        "MODEL_BASE_URL",
        "MODEL_API_KEY",
        "MODEL",
        "PUBLIC_URL",
        "A2A_BEARER_TOKENS",
        "ADAM_AGENT_DIR is required",
    ] {
        assert!(err.contains(name), "{name} missing from:\n{err}");
    }

    // Everything else valid: the folder is what is missing, for every role.
    let tmp = tempfile::tempdir().unwrap();
    for role in ["all", "control-plane", "worker"] {
        let mut env = role_env(role, "postgres://u:p@127.0.0.1:1/x", tmp.path());
        env.retain(|(k, _)| k != "ADAM_AGENT_DIR");
        let err = refused(&env, 78).await;
        assert!(err.contains("ADAM_AGENT_DIR is required"), "{role}: {err}");
        assert!(
            !err.contains("connecting to Postgres"),
            "{role}: a bad configuration must stop before anything connects:\n{err}"
        );
    }

    // A folder that is not there, or is a file.
    let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
    env.retain(|(k, _)| k != "ADAM_AGENT_DIR");
    env.push(pair("ADAM_AGENT_DIR", "/nonexistent/agent"));
    let err = refused(&env, 78).await;
    assert!(err.contains("/nonexistent/agent"), "{err}");
    assert!(err.contains("is not a directory"), "{err}");
}

/// A mistake in the files stops every role before anything connects, with exit 78 and every
/// finding as `path:line: error: ...` in the one failure line.
#[tokio::test]
async fn a_folder_with_errors_exits_78_with_every_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let agent = assistant();
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
        let env = role_env(role, "postgres://u:p@127.0.0.1:1/x", agent.path());
        let err = refused(&env, 78).await;
        for file in ["broken", "worse"] {
            let at = format!("agent/subagents/{file}.md:");
            assert!(err.contains(&at), "{role}: {at} missing from {err}");
        }
        assert!(err.contains(": error: "), "{role}: {err}");
        assert!(
            err.contains("cannot read the agent folder"),
            "{role}: {err}"
        );
        assert!(
            !err.contains("connecting to Postgres"),
            "files are read before anything connects: {err}"
        );
    }
    drop(tmp);
}

/// One agent per process: a folder of several is refused naming them (exit 78), and so is a folder
/// whose card cannot be made.
#[tokio::test]
async fn a_folder_of_two_agents_or_without_a_description_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["one", "two"] {
        let dir = tmp.path().join(format!("agents/{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("instructions.md"),
            format!("---\nname: {name}\ndescription: An agent.\n---\nHi.\n"),
        )
        .unwrap();
    }
    let env = valid_env("postgres://u:p@127.0.0.1:1/x", tmp.path());
    let err = refused(&env, 78).await;
    assert!(err.contains("one") && err.contains("two"), "{err}");

    // No description: no card. Only the roles that serve it need one.
    let mute = common::folder_with("---\nname: mute\n---\nHello.\n");
    let env = role_env("control-plane", "postgres://u:p@127.0.0.1:1/x", mute.path());
    let err = refused(&env, 78).await;
    assert!(err.contains("building the agent card"), "{err}");
}

/// Postgres unreachable at boot: a clear error, exit code 69 (so a supervisor retries later), no
/// panic and no password in the output.
#[tokio::test]
async fn boot_fails_fast_when_postgres_is_unreachable() {
    let agent = assistant();
    let env = valid_env(
        "postgres://adam:s3cr3tpassw0rd@127.0.0.1:1/adam",
        agent.path(),
    );
    let mut p = Proc::spawn(&env);
    let status = p.exit_within(Duration::from_secs(60)).await;
    assert_eq!(status.code(), Some(69), "{}", p.logs());
    let fields = failure(&p);
    assert_eq!(fields["code"], 69);
    let chain = fields["error"].as_str().unwrap();
    assert!(chain.contains("connecting to Postgres"), "{chain}");
    let (out, err) = (p.stdout(), p.stderr());
    for text in [&out, &err] {
        assert!(!text.contains("s3cr3tpassw0rd"), "password leaked:\n{text}");
        assert!(!text.contains("panicked"), "{text}");
    }
    assert!(
        out.contains("starting adam-agent") && !out.contains("\"message\":\"listening\""),
        "it starts, logs its configuration, never serves:\n{out}"
    );
    assert!(
        p.line("agent files").is_some(),
        "the files were read first:\n{out}"
    );
}

/// A worker whose folder disagrees with the code (an unknown tool) fails at startup naming it,
/// with exit 78: not in the middle of a run. A flag the deployment gets wrong is a configuration
/// error naming the variable.
#[tokio::test]
async fn a_worker_whose_folder_cannot_be_assembled_exits_78_naming_the_problem() {
    let agent = assistant();
    edit_instructions(&agent, |t| {
        t.replacen("limits:", "tools: [ask_usr]\nlimits:", 1)
    });
    let env = role_env("worker", "postgres://u:p@127.0.0.1:1/x", agent.path());
    let err = refused(&env, 78).await;
    assert!(err.contains("assembling the agent"), "{err}");
    assert!(
        err.contains("ask_usr") && err.contains("did you mean `ask_user`"),
        "{err}"
    );

    let good = assistant();
    let mut env = valid_env("postgres://u:p@127.0.0.1:1/x", good.path());
    env.push(pair("MCP_ALLOW_STDIO", "maybe"));
    let err = refused(&env, 78).await;
    assert!(err.contains("MCP_ALLOW_STDIO"), "{err}");
    assert!(!err.contains("connecting to Postgres"), "{err}");
}

// ------------------------------------------------------------ with a database

/// The process serves the card of its folder (URL from `PUBLIC_URL`) and `/healthz`, refuses
/// unauthenticated calls, logs which files it runs, and stops on SIGTERM with exit code 0.
#[tokio::test]
async fn serves_the_card_of_its_folder_and_stops_cleanly_on_sigterm() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let agent = assistant();
    let mut p = Proc::spawn(&valid_env(&db.url(), agent.path()));
    let addr = p.ready().await;
    p.notifying().await;

    let (status, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200, "{card}");
    let card_json = common::json_of(&card);
    assert_eq!(card_json["name"], "Assistant", "{card}");
    assert_eq!(card_json["skills"][0]["id"], "conversation", "{card}");
    assert_eq!(card_json["version"], env!("CARGO_PKG_VERSION"), "{card}");
    assert!(card.contains(PUBLIC_URL), "{card}");
    let (status, _) = common::raw(addr, "POST", "/", None).await;
    assert_eq!(status, 401, "an unauthenticated call is refused");
    let (status, _) = common::raw(addr, "POST", "/", Some("wrong")).await;
    assert_eq!(status, 401, "a wrong token is refused");

    let files = p.line("agent files").expect("the `agent files` line");
    assert_eq!(files["source"], "folder", "{files}");
    assert_eq!(files["agent"], "assistant", "{files}");
    assert_eq!(files["warnings"], 0, "{files}");
    assert!(
        files["digest"].as_str().unwrap().starts_with("sha256:"),
        "{files}"
    );
    assert!(
        files["path"]
            .as_str()
            .unwrap()
            .contains(agent.path().to_str().unwrap()),
        "{files}"
    );
    let listening = p.line("listening").unwrap();
    assert_eq!(listening["role"], "all", "{listening}");

    p.stop_cleanly().await;
    let out = p.stdout();
    assert!(
        out.contains("shutdown requested") && out.contains("stopped"),
        "{out}"
    );
    db.finish().await;
}

/// Answers `POST /chat/completions` the way the `mock-assistant` mapping of `dev/wiremock` does: from
/// the two persona lines at the top of the system prompt it is sent. The requests it got are kept.
struct Persona {
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Respond for Persona {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        self.seen.lock().unwrap().push(body.clone());
        let system = body["messages"][0]["content"].as_str().unwrap_or_default();
        match greeting_for(system) {
            Some(greeting) => ResponseTemplate::new(200).set_body_json(text_reply(&greeting)),
            None => ResponseTemplate::new(500).set_body_string("no persona lines"),
        }
    }
}

async fn mount_persona_model() -> (MockServer, Arc<Mutex<Vec<Value>>>) {
    let model = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Persona { seen: seen.clone() })
        .mount(&model)
        .await;
    (model, seen)
}

/// What a streamed response says about the task, if anything: the state and the words of the status of
/// the snapshot the stream starts with, or of a status update.
fn status_of(event: &StreamResponse) -> Option<&TaskStatus> {
    match event {
        StreamResponse::Task(task) => Some(&task.status),
        StreamResponse::StatusUpdate(update) => Some(&update.status),
        _ => None,
    }
}

/// Send `text` to the agent at `addr` over A2A (the official client, streaming) and return the final
/// state of the task and the words of its last status.
///
/// The stream starts with a snapshot of the task, and that snapshot is the whole stream when the run
/// is already over by the time the subscription takes it (a worker in the same process answers a
/// scripted model in milliseconds, so on a loaded machine it can win the race): the state is read
/// from the snapshot as well as from the updates that follow it.
async fn talk(addr: SocketAddr, text: &str) -> (TaskState, String) {
    let client = common::a2a_client(addr, A2A_TOKEN).await;
    let mut stream = client
        .send_streaming_message(&SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text(text)]),
            configuration: None,
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    let mut state = None;
    let mut words = String::new();
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !state.as_ref().is_some_and(TaskState::is_terminal) {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(event))) => {
                if let Some(status) = status_of(&event) {
                    if let Some(text) = status.message.as_ref().and_then(|m| m.text()) {
                        words = text.to_owned();
                    }
                    state = Some(status.state.clone());
                }
                seen.push(event);
            }
            other => panic!("the stream ended before the task did: {other:?}\nseen: {seen:#?}"),
        }
    }
    (state.unwrap(), words)
}

/// What the point of the binary is, end to end: a chat folder, a model, a task over A2A, and an
/// answer in role. "hi" is greeted with the name and the one-sentence summary of the folder; a
/// process restarted on an edited copy of the folder says the edited words, with no build; and the
/// model was sent the folder's prompt both times. (The model is scripted by the prompt; what a live
/// model does with it is unverified.)
#[tokio::test]
async fn a_chat_folder_answers_in_role_over_a2a_and_an_edit_changes_it_after_a_restart() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (model, seen) = mount_persona_model().await;
    let env = |agent: &std::path::Path| {
        let mut env = valid_env(&db.url(), agent);
        env.retain(|(k, _)| k != "MODEL_BASE_URL");
        env.push(pair("MODEL_BASE_URL", model.uri()));
        env
    };

    let agent = chat();
    let mut p = Proc::spawn(&env(agent.path()));
    let addr = p.ready().await;
    let (state, said) = talk(addr, "hi").await;
    assert_eq!(state, TaskState::Completed, "{said}\n{}", p.logs());
    assert_eq!(said, "Hi! I'm Chat. I talk things through with you.");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one model turn: {seen:?}");
        assert_eq!(seen[0]["model"], "test-model");
        let system = seen[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with("Your name is Chat.\n"), "{system}");
        assert!(
            system.contains("You are Chat, a general-purpose assistant"),
            "{system}"
        );
        let tools: Vec<&str> = seen[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(tools, ["ask_user"]);
    }
    p.stop_cleanly().await;

    // The same database, the same agent name, the edited folder: the restart is the deploy.
    edit_instructions(&agent, |t| {
        t.replacen("display_name: Chat", "display_name: Cody", 1)
            .replacen("  name: Chat", "  name: Cody", 1)
            .replacen(
                "In one sentence: I talk things through with you.",
                "In one sentence: I only fix typos.",
                1,
            )
    });
    let mut p = Proc::spawn(&env(agent.path()));
    let addr = p.ready().await;
    let (_, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(common::json_of(&card)["name"], "Cody", "{card}");
    let (state, said) = talk(addr, "hello").await;
    assert_eq!(state, TaskState::Completed, "{said}\n{}", p.logs());
    assert_eq!(said, "Hi! I'm Cody. I only fix typos.");
    p.stop_cleanly().await;
    db.finish().await;
}

/// A control plane serves the card of the folder and starts runs with no model, no MCP flag and
/// nothing else a worker needs; a worker in another process steps the run, serves only `/healthz`,
/// and needs no front variable. They meet in the database.
#[tokio::test]
async fn a_control_plane_and_a_worker_process_complete_a_task_over_one_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (model, _seen) = mount_persona_model().await;
    let agent = chat();

    let mut front = Proc::spawn(&role_env("control-plane", &db.url(), agent.path()));
    let front_addr = front.ready().await;
    front.notifying().await;
    let front_out = front.stdout();
    assert!(
        !front_out.contains("model_base_url"),
        "the control plane logged model configuration:\n{front_out}"
    );
    let (_, card) = common::raw(front_addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(common::json_of(&card)["name"], "Chat", "{card}");

    let client = common::a2a_client(front_addr, A2A_TOKEN).await;
    let mut stream = client
        .send_streaming_message(&SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    let Some(Ok(StreamResponse::Task(task))) = stream.next().await else {
        panic!("the first event is the task\n{}", front.logs());
    };
    assert_eq!(task.status.state, TaskState::Submitted);

    // A worker joins, without the front's variables.
    let mut worker_env = role_env("worker", &db.url(), agent.path());
    worker_env.retain(|(k, _)| k != "MODEL_BASE_URL");
    worker_env.push(pair("MODEL_BASE_URL", model.uri()));
    let mut worker = Proc::spawn(&worker_env);
    let worker_addr = worker.ready().await;
    worker.notifying().await;
    for (m, p) in [("GET", "/.well-known/agent-card.json"), ("POST", "/")] {
        let (status, body) = common::raw(worker_addr, m, p, None).await;
        assert_eq!(status, 404, "a worker has no A2A: {m} {p}: {body}");
    }

    // The control plane's stream reports what the worker did.
    let mut last = None;
    let mut words = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !last.as_ref().is_some_and(TaskState::is_terminal) {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(event))) => {
                if let Some(status) = status_of(&event) {
                    if let Some(text) = status.message.as_ref().and_then(|m| m.text()) {
                        words = text.to_owned();
                    }
                    last = Some(status.state.clone());
                }
            }
            other => panic!(
                "the stream ended before the task did: {other:?}\n{}\n{}",
                front.logs(),
                worker.logs()
            ),
        }
    }
    assert_eq!(last, Some(TaskState::Completed));
    assert_eq!(words, "Hi! I'm Chat. I talk things through with you.");

    worker.stop_cleanly().await;
    front.stop_cleanly().await;
    db.finish().await;
}

/// `mcp.json` in the folder: a worker connects the servers it names at startup (with the token from
/// the environment in the header) before it serves, never logs the token, and stops on SIGTERM
/// with exit code 0. A control plane steps no run, so it connects none (the server it names is
/// down and it starts).
#[tokio::test]
async fn a_worker_connects_the_mcp_servers_of_its_folder_and_a_control_plane_does_not() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let server = adam_mcp_testkit::TestHttpServer::start(Some(MCP_TOKEN)).await;
    let agent = chat();
    write_mcp_json(&agent, &server.url());

    let mut env = role_env("worker", &db.url(), agent.path());
    env.push(pair("TEST_MCP_TOKEN", MCP_TOKEN));
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
            .all(|a| a == &format!("Bearer {MCP_TOKEN}")),
        "{:?}",
        server.authorizations()
    );
    assert!(
        !p.logs().contains(MCP_TOKEN),
        "the token is not logged:\n{}",
        p.logs()
    );
    p.stop_cleanly().await;

    let down = chat();
    write_mcp_json(&down, "http://127.0.0.1:1/mcp");
    let mut p = Proc::spawn(&role_env("control-plane", &db.url(), down.path()));
    p.ready().await;
    p.stop_cleanly().await;
    db.finish().await;
}

/// Answers `POST /chat/completions` like the stack's `mock-researcher` model: a request that holds no
/// tool result calls `search__web_search` with the person's words as the query, and one that holds the
/// result answers with the first link in it.
struct Researcher {
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Respond for Researcher {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        self.seen.lock().unwrap().push(body.clone());
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        let result = messages
            .iter()
            .rev()
            .find(|m| m["role"] == "tool")
            .and_then(|m| m["content"].as_str());
        let reply = match result {
            Some(result) => {
                let link = result
                    .split_whitespace()
                    .find(|w| w.starts_with("https://"))
                    .unwrap_or("none");
                text_reply(&format!(
                    "I searched the web for you. The best source I found is {link}."
                ))
            }
            None => {
                let query = messages
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or_default();
                tool_reply(
                    "researcher-call-1",
                    "search__web_search",
                    serde_json::json!({"query": query}),
                )
            }
        };
        ResponseTemplate::new(200).set_body_json(reply)
    }
}

/// A researcher end to end, as the stack runs it: the process connects a **stateless** web-search MCP
/// server at startup (the token from the environment, in the header), a task over A2A makes the model
/// call `search__web_search`, the results come back, and the task completes with the source the
/// model names. The researcher's own persona is served on its card.
#[tokio::test]
async fn a_researcher_answers_with_its_source_through_a_stateless_mcp_server() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let search = SearchServer::start(Some(MCP_TOKEN)).await;
    let agent = researcher(&search.url());
    let model = MockServer::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Researcher { seen: seen.clone() })
        .mount(&model)
        .await;

    let mut env = valid_env(&db.url(), agent.path());
    env.retain(|(k, _)| k != "MODEL_BASE_URL");
    env.push(pair("MODEL_BASE_URL", model.uri()));
    env.push(pair("SEARCH_TOKEN", MCP_TOKEN));
    let mut p = Proc::spawn(&env);
    let addr = p.ready().await;
    assert_eq!(
        search.initializations(),
        1,
        "connected at startup:\n{}",
        p.logs()
    );

    let (_, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    let card = common::json_of(&card);
    assert_eq!(card["name"], "Researcher");
    assert_eq!(card["skills"][0]["id"], "web-research");
    assert_eq!(
        card["skills"][0]["tags"],
        serde_json::json!(["search", "sources"])
    );

    let (state, said) = talk(addr, "rust async").await;
    assert_eq!(state, TaskState::Completed, "{said}\n{}", p.logs());
    assert_eq!(
        said,
        "I searched the web for you. The best source I found is https://example.org/mock-search/1."
    );
    assert_eq!(search.calls(), [serde_json::json!({"query": "rust async"})]);
    assert!(
        search
            .authorizations()
            .iter()
            .all(|a| a == &format!("Bearer {MCP_TOKEN}")),
        "{:?}",
        search.authorizations()
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "a turn for the call, a turn for the answer");
        let tools: Vec<&str> = seen[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(tools, ["ask_user", "search__web_search"]);
    }
    assert!(
        !p.logs().contains(MCP_TOKEN),
        "the token is not logged:\n{}",
        p.logs()
    );
    p.stop_cleanly().await;
    db.finish().await;
}

/// What a worker cannot connect stops it at startup with the exit code of the cause: a server that
/// is down is 69 (a supervisor retries), a local process the deployment does not allow, a variable
/// nobody set and a `${VAR}` in a URL are 78. The failure line names which variables decide, and
/// never the value of one.
#[tokio::test]
async fn an_mcp_server_that_cannot_be_connected_stops_the_worker_with_the_code_of_the_cause() {
    let Some(db) = TestDb::create().await else {
        return;
    };
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
        let agent = chat();
        std::fs::write(agent.path().join("agent/mcp.json"), mcp_json).unwrap();
        let mut env = role_env("worker", &db.url(), agent.path());
        if !url.is_empty() {
            env.push(pair("TEST_MCP_URL", url));
        }
        let err = refused(&env, code).await;
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

    // With `MCP_ALLOW_URL_VARS` the same `${VAR}` in a url is read, and the server is found.
    let server = adam_mcp_testkit::TestHttpServer::start(None).await;
    let agent = chat();
    std::fs::write(
        agent.path().join("agent/mcp.json"),
        r#"{"mcpServers": {"x": {"type": "http", "url": "${TEST_MCP_URL}", "tools": ["echo"]}}}"#,
    )
    .unwrap();
    let mut env = role_env("worker", &db.url(), agent.path());
    env.push(pair("TEST_MCP_URL", server.url()));
    env.push(pair("MCP_ALLOW_URL_VARS", "true"));
    let mut p = Proc::spawn(&env);
    p.ready().await;
    assert!(server.initializations() >= 1, "{}", p.logs());
    p.stop_cleanly().await;
    db.finish().await;
}

/// Writes `agent/mcp.json` for the folder `agent`: one server `test` at `url`, taking its token from
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
