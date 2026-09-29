//! The backend against a real `Runtime` over the in-memory store, with a tiny
//! scripted agent. Restart cases build a second runtime + backend over the same
//! store, which is what a replica or a restarted process looks like.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, TaskBackend, TaskEvent,
};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::{DynStore, MemoryStore};
use adam_runtime::{
    Agent, AgentError, AgentStarter, BroadcastSink, Ctx, Inbound, RunEvent, Runtime, Transition,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// Behaviour is chosen by markers in the first message:
///
/// * (none): progress, artifact `report`, then done with `finished`;
/// * `[input]`: asks "which colour?", parks; the answer completes the run;
/// * `[hold]`: parks on a far timer (so it is `working` until cancelled);
/// * `[fail]`: fails with `boom`;
/// * `[reject]`: `init` refuses the start message, like an agent that cannot read it.
struct Scripted;

/// The start-only half of [`Scripted`], for a front that never steps a run.
struct ScriptedStarter;

impl AgentStarter for ScriptedStarter {
    type State = Value;

    fn name(&self) -> &str {
        "scripted"
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        scripted_init(input)
    }
}

fn scripted_init(input: Inbound) -> Result<Value, AgentError> {
    let text = input.payload["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if text.contains("[reject]") {
        return Err(AgentError::permanent(
            "unusable start message: it asks for something this agent cannot do",
        ));
    }
    Ok(json!({"text": text, "phase": 0}))
}

#[async_trait]
impl Agent for Scripted {
    type State = Value;

    fn name(&self) -> &str {
        "scripted"
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        scripted_init(input)
    }

    async fn step(&self, ctx: &mut Ctx, mut state: Value) -> Result<Transition<Value>, AgentError> {
        let text = state["text"].as_str().unwrap_or_default().to_owned();
        let phase = state["phase"].as_u64().unwrap_or(0);
        if text.contains("[fail]") {
            return Ok(Transition::Fail {
                state,
                error: "boom".into(),
            });
        }
        if text.contains("[hold]") {
            let wake_at = ctx.now() + chrono::Duration::hours(1);
            return Ok(Transition::Park {
                state,
                wake_at: Some(wake_at),
            });
        }
        if text.contains("[input]") {
            if phase == 0 {
                ctx.emit(RunEvent::Progress {
                    message: "asking".into(),
                })
                .await;
                state["phase"] = json!(1);
                state["pending_question"] = json!({"question": "which colour?"});
                return Ok(Transition::Park {
                    state,
                    wake_at: None,
                });
            }
            let answers = ctx.take_inbox();
            let Some(answer) = answers.first() else {
                return Ok(Transition::Park {
                    state,
                    wake_at: None,
                });
            };
            let answer = answer.payload["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            state["pending_question"] = Value::Null;
            ctx.emit(RunEvent::Artifact {
                name: "answer".into(),
                mime_type: Some("text/plain".into()),
                data: json!(answer),
            })
            .await;
            return Ok(Transition::Done {
                state,
                output: json!({"text": format!("answered: {answer}")}),
            });
        }
        if phase == 0 {
            ctx.emit(RunEvent::Progress {
                message: "working".into(),
            })
            .await;
            ctx.emit(RunEvent::Artifact {
                name: "report".into(),
                mime_type: Some("text/markdown".into()),
                data: json!("# done"),
            })
            .await;
            state["phase"] = json!(1);
            return Ok(Transition::Continue(state));
        }
        Ok(Transition::Done {
            state,
            output: json!({"text": "finished"}),
        })
    }
}

struct Rig {
    store: DynStore,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

impl Rig {
    fn new() -> Self {
        Self::over(Arc::new(MemoryStore::new()))
    }

    /// A runtime + backend over `store` (a replica).
    fn over(store: DynStore) -> Self {
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store.clone())
            .agent(Scripted)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, "scripted")
            .with_poll_interval(Duration::from_millis(10));
        Self {
            store,
            runtime,
            backend,
        }
    }

    /// A front that only accepts tasks: its runtime registers the start-only
    /// half of the agent, so it can start runs but never steps them.
    fn front_only(store: DynStore) -> Self {
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store.clone())
            .starter(ScriptedStarter)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, "scripted")
            .with_poll_interval(Duration::from_millis(10));
        Self {
            store,
            runtime,
            backend,
        }
    }

    /// A replica whose backend hears nothing live: it has to rely on the store.
    fn deaf_replica(&self) -> Self {
        let runtime = Runtime::builder(self.store.clone())
            .agent(Scripted)
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend =
            RuntimeTaskBackend::new(runtime.clone(), BroadcastSink::default(), "scripted")
                .with_poll_interval(Duration::from_millis(10));
        Self {
            store: self.store.clone(),
            runtime,
            backend,
        }
    }

    fn worker(&self) -> Worker {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.runtime.clone();
        let handle = tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        Worker { stop, handle }
    }
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("worker stops")
            .expect("worker task");
    }
}

