//! A whole agent behind A2A, as the screen's orchestrator drives it: a message with the
//! `ui-catalog/v1` and `thread-tools/v1` metadata, a scripted model that asks three questions at once,
//! the person's answer as an A2UI action, and the thread tools of a fake endpoint offered to the model
//! at every turn. A real `Runtime`, `LlmAgent`, `RuntimeTaskBackend` with `vymalo_inbound`, and the
//! fake endpoint of `adam-mcp-testkit`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, Task, TaskState};
use adam_a2a::{
    A2UI_MEDIA_TYPE, Caller, THREAD_TOOLS_EXTENSION, TaskBackend, UI_CATALOG_EXTENSION,
};
use adam_a2a_runtime::{RuntimeTaskBackend, vymalo_inbound};
use adam_core::{DynStore, MemoryStore};
use adam_llm_agent::LlmAgent;
use adam_mcp::McpPolicy;
use adam_mcp_testkit::ThreadToolsServer;
use adam_model::{MockModel, ModelRequest, ToolCall};
use adam_runtime::{BroadcastSink, Runtime};
use adam_ui::{Claimed, Ui};
use serde_json::{Value, json};
use tokio::sync::oneshot;

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const FIXTURE: &str = include_str!("fixtures/catalog-v2.json");
const LOCK: &str = include_str!("fixtures/catalog-v2.lock.json");
const SECRET: &str = "sekret-token-4f1c9a";

fn document() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn claimed() -> Claimed {
    let lock: Value = serde_json::from_str(LOCK).unwrap();
    Claimed {
        catalog_id: CATALOG_ID.to_owned(),
        version: u32::try_from(lock["version"].as_u64().unwrap()).unwrap(),
        digest: lock["digest"].as_str().unwrap().to_owned(),
    }
}

