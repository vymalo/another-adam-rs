//! The agent a folder describes, in this process: the card, the answer in role, the tools, the MCP
//! servers, the subagents and the refusals, over the in-memory store (no Postgres) with scripted
//! models. The process as a binary is `tests/binary.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam::AgentDef;
use adam::mcp::McpPolicy;
use adam_a2a::{Caller, TaskBackend as _, TaskEvent};
use adam_agent::agents as build;
use adam_agent::{AgentError, VERSION, WorkerParts, card_of, exit_code, folder};
use adam_core::{DynStore, MemoryStore};
use adam_mcp_testkit::TestHttpServer;
use adam_model::{DynModel, MockModel, ToolCall};
use adam_runtime::Runtime;
use adam_service::{Agents, RuntimeOptions, ServeError, Service};
use common::{
    PersonaModel, SEARCH_RESULT, SearchServer, assistant, chat, edit_instructions, folder_with,
    researcher,
};
use serde_json::json;
use url::Url;

const MCP_TOKEN: &str = "mcp-tok-7d1c4e90-secret";

fn url() -> Url {
    "https://agents.example.com/chat/".parse().unwrap()
}

fn def_of(folder: &tempfile::TempDir) -> AgentDef {
    folder::load(folder.path()).expect("the folder loads").def
}

fn options() -> RuntimeOptions {
    RuntimeOptions {
        poll_interval: Duration::from_millis(10),
        ..RuntimeOptions::default()
    }
}

/// What a worker needs: the model, the alias, the policy of the MCP servers.
fn worker(model: DynModel, mcp: McpPolicy) -> WorkerParts<'static> {
    WorkerParts {
        model,
        alias: "test-model",
        mcp,
        options: options(),
    }
}

/// A service over `store` for the agents a process built, as `adam_service::serve` composes it
/// (without Postgres).
fn service_over(agents: Agents, store: &DynStore) -> Service {
    let Agents {
        name,
        register,
        options,
        inbound,
        ..
    } = agents;
    Service::new(register(Runtime::builder(store.clone())), name, &options).with_inbound(inbound)
}

fn alice() -> Caller {
    Caller::new("token-0")
}

