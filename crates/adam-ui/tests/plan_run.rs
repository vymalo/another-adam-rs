//! The plan tools of `adam-ui` driven by a real `Runtime` and `LlmAgent` behind the A2A backend, on
//! a screen whose catalog has a `Checklist`, with a scripted model: what the task holds (one plan
//! artifact, however often the plan changed), what a continued task draws on (the same surface),
//! and what the model is sent (the system prompt and the tools never change, each request is the
//! one before and what happened since).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, Task, TaskState};
use adam_a2a::{Caller, TaskBackend, UI_CATALOG_EXTENSION};
use adam_a2a_runtime::{RuntimeTaskBackend, vymalo_inbound};
use adam_core::{DynStore, MemoryStore};
use adam_llm_agent::LlmAgent;
use adam_mcp::McpPolicy;
use adam_model::{MockModel, ModelRequest, ToolCall};
use adam_runtime::{BroadcastSink, Runtime};
use adam_ui::Ui;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const CATALOG_ID: &str = "https://agents.vymalo.com/a2ui/catalogs/chat";
const FIXTURE: &str = include_str!("fixtures/catalog-v6.json");
const LOCK: &str = include_str!("fixtures/catalog-v6.lock.json");

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

/// A message from a screen whose catalog (inline, numbers as doubles) has a `Checklist`.
fn from_the_screen(text: &str, references: &[&str]) -> Message {
    let lock: Value = serde_json::from_str(LOCK).unwrap();
    let document: Value = serde_json::from_str(FIXTURE).unwrap();
    let metadata = json!({
        UI_CATALOG_EXTENSION: {
            "catalogId": CATALOG_ID, "version": lock["version"].as_f64().unwrap(),
            "digest": lock["digest"], "inline": true},
        "a2uiClientCapabilities": {"v0.9.1": {
            "supportedCatalogIds": [CATALOG_ID],
            "inlineCatalogs": [as_doubles(&document)]}},
    });
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.metadata = Some(
        metadata
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    if !references.is_empty() {
        message.reference_task_ids = Some(references.iter().map(|r| (*r).to_owned()).collect());
    }
    message
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
        let ui = Ui::new(McpPolicy::default());
        let agent = LlmAgent::builder("planner", model.clone(), "m")
            .instructions("You are a test agent.")
            .tools(ui.tools())
            .build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, "planner")
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

    async fn wait(&self, task: &Task) -> Task {
        for _ in 0..1000 {
            let got = self.backend.get(&alice(), &task.id).await.unwrap().unwrap();
            if got.status.state == TaskState::Completed {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {} never completed", task.id);
    }
}

fn alice() -> Caller {
    Caller::new("token-0")
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// The plan artifacts of a task: the A2UI ones whose id starts with `plan-`, as `(artifact id,
/// surface id, item states)`.
fn plans_of(task: &Task) -> Vec<(String, String, Vec<String>)> {
    task.artifacts
        .iter()
        .flatten()
        .filter(|a| a.artifact_id.starts_with("plan-"))
        .map(|a| {
            let PartContent::Data(messages) = &a.parts[0].content else {
                panic!("a plan is data: {a:?}");
            };
            let surface = messages[0]["createSurface"]["surfaceId"]
                .as_str()
                .unwrap()
                .to_owned();
            let states = messages[1]["updateComponents"]["components"][0]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["state"].as_str().unwrap().to_owned())
                .collect();
            (a.artifact_id.clone(), surface, states)
        })
        .collect()
}

fn assert_prefix_holds(requests: &[ModelRequest]) {
    for pair in requests.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert_eq!(
            serde_json::to_string(&a.system).unwrap(),
            serde_json::to_string(&b.system).unwrap(),
            "the system prompt is the same on every request"
        );
        assert_eq!(
            serde_json::to_string(&a.tools).unwrap(),
            serde_json::to_string(&b.tools).unwrap(),
            "the tool specs are the same, in the same order"
        );
        assert!(b.messages.len() > a.messages.len());
        assert_eq!(
            serde_json::to_string(&a.messages).unwrap(),
            serde_json::to_string(&b.messages[..a.messages.len()]).unwrap(),
            "a request is the previous one plus what happened since"
        );
    }
}

#[tokio::test]
async fn the_real_plan_tools_leave_one_artifact_a_continued_task_draws_on_the_same_surface_and_the_prefix_holds()
 {
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![call(
            "c1",
            "plan_work",
            json!({"title": "Ship it", "items": ["a", "b", "c"]}),
        )])
        .push_tool_calls(vec![call(
            "c2",
            "plan_check",
            json!({"id": 1, "status": "done"}),
        )])
        .push_tool_calls(vec![
            call("c3", "plan_check", json!({"id": 2, "status": "doing"})),
            call("c4", "plan_add", json!({"items": ["d"]})),
        ])
        .push_text("first done")
        .push_tool_calls(vec![call(
            "c5",
            "plan_check",
            json!({"id": 3, "status": "doing"}),
        )])
        .push_text("second done");
    let (stop, handle) = rig.worker();

    let first = rig
        .backend
        .submit(
            alice(),
            from_the_screen("plan it", &[]),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let first = rig.wait(&first).await;
    // Four plan calls, five states of the plan: one artifact, the newest.
    let plans = plans_of(&first);
    assert_eq!(plans.len(), 1, "{plans:?}");
    let (artifact_id, surface_id, states) = plans[0].clone();
    assert_eq!(
        artifact_id, surface_id,
        "the artifact is named for its surface"
    );
    assert_eq!(states, ["done", "doing", "todo", "todo"]);
    let run = adam_core::RunId(first.id.parse().unwrap());
    let view = rig.runtime.view(run).await.unwrap().unwrap();
    assert_eq!(
        view.artifacts
            .iter()
            .filter(|a| a.id.as_deref() == Some(artifact_id.as_str()))
            .count(),
        1,
        "one in the run's own record too"
    );
    let output = view.output.unwrap();
    assert_eq!(output["artifacts"].as_array().unwrap().len(), 1, "{output}");

    // The task that continues it draws on the same surface, so the person's screen updates the
    // plan it has.
    let second = rig
        .backend
        .submit(
            alice(),
            from_the_screen("go on", &[first.id.as_str()]),
            None,
            Some("ctx".into()),
        )
        .await
        .unwrap();
    let second = rig.wait(&second).await;
    let plans = plans_of(&second);
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert_eq!(
        plans[0].0, artifact_id,
        "the same surface as the task before"
    );
    assert_eq!(plans[0].2, ["done", "doing", "doing", "todo"]);
    // Another conversation has another surface.
    stop.send(()).unwrap();
    handle.await.unwrap();

    let requests = rig.model.requests();
    assert_eq!(requests.len(), 6);
    assert_prefix_holds(&requests[..4]);
    for request in &requests {
        let tools: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tools.contains(&"plan_work"), "{tools:?}");
        let system = request.system.clone().unwrap_or_default();
        assert!(!system.contains("Ship it"), "no plan in the system prompt");
    }
}

#[tokio::test]
async fn another_conversation_draws_on_another_surface() {
    let rig = Rig::new();
    for _ in 0..2 {
        rig.model
            .push_tool_calls(vec![call("c1", "plan_work", json!({"items": ["a"]}))])
            .push_text("done");
    }
    let (stop, handle) = rig.worker();
    let mut surfaces = Vec::new();
    for context in ["ctx-a", "ctx-b"] {
        let task = rig
            .backend
            .submit(
                alice(),
                from_the_screen("plan", &[]),
                None,
                Some(context.into()),
            )
            .await
            .unwrap();
        surfaces.push(plans_of(&rig.wait(&task).await)[0].0.clone());
    }
    stop.send(()).unwrap();
    handle.await.unwrap();
    assert_ne!(surfaces[0], surfaces[1], "{surfaces:?}");
}
