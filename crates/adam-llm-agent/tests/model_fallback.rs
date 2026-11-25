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
    COMPACTION_TURN, CompactionReason, Conversation, FnTool, Limits, LlmAgent, LlmAgentBuilder,
    MODEL_CONTEXT_KEY, ModelBinding, ModelPark, ToolError, user_message,
};
use adam_model::{
    DynModel, MockModel, ModelError, ModelRequest, Park, ReasoningEffort, ToolCall, ToolChoice,
};
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
        Self::with(MockModel::new(), store, configure)
    }

    fn with(
        mock: MockModel,
        store: DynStore,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        let mock = Arc::new(mock);
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

fn over_limit() -> ModelError {
    ModelError::Refused {
        status: 402,
        slug: Some("over_limit".into()),
        retry_after: None,
        park: Park::UntilWoken,
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

/// A 402 — the person's usage limit is reached for the model the call named — takes the switch a
/// `model_locked` refusal takes (ADR 0064, amended 2026-11-25): the same turn is asked once of the
/// fallback, the run says so as one step, and it completes instead of parking on the first call.
#[tokio::test]
async fn a_limit_reached_on_the_primary_continues_once_on_the_fallback() {
    let rig = Rig::new(|b| b.model_fallback(lite()));
    rig.mock.push_error(over_limit()).push_text("answered");
    let run = rig.start().await;
    let view = rig.until(run, RunStatus::Done).await;
    assert_eq!(text_of(&view).as_deref(), Some("answered"));

    assert_eq!(
        models(&rig.mock.requests()),
        [BASE, LITE],
        "the fallback is asked after the refusal"
    );
    let events = rig.sink.events_for(run);
    let switches = events
        .iter()
        .filter(|e| matches!(e, RunEvent::Step(s) if s.label == "Switched model"))
        .count();
    assert_eq!(switches, 1, "exactly one switch");
    let state = rig.state(run).await;
    let switch = state.model_switch.expect("the switch is kept");
    assert_eq!((switch.from.as_str(), switch.to.as_str()), (BASE, LITE));
    assert!(state.model_wait.is_none(), "the run did not park");
    rig.stop().await;
}

/// A 402 on the fallback too: the run has no unlocked model left after its one switch, and parks
/// as over limit instead of failing.
#[tokio::test]
async fn a_limit_reached_on_the_fallback_too_parks_the_run_as_over_limit() {
    let rig = Rig::new(|b| b.model_fallback(lite()));
    rig.mock.push_error(over_limit()).push_error(over_limit());
    let run = rig.start().await;
    rig.until(run, RunStatus::Parked).await;
    assert_eq!(
        models(&rig.mock.requests()),
        [BASE, LITE],
        "asked once each"
    );
    let state = rig.state(run).await;
    let wait = state.model_wait.expect("the run waits on the person");
    assert_eq!(wait.reason, "the model gateway's limit is reached");
    assert_eq!(
        (wait.status, wait.slug.as_deref()),
        (Some(402), Some("over_limit"))
    );
    assert_eq!(wait.park, ModelPark::UntilWoken);
    assert!(
        state.model_switch.is_some(),
        "the one switch is kept in the state"
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

/// A tool whose answer is `n` letters: results of a size the test chooses.
fn big_tool(b: LlmAgentBuilder) -> LlmAgentBuilder {
    b.tool(FnTool::raw(
        "big",
        "answers a text of the size asked",
        json!({"type": "object"}),
        |_ctx, args| async move {
            let n = args["n"].as_u64().unwrap_or(10) as usize;
            let tag = args["tag"].as_str().unwrap_or("t");
            Ok::<_, ToolError>(format!("{tag}:{}", "x".repeat(n)))
        },
    ))
}

fn big(id: &str, n: u64) -> ToolCall {
    ToolCall::new(id, "big", json!({"n": n, "tag": id}))
}

fn is_summary(request: &ModelRequest) -> bool {
    request.tool_choice == ToolChoice::None
        && request
            .messages
            .last()
            .is_some_and(|m| m.text().ends_with(COMPACTION_TURN))
}

/// The tokens a request holds, estimated as the agent does (characters / 4).
fn tokens(request: &ModelRequest) -> usize {
    request
        .messages
        .iter()
        .map(|m| m.text().len())
        .sum::<usize>()
        / 4
}

/// After the switch the compaction budget, and so what the fallback is sent, are the fallback's own
/// window's, not the agent's: a 70,000-token history fits the agent's 200,000-token window, not the
/// fallback's 64,000, so it is summarised before the fallback is asked.
#[tokio::test]
async fn a_smaller_fallback_is_sent_a_history_that_fits_its_own_window() {
    let mock = MockModel::new()
        .with_context_window(BASE, 200_000)
        .with_context_window(LITE, 64_000);
    let rig = Rig::with(mock, Arc::new(MemoryStore::new()), |b| {
        big_tool(b)
            .limits(Limits {
                max_output_tokens: 1024,
                ..Limits::default()
            })
            .model_fallback(ModelBinding::new(LITE, 1024))
    });
    for n in 0..20 {
        rig.mock
            .push_tool_calls(vec![big(&format!("c{n}"), 14_000)]);
    }
    rig.mock
        .push_error(locked(BASE))
        .push_text("SUMMARY")
        .push_text("answered");
    let run = rig.start().await;
    let view = rig.until(run, RunStatus::Done).await;
    assert_eq!(text_of(&view).as_deref(), Some("answered"));
    let requests = rig.mock.requests();
    let on_lite: Vec<&ModelRequest> = requests.iter().filter(|r| r.model == LITE).collect();
    assert!(
        on_lite.first().is_some_and(|r| is_summary(r)),
        "the history is summarised on the fallback first: {:?}",
        models(&requests)
    );
    let answered = on_lite.last().unwrap();
    assert!(
        tokens(answered) + 1024 < 64_000,
        "the request to the fallback holds {} tokens",
        tokens(answered)
    );
    rig.stop().await;
}

/// A summary call the endpoint refuses as locked switches the run too, and is asked again of the
/// fallback: a budget compaction.
#[tokio::test]
async fn a_lock_on_a_budget_summary_switches_and_the_summary_is_made_on_the_fallback() {
    let rig = Rig::with(MockModel::new(), Arc::new(MemoryStore::new()), |b| {
        big_tool(b)
            .limits(Limits {
                max_history_tokens: 1200,
                ..Limits::default()
            })
            .model_fallback(lite())
    });
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 1200)]);
    }
    rig.mock
        .push_error(locked(BASE))
        .push_text("SUMMARY")
        .push_text("answered");
    let run = rig.start().await;
    let view = rig.until(run, RunStatus::Done).await;
    let requests = rig.mock.requests();
    assert_eq!(text_of(&view).as_deref(), Some("answered"));
    let summaries: Vec<&str> = requests
        .iter()
        .filter(|r| is_summary(r))
        .map(|r| r.model.as_str())
        .collect();
    assert_eq!(summaries, [BASE, LITE], "{:?}", models(&requests));
    let state = rig.state(run).await;
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(state.compactions[0].reason, CompactionReason::Budget);
    assert!(state.model_switch.is_some());
    rig.stop().await;
}

/// The same, for the summary that follows a prompt the provider rejected as too long: the run does
/// not fail while a fallback exists.
#[tokio::test]
async fn a_lock_on_an_overflow_summary_switches_instead_of_failing_the_run() {
    let rig = Rig::with(MockModel::new(), Arc::new(MemoryStore::new()), |b| {
        big_tool(b)
            .limits(Limits {
                max_history_tokens: 0,
                ..Limits::default()
            })
            .model_fallback(lite())
    });
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 3000)]);
    }
    rig.mock
        .push_error(ModelError::ContextLength("prompt is too long".into()))
        .push_error(locked(BASE))
        .push_text("SUMMARY")
        .push_text("answered");
    let run = rig.start().await;
    let view = rig.until(run, RunStatus::Done).await;
    assert_eq!(text_of(&view).as_deref(), Some("answered"));
    let state = rig.state(run).await;
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(state.compactions[0].reason, CompactionReason::Overflow);
    assert_eq!(
        rig.mock.requests().last().map(|r| r.model.as_str()),
        Some(LITE)
    );
    rig.stop().await;
}

/// A message that picks the fallback itself, and is refused as locked there, fails at once: there
/// is no other model to continue on, and no second call is made to the locked one.
#[tokio::test]
async fn a_lock_on_the_model_that_is_also_the_fallback_fails_without_a_second_call() {
    let rig = Rig::new(|b| b.model_fallback(lite()).model_choices(vec![lite()]));
    rig.mock.push_error(locked(LITE)).push_text("never asked");
    let run = rig
        .start_with(json!({"text": "go", "context": {MODEL_CONTEXT_KEY: {"model": LITE}}}))
        .await;
    let error = rig.until(run, RunStatus::Failed).await.error.unwrap();
    assert!(error.contains(&format!("`{LITE}`")), "{error}");
    assert_eq!(models(&rig.mock.requests()), [LITE], "no second call");
    assert!(rig.state(run).await.model_switch.is_none());
    rig.stop().await;
}
