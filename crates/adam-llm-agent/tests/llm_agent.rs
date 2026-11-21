//! Behavioural suite of `LlmAgent`: scripted `MockModel`, `MemoryStore`, and a
//! real `Runtime` with workers. Crash cases abort a worker mid-step exactly
//! like adam-runtime's `crash_safety` test.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Artifact, CLOSING_TURN, Conversation, Limits, LlmAgent, LlmAgentBuilder, LlmStarter,
    MAX_CARRIED_BYTES, OMITTED_MARKER_PREFIX, PendingQuestion, PendingWait, StepIo, StepStyle,
    TRUNCATION_MARKER_PREFIX, Tool, ToolCtx, ToolError, ToolOutput, user_message,
};
use adam_model::{
    ContentPart, DynModel, FinishReason, Message, MockModel, ModelClient, ModelDelta, ModelError,
    ModelRequest, ModelResponse, ToolCall, ToolChoice, ToolSpec,
};
use adam_runtime::{
    Agent, AgentStarter, ArtifactRef, CancelToken, Clock, CollectingSink, Inbound, ManualClock,
    RetryPolicy, RunEvent, RunView, Runtime, RuntimeBuilder, StepEvent, StepIcon, StepKind,
    StepOutput, StepState,
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
        ..Default::default()
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

    /// Events of a run without the runtime's own status events, and without the pieces of streamed
    /// text and the id of the stream in `agent_text` (random, and the subject of `tests/streaming.rs`),
    /// and without the usage reports (their ids are random too: `tests/usage.rs`).
    fn events(&self, run: RunId) -> Vec<RunEvent> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter(|e| {
                !matches!(
                    e,
                    RunEvent::Status { .. } | RunEvent::TextDelta { .. } | RunEvent::Usage(_)
                )
            })
            .map(|mut e| {
                if let RunEvent::Custom { payload, .. } = &mut e
                    && let Some(fields) = payload.as_object_mut()
                {
                    fields.remove("stream");
                }
                e
            })
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

/// A run's output without the id of the stream its answer was sent as (random; `tests/streaming.rs`
/// is about it).
fn output_of(view: &RunView) -> Option<Value> {
    let mut output = view.output.clone();
    if let Some(Value::Object(fields)) = &mut output {
        fields.remove("stream");
    }
    output
}

async fn conversation(rt: &Runtime, view: &RunView) -> Conversation {
    let state = rt.state(view.id).await.expect("state").expect("the run");
    serde_json::from_value(state).expect("state is a Conversation")
}

fn custom(kind: &str, payload: Value) -> RunEvent {
    RunEvent::Custom {
        kind: kind.into(),
        payload,
    }
}

/// The report that the call `id` of the tool `name` started, with the arguments it was given.
fn tool_start(name: &str, id: &str, input: Value) -> RunEvent {
    let Value::Object(input) = input else {
        panic!("the arguments of a call are an object");
    };
    RunEvent::Step(
        StepEvent::new(
            format!("tool:{id}"),
            StepKind::Tool,
            name,
            StepState::Running,
        )
        .with_input(input),
    )
}

/// The report that the call `id` of the tool `name` is in the state `status` names, in the words
/// the events had before they were steps: `ok` (completed), `error` and `transient_error`
/// (failed), `needs_input` and `waiting`. `said` is what the call answered (the step's `output`): the
/// result, or the error (`error` and `transient_error` are errors).
fn tool_end(name: &str, id: &str, status: &str, said: Option<&str>) -> RunEvent {
    let state = match status {
        "ok" => StepState::Completed,
        "error" | "transient_error" => StepState::Failed,
        "needs_input" | "waiting" => StepState::Waiting,
        other => panic!("no such status {other}"),
    };
    let step = StepEvent::new(format!("tool:{id}"), StepKind::Tool, name, state);
    RunEvent::Step(match said {
        Some(text) => step.with_output(StepOutput::new(text, state == StepState::Failed)),
        None => step,
    })
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
        // One call at a time: the system prompt and the schemas are the agent's own.
        .max_parallel(1)
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
        output_of(&view),
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
            reasoning: None,
        },
        Message::tool_result("c1", "a-out"),
        Message::Assistant {
            content: vec![],
            tool_calls: vec![b_call.clone()],
            reasoning: None,
        },
        Message::tool_result("c2", "b-out"),
        Message::assistant_text("all done"),
    ];
    let state = conversation(&rt, &view).await;
    assert_eq!(state.messages, expected);
    assert_eq!((state.turns, state.tool_calls), (3, 2));
    assert!(state.pending_calls.is_empty() && state.pending_wait.is_none());

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
            tool_start("a", "c1", json!({"x": "1"})),
            tool_end("a", "c1", "ok", Some("a-out")),
            tool_start("b", "c2", json!({"x": "2"})),
            tool_end("b", "c2", "ok", Some("b-out")),
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
                reasoning: None,
            },
            finish: adam_model::FinishReason::ToolCalls,
            usage: adam_model::Usage::new(5, 7),
            reasoning: None,
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

    let state = conversation(&rt, &view).await;
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
        Err(AgentError::Permanent { .. })
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
    assert_eq!(conversation(&rt, &view).await.messages.len(), 5);
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

    let state = conversation(&rt, &view).await;
    let results = &state.messages[2..5];
    let mut unknown = String::new();
    match &results[0] {
        Message::Tool {
            call_id,
            content,
            is_error,
            ..
        } => {
            assert_eq!(call_id, "c1");
            assert!(*is_error);
            assert!(content.contains("unknown tool `nope`"), "{content}");
            assert!(content.contains("bad_args, broken"), "{content}");
            unknown.clone_from(content);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(results[1], Message::tool_error("c2", "missing field x"));
    assert_eq!(results[2], Message::tool_error("c3", "disk on fire"));
    assert_eq!(view.output.expect("output")["text"], "I see the errors");

    // The call's step, after it started: all three ended in failure.
    let ends: Vec<RunEvent> = h
        .events(run)
        .into_iter()
        .filter(|e| matches!(e, RunEvent::Step(step) if step.state.is_end()))
        .collect();
    assert_eq!(
        ends,
        vec![
            // Each says why it failed, as the model reads it.
            tool_end("nope", "c1", "error", Some(&unknown)),
            tool_end("bad_args", "c2", "error", Some("missing field x")),
            tool_end("broken", "c3", "error", Some("disk on fire")),
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
                    Ok(ToolOutput::text("wrote it").with_artifact(Artifact::new(
                        "report.md",
                        Some("text/markdown".into()),
                        json!("# hi"),
                    )))
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
        vec![ArtifactRef::new(
            "report.md",
            Some("text/markdown".into()),
            json!("# hi")
        )]
    );
    assert_eq!(
        output_of(&view),
        Some(json!({
            "text": "here you go",
            "artifacts": [{"name": "report.md", "mime_type": "text/markdown"}]
        }))
    );
    assert_eq!(
        h.events(run),
        vec![
            // No arguments, so no input.
            RunEvent::Step(StepEvent::new(
                "tool:c1",
                StepKind::Tool,
                "async_tool",
                StepState::Running
            )),
            // `emit_progress` is an update of the call's own step, the text in its detail.
            RunEvent::Step(
                StepEvent::new("tool:c1", StepKind::Tool, "async_tool", StepState::Running)
                    .with_detail("halfway")
            ),
            RunEvent::Artifact {
                id: None,
                name: "report.md".into(),
                mime_type: Some("text/markdown".into()),
                data: json!("# hi"),
                file: None,
            },
            tool_end("async_tool", "c1", "ok", Some("wrote it")),
            custom("agent_text", json!({"text": "here you go", "turn": 1})),
        ]
    );
}

/// A tool shares a file: it is an artifact of the run, byte for byte, and the model reads one line
/// about it. The bytes are in the journal and in the run's artifacts, never in the model's history,
/// in its requests, in the step the observer sees, or in the run's output.
#[tokio::test]
async fn a_shared_file_is_an_artifact_and_its_bytes_never_reach_the_model() {
    use base64::Engine as _;

    // Bytes that are neither text nor short, so that any copy of them would show.
    let bytes: Vec<u8> = (0..3000u32).map(|i| (i * 7 % 251) as u8).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let h = Harness::new();
    let agent = h
        .agent()
        .tool({
            let bytes = bytes.clone();
            AsyncTool(move |_ctx: ToolCtx| {
                let bytes = bytes.clone();
                async move {
                    Ok(
                        ToolOutput::text("Shared chart.png (2.9 KiB, image/png).").with_artifact(
                            Artifact::file("chart.png", "image/png", "chart.png", bytes).unwrap(),
                        ),
                    )
                }
            })
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "async_tool", json!({}))])
        .push_text("here is the chart");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("draw"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    // The file is the run's artifact, whole.
    assert_eq!(view.artifacts.len(), 1);
    let file = view.artifacts[0].file.as_ref().expect("a file artifact");
    assert_eq!(file.filename, "chart.png");
    assert_eq!(file.size, bytes.len() as u64);
    assert_eq!(file.digest(), adam_runtime::blob_id(&bytes));
    let served = rt.artifact_bytes(file).await.unwrap().expect("the bytes");
    assert_eq!(&served[..], &bytes[..], "the view's file is the run's file");
    assert_eq!(view.artifacts[0].mime_type.as_deref(), Some("image/png"));

    // The model read the line the tool wrote, in the history and in its next request.
    let requests = h.mock.requests();
    let second = serde_json::to_string(&requests[1]).unwrap();
    assert!(
        second.contains("Shared chart.png (2.9 KiB, image/png)."),
        "{second}"
    );
    assert!(!second.contains(&b64[..64]), "the bytes are in the request");
    let history = serde_json::to_string(&conversation(&rt, &view).await.messages).unwrap();
    assert!(
        !history.contains(&b64[..64]),
        "the bytes are in the history"
    );

    // The reference the run keeps, and the answer's summary, carry the size and nothing else.
    assert_eq!(
        output_of(&view),
        Some(json!({
            "text": "here is the chart",
            "artifacts": [{"name": "chart.png", "mime_type": "image/png", "bytes": 3000}]
        }))
    );
    // The step the observer sees says the same line.
    assert!(
        h.events(run).contains(&tool_end(
            "async_tool",
            "c1",
            "ok",
            Some("Shared chart.png (2.9 KiB, image/png).")
        )),
        "{:?}",
        h.events(run)
    );
}

/// The files of one run are bounded in all: a file that would go over the run's budget is not
/// shared, and the model is told so (an error result) and can go on.
#[tokio::test]
async fn a_run_may_share_only_so_many_bytes_of_files() {
    use adam_runtime::MAX_RUN_FILE_BYTES;

    let h = Harness::new();
    let n = std::sync::atomic::AtomicUsize::new(0);
    let n = Arc::new(n);
    let agent = h
        .agent()
        .tool(AsyncTool(move |_ctx: ToolCtx| {
            let nth = n.fetch_add(1, SeqCst);
            async move {
                // Four MiB each: the first fits the 6 MiB budget, the second does not.
                let bytes = vec![nth as u8; 4 * 1024 * 1024];
                Ok(
                    ToolOutput::text(format!("Shared f{nth}.bin.")).with_artifact(
                        Artifact::file(
                            format!("f{nth}"),
                            "application/octet-stream",
                            format!("f{nth}.bin"),
                            bytes,
                        )
                        .unwrap(),
                    ),
                )
            }
        }))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "async_tool", json!({}))])
        .push_tool_calls(vec![call("c2", "async_tool", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("two files"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    const { assert!(4 * 1024 * 1024 * 2 > MAX_RUN_FILE_BYTES) };
    let names: Vec<_> = view.artifacts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["f0"], "the second file was not kept");
    let history = conversation(&rt, &view).await.messages;
    let told = history
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                content, is_error, ..
            } => Some((content.as_str(), *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(told.len(), 2);
    assert!(!told[0].1, "{told:?}");
    assert!(
        told[1].1 && told[1].0.contains("Not shared: f1"),
        "{told:?}"
    );
    assert!(told[1].0.contains("6 MiB"), "{told:?}");
}

/// Every call is told what the run may still share (`ToolCtx::files_left`): the whole 6 MiB at
/// first, then less what the run kept.
#[tokio::test]
async fn a_call_is_told_what_the_run_may_still_share() {
    use adam_runtime::MAX_RUN_FILE_BYTES;

    let h = Harness::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let agent = h
        .agent()
        .tool(AsyncTool(move |ctx: ToolCtx| {
            let record = Arc::clone(&record);
            async move {
                let mut seen = record.lock().unwrap();
                seen.push(ctx.files_left());
                let nth = seen.len();
                Ok(ToolOutput::text("a file").with_artifact(
                    Artifact::file(
                        format!("f{nth}"),
                        "application/octet-stream",
                        format!("f{nth}.bin"),
                        vec![0; 1024 * 1024],
                    )
                    .unwrap(),
                ))
            }
        }))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "async_tool", json!({}))])
        .push_tool_calls(vec![call("c2", "async_tool", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("two files"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    let all = MAX_RUN_FILE_BYTES as u64;
    assert_eq!(*seen.lock().unwrap(), [all, all - 1024 * 1024]);
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
        b.retry(
            RetryPolicy::new(3)
                .with_backoff(Duration::from_millis(10), Duration::from_millis(10))
                .with_multiplier(1.0),
        )
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
        conversation(&rt, &view).await.messages[2],
        Message::tool_result("c1", "finally")
    );
    assert!(
        h.events(run)
            .contains(&tool_end("flaky", "c1", "transient_error", Some("timeout")))
    );
}

/// A tool with a retry policy of its own (ADR 0061).
struct Patient<T>(T, RetryPolicy);

#[async_trait]
impl<T: Tool> Tool for Patient<T> {
    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }
    // Shares nothing, so the calls of a turn run at the same time.
    fn access(&self, _args: &Value) -> adam_llm_agent::ToolAccess {
        adam_llm_agent::ToolAccess::Independent
    }
    fn retry_policy(&self) -> Option<RetryPolicy> {
        Some(self.1)
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.0.call(ctx, args).await
    }
}

/// A tool that always fails with a rate limit, counting its calls.
fn rate_limited(calls: &Arc<AtomicUsize>) -> impl Tool {
    let calls = calls.clone();
    fn_tool("busy", move |_ctx, _| {
        calls.fetch_add(1, SeqCst);
        Err(ToolError::retryable(
            "slow down",
            adam_error::ErrorClass::RateLimited,
        ))
    })
}

fn quick(attempts: u32) -> RetryPolicy {
    RetryPolicy::new(attempts).with_backoff(Duration::from_millis(5), Duration::from_millis(5))
}

/// A tool's own policy decides its retries, counted under `tool:<name>`, and a policy set for it
/// by name wins over it.
#[tokio::test]
async fn a_tools_policy_decides_its_retries_and_one_set_by_name_wins() {
    for (by_name, attempts) in [(None, 2), (Some(quick(1)), 1)] {
        let h = Harness::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut builder = h.agent().tool(Patient(rate_limited(&calls), quick(2)));
        if let Some(policy) = by_name {
            builder = builder.tool_retry("busy", policy);
        }
        let agent = builder.build();
        for _ in 0..attempts {
            h.mock.push_tool_calls(vec![call("c1", "busy", json!({}))]);
        }
        let rt = h.runtime_with(&agent, "w", |b| b.retry(quick(9)));
        let run = rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let view = wait_failed(&rt, run).await;
        worker.stop().await;
        let error = view.error.expect("error");
        assert!(
            error.contains(&format!("gave up after {attempts} attempts of `tool:busy`"))
                && error.contains("slow down"),
            "{error}"
        );
        assert_eq!(calls.load(SeqCst), attempts as usize);
    }
}

/// Beside another call of its turn, running at the same time, a tool's failure is still counted under
/// its own key and policy.
#[tokio::test]
async fn a_tools_policy_holds_beside_other_calls() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Patient(rate_limited(&calls), quick(2)))
        .tool(Patient(
            fn_tool("fine", |_ctx, _| Ok(ToolOutput::text("ok"))),
            quick(9),
        ))
        .build();
    for _ in 0..2 {
        h.mock.push_tool_calls(vec![
            call("c1", "fine", json!({})),
            call("c2", "busy", json!({})),
        ]);
    }
    let rt = h.runtime_with(&agent, "w", |b| b.retry(quick(9)));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    let error = view.error.expect("error");
    assert!(
        error.contains("gave up after 2 attempts of `tool:busy`"),
        "{error}"
    );
    assert_eq!(calls.load(SeqCst), 2);
}

