//! `agent-mode/v1` on the A2A backend: a message that activated the extension names a mode the
//! agent declares, it reaches the run as its context and holds until another message names another;
//! a mode the agent does not declare is refused as `mode_not_offered` before anything is written; a
//! message whose request did not activate the extension, or a backend that declares none, carries
//! no mode.
//!
//! A real `LlmAgent` on a real `Runtime` over `MemoryStore`, with `MockModel` recording every
//! request, as in `model_pick.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    AGENT_MODE_EXTENSION, AgentModes, BackendError, Caller, MODE_NOT_OFFERED, ModeInfo,
    STEER_EXTENSION, TaskBackend,
};
use adam_a2a_runtime::{CONTEXT_MODE, RuntimeTaskBackend, task_id_for};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

/// A tool that waits until it is let through, so a steer reaches the task while it works: what the
/// "the mode holds" test needs.
struct Gate {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Tool for Gate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "gate".into(),
            description: "waits".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.reached.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::text("released"))
    }
}

struct Rig {
    name: String,
    mock: Arc<MockModel>,
    reached: Arc<Notify>,
    release: Arc<Notify>,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

impl Rig {
    /// `modes`: what the backend declares; `None` is a card that lists no `agent-mode/v1`.
    fn new(modes: Option<AgentModes>) -> Self {
        let name = uniq("mode");
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let (reached, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let agent = LlmAgent::builder(&name, model, "m")
            .tool(Gate {
                reached: reached.clone(),
                release: release.clone(),
            })
            .build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let mut backend = RuntimeTaskBackend::new(runtime.clone(), events, name.clone())
            .with_poll_interval(Duration::from_millis(10));
        if let Some(modes) = modes {
            backend = backend.with_agent_modes(modes);
        }
        Self {
            name,
            mock,
            reached,
            release,
            runtime,
            backend,
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

    async fn state_of(&self, task: &Task) -> Value {
        let run = RunId(task.id.parse().unwrap());
        self.runtime.view(run).await.unwrap().unwrap().state
    }
}

fn declared() -> AgentModes {
    AgentModes::new(
        [
            ModeInfo::new("build", "Build", "Changes things."),
            ModeInfo::new("plan", "Plan", "Reads and proposes."),
        ],
        "build",
    )
    .expect("declared")
}

fn picking(who: &str) -> Caller {
    Caller::new(format!("token-{who}")).with_extensions([AGENT_MODE_EXTENSION])
}

fn plain(who: &str) -> Caller {
    Caller::new(format!("token-{who}"))
}

fn naming(text: &str, id: &str, entry: Value) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m.metadata = Some(HashMap::from([(AGENT_MODE_EXTENSION.to_owned(), entry)]));
    m
}

fn bare(text: &str, id: &str) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m
}

async fn wait_state(rig: &Rig, who: &Caller, id: &str, state: TaskState) -> Task {
    let mut last = None;
    for _ in 0..2000 {
        let task = rig.backend.get(who, id).await.unwrap().unwrap();
        if task.status.state == state {
            return task;
        }
        last = Some(task);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}: {last:#?}");
}

#[tokio::test]
async fn a_declared_mode_reaches_the_run_as_its_context() {
    let rig = Rig::new(Some(declared()));
    rig.mock.push_text("done");
    let who = uniq("alice");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            picking(&who),
            naming("look around", "m-0", json!({"mode": "plan"})),
            None,
            None,
        )
        .await
        .unwrap();
    // The mode is the run's context from the start.
    assert_eq!(
        rig.state_of(&task).await["context"][CONTEXT_MODE],
        json!({"mode": "plan"})
    );
    wait_state(&rig, &picking(&who), &task.id, TaskState::Completed).await;
    stop.send(()).ok();
    handle.await.ok();
}

#[tokio::test]
async fn the_mode_holds_until_another_message_names_another() {
    let rig = Rig::new(Some(declared()));
    // The first answer calls a tool that waits, so a steer reaches the task while it works.
    rig.mock.push_tool_calls(vec![ToolCall {
        id: "c-1".into(),
        name: "gate".into(),
        arguments: json!({}),
        ..Default::default()
    }]);
    rig.mock.push_text("done");
    let who = uniq("bob");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            picking(&who),
            naming("start", "m-0", json!({"mode": "plan"})),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.state_of(&task).await["context"][CONTEXT_MODE],
        json!({"mode": "plan"})
    );
    tokio::time::timeout(Duration::from_secs(10), rig.reached.notified())
        .await
        .expect("the tool was reached");
    // A message that names another mode while the task works, as a steer: the run keeps it from its
    // next step.
    let steering = Caller::new(format!("token-{who}"))
        .with_extensions([AGENT_MODE_EXTENSION, STEER_EXTENSION]);
    rig.backend
        .submit(
            steering,
            naming("now build", "m-1", json!({"mode": "build"})),
            Some(task.id.clone()),
            None,
        )
        .await
        .expect("the steer is delivered");
    rig.release.notify_one();
    let done = wait_state(&rig, &picking(&who), &task.id, TaskState::Completed).await;
    assert_eq!(
        rig.state_of(&done).await["context"][CONTEXT_MODE],
        json!({"mode": "build"}),
        "the newest message that names one wins"
    );
    stop.send(()).ok();
    handle.await.ok();
}

#[tokio::test]
async fn a_mode_the_agent_does_not_declare_is_refused_and_writes_nothing() {
    let rig = Rig::new(Some(declared()));
    let who = uniq("carol");
    let cases = [
        (
            naming("x", "m-other", json!({"mode": "review"})),
            "does not declare",
        ),
        (
            naming("x", "m-case", json!({"mode": "Plan"})),
            "does not declare",
        ),
        (bare("x", "m-none"), "names no mode"),
        (naming("x", "m-str", json!("plan")), "not an object"),
        (naming("x", "m-empty", json!({})), "no string"),
        (naming("x", "m-num", json!({"mode": 3})), "no string"),
    ];
    for (message, wanted) in cases {
        let id = message.message_id.clone();
        let err = rig
            .backend
            .submit(picking(&who), message, None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BackendError::InvalidParams(m)
                if m.starts_with(MODE_NOT_OFFERED)
                    && m.contains(wanted)
                    && m.contains("build, plan")),
            "{id}: {err:?}"
        );
        // Nothing was written: the task the message would have been is not there.
        let run = task_id_for(&rig.name, &format!("token-{who}"), None, &id);
        assert!(rig.runtime.view(run).await.unwrap().is_none(), "{id}");
    }
}

#[tokio::test]
async fn a_message_that_did_not_activate_the_extension_carries_no_mode() {
    let rig = Rig::new(Some(declared()));
    rig.mock.push_text("done");
    let who = uniq("dan");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            plain(&who),
            naming("go", "m-0", json!({"mode": "plan"})),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_MODE)
            .is_none()
    );
    wait_state(&rig, &plain(&who), &task.id, TaskState::Completed).await;
    stop.send(()).ok();
    handle.await.ok();
}

#[tokio::test]
async fn a_backend_that_declares_no_mode_reads_none() {
    // A card that lists no `agent-mode/v1` never reads one, whatever the request says.
    let rig = Rig::new(None);
    rig.mock.push_text("done");
    let who = uniq("erin");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            picking(&who),
            naming("go", "m-0", json!({"mode": "plan"})),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_MODE)
            .is_none()
    );
    wait_state(&rig, &picking(&who), &task.id, TaskState::Completed).await;
    stop.send(()).ok();
    handle.await.ok();
}
