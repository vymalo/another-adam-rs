//! Remote sessions (ADR 0045, slice 5): `subagent_start` and the other session tools on a subagent
//! that lives on another A2A agent. Each round is a run of the assembly's round agent, which sends
//! the message, looks at the task and reports. Run end to end on `MockModel` through the runtime,
//! against an in-process `adam-a2a` server behind a bearer token whose backend records what it was
//! asked. The memory store always, PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{Artifact, Message, Part, Role, Task, TaskState, TaskStatus};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, ExtensionConfig, STEER_EXTENSION,
    TaskBackend, TaskEvent,
};
use adam_assembly::{AgentDef, Assembly};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus, Store};
use adam_llm_agent::{FnTool, ToolError, ToolOutput, ToolSet, user_message};
use adam_model::{Message as ModelMessage, MockModel, ModelRequest, ToolCall};
use adam_runtime::{Agent, RetryPolicy, Runtime, child_run_id};
use async_trait::async_trait;
use common::{def, instructions, spawn_worker, wait_done, wait_for};
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const TOKEN: &str = "tok-7f3a9c2e51d84b06";
const VAR: &str = "BILLING_AGENT_TOKEN";

// --- harness -------------------------------------------------------------------------------

async fn stores() -> Vec<(&'static str, DynStore)> {
    let mut all: Vec<(&'static str, DynStore)> = vec![("memory", Arc::new(MemoryStore::new()))];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        store.migrate().await.expect("migrate");
        all.push(("postgres", Arc::new(store)));
    }
    all
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new().0.simple())
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// What the remote was asked in one `SendMessage`.
#[derive(Clone, Debug)]
struct Submit {
    message_id: String,
    text: String,
    task_id: Option<String>,
    context_id: Option<String>,
    reference_task_ids: Vec<String>,
    extensions: Vec<String>,
    subject: String,
}

struct Entry {
    task: Task,
    text: String,
    gets: usize,
}

#[derive(Default)]
struct Book {
    tasks: HashMap<String, Entry>,
    /// Task ids with their contexts, in the order they were made.
    made: Vec<(String, String)>,
    by_message: HashMap<String, String>,
    submits: Vec<Submit>,
    cancels: Vec<String>,
    cancel_attempts: usize,
    gets: usize,
}

/// A backend that records what it is asked and plays scripts: a task starts `working` and ends on
/// the `done_after`-th `GetTask`; the text of the message picks the ending (`[fail]`, `[input]`, or
/// the artifact `done: <text>`). A message that names a task and activates `steer/v1` is added to
/// that task and made no task of its own.
#[derive(Clone)]
struct Remote {
    book: Arc<Mutex<Book>>,
    done_after: Arc<AtomicUsize>,
    lose_first_response: Arc<AtomicBool>,
    fail_next_cancel: Arc<AtomicBool>,
    /// Refuse a steer, as a remote whose task just ended does.
    refuse_steer: Arc<AtomicBool>,
    /// Cancel this round once, while the first `SendMessage` is being answered.
    cancel_on_submit: Arc<Mutex<Option<(Runtime, RunId)>>>,
}

impl Remote {
    fn new(done_after: usize) -> Self {
        Self {
            book: Arc::default(),
            done_after: Arc::new(AtomicUsize::new(done_after)),
            lose_first_response: Arc::default(),
            fail_next_cancel: Arc::default(),
            refuse_steer: Arc::default(),
            cancel_on_submit: Arc::default(),
        }
    }

    fn submits(&self) -> Vec<Submit> {
        self.book.lock().unwrap().submits.clone()
    }

    fn made(&self) -> Vec<(String, String)> {
        self.book.lock().unwrap().made.clone()
    }

    fn cancels(&self) -> Vec<String> {
        self.book.lock().unwrap().cancels.clone()
    }

    fn cancel_attempts(&self) -> usize {
        self.book.lock().unwrap().cancel_attempts
    }

    fn gets(&self) -> usize {
        self.book.lock().unwrap().gets
    }

    /// Let every task end at its next look.
    fn release(&self) {
        self.done_after.store(0, SeqCst);
    }
}

fn status(state: TaskState, text: Option<&str>) -> TaskStatus {
    TaskStatus {
        state,
        message: text.map(|t| Message::new(Role::Agent, vec![Part::text(t)])),
        timestamp: None,
    }
}