fn alice() -> Caller {
    Caller::new("token-0")
}

fn bob() -> Caller {
    Caller::new("token-1")
}

fn user(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

type Events = BoxStream<'static, Result<TaskEvent, BackendError>>;

async fn next(events: &mut Events) -> TaskEvent {
    tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .expect("timed out waiting for an event")
        .expect("stream ended early")
        .expect("stream item was an error")
}

/// Everything until the stream ends.
async fn rest(events: &mut Events) -> Vec<TaskEvent> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), events.next()).await {
            Ok(Some(item)) => out.push(item.expect("stream item was an error")),
            Ok(None) => return out,
            Err(_) => panic!("timed out draining the stream; got {out:?}"),
        }
    }
}

fn label(e: &TaskEvent) -> String {
    match e {
        TaskEvent::Snapshot(t) => format!("task:{:?}", t.status.state),
        TaskEvent::Status(u) => format!("status:{:?}", u.status.state),
        TaskEvent::Artifact(u) => {
            format!("artifact:{}", u.artifact.name.clone().unwrap_or_default())
        }
    }
}

fn status_text(e: &TaskEvent) -> Option<String> {
    match e {
        TaskEvent::Status(u) => u.status.message.as_ref()?.text().map(str::to_owned),
        TaskEvent::Snapshot(t) => t.status.message.as_ref()?.text().map(str::to_owned),
        TaskEvent::Artifact(_) => None,
    }
}

