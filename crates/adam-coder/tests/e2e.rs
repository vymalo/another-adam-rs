//! The coder end to end, offline: A2A client -> A2A server -> runtime -> the
//! coder agent (scripted `MockModel`) -> real worktrees over a local bare git
//! remote, the adam-acp fake agent for OpenCode, and a mock GitHub.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use a2a::{
    Message, Part, Role, SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskState,
};
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::auth::AuthInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use adam_a2a::{AuthConfig, BackendError, Caller, TaskBackend, TaskEvent};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::{AGENT_NAME, Coder, CoderAgent, RuntimeOptions, ToolEnv, coder_tools};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Conversation, DynTool, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{
    DynModel, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse, ToolSpec,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use adam_runtime::{BroadcastSink, RetryPolicy, RunView, Runtime};
use async_trait::async_trait;
use common::{Fixture, PR_URL, call, happy_script, pg, text_reply, tool_reply};
use futures::StreamExt;
use futures::stream::BoxStream;
use secrecy::SecretString;
use serde_json::json;
use tokio::sync::{Notify, oneshot};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

type Client = A2AClient<Box<dyn Transport>>;

const TOKEN: &str = common::A2A_TOKEN;

struct Server {
    coder: Coder,
    client: Client,
}

impl Server {
    /// Serve `coder` on a random port and connect the official A2A client.
    async fn start(coder: Coder) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/").parse().unwrap();
        let app = coder.router(
            &url,
            AuthConfig::BearerTokens(vec![SecretString::from(TOKEN)]),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let card = AgentCardResolver::new(None)
            .resolve(&format!("http://{addr}"))
            .await
            .unwrap();
        let client = A2AClientFactory::builder()
            .with_interceptor(Arc::new(AuthInterceptor::bearer(TOKEN)))
            .build()
            .create_from_card(&card)
            .await
            .unwrap();
        Self { coder, client }
    }
}

fn options() -> RuntimeOptions {
    RuntimeOptions {
        worker_id: None,
        claim_scope: adam_core::ClaimScope::Any,
        concurrency: 2,
        lease_ttl: Duration::from_secs(30),
        poll_interval: Duration::from_millis(20),
    }
}

fn coder_with(fx: &Fixture, mock: &Arc<MockModel>, store: DynStore) -> Coder {
    let model: DynModel = mock.clone();
    Coder::new(
        store,
        CoderAgent::new(model, "test-model", fx.env.clone()),
        &options(),
    )
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

fn spawn_worker(coder: &Coder) -> Worker {
    let (stop, rx) = oneshot::channel::<()>();
    let runtime = coder.runtime.clone();
    let handle = tokio::spawn(async move {
        let _ = runtime
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    Worker { stop, handle }
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(20), self.handle)
            .await
            .expect("worker stops in time")
            .expect("worker task");
    }
}

fn user(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

fn request(message: Message) -> SendMessageRequest {
    SendMessageRequest {
        message,
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

/// What a client sees on a stream, in order, as short labels; plus the texts
/// of status messages and the names of artifacts.
#[derive(Default, Debug)]
struct Seen {
    labels: Vec<String>,
    messages: Vec<String>,
    artifacts: Vec<(String, a2a::Artifact)>,
    task_id: String,
    last_state: Option<TaskState>,
}

impl Seen {
    fn record(&mut self, item: StreamResponse) {
        match item {
            StreamResponse::Task(t) => {
                self.task_id = t.id.clone();
                self.last_state = Some(t.status.state.clone());
                self.labels.push(format!("task:{:?}", t.status.state));
                self.push_message(t.status.message.as_ref());
            }
            StreamResponse::StatusUpdate(u) => {
                self.last_state = Some(u.status.state.clone());
                self.labels.push(format!("status:{:?}", u.status.state));
                self.push_message(u.status.message.as_ref());
            }
            StreamResponse::ArtifactUpdate(u) => {
                let name = u.artifact.name.clone().unwrap_or_default();
                self.labels.push(format!("artifact:{name}"));
                self.artifacts.push((name, u.artifact));
            }
            StreamResponse::Message(_) => self.labels.push("message".into()),
        }
    }

    fn push_message(&mut self, message: Option<&Message>) {
        if let Some(m) = message {
            for part in &m.parts {
                match &part.content {
                    a2a::PartContent::Text(t) => self.messages.push(t.clone()),
                    a2a::PartContent::Data(v) => self.messages.push(v.to_string()),
                    _ => {}
                }
            }
        }
    }

    fn position(&self, label: &str) -> usize {
        self.labels
            .iter()
            .position(|l| l == label)
            .unwrap_or_else(|| panic!("{label} missing from {:?}", self.labels))
    }

    fn saw_message(&self, needle: &str) -> bool {
        self.messages.iter().any(|m| m.contains(needle))
    }
}

async fn wait_for(
    rt: &adam_runtime::Runtime,
    run: RunId,
    what: &str,
    pred: impl Fn(&RunView) -> bool,
) -> RunView {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let view = rt.view(run).await.unwrap().expect("run exists");
        if pred(&view) {
            return view;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn run_id(task_id: &str) -> RunId {
    RunId(task_id.parse().expect("task id is a run id"))
}

/// A store for one test, and what it takes to throw it away.
struct Backing {
    store: DynStore,
    db: Option<pg::TestDb>,
}

impl Backing {
    fn store(&self) -> DynStore {
        self.store.clone()
    }

    async fn finish(self) {
        drop(self.store);
        if let Some(db) = self.db {
            db.finish().await;
        }
    }
}

async fn memory_backing() -> Option<Backing> {
    Some(Backing {
        store: Arc::new(MemoryStore::new()),
        db: None,
    })
}

/// A database of its own: the coder's runs are claimed by agent name, so
/// tests must not share tables.
async fn postgres_backing() -> Option<Backing> {
    let db = pg::TestDb::create().await?;
    Some(Backing {
        store: db.store(),
        db: Some(db),
    })
}

// ------------------------------------------------------------------- happy path

async fn add_hello_txt_streams_working_progress_checks_artifact_completed(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    happy_script(&mock, &fx.remote_url());
    let server = Server::start(coder_with(&fx, &mock, store)).await;

    let mut stream = server
        .client
        .send_streaming_message(&request(user(&format!(
            "In {} (base branch main) add hello.txt containing hello",
            fx.remote_url()
        ))))
        .await
        .unwrap();
    let mut seen = Seen::default();
    // The snapshot arrives once the subscription is attached; only then start
    // working, so the live progress events cannot be missed.
    seen.record(stream.next().await.expect("snapshot").unwrap());
    let worker = spawn_worker(&server.coder);
    while let Some(item) = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .expect("the stream ends")
    {
        seen.record(item.unwrap());
    }
    worker.stop().await;

    // working -> progress (OpenCode's updates) -> checks -> artifacts -> completed
    assert_eq!(
        seen.last_state,
        Some(TaskState::Completed),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.labels.contains(&"status:Working".to_owned()),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.saw_message("preparing a worktree"),
        "{:#?}",
        seen.messages
    );
    assert!(
        seen.saw_message("opencode: edit: Write"),
        "OpenCode's tool call is streamed as progress: {:#?}",
        seen.messages
    );
    assert!(
        seen.saw_message("opencode: tool call tc-1 completed"),
        "{:#?}",
        seen.messages
    );
    assert!(
        seen.saw_message("running checks: test -f hello.txt"),
        "{:#?}",
        seen.messages
    );
    assert!(seen.saw_message("checks passed"), "{:#?}", seen.messages);
    assert!(seen.saw_message("pushing agent/"), "{:#?}", seen.messages);
    let opencode = seen
        .messages
        .iter()
        .position(|m| m.contains("opencode:"))
        .unwrap();
    let checks = seen
        .messages
        .iter()
        .position(|m| m.contains("running checks"))
        .unwrap();
    assert!(
        opencode < checks,
        "OpenCode runs before the checks: {:#?}",
        seen.messages
    );
    // checks (of HEAD, from run_checks), checks (bound to the pushed commit), branch, pull request
    let artifact_labels: Vec<_> = seen
        .labels
        .iter()
        .filter(|l| l.starts_with("artifact:"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        artifact_labels,
        [
            "artifact:checks",
            "artifact:checks",
            "artifact:branch",
            "artifact:pull_request"
        ],
        "{:?}",
        seen.labels
    );
    assert!(seen.position("artifact:branch") < seen.position("artifact:pull_request"));
    assert!(seen.position("artifact:pull_request") < seen.position("status:Completed"));
    assert_eq!(
        seen.labels.last().map(String::as_str),
        Some("status:Completed")
    );
    assert!(
        seen.saw_message("Opened the pull request."),
        "{:#?}",
        seen.messages
    );

    // The PR artifact carries the URL the mock GitHub returned.
    let (_, pr) = seen
        .artifacts
        .iter()
        .find(|(n, _)| n == "pull_request")
        .unwrap();
    let a2a::PartContent::Data(data) = &pr.parts[0].content else {
        panic!("data part expected")
    };
    assert_eq!(data["url"], PR_URL);
    assert_eq!(data["number"], "7", "numbers travel as strings over A2A");
    // And the same URL as a link a chat UI can show: an A2A `url` part after the data part.
    assert_eq!(pr.parts.len(), 2, "{:?}", pr.parts);
    assert_eq!(
        pr.parts[1].content,
        a2a::PartContent::Url(PR_URL.to_owned())
    );
    assert_eq!(
        serde_json::to_value(&pr.parts[1]).unwrap(),
        json!({"url": PR_URL})
    );
    // The branch artifact has no URL of its own: it stays one data part.
    let (_, branch) = seen.artifacts.iter().find(|(n, _)| n == "branch").unwrap();
    assert_eq!(branch.parts.len(), 1, "{:?}", branch.parts);

    // The checks artifact: passed, on the commit the worktree was at when they ran (the base of
    // the run's branch: the change was still uncommitted), one data part, no findings.
    let (_, checks) = seen.artifacts.iter().find(|(n, _)| n == "checks").unwrap();
    assert_eq!(checks.parts.len(), 1, "{:?}", checks.parts);
    let a2a::PartContent::Data(data) = &checks.parts[0].content else {
        panic!("data part expected")
    };
    assert_eq!(data["passed"], true, "{data}");
    assert_eq!(data["tree"].as_str().unwrap().len(), 40, "{data}");
    let commit = data["commit"].as_str().unwrap();
    assert_eq!(commit.len(), 40, "{data}");
    assert_eq!(
        commit,
        common::git(&fx.remote, &["rev-parse", "main"]),
        "{data}"
    );
    assert!(
        data["summary"]
            .as_str()
            .unwrap()
            .contains("test -f hello.txt")
    );
    assert!(data.get("findings").is_none(), "{data}");

    // The branch is on the remote with the file, in one commit.
    let branches = fx.agent_branches();
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert_eq!(fx.file_on(&branches[0], "hello.txt"), "hello");
    assert_eq!(fx.commits_ahead(&branches[0]), 1);

    // The second checks artifact is bound to the pushed commit: the SHA on the remote branch, and
    // the tree of that commit, which is the tree the check ran on.
    let pushed = common::git(&fx.remote, &["rev-parse", &branches[0]]);
    let bound: Vec<_> = seen
        .artifacts
        .iter()
        .filter(|(n, _)| n == "checks")
        .collect();
    let a2a::PartContent::Data(first) = &bound[0].1.parts[0].content else {
        panic!("data part expected")
    };
    let a2a::PartContent::Data(second) = &bound[1].1.parts[0].content else {
        panic!("data part expected")
    };
    assert_eq!(second["passed"], true, "{second}");
    assert_eq!(second["commit"], pushed.as_str(), "{second}");
    assert_eq!(
        second["tree"],
        common::git(&fx.remote, &["rev-parse", &format!("{pushed}^{{tree}}")]).as_str()
    );
    assert_eq!(
        second["tree"], first["tree"],
        "the tree that was checked is the tree that was pushed"
    );
    assert_ne!(second["commit"], first["commit"]);
    assert_ne!(bound[0].1.artifact_id, bound[1].1.artifact_id);
    assert_eq!(
        common::git(
            &fx.remote,
            &["log", "-1", "--format=%an <%ae>|%s", &branches[0]]
        ),
        "adam-coder <adam-coder@users.noreply.github.com>|feat: add hello.txt"
    );

    // The PR call hit the mock GitHub, once, with the right shape.
    let pulls = fx.created_pulls().await;
    assert_eq!(pulls.len(), 1, "{pulls:?}");
    assert_eq!(pulls[0]["head"], branches[0].as_str());
    assert_eq!(pulls[0]["base"], "main");
    assert_eq!(pulls[0]["title"], "feat: add hello.txt");
    assert_eq!(pulls[0]["draft"], false);
    assert!(pulls[0]["body"].as_str().unwrap().contains("Verification"));

    // The task, fetched again, is complete with both artifacts.
    let done = server
        .client
        .get_task(&a2a::GetTaskRequest {
            id: seen.task_id.clone(),
            history_length: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(done.status.state, TaskState::Completed);
    let names: Vec<_> = done
        .artifacts
        .unwrap()
        .iter()
        .filter_map(|a| a.name.clone())
        .collect();
    assert_eq!(names, ["checks", "checks", "branch", "pull_request"]);

    // The model saw the checks output as a tool result, and the instructions.
    let requests = mock.requests();
    assert_eq!(requests.len(), 6);
    let system = requests[0].system.clone().unwrap();
    assert!(system.contains("at most 3 times"), "{system}");
    let names: Vec<_> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "prepare_workspace",
            "delegate_to_opencode",
            "run_checks",
            "commit_and_push",
            "open_pull_request",
            "ask_user"
        ]
    );
}

// ------------------------------------------------------------------ input-required

async fn ask_user_parks_and_an_a2a_follow_up_resumes(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "q1",
        "ask_user",
        json!({"question": "Which base branch should I use?"}),
    )])
    .push_text("Understood: main.");
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);

    let mut stream = server
        .client
        .send_streaming_message(&request(user(&format!(
            "add hello.txt in {}",
            fx.remote_url()
        ))))
        .await
        .unwrap();
    let mut seen = Seen::default();
    while let Some(item) = tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("the stream ends when the task waits")
    {
        seen.record(item.unwrap());
    }
    assert_eq!(
        seen.last_state,
        Some(TaskState::InputRequired),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.saw_message("Which base branch should I use?"),
        "the question is the status message: {:#?}",
        seen.messages
    );

    // The run is parked with no timer: really waiting, not polling the model.
    let run = run_id(&seen.task_id);
    let view = server.coder.runtime.view(run).await.unwrap().unwrap();
    assert!(view.waiting);
    assert_eq!(mock.requests().len(), 1);

    // The follow-up resumes it; the answer becomes the tool result.
    let mut follow = user("use main");
    follow.task_id = Some(seen.task_id.clone());
    let response = server.client.send_message(&request(follow)).await.unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("a task expected")
    };
    worker.stop().await;
    // The model's last reply is text and nothing was delivered, so it is a question again.
    assert_eq!(task.status.state, TaskState::InputRequired);
    assert_eq!(
        task.status.message.as_ref().and_then(|m| m.text()),
        Some("Understood: main.")
    );
    let second = mock.requests()[1].clone();
    assert!(
        format!("{:?}", second.messages).contains("use main"),
        "{:?}",
        second.messages
    );
}

// ----------------------------------------------- a stop with nothing delivered

/// The owner's report: the person typed "Hi", the model answered in plain text without calling
/// `ask_user`. That text is a question, not a completion: the task is `input-required` with the
/// text as the question, the run waits with no timer, and the answer resumes it (the model sees
/// it, names the repository, and the run goes on to a pull request).
async fn a_plain_text_stop_is_a_question_and_the_answer_resumes_the_run(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    let greeting =
        "Hi! I'm ready to help. I need: 1. the repository 2. the base branch 3. the task.";
    mock.push_text(greeting);
    happy_script(&mock, &fx.remote_url());
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, "Hi").await;
    assert_eq!(
        seen.last_state,
        Some(TaskState::InputRequired),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.saw_message(greeting),
        "the model's text is the question: {:#?}",
        seen.messages
    );
    assert!(
        !seen.labels.iter().any(|l| l.contains("Completed")),
        "{:?}",
        seen.labels
    );
    let run = run_id(&seen.task_id);
    let view = server.coder.runtime.view(run).await.unwrap().unwrap();
    assert!(view.waiting, "parked with no timer");
    assert_eq!(view.status, RunStatus::Parked);
    assert_eq!(mock.requests().len(), 1, "the model is not asked again");
    assert!(fx.created_pulls().await.is_empty());

    // The answer resumes the run; it reaches the model; the run goes on.
    let answer = format!(
        "In {} (base branch main) add hello.txt containing hello",
        fx.remote_url()
    );
    let mut follow = user(&answer);
    follow.task_id = Some(seen.task_id.clone());
    let response = server.client.send_message(&request(follow)).await.unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("a task expected")
    };
    assert_ne!(task.status.state, TaskState::Failed, "resumed");
    let done = wait_for(&server.coder.runtime, run, "the run to finish", |v| {
        v.status.is_terminal()
    })
    .await;
    worker.stop().await;
    assert_eq!(done.status, RunStatus::Done, "{:?}", done.error);

    let second = mock.requests()[1].clone();
    let shape: Vec<String> = second
        .messages
        .iter()
        .map(|m| match m {
            adam_model::Message::User { .. } => format!("user:{}", m.text()),
            adam_model::Message::Assistant { tool_calls, .. } => format!(
                "assistant:{}:{}",
                m.text(),
                tool_calls
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            adam_model::Message::Tool { content, .. } => format!("tool:{content}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "user:Hi".to_owned(),
            format!("assistant:{greeting}:ask_user"),
            format!("tool:{answer}"),
        ],
        "the model sees its own stop as the question it was, then the answer"
    );
    assert_eq!(fx.created_pulls().await.len(), 1);
    assert_eq!(fx.agent_branches().len(), 1);
}

/// A model that stops with no text at all still parks the run, with a fixed question.
async fn a_stop_without_text_asks_what_to_do(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_text("  \n ");
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);
    let seen = run_to_end(&server, "Hi").await;
    worker.stop().await;
    assert_eq!(
        seen.last_state,
        Some(TaskState::InputRequired),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.saw_message("Which repository should I work on"),
        "{:#?}",
        seen.messages
    );
}

/// The shape of the second report: the model ran a check on the repository (green, here a
/// `git status`), opened no pull request and ended with text. Nothing was delivered: the task
/// waits for the person instead of completing.
async fn checks_without_a_pull_request_then_text_is_a_question(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "c1",
        "prepare_workspace",
        json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
    )])
    .push_tool_calls(vec![call(
        "c2",
        "run_checks",
        json!({"command": "git status && git diff --stat"}),
    )])
    .push_text("Nothing to change: the worktree is clean. Should I do anything else?");
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);
    let seen = run_to_end(&server, &format!("Check {} and report", fx.remote_url())).await;
    worker.stop().await;
    assert_eq!(
        seen.last_state,
        Some(TaskState::InputRequired),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.saw_message("Should I do anything else?"),
        "{:#?}",
        seen.messages
    );
    assert!(
        !seen.labels.iter().any(|l| l == "artifact:pull_request"),
        "{:?}",
        seen.labels
    );
    assert!(fx.created_pulls().await.is_empty());
}

