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
use adam_agent::{
    AgentError, WorkerParts, build_version, card_of, card_of_folder, exit_code, folder,
};
use adam_core::{DynStore, MemoryStore};
use adam_mcp_testkit::TestHttpServer;
use adam_model::{DynModel, MockModel, ToolCall};
use adam_runtime::Runtime;
use adam_service::{Agents, RuntimeOptions, ServeError, Service};
use common::{
    PersonaModel, SEARCH_RESULT, SearchServer, assistant, chat, edit_instructions, folder_with,
    researcher, shipped_researcher,
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
        step_io: adam_llm_agent::StepIo::default(),
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
    assert_eq!(card.version, build_version());
    assert!(
        card.version
            .starts_with(&format!("{}+", env!("CARGO_PKG_VERSION"))),
        "the revision is semver build metadata: {}",
        card.version
    );
    assert!(
        card.description.contains("general-purpose assistant"),
        "{card:?}"
    );
    assert_eq!(card.skills.len(), 1);
    assert_eq!(card.skills[0].id, "conversation");
    assert_eq!(card.skills[0].tags, ["chat"]);
    // What it speaks, besides A2A: the screen's extensions, steps for every tool call, the model's
    // answer as it is written and the tokens of each model call.
    let uris: Vec<&str> = card.extensions.iter().map(|e| e.uri.as_str()).collect();
    assert_eq!(
        uris,
        [
            adam_a2a::A2UI_EXTENSION_V0_9_1,
            adam_a2a::UI_CATALOG_EXTENSION,
            adam_a2a::THREAD_TOOLS_EXTENSION,
            adam_a2a::MENTIONS_EXTENSION,
            adam_a2a::STEER_EXTENSION,
            adam_a2a::STEPS_EXTENSION,
            adam_a2a::TEXT_STREAM_EXTENSION,
            adam_a2a::USAGE_EXTENSION,
        ]
    );
    let usage = card
        .extensions
        .iter()
        .find(|e| e.uri == adam_a2a::USAGE_EXTENSION)
        .unwrap();
    assert!(!usage.required);
    assert!(usage.params.is_empty());

    // A folder that renames itself changes the card, with no code involved.
    let card = card_of(&def_of(&chat()), &url()).unwrap();
    assert_eq!(card.name, "Chat");
}

/// The card of a folder says which build answers and which files it runs, in `build/v1`: the digest
/// is the folder's own (the one the startup line logs), and the extension is optional.
#[tokio::test]
async fn the_card_of_a_folder_says_its_build_and_its_digest() {
    let tmp = assistant();
    let loaded = folder::load(tmp.path()).unwrap();
    let card = card_of_folder(&loaded, &url()).unwrap();
    assert_eq!(card.version, build_version());
    let build = card
        .extensions
        .iter()
        .find(|e| e.uri == adam_a2a::BUILD_EXTENSION)
        .expect("build/v1 is on the card");
    assert!(!build.required);
    assert_eq!(build.params["folderDigest"], loaded.digest.as_str());
    assert_eq!(
        build.params["revision"],
        adam_a2a::revision_of(adam_agent::BUILD_REVISION)
    );
    assert!(
        loaded.digest.as_str().starts_with("sha256:"),
        "{}",
        loaded.digest
    );
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

/// A server whose `mcp.json` entry says `files: true` (a headless browser's screenshot tool, say):
/// the image its tool answers with reaches the A2A client as a standard file artifact, one `raw`
/// part with its media type and filename, the shape `share_file` gives (ADR 0012, ADR 0033), and
/// the model reads one line about it.
#[tokio::test]
async fn a_screenshot_of_a_files_true_server_reaches_the_a2a_client_as_a_file() {
    let server = TestHttpServer::start(Some(MCP_TOKEN)).await;
    let folder = chat();
    std::fs::write(
        folder.path().join("agent/mcp.json"),
        format!(
            r#"{{"mcpServers": {{"browser": {{"type": "http", "url": "{}",
                "headers": {{"Authorization": "Bearer ${{TEST_MCP_TOKEN}}"}},
                "tools": ["screenshot"], "files": true}}}}}}"#,
            server.url()
        ),
    )
    .unwrap();
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "m1".into(),
        name: "browser__screenshot".into(),
        arguments: json!({}),
    }])
    .push_text("Here is the page.");
    let model: DynModel = mock.clone();
    let store = store();
    let agents = build(def_with_env(&folder, None), None, Some(worker_parts(model)))
        .await
        .expect("the folder assembles with its MCP tools");
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);

    let done = ask(&service, "show me the page").await;
    worker.stop().await;
    assert_eq!(said(&done), Some("Here is the page."));
    let artifacts = done.artifacts.as_deref().unwrap_or_default();
    let shot: Vec<&a2a::Artifact> = artifacts
        .iter()
        .filter(|a| a.name.as_deref() == Some("screenshot-1.png"))
        .collect();
    assert_eq!(shot.len(), 1, "{artifacts:?}");
    assert_eq!(shot[0].parts.len(), 1);
    let part = &shot[0].parts[0];
    assert_eq!(
        part.content,
        a2a::PartContent::Raw(adam_mcp_testkit::PNG.to_vec())
    );
    assert_eq!(part.media_type.as_deref(), Some("image/png"));
    assert_eq!(part.filename.as_deref(), Some("screenshot-1.png"));
    match mock.requests()[1].messages.last().unwrap() {
        adam_model::Message::Tool {
            content, is_error, ..
        } => assert_eq!(
            (content.as_str(), *is_error),
            (
                "Shared screenshot-1.png (67 bytes, image/png). To show it in your answer, \
                 write ![description](screenshot-1.png).",
                false
            )
        ),
        other => panic!("{other:?}"),
    }
}

