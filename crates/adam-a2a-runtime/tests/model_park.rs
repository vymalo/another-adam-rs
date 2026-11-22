//! A run parked on the model, over A2A (ADR 0067 decision 4, `docs/reference/a2a-server.md`, "A run
//! parked on the model"): the three states and their messages, the list filter, a message to a
//! park on a timer, a textless `adam.resume`, the resume refused by a task that is not parked on
//! the model (asking, finished, canceled) with nothing written, and cancel. A real `LlmAgent` on a
//! scripted model whose refusals are injected, a real `Runtime`, and the backend; the textless
//! resume also goes through the real A2A server over HTTP.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, RESUME_METADATA_KEY, TaskBackend,
    TaskQuery,
};
use adam_a2a_runtime::{NOT_PARKED, RuntimeTaskBackend};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{DynModel, MockModel, ModelError, ModelRequest, Park, ToolCall, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn refused(status: u16, slug: &str, park: Park) -> ModelError {
    ModelError::Refused {
        status,
        slug: Some(slug.to_owned()),
        retry_after: None,
        park,
    }
}

/// A park on a timer long enough that no test sees it end on its own.
fn timer() -> Park {
    Park::OnTimer(Duration::from_secs(600))
}

/// A tool that asks the person.
struct Ask;

#[async_trait]
impl Tool for Ask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask".into(),
            description: "asks".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    fn asks_user(&self) -> bool {
        true
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input("Which one?"))
    }
}

/// The subagent tool: starts the child `helper` and waits for it.
struct Spawn;

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sub".into(),
            description: "hands the work to the helper".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx.start_child("helper", "go").await?;
        Err(ToolError::AwaitRun { run })
    }
}