/// The weak model invents a repository when it was given none. The tool refuses, telling it to
/// ask; the refusal is a tool result, not a failure; nothing is fetched or created; and once the
/// model asks, the person's answer is what names the repository.
async fn an_invented_repository_is_refused_and_the_model_must_ask(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "c1",
        "prepare_workspace",
        json!({"repo_url": "https://github.com/rust-lang/rust-clippy", "base_branch": "master"}),
    )])
    .push_tool_calls(vec![call(
        "c2",
        "ask_user",
        json!({"question": "Which repository should I work on?"}),
    )]);
    happy_script(&mock, &fx.remote_url());
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, "Hi").await;
    assert_eq!(
        seen.last_state,
        Some(TaskState::InputRequired),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.saw_message("Which repository should I work on?"),
        "{:#?}",
        seen.messages
    );
    let refusal = tool_results(&mock.requests()[1].messages);
    assert_eq!(refusal.len(), 1, "{refusal:?}");
    let (id, text, is_error) = &refusal[0];
    assert_eq!(id, "c1");
    assert!(*is_error && text.contains("ask_user"), "{text}");
    assert!(
        !fx.root.join("git").exists() && !fx.root.join("worktrees").exists(),
        "nothing was fetched or created for the invented repository"
    );

    // The person names the repository; now the tool works on it.
    let mut follow = user(&fx.remote_url());
    follow.task_id = Some(seen.task_id.clone());
    server.client.send_message(&request(follow)).await.unwrap();
    let run = run_id(&seen.task_id);
    let done = wait_for(&server.coder.runtime, run, "the run to finish", |v| {
        v.status.is_terminal()
    })
    .await;
    worker.stop().await;
    assert_eq!(done.status, RunStatus::Done, "{:?}", done.error);
    assert_eq!(fx.created_pulls().await.len(), 1);
}

