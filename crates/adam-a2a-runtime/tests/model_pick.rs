//! `model-pick/v1` on the A2A backend: a message that activated the extension names a model the
//! card offers, it reaches the run as its context and the model request is made to it; a model the
//! agent does not offer is refused as `model_not_offered` before anything is written; a message
//! whose request did not activate the extension, or a backend that lists none, carries no pick.
//!
//! A real `LlmAgent` on a real `Runtime` over `MemoryStore`, with `MockModel` recording every
//! request, as in `access_tier.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    BackendError, Caller, MODEL_NOT_OFFERED, MODEL_PICK_EXTENSION, ModelChoices, TaskBackend,
};
use adam_a2a_runtime::{CONTEXT_MODEL, RuntimeTaskBackend, task_id_for};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{LlmAgent, ModelBinding};
use adam_model::{DynModel, MockModel};
use adam_runtime::{BroadcastSink, Runtime};
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

/// The agent's own model is `own`; `alias-a` and `alias-b` are what the deployment offers it.
const OWN: &str = "own";
const A: &str = "alias-a";
const B: &str = "alias-b";

struct Rig {
    name: String,
    mock: Arc<MockModel>,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

impl Rig {
    /// `choices`: what the backend offers; `None` is a card that lists no `model-pick/v1`.
    fn new(choices: Option<ModelChoices>) -> Self {
        let name = uniq("pick");
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = LlmAgent::builder(&name, model, OWN)
            .model_choices(vec![ModelBinding::new(A, 4096), ModelBinding::new(B, 2048)])
            .build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let mut backend = RuntimeTaskBackend::new(runtime.clone(), events, name.clone())
            .with_poll_interval(Duration::from_millis(10));
        if let Some(choices) = choices {
            backend = backend.with_model_choices(choices);
        }
        Self {
            name,
            mock,
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

fn all_choices() -> ModelChoices {
    ModelChoices::from_aliases([A, B])
}

fn picking(who: &str) -> Caller {
    Caller::new(format!("token-{who}")).with_extensions([MODEL_PICK_EXTENSION])
}

fn plain(who: &str) -> Caller {
    Caller::new(format!("token-{who}"))
}

fn naming(text: &str, id: &str, entry: Value) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m.metadata = Some(HashMap::from([(MODEL_PICK_EXTENSION.to_owned(), entry)]));
    m
}

fn bare(text: &str, id: &str) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m
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
async fn a_listed_pick_reaches_the_model_request() {
    let rig = Rig::new(Some(all_choices()));
    rig.mock.push_text("done");
    let who = uniq("alice");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            picking(&who),
            naming("go", "m-0", json!({"model": A})),
            None,
            None,
        )
        .await
        .unwrap();
    // The pick is the run's context from the start.
    assert_eq!(
        rig.state_of(&task).await["context"][CONTEXT_MODEL],
        json!({"model": A})
    );
    wait_state(&rig, &picking(&who), &task.id, TaskState::Completed).await;
    let requests = rig.mock.requests();
    let request = requests.last().expect("a model request");
    assert_eq!(request.model, A);
    // The binding's own cap, not the agent's: the run took the picked model, not only its name.
    assert_eq!(request.max_output_tokens, Some(4096));
    stop.send(()).ok();
    handle.await.ok();
}

#[tokio::test]
async fn an_unlisted_pick_is_refused_and_writes_nothing() {
    let rig = Rig::new(Some(all_choices()));
    let who = uniq("bob");
    let cases = [
        (
            naming("x", "m-other", json!({"model": "alias-c"})),
            "does not offer",
        ),
        (
            naming("x", "m-case", json!({"model": "Alias-A"})),
            "does not offer",
        ),
        (bare("x", "m-none"), "names no model"),
        (naming("x", "m-str", json!("alias-a")), "not an object"),
        (naming("x", "m-empty", json!({})), "no string"),
        (naming("x", "m-num", json!({"model": 3})), "no string"),
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
                if m.starts_with(MODEL_NOT_OFFERED)
                    && m.contains(wanted)
                    && m.contains("alias-a, alias-b")),
            "{id}: {err:?}"
        );
        // Nothing was written: the task the message would have been is not there.
        let run = task_id_for(&rig.name, &format!("token-{who}"), None, &id);
        assert!(rig.runtime.view(run).await.unwrap().is_none(), "{id}");
    }
}

#[tokio::test]
async fn a_card_without_the_extension_leaves_the_pick_unread() {
    // A backend that lists no choices never reads one, whatever the request says.
    let rig = Rig::new(None);
    rig.mock.push_text("done");
    let who = uniq("carol");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            picking(&who),
            naming("go", "m-0", json!({"model": A})),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_MODEL)
            .is_none()
    );
    wait_state(&rig, &picking(&who), &task.id, TaskState::Completed).await;
    assert_eq!(rig.mock.last_request().expect("a model request").model, OWN);
    stop.send(()).ok();
    handle.await.ok();
}

#[tokio::test]
async fn a_message_that_did_not_activate_the_extension_carries_no_pick() {
    let rig = Rig::new(Some(all_choices()));
    rig.mock.push_text("done");
    let who = uniq("dan");
    let (stop, handle) = rig.worker();
    // It names `alias-a`, and the request did not activate the extension: nothing is read.
    let task = rig
        .backend
        .submit(
            plain(&who),
            naming("go", "m-0", json!({"model": A})),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_MODEL)
            .is_none()
    );
    wait_state(&rig, &plain(&who), &task.id, TaskState::Completed).await;
    assert_eq!(rig.mock.last_request().expect("a model request").model, OWN);
    stop.send(()).ok();
    handle.await.ok();
}
