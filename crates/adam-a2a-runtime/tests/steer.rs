//! `steer/v1` on the A2A backend: a message that names a `submitted` or `working` task and
//! activates the extension is delivered to the open task and read at its next step, by a real
//! `LlmAgent` on a real `Runtime`; without the activation it is refused as before; a terminal task
//! refuses with A2A's error; and a message sent while the model writes the final answer is answered.
//! The last case goes through the real A2A server over HTTP, as the orchestration layer does.
//!
//! The first model call of a task is held at a gate by several cases, and that is the case that
//! matters most: a whole turn commits once, so until it ends nothing is committed, and the task
//! has to read `working` from the moment a worker takes it (`a_task_a_worker_has_taken_is_working_...`),
//! in a read and as an event to a subscriber, or a message sent meanwhile would be refused or
//! wait for the next turn.
//!
//! Run against `MemoryStore` always and against PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set.
//! Nothing sleeps for a fixed time: the model and the tool are held at gates, so a message is
//! always delivered *during* the step.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, ExtensionConfig, STEER_EXTENSION,
    TaskBackend, TaskEvent,
};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{
    DynModel, Message as ModelMessage, MockModel, ModelClient, ModelDelta, ModelError,
    ModelRequest, ModelResponse, ToolCall, ToolSpec,
};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn stores() -> Vec<(&'static str, DynStore)> {
    let mut all: Vec<(&'static str, DynStore)> = vec![("memory", Arc::new(MemoryStore::new()))];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        adam_core::Store::migrate(&store).await.expect("migrate");
        all.push(("postgres", Arc::new(store)));
    }
    all
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

/// A point two tasks meet at: the one that reaches it says so, and waits to be let through.
#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

impl Gate {
    async fn wait_reached(&self) {
        tokio::time::timeout(Duration::from_secs(20), self.reached.notified())
            .await
            .expect("timed out waiting for the gate to be reached");
    }
}

/// Wraps a model: the `hold_on`-th call (0-based) waits at the gate before it is answered.
struct GateModel {
    inner: Arc<MockModel>,
    hold_on: Option<usize>,
    calls: AtomicUsize,
    gate: Arc<Gate>,
}

impl GateModel {
    async fn hold(&self) {
        let n = self.calls.fetch_add(1, SeqCst);
        if self.hold_on == Some(n) {
            self.gate.reached.notify_one();
            self.gate.release.notified().await;
        }
    }
}

#[async_trait]
impl ModelClient for GateModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.hold().await;
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.hold().await;
        self.inner.stream(req).await
    }
}

/// A tool that waits at its gate, then answers `released`.
struct GateTool {
    gate: Arc<Gate>,
}

#[async_trait]
impl Tool for GateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "gate".into(),
            description: "waits".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.gate.reached.notify_one();
        self.gate.release.notified().await;
        Ok(ToolOutput::text("released"))
    }
}

fn gate_call() -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: "gate".into(),
        arguments: json!({}),
    }
}

/// An agent (a real `LlmAgent` on a mock model), the runtime that steps it and the backend that
/// serves it over `store`.
struct Rig {
    mock: Arc<MockModel>,
    gate: Arc<Gate>,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

impl Rig {
    /// `hold_model_on`: the model call that waits at the gate. `with_tool`: the agent has the gate tool.
    fn new(store: &DynStore, hold_model_on: Option<usize>, with_tool: bool) -> Self {
        Self::polling(store, hold_model_on, with_tool, Duration::from_millis(10))
    }

    /// As [`Rig::new`], with the backend re-reading the run every `poll` (its subscriptions go by
    /// the live events in between).
    fn polling(
        store: &DynStore,
        hold_model_on: Option<usize>,
        with_tool: bool,
        poll: Duration,
    ) -> Self {
        let name = uniq("steer");
        let mock = Arc::new(MockModel::new());
        let gate = Arc::new(Gate::default());
        let model: DynModel = Arc::new(GateModel {
            inner: mock.clone(),
            hold_on: hold_model_on,
            calls: AtomicUsize::new(0),
            gate: gate.clone(),
        });
        let mut builder = LlmAgent::builder(&name, model, "m");
        if with_tool {
            builder = builder.tool(GateTool { gate: gate.clone() });
        }
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store.clone())
            .agent(builder.build())
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend =
            RuntimeTaskBackend::new(runtime.clone(), events, name).with_poll_interval(poll);
        Self {
            mock,
            gate,
            runtime,
            backend,
        }
    }