/// What the orchestrator's rework looks like at the coder when the run is parked: a message in
/// the same context, without a task id, is delivered to the open task (the runtime allows one
/// open run per conversation), so it continues the same conversation and a repository named in
/// the original request still counts, although the rework text does not repeat it.
async fn a_message_in_the_context_of_a_parked_run_continues_it(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_text("Which base branch should I use?");
    happy_script(&mock, &fx.remote_url());
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);
    let owner = Caller::new("token-0");
    let first = server
        .coder
        .backend
        .submit(
            owner.clone(),
            user(&format!(
                "In {} add hello.txt containing hello",
                fx.remote_url()
            )),
            None,
            Some("ctx-1".into()),
        )
        .await
        .unwrap();
    let run = run_id(&first.id);
    wait_for(&server.coder.runtime, run, "the run to wait", |v| v.waiting).await;

    // Does not name the repository again, and carries no task id.
    let again = server
        .coder
        .backend
        .submit(owner.clone(), user("use main"), None, Some("ctx-1".into()))
        .await
        .unwrap();
    assert_eq!(
        again.id, first.id,
        "delivered to the open task, not a new one"
    );
    let done = wait_for(&server.coder.runtime, run, "the run to finish", |v| {
        v.status.is_terminal()
    })
    .await;
    worker.stop().await;
    assert_eq!(done.status, RunStatus::Done, "{:?}", done.error);
    assert_eq!(fx.created_pulls().await.len(), 1);
}

/// CancelTask on a run parked by a plain-text stop: the task ends `canceled`, like any waiting
/// task, and nothing is delivered.
async fn a_run_parked_by_a_plain_text_stop_can_be_canceled(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_text("What should I do?");
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);
    let seen = run_to_end(&server, "Hi").await;
    assert_eq!(
        seen.last_state,
        Some(TaskState::InputRequired),
        "{:?}",
        seen.labels
    );
    let canceled = server
        .client
        .cancel_task(&a2a::CancelTaskRequest {
            id: seen.task_id.clone(),
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    worker.stop().await;
    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert_eq!(mock.requests().len(), 1);
    assert!(fx.created_pulls().await.is_empty());
}

// ------------------------------------------------------------------- red checks

/// Failing checks `MAX_CHECK_CYCLES` times end the run `failed`, with the
/// findings, and no pull request; the tools hold the line even when the model
/// keeps going.
async fn red_checks_n_times_fail_the_run_with_the_findings_and_no_pr(store: DynStore) {
    let fx = Fixture::with("hello\n", |s| s.max_check_cycles = 2).await;
    let mock = Arc::new(MockModel::new());
    let failing = "echo 'assertion failed: hello.txt is not enough'; exit 1";
    mock.push_tool_calls(vec![call(
        "c1",
        "prepare_workspace",
        json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
    )])
    .push_tool_calls(vec![call(
        "c2",
        "delegate_to_opencode",
        json!({"instructions": "add hello.txt"}),
    )])
    .push_tool_calls(vec![call("c3", "run_checks", json!({"command": failing}))])
    .push_tool_calls(vec![call(
        "c4",
        "delegate_to_opencode",
        json!({"instructions": "fix the failure"}),
    )])
    .push_tool_calls(vec![call("c5", "run_checks", json!({"command": failing}))])
    // The model ignores the limit and tries everything it is told not to.
    .push_tool_calls(vec![
        call("c6", "run_checks", json!({"command": "true"})),
        call(
            "c7",
            "commit_and_push",
            json!({"message": "feat: add hello.txt"}),
        ),
        call(
            "c8",
            "open_pull_request",
            json!({"title": "t", "body": "b", "accept_red_checks": true}),
        ),
    ])
    .push_text("I could not get the checks to pass.");
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);

    let mut stream = server
        .client
        .send_streaming_message(&request(user(&format!(
            "add hello.txt in {}",
            fx.remote_url()
        ))))
        .await
        .unwrap();
    let mut seen = Seen::default();
    while let Some(item) = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .expect("the stream ends")
    {
        seen.record(item.unwrap());
    }
    worker.stop().await;

    assert_eq!(
        seen.last_state,
        Some(TaskState::Failed),
        "{:?}",
        seen.labels
    );
    assert!(
        seen.saw_message("assertion failed: hello.txt is not enough"),
        "the findings are the error: {:#?}",
        seen.messages
    );
    assert!(
        seen.saw_message("no pull request was opened"),
        "{:#?}",
        seen.messages
    );
    assert!(
        !seen.labels.iter().any(|l| l == "artifact:pull_request"),
        "{:?}",
        seen.labels
    );

    // Nothing was pushed, nothing was opened.
    assert!(fx.agent_branches().is_empty(), "{:?}", fx.agent_branches());
    assert!(fx.created_pulls().await.is_empty());

    // What the model was told at each step.
    let results = tool_results(&mock.requests().last().unwrap().messages);
    let by_id = |id: &str| {
        results
            .iter()
            .find(|(c, _, _)| c == id)
            .unwrap_or_else(|| panic!("{id}: {results:?}"))
            .clone()
    };
    let (_, first, first_err) = by_id("c3");
    assert!(
        first_err && first.contains("failed check run 1 of 2"),
        "{first}"
    );
    let (_, second, second_err) = by_id("c5");
    assert!(
        second_err && second.contains("Check-cycle limit reached (2 of 2)"),
        "{second}"
    );
    for id in ["c6", "c7", "c8"] {
        let (_, text, is_err) = by_id(id);
        assert!(
            is_err && text.contains("Check-cycle limit reached"),
            "{id}: {text}"
        );
    }
    let (_, refused, _) = by_id("c6");
    assert!(
        refused.contains("assertion failed: hello.txt is not enough"),
        "the findings are repeated: {refused}"
    );
}

