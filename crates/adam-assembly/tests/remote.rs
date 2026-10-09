//! Remote (A2A) subagents: a tool on the parent whose call is a journaled `SendMessage`, and a
//! wait that looks at the remote task with `GetTask` on the timer. Run end to end on `MockModel`
//! through the runtime, against an in-process `adam-a2a` server: the reference `InMemoryBackend`
//! where it does what the case needs, and a scripted backend where it cannot (a failing task, a
//! server that recognises repeated message ids, a lost response). The memory store always,
//! PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{Artifact, Message, Part, Role, Task, TaskState, TaskStatus};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, InMemoryBackend, TaskBackend,
    TaskEvent,
};
use adam_agent_fs::Subagent;
use adam_assembly::{AgentDef, Assembly, Error, RemoteAuthProblem, RemoteUrlProblem, ToolClash};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus, Store};
use adam_llm_agent::{ToolSet, user_message};
use adam_model::{Message as ModelMessage, MockModel, ModelRequest, ToolCall};
use adam_runtime::{CollectingSink, RetryPolicy, Runtime, child_run_id};
use async_trait::async_trait;
use axum::extract::{Request, State};
use axum::middleware::{self, Next};
use common::{def, instructions, spawn_worker, tools, wait_done, wait_for};
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::net::TcpListener;

const TOKEN: &str = "tok-7f3a9c2e51d84b06";
/// The start of a PNG: enough for the bytes to say what they are.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const VAR: &str = "BILLING_AGENT_TOKEN";
const NOTE: &str =
    "The agent does not see this conversation; put everything it needs in `message`.";

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
    }
}

fn last_tool_result(request: &ModelRequest, id: &str) -> (String, bool) {
    match request.messages.last().unwrap() {
        ModelMessage::Tool {
            call_id,
            content,
            is_error,
        } if call_id == id => (content.clone(), *is_error),
        other => panic!("the last message is not the result of `{id}`: {other:?}"),
    }
}

type Seen = Arc<Mutex<Vec<Option<String>>>>;

/// An A2A server on a local port, behind bearer authentication, that notes the `Authorization`
/// header of every request it gets.
struct Server {
    addr: SocketAddr,
    authorization: Seen,
}

impl Server {
    async fn start(backend: impl TaskBackend, token: &str) -> Self {
        Self::start_as(backend, token, |addr| format!("http://{addr}/")).await
    }

    /// `public` is the URL the card advertises for the JSON-RPC endpoint.
    async fn start_as(
        backend: impl TaskBackend,
        token: &str,
        public: impl FnOnce(SocketAddr) -> String,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let card = AgentCardConfig::new(
            "billing",
            "Answers billing questions",
            public(addr).parse().unwrap(),
            "0.1.0",
        );
        let auth = AuthConfig::BearerTokens(vec![token.to_owned().into()]);
        let authorization = Seen::default();
        let app =
            A2aServer::router(card, Arc::new(backend), auth).layer(middleware::from_fn_with_state(
                authorization.clone(),
                |State(seen): State<Seen>, request: Request, next: Next| async move {
                    let header = request
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .map(|v| v.to_str().unwrap().to_owned());
                    seen.lock().unwrap().push(header);
                    next.run(request).await
                },
            ));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            addr,
            authorization,
        }
    }

    fn card_url(&self) -> String {
        format!("http://{}/.well-known/agent-card.json", self.addr)
    }
}

/// A backend that plays scripts. Each task starts `working` and reaches its final state on the
/// `done_after`-th `GetTask`. The text of the message picks the ending: `[fail]` fails it, anything
/// else completes it with the artifact `done: <text>`.
#[derive(Clone)]
struct Scripted {
    state: Arc<Mutex<ScriptState>>,
    /// The `GetTask` on which a task ends; the count of `usize::MAX` means never.
    done_after: Arc<AtomicUsize>,
    /// Hand back the task a repeated message id already made, as `adam-a2a-runtime` does.
    dedupe: bool,
    /// The first `SendMessage` is processed and then answered with an error: a lost response.
    lose_first_response: Arc<AtomicBool>,
}

#[derive(Default)]
struct ScriptState {
    tasks: HashMap<String, Entry>,
    by_message: HashMap<String, String>,
    submits: Vec<Submit>,
    gets: usize,
}

struct Entry {
    task: Task,
    text: String,
    gets: usize,
}

#[derive(Clone, Debug)]
struct Submit {
    message_id: String,
    text: String,
    subject: String,
}

impl Scripted {
    fn new(done_after: usize) -> Self {
        Self {
            state: Arc::default(),
            done_after: Arc::new(AtomicUsize::new(done_after)),
            dedupe: true,
            lose_first_response: Arc::default(),
        }
    }