#[async_trait]
impl TaskBackend for Remote {
    async fn submit(
        &self,
        caller: Caller,
        message: Message,
        task_id: Option<String>,
        context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        let text = message.text().unwrap_or_default().to_owned();
        let steering = message
            .extensions
            .iter()
            .flatten()
            .any(|e| e == STEER_EXTENSION);
        if steering && self.refuse_steer.load(SeqCst) {
            self.book.lock().unwrap().submits.push(Submit {
                message_id: message.message_id.clone(),
                text,
                task_id,
                context_id,
                reference_task_ids: Vec::new(),
                extensions: vec![STEER_EXTENSION.to_owned()],
                subject: caller.subject.clone(),
            });
            return Err(BackendError::UnsupportedOperation("the task ended".into()));
        }
        let task = {
            let mut book = self.book.lock().unwrap();
            let extensions = message.extensions.clone().unwrap_or_default();
            book.submits.push(Submit {
                message_id: message.message_id.clone(),
                text: text.clone(),
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                reference_task_ids: message.reference_task_ids.clone().unwrap_or_default(),
                extensions: extensions.clone(),
                subject: caller.subject.clone(),
            });
            if let Some(known) = book.by_message.get(&message.message_id).cloned() {
                book.tasks[&known].task.clone()
            } else if let Some(entry) = task_id
                .as_ref()
                .filter(|_| extensions.iter().any(|e| e == STEER_EXTENSION))
                .and_then(|id| book.tasks.get_mut(id))
            {
                entry.text.push_str(&format!(" + {text}"));
                entry.task.clone()
            } else {
                let id = a2a::new_task_id();
                let context = context_id.unwrap_or_else(a2a::new_context_id);
                let task = Task {
                    id: id.clone(),
                    context_id: context.clone(),
                    status: status(TaskState::Working, None),
                    artifacts: None,
                    history: None,
                    metadata: None,
                };
                book.by_message
                    .insert(message.message_id.clone(), id.clone());
                book.made.push((id.clone(), context));
                book.tasks.insert(
                    id,
                    Entry {
                        task: task.clone(),
                        text,
                        gets: 0,
                    },
                );
                task
            }
        };
        let hook = self.cancel_on_submit.lock().unwrap().take();
        if let Some((rt, run)) = hook {
            rt.cancel(run, "stop").await.unwrap();
        }
        if self.lose_first_response.swap(false, SeqCst) {
            return Err(BackendError::unavailable("the response was lost"));
        }
        Ok(task)
    }

    async fn get(&self, _caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        let mut book = self.book.lock().unwrap();
        book.gets += 1;
        let Some(entry) = book.tasks.get_mut(task_id) else {
            return Ok(None);
        };
        entry.gets += 1;
        if entry.gets >= self.done_after.load(SeqCst)
            && entry.task.status.state == TaskState::Working
        {
            if entry.text.contains("[fail]") {
                entry.task.status = status(TaskState::Failed, Some("out of budget"));
            } else if entry.text.contains("[input]") {
                entry.task.status = status(TaskState::InputRequired, Some("which invoice?"));
            } else {
                entry.task.status = status(TaskState::Completed, None);
                entry.task.artifacts = Some(vec![Artifact {
                    artifact_id: a2a::new_artifact_id(),
                    name: None,
                    description: None,
                    parts: vec![Part::text(format!("done: {}", entry.text))],
                    metadata: None,
                    extensions: None,
                }]);
            }
        }
        Ok(Some(entry.task.clone()))
    }

    async fn cancel(&self, _caller: &Caller, task_id: &str) -> Result<Task, BackendError> {
        let mut book = self.book.lock().unwrap();
        book.cancel_attempts += 1;
        if self.fail_next_cancel.swap(false, SeqCst) {
            return Err(BackendError::unavailable("the cancel was lost"));
        }
        let Some(entry) = book.tasks.get_mut(task_id) else {
            return Err(BackendError::TaskNotFound(task_id.to_owned()));
        };
        // Cancelling a task that is cancelled already changes nothing: the closing round of a
        // session asks again, as a backstop.
        let again = entry.task.status.state == TaskState::Canceled;
        entry.task.status = status(TaskState::Canceled, None);
        let task = entry.task.clone();
        if !again {
            book.cancels.push(task_id.to_owned());
        }
        Ok(task)
    }

    fn subscribe(
        &self,
        _caller: &Caller,
        task_id: &str,
    ) -> BoxStream<'static, Result<TaskEvent, BackendError>> {
        let error = BackendError::TaskNotFound(task_id.to_owned());
        Box::pin(futures::stream::once(async move { Err(error) }))
    }
}