fn tool_results(messages: &[adam_model::Message]) -> Vec<(String, String, bool)> {
    messages
        .iter()
        .filter_map(|m| match m {
            adam_model::Message::Tool {
                call_id,
                content,
                is_error,
            } => Some((call_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// The explicit-acceptance path: a red check does not block a pull request the
/// user accepted, and the pull request says so.
async fn a_pull_request_with_red_checks_needs_explicit_acceptance(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "c1",
        "prepare_workspace",
        json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
    )])
    .push_tool_calls(vec![call(
        "c2",
        "delegate_to_opencode",
        json!({"instructions": "add hello.txt"}),
    )])
    .push_tool_calls(vec![call(
        "c3",
        "run_checks",
        json!({"command": "echo flaky; exit 1"}),
    )])
    .push_tool_calls(vec![call(
        "c4",
        "commit_and_push",
        json!({"message": "feat: add hello.txt"}),
    )])
    .push_tool_calls(vec![call(
        "c5",
        "open_pull_request",
        json!({"title": "feat: hello", "body": "b"}),
    )])
    .push_tool_calls(vec![call(
        "c6",
        "ask_user",
        json!({"question": "Checks are red. Open the PR anyway?"}),
    )])
    .push_tool_calls(vec![call(
        "c7",
        "open_pull_request",
        json!({"title": "feat: hello", "body": "b", "accept_red_checks": true}),
    )])
    .push_text("Opened it, as you accepted red checks.");
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);

    let mut stream = server
        .client
        .send_streaming_message(&request(user(&format!(
            "add hello.txt in {}",
            fx.remote_url()
        ))))
        .await
        .unwrap();
    let mut seen = Seen::default();
    while let Some(item) = stream.next().await {
        seen.record(item.unwrap());
    }
    assert_eq!(
        seen.last_state,
        Some(TaskState::InputRequired),
        "{:?}",
        seen.labels
    );
    assert!(
        fx.created_pulls().await.is_empty(),
        "refused before the user answered"
    );

    let mut follow = user("yes, open it anyway");
    follow.task_id = Some(seen.task_id.clone());
    let SendMessageResponse::Task(task): SendMessageResponse =
        server.client.send_message(&request(follow)).await.unwrap()
    else {
        panic!("a task expected")
    };
    worker.stop().await;
    assert_eq!(task.status.state, TaskState::Completed);

    let pulls = fx.created_pulls().await;
    assert_eq!(pulls.len(), 1);
    assert!(
        pulls[0]["body"]
            .as_str()
            .unwrap()
            .contains("checks were not green"),
        "{pulls:?}"
    );
    let results = tool_results(&mock.requests().last().unwrap().messages);
    let refused = results.iter().find(|(c, _, _)| c == "c5").unwrap();
    assert!(
        refused.2 && refused.1.contains("Refusing to open a pull request"),
        "{refused:?}"
    );
}

// ------------------------------------------------------------------ crash safety

/// Wraps a tool: once, after the inner call did its side effect but before
/// the result reaches the journal, the "worker dies" (the call hangs; the test
/// aborts the worker).
struct HangAfter {
    inner: DynTool,
    armed: Arc<AtomicBool>,
    reached: Arc<Notify>,
}

#[async_trait]
impl Tool for HangAfter {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    async fn call(&self, ctx: &ToolCtx, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let out = self.inner.call(ctx, args).await;
        if self.armed.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        out
    }
}

/// Wraps a model: the `hang_on`-th call (0-based) hangs the first time.
struct HangingModel {
    inner: Arc<MockModel>,
    hang_on: usize,
    calls: AtomicUsize,
    armed: Arc<AtomicBool>,
    reached: Arc<Notify>,
}

#[async_trait]
impl ModelClient for HangingModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let n = self.calls.fetch_add(1, SeqCst);
        if n == self.hang_on && self.armed.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.inner.stream(req).await
    }
}

enum CrashPoint {
    /// After `run_checks` ran the check, before its result (and its `checks` artifact) was
    /// journaled, so the whole call runs again on takeover.
    InsideRunChecks,
    /// After `commit_and_push` pushed, before its result was journaled.
    InsideCommitAndPush,
    /// After `commit_and_push` was journaled and committed, at the next model call.
    AfterCommitAndPush,
    /// After `open_pull_request` created the PR, before its result was journaled.
    InsideOpenPullRequest,
    /// After OpenCode finished and edited files, before `delegate_to_opencode`'s
    /// result was journaled (so the whole call runs again on takeover).
    InsideDelegate,
}

fn short_lease() -> RuntimeOptions {
    RuntimeOptions {
        lease_ttl: Duration::from_millis(400),
        ..options()
    }
}

/// Two workers (two runtimes over one store) drive one run; the first dies at
/// `point`; the second takes over and the run completes with exactly one
/// commit, one branch update and one pull request.
async fn crash_at(point: CrashPoint, store: DynStore) {
    // OpenCode is launched through a script that logs every launch, so the
    // delegate case can tell a rerun from a replay.
    let agent_dir = tempfile::tempdir().unwrap();
    let (scripted, launch_log) = common::scripted_agent(
        agent_dir.path(),
        "exec \"$AGENT\"",
        OpenCodeLaunch::program("unused")
            .env("FAKE_ACP_SCENARIO", "write-file")
            .env("FAKE_ACP_WRITE_PATH", "hello.txt")
            .env("FAKE_ACP_WRITE_CONTENT", "hello\n"),
    );
    let fx = Fixture::with("hello\n", |s| s.opencode = scripted).await;
    let mock = Arc::new(MockModel::new());
    happy_script(&mock, &fx.remote_url());
    let reached = Arc::new(Notify::new());
    let armed = Arc::new(AtomicBool::new(true));

    let (model, wrap): (DynModel, Option<&'static str>) = match point {
        CrashPoint::InsideRunChecks => (mock.clone(), Some("run_checks")),
        CrashPoint::InsideCommitAndPush => (mock.clone(), Some("commit_and_push")),
        CrashPoint::InsideOpenPullRequest => (mock.clone(), Some("open_pull_request")),
        CrashPoint::InsideDelegate => (mock.clone(), Some("delegate_to_opencode")),
        CrashPoint::AfterCommitAndPush => (
            Arc::new(HangingModel {
                inner: mock.clone(),
                hang_on: 4,
                calls: AtomicUsize::new(0),
                armed: armed.clone(),
                reached: reached.clone(),
            }),
            None,
        ),
    };
    let tools = |env: &Arc<ToolEnv>| -> Vec<DynTool> {
        coder_tools(env)
            .into_iter()
            .map(|tool| -> DynTool {
                if Some(tool.spec().name.as_str()) == wrap {
                    Arc::new(HangAfter {
                        inner: tool,
                        armed: armed.clone(),
                        reached: reached.clone(),
                    })
                } else {
                    tool
                }
            })
            .collect()
    };
    let doomed_agent = CoderAgent::with_tools(model.clone(), "m", fx.env.clone(), tools(&fx.env));
    let survivor_model: DynModel = mock.clone();
    // The survivor has plain tools (the wrapper is disarmed by then anyway).
    let survivor_agent = CoderAgent::new(survivor_model, "m", fx.env.clone());
    let opts = |id: &str| RuntimeOptions {
        worker_id: Some(id.to_owned()),
        ..short_lease()
    };
    let doomed = Coder::new(store.clone(), doomed_agent, &opts("crash-a"));
    let survivor = Coder::new(store.clone(), survivor_agent, &opts("crash-b"));

    let owner = Caller::new("token-0");
    let task = doomed
        .backend
        .submit(
            owner.clone(),
            user(&format!("add hello.txt in {}", fx.remote_url())),
            None,
            None,
        )
        .await
        .unwrap();
    let run = run_id(&task.id);

    let a = spawn_worker(&doomed);
    tokio::time::timeout(Duration::from_secs(60), reached.notified())
        .await
        .expect("the crash point is reached");
    // Kill worker A: the run is left leased, mid-flight.
    a.handle.abort();
    assert!(a.handle.await.expect_err("aborted").is_cancelled());
    let view = doomed.runtime.view(run).await.unwrap().unwrap();
    assert_eq!(view.status, RunStatus::Runnable, "not finished: {view:#?}");
    if matches!(point, CrashPoint::InsideDelegate) {
        // OpenCode ran once and the run's worktree holds what it did; add an
        // edit of "an earlier attempt" that only survives if the takeover
        // reuses the same worktree instead of starting over.
        assert_eq!(common::launches(&launch_log), 1);
        let worktree = fx.root.join("worktrees").join(&task.id);
        assert_eq!(
            std::fs::read_to_string(worktree.join("hello.txt")).unwrap(),
            "hello\n",
            "OpenCode's edit is in the worktree before the takeover"
        );
        std::fs::write(worktree.join("wip.txt"), "earlier edit\n").unwrap();
    }

    // A subscription on the survivor's replica follows the run to the end.
    let mut events = survivor.backend.subscribe(&owner, &task.id);
    let first = events.next().await.expect("snapshot").unwrap();
    assert!(matches!(first, TaskEvent::Snapshot(_)));
    let b = spawn_worker(&survivor);
    let view = wait_for(&survivor.runtime, run, "the run to finish", |v| {
        v.status.is_terminal()
    })
    .await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    let mut last = None;
    while let Some(Ok(event)) = tokio::time::timeout(Duration::from_secs(30), events.next())
        .await
        .expect("the subscription ends")
    {
        last = Some(event);
    }
    b.stop().await;
    match last {
        Some(TaskEvent::Status(u)) => assert_eq!(u.status.state, TaskState::Completed),
        other => panic!("expected the completed status last, got {other:?}"),
    }

    // Exactly one commit, one update of the branch, one pull request.
    let branches = fx.agent_branches();
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert_eq!(fx.commits_ahead(&branches[0]), 1, "no duplicate commit");
    assert_eq!(fx.ref_updates(&branches[0]), 1, "no duplicate push");
    assert_eq!(
        fx.created_pulls().await.len(),
        1,
        "no duplicate pull request"
    );
    assert_eq!(fx.file_on(&branches[0], "hello.txt"), "hello");
    if matches!(point, CrashPoint::InsideDelegate) {
        assert_eq!(
            common::launches(&launch_log),
            2,
            "the interrupted call ran again, once"
        );
        assert_eq!(
            fx.file_on(&branches[0], "wip.txt"),
            "earlier edit",
            "the takeover kept working in the same worktree"
        );
    }

    // The durable record has both artifacts and the URL.
    let task = survivor
        .backend
        .get(&owner, &task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.status.state, TaskState::Completed);
    let names: Vec<_> = task
        .artifacts
        .clone()
        .unwrap()
        .iter()
        .filter_map(|a| a.name.clone())
        .collect();
    assert_eq!(names, ["checks", "checks", "branch", "pull_request"]);
    let conversation: Conversation = serde_json::from_value(view.state).unwrap();
    assert!(
        conversation
            .artifacts
            .iter()
            .any(|a| a.name == "pull_request")
    );
    // Whatever the crash: one checks artifact per commit. The first names the commit the
    // worktree was at when the check ran; the second is bound to the pushed commit, once.
    let checks: Vec<_> = task
        .artifacts
        .unwrap()
        .into_iter()
        .filter(|a| a.name.as_deref() == Some("checks"))
        .collect();
    assert_eq!(checks.len(), 2, "no duplicate checks artifact: {checks:?}");
    let data: Vec<_> = checks
        .iter()
        .map(|a| match &a.parts[0].content {
            a2a::PartContent::Data(d) => d.clone(),
            other => panic!("data part expected, got {other:?}"),
        })
        .collect();
    assert_eq!(data[0]["passed"], true, "{data:?}");
    assert_eq!(
        data[0]["commit"],
        common::git(&fx.remote, &["rev-parse", "main"]).as_str()
    );
    let pushed = common::git(&fx.remote, &["rev-parse", &branches[0]]);
    assert_eq!(data[1]["passed"], true, "{data:?}");
    assert_eq!(
        data[1]["commit"],
        pushed.as_str(),
        "bound to the pushed commit"
    );
    assert_eq!(data[1]["tree"], data[0]["tree"]);
}

