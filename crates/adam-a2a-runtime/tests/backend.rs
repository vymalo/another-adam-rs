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
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{Conversation, LlmAgent, LlmStarter};
use adam_model::{Message as ModelMessage, MockModel};
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
///
/// A task started as the continuation of another lists the texts of the tasks before it in
/// `state.earlier`.
struct Scripted(String);

/// The start-only half of [`Scripted`], for a front that never steps a run.
struct ScriptedStarter(String);

impl AgentStarter for ScriptedStarter {
    type State = Value;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        scripted_init(input)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Value,
        _prior_run: RunId,
    ) -> Result<Value, AgentError> {
        scripted_continue(input, prior)
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

/// A task that continues another remembers what the tasks before it were asked, oldest first, in
/// `earlier`. A task that continues nothing has an empty (absent) `earlier`.
fn scripted_continue(input: Inbound, prior: &Value) -> Result<Value, AgentError> {
    let mut state = scripted_init(input)?;
    let mut earlier = prior["earlier"].as_array().cloned().unwrap_or_default();
    earlier.push(prior["text"].clone());
    state["earlier"] = Value::Array(earlier);
    Ok(state)
}

#[async_trait]
impl Agent for Scripted {
    type State = Value;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        scripted_init(input)
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Value,
        _prior_run: RunId,
    ) -> Result<Value, AgentError> {
        scripted_continue(input, prior)
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
    agent: String,
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

/// What every case but the database ones calls its agent. A case on a database shared with
/// others takes a name of its own (`over_as`), so that another case's worker never steps its runs.
const AGENT: &str = "scripted";

impl Rig {
    fn new() -> Self {
        Self::over(Arc::new(MemoryStore::new()))
    }

    /// A runtime + backend over `store` (a replica).
    fn over(store: DynStore) -> Self {
        Self::over_as(store, AGENT)
    }

    fn over_as(store: DynStore, agent: &str) -> Self {
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store.clone())
            .agent(Scripted(agent.to_owned()))
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, agent)
            .with_poll_interval(Duration::from_millis(10));
        Self {
            store,
            agent: agent.to_owned(),
            runtime,
            backend,
        }
    }

    /// A front that only accepts tasks: its runtime registers the start-only
    /// half of the agent, so it can start runs but never steps them.
    fn front_only(store: DynStore) -> Self {
        Self::front_only_as(store, AGENT)
    }

    fn front_only_as(store: DynStore, agent: &str) -> Self {
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store.clone())
            .starter(ScriptedStarter(agent.to_owned()))
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, agent)
            .with_poll_interval(Duration::from_millis(10));
        Self {
            store,
            agent: agent.to_owned(),
            runtime,
            backend,
        }
    }

    /// A replica whose backend hears nothing live: it has to rely on the store.
    fn deaf_replica(&self) -> Self {
        let runtime = Runtime::builder(self.store.clone())
            .agent(Scripted(self.agent.clone()))
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(
            runtime.clone(),
            BroadcastSink::default(),
            self.agent.clone(),
        )
        .with_poll_interval(Duration::from_millis(10));
        Self {
            store: self.store.clone(),
            agent: self.agent.clone(),
            runtime,
            backend,
        }
    }

    fn worker(&self) -> Worker {
        spawn_worker(self.runtime.clone())
    }
}