/// What an MCP tool was given and answered is in its step (ADR 0011), under the title its server gave
/// it, and the process's secrets are scrubbed from both: the model's key (from the configuration), a
/// variable named like a secret (from the environment) and the token of the MCP server itself.
#[tokio::test]
async fn an_mcp_tools_step_carries_its_title_input_and_output_without_the_processs_secrets() {
    use futures::StreamExt as _;
    const FROM_THE_ENVIRONMENT: &str = "env-secret-4f9a1c7d";
    // A value whose variable's name says nothing (`SEARCH_ACCESS`): only the `mcp.json` that reads it
    // makes it a secret (`AgentDef::mcp_env_references`).
    const NAMED_ONLY: &str = "named-only-5d2b8e61";
    const MODEL_KEY: &str = "sk-model-key-8b2e";

    let server = TestHttpServer::start(Some(MCP_TOKEN)).await;
    let folder = chat();
    write_mcp_json(&folder, &server.url());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "m1".into(),
        name: "test__echo".into(),
        arguments: json!({"text": format!("found {FROM_THE_ENVIRONMENT} and {MODEL_KEY} and {NAMED_ONLY}")}),
    }])
    .push_text("ok");
    let model: DynModel = mock.clone();

    // The configuration of the process: the model's key is in it, and the variable is in its environment.
    let lookup = |name: &str| {
        Some(
            match name {
                "DATABASE_URL" => "postgres://u:db-pass-77@db/adam",
                "MODEL_BASE_URL" => "https://gw.example/v1",
                "MODEL_API_KEY" => MODEL_KEY,
                "MODEL" => "large",
                "A2A_BEARER_TOKENS" => "bearer-one",
                "PUBLIC_URL" => "http://agent.svc:8080/",
                other if other == adam::AGENT_DIR_ENV => {
                    return Some(folder.path().display().to_string());
                }
                _ => return None,
            }
            .to_owned(),
        )
    };
    let config = adam_agent::Config::from_lookup(lookup).expect("a valid configuration");
    let mut parts = worker_parts(model);
    parts.step_io = adam_agent::redact::step_io_named(
        &config,
        [
            ("SEARCH_API_KEY".to_owned(), FROM_THE_ENVIRONMENT.to_owned()),
            ("SEARCH_ACCESS".to_owned(), NAMED_ONLY.to_owned()),
        ],
        &["SEARCH_ACCESS".to_owned()].into(),
    );
    let agents = build(def_with_env(&folder, None), None, Some(parts))
        .await
        .expect("the folder assembles with its MCP tools");
    let service = service_over(agents, &store());

    // A client that activated `steps/v1`, subscribed before the worker steps the run.
    let caller = Caller::new("token-0").with_extensions([adam_a2a::STEPS_EXTENSION]);
    let task = service
        .backend
        .submit(caller.clone(), user("echo it"), None, None)
        .await
        .expect("submit");
    let mut stream = service.backend.subscribe(&caller, &task.id);
    assert!(matches!(
        stream.next().await.unwrap().unwrap(),
        TaskEvent::Snapshot(_)
    ));
    let worker = Worker::start(&service);
    let mut reports: Vec<serde_json::Value> = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(20), stream.next())
        .await
        .expect("the stream goes on")
    {
        if let TaskEvent::Status(update) = event.unwrap()
            && let Some(message) = update.status.message
            && let Some(report) = message
                .metadata
                .as_ref()
                .and_then(|m| m.get(adam_a2a::STEPS_EXTENSION))
        {
            reports.push(report.clone());
        }
    }
    worker.stop().await;

    let report = |end: bool| {
        reports
            .iter()
            .find(|r| (r["state"] == "completed") == end && r["id"] == "tool:m1")
            .unwrap_or_else(|| panic!("no report (end: {end}) in {reports:#?}"))
    };
    // Under the title the server gave the tool, not `test__echo`.
    assert_eq!(report(false)["label"], "Echo it back");
    assert_eq!(
        report(false)["input"],
        json!({"text": "found [redacted] and [redacted] and [redacted]"})
    );
    assert_eq!(
        report(true)["output"],
        json!({"text": "found [redacted] and [redacted] and [redacted]"})
    );
    // What the model was told is what the tool answered: the copy for the observer is the one scrubbed.
    let everything = serde_json::to_string(&reports).unwrap();
    for secret in [FROM_THE_ENVIRONMENT, MODEL_KEY, NAMED_ONLY, MCP_TOKEN] {
        assert!(!everything.contains(secret), "{secret} in {everything}");
    }
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