async fn crash_after_commit_and_push_was_journaled_repeats_nothing(store: DynStore) {
    crash_at(CrashPoint::AfterCommitAndPush, store).await;
}

/// The worker dies after the check ran, before the result reached the journal. The takeover runs
/// the check again and reports it once: one `checks` artifact, not two under different ids.
async fn crash_inside_run_checks_before_the_journal_emits_one_checks_artifact(store: DynStore) {
    crash_at(CrashPoint::InsideRunChecks, store).await;
}

async fn crash_inside_commit_and_push_before_the_journal_is_idempotent(store: DynStore) {
    crash_at(CrashPoint::InsideCommitAndPush, store).await;
}

async fn crash_inside_open_pull_request_before_the_journal_is_idempotent(store: DynStore) {
    crash_at(CrashPoint::InsideOpenPullRequest, store).await;
}

/// The crash the plan calls E6: the worker dies while OpenCode's turn is
/// done but not journaled. The takeover reruns `delegate_to_opencode` in the
/// same worktree (earlier edits kept), and the run still delivers one commit,
/// one push and one pull request.
async fn crash_during_delegate_to_opencode_reruns_on_the_same_worktree(store: DynStore) {
    crash_at(CrashPoint::InsideDelegate, store).await;
}

// ------------------------------------------------------------ OpenCode failures

/// A coder whose runtime retries transient failures quickly (the default
/// backoff is 1, 2, 4, 8 seconds).
fn coder_retrying(fx: &Fixture, model: DynModel, store: DynStore, max_attempts: u32) -> Coder {
    let events = BroadcastSink::default();
    let opts = options();
    let runtime = Runtime::builder(store)
        .agent(CoderAgent::new(model, "test-model", fx.env.clone()))
        .event_sink(events.clone())
        .concurrency(opts.concurrency)
        .lease_ttl(opts.lease_ttl)
        .poll_interval(opts.poll_interval)
        .retry(RetryPolicy {
            max_attempts,
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(100),
            multiplier: 2.0,
        })
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, AGENT_NAME)
        .with_poll_interval(opts.poll_interval);
    Coder { runtime, backend }
}

/// Send `text` and follow the stream until the task stops.
async fn run_to_end(server: &Server, text: &str) -> Seen {
    let mut stream = server
        .client
        .send_streaming_message(&request(user(text)))
        .await
        .unwrap();
    let mut seen = Seen::default();
    while let Some(item) = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .expect("the stream ends")
    {
        seen.record(item.unwrap());
    }
    seen
}

fn prepare_and_delegate(mock: &MockModel, fx: &Fixture, delegate_ids: &[&str]) {
    mock.push_tool_calls(vec![call(
        "c1",
        "prepare_workspace",
        json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
    )]);
    for id in delegate_ids {
        mock.push_tool_calls(vec![call(
            id,
            "delegate_to_opencode",
            json!({"instructions": "add hello.txt containing hello"}),
        )]);
    }
}

/// OpenCode dies on every launch: the tool error is transient, the runtime
/// retries within its budget and then fails the run with what happened,
/// including the child's stderr. Nothing is pushed or opened.
async fn opencode_crashing_every_time_fails_the_run_with_its_stderr(store: DynStore) {
    let fx = Fixture::with("hello\n", |s| {
        s.opencode =
            OpenCodeLaunch::program(common::fake_agent()).env("FAKE_ACP_SCENARIO", "crash");
    })
    .await;
    let mock = Arc::new(MockModel::new());
    // The retried turn asks the model again: one delegate call per attempt.
    prepare_and_delegate(&mock, &fx, &["c2", "c2b"]);
    let coder = coder_retrying(&fx, mock.clone(), store, 2);
    let server = Server::start(coder).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, &format!("add hello.txt in {}", fx.remote_url())).await;
    worker.stop().await;

    assert_eq!(
        seen.last_state,
        Some(TaskState::Failed),
        "{:?}",
        seen.labels
    );
    let transient = |seen: &Seen| {
        seen.messages
            .iter()
            .filter(|m| m.contains("delegate_to_opencode") && m.contains("transient_error"))
            .count()
    };
    assert_eq!(
        transient(&seen),
        2,
        "the client sees both attempts fail: {:#?}",
        seen.messages
    );
    let error = seen
        .messages
        .iter()
        .find(|m| m.contains("gave up after 2 attempts"))
        .unwrap_or_else(|| panic!("no failure message in {:#?}", seen.messages));
    assert!(error.contains("OpenCode: ACP agent exited"), "{error}");
    assert!(
        error.contains("simulated crash"),
        "the child's stderr explains it: {error}"
    );
    assert!(fx.agent_branches().is_empty(), "nothing was pushed");
    assert!(fx.created_pulls().await.is_empty(), "no pull request");
    assert_eq!(
        mock.requests().len(),
        3,
        "prepare, then one turn per attempt"
    );
}

