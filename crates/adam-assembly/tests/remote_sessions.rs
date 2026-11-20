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

/// The time limit of a round in the tests that move the clock past it.
const LIMIT: Duration = Duration::from_secs(3600);
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
        self.def(limit)
            .bind(ToolSet::new().tool(gate(&self.remote, &self.open)))
            .unwrap()
            .wait_poll(Duration::from_millis(30))
            .model(self.model.clone(), "default")
            .unwrap()
    }

    /// The definition of the root's files, as a worker and a control plane both read it.
    fn def(&self, limit: Option<Duration>) -> AgentDef {
        let owned = files(&self.root, &self.server.card_url());
        let refs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let mut def: AgentDef = def(&refs).env(VAR, TOKEN);
        if let Some(limit) = limit {
            def = def.remote_timeout(limit);
        }
        def
    }

    fn runtime(&self, store: &DynStore, limit: Option<Duration>) -> Runtime {
        self.runtime_with_idle(store, limit, None)
    }

    /// [`runtime`](Self::runtime) whose sessions read closed after `idle` (the deployment's
    /// `SESSION_IDLE_DAYS`, which every process takes the same).
    fn runtime_with_idle(
        &self,
        store: &DynStore,
        limit: Option<Duration>,
        idle: Option<Duration>,
    ) -> Runtime {
        self.runtime_timed(store, limit, idle, None)
    }

    /// [`runtime_with_idle`](Self::runtime_with_idle) on a clock a test moves.
    fn runtime_timed(
        &self,
        store: &DynStore,
        limit: Option<Duration>,
        idle: Option<Duration>,
        clock: Option<&adam_runtime::ManualClock>,
    ) -> Runtime {
        let mut builder = Runtime::builder(store.clone());
        if let Some(idle) = idle {
            builder = builder.session_idle_ttl(idle);
        }
        if let Some(clock) = clock {
            builder = builder.clock(clock.clone());
        }
        self.assemble(limit)
            .register(builder)
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
        // The limit passes when the test moves the clock, once the remote has the task: a limit of
        // wall time could pass before the send on a busy machine, and then there is no task to cancel.
        let clock = adam_runtime::ManualClock::new();
        let rt = rig.runtime_timed(&store, Some(LIMIT), None, Some(&clock));
        let parent = rig
            .start(&rt, |model, session| {
                model
                    .push_tool_calls(vec![start("invoice 7", false)])
                    .push_tool_calls(vec![read("c2", session)])
                    .push_text("gave up");
            })
            .await;
        let worker = spawn_worker(&rt);
        wait_for("the remote's task", || async {
            (!rig.remote.made().is_empty()).then_some(())
        })
        .await;
        clock.advance(LIMIT * 2);
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
        // The limit passes when the test moves the clock, once the remote has the task.
        let clock = adam_runtime::ManualClock::new();
        let rt = rig.runtime_timed(&store, Some(LIMIT), None, Some(&clock));
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
        wait_for("the remote's task", || async {
            (!rig.remote.made().is_empty()).then_some(())
        })
        .await;
        clock.advance(LIMIT * 2);
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

// --- a person's session on a remote helper, a control plane and a worker --------------------

/// The control plane serves the A2A front and registers only the starters of the helpers, so it
/// knows no endpoint and no credential; the worker has the assembly and steps the rounds. Both run
/// on one store.
#[cfg(feature = "a2a")]
mod split {
    use super::*;
    use a2a::Role as A2aRole;
    use adam_a2a::CHILD_SESSIONS_EXTENSION;
    use adam_a2a_runtime::RuntimeTaskBackend;
    use adam_llm_agent::LlmStarter;
    use adam_runtime::{BroadcastSink, ManualClock, SessionView, parse_session_conversation_id};
    use std::time::Instant;

    pub(super) struct Split {
        pub backend: RuntimeTaskBackend,
        pub worker: Option<common::Worker>,
        pub alice: Caller,
    }

    pub(super) fn alice() -> Caller {
        let mut caller = Caller::new("alice");
        caller.extensions = vec![CHILD_SESSIONS_EXTENSION.to_owned()];
        caller
    }

    pub(super) fn said(text: &str, context: Option<&str>, metadata: Value) -> Message {
        let mut message = Message::new(A2aRole::User, vec![Part::text(text)]);
        message.context_id = context.map(str::to_owned);
        message.metadata = Some(HashMap::from([(
            CHILD_SESSIONS_EXTENSION.to_owned(),
            metadata,
        )]));
        message
    }

    pub(super) fn object(task: &Task) -> Value {
        task.metadata
            .as_ref()
            .and_then(|m| m.get(CHILD_SESSIONS_EXTENSION))
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub(super) async fn until(split: &Split, id: &str, state: TaskState) -> Task {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let task = split
                .backend
                .get(&split.alice, id)
                .await
                .unwrap()
                .expect("the round");
            if task.status.state == state {
                return task;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {state:?}; last {:?}",
                task.status.state
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    impl Split {
        /// Stop the worker (what a test that moves the clock past the timers does, so that
        /// only the control plane acts), and keep the control plane.
        pub async fn stop_worker(&mut self) {
            if let Some(worker) = self.worker.take() {
                worker.stop().await;
            }
        }

        pub async fn stop(mut self) {
            self.stop_worker().await;
        }

        pub async fn new(rig: &Rig, store: &DynStore) -> Self {
            Self::with_idle_ttl(rig, store, None).await
        }

        /// As [`with_idle_ttl`](Self::with_idle_ttl), with the rounds limited to `rounds`.
        pub async fn with_max_rounds(rig: &Rig, store: &DynStore, rounds: u32) -> Self {
            Self::build(rig, store, None, Some(rounds), None).await
        }

        /// The control plane reads a session closed after `idle` of nobody writing to it.
        pub async fn with_idle_ttl(rig: &Rig, store: &DynStore, idle: Option<Duration>) -> Self {
            Self::build(rig, store, idle, None, None).await
        }

        /// As [`with_idle_ttl`](Self::with_idle_ttl), on a clock the test moves: a session expires
        /// when it says so and a timer is due when it says so, and nothing sleeps.
        pub async fn with_clock(
            rig: &Rig,
            store: &DynStore,
            idle: Duration,
            clock: &ManualClock,
        ) -> Self {
            Self::build(rig, store, Some(idle), None, Some(clock)).await
        }

        async fn build(
            rig: &Rig,
            store: &DynStore,
            idle: Option<Duration>,
            rounds: Option<u32>,
            clock: Option<&ManualClock>,
        ) -> Self {
            let worker_rt = rig.runtime_timed(store, None, idle, clock);
            let events = BroadcastSink::default();
            let mut def = rig.def(None);
            if let Some(rounds) = rounds {
                def = def.session_limits(adam_runtime::SessionLimits::new(16, 4, rounds).unwrap());
            }
            let sessions = def.child_sessions().expect("a root with a helper");
            let mut builder = Runtime::builder(store.clone()).starter(LlmStarter::new(&rig.root));
            if let Some(idle) = idle {
                builder = builder.session_idle_ttl(idle);
            }
            if let Some(clock) = clock {
                builder = builder.clock(clock.clone());
            }
            let control = def
                .session_starters(builder)
                .event_sink(events.clone())
                .poll_interval(Duration::from_millis(10))
                .build();
            assert_eq!(
                control.agent_names(),
                [rig.root.clone(), format!("{}/billing", rig.root)],
                "the control plane has starters and no agent"
            );
            let backend = RuntimeTaskBackend::new(control.clone(), events, rig.root.clone())
                .with_poll_interval(Duration::from_millis(10))
                .with_child_sessions(sessions);
            Self {
                backend,
                worker: Some(spawn_worker(&worker_rt)),
                alice: alice(),
            }
        }

        pub async fn start(&self, text: &str, keep_open: bool) -> Task {
            self.backend
                .submit(
                    self.alice.clone(),
                    said(
                        text,
                        Some("ctx-1"),
                        json!({"start": {"agent": "billing", "description": "billing",
                                         "keepOpen": keep_open}}),
                    ),
                    None,
                    Some("ctx-1".into()),
                )
                .await
                .unwrap_or_else(|e| panic!("the start was refused: {e:?}"))
        }
    }

    /// A round on a remote helper started on the control plane is stepped by the worker's round
    /// agent: it sends the person's words to the remote and reports the remote's answer. Were the
    /// control plane to register another starter for the helper, the worker could not read the
    /// state it made and the round would not complete.
    #[tokio::test]
    async fn a_round_on_a_remote_helper_started_on_a_control_plane_is_worked_by_the_worker() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let split = Split::new(&rig, &store).await;
            let start = split.start("invoice 7", true).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            rig.remote.release();
            let done = until(&split, &start.id, TaskState::Completed).await;
            let answer = serde_json::to_string(&done).unwrap();
            assert!(answer.contains("done: invoice 7"), "{backend}: {answer}");
            assert_eq!(object(&done)["state"], "idle", "{backend}: kept open");
            let submits = rig.remote.submits();
            assert_eq!(submits.len(), 1, "{backend}");
            assert_eq!(submits[0].text, "invoice 7", "{backend}");
            split.stop().await;
        }
    }

    /// The control plane holds no endpoint, so it cannot cancel the remote task itself. A cancel
    /// of a round that closes its session (`keep_open: false`) still makes the worker send it.
    #[tokio::test]
    async fn cancelling_a_round_of_a_session_that_closes_with_it_cancels_the_remote_task() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let split = Split::new(&rig, &store).await;
            let start = split.start("invoice 7", false).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            let made = rig.remote.made();
            let task = split.backend.cancel(&split.alice, &start.id).await.unwrap();
            assert_eq!(task.status.state, TaskState::Canceled, "{backend}");
            wait_for("the CancelTask", || async {
                (!rig.remote.cancels().is_empty()).then_some(())
            })
            .await;
            assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
            // The session is closed, and one closing round was made for it.
            let task = split
                .backend
                .get(&split.alice, &start.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(object(&task)["state"], "closed", "{backend}");
            split.stop().await;
        }
    }

    /// A close that cancels, of a session that does not stay open, ends the remote task too.
    #[tokio::test]
    async fn closing_with_cancel_a_session_that_does_not_stay_open_cancels_the_remote_task() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let split = Split::new(&rig, &store).await;
            let start = split.start("invoice 7", false).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            let made = rig.remote.made();
            let context = start.context_id.clone();
            split
                .backend
                .submit(
                    split.alice.clone(),
                    said("stop", Some(&context), json!({"close": {"cancel": true}})),
                    None,
                    Some(context.clone()),
                )
                .await
                .unwrap_or_else(|e| panic!("{backend}: the close was refused: {e:?}"));
            wait_for("the CancelTask", || async {
                (!rig.remote.cancels().is_empty()).then_some(())
            })
            .await;
            assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
            split.stop().await;
        }
    }

    /// A cancel of a round of a session that stays open sends the remote `CancelTask` too, and the
    /// session stays open: it is idle, not closed, and takes the next message as a new round.
    #[tokio::test]
    async fn cancelling_a_round_of_a_kept_open_session_cancels_the_remote_task_and_keeps_it_open() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let split = Split::new(&rig, &store).await;
            let start = split.start("invoice 7", true).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            let made = rig.remote.made();
            split.backend.cancel(&split.alice, &start.id).await.unwrap();
            wait_for("the CancelTask", || async {
                (!rig.remote.cancels().is_empty()).then_some(())
            })
            .await;
            assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
            // The cleanup round is done once the session is idle again, and it is still open.
            let task = wait_for("the session idle", || async {
                let task = split.backend.get(&split.alice, &start.id).await.ok()??;
                (object(&task)["state"] == "idle").then_some(task)
            })
            .await;
            assert_eq!(object(&task)["keepOpen"], true, "{backend}");
            rig.remote.release();
            let again = split
                .backend
                .submit(
                    split.alice.clone(),
                    said("invoice 8", Some(&start.context_id), json!({})),
                    None,
                    Some(start.context_id.clone()),
                )
                .await
                .unwrap_or_else(|e| panic!("{backend}: an open session takes a message: {e:?}"));
            let done = until(&split, &again.id, TaskState::Completed).await;
            assert!(
                serde_json::to_string(&done)
                    .unwrap()
                    .contains("done: invoice 8"),
                "{backend}"
            );
            split.stop().await;
        }
    }

    /// A session on a remote helper that expires idle, with the remote task still open (it asked a
    /// question and nobody answered), is followed by a closing round that cancels the task.
    #[tokio::test]
    async fn a_remote_session_that_expires_idle_has_its_remote_task_cancelled() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let clock = ManualClock::new();
            let split = Split::with_clock(&rig, &store, HOUR, &clock).await;
            let start = split.start("invoice [input]", true).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            let made = rig.remote.made();
            rig.remote.release();
            until(&split, &start.id, TaskState::Completed).await;
            assert!(rig.remote.cancels().is_empty(), "{backend}: nothing yet");
            clock.advance(2 * HOUR);
            // Whoever looks at the session after it expired starts the cleanup.
            wait_for("the CancelTask after the expiry", || async {
                let _ = split.backend.get(&split.alice, &start.id).await;
                (!rig.remote.cancels().is_empty()).then_some(())
            })
            .await;
            assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
            split.stop().await;
        }
    }

    /// Nobody reads the session again: the worker's own timer ends the remote task after the idle
    /// TTL, as a question nobody answers must not stay open for good.
    #[tokio::test]
    async fn a_remote_session_nobody_reads_again_has_its_remote_task_cancelled_after_the_ttl() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let clock = ManualClock::new();
            let split = Split::with_clock(&rig, &store, HOUR, &clock).await;
            let start = split.start("invoice [input]", true).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            let made = rig.remote.made();
            rig.remote.release();
            until(&split, &start.id, TaskState::Completed).await;
            // No request about the session from here on: only the remote is watched.
            wait_for("the timer parked", || async {
                wakes(timers(&store, &rig, &start.id).await.first()?)
            })
            .await;
            clock.advance(2 * HOUR);
            wait_for("the CancelTask with nobody reading", || async {
                (!rig.remote.cancels().is_empty()).then_some(())
            })
            .await;
            assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
            split.stop().await;
        }
    }

    /// The round that cleans up after a cancel is ours: a person's session with room for two
    /// rounds still takes its second after the first was cancelled.
    #[tokio::test]
    async fn a_cleanup_round_does_not_count_toward_the_rounds_of_a_session() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let split = Split::with_max_rounds(&rig, &store, 2).await;
            let start = split.start("invoice 7", true).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            split.backend.cancel(&split.alice, &start.id).await.unwrap();
            wait_for("the CancelTask", || async {
                (!rig.remote.cancels().is_empty()).then_some(())
            })
            .await;
            wait_for("the session idle", || async {
                let task = split.backend.get(&split.alice, &start.id).await.ok()??;
                (object(&task)["state"] == "idle").then_some(())
            })
            .await;
            rig.remote.release();
            let send = |text: &'static str| {
                split.backend.submit(
                    split.alice.clone(),
                    said(text, Some(&start.context_id), json!({})),
                    None,
                    Some(start.context_id.clone()),
                )
            };
            let second = send("invoice 8")
                .await
                .unwrap_or_else(|e| panic!("{backend}: the second round of two: {e:?}"));
            until(&split, &second.id, TaskState::Completed).await;
            let third = send("invoice 9").await;
            assert!(
                matches!(&third, Err(adam_a2a::BackendError::UnsupportedOperation(_))),
                "{backend}: the limit still holds for the person's own rounds: {third:?}"
            );
            split.stop().await;
        }
    }
    const HOUR: Duration = Duration::from_secs(3600);

    /// The timer runs of a session's rounds: runs of the helper that carry a `reap`.
    async fn timers(store: &DynStore, rig: &Rig, session: &str) -> Vec<adam_core::RunRecord> {
        let query = adam_core::RunQuery::new(
            format!("{}/billing", rig.root),
            adam_core::ConversationScope::Prefix(String::new()),
            100,
        );
        store
            .list_runs(&query)
            .await
            .unwrap()
            .into_iter()
            .filter(|run| run.state["agent"]["reap"]["session"] == session)
            .collect()
    }

    /// When the timer run is parked until, as it says itself.
    fn wakes(timer: &adam_core::RunRecord) -> Option<adam_core::time::Timestamp> {
        serde_json::from_value(timer.state["agent_wake"].clone()).ok()
    }

    /// The session whose first round is `id`, read through `rt`, which asks nothing of it.
    async fn session_of(rt: &Runtime, store: &DynStore, id: &str) -> SessionView {
        let session = RunId(id.parse().unwrap());
        let record = store.load_run(session).await.unwrap().expect("round 1");
        let (parent, _) =
            parse_session_conversation_id(record.conversation_id.as_deref().unwrap()).unwrap();
        rt.session_view(&parent, session)
            .await
            .unwrap()
            .expect("the session")
    }

    async fn run_status(store: &DynStore, run: RunId) -> RunStatus {
        store.load_run(run).await.unwrap().expect("the run").status
    }

    /// A round that may leave its session open sets one timer, parked for the idle TTL and a child
    /// of the round (so a cancel of the round reaches it); a round that ends a session that was
    /// asked to close sets none.
    #[tokio::test]
    async fn a_timer_is_set_only_for_a_round_that_may_stay_open_and_waits_for_the_ttl() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let clock = ManualClock::new();
            let split = Split::with_clock(&rig, &store, HOUR, &clock).await;
            // A close is asked while the round works: the session closes when the round ends.
            let closing = split.start("invoice 1", true).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            let context = closing.context_id.clone();
            split
                .backend
                .submit(
                    split.alice.clone(),
                    said("bye", Some(&context), json!({"close": {"cancel": false}})),
                    None,
                    Some(context.clone()),
                )
                .await
                .unwrap_or_else(|e| panic!("{backend}: the close was refused: {e:?}"));
            rig.remote.release();
            let round = RunId(closing.id.parse().unwrap());
            wait_for("the round to end", || async {
                (run_status(&store, round).await == RunStatus::Done).then_some(())
            })
            .await;
            let kept = split.start("invoice 2", true).await;
            until(&split, &kept.id, TaskState::Completed).await;

            let timer = wait_for("the timer parked", || async {
                let timers = timers(&store, &rig, &kept.id).await;
                (timers.len() == 1 && wakes(&timers[0]).is_some()).then(|| timers[0].clone())
            })
            .await;
            assert_eq!(
                timer.parent_id,
                Some(RunId(kept.id.parse().unwrap())),
                "{backend}: a child of its round"
            );
            let parked_for = wakes(&timer).expect("parked until") - timer.created_at;
            assert!(
                parked_for >= jiff::SignedDuration::from_secs(3590),
                "{backend}: parked for the idle TTL, not less: {parked_for}"
            );
            assert!(
                timers(&store, &rig, &closing.id).await.is_empty(),
                "{backend}: a session that closes with its round needs no timer"
            );
            split.stop().await;
        }
    }

    /// The timer of a round is for that round: once a later round exists, it does nothing, and
    /// the later round's own timer is the one that waits.
    #[tokio::test]
    async fn a_timer_of_an_earlier_round_does_nothing_once_a_later_round_exists() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let clock = ManualClock::new();
            let split = Split::with_clock(&rig, &store, HOUR, &clock).await;
            rig.remote.release();
            let start = split.start("invoice 1", true).await;
            until(&split, &start.id, TaskState::Completed).await;
            clock.advance(HOUR / 2);
            let again = split
                .backend
                .submit(
                    split.alice.clone(),
                    said("invoice 2", Some(&start.context_id), json!({})),
                    None,
                    Some(start.context_id.clone()),
                )
                .await
                .unwrap();
            until(&split, &again.id, TaskState::Completed).await;
            // The first round's timer is due, the second's is not.
            clock.advance(HOUR * 2 / 3);
            let first = child_run_id(RunId(start.id.parse().unwrap()), "reap");
            wait_for("the first timer to end", || async {
                (run_status(&store, first).await == RunStatus::Done).then_some(())
            })
            .await;
            let second = child_run_id(RunId(again.id.parse().unwrap()), "reap");
            assert_ne!(
                run_status(&store, second).await,
                RunStatus::Done,
                "{backend}"
            );
            let observer = rig.runtime_timed(&store, None, Some(HOUR), Some(&clock));
            let view = session_of(&observer, &store, &start.id).await;
            assert_eq!(view.round, 2, "{backend}: no closing round was started");
            assert!(!view.header.expect("a header").closing, "{backend}");
            assert!(rig.remote.cancels().is_empty(), "{backend}");
            split.stop().await;
        }
    }

    /// The timer closes the session when the session has expired as the process that reads it
    /// counts, not when its own clock says so: a worker that keeps a session longer than the one
    /// that set the timer looks again, and the closing round, once it starts, closes the session
    /// and cancels the remote task.
    #[tokio::test]
    async fn a_timer_that_wakes_before_the_session_expires_looks_again_and_then_closes_it() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let clock = ManualClock::new();
            let mut split = Split::with_clock(&rig, &store, HOUR, &clock).await;
            let start = split.start("invoice [input]", true).await;
            wait_for("the remote's task", || async {
                (!rig.remote.made().is_empty()).then_some(())
            })
            .await;
            let made = rig.remote.made();
            rig.remote.release();
            until(&split, &start.id, TaskState::Completed).await;
            let due = wait_for("the timer parked", || async {
                wakes(timers(&store, &rig, &start.id).await.first()?)
            })
            .await;
            split.stop_worker().await;

            // Another worker keeps sessions for two hours.
            let observer = rig.runtime_timed(&store, None, Some(2 * HOUR), Some(&clock));
            let worker = spawn_worker(&observer);
            clock.advance(HOUR * 7 / 6);
            let timer = wait_for("the timer to look and park again", || async {
                let timer = timers(&store, &rig, &start.id).await.remove(0);
                (timer.status == RunStatus::Done || wakes(&timer) != Some(due)).then_some(timer)
            })
            .await;
            assert_ne!(
                timer.status,
                RunStatus::Done,
                "{backend}: it must not close early"
            );
            let view = session_of(&observer, &store, &start.id).await;
            assert!(!view.header.expect("a header").closing, "{backend}");
            assert!(rig.remote.cancels().is_empty(), "{backend}");

            clock.advance(2 * HOUR);
            wait_for("the CancelTask", || async {
                (!rig.remote.cancels().is_empty()).then_some(())
            })
            .await;
            assert_eq!(rig.remote.cancels(), [made[0].0.clone()], "{backend}");
            let view = wait_for("the closing round to end", || async {
                let view = session_of(&observer, &store, &start.id).await;
                (view.state == adam_runtime::SessionState::Closed).then_some(view)
            })
            .await;
            let header = view.header.expect("a header");
            assert!(header.closing && header.closed, "{backend}: {header:?}");
            worker.stop().await;
        }
    }

    /// A session that expired idle, with its remote task open and nobody to cancel it, is
    /// followed by a closing round as soon as anything lists the sessions of its context, a
    /// message to start another one included.
    #[tokio::test]
    async fn listing_the_sessions_of_a_context_cleans_up_an_expired_remote_one() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let clock = ManualClock::new();
            let mut split = Split::with_clock(&rig, &store, HOUR, &clock).await;
            let start = split.start("invoice [input]", true).await;
            rig.remote.release();
            until(&split, &start.id, TaskState::Completed).await;
            split.stop_worker().await;
            clock.advance(2 * HOUR);

            split.start("invoice 2", true).await;
            let observer = rig.runtime_timed(&store, None, Some(HOUR), Some(&clock));
            let view = session_of(&observer, &store, &start.id).await;
            assert_eq!(view.round, 2, "{backend}: the listing started the cleanup");
            assert!(view.header.expect("a header").closing, "{backend}");
        }
    }

    /// Closing an expired session whose remote task is still open answers with the round that ends
    /// it, not with the round that expired.
    #[tokio::test]
    async fn closing_an_expired_remote_session_answers_with_the_round_that_cancels() {
        for (backend, store) in stores().await {
            let rig = Rig::new(usize::MAX, false).await;
            let clock = ManualClock::new();
            let mut split = Split::with_clock(&rig, &store, HOUR, &clock).await;
            let start = split.start("invoice [input]", true).await;
            rig.remote.release();
            until(&split, &start.id, TaskState::Completed).await;
            split.stop_worker().await;
            clock.advance(2 * HOUR);

            let context = start.context_id.clone();
            let answered = split
                .backend
                .submit(
                    split.alice.clone(),
                    said("bye", Some(&context), json!({"close": {"cancel": false}})),
                    None,
                    Some(context.clone()),
                )
                .await
                .unwrap_or_else(|e| panic!("{backend}: the close was refused: {e:?}"));
            assert_ne!(
                answered.id, start.id,
                "{backend}: the answer is the round that cancels, not the expired one"
            );
        }
    }
}