/// An A2A server on a local port behind bearer authentication.
struct Server {
    addr: SocketAddr,
    /// How many times the agent card was fetched.
    card_hits: Arc<AtomicUsize>,
}

impl Server {
    async fn start(backend: Remote, steer: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut card = AgentCardConfig::new(
            "billing",
            "Answers billing questions",
            format!("http://{addr}/").parse().unwrap(),
            "0.1.0",
        );
        if steer {
            card = card.with_extension(ExtensionConfig::steer());
        }
        let auth = AuthConfig::BearerTokens(vec![TOKEN.to_owned().into()]);
        let card_hits = Arc::new(AtomicUsize::new(0));
        let counter = card_hits.clone();
        let app =
            A2aServer::router(card, Arc::new(backend), auth).layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let counter = counter.clone();
                    async move {
                        if request.uri().path().ends_with("agent-card.json") {
                            counter.fetch_add(1, SeqCst);
                        }
                        next.run(request).await
                    }
                },
            ));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, card_hits }
    }

    fn card_url(&self) -> String {
        format!("http://{}/.well-known/agent-card.json", self.addr)
    }
}

/// The root's files, with the remote subagent `billing` and the test's own tool `gate`.
fn files(root: &str, card_url: &str) -> Vec<(String, String)> {
    vec![
        (
            "agent/instructions.md".into(),
            instructions(&format!("name: {root}\ntools: [gate]"), "You are the root."),
        ),
        (
            "agent/subagents/billing.md".into(),
            format!(
                "---\ndescription: Handles billing questions.\na2a: {card_url}\n\
                 auth: bearer:{VAR}\n---\n"
            ),
        ),
    ]
}