/// Wraps the mock: the `hold_on`-th call (0-based) waits until the test lets it go.
struct GateModel {
    inner: Arc<MockModel>,
    hold_on: usize,
    calls: std::sync::atomic::AtomicUsize,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl GateModel {
    async fn hold(&self) {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == self.hold_on {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

#[async_trait]
impl adam_model::ModelClient for GateModel {
    async fn complete(&self, req: ModelRequest) -> Result<adam_model::ModelResponse, ModelError> {
        self.hold().await;
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<adam_model::ModelDelta, ModelError>>,
        ModelError,
    > {
        self.hold().await;
        self.inner.stream(req).await
    }
}

struct Rig {
    mock: Arc<MockModel>,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Rig {
    fn new() -> Self {
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        Self::over(mock, model)
    }

    /// The agent (with `ask` and `sub`, which hands work to the subagent `helper`) on `model`.
    fn over(mock: Arc<MockModel>, model: DynModel) -> Self {
        let name = format!("park-{}", RunId::new());
        let store: DynStore = Arc::new(MemoryStore::new());
        let agent = LlmAgent::builder(&name, model.clone(), "m")
            .tool(Ask)
            .tool(Spawn)
            .build();
        let helper = LlmAgent::builder("helper", model, "m").build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .agent(helper)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, name)
            .with_poll_interval(Duration::from_millis(10));
        let (stop, rx) = oneshot::channel::<()>();
        let worker = {
            let rt = runtime.clone();
            tokio::spawn(async move {
                let _ = rt
                    .run_worker(async {
                        let _ = rx.await;
                    })
                    .await;
            })
        };
        Self {
            mock,
            runtime,
            backend,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            tokio::time::timeout(Duration::from_secs(10), worker)
                .await
                .expect("worker stops in time")
                .expect("worker task");
        }
    }

    async fn start(&self, who: &Caller, context: &str) -> Task {
        self.backend
            .submit(
                who.clone(),
                user("go", &format!("m-{}", RunId::new())),
                None,
                Some(context.into()),
            )
            .await
            .expect("submit")
    }

    /// The task once it is in `state` and, when `text` is given, says it.
    async fn wait(&self, who: &Caller, id: &str, state: TaskState, text: Option<&str>) -> Task {
        for _ in 0..2000 {
            let task = self.backend.get(who, id).await.expect("get").expect("task");
            if task.status.state == state && text.is_none_or(|t| said(&task) == Some(t.into())) {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let task = self.backend.get(who, id).await.unwrap().unwrap();
        panic!(
            "task {id} never reached {state:?} {text:?}: {:?}",
            task.status
        );
    }

    async fn written(&self, task: &Task) -> (u64, usize) {
        let view = self
            .runtime
            .view(RunId(task.id.parse().unwrap()))
            .await
            .unwrap()
            .unwrap();
        (view.version, view.pending_inbox)
    }
}

fn caller() -> Caller {
    Caller::new(format!("token-{}", RunId::new()))
}

fn user(text: &str, id: &str) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m
}

/// A resume with no part at all.
fn resume(id: &str) -> Message {
    let mut m = Message::new(Role::User, vec![]);
    m.message_id = id.into();
    m.metadata = Some(
        [(RESUME_METADATA_KEY.to_owned(), json!(true))]
            .into_iter()
            .collect(),
    );
    m
}

fn said(task: &Task) -> Option<String> {
    task.status
        .message
        .as_ref()
        .and_then(|m| m.text())
        .map(str::to_owned)
}

fn texts(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .map(adam_model::Message::text)
        .collect()
}

fn is_not_parked(err: &BackendError) -> bool {
    matches!(err, BackendError::InvalidParams(m) if m == NOT_PARKED)
}

/// Each park shows its state with its reason as the status message, and `ListTasks` returns each
/// task when a client filters by its state.
#[tokio::test]
async fn the_three_states_say_the_reason_and_a_list_filters_by_them() {
    let rig = Rig::new();
    let who = caller();
    rig.mock.push_error(refused(403, "run_not_bound", timer()));
    let auth = rig.start(&who, "c-auth").await;
    let auth = rig
        .wait(
            &who,
            &auth.id,
            TaskState::AuthRequired,
            Some("the model gateway has not bound this run yet"),
        )
        .await;
    rig.mock
        .push_error(refused(402, "over_limit", Park::UntilWoken));
    let input = rig.start(&who, "c-input").await;
    let input = rig
        .wait(
            &who,
            &input.id,
            TaskState::InputRequired,
            Some("the model gateway's limit is reached"),
        )
        .await;
    rig.mock.push_error(refused(503, "unavailable", timer()));
    let working = rig.start(&who, "c-working").await;
    let working = rig
        .wait(
            &who,
            &working.id,
            TaskState::Working,
            Some("the model gateway is busy"),
        )
        .await;
    for (state, task) in [
        (TaskState::AuthRequired, &auth),
        (TaskState::InputRequired, &input),
        (TaskState::Working, &working),
    ] {
        let mut query = TaskQuery::new();
        query.status = Some(state.clone());
        let page = rig.backend.list(&who, &query).await.expect("list");
        let ids: Vec<&str> = page.tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, [task.id.as_str()], "{state:?}");
        assert_eq!(said(&page.tasks[0]), said(task), "{state:?}");
    }
    rig.stop().await;
}

/// A message to a task parked on a timer is accepted, and its text is the person's turn, added
/// before the call is made again: on `working` and on `auth-required` alike.
#[tokio::test]
async fn a_message_to_a_timer_park_is_accepted_and_its_text_added() {
    for (status, slug, state) in [
        (503, "unavailable", TaskState::Working),
        (401, "unauthenticated", TaskState::AuthRequired),
    ] {
        let rig = Rig::new();
        let who = caller();
        rig.mock
            .push_error(refused(status, slug, timer()))
            .push_text("answered");
        let task = rig.start(&who, "c").await;
        rig.wait(&who, &task.id, state.clone(), None).await;
        let answered = rig
            .backend
            .submit(
                who.clone(),
                user("are you there?", "m-2"),
                Some(task.id.clone()),
                Some("c".into()),
            )
            .await
            .expect("a message to a timer park is accepted");
        assert_eq!(answered.status.state, TaskState::Working, "it woke");
        assert_eq!(said(&answered), None, "{state:?} woke: no reason to say");
        rig.wait(&who, &task.id, TaskState::Completed, Some("answered"))
            .await;
        let requests = rig.mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(texts(&requests[1]), ["go", "are you there?"], "{slug}");
        rig.stop().await;
    }
}

/// A resume with no text wakes a task parked until woken and adds nothing: the same call is made
/// again. The server lets it through with no part.
#[tokio::test]
async fn a_textless_resume_wakes_the_task_and_adds_nothing() {
    let rig = Rig::new();
    let who = caller();
    rig.mock
        .push_error(refused(402, "over_limit", Park::UntilWoken))
        .push_text("answered");
    let task = rig.start(&who, "c").await;
    rig.wait(&who, &task.id, TaskState::InputRequired, None)
        .await;
    let woken = rig
        .backend
        .submit(who.clone(), resume("r-1"), Some(task.id.clone()), None)
        .await
        .expect("a resume is accepted");
    assert_eq!(woken.status.state, TaskState::Working);
    rig.wait(&who, &task.id, TaskState::Completed, Some("answered"))
        .await;
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(texts(&requests[0]), texts(&requests[1]), "no turn added");
    rig.stop().await;
}

/// Over HTTP, as the orchestration layer sends it: a `SendMessage` with no part and the resume key.
#[tokio::test]
async fn a_textless_resume_goes_through_the_server() {
    let rig = Rig::new();
    let who = Caller::new("token-0");
    rig.mock
        .push_error(refused(402, "over_limit", Park::UntilWoken))
        .push_text("answered");
    let task = rig.start(&who, "c").await;
    rig.wait(&who, &task.id, TaskState::InputRequired, None)
        .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let card = AgentCardConfig::new(
        "parked",
        "A test agent that parks",
        format!("http://{addr}/").parse().unwrap(),
        "0.1.0",
    );
    let app = A2aServer::router(
        card,
        Arc::new(rig.backend.clone()),
        AuthConfig::BearerTokens(vec![secrecy::SecretString::from("t0")]),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let message = |id: &str| {
        json!({
            "messageId": id,
            "role": "ROLE_USER",
            "parts": [],
            "taskId": task.id,
            "metadata": {RESUME_METADATA_KEY: true},
        })
    };
    let (status, body) = raw_send(addr, message("r-http")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("error").is_none(), "{body}");
    rig.wait(&who, &task.id, TaskState::Completed, Some("answered"))
        .await;

    // The same request to the finished task is refused with -32602 and the reason.
    let (_, body) = raw_send(addr, message("r-http-2")).await;
    assert_eq!(body["error"]["code"], -32602, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains(NOT_PARKED)),
        "{body}"
    );
    // A message with no part that is no resume is refused as before.
    let mut plain = message("r-http-3");
    plain["metadata"] = json!({});
    let (_, body) = raw_send(addr, plain).await;
    assert_eq!(body["error"]["code"], -32602, "{body}");
    server.abort();
    rig.stop().await;
}

/// A raw JSON-RPC `SendMessage` over a socket, with the bearer token: the status code and the body.
async fn raw_send(addr: std::net::SocketAddr, message: Value) -> (u16, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = json!({
        "jsonrpc": "2.0", "id": "1", "method": "SendMessage",
        "params": {"message": message, "configuration": {"returnImmediately": true}}
    })
    .to_string();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nAuthorization: Bearer t0\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut bytes))
        .await
        .expect("timed out reading the response")
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

/// A resume to a task that is not parked on the model is refused with `InvalidParams` (-32602) and
/// the reason, and nothing is written: a task that waits on a question, a finished one, a canceled
/// one. A resume never answers a question.
#[tokio::test]
async fn a_resume_to_a_task_not_parked_on_the_model_is_refused_and_writes_nothing() {
    let rig = Rig::new();
    let who = caller();

    // Asking a question.
    rig.mock.push_tool_calls(vec![ToolCall {
        id: "c1".into(),
        name: "ask".into(),
        arguments: json!({}),
        ..Default::default()
    }]);
    let asking = rig.start(&who, "c-ask").await;
    let asking = rig
        .wait(
            &who,
            &asking.id,
            TaskState::InputRequired,
            Some("Which one?"),
        )
        .await;
    // Finished.
    rig.mock.push_text("done");
    let done = rig.start(&who, "c-done").await;
    let done = rig.wait(&who, &done.id, TaskState::Completed, None).await;
    // Canceled while parked on the model.
    rig.mock
        .push_error(refused(402, "over_limit", Park::UntilWoken));
    let canceled = rig.start(&who, "c-cancel").await;
    rig.wait(&who, &canceled.id, TaskState::InputRequired, None)
        .await;
    let canceled = rig
        .backend
        .cancel(&who, &canceled.id)
        .await
        .expect("cancel");
    assert_eq!(canceled.status.state, TaskState::Canceled);

    for (what, task) in [
        ("asking", &asking),
        ("done", &done),
        ("canceled", &canceled),
    ] {
        let before = rig.written(task).await;
        let err = rig
            .backend
            .submit(
                who.clone(),
                resume(&format!("r-{what}")),
                Some(task.id.clone()),
                None,
            )
            .await
            .unwrap_err();
        assert!(is_not_parked(&err), "{what}: {err:?}");
        assert_eq!(
            a2a::A2AError::from(err).code,
            a2a::error_code::INVALID_PARAMS,
            "{what}"
        );
        assert_eq!(rig.written(task).await, before, "{what}: nothing written");
    }
    assert_eq!(
        rig.backend
            .get(&who, &asking.id)
            .await
            .unwrap()
            .unwrap()
            .status
            .state,
        TaskState::InputRequired,
        "the question is not answered"
    );
    rig.stop().await;
}

/// Cancelling ends a park on the model, timer or not.
#[tokio::test]
async fn cancel_ends_a_park() {
    for (error, state, reason) in [
        (
            refused(402, "over_limit", Park::UntilWoken),
            TaskState::InputRequired,
            "the model gateway's limit is reached",
        ),
        (
            refused(503, "unavailable", timer()),
            TaskState::Working,
            "the model gateway is busy",
        ),
    ] {
        let rig = Rig::new();
        let who = caller();
        rig.mock.push_error(error);
        let task = rig.start(&who, "c").await;
        rig.wait(&who, &task.id, state, Some(reason)).await;
        let canceled = rig.backend.cancel(&who, &task.id).await.expect("cancel");
        assert_eq!(canceled.status.state, TaskState::Canceled);
        rig.stop().await;
    }
}

/// A resume to a task whose run was woken and is making its call is refused: it is working, not
/// parked. The run makes two requests and completes once.
#[tokio::test]
async fn a_resume_while_the_woken_call_is_made_is_refused() {
    let mock = Arc::new(MockModel::new());
    let (reached, release) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let model: DynModel = Arc::new(GateModel {
        inner: mock.clone(),
        hold_on: 1,
        calls: Default::default(),
        reached: reached.clone(),
        release: release.clone(),
    });
    let rig = Rig::over(mock, model);
    let who = caller();
    rig.mock
        .push_error(refused(402, "over_limit", Park::UntilWoken))
        .push_text("answered");
    let task = rig.start(&who, "c").await;
    rig.wait(&who, &task.id, TaskState::InputRequired, None)
        .await;
    rig.backend
        .submit(who.clone(), resume("r-1"), Some(task.id.clone()), None)
        .await
        .expect("the first resume wakes it");
    tokio::time::timeout(Duration::from_secs(20), reached.notified())
        .await
        .expect("the woken call is made");
    let before = rig.written(&task).await;
    let err = rig
        .backend
        .submit(who.clone(), resume("r-2"), Some(task.id.clone()), None)
        .await
        .unwrap_err();
    assert!(is_not_parked(&err), "{err:?}");
    assert_eq!(rig.written(&task).await, before, "nothing written");
    release.notify_one();
    rig.wait(&who, &task.id, TaskState::Completed, Some("answered"))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let done = rig.backend.get(&who, &task.id).await.unwrap().unwrap();
    assert_eq!(done.status.state, TaskState::Completed);
    assert_eq!(rig.mock.requests().len(), 2);
    rig.stop().await;
}

/// A task waiting on a child parked on the model shows the child's reason — `input-required` for a
/// park until woken, `working` for one on a timer — takes a resume to the root and forwards it, and
/// a message whose words stay on the root (ADR 0067 decision 5). A resume once the child has
/// finished is refused, and writes nothing.
#[tokio::test]
async fn a_task_waiting_on_a_parked_child_shows_the_reason_and_takes_a_resume() {
    // Until woken: the root reads `input-required` with the child's reason.
    let rig = Rig::new();
    let who = caller();
    rig.mock
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "sub".into(),
            arguments: json!({}),
            ..Default::default()
        }])
        .push_error(refused(402, "over_limit", Park::UntilWoken))
        .push_text("the helper's answer")
        .push_text("all done");
    let task = rig.start(&who, "c").await;
    let run = RunId(task.id.parse().unwrap());
    let child = adam_runtime::child_run_id(run, "c1");
    let parent = rig
        .wait(
            &who,
            &task.id,
            TaskState::InputRequired,
            Some("the model gateway's limit is reached"),
        )
        .await;
    assert!(
        rig.runtime
            .view(child)
            .await
            .unwrap()
            .expect("the child")
            .waiting,
        "the child parked until woken"
    );
    assert_eq!(
        said(&parent),
        Some("the model gateway's limit is reached".into())
    );

    // A resume to the root is accepted, wakes the child through it, and the task works.
    let before = rig.written(&task).await;
    let resumed = rig
        .backend
        .submit(who.clone(), resume("r-1"), Some(task.id.clone()), None)
        .await
        .expect("a resume to the root");
    assert_eq!(resumed.status.state, TaskState::Working);
    assert!(
        rig.written(&task).await.0 > before.0,
        "the version moved: the resume was delivered"
    );
    rig.wait(&who, &task.id, TaskState::Completed, Some("all done"))
        .await;
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        texts(&requests[2]),
        texts(&requests[1]),
        "the child makes the same call"
    );
    let state = rig.runtime.state(run).await.unwrap().unwrap();
    let conversation: adam_llm_agent::Conversation = serde_json::from_value(state).unwrap();
    assert_eq!(
        conversation
            .messages
            .iter()
            .filter(|m| matches!(m, adam_model::Message::User { .. }))
            .count(),
        1,
        "the resume is no turn of the person's"
    );

