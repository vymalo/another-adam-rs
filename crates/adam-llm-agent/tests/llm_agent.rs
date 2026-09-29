//! Behavioural suite of `LlmAgent`: scripted `MockModel`, `MemoryStore`, and a
//! real `Runtime` with workers. Crash cases abort a worker mid-step exactly
//! like adam-runtime's `crash_safety` test.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Artifact, Conversation, Limits, LlmAgent, LlmAgentBuilder, PendingQuestion,
    TRUNCATION_MARKER_PREFIX, Tool, ToolCtx, ToolError, ToolOutput, user_message,
};
use adam_model::{
    DynModel, FinishReason, Message, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest,
    ModelResponse, ToolCall, ToolSpec,
};
use adam_runtime::{
    CancelToken, Clock, CollectingSink, ManualClock, RetryPolicy, RunEvent, RunView, Runtime,
    RuntimeBuilder,
};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn store() -> DynStore {
    Arc::new(MemoryStore::new())
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
    }
}

/// Counts its calls and answers `"<name>-out"`.
struct CountingTool {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

impl CountingTool {
    fn new(name: &'static str) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn spec(&self) -> ToolSpec {
        spec(self.name)
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        Ok(ToolOutput::text(format!("{}-out", self.name)))
    }
}

/// Answers with a fixed closure result.
struct FnTool<F> {
    name: &'static str,
    f: F,
}

#[async_trait]
impl<F> Tool for FnTool<F>
where
    F: Fn(&ToolCtx, Value) -> Result<ToolOutput, ToolError> + Send + Sync + 'static,
{
    fn spec(&self) -> ToolSpec {
        spec(self.name)
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        (self.f)(ctx, args)
    }
}

fn fn_tool<F>(name: &'static str, f: F) -> FnTool<F>
where
    F: Fn(&ToolCtx, Value) -> Result<ToolOutput, ToolError> + Send + Sync + 'static,
{
    FnTool { name, f }
}

struct Harness {
    store: DynStore,
    sink: CollectingSink,
    mock: Arc<MockModel>,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: store(),
            sink: CollectingSink::new(),
            mock: Arc::new(MockModel::new()),
        }
    }

    fn agent(&self) -> LlmAgentBuilder {
        let model: DynModel = self.mock.clone();
        LlmAgent::builder("llm", model, "test-model")
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        self.runtime_with(agent, "w", |b| b)
    }

    fn runtime_with(
        &self,
        agent: &LlmAgent,
        worker: &str,
        f: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Runtime {
        f(Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .event_sink(self.sink.clone())
            .worker_id(worker)
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10)))
        .build()
    }

    /// Events of a run without the runtime's own status events.
    fn events(&self, run: RunId) -> Vec<RunEvent> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter(|e| !matches!(e, RunEvent::Status { .. }))
            .collect()
    }
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
}

fn spawn_worker(rt: &Runtime) -> Worker {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
    });
    Worker { stop, handle }
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("worker stops in time")
            .expect("worker task")
            .expect("worker result");
    }
}

async fn wait_for(
    rt: &Runtime,
    run: RunId,
    what: &str,
    pred: impl Fn(&RunView) -> bool,
) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if pred(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "done", |v| v.status == RunStatus::Done).await
}

async fn wait_failed(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "failed", |v| v.status == RunStatus::Failed).await
}

async fn wait_waiting(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "parked and waiting", |v| v.waiting).await
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

fn custom(kind: &str, payload: Value) -> RunEvent {
    RunEvent::Custom {
        kind: kind.into(),
        payload,
    }
}

fn tool_start(name: &str, id: &str) -> RunEvent {
    custom(
        "tool_start",
        json!({"name": name, "call_id": id, "status": "running"}),
    )
}