/// A closure tool's policy (`FnTool::with_retry_policy`) governs a plain transient failure too.
#[tokio::test]
async fn a_closure_tools_policy_governs_a_transient_failure() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = adam_llm_agent::FnTool::raw("flaky", "Fails.", json!({"type": "object"}), {
        let calls = calls.clone();
        move |_ctx, _args| {
            calls.fetch_add(1, SeqCst);
            async { Err::<String, _>(ToolError::Transient("timeout".into())) }
        }
    })
    .with_retry_policy(quick(3));
    let agent = h.agent().tool(tool).build();
    for _ in 0..3 {
        h.mock.push_tool_calls(vec![call("c1", "flaky", json!({}))]);
    }
    let rt = h.runtime_with(&agent, "w", |b| b.retry(quick(9)));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    let error = view.error.expect("error");
    assert!(
        error.contains("gave up after 3 attempts of `tool:flaky`") && error.contains("timeout"),
        "{error}"
    );
    assert_eq!(calls.load(SeqCst), 3);
}

/// A tool's failure keeps its class: a policy that retries only transient errors does not retry a
/// rate limit.
#[tokio::test]
async fn a_tools_failure_keeps_its_class() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let only_transient = quick(5).with_retry_on(adam_runtime::RetryClasses::of([
        adam_error::ErrorClass::Transient,
    ]));
    let agent = h
        .agent()
        .tool(Patient(rate_limited(&calls), only_transient))
        .build();
    h.mock.push_tool_calls(vec![call("c1", "busy", json!({}))]);
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    let error = view.error.expect("error");
    assert!(
        error.contains("the policy of `tool:busy` does not retry rate-limited"),
        "{error}"
    );
    assert_eq!(calls.load(SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Model errors
// ---------------------------------------------------------------------------

fn quick_retry(b: RuntimeBuilder) -> RuntimeBuilder {
    b.retry(
        RetryPolicy::new(3)
            .with_backoff(Duration::from_millis(10), Duration::from_millis(10))
            .with_multiplier(1.0),
    )
}

/// A model provider's rate limit is a rate limit to the runtime's policy (ADR 0061): a runtime
/// that retries only transient errors fails it at once.
#[tokio::test]
async fn a_model_rate_limit_keeps_its_class() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock
        .push_error(ModelError::RateLimited { retry_after: None })
        .push_text("never");
    let rt = h.runtime_with(&agent, "w", |b| {
        b.retry(quick(5).with_retry_on(adam_runtime::RetryClasses::of([
            adam_error::ErrorClass::Transient,
        ])))
    });
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    let error = view.error.expect("error");
    assert!(error.contains("does not retry rate-limited"), "{error}");
    assert_eq!(h.mock.requests().len(), 1);
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
        output_of(&view),
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
    let thirty = jiff::SignedDuration::from_secs(30);
    assert!(wake_at >= t0 + thirty, "Retry-After ignored: {wake_at}");
    assert!(wake_at <= clock.now() + thirty, "{wake_at}");

    // Five seconds short of the hint, several polls later: no second call.
    let gap =
        std::time::Duration::try_from(wake_at - clock.now() - jiff::SignedDuration::from_secs(5))
            .expect("in the future");
    clock.advance(gap);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(h.mock.requests().len(), 1, "retried before Retry-After");

    clock.advance(Duration::from_secs(10));
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        output_of(&view),
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
        output_of(&cut),
        Some(json!({"text": "the answer was cut o", "artifacts": [], "truncated": true}))
    );
    assert_eq!(
        output_of(&whole),
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

// ---------------------------------------------------------------------------
// Cancellation reaches the model call in flight
// ---------------------------------------------------------------------------

/// Where a [`StalledModel`] goes quiet.
#[derive(Clone, Copy, Debug)]
enum Stall {
    /// Before it has said anything: the request is made and nothing comes back.
    BeforeTheAnswer,
    /// In the middle of a streamed answer: the first words arrive, then nothing.
    AfterTheFirstWords,
}

/// Counts a model call for as long as its request (or the stream of its answer) is alive: dropping
/// either ends it.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn begin(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, SeqCst);
        Self(count.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, SeqCst);
    }
}

/// A model that takes `delay` to answer (the provider is slow, or the connection hangs), and says
/// how many of its calls are still alive: a call that was dropped is not.
struct StalledModel {
    inner: Arc<MockModel>,
    delay: Duration,
    stall: Stall,
    /// Signalled once the model is silent, with the call alive.
    quiet: Arc<Notify>,
    in_flight: Arc<AtomicUsize>,
}

impl StalledModel {
    fn new(inner: &Arc<MockModel>, stall: Stall) -> Arc<Self> {
        Arc::new(Self {
            inner: inner.clone(),
            delay: Duration::from_secs(30),
            stall,
            quiet: Arc::new(Notify::new()),
            in_flight: Arc::new(AtomicUsize::new(0)),
        })
    }
}

#[async_trait]
impl ModelClient for StalledModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let _alive = InFlight::begin(&self.in_flight);
        self.quiet.notify_one();
        tokio::time::sleep(self.delay).await;
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let alive = InFlight::begin(&self.in_flight);
        match self.stall {
            Stall::BeforeTheAnswer => {
                self.quiet.notify_one();
                tokio::time::sleep(self.delay).await;
                let answer = self.inner.stream(req).await?;
                Ok(Box::pin(futures::StreamExt::map(answer, move |item| {
                    let _alive = &alive;
                    item
                })))
            }
            Stall::AfterTheFirstWords => {
                // Its first words now, the rest of the answer after the delay.
                let answer = self.inner.stream(req).await?;
                let (quiet, delay) = (self.quiet.clone(), self.delay);
                Ok(Box::pin(futures::StreamExt::then(answer, move |item| {
                    let (quiet, alive) = (quiet.clone(), &alive);
                    let _ = alive;
                    async move {
                        if matches!(item, Ok(ModelDelta::Finished(_))) {
                            quiet.notify_one();
                            tokio::time::sleep(delay).await;
                        }
                        item
                    }
                })))
            }
        }
    }
}

/// A cancel while the model is being waited for ends the run at once: the request is dropped, no
/// time is spent on the 30 s the provider would have taken, and nothing the model might have said
/// is acted on or sent.
async fn a_cancel_stops_the_model_call_in_flight(stream_text: bool, stall: Stall) {
    let h = Harness::new();
    h.mock.push_text("Hello there, a long answer");
    let model = StalledModel::new(&h.mock, stall);
    let dynamic: DynModel = model.clone();
    let agent = LlmAgent::builder("llm", dynamic, "test-model")
        .stream_text(stream_text)
        .build();
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    notified(&model.quiet, "the model to go quiet").await;
    assert_eq!(model.in_flight.load(SeqCst), 1, "the call is in flight");

    let cancelled_at = Instant::now();
    rt.cancel(run, "user pressed stop").await.expect("cancel");
    // The worker stops when its steps have: a model call that was not dropped holds it for 30 s.
    worker.stop().await;
    let took = cancelled_at.elapsed();
    eprintln!("cancel to the end of the step ({stream_text}, {stall:?}): {took:?}");
    assert!(
        took < Duration::from_secs(2),
        "a cancel does not wait for the model: {took:?}"
    );

    assert_eq!(model.in_flight.load(SeqCst), 0, "the request was dropped");
    let view = rt.view(run).await.expect("view").expect("run");
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.error.as_deref(), Some("cancelled: user pressed stop"));
    assert_eq!(
        h.mock.requests().len(),
        usize::from(matches!(stall, Stall::AfterTheFirstWords))
    );
    // The turn was not carried on: no words said whole, no tool step, no extra status.
    assert!(h.events(run).is_empty(), "{:?}", h.events(run));
    // What had been streamed ends abandoned: nobody is left waiting for more.
    let pieces: Vec<(String, bool, bool)> = h
        .sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::TextDelta {
                text,
                last,
                abandoned,
                ..
            } => Some((text, last, abandoned)),
            _ => None,
        })
        .collect();
    match stall {
        Stall::AfterTheFirstWords if stream_text => {
            let said: String = pieces.iter().map(|p| p.0.as_str()).collect();
            assert_eq!(said, "Hello there, a long answer");
            let last = pieces.last().expect("a piece");
            assert!(last.1 && last.2, "the stream ends abandoned: {pieces:?}");
        }
        _ => assert!(pieces.is_empty(), "{pieces:?}"),
    }
}