    // A resume once the child has finished: the root is not parked on the model any more.
    let before = rig.written(&task).await;
    let err = rig
        .backend
        .submit(who.clone(), resume("r-2"), Some(task.id.clone()), None)
        .await
        .unwrap_err();
    assert!(is_not_parked(&err), "{err:?}");
    assert_eq!(rig.written(&task).await, before, "nothing written");
    rig.stop().await;

    // On a timer: the root reads `working` with the child's reason, takes a message and keeps its
    // words there.
    let rig = Rig::new();
    let who = caller();
    rig.mock
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "sub".into(),
            arguments: json!({}),
            ..Default::default()
        }])
        .push_error(refused(503, "unavailable", timer()))
        .push_text("the helper's answer")
        .push_text("all done");
    let task = rig.start(&who, "c").await;
    rig.wait(
        &who,
        &task.id,
        TaskState::Working,
        Some("the model gateway is busy"),
    )
    .await;
    rig.backend
        .submit(
            who.clone(),
            user("the gateway is back", "m-1"),
            Some(task.id.clone()),
            None,
        )
        .await
        .expect("a message to the root");
    rig.wait(&who, &task.id, TaskState::Completed, Some("all done"))
        .await;
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        texts(&requests[3]).contains(&"the gateway is back".to_owned()),
        "the words stayed on the root: {:?}",
        texts(&requests[3])
    );
    assert!(
        !texts(&requests[2]).contains(&"the gateway is back".to_owned()),
        "and never reached the child"
    );
    rig.stop().await;
}