fn tool_end(name: &str, id: &str, status: &str) -> RunEvent {
    custom(
        "tool_end",
        json!({"name": name, "call_id": id, "status": status}),
    )
}

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_calls_tool_a_then_b_then_answers() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let (b, b_calls) = CountingTool::new("b");
    let agent = h
        .agent()
        .instructions("be brief")
        .tool(a)
        .tool(b)
        .limits(Limits {
            max_output_tokens: 321,
            ..Limits::default()
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "a", json!({"x": "1"}))])
        .push_tool_calls(vec![call("c2", "b", json!({"x": "2"}))])
        .push_text("all done");

    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("please do a then b"), Some("conv-1"))
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        view.output,
        Some(json!({"text": "all done", "artifacts": []}))
    );
    assert_eq!(a_calls.load(SeqCst), 1);
    assert_eq!(b_calls.load(SeqCst), 1);

    let a_call = call("c1", "a", json!({"x": "1"}));
    let b_call = call("c2", "b", json!({"x": "2"}));
    let expected = vec![
        Message::user_text("please do a then b"),
        Message::Assistant {
            content: vec![],
            tool_calls: vec![a_call.clone()],
        },
        Message::tool_result("c1", "a-out"),
        Message::Assistant {
            content: vec![],
            tool_calls: vec![b_call.clone()],
        },
        Message::tool_result("c2", "b-out"),
        Message::assistant_text("all done"),
    ];
    let state = conversation(&view);
    assert_eq!(state.messages, expected);
    assert_eq!((state.turns, state.tool_calls), (3, 2));
    assert!(state.pending_calls.is_empty() && state.pending_question.is_none());

    // What the model saw: history grows, tools and settings ride along.
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 3);
    for r in &requests {
        assert_eq!(r.model, "test-model");
        assert_eq!(r.system.as_deref(), Some("be brief"));
        assert_eq!(r.tools, vec![spec("a"), spec("b")]);
        assert_eq!(r.max_output_tokens, Some(321));
    }
    assert_eq!(requests[0].messages, expected[..1]);
    assert_eq!(requests[1].messages, expected[..3]);
    assert_eq!(requests[2].messages, expected[..5]);

    assert_eq!(
        h.events(run),
        vec![
            tool_start("a", "c1"),
            tool_end("a", "c1", "ok"),
            tool_start("b", "c2"),
            tool_end("b", "c2", "ok"),
            custom("agent_text", json!({"text": "all done", "turn": 2})),
        ]
    );
}

#[tokio::test]
async fn parallel_calls_in_one_message_run_in_order() {
    let h = Harness::new();
    let (a, _) = CountingTool::new("a");
    let (b, _) = CountingTool::new("b");
    let agent = h.agent().tool(a).tool(b).build();
    h.mock
        .push_response(ModelResponse {
            message: Message::Assistant {
                content: vec![adam_model::ContentPart::text("on it")],
                tool_calls: vec![call("c1", "a", json!({})), call("c2", "b", json!({}))],
            },
            finish: adam_model::FinishReason::ToolCalls,
            usage: adam_model::Usage {
                input_tokens: 5,
                output_tokens: 7,
            },
        })
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let state = conversation(&view);
    assert_eq!(state.messages[2], Message::tool_result("c1", "a-out"));
    assert_eq!(state.messages[3], Message::tool_result("c2", "b-out"));
    assert_eq!(state.usage.input_tokens, 5);
    assert_eq!(state.usage.output_tokens, 7);
    let events = h.events(run);
    assert_eq!(
        events[0],
        custom("agent_text", json!({"text": "on it", "turn": 0}))
    );
}

#[tokio::test]
async fn init_rejects_unreadable_start_input() {
    use adam_runtime::{Agent, AgentError, Inbound};
    let h = Harness::new();
    let agent = h.agent().build();
    assert_eq!(
        agent.init(user_message("hi")).expect("ok").messages,
        vec![Message::user_text("hi")]
    );
    assert!(agent.init(Inbound::new("x", json!("bare"))).is_ok());
    assert!(matches!(
        agent.init(Inbound::new("message", json!({"nope": 1}))),
        Err(AgentError::Permanent(_))
    ));
}

#[tokio::test]
async fn messages_delivered_mid_run_join_the_history() {
    let h = Harness::new();
    let (a, _) = CountingTool::new("a");
    let agent = h.agent().tool(a).build();
    h.mock
        .push_tool_calls(vec![call("c1", "a", json!({}))])
        .push_text("noted");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("first"), None)
        .await
        .expect("start");
    // Delivered before any worker ran: both messages are in the first turn.
    rt.deliver(run, user_message("second"))
        .await
        .expect("deliver");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    assert_eq!(
        requests[0].messages,
        vec![Message::user_text("first"), Message::user_text("second")]
    );
    assert_eq!(conversation(&view).messages.len(), 5);
}

