//! The time limit of a tool call: a call past it is stopped and answered with an error result, the
//! answer is journaled, so a replay never waits.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, DEFAULT_TOOL_TIME_LIMIT, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput,
    user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, RunEvent, RunView, Runtime, StepState};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
        ..Default::default()
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object"}),
    }
}

/// A tool named `hang` that never finishes by itself. It declares `limit`; when `listens` it stops
/// as soon as its token fires (and then answers, which the agent must not use), otherwise it
/// ignores the token.
struct Hang {
    limit: Option<Duration>,
    listens: bool,
    calls: Arc<AtomicUsize>,
    token_fired: Arc<AtomicBool>,
}

impl Hang {
    fn new(limit: Option<Duration>, listens: bool) -> (Self, Arc<AtomicUsize>, Arc<AtomicBool>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let token_fired = Arc::new(AtomicBool::new(false));
        (
            Self {
                limit,
                listens,
                calls: calls.clone(),
                token_fired: token_fired.clone(),
            },
            calls,
            token_fired,
        )
    }
}

#[async_trait]
impl Tool for Hang {
    fn spec(&self) -> ToolSpec {
        spec("hang")
    }
    fn time_limit(&self) -> Option<Duration> {
        self.limit
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        if self.listens {
            ctx.cancelled().await;
            self.token_fired.store(true, SeqCst);
            return Ok(ToolOutput::text("late answer, never used"));
        }
        std::future::pending::<()>().await;
        Ok(ToolOutput::text("unreachable"))
    }
}

/// A tool named `sleeper` that sleeps `for_` and answers "woke up".
struct Sleeper(Duration);

#[async_trait]
impl Tool for Sleeper {
    fn spec(&self) -> ToolSpec {
        spec("sleeper")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(self.0).await;
        Ok(ToolOutput::text("woke up"))
    }
}

fn runtime(
    agent: &LlmAgent,
    store: &DynStore,
    worker: &str,
    sink: CollectingSinkOrHold,
) -> Runtime {
    let builder = Runtime::builder(store.clone())
        .agent(agent.clone())
        .worker_id(worker)
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_millis(300));
    match sink {
        CollectingSinkOrHold::Collect(sink) => builder.event_sink(sink),
        CollectingSinkOrHold::Hold(sink) => builder.event_sink(sink),
    }
    .build()
}

enum CollectingSinkOrHold {
    Collect(CollectingSink),
    Hold(Arc<HoldAtStepEnd>),
}

fn spawn_worker(
    rt: &Runtime,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
) {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
    });
    (stop, handle)
}