    fn submits(&self) -> Vec<Submit> {
        self.state.lock().unwrap().submits.clone()
    }

    fn tasks(&self) -> usize {
        self.state.lock().unwrap().tasks.len()
    }

    fn gets(&self) -> usize {
        self.state.lock().unwrap().gets
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
impl TaskBackend for Scripted {
    async fn submit(
        &self,
        caller: Caller,
        message: Message,
        _task_id: Option<String>,
        _context_id: Option<String>,
    ) -> Result<Task, BackendError> {
        let text = message.text().unwrap_or_default().to_owned();
        let task = {
            let mut state = self.state.lock().unwrap();
            state.submits.push(Submit {
                message_id: message.message_id.clone(),
                text: text.clone(),
                subject: caller.subject.clone(),
            });
            let known = state.by_message.get(&message.message_id).cloned();
            match known.filter(|_| self.dedupe) {
                Some(id) => state.tasks[&id].task.clone(),
                None => {
                    let id = a2a::new_task_id();
                    let task = Task {
                        id: id.clone(),
                        context_id: a2a::new_context_id(),
                        status: status(TaskState::Working, None),
                        artifacts: None,
                        history: None,
                        metadata: None,
                    };
                    state
                        .by_message
                        .insert(message.message_id.clone(), id.clone());
                    state.tasks.insert(
                        id,
                        Entry {
                            task: task.clone(),
                            text,
                            gets: 0,
                        },
                    );
                    task
                }
            }
        };
        if self.lose_first_response.swap(false, SeqCst) {
            return Err(BackendError::unavailable("the response was lost"));
        }
        Ok(task)
    }

    async fn get(&self, _caller: &Caller, task_id: &str) -> Result<Option<Task>, BackendError> {
        let mut state = self.state.lock().unwrap();
        state.gets += 1;
        let Some(entry) = state.tasks.get_mut(task_id) else {
            return Ok(None);
        };
        entry.gets += 1;
        if entry.gets >= self.done_after.load(SeqCst)
            && entry.task.status.state == TaskState::Working
        {
            if entry.text.contains("[fail]") {
                entry.task.status = status(TaskState::Failed, Some("out of budget"));
            } else if entry.text.contains("[file]") {
                // A browser's answer: a line and a screenshot, the file `share_file` would make.
                let mut shot = Part::raw(PNG.to_vec());
                shot.filename = Some("page.png".into());
                shot.media_type = Some("image/png".into());
                entry.task.status = status(TaskState::Completed, None);
                entry.task.artifacts = Some(vec![
                    Artifact {
                        artifact_id: a2a::new_artifact_id(),
                        name: None,
                        description: None,
                        parts: vec![Part::text("The page is blank.")],
                        metadata: None,
                        extensions: None,
                    },
                    Artifact {
                        artifact_id: a2a::new_artifact_id(),
                        name: Some("page.png".into()),
                        description: None,
                        parts: vec![shot],
                        metadata: None,
                        extensions: None,
                    },
                ]);
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
        Err(BackendError::TaskNotFound(task_id.to_owned()))
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

type Files = Vec<(String, String)>;

/// A root (no tools of its own) with the remote subagent `billing` at `card_url`.
fn remote_files(root: &str, card_url: &str, auth: Option<&str>) -> Files {
    let auth = auth.map_or_else(String::new, |var| format!("\nauth: bearer:{var}"));
    vec![
        (
            "agent/instructions.md".into(),
            instructions(&format!("name: {root}\ntools: []"), "You are the root."),
        ),
        (
            "agent/subagents/billing.md".into(),
            format!(
                "---\ndescription: Handles billing questions for a customer account.\n\
                 a2a: {card_url}{auth}\n---\n"
            ),
        ),
    ]
}

fn def_of(files: &Files) -> AgentDef {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    def(&refs)
}

/// The assembly of `files`, with the token in `VAR` and a wait timer of 30 ms.
fn assemble(files: &Files, model: &Arc<MockModel>) -> Assembly {
    def_of(files)
        .env(VAR, TOKEN)
        .bind(ToolSet::new())
        .unwrap()
        .wait_poll(Duration::from_millis(30))
        .model(model.clone(), "default")
        .unwrap()
}

fn runtime(assembly: &Assembly, store: &DynStore) -> Runtime {
    assembly
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

/// The model calls `billing` (call id `c1`) with `message`, then says `last`.
fn script(model: &MockModel, message: &str, last: &str) {
    model
        .push_tool_calls(vec![call("c1", "billing", json!({"message": message}))])
        .push_text(last);
}

/// Run a parent that calls `billing` once with `message`, on every store, and hand each run to
/// `check` with the assembly's model requests.
async fn run_case(
    files: impl Fn() -> Files,
    message: &str,
    check: impl AsyncFn(&str, &DynStore, RunId, Vec<ModelRequest>),
) {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let model = Arc::new(MockModel::new());
        script(&model, message, "noted");
        let mut files = files();
        files[0].1 = instructions(&format!("name: {root}\ntools: []"), "You are the root.");
        let assembly = assemble(&files, &model);
        let rt = runtime(&assembly, &store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;
        check(backend, &store, run, model.requests()).await;
    }
}

// --- the happy path ------------------------------------------------------------------------

#[tokio::test]
async fn a_remote_subagent_is_called_polled_and_its_answer_comes_back() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let memory = InMemoryBackend::new();
        let server = Server::start(memory.clone(), TOKEN).await;
        let files = remote_files(&root, &server.card_url(), Some(VAR));
        let model = Arc::new(MockModel::new());
        script(&model, "invoice 7", "the invoice is settled");
        let assembly = assemble(&files, &model);
        // The tool is the parent's, after its own (none), like a local subagent.
        assert_eq!(assembly.info()[0].tools, ["billing"], "{backend}");
        assert_eq!(assembly.remotes().len(), 1, "{backend}");

        let sink = CollectingSink::new();
        let rt = assembly
            .register(Runtime::builder(store.clone()))
            .poll_interval(Duration::from_millis(20))
            .event_sink(sink.clone())
            .build();
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        let view = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            view.output.as_ref().unwrap()["text"],
            "the invoice is settled",
            "{backend}"
        );

        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        // Same shape as a local subagent: `{message}` and the same suffix.
        assert_eq!(requests[0].tools.len(), 1);
        assert_eq!(
            requests[0].tools[0].description,
            format!("Handles billing questions for a customer account. {NOTE}"),
            "{backend}"
        );
        assert_eq!(
            requests[0].tools[0].parameters["required"],
            json!(["message"])
        );
        assert_eq!(
            last_tool_result(&requests[1], "c1"),
            ("echo: invoice 7".into(), false),
            "{backend}"
        );

        // One task on the remote, made by one message whose id is derived from run and call, and
        // asked for on behalf of the first configured token.
        let ids = memory.task_ids();
        assert_eq!(ids.len(), 1, "{backend}");
        let task = memory
            .get(&Caller::new("token-0"), &ids[0])
            .await
            .unwrap()
            .expect("the task belongs to the token's caller");
        assert_eq!(
            task.history.as_ref().unwrap()[0].message_id,
            child_run_id(run, "c1").to_string(),
            "{backend}"
        );

        // Every request said who it was: the card, the send, each poll.
        let seen = server.authorization.lock().unwrap().clone();
        assert!(seen.len() >= 3, "{backend}: {seen:?}");
        for header in &seen {
            assert_eq!(header.as_deref(), Some(format!("Bearer {TOKEN}").as_str()));
        }

        // The journal records a send that parked and at least one look, and the token is nowhere:
        // not in the journal, the run's state, its result or the events.
        let journal = store.journal_list(run).await.unwrap();
        let names: Vec<&str> = journal.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"tool:c1"), "{backend}: {names:?}");
        assert!(names.contains(&"poll:c1"), "{backend}: {names:?}");
        let tool_entry = journal.iter().find(|e| e.name == "tool:c1").unwrap();
        assert!(!tool_entry.ok, "the send is journaled as a wait");
        assert!(
            tool_entry.payload["AwaitRemote"]["task"].is_string(),
            "{backend}: {:?}",
            tool_entry.payload
        );
        let everything = format!(
            "{journal:?}{:?}{:?}{:?}",
            view.state,
            view.output,
            sink.events()
        );
        assert!(!everything.contains(TOKEN), "{backend}: the token leaked");
    }
}

// --- endings that are not success ----------------------------------------------------------

#[tokio::test]
async fn a_remote_task_that_fails_is_an_error_result_and_the_parent_goes_on() {
    let scripted = Scripted::new(2);
    let server = Server::start(scripted.clone(), TOKEN).await;
    let url = server.card_url();
    run_case(
        || remote_files("x", &url, Some(VAR)),
        "please [fail]",
        async |backend, _store, _run, requests| {
            assert_eq!(requests.len(), 2, "{backend}");
            assert_eq!(
                last_tool_result(&requests[1], "c1"),
                (
                    "the remote agent `billing` failed: out of budget".into(),
                    true
                ),
                "{backend}"
            );
        },
    )
    .await;
    assert!(scripted.gets() >= 2);
}

#[tokio::test]
async fn a_remote_task_that_is_canceled_is_an_error_result() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let memory = InMemoryBackend::new();
        let server = Server::start(memory.clone(), TOKEN).await;
        let files = remote_files(&root, &server.card_url(), Some(VAR));
        let model = Arc::new(MockModel::new());
        script(&model, "[hold] wait for me", "gave up");
        let rt = runtime(&assemble(&files, &model), &store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();

        // The task is held; someone on the remote side cancels it while the parent waits.
        let id = wait_for("the remote task", || async {
            memory.task_ids().first().cloned()
        })
        .await;
        wait_for("the parent to wait on it", || async {
            let record = store.load_run(run).await.unwrap()?;
            (record.status == RunStatus::Parked).then_some(())
        })
        .await;
        memory.cancel(&Caller::new("token-0"), &id).await.unwrap();

        wait_done(&rt, run).await;
        worker.stop().await;
        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        let (text, is_error) = last_tool_result(&requests[1], "c1");
        assert!(is_error, "{backend}");
        assert!(
            text.starts_with("the remote task of `billing` was canceled"),
            "{backend}: {text}"
        );
    }
}

#[tokio::test]
async fn a_remote_that_needs_input_is_an_error_result_because_nobody_can_answer() {
    let server = Server::start(InMemoryBackend::new(), TOKEN).await;
    let url = server.card_url();
    run_case(
        || remote_files("x", &url, Some(VAR)),
        "[input-required] which invoice?",
        async |backend, _store, _run, requests| {
            let (text, is_error) = last_tool_result(&requests[1], "c1");
            assert!(is_error, "{backend}");
            assert!(
                text.starts_with(
                    "the remote agent `billing` needs more input: more input required."
                ),
                "{backend}: {text}"
            );
            assert!(text.contains("cannot ask the user"), "{backend}: {text}");
        },
    )
    .await;
}

#[tokio::test]
async fn a_blank_message_is_an_error_result_and_sends_nothing() {
    let scripted = Scripted::new(1);
    let server = Server::start(scripted.clone(), TOKEN).await;
    let url = server.card_url();
    run_case(
        || remote_files("x", &url, Some(VAR)),
        "  ",
        async |backend, _store, _run, requests| {
            let (text, is_error) = last_tool_result(&requests[1], "c1");
            assert!(is_error, "{backend}");
            assert!(
                text.contains("`billing` needs `message`"),
                "{backend}: {text}"
            );
        },
    )
    .await;
    assert!(scripted.submits().is_empty());
    assert!(
        server.authorization.lock().unwrap().is_empty(),
        "not even the card was fetched"
    );
}

// --- credentials ---------------------------------------------------------------------------

#[test]
fn a_missing_token_variable_is_a_bind_error_that_names_it() {
    let files = remote_files("coder", "https://billing.example.com/card", Some(VAR));
    // No `env(..)`, and nothing sets the variable in the process.
    let error = def_of(&files).bind(ToolSet::new()).unwrap_err();
    let Error::RemoteAuth {
        origin,
        var,
        problem,
    } = &error
    else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(origin.agent, "coder/billing");
    assert_eq!(origin.file.to_string_lossy(), "agent/subagents/billing.md");
    assert_eq!((var.as_str(), *problem), (VAR, RemoteAuthProblem::Missing));
    assert!(error.to_string().contains(VAR), "{error}");

    // Empty, or not a token: refused too, without the value in the message.
    for (value, problem) in [
        ("  ", RemoteAuthProblem::Empty),
        ("has space", RemoteAuthProblem::NotAToken),
    ] {
        let error = def_of(&files)
            .env(VAR, value)
            .bind(ToolSet::new())
            .unwrap_err();
        assert!(
            matches!(&error, Error::RemoteAuth { problem: p, .. } if *p == problem),
            "{error}"
        );
        assert!(!error.to_string().contains(value.trim()) || value.trim().is_empty());
    }

    // A variable set in the process is read too (`PATH` is always there).
    let files = remote_files("coder", "https://billing.example.com/card", Some("PATH"));
    def_of(&files).bind(ToolSet::new()).unwrap();
}

#[tokio::test]
async fn a_wrong_token_is_an_error_result_not_a_failed_run_and_the_token_stays_out_of_it() {
    let server = Server::start(InMemoryBackend::new(), TOKEN).await;
    let url = server.card_url();
    for (backend, store) in stores().await {
        let root = uniq("root");
        let model = Arc::new(MockModel::new());
        script(&model, "invoice 7", "could not ask");
        let files = remote_files(&root, &url, Some(VAR));
        let assembly = def_of(&files)
            .env(VAR, "some-other-token")
            .bind(ToolSet::new())
            .unwrap()
            .model(model.clone(), "default")
            .unwrap();
        let rt = runtime(&assembly, &store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;
        let (text, is_error) = last_tool_result(&model.requests()[1], "c1");
        assert!(is_error, "{backend}");
        assert!(
            text.starts_with("cannot send the message to the remote agent `billing`"),
            "{backend}: {text}"
        );
        assert!(!text.contains("some-other-token"), "{text}");
    }
}

#[tokio::test]
async fn a_card_that_points_the_token_at_another_origin_is_refused_before_anything_is_sent() {
    let scripted = Scripted::new(1);
    // The card is served here but advertises `localhost`: another host as far as the token goes.
    let server = Server::start_as(scripted.clone(), TOKEN, |addr| {
        format!("http://localhost:{}/", addr.port())
    })
    .await;
    let url = server.card_url();
    run_case(
        || remote_files("x", &url, Some(VAR)),
        "invoice 7",
        async |backend, _store, _run, requests| {
            let (text, is_error) = last_tool_result(&requests[1], "c1");
            assert!(is_error, "{backend}");
            assert!(
                text.contains("offers no interface that can be used")
                    && text.contains("same host and port as the card"),
                "{backend}: {text}"
            );
        },
    )
    .await;
    assert!(scripted.submits().is_empty(), "nothing was sent");
    let seen = server.authorization.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|h| h.as_deref() == Some(&format!("Bearer {TOKEN}"))),
        "{seen:?}"
    );
    // Only the card was asked for (once per call, per store).
    assert!(!seen.is_empty());
}