fn user(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

fn said(task: &Task) -> Option<&str> {
    task.status.message.as_ref().and_then(Message::text)
}

/// Run the workers of `service` until the returned function is called.
struct Worker {
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Worker {
    fn start(service: &Service) -> Self {
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let runtime = service.runtime.clone();
        let handle = tokio::spawn(async move {
            let _ = runtime
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        Self { stop, handle }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("the worker stops")
            .unwrap();
    }
}

async fn wait_for(service: &Service, id: &str, state: TaskState) -> Task {
    for _ in 0..1000 {
        let task = service
            .backend
            .get(&alice(), id)
            .await
            .expect("get")
            .expect("the task exists");
        if task.status.state == state {
            return task;
        }
        assert!(
            !task.status.state.is_terminal(),
            "the task ended {:?} while waiting for {state:?}: {task:?}",
            task.status.state
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

/// Submit `text` to a service that has a worker, and wait for it to complete.
async fn ask(service: &Service, text: &str) -> Task {
    let task = service
        .backend
        .submit(alice(), user(text), None, None)
        .await
        .expect("submit");
    wait_for(service, &task.id, TaskState::Completed).await
}

fn store() -> DynStore {
    Arc::new(MemoryStore::new())
}

// ---------------------------------------------------------------------------- the card

/// The card is the folder's: its name and skills from `card:`, its description, the URL the
/// deployment gives and the version of this binary.
#[tokio::test]
async fn the_card_is_the_one_the_folder_declares() {
    let card = card_of(&def_of(&assistant()), &url()).unwrap();
    assert_eq!(card.name, "Assistant");
    assert_eq!(card.url, url());
    assert_eq!(card.version, VERSION);
    assert_eq!(card.version, env!("CARGO_PKG_VERSION"));
    assert!(
        card.description.contains("general-purpose assistant"),
        "{card:?}"
    );
    assert_eq!(card.skills.len(), 1);
    assert_eq!(card.skills[0].id, "conversation");
    assert_eq!(card.skills[0].tags, ["chat"]);

    // A folder that renames itself changes the card, with no code involved.
    let card = card_of(&def_of(&chat()), &url()).unwrap();
    assert_eq!(card.name, "Chat");
}

/// A card needs a description; a folder without one is refused, as the deployment's mistake.
#[tokio::test]
async fn a_folder_without_a_description_has_no_card() {
    let tmp = folder_with("---\nname: mute\n---\nHello.\n");
    let error = card_of(&def_of(&tmp), &url()).unwrap_err();
    assert!(matches!(error, AgentError::Card(_)), "{error}");
    assert_eq!(error.to_string(), "building the agent card");
    assert_eq!(exit_code(&error), 78);
}

// ------------------------------------------------------------------- answering in role

/// "hi" to a chat folder is answered in role, over A2A: the task completes with the greeting that
/// the folder's two persona lines give (the model is scripted by the prompt), the model is sent the
/// folder's rendered prompt, and `ask_user` is the only tool it is offered.
#[tokio::test]
async fn a_chat_folder_answers_in_role_through_a2a() {
    let store = store();
    let model = PersonaModel::new();
    let dynamic: DynModel = model.clone();
    let agents = build(
        def_of(&chat()),
        None,
        Some(worker(dynamic, McpPolicy::default())),
    )
    .await
    .expect("the folder assembles");
    assert_eq!(agents.name, "chat");
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);

    let done = ask(&service, "hi").await;
    assert_eq!(
        said(&done),
        Some("Hi! I'm Chat. I talk things through with you.")
    );

    let requests = model.requests();
    assert_eq!(requests.len(), 1, "one turn, no tool call");
    let system = requests[0].system.as_deref().unwrap();
    assert!(
        system
            .starts_with("Your name is Chat.\nIn one sentence: I talk things through with you.\n"),
        "{system}"
    );
    assert!(
        system.contains("You are Chat, a general-purpose assistant"),
        "{system}"
    );
    assert!(
        !system.contains("{{"),
        "every placeholder was rendered: {system}"
    );
    assert_eq!(requests[0].model, "test-model");
    assert_eq!(
        requests[0].max_output_tokens,
        Some(2048),
        "the limits are the folder's"
    );
    let tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tools, ["ask_user", "show", "ui_catalog"]);
    worker.stop().await;
}

/// A subscription taken after the run finished starts with the snapshot of the task, already final,
/// and ends with it: that is what a streaming client sees when the worker wins the race against the
/// subscription (a worker in the same process answers a scripted model in milliseconds, and on a loaded
/// machine it can finish first). A client must read the state of the snapshot, not only of the updates
/// after it, which is what the helper of `tests/binary.rs` does.
#[tokio::test]
async fn a_task_that_finished_before_it_was_subscribed_to_is_one_final_snapshot() {
    use futures::StreamExt as _;
    let store = store();
    let model = PersonaModel::new();
    let dynamic: DynModel = model.clone();
    let agents = build(def_of(&chat()), None, Some(worker_parts(dynamic)))
        .await
        .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);
    let done = ask(&service, "hi").await;

    let mut events = service.backend.subscribe(&alice(), &done.id);
    let first = events
        .next()
        .await
        .expect("an event")
        .expect("not an error");
    let TaskEvent::Snapshot(task) = first else {
        panic!("the stream starts with the snapshot: {first:?}");
    };
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(
        said(&task),
        Some("Hi! I'm Chat. I talk things through with you.")
    );
    assert!(
        events.next().await.is_none(),
        "a final snapshot is the whole stream"
    );
    worker.stop().await;
}

/// The point of the binary: edit the folder and the answer changes, with no build. The name and
/// the summary are two lines of a file.
#[tokio::test]
async fn editing_the_folder_changes_what_the_agent_says() {
    let folder = assistant();
    edit_instructions(&folder, |text| {
        text.replacen("display_name: Assistant", "display_name: Cody", 1)
            .replacen("  name: Assistant", "  name: Cody", 1)
            .replacen(
                "In one sentence: I answer your questions in plain words and ask when I need to know more.",
                "In one sentence: I only fix typos.",
                1,
            )
    });
    let store = store();
    let model = PersonaModel::new();
    let dynamic: DynModel = model.clone();
    let agents = build(
        def_of(&folder),
        None,
        Some(worker(dynamic, McpPolicy::default())),
    )
    .await
    .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);
    let done = ask(&service, "hello there").await;
    assert_eq!(said(&done), Some("Hi! I'm Cody. I only fix typos."));
    worker.stop().await;

    // The shipped example, untouched, says what it said.
    let store = self::store();
    let model = PersonaModel::new();
    let dynamic: DynModel = model.clone();
    let agents = build(def_of(&assistant()), None, Some(worker_parts(dynamic)))
        .await
        .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);
    let done = ask(&service, "hi").await;
    assert_eq!(
        said(&done),
        Some(
            "Hi! I'm Assistant. I answer your questions in plain words and ask when I need to know more."
        )
    );
    worker.stop().await;
}

fn worker_parts(model: DynModel) -> WorkerParts<'static> {
    worker(model, McpPolicy::default())
}

/// `ask_user` is the built-in tool: the run parks as `input-required` with the question, the
/// person's answer resumes it, and the model goes on with it.
#[tokio::test]
async fn ask_user_parks_the_run_and_the_answer_resumes_it() {
    let store = store();
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "q1".into(),
        name: "ask_user".into(),
        arguments: json!({"question": "Which city?"}),
    }])
    .push_text("Paris it is.");
    let model: DynModel = mock.clone();
    let agents = build(def_of(&chat()), None, Some(worker_parts(model)))
        .await
        .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);

    let task = service
        .backend
        .submit(alice(), user("What is the weather?"), None, None)
        .await
        .unwrap();
    let waiting = wait_for(&service, &task.id, TaskState::InputRequired).await;
    assert_eq!(said(&waiting), Some("Which city?"));

    service
        .backend
        .submit(alice(), user("Paris"), Some(task.id.clone()), None)
        .await
        .expect("the answer is delivered to the task");
    let done = wait_for(&service, &task.id, TaskState::Completed).await;
    assert_eq!(said(&done), Some("Paris it is."));
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    match requests[1].messages.last().unwrap() {
        adam_model::Message::Tool {
            call_id, content, ..
        } => {
            assert_eq!(call_id, "q1");
            assert_eq!(content, "Paris");
        }
        other => panic!("{other:?}"),
    }
    worker.stop().await;
}