/// A tool that waits for the remote: until it has had `submits` messages, `cancel_attempts` cancels,
/// and (with `open: true`) until the test says go.
fn gate(remote: &Remote, open: &Arc<AtomicBool>) -> FnTool {
    let (remote, open) = (remote.clone(), open.clone());
    FnTool::raw(
        "gate",
        "waits for the test",
        json!({"type": "object", "properties": {}}),
        move |_ctx, args| {
            let (remote, open) = (remote.clone(), open.clone());
            async move {
                let want = |key: &str| args.get(key).and_then(Value::as_u64).unwrap_or(0) as usize;
                let manual = args.get("open").and_then(Value::as_bool).unwrap_or(false);
                for _ in 0..1000 {
                    if remote.submits().len() >= want("submits")
                        && remote.cancel_attempts() >= want("cancels")
                        && (!manual || open.load(SeqCst))
                    {
                        return Ok::<_, ToolError>(ToolOutput::text("open"));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(ToolOutput::error("the gate never opened"))
            }
        },
    )
}

struct Rig {
    root: String,
    remote: Remote,
    server: Server,
    model: Arc<MockModel>,
    open: Arc<AtomicBool>,
}

impl Rig {
    async fn new(done_after: usize, steer: bool) -> Self {
        let remote = Remote::new(done_after);
        let server = Server::start(remote.clone(), steer).await;
        Self {
            root: uniq("root"),
            remote,
            server,
            model: Arc::new(MockModel::new()),
            open: Arc::default(),
        }
    }

    /// The assembly of the root's files, with a wait timer of 30 ms and the remote's time limit
    /// `limit` when given.
    fn assemble(&self, limit: Option<Duration>) -> Assembly {
        let owned = files(&self.root, &self.server.card_url());
        let refs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let mut def: AgentDef = def(&refs).env(VAR, TOKEN);
        if let Some(limit) = limit {
            def = def.remote_timeout(limit);
        }
        def.bind(ToolSet::new().tool(gate(&self.remote, &self.open)))
            .unwrap()
            .wait_poll(Duration::from_millis(30))
            .model(self.model.clone(), "default")
            .unwrap()
    }

    fn runtime(&self, store: &DynStore, limit: Option<Duration>) -> Runtime {
        self.assemble(limit)
            .register(Runtime::builder(store.clone()))
            .poll_interval(Duration::from_millis(20))
            .retry(RetryPolicy {
                max_attempts: 4,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
                multiplier: 1.0,
            })
            .build()
    }

    /// Start the parent. Its script is given the session id, which its first call (`c1`) makes.
    async fn start(&self, rt: &Runtime, script: impl FnOnce(&MockModel, RunId)) -> RunId {
        let run = rt
            .start(&self.root, user_message("go"), None)
            .await
            .unwrap();
        script(&self.model, child_run_id(run, "c1"));
        run
    }
}

fn start(prompt: &str, keep_open: bool) -> ToolCall {
    call(
        "c1",
        "subagent_start",
        json!({"agent": "billing", "description": "billing", "prompt": prompt,
               "keep_open": keep_open}),
    )
}

fn read(id: &str, session: RunId) -> ToolCall {
    call(
        id,
        "subagent_read",
        json!({"session": session.to_string(), "wait": true}),
    )
}

fn send(id: &str, session: RunId, message: &str) -> ToolCall {
    call(
        id,
        "subagent_send",
        json!({"session": session.to_string(), "message": message}),
    )
}

fn close(id: &str, session: RunId, cancel: bool) -> ToolCall {
    call(
        id,
        "subagent_close",
        json!({"session": session.to_string(), "cancel": cancel}),
    )
}

fn gate_call(id: &str, args: Value) -> ToolCall {
    call(id, "gate", args)
}

/// The results of the tool calls in the model's last request, by call id.
fn results(model: &MockModel) -> HashMap<String, (String, bool)> {
    let requests: Vec<ModelRequest> = model.requests();
    requests
        .last()
        .unwrap()
        .messages
        .iter()
        .filter_map(|m| match m {
            ModelMessage::Tool {
                call_id,
                content,
                is_error,
                ..
            } => Some((call_id.clone(), (content.clone(), *is_error))),
            _ => None,
        })
        .collect()
}

fn text_of(results: &HashMap<String, (String, bool)>, id: &str) -> String {
    results
        .get(id)
        .unwrap_or_else(|| panic!("no result for {id}: {results:#?}"))
        .0
        .clone()
}

fn json_of(results: &HashMap<String, (String, bool)>, id: &str) -> Value {
    serde_json::from_str(&text_of(results, id)).unwrap_or_else(|e| panic!("{id} is not JSON: {e}"))
}

// --- the rounds of a session -----------------------------------------------------------------

#[tokio::test]
async fn a_remote_session_keeps_its_context_across_rounds() {
    for (backend, store) in stores().await {
        let rig = Rig::new(1, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_tool_calls(vec![send("c3", session, "invoice 8")])
                    .push_tool_calls(vec![read("c4", session)])
                    .push_tool_calls(vec![close("c5", session, false)])
                    .push_text("settled");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;

        let results = results(&rig.model);
        assert!(
            text_of(&results, "c2").contains("done: invoice 7"),
            "{backend}"
        );
        assert!(
            text_of(&results, "c4").contains("done: invoice 8"),
            "{backend}"
        );
        let (session, second) = (child_run_id(parent, "c1"), child_run_id(parent, "c3"));
        let (submits, made) = (rig.remote.submits(), rig.remote.made());
        assert_eq!(submits.len(), 2, "{backend}: {submits:?}");
        // Round 1 names no context; its message id is the round's run.
        assert_eq!(submits[0].context_id, None, "{backend}");
        assert!(submits[0].reference_task_ids.is_empty(), "{backend}");
        assert_eq!(submits[0].message_id, session.to_string(), "{backend}");
        assert_eq!(submits[0].subject, "token-0", "{backend}");
        // Round 2 continues the remote's context and names the task before it.
        assert_eq!(submits[1].text, "invoice 8", "{backend}");
        assert_eq!(
            submits[1].context_id.as_ref(),
            Some(&made[0].1),
            "{backend}"
        );
        assert_eq!(
            submits[1].reference_task_ids,
            [made[0].0.clone()],
            "{backend}"
        );
        assert_eq!(submits[1].message_id, second.to_string(), "{backend}");
        assert_eq!(submits[1].task_id, None, "{backend}");
        assert!(rig.remote.cancels().is_empty(), "{backend}");
    }
}

#[tokio::test]
async fn a_message_to_a_working_remote_session_is_steered_when_the_card_lists_steer() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, true).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![gate_call("g", json!({"submits": 1}))])
                    .push_tool_calls(vec![send("c3", session, "and invoice 9")])
                    .push_tool_calls(vec![gate_call("h", json!({"submits": 2}))])
                    .push_tool_calls(vec![read("c5", session)])
                    .push_text("settled");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_for("the steer to reach the remote", || async {
            (rig.remote.submits().len() >= 2).then_some(())
        })
        .await;
        rig.remote.release();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let results = results(&rig.model);
        assert_eq!(json_of(&results, "c3")["delivered"], "steered", "{backend}");
        let (submits, made) = (rig.remote.submits(), rig.remote.made());
        assert_eq!(made.len(), 1, "{backend}: a steer makes no task");
        assert_eq!(submits.len(), 2, "{backend}: {submits:?}");
        let steer = &submits[1];
        assert_eq!(steer.task_id.as_ref(), Some(&made[0].0), "{backend}");
        assert_eq!(steer.context_id.as_ref(), Some(&made[0].1), "{backend}");
        assert!(
            steer.extensions.iter().any(|e| e == STEER_EXTENSION),
            "{backend}"
        );
        assert_eq!(
            steer.message_id,
            child_run_id(child_run_id(parent, "c1"), "c3").to_string(),
            "{backend}"
        );
        assert!(
            text_of(&results, "c5").contains("and invoice 9"),
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_message_is_queued_and_sent_as_the_next_task_when_the_card_does_not() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![gate_call("g", json!({"submits": 1}))])
                    .push_tool_calls(vec![send("c3", session, "and invoice 9")])
                    .push_tool_calls(vec![read("c4", session)])
                    .push_text("settled");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_for("the first task", || async {
            (!rig.remote.made().is_empty()).then_some(())
        })
        .await;
        // The round has taken the message and keeps it: the remote has been asked once.
        let round = child_run_id(parent, "c1");
        wait_for("the message to be queued", || async {
            let rec = store.load_run(round).await.unwrap()?;
            let agent = &rec.state["agent"];
            (agent["task"].is_string() && agent["queued"].as_array()?.len() == 1).then_some(())
        })
        .await;
        assert_eq!(rig.remote.submits().len(), 1, "{backend}");
        rig.remote.release();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let results = results(&rig.model);
        assert_eq!(json_of(&results, "c3")["delivered"], "queued", "{backend}");
        let (submits, made) = (rig.remote.submits(), rig.remote.made());
        assert_eq!(
            made.len(),
            2,
            "{backend}: the queued message is a task of its own"
        );
        assert_eq!(submits.len(), 2, "{backend}: {submits:?}");
        assert_eq!(submits[1].text, "and invoice 9", "{backend}");
        assert_eq!(submits[1].task_id, None, "{backend}");
        assert_eq!(
            submits[1].context_id.as_ref(),
            Some(&made[0].1),
            "{backend}"
        );
        assert_eq!(
            submits[1].reference_task_ids,
            [made[0].0.clone()],
            "{backend}"
        );
        assert_eq!(
            submits[1].message_id,
            child_run_id(child_run_id(parent, "c1"), "queued:1").to_string(),
            "{backend}"
        );
        let report = text_of(&results, "c4");
        assert!(
            report.contains("done: invoice 7") && report.contains("done: and invoice 9"),
            "{backend}: both answers are the report: {report}"
        );
    }
}