// ---------------------------------------------------------------------------
// Tool results, errors, artifacts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_tool_and_tool_errors_go_back_to_the_model() {
    let h = Harness::new();
    let agent = h
        .agent()
        .tool(fn_tool("bad_args", |_, _| {
            Ok(ToolOutput::error("missing field x"))
        }))
        .tool(fn_tool("broken", |_, _| {
            Err(ToolError::Permanent("disk on fire".into()))
        }))
        .build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "nope", json!({})),
            call("c2", "bad_args", json!({})),
            call("c3", "broken", json!({})),
        ])
        .push_text("I see the errors");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let state = conversation(&view);
    let results = &state.messages[2..5];
    match &results[0] {
        Message::Tool {
            call_id,
            content,
            is_error,
        } => {
            assert_eq!(call_id, "c1");
            assert!(*is_error);
            assert!(content.contains("unknown tool `nope`"), "{content}");
            assert!(content.contains("bad_args, broken"), "{content}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(results[1], Message::tool_error("c2", "missing field x"));
    assert_eq!(results[2], Message::tool_error("c3", "disk on fire"));
    assert_eq!(view.output.expect("output")["text"], "I see the errors");

    let ends: Vec<RunEvent> = h
        .events(run)
        .into_iter()
        .filter(|e| matches!(e, RunEvent::Custom { kind, .. } if kind == "tool_end"))
        .collect();
    assert_eq!(
        ends,
        vec![
            tool_end("nope", "c1", "error"),
            tool_end("bad_args", "c2", "error"),
            tool_end("broken", "c3", "error"),
        ]
    );
}

#[tokio::test]
async fn artifacts_progress_and_context_reach_observers() {
    let h = Harness::new();
    type Seen = (String, Option<String>, u32, String);
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let agent = h
        .agent()
        .tool({
            let seen = seen.clone();
            AsyncTool(move |ctx: ToolCtx| {
                let seen = seen.clone();
                async move {
                    seen.lock().expect("lock").push((
                        ctx.call_id().to_owned(),
                        ctx.conversation_id().map(str::to_owned),
                        ctx.attempt(),
                        ctx.tool_name().to_owned(),
                    ));
                    ctx.emit_progress("halfway").await;
                    Ok(ToolOutput::text("wrote it").with_artifact(Artifact {
                        name: "report.md".into(),
                        mime_type: Some("text/markdown".into()),
                        data: json!("# hi"),
                    }))
                }
            })
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "async_tool", json!({}))])
        .push_text("here you go");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("write"), Some("conv-9"))
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        seen.lock().expect("lock").clone(),
        vec![("c1".into(), Some("conv-9".into()), 0, "async_tool".into())]
    );
    assert_eq!(
        view.artifacts,
        vec![adam_runtime::Artifact {
            name: "report.md".into(),
            mime_type: Some("text/markdown".into()),
            data: json!("# hi"),
        }]
    );
    assert_eq!(
        view.output,
        Some(json!({
            "text": "here you go",
            "artifacts": [{"name": "report.md", "mime_type": "text/markdown"}]
        }))
    );
    assert_eq!(
        h.events(run),
        vec![
            tool_start("async_tool", "c1"),
            RunEvent::Progress {
                message: "halfway".into()
            },
            RunEvent::Artifact {
                name: "report.md".into(),
                mime_type: Some("text/markdown".into()),
                data: json!("# hi"),
            },
            tool_end("async_tool", "c1", "ok"),
            custom("agent_text", json!({"text": "here you go", "turn": 1})),
        ]
    );
}

/// A tool named `async_tool` backed by an async closure.
struct AsyncTool<F>(F);

#[async_trait]
impl<F, Fut> Tool for AsyncTool<F>
where
    F: Fn(ToolCtx) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<ToolOutput, ToolError>> + Send,
{
    fn spec(&self) -> ToolSpec {
        spec("async_tool")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        (self.0)(ctx.clone()).await
    }
}

