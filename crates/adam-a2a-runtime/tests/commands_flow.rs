//! `commands/v1` beside the other message paths: a typed reply to a pending question, a steering
//! message and a form's answer, with `vymalo_inbound` as a screen's inbound function.
//!
//! The text form (`/name`) is read on every message that is typed text, wherever it lands, so a
//! reply to a question or a steering message that begins `/name` is expanded (`//name` is the
//! escape). A message that carries a form's answer is the person's click and is never a command.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    A2UI_MEDIA_TYPE, BackendError, COMMANDS_EXTENSION, Caller, STEER_EXTENSION, TaskBackend,
};
use adam_a2a_runtime::{Commands, RuntimeTaskBackend, vymalo_inbound};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{Message as ModelMessage, MockModel, ToolCall, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";

fn surface() -> Value {
    json!([
        {"version": "1.0", "createSurface": {"surfaceId": "ask-1", "catalogId": CATALOG_ID}},
        {"version": "1.0", "updateComponents": {"surfaceId": "ask-1", "components": [
            {"id": "root", "component": "Choices", "questions": [
                {"id": "db", "question": "Which database?", "options": [
                    {"value": "pg", "label": "Postgres"}, {"value": "sqlite", "label": "SQLite"}]}]}]}}
    ])
}

struct Ask;

#[async_trait]
impl Tool for Ask {
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

#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

struct Wait(Arc<Gate>);

#[async_trait]
impl Tool for Wait {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "gate".into(),
            description: "waits".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.0.reached.notify_one();
        self.0.release.notified().await;
        Ok(ToolOutput::text("released"))
    }
}

struct Rig {
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    model: Arc<MockModel>,
    gate: Arc<Gate>,
}

impl Rig {
    fn new() -> Self {
        let store: DynStore = Arc::new(MemoryStore::new());
        let model = Arc::new(MockModel::new());
        let gate = Arc::new(Gate::default());
        let agent = LlmAgent::builder("flow", model.clone(), "m")
            .tool(Ask)
            .tool(Wait(gate.clone()))
            .build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, "flow")
            .with_poll_interval(Duration::from_millis(10))
            .with_inbound(vymalo_inbound)
            .with_commands(Commands::new([("echo", "[$ARGUMENTS]")]));
        Self {
            runtime,
            backend,
            model,
            gate,
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

    fn call(&self, name: &str) {
        self.model.push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({}),
            ..Default::default()
        }]);
    }

    async fn wait(&self, who: &Caller, task: &Task, state: TaskState) {
        for _ in 0..1000 {
            let got = self.backend.get(who, &task.id).await.unwrap().unwrap();
            if got.status.state == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {} never reached {state:?}", task.id);
    }

    /// The task is asking "Which database?".
    async fn asking(&self, who: &Caller) -> Task {
        self.call("ask_user");
        let task = self
            .backend
            .submit(
                who.clone(),
                Message::new(Role::User, vec![Part::text("set it up")]),
                None,
                Some("ctx".into()),
            )
            .await
            .unwrap();
        self.wait(who, &task, TaskState::InputRequired).await;
        task
    }

    async fn run_state(&self, task: &Task) -> String {
        let run = RunId(task.id.parse().unwrap());
        format!("{:?}", self.runtime.view(run).await.unwrap().unwrap().state)
    }
}

fn alice() -> Caller {
    Caller::new("token-alice")
}

fn answer_action() -> Part {
    let mut part = Part::data(json!([{"version": "1.0", "action": {
        "name": "answer", "surfaceId": "ask-1", "sourceComponentId": "root",
        "timestamp": "2026-10-01T10:00:00Z",
        "context": {"answers": [{"id": "db", "values": ["pg"]}]}}}]))
    .with_media_type(A2UI_MEDIA_TYPE);
    part.metadata = Some(HashMap::from([(
        "mimeType".to_owned(),
        json!(A2UI_MEDIA_TYPE),
    )]));
    part
}

fn tool_result_after_ask(model: &MockModel) -> String {
    format!("{:?}", model.requests()[1].messages.last().unwrap())
}