    fn worker(&self) -> Worker {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.runtime.clone();
        let handle = tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        Worker { stop, handle }
    }

    async fn pending(&self, task: &Task) -> usize {
        let run = RunId(task.id.parse().unwrap());
        self.runtime.view(run).await.unwrap().unwrap().pending_inbox
    }
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("worker stops in time")
            .expect("worker task");
    }
}

/// A caller of its own (the store may be shared with other cases), with or without the steer
/// extension activated on the request.
fn caller(unique: &str) -> Caller {
    Caller::new(format!("token-{unique}"))
}

fn steering(unique: &str) -> Caller {
    caller(unique).with_extensions([STEER_EXTENSION])
}

fn user(text: &str, id: &str) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m
}

async fn wait_state(rig: &Rig, who: &Caller, id: &str, state: TaskState) -> Task {
    for _ in 0..2000 {
        let task = rig
            .backend
            .get(who, id)
            .await
            .expect("get")
            .expect("task exists");
        if task.status.state == state {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

/// The next event of a subscription, within a bound that is for a bug and never relied on.
async fn next(events: &mut BoxStream<'static, Result<TaskEvent, BackendError>>) -> TaskEvent {
    tokio::time::timeout(Duration::from_secs(20), events.next())
        .await
        .expect("timed out waiting for an event")
        .expect("the stream ended early")
        .expect("the stream item was an error")
}

fn answer(task: &Task) -> String {
    task.status
        .message
        .as_ref()
        .and_then(|m| m.text())
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// Activated: the message is delivered to the working task, the answer is the task still working,
/// and the model's next request carries the text, after the result of the tool that was running. The
/// same message sent twice (the orchestration layer repeats one after a lost lease) is read once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steered_message_is_delivered_to_the_working_task_and_the_model_reads_it() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, None, true);
        rig.mock
            .push_tool_calls(vec![gate_call()])
            .push_text("blue it is");
        let who = uniq("alice");
        let worker = rig.worker();

        let task = rig
            .backend
            .submit(caller(&who), user("pick a colour", "m-0"), None, None)
            .await
            .unwrap();
        rig.gate.wait_reached().await;

        for _ in 0..2 {
            let answered = rig
                .backend
                .submit(
                    steering(&who),
                    user("you were wrong since line 1", "m-steer"),
                    Some(task.id.clone()),
                    Some(task.context_id.clone()),
                )
                .await
                .unwrap();
            assert_eq!(answered.id, task.id, "{backend}: the same task");
            assert_eq!(
                answered.status.state,
                TaskState::Working,
                "{backend}: non-terminal"
            );
        }
        rig.gate.release.notify_one();
        let done = wait_state(&rig, &caller(&who), &task.id, TaskState::Completed).await;
        worker.stop().await;

        assert_eq!(answer(&done), "blue it is", "{backend}");
        let requests = rig.mock.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        let steered = ModelMessage::user_text("you were wrong since line 1");
        assert_eq!(
            requests[1]
                .messages
                .iter()
                .filter(|m| **m == steered)
                .count(),
            1,
            "{backend}: the model's next request carries the steered text, once"
        );
        assert_eq!(requests[1].messages.last(), Some(&steered), "{backend}");
        assert_eq!(
            rig.pending(&done).await,
            0,
            "{backend}: nothing is left unread"
        );
    }
}

/// A task nobody has stepped yet is `submitted`: it takes a steer too, and the first model request
/// has both messages, in the order they came.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_submitted_task_takes_a_steer_before_its_first_step() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, None, false);
        rig.mock.push_text("noted");
        let who = uniq("alice");
        let task = rig
            .backend
            .submit(caller(&who), user("first", "m-0"), None, None)
            .await
            .unwrap();
        assert_eq!(task.status.state, TaskState::Submitted, "{backend}");

        rig.backend
            .submit(
                steering(&who),
                user("second", "m-1"),
                Some(task.id.clone()),
                None,
            )
            .await
            .unwrap();
        let worker = rig.worker();
        wait_state(&rig, &caller(&who), &task.id, TaskState::Completed).await;
        worker.stop().await;

        assert_eq!(
            rig.mock.requests()[0].messages,
            [
                ModelMessage::user_text("first"),
                ModelMessage::user_text("second")
            ],
            "{backend}"
        );
    }
}