async fn wait_state(rig: &Rig, caller: &Caller, id: &str, state: TaskState) -> Task {
    for _ in 0..1000 {
        let task = rig
            .backend
            .get(caller, id)
            .await
            .expect("get")
            .expect("task exists");
        if task.status.state == state {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("task {id} never reached {state:?}");
}

#[tokio::test]
async fn a_new_task_is_a_run_and_streams_snapshot_progress_artifact_completed() {
    let rig = Rig::new();
    let task = rig
        .backend
        .submit(alice(), user("do it"), None, Some("ctx-1".into()))
        .await
        .expect("submit");
    assert_eq!(task.status.state, TaskState::Submitted);
    assert_eq!(task.context_id, "ctx-1");
    // task id = run id
    let run = adam_core::RunId(task.id.parse().expect("task id is a run id"));
    let view = rig.runtime.view(run).await.unwrap().expect("run exists");
    assert_eq!(view.agent, "scripted");
    assert_eq!(task.history.as_ref().map(Vec::len), Some(1));

    let mut events = rig.backend.subscribe(&alice(), &task.id);
    // The snapshot arrives first, and only after the subscription attached, so
    // starting the worker now cannot race the live progress event.
    let first = next(&mut events).await;
    assert_eq!(label(&first), "task:Submitted");
    let worker = rig.worker();
    let events = rest(&mut events).await;
    worker.stop().await;

    let labels: Vec<String> = events.iter().map(label).collect();
    let pos = |wanted: &str| {
        labels
            .iter()
            .position(|l| l == wanted)
            .unwrap_or_else(|| panic!("{wanted} missing from {labels:?}"))
    };
    assert!(
        pos("artifact:report") < pos("status:Completed"),
        "{labels:?}"
    );
    assert_eq!(labels.last().map(String::as_str), Some("status:Completed"));
    assert_eq!(
        labels.iter().filter(|l| l.starts_with("artifact:")).count(),
        1,
        "the live and the durable copy of an artifact are one event: {labels:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| status_text(e).as_deref() == Some("working")),
        "live progress is streamed as a working status: {labels:?}"
    );
    assert_eq!(
        status_text(events.last().unwrap()).as_deref(),
        Some("finished")
    );

    let done = rig.backend.get(&alice(), &task.id).await.unwrap().unwrap();
    assert_eq!(done.status.state, TaskState::Completed);
    // The status message is the same message wherever it is read: in the
    // stream, and in any number of snapshots.
    let streamed = match events.last().unwrap() {
        TaskEvent::Status(u) => u.status.message.clone().expect("completed message"),
        other => panic!("not a status: {other:?}"),
    };
    let polled = done.status.message.clone().expect("completed message");
    assert_eq!(streamed.message_id, polled.message_id);
    let again = rig.backend.get(&alice(), &task.id).await.unwrap().unwrap();
    assert_eq!(
        again.status.message.map(|m| m.message_id),
        Some(polled.message_id)
    );
    let artifacts = done.artifacts.expect("artifacts on the task");
    assert_eq!(artifacts[0].name.as_deref(), Some("report"));
    assert_eq!(artifacts[0].parts[0].as_text(), Some("# done"));
}

#[tokio::test]
async fn subscribing_to_a_finished_task_yields_just_the_snapshot() {
    let rig = Rig::new();
    let worker = rig.worker();
    let task = rig
        .backend
        .submit(alice(), user("go"), None, None)
        .await
        .unwrap();
    wait_state(&rig, &alice(), &task.id, TaskState::Completed).await;
    worker.stop().await;

    let mut events = rig.backend.subscribe(&alice(), &task.id);
    let all = rest(&mut events).await;
    assert_eq!(
        all.iter().map(label).collect::<Vec<_>>(),
        ["task:Completed"]
    );
    let TaskEvent::Snapshot(task) = &all[0] else {
        panic!("not a snapshot")
    };
    assert_eq!(task.artifacts.as_ref().map(Vec::len), Some(1));
}

#[tokio::test]
async fn input_required_carries_the_question_and_a_follow_up_resumes_in_working() {
    let rig = Rig::new();
    let worker = rig.worker();
    let task = rig
        .backend
        .submit(alice(), user("[input] paint it"), None, None)
        .await
        .unwrap();

    let waiting = wait_state(&rig, &alice(), &task.id, TaskState::InputRequired).await;
    assert_eq!(
        waiting.status.message.as_ref().and_then(|m| m.text()),
        Some("which colour?")
    );

    // A subscription to a waiting task is the snapshot and nothing else.
    let mut events = rig.backend.subscribe(&alice(), &task.id);
    let all = rest(&mut events).await;
    assert_eq!(
        all.iter().map(label).collect::<Vec<_>>(),
        ["task:InputRequired"]
    );
    assert_eq!(status_text(&all[0]).as_deref(), Some("which colour?"));

    // The follow-up returns the task already working, and a new subscription
    // does not see the stale interrupted state.
    let resumed = rig
        .backend
        .submit(alice(), user("blue"), Some(task.id.clone()), None)
        .await
        .unwrap();
    assert_eq!(resumed.status.state, TaskState::Working);
    assert_eq!(resumed.id, task.id);
    let mut events = rig.backend.subscribe(&alice(), &task.id);
    let labels: Vec<String> = {
        let mut all = vec![next(&mut events).await];
        all.extend(rest(&mut events).await);
        all.iter().map(label).collect()
    };
    worker.stop().await;
    assert_ne!(labels[0], "task:InputRequired", "{labels:?}");
    assert_eq!(labels.last().map(String::as_str), Some("status:Completed"));

    let done = rig.backend.get(&alice(), &task.id).await.unwrap().unwrap();
    assert_eq!(
        done.status.message.as_ref().and_then(|m| m.text()),
        Some("answered: blue")
    );
    assert_eq!(done.artifacts.unwrap()[0].parts[0].as_text(), Some("blue"));
}

#[tokio::test]
async fn subscriptions_are_rebuilt_from_the_store_after_a_restart() {
    restart_scenario(Arc::new(MemoryStore::new())).await;
}

/// The same restart over a real PostgreSQL (skipped unless
/// `ADAM_TEST_POSTGRES_URL` is set): what a second replica sees is what the
/// database holds.
#[tokio::test]
async fn subscriptions_are_rebuilt_from_postgres_after_a_restart() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let store = adam_store_postgres::PgStore::connect(&url)
        .await
        .expect("connect to postgres");
    adam_core::Store::migrate(&store).await.expect("migrate");
    restart_scenario(Arc::new(store)).await;
}

async fn restart_scenario(store: DynStore) {
    // Unique per run: a shared database keeps earlier runs' rows.
    let unique = uuid_like();
    let context = format!("ctx-restart-{unique}");
    let alice = Caller::new(format!("token-{unique}"));
    let rig = Rig::over(store);
    let worker = rig.worker();
    let task = rig
        .backend
        .submit(alice.clone(), user("[input]"), None, Some(context.clone()))
        .await
        .unwrap();
    wait_state(&rig, &alice, &task.id, TaskState::InputRequired).await;
    worker.stop().await;
    drop(rig.backend.clone());

    // "Restart": a brand-new runtime and backend over the same store, whose
    // backend hears no live events at all.
    let restarted = rig.deaf_replica();
    let seen = restarted
        .backend
        .get(&alice, &task.id)
        .await
        .unwrap()
        .expect("the task survives the restart");
    assert_eq!(seen.status.state, TaskState::InputRequired);
    assert_eq!(seen.context_id, context);
    assert_eq!(
        seen.status.message.as_ref().and_then(|m| m.text()),
        Some("which colour?")
    );

    let resumed = restarted
        .backend
        .submit(alice.clone(), user("green"), Some(task.id.clone()), None)
        .await
        .unwrap();
    assert_eq!(resumed.status.state, TaskState::Working);

    let mut events = restarted.backend.subscribe(&alice, &task.id);
    let first = next(&mut events).await;
    assert_eq!(label(&first), "task:Working");
    let worker = restarted.worker();
    let events = rest(&mut events).await;
    worker.stop().await;

    let labels: Vec<String> = events.iter().map(label).collect();
    assert_eq!(
        labels,
        ["artifact:answer", "status:Completed"],
        "polling alone delivers the artifact and the terminal state"
    );
}

fn uuid_like() -> String {
    adam_core::RunId::new().to_string()
}

#[tokio::test]
async fn a_subscription_started_mid_flight_ends_at_completion_from_polling_alone() {
    let rig = Rig::new();
    let task = rig
        .backend
        .submit(alice(), user("go"), None, None)
        .await
        .unwrap();
    let replica = rig.deaf_replica();
    let mut events = replica.backend.subscribe(&alice(), &task.id);
    let first = next(&mut events).await;
    assert_eq!(label(&first), "task:Submitted");
    let worker = replica.worker();
    let labels: Vec<String> = rest(&mut events).await.iter().map(label).collect();
    worker.stop().await;
    assert_eq!(
        labels.last().map(String::as_str),
        Some("status:Completed"),
        "{labels:?}"
    );
    assert!(labels.contains(&"artifact:report".to_owned()), "{labels:?}");
}

#[tokio::test]
async fn tasks_belong_to_their_caller() {
    let rig = Rig::new();
    let worker = rig.worker();
    let task = rig
        .backend
        .submit(alice(), user("[input]"), None, Some("shared".into()))
        .await
        .unwrap();
    wait_state(&rig, &alice(), &task.id, TaskState::InputRequired).await;

    assert!(rig.backend.get(&bob(), &task.id).await.unwrap().is_none());
    assert!(matches!(
        rig.backend.cancel(&bob(), &task.id).await,
        Err(BackendError::TaskNotFound(_))
    ));
    assert!(matches!(
        rig.backend
            .submit(bob(), user("hijack"), Some(task.id.clone()), None)
            .await,
        Err(BackendError::TaskNotFound(_))
    ));
    let mut events = rig.backend.subscribe(&bob(), &task.id);
    assert!(matches!(
        events.next().await,
        Some(Err(BackendError::TaskNotFound(_)))
    ));
    assert!(events.next().await.is_none());

    // The same context id under another caller is another conversation.
    let other = rig
        .backend
        .submit(bob(), user("go"), None, Some("shared".into()))
        .await
        .unwrap();
    assert_ne!(other.id, task.id);

    // Unknown and malformed ids are just "not found".
    assert!(
        rig.backend
            .get(&alice(), "no-such-task")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        rig.backend
            .get(&alice(), &adam_core::RunId::new().to_string())
            .await
            .unwrap()
            .is_none()
    );
    worker.stop().await;
}

#[tokio::test]
async fn follow_ups_are_only_for_tasks_waiting_for_input() {
    let rig = Rig::new();
    let worker = rig.worker();
    let running = rig
        .backend
        .submit(alice(), user("[hold]"), None, None)
        .await
        .unwrap();
    wait_state(&rig, &alice(), &running.id, TaskState::Working).await;
    let err = rig
        .backend
        .submit(alice(), user("more"), Some(running.id.clone()), None)
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::InvalidParams(_)), "{err:?}");

    let done = rig
        .backend
        .submit(alice(), user("go"), None, None)
        .await
        .unwrap();
    wait_state(&rig, &alice(), &done.id, TaskState::Completed).await;
    let err = rig
        .backend
        .submit(alice(), user("more"), Some(done.id.clone()), None)
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::InvalidParams(_)), "{err:?}");

    let waiting = rig
        .backend
        .submit(alice(), user("[input]"), None, None)
        .await
        .unwrap();
    wait_state(&rig, &alice(), &waiting.id, TaskState::InputRequired).await;
    let err = rig
        .backend
        .submit(
            alice(),
            user("x"),
            Some(waiting.id.clone()),
            Some("other-context".into()),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::InvalidParams(_)), "{err:?}");

    let err = rig
        .backend
        .submit(
            alice(),
            Message::new(Role::User, vec![Part::text("  ")]),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, BackendError::InvalidParams(_)), "{err:?}");
    worker.stop().await;
}