// ------------------------------------------------------------------------------ roles

/// A control plane registers the start-only half: it takes a task, nobody steps it there, and a
/// worker over the same store (the whole agent) completes it. Neither needed more than its name
/// and the files.
#[tokio::test]
async fn a_control_plane_starts_runs_and_a_worker_over_the_store_completes_them() {
    let store = store();
    let card = card_of(&def_of(&chat()), &url()).unwrap();
    let front_agents = build(def_of(&chat()), Some(card), None)
        .await
        .expect("a control plane needs no model");
    assert_eq!(front_agents.name, "chat");
    assert!(front_agents.card.is_some(), "it serves the card");
    let front = service_over(front_agents, &store);

    let task = front
        .backend
        .submit(alice(), user("hi"), None, None)
        .await
        .expect("the control plane starts the run");
    // Its own worker claims nothing: the agent is registered as a starter only.
    let idle = Worker::start(&front);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let waiting = front
        .backend
        .get(&alice(), &task.id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(waiting.status.state, TaskState::Completed, "{waiting:?}");
    idle.stop().await;

    let model = PersonaModel::new();
    let dynamic: DynModel = model.clone();
    let back = service_over(
        build(def_of(&chat()), None, Some(worker_parts(dynamic)))
            .await
            .unwrap(),
        &store,
    );
    let worker = Worker::start(&back);
    let done = wait_for(&front, &task.id, TaskState::Completed).await;
    assert_eq!(
        said(&done),
        Some("Hi! I'm Chat. I talk things through with you.")
    );
    worker.stop().await;
}

// ------------------------------------------------------------------------- mcp.json

/// `agent/mcp.json` of `folder`: one server `test` at `url`, with the token from
/// `${TEST_MCP_TOKEN}`, offering only `echo`.
fn write_mcp_json(folder: &tempfile::TempDir, url: &str) {
    std::fs::write(
        folder.path().join("agent/mcp.json"),
        format!(
            r#"{{"mcpServers": {{"test": {{"type": "http", "url": "{url}",
                "headers": {{"Authorization": "Bearer ${{TEST_MCP_TOKEN}}"}},
                "tools": ["echo"]}}}}}}"#
        ),
    )
    .unwrap();
}