/// The token is in no log line, at any level, from the bind to the end of the run.
///
/// The capture is `adam_mcp_testkit::LogCapture`: one global subscriber for the test binary, a
/// buffer per thread. A scoped subscriber (`tracing::subscriber::set_default`) made this test
/// flaky: the callsite of "remote task started" caches whether anybody listens, a sibling test
/// that reaches it first on its own thread (where no subscriber is set yet) caches "nobody", and
/// that can land after this test's subscriber was registered, so the line was never written.
#[tokio::test]
async fn the_token_is_not_logged() {
    let logs = adam_mcp_testkit::LogCapture::start();
    let memory = InMemoryBackend::new();
    let server = Server::start(memory, TOKEN).await;
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    script(&model, "invoice 7", "done");
    let files = remote_files(&root, &server.card_url(), Some(VAR));
    let assembly = assemble(&files, &model);
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = runtime(&assembly, &store);
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;

    let text = logs.text();
    assert!(
        text.contains("remote task started"),
        "the logs are being captured"
    );
    assert!(!text.contains(TOKEN), "the token is in the logs");
    assert!(!format!("{assembly:?}").contains(TOKEN));
}

// --- durability ----------------------------------------------------------------------------

#[tokio::test]
async fn a_restart_mid_wait_resumes_polling_without_sending_again() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let scripted = Scripted::new(usize::MAX);
        let server = Server::start(scripted.clone(), TOKEN).await;
        let files = remote_files(&root, &server.card_url(), Some(VAR));
        let model = Arc::new(MockModel::new());
        script(&model, "invoice 7", "settled after the restart");

        // The first process sends, parks, and looks a few times: still going.
        let first = runtime(&assemble(&files, &model), &store);
        let worker = spawn_worker(&first);
        let run = first.start(&root, user_message("go"), None).await.unwrap();
        wait_for("a few looks", || async {
            (scripted.gets() >= 2).then_some(())
        })
        .await;
        worker.stop().await;
        drop(first);
        assert_eq!(scripted.submits().len(), 1, "{backend}");
        assert_eq!(model.requests().len(), 1, "{backend}: only the send so far");

        // The remote finishes while nobody is looking. A new process, from the same files (its
        // own tool, its own client), finds the wait in the run's state and asks again.
        scripted.done_after.store(0, SeqCst);
        let second = runtime(&assemble(&files, &model), &store);
        let worker = spawn_worker(&second);
        let view = wait_done(&second, run).await;
        worker.stop().await;
        assert_eq!(
            view.output.as_ref().unwrap()["text"],
            "settled after the restart",
            "{backend}"
        );
        assert_eq!(
            scripted.submits().len(),
            1,
            "{backend}: the message was not sent again"
        );
        assert_eq!(scripted.tasks(), 1, "{backend}");
        assert_eq!(
            last_tool_result(&model.requests()[1], "c1"),
            ("done: invoice 7".into(), false),
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_send_whose_response_is_lost_is_retried_under_the_same_message_id() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let scripted = Scripted::new(1);
        scripted.lose_first_response.store(true, SeqCst);
        let server = Server::start(scripted.clone(), TOKEN).await;
        let files = remote_files(&root, &server.card_url(), Some(VAR));
        let model = Arc::new(MockModel::new());
        // A retry runs the whole transition again, the model turn included: the model is asked
        // twice, and asks for the same call twice.
        model
            .push_tool_calls(vec![call("c1", "billing", json!({"message": "invoice 7"}))])
            .push_tool_calls(vec![call("c1", "billing", json!({"message": "invoice 7"}))])
            .push_text("settled");
        let rt = runtime(&assemble(&files, &model), &store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;

        // The server saw the message twice, under one id, and made one task: the retry found it.
        let submits = scripted.submits();
        assert_eq!(submits.len(), 2, "{backend}: {submits:?}");
        assert_eq!(submits[0].message_id, submits[1].message_id, "{backend}");
        assert_eq!(
            submits[0].message_id,
            child_run_id(run, "c1").to_string(),
            "{backend}"
        );
        assert_eq!(submits[0].subject, "token-0");
        assert_eq!(submits[0].text, "invoice 7");
        assert_eq!(scripted.tasks(), 1, "{backend}");
        let requests = model.requests();
        assert_eq!(requests.len(), 3, "{backend}");
        assert_eq!(
            last_tool_result(&requests[2], "c1"),
            ("done: invoice 7".into(), false),
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_task_that_never_ends_is_given_up_on_after_the_limit() {
    let scripted = Scripted::new(usize::MAX);
    let server = Server::start(scripted.clone(), TOKEN).await;
    for (backend, store) in stores().await {
        let root = uniq("root");
        let model = Arc::new(MockModel::new());
        script(&model, "invoice 7", "gave up");
        let files = remote_files(&root, &server.card_url(), Some(VAR));
        let assembly = def_of(&files)
            .env(VAR, TOKEN)
            .remote_timeout(Duration::from_millis(150))
            .bind(ToolSet::new())
            .unwrap()
            .wait_poll(Duration::from_millis(30))
            .model(model.clone(), "default")
            .unwrap();
        let rt = runtime(&assembly, &store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;
        let (text, is_error) = last_tool_result(&model.requests()[1], "c1");
        assert!(is_error, "{backend}");
        assert!(
            text.contains("did not finish in the time allowed"),
            "{backend}: {text}"
        );
    }
}

// --- the URL, and the names ----------------------------------------------------------------

#[test]
fn a_remote_over_plain_http_is_refused_unless_it_is_local_or_the_deployment_allows_it() {
    let files = remote_files("coder", "http://billing.example.com/card", None);
    let error = def_of(&files).bind(ToolSet::new()).unwrap_err();
    let Error::RemoteUrl {
        origin,
        url,
        problem,
    } = &error
    else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(origin.agent, "coder/billing");
    assert_eq!(url, "http://billing.example.com/card");
    assert_eq!(*problem, RemoteUrlProblem::Insecure);
    assert!(error.to_string().contains("in the clear"), "{error}");

    // The explicit development switch.
    def_of(&files)
        .allow_insecure_remotes(true)
        .bind(ToolSet::new())
        .unwrap();
    // This machine never needs it.
    for local in [
        "http://localhost:8080/card",
        "http://127.0.0.1:8080/card",
        "http://[::1]:8080/card",
    ] {
        def_of(&remote_files("coder", local, None))
            .bind(ToolSet::new())
            .unwrap();
    }
    // https is always fine; credentials in a URL never are.
    def_of(&remote_files(
        "coder",
        "https://billing.example.com/card",
        None,
    ))
    .bind(ToolSet::new())
    .unwrap();
    let error = def_of(&remote_files(
        "coder",
        "https://u:p@billing.example.com/card",
        None,
    ))
    .bind(ToolSet::new())
    .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::RemoteUrl { url, problem: RemoteUrlProblem::Credentials, .. }
                if url == "https://billing.example.com/card"
        ),
        "{error}"
    );
}

#[test]
fn a_remote_named_like_a_tool_of_its_parent_or_a_skill_tool_or_a_subagent_is_refused() {
    // The root has every registered tool, and one is called `billing`.
    let files = remote_files("coder", "https://billing.example.com/card", None);
    let mut files = files;
    files[0].1 = instructions("name: coder", "You are the root.");
    let error = def_of(&files).bind(tools(&["billing"])).unwrap_err();
    let Error::SubagentToolClash {
        origin,
        parent,
        tool,
        clash,
    } = &error
    else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(origin.agent, "coder/billing");
    assert_eq!(origin.file.to_string_lossy(), "agent/subagents/billing.md");
    assert_eq!((parent.as_str(), tool.as_str()), ("coder", "billing"));
    assert_eq!(*clash, ToolClash::Tool);

    // A registered tool the parent does not list is no clash.
    let mut ok = remote_files("coder", "https://billing.example.com/card", None);
    ok[0].1 = instructions("name: coder\ntools: [other]", "You are the root.");
    let assembly = def_of(&ok)
        .bind(tools(&["other", "billing"]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "default")
        .unwrap();
    assert_eq!(assembly.info()[0].tools, ["other", "billing"]);

    // A remote named `load_skill`, on a parent with skills.
    let mut skill = remote_files("coder", "https://billing.example.com/card", None);
    skill[1].0 = "agent/subagents/load_skill.md".into();
    skill.push((
        "agent/skills/notes/SKILL.md".into(),
        "---\nname: notes\ndescription: Notes about things.\n---\nBody.\n".into(),
    ));
    let error = def_of(&skill).bind(ToolSet::new()).unwrap_err();
    assert!(
        matches!(&error, Error::SubagentToolClash { clash: ToolClash::SkillTool, tool, .. } if tool == "load_skill"),
        "{error}"
    );

    // A remote and a local subagent of one name cannot be written as files (the loader refuses
    // it), but a manifest built by hand can have both, and then bind refuses.
    let mut both = remote_files("coder", "https://billing.example.com/card", None);
    both.push((
        "agent/subagents/other.md".into(),
        instructions("description: Bills.", "You bill."),
    ));
    let refs: Vec<(&str, &str)> = both
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let (_dir, manifests) = common::manifests(&refs);
    let mut manifest = manifests[0].clone();
    let Some(Subagent::Local(local)) = manifest
        .subagents
        .iter()
        .find(|s| matches!(s, Subagent::Local(_)))
        .cloned()
    else {
        panic!("a local subagent");
    };
    let mut twin = local;
    twin.name = "billing".into();
    manifest.subagents.push(Subagent::Local(twin));
    let error = AgentDef::from_manifest(manifest)
        .unwrap()
        .bind(ToolSet::new())
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::SubagentToolClash {
                clash: ToolClash::Subagent { file },
                tool,
                ..
            } if tool == "billing" && file.to_string_lossy() == "agent/subagents/billing.md"
        ),
        "{error}"
    );

    // The loader itself refuses the pair written as files.
    let mut same = remote_files("coder", "https://billing.example.com/card", None);
    same.push((
        "agent/subagents/billing/instructions.md".into(),
        instructions("description: Bills.", "You bill."),
    ));
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in &same {
        common::write(dir.path(), &[(path.as_str(), text.as_str())]);
    }
    let loaded = adam_agent_fs::ManifestSource::load(&adam_agent_fs::Dir::new(dir.path()))
        .unwrap()
        .into_package(adam_agent_fs::Strictness::Lenient);
    assert!(
        loaded.is_err(),
        "the loader refuses two subagents of one name"
    );
}

#[test]
fn a_remote_subagent_is_a_tool_in_the_order_of_the_manifest_beside_local_ones() {
    let mut files = remote_files("coder", "https://billing.example.com/card", None);
    files.push((
        "agent/subagents/audit.md".into(),
        instructions("description: Audits.", "You audit."),
    ));
    files[0].1 = instructions("name: coder", "You are the root.");
    let assembly = def_of(&files)
        .bind(tools(&["read"]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "default")
        .unwrap();
    // Own tools first, then the subagents by file name: `audit` (local) before `billing` (remote).
    assert_eq!(assembly.info()[0].tools, ["read", "audit", "billing"]);
    // The remote is not an agent of the assembly: nothing to register on the runtime.
    assert_eq!(assembly.agents().len(), 2);
}

// --- a remote under a local subagent ---------------------------------------------------------

/// A local subagent calls a remote one: the remote is declared under the subagent's own directory
/// (`subagents/researcher/subagents/browser.md`), so it is a tool of the researcher, with its own
/// `auth`, and the researcher's child run sends, parks on the remote task, polls it and goes on,
/// exactly as a root does. Only text travels: the researcher's answer is what the root reads.
#[tokio::test]
async fn a_local_subagent_calls_a_remote_subagent_declared_in_its_own_directory() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let memory = InMemoryBackend::new();
        let server = Server::start(memory.clone(), TOKEN).await;
        let files: Files = vec![
            (
                "agent/instructions.md".into(),
                instructions(&format!("name: {root}\ntools: []"), "You are the root."),
            ),
            (
                "agent/subagents/researcher/instructions.md".into(),
                instructions(
                    "description: Researches a question.\ntools: []",
                    "You research.",
                ),
            ),
            (
                "agent/subagents/researcher/subagents/browser.md".into(),
                format!(
                    "---\ndescription: Reads web pages.\na2a: {}\nauth: bearer:{VAR}\n---\n",
                    server.card_url()
                ),
            ),
        ];
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![call(
                "c1",
                "researcher",
                json!({"message": "what does example.com say?"}),
            )])
            .push_tool_calls(vec![call(
                "r1",
                "browser",
                json!({"message": "read example.com"}),
            )])
            .push_text("It says: echo: read example.com")
            .push_text("The researcher found it.");
        let assembly = assemble(&files, &model);
        // The remote is a tool of the subagent whose directory declares it, not of the root.
        let tools_of = |name: &str| {
            assembly
                .info()
                .iter()
                .find(|i| i.name == name)
                .unwrap()
                .tools
                .clone()
        };
        assert_eq!(tools_of(&root), ["researcher"], "{backend}");
        assert_eq!(
            tools_of(&format!("{root}/researcher")),
            ["browser"],
            "{backend}"
        );

        let rt = runtime(&assembly, &store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        let view = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            view.output.as_ref().unwrap()["text"],
            "The researcher found it.",
            "{backend}"
        );

        let requests = model.requests();
        assert_eq!(requests.len(), 4, "{backend}");
        assert_eq!(
            last_tool_result(&requests[2], "r1"),
            ("echo: read example.com".into(), false),
            "{backend}: the researcher read the remote's answer"
        );
        assert_eq!(
            last_tool_result(&requests[3], "c1"),
            ("It says: echo: read example.com".into(), false),
            "{backend}: the root read the researcher's text"
        );
        // The send and its wait are journaled steps of the researcher's run, under the id derived
        // from that run and its call.
        let child = child_run_id(run, "c1");
        let names: Vec<String> = store
            .journal_list(child)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.iter().any(|n| n == "tool:r1"), "{backend}: {names:?}");
        let ids = memory.task_ids();
        assert_eq!(ids.len(), 1, "{backend}");
        let task = memory
            .get(&Caller::new("token-0"), &ids[0])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            task.history.as_ref().unwrap()[0].message_id,
            child_run_id(child, "r1").to_string(),
            "{backend}"
        );
        for header in server.authorization.lock().unwrap().iter() {
            assert_eq!(header.as_deref(), Some(format!("Bearer {TOKEN}").as_str()));
        }
    }
}