#[tokio::test]
async fn a_cancel_stops_a_model_call_that_has_not_answered() {
    a_cancel_stops_the_model_call_in_flight(false, Stall::BeforeTheAnswer).await;
}

#[tokio::test]
async fn a_cancel_stops_a_streamed_model_call_that_has_not_answered() {
    a_cancel_stops_the_model_call_in_flight(true, Stall::BeforeTheAnswer).await;
}

#[tokio::test]
async fn a_cancel_stops_a_streamed_answer_that_went_quiet_in_the_middle() {
    a_cancel_stops_the_model_call_in_flight(true, Stall::AfterTheFirstWords).await;
}

/// The calls of a turn that are still owed when the run is cancelled do not start: the tool that
/// was running ends, and the next call is not made.
#[tokio::test]
async fn a_cancel_ends_the_turn_between_its_calls() {
    let h = Harness::new();
    let started = Arc::new(Notify::new());
    let (second, second_calls) = CountingTool::new("second");
    let agent = h
        .agent()
        .tool(AsyncTool({
            let started = started.clone();
            move |ctx: ToolCtx| {
                let started = started.clone();
                async move {
                    started.notify_one();
                    // A tool that does not look at the cancel: it ends on its own a little later.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    assert!(ctx.is_cancelled());
                    Ok(ToolOutput::text("finished anyway"))
                }
            }
        }))
        .tool(second)
        .build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "async_tool", json!({})),
            call("c2", "second", json!({})),
        ])
        .push_text("never reached");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    notified(&started, "the first tool to start").await;

    rt.cancel(run, "stop").await.expect("cancel");
    worker.stop().await;

    assert_eq!(
        second_calls.load(SeqCst),
        0,
        "the second call never started"
    );
    assert_eq!(h.mock.requests().len(), 1, "no model call after the cancel");
    let view = rt.view(run).await.expect("view").expect("run");
    assert_eq!(view.error.as_deref(), Some("cancelled: stop"));
}

/// A run parked on a question is cancelled; the run that continues it starts from a history in
/// which the call that was never answered has a result, and the model's provider never sees a call
/// without one.
#[tokio::test]
async fn a_run_that_continues_a_cancelled_one_answers_the_calls_that_were_owed() {
    let h = Harness::new();
    let (second, second_calls) = CountingTool::new("second");
    let agent = h
        .agent()
        .tool(fn_tool("ask", |_, _| {
            Err(ToolError::NeedsInput {
                question: "which one?".into(),
                ui: None,
            })
        }))
        .tool(second)
        .build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "ask", json!({})),
            call("c2", "second", json!({})),
        ])
        .push_text("fine, starting again");
    let rt = h.runtime(&agent);
    let first = rt
        .start("llm", user_message("do the thing"), Some("ctx"))
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_waiting(&rt, first).await;
    rt.cancel(first, "changed my mind").await.expect("cancel");
    assert_eq!(second_calls.load(SeqCst), 0);

    let next = rt
        .start_continuing(
            "llm",
            user_message("never mind, do it differently"),
            Some("ctx"),
            first,
        )
        .await
        .expect("continue");
    wait_done(&rt, next).await;
    worker.stop().await;

    let history = h.mock.requests()[1].messages.clone();
    assert_calls_are_answered(&history);
    assert_eq!(
        history,
        [
            Message::user_text("do the thing"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![
                    call("c1", "ask", json!({})),
                    call("c2", "second", json!({})),
                ],
                reasoning: None,
            },
            Message::tool_error("c1", adam_llm_agent::STOPPED_BY_THE_PERSON),
            Message::tool_error("c2", adam_llm_agent::STOPPED_BY_THE_PERSON),
            Message::user_text("never mind, do it differently"),
        ]
    );
    assert_eq!(second_calls.load(SeqCst), 0, "a stopped call is not run");
}

/// What a provider checks: every call of an assistant message is answered by a tool message before
/// anything else comes, and no tool message answers a call nobody made.
fn assert_calls_are_answered(history: &[Message]) {
    let mut owed: Vec<String> = Vec::new();
    for message in history {
        match message {
            Message::Tool { call_id, .. } => {
                let at = owed
                    .iter()
                    .position(|id| id == call_id)
                    .unwrap_or_else(|| panic!("a result for {call_id}, which nobody asked for"));
                owed.remove(at);
            }
            other => {
                assert!(owed.is_empty(), "calls without a result: {owed:?}");
                owed = other.tool_calls().iter().map(|c| c.id.clone()).collect();
            }
        }
    }
    assert!(owed.is_empty(), "calls without a result: {owed:?}");
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
        Err(ToolError::needs_input("which environment?"))
    })
}

/// Nobody answers a firing of a schedule (ADR 0060): in its conversation a question is refused as
/// the tool's result, the run goes on, and it never parks.
#[tokio::test]
async fn needs_input_in_a_firing_of_a_schedule_is_refused_and_the_run_goes_on() {
    let h = Harness::new();
    let (echo, echo_calls) = CountingTool::new("echo");
    let agent = h.agent().tool(asking_tool()).tool(echo).build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "ask", json!({})),
            call("c2", "echo", json!({})),
        ])
        .push_text("deployed to staging; prod needs a person's yes");
    let rt = h.runtime(&agent);
    let run = rt
        .start(
            "llm",
            user_message("deploy"),
            Some("adam.schedule:llm/nightly@2026-12-24T17:00:00Z"),
        )
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert!(!view.waiting);
    assert_eq!(echo_calls.load(SeqCst), 1, "the next call ran");
    let second = serde_json::to_string(&h.mock.requests()[1].messages).unwrap();
    assert!(second.contains("nobody answers this run"), "{second}");
    assert!(
        second.contains("The question was: which environment?"),
        "{second}"
    );
    assert!(
        !h.events(run).contains(&custom(
            "input_required",
            json!({"question": "which environment?", "call_id": "c1"})
        )),
        "nothing is asked of a person"
    );
    // The asking call's step failed, saying why.
    let ended: Vec<_> = h
        .events(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) if step.id == "tool:c1" => Some(step.state),
            _ => None,
        })
        .collect();
    assert_eq!(ended.last(), Some(&StepState::Failed), "{ended:?}");
    let ctx = |v: serde_json::Value| v.as_object().cloned().unwrap();
    assert!(adam_llm_agent::is_unattended(
        Some("adam.schedule:llm/nightly@x"),
        None
    ));
    assert!(adam_llm_agent::is_unattended(
        None,
        Some(&ctx(json!({ "adam.unattended": true })))
    ));
    assert!(!adam_llm_agent::is_unattended(
        None,
        Some(&ctx(json!({ "adam.unattended": "yes" })))
    ));
    assert!(!adam_llm_agent::is_unattended(Some("token-1:chat"), None));
    assert!(!adam_llm_agent::is_unattended(None, None));
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
        conversation(&rt, &parked).await.pending_wait,
        Some(PendingWait::Question(PendingQuestion {
            call_id: "c1".into(),
            tool: "ask".into(),
            question: "which environment?".into(),
            ui: None,
            stream: None,
        }))
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
    // The call's step is waiting for the person, and stays open until the answer.
    let steps = |h: &Harness| -> Vec<(String, StepState)> {
        h.events(run)
            .into_iter()
            .filter_map(|e| match e {
                RunEvent::Step(step) => Some((step.id, step.state)),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        steps(&h),
        [
            ("tool:c1".to_owned(), StepState::Running),
            ("tool:c1".to_owned(), StepState::Waiting),
        ]
    );

    rt.deliver(run, user_message("prod"))
        .await
        .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    // The answer ended it, before the next call began.
    assert_eq!(
        steps(&h),
        [
            ("tool:c1".to_owned(), StepState::Running),
            ("tool:c1".to_owned(), StepState::Waiting),
            ("tool:c1".to_owned(), StepState::Completed),
            ("tool:c2".to_owned(), StepState::Running),
            ("tool:c2".to_owned(), StepState::Completed),
        ]
    );

    // The step that waited ends with the person's answer as its output; the one that started the
    // wait had none.
    let ended: Vec<Option<StepOutput>> = h
        .events(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) if step.id == "tool:c1" && step.state.is_end() => {
                Some(step.output)
            }
            _ => None,
        })
        .collect();
    assert_eq!(ended, [Some(StepOutput::new("prod", false))]);
    assert_eq!(echo_calls.load(SeqCst), 1);
    let state = conversation(&rt, &view).await;
    assert!(state.pending_wait.is_none() && state.pending_calls.is_empty());
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

    let messages = conversation(&rt, &view).await.messages;
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
        conversation(&rt, &rt.view(run).await.unwrap().unwrap())
            .await
            .pending_wait
            .is_some()
    );

    rt.deliver(run, user_message("yes")).await.expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        conversation(&rt, &view).await.messages[2],
        Message::tool_result("c1", "yes")
    );
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Holds the emission of the words of turn `turn` (after that turn's model call is journaled and its
/// state is not yet committed): it says so, then waits for `release`, or for ever without one.
struct PauseSink {
    inner: CollectingSink,
    turn: u64,
    reached: Arc<Notify>,
    release: Option<Arc<Notify>>,
    armed: AtomicBool,
}

#[async_trait]
impl adam_runtime::EventSink for PauseSink {
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent) {
        let at_turn = matches!(
            &event,
            RunEvent::Custom { kind, payload }
                if kind == "agent_text" && payload["turn"] == self.turn
        );
        self.inner.emit(run, agent, event).await;
        if at_turn && self.armed.swap(false, SeqCst) {
            self.reached.notify_one();
            match &self.release {
                Some(release) => release.notified().await,
                None => std::future::pending::<()>().await,
            }
        }
    }
}

fn pausing_runtime(
    agent: &LlmAgent,
    store: &DynStore,
    worker: &str,
    sink: impl adam_runtime::EventSink,
) -> Runtime {
    Runtime::builder(store.clone())
        .agent(agent.clone())
        .event_sink(sink)
        .worker_id(worker)
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_millis(300))
        .build()
}

fn pause_sink(release: Option<Arc<Notify>>) -> (Arc<PauseSink>, Arc<Notify>) {
    let reached = Arc::new(Notify::new());
    let sink = Arc::new(PauseSink {
        inner: CollectingSink::new(),
        turn: 2,
        reached: reached.clone(),
        release,
        armed: AtomicBool::new(true),
    });
    (sink, reached)
}

fn two_turns(h: &Harness, a: CountingTool) -> LlmAgent {
    h.agent()
        .tool(a)
        .limits(Limits {
            max_turns: 2,
            ..Limits::default()
        })
        .build()
}

fn looping(h: &Harness) {
    for i in 0..2 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({}))]);
    }
}