fn spawn_worker(rt: Runtime) -> Worker {
    let (stop, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let _ = rt
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    Worker { stop, handle }
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
    wait_task(&rig.backend, caller, id, state).await
}

async fn wait_task(
    backend: &RuntimeTaskBackend,
    caller: &Caller,
    id: &str,
    state: TaskState,
) -> Task {
    for _ in 0..1000 {
        let task = backend
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
        adam_a2a_runtime::task_id_for("scripted", "token-0", Some("c1"), "m-1").to_string(),
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

// ---------------------------------------------------------------------------
// A new task continues the task it references
// ---------------------------------------------------------------------------

/// A message that references the tasks it builds on.
fn user_refs(text: &str, references: &[&str]) -> Message {
    let mut m = user(text);
    m.reference_task_ids = Some(references.iter().map(|r| (*r).to_owned()).collect());
    m
}

/// The texts of the tasks that the run behind `task` continued, oldest first.
async fn earlier(rig: &Rig, task: &Task) -> Vec<String> {
    let run = RunId(task.id.parse().unwrap());
    let view = rig.runtime.view(run).await.unwrap().unwrap();
    view.state["earlier"]
        .as_array()
        .map(|texts| {
            texts
                .iter()
                .map(|t| t.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Submit `text` in `context`, referencing `references`, and return the task.
async fn send(
    rig: &Rig,
    who: &Caller,
    text: &str,
    context: Option<&str>,
    references: &[&str],
) -> Task {
    rig.backend
        .submit(
            who.clone(),
            user_refs(text, references),
            None,
            context.map(str::to_owned),
        )
        .await
        .expect("submit")
}

/// Submit `text` in `context` and wait until the task is `state`.
async fn run_to(
    rig: &Rig,
    who: &Caller,
    text: &str,
    context: &str,
    references: &[&str],
    state: TaskState,
) -> Task {
    let task = send(rig, who, text, Some(context), references).await;
    wait_state(rig, who, &task.id, state).await
}

#[tokio::test]
async fn a_new_task_that_references_a_finished_task_continues_it() {
    let rig = Rig::new();
    let worker = rig.worker();
    let first = run_to(&rig, &alice(), "one", "c1", &[], TaskState::Completed).await;
    assert_eq!(earlier(&rig, &first).await, Vec::<String>::new());

    let second = send(&rig, &alice(), "two", Some("c1"), &[first.id.as_str()]).await;
    assert_ne!(second.id, first.id, "a new task, not the old one reopened");
    assert_eq!(second.context_id, "c1");
    assert_eq!(second.history.as_ref().map(Vec::len), Some(1));
    assert_eq!(earlier(&rig, &second).await, ["one"]);
    let second = wait_state(&rig, &alice(), &second.id, TaskState::Completed).await;

    // The chain grows one task at a time, each referencing the one before.
    let third = send(&rig, &alice(), "three", Some("c1"), &[second.id.as_str()]).await;
    assert_eq!(earlier(&rig, &third).await, ["one", "two"]);
    wait_state(&rig, &alice(), &third.id, TaskState::Completed).await;
    worker.stop().await;
}

#[tokio::test]
async fn without_a_reference_a_new_task_starts_from_nothing() {
    let rig = Rig::new();
    let worker = rig.worker();
    run_to(&rig, &alice(), "one", "c1", &[], TaskState::Completed).await;

    // The context has a finished task, and that is not enough: nothing is guessed.
    let second = send(&rig, &alice(), "two", Some("c1"), &[]).await;
    assert_eq!(earlier(&rig, &second).await, Vec::<String>::new());
    wait_state(&rig, &alice(), &second.id, TaskState::Completed).await;
    // An empty list is no reference either.
    let mut message = user("three");
    message.reference_task_ids = Some(Vec::new());
    let third = rig
        .backend
        .submit(alice(), message, None, Some("c1".into()))
        .await
        .unwrap();
    assert_eq!(earlier(&rig, &third).await, Vec::<String>::new());
    worker.stop().await;
}

/// A reference that is someone else's, unknown or malformed is skipped, and the caller cannot tell
/// which: each gets the same fresh task an id that never existed gets.
#[tokio::test]
async fn a_reference_to_another_callers_task_is_ignored_like_an_unknown_one() {
    let rig = Rig::new();
    let worker = rig.worker();
    let alices = run_to(&rig, &alice(), "secret", "c1", &[], TaskState::Completed).await;

    // The same context id, but bob's: his conversation is his own.
    let foreign = send(&rig, &bob(), "hi", Some("c1"), &[alices.id.as_str()]).await;
    wait_state(&rig, &bob(), &foreign.id, TaskState::Completed).await;
    let unknown_id = RunId::new().to_string();
    let unknown = send(&rig, &bob(), "hi", Some("c1"), &[unknown_id.as_str()]).await;
    wait_state(&rig, &bob(), &unknown.id, TaskState::Completed).await;
    let malformed = send(&rig, &bob(), "hi", Some("c1"), &["not-a-task-id"]).await;
    wait_state(&rig, &bob(), &malformed.id, TaskState::Completed).await;
    assert_ne!(foreign.id, unknown.id, "each one is a task of its own");
    for task in [&foreign, &unknown, &malformed] {
        assert_eq!(earlier(&rig, task).await, Vec::<String>::new());
    }
    // Same answer in shape: a new task in his context, no error, nothing said about why.
    assert_eq!(foreign.context_id, unknown.context_id);
    assert_eq!(
        foreign.history.as_ref().map(Vec::len),
        unknown.history.as_ref().map(Vec::len)
    );
    // The owner's task is untouched by any of it.
    let still = rig
        .backend
        .get(&alice(), &alices.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.status.state, TaskState::Completed);
    worker.stop().await;
}

#[tokio::test]
async fn a_reference_to_a_task_of_another_context_is_ignored() {
    let rig = Rig::new();
    let worker = rig.worker();
    let other = run_to(&rig, &alice(), "elsewhere", "c1", &[], TaskState::Completed).await;

    let in_c2 = send(&rig, &alice(), "here", Some("c2"), &[other.id.as_str()]).await;
    assert_eq!(in_c2.context_id, "c2");
    assert_eq!(earlier(&rig, &in_c2).await, Vec::<String>::new());
    // A message with no context id gets one of its own, so nothing is "the same context".
    let no_context = send(&rig, &alice(), "there", None, &[other.id.as_str()]).await;
    assert_ne!(no_context.context_id, "c1");
    assert_eq!(earlier(&rig, &no_context).await, Vec::<String>::new());
    worker.stop().await;
}

/// A task that is not finished is not continued: a message for the context goes to its open task
/// as it always did, reference or not.
#[tokio::test]
async fn a_reference_to_an_open_task_keeps_todays_semantics() {
    let rig = Rig::new();
    let worker = rig.worker();
    let open = run_to(
        &rig,
        &alice(),
        "[input]",
        "c1",
        &[],
        TaskState::InputRequired,
    )
    .await;

    let again = send(&rig, &alice(), "red", Some("c1"), &[open.id.as_str()]).await;
    assert_eq!(again.id, open.id, "delivered to the open task, no new run");
    let done = wait_state(&rig, &alice(), &open.id, TaskState::Completed).await;
    assert_eq!(
        done.status.message.as_ref().and_then(|m| m.text()),
        Some("answered: red")
    );
    assert_eq!(earlier(&rig, &open).await, Vec::<String>::new());

    // `working` is open too.
    let held = run_to(&rig, &alice(), "[hold]", "c2", &[], TaskState::Working).await;
    let more = send(&rig, &alice(), "more", Some("c2"), &[held.id.as_str()]).await;
    assert_eq!(more.id, held.id);
    worker.stop().await;
}

#[tokio::test]
async fn the_first_reference_that_qualifies_is_taken_and_the_list_is_bounded() {
    let rig = Rig::new();
    let worker = rig.worker();
    let a = run_to(&rig, &alice(), "a", "c1", &[], TaskState::Completed).await;
    let b = run_to(&rig, &alice(), "b", "c1", &[], TaskState::Completed).await;
    let elsewhere = run_to(&rig, &alice(), "z", "c9", &[], TaskState::Completed).await;
    let theirs = run_to(&rig, &bob(), "y", "c1", &[], TaskState::Completed).await;
    let unknown = RunId::new().to_string();

    // Skips what does not qualify, then takes the first that does, and stops there.
    let skipped = [unknown.as_str(), theirs.id.as_str(), elsewhere.id.as_str()];
    let refs = [
        skipped[0],
        skipped[1],
        skipped[2],
        a.id.as_str(),
        b.id.as_str(),
    ];
    let t = send(&rig, &alice(), "next", Some("c1"), &refs).await;
    assert_eq!(earlier(&rig, &t).await, ["a"]);
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;

    // Order is the client's: the same two references the other way round.
    let t = send(
        &rig,
        &alice(),
        "next again",
        Some("c1"),
        &[b.id.as_str(), a.id.as_str()],
    )
    .await;
    assert_eq!(earlier(&rig, &t).await, ["b"]);
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;

    // Only the first MAX_REFERENCES are looked at: a qualifying one after them is not found.
    let filler: Vec<String> = (0..adam_a2a_runtime::MAX_REFERENCES)
        .map(|_| RunId::new().to_string())
        .collect();
    let mut late: Vec<&str> = filler.iter().map(String::as_str).collect();
    late.push(a.id.as_str());
    let t = send(&rig, &alice(), "too late", Some("c1"), &late).await;
    assert_eq!(earlier(&rig, &t).await, Vec::<String>::new());
    worker.stop().await;
}

#[tokio::test]
async fn a_failed_or_canceled_task_can_be_continued_too() {
    let rig = Rig::new();
    let worker = rig.worker();
    let failed = run_to(&rig, &alice(), "[fail]", "c1", &[], TaskState::Failed).await;
    let after_failure = send(
        &rig,
        &alice(),
        "try again",
        Some("c1"),
        &[failed.id.as_str()],
    )
    .await;
    assert_eq!(earlier(&rig, &after_failure).await, ["[fail]"]);
    wait_state(&rig, &alice(), &after_failure.id, TaskState::Completed).await;

    let held = run_to(&rig, &alice(), "[hold]", "c2", &[], TaskState::Working).await;
    rig.backend.cancel(&alice(), &held.id).await.unwrap();
    let after_cancel = send(
        &rig,
        &alice(),
        "never mind",
        Some("c2"),
        &[held.id.as_str()],
    )
    .await;
    assert_eq!(earlier(&rig, &after_cancel).await, ["[hold]"]);
    worker.stop().await;
}

#[tokio::test]
async fn repeating_a_continuing_request_starts_one_task() {
    let rig = Rig::new();
    let worker = rig.worker();
    let first = run_to(&rig, &alice(), "one", "c1", &[], TaskState::Completed).await;
    let mut request = user_refs("two", &[first.id.as_str()]);
    request.message_id = "m-2".into();
    let second = rig
        .backend
        .submit(alice(), request.clone(), None, Some("c1".into()))
        .await
        .unwrap();
    let retry = rig
        .backend
        .submit(alice(), request, None, Some("c1".into()))
        .await
        .unwrap();
    assert_eq!(retry.id, second.id);
    assert_eq!(
        adam_a2a_runtime::task_id_for("scripted", "token-0", Some("c1"), "m-2").to_string(),
        second.id,
        "the id does not depend on the references"
    );
    assert_eq!(earlier(&rig, &retry).await, ["one"]);
    wait_state(&rig, &alice(), &second.id, TaskState::Completed).await;
    // And after it finished, too.
    let mut request = user_refs("two", &[first.id.as_str()]);
    request.message_id = "m-2".into();
    let late = rig
        .backend
        .submit(alice(), request, None, Some("c1".into()))
        .await
        .unwrap();
    assert_eq!(late.id, second.id);
    worker.stop().await;
}

/// The same, for a request without a message id (nothing to recognise a repeat by).
#[tokio::test]
async fn a_request_without_a_message_id_continues_too() {
    let rig = Rig::new();
    let worker = rig.worker();
    let first = run_to(&rig, &alice(), "one", "c1", &[], TaskState::Completed).await;
    let mut request = user_refs("two", &[first.id.as_str()]);
    request.message_id = String::new();
    let second = rig
        .backend
        .submit(alice(), request, None, Some("c1".into()))
        .await
        .unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(earlier(&rig, &second).await, ["one"]);
    worker.stop().await;
}

/// A front that holds only the starter continues a task a separate worker finished.
#[tokio::test]
async fn a_starter_only_front_continues_what_a_separate_worker_finished() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let front = Rig::front_only(store.clone());
    let back = Rig::over(store);
    let worker = back.worker();

    let first = front
        .backend
        .submit(alice(), user("one"), None, Some("c1".into()))
        .await
        .unwrap();
    wait_state(&front, &alice(), &first.id, TaskState::Completed).await;
    let second = front
        .backend
        .submit(
            alice(),
            user_refs("two", &[first.id.as_str()]),
            None,
            Some("c1".into()),
        )
        .await
        .unwrap();
    assert_eq!(earlier(&front, &second).await, ["one"]);
    wait_state(&front, &alice(), &second.id, TaskState::Completed).await;
    worker.stop().await;
}

/// A continuation after a restart: the first task was finished by one process, and a brand-new
/// runtime and backend over the same store continue it, then a process that holds only the
/// starter continues that. Another caller still cannot.
async fn continuation_restart_scenario(store: DynStore) {
    // Unique per run: a shared database keeps earlier runs' rows.
    let unique = uuid_like();
    let context = format!("ctx-cont-{unique}");
    let who = Caller::new(format!("token-{unique}"));
    let stranger = Caller::new(format!("token-other-{unique}"));

    // An agent name of its own: on a shared database, no other case's worker steps these runs.
    let agent = format!("scripted-{unique}");
    let before = Rig::over_as(store.clone(), &agent);
    let worker = before.worker();
    let first = run_to(&before, &who, "one", &context, &[], TaskState::Completed).await;
    worker.stop().await;
    drop(before);

    // "Restart": a brand-new runtime and backend over the same store.
    let restarted = Rig::over_as(store.clone(), &agent);
    let worker = restarted.worker();
    let second = send(
        &restarted,
        &who,
        "two",
        Some(&context),
        &[first.id.as_str()],
    )
    .await;
    assert_eq!(earlier(&restarted, &second).await, ["one"]);
    wait_state(&restarted, &who, &second.id, TaskState::Completed).await;

    let front = Rig::front_only_as(store, &agent);
    let third = send(&front, &who, "three", Some(&context), &[second.id.as_str()]).await;
    assert_eq!(earlier(&front, &third).await, ["one", "two"]);
    wait_state(&restarted, &who, &third.id, TaskState::Completed).await;

    let theirs = send(
        &restarted,
        &stranger,
        "x",
        Some(&context),
        &[first.id.as_str()],
    )
    .await;
    assert_eq!(earlier(&restarted, &theirs).await, Vec::<String>::new());
    worker.stop().await;
}

#[tokio::test]
async fn a_continuation_survives_a_restart_in_memory() {
    continuation_restart_scenario(Arc::new(MemoryStore::new())).await;
}

/// The same against a real PostgreSQL (skipped unless `ADAM_TEST_POSTGRES_URL` is set).
#[tokio::test]
async fn a_continuation_survives_a_restart_in_postgres() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let store = adam_store_postgres::PgStore::connect(&url)
        .await
        .expect("connect to postgres");
    adam_core::Store::migrate(&store).await.expect("migrate");
    continuation_restart_scenario(Arc::new(store)).await;
}

/// A real `LlmAgent` behind the backend, split as in a deployment: a front that holds only the
/// `LlmStarter` accepts the tasks and a worker with the agent steps them. The model of the second
/// task is shown the first task's conversation, and its state says which run it continued.
async fn llm_continuation_scenario(store: DynStore) {
    let unique = uuid_like();
    let agent = format!("llm-{unique}");
    let context = format!("ctx-llm-{unique}");
    let who = Caller::new(format!("token-{unique}"));

    let model = Arc::new(MockModel::new());
    model.push_text("answer one").push_text("answer two");
    let worker_runtime = Runtime::builder(store.clone())
        .agent(LlmAgent::builder(&agent, model.clone(), "m").build())
        .poll_interval(Duration::from_millis(10))
        .build();
    let events = BroadcastSink::default();
    let front_runtime = Runtime::builder(store)
        .starter(LlmStarter::new(&agent))
        .event_sink(events.clone())
        .build();
    let front = RuntimeTaskBackend::new(front_runtime.clone(), events, agent.clone())
        .with_poll_interval(Duration::from_millis(10));
    let worker = spawn_worker(worker_runtime);

    let first = front
        .submit(who.clone(), user("first task"), None, Some(context.clone()))
        .await
        .unwrap();
    wait_task(&front, &who, &first.id, TaskState::Completed).await;
    let second = front
        .submit(
            who.clone(),
            user_refs("second task", &[first.id.as_str()]),
            None,
            Some(context.clone()),
        )
        .await
        .unwrap();
    assert_ne!(second.id, first.id);
    let done = wait_task(&front, &who, &second.id, TaskState::Completed).await;
    worker.stop().await;
    assert_eq!(
        done.status.message.as_ref().and_then(|m| m.text()),
        Some("answer two")
    );

    // What the model was asked the second time: the whole conversation so far, then the new
    // message (and the first time, only the first message).
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].messages,
        [ModelMessage::user_text("first task")]
    );
    assert_eq!(
        requests[1].messages,
        [
            ModelMessage::user_text("first task"),
            ModelMessage::assistant_text("answer one"),
            ModelMessage::user_text("second task"),
        ]
    );
    let run = RunId(second.id.parse().unwrap());
    let view = front_runtime.view(run).await.unwrap().unwrap();
    let state: Conversation = serde_json::from_value(view.state).unwrap();
    assert_eq!(state.continued_from, Some(RunId(first.id.parse().unwrap())));
    assert_eq!(state.turns, 1, "limits and counters are per task");
}

#[tokio::test]
async fn the_model_of_a_continued_task_sees_the_earlier_messages() {
    llm_continuation_scenario(Arc::new(MemoryStore::new())).await;
}

#[tokio::test]
async fn the_model_of_a_continued_task_sees_the_earlier_messages_in_postgres() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let store = adam_store_postgres::PgStore::connect(&url)
        .await
        .expect("connect to postgres");
    adam_core::Store::migrate(&store).await.expect("migrate");
    llm_continuation_scenario(Arc::new(store)).await;
}

// --------------------------------------------- what a reference can and cannot do to a request

/// A terminal record of `AGENT` in `subject`'s conversation `context` whose state is not a valid
/// envelope (a version this build does not know), written straight into the store.
async fn unreadable_record(rig: &Rig, subject: &str, context: &str) -> RunId {
    let id = RunId::new();
    rig.store
        .create_run(
            adam_core::NewRun::new(AGENT, json!({"v": 99, "agent": null}))
                .with_id(id)
                .conversation(format!("{subject}:{context}"))
                .status(adam_core::RunStatus::Done),
        )
        .await
        .expect("create");
    id
}

/// A record the caller does not own is not even decoded, so it cannot fail the request or tell the
/// caller it exists; one of the caller's own that cannot be read is skipped. Neither is an error,
/// and neither blocks a reference that can be continued.
#[tokio::test]
async fn an_unreadable_referenced_record_is_skipped_not_an_error() {
    let rig = Rig::new();
    let worker = rig.worker();
    let good = run_to(&rig, &alice(), "one", "c1", &[], TaskState::Completed).await;
    let foreign = unreadable_record(&rig, "token-1", "c1").await.to_string();
    let own = unreadable_record(&rig, "token-0", "c1").await.to_string();
    let unknown = RunId::new().to_string();

    // Bob's unreadable record looks exactly like an id that never existed.
    let t = send(&rig, &alice(), "two", Some("c1"), &[foreign.as_str()]).await;
    assert_eq!(earlier(&rig, &t).await, Vec::<String>::new());
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;
    let t = send(&rig, &alice(), "three", Some("c1"), &[unknown.as_str()]).await;
    assert_eq!(earlier(&rig, &t).await, Vec::<String>::new());
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;

    // Her own, alone: a fresh task, not a -32603.
    let t = send(&rig, &alice(), "four", Some("c1"), &[own.as_str()]).await;
    assert_eq!(earlier(&rig, &t).await, Vec::<String>::new());
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;

    // In front of one that can be continued, it is skipped and the next one is taken.
    let t = send(
        &rig,
        &alice(),
        "five",
        Some("c1"),
        &[foreign.as_str(), own.as_str(), good.id.as_str()],
    )
    .await;
    assert_eq!(earlier(&rig, &t).await, ["one"]);
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;

    // The same for a request without a message id.
    let mut message = user_refs("six", &[own.as_str(), good.id.as_str()]);
    message.message_id = String::new();
    let t = rig
        .backend
        .submit(alice(), message, None, Some("c1".into()))
        .await
        .unwrap();
    assert_eq!(earlier(&rig, &t).await, ["one"]);
    worker.stop().await;
}

/// The anonymous caller is every client of an unauthenticated server at once, so what one of them
/// said is not another's history: it continues nothing, and gets a fresh task.
#[tokio::test]
async fn the_anonymous_caller_continues_nothing() {
    let rig = Rig::new();
    let worker = rig.worker();
    let anonymous = Caller::anonymous();
    let first = run_to(&rig, &anonymous, "one", "c1", &[], TaskState::Completed).await;
    let second = send(&rig, &anonymous, "two", Some("c1"), &[first.id.as_str()]).await;
    assert_ne!(second.id, first.id);
    assert_eq!(earlier(&rig, &second).await, Vec::<String>::new());
    wait_state(&rig, &anonymous, &second.id, TaskState::Completed).await;

    // The same request from an authenticated subject is continued, so it is the caller, and not
    // the shape of the request, that decides.
    let first = run_to(&rig, &alice(), "one", "c2", &[], TaskState::Completed).await;
    let second = send(&rig, &alice(), "two", Some("c2"), &[first.id.as_str()]).await;
    assert_eq!(earlier(&rig, &second).await, ["one"]);
    worker.stop().await;
}

/// Two messages that both continue the same finished task at once: one run is created, and the
/// other message is delivered to it, as two messages to an open context always were.
async fn concurrent_continuations_scenario(store: DynStore) {
    let unique = uuid_like();
    let agent = format!("scripted-{unique}");
    let who = Caller::new(format!("token-{unique}"));
    for round in 0..8 {
        let context = format!("ctx-{unique}-{round}");
        let rig = Rig::over_as(store.clone(), &agent);
        let worker = rig.worker();
        let first = run_to(&rig, &who, "one", &context, &[], TaskState::Completed).await;
        // Nothing steps what starts from here on, so the created task stays open.
        worker.stop().await;

        let submit = |text: &'static str, id: &str| {
            let mut message = user_refs(text, &[first.id.as_str()]);
            message.message_id = id.to_owned();
            let backend = rig.backend.clone();
            let (who, context) = (who.clone(), context.clone());
            tokio::spawn(async move { backend.submit(who, message, None, Some(context)).await })
        };
        let (a, b) = (submit("two-a", "m-a"), submit("two-b", "m-b"));
        let (a, b) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());

        assert_eq!(a.id, b.id, "round {round}: one task took both messages");
        let derived = |message_id: &str| {
            adam_a2a_runtime::task_id_for(&agent, &who.subject, Some(&context), message_id)
                .to_string()
        };
        let (from_a, from_b) = (derived("m-a"), derived("m-b"));
        assert!(a.id == from_a || a.id == from_b, "round {round}");
        let other = if a.id == from_a { &from_b } else { &from_a };
        assert!(
            rig.backend.get(&who, other).await.unwrap().is_none(),
            "round {round}: the losing message made no task of its own"
        );
        // The task that won continued the finished one, and the other message is in its inbox.
        assert_eq!(earlier(&rig, &a).await, ["one"], "round {round}");
        assert_eq!(pending(&rig, &a).await, 1, "round {round}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_continuing_submissions_make_one_task_and_the_other_joins() {
    concurrent_continuations_scenario(Arc::new(MemoryStore::new())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_continuing_submissions_make_one_task_in_postgres() {
    let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") else {
        return;
    };
    let store = adam_store_postgres::PgStore::connect(&url)
        .await
        .expect("connect to postgres");
    adam_core::Store::migrate(&store).await.expect("migrate");
    concurrent_continuations_scenario(Arc::new(store)).await;
}

/// The wire: `referenceTaskIds` in a JSON-RPC `SendMessage` reaches the backend and continues the
/// task, through the official client and the server's own (de)serialisation.
#[tokio::test]
async fn reference_task_ids_travel_over_http_and_continue() {
    use a2a::{SendMessageRequest, SendMessageResponse};
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
    let send = |message: Message| {
        let client = &client;
        async move {
            let response = client
                .send_message(&SendMessageRequest {
                    message,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                })
                .await
                .unwrap();
            let SendMessageResponse::Task(task) = response else {
                panic!("expected a task")
            };
            task
        }
    };
    let in_context = |text: &str, references: &[&str]| {
        let mut message = user_refs(text, references);
        message.context_id = Some("c-http".into());
        message
    };

    let first = send(in_context("one", &[])).await;
    // The server subject of the bearer token is "token-0".
    wait_state(&rig, &alice(), &first.id, TaskState::Completed).await;
    assert_eq!(earlier(&rig, &first).await, Vec::<String>::new());

    let second = send(in_context("two", &[first.id.as_str()])).await;
    assert_ne!(second.id, first.id);
    assert_eq!(second.context_id, "c-http");
    assert_eq!(earlier(&rig, &second).await, ["one"]);
    wait_state(&rig, &alice(), &second.id, TaskState::Completed).await;

    // Over the same wire, no reference is a fresh task.
    let third = send(in_context("three", &[])).await;
    assert_eq!(earlier(&rig, &third).await, Vec::<String>::new());
    worker.stop().await;
}

// ------------------------------------------------------------------ what the operator sees

/// Log lines of the test's thread, as text.
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    fn capture(level: tracing::Level) -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(level)
                .with_ansi(false)
                .with_writer(logs.clone())
                .finish(),
        );
        (logs, guard)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// How many lines say that references continued nothing.
fn unmatched_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| l.contains("none of the referenceTaskIds could be continued"))
        .count()
}