#[tokio::test]
async fn a_message_in_a_context_with_an_open_task_goes_to_that_task() {
    let rig = Rig::new();
    let worker = rig.worker();
    let task = rig
        .backend
        .submit(alice(), user("[input]"), None, Some("c1".into()))
        .await
        .unwrap();
    wait_state(&rig, &alice(), &task.id, TaskState::InputRequired).await;

    let again = rig
        .backend
        .submit(alice(), user("red"), None, Some("c1".into()))
        .await
        .unwrap();
    assert_eq!(again.id, task.id, "one open run per conversation");
    let done = wait_state(&rig, &alice(), &task.id, TaskState::Completed).await;
    assert_eq!(
        done.status.message.as_ref().and_then(|m| m.text()),
        Some("answered: red")
    );

    // Once finished, the context starts a new task.
    let next_task = rig
        .backend
        .submit(alice(), user("go"), None, Some("c1".into()))
        .await
        .unwrap();
    assert_ne!(next_task.id, task.id);
    assert_eq!(next_task.context_id, "c1");
    worker.stop().await;
}

#[tokio::test]
async fn cancel_maps_to_the_runtime_cancel_and_is_idempotent() {
    let rig = Rig::new();
    let worker = rig.worker();
    let task = rig
        .backend
        .submit(alice(), user("[hold]"), None, None)
        .await
        .unwrap();
    wait_state(&rig, &alice(), &task.id, TaskState::Working).await;

    let mut events = rig.backend.subscribe(&alice(), &task.id);
    assert_eq!(label(&next(&mut events).await), "task:Working");

    let canceled = rig.backend.cancel(&alice(), &task.id).await.unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);
    let labels: Vec<String> = rest(&mut events).await.iter().map(label).collect();
    assert_eq!(labels.last().map(String::as_str), Some("status:Canceled"));

    let again = rig.backend.cancel(&alice(), &task.id).await.unwrap();
    assert_eq!(again.status.state, TaskState::Canceled);
    let run = adam_core::RunId(task.id.parse().unwrap());
    let view = rig.runtime.view(run).await.unwrap().unwrap();
    assert!(view.error.unwrap().starts_with("cancelled: "));

    // Another terminal state cannot be canceled.
    let done = rig
        .backend
        .submit(alice(), user("go"), None, None)
        .await
        .unwrap();
    wait_state(&rig, &alice(), &done.id, TaskState::Completed).await;
    assert!(matches!(
        rig.backend.cancel(&alice(), &done.id).await,
        Err(BackendError::NotCancelable { .. })
    ));
    worker.stop().await;
}