#[tokio::test]
async fn a_run_at_max_turns_ends_with_a_limited_answer() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let agent = two_turns(&h, a);
    looping(&h);
    h.mock.push_text("I got as far as two lookups");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    assert_eq!(requests.len(), 3, "two turns with tools, one closing turn");
    assert_eq!(requests[2].tool_choice, ToolChoice::None);
    assert_eq!(requests[2].tools, requests[0].tools, "the tools stay");
    assert_eq!(requests[2].system, requests[0].system);
    assert_eq!(
        requests[2].messages.last(),
        Some(&Message::user_text(CLOSING_TURN))
    );
    assert_eq!(requests[0].tool_choice, ToolChoice::Auto);
    assert_eq!(a_calls.load(SeqCst), 2);
    assert_eq!(
        output_of(&view),
        Some(json!({
            "text": "I got as far as two lookups",
            "artifacts": [],
            "limited": true,
            "limit": "max_turns"
        }))
    );
    let state = conversation(&rt, &view).await;
    assert_eq!(state.turns, 3);
    assert!(
        state
            .messages
            .iter()
            .all(|m| *m != Message::user_text(CLOSING_TURN)),
        "the instruction is for that one request, not the history"
    );
}

#[tokio::test]
async fn a_closing_turn_that_calls_tools_anyway_keeps_only_its_text() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let agent = two_turns(&h, a);
    looping(&h);
    h.mock.push_response(ModelResponse {
        message: Message::Assistant {
            content: vec![ContentPart::Text {
                text: "summary".into(),
            }],
            tool_calls: vec![call("c9", "a", json!({}))],
            reasoning: None,
        },
        finish: FinishReason::ToolCalls,
        ..ModelResponse::text("")
    });
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(a_calls.load(SeqCst), 2, "the stray call never runs");
    assert_eq!(view.output.as_ref().expect("output")["text"], "summary");
    let state = conversation(&rt, &view).await;
    assert!(state.pending_calls.is_empty());
    let Some(Message::Assistant { tool_calls, .. }) = state.messages.last() else {
        panic!("{:?}", state.messages)
    };
    assert!(
        tool_calls.is_empty(),
        "no call without a result in the history"
    );
}

#[tokio::test]
async fn a_closing_turn_with_no_words_fails_naming_the_limit() {
    let h = Harness::new();
    let (a, _) = CountingTool::new("a");
    let agent = two_turns(&h, a);
    looping(&h);
    h.mock.push_text("");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    let error = view.error.expect("error");
    assert!(error.contains("turn limit exceeded"), "{error}");
    assert!(error.contains("max_turns = 2"), "{error}");
}

/// Runs `loop` up to the closing answer of turn 2 and holds the run there (the answer is journaled,
/// not committed); `during` runs while it is held.
async fn held_at_the_closing_answer<F, Fut>(
    mock: &Arc<MockModel>,
    during: F,
) -> (Runtime, RunView, Arc<MockModel>)
where
    F: FnOnce(Runtime, RunId) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let release = Arc::new(Notify::new());
    let (sink, reached) = pause_sink(Some(release.clone()));
    let (a, _) = CountingTool::new("a");
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "m")
        .tool(a)
        .limits(Limits {
            max_turns: 2,
            ..Limits::default()
        })
        .build();
    let store = store();
    let rt = pausing_runtime(&agent, &store, "w", sink);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    notified(&reached, "the closing answer").await;
    during(rt.clone(), run).await;
    release.notify_one();
    let view = wait_for(&rt, run, "done with the inbox read", |v| {
        v.status == RunStatus::Done && v.pending_inbox == 0
    })
    .await;
    worker.stop().await;
    (rt, view, mock.clone())
}

fn closing_script() -> Arc<MockModel> {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c0", "a", json!({}))])
        .push_tool_calls(vec![call("c1", "a", json!({}))])
        .push_text("closing words");
    mock
}

#[tokio::test]
async fn a_message_after_the_closing_answer_gets_a_closing_turn_of_its_own() {
    let mock = closing_script();
    mock.push_text("second closing words");
    let (_rt, view, mock) = held_at_the_closing_answer(&mock, |rt, run| async move {
        rt.deliver(run, user_message("one more thing"))
            .await
            .expect("deliver");
    })
    .await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 4, "the message is read by a closing turn");
    assert_eq!(requests[3].tool_choice, ToolChoice::None);
    assert_eq!(
        requests[3].messages.last(),
        Some(&Message::user_text(CLOSING_TURN))
    );
    assert!(
        requests[3]
            .messages
            .contains(&Message::user_text("one more thing"))
    );
    let output = view.output.as_ref().expect("output");
    assert_eq!(output["text"], "second closing words");
    assert_eq!(output["limited"], true);
    assert_eq!(output["limit"], "max_turns");
}

#[tokio::test]
async fn a_reopen_with_nothing_new_re_finishes_identically() {
    let mock = closing_script();
    mock.push_text("must never be asked for");
    let (rt, view, mock) = held_at_the_closing_answer(&mock, |rt, run| async move {
        // A message the loop cannot read reopens the run and adds nothing to the history.
        rt.deliver(
            run,
            adam_runtime::Inbound::new("approval", json!({"approved": true})),
        )
        .await
        .expect("deliver");
    })
    .await;

    assert_eq!(mock.requests().len(), 3, "no fourth model call");
    let state = conversation(&rt, &view).await;
    let kept = state
        .limit
        .as_ref()
        .and_then(|hit| hit.answer.clone())
        .expect("the answer is kept");
    assert_eq!(view.output, Some(kept.clone()), "field by field");
    assert_eq!(kept["text"], "closing words");
    assert_eq!(kept["limited"], true);
    assert!(kept["stream"].is_string(), "its stream is kept: {kept}");
}

#[tokio::test]
async fn crash_after_the_closing_answer_is_journaled_replays_it_without_a_model_call() {
    let mock = closing_script();
    let (sink, reached) = pause_sink(None);
    let (a, a_calls) = CountingTool::new("a");
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "m")
        .tool(a)
        .limits(Limits {
            max_turns: 2,
            ..Limits::default()
        })
        .build();
    let store = store();
    let rt_a = pausing_runtime(&agent, &store, "crash-a", sink);
    let rt_b = pausing_runtime(&agent, &store, "crash-b", CollectingSink::new());
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "the closing answer").await;
    doomed.handle.abort();
    assert!(doomed.handle.await.expect_err("aborted").is_cancelled());
    assert_eq!(mock.requests().len(), 3);

    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(mock.requests().len(), 3, "the journaled answer is replayed");
    assert_eq!(a_calls.load(SeqCst), 2);
    let output = output_of(&view).expect("output");
    assert_eq!(output["text"], "closing words");
    assert_eq!(output["limited"], true);
}

#[tokio::test]
async fn crash_during_the_closing_turn_replays_its_answer() {
    let mock = Arc::new(MockModel::new());
    let reached = Arc::new(Notify::new());
    let model: DynModel = Arc::new(HangingModel {
        inner: mock.clone(),
        hang_on: 2,
        calls: AtomicUsize::new(0),
        armed: AtomicBool::new(true),
        reached: reached.clone(),
    });
    let (a, a_calls) = CountingTool::new("a");
    let agent = LlmAgent::builder("llm", model, "m")
        .tool(a)
        .limits(Limits {
            max_turns: 2,
            ..Limits::default()
        })
        .build();
    mock.push_tool_calls(vec![call("c0", "a", json!({}))])
        .push_tool_calls(vec![call("c1", "a", json!({}))])
        .push_text("closing words");
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
    notified(&reached, "the closing turn").await;
    doomed.handle.abort();
    assert!(doomed.handle.await.expect_err("aborted").is_cancelled());

    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(a_calls.load(SeqCst), 2);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].tool_choice, ToolChoice::None);
    assert_eq!(
        requests[2].messages.last(),
        Some(&Message::user_text(CLOSING_TURN))
    );
    let output = view.output.expect("output");
    assert_eq!(output["text"], "closing words");
    assert_eq!(output["limited"], true);
}

#[tokio::test]
async fn max_tool_calls_ends_the_run_limited_across_turns() {
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
    for i in 0..3 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({}))]);
    }
    h.mock.push_text("two calls made");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(a_calls.load(SeqCst), 2, "the third call never runs");
    assert_eq!(h.mock.requests().len(), 4);
    let output = view.output.clone().expect("output");
    assert_eq!(output["limit"], "max_tool_calls");
    assert_eq!(output["text"], "two calls made");
    let state = conversation(&rt, &view).await;
    let Message::Tool {
        content, is_error, ..
    } = &state.messages[6]
    else {
        panic!("{:?}", state.messages)
    };
    assert!(*is_error && content.contains("not run"), "{content}");
}

#[tokio::test]
async fn max_tool_calls_answers_an_oversized_turn_not_run_and_then_closes() {
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
    h.mock
        .push_tool_calls(vec![
            call("c1", "a", json!({})),
            call("c2", "a", json!({})),
            call("c3", "a", json!({})),
        ])
        .push_text("I could not run them");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("many"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(a_calls.load(SeqCst), 0);
    let state = conversation(&rt, &view).await;
    for (i, id) in ["c1", "c2", "c3"].into_iter().enumerate() {
        let Message::Tool {
            call_id,
            content,
            is_error,
            ..
        } = &state.messages[2 + i]
        else {
            panic!("{:?}", state.messages)
        };
        assert_eq!(call_id, id);
        assert!(*is_error);
        assert!(content.contains("not run"), "{content}");
        assert!(content.contains("max_tool_calls = 2"), "{content}");
    }
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].tool_choice, ToolChoice::None);
    assert_eq!(
        requests[1].messages.last(),
        Some(&Message::user_text(CLOSING_TURN))
    );
    let output = view.output.expect("output");
    assert_eq!(output["text"], "I could not run them");
    assert_eq!(output["limited"], true);
    assert_eq!(output["limit"], "max_tool_calls");
}

fn invalid_call_response(id: &str, name: &str, raw: &str, finish: FinishReason) -> ModelResponse {
    ModelResponse {
        message: Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall::with_invalid_arguments(id, name, raw)],
            reasoning: None,
        },
        finish,
        ..ModelResponse::text("")
    }
}

#[tokio::test]
async fn a_stream_without_finish_reason_is_retried() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock
        .push_error(ModelError::transient(
            "stream ended without a finish_reason",
        ))
        .push_text("whole answer");
    let rt = h.runtime_with(&agent, "w", quick_retry);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.output.expect("output")["text"], "whole answer");
    assert_eq!(h.mock.requests().len(), 2);
}

#[tokio::test]
async fn invalid_tool_arguments_are_an_error_result_and_the_run_completes() {
    let h = Harness::new();
    let (a, a_calls) = CountingTool::new("a");
    let agent = h.agent().tool(a).build();
    h.mock
        .push_response(invalid_call_response(
            "c1",
            "a",
            "{\"city\": ",
            FinishReason::Length,
        ))
        .push_text("fixed it");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        a_calls.load(SeqCst),
        0,
        "a call that is not JSON never runs"
    );
    let state = conversation(&rt, &view).await;
    let Message::Tool {
        call_id,
        content,
        is_error,
        ..
    } = &state.messages[2]
    else {
        panic!("{:?}", state.messages)
    };
    assert_eq!(call_id, "c1");
    assert!(*is_error);
    for needle in [
        "not valid JSON",
        "{\"city\": ",
        "did not run",
        "output limit",
    ] {
        assert!(content.contains(needle), "{needle}: {content}");
    }
    // What goes back to the model as the call's arguments is `{}`, never the raw text.
    let Message::Assistant { tool_calls, .. } = &h.mock.requests()[1].messages[1] else {
        panic!("{:?}", h.mock.requests()[1].messages)
    };
    assert_eq!(
        tool_calls[0].arguments,
        json!(adam_model::INVALID_ARGUMENTS_MARKER)
    );
    assert_eq!(view.output.expect("output")["text"], "fixed it");
    let journal = h.store.journal_list(run).await.expect("journal");
    assert!(
        journal.iter().all(|e| e.name != "tool:c1"),
        "the answer comes from the journaled model turn alone"
    );
}