/// OpenCode dies once: the retry launches it again in the same worktree and
/// the run delivers as if nothing had happened.
async fn opencode_crashing_once_is_retried_and_completes(store: DynStore) {
    let agent_dir = tempfile::tempdir().unwrap();
    let marker = agent_dir.path().join("crashed-once");
    let (launch, log) = common::scripted_agent(
        agent_dir.path(),
        "if [ ! -e \"$MARKER\" ]; then : > \"$MARKER\"; FAKE_ACP_SCENARIO=crash exec \"$AGENT\"; fi\nexec \"$AGENT\"",
        OpenCodeLaunch::program("unused")
            .env("FAKE_ACP_SCENARIO", "write-file")
            .env("FAKE_ACP_WRITE_PATH", "hello.txt")
            .env("FAKE_ACP_WRITE_CONTENT", "hello\n")
            .env("MARKER", marker.to_string_lossy()),
    );
    let fx = Fixture::with("hello\n", |s| s.opencode = launch).await;
    let mock = Arc::new(MockModel::new());
    prepare_and_delegate(&mock, &fx, &["c2", "c2b"]);
    mock.push_tool_calls(vec![call(
        "c3",
        "run_checks",
        json!({"command": "test -f hello.txt && cat hello.txt"}),
    )])
    .push_tool_calls(vec![call(
        "c4",
        "commit_and_push",
        json!({"message": "feat: add hello.txt"}),
    )])
    .push_tool_calls(vec![call(
        "c5",
        "open_pull_request",
        json!({"title": "feat: add hello.txt", "body": "Adds hello.txt.\n\n## Verification\n- `test -f hello.txt`: passed"}),
    )])
    .push_text("Opened the pull request.");
    let server = Server::start(coder_retrying(&fx, mock.clone(), store, 2)).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, &format!("add hello.txt in {}", fx.remote_url())).await;
    worker.stop().await;

    assert_eq!(
        seen.last_state,
        Some(TaskState::Completed),
        "{:?} {:#?}",
        seen.labels,
        seen.messages
    );
    assert_eq!(
        seen.messages
            .iter()
            .filter(|m| m.contains("delegate_to_opencode") && m.contains("transient_error"))
            .count(),
        1,
        "the client sees the first attempt fail: {:#?}",
        seen.messages
    );
    assert_eq!(common::launches(&log), 2, "one crash, one good launch");
    let branches = fx.agent_branches();
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert_eq!(fx.file_on(&branches[0], "hello.txt"), "hello");
    assert_eq!(fx.commits_ahead(&branches[0]), 1);
    assert_eq!(fx.created_pulls().await.len(), 1);
    assert!(seen.artifacts.iter().any(|(n, _)| n == "pull_request"));
}

// ------------------------------------------------------------ rate limits

/// A model gateway (`POST /chat/completions`) that answers its first request
/// with `429` and `Retry-After: <hint_secs>`, then replies by *turn* (the
/// number of tool results in the conversation, so a retried request gets the
/// same answer). Every request is logged with its arrival time and status.
struct RateLimitedOnce {
    hint_secs: u64,
    replies: Vec<serde_json::Value>,
    log: Arc<Mutex<Vec<(Instant, u16)>>>,
}

impl Respond for RateLimitedOnce {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
        let turn = body["messages"]
            .as_array()
            .map_or(0, |m| m.iter().filter(|m| m["role"] == "tool").count());
        let mut log = self.log.lock().unwrap();
        let (status, template) = if log.is_empty() {
            (
                429,
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", self.hint_secs.to_string().as_str())
                    .set_body_json(
                        json!({"error": {"message": "slow down", "type": "rate_limit_exceeded"}}),
                    ),
            )
        } else {
            match self.replies.get(turn) {
                Some(reply) => (200, ResponseTemplate::new(200).set_body_json(reply)),
                None => (
                    500,
                    ResponseTemplate::new(500).set_body_string("script exhausted"),
                ),
            }
        };
        log.push((Instant::now(), status));
        template
    }
}

/// The model answers the first request with `429` and `Retry-After`. The
/// real OpenAI-compatible client carries the hint to the runtime
/// (`AgentError::Transient` with `retry_after`), which waits at least that long before the
/// retry (the policy's own backoff here is 50 ms), and the run then completes
/// with exactly one pull request.
async fn rate_limited_model_backs_off_and_completes(store: DynStore) {
    const HINT: Duration = Duration::from_secs(1);
    let fx = Fixture::new("hello\n").await;
    let replies = vec![
        tool_reply(
            "c1",
            "prepare_workspace",
            json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
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
        tool_reply(
            "c4",
            "commit_and_push",
            json!({"message": "feat: add hello.txt"}),
        ),
        tool_reply(
            "c5",
            "open_pull_request",
            json!({"title": "feat: add hello.txt", "body": "Adds hello.txt.\n\n## Verification\n- `test -f hello.txt`: passed"}),
        ),
        text_reply("Opened the pull request."),
    ];
    let log = Arc::new(Mutex::new(Vec::new()));
    let gateway = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(RateLimitedOnce {
            hint_secs: HINT.as_secs(),
            replies,
            log: log.clone(),
        })
        .mount(&gateway)
        .await;
    let model: DynModel = Arc::new(
        OpenAiCompatible::new(OpenAiConfig::new(
            gateway.uri(),
            SecretString::from("test-key"),
        ))
        .unwrap(),
    );
    let server = Server::start(coder_retrying(&fx, model, store, 3)).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, &format!("add hello.txt in {}", fx.remote_url())).await;
    worker.stop().await;

    assert_eq!(
        seen.last_state,
        Some(TaskState::Completed),
        "{:?} {:#?}",
        seen.labels,
        seen.messages
    );
    let log = log.lock().unwrap().clone();
    let statuses: Vec<u16> = log.iter().map(|(_, s)| *s).collect();
    assert_eq!(
        statuses,
        [429, 200, 200, 200, 200, 200, 200],
        "one rate limit, then the six turns of the script"
    );
    let waited = log[1].0.duration_since(log[0].0);
    assert!(
        waited >= HINT,
        "the retry came {waited:?} after the 429, before its Retry-After of {HINT:?}"
    );
    assert!(
        waited < HINT + Duration::from_secs(20),
        "and it did not wait much longer than asked: {waited:?}"
    );
    let branches = fx.agent_branches();
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert_eq!(fx.commits_ahead(&branches[0]), 1);
    assert_eq!(
        fx.created_pulls().await.len(),
        1,
        "exactly one pull request"
    );
    assert!(seen.artifacts.iter().any(|(n, _)| n == "pull_request"));
}

// ------------------------------------------------------------------ cancel

/// CancelTask while OpenCode is mid-turn. The task ends `canceled` at once;
/// the step's cancel token reaches the delegate tool, which stops the ACP turn
/// (`session/cancel`) and kills OpenCode and what it started, so nothing is
/// left running. The model is never asked again, so nothing is committed,
/// pushed or opened.
async fn cancel_during_opencode_turn_cancels_without_push_or_pr(store: DynStore) {
    let agent_dir = tempfile::tempdir().unwrap();
    let pid_file = agent_dir.path().join("agent.pid");
    let child_file = agent_dir.path().join("child.pid");
    let fx = Fixture::with("hello\n", |s| {
        s.opencode = OpenCodeLaunch::program(common::fake_agent())
            .env("FAKE_ACP_SCENARIO", "slow")
            .env("FAKE_ACP_PID_FILE", pid_file.to_string_lossy())
            .env("FAKE_ACP_CHILD_PID_FILE", child_file.to_string_lossy());
    })
    .await;
    let mock = Arc::new(MockModel::new());
    // The whole happy path is scripted: what a cancelled run must not reach
    // (checks, commit, push, pull request) is there to be reached.
    happy_script(&mock, &fx.remote_url());
    let server = Server::start(coder_with(&fx, &mock, store)).await;

    let mut stream = server
        .client
        .send_streaming_message(&request(user(&format!(
            "add hello.txt in {}",
            fx.remote_url()
        ))))
        .await
        .unwrap();
    let mut seen = Seen::default();
    seen.record(stream.next().await.expect("snapshot").unwrap());
    let worker = spawn_worker(&server.coder);

    // OpenCode is mid-turn once it has started its own child.
    let agent = common::wait_for_pid(&pid_file).await;
    let grandchild = common::wait_for_pid(&child_file).await;
    assert!(
        !common::process_gone(agent, true),
        "OpenCode should be running"
    );

    let cancelled_at = Instant::now();
    let canceled = server
        .client
        .cancel_task(&a2a::CancelTaskRequest {
            id: seen.task_id.clone(),
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);
    while let Some(item) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream ends after the cancel")
    {
        seen.record(item.unwrap());
    }
    assert_eq!(
        seen.last_state,
        Some(TaskState::Canceled),
        "{:?}",
        seen.labels
    );
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(2),
        "canceled within 2 s, took {:?}",
        cancelled_at.elapsed()
    );

    // No orphan: OpenCode is reaped and its child is dead, within 5 s.
    assert!(
        common::wait_gone(agent, true, Duration::from_secs(5)).await,
        "OpenCode (pid {agent}) is still there after the cancel"
    );
    assert!(
        common::wait_gone(grandchild, false, Duration::from_secs(5)).await,
        "OpenCode's child (pid {grandchild}) is still there after the cancel"
    );

    // The run is over for good: the step that was cancelled does not go on.
    worker.stop().await;
    let done = server
        .client
        .get_task(&a2a::GetTaskRequest {
            id: seen.task_id.clone(),
            history_length: None,
            tenant: None,
        })
        .await
        .unwrap();
    assert_eq!(done.status.state, TaskState::Canceled);
    assert!(
        !done
            .artifacts
            .unwrap_or_default()
            .iter()
            .any(|a| { matches!(a.name.as_deref(), Some("branch" | "pull_request")) }),
        "a canceled run delivers nothing"
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "prepare and delegate, and no model turn after the cancel"
    );
    assert!(fx.agent_branches().is_empty(), "nothing was pushed");
    assert!(fx.created_pulls().await.is_empty(), "no pull request");
}