#[tokio::test]
async fn a_failed_run_is_a_failed_task_with_the_error() {
    let rig = Rig::new();
    let worker = rig.worker();
    let task = rig
        .backend
        .submit(alice(), user("[fail]"), None, None)
        .await
        .unwrap();
    let failed = wait_state(&rig, &alice(), &task.id, TaskState::Failed).await;
    worker.stop().await;
    assert_eq!(
        failed.status.message.as_ref().and_then(|m| m.text()),
        Some("boom")
    );
    assert!(failed.artifacts.is_none());
}

// ------------------------------------------------------------------ over HTTP

#[tokio::test]
async fn round_trip_with_the_official_client_over_http() {
    use a2a::{SendMessageRequest, StreamResponse};
    use a2a_client::A2AClientFactory;
    use a2a_client::agent_card::AgentCardResolver;
    use a2a_client::auth::AuthInterceptor;
    use secrecy::SecretString;

    let rig = Rig::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let card = AgentCardConfig::new(
        "scripted",
        "Scripted test agent",
        format!("http://{addr}/").parse().unwrap(),
        "0.1.0",
    );
    let app = A2aServer::router(
        card,
        Arc::new(rig.backend.clone()),
        AuthConfig::BearerTokens(vec![SecretString::from("t0")]),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let worker = rig.worker();

    let base = format!("http://{addr}");
    let card = AgentCardResolver::new(None).resolve(&base).await.unwrap();
    let client = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer("t0")))
        .build()
        .create_from_card(&card)
        .await
        .unwrap();

    let request = SendMessageRequest {
        message: user("[input] over http"),
        configuration: None,
        metadata: None,
        tenant: None,
    };
    let mut stream = client.send_streaming_message(&request).await.unwrap();
    let mut states = Vec::new();
    let mut task_id = String::new();
    while let Some(item) = stream.next().await {
        match item.unwrap() {
            StreamResponse::Task(t) => {
                task_id = t.id.clone();
                states.push(format!("task:{:?}", t.status.state));
            }
            StreamResponse::StatusUpdate(u) => states.push(format!("status:{:?}", u.status.state)),
            other => states.push(format!("{other:?}")),
        }
    }
    assert!(!task_id.is_empty());
    assert_eq!(
        states.last().map(String::as_str),
        Some("status:InputRequired"),
        "the stream ends when the task waits for input: {states:?}"
    );

    // The follow-up over the wire resumes it; a blocking send returns the result.
    let mut follow = user("violet");
    follow.task_id = Some(task_id.clone());
    let response = client
        .send_message(&SendMessageRequest {
            message: follow,
            configuration: None,
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap();
    let a2a::SendMessageResponse::Task(done) = response else {
        panic!("expected a task")
    };
    assert_eq!(done.status.state, TaskState::Completed);
    assert_eq!(
        done.status.message.as_ref().and_then(|m| m.text()),
        Some("answered: violet")
    );
    worker.stop().await;
}

// -------------------------------------------------------------- error mapping

/// Regression for A3: an `init` rejection (`AgentError::Permanent`) reached the client as
/// `-32603 internal error` through the catch-all; it is the request that is wrong, so it is
/// invalid params, with the agent's own message.
#[tokio::test]
async fn an_init_rejection_is_invalid_params_not_internal() {
    let rig = Rig::new();
    let err = rig
        .backend
        .submit(alice(), user("[reject] please"), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BackendError::InvalidParams(m) if m.contains("unusable start message")),
        "{err:?}"
    );
    assert_eq!(a2a::A2AError::from(err).code, -32602);
}

