//! `access-tier/v1` on the A2A backend: a message that activated the extension names a tier the
//! agent honours or is refused before anything is written; the tier reaches the run as its context;
//! a caller that activated the extension is told the tier a task works under (the task, the first
//! frame of a stream, and a status update when it changes); one that did not reads none.
//!
//! A real `LlmAgent` on a real `Runtime` over `MemoryStore`, as in `steer.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    ACCESS_TIER_EXTENSION, AccessTier, AccessTiers, BackendError, Caller, STEER_EXTENSION,
    TaskBackend, TaskEvent,
};
use adam_a2a_runtime::{CONTEXT_ACCESS_TIER, RuntimeTaskBackend, task_id_for};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

/// A tool that waits until it is let through, so that a message arrives while the task works.
struct GateTool {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Tool for GateTool {
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
    /// `tiers`: what the backend declares; `None` declares nothing.
    fn new(tiers: Option<AccessTiers>) -> Self {
        let name = uniq("tier");
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let (reached, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let agent = LlmAgent::builder(&name, model, "m")
            .tool(GateTool {
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
        if let Some(tiers) = tiers {
            backend = backend.with_access_tiers(tiers);
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

fn honours_all() -> AccessTiers {
    AccessTiers::all(AccessTier::Auto)
}

fn tiered(who: &str) -> Caller {
    Caller::new(format!("token-{who}")).with_extensions([ACCESS_TIER_EXTENSION])
}

fn plain(who: &str) -> Caller {
    Caller::new(format!("token-{who}"))
}

fn naming(text: &str, id: &str, entry: Value) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m.metadata = Some(HashMap::from([(ACCESS_TIER_EXTENSION.to_owned(), entry)]));
    m
}

fn bare(text: &str, id: &str) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m
}

fn said(task: &Task) -> Option<Value> {
    task.metadata.as_ref()?.get(ACCESS_TIER_EXTENSION).cloned()
}

async fn wait_state(rig: &Rig, who: &Caller, id: &str, state: TaskState) -> Task {
    for _ in 0..2000 {
        let task = rig.backend.get(who, id).await.unwrap().unwrap();
        if task.status.state == state {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

#[tokio::test]
async fn a_tier_the_message_names_reaches_the_run_and_the_task_says_it() {
    let rig = Rig::new(Some(honours_all()));
    let who = uniq("alice");
    let task = rig
        .backend
        .submit(
            tiered(&who),
            naming("look around", "m-0", json!({"tier": "plan"})),
            None,
            None,
        )
        .await
        .unwrap();
    // The answer of SendMessage says it, the run's context holds it, and GetTask says it again.
    assert_eq!(said(&task), Some(json!({"tier": "plan"})));
    assert_eq!(
        rig.state_of(&task).await["context"][CONTEXT_ACCESS_TIER],
        json!({"tier": "plan"})
    );
    let got = rig
        .backend
        .get(&tiered(&who), &task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(said(&got), Some(json!({"tier": "plan"})));
    // The first frame of a stream is the task, and it says it.
    let mut events = rig.backend.subscribe(&tiered(&who), &task.id);
    let TaskEvent::Snapshot(first) = events.next().await.unwrap().unwrap() else {
        panic!("a stream begins with the task");
    };
    assert_eq!(said(&first), Some(json!({"tier": "plan"})));
    // A caller that did not activate the extension is told nothing of it.
    let got = rig
        .backend
        .get(&plain(&who), &task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(said(&got), None);
}

#[tokio::test]
async fn an_activated_message_with_no_valid_tier_is_refused_and_writes_nothing() {
    let rig = Rig::new(Some(
        AccessTiers::new([AccessTier::Plan, AccessTier::Edits], AccessTier::Plan).unwrap(),
    ));
    let who = uniq("bob");
    let cases = [
        (bare("x", "m-none"), "names no tier"),
        (naming("x", "m-str", json!("plan")), "`tier` is a string"),
        (naming("x", "m-empty", json!({})), "`tier` is a string"),
        (
            naming("x", "m-case", json!({"tier": "Plan"})),
            "is not an access tier",
        ),
        (
            naming("x", "m-word", json!({"tier": "root"})),
            "is not an access tier",
        ),
        // A tier the agent does not honour is refused, never rounded down.
        (
            naming("x", "m-high", json!({"tier": "bypass"})),
            "does not take the access tier bypass",
        ),
    ];
    for (message, wanted) in cases {
        let id = message.message_id.clone();
        let err = rig
            .backend
            .submit(tiered(&who), message, None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BackendError::InvalidParams(m) if m.contains(wanted)),
            "{id}: {err:?}"
        );
        // Nothing was written: the task the message would have been is not there.
        let run = task_id_for(&rig.name, &format!("token-{who}"), None, &id);
        assert!(rig.runtime.view(run).await.unwrap().is_none(), "{id}");
    }
}

#[tokio::test]
async fn a_message_that_did_not_activate_the_extension_carries_no_tier() {
    let rig = Rig::new(Some(honours_all()));
    let who = uniq("carol");
    // It names `bypass`, and the request did not activate the extension: nothing is read.
    let task = rig
        .backend
        .submit(
            plain(&who),
            naming("go", "m-0", json!({"tier": "bypass"})),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(said(&task), None);
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_ACCESS_TIER)
            .is_none()
    );
    // An activated caller reading that task is told what the run works under: the default.
    let got = rig
        .backend
        .get(&tiered(&who), &task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(said(&got), Some(json!({"tier": "auto"})));
}

#[tokio::test]
async fn a_backend_that_declares_no_tiers_neither_reads_nor_says_one() {
    let rig = Rig::new(None);
    let who = uniq("dan");
    // The caller names the extension, a card that does not declare it would not have activated it:
    // the backend still never reads or says a tier.
    let task = rig
        .backend
        .submit(
            tiered(&who),
            naming("go", "m-0", json!({"tier": "plan"})),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(said(&task), None);
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_ACCESS_TIER)
            .is_none()
    );
}

/// A message sent to the working task changes the tier from its next step, and the stream says so
/// once: the first frame said the tier the task began under.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tier_that_changes_while_the_task_works_is_said_on_the_stream() {
    let rig = Rig::new(Some(honours_all()));
    rig.mock
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "gate".into(),
            arguments: json!({}),
        }])
        .push_text("done");
    let who = uniq("erin");
    let (stop, handle) = rig.worker();
    let activated = Caller::new(format!("token-{who}"))
        .with_extensions([ACCESS_TIER_EXTENSION, STEER_EXTENSION]);

    let task = rig
        .backend
        .submit(
            activated.clone(),
            naming("look", "m-0", json!({"tier": "plan"})),
            None,
            None,
        )
        .await
        .unwrap();
    let mut events = rig.backend.subscribe(&activated, &task.id);
    let TaskEvent::Snapshot(first) = events.next().await.unwrap().unwrap() else {
        panic!("a stream begins with the task");
    };
    assert_eq!(said(&first), Some(json!({"tier": "plan"})));

    tokio::time::timeout(Duration::from_secs(20), rig.reached.notified())
        .await
        .expect("the tool is reached");
    let answered = rig
        .backend
        .submit(
            activated.clone(),
            naming("now edit", "m-1", json!({"tier": "edits"})),
            Some(task.id.clone()),
            Some(task.context_id.clone()),
        )
        .await
        .unwrap();
    // The tier in force does not change in the middle of the call that is running.
    assert_eq!(said(&answered), Some(json!({"tier": "plan"})));
    rig.release.notify_one();

    let mut told: Vec<Value> = Vec::new();
    while let Ok(Some(Ok(event))) =
        tokio::time::timeout(Duration::from_secs(20), events.next()).await
    {
        if let TaskEvent::Status(update) = &event
            && let Some(entry) = update
                .metadata
                .as_ref()
                .and_then(|m| m.get(ACCESS_TIER_EXTENSION))
        {
            told.push(entry.clone());
        }
        if event.ends_stream() {
            break;
        }
    }
    assert_eq!(
        told,
        [json!({"tier": "edits"})],
        "said once, when it changed"
    );
    let done = wait_state(&rig, &activated, &task.id, TaskState::Completed).await;
    assert_eq!(said(&done), Some(json!({"tier": "edits"})));
    let _ = stop.send(());
    handle.await.unwrap();
}