// ------------------------------------------------------- concurrent tasks

/// One scripted model per task, chosen by a marker in the task's text: two
/// runs interleave their model calls, so a single shared script cannot serve
/// both.
struct PerTaskModel {
    scripts: Vec<(&'static str, Arc<MockModel>)>,
}

impl PerTaskModel {
    fn pick(&self, req: &ModelRequest) -> &Arc<MockModel> {
        let text = format!("{:?}", req.messages);
        self.scripts
            .iter()
            .find(|(marker, _)| text.contains(marker))
            .map(|(_, model)| model)
            .expect("the conversation carries a task marker")
    }
}

#[async_trait]
impl ModelClient for PerTaskModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.pick(&req).complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.pick(&req).stream(req).await
    }
}

/// Two tasks on the same repository, advanced by one worker at the same time
/// (concurrency 2): each gets its own worktree and branch, and its own pull
/// request; the shared mirror serves both.
async fn two_concurrent_tasks_on_one_repo_get_two_branches_and_two_prs(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let (alpha, beta) = (Arc::new(MockModel::new()), Arc::new(MockModel::new()));
    common::happy_script_titled(&alpha, &fx.remote_url(), "feat: alpha");
    common::happy_script_titled(&beta, &fx.remote_url(), "feat: beta");
    let model: DynModel = Arc::new(PerTaskModel {
        scripts: vec![("task-alpha", alpha.clone()), ("task-beta", beta.clone())],
    });
    let coder = Coder::new(
        store,
        CoderAgent::new(model, "test-model", fx.env.clone()),
        &options(),
    );
    let owner = Caller::new("token-0");
    let a = coder
        .backend
        .submit(
            owner.clone(),
            user(&format!("task-alpha: add hello.txt in {}", fx.remote_url())),
            None,
            None,
        )
        .await
        .unwrap();
    let b = coder
        .backend
        .submit(
            owner.clone(),
            user(&format!("task-beta: add hello.txt in {}", fx.remote_url())),
            None,
            None,
        )
        .await
        .unwrap();
    assert_ne!(a.id, b.id);
    let worker = spawn_worker(&coder);
    for task in [&a, &b] {
        let view = wait_for(&coder.runtime, run_id(&task.id), "the run to finish", |v| {
            v.status.is_terminal()
        })
        .await;
        assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    }
    worker.stop().await;

    let branches = fx.agent_branches();
    assert_eq!(branches.len(), 2, "{branches:?}");
    for branch in &branches {
        assert_eq!(fx.commits_ahead(branch), 1, "{branch}");
        assert_eq!(fx.ref_updates(branch), 1, "{branch}");
        assert_eq!(fx.file_on(branch, "hello.txt"), "hello");
    }
    let mut heads: Vec<String> = fx
        .created_pulls()
        .await
        .iter()
        .map(|p| p["head"].as_str().unwrap().to_owned())
        .collect();
    heads.sort();
    assert_eq!(heads, branches, "one pull request per branch");
    let titles: std::collections::BTreeSet<String> = fx
        .created_pulls()
        .await
        .iter()
        .map(|p| p["title"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        titles,
        ["feat: alpha", "feat: beta"].map(String::from).into()
    );

    // Each task reports its own pull request.
    let mut urls = Vec::new();
    for (task, title) in [(&a, "feat: alpha"), (&b, "feat: beta")] {
        let task = coder.backend.get(&owner, &task.id).await.unwrap().unwrap();
        assert_eq!(task.status.state, TaskState::Completed, "{title}");
        let pr = task
            .artifacts
            .unwrap()
            .into_iter()
            .find(|a| a.name.as_deref() == Some("pull_request"))
            .unwrap_or_else(|| panic!("{title} has no pull_request artifact"));
        let a2a::PartContent::Data(data) = &pr.parts[0].content else {
            panic!("data part expected")
        };
        urls.push(data["url"].as_str().unwrap().to_owned());
        // The durable copy carries the link part too, and it is the same URL.
        assert_eq!(pr.parts.len(), 2, "{title}: {:?}", pr.parts);
        assert_eq!(
            pr.parts[1].content,
            a2a::PartContent::Url(data["url"].as_str().unwrap().to_owned()),
            "{title}"
        );
    }
    urls.sort();
    assert_eq!(urls, [PR_URL.to_owned(), common::pull_url(8)]);
    // Two worktrees, on two different branches.
    let worktrees: Vec<_> = std::fs::read_dir(fx.root.join("worktrees"))
        .unwrap()
        .collect();
    assert_eq!(worktrees.len(), 2);
}

// ----------------------------------------------------------------- GitHub 401

/// GitHub rejects the token when the coder opens the pull request. The model
/// cannot fix a bad credential, so the run must not "complete" without a pull
/// request: it fails, and the error says what to look at.
async fn a_github_401_fails_the_run_with_a_clear_message(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    common::github_fails_with(&fx.github, 401, "Bad credentials").await;
    let mock = Arc::new(MockModel::new());
    happy_script(&mock, &fx.remote_url());
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, &format!("add hello.txt in {}", fx.remote_url())).await;
    worker.stop().await;

    assert_eq!(
        seen.last_state,
        Some(TaskState::Failed),
        "{:?} {:#?}",
        seen.labels,
        seen.messages
    );
    let error = seen
        .messages
        .iter()
        .find(|m| m.contains("Bad credentials"))
        .unwrap_or_else(|| panic!("the GitHub message is reported: {:#?}", seen.messages));
    assert!(
        error.contains("authentication failed") && error.contains("GITHUB_TOKEN"),
        "{error}"
    );
    assert!(
        !error.contains(common::GITHUB_TOKEN),
        "the token never appears in a message: {error}"
    );
    assert!(
        !seen.labels.iter().any(|l| l == "artifact:pull_request"),
        "{:?}",
        seen.labels
    );
    // The branch was pushed (git is the durable artifact) but no PR exists.
    assert_eq!(fx.agent_branches().len(), 1);
    assert!(fx.created_pulls().await.is_empty());
}

// ------------------------------------------------------------- the A2A front