/// The folder's definition with the values its `mcp.json` reads from the environment, given in
/// code (`AgentDef::env`), so no test touches the process environment.
fn def_with_env(folder: &tempfile::TempDir, url: Option<&str>) -> AgentDef {
    let def = def_of(folder).env("TEST_MCP_TOKEN", MCP_TOKEN);
    match url {
        Some(url) => def.env("TEST_MCP_URL", url),
        None => def,
    }
}

/// A folder with an `mcp.json` gives the agent the tools of its servers, named `<server>__<tool>`
/// after `ask_user`; a call by the model reaches the server (with the token from the
/// environment) and its text comes back as the tool's result.
#[tokio::test]
async fn the_servers_of_mcp_json_give_the_agent_their_tools() {
    let server = TestHttpServer::start(Some(MCP_TOKEN)).await;
    let folder = chat();
    write_mcp_json(&folder, &server.url());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "m1".into(),
        name: "test__echo".into(),
        arguments: json!({"text": "ping"}),
    }])
    .push_text("The server said ping.");
    let model: DynModel = mock.clone();
    let store = store();
    let agents = build(def_with_env(&folder, None), None, Some(worker_parts(model)))
        .await
        .expect("the folder assembles with its MCP tools");
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);

    let done = ask(&service, "echo ping").await;
    assert_eq!(said(&done), Some("The server said ping."));
    assert_eq!(server.calls(), 1, "the call reached the server");
    assert!(
        server
            .authorizations()
            .iter()
            .all(|a| a == &format!("Bearer {MCP_TOKEN}")),
        "{:?}",
        server.authorizations()
    );
    let requests = mock.requests();
    let offered: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(offered, ["ask_user", "show", "ui_catalog", "test__echo"]);
    match requests[1].messages.last().unwrap() {
        adam_model::Message::Tool {
            call_id,
            content,
            is_error,
        } => {
            assert_eq!(
                (call_id.as_str(), content.as_str(), *is_error),
                ("m1", "ping", false)
            );
        }
        other => panic!("{other:?}"),
    }
    worker.stop().await;
}

/// `${VAR}` in a `url` is refused unless the deployment opts in (`MCP_ALLOW_URL_VARS`), and the
/// refusal names the variable, never its value. A header may always use one.
#[tokio::test]
async fn a_variable_in_a_url_needs_the_deployments_say_so() {
    let server = TestHttpServer::start(Some(MCP_TOKEN)).await;
    let folder = chat();
    write_mcp_json(&folder, "${TEST_MCP_URL}");
    let mock: DynModel = Arc::new(MockModel::new());

    let error = build(
        def_with_env(&folder, Some(&server.url())),
        None,
        Some(worker_parts(mock.clone())),
    )
    .await
    .expect_err("a variable in a URL is refused by default");
    assert!(matches!(error, AgentError::Mcp(_)), "{error}");
    let text = format!("{error:#?}");
    assert!(text.contains("TEST_MCP_URL"), "{text}");
    assert!(!text.contains(&server.url()), "{text}");
    assert_eq!(exit_code(&error), 78);

    let policy = McpPolicy::default().allow_url_secrets(true);
    build(
        def_with_env(&folder, Some(&server.url())),
        None,
        Some(worker(mock, policy)),
    )
    .await
    .expect("once the deployment opts in, the URL is read from the variable");
}

/// What the policy refuses is refused before anything starts (78); a server that is down may be up
/// later (69, so a supervisor retries); a variable nobody set is the deployment's mistake (78).
#[tokio::test]
async fn the_policy_and_the_servers_decide_the_exit_code() {
    let model: DynModel = Arc::new(MockModel::new());
    let code = |folder: &tempfile::TempDir, env: bool| {
        let model = model.clone();
        let def = if env {
            def_with_env(folder, None)
        } else {
            def_of(folder)
        };
        async move {
            let error = build(def, None, Some(worker_parts(model)))
                .await
                .expect_err("the folder is refused");
            assert!(matches!(error, AgentError::Mcp(_)), "{error}");
            exit_code(&error)
        }
    };

    let local = chat();
    std::fs::write(
        local.path().join("agent/mcp.json"),
        r#"{"mcpServers": {"local": {"command": "adam-mcp-test-server"}}}"#,
    )
    .unwrap();
    assert_eq!(
        code(&local, false).await,
        78,
        "a local process is not allowed by default"
    );

    let down = chat();
    write_mcp_json(&down, "http://127.0.0.1:1/mcp");
    assert_eq!(code(&down, true).await, 69, "nothing listens on port 1");

    let unset = chat();
    write_mcp_json(&unset, "http://127.0.0.1:1/mcp");
    assert_eq!(
        code(&unset, false).await,
        78,
        "TEST_MCP_TOKEN is set nowhere"
    );
}