#[tokio::test]
async fn closing_a_remote_session_is_local() {
    for (backend, store) in stores().await {
        let rig = Rig::new(1, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_tool_calls(vec![close("c3", session, false)])
                    .push_tool_calls(vec![call("c4", "subagent_list", json!({}))])
                    .push_text("closed");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;

        let results = results(&rig.model);
        assert!(!results["c3"].1, "{backend}: {results:#?}");
        assert!(text_of(&results, "c3").contains("closed"), "{backend}");
        // The closing round is a run of the round agent that calls nothing and repeats the last report.
        let closing = child_run_id(parent, "c3");
        let ended = wait_for("the closing round", || async {
            let view = rt.view(closing).await.unwrap()?;
            (view.status == RunStatus::Done).then_some(view)
        })
        .await;
        worker.stop().await;
        assert_eq!(
            ended.output.unwrap()["text"],
            "done: invoice 7",
            "{backend}"
        );
        assert_eq!(
            rig.remote.submits().len(),
            1,
            "{backend}: no message went out"
        );
        assert!(rig.remote.cancels().is_empty(), "{backend}");
    }
}

#[tokio::test]
async fn a_restart_mid_round_resumes_polling_without_sending_again() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let first = rig.runtime(&store, None);
        let parent = rig
            .start(&first, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", false)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_text("settled after the restart");
            })
            .await;
        let worker = spawn_worker(&first);
        wait_for("a few looks", || async {
            (rig.remote.gets() >= 2).then_some(())
        })
        .await;
        worker.stop().await;
        drop(first);
        assert_eq!(rig.remote.submits().len(), 1, "{backend}");

        // A new process, from the same files, with its own client, finds the task in the round's
        // state and looks at it.
        rig.remote.release();
        let second = rig.runtime(&store, None);
        let worker = spawn_worker(&second);
        wait_done(&second, parent).await;
        worker.stop().await;
        assert_eq!(rig.remote.submits().len(), 1, "{backend}: not sent again");
        assert!(
            text_of(&results(&rig.model), "c2").contains("done: invoice 7"),
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_lost_send_response_is_retried_under_the_same_message_id() {
    for (backend, store) in stores().await {
        let rig = Rig::new(1, false).await;
        rig.remote.lose_first_response.store(true, SeqCst);
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", false)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_text("settled");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;

        let submits = rig.remote.submits();
        assert_eq!(submits.len(), 2, "{backend}: {submits:?}");
        assert_eq!(submits[0].message_id, submits[1].message_id, "{backend}");
        assert_eq!(
            rig.remote.made().len(),
            1,
            "{backend}: the retry found the task"
        );
        assert!(
            text_of(&results(&rig.model), "c2").contains("done: invoice 7"),
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_remote_round_that_fails_reads_failed_and_the_session_stays_usable() {
    for (backend, store) in stores().await {
        let rig = Rig::new(1, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice [fail]", true)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_tool_calls(vec![send("c3", session, "invoice 8")])
                    .push_tool_calls(vec![read("c4", session)])
                    .push_text("recovered");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;

        let results = results(&rig.model);
        let failed = text_of(&results, "c2");
        assert!(
            failed.contains("failed") && failed.contains("out of budget"),
            "{backend}: {failed}"
        );
        assert!(
            text_of(&results, "c4").contains("done: invoice 8"),
            "{backend}"
        );
        let (submits, made) = (rig.remote.submits(), rig.remote.made());
        assert_eq!(submits.len(), 2, "{backend}");
        assert_eq!(
            submits[1].context_id.as_ref(),
            Some(&made[0].1),
            "{backend}"
        );
        assert_eq!(
            submits[1].reference_task_ids,
            [made[0].0.clone()],
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_question_is_reported_and_the_next_round_answers_by_task_id() {
    for (backend, store) in stores().await {
        let rig = Rig::new(1, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("pay [input]", true)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_tool_calls(vec![send("c3", session, "invoice 7")])
                    .push_tool_calls(vec![read("c4", session)])
                    .push_text("answered");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;

        let results = results(&rig.model);
        assert!(
            text_of(&results, "c2").contains("which invoice?"),
            "{backend}"
        );
        let (submits, made) = (rig.remote.submits(), rig.remote.made());
        assert_eq!(submits.len(), 2, "{backend}");
        assert_eq!(submits[1].task_id.as_ref(), Some(&made[0].0), "{backend}");
        assert!(submits[1].reference_task_ids.is_empty(), "{backend}");
    }
}

// --- cancelling ------------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_a_remote_session_sends_cancel_task() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![gate_call("g", json!({"submits": 1}))])
                    .push_tool_calls(vec![close("c3", session, true)])
                    .push_text("cancelled");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        let made = rig.remote.made();
        wait_for("the CancelTask", || async {
            (!rig.remote.cancels().is_empty()).then_some(())
        })
        .await;
        worker.stop().await;
        assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
        let view = rt.view(child_run_id(parent, "c1")).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Failed, "{backend}");
    }
}

#[tokio::test]
async fn cancelling_the_parent_sends_cancel_task() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_text("never");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_for("the first task", || async {
            (!rig.remote.made().is_empty()).then_some(())
        })
        .await;
        rt.cancel(parent, "stop").await.unwrap();
        let made = rig.remote.made();
        wait_for("the CancelTask", || async {
            (!rig.remote.cancels().is_empty()).then_some(())
        })
        .await;
        worker.stop().await;
        assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
    }
}

#[tokio::test]
async fn a_round_past_its_limit_cancels_the_remote_task() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let rt = rig.runtime(&store, Some(Duration::from_millis(400)));
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", false)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_text("gave up");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        let made = rig.remote.made();
        wait_for("the CancelTask", || async {
            (!rig.remote.cancels().is_empty()).then_some(())
        })
        .await;
        worker.stop().await;
        assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
        assert!(
            text_of(&results(&rig.model), "c2").contains("time limit"),
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_lost_cancel_is_sent_by_the_next_round() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        rig.remote.fail_next_cancel.store(true, SeqCst);
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![gate_call(
                        "g",
                        json!({"submits": 1, "cancels": 1, "open": true}),
                    )])
                    .push_tool_calls(vec![send("c3", session, "invoice 8")])
                    .push_tool_calls(vec![gate_call("h", json!({"submits": 2}))])
                    .push_tool_calls(vec![close("c5", session, true)])
                    .push_text("done");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_for("the first task", || async {
            (!rig.remote.made().is_empty()).then_some(())
        })
        .await;
        // The round is cancelled and its CancelTask is lost.
        rt.cancel(child_run_id(parent, "c1"), "stop").await.unwrap();
        wait_for("the lost cancel", || async {
            (rig.remote.cancel_attempts() >= 1).then_some(())
        })
        .await;
        assert!(rig.remote.cancels().is_empty(), "{backend}");
        rig.open.store(true, SeqCst);
        wait_done(&rt, parent).await;
        let made = rig.remote.made();
        wait_for("the next round's cancel", || async {
            (!rig.remote.cancels().is_empty()).then_some(())
        })
        .await;
        worker.stop().await;
        assert!(rig.remote.cancels().contains(&made[0].0), "{backend}");
    }
}

#[tokio::test]
async fn a_cancel_between_the_send_and_its_commit_still_cancels_the_task() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, _| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_text("started");
            })
            .await;
        // The round is cancelled while the remote is answering its send: the task id never reaches
        // the round's state.
        let round = child_run_id(parent, "c1");
        *rig.remote.cancel_on_submit.lock().unwrap() = Some((rt.clone(), round));
        let worker = spawn_worker(&rt);
        wait_for("the CancelTask", || async {
            (!rig.remote.cancels().is_empty()).then_some(())
        })
        .await;
        worker.stop().await;
        let made = rig.remote.made();
        assert_eq!(made.len(), 1, "{backend}: the recovery made no second task");
        assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
    }
}