#[tokio::test]
async fn transient_tool_error_retries_the_step() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let agent = h
        .agent()
        .tool({
            let (calls, attempts) = (calls.clone(), attempts.clone());
            fn_tool("flaky", move |ctx, _| {
                attempts.lock().expect("lock").push(ctx.attempt());
                if calls.fetch_add(1, SeqCst) == 0 {
                    Err(ToolError::Transient("timeout".into()))
                } else {
                    Ok(ToolOutput::text("finally"))
                }
            })
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "flaky", json!({}))])
        // The model is asked again on the retry (steps are at-least-once
        // across transient retries).
        .push_tool_calls(vec![call("c1", "flaky", json!({}))])
        .push_text("ok");
    let rt = h.runtime_with(&agent, "w", |b| {
        b.retry(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            multiplier: 1.0,
        })
    });
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(calls.load(SeqCst), 2);
    assert_eq!(*attempts.lock().expect("lock"), vec![0, 1]);
    assert_eq!(view.output.clone().expect("output")["text"], "ok");
    assert_eq!(
        conversation(&view).messages[2],
        Message::tool_result("c1", "finally")
    );
    assert!(
        h.events(run)
            .contains(&tool_end("flaky", "c1", "transient_error"))
    );
}

// ---------------------------------------------------------------------------
// Model errors
// ---------------------------------------------------------------------------

fn quick_retry(b: RuntimeBuilder) -> RuntimeBuilder {
    b.retry(RetryPolicy {
        max_attempts: 3,
        initial_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(10),
        multiplier: 1.0,
    })
}

#[tokio::test]
async fn transient_model_error_retries_then_succeeds() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock
        .push_error(ModelError::transient("502"))
        .push_error(ModelError::RateLimited { retry_after: None })
        .push_text("recovered");
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        view.output,
        Some(json!({"text": "recovered", "artifacts": []}))
    );
    assert_eq!(h.mock.requests().len(), 3);
    assert_eq!(view.attempt, 0, "a successful transition resets the count");
    // The same request was retried, not a changed one.
    let requests = h.mock.requests();
    assert_eq!(requests[0], requests[2]);
}

#[tokio::test]
async fn retryable_model_error_that_persists_fails_after_the_retry_budget() {
    let h = Harness::new();
    let agent = h.agent().build();
    for _ in 0..3 {
        h.mock.push_error(ModelError::transient("down"));
    }
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    let error = view.error.expect("error");
    assert!(error.contains("gave up after 3 attempts"), "{error}");
    assert!(error.contains("down"), "{error}");
}

#[tokio::test]
async fn permanent_model_error_fails_the_run_without_retrying() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock.push_error(ModelError::Auth("bad key".into()));
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    let error = view.error.expect("error");
    assert!(error.contains("model call failed"), "{error}");
    assert!(error.contains("bad key"), "{error}");
    assert_eq!(h.mock.requests().len(), 1);
    assert_eq!(view.attempt, 0);
}

#[tokio::test]
async fn rate_limited_model_waits_for_retry_after_then_succeeds() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock
        .push_error(ModelError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        })
        .push_text("through");
    let clock = ManualClock::new();
    let t0 = clock.now();
    let rt = h.runtime_with(&agent, "w", |b| quick_retry(b).clock(clock.clone()));
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);

    let scheduled = wait_for(&rt, run, "the retry to be scheduled", |v| v.attempt == 1).await;
    let wake_at = scheduled.wake_at.expect("a retry carries a timer");
    let thirty = chrono::Duration::seconds(30);
    assert!(wake_at >= t0 + thirty, "Retry-After ignored: {wake_at}");
    assert!(wake_at <= clock.now() + thirty, "{wake_at}");

    // Five seconds short of the hint, several polls later: no second call.
    let gap = (wake_at - clock.now() - chrono::Duration::seconds(5))
        .to_std()
        .expect("in the future");
    clock.advance(gap);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.mock.requests().len(), 1, "retried before Retry-After");

    clock.advance(Duration::from_secs(10));
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        view.output,
        Some(json!({"text": "through", "artifacts": []}))
    );
    assert_eq!(h.mock.requests().len(), 2);
    let retrying: Vec<String> = h
        .sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Status {
                detail: Some(d), ..
            } if d.contains("retrying") => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(retrying.len(), 1);
    assert!(retrying[0].contains("retrying in 30s"), "{retrying:?}");
    assert!(retrying[0].contains("rate limited"), "{retrying:?}");
}

/// Journal entries written before retry hints existed have no
/// `retry_after_ms`; they must still decode and retry like a plain transient
/// error.
#[tokio::test]
async fn model_failures_journaled_before_retry_hints_still_decode() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock.push_text("recovered");
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    h.store
        .journal_put(
            run,
            JournalEntry::err(
                0,
                "model:0",
                json!({"retryable": true, "message": "rate limited (from an old journal)"}),
            ),
        )
        .await
        .expect("seed the journal");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.output.expect("output")["text"], "recovered");
    assert_eq!(
        h.mock.requests().len(),
        1,
        "the recorded failure was replayed"
    );
}

