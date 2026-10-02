//! A screen as the sender and the asker: the backend with `vymalo_inbound` over a real `Runtime` and
//! an `LlmAgent`. A message's extensions reach the run as its context, a question can carry an A2UI
//! interface in its `input-required` status, and the person's answer through that interface is the
//! tool result the model reads.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, Task, TaskState};
use adam_a2a::{
    A2UI_MEDIA_TYPE, Caller, MENTIONS_EXTENSION, THREAD_TOOLS_EXTENSION, TaskBackend, TaskEvent,
    UI_CATALOG_EXTENSION,
};
use adam_a2a_runtime::{
    CONTEXT_MENTIONS, CONTEXT_THREAD_TOOLS, CONTEXT_UI_CATALOG, CONTEXT_UI_REF, RuntimeTaskBackend,
    vymalo_inbound,
};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{Conversation, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{Message as ModelMessage, MockModel, ToolCall, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const DIGEST: &str = "sha256:4ed91bcc9db52d5e2262aef2091d2b3eeccbf5bfe51519d7326fdc6641fb7856";

fn surface() -> Value {
    json!([
        {"version": "v0.9.1", "createSurface": {"surfaceId": "ask-1", "catalogId": CATALOG_ID}},
        {"version": "v0.9.1", "updateComponents": {"surfaceId": "ask-1", "components": [
            {"id": "root", "component": "Choices", "questions": [
                {"id": "db", "question": "Which database?", "options": [
                    {"value": "pg", "label": "Postgres"}, {"value": "sqlite", "label": "SQLite"}]}]}]}}
    ])
}

/// Asks "Which database?" with the surface above.
struct AskWithUi;

#[async_trait]
impl Tool for AskWithUi {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "ask".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    fn asks_user(&self) -> bool {
        true
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input_with_ui("Which database?", surface()))
    }
}

struct Rig {
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    model: Arc<MockModel>,
}

impl Rig {
    fn new() -> Self {
        let store: DynStore = Arc::new(MemoryStore::new());
        let model = Arc::new(MockModel::new());
        let agent = LlmAgent::builder("asker", model.clone(), "m")
            .tool(AskWithUi)
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

    async fn state(&self, task: &Task) -> Conversation {
        let run = RunId(task.id.parse().unwrap());
        let view = self.runtime.view(run).await.unwrap().unwrap();
        serde_json::from_value(view.state).unwrap()
    }
}

fn alice() -> Caller {
    Caller::new("token-0")
}

async fn wait_task(rig: &Rig, id: &str, state: TaskState) -> Task {
    for _ in 0..1000 {
        let task = rig.backend.get(&alice(), id).await.unwrap().unwrap();
        if task.status.state == state {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

fn with_metadata(mut message: Message, metadata: Value) -> Message {
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

fn a2ui_action(answers: Value) -> Message {
    let mut part = Part::data(json!([{"version": "v0.9.1", "action": {
        "name": "answer", "surfaceId": "ask-1", "sourceComponentId": "root",
        "timestamp": "2026-10-01T10:00:00Z", "context": {"answers": answers}}}]))
    .with_media_type(A2UI_MEDIA_TYPE);
    part.metadata = Some(HashMap::from([(
        "mimeType".to_owned(),
        json!(A2UI_MEDIA_TYPE),
    )]));
    Message::new(Role::User, vec![part])
}

#[tokio::test]
async fn a_messages_extensions_reach_the_run_as_its_context() {
    let rig = Rig::new();
    let metadata = json!({
        UI_CATALOG_EXTENSION: {"catalogId": CATALOG_ID, "version": 2.0, "digest": DIGEST, "inline": true},
        "a2uiClientCapabilities": {"v0.9.1": {
            "supportedCatalogIds": [CATALOG_ID],
            "inlineCatalogs": [{"catalogId": CATALOG_ID, "components": {"Text": {"maxLength": 4000.0}}}]}},
        THREAD_TOOLS_EXTENSION: {
            "url": "http://orchestrator/thread-tools/t1/mcp", "token": "a.b.c",
            "expiresAt": "2999-01-01T00:00:00Z"},
    });
    let message = with_metadata(
        Message::new(Role::User, vec![Part::text("choose")]),
        metadata,
    );
    let task = rig
        .backend
        .submit(alice(), message, None, Some("ctx".into()))
        .await
        .unwrap();
    let context = rig.state(&task).await.context;

    assert_eq!(
        context[CONTEXT_UI_REF],
        json!({"catalogId": CATALOG_ID, "version": 2, "digest": DIGEST})
    );
    assert_eq!(
        context[CONTEXT_UI_CATALOG]["catalog"]["components"]["Text"]["maxLength"],
        4000
    );
    assert!(context[CONTEXT_UI_CATALOG]["catalog"]["components"]["Text"]["maxLength"].is_i64());
    assert_eq!(context[CONTEXT_THREAD_TOOLS]["token"], "a.b.c");
}

/// The mentions of a message are the run's context, and the next job of the thread (a task that
/// continues this one) starts with the mentions of its own message, not these.
#[tokio::test]
async fn the_mentions_of_a_message_are_the_runs_context_and_the_next_task_does_not_inherit_them() {
    let rig = Rig::new();
    rig.model.push_text("ok").push_text("ok again");
    let (stop, handle) = rig.worker();
    let grant = json!({"url": "http://orchestrator/thread-tools/t1/mcp", "token": "a.b.c",
                       "expiresAt": "2999-01-01T00:00:00Z"});

    let first = with_metadata(
        Message::new(
            Role::User,
            vec![Part::text("first @researcher then @coder")],
        ),
        json!({
            THREAD_TOOLS_EXTENSION: grant,
            MENTIONS_EXTENSION: {
                "mentions": [
                    {"agentId": "mock-researcher", "name": "Mock researcher", "label": "@researcher",
                     "start": 6.0, "end": 17.0,
                     "cardUrl": "http://mock-researcher:8080/.well-known/agent-card.json"},
                    {"agentId": "mock-coder", "name": "Mock coder", "label": "@coder",
                     "start": 23.0, "end": 29.0}],
                "coordinate": {"tool": "ask_agent"}}}),
    );
    let one = rig
        .backend
        .submit(alice(), first, None, Some("ctx".into()))
        .await
        .unwrap();
    let context = rig.state(&one).await.context;
    assert_eq!(
        context[CONTEXT_MENTIONS]["coordinate"],
        json!({"tool": "ask_agent"})
    );
    let mentioned: Vec<(&str, i64)> = context[CONTEXT_MENTIONS]["mentions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["agentId"].as_str().unwrap(), m["start"].as_i64().unwrap()))
        .collect();
    assert_eq!(mentioned, [("mock-researcher", 6), ("mock-coder", 23)]);
    wait_task(&rig, &one.id, TaskState::Completed).await;

    // The next task of the conversation: its message names nobody.
    let mut second = with_metadata(
        Message::new(Role::User, vec![Part::text("thanks, and now?")]),
        json!({THREAD_TOOLS_EXTENSION: grant}),
    );
    second.reference_task_ids = Some(vec![one.id.clone()]);
    let two = rig
        .backend
        .submit(alice(), second, None, Some("ctx".into()))
        .await
        .unwrap();
    let context = rig.state(&two).await.context;
    assert!(
        context.get(CONTEXT_MENTIONS).is_none(),
        "the earlier message's mentions are not this one's: {context:?}"
    );
    assert_eq!(
        context[CONTEXT_THREAD_TOOLS]["token"], "a.b.c",
        "the rest is carried"
    );
    wait_task(&rig, &two.id, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn a_question_with_an_interface_is_input_required_with_text_and_a_2ui_part_and_the_answer_comes_back()
 {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "ask_user".into(),
            arguments: json!({}),
        }])
        .push_text("Going with Postgres.");
    let (stop, handle) = rig.worker();

    let task = rig
        .backend
        .submit(
            alice(),
            Message::new(Role::User, vec![Part::text("set it up")]),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let asked = wait_task(&rig, &task.id, TaskState::InputRequired).await;
    let status = asked.status.message.as_ref().expect("a status message");
    assert_eq!(status.parts.len(), 2);
    assert_eq!(status.text(), Some("Which database?"));
    let ui = &status.parts[1];
    assert_eq!(ui.content, PartContent::Data(surface()));
    assert_eq!(ui.media_type.as_deref(), Some(A2UI_MEDIA_TYPE));
    assert_eq!(
        ui.metadata.as_ref().and_then(|m| m.get("mimeType")),
        Some(&json!(A2UI_MEDIA_TYPE)),
        "both spellings of the media type"
    );

    // The id follows the status, not the read.
    let again = rig.backend.get(&alice(), &task.id).await.unwrap().unwrap();
    assert_eq!(
        again.status.message.as_ref().unwrap().message_id,
        status.message_id
    );
    // A stream that opens now is told the same status once, with the interface.
    let mut events = rig.backend.subscribe(&alice(), &task.id);
    let first = events.next().await.unwrap().unwrap();
    let TaskEvent::Snapshot(snapshot) = first else {
        panic!("the first frame is the snapshot: {first:?}");
    };
    assert_eq!(snapshot.status.message.as_ref().unwrap().parts.len(), 2);
    drop(events);

    // The person answers through the interface: an A2UI action on the same task.
    rig.backend
        .submit(
            alice(),
            a2ui_action(json!([{"id": "db", "values": ["pg"]}])),
            Some(task.id.clone()),
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let done = wait_task(&rig, &task.id, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(
        done.status.message.as_ref().and_then(Message::text),
        Some("Going with Postgres.")
    );
    // What the model read as the result of its question.
    let requests = rig.model.requests();
    assert_eq!(
        requests[1].messages.last(),
        Some(&ModelMessage::tool_result(
            "c1",
            "The person answered through the interface:\n- db: pg"
        ))
    );
}