// --- files of a remote's answer ------------------------------------------------------------

/// A remote subagent whose file says `files: true` (a browser agent): the screenshot its task
/// answers with is an artifact of the run that called it (in the run's view, so the parent's A2A
/// client gets it as it gets a shared file), the model reads a line in its place, and the same remote
/// without the key keeps nothing.
#[tokio::test]
async fn the_files_of_a_remote_with_files_true_are_artifacts_of_the_calling_run() {
    for files in [true, false] {
        let scripted = Scripted::new(1);
        let server = Server::start(scripted.clone(), TOKEN).await;
        let url = server.card_url();
        run_case(
            || {
                let mut files_of = remote_files("x", &url, Some(VAR));
                if files {
                    files_of[1].1 = files_of[1].1.replace("\n---\n", "\nfiles: true\n---\n");
                }
                files_of
            },
            "screenshot it [file]",
            async |backend, store, run, requests| {
                let (text, is_error) = last_tool_result(&requests[1], "c1");
                assert!(!is_error, "{backend}: {text}");
                let view = Runtime::builder(store.clone())
                    .build()
                    .view(run)
                    .await
                    .unwrap()
                    .unwrap();
                if files {
                    assert_eq!(
                        text,
                        "The page is blank.\n\nShared page.png (16 bytes, image/png). To show it in \
                         your answer, write ![description](page.png).",
                        "{backend}"
                    );
                    let names: Vec<&str> =
                        view.artifacts.iter().map(|a| a.name.as_str()).collect();
                    assert_eq!(names, ["page.png"], "{backend}");
                    assert_eq!(view.artifacts[0].file.as_ref().unwrap().bytes, PNG);
                } else {
                    assert!(
                        text.ends_with("[file `page.png` not included: 16 bytes, image/png]"),
                        "{backend}: {text}"
                    );
                    assert!(view.artifacts.is_empty(), "{backend}");
                }
            },
        )
        .await;
    }
}