fn length_limited(text: &str) -> ModelResponse {
    ModelResponse {
        finish: FinishReason::Length,
        ..ModelResponse::text(text)
    }
}

#[tokio::test]
async fn length_finish_without_tools_is_done_and_marked_truncated() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock
        .push_response(length_limited("the answer was cut o"))
        .push_text("a complete answer");
    let rt = h.runtime(&agent);
    let worker = spawn_worker(&rt);
    let cut = rt
        .start("llm", user_message("long please"), None)
        .await
        .expect("start");
    let cut = wait_done(&rt, cut).await;
    let whole = rt
        .start("llm", user_message("short please"), None)
        .await
        .expect("start");
    let whole = wait_done(&rt, whole).await;
    worker.stop().await;

    assert_eq!(
        cut.output,
        Some(json!({"text": "the answer was cut o", "artifacts": [], "truncated": true}))
    );
    assert_eq!(
        whole.output,
        Some(json!({"text": "a complete answer", "artifacts": []})),
        "a normal stop is not marked"
    );
}

#[tokio::test]
async fn context_length_error_fails_with_the_message() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock.push_error(ModelError::ContextLength(
        "prompt is 300000 tokens, the window is 128000".into(),
    ));
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("a huge prompt"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    let error = view.error.expect("error");
    assert!(error.contains("model call failed"), "{error}");
    assert!(error.contains("context length exceeded"), "{error}");
    assert!(error.contains("300000 tokens"), "{error}");
    assert_eq!(h.mock.requests().len(), 1, "not retried");
    assert_eq!(view.attempt, 0);
}

#[tokio::test]
async fn persistent_transient_tool_error_fails_after_the_retry_budget() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool({
            let calls = calls.clone();
            fn_tool("flaky", move |_, _| {
                calls.fetch_add(1, SeqCst);
                Err(ToolError::Transient("disk busy".into()))
            })
        })
        .build();
    for _ in 0..3 {
        // Each retry asks the model again (see the retry test above).
        h.mock.push_tool_calls(vec![call("c1", "flaky", json!({}))]);
    }
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    let error = view.error.expect("error");
    assert!(error.contains("gave up after 3 attempts"), "{error}");
    assert!(error.contains("tool `flaky` failed"), "{error}");
    assert!(error.contains("disk busy"), "{error}");
    assert_eq!(calls.load(SeqCst), 3, "one call per attempt");
    assert_eq!(h.mock.requests().len(), 3);
    assert_eq!(view.attempt, 3);
}

// ---------------------------------------------------------------------------
// Cancellation reaches a running tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_the_run_stops_a_running_tool() {
    let h = Harness::new();
    let started = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let agent = h
        .agent()
        .tool(AsyncTool({
            let (started, stopped) = (started.clone(), stopped.clone());
            move |ctx: ToolCtx| {
                let (started, stopped) = (started.clone(), stopped.clone());
                async move {
                    assert!(!ctx.is_cancelled());
                    started.notify_one();
                    tokio::select! {
                        () = ctx.cancelled() => {
                            stopped.store(true, SeqCst);
                            Err(ToolError::Permanent("cancelled while running".into()))
                        }
                        () = tokio::time::sleep(Duration::from_secs(60)) => {
                            Ok(ToolOutput::text("ran to the end"))
                        }
                    }
                }
            }
        }))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "async_tool", json!({}))])
        .push_text("never reached");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    notified(&started, "the tool to start").await;

    rt.cancel(run, "user pressed stop").await.expect("cancel");
    // A tool that ignored the signal would hold `stop` for 60 s.
    worker.stop().await;

    assert!(stopped.load(SeqCst), "the tool saw the cancellation");
    let view = rt.view(run).await.expect("view").expect("run");
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.error.as_deref(), Some("cancelled: user pressed stop"));
    assert_eq!(h.mock.requests().len(), 1, "no model call after the cancel");
}