/// The coder's own router (not just `adam-a2a`'s) refuses a missing or wrong
/// token with 401 and never runs anything, while the agent card and
/// `/healthz` stay open.
async fn wrong_token_on_the_coder_router_is_401(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    let coder = coder_with(&fx, &mock, store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = coder.router(
        &format!("http://{addr}/").parse().unwrap(),
        AuthConfig::BearerTokens(vec![SecretString::from(TOKEN)]),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    for bearer in [None, Some("wrong"), Some(""), Some("coder-test-toke")] {
        let (status, response) = common::raw(addr, "POST", "/", bearer).await;
        assert_eq!(status, 401, "{bearer:?}: {response}");
        assert!(
            response
                .to_ascii_lowercase()
                .contains("www-authenticate: bearer"),
            "{response}"
        );
        assert!(!response.contains(TOKEN), "{response}");
    }
    // The right token gets past authentication (the task does not exist).
    let (status, response) = common::raw(addr, "POST", "/", Some(TOKEN)).await;
    assert_ne!(status, 401, "{response}");

    let (status, card) = common::raw(addr, "GET", "/.well-known/agent-card.json", None).await;
    assert_eq!(status, 200, "{card}");
    assert!(card.contains("adam-coder") && card.contains(&format!("http://{addr}/")));
    let (status, _) = common::raw(addr, "GET", "/healthz", None).await;
    assert_eq!(status, 200);

    assert!(mock.requests().is_empty(), "no run was started");
}

// ------------------------------------------------------------------ secrets

/// Everything a client (or an operator reading the run) can see of a run:
/// the stream, the fetched task, the run's error, output and stored state, the
/// notes on disk.
async fn everything_visible(server: &Server, fx: &Fixture, seen: &Seen) -> String {
    let run = run_id(&seen.task_id);
    let view = server.coder.runtime.view(run).await.unwrap().unwrap();
    let task = server
        .client
        .get_task(&a2a::GetTaskRequest {
            id: seen.task_id.clone(),
            history_length: None,
            tenant: None,
        })
        .await
        .unwrap();
    let notes = std::fs::read_to_string(fx.root.join("coder").join(format!("{run}.json")))
        .unwrap_or_default();
    format!(
        "{:?}\n{:?}\n{:?}\n{}\n{:?} {:?} {}\n{notes}",
        seen.labels,
        seen.messages,
        seen.artifacts,
        serde_json::to_string(&task).unwrap(),
        view.error,
        view.output,
        view.state,
    )
}

fn assert_no_secret(haystack: &str, what: &str) {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    for secret in common::SECRETS {
        for form in [
            secret.to_owned(),
            b64.encode(secret),
            b64.encode(format!("x-access-token:{secret}")),
        ] {
            assert!(
                !haystack.contains(&form),
                "{what}: {form} appears in what a client can see:\n{haystack}"
            );
        }
    }
}

/// OpenCode dies printing secrets to stderr (a key in a message, an
/// Authorization header): the stderr tail is part of the error a client
/// reads, and the values must be gone from it, from the stream, the task, the
/// run's stored error and state.
async fn secrets_in_opencode_stderr_never_reach_the_client(store: DynStore) {
    let agent_dir = tempfile::tempdir().unwrap();
    let (launch, _) = common::scripted_agent(
        agent_dir.path(),
        "printf 'auth failed for key %s\\n' \"$LEAK_KEY\" >&2\n\
         printf 'Authorization: Basic %s\\n' \"$(printf 'x-access-token:%s' \"$LEAK_GH\" | base64 | tr -d '\\n')\" >&2\n\
         printf 'bearer %s and db password %s\\n' \"$LEAK_A2A\" \"$LEAK_DB\" >&2\n\
         FAKE_ACP_SCENARIO=crash exec \"$AGENT\"",
        OpenCodeLaunch::program("unused")
            .env("LEAK_KEY", common::MODEL_KEY)
            .env("LEAK_GH", common::GITHUB_TOKEN)
            .env("LEAK_A2A", common::A2A_TOKEN)
            .env("LEAK_DB", common::DB_PASSWORD),
    );
    let fx = Fixture::with("hello\n", |s| s.opencode = launch).await;
    let mock = Arc::new(MockModel::new());
    prepare_and_delegate(&mock, &fx, &["c2"]);
    // One attempt only: the first transient failure ends the run.
    let server = Server::start(coder_retrying(&fx, mock.clone(), store, 1)).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, &format!("add hello.txt in {}", fx.remote_url())).await;
    worker.stop().await;

    assert_eq!(
        seen.last_state,
        Some(TaskState::Failed),
        "{:?}",
        seen.labels
    );
    let error = seen
        .messages
        .iter()
        .find(|m| m.contains("gave up after 1 attempts"))
        .unwrap_or_else(|| panic!("no failure message in {:#?}", seen.messages));
    assert!(
        error.contains("auth failed for key [redacted]"),
        "the diagnosis is kept, the key is not: {error}"
    );
    assert!(
        error.contains("bearer [redacted] and db password [redacted]"),
        "{error}"
    );
    assert!(error.contains("Authorization: Basic [redacted]"), "{error}");
    assert_no_secret(
        &everything_visible(&server, &fx, &seen).await,
        "opencode stderr",
    );
}

/// A failing check prints secrets (say a test dumps its environment): the
/// findings a client reads as the run's error, the tool result the model sees,
/// the notes and the progress lines are clean.
async fn secrets_in_check_output_never_reach_the_client(store: DynStore) {
    // OpenCode writes a file that holds secrets (a dumped environment); the
    // failing check prints it.
    let dump = format!(
        "GITHUB_TOKEN={}\nMODEL_API_KEY={}\ndb=postgres://u:{}@h/db\n",
        common::GITHUB_TOKEN,
        common::MODEL_KEY,
        common::DB_PASSWORD
    );
    let fx = Fixture::with(&dump, |s| s.max_check_cycles = 1).await;
    let mock = Arc::new(MockModel::new());
    prepare_and_delegate(&mock, &fx, &["c2"]);
    mock.push_tool_calls(vec![call(
        "c3",
        "run_checks",
        json!({"command": "cat hello.txt; exit 1"}),
    )])
    .push_text("The checks fail.");
    let server = Server::start(coder_with(&fx, &mock, store)).await;
    let worker = spawn_worker(&server.coder);

    let seen = run_to_end(&server, &format!("add hello.txt in {}", fx.remote_url())).await;
    worker.stop().await;

    assert_eq!(
        seen.last_state,
        Some(TaskState::Failed),
        "{:?}",
        seen.labels
    );
    let error = seen
        .messages
        .iter()
        .find(|m| m.contains("checks are failing and no pull request was opened"))
        .unwrap_or_else(|| panic!("no verdict in {:#?}", seen.messages));
    assert!(
        error.contains("GITHUB_TOKEN=[redacted]")
            && error.contains("db=postgres://u:[redacted]@h/db"),
        "the findings are kept, the secrets are not: {error}"
    );
    let results = tool_results(&mock.requests().last().unwrap().messages);
    let (_, seen_by_model, _) = results.iter().find(|(c, _, _)| c == "c3").unwrap();
    assert!(
        seen_by_model.contains("GITHUB_TOKEN=[redacted]"),
        "{seen_by_model}"
    );
    assert_no_secret(seen_by_model, "the model's tool result");
    assert_no_secret(
        &everything_visible(&server, &fx, &seen).await,
        "check output",
    );
}

// -------------------------------------------------------------------- ownership

async fn another_caller_cannot_see_or_resume_the_task(store: DynStore) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "q1",
        "ask_user",
        json!({"question": "Which repo?"}),
    )]);
    let coder = coder_with(&fx, &mock, store);
    let worker = spawn_worker(&coder);
    let alice = Caller::new("token-0");
    let task = coder
        .backend
        .submit(alice.clone(), user("do something"), None, None)
        .await
        .unwrap();
    wait_for(&coder.runtime, run_id(&task.id), "waiting", |v| v.waiting).await;
    worker.stop().await;

    let bob = Caller::new("token-1");
    assert!(coder.backend.get(&bob, &task.id).await.unwrap().is_none());
    assert!(matches!(
        coder
            .backend
            .submit(bob.clone(), user("hi"), Some(task.id.clone()), None)
            .await,
        Err(BackendError::TaskNotFound(_))
    ));
    assert!(matches!(
        coder.backend.cancel(&bob, &task.id).await,
        Err(BackendError::TaskNotFound(_))
    ));
    let seen: Task = coder.backend.get(&alice, &task.id).await.unwrap().unwrap();
    assert_eq!(seen.status.state, TaskState::InputRequired);
}

// ------------------------------------------------------- one suite per store

/// Every case runs once per store: in memory always, and on PostgreSQL when
/// `ADAM_TEST_POSTGRES_URL` is set (each case gets a private database).
macro_rules! coder_suite {
    ($module:ident, $make:path) => {
        mod $module {
            coder_suite!(@cases $make;
                add_hello_txt_streams_working_progress_checks_artifact_completed,
                ask_user_parks_and_an_a2a_follow_up_resumes,
                a_plain_text_stop_is_a_question_and_the_answer_resumes_the_run,
                a_stop_without_text_asks_what_to_do,
                checks_without_a_pull_request_then_text_is_a_question,
                an_invented_repository_is_refused_and_the_model_must_ask,
                a_message_in_the_context_of_a_parked_run_continues_it,
                a_run_parked_by_a_plain_text_stop_can_be_canceled,
                red_checks_n_times_fail_the_run_with_the_findings_and_no_pr,
                a_pull_request_with_red_checks_needs_explicit_acceptance,
                crash_after_commit_and_push_was_journaled_repeats_nothing,
                crash_inside_run_checks_before_the_journal_emits_one_checks_artifact,
                crash_inside_commit_and_push_before_the_journal_is_idempotent,
                crash_inside_open_pull_request_before_the_journal_is_idempotent,
                crash_during_delegate_to_opencode_reruns_on_the_same_worktree,
                opencode_crashing_every_time_fails_the_run_with_its_stderr,
                opencode_crashing_once_is_retried_and_completes,
                rate_limited_model_backs_off_and_completes,
                cancel_during_opencode_turn_cancels_without_push_or_pr,
                two_concurrent_tasks_on_one_repo_get_two_branches_and_two_prs,
                a_github_401_fails_the_run_with_a_clear_message,
                secrets_in_opencode_stderr_never_reach_the_client,
                secrets_in_check_output_never_reach_the_client,
                wrong_token_on_the_coder_router_is_401,
                another_caller_cannot_see_or_resume_the_task,
            );
        }
    };
    (@cases $make:path; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                let Some(backing) = $make().await else {
                    eprintln!("skipped: store not configured");
                    return;
                };
                super::$case(backing.store()).await;
                backing.finish().await;
            }
        )+
    };
}

coder_suite!(memory, super::memory_backing);
coder_suite!(postgres, super::postgres_backing);