/// A researcher on a **stateless** web-search MCP server (JSON responses, no session id, `405` for
/// `GET` and `DELETE`: what a small mock looks like): the folder's `mcp.json` is enough for the agent
/// to have `search__web_search` with the server's own schema, a call carries the token from the
/// environment, and the results come back as the tool's text, from which the model names its source.
#[tokio::test]
async fn a_researcher_searches_a_stateless_mcp_server_and_answers_with_its_source() {
    let server = SearchServer::start(Some(MCP_TOKEN)).await;
    let folder = researcher(&server.url());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "r1".into(),
        name: "search__web_search".into(),
        arguments: json!({"query": "rust async"}),
    }])
    .push_text("I searched the web. The best source I found is https://example.org/mock-search/1.");
    let model: DynModel = mock.clone();
    let store = store();
    let agents = build(
        def_of(&folder).env("SEARCH_TOKEN", MCP_TOKEN),
        None,
        Some(worker_parts(model)),
    )
    .await
    .expect("the researcher assembles against a stateless server");
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);

    let done = ask(&service, "What is async Rust?").await;
    assert_eq!(
        said(&done),
        Some("I searched the web. The best source I found is https://example.org/mock-search/1.")
    );
    assert_eq!(server.initializations(), 1);
    assert_eq!(server.calls(), [json!({"query": "rust async"})]);
    assert!(
        server
            .authorizations()
            .iter()
            .all(|a| a == &format!("Bearer {MCP_TOKEN}")),
        "{:?}",
        server.authorizations()
    );

    let requests = mock.requests();
    let tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tools,
        ["ask_user", "show", "ui_catalog", "search__web_search"]
    );
    let spec = &requests[0].tools[3];
    assert!(
        spec.description.contains("Search the web"),
        "{}",
        spec.description
    );
    assert_eq!(spec.parameters["required"], json!(["query"]));
    match requests[1].messages.last().unwrap() {
        adam_model::Message::Tool {
            call_id,
            content,
            is_error,
        } => {
            assert_eq!(call_id, "r1");
            assert_eq!(content, SEARCH_RESULT);
            assert!(!is_error);
        }
        other => panic!("{other:?}"),
    }
    worker.stop().await;
}

/// What the server says goes to the model as it is: an empty search is text, a failing backend is an
/// error result the model reads (the run does not fail), and the run goes on to the model's answer.
#[tokio::test]
async fn an_empty_search_and_a_failing_one_are_results_the_model_reads() {
    let server = SearchServer::start(None).await;
    let folder = researcher(&server.url());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "r1".into(),
        name: "search__web_search".into(),
        arguments: json!({"query": "[mock:empty]"}),
    }])
    .push_tool_calls(vec![ToolCall {
        id: "r2".into(),
        name: "search__web_search".into(),
        arguments: json!({"query": "[mock:error]"}),
    }])
    .push_text("I found nothing, and the search failed once.");
    let model: DynModel = mock.clone();
    let store = store();
    // The header names `${SEARCH_TOKEN}`, which the server does not ask for but the file needs.
    let agents = build(
        def_of(&folder).env("SEARCH_TOKEN", "unused"),
        None,
        Some(worker_parts(model)),
    )
    .await
    .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);

    let done = ask(&service, "something obscure").await;
    assert_eq!(
        said(&done),
        Some("I found nothing, and the search failed once.")
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let result = |i: usize| match requests[i].messages.last().unwrap() {
        adam_model::Message::Tool {
            content, is_error, ..
        } => (content.clone(), *is_error),
        other => panic!("{other:?}"),
    };
    assert_eq!(result(1), ("No results.".to_owned(), false));
    let (text, is_error) = result(2);
    assert!(is_error, "{text}");
    assert!(text.contains("The search backend is down."), "{text}");
    assert_eq!(server.calls().len(), 2);
    worker.stop().await;
}