/// Not activated: refused exactly as before, and the task goes on without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_extension_a_message_for_a_working_task_is_refused_as_before() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, None, true);
        rig.mock
            .push_tool_calls(vec![gate_call()])
            .push_text("done");
        let who = uniq("alice");
        let worker = rig.worker();
        let task = rig
            .backend
            .submit(caller(&who), user("go", "m-0"), None, None)
            .await
            .unwrap();
        rig.gate.wait_reached().await;

        let err = rig
            .backend
            .submit(
                caller(&who),
                user("you were wrong", "m-1"),
                Some(task.id.clone()),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BackendError::InvalidParams(m) if m.contains("cannot take a follow-up")),
            "{backend}: {err:?}"
        );
        assert_eq!(
            rig.pending(&task).await,
            0,
            "{backend}: nothing was delivered"
        );

        rig.gate.release.notify_one();
        let done = wait_state(&rig, &caller(&who), &task.id, TaskState::Completed).await;
        worker.stop().await;
        assert_eq!(answer(&done), "done", "{backend}");
        assert!(
            rig.mock.requests()[1]
                .messages
                .iter()
                .all(|m| *m != ModelMessage::user_text("you were wrong")),
            "{backend}: the model never saw it"
        );
    }
}

/// A terminal task refuses with A2A's error, whether or not the extension is activated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_task_refuses_with_the_specs_error() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, None, false);
        rig.mock.push_text("done");
        let who = uniq("alice");
        let worker = rig.worker();
        let task = rig
            .backend
            .submit(caller(&who), user("go", "m-0"), None, None)
            .await
            .unwrap();
        wait_state(&rig, &caller(&who), &task.id, TaskState::Completed).await;
        worker.stop().await;

        for who in [caller(&who), steering(&who)] {
            let err = rig
                .backend
                .submit(
                    who,
                    user("too late", "m-1"),
                    Some(task.id.clone()),
                    Some(task.context_id.clone()),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&err, BackendError::UnsupportedOperation(m) if m.contains("Completed")),
                "{backend}: {err:?}"
            );
            assert_eq!(a2a::A2AError::from(err).code, -32004, "{backend}");
        }
        assert_eq!(rig.pending(&task).await, 0, "{backend}");
    }
}

/// A canceled task is terminal too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_canceled_task_refuses_a_steer() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, None, true);
        rig.mock.push_tool_calls(vec![gate_call()]);
        let who = uniq("alice");
        let worker = rig.worker();
        let task = rig
            .backend
            .submit(caller(&who), user("go", "m-0"), None, None)
            .await
            .unwrap();
        rig.gate.wait_reached().await;
        rig.backend.cancel(&caller(&who), &task.id).await.unwrap();
        rig.gate.release.notify_one();
        worker.stop().await;

        let err = rig
            .backend
            .submit(
                steering(&who),
                user("too late", "m-1"),
                Some(task.id.clone()),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, BackendError::UnsupportedOperation(_)),
            "{backend}: {err:?}"
        );
    }
}