/// A typed reply to a pending question that begins `/name` is a command: the model reads the
/// expansion as the answer.
#[tokio::test]
async fn a_typed_reply_to_a_question_that_starts_with_a_command_expands() {
    let rig = Rig::new();
    let (stop, handle) = rig.worker();
    let task = rig.asking(&alice()).await;
    rig.model.push_text("done");
    rig.backend
        .submit(
            alice(),
            Message::new(Role::User, vec![Part::text("/echo pg")]),
            Some(task.id.clone()),
            Some("ctx".into()),
        )
        .await
        .unwrap();
    rig.wait(&alice(), &task, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    let read = tool_result_after_ask(&rig.model);
    assert!(read.contains("[pg]"), "{read}");
    assert!(!read.contains("/echo"), "{read}");
}

/// The escape: `//echo` is the text `/echo`.
#[tokio::test]
async fn a_doubled_slash_in_a_reply_is_the_literal_text() {
    let rig = Rig::new();
    let (stop, handle) = rig.worker();
    let task = rig.asking(&alice()).await;
    rig.model.push_text("done");
    rig.backend
        .submit(
            alice(),
            Message::new(Role::User, vec![Part::text("//echo pg")]),
            Some(task.id.clone()),
            Some("ctx".into()),
        )
        .await
        .unwrap();
    rig.wait(&alice(), &task, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    let read = tool_result_after_ask(&rig.model);
    assert!(
        read.contains("/echo pg") && !read.contains("[pg]"),
        "{read}"
    );
}

/// A steering message to a working task is read at its next step, and a command in it is expanded.
#[tokio::test]
async fn a_steering_message_that_starts_with_a_command_expands() {
    let rig = Rig::new();
    rig.call("gate");
    rig.model.push_text("done");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            alice(),
            Message::new(Role::User, vec![Part::text("go")]),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), rig.gate.reached.notified())
        .await
        .expect("the tool is reached");
    rig.backend
        .submit(
            alice().with_extensions([STEER_EXTENSION]),
            Message::new(Role::User, vec![Part::text("/echo steer me")]),
            Some(task.id.clone()),
            Some("ctx".into()),
        )
        .await
        .unwrap();
    rig.gate.release.notify_one();
    rig.wait(&alice(), &task, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    let steered = ModelMessage::user_text("[steer me]");
    assert!(
        rig.model.requests()[1].messages.contains(&steered),
        "{:?}",
        rig.model.requests()[1].messages
    );
}

/// A form's answer is never a command: the text beside it is not read as `/name`, and the answer
/// is stored as it is.
#[tokio::test]
async fn a_message_that_answers_a_form_is_not_read_as_a_command() {
    let rig = Rig::new();
    let (stop, handle) = rig.worker();
    let task = rig.asking(&alice()).await;
    rig.model.push_text("done");
    rig.backend
        .submit(
            alice(),
            Message::new(
                Role::User,
                vec![Part::text("/echo sneaky"), answer_action()],
            ),
            Some(task.id.clone()),
            Some("ctx".into()),
        )
        .await
        .unwrap();
    rig.wait(&alice(), &task, TaskState::Completed).await;
    stop.send(()).unwrap();
    handle.await.unwrap();
    let read = tool_result_after_ask(&rig.model);
    assert!(read.contains("/echo sneaky"), "{read}");
    assert!(!read.contains("[sneaky]"), "{read}");
    assert!(
        read.contains("The person answered through the interface"),
        "{read}"
    );
}

/// A command named in the metadata beside a form's answer is refused, and nothing is stored.
#[tokio::test]
async fn a_command_in_the_metadata_of_a_form_answer_is_invalid_params() {
    let rig = Rig::new();
    let (stop, handle) = rig.worker();
    let task = rig.asking(&alice()).await;
    let before = rig.run_state(&task).await;
    let mut message = Message::new(Role::User, vec![answer_action()]);
    message.metadata = Some(HashMap::from([(
        COMMANDS_EXTENSION.to_owned(),
        json!({"command": "echo", "arguments": "x"}),
    )]));
    let err = rig
        .backend
        .submit(
            alice().with_extensions([COMMANDS_EXTENSION]),
            message,
            Some(task.id.clone()),
            Some("ctx".into()),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BackendError::InvalidParams(m) if m.contains("answers a form")),
        "{err:?}"
    );
    assert_eq!(rig.run_state(&task).await, before);
    let still = rig.backend.get(&alice(), &task.id).await.unwrap().unwrap();
    assert_eq!(still.status.state, TaskState::InputRequired);
    stop.send(()).unwrap();
    handle.await.unwrap();
}