fn as_doubles(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => json!(n.as_f64().unwrap()),
        Value::Array(items) => Value::Array(items.iter().map(as_doubles).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), as_doubles(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

struct Rig {
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    model: Arc<MockModel>,
    ui: Ui,
}

impl Rig {
    fn new() -> Self {
        let store: DynStore = Arc::new(MemoryStore::new());
        let model = Arc::new(MockModel::new());
        let ui = Ui::new(McpPolicy::default());
        let agent = LlmAgent::builder("asker", model.clone(), "m")
            .instructions("You are a test agent.")
            .tools(ui.tools())
            .tool_source(ui.source())
            .build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, "asker")
            .with_poll_interval(Duration::from_millis(10))
            .with_inbound(vymalo_inbound);
        Self {
            runtime,
            backend,
            model,
            ui,
        }
    }

    fn worker(&self) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.runtime.clone();
        let handle = tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        (stop, handle)
    }

    async fn wait(&self, task: &Task, state: TaskState) -> Task {
        for _ in 0..1000 {
            let got = self.backend.get(&alice(), &task.id).await.unwrap().unwrap();
            if got.status.state == state {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {} never reached {state:?}", task.id);
    }
}

fn alice() -> Caller {
    Caller::new("token-0")
}

/// The message the orchestrator sends: the text, the extensions' metadata (numbers as doubles, as an
/// A2A server hands them over), and, when `inline`, the catalog in the renderer's capabilities.
fn from_the_screen(text: &str, grant: Value, inline: bool) -> Message {
    let c = claimed();
    let mut metadata = json!({
        UI_CATALOG_EXTENSION: {
            "catalogId": c.catalog_id, "version": f64::from(c.version), "digest": c.digest,
            "inline": inline},
        "a2uiClientCapabilities": {"v0.9.1": {"supportedCatalogIds": [CATALOG_ID]}},
        THREAD_TOOLS_EXTENSION: grant,
    });
    if inline {
        metadata["a2uiClientCapabilities"]["v0.9.1"]["inlineCatalogs"] =
            json!([as_doubles(&document())]);
    }
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.metadata = Some(
        metadata
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    message
}

fn grant(server: &ThreadToolsServer, expires_at: &str) -> Value {
    json!({"url": server.url("thread-1"), "token": SECRET, "expiresAt": expires_at})
}

fn answer_action(answers: Value) -> Message {
    let mut part = Part::data(json!([{"version": "v0.9.1", "action": {
        "name": "answer", "surfaceId": "ask-choices-call-1", "sourceComponentId": "root",
        "timestamp": "2026-10-01T10:00:00Z", "context": {"answers": answers}}}]))
    .with_media_type(A2UI_MEDIA_TYPE);
    part.metadata = Some(HashMap::from([(
        "mimeType".to_owned(),
        json!(A2UI_MEDIA_TYPE),
    )]));
    Message::new(Role::User, vec![part])
}

fn three_questions_call() -> ToolCall {
    ToolCall {
        id: "choices-call-1".into(),
        name: "ask_user".into(),
        arguments: json!({
            "question": "Three quick questions before I start",
            "choices": [
                {"id": "db", "question": "Which database?",
                 "options": [{"value": "pg", "label": "Postgres"}, {"value": "sqlite", "label": "SQLite"}]},
                {"id": "auth", "question": "Which login?",
                 "options": [{"value": "keycloak", "label": "Keycloak"}, {"value": "none", "label": "No login"}]},
                {"id": "deploy", "question": "Where does it run?",
                 "options": [{"value": "k8s", "label": "Kubernetes"}, {"value": "compose", "label": "Docker Compose"}]}
            ]
        }),
    }
}

fn names(request: &ModelRequest) -> Vec<&str> {
    request.tools.iter().map(|t| t.name.as_str()).collect()
}

fn future() -> &'static str {
    "2999-01-01T00:00:00Z"
}

#[tokio::test]
async fn the_agent_asks_three_questions_as_one_form_the_person_answers_and_it_goes_on() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![three_questions_call()])
        .push_text("Going with Postgres, Keycloak and Compose.");
    let (stop, handle) = rig.worker();

    let task = rig
        .backend
        .submit(
            alice(),
            from_the_screen("set up the project", grant(&server, future()), true),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let asked = rig.wait(&task, TaskState::InputRequired).await;

    // The question is text and a form: one Choices of three questions under the screen's catalogId.
    let status = asked.status.message.as_ref().expect("a status message");
    assert_eq!(status.text(), Some("Three quick questions before I start"));
    assert_eq!(status.parts.len(), 2);
    let PartContent::Data(ui) = &status.parts[1].content else {
        panic!("the second part is the interface: {status:?}");
    };
    assert_eq!(status.parts[1].media_type.as_deref(), Some(A2UI_MEDIA_TYPE));
    let golden: Value = serde_json::from_str(include_str!("golden/ask_choices.json")).unwrap();
    assert_eq!(ui, &golden);

    // The person answers all three with one action.
    rig.backend
        .submit(
            alice(),
            answer_action(json!([
                {"id": "db", "values": ["pg"]},
                {"id": "auth", "values": ["keycloak"]},
                {"id": "deploy", "values": ["compose"]}
            ])),
            Some(task.id.clone()),
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let done = rig.wait(&task, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();

    assert_eq!(
        done.status.message.as_ref().and_then(Message::text),
        Some("Going with Postgres, Keycloak and Compose.")
    );
    let requests = rig.model.requests();
    assert_eq!(requests.len(), 2);
    // What the model was offered: the three tools, then what the thread's endpoint lists.
    assert_eq!(
        names(&requests[0]),
        ["ask_user", "show", "ui_catalog", "get_ui_catalog"]
    );
    // What it read as the person's answer.
    assert_eq!(
        requests[1].messages.last(),
        Some(&adam_model::Message::tool_result(
            "choices-call-1",
            "The person answered through the interface:\n- db: pg\n- auth: keycloak\n- deploy: compose"
        ))
    );
    // The catalog came inline, so the endpoint was listed twice (once a turn) and never asked for it.
    assert_eq!(server.lists(), 2);
    assert!(server.catalog_requests().is_empty());
    assert_eq!(rig.ui.cache().len(), 1);
}

#[tokio::test]
async fn a_tool_the_endpoint_lists_is_offered_under_its_name_and_called_through_the_endpoint() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    server.add_tool(
        "relay__search",
        "Search the web.",
        json!({"type": "object", "properties": {"query": {"type": "string"}}}),
        "found it",
    );
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "relay__search".into(),
            arguments: json!({"query": "rust"}),
        }])
        .push_text("It says: found it.");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            alice(),
            from_the_screen("look it up", grant(&server, future()), false),
            None,
            None,
        )
        .await
        .unwrap();
    let done = rig.wait(&task, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();

    let requests = rig.model.requests();
    assert_eq!(
        names(&requests[0]),
        [
            "ask_user",
            "show",
            "ui_catalog",
            "get_ui_catalog",
            "relay__search"
        ]
    );
    assert_eq!(
        requests[1].messages.last(),
        Some(&adam_model::Message::tool_result(
            "c1",
            r#"found it {"query":"rust"}"#
        ))
    );
    assert_eq!(
        done.status.message.as_ref().and_then(Message::text),
        Some("It says: found it.")
    );
    // A tool the orchestrator attaches later is on the next turn: nothing here changed.
    assert_eq!(
        server.calls(),
        [("relay__search".to_owned(), json!({"query": "rust"}))]
    );
}