#[tokio::test]
async fn messages_that_waited_for_a_failed_round_go_first_in_the_next() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice [fail]", true)])
                    .push_tool_calls(vec![gate_call("g", json!({"submits": 1}))])
                    .push_tool_calls(vec![send("c3", session, "second")])
                    .push_tool_calls(vec![gate_call("h", json!({"open": true}))])
                    .push_tool_calls(vec![read("c5", session)])
                    .push_tool_calls(vec![send("c6", session, "third")])
                    .push_tool_calls(vec![read("c7", session)])
                    .push_text("done");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_for("the first task", || async {
            (!rig.remote.made().is_empty()).then_some(())
        })
        .await;
        // `second` is read by the round before the task fails.
        let round = child_run_id(parent, "c1");
        wait_for("the message to be taken", || async {
            let rec = store.load_run(round).await.unwrap()?;
            let agent = &rec.state["agent"];
            (agent["task"].is_string() && agent["queued"].as_array()?.len() == 1).then_some(())
        })
        .await;
        rig.remote.release();
        rig.open.store(true, SeqCst);
        wait_done(&rt, parent).await;
        worker.stop().await;

        let results = results(&rig.model);
        assert!(
            text_of(&results, "c5").contains("out of budget"),
            "{backend}"
        );
        let submits = rig.remote.submits();
        assert_eq!(submits.len(), 2, "{backend}: {submits:?}");
        assert_eq!(submits[1].text, "second\n\nthird", "{backend}");
    }
}