async fn finish(rt: &Runtime, run: RunId) -> RunView {
    let (stop, handle) = spawn_worker(rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    let view = loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            break view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let _ = stop.send(());
    handle.await.expect("worker task").expect("worker result");
    view
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

fn agent_with(mock: &Arc<MockModel>, tools: Vec<Arc<dyn Tool>>) -> LlmAgent {
    let model: DynModel = mock.clone();
    let mut builder = LlmAgent::builder("llm", model, "test-model");
    for tool in tools {
        builder = builder.dyn_tool(tool);
    }
    builder.build()
}

fn store() -> DynStore {
    Arc::new(MemoryStore::new())
}

const SAID_120: &str = "`hang` did not finish within 120 s and was stopped. What it had done is not \
                        undone: look at the state again before relying on it, and try a smaller step.";

#[tokio::test(start_paused = true)]
async fn a_tool_past_its_limit_is_an_error_result_and_its_token_fires() {
    let mock = Arc::new(MockModel::new());
    let (hang, calls, fired) = Hang::new(Some(Duration::from_secs(120)), true);
    let agent = agent_with(&mock, vec![Arc::new(hang)]);
    mock.push_tool_calls(vec![call("c1", "hang")])
        .push_text("it did not finish");
    let sink = CollectingSink::new();
    let rt = runtime(
        &agent,
        &store(),
        "w",
        CollectingSinkOrHold::Collect(sink.clone()),
    );
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let view = finish(&rt, run).await;

    assert_eq!(view.status, RunStatus::Done, "the run goes on");
    assert_eq!(calls.load(SeqCst), 1);
    assert!(
        fired.load(SeqCst),
        "the call's token fired when the limit passed"
    );
    assert_eq!(
        conversation(&view).messages[2],
        Message::tool_error("c1", SAID_120),
        "the late answer is not the result"
    );
    assert_eq!(view.output.unwrap()["text"], "it did not finish");
    let failed_step = sink.events_for(run).into_iter().find_map(|e| match e {
        RunEvent::Step(s) if s.state == StepState::Failed => Some(s),
        _ => None,
    });
    let output = failed_step.expect("the step ended failed").output;
    assert_eq!(output.expect("with the error").text, SAID_120);
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_ignores_its_token_is_dropped_after_the_grace() {
    let mock = Arc::new(MockModel::new());
    let (hang, calls, _) = Hang::new(Some(Duration::from_secs(10)), false);
    let agent = agent_with(&mock, vec![Arc::new(hang)]);
    mock.push_tool_calls(vec![call("c1", "hang")])
        .push_text("done");
    let rt = runtime(
        &agent,
        &store(),
        "w",
        CollectingSinkOrHold::Collect(CollectingSink::new()),
    );
    let started = tokio::time::Instant::now();
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let view = finish(&rt, run).await;

    assert_eq!(calls.load(SeqCst), 1);
    assert!(
        started.elapsed() >= Duration::from_secs(15),
        "the limit and the grace both passed: {:?}",
        started.elapsed()
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    let Message::Tool {
        content, is_error, ..
    } = &conversation(&view).messages[2]
    else {
        panic!("not a tool result")
    };
    assert!(
        *is_error && content.contains("did not finish within 10 s"),
        "{content}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_undeclared_tool_gets_300_s() {
    assert_eq!(DEFAULT_TOOL_TIME_LIMIT, Duration::from_secs(300));
    let mock = Arc::new(MockModel::new());
    let (hang, _, _) = Hang::new(None, true);
    // Quicker than the limit it is not cut: `sleeper` takes 299 s and answers.
    let agent = agent_with(
        &mock,
        vec![Arc::new(hang), Arc::new(Sleeper(Duration::from_secs(299)))],
    );
    mock.push_tool_calls(vec![call("c1", "sleeper"), call("c2", "hang")])
        .push_text("done");
    let rt = runtime(
        &agent,
        &store(),
        "w",
        CollectingSinkOrHold::Collect(CollectingSink::new()),
    );
    let started = tokio::time::Instant::now();
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let view = finish(&rt, run).await;

    let messages = conversation(&view).messages;
    assert_eq!(messages[2], Message::tool_result("c1", "woke up"));
    let Message::Tool {
        content, is_error, ..
    } = &messages[3]
    else {
        panic!("not a tool result")
    };
    assert!(
        *is_error && content.contains("did not finish within 300 s"),
        "{content}"
    );
    assert!(started.elapsed() >= Duration::from_secs(599));
}

/// Hangs the worker that emits the end of a failed step: the call is journaled, and its result is
/// not committed yet.
struct HoldAtStepEnd {
    inner: CollectingSink,
    reached: Arc<Notify>,
    armed: AtomicBool,
}

#[async_trait]
impl adam_runtime::EventSink for HoldAtStepEnd {
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent) {
        let at_end = matches!(&event, RunEvent::Step(s) if s.state == StepState::Failed);
        self.inner.emit(run, agent, event).await;
        if at_end && self.armed.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

#[tokio::test]
async fn a_timed_out_call_replays_without_waiting() {
    let mock = Arc::new(MockModel::new());
    let (hang, calls, _) = Hang::new(Some(Duration::from_millis(100)), true);
    let agent = agent_with(&mock, vec![Arc::new(hang)]);
    mock.push_tool_calls(vec![call("c1", "hang")])
        .push_text("done");
    let store = store();
    let reached = Arc::new(Notify::new());
    let hold = Arc::new(HoldAtStepEnd {
        inner: CollectingSink::new(),
        reached: reached.clone(),
        armed: AtomicBool::new(true),
    });
    let rt_a = runtime(&agent, &store, "crash-a", CollectingSinkOrHold::Hold(hold));
    let rt_b = runtime(
        &agent,
        &store,
        "crash-b",
        CollectingSinkOrHold::Collect(CollectingSink::new()),
    );
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let (_stop, doomed) = spawn_worker(&rt_a);
    tokio::time::timeout(Duration::from_secs(20), reached.notified())
        .await
        .expect("the timed-out call is journaled");
    doomed.abort();
    assert!(doomed.await.expect_err("aborted").is_cancelled());
    assert_eq!(calls.load(SeqCst), 1);

    let view = finish(&rt_b, run).await;

    assert_eq!(
        calls.load(SeqCst),
        1,
        "the recorded error is replayed: the tool is not called, nothing waits"
    );
    let Message::Tool {
        content, is_error, ..
    } = &conversation(&view).messages[2]
    else {
        panic!("not a tool result")
    };
    assert!(
        *is_error && content.contains("did not finish within 100 ms"),
        "{content}"
    );
}