/// The line is for a request that started a fresh task though it named references, and it is said
/// once: not when the message is delivered to the open task of its context (where a reference
/// means nothing), not when a reference was continued, and not again for a repeat of the request.
#[tokio::test]
async fn the_line_about_references_that_continued_nothing_is_said_once_for_the_real_outcome() {
    let rig = Rig::new();
    let worker = rig.worker();
    let nowhere = RunId::new().to_string();

    // Delivered to the open task of the context: no line.
    let open = send(&rig, &alice(), "[input]", Some("c1"), &[]).await;
    wait_state(&rig, &alice(), &open.id, TaskState::InputRequired).await;
    let (logs, guard) = Logs::capture(tracing::Level::INFO);
    let delivered = send(&rig, &alice(), "red", Some("c1"), &[nowhere.as_str()]).await;
    drop(guard);
    assert_eq!(delivered.id, open.id, "it joined the open task");
    assert_eq!(unmatched_lines(&logs.text()), 0, "{}", logs.text());
    wait_state(&rig, &alice(), &open.id, TaskState::Completed).await;

    // A reference that was continued: no line.
    let (logs, guard) = Logs::capture(tracing::Level::INFO);
    let next = send(&rig, &alice(), "again", Some("c1"), &[open.id.as_str()]).await;
    drop(guard);
    assert_eq!(unmatched_lines(&logs.text()), 0, "{}", logs.text());
    wait_state(&rig, &alice(), &next.id, TaskState::Completed).await;

    // A fresh task though a reference was named: one line. The same request again (the same
    // message id) finds its task and says nothing more.
    let mut message = user_refs("fresh", &[nowhere.as_str()]);
    message.message_id = "m-fresh".into();
    let (logs, guard) = Logs::capture(tracing::Level::INFO);
    let first = rig
        .backend
        .submit(alice(), message.clone(), None, Some("c2".into()))
        .await
        .unwrap();
    assert_eq!(unmatched_lines(&logs.text()), 1, "{}", logs.text());
    let repeat = rig
        .backend
        .submit(alice(), message, None, Some("c2".into()))
        .await
        .unwrap();
    drop(guard);
    assert_eq!(repeat.id, first.id, "a repeat, not a second task");
    assert_eq!(
        unmatched_lines(&logs.text()),
        1,
        "the repeat said nothing: {}",
        logs.text()
    );
    wait_state(&rig, &alice(), &first.id, TaskState::Completed).await;
    worker.stop().await;
}