#[tokio::test]
async fn a_steer_the_remote_refuses_is_sent_as_the_next_task() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, true).await;
        rig.remote.refuse_steer.store(true, SeqCst);
        let rt = rig.runtime(&store, None);
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", true)])
                    .push_tool_calls(vec![gate_call("g", json!({"submits": 1}))])
                    .push_tool_calls(vec![send("c3", session, "and invoice 9")])
                    .push_tool_calls(vec![gate_call("h", json!({"submits": 3}))])
                    .push_tool_calls(vec![read("c5", session)])
                    .push_text("settled");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_for("the refused steer", || async {
            (rig.remote.submits().len() >= 2).then_some(())
        })
        .await;
        rig.remote.release();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let submits = rig.remote.submits();
        assert_eq!(submits.len(), 3, "{backend}: {submits:?}");
        assert!(submits[1].task_id.is_some(), "{backend}: the refused steer");
        assert_eq!(submits[2].text, "and invoice 9", "{backend}");
        assert_eq!(submits[2].task_id, None, "{backend}");
        let report = text_of(&results(&rig.model), "c5");
        assert!(
            report.contains("done: and invoice 9"),
            "{backend}: {report}"
        );
    }
}

#[tokio::test]
async fn a_round_started_without_a_limit_gives_up_at_the_remote_timeout() {
    for (backend, store) in stores().await {
        let rig = Rig::new(usize::MAX, false).await;
        let rt = rig.runtime(&store, Some(Duration::from_millis(400)));
        let name = format!("{}/billing", rig.root);
        // As the A2A front starts a person's round: no time limit from the runtime.
        let round = rt
            .start(
                &name,
                adam_runtime::Inbound::new("start", json!({"text": "invoice 7"})),
                None,
            )
            .await
            .unwrap();
        let worker = spawn_worker(&rt);
        let view = wait_for("the round to give up", || async {
            let view = rt.view(round).await.unwrap()?;
            (view.status == RunStatus::Failed).then_some(view)
        })
        .await;
        wait_for("the CancelTask", || async {
            (!rig.remote.cancels().is_empty()).then_some(())
        })
        .await;
        worker.stop().await;
        assert!(view.error.unwrap().contains("time limit"), "{backend}");
    }
}