#[tokio::test]
async fn a_detached_tool_ctx_is_never_cancelled_unless_given_a_token() {
    let sink: adam_runtime::DynEventSink = Arc::new(CollectingSink::new());
    let ctx = ToolCtx::detached("t", "c1", sink.clone());
    assert!(!ctx.is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), ctx.cancelled())
            .await
            .is_err(),
        "nothing fires a detached context"
    );

    let token = CancelToken::new();
    let ctx = ToolCtx::detached("t", "c1", sink).with_cancel_token(token.clone());
    let waiter = tokio::spawn({
        let ctx = ctx.clone();
        async move { ctx.cancelled().await }
    });
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("released")
        .expect("task");
    assert!(ctx.is_cancelled() && ctx.cancel_token().is_cancelled());
}

// ---------------------------------------------------------------------------
// NeedsInput
// ---------------------------------------------------------------------------

fn asking_tool() -> impl Tool {
    fn_tool("ask", |_, _| {
        Err(ToolError::NeedsInput {
            question: "which environment?".into(),
        })
    })
}

#[tokio::test]
async fn needs_input_parks_and_the_answer_becomes_the_tool_result() {
    let h = Harness::new();
    let (echo, echo_calls) = CountingTool::new("echo");
    let agent = h.agent().tool(asking_tool()).tool(echo).build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "ask", json!({})),
            call("c2", "echo", json!({})),
        ])
        .push_text("deploying to prod");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("deploy"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);

    let parked = wait_waiting(&rt, run).await;
    assert_eq!(parked.status, RunStatus::Parked);
    assert!(parked.waiting);
    assert_eq!(
        conversation(&parked).pending_question,
        Some(PendingQuestion {
            call_id: "c1".into(),
            tool: "ask".into(),
            question: "which environment?".into(),
        })
    );
    assert_eq!(
        echo_calls.load(SeqCst),
        0,
        "calls after the parked one wait"
    );
    assert!(h.events(run).contains(&custom(
        "input_required",
        json!({"question": "which environment?", "call_id": "c1"})
    )));

    rt.deliver(run, user_message("prod"))
        .await
        .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(echo_calls.load(SeqCst), 1);
    let state = conversation(&view);
    assert!(state.pending_question.is_none() && state.pending_calls.is_empty());
    assert_eq!(state.messages[2], Message::tool_result("c1", "prod"));
    assert_eq!(state.messages[3], Message::tool_result("c2", "echo-out"));
    // The model's second call saw the answer as the tool result.
    assert_eq!(
        h.mock.requests()[1].messages[2],
        Message::tool_result("c1", "prod")
    );
    assert_eq!(view.output.expect("output")["text"], "deploying to prod");
}

#[tokio::test]
async fn extra_user_messages_wait_behind_the_owed_tool_results() {
    let h = Harness::new();
    let (echo, _) = CountingTool::new("echo");
    let agent = h.agent().tool(asking_tool()).tool(echo).build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "ask", json!({})),
            call("c2", "echo", json!({})),
        ])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("deploy"), None)
        .await
        .expect("start");

    let worker = spawn_worker(&rt);
    wait_waiting(&rt, run).await;
    worker.stop().await;
    // Both arrive while parked, so one transition sees both.
    rt.deliver(run, user_message("prod"))
        .await
        .expect("deliver");
    rt.deliver(run, user_message("and be quick"))
        .await
        .expect("deliver");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let messages = conversation(&view).messages;
    assert_eq!(messages[2], Message::tool_result("c1", "prod"));
    assert_eq!(messages[3], Message::tool_result("c2", "echo-out"));
    assert_eq!(messages[4], Message::user_text("and be quick"));
    assert_eq!(h.mock.requests()[1].messages[..], messages[..5]);
}

#[tokio::test]
async fn an_unreadable_message_does_not_answer_the_question() {
    let h = Harness::new();
    let agent = h.agent().tool(asking_tool()).build();
    h.mock
        .push_tool_calls(vec![call("c1", "ask", json!({}))])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_waiting(&rt, run).await;

    rt.deliver(
        run,
        adam_runtime::Inbound::new("approval", json!({"approved": true})),
    )
    .await
    .expect("deliver");
    // Woken, finds nothing usable, parks again on the same question.
    wait_for(&rt, run, "re-parked with the inbox consumed", |v| {
        v.waiting && v.pending_inbox == 0 && v.version > 3
    })
    .await;
    assert!(
        conversation(&rt.view(run).await.unwrap().unwrap())
            .pending_question
            .is_some()
    );

    rt.deliver(run, user_message("yes")).await.expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        conversation(&view).messages[2],
        Message::tool_result("c1", "yes")
    );
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_turns_fails_the_run() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let agent = h
        .agent()
        .tool(a)
        .limits(Limits {
            max_turns: 2,
            ..Limits::default()
        })
        .build();
    for i in 0..5 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({}))]);
    }
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    let error = view.error.clone().expect("error");
    assert!(error.contains("max_turns = 2"), "{error}");
    assert_eq!(h.mock.requests().len(), 2, "no third model call");
    assert_eq!(a_calls.load(SeqCst), 2);
    assert_eq!(conversation(&view).turns, 2);
}

