//! The tokens of an agent's model calls over A2A (`usage/v1`): a report per completed call, as a
//! `working` status update with no message, to a client that activated the extension and to no
//! other; a subagent's calls under its step; the same report under the same id to a client that
//! subscribes again; and the task's totals in its metadata when it ends or waits, whoever reads it.
//! Over a real `Runtime` on the in-memory store running real `LlmAgent`s with scripted models, and
//! over HTTP, both bindings.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, ExtensionConfig, TaskBackend,
    TaskEvent, USAGE_EXTENSION,
};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::MemoryStore;
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{DynModel, MockModel, ModelError, ModelResponse, ToolCall, ToolSpec, Usage};
use adam_runtime::{BroadcastSink, Runtime, RuntimeBuilder};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

fn calls_with(calls: Vec<ToolCall>, usage: Usage) -> ModelResponse {
    let mut response = ModelResponse::tool_calls(calls);
    response.usage = usage;
    response
}

fn text_with(text: &str, usage: Usage) -> ModelResponse {
    let mut response = ModelResponse::text(text);
    response.usage = usage;
    response
}

/// Answers at once.
struct Noop;

#[async_trait]
impl Tool for Noop {
    fn spec(&self) -> ToolSpec {
        spec("noop")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("ok"))
    }
}

/// Asks the person.
struct Ask;

#[async_trait]
impl Tool for Ask {
    fn spec(&self) -> ToolSpec {
        spec("ask")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input("Which one?"))
    }
}

/// Waits until the test lets it go.
struct Held(Arc<Notify>);

#[async_trait]
impl Tool for Held {
    fn spec(&self) -> ToolSpec {
        spec("held")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.0.notified().await;
        Ok(ToolOutput::text("let go"))
    }
}

/// Starts the agent `target` as a child run and waits for it, as `SubagentTool` does.
struct Spawn(&'static str);

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> ToolSpec {
        spec("explorer")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx.start_child(self.0, "look around").await?;
        Err(ToolError::AwaitRun { run })
    }
}

/// A model that says it is `openai` and that the alias `m` has a window of 4096 tokens.
fn model() -> Arc<MockModel> {
    Arc::new(
        MockModel::new()
            .with_provider("openai")
            .with_context_window("m", 4096),
    )
}

struct Rig {
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

fn rig(register: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder) -> Rig {
    let events = BroadcastSink::default();
    let runtime = register(Runtime::builder(Arc::new(MemoryStore::new())))
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, "llm")
        .with_poll_interval(Duration::from_millis(10));
    Rig { runtime, backend }
}