#[tokio::test]
async fn invalid_tool_arguments_count_against_max_tool_calls() {
    let h = Harness::new();
    let (a, _) = CountingTool::new("a");
    let agent = h
        .agent()
        .tool(a)
        .limits(Limits {
            max_tool_calls: 2,
            ..Limits::default()
        })
        .build();
    for i in 0..5 {
        h.mock.push_response(invalid_call_response(
            &format!("c{i}"),
            "a",
            "{oops",
            FinishReason::ToolCalls,
        ));
    }
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("loop"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;
    assert!(
        view.error
            .clone()
            .expect("error")
            .contains("max_tool_calls = 2")
    );
    assert_eq!(conversation(&rt, &view).await.tool_calls, 2);
}

#[tokio::test]
async fn a_replayed_invalid_call_calls_no_model_and_answers_the_same() {
    let h = Harness::new();
    let agent = h.agent().build();
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let recorded = json!({
        "message": serde_json::to_value(Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall::with_invalid_arguments("c1", "a", "{oops")],
            reasoning: None,
        })
        .unwrap(),
        "finish": "tool_calls",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    });
    h.store
        .journal_put(run, JournalEntry::ok(0, "model:0", recorded))
        .await
        .expect("seed the journal");
    h.mock.push_text("after");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(h.mock.requests().len(), 1, "turn 0 was recorded");
    let state = conversation(&rt, &view).await;
    assert!(
        matches!(&state.messages[2], Message::Tool { content, is_error: true, .. } if content.contains("{oops"))
    );
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

    // The stored history is what was sent: shortened once, at the turn that did it (ADR 0044
    // slice 4); the newest output is whole.
    let state = conversation(&rt, &view).await;
    assert_eq!(
        state.messages[2].text(),
        old,
        "the state holds the shortened output"
    );
    assert_eq!(state.messages[4].text(), big('b'));
    assert_eq!(state.shortenings, [2]);
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

impl HangingModel {
    /// Hangs the call this model was told to hang, for ever.
    async fn hang(&self) {
        let n = self.calls.fetch_add(1, SeqCst);
        if n == self.hang_on && self.armed.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait]
impl ModelClient for HangingModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.hang().await;
        self.inner.complete(req).await
    }

    // The agent streams its model calls: the hang is there too.
    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.hang().await;
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
        output_of(&view),
        Some(json!({"text": "finished", "artifacts": []}))
    );
    let messages = conversation(&rt_b, &view).await.messages;
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
    let messages = conversation(&rt_b, &view).await.messages;
    assert_eq!(messages[2], Message::tool_result("c1", "a-out"));
    assert_eq!(messages[3], Message::tool_result("c2", "b-out"));
    assert_eq!(view.output.expect("output")["text"], "both done");
}

/// A model client that makes up the ids of the tool calls it answers with, as a client does for a
/// provider that sends none: a new set at every model call, `g<call>-<n>`, in the stream and in the
/// assembled response alike.
struct MadeUpIds {
    inner: Arc<MockModel>,
    generation: AtomicUsize,
}

impl MadeUpIds {
    fn rewrite(response: &mut ModelResponse, generation: usize) {
        if let Message::Assistant { tool_calls, .. } = &mut response.message {
            for (n, call) in tool_calls.iter_mut().enumerate() {
                call.id = format!("g{generation}-{n}");
            }
        }
    }
}

#[async_trait]
impl ModelClient for MadeUpIds {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let generation = self.generation.fetch_add(1, SeqCst) + 1;
        let mut response = self.inner.complete(req).await?;
        Self::rewrite(&mut response, generation);
        Ok(response)
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        use futures::StreamExt as _;
        let generation = self.generation.fetch_add(1, SeqCst) + 1;
        let started = Arc::new(AtomicUsize::new(0));
        let inner = self.inner.stream(req).await?;
        Ok(inner
            .map(move |delta| {
                delta.map(|delta| match delta {
                    ModelDelta::ToolCallStarted { name, .. } => ModelDelta::ToolCallStarted {
                        id: format!("g{generation}-{}", started.fetch_add(1, SeqCst)),
                        name,
                    },
                    ModelDelta::Finished(mut response) => {
                        Self::rewrite(&mut response, generation);
                        ModelDelta::Finished(response)
                    }
                    other => other,
                })
            })
            .boxed())
    }
}

/// The ids of calls a client made up are journaled with the model's answer: a worker that dies in
/// the middle of the step and the survivor that replays it see the same ids, and the model is not
/// asked again (it would make up others).
#[tokio::test]
async fn call_ids_a_client_made_up_are_journaled_and_a_resumed_run_keeps_them() {
    let mock = Arc::new(MockModel::new());
    let model: DynModel = Arc::new(MadeUpIds {
        inner: mock.clone(),
        generation: AtomicUsize::new(0),
    });
    let (a, a_calls) = CountingTool::new("a");
    let reached = Arc::new(Notify::new());
    let first = Arc::new(AtomicBool::new(true));
    let agent = LlmAgent::builder("llm", model, "m")
        .tool(a)
        .tool(AsyncTool2 {
            name: "b",
            calls: Arc::new(AtomicUsize::new(0)),
            first: first.clone(),
            reached: reached.clone(),
        })
        .build();
    mock.push_tool_calls(vec![call("x", "a", json!({})), call("y", "b", json!({}))])
        .push_text("both done");

    let h = Harness::new();
    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "ids-a", short_lease),
        h.runtime_with(&agent, "ids-b", short_lease),
    );
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "tool b starting").await;
    doomed.handle.abort();
    assert!(doomed.handle.await.expect_err("aborted").is_cancelled());

    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    let messages = conversation(&rt_b, &view).await.messages;
    // The calls the model made are the ones the first model call made up, and so are their results.
    let Message::Assistant { tool_calls, .. } = &messages[1] else {
        panic!("{messages:?}")
    };
    assert_eq!(
        tool_calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        ["g1-0", "g1-1"],
        "the two calls have distinct ids"
    );
    assert_eq!(messages[2], Message::tool_result("g1-0", "a-out"));
    assert_eq!(messages[3], Message::tool_result("g1-1", "b-out"));
    // The model answered twice (the calls, the closing words): the survivor replayed the first.
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(a_calls.load(SeqCst), 1);
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
        conversation(&rt, &view).await.messages[2],
        Message::tool_result("c1", "second")
    );
    assert_eq!(adam_runtime::Agent::name(&agent), "llm");
}

/// The start-only half is the same function as `LlmAgent::init`: it accepts
/// what the agent accepts and rejects what it rejects, with the same error.
#[test]
fn a_starter_inits_exactly_like_the_agent() {
    let agent = LlmAgent::builder("assistant", Arc::new(MockModel::new()), "m").build();
    let starter = LlmStarter::new("assistant");
    assert_eq!(starter.name(), agent.name());

    for input in [
        user_message("fix the bug"),
        Inbound::new("anything", json!("a bare string")),
    ] {
        let from_agent = agent.init(input.clone()).unwrap();
        assert_eq!(starter.init(input).unwrap(), from_agent);
    }

    for payload in [json!({"text": 7}), json!({}), json!(null), json!([1])] {
        let input = Inbound::new("message", payload);
        let from_agent = agent.init(input.clone()).unwrap_err();
        let from_starter = starter.init(input).unwrap_err();
        assert_eq!(from_starter.to_string(), from_agent.to_string());
        assert!(from_starter.to_string().contains("unusable start message"));
    }
}

// ---------------------------------------------------------------------------
// Continuing another run
// ---------------------------------------------------------------------------

/// A conversation as a finished run leaves it: one task, one tool exchange, an answer.
fn finished_conversation() -> Conversation {
    Conversation {
        messages: vec![
            Message::user_text("first task"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![call("c1", "a", json!({}))],
                reasoning: None,
            },
            Message::tool_result("c1", "a-out"),
            Message::assistant_text("answer one"),
        ],
        turns: 2,
        tool_calls: 1,
        ..Conversation::default()
    }
}

/// The front's `LlmStarter` and the worker's `LlmAgent` continue a conversation identically, and
/// refuse the same inputs with the same error.
#[test]
fn a_starter_continues_exactly_like_the_agent() {
    let agent = LlmAgent::builder("assistant", Arc::new(MockModel::new()), "m").build();
    let starter = LlmStarter::new("assistant");
    let prior = finished_conversation();
    let run = RunId::new();

    for input in [
        user_message("second task"),
        Inbound::new("anything", json!("a bare string")),
    ] {
        let from_agent = agent.init_continuing(input.clone(), &prior, run).unwrap();
        let from_starter = starter.init_continuing(input, &prior, run).unwrap();
        assert_eq!(from_starter, from_agent);
        assert_eq!(from_starter.continued_from, Some(run));
        assert_eq!(from_starter.messages.len(), prior.messages.len() + 1);
    }

    for payload in [json!({"text": 7}), json!({}), json!(null), json!([1])] {
        let input = Inbound::new("message", payload);
        let from_agent = agent
            .init_continuing(input.clone(), &prior, run)
            .unwrap_err();
        let from_starter = starter.init_continuing(input, &prior, run).unwrap_err();
        assert_eq!(from_starter.to_string(), from_agent.to_string());
        assert!(from_starter.to_string().contains("unusable start message"));
    }
}

/// End to end on one runtime: the run that continues a finished one calls the model with the
/// earlier messages and then the new one, and counts its own turns.
#[tokio::test]
async fn a_new_run_continues_a_finished_one_and_the_model_sees_the_earlier_messages() {
    let h = Harness::new();
    let (a, _) = CountingTool::new("a");
    let agent = h.agent().tool(a).build();
    h.mock
        .push_tool_calls(vec![call("c1", "a", json!({}))])
        .push_text("answer one")
        .push_text("answer two");
    let rt = h.runtime(&agent);
    let worker = spawn_worker(&rt);

    let first = rt
        .start("llm", user_message("first task"), Some("conv"))
        .await
        .expect("start");
    let first_view = wait_done(&rt, first).await;
    let earlier = conversation(&rt, &first_view).await.messages;
    assert_eq!(earlier.len(), 4);

    let second = RunId::new();
    assert!(
        rt.start_with_id_continuing(
            second,
            "llm",
            user_message("second task"),
            Some("conv"),
            first
        )
        .await
        .expect("continue")
    );
    let view = wait_done(&rt, second).await;
    worker.stop().await;

    // The model's only call for the second run saw everything before, then the new message.
    let requests = h.mock.requests();
    let mut expected = earlier.clone();
    expected.push(Message::user_text("second task"));
    assert_eq!(requests.last().unwrap().messages, expected);

    let state = conversation(&rt, &view).await;
    assert_eq!(state.continued_from, Some(first));
    assert_eq!((state.turns, state.tool_calls), (1, 0));
    assert_eq!(
        state.messages.last(),
        Some(&Message::assistant_text("answer two"))
    );
    assert_eq!(
        output_of(&view),
        Some(json!({"text": "answer two", "artifacts": []}))
    );
    // The first run is as it was.
    assert_eq!(
        conversation(&rt, &rt.view(first).await.unwrap().unwrap())
            .await
            .messages,
        earlier
    );
}

/// What an A2A front does: it holds only the starter, and a worker with the agent steps the run.
#[tokio::test]
async fn a_start_only_front_continues_and_a_worker_steps() {
    let h = Harness::new();
    h.mock.push_text("answer one").push_text("answer two");
    let agent = h.agent().build();
    let worker_rt = h.runtime(&agent);
    let front = Runtime::builder(h.store.clone())
        .starter(LlmStarter::new("llm"))
        .worker_id("front")
        .build();
    let worker = spawn_worker(&worker_rt);

    let first = RunId::new();
    assert!(
        front
            .start_with_id(first, "llm", user_message("first task"), Some("conv"))
            .await
            .unwrap()
    );
    wait_done(&worker_rt, first).await;

    let second = RunId::new();
    assert!(
        front
            .start_with_id_continuing(
                second,
                "llm",
                user_message("second task"),
                Some("conv"),
                first
            )
            .await
            .unwrap()
    );
    let started = conversation(&front, &front.view(second).await.unwrap().unwrap()).await;
    assert_eq!(
        started.messages,
        [
            Message::user_text("first task"),
            Message::assistant_text("answer one"),
            Message::user_text("second task"),
        ]
    );
    wait_done(&worker_rt, second).await;
    worker.stop().await;
    assert_eq!(h.mock.requests().last().unwrap().messages, started.messages);
}