// --------------------------------------------------------------------------- subagents

/// The subagents of a folder are registered beside the agent: the model calls the subagent's tool,
/// a child run answers with its own prompt, and the agent goes on with the child's text.
#[tokio::test]
async fn a_local_subagent_of_the_folder_runs_as_a_child_run() {
    let folder = chat();
    std::fs::create_dir_all(folder.path().join("agent/subagents")).unwrap();
    std::fs::write(
        folder.path().join("agent/subagents/reviewer.md"),
        "---\ndescription: Reviews a text.\n---\nYou review texts.\n",
    )
    .unwrap();
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "c1".into(),
        name: "reviewer".into(),
        arguments: json!({"message": "review this"}),
    }])
    .push_text("LGTM, nothing to fix.")
    .push_text("The reviewer is happy.");
    let model: DynModel = mock.clone();
    let store = store();
    let agents = build(def_of(&folder), None, Some(worker_parts(model)))
        .await
        .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);

    let done = ask(&service, "have it reviewed").await;
    assert_eq!(said(&done), Some("The reviewer is happy."));
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let offered =
        |i: usize| -> Vec<&str> { requests[i].tools.iter().map(|t| t.name.as_str()).collect() };
    assert_eq!(offered(0), ["ask_user", "show", "ui_catalog", "reviewer"]);
    // The child: its own prompt, and no tool (a subagent gets none it does not list).
    assert_eq!(requests[1].system.as_deref(), Some("You review texts."));
    assert!(offered(1).is_empty(), "{:?}", offered(1));
    match requests[2].messages.last().unwrap() {
        adam_model::Message::Tool {
            call_id, content, ..
        } => {
            assert_eq!(
                (call_id.as_str(), content.as_str()),
                ("c1", "LGTM, nothing to fix.")
            );
        }
        other => panic!("{other:?}"),
    }
    worker.stop().await;
}

// ---------------------------------------------------------------------------- refusals

/// The files and the code disagreeing is found at startup, naming the problem, and is the
/// deployment's mistake (78): an unknown tool (with a suggestion), a var nothing supplies (no value
/// in the file, and nothing in this binary gives one), a model alias the assembly refuses.
#[tokio::test]
async fn files_that_disagree_with_the_code_are_refused_at_assembly() {
    let model: DynModel = Arc::new(MockModel::new());
    let refuse = |folder: tempfile::TempDir, alias: &'static str| {
        let model = model.clone();
        async move {
            let mut parts = worker_parts(model);
            parts.alias = alias;
            build(def_of(&folder), None, Some(parts))
                .await
                .expect_err("the folder is refused")
        }
    };

    let typo = assistant();
    edit_instructions(&typo, |t| {
        t.replacen("limits:", "tools: [ask_usr]\nlimits:", 1)
    });
    let error = refuse(typo, "test-model").await;
    assert!(matches!(error, AgentError::Assembly(_)), "{error}");
    assert_eq!(error.to_string(), "assembling the agent");
    let cause = std::error::Error::source(&error).unwrap().to_string();
    assert!(
        cause.contains("ask_usr") && cause.contains("did you mean `ask_user`"),
        "{cause}"
    );
    assert_eq!(exit_code(&error), 78);

    let unset = assistant();
    edit_instructions(&unset, |t| {
        t.replacen("vars:\n", "vars:\n  city:\n", 1).replacen(
            "A greeting gets",
            "In {{city}}. A greeting gets",
            1,
        )
    });
    let error = refuse(unset, "test-model").await;
    let cause = std::error::Error::source(&error).unwrap().to_string();
    assert!(cause.contains("city"), "{cause}");
    assert_eq!(exit_code(&error), 78);

    let error = refuse(assistant(), "two words").await;
    assert!(matches!(error, AgentError::Assembly(_)), "{error}");
    assert_eq!(exit_code(&error), 78);
}

