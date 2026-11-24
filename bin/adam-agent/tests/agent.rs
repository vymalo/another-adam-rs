//! The agent a folder describes, in this process: the card, the answer in role, the tools, the MCP
//! servers, the subagents and the refusals, over the in-memory store (no Postgres) with scripted
//! models. The process as a binary is `tests/binary.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::Path;
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
use adam_model::{DynModel, MockModel, ReasoningEffort, ToolCall};
use adam_runtime::Runtime;
use adam_service::{Agents, RuntimeOptions, ServeError, Service};
use common::{
    PersonaModel, SEARCH_RESULT, SearchServer, assistant, chat, edit_instructions, folder_with,
    researcher, shipped_researcher,
};
use serde_json::json;
use url::Url;

/// The system prompt of a request without the sentence the loop appends about `after_previous`
/// (ADR 0049): the agent's own words, as the test wrote them. The sentence must be there: a request
/// at a limit above one always carries it.
fn plain(system: &Option<String>) -> Option<String> {
    system.as_ref().map(|s| {
        let note = format!("\n\n{}", adam_llm_agent::AFTER_PREVIOUS_INSTRUCTION);
        assert!(
            s.contains(&note),
            "the sentence about after_previous is not in the system prompt: {s:?}"
        );
        s.replacen(&note, "", 1)
    })
}

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
        profiles: adam_model::ModelProfiles::default(),
        mcp,
        options: options(),
        step_io: adam_llm_agent::StepIo::default(),
        max_parallel: 10,
        parking: adam_llm_agent::ModelParking::default(),
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
        access_tiers,
        model_choices,
        reasoning_efforts,
        commands,
        ..
    } = agents;
    Service::new(register(Runtime::builder(store.clone())), name, &options)
        .with_inbound(inbound)
        .with_access_tiers(access_tiers)
        .with_model_choices(model_choices)
        .with_reasoning_efforts(reasoning_efforts)
        .with_commands(commands)
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
            adam_a2a::A2UI_EXTENSION_V1_0,
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