/// A run that stopped mid-turn, parked on a question and then cancelled, leaves an assistant
/// message whose call never got its result. The run that continues it must not ask the model
/// to answer a question nobody is waiting for, and must not send a call without a result: the
/// call is answered as stopped.
#[tokio::test]
async fn a_run_cancelled_on_a_question_continues_without_the_stale_wait() {
    let h = Harness::new();
    let agent = h.agent().tool(asking_tool()).build();
    h.mock
        .push_tool_calls(vec![call("c1", "ask", json!({}))])
        .push_text("deploying to staging");
    let rt = h.runtime(&agent);
    let worker = spawn_worker(&rt);

    let first = rt
        .start("llm", user_message("deploy"), Some("conv"))
        .await
        .expect("start");
    let parked = wait_waiting(&rt, first).await;
    assert!(conversation(&rt, &parked).await.pending_wait.is_some());
    rt.cancel(first, "changed my mind").await.unwrap();

    let second = RunId::new();
    assert!(
        rt.start_with_id_continuing(
            second,
            "llm",
            user_message("just deploy to staging"),
            Some("conv"),
            first
        )
        .await
        .unwrap()
    );
    let started = conversation(&rt, &rt.view(second).await.unwrap().unwrap()).await;
    assert!(started.pending_wait.is_none() && started.pending_calls.is_empty());
    // The model keeps its call, is told it was stopped, and the new message follows.
    assert_eq!(
        started.messages,
        [
            Message::user_text("deploy"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![call("c1", "ask", json!({}))],
                reasoning: None,
            },
            Message::tool_error("c1", adam_llm_agent::STOPPED_BY_THE_PERSON),
            Message::user_text("just deploy to staging"),
        ]
    );
    let done = wait_done(&rt, second).await;
    worker.stop().await;
    assert_eq!(done.output.unwrap()["text"], "deploying to staging");
    // The model was never shown a call without its result.
    let requests = h.mock.requests();
    assert_calls_are_answered(&requests.last().unwrap().messages);
}

/// The bound on what is carried, seen through the starter the A2A front uses: over 256 KiB old
/// tool outputs are shortened first, the oldest whole turns go only after that (with one marker),
/// and the first user message is never given up.
#[test]
fn the_carried_history_is_bounded_shortening_first_and_the_task_is_kept() {
    let turn = |i: usize, output: usize, answer: usize| {
        vec![
            Message::user_text(format!("task {i}")),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![call(&format!("c{i}"), "a", json!({}))],
                reasoning: None,
            },
            Message::tool_result(format!("c{i}"), "x".repeat(output)),
            Message::assistant_text(format!("{i}{}", "y".repeat(answer))),
        ]
    };
    let size = |c: &Conversation| {
        c.messages
            .iter()
            .map(|m| serde_json::to_vec(m).unwrap().len())
            .sum::<usize>()
    };
    let starter = LlmStarter::new("assistant");

    // Tool output is the bulk: shortening the oldest covers it, no turn is given up.
    let prior = Conversation {
        messages: (0..3).flat_map(|i| turn(i, 100_000, 10)).collect(),
        ..Conversation::default()
    };
    assert!(size(&prior) > MAX_CARRIED_BYTES);
    let next = starter
        .init_continuing(user_message("task 3"), &prior, RunId::new())
        .unwrap();
    assert!(size(&next) <= MAX_CARRIED_BYTES, "{}", size(&next));
    assert_eq!(next.omitted_turns, 0);
    let users: Vec<String> = next
        .messages
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .map(Message::text)
        .collect();
    assert_eq!(users, ["task 0", "task 1", "task 2", "task 3"]);
    assert!(next.messages.iter().any(
        |m| matches!(m, Message::Tool { content, .. } if content.contains(TRUNCATION_MARKER_PREFIX))
    ));

    // What the assistant said is the bulk: nothing to shorten, so the oldest turns go, the task
    // stays as the first part of the first message, and the marker says how many.
    let prior = Conversation {
        messages: (0..3).flat_map(|i| turn(i, 10, 100_000)).collect(),
        ..Conversation::default()
    };
    assert!(size(&prior) > MAX_CARRIED_BYTES);
    let next = starter
        .init_continuing(user_message("task 3"), &prior, RunId::new())
        .unwrap();
    assert!(size(&next) <= MAX_CARRIED_BYTES, "{}", size(&next));
    assert_eq!(
        next.omitted_turns, 1,
        "the body of the first turn was enough"
    );
    let Message::User { content } = &next.messages[0] else {
        panic!("the first message is the task");
    };
    assert_eq!(content[0].as_text(), "task 0");
    assert!(content[1].as_text().starts_with(OMITTED_MARKER_PREFIX));
    assert_eq!(content[2].as_text(), "task 1");
    assert_eq!(next.messages[1..8], prior.messages[5..12]);
    assert_eq!(next.messages.last(), Some(&Message::user_text("task 3")));

    // A small conversation is carried whole, without a marker.
    let small = starter
        .init_continuing(user_message("next"), &finished_conversation(), RunId::new())
        .unwrap();
    assert_eq!(small.omitted_turns, 0);
    assert!(
        !small
            .messages
            .iter()
            .any(|m| m.text().starts_with(OMITTED_MARKER_PREFIX))
    );
}

/// A conversation as an older build stored it (no `continued_from`, and the wait under its old
/// name) is a fine thing to continue.
#[test]
fn state_stored_before_continuation_existed_can_be_continued() {
    let old: Conversation = serde_json::from_value(json!({
        "messages": [{"role": "user", "content": [{"type": "text", "text": "deploy"}]}],
        "turns": 1,
        "tool_calls": 0,
        "pending_question": null
    }))
    .unwrap();
    assert_eq!(old.continued_from, None);
    let run = RunId::new();
    let next = LlmStarter::new("assistant")
        .init_continuing(user_message("and then?"), &old, run)
        .unwrap();
    assert_eq!(next.continued_from, Some(run));
    assert_eq!(next.omitted_turns, 0);
    // "deploy" had no answer, so "and then?" joins it as a second part.
    assert_eq!(
        next.messages,
        [Message::User {
            content: vec![ContentPart::text("deploy"), ContentPart::text("and then?")]
        }]
    );
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// A tool that drives another agent: its own style (a sub-agent, labelled and drawn as one), a progress
/// line, and steps it reports that run under its own and under each other.
struct Driver;

#[async_trait]
impl Tool for Driver {
    fn spec(&self) -> ToolSpec {
        spec("driver")
    }

    fn step_style(&self) -> StepStyle {
        StepStyle::new(StepKind::Subagent)
            .with_label("Helper")
            .with_icon(StepIcon::Agent)
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        assert_eq!(ctx.step_id(), "tool:c1");
        ctx.emit_progress("starting Helper").await;
        ctx.report_step(
            StepEvent::new(
                "acp:c1:1",
                StepKind::Command,
                "npm test",
                StepState::Running,
            )
            .with_icon(StepIcon::Execute),
        )
        .await;
        ctx.report_step(
            StepEvent::new("acp:c1:1", StepKind::Command, "npm test", StepState::Failed)
                .with_detail("1 failed"),
        )
        .await;
        // A step under one the call reported, not under the call.
        ctx.report_step(
            StepEvent::new("acp:c1:2", StepKind::Tool, "edit", StepState::Completed)
                .under("acp:c1:1"),
        )
        .await;
        Ok(ToolOutput::text("done"))
    }
}

#[tokio::test]
async fn a_tool_is_a_step_in_its_own_style_and_reports_the_steps_that_run_under_it() {
    let h = Harness::new();
    let agent = h.agent().tool(Driver).build();
    h.mock
        .push_tool_calls(vec![call("c1", "driver", json!({}))])
        .push_text("all done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let own = |state| {
        StepEvent::new("tool:c1", StepKind::Subagent, "Helper", state).with_icon(StepIcon::Agent)
    };
    assert_eq!(
        h.events(run),
        vec![
            // The call starts without arguments (the model gave none, so there is no input) and
            // ends with what it answered.
            RunEvent::Step(own(StepState::Running)),
            // The progress line is an update of the call's own step, in its style.
            RunEvent::Step(
                StepEvent::new("tool:c1", StepKind::Subagent, "Helper", StepState::Running)
                    .with_icon(StepIcon::Agent)
                    .with_detail("starting Helper")
            ),
            // Reported steps run under the call's step ...
            RunEvent::Step(
                StepEvent::new(
                    "acp:c1:1",
                    StepKind::Command,
                    "npm test",
                    StepState::Running
                )
                .under("tool:c1")
                .with_icon(StepIcon::Execute)
            ),
            RunEvent::Step(
                StepEvent::new("acp:c1:1", StepKind::Command, "npm test", StepState::Failed)
                    .under("tool:c1")
                    .with_detail("1 failed")
            ),
            // ... unless they say they run under another one.
            RunEvent::Step(
                StepEvent::new("acp:c1:2", StepKind::Tool, "edit", StepState::Completed)
                    .under("acp:c1:1")
            ),
            RunEvent::Step(own(StepState::Completed).with_output(StepOutput::new("done", false))),
            custom("agent_text", json!({"text": "all done", "turn": 1})),
        ]
    );
}

#[tokio::test]
async fn a_tool_that_says_nothing_is_a_plain_step_labelled_with_its_name() {
    let h = Harness::new();
    let (plain, _) = CountingTool::new("lookup");
    let agent = h.agent().tool(plain).build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "lookup", json!({})),
            call("c2", "nobody_has_this", json!({})),
        ])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let steps: Vec<StepEvent> = h
        .events(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) => Some(step),
            _ => None,
        })
        .collect();
    let plain = |id: &str, name: &str, state| StepEvent::new(id, StepKind::Tool, name, state);
    let unknown = steps[3].output.clone().expect("it says why it failed");
    assert!(unknown.error && unknown.text.contains("unknown tool `nobody_has_this`"));
    assert_eq!(
        steps,
        [
            plain("tool:c1", "lookup", StepState::Running),
            plain("tool:c1", "lookup", StepState::Completed)
                .with_output(StepOutput::new("lookup-out", false)),
            // A name that is none of the agent's tools is a failed step with that name.
            plain("tool:c2", "nobody_has_this", StepState::Running),
            plain("tool:c2", "nobody_has_this", StepState::Failed).with_output(unknown),
        ]
    );
    assert!(
        steps
            .iter()
            .all(|s| s.parent.is_none() && s.icon.is_none() && s.detail.is_none())
    );
}

/// A tool that answers with the `answer` it is given, or fails with it (`fail`: `result` or `error`).
struct Said(&'static str);

#[async_trait]
impl Tool for Said {
    fn spec(&self) -> ToolSpec {
        spec(self.0)
    }
    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let answer = args["answer"].as_str().unwrap_or_default().to_owned();
        match args["fail"].as_str() {
            Some("result") => Ok(ToolOutput::error(answer)),
            Some("error") => Err(ToolError::Permanent(answer)),
            _ => Ok(ToolOutput::text(answer)),
        }
    }
}

/// The steps a run reported, in order.
fn steps_of(h: &Harness, run: RunId) -> Vec<StepEvent> {
    h.events(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) => Some(step),
            _ => None,
        })
        .collect()
}