// ---------------------------------------------------- the researcher on the person's screen

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
/// Version 3 of the web's catalog (`Cards` and `Mermaid` among its components) and version 2 (without
/// them), as `adam-ui`'s tests pin them: a screen of each age.
const CATALOG_V3: (&str, &str) = (
    include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v3.json"),
    include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v3.lock.json"),
);
const CATALOG_V2: (&str, &str) = (
    include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v2.json"),
    include_str!("../../../crates/adam-ui/tests/fixtures/catalog-v2.lock.json"),
);

/// The message the orchestrator sends from a screen that draws `catalog` (the document and its lock),
/// with the catalog inline: the `ui-catalog/v1` metadata and the A2UI capabilities.
fn from_the_screen(text: &str, (catalog, lock): (&str, &str)) -> Message {
    let lock: serde_json::Value = serde_json::from_str(lock).unwrap();
    let catalog: serde_json::Value = serde_json::from_str(catalog).unwrap();
    let mut message = user(text);
    message.metadata = Some(
        json!({
            adam_a2a::UI_CATALOG_EXTENSION: {
                "catalogId": CATALOG_ID, "version": lock["version"], "digest": lock["digest"],
                "inline": true},
            "a2uiClientCapabilities": {"v0.9.1": {
                "supportedCatalogIds": [CATALOG_ID], "inlineCatalogs": [catalog]}},
        })
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect(),
    );
    message
}

/// One question put to the shipped researcher by `message`. The model is scripted the way a good one
/// follows the folder's instructions: search, read what the screen can draw, show two sources as
/// cards and how they relate as a graph, then answer in words. Returns the finished task and what the
/// model was sent.
async fn researcher_run(message: Message) -> (Task, Vec<adam_model::ModelRequest>) {
    let server = SearchServer::start(Some(MCP_TOKEN)).await;
    let folder = shipped_researcher(&server.url());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "r1".into(),
        name: "search__web_search".into(),
        arguments: json!({"query": "rust async"}),
    }])
    .push_tool_calls(vec![ToolCall {
        id: "r2".into(),
        name: "ui_catalog".into(),
        arguments: json!({}),
    }])
    .push_tool_calls(vec![ToolCall {
        id: "r3".into(),
        name: "show".into(),
        arguments: json!({"blocks": [
            {"component": "Cards", "title": "Sources", "cards": [
                {"title": "The Rust language", "subtitle": "example.org",
                 "body": "A language empowering everyone.",
                 "url": "https://example.org/mock-search/1", "tags": ["rust"]},
                {"title": "Async in Rust", "subtitle": "example.org",
                 "body": "How futures work.",
                 "url": "https://example.org/mock-search/2"}]},
            {"component": "Mermaid", "code": "graph TD\n  Future --> Executor\n  Executor --> Future"}
        ]}),
    }])
    .push_text(
        "Async Rust is built on futures: https://example.org/mock-search/1 and https://example.org/mock-search/2.",
    );
    let model: DynModel = mock.clone();
    let agents = build(
        def_of(&folder).env("SEARCH_MCP_TOKEN", MCP_TOKEN),
        None,
        Some(worker_parts(model)),
    )
    .await
    .expect("the shipped researcher assembles");
    let service = service_over(agents, &store());
    let worker = Worker::start(&service);
    let task = service
        .backend
        .submit(alice(), message, None, Some("ctx".into()))
        .await
        .expect("submit");
    let done = wait_for(&service, &task.id, TaskState::Completed).await;
    worker.stop().await;
    (done, mock.requests())
}

/// What the model read as the result of its last tool call: the content, and whether it was an error.
fn last_tool_result(request: &adam_model::ModelRequest) -> (String, bool) {
    match request.messages.last().unwrap() {
        adam_model::Message::Tool {
            content, is_error, ..
        } => (content.clone(), *is_error),
        other => panic!("{other:?}"),
    }
}