/// The same over the wire: the JSON-RPC error object carries `-32602` and the agent's message,
/// not `-32603 internal error`.
#[tokio::test]
async fn an_init_rejection_is_a_32602_over_http() {
    use a2a::SendMessageRequest;
    use a2a_client::A2AClientFactory;
    use a2a_client::agent_card::AgentCardResolver;
    use a2a_client::auth::AuthInterceptor;
    use secrecy::SecretString;

    let rig = Rig::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let card = AgentCardConfig::new(
        "scripted",
        "Scripted test agent",
        format!("http://{addr}/").parse().unwrap(),
        "0.1.0",
    );
    let app = A2aServer::router(
        card,
        Arc::new(rig.backend.clone()),
        AuthConfig::BearerTokens(vec![SecretString::from("t0")]),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let card = AgentCardResolver::new(None).resolve(&base).await.unwrap();
    let client = A2AClientFactory::builder()
        .with_interceptor(Arc::new(AuthInterceptor::bearer("t0")))
        .build()
        .create_from_card(&card)
        .await
        .unwrap();

    let err = client
        .send_message(&SendMessageRequest {
            message: user("[reject]"),
            configuration: None,
            metadata: None,
            tenant: None,
        })
        .await
        .unwrap_err();
    let text = format!("{err:?}");
    assert!(text.contains("-32602"), "{text}");
    assert!(text.contains("unusable start message"), "{text}");
    assert!(!text.contains("-32603"), "{text}");
}

#[tokio::test]
async fn a_starter_only_front_accepts_a_task_a_separate_worker_completes_it() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let front = Rig::front_only(store.clone());
    let task = front
        .backend
        .submit(alice(), user("do it"), None, Some("ctx-1".into()))
        .await
        .expect("submit");
    assert_eq!(task.status.state, TaskState::Submitted);

    // The front's own worker loop has nothing to step the run with.
    let idle = front.worker();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let still = front
        .backend
        .get(&alice(), &task.id)
        .await
        .unwrap()
        .unwrap();
    idle.stop().await;
    assert_eq!(still.status.state, TaskState::Submitted);

    // A separate process with the full agent, over the same store, finishes it
    // and the front reads the result from the durable record.
    let back = Rig::over(store);
    let worker = back.worker();
    let done = wait_state(&front, &alice(), &task.id, TaskState::Completed).await;
    worker.stop().await;
    assert_eq!(done.id, task.id);
    assert!(
        front.backend.get(&bob(), &task.id).await.unwrap().is_none(),
        "the front still enforces ownership"
    );
}