/// An unknown task, another caller's task, and another context are not found, for a steer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_for_an_unknown_foreign_or_other_context_task_is_not_found() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, None, true);
        rig.mock
            .push_tool_calls(vec![gate_call()])
            .push_text("done");
        let (mine, theirs) = (uniq("alice"), uniq("bob"));
        let worker = rig.worker();
        let task = rig
            .backend
            .submit(caller(&mine), user("go", "m-0"), None, Some("c1".into()))
            .await
            .unwrap();
        rig.gate.wait_reached().await;

        let attempts = [
            (steering(&mine), RunId::new().to_string(), None),
            (steering(&theirs), task.id.clone(), None),
            (steering(&mine), task.id.clone(), Some("c2".to_owned())),
            (steering(&mine), "not-a-task-id".to_owned(), None),
        ];
        for (who, id, context) in attempts {
            let err = rig
                .backend
                .submit(who, user("steer", "m-1"), Some(id.clone()), context)
                .await
                .unwrap_err();
            assert!(
                matches!(err, BackendError::TaskNotFound(_)),
                "{backend}: {id}: {err:?}"
            );
        }
        assert_eq!(rig.pending(&task).await, 0, "{backend}");

        rig.gate.release.notify_one();
        wait_state(&rig, &caller(&mine), &task.id, TaskState::Completed).await;
        worker.stop().await;
    }
}

/// A message sent while the model writes the final answer is answered: the task does not complete
/// with the first answer, another model turn reads the message, and the final answer reflects it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_during_the_final_model_call_is_answered() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, Some(0), false);
        rig.mock
            .push_text("the colour is red")
            .push_text("the colour is blue");
        let who = uniq("alice");
        let worker = rig.worker();
        let task = rig
            .backend
            .submit(caller(&who), user("which colour?", "m-0"), None, None)
            .await
            .unwrap();
        rig.gate.wait_reached().await;

        let answered = rig
            .backend
            .submit(
                steering(&who),
                user("I meant the sea", "m-1"),
                Some(task.id.clone()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(answered.status.state, TaskState::Working, "{backend}");
        rig.gate.release.notify_one();
        let done = wait_state(&rig, &caller(&who), &task.id, TaskState::Completed).await;
        worker.stop().await;

        assert_eq!(answer(&done), "the colour is blue", "{backend}");
        assert_eq!(
            rig.mock.requests()[1].messages,
            [
                ModelMessage::user_text("which colour?"),
                ModelMessage::assistant_text("the colour is red"),
                ModelMessage::user_text("I meant the sea"),
            ],
            "{backend}"
        );
        assert_eq!(rig.pending(&done).await, 0, "{backend}");
    }
}