/// The researcher of the repository's folder answers with its sources as cards and how they relate as
/// a graph, on a screen whose catalog (version 3) has `Cards` and `Mermaid`: one A2UI surface as the
/// run's `ui` artifact, every component of it valid for the catalog the screen announced, under the
/// screen's `catalogId`, and the answer in words beside it.
#[tokio::test]
async fn the_researcher_answers_with_cards_and_a_graph_on_a_screen_that_draws_them() {
    let (done, requests) = researcher_run(from_the_screen("What is async Rust?", CATALOG_V3)).await;
    assert_eq!(
        said(&done),
        Some(
            "Async Rust is built on futures: https://example.org/mock-search/1 and https://example.org/mock-search/2."
        )
    );
    assert_eq!(requests.len(), 4);

    // The folder tells the model to show what it found, and to look at the screen first.
    let system = requests[0].system.as_deref().unwrap();
    for needle in [
        "Show what you found",
        "`ui_catalog`",
        "`Cards`",
        "`Mermaid`",
        "`show`",
    ] {
        assert!(
            system.contains(needle),
            "{needle} is not in the prompt: {system}"
        );
    }
    // It read the screen's components: version 3, with the two new ones.
    let (listed, is_error) = last_tool_result(&requests[2]);
    assert!(!is_error, "{listed}");
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["version"], 3);
    let names: Vec<&str> = listed["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Cards", "Choices", "Column", "Mermaid", "Text"]);
    assert_eq!(
        last_tool_result(&requests[3]),
        ("Shown to the person.".to_owned(), false)
    );

    // The surface: one `ui` artifact of A2UI messages.
    let artifacts = done.artifacts.as_deref().unwrap_or_default();
    assert_eq!(artifacts.len(), 1, "{artifacts:?}");
    let artifact = &artifacts[0];
    assert_eq!(artifact.name.as_deref(), Some("ui"));
    let part = &artifact.parts[0];
    assert_eq!(part.media_type.as_deref(), Some(adam_a2a::A2UI_MEDIA_TYPE));
    let a2a::PartContent::Data(messages) = &part.content else {
        panic!("the artifact is data: {part:?}");
    };
    assert_eq!(messages[0]["createSurface"]["catalogId"], CATALOG_ID);
    let components = messages[1]["updateComponents"]["components"]
        .as_array()
        .unwrap();
    let kinds: Vec<&str> = components
        .iter()
        .map(|c| c["component"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["Column", "Cards", "Mermaid"]);
    let lock: serde_json::Value = serde_json::from_str(CATALOG_V3.1).unwrap();
    let catalog = adam_ui::Catalog::from_document(
        serde_json::from_str(CATALOG_V3.0).unwrap(),
        &adam_ui::Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: 3,
            digest: lock["digest"].as_str().unwrap().to_owned(),
        },
    )
    .unwrap();
    for component in components {
        catalog
            .validate(component)
            .unwrap_or_else(|problem| panic!("{component}: {problem}"));
    }
    let urls: Vec<&str> = components[1]["cards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        urls,
        [
            "https://example.org/mock-search/1",
            "https://example.org/mock-search/2"
        ]
    );
}

/// A screen that cannot draw cards (a catalog of version 2, or none) gets the answer in words: `show`
/// says so to the model, which goes on, and the run ends completed with no surface.
#[tokio::test]
async fn on_a_screen_without_cards_or_a_catalog_the_researcher_answers_in_words_only() {
    for (what, message, refused) in [
        (
            "version 2",
            from_the_screen("What is async Rust?", CATALOG_V2),
            "`Cards` is not a component of this screen; the components are: Choices, Column, Text",
        ),
        (
            "no catalog",
            user("What is async Rust?"),
            "this screen has no component catalog; answer in text",
        ),
    ] {
        let (done, requests) = researcher_run(message).await;
        assert!(
            said(&done).is_some_and(|t| t.contains("https://example.org/mock-search/1")),
            "{what}: {:?}",
            said(&done)
        );
        assert!(
            done.artifacts.as_deref().unwrap_or_default().is_empty(),
            "{what}: {:?}",
            done.artifacts
        );
        // The model read why, as an error result, and the run went on to its answer.
        let (text, is_error) = last_tool_result(&requests[3]);
        assert!(is_error && text.contains(refused), "{what}: {text}");
    }
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
        extra_body: None,
        echo_reasoning: None,
        context_window: None,
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

/// The shipped researcher reads cleanly too, and says in its card what it does with a screen.
#[tokio::test]
async fn the_shipped_researcher_loads_without_a_warning() {
    let loaded = folder::load(&common::researcher_dir()).expect("the researcher loads");
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.def.name(), "researcher");
    let card = card_of(&loaded.def, &url()).unwrap();
    assert_eq!(card.name, "Researcher");
    assert_eq!(card.skills[0].id, "web-research");
    assert!(card.skills[0].description.contains("cards"));
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