#[tokio::test]
async fn max_tool_calls_fails_the_run_across_turns() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let agent = h
        .agent()
        .tool(a)
        .limits(Limits {
            max_tool_calls: 2,
            ..Limits::default()
        })
        .build();
    for i in 0..5 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({}))]);
    }
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    let error = view.error.expect("error");
    assert!(error.contains("max_tool_calls = 2"), "{error}");
    assert_eq!(a_calls.load(SeqCst), 2, "the third call never runs");
    assert_eq!(h.mock.requests().len(), 3);
}

#[tokio::test]
async fn max_tool_calls_rejects_an_oversized_turn_before_running_any_tool() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let agent = h
        .agent()
        .tool(a)
        .limits(Limits {
            max_tool_calls: 2,
            ..Limits::default()
        })
        .build();
    h.mock.push_tool_calls(vec![
        call("c1", "a", json!({})),
        call("c2", "a", json!({})),
        call("c3", "a", json!({})),
    ]);
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("many"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    assert!(view.error.expect("error").contains("asked for 3 more"));
    assert_eq!(a_calls.load(SeqCst), 0);
}

#[tokio::test]
async fn history_truncates_old_tool_outputs_and_keeps_the_newest() {
    let h = Harness::new();
    let big = |ch: char| ch.to_string().repeat(4000);
    let agent = h
        .agent()
        .tool(fn_tool("dump", |_, args| {
            let ch = args["ch"]
                .as_str()
                .and_then(|s| s.chars().next())
                .unwrap_or('?');
            Ok(ToolOutput::text(ch.to_string().repeat(4000)))
        }))
        .limits(Limits {
            // 1500 tokens ~ 6000 chars: two 4000-char outputs cannot both fit.
            max_history_tokens: 1500,
            ..Limits::default()
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "dump", json!({"ch": "a"}))])
        .push_tool_calls(vec![call("c2", "dump", json!({"ch": "b"}))])
        .push_text("summarised");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("dump"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    // Turn 2: one output only, fits untouched.
    assert_eq!(requests[1].messages[2].text(), big('a'));
    // Turn 3: the old output is truncated (not dropped), the newest intact.
    let third = &requests[2].messages;
    assert_eq!(third.len(), 5, "nothing dropped");
    let old = third[2].text();
    assert!(old.len() < 4000, "old output shortened");
    assert!(
        old.starts_with("aaaa") && old.contains(TRUNCATION_MARKER_PREFIX),
        "{old}"
    );
    assert!(matches!(&third[2], Message::Tool { call_id, .. } if call_id == "c1"));
    assert_eq!(third[4].text(), big('b'), "newest output intact");

    // The durable history is lossless.
    let state = conversation(&view);
    assert_eq!(state.messages[2].text(), big('a'));
    assert_eq!(state.messages[4].text(), big('b'));
}

#[tokio::test]
async fn zero_output_tokens_leaves_the_choice_to_the_model() {
    let h = Harness::new();
    let agent = h
        .agent()
        .limits(Limits {
            max_output_tokens: 0,
            ..Limits::default()
        })
        .build();
    h.mock.push_text("hi");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(h.mock.requests()[0].max_output_tokens, None);
}

// ---------------------------------------------------------------------------
// Crash safety
// ---------------------------------------------------------------------------

/// Wraps a model; the `hang_on`-th call (0-based) hangs forever the first
/// time it is made, after signalling. That is the worker "dying" right there.
struct HangingModel {
    inner: Arc<MockModel>,
    hang_on: usize,
    calls: AtomicUsize,
    armed: AtomicBool,
    reached: Arc<Notify>,
}

#[async_trait]
impl ModelClient for HangingModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let n = self.calls.fetch_add(1, SeqCst);
        if n == self.hang_on && self.armed.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.inner.stream(req).await
    }
}

