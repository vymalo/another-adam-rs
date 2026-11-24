//! `reasoning/v1` on the A2A backend: a message that activated the extension names an effort the
//! model of its run takes, it reaches the run as its context and the model request asks for it; an
//! effort the agent does not take is refused before anything is written; a message whose request did
//! not activate the extension, or a backend that lists no level, carries none.
//!
//! The last test carries the level all the way through the real client: the request the run makes
//! goes out through `adam-model-openai` to a mock server, and the body it receives asks for the
//! effort the message named (ADR 0063, decisions 5 and 6).
//!
//! A real `LlmAgent` on a real `Runtime` over `MemoryStore`, as in `model_pick.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{BackendError, Caller, REASONING_EXTENSION, ReasoningEfforts, TaskBackend as _};
use adam_a2a_runtime::{CONTEXT_REASONING_EFFORT, RuntimeTaskBackend, task_id_for};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::LlmAgent;
use adam_model::{DynModel, MockModel, ReasoningEffort};
use adam_runtime::{BroadcastSink, Runtime};
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// The agent's own model alias.
const OWN: &str = "own";

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

/// The levels the root's model takes in these tests, and its resolved default.
fn taking() -> (Vec<ReasoningEffort>, Option<ReasoningEffort>) {
    (
        vec![ReasoningEffort::Low, ReasoningEffort::High],
        Some(ReasoningEffort::Low),
    )
}