/// A folder with `commands/` lists them on the card (`commands/v1`, sorted) and the agent expands
/// a message that names one, as `/name arguments`, before it is stored.
#[tokio::test]
async fn a_folder_serves_its_commands() {
    let folder = assistant();
    for (name, text) in [
        (
            "greet",
            "---\ndescription: Greet a person.\nargument-hint: \"<name>\"\n---\nSay hi to $1.\n",
        ),
        (
            "bye",
            "---\ndescription: Say goodbye.\n---\nSay goodbye to $ARGUMENTS.\n",
        ),
    ] {
        let path = folder
            .path()
            .join("agent/commands")
            .join(format!("{name}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let card = card_of(&def_of(&folder), &url()).unwrap();
    let entry = card
        .extensions
        .iter()
        .find(|e| e.uri == adam_a2a::COMMANDS_EXTENSION)
        .expect("the card lists commands/v1");
    assert_eq!(
        serde_json::Value::Object(entry.params.clone()),
        json!({"commands": [
            {"name": "bye", "description": "Say goodbye."},
            {"name": "greet", "description": "Greet a person.", "argumentHint": "<name>"},
        ]})
    );

    let mock = Arc::new(MockModel::new());
    mock.push_text("hello");
    let dynamic: DynModel = mock.clone();
    let agents = build(
        def_of(&folder),
        None,
        Some(worker(dynamic, McpPolicy::default())),
    )
    .await
    .unwrap();
    let service = service_over(agents, &store());
    let worker = Worker::start(&service);
    ask(&service, "/greet Bob").await;
    worker.stop().await;
    assert_eq!(
        mock.requests()[0].messages[0],
        adam_model::Message::user_text("Say hi to Bob.")
    );
}

/// A folder's schedule runs (ADR 0060): the worker runtime `agents` registers steps the scheduler,
/// and a firing reaches the agent as a person's message whose text is the schedule's prompt.
#[tokio::test]
async fn a_schedule_of_the_folder_fires_the_agent() {
    let folder = assistant();
    let path = folder.path().join("agent/schedules/soon.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "---\nafter: 1s\n---\nSay what is due.\n").unwrap();
    let def = def_of(&folder);
    let schedules = def.manifest().schedules.clone();
    assert_eq!(schedules.len(), 1);

    let mock = Arc::new(MockModel::new());
    mock.push_text("Nothing is due.");
    let dynamic: DynModel = mock.clone();
    let agents = build(
        def.clone(),
        None,
        Some(worker(dynamic, McpPolicy::default())),
    )
    .await
    .unwrap();
    assert!(format!("{agents:?}").contains("schedules"), "{agents:?}");
    // A control plane steps nothing, so it neither registers the scheduler nor reconciles.
    let control = build(def.clone(), None, None).await.unwrap();
    assert!(!format!("{control:?}").contains("schedules"), "{control:?}");
    let store = store();
    let service = service_over(agents, &store);
    let done = adam_schedule::reconcile_store(store.clone(), def.name(), &schedules)
        .await
        .unwrap();
    assert_eq!(done.started, ["soon"]);
    let worker = Worker::start(&service);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let fired = loop {
        let runs = store
            .list_runs(&adam_core::RunQuery::new(
                def.name(),
                adam_core::ConversationScope::Prefix(format!("adam.schedule:{}/soon@", def.name())),
                10,
            ))
            .await
            .unwrap();
        if let Some(run) = runs
            .into_iter()
            .find(|r| r.status == adam_core::RunStatus::Done)
        {
            break run;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the schedule did not fire"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    worker.stop().await;
    let trigger = adam_schedule::trigger_of(fired.conversation_id.as_deref().unwrap()).unwrap();
    assert_eq!(trigger.name, "soon");
    assert_eq!(
        mock.requests()[0].messages[0],
        adam_model::Message::user_text("Say what is due.")
    );
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
/// folder's rendered prompt, and the person's screen and the plan are the only tools it is offered.
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
    assert_eq!(
        tools,
        [
            "ask_user",
            "show",
            "ui_catalog",
            "plan_work",
            "plan_check",
            "plan_add"
        ]
    );
    worker.stop().await;
}

/// A folder with no `tools:` is offered the plan tools with the screen's, and a call to one keeps a
/// plan (ADR 0044 decision 5); a `tools:` list narrows them by name like the other built-in tools.
#[tokio::test]
async fn a_folder_agent_without_tools_offers_the_plan_tools() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "p1".into(),
        name: "plan_work".into(),
        arguments: json!({"items": ["Read the file", "Answer"]}),
        ..Default::default()
    }])
    .push_text("Planned.");
    let model: DynModel = mock.clone();
    let store = store();
    let agents = build(def_of(&chat()), None, Some(worker_parts(model)))
        .await
        .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);
    let done = ask(&service, "two things please").await;
    assert_eq!(said(&done), Some("Planned."));
    let requests = mock.requests();
    let offered: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        offered,
        [
            "ask_user",
            "show",
            "ui_catalog",
            "plan_work",
            "plan_check",
            "plan_add"
        ]
    );
    match requests[1].messages.last().unwrap() {
        adam_model::Message::Tool {
            content, is_error, ..
        } => {
            assert!(!is_error, "{content}");
            assert!(
                content.starts_with("Plan saved (2 items).\nPlan\n1. [ ] Read the file\n"),
                "{content}"
            );
        }
        other => panic!("expected the plan call's result, got {other:?}"),
    }
    worker.stop().await;

    // `tools:` narrows them by name, with `*`.
    let narrowed = chat();
    edit_instructions(&narrowed, |t| {
        t.replacen(
            "limits:",
            "tools: [ask_user, plan_work, plan_check]\nlimits:",
            1,
        )
    });
    let mock = Arc::new(MockModel::new());
    mock.push_text("Hi.");
    let model: DynModel = mock.clone();
    let store = self::store();
    let agents = build(def_of(&narrowed), None, Some(worker_parts(model)))
        .await
        .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);
    ask(&service, "hi").await;
    let offered: Vec<String> = mock.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(offered, ["ask_user", "plan_work", "plan_check"]);
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
        ..Default::default()
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
        ..Default::default()
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
    assert_eq!(
        offered,
        [
            "ask_user",
            "show",
            "ui_catalog",
            "plan_work",
            "plan_check",
            "plan_add",
            "test__echo",
            "read_output"
        ]
    );
    match requests[1].messages.last().unwrap() {
        adam_model::Message::Tool {
            call_id,
            content,
            is_error,
            ..
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
        ..Default::default()
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
        .filter(|a| {
            a.name
                .as_deref()
                .is_some_and(|n| n.starts_with("screenshot-"))
        })
        .collect();
    assert_eq!(shot.len(), 1, "{artifacts:?}");
    assert_eq!(shot[0].parts.len(), 1);
    let part = &shot[0].parts[0];
    assert_eq!(
        part.content,
        a2a::PartContent::Raw(adam_mcp_testkit::PNG.to_vec())
    );
    assert_eq!(part.media_type.as_deref(), Some("image/png"));
    let name = part.filename.clone().unwrap();
    assert!(name.ends_with(".png") && shot[0].name.as_deref() == Some(name.as_str()));
    let hash =
        adam_runtime::Artifact::file("x", "image/png", "x.png", adam_mcp_testkit::PNG.to_vec())
            .unwrap()
            .file_sha256()
            .unwrap();
    match mock.requests()[1].messages.last().unwrap() {
        adam_model::Message::Tool {
            content, is_error, ..
        } => assert_eq!(
            (content.as_str(), *is_error),
            (
                format!(
                    "Shared {name} (67 bytes, image/png). To show it in your answer, write \
                     ![description]({name}). To use it as the picture of an `ask_user` option, \
                     pass image {{sha256: \"{hash}\", alt}}. In a `show` block, an `Image` names \
                     it: {{component: \"Image\", artifact: \"{hash}\", alt}}."
                )
                .as_str(),
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
        arguments: json!({"text": format!("found {FROM_THE_ENVIRONMENT} and {MODEL_KEY} and {NAMED_ONLY}")}), ..Default::default()
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
    let marker = adam_agent::redact::REDACTED;
    let scrubbed = format!("found {marker} and {marker} and {marker}");
    assert_eq!(report(false)["input"], json!({"text": scrubbed}));
    assert_eq!(report(true)["output"], json!({"text": scrubbed}));
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
        ..Default::default()
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
        [
            "ask_user",
            "show",
            "ui_catalog",
            "plan_work",
            "plan_check",
            "plan_add",
            "search__web_search",
            "read_output"
        ]
    );
    let spec = &requests[0].tools[6];
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
            ..
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
        ..Default::default()
    }])
    .push_tool_calls(vec![ToolCall {
        id: "r2".into(),
        name: "search__web_search".into(),
        arguments: json!({"query": "[mock:error]"}),
        ..Default::default()
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

/// The published catalog (versions 2 to 8 have no `protocolVersion`) as a screen sends it now: a
/// `ui-catalog/v2` document, whose digest is of what is sent.
fn catalog_v2(published: &str) -> serde_json::Value {
    let mut catalog: serde_json::Value = serde_json::from_str(published).unwrap();
    catalog["protocolVersion"] = json!("1.0");
    catalog
}

/// The message the orchestrator sends from a screen that draws `catalog` (the document and its lock),
/// with the catalog inline: the `ui-catalog/v2` metadata and the A2UI capabilities.
fn from_the_screen(text: &str, (catalog, lock): (&str, &str)) -> Message {
    let lock: serde_json::Value = serde_json::from_str(lock).unwrap();
    let catalog = catalog_v2(catalog);
    let mut message = user(text);
    message.metadata = Some(
        json!({
            adam_a2a::UI_CATALOG_EXTENSION: {
                "catalogId": CATALOG_ID, "version": lock["version"],
                "digest": adam_ui::catalog_digest(&catalog).unwrap(), "inline": true},
            "a2uiRendererCapabilities": {"1.0": {
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
        arguments: json!({"query": "rust async"}), ..Default::default()
    }])
    .push_tool_calls(vec![ToolCall {
        id: "r2".into(),
        name: "ui_catalog".into(),
        arguments: json!({}), ..Default::default()
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
        ]}), ..Default::default()
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
    let document = catalog_v2(CATALOG_V3.0);
    let catalog = adam_ui::Catalog::from_document(
        document.clone(),
        &adam_ui::Claimed {
            catalog_id: CATALOG_ID.to_owned(),
            version: 3,
            digest: adam_ui::catalog_digest(&document).unwrap(),
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
            "this screen did not offer a component catalog this turn; answer in text",
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
        ..Default::default()
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
    assert_eq!(
        offered(0),
        [
            "ask_user",
            "show",
            "ui_catalog",
            "plan_work",
            "plan_check",
            "plan_add",
            "reviewer",
            "subagent_start",
            "subagent_send",
            "subagent_read",
            "subagent_close",
            "subagent_list"
        ]
    );
    // The child: its own prompt, and no tool (a subagent gets none it does not list).
    assert_eq!(
        plain(&requests[1].system).as_deref(),
        Some("You review texts.")
    );
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

/// A remote subagent a deployment points at: the chart renders the folder with the agent's URL in
/// `a2a:`, and its token comes from the environment through `auth: bearer:VAR`, never from a file.
/// An unset variable and an in-cluster plain-`http` URL are the deployment's mistakes (78) until it
/// gives the token and says plain `http` is fine (`A2A_ALLOW_INSECURE_REMOTES`, which `serve` turns
/// into `AgentDef::allow_insecure_remotes`).
#[tokio::test]
async fn a_remote_subagent_a_deployment_points_at() {
    let folder = chat();
    std::fs::create_dir_all(folder.path().join("agent/subagents")).unwrap();
    std::fs::write(
        folder.path().join("agent/subagents/browser.md"),
        "---\ndescription: Reads web pages and takes screenshots.\n\
         a2a: http://browser.agents.svc:8080/.well-known/agent-card.json\n\
         auth: bearer:BROWSER_A2A_TOKEN\n---\n",
    )
    .unwrap();
    let model: DynModel = Arc::new(MockModel::new());
    let attempt = |def: AgentDef| {
        let model = model.clone();
        async move { build(def, None, Some(worker_parts(model))).await }
    };

    let error = attempt(def_of(&folder).env("BROWSER_A2A_TOKEN", "tok-browser-1"))
        .await
        .expect_err("plain http to another machine is refused");
    let cause = std::error::Error::source(&error).unwrap().to_string();
    assert!(
        cause.contains("browser") && cause.contains("http"),
        "{cause}"
    );
    assert_eq!(exit_code(&error), 78);

    let error = attempt(def_of(&folder).allow_insecure_remotes(true))
        .await
        .expect_err("no token, no subagent");
    let cause = std::error::Error::source(&error).unwrap().to_string();
    assert!(cause.contains("BROWSER_A2A_TOKEN"), "{cause}");
    assert_eq!(exit_code(&error), 78);

    let agents = attempt(
        def_of(&folder)
            .allow_insecure_remotes(true)
            .env("BROWSER_A2A_TOKEN", "tok-browser-1"),
    )
    .await
    .expect("the deployment gave the token and allowed the URL");
    assert_eq!(agents.name, "chat");
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
        device_grant: None,
        injected_run: false,
        ca_roots: None,
        park_limit: std::time::Duration::from_secs(24 * 60 * 60),
        alias: "m".into(),
        model_aliases_file: None,
        profiles: Default::default(),
        extra_body: None,
        echo_reasoning: None,
        context_window: None,
        prompt_cache_key: adam_service::PromptCacheKey::Off,
        images: None,
    };
    let error = AgentError::Model(model.client().err().expect("not a URL"));
    assert_eq!(exit_code(&error), 78);
    assert_eq!(error.to_string(), "building the model client");

    // The service: Postgres gone is 69, a taken port 71, a component that stopped 70.
    let down = ServeError::Connect {
        backend: "Postgres",
        source: adam_core::StoreError::unavailable(std::io::Error::other("refused")),
    };
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

// ------------------------------------------------------------------ MCP prompts as commands

fn docs_script() -> adam_mcp_testkit::Script {
    adam_mcp_testkit::Script {
        instructions: None,
        prompts: vec![
            adam_mcp_testkit::TestPrompt::new("summarize", "Summarize a topic.")
                .argument("topic", true)
                .argument("style", false)
                .user("Summarize {{topic}} in a {{style}} style."),
        ],
        ..Default::default()
    }
}

fn write_docs_mcp_json(folder: &tempfile::TempDir, url: &str) {
    std::fs::write(
        folder.path().join("agent/mcp.json"),
        format!(r#"{{"mcpServers": {{"docs": {{"type": "http", "url": "{url}"}}}}}}"#),
    )
    .unwrap();
}

/// The line the model reads before the text of the `summarize` prompt of the server `docs`.
const SUMMARIZE_LINE: &str =
    "(the prompt summarize of the MCP server docs, with the person's arguments)\n\n";

/// The prompts of an MCP server are commands of the agent: the card lists them (`commands/v1`,
/// with the arguments the prompt declares as the hint), a message that names one is expanded with
/// `prompts/get` before it is stored, and the run reads the server's text.
#[tokio::test]
async fn the_prompts_of_an_mcp_server_are_commands_of_the_agent() {
    let server = TestHttpServer::start_scripted(None, docs_script()).await;
    let folder = chat();
    write_docs_mcp_json(&folder, &server.url());
    let card = card_of(&def_of(&folder), &url()).unwrap();
    assert!(
        card.extensions
            .iter()
            .all(|e| e.uri != adam_a2a::COMMANDS_EXTENSION),
        "before the servers are connected the card knows no prompt"
    );

    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let agents = build(def_of(&folder), Some(card), Some(worker_parts(model)))
        .await
        .unwrap();
    let listed = agents.card.clone().expect("the card is kept");
    let entry = listed
        .extensions
        .iter()
        .find(|e| e.uri == adam_a2a::COMMANDS_EXTENSION)
        .expect("the card lists commands/v1 once the prompts are known");
    assert_eq!(
        serde_json::Value::Object(entry.params.clone()),
        json!({"commands": [{
            "name": "docs__summarize",
            "description": "Summarize a topic.",
            "argumentHint": "<topic> [style]"
        }]})
    );

    let service = service_over(agents, &store());
    let worker = Worker::start(&service);
    ask(&service, "/docs__summarize rust formal").await;
    worker.stop().await;
    assert_eq!(
        mock.requests()[0].messages[0],
        adam_model::Message::user_text(format!(
            "{SUMMARIZE_LINE}Summarize rust in a formal style."
        ))
    );
    assert_eq!(server.prompt_gets(), 1, "asked once, at submit");
}

/// A command of the folder with the name of a prompt wins: the prompt is not served and not listed.
#[tokio::test]
async fn a_folders_command_wins_over_an_mcp_prompt_of_the_same_name() {
    let server = TestHttpServer::start_scripted(None, docs_script()).await;
    let folder = chat();
    write_docs_mcp_json(&folder, &server.url());
    let commands = folder.path().join("agent/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("docs__summarize.md"),
        "---\ndescription: The folder's own.\n---\nFrom the folder: $ARGUMENTS\n",
    )
    .unwrap();
    let card = card_of(&def_of(&folder), &url()).unwrap();
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let agents = build(def_of(&folder), Some(card), Some(worker_parts(model)))
        .await
        .unwrap();
    let listed = agents.card.clone().unwrap();
    let entry = listed
        .extensions
        .iter()
        .find(|e| e.uri == adam_a2a::COMMANDS_EXTENSION)
        .unwrap();
    assert_eq!(
        serde_json::Value::Object(entry.params.clone()),
        json!({"commands": [{
            "name": "docs__summarize",
            "description": "The folder's own."
        }]})
    );
    let service = service_over(agents, &store());
    let worker = Worker::start(&service);
    ask(&service, "/docs__summarize rust").await;
    worker.stop().await;
    assert_eq!(
        mock.requests()[0].messages[0],
        adam_model::Message::user_text("From the folder: rust")
    );
    assert_eq!(server.prompt_gets(), 0, "the prompt was never asked for");
}

/// A prompt that is not text of the user is refused as invalid params, and nothing is written.
#[tokio::test]
async fn a_prompt_that_is_not_text_is_refused_before_anything_is_stored() {
    let script = adam_mcp_testkit::Script {
        instructions: None,
        prompts: vec![adam_mcp_testkit::TestPrompt::new("picture", "An image.").image()],
        ..Default::default()
    };
    let server = TestHttpServer::start_scripted(None, script).await;
    let folder = chat();
    write_docs_mcp_json(&folder, &server.url());
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let agents = build(def_of(&folder), None, Some(worker_parts(model)))
        .await
        .unwrap();
    let service = service_over(agents, &store());
    let error = service
        .backend
        .submit(alice(), user("/docs__picture"), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, adam_a2a::BackendError::InvalidParams(why) if why.contains("not text")),
        "{error:?}"
    );
}

/// The shipped painter reads cleanly, names its image server with a longer timeout than the default
/// and says in its mcp.json that the images are shared as files.
#[tokio::test]
async fn the_shipped_painter_loads_without_a_warning() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/agents/painter");
    let loaded = folder::load(&dir).expect("the painter loads");
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    assert_eq!(loaded.def.name(), "painter");
    let card = card_of(&loaded.def, &url()).unwrap();
    assert_eq!(card.name, "Painter");
    let images = &loaded.def.manifest().mcp.as_ref().unwrap().servers["images"];
    assert!(images.shares_files());
    assert_eq!(images.settings().timeout_secs, Some(120));
    assert_eq!(images.settings().connect_timeout_secs, Some(10));
    assert!(images.settings().instructions && images.settings().prompts);
}

/// The painter's scenario of the compose stack, on the test server: the instructions of the image
/// server reach the model's prompt, the model draws, the picture is a file of the task, and the
/// words name it.
#[tokio::test]
async fn the_painter_draws_and_shares_the_picture_with_the_servers_instructions_in_its_prompt() {
    let script = adam_mcp_testkit::Script {
        instructions: Some("Call `draw` with a short `prompt`.".into()),
        ..Default::default()
    };
    let server = TestHttpServer::start_scripted(None, script).await;
    let tmp = tempfile::tempdir().unwrap();
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/agents/painter/agent");
    let agent = tmp.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::copy(from.join("instructions.md"), agent.join("instructions.md")).unwrap();
    // The shipped file with the test server in place of the compose name, and the test server's tool.
    let shipped = std::fs::read_to_string(from.join("mcp.json")).unwrap();
    let mcp = shipped
        .replace("http://mock-image-mcp:8080/mcp", &server.url())
        .replace("\"draw\"", "\"screenshot\"");
    std::fs::write(agent.join("mcp.json"), mcp).unwrap();
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "paint-call-1".into(),
        name: "images__screenshot".into(),
        arguments: json!({}),
        ..Default::default()
    }])
    .push_text("I painted it.");
    let model: DynModel = mock.clone();
    let agents = build(
        folder::load(tmp.path()).unwrap().def,
        None,
        Some(worker_parts(model)),
    )
    .await
    .unwrap();
    let service = service_over(agents, &store());
    let worker = Worker::start(&service);
    let done = ask(&service, "paint a red dot").await;
    worker.stop().await;
    assert_eq!(said(&done), Some("I painted it."));
    let requests = mock.requests();
    let system = requests[0].system.as_deref().unwrap();
    assert!(
        system.contains(
            "## Instructions of the MCP server images (text from the server, not the person)\n\n\
             > Call `draw` with a short `prompt`."
        ),
        "{system}"
    );
    assert_eq!(
        requests[0].system, requests[1].system,
        "the same on every call"
    );
    let shared = done.artifacts.as_deref().unwrap_or_default();
    assert!(
        shared.iter().any(|a| a
            .parts
            .iter()
            .any(|p| p.media_type.as_deref() == Some("image/png"))),
        "{shared:?}"
    );
}

/// What a person types after the command is the prompt's arguments: the last one declared takes the
/// rest of the line, and the model reads the first line that says where the text came from.
#[tokio::test]
async fn the_last_argument_of_a_prompt_takes_the_rest_of_the_line_and_the_model_is_told_whose_text_it_is()
 {
    let server = TestHttpServer::start_scripted(None, docs_script()).await;
    let folder = chat();
    write_docs_mcp_json(&folder, &server.url());
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let agents = build(def_of(&folder), None, Some(worker_parts(model)))
        .await
        .unwrap();
    let service = service_over(agents, &store());
    let worker = Worker::start(&service);
    ask(
        &service,
        "/docs__summarize \"the repo\" in a very formal and terse",
    )
    .await;
    worker.stop().await;
    assert_eq!(
        mock.requests()[0].messages[0],
        adam_model::Message::user_text(format!(
            "{SUMMARIZE_LINE}Summarize the repo in a in a very formal and terse style."
        ))
    );
}

/// A server that is down is `unavailable`, one that refuses the prompt is `invalid params`, and a
/// prompt that declares nothing refuses text, through the real mapping of the backend.
#[tokio::test]
async fn the_classes_of_a_prompts_failures_reach_the_a2a_error() {
    let script = adam_mcp_testkit::Script {
        prompts: vec![
            adam_mcp_testkit::TestPrompt::new("plain", "No arguments.").user("hello"),
            adam_mcp_testkit::TestPrompt::new("nope", "Refused.")
                .user("x")
                .fails("not today"),
        ],
        ..Default::default()
    };
    let mut server = TestHttpServer::start_scripted(None, script).await;
    let folder = chat();
    write_docs_mcp_json(&folder, &server.url());
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let mut parts = worker_parts(model);
    parts.mcp = McpPolicy::default()
        .connect_timeout(Duration::from_secs(2))
        .call_timeout(Duration::from_secs(2));
    let agents = build(def_of(&folder), None, Some(parts)).await.unwrap();
    let service = service_over(agents, &store());
    let submit = |text: &'static str| {
        let service = &service;
        async move {
            service
                .backend
                .submit(alice(), user(text), None, None)
                .await
                .unwrap_err()
        }
    };
    assert!(matches!(
        submit("/docs__plain some text").await,
        adam_a2a::BackendError::InvalidParams(_)
    ));
    assert!(matches!(
        submit("/docs__nope").await,
        adam_a2a::BackendError::InvalidParams(why) if why.contains("not today")
    ));
    server.stop().await;
    assert!(matches!(
        submit("/docs__plain").await,
        adam_a2a::BackendError::Unavailable { .. }
    ));
}

/// The folder's commands and the prompts of its servers are at most 50, the folder's first.
#[tokio::test]
async fn the_folders_commands_and_the_prompts_together_are_at_most_the_limit() {
    let many = adam_mcp_testkit::Script {
        prompts: (0..51)
            .map(|n| adam_mcp_testkit::TestPrompt::new(&format!("p{n:02}"), "One.").user("x"))
            .collect(),
        ..Default::default()
    };
    let server = TestHttpServer::start_scripted(None, many).await;
    let logs = adam_mcp_testkit::LogCapture::start();
    let folder = chat();
    write_docs_mcp_json(&folder, &server.url());
    let commands = folder.path().join("agent/commands");
    std::fs::create_dir_all(&commands).unwrap();
    for n in 0..3 {
        std::fs::write(
            commands.join(format!("own{n}.md")),
            "---\ndescription: The folder's own.\n---\nown\n",
        )
        .unwrap();
    }
    let card = card_of(&def_of(&folder), &url()).unwrap();
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let agents = build(def_of(&folder), Some(card), Some(worker_parts(model)))
        .await
        .unwrap();
    let listed = agents.card.clone().unwrap();
    let entry = listed
        .extensions
        .iter()
        .find(|e| e.uri == adam_a2a::COMMANDS_EXTENSION)
        .unwrap();
    let names: Vec<&str> = entry.params["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 50, "{names:?}");
    assert!(
        names.contains(&"own0") && names.contains(&"own2"),
        "the folder's come first"
    );
    assert!(
        names.contains(&"docs__p46") && !names.contains(&"docs__p47"),
        "the last prompts are dropped"
    );
    assert_eq!(
        agents.commands.as_ref().unwrap().served(),
        50,
        "the backend serves what the card lists"
    );
    let text = logs.text();
    assert_eq!(
        text.matches("over the limit of commands").count(),
        1,
        "{text}"
    );
    assert!(
        text.contains("dropped=4") || text.contains("dropped: 4"),
        "{text}"
    );
}

/// A control plane connects no MCP server: it refuses what names the prompt of one of the folder's
/// servers (and says at startup that it does), and sends nothing to a model as plain text.
#[tokio::test]
async fn a_control_plane_refuses_a_prompt_it_cannot_expand_and_warns_at_start() {
    let server = TestHttpServer::start_scripted(None, docs_script()).await;
    let folder = chat();
    write_docs_mcp_json(&folder, &server.url());
    let logs = adam_mcp_testkit::LogCapture::start();
    let agents = build(def_of(&folder), None, None).await.unwrap();
    let text = logs.text();
    assert!(
        text.contains("connects no MCP server") && text.contains("docs"),
        "{text}"
    );
    let service = service_over(agents, &store());
    let error = service
        .backend
        .submit(alice(), user("/docs__summarize rust"), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, adam_a2a::BackendError::InvalidParams(why) if why.contains("control plane")),
        "{error:?}"
    );
    assert_eq!(server.prompt_gets(), 0, "a control plane asks no server");
    // A server that serves no prompts is nothing to refuse (and nothing to warn about).
    let quiet = chat();
    std::fs::write(
        quiet.path().join("agent/mcp.json"),
        format!(
            r#"{{"mcpServers": {{"docs": {{"type": "http", "url": "{}", "prompts": false}}}}}}"#,
            server.url()
        ),
    )
    .unwrap();
    let logs = adam_mcp_testkit::LogCapture::start();
    let agents = build(def_of(&quiet), None, None).await.unwrap();
    assert!(
        !logs.text().contains("connects no MCP server"),
        "{}",
        logs.text()
    );
    let service = service_over(agents, &store());
    assert!(
        service
            .backend
            .submit(alice(), user("/docs__summarize rust"), None, None)
            .await
            .is_ok()
    );
}

/// `MAX_PARALLEL_TOOL_CALLS` reaches the folder's agents, and the folder's own `limits.max_parallel`
/// can only lower it (ADR 0049).
#[tokio::test]
async fn the_deployments_parallel_limit_reaches_the_agent_and_the_file_only_lowers_it() {
    let limit = |folder: &tempfile::TempDir, setting: usize| {
        let def = def_of(folder);
        let model: DynModel = Arc::new(MockModel::new());
        async move {
            adam_agent::assemble_limited(
                def,
                model,
                "test-model",
                &McpPolicy::default(),
                adam_llm_agent::StepIo::default(),
                setting,
            )
            .await
            .expect("the folder assembles")
            .root()
            .max_parallel()
        }
    };
    let folder = chat();
    assert_eq!(limit(&folder, 6).await, 6);
    assert_eq!(limit(&folder, 1).await, 1);
    edit_instructions(&folder, |t| {
        t.replacen("limits:", "limits:\n  max_parallel: 2", 1)
    });
    assert_eq!(limit(&folder, 6).await, 2, "the file lowers it");
    assert_eq!(limit(&folder, 1).await, 1, "and never raises it");
}

/// `MODEL_ALIASES_FILE`'s profiles reach the agent through `WorkerParts`: the folder's
/// `reasoning_effort` is checked against the model's profile, and the request asks for it. A value
/// the model does not take is a startup error naming the agent.
#[tokio::test]
async fn the_deployments_model_profiles_reach_the_agents_request_and_refuse_a_bad_effort() {
    let folder = chat();
    edit_instructions(&folder, |t| {
        t.replacen("limits:", "reasoning_effort: high\nlimits:", 1)
    });
    let profiles = |efforts: &str| {
        adam_model::ModelProfiles::parse_json(&format!(
            r#"{{"profiles": {{"test-model": {{"reasoningEfforts": {efforts}, "reasoningWire": "reasoning_effort"}}}}}}"#
        ))
        .expect("valid profiles")
    };

    let mock = Arc::new(MockModel::new());
    mock.push_text("Hi.");
    let model: DynModel = mock.clone();
    let mut parts = worker(model, McpPolicy::default());
    parts.profiles = profiles(r#"["low", "high"]"#);
    let agents = build(def_of(&folder), None, Some(parts)).await.unwrap();
    let service = service_over(agents, &store());
    let started = Worker::start(&service);
    ask(&service, "hi").await;
    started.stop().await;
    assert_eq!(
        mock.requests()[0].reasoning_effort,
        Some(ReasoningEffort::High),
        "the folder's value, checked against the model's profile"
    );

    let model: DynModel = Arc::new(MockModel::new());
    let mut parts = worker(model, McpPolicy::default());
    parts.profiles = profiles(r#"["low"]"#);
    let error = build(def_of(&folder), None, Some(parts))
        .await
        .expect_err("high is not one of the model's levels");
    let AgentError::Assembly(error) = &error else {
        panic!("{error}");
    };
    let adam::AssemblyError::ReasoningEffort {
        origin,
        effort,
        model,
        ..
    } = &**error
    else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "chat");
    assert_eq!(effort, "high");
    assert_eq!(model, "test-model");
}

/// A deployment that profiles a second model offers it: `model-pick/v1`'s choices reach the
/// backend, and a message that names one is answered by that model. An alias the card does not
/// offer is refused before anything is written.
#[tokio::test]
async fn a_served_agent_offers_the_deployments_other_models_and_takes_a_pick() {
    use adam_a2a::{MODEL_NOT_OFFERED, MODEL_PICK_EXTENSION};
    use adam_model::ModelProfiles;
    use std::collections::HashMap;

    let folder = chat();
    let mock = Arc::new(MockModel::new());
    mock.push_text("Hi.");
    let model: DynModel = mock.clone();
    let mut parts = worker(model, McpPolicy::default());
    parts.profiles = ModelProfiles::parse_json(
        r#"{"profiles": {"test-model": {}, "alias-a": {"maxOutputTokens": 2048, "endpoint": "gateway-a"}}}"#,
    )
    .expect("valid profiles");
    let agents = build(def_of(&folder), None, Some(parts)).await.unwrap();
    // The root's choices are what the card advertises: its own model is not one of them, and the
    // choice carries the endpoint label the deployment's profile names (ANR-623).
    let choices = agents
        .model_choices
        .as_ref()
        .expect("the deployment offers a model");
    assert_eq!(choices.aliases(), ["alias-a"]);
    assert_eq!(choices.endpoint_of("alias-a"), Some("gateway-a"));
    let service = service_over(agents, &store());
    let started = Worker::start(&service);

    let picking = || Caller::new("token-0").with_extensions([MODEL_PICK_EXTENSION]);
    let naming = |text: &str, alias: &str| {
        let mut message = user(text);
        message.metadata = Some(HashMap::from([(
            MODEL_PICK_EXTENSION.to_owned(),
            json!({"model": alias}),
        )]));
        message
    };
    let task = service
        .backend
        .submit(picking(), naming("hi", "alias-a"), None, None)
        .await
        .expect("an offered model");
    wait_for(&service, &task.id, TaskState::Completed).await;
    assert_eq!(mock.requests()[0].model, "alias-a");
    assert_eq!(
        mock.requests()[0].max_output_tokens,
        Some(2048),
        "the picked model's own cap"
    );

    let error = service
        .backend
        .submit(picking(), naming("hi", "alias-b"), None, None)
        .await
        .expect_err("not offered");
    assert!(
        matches!(&error, adam_a2a::BackendError::InvalidParams(m)
            if m.starts_with(MODEL_NOT_OFFERED) && m.contains("alias-a")),
        "{error:?}"
    );
    started.stop().await;
}

/// The root's model takes reasoning levels (`MODEL_ALIASES_FILE`): the card lists them
/// (`reasoning/v1`: the model's levels, lowest first, and the level the folder resolved), the backend
/// refuses an effort the model does not take before anything is written, and one it takes reaches the
/// request (ADR 0063, decision 5).
#[tokio::test]
async fn a_served_agent_lists_its_models_reasoning_levels_and_takes_one() {
    use adam_a2a::{REASONING_EFFORT_MEMBER, REASONING_EXTENSION};
    use adam_model::ModelProfiles;
    use std::collections::HashMap;

    let folder = chat();
    edit_instructions(&folder, |t| {
        t.replacen("limits:", "reasoning_effort: high\nlimits:", 1)
    });
    let mock = Arc::new(MockModel::new());
    mock.push_text("Hi.");
    let model: DynModel = mock.clone();
    let mut parts = worker(model, McpPolicy::default());
    parts.profiles = ModelProfiles::parse_json(
        r#"{"profiles": {"test-model": {"reasoningEfforts": ["low", "high"], "reasoningWire": "reasoning_effort"}}}"#,
    )
    .expect("valid profiles");
    let base = card_of(&def_of(&folder), &url()).unwrap();
    let agents = build(def_of(&folder), Some(base), Some(parts))
        .await
        .unwrap();
    // The levels the backend reads, and the card the process serves, are the model's and the
    // folder's resolved default.
    let efforts = agents
        .reasoning_efforts
        .as_ref()
        .expect("the model takes a level");
    assert_eq!(
        efforts.levels("chat"),
        [ReasoningEffort::Low, ReasoningEffort::High]
    );
    assert_eq!(efforts.default_of("chat"), Some(ReasoningEffort::High));
    // `serve` composes the card it answers with, so this is the same call it makes.
    let card =
        adam_service::card_with_reasoning(agents.card.clone().expect("a card"), efforts, "chat");
    let entry = card
        .extensions
        .iter()
        .find(|e| e.uri == REASONING_EXTENSION)
        .expect("the card lists reasoning/v1");
    assert!(
        !entry.required,
        "a client that does not know it must still talk to the agent"
    );
    assert_eq!(
        serde_json::Value::Object(entry.params.clone()),
        json!({"efforts": ["low", "high"], "default": "high"})
    );

    let service = service_over(agents, &store());
    let started = Worker::start(&service);
    let asking = || Caller::new("token-0").with_extensions([REASONING_EXTENSION]);
    let naming = |text: &str, effort: &str| {
        let mut message = user(text);
        message.metadata = Some(HashMap::from([(
            REASONING_EXTENSION.to_owned(),
            json!({ REASONING_EFFORT_MEMBER: effort }),
        )]));
        message
    };
    // A level the model does not take is refused, naming the levels, before anything is written.
    let error = service
        .backend
        .submit(asking(), naming("hi", "max"), None, None)
        .await
        .expect_err("max is not one of the model's levels");
    assert!(
        matches!(&error, adam_a2a::BackendError::InvalidParams(m)
            if m.contains("does not take") && m.contains("low, high")),
        "{error:?}"
    );
    // One it takes is the run's, and the request asks for it.
    let task = service
        .backend
        .submit(asking(), naming("hi", "low"), None, None)
        .await
        .expect("a level the model takes");
    wait_for(&service, &task.id, TaskState::Completed).await;
    assert_eq!(
        mock.requests()[0].reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    started.stop().await;
}

// ------------------------------------------------------------------ S0: the lean first request

/// The first request's bytes: the system prompt plus each tool schema as the OpenAI wire wraps it.
fn first_request_bytes(req: &adam_model::ModelRequest) -> (usize, usize) {
    let system = req.system.as_deref().map_or(0, str::len);
    let tools = req
        .tools
        .iter()
        .map(|t| {
            serde_json::to_vec(&json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            }))
            .expect("a tool schema serialises")
            .len()
        })
        .sum();
    (system, tools)
}

/// One line per tool, sorted by name: the generated part of
/// `plans/2026-11-21-tool-loading-table.md`. A new tool, a renamed one or a changed schema changes
/// this table, so the golden file changes and a reviewer sees it.
fn tool_table(req: &adam_model::ModelRequest) -> String {
    let mut rows: Vec<(String, usize)> = req
        .tools
        .iter()
        .map(|t| {
            let bytes = serde_json::to_vec(&json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            }))
            .expect("a tool schema serialises")
            .len();
            (t.name.clone(), bytes)
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("# tool\tbytes\ttokens(chars/4)\n");
    for (name, bytes) in rows {
        out.push_str(&format!("{name}\t{bytes}\t{}\n", bytes / 4));
    }
    out
}

/// Compare the generated table with its golden file, or rewrite it with `UPDATE_GOLDEN=1`.
fn check_tool_golden(generated: &str) {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tool-tokens.txt"
    );
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, generated).expect("the golden is writable");
        println!("updated {path}");
        return;
    }
    let golden = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(
        golden, generated,
        "the tool table changed. If the change is deliberate, regenerate it with \
         `UPDATE_GOLDEN=1 cargo test -p adam-agent --test agent \
         a_folder_agents_first_request_stays_under_the_lean_budget` and commit the diff; \
         a new tool must never arrive without its row."
    );
}

/// S0 of the lean-first-request plan (`plans/2026-11-21-lean-first-request.md`): a plain folder
/// agent's first request is measured, pinned against growth, and held to the owner's **5,000-token**
/// budget for a lazy agent (ADR 0066). Today it is already under it; the test says by how much and
/// fails if a change eats the margin.
///
/// The plan's numbers are `cl100k` tokens; CI has no tokenizer, so the pin is in **bytes** and the
/// print gives the repository's own estimate (`chars / 4`, about 6 % above `cl100k`). The message is
/// not counted: it is the same "hi" in every measurement.
#[tokio::test]
async fn a_folder_agents_first_request_stays_under_the_lean_budget() {
    const BASELINE_BYTES: usize = 17437; // measured 2026-11-21, `origin/main` at e62e00b
    const GROWTH_ALLOWED: usize = 1024;
    const LEAN_BUDGET_TOKENS: usize = 5000;

    let mock = Arc::new(MockModel::new());
    mock.push_text("Hi.");
    let model: DynModel = mock.clone();
    let store = store();
    let agents = build(def_of(&chat()), None, Some(worker_parts(model)))
        .await
        .unwrap();
    let service = service_over(agents, &store);
    let worker = Worker::start(&service);
    ask(&service, "hi").await;
    worker.stop().await;

    let first = &mock.requests()[0];
    let (system, tools) = first_request_bytes(first);
    let total = system + tools;
    // Written before the asserts so one `UPDATE_GOLDEN=1` run both prints the numbers and
    // records the table, even while the baseline is still unpinned.
    check_tool_golden(&tool_table(first));
    let tokens = total / 4;
    println!(
        "folder agent first request: system {system} B + tools {tools} B = {total} B \
         (~{tokens} tokens at chars/4); lean budget {LEAN_BUDGET_TOKENS} tokens \
         (~{} B), margin {} tokens; baseline {BASELINE_BYTES} B, allowed +{GROWTH_ALLOWED} B",
        LEAN_BUDGET_TOKENS * 4,
        LEAN_BUDGET_TOKENS.saturating_sub(tokens)
    );
    assert!(
        tokens <= LEAN_BUDGET_TOKENS,
        "the folder agent's first request is ~{tokens} tokens (chars/4), over the \
         {LEAN_BUDGET_TOKENS}-token budget for a lazy agent (ADR 0066)"
    );
    assert!(
        total <= BASELINE_BYTES + GROWTH_ALLOWED,
        "the folder agent's first request grew to {total} B (baseline {BASELINE_BYTES} B, allowed \
         +{GROWTH_ALLOWED} B). A deliberate growth updates the baseline in the same commit; \
         anything else is the regression this test exists for."
    );
}

/// The shipped examples say how their tools load (ADR 0066, decision 1): the chat persona starts
/// lean, and the three agents whose tools are their job opt out.
#[test]
fn the_shipped_examples_say_how_their_tools_load() {
    use adam::agent_fs::ToolsLoading;

    let loading = |name: &str| {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../dev/agents")
            .join(name);
        folder::load(&dir)
            .unwrap_or_else(|e| panic!("the shipped {name} folder loads: {e}"))
            .def
            .manifest()
            .frontmatter
            .tools_loading
    };
    assert_eq!(loading("assistant"), ToolsLoading::Lazy);
    assert_eq!(loading("researcher"), ToolsLoading::Eager);
    assert_eq!(loading("painter"), ToolsLoading::Eager);
}