/// A rig whose agent `llm` talks to `model` (alias `m`) with the tools `tools`.
fn agent_rig(model: Arc<MockModel>, tools: Vec<Arc<dyn Tool>>) -> Rig {
    let model: DynModel = model;
    let mut builder = LlmAgent::builder("llm", model, "m");
    for tool in tools {
        builder = builder.dyn_tool(tool);
    }
    let agent = builder.build();
    rig(|b| b.agent(agent))
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Rig {
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

    /// The task as `GetTask` answers it, once it is in `state`.
    async fn task_in(&self, caller: &Caller, id: &str, state: TaskState) -> Task {
        for _ in 0..2000 {
            let task = self.backend.get(caller, id).await.unwrap().unwrap();
            if task.status.state == state {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("task {id} never reached {state:?}");
    }
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        self.handle.await.unwrap();
    }
}

fn user(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

fn activated() -> Caller {
    Caller::new("token-0").with_extensions([USAGE_EXTENSION])
}

fn plain() -> Caller {
    Caller::new("token-0")
}

/// The report a status update carries under the extension, if it carries one.
fn report_of(event: &TaskEvent) -> Option<Value> {
    match event {
        TaskEvent::Status(update) => update
            .metadata
            .as_ref()
            .and_then(|m| m.get(USAGE_EXTENSION))
            .cloned(),
        _ => None,
    }
}

/// Every event of `stream` until it ends.
async fn drain(mut stream: BoxStream<'static, Result<TaskEvent, BackendError>>) -> Vec<TaskEvent> {
    let mut events = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(20), stream.next()).await {
            Ok(Some(item)) => events.push(item.expect("not an error")),
            Ok(None) => return events,
            Err(_) => panic!("timed out; read so far: {events:#?}"),
        }
    }
}

/// Submit `text` as `caller`, subscribe, run the worker, and read the stream to its end.
async fn read(rig: &Rig, caller: Caller, text: &str) -> (String, Vec<TaskEvent>) {
    let task = rig
        .backend
        .submit(caller.clone(), user(text), None, None)
        .await
        .expect("submit");
    let stream = rig.backend.subscribe(&caller, &task.id);
    let worker = rig.worker();
    let events = drain(stream).await;
    worker.stop().await;
    (task.id, events)
}

/// The `usage/v1` totals in a task's metadata.
fn totals(task: &Task) -> Option<Value> {
    task.metadata
        .as_ref()
        .and_then(|m| m.get(USAGE_EXTENSION))
        .map(|u| u["totals"].clone())
}

// ---------------------------------------------------------------------------
// Call reports
// ---------------------------------------------------------------------------

/// Each model call is one report, on a `working` status with no message (the task's state and text do
/// not change), with the contract's members; the turn ends as it always did.
#[tokio::test]
async fn an_activated_client_reads_a_report_per_model_call_on_a_status_with_no_message() {
    let model = model();
    model
        .push_response(calls_with(
            vec![call("c1", "noop")],
            Usage::new(41_250, 812)
                .with_reasoning_tokens(300)
                .with_cached_input_tokens(38_000),
        ))
        .push_response(text_with("done", Usage::new(42_100, 9)));
    let rig = agent_rig(model, vec![Arc::new(Noop)]);
    let (task, events) = read(&rig, activated(), "go").await;

    let reports: Vec<(usize, Value)> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| report_of(e).map(|r| (i, r)))
        .collect();
    assert_eq!(reports.len(), 2, "{events:#?}");
    for (i, _) in &reports {
        let TaskEvent::Status(update) = &events[*i] else {
            unreachable!()
        };
        assert_eq!(update.status.state, TaskState::Working);
        assert!(update.status.message.is_none(), "{update:?}");
    }
    let first = &reports[0].1;
    assert!(
        first["call"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{task}-c0-"))
    );
    assert_eq!(
        first,
        &json!({"call": first["call"], "provider": "openai", "model": "m",
                "inputTokens": 41250, "outputTokens": 812, "totalTokens": 42062,
                "reasoningTokens": 300, "cachedInputTokens": 38000, "contextWindow": 4096})
    );
    assert!(first.get("stepId").is_none(), "the agent's own call");
    assert_eq!(reports[1].1["totalTokens"], json!(42109));
    assert_ne!(reports[0].1["call"], reports[1].1["call"]);
    // The stream still ends with the answer.
    let TaskEvent::Status(last) = events.last().unwrap() else {
        panic!("a status ends the stream");
    };
    assert_eq!(last.status.state, TaskState::Completed);
    assert_eq!(last.status.message.as_ref().unwrap().text(), Some("done"));
}

/// Without the activation nothing is said of the calls, and nothing else changes; the totals are on
/// the task either way.
#[tokio::test]
async fn a_client_that_did_not_activate_reads_no_report_and_the_task_still_has_its_totals() {
    let model = model();
    model.push_response(text_with("hello", Usage::new(10, 2)));
    let rig = agent_rig(model, Vec::new());
    let (task, events) = read(&rig, plain(), "hi").await;
    assert!(events.iter().all(|e| report_of(e).is_none()), "{events:#?}");
    assert!(
        events.iter().all(|e| match e {
            TaskEvent::Status(u) => u.metadata.is_none(),
            _ => true,
        }),
        "no event carries anything of the extension"
    );
    let done = rig.task_in(&plain(), &task, TaskState::Completed).await;
    assert_eq!(
        totals(&done),
        Some(json!([{"provider": "openai", "model": "m",
                      "inputTokens": 10, "outputTokens": 2, "totalTokens": 12}]))
    );
}

/// A subagent's calls are reported on the task under the step of the call that started it, and the
/// task's totals hold them beside the agent's own, one entry per model.
#[tokio::test]
async fn a_subagents_calls_carry_its_step_and_are_in_the_tasks_totals() {
    let (parent, child) = (model(), Arc::new(MockModel::new().with_provider("openai")));
    parent
        .push_response(calls_with(
            vec![call("call_2", "explorer")],
            Usage::new(100, 10),
        ))
        .push_response(text_with("found it", Usage::new(200, 20)));
    child
        .push_response(calls_with(vec![call("k1", "noop")], Usage::new(7, 1)))
        .push_response(text_with("it is there", Usage::new(8, 2)));
    let (parent_model, child_model): (DynModel, DynModel) = (parent, child);
    let llm = LlmAgent::builder("llm", parent_model, "m")
        .tool(Spawn("llm/explorer"))
        .build();
    let explorer = LlmAgent::builder("llm/explorer", child_model, "small")
        .tool(Noop)
        .build();
    let rig = rig(|b| b.agent(llm).agent(explorer));
    let (task, events) = read(&rig, activated(), "where is it?").await;

    let reports: Vec<Value> = events.iter().filter_map(report_of).collect();
    assert_eq!(reports.len(), 4, "{reports:#?}");
    let (own, sub): (Vec<&Value>, Vec<&Value>) =
        reports.iter().partition(|r| r.get("stepId").is_none());
    assert_eq!(own.len(), 2);
    assert_eq!(sub.len(), 2);
    for report in &sub {
        assert_eq!(report["stepId"], "tool:call_2", "{report}");
        assert_eq!(report["model"], "small");
        assert!(report.get("contextWindow").is_none());
    }
    let done = rig.task_in(&activated(), &task, TaskState::Completed).await;
    assert_eq!(
        totals(&done),
        Some(json!([
            {"provider": "openai", "model": "m", "inputTokens": 300, "outputTokens": 30, "totalTokens": 330},
            {"provider": "openai", "model": "small", "inputTokens": 15, "outputTokens": 3, "totalTokens": 18},
        ]))
    );
}

/// A client that subscribes again while the task works hears the recent reports again (the replay of
/// live events), **under the same ids**, so the orchestrator logs each call once.
#[tokio::test]
async fn a_resubscribe_hears_a_report_again_under_the_same_id() {
    let model = model();
    model
        .push_response(calls_with(vec![call("c1", "held")], Usage::new(5, 5)))
        .push_response(text_with("done", Usage::new(6, 6)));
    let gate = Arc::new(Notify::new());
    let rig = agent_rig(model.clone(), vec![Arc::new(Held(gate.clone()))]);
    let caller = activated();
    let task = rig
        .backend
        .submit(caller.clone(), user("go"), None, None)
        .await
        .unwrap();
    let worker = rig.worker();
    // The first call is made and the tool holds the turn: the report is among the recent events.
    for _ in 0..2000 {
        if model.requests().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut ids = Vec::new();
    for _ in 0..2 {
        let mut stream = rig.backend.subscribe(&caller, &task.id);
        let id = loop {
            let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .expect("an event in time")
                .expect("an event")
                .expect("not an error");
            if let Some(report) = report_of(&event) {
                break report["call"].as_str().unwrap().to_owned();
            }
        };
        ids.push(id);
    }
    assert_eq!(ids[0], ids[1], "the same call, the same id");
    assert!(ids[0].starts_with(&format!("{}-c0-", task.id)));
    gate.notify_one();
    rig.task_in(&caller, &task.id, TaskState::Completed).await;
    worker.stop().await;
}

// ---------------------------------------------------------------------------
// The task's totals
// ---------------------------------------------------------------------------

/// A task that waits for its caller carries the totals so far; once it is answered it is working and
/// carries none; when it ends, the totals cover every call, the ones before the question too.
#[tokio::test]
async fn a_task_that_waits_carries_its_totals_and_once_resumed_they_cover_every_call() {
    let model = model();
    model
        .push_response(calls_with(vec![call("c1", "ask")], Usage::new(10, 1)))
        .push_response(text_with("thanks", Usage::new(20, 2)));
    let rig = agent_rig(model, vec![Arc::new(Ask)]);
    let caller = plain();
    let task = rig
        .backend
        .submit(caller.clone(), user("go"), None, None)
        .await
        .unwrap();
    assert!(task.metadata.is_none(), "a new task has no totals");
    let worker = rig.worker();
    let waiting = rig
        .task_in(&caller, &task.id, TaskState::InputRequired)
        .await;
    assert_eq!(
        totals(&waiting),
        Some(json!([{"provider": "openai", "model": "m",
                      "inputTokens": 10, "outputTokens": 1, "totalTokens": 11}]))
    );
    let mut answer = user("the first");
    answer.task_id = Some(task.id.clone());
    let resumed = rig
        .backend
        .submit(
            caller.clone(),
            answer,
            Some(task.id.clone()),
            Some(task.context_id.clone()),
        )
        .await
        .unwrap();
    assert_eq!(resumed.status.state, TaskState::Working);
    assert!(resumed.metadata.is_none(), "a working task says no totals");
    let done = rig.task_in(&caller, &task.id, TaskState::Completed).await;
    assert_eq!(
        totals(&done),
        Some(json!([{"provider": "openai", "model": "m",
                      "inputTokens": 30, "outputTokens": 3, "totalTokens": 33}]))
    );
    // `ListTasks` reads the same task.
    let page = rig
        .backend
        .list(&caller, &adam_a2a::TaskQuery::new())
        .await
        .unwrap();
    assert_eq!(totals(&page.tasks[0]), totals(&done));
    worker.stop().await;
}

/// A task that failed, or was canceled while it waited, carries the totals of the calls it made.
#[tokio::test]
async fn a_failed_or_canceled_task_carries_its_totals() {
    // Failed: the second call is refused for good.
    let model = model();
    model
        .push_response(calls_with(vec![call("c1", "noop")], Usage::new(10, 1)))
        .push_error(ModelError::Auth("the key was revoked".into()));
    let rig = agent_rig(model, vec![Arc::new(Noop)]);
    let caller = plain();
    let task = rig
        .backend
        .submit(caller.clone(), user("go"), None, None)
        .await
        .unwrap();
    let worker = rig.worker();
    let failed = rig.task_in(&caller, &task.id, TaskState::Failed).await;
    assert_eq!(
        totals(&failed),
        Some(json!([{"provider": "openai", "model": "m",
                      "inputTokens": 10, "outputTokens": 1, "totalTokens": 11}]))
    );
    worker.stop().await;

    // Canceled: the task waited for an answer and the client gave up.
    let asking = self::model();
    asking.push_response(calls_with(vec![call("c1", "ask")], Usage::new(3, 4)));
    let rig = agent_rig(asking, vec![Arc::new(Ask)]);
    let task = rig
        .backend
        .submit(caller.clone(), user("go"), None, None)
        .await
        .unwrap();
    let worker = rig.worker();
    rig.task_in(&caller, &task.id, TaskState::InputRequired)
        .await;
    let canceled = rig.backend.cancel(&caller, &task.id).await.unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert_eq!(
        totals(&canceled),
        Some(json!([{"provider": "openai", "model": "m",
                      "inputTokens": 3, "outputTokens": 4, "totalTokens": 7}]))
    );
    worker.stop().await;
}

// ---------------------------------------------------------------------------
// Over HTTP, both bindings
// ---------------------------------------------------------------------------

/// A whole number, as the SDK writes a number of metadata (a float that is whole).
fn whole(value: &Value) -> u64 {
    let n = value
        .as_f64()
        .unwrap_or_else(|| panic!("a number: {value}"));
    assert_eq!(n.fract(), 0.0, "{value}");
    n as u64
}

async fn serve(rig: &Rig) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let card = AgentCardConfig::new(
        "llm",
        "Counts its tokens",
        format!("http://{addr}/").parse().unwrap(),
        "0.1.0",
    )
    .with_extension(ExtensionConfig::usage());
    let app = A2aServer::router(
        card,
        Arc::new(rig.backend.clone()),
        AuthConfig::AllowAnonymous,
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// `method path` with `body` and, when given, the `A2A-Extensions` header: the response head and
/// body.
async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
    header: Option<&str>,
) -> (String, String) {
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(header) = header {
        request.push_str(&format!("A2A-Extensions: {header}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut bytes))
        .await
        .expect("the response ends in time")
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a response");
    (head.to_owned(), body.to_owned())
}

/// The `data:` events of an SSE body.
fn frames(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| serde_json::from_str(d.trim()).expect("JSON"))
        .collect()
}

/// Each binding's stream, with and without the header: the reports reach only the client that named
/// the extension, as status updates with no message and whole numbers, and `GetTask` says the totals
/// on both bindings.
#[tokio::test]
async fn over_http_both_bindings_send_the_reports_to_an_activated_client_and_say_the_totals() {
    for (rest, header) in [
        (false, Some(USAGE_EXTENSION)),
        (true, Some(USAGE_EXTENSION)),
        (false, None),
        (true, None),
    ] {
        let model = model();
        model
            .push_response(calls_with(
                vec![call("c1", "noop")],
                Usage::new(41_250, 812),
            ))
            .push_response(text_with("done", Usage::new(42_000, 20)));
        let rig = agent_rig(model, vec![Arc::new(Noop)]);
        let addr = serve(&rig).await;
        let message = json!({"messageId": format!("m-{rest}-{}", header.is_some()),
                             "role": "ROLE_USER", "parts": [{"text": "go"}]});
        let worker = rig.worker();
        let (head, body) = if rest {
            http(
                addr,
                "POST",
                "/message:stream",
                Some(&json!({"message": message})),
                header,
            )
            .await
        } else {
            let rpc = json!({"jsonrpc": "2.0", "id": "1", "method": "SendStreamingMessage",
                             "params": {"message": message}});
            http(addr, "POST", "/", Some(&rpc), header).await
        };
        let events: Vec<Value> = frames(&body)
            .into_iter()
            .map(|f| if rest { f } else { f["result"].clone() })
            .collect();
        let updates: Vec<&Value> = events
            .iter()
            .filter_map(|e| e.get("statusUpdate"))
            .collect();
        let reports: Vec<&Value> = updates
            .iter()
            .filter_map(|u| u.get("metadata").and_then(|m| m.get(USAGE_EXTENSION)))
            .collect();
        if header.is_none() {
            assert!(reports.is_empty(), "rest {rest}: {body}");
            assert!(!head.to_lowercase().contains("a2a-extensions"), "{head}");
        } else {
            assert!(
                head.to_lowercase()
                    .contains(&format!("a2a-extensions: {USAGE_EXTENSION}").to_lowercase()),
                "rest {rest}: the response names what it activated: {head}"
            );
            assert_eq!(reports.len(), 2, "rest {rest}: {body}");
            for update in updates.iter().filter(|u| {
                u.get("metadata")
                    .is_some_and(|m| m.get(USAGE_EXTENSION).is_some())
            }) {
                assert_eq!(update["status"]["state"], "TASK_STATE_WORKING", "{update}");
                assert!(update["status"].get("message").is_none(), "{update}");
            }
            let first = reports[0];
            assert_eq!(whole(&first["inputTokens"]), 41_250);
            assert_eq!(whole(&first["outputTokens"]), 812);
            assert_eq!(whole(&first["totalTokens"]), 42_062);
            assert_eq!(whole(&first["contextWindow"]), 4096);
            assert_eq!(first["model"], "m");
            assert_eq!(first["provider"], "openai");
            assert!(first["call"].as_str().unwrap().contains("-c0-"));
        }
        let done = updates.last().expect("a status");
        assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED");
        let task_id = done["taskId"].as_str().unwrap().to_owned();

        // `GetTask` on the binding that streamed, whoever asks: the totals, in whole numbers.
        let task = if rest {
            let (_, body) = http(addr, "GET", &format!("/tasks/{task_id}"), None, None).await;
            serde_json::from_str::<Value>(&body).unwrap()
        } else {
            let rpc = json!({"jsonrpc": "2.0", "id": "2", "method": "GetTask",
                             "params": {"id": task_id}});
            let (_, body) = http(addr, "POST", "/", Some(&rpc), None).await;
            serde_json::from_str::<Value>(&body).unwrap()["result"].clone()
        };
        let totals = &task["metadata"][USAGE_EXTENSION]["totals"];
        assert_eq!(
            totals.as_array().map(Vec::len),
            Some(1),
            "rest {rest}: {task}"
        );
        assert_eq!(whole(&totals[0]["inputTokens"]), 83_250);
        assert_eq!(whole(&totals[0]["outputTokens"]), 832);
        assert_eq!(whole(&totals[0]["totalTokens"]), 84_082);
        assert_eq!(totals[0]["model"], "m");
        // The task's history and status are what they always were: the reports add no message.
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
        worker.stop().await;
    }
}
