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
use adam_runtime::{CollectingSink, ManualClock, RunEvent, RunView, Runtime, StepState};
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
            return Err(ToolError::Permanent("stopped".into()));
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

async fn conversation(rt: &Runtime, view: &RunView) -> Conversation {
    let state = rt.state(view.id).await.expect("state").expect("the run");
    serde_json::from_value(state).expect("state is a Conversation")
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
        conversation(&rt, &view).await.messages[2],
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
    } = &conversation(&rt, &view).await.messages[2]
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

    let messages = conversation(&rt, &view).await.messages;
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

#[tokio::test(start_paused = true)]
async fn a_timed_out_call_replays_without_waiting() {
    let mock = Arc::new(MockModel::new());
    let (hang, calls, _) = Hang::new(Some(Duration::from_secs(120)), true);
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
    tokio::time::timeout(Duration::from_secs(3600), reached.notified())
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
    } = &conversation(&rt_b, &view).await.messages[2]
    else {
        panic!("not a tool result")
    };
    assert!(
        *is_error && content.contains("did not finish within 120 s"),
        "{content}"
    );
}

/// Starts the agent `child` with a time limit of a minute and waits for it.
struct Delegate;

#[async_trait]
impl Tool for Delegate {
    fn spec(&self) -> ToolSpec {
        spec("delegate")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx
            .start_child_within("child", "go", Some(Duration::from_secs(60)))
            .await?;
        Err(ToolError::AwaitRun { run })
    }
}

#[tokio::test]
async fn a_child_past_its_limit_is_an_error_result_for_its_parent() {
    let parent_model = Arc::new(MockModel::new());
    parent_model
        .push_tool_calls(vec![call("d1", "delegate")])
        .push_text("it took too long");
    let child_model = Arc::new(MockModel::new());
    child_model.push_tool_calls(vec![call("h1", "hang")]);
    let (hang, calls, fired) = Hang::new(Some(Duration::from_secs(3600)), true);
    let (parent_model, child_model): (DynModel, DynModel) = (parent_model, child_model);
    let parent = LlmAgent::builder("llm", parent_model, "m")
        .tool(Delegate)
        .build();
    let child = LlmAgent::builder("child", child_model, "m")
        .tool(hang)
        .build();
    let clock = ManualClock::new();
    let rt = Runtime::builder(store())
        .agent(parent)
        .agent(child)
        .clock(clock.clone())
        .event_sink(CollectingSink::new())
        .worker_id("w")
        .poll_interval(Duration::from_millis(20))
        .build();
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    while calls.load(SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the child's tool never started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    clock.advance(Duration::from_secs(61));
    let view = loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.status == RunStatus::Done {
            break view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let _ = stop.send(());
    handle.await.expect("worker task").expect("worker result");

    assert!(fired.load(SeqCst), "the child's tool saw its token fire");
    assert_eq!(
        conversation(&rt, &view).await.messages[2],
        Message::tool_error(
            "d1",
            "the run failed: cancelled: time limit: the run did not finish within 1 min"
        )
    );
    assert_eq!(view.output.unwrap()["text"], "it took too long");
}

/// A tool named `slow` that finishes by itself after `after` with a real result, and says what it
/// was told: it never looks at its token.
struct Finishes {
    limit: Duration,
    after: Duration,
}

#[async_trait]
impl Tool for Finishes {
    fn spec(&self) -> ToolSpec {
        spec("slow")
    }
    fn time_limit(&self) -> Option<Duration> {
        Some(self.limit)
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        tokio::time::sleep(self.after).await;
        Ok(ToolOutput::text("its own result"))
    }
}

async fn result_of_a_slow_call(limit: u64, after: u64) -> Message {
    let mock = Arc::new(MockModel::new());
    let agent = agent_with(
        &mock,
        vec![Arc::new(Finishes {
            limit: Duration::from_secs(limit),
            after: Duration::from_secs(after),
        })],
    );
    mock.push_tool_calls(vec![call("c1", "slow")])
        .push_text("done");
    let rt = runtime(
        &agent,
        &store(),
        "w",
        CollectingSinkOrHold::Collect(CollectingSink::new()),
    );
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let view = finish(&rt, run).await;
    conversation(&rt, &view).await.messages[2].clone()
}

#[tokio::test(start_paused = true)]
async fn a_result_that_arrives_while_the_tool_is_told_to_stop_is_kept() {
    // Limit 10 s, grace 5 s: the tool finishes at 12 s, ignoring its token.
    assert_eq!(
        result_of_a_slow_call(10, 12).await,
        Message::tool_result("c1", "its own result")
    );
}

#[tokio::test(start_paused = true)]
async fn a_result_that_arrives_at_the_instant_of_the_limit_wins() {
    assert_eq!(
        result_of_a_slow_call(10, 10).await,
        Message::tool_result("c1", "its own result")
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_that_finishes_after_the_grace_is_reported_as_stopped() {
    let Message::Tool {
        content, is_error, ..
    } = result_of_a_slow_call(10, 16).await
    else {
        panic!("not a tool result")
    };
    assert!(
        is_error && content.contains("did not finish within 10 s"),
        "{content}"
    );
}

/// A source with `noted`, a tool its listing gives a `timeout_ms` of 5 s, and `plain`, with no note.
/// Both wait for their token to fire, so the time they are stopped at is the limit.
struct Waiters;

#[async_trait]
impl adam_llm_agent::ToolSource for Waiters {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
        vec![spec("noted"), spec("plain")]
    }
    async fn listing(&self, ctx: &adam_llm_agent::SourceCtx) -> adam_llm_agent::Listing {
        adam_llm_agent::Listing::new(self.specs(ctx).await)
            .with_note(adam_llm_agent::ToolNote::new("noted").with_timeout(Duration::from_secs(5)))
    }
    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        if name != "noted" && name != "plain" {
            return None;
        }
        ctx.cancelled().await;
        Some(Err(ToolError::Permanent("stopped".into())))
    }
}

#[tokio::test(start_paused = true)]
async fn a_source_tool_gets_its_noted_timeout_plus_ten_seconds_and_300_s_without_one() {
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "m")
        .tool_source(Waiters)
        .build();
    mock.push_tool_calls(vec![call("c1", "noted")])
        .push_tool_calls(vec![call("c2", "plain")])
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
    let messages = conversation(&rt, &view).await.messages;
    for (i, expect) in [(2, "within 15 s"), (4, "within 300 s")] {
        let Message::Tool {
            content, is_error, ..
        } = &messages[i]
        else {
            panic!("{messages:?}")
        };
        assert!(*is_error && content.contains(expect), "{content}");
    }
    assert!(started.elapsed() >= Duration::from_secs(315));
}