/// The class of what a process fails with decides its exit code; the service's own errors keep
/// theirs.
#[tokio::test]
async fn each_failure_maps_to_the_exit_code_of_its_cause() {
    // A folder that cannot be read: the deployment's mistake.
    let missing = tempfile::tempdir().unwrap().path().join("nowhere");
    let error = folder::load(&missing).unwrap_err();
    assert!(matches!(error, AgentError::Folder { .. }), "{error}");
    assert_eq!(exit_code(&error), 78);

    // A model gateway that is not a URL.
    let model = adam_service::ModelConfig {
        base_url: "not a url".into(),
        api_key: String::new().into(),
        alias: "m".into(),
    };
    let error = AgentError::Model(model.client().err().expect("not a URL"));
    assert_eq!(exit_code(&error), 78);
    assert_eq!(error.to_string(), "building the model client");

    // The service: Postgres gone is 69, a taken port 71, a component that stopped 70.
    let down = ServeError::Connect(adam_core::StoreError::unavailable(std::io::Error::other(
        "refused",
    )));
    assert_eq!(exit_code(&AgentError::Serve(down)), 69);
    let bind = ServeError::Bind {
        addr: "127.0.0.1:1".parse().unwrap(),
        source: std::io::Error::from(std::io::ErrorKind::AddrInUse),
    };
    assert_eq!(exit_code(&AgentError::Serve(bind)), 71);
    let stopped = ServeError::Host(adam_host::HostError::EndedEarly {
        component: "worker".into(),
    });
    assert_eq!(exit_code(&AgentError::Serve(stopped)), 70);
    assert_eq!(exit_code(&AgentError::Serve(ServeError::NoCard)), 78);
}

// ----------------------------------------------------------------------------- folders

/// The shipped example reads cleanly (no warning), and the same files have the same digest
/// wherever they were read from.
#[tokio::test]
async fn the_shipped_example_loads_without_a_warning() {
    let loaded = folder::load(&common::example_dir()).expect("the example loads");
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.def.name(), "assistant");
    assert!(loaded.digest.as_str().starts_with("sha256:"));
    folder::log(&loaded);
    // `agent/` itself names the same folder.
    let again = folder::load(&common::example_dir().join("agent")).unwrap();
    assert_eq!(again.digest.as_str(), loaded.digest.as_str());
}

/// A broken folder is refused with every finding, once, as `path:line: error: ...`, and as the
/// deployment's mistake.
#[tokio::test]
async fn every_diagnostic_of_a_broken_folder_is_in_the_message() {
    let tmp = folder_with("---\nname: chat\ndescription: Chat.\n---\nHi.\n");
    std::fs::create_dir_all(tmp.path().join("agent/subagents")).unwrap();
    for name in ["broken", "worse"] {
        std::fs::write(
            tmp.path().join(format!("agent/subagents/{name}.md")),
            "---\n: : [\n---\nSub.\n",
        )
        .unwrap();
    }
    let error = folder::load(tmp.path()).unwrap_err();
    let text = error.to_string();
    for file in ["broken", "worse"] {
        assert!(
            text.contains(&format!("agent/subagents/{file}.md:")),
            "{file} missing from {text}"
        );
    }
    assert!(text.contains(": error: "), "{text}");
    assert!(text.contains("ADAM_AGENT_DIR"), "{text}");
    assert_eq!(error.diagnostics().len(), 2, "{:?}", error.diagnostics());
    assert_eq!(text.matches("broken.md").count(), 1, "said once: {text}");
    assert_eq!(exit_code(&error), 78);
}

/// One agent per process: a folder of several agents (`agents/`) is refused, naming them.
#[tokio::test]
async fn a_folder_of_several_agents_is_refused() {
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
    let error = folder::load(tmp.path()).unwrap_err();
    assert!(matches!(error, AgentError::Folder { .. }), "{error}");
    let text = error.to_string();
    assert!(text.contains("one") && text.contains("two"), "{text}");
    assert_eq!(exit_code(&error), 78);
}

/// What the loader finds that does not stop the process is a warning, kept and logged.
#[tokio::test]
async fn a_warning_is_kept_and_does_not_stop_the_load() {
    let tmp =
        folder_with("---\nname: chat\ndescription: Chat.\nfavourite_colour: green\n---\nHi.\n");
    let loaded = folder::load(tmp.path()).unwrap();
    assert_eq!(loaded.warnings.len(), 1);
    assert!(
        loaded.warnings[0].to_string().contains("favourite_colour"),
        "{:?}",
        loaded.warnings
    );
    folder::log(&loaded);
}
