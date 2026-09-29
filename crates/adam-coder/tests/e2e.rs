//! The coder end to end, offline: A2A client -> A2A server -> runtime -> the
//! coder agent (scripted `MockModel`) -> real worktrees over a local bare git
//! remote, the adam-acp fake agent for OpenCode, and a mock GitHub.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use a2a::{
    Message, Part, Role, SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskState,
};
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::auth::AuthInterceptor;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use adam_a2a::{AuthConfig, BackendError, Caller, TaskBackend, TaskEvent};
use adam_coder::{Coder, CoderAgent, RuntimeOptions, ToolEnv, coder_tools};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Conversation, DynTool, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{
    DynModel, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse, ToolSpec,
};
use adam_runtime::RunView;
use async_trait::async_trait;
use common::{Fixture, PR_URL, call, happy_script};
use futures::StreamExt;
use futures::stream::BoxStream;
use secrecy::SecretString;
use serde_json::json;
use tokio::sync::{Notify, oneshot};

type Client = A2AClient<Box<dyn Transport>>;

const TOKEN: &str = "coder-test-token";

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

fn store() -> DynStore {
    Arc::new(MemoryStore::new())
}

// ------------------------------------------------------------------- happy path

#[tokio::test]
async fn add_hello_txt_streams_working_progress_checks_artifact_completed() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    happy_script(&mock, &fx.remote_url());
    let server = Server::start(coder_with(&fx, &mock, store())).await;

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

    // The branch is on the remote with the file, in one commit.
    let branches = fx.agent_branches();
    assert_eq!(branches.len(), 1, "{branches:?}");
    assert_eq!(fx.file_on(&branches[0], "hello.txt"), "hello");
    assert_eq!(fx.commits_ahead(&branches[0]), 1);
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
    assert_eq!(names, ["branch", "pull_request"]);

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

#[tokio::test]
async fn ask_user_parks_and_an_a2a_follow_up_resumes() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "q1",
        "ask_user",
        json!({"question": "Which base branch should I use?"}),
    )])
    .push_text("Understood: main.");
    let server = Server::start(coder_with(&fx, &mock, store())).await;
    let worker = spawn_worker(&server.coder);

    let mut stream = server
        .client
        .send_streaming_message(&request(user("add hello.txt")))
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
    assert_eq!(task.status.state, TaskState::Completed);
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

// ------------------------------------------------------------------- red checks

/// Failing checks `MAX_CHECK_CYCLES` times end the run `failed`, with the
/// findings, and no pull request; the tools hold the line even when the model
/// keeps going.
#[tokio::test]
async fn red_checks_n_times_fail_the_run_with_the_findings_and_no_pr() {
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
    let server = Server::start(coder_with(&fx, &mock, store())).await;
    let worker = spawn_worker(&server.coder);

    let mut stream = server
        .client
        .send_streaming_message(&request(user("add hello.txt")))
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
#[tokio::test]
async fn a_pull_request_with_red_checks_needs_explicit_acceptance() {
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
    let server = Server::start(coder_with(&fx, &mock, store())).await;
    let worker = spawn_worker(&server.coder);

    let mut stream = server
        .client
        .send_streaming_message(&request(user("add hello.txt")))
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
    /// After `commit_and_push` pushed, before its result was journaled.
    InsideCommitAndPush,
    /// After `commit_and_push` was journaled and committed, at the next model call.
    AfterCommitAndPush,
    /// After `open_pull_request` created the PR, before its result was journaled.
    InsideOpenPullRequest,
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
async fn crash_at(point: CrashPoint) {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    happy_script(&mock, &fx.remote_url());
    let store = store();
    let reached = Arc::new(Notify::new());
    let armed = Arc::new(AtomicBool::new(true));

    let (model, wrap): (DynModel, Option<&'static str>) = match point {
        CrashPoint::InsideCommitAndPush => (mock.clone(), Some("commit_and_push")),
        CrashPoint::InsideOpenPullRequest => (mock.clone(), Some("open_pull_request")),
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
        .submit(owner.clone(), user("add hello.txt"), None, None)
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
        .unwrap()
        .iter()
        .filter_map(|a| a.name.clone())
        .collect();
    assert_eq!(names, ["branch", "pull_request"]);
    let conversation: Conversation = serde_json::from_value(view.state).unwrap();
    assert!(
        conversation
            .artifacts
            .iter()
            .any(|a| a.name == "pull_request")
    );
}

#[tokio::test]
async fn crash_after_commit_and_push_was_journaled_repeats_nothing() {
    crash_at(CrashPoint::AfterCommitAndPush).await;
}

#[tokio::test]
async fn crash_inside_commit_and_push_before_the_journal_is_idempotent() {
    crash_at(CrashPoint::InsideCommitAndPush).await;
}

#[tokio::test]
async fn crash_inside_open_pull_request_before_the_journal_is_idempotent() {
    crash_at(CrashPoint::InsideOpenPullRequest).await;
}

// -------------------------------------------------------------------- ownership

#[tokio::test]
async fn another_caller_cannot_see_or_resume_the_task() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "q1",
        "ask_user",
        json!({"question": "Which repo?"}),
    )]);
    let coder = coder_with(&fx, &mock, store());
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
