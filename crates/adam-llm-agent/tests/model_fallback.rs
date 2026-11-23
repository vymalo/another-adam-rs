//! The switch to the fallback model (ADR 0064, amended 2026-11-23): a model call the endpoint
//! refuses as locked ([`ModelError::ModelLocked`]) is made again, once, to the agent's fallback with
//! its profile's output cap and reasoning level; the run says so as a step, reports its calls under
//! the fallback and never goes back; a second lock fails the run naming both models. A scripted
//! `MockModel`, a real `Runtime` with a worker.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, FnTool, LlmAgent, LlmAgentBuilder, MODEL_CONTEXT_KEY, ModelBinding, ToolError,
    user_message,
};
use adam_model::{DynModel, MockModel, ModelError, ModelRequest, ReasoningEffort};
use adam_runtime::{
    CollectingSink, Inbound, ManualClock, RetryPolicy, RunEvent, RunView, Runtime, StepState,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

const BASE: &str = "big-model";
const LITE: &str = "lite-model";

struct Rig {
    mock: Arc<MockModel>,
    sink: CollectingSink,
    store: DynStore,
    rt: Runtime,
    stop: Option<oneshot::Sender<()>>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Rig {
    fn new(configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder) -> Self {
        Self::over(Arc::new(MemoryStore::new()), configure)
    }

    fn over(store: DynStore, configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder) -> Self {
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent: LlmAgent = configure(LlmAgent::builder("llm", model, BASE)).build();
        let sink = CollectingSink::new();
        let rt = Runtime::builder(store.clone())
            .clock(ManualClock::new())
            .event_sink(sink.clone())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .lease_ttl(Duration::from_secs(10))
            .retry(RetryPolicy::new(3).with_backoff(Duration::ZERO, Duration::ZERO))
            .agent(agent)
            .build();
        let (stop, rx) = oneshot::channel::<()>();
        let worker_rt = rt.clone();
        let worker = tokio::spawn(async move {
            worker_rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await
                .expect("worker result");
        });
        Self {
            mock,
            sink,
            store,
            rt,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    async fn start(&self) -> RunId {
        self.rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start")
    }

    async fn start_with(&self, payload: Value) -> RunId {
        self.rt
            .start("llm", Inbound::new("message", payload), None)
            .await
            .expect("start")
    }

    async fn until(&self, run: RunId, status: RunStatus) -> RunView {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = self.rt.view(run).await.expect("view").expect("run exists");
            if view.status == status {
                return view;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {status:?}; last view: {view:#?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn state(&self, run: RunId) -> Conversation {
        let state = self.rt.state(run).await.expect("state").expect("the run");
        serde_json::from_value(state).expect("a Conversation")
    }

    async fn journal(&self, run: RunId) -> String {
        let entries = self.store.journal_list(run).await.expect("journal");
        serde_json::to_string(
            &entries
                .iter()
                .map(|e| json!({"name": e.name, "ok": e.ok, "payload": e.payload}))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            tokio::time::timeout(Duration::from_secs(10), worker)
                .await
                .expect("worker stops in time")
                .expect("worker task");
        }
    }
}

fn locked(model: &str) -> ModelError {
    ModelError::ModelLocked {
        model: model.into(),
    }
}

fn lite() -> ModelBinding {
    ModelBinding::new(LITE, 2048).reasoning_effort(Some(ReasoningEffort::Low))
}

fn models(requests: &[ModelRequest]) -> Vec<&str> {
    requests.iter().map(|r| r.model.as_str()).collect()
}

fn text_of(view: &RunView) -> Option<String> {
    view.output
        .as_ref()
        .and_then(|o| o.get("text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The decisive run: the first call is refused as locked, the same turn is asked again of the
/// fallback with the fallback's cap and level, the run says so as a step, its usage names the
/// fallback, and the state keeps the one switch.
#[tokio::test]
async fn a_locked_model_continues_once_on_the_fallback() {
    let rig = Rig::new(|b| b.model_fallback(lite()));
    rig.mock.push_error(locked(BASE)).push_text("answered");
    let run = rig.start().await;
    let view = rig.until(run, RunStatus::Done).await;
    assert_eq!(text_of(&view).as_deref(), Some("answered"));

    let requests = rig.mock.requests();
    assert_eq!(
        models(&requests),
        [BASE, LITE],
        "asked again, of the fallback"
    );
    assert_eq!(requests[1].max_output_tokens, Some(2048));
    assert_eq!(requests[1].reasoning_effort, Some(ReasoningEffort::Low));
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>(),
        requests[1]
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>(),
        "the same history"
    );

    let events = rig.sink.events_for(run);
    let said = events.iter().find_map(|e| match e {
        RunEvent::Step(s) if s.label == "Switched model" => Some(s.clone()),
        _ => None,
    });
    let said = said.expect("a step says the model was switched");
    assert_eq!(said.state, StepState::Completed);
    let detail = said.detail.unwrap();
    assert!(detail.contains(BASE) && detail.contains(LITE), "{detail}");
    let usage: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Usage(u) => Some(u.model.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        usage,
        [LITE],
        "only the call that answered is reported, under the fallback"
    );

    let state = rig.state(run).await;
    let switch = state.model_switch.expect("the switch is kept");
    assert_eq!((switch.from.as_str(), switch.to.as_str()), (BASE, LITE));
    assert!(
        rig.journal(run).await.contains(BASE),
        "the refusal is in the journal"
    );
    rig.stop().await;
}

/// No switch back: after the switch every later call, tool turns included, names the fallback.
#[tokio::test]
async fn the_run_never_switches_back() {
    let rig = Rig::new(|b| {
        b.model_fallback(lite()).tool(FnTool::raw(
            "noop",
            "does nothing",
            json!({"type": "object", "properties": {}}),
            |_ctx, _args| async { Ok::<_, ToolError>("ok".to_owned()) },
        ))
    });
    rig.mock
        .push_error(locked(BASE))
        .push_tool_calls(vec![adam_model::ToolCall::new("c1", "noop", json!({}))])
        .push_text("done");
    let run = rig.start().await;
    rig.until(run, RunStatus::Done).await;
    assert_eq!(models(&rig.mock.requests()), [BASE, LITE, LITE]);
    rig.stop().await;
}

/// A second lock, on the fallback, fails the run naming both models, and asks no third time.
#[tokio::test]
async fn a_second_lock_fails_the_run_naming_both_models() {
    let rig = Rig::new(|b| b.model_fallback(lite()));
    rig.mock.push_error(locked(BASE)).push_error(locked(LITE));
    let run = rig.start().await;
    let error = rig.until(run, RunStatus::Failed).await.error.unwrap();
    assert!(
        error.starts_with("model call failed: ")
            && error.contains(&format!("`{BASE}`"))
            && error.contains(&format!("`{LITE}`")),
        "{error}"
    );
    assert_eq!(models(&rig.mock.requests()), [BASE, LITE]);
    rig.stop().await;
}

/// An agent with no fallback fails as before.
#[tokio::test]
async fn without_a_fallback_a_locked_model_fails_the_run_as_before() {
    let rig = Rig::new(|b| b);
    rig.mock.push_error(locked(BASE));
    let run = rig.start().await;
    let error = rig.until(run, RunStatus::Failed).await.error.unwrap();
    assert_eq!(
        error,
        format!("model call failed: the model endpoint does not allow the model `{BASE}`")
    );
    assert_eq!(rig.mock.requests().len(), 1);
    rig.stop().await;
}

/// A message may pick a model the agent has a binding for, with that binding's cap and level; one it
/// has none for, and an agent given no choices, answer from the agent's own model.
#[tokio::test]
async fn a_message_may_pick_only_a_model_the_agent_was_given() {
    let pick =
        |alias: &str| json!({"text": "go", "context": {MODEL_CONTEXT_KEY: {"model": alias}}});
    let rig = Rig::new(|b| b.model_choices(vec![lite()]));
    rig.mock.push_text("a").push_text("b");
    let run = rig.start_with(pick(LITE)).await;
    rig.until(run, RunStatus::Done).await;
    let run = rig.start_with(pick("unknown")).await;
    rig.until(run, RunStatus::Done).await;
    let requests = rig.mock.requests();
    assert_eq!(models(&requests), [LITE, BASE]);
    assert_eq!(requests[0].max_output_tokens, Some(2048));
    rig.stop().await;

    let rig = Rig::new(|b| b);
    rig.mock.push_text("a");
    let run = rig.start_with(pick(LITE)).await;
    rig.until(run, RunStatus::Done).await;
    assert_eq!(models(&rig.mock.requests()), [BASE]);
    rig.stop().await;
}

/// A crash between the refused call's step and the commit of the switch: the transition is replayed
/// from the journal, which holds the same refusal, so the replay makes no second call to the locked
/// model and the requests are the ones of a run that did not crash.
#[tokio::test]
async fn a_switch_replayed_after_a_lost_commit_gives_the_same_requests() {
    use adam_store_testkit::fault::{FaultyStore, Method};

    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = faulty.clone();
    // The worker must not start before the fault is armed: build, arm, then start the run.
    let rig = Rig::over(store, |b| b.model_fallback(lite()));
    rig.mock.push_error(locked(BASE)).push_text("answered");
    let run = rig.start().await;
    faulty.fail_run(Method::CommitRun, run, 1);
    let view = rig.until(run, RunStatus::Done).await;
    assert_eq!(
        faulty.injected(Method::CommitRun),
        1,
        "the commit was lost once"
    );
    assert_eq!(text_of(&view).as_deref(), Some("answered"));
    assert_eq!(
        models(&rig.mock.requests()),
        [BASE, LITE],
        "the replay of the lost transition asked the locked model nothing"
    );
    assert_eq!(
        rig.state(run).await.model_switch.map(|s| s.to),
        Some(LITE.to_owned())
    );
    rig.stop().await;
}