/// Messages delivered to the task's run on top of its start input.
async fn pending(rig: &Rig, task: &Task) -> usize {
    let run = adam_core::RunId(task.id.parse().unwrap());
    rig.runtime.view(run).await.unwrap().unwrap().pending_inbox
}

fn user_with_id(text: &str, id: &str) -> Message {
    let mut m = user(text);
    m.message_id = id.into();
    m
}

#[tokio::test]
async fn repeating_a_message_returns_the_task_and_delivers_the_input_once() {
    let rig = Rig::new();
    let first = rig
        .backend
        .submit(alice(), user_with_id("go", "m-1"), None, Some("c1".into()))
        .await
        .unwrap();
    // The retry after a crash, while the task is still open (no worker runs).
    let retry = rig
        .backend
        .submit(alice(), user_with_id("go", "m-1"), None, Some("c1".into()))
        .await
        .unwrap();
    assert_eq!(retry.id, first.id);
    assert_eq!(retry.context_id, "c1");
    assert_eq!(
        adam_a2a_runtime::task_id_for("token-0", Some("c1"), "m-1").to_string(),
        first.id
    );
    // The start message is the run's input; nothing was delivered on top.
    assert_eq!(pending(&rig, &first).await, 0);

    // ... and after the task finished the repeat still finds it, not a new one.
    let worker = rig.worker();
    wait_state(&rig, &alice(), &first.id, TaskState::Completed).await;
    let late = rig
        .backend
        .submit(alice(), user_with_id("go", "m-1"), None, Some("c1".into()))
        .await
        .unwrap();
    assert_eq!(late.id, first.id);
    assert_eq!(late.status.state, TaskState::Completed);
    worker.stop().await;
}

#[tokio::test]
async fn a_repeat_without_a_context_id_finds_the_context_the_first_attempt_made() {
    let rig = Rig::new();
    let first = rig
        .backend
        .submit(alice(), user_with_id("go", "m-1"), None, None)
        .await
        .unwrap();
    let retry = rig
        .backend
        .submit(alice(), user_with_id("go", "m-1"), None, None)
        .await
        .unwrap();
    assert_eq!(retry.id, first.id);
    assert_eq!(retry.context_id, first.context_id);
    assert_eq!(pending(&rig, &first).await, 0);
}

#[tokio::test]
async fn different_messages_contexts_and_callers_are_different_tasks() {
    let rig = Rig::new();
    let submit = |who: Caller, id: &'static str, ctx: Option<&'static str>| {
        let backend = rig.backend.clone();
        async move {
            backend
                .submit(who, user_with_id("go", id), None, ctx.map(str::to_owned))
                .await
                .unwrap()
                .id
        }
    };
    let base = submit(alice(), "m-1", Some("c1")).await;
    assert_ne!(base, submit(alice(), "m-2", None).await, "other message");
    assert_ne!(
        base,
        submit(alice(), "m-1", Some("c2")).await,
        "other context"
    );
    assert_ne!(base, submit(bob(), "m-1", Some("c1")).await, "other caller");
    assert_eq!(base, submit(alice(), "m-1", Some("c1")).await);
}

#[tokio::test]
async fn a_new_message_in_an_open_context_is_still_delivered_to_its_task() {
    let rig = Rig::new();
    let first = rig
        .backend
        .submit(alice(), user_with_id("go", "m-1"), None, Some("c1".into()))
        .await
        .unwrap();
    let second = rig
        .backend
        .submit(
            alice(),
            user_with_id("more", "m-2"),
            None,
            Some("c1".into()),
        )
        .await
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(pending(&rig, &first).await, 1);
}