/// A worker that has taken a task makes it `working`, though nothing is committed until the turn
/// ends and the model is still at its first call. A message sent now is steered into the task and
/// read by the next model turn, exactly as it was when the task read `submitted`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_a_worker_has_taken_is_working_before_its_first_commit_and_takes_a_steer() {
    for (backend, store) in stores().await {
        let rig = Rig::new(&store, Some(0), false);
        rig.mock.push_text("red").push_text("blue");
        let who = uniq("alice");
        let task = rig
            .backend
            .submit(caller(&who), user("which colour?", "m-0"), None, None)
            .await
            .unwrap();
        // Nobody has taken it yet: no worker is running.
        let before = rig
            .backend
            .get(&caller(&who), &task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.status.state, TaskState::Submitted, "{backend}");

        let worker = rig.worker();
        rig.gate.wait_reached().await;
        let during = rig
            .backend
            .get(&caller(&who), &task.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(during.status.state, TaskState::Working, "{backend}");
        let view = rig
            .runtime
            .view(RunId(task.id.parse().unwrap()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (view.version, view.claimed),
            (1, true),
            "{backend}: nothing is committed, and a worker holds the run"
        );

        let answered = rig
            .backend
            .submit(
                steering(&who),
                user("I meant the sea", "m-1"),
                Some(task.id.clone()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(answered.status.state, TaskState::Working, "{backend}");
        rig.gate.release.notify_one();
        let done = wait_state(&rig, &caller(&who), &task.id, TaskState::Completed).await;
        worker.stop().await;
        assert_eq!(answer(&done), "blue", "{backend}");
        assert_eq!(
            rig.mock.requests()[1].messages.last(),
            Some(&ModelMessage::user_text("I meant the sea")),
            "{backend}"
        );
    }
}

/// A subscriber hears `working` when the worker takes the task, before the first commit, from the
/// live event and not from a poll: the backend here re-reads only once a minute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_is_told_working_when_a_worker_takes_the_task() {
    for (backend, store) in stores().await {
        let rig = Rig::polling(&store, Some(0), false, Duration::from_secs(60));
        rig.mock.push_text("done");
        let who = uniq("alice");
        let task = rig
            .backend
            .submit(caller(&who), user("go", "m-0"), None, None)
            .await
            .unwrap();
        let mut events = rig.backend.subscribe(&caller(&who), &task.id);
        let first = next(&mut events).await;
        assert!(
            matches!(&first, TaskEvent::Snapshot(t) if t.status.state == TaskState::Submitted),
            "{backend}: {first:?}"
        );

        // The model is held at its first call, so this is before anything is committed.
        let worker = rig.worker();
        let second = next(&mut events).await;
        assert!(
            matches!(&second, TaskEvent::Status(u) if u.status.state == TaskState::Working),
            "{backend}: {second:?}"
        );
        let view = rig
            .runtime
            .view(RunId(task.id.parse().unwrap()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.version, 1, "{backend}: still nothing committed");

        rig.gate.release.notify_one();
        let rest = tokio::time::timeout(Duration::from_secs(20), async {
            let mut labels = Vec::new();
            while let Some(event) = events.next().await {
                labels.push(match event.expect("an event") {
                    TaskEvent::Status(u) => u.status.state,
                    other => panic!("{backend}: unexpected {other:?}"),
                });
            }
            labels
        })
        .await;
        worker.stop().await;
        // The agent's own progress events are `working` too; what matters is that the task never
        // falls back, and that it ends. The poll is a minute away, so the end is the live status.
        let rest = rest.expect("the stream ends");
        assert_eq!(rest.last(), Some(&TaskState::Completed), "{backend}");
        assert!(
            rest[..rest.len() - 1]
                .iter()
                .all(|s| *s == TaskState::Working),
            "{backend}: {rest:?}"
        );
    }
}

// ------------------------------------------------------------------ over HTTP

/// A raw JSON-RPC `SendMessage` over a socket, so that the `A2A-Extensions` header (and not
/// `message.extensions`) is what activates: the status code and the body.
async fn raw_send(
    addr: std::net::SocketAddr,
    headers: &[(&str, &str)],
    message: Value,
) -> (u16, Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = json!({
        "jsonrpc": "2.0", "id": "1", "method": "SendMessage",
        "params": {"message": message, "configuration": {"returnImmediately": true}}
    })
    .to_string();
    let mut request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
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

/// The whole path, as the orchestration layer takes it: a task is started with a streaming send;
/// while it works, a streaming send for the same task activates `steer/v1` and its first event is
/// the task, still working; the stream is dropped; the task reads the message at its next step and
/// completes on the stream that started it. Over the real A2A server and the official client.
/// Without the activation the same request is a JSON-RPC error, and for a finished task it is
/// A2A's `UnsupportedOperationError`. The activation by header alone works too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steering_over_http_with_the_official_client() {
    use a2a::{SendMessageRequest, StreamResponse};
    use a2a_client::A2AClientFactory;
    use a2a_client::agent_card::AgentCardResolver;
    use a2a_client::auth::AuthInterceptor;
    use secrecy::SecretString;

    for (backend, store) in stores().await {
        let rig = Rig::new(&store, None, true);
        rig.mock
            .push_tool_calls(vec![gate_call()])
            .push_text("blue it is");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let card = AgentCardConfig::new(
            "steerable",
            "A steerable test agent",
            format!("http://{addr}/").parse().unwrap(),
            "0.1.0",
        )
        .with_extension(ExtensionConfig::steer());
        let app = A2aServer::router(
            card,
            Arc::new(rig.backend.clone()),
            AuthConfig::BearerTokens(vec![SecretString::from("t0")]),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let worker = rig.worker();

        let resolved = AgentCardResolver::new(None)
            .resolve(&format!("http://{addr}"))
            .await
            .unwrap();
        assert!(
            resolved
                .capabilities
                .extensions
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|e| e.uri == STEER_EXTENSION),
            "{backend}: the card lists steer/v1"
        );
        let client = A2AClientFactory::builder()
            .with_interceptor(Arc::new(AuthInterceptor::bearer("t0")))
            .build()
            .create_from_card(&resolved)
            .await
            .unwrap();
        let request = |message: Message| SendMessageRequest {
            message,
            configuration: None,
            metadata: None,
            tenant: None,
        };

        // The task that works: its stream stays open.
        let mut results = client
            .send_streaming_message(&request(user("pick a colour", "m-0")))
            .await
            .unwrap();
        let Some(Ok(StreamResponse::Task(task))) = results.next().await else {
            panic!("{backend}: the stream starts with the task")
        };
        rig.gate.wait_reached().await;

        // The steer: a streaming send naming the task, activating the extension in the message.
        let mut steer = user("you were wrong since line 1", "m-steer");
        steer.task_id = Some(task.id.clone());
        steer.context_id = Some(task.context_id.clone());
        steer.extensions = Some(vec![STEER_EXTENSION.to_owned()]);
        let mut first = client
            .send_streaming_message(&request(steer))
            .await
            .unwrap();
        let Some(Ok(StreamResponse::Task(answered))) = first.next().await else {
            panic!("{backend}: the first event of a steer is the task")
        };
        assert_eq!(answered.id, task.id, "{backend}");
        assert_eq!(answered.status.state, TaskState::Working, "{backend}");
        drop(first);

        // Not activated: an error, and nothing is delivered.
        let mut plain = user("not activated", "m-plain");
        plain.task_id = Some(task.id.clone());
        let err = client
            .send_streaming_message(&request(plain))
            .await
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.code, a2a::error_code::INVALID_PARAMS, "{backend}");

        // Activated by the header alone (no `message.extensions`), over a raw request.
        let mut by_header = json!({
            "messageId": "m-header", "role": "ROLE_USER", "taskId": task.id,
            "parts": [{"text": "and by header"}],
        });
        let (status, body) = raw_send(
            addr,
            &[
                ("Authorization", "Bearer t0"),
                ("A2A-Extensions", STEER_EXTENSION),
            ],
            by_header.clone(),
        )
        .await;
        assert_eq!(status, 200, "{backend}: {body}");
        assert!(body.get("error").is_none(), "{backend}: {body}");
        // Without the header, the same message is refused.
        by_header["messageId"] = json!("m-header-2");
        let (_, body) = raw_send(addr, &[("Authorization", "Bearer t0")], by_header).await;
        assert_eq!(body["error"]["code"], -32602, "{backend}: {body}");

        // The task reads them at its next step and completes on the stream that started it.
        rig.gate.release.notify_one();
        let mut last = None;
        while let Some(item) = results.next().await {
            if let StreamResponse::StatusUpdate(update) = item.unwrap() {
                last = Some(update.status);
            }
        }
        let last = last.expect("the stream ends with a status");
        assert_eq!(last.state, TaskState::Completed, "{backend}");
        assert_eq!(
            last.message.as_ref().and_then(|m| m.text()),
            Some("blue it is"),
            "{backend}"
        );
        let request2 = &rig.mock.requests()[1];
        let said: Vec<&ModelMessage> = request2.messages.iter().skip(3).collect();
        assert_eq!(
            said,
            [
                &ModelMessage::user_text("you were wrong since line 1"),
                &ModelMessage::user_text("and by header"),
            ],
            "{backend}: the steered texts, in order, after the tool result"
        );

        // A finished task: A2A's error, activated or not.
        let mut late = user("too late", "m-late");
        late.task_id = Some(task.id.clone());
        late.extensions = Some(vec![STEER_EXTENSION.to_owned()]);
        let err = client
            .send_streaming_message(&request(late))
            .await
            .map(|_| ())
            .unwrap_err();
        assert_eq!(
            err.code,
            a2a::error_code::UNSUPPORTED_OPERATION,
            "{backend}"
        );
        worker.stop().await;
    }
}