#[tokio::test]
async fn a_stale_digest_is_read_again_over_the_thread_tools_and_the_form_is_drawn() {
    let c = claimed();
    let server = ThreadToolsServer::start(&[SECRET]).await;
    server.set_catalog(Some((
        &c.catalog_id,
        u64::from(c.version),
        &c.digest,
        document(),
    )));
    let rig = Rig::new();
    rig.model.push_tool_calls(vec![three_questions_call()]);
    let (stop, handle) = rig.worker();
    // The message says which catalog is current, and does not carry it.
    let task = rig
        .backend
        .submit(
            alice(),
            from_the_screen("set up", grant(&server, future()), false),
            None,
            None,
        )
        .await
        .unwrap();
    let asked = rig.wait(&task, TaskState::InputRequired).await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(
        asked.status.message.as_ref().unwrap().parts.len(),
        2,
        "a form, not text"
    );
    assert_eq!(server.catalog_requests(), [None], "one refetch");
}

#[tokio::test]
async fn with_an_expired_grant_the_agent_offers_no_thread_tools_and_asks_in_text() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    let rig = Rig::new();
    rig.model.push_tool_calls(vec![three_questions_call()]);
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            alice(),
            // The catalog is only referenced, the grant is old: nothing can be read again.
            from_the_screen("set up", grant(&server, "2020-01-01T00:00:00Z"), false),
            None,
            None,
        )
        .await
        .unwrap();
    let asked = rig.wait(&task, TaskState::InputRequired).await;
    stop.send(()).unwrap();
    handle.await.unwrap();

    assert_eq!(
        names(&rig.model.requests()[0]),
        ["ask_user", "show", "ui_catalog"]
    );
    let status = asked.status.message.as_ref().unwrap();
    assert_eq!(status.parts.len(), 1, "text only");
    let text = status.text().unwrap();
    assert!(
        text.contains("1. Which database?") && text.contains("a) Postgres"),
        "{text}"
    );
    assert_eq!(server.lists(), 0);
    assert!(
        server.authorizations().is_empty(),
        "the endpoint was never called with it"
    );
}

#[tokio::test]
async fn the_token_never_reaches_the_model_or_the_person() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    server.add_tool(
        "relay__search",
        "Search.",
        json!({"type": "object"}),
        "found",
    );
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "relay__search".into(),
            arguments: json!({}),
        }])
        .push_tool_calls(vec![three_questions_call()]);
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            alice(),
            from_the_screen("go", grant(&server, future()), true),
            None,
            None,
        )
        .await
        .unwrap();
    let asked = rig.wait(&task, TaskState::InputRequired).await;
    stop.send(()).unwrap();
    handle.await.unwrap();

    for request in rig.model.requests() {
        let seen = serde_json::to_string(&request.messages).unwrap()
            + &serde_json::to_string(&request.tools).unwrap()
            + request.system.as_deref().unwrap_or_default();
        assert!(!seen.contains(SECRET), "the model was shown the token");
    }
    assert!(
        !serde_json::to_string(&asked.status)
            .unwrap()
            .contains(SECRET)
    );
    // It is in the run's durable state until it expires (the agent's own decision, ADR 0006).
    let run = adam_core::RunId(task.id.parse().unwrap());
    let state = rig.runtime.view(run).await.unwrap().unwrap().state;
    assert_eq!(state["context"]["vymalo.threadTools"]["token"], SECRET);
}