/// A request that named references and got no continuation from any leaves one line at the
/// default level with a count per reason, and not one id of another caller's task; at debug the
/// references are shown escaped and cut short.
#[tokio::test]
async fn references_that_all_miss_leave_one_line_with_counts_and_no_foreign_ids() {
    let rig = Rig::new();
    let worker = rig.worker();
    let bobs = run_to(&rig, &bob(), "secret", "c1", &[], TaskState::Completed).await;
    let elsewhere = run_to(&rig, &alice(), "there", "c9", &[], TaskState::Completed).await;
    let own_unreadable = unreadable_record(&rig, "token-0", "c1").await.to_string();
    let hostile = format!("line one\nline two {}", "z".repeat(200));
    let references = [
        RunId::new().to_string(),
        "not-a-task-id".to_owned(),
        bobs.id.clone(),
        elsewhere.id.clone(),
        own_unreadable,
        hostile.clone(),
    ];
    let refs: Vec<&str> = references.iter().map(String::as_str).collect();

    let (info, guard) = Logs::capture(tracing::Level::INFO);
    let t = send(&rig, &alice(), "next", Some("c1"), &refs).await;
    drop(guard);
    assert_eq!(earlier(&rig, &t).await, Vec::<String>::new());
    let text = info.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("none of the referenceTaskIds could be continued"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    for count in [
        "given=6",
        "malformed=2",
        "unknown=1",
        "not_the_callers=1",
        "other_context=1",
        "unreadable=1",
    ] {
        assert!(lines[0].contains(count), "{count} in {}", lines[0]);
    }
    assert!(
        !text.contains(&bobs.id),
        "no id of another caller's task: {text}"
    );
    // The unreadable record of the caller's own is worth a warning of its own.
    assert!(text.contains("unreadable state"), "{text}");
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;

    // At debug each one is shown, escaped and cut short (what a client sends is not trusted).
    let (debug, guard) = Logs::capture(tracing::Level::DEBUG);
    let t = send(&rig, &alice(), "again", Some("c1"), &[hostile.as_str()]).await;
    drop(guard);
    let text = debug.text();
    assert!(text.contains(r"line one\nline two"), "{text}");
    assert!(!text.contains(&hostile), "cut short: {text}");
    assert!(text.contains("..."), "{text}");
    wait_state(&rig, &alice(), &t.id, TaskState::Completed).await;
    worker.stop().await;
}