/// The step of a call says what the call was given and what it answered, scrubbed by the agent's
/// redactor first and cut to the contract's bounds after (ADR 0011).
#[tokio::test]
async fn a_calls_step_carries_its_input_and_output_scrubbed_and_cut() {
    const SECRET: &str = "hunter2-hunter2";
    let h = Harness::new();
    let agent = h
        .agent()
        .tool(Said("search"))
        .step_io(StepIo::default().redact(|text| text.replace(SECRET, "[redacted]")))
        .build();
    // A result of 20 KiB with the secret in the middle and the reason of the failure at the end.
    let long = format!(
        "start {}{SECRET}{} the end: boom",
        "a".repeat(10_000),
        "b".repeat(10_000)
    );
    h.mock
        .push_tool_calls(vec![
            call(
                "c1",
                "search",
                json!({"answer": format!("found {SECRET}"), "query": {"q": [format!("x {SECRET}"), 3]}}),
            ),
            call("c2", "search", json!({"answer": long, "fail": "result"})),
            call("c3", "search", json!({"answer": "denied", "fail": "error"})),
            call("c4", "search", json!({"pad": "p".repeat(5000)})),
        ])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let steps = steps_of(&h, run);
    let by = |id: &str, ends: bool| {
        steps
            .iter()
            .find(|s| s.id == id && s.state.is_end() == ends)
            .unwrap_or_else(|| panic!("no step {id} (end: {ends}) in {steps:#?}"))
    };
    // The start has the arguments, scrubbed, and no output.
    let c1 = by("tool:c1", false);
    assert_eq!(
        c1.input,
        json!({"answer": "found [redacted]", "query": {"q": ["x [redacted]", 3]}})
            .as_object()
            .cloned()
    );
    assert_eq!(c1.output, None);
    // The end has the result, scrubbed, and no input.
    let end = by("tool:c1", true);
    assert_eq!(
        (end.state, end.input.as_ref()),
        (StepState::Completed, None)
    );
    assert_eq!(end.output, Some(StepOutput::new("found [redacted]", false)));
    // What the model was told is its own business: the model got the raw result (the tools
    // scrub for the model, the step for the observer).
    assert_eq!(
        conversation(&rt, &view).await.messages[2],
        Message::tool_result("c1", format!("found {SECRET}"))
    );
    // A result of 20 KiB is cut to 8 KiB, head and tail, marked as an error; the secret is gone.
    let c2 = by("tool:c2", true);
    assert_eq!(c2.state, StepState::Failed);
    let output = c2.output.as_ref().expect("an output");
    assert!(output.error && output.truncated);
    assert_eq!(
        output.bytes,
        Some(long.len() as u64 - SECRET.len() as u64 + "[redacted]".len() as u64)
    );
    assert!(output.text.len() <= 8192, "{}", output.text.len());
    assert!(output.text.starts_with("start aaa") && output.text.ends_with("the end: boom"));
    assert!(!output.text.contains(SECRET));
    // A tool that failed outright says why.
    assert_eq!(
        by("tool:c3", true).output,
        Some(StepOutput::new("denied", true))
    );
    // An input over the bound is replaced by its size (the string is cut first, to 512
    // characters, which fits: 5000 characters of padding become 512).
    let c4 = by("tool:c4", false).input.as_ref().expect("an input");
    assert_eq!(c4["pad"].as_str().map(|p| p.chars().count()), Some(512));
}

/// A step is a label and a state when the agent says it sends neither input nor output.
#[tokio::test]
async fn a_calls_step_carries_neither_when_the_agent_turns_them_off() {
    let h = Harness::new();
    let agent = h
        .agent()
        .tool(Said("search"))
        .step_io(StepIo::off())
        .build();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "search",
            json!({"answer": "secret stuff"}),
        )])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    let plain = |state| StepEvent::new("tool:c1", StepKind::Tool, "search", state);
    assert_eq!(
        steps_of(&h, run),
        [plain(StepState::Running), plain(StepState::Completed)]
    );
}

/// A NUL a tool printed is stored and shown as U+FFFD, in the journaled step and in the model's
/// message, so a replay shows exactly what the live run showed (ADR 0051).
#[tokio::test]
async fn a_nul_in_a_tools_output_is_the_replacement_character_in_the_journal_and_the_message() {
    let h = Harness::new();
    let agent = h.agent().tool(Said("search")).build();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "search",
            json!({"answer": "before\u{0}after"}),
        )])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let state = conversation(&rt, &view).await;
    let said = state.messages.iter().find_map(|m| match m {
        Message::Tool { content, .. } => Some(content.as_str()),
        _ => None,
    });
    assert_eq!(said, Some("before\u{FFFD}after"));
    let joined = serde_json::to_string(&state).expect("the joined state is JSON");
    assert!(
        !joined.contains("\\u0000"),
        "no NUL in the joined state: {joined}"
    );

    let entry = h
        .store
        .journal_get(run, 1)
        .await
        .unwrap()
        .expect("the tool step is journaled");
    let payload = entry.payload.to_string();
    assert!(payload.contains("before\u{FFFD}after"), "{payload}");
    assert!(
        !payload.contains("\\u0000"),
        "no NUL in the journal: {payload}"
    );
}

/// A tool that says what it is called is labelled so in its step; the model still knows it by its name.
#[tokio::test]
async fn a_tools_own_label_is_the_label_of_its_step() {
    struct Titled;
    #[async_trait]
    impl Tool for Titled {
        fn spec(&self) -> ToolSpec {
            spec("search__web_search")
        }
        fn step_style(&self) -> StepStyle {
            StepStyle::default().with_label("Search the web")
        }
        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("1. Example"))
        }
    }
    let h = Harness::new();
    let agent = h.agent().tool(Titled).build();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "search__web_search",
            json!({"query": "adam"}),
        )])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    let steps = steps_of(&h, run);
    assert!(
        steps.iter().all(|s| s.label == "Search the web"),
        "{steps:#?}"
    );
    assert_eq!(steps[0].input.as_ref().unwrap()["query"], "adam");
    assert_eq!(steps[1].output, Some(StepOutput::new("1. Example", false)));
}

#[tokio::test]
async fn a_detached_context_reports_its_steps_to_the_sink() {
    let sink = CollectingSink::new();
    let ctx = ToolCtx::detached("shell", "c7", Arc::new(sink.clone()));
    assert_eq!(ctx.step_id(), "tool:c7");
    ctx.emit_progress("running: ls").await;
    ctx.report_step(StepEvent::new(
        "sh:c7:1",
        StepKind::Command,
        "ls",
        StepState::Running,
    ))
    .await;
    let events: Vec<RunEvent> = sink.events().into_iter().map(|e| e.event).collect();
    assert_eq!(
        events,
        [
            RunEvent::Step(
                StepEvent::new("tool:c7", StepKind::Tool, "shell", StepState::Running)
                    .with_detail("running: ls")
            ),
            RunEvent::Step(
                StepEvent::new("sh:c7:1", StepKind::Command, "ls", StepState::Running)
                    .under("tool:c7")
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// The backstop on a tool result (ADR 0044, decision 2)
// ---------------------------------------------------------------------------

/// Answers `size` bytes. It says it `paged` (the declaration the loop goes by) or not; its schema
/// has an `offset` when `schema_offset`, whatever it declares.
struct Bulky {
    size: usize,
    paged: bool,
    schema_offset: bool,
}

#[async_trait]
impl Tool for Bulky {
    fn spec(&self) -> ToolSpec {
        let properties = if self.schema_offset {
            json!({"offset": {"type": "integer"}})
        } else {
            json!({})
        };
        ToolSpec {
            name: "bulky".into(),
            description: "bulky".into(),
            parameters: json!({"type": "object", "properties": properties}),
        }
    }
    fn paged(&self) -> bool {
        self.paged
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("é".repeat(self.size / 2)))
    }
}

/// What the model was sent for the one call of a run, and what the history holds.
async fn tool_result_of(paged: bool) -> (String, String) {
    tool_result_of_tool(Bulky {
        size: 800_000,
        paged,
        schema_offset: paged,
    })
    .await
}

async fn tool_result_of_tool(tool: impl Tool) -> (String, String) {
    let h = Harness::new();
    let name = tool.spec().name;
    let agent = h.agent().tool(tool).build();
    h.mock
        .push_tool_calls(vec![call("c1", &name, json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    let sent = h.mock.requests()[1].messages[2].text();
    let kept = conversation(&rt, &view).await.messages[2].text();
    (sent, kept)
}

#[tokio::test]
async fn a_tool_result_over_64_kib_is_cut_with_a_marker_before_history() {
    let (sent, kept) = tool_result_of(false).await;
    assert_eq!(sent, kept, "the history holds what the model is sent");
    assert!(sent.len() < 65 * 1024, "{}", sent.len());
    assert!(
        sent.ends_with("\n[cut by the loop: 65536 of 800000 bytes shown]"),
        "{}",
        &sent[sent.len() - 80..]
    );
}

#[tokio::test]
async fn the_marker_names_offset_only_for_a_paged_tool() {
    let (paged, _) = tool_result_of(true).await;
    assert!(
        paged.ends_with(
            "[cut by the loop: 65536 of 800000 bytes shown. Call bulky again with a later offset to read the rest]"
        ),
        "{}",
        &paged[paged.len() - 120..]
    );
    let (plain, _) = tool_result_of(false).await;
    assert!(!plain.contains("offset"), "{plain}");
}

/// An unrelated tool (an MCP tool, say) with an `offset` argument is not paged: its result is cut
/// as any other, with no word about a later offset.
#[tokio::test]
async fn a_tool_with_an_offset_argument_is_not_paged_unless_it_says_so() {
    let (sent, _) = tool_result_of_tool(Bulky {
        size: 800_000,
        paged: false,
        schema_offset: true,
    })
    .await;
    assert!(
        sent.ends_with("[cut by the loop: 65536 of 800000 bytes shown]"),
        "{}",
        &sent[sent.len() - 80..]
    );
    assert!(!sent.contains("later offset"), "{sent}");
}

/// A tool that answers the biggest page the convention allows.
struct BigPage;

fn big_page() -> String {
    let items: Vec<String> = (0..100)
        .map(|i| format!("path/{i:03}:{i}:{}", "m".repeat(2000)))
        .collect();
    adam_llm_agent::paging::page_items(&items, 1, 100, "matches", "(none)", "matches", &[]).unwrap()
}

#[async_trait]
impl Tool for BigPage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "big_page".into(),
            description: "a page".into(),
            parameters: json!({"type": "object", "properties": {"offset": {"type": "integer"}}}),
        }
    }
    fn paged(&self) -> bool {
        true
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(big_page()))
    }
}

/// A page the convention makes (100 matches of 2,000 characters, cut at the byte cap) reaches the
/// model whole: the loop's backstop leaves it alone, and its final line is last.
#[tokio::test]
async fn a_paged_result_is_never_touched_by_the_backstop() {
    let (sent, kept) = tool_result_of_tool(BigPage).await;
    let page = big_page();
    assert!(
        page.len() > 40 * 1024,
        "the page is a big one: {}",
        page.len()
    );
    assert_eq!(sent, page);
    assert_eq!(kept, page);
    assert!(!sent.contains("[cut by the loop"));
    assert!(sent.rsplit('\n').next().unwrap().starts_with("[more: "));
}

/// A worker dies while the second tool runs; the survivor replays the first tool's recorded
/// result, and the cut it is given is the cut of a run that never crashed.
#[tokio::test]
async fn replay_gives_the_same_cut_bytes() {
    let h = Harness::new();
    let reached = Arc::new(Notify::new());
    let agent = h
        .agent()
        .tool(Bulky {
            size: 800_000,
            paged: false,
            schema_offset: false,
        })
        .tool(AsyncTool2 {
            name: "b",
            calls: Arc::new(AtomicUsize::new(0)),
            first: Arc::new(AtomicBool::new(true)),
            reached: reached.clone(),
        })
        .build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "bulky", json!({})),
            call("c2", "b", json!({})),
        ])
        .push_text("both done");
    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "crash-a", short_lease),
        h.runtime_with(&agent, "crash-b", short_lease),
    );
    let run = rt_a.start("llm", user_message("go"), None).await.unwrap();
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "tool b starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;
    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    let whole = "é".repeat(400_000);
    let expected = adam_llm_agent::paging::backstop(&whole, "bulky", false);
    assert_eq!(
        conversation(&rt_b, &view).await.messages[2].text(),
        expected.as_ref()
    );
}