fn short_lease(b: RuntimeBuilder) -> RuntimeBuilder {
    b.lease_ttl(Duration::from_millis(300))
}

/// The tool ran and its result was committed; the worker dies on the very
/// next model call. The survivor must not run the tool again.
#[tokio::test]
async fn crash_between_a_tool_and_the_next_model_call_does_not_repeat_the_tool() {
    let mock = Arc::new(MockModel::new());
    let reached = Arc::new(Notify::new());
    let model: DynModel = Arc::new(HangingModel {
        inner: mock.clone(),
        hang_on: 1,
        calls: AtomicUsize::new(0),
        armed: AtomicBool::new(true),
        reached: reached.clone(),
    });
    let (a, a_calls) = CountingTool::new("a");
    let agent = LlmAgent::builder("llm", model, "m").tool(a).build();
    mock.push_tool_calls(vec![call("c1", "a", json!({}))])
        .push_text("finished");

    let h = Harness::new();
    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "crash-a", short_lease),
        h.runtime_with(&agent, "crash-b", short_lease),
    );
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");

    let doomed = spawn_worker(&rt_a);
    notified(&reached, "the second model call").await;
    doomed.handle.abort();
    assert!(doomed.handle.await.expect_err("aborted").is_cancelled());
    assert_eq!(a_calls.load(SeqCst), 1);

    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(a_calls.load(SeqCst), 1, "the tool must not run again");
    assert_eq!(
        view.output,
        Some(json!({"text": "finished", "artifacts": []}))
    );
    let messages = conversation(&view).messages;
    assert_eq!(messages[2], Message::tool_result("c1", "a-out"));
}

/// Two tools in one model message; the worker dies inside the second, before
/// anything of this transition was committed. The survivor replays the
/// journal: neither the model nor the first tool runs again.
#[tokio::test]
async fn crash_mid_step_replays_the_journal_instead_of_repeating_effects() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let reached = Arc::new(Notify::new());
    let b_calls = Arc::new(AtomicUsize::new(0));
    let first = Arc::new(AtomicBool::new(true));
    let agent = h
        .agent()
        .tool(a)
        .tool(AsyncTool2 {
            name: "b",
            calls: b_calls.clone(),
            first: first.clone(),
            reached: reached.clone(),
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "a", json!({})), call("c2", "b", json!({}))])
        .push_text("both done");

    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "crash-a", short_lease),
        h.runtime_with(&agent, "crash-b", short_lease),
    );
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");

    let doomed = spawn_worker(&rt_a);
    notified(&reached, "tool b starting").await;
    doomed.handle.abort();
    assert!(doomed.handle.await.expect_err("aborted").is_cancelled());
    assert_eq!(a_calls.load(SeqCst), 1);
    assert_eq!(h.mock.requests().len(), 1);

    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(a_calls.load(SeqCst), 1, "recorded tool result is replayed");
    assert_eq!(b_calls.load(SeqCst), 2, "the in-flight tool never recorded");
    assert_eq!(
        h.mock.requests().len(),
        2,
        "the recorded model turn is replayed"
    );
    let messages = conversation(&view).messages;
    assert_eq!(messages[2], Message::tool_result("c1", "a-out"));
    assert_eq!(messages[3], Message::tool_result("c2", "b-out"));
    assert_eq!(view.output.expect("output")["text"], "both done");
}

/// Tool `b`: hangs the first time it is called.
struct AsyncTool2 {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    first: Arc<AtomicBool>,
    reached: Arc<Notify>,
}

#[async_trait]
impl Tool for AsyncTool2 {
    fn spec(&self) -> ToolSpec {
        spec(self.name)
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        if self.first.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text(format!("{}-out", self.name)))
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registering_a_tool_twice_keeps_the_last_and_one_spec() {
    let h = Harness::new();
    let agent = h
        .agent()
        .tool(fn_tool("t", |_, _| Ok(ToolOutput::text("first"))))
        .tool(fn_tool("t", |_, _| Ok(ToolOutput::text("second"))))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "t", json!({}))])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(h.mock.requests()[0].tools.len(), 1);
    assert_eq!(
        conversation(&view).messages[2],
        Message::tool_result("c1", "second")
    );
    assert_eq!(adam_runtime::Agent::name(&agent), "llm");
}