// --- the endpoint and the cancel hook, asked directly ---------------------------------------

fn round_agent(rig: &Rig) -> adam_assembly::RemoteRoundAgent {
    let assembly = rig.assemble(None);
    assert_eq!(assembly.remote_round_agents().len(), 1);
    assembly.remote_round_agents()[0].clone()
}

fn probe_request(text: &str) -> Value {
    serde_json::to_value(a2a::SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(text)]),
        configuration: Some(a2a::SendMessageConfiguration {
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: Some(0),
            return_immediately: Some(true),
        }),
        metadata: None,
        tenant: None,
    })
    .unwrap()
}

#[tokio::test]
async fn a_round_fetches_the_card_once_and_then_reuses_the_client() {
    let rig = Rig::new(1, false).await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = rig.runtime(&store, None);
    let parent = rig
        .start(&rt, |model, session| {
            model
                .push_tool_calls(vec![start("invoice 7", false)])
                .push_tool_calls(vec![read("c2", session)])
                .push_text("done");
        })
        .await;
    let worker = spawn_worker(&rt);
    wait_done(&rt, parent).await;
    worker.stop().await;
    assert!(text_of(&results(&rig.model), "c2").contains("done: invoice 7"));
    assert_eq!(
        rig.server.card_hits.load(SeqCst),
        1,
        "the card is read for the round, and the client it made serves the send and the looks"
    );
}

#[tokio::test]
async fn callers_that_arrive_together_dial_the_card_once() {
    let rig = Rig::new(usize::MAX, false).await;
    let agent = round_agent(&rig);
    // Each cancel asks the endpoint for its client, and the task it names is unknown to the remote.
    let state = json!({ "prior_open_task": "no-such-task" });
    futures::future::join_all((0..6).map(|_| agent.on_cancelled(RunId::new(), state.clone())))
        .await;
    assert_eq!(
        rig.remote.cancel_attempts(),
        6,
        "the prior task is cancelled by a cancelled round"
    );
    assert_eq!(rig.server.card_hits.load(SeqCst), 1);
}

#[tokio::test]
async fn a_cancelled_round_repeats_a_send_only_when_it_does_not_know_the_task() {
    let rig = Rig::new(usize::MAX, false).await;
    let agent = round_agent(&rig);
    // The task is known: it is cancelled and the send is not repeated.
    let known = json!({ "task": "t-known", "sending": probe_request("p") });
    agent.on_cancelled(RunId::new(), known).await;
    assert_eq!(rig.remote.cancel_attempts(), 1);
    assert!(
        rig.remote.submits().is_empty(),
        "{:?}",
        rig.remote.submits()
    );
    // The task is not known: the send is repeated to find it, and what it made is cancelled.
    let lost = json!({ "sending": probe_request("p") });
    agent.on_cancelled(RunId::new(), lost).await;
    assert_eq!(rig.remote.submits().len(), 1);
    assert_eq!(rig.remote.made().len(), 1);
    assert_eq!(rig.remote.cancels(), [rig.remote.made()[0].0.clone()]);
}