struct Rig {
    name: String,
    mock: Arc<MockModel>,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

impl Rig {
    /// `levels`: the root model's levels and default; `None` is a card that lists no
    /// `reasoning/v1`.
    fn new(levels: Option<(Vec<ReasoningEffort>, Option<ReasoningEffort>)>) -> Self {
        let name = uniq("reason");
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = LlmAgent::builder(&name, model, OWN).build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let mut backend = RuntimeTaskBackend::new(runtime.clone(), events, name.clone())
            .with_poll_interval(Duration::from_millis(10));
        if let Some((levels, default)) = levels {
            backend = backend.with_reasoning_efforts(ReasoningEfforts::new().with_agent(
                name.clone(),
                levels,
                default,
            ));
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

fn asking(who: &str) -> Caller {
    Caller::new(format!("token-{who}")).with_extensions([REASONING_EXTENSION])
}

fn plain(who: &str) -> Caller {
    Caller::new(format!("token-{who}"))
}

fn naming(text: &str, id: &str, entry: Value) -> Message {
    let mut m = Message::new(Role::User, vec![Part::text(text)]);
    m.message_id = id.into();
    m.metadata = Some(HashMap::from([(REASONING_EXTENSION.to_owned(), entry)]));
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
async fn a_level_the_agent_takes_reaches_the_run_and_its_model_request() {
    let rig = Rig::new(Some(taking()));
    rig.mock.push_text("done");
    let who = uniq("alice");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            asking(&who),
            naming("go", "m-0", json!({"effort": "high"})),
            None,
            None,
        )
        .await
        .unwrap();
    // The level is the run's context from the start, in the shape the loop reads.
    assert_eq!(
        rig.state_of(&task).await["context"][CONTEXT_REASONING_EFFORT],
        json!({"effort": "high"})
    );
    wait_state(&rig, &asking(&who), &task.id, TaskState::Completed).await;
    assert_eq!(
        rig.mock
            .last_request()
            .expect("a model request")
            .reasoning_effort,
        Some(ReasoningEffort::High),
        "the run's level wins over the model's own default"
    );
    stop.send(()).ok();
    handle.await.ok();
}

#[tokio::test]
async fn a_level_the_agent_does_not_take_is_refused_and_writes_nothing() {
    let rig = Rig::new(Some(taking()));
    let who = uniq("bob");
    let cases = [
        (bare("x", "m-none"), "carries no"),
        (naming("x", "m-str", json!("high")), "not an object"),
        (naming("x", "m-empty", json!({})), "no string"),
        (naming("x", "m-num", json!({"effort": 3})), "no string"),
        (
            naming("x", "m-word", json!({"effort": "ultra"})),
            "not one of",
        ),
        // The seven words, but not among this model's levels.
        (
            naming("x", "m-max", json!({"effort": "max"})),
            "does not take",
        ),
        (
            naming("x", "m-none-level", json!({"effort": "none"})),
            "does not take",
        ),
    ];
    for (message, wanted) in cases {
        let id = message.message_id.clone();
        let err = rig
            .backend
            .submit(asking(&who), message, None, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BackendError::InvalidParams(m)
                if m.contains(&wanted) && m.contains("low, high")),
            "{id}: {err:?}"
        );
        // Nothing was written: the task the message would have been is not there.
        let run = task_id_for(&rig.name, &format!("token-{who}"), None, &id);
        assert!(rig.runtime.view(run).await.unwrap().is_none(), "{id}");
    }
}

#[tokio::test]
async fn a_card_without_the_extension_leaves_the_effort_unread() {
    // A backend that lists no level never reads one, whatever the request says.
    let rig = Rig::new(None);
    rig.mock.push_text("done");
    let who = uniq("carol");
    let (stop, handle) = rig.worker();
    let task = rig
        .backend
        .submit(
            asking(&who),
            naming("go", "m-0", json!({"effort": "high"})),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_REASONING_EFFORT)
            .is_none()
    );
    wait_state(&rig, &asking(&who), &task.id, TaskState::Completed).await;
    assert_eq!(
        rig.mock
            .last_request()
            .expect("a model request")
            .reasoning_effort,
        None,
        "nothing is asked for when nothing was read"
    );
    stop.send(()).ok();
    handle.await.ok();
}

#[tokio::test]
async fn an_empty_set_leaves_the_effort_unread() {
    // A card that lists no level takes none: a backend handed the empty set reads nothing.
    let name = uniq("reason");
    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder(&name, model, OWN).build();
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(store)
        .agent(agent)
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, name.clone())
        .with_poll_interval(Duration::from_millis(10))
        .with_reasoning_efforts(ReasoningEfforts::new());
    assert!(backend.reasoning_efforts().is_none());
    mock.push_text("done");
    let who = uniq("erin");
    let task = backend
        .submit(
            asking(&who),
            naming("go", "m-0", json!({"effort": "high"})),
            None,
            None,
        )
        .await
        .unwrap();
    let run = RunId(task.id.parse().unwrap());
    assert!(
        runtime.view(run).await.unwrap().unwrap().state["context"]
            .get(CONTEXT_REASONING_EFFORT)
            .is_none()
    );
}

#[tokio::test]
async fn a_message_that_did_not_activate_the_extension_carries_no_effort() {
    let rig = Rig::new(Some(taking()));
    rig.mock.push_text("done");
    let who = uniq("dan");
    let (stop, handle) = rig.worker();
    // It names `high`, and the request did not activate the extension: nothing is read.
    let task = rig
        .backend
        .submit(
            plain(&who),
            naming("go", "m-0", json!({"effort": "high"})),
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        rig.state_of(&task).await["context"]
            .get(CONTEXT_REASONING_EFFORT)
            .is_none()
    );
    wait_state(&rig, &plain(&who), &task.id, TaskState::Completed).await;
    assert_eq!(
        rig.mock
            .last_request()
            .expect("a model request")
            .reasoning_effort,
        None
    );
    stop.send(()).ok();
    handle.await.ok();
}

/// The whole chain, through the real client: the message's level is the run's context, the loop
/// asks for it, and `adam-model-openai` writes it in the body through the model profile's wire.
#[tokio::test]
async fn a_level_the_agent_takes_reaches_the_wire_of_an_openai_client() {
    use adam_model::ModelProfiles;
    use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
    use secrecy::SecretString;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "done"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        })))
        .mount(&server)
        .await;
    let profiles = ModelProfiles::parse_json(&format!(
        r#"{{"profiles": {{"{OWN}": {{"reasoningEfforts": ["low", "high"], "reasoningWire": "reasoning_effort"}}}}}}"#
    ))
    .expect("valid profiles");
    let client = OpenAiCompatible::new(OpenAiConfig::new(
        format!("{}/v1", server.uri()),
        SecretString::from(""),
    ))
    .expect("client")
    .with_profiles(profiles);

    let name = uniq("wire");
    let store: DynStore = Arc::new(MemoryStore::new());
    let model: DynModel = Arc::new(client);
    let agent = LlmAgent::builder(&name, model, OWN)
        .stream_text(false)
        .build();
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(store)
        .agent(agent)
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, name.clone())
        .with_poll_interval(Duration::from_millis(10))
        .with_reasoning_efforts(ReasoningEfforts::new().with_agent(
            name.clone(),
            [ReasoningEffort::Low, ReasoningEffort::High],
            Some(ReasoningEffort::Low),
        ));
    let (stop, rx) = oneshot::channel::<()>();
    let worker = {
        let rt = runtime.clone();
        tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        })
    };
    let who = uniq("frank");
    let task = backend
        .submit(
            asking(&who),
            naming("go", "m-0", json!({"effort": "high"})),
            None,
            None,
        )
        .await
        .unwrap();
    for _ in 0..2000 {
        let seen = backend.get(&asking(&who), &task.id).await.unwrap().unwrap();
        if seen.status.state == TaskState::Completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let requests = server.received_requests().await.expect("recording");
    assert!(!requests.is_empty(), "the run made a model call");
    let body: Value = requests.last().unwrap().body_json().expect("json body");
    assert_eq!(body["reasoning_effort"], "high", "{body}");
    assert_eq!(body["model"], OWN, "{body}");
    stop.send(()).ok();
    worker.await.ok();
}