// ---------------------------------------------------------------------------
// Kept output (ADR 0044, slice 3): a long result is kept and read back by `read_output`
// ---------------------------------------------------------------------------

/// Answers a long text, kept for `read_output` the way `adam-mcp` keeps a long result.
struct Wordy {
    text: String,
}

#[async_trait]
impl Tool for Wordy {
    fn spec(&self) -> ToolSpec {
        spec("wordy")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let (shown, kept) = adam_llm_agent::keep_long(ctx, &self.text).await;
        let mut output = ToolOutput::text(shown);
        output.kept = kept;
        Ok(output)
    }
}

/// 7,000 lines of 11 bytes: 77,000 bytes, four pages of at most 2,000 lines.
fn rows() -> String {
    (0..7000).map(|i| format!("row-{i:06}\n")).collect()
}

/// What each request carries ahead of its last message must be what the request before it carried:
/// `system` and `tools` byte for byte, and the messages as a prefix.
fn assert_prefix_holds(requests: &[adam_model::ModelRequest]) {
    assert!(requests.len() > 1);
    for pair in requests.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert_eq!(
            serde_json::to_string(&(&a.system, &a.tools)).unwrap(),
            serde_json::to_string(&(&b.system, &b.tools)).unwrap(),
            "system and tools are the same in every request"
        );
        assert!(b.messages.len() > a.messages.len());
        assert_eq!(
            serde_json::to_string(&a.messages[..]).unwrap(),
            serde_json::to_string(&b.messages[..a.messages.len()]).unwrap(),
            "a request starts with the messages of the one before it"
        );
    }
}

async fn tool_texts(rt: &Runtime, view: &RunView) -> Vec<String> {
    conversation(rt, view)
        .await
        .messages
        .iter()
        .filter(|m| matches!(m, adam_model::Message::Tool { .. }))
        .map(adam_model::Message::text)
        .collect()
}

#[tokio::test]
async fn a_long_result_is_kept_in_the_state_and_read_back_whole_page_by_page() {
    let h = Harness::new();
    let text = rows();
    let agent = h
        .agent()
        .instructions("be brief")
        .tool(Wordy { text: text.clone() })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    let rt = h.runtime(&agent);
    // The id names the run, so the run starts before the model is scripted to read it.
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let id = adam_llm_agent::output_id(&run.to_string(), "c1");
    h.mock.push_tool_calls(vec![call("c1", "wordy", json!({}))]);
    for offset in [2001, 4001, 6001] {
        h.mock.push_tool_calls(vec![call(
            &format!("r{offset}"),
            "read_output",
            json!({"id": id, "offset": offset}),
        )]);
    }
    h.mock.push_text("done");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let state = conversation(&rt, &view).await;
    assert_eq!(state.kept_outputs.get(&id), Some(&text));
    let texts = tool_texts(&rt, &view).await;
    assert_eq!(texts.len(), 4);
    let head = format!("[kept as {id}: 7000 lines; read_output {{\"id\":\"{id}\"}} reads it]\n");
    assert!(texts[0].starts_with(&head), "{}", &texts[0][..120]);
    assert!(
        texts[0].ends_with("[more: 5000 lines left; next page: offset=2001]"),
        "{}",
        &texts[0][texts[0].len() - 80..]
    );
    let mut all = String::new();
    for (n, said) in texts.iter().enumerate() {
        let body = said.strip_prefix(&head).unwrap_or(said);
        let (lines, last) = body.rsplit_once('\n').unwrap();
        all.push_str(lines);
        all.push('\n');
        if n < 3 {
            assert!(last.starts_with("[more: "), "{last}");
        } else {
            assert_eq!(last, "[end: 7000 lines]");
        }
    }
    assert_eq!(all, text, "no byte lost");
    assert_prefix_holds(&h.mock.requests());

    // The same call again gives the same bytes.
    let again = adam_llm_agent::page_of(&id, &text, 2001, 2000).unwrap();
    assert_eq!(again, texts[1]);
}

#[tokio::test]
async fn kept_output_survives_a_replay() {
    let h = Harness::new();
    let reached = Arc::new(Notify::new());
    let text = rows();
    let agent = h
        .agent()
        .tool(Wordy { text: text.clone() })
        .tool(AsyncTool2 {
            name: "b",
            calls: Arc::new(AtomicUsize::new(0)),
            first: Arc::new(AtomicBool::new(true)),
            reached: reached.clone(),
        })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "wordy", json!({})),
            call("c2", "b", json!({})),
        ])
        .push_text("done");
    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "crash-a", short_lease),
        h.runtime_with(&agent, "crash-b", short_lease),
    );
    let run = rt_a.start("llm", user_message("go"), None).await.unwrap();
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "tool b starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;
    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;
    let id = adam_llm_agent::output_id(&run.to_string(), "c1");
    assert_eq!(
        conversation(&rt_b, &view).await.kept_outputs.get(&id),
        Some(&text),
        "the recorded result carried the kept text across the crash"
    );
}

#[tokio::test]
async fn a_result_over_the_runs_room_is_not_kept_and_says_so() {
    let h = Harness::new();
    let line = "x".repeat(70) + "\n";
    let text = line.repeat(adam_llm_agent::MAX_RUN_KEPT_BYTES / line.len() + 10);
    let agent = h
        .agent()
        .tool(Wordy { text })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "wordy", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert!(conversation(&rt, &view).await.kept_outputs.is_empty());
    let shown = &tool_texts(&rt, &view).await[0];
    assert!(
        shown.contains("[not kept: a run keeps at most 4 MiB of long results here"),
        "{}",
        &shown[shown.len().saturating_sub(200)..]
    );
    assert!(shown.starts_with(&"x".repeat(70)));
}

#[tokio::test]
async fn read_output_says_what_is_wrong_with_the_id() {
    let h = Harness::new();
    let agent = h.agent().tool(adam_llm_agent::ReadOutput).build();
    h.mock
        .push_tool_calls(vec![
            call("r1", "read_output", json!({"id": "../etc/passwd"})),
            call("r2", "read_output", json!({"id": "o-0123456789abcdef"})),
            call("r3", "read_output", json!({})),
        ])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    let said = tool_texts(&rt, &view).await;
    assert!(said[0].contains("is not an output id"), "{}", said[0]);
    assert!(
        said[1].contains("no kept output `o-0123456789abcdef` in this run"),
        "{}",
        said[1]
    );
    assert_eq!(said[2], "id is required");
}

#[tokio::test]
async fn with_a_store_in_the_state_the_text_is_not_in_the_run() {
    let h = Harness::new();
    let text = rows();
    let store = Arc::new(adam_llm_agent::KeptStore(Arc::new(
        adam_llm_agent::MemoryKept::new(),
    )));
    let agent = h
        .agent()
        .state(store)
        .tool(Wordy { text })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "wordy", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    let joined = conversation(&rt, &view).await;
    assert!(joined.kept_outputs.is_empty());
    assert!(tool_texts(&rt, &view).await[0].starts_with("[kept as o-"));
    assert!(
        !serde_json::to_string(&joined)
            .expect("the joined state is JSON")
            .contains("row-006999"),
        "nothing of the text but the first page is in the run's state"
    );
}

// ---------------------------------------------------------------------------
// Kept output, round 2: what the reviews found
// ---------------------------------------------------------------------------

/// Answers what it is given as a kept text, as an error result when `fail` says so.
struct Keeper {
    text: String,
    fail: bool,
}

#[async_trait]
impl Tool for Keeper {
    fn spec(&self) -> ToolSpec {
        spec("keeper")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let (shown, kept) = adam_llm_agent::keep_long(ctx, &self.text).await;
        let mut output = if self.fail {
            ToolOutput::error(shown)
        } else {
            ToolOutput::text(shown)
        };
        output.kept = kept;
        Ok(output)
    }
}

#[tokio::test]
async fn the_runs_four_mib_is_a_bound_and_every_id_the_model_was_shown_reads() {
    let h = Harness::new();
    let line = format!("{}\n", "x".repeat(70));
    let text = line.repeat(3 * 1024 * 1024 / line.len());
    let agent = h
        .agent()
        .tool(Keeper {
            text: text.clone(),
            fail: false,
        })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let first = adam_llm_agent::output_id(&run.to_string(), "c1");
    h.mock
        .push_tool_calls(vec![call("c1", "keeper", json!({}))])
        .push_tool_calls(vec![call("c2", "keeper", json!({}))])
        .push_tool_calls(vec![call(
            "r1",
            "read_output",
            json!({"id": first, "offset": -1}),
        )])
        .push_text("done");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    let said = tool_texts(&rt, &view).await;
    assert!(
        said[0].starts_with(&format!("[kept as {first}:")),
        "{}",
        &said[0][..80]
    );
    assert!(
        said[1].contains("[not kept: a run keeps at most 4 MiB"),
        "{}",
        said[1].chars().rev().take(200).collect::<String>()
    );
    assert!(
        !said[1].contains("[kept as"),
        "the second names no id: it is not kept"
    );
    assert!(said[2].contains("[end: "), "{}", said[2]);
    assert_eq!(conversation(&rt, &view).await.kept_outputs.len(), 1);
}

#[tokio::test]
async fn a_long_line_is_kept_folded_and_reassembles_from_its_pages() {
    let h = Harness::new();
    let text = format!("{}\nshort\n", "ab".repeat(20_000));
    let agent = h
        .agent()
        .tool(Keeper {
            text: text.clone(),
            fail: false,
        })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let id = adam_llm_agent::output_id(&run.to_string(), "c1");
    h.mock
        .push_tool_calls(vec![call("c1", "keeper", json!({}))])
        .push_tool_calls(vec![call(
            "r1",
            "read_output",
            json!({"id": id, "offset": 1}),
        )])
        .push_text("done");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    let state = conversation(&rt, &view).await;
    // What the state keeps is the prepared text (folded), not the raw one.
    let kept = state.kept_outputs.get(&id).unwrap();
    assert!(kept.lines().all(|l| l.chars().count() <= 2000));
    assert_eq!(kept.replace('\n', ""), text.replace('\n', ""));
    let said = tool_texts(&rt, &view).await;
    let page = &said[1];
    let body = page.rsplit_once('\n').unwrap().0;
    assert_eq!(body.replace('\n', ""), text.replace('\n', ""));
}

#[tokio::test]
async fn an_error_result_never_puts_a_text_in_the_state() {
    let h = Harness::new();
    let agent = h
        .agent()
        .tool(Keeper {
            text: rows(),
            fail: true,
        })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "keeper", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert!(conversation(&rt, &view).await.kept_outputs.is_empty());
}

/// With a keeper in the state, what the model is shown of the first page is made from what the
/// keeper kept (scrubbed by it), never from the text the tool gave.
#[tokio::test]
async fn the_first_page_is_made_from_what_the_keeper_kept() {
    let h = Harness::new();
    let secret = "tok-1234-secret-value";
    let text = format!("first line {secret}\n{}", rows());
    let store = Arc::new(adam_llm_agent::KeptStore(Arc::new(
        adam_llm_agent::MemoryKept::new()
            .scrubbing(|t| t.replace("tok-1234-secret-value", "[redacted]")),
    )));
    let agent = h
        .agent()
        .state(store)
        .tool(Keeper { text, fail: false })
        .tool(adam_llm_agent::ReadOutput)
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "keeper", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    let shown = &tool_texts(&rt, &view).await[0];
    assert!(!shown.contains(secret), "{}", &shown[..200]);
    assert!(shown.contains("first line [redacted]"), "{}", &shown[..200]);
}
