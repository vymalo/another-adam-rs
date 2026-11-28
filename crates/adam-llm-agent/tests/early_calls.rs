//! A tool call starts as soon as its arguments are complete in the model's stream (ADR 0049,
//! amended 2026-11-18): the call runs beside the stream, its step is written after the turn, and a
//! call that cannot start early waits for the turn exactly as before.
//!
//! The model here is scripted step by step, so a stream can wait for a tool to begin (or never
//! finish), and the probe records what ran and when.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, LlmAgentBuilder, Tool, ToolAccess, ToolCtx, ToolError, ToolOutput,
    user_message,
};
use adam_model::{
    ContentPart, FinishReason, Message, ModelClient, ModelDelta, ModelError, ModelRequest,
    ModelResponse, ToolCall, ToolSpec, Usage,
};
use adam_runtime::{
    AGENT_TEXT_KIND, CollectingSink, RetryPolicy, RunEvent, RunView, Runtime, StepState,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// A scripted model
// ---------------------------------------------------------------------------

/// One thing that happens in a scripted stream, in order.
#[allow(
    clippy::large_enum_variant,
    reason = "a scripted step is the value; boxing it would allocate for every step"
)]
enum Step {
    /// An item of the stream.
    Delta(Result<ModelDelta, ModelError>),
    /// Nothing more arrives until `signal` is notified.
    Wait(Arc<Notify>),
    /// Nothing more ever arrives: the process is killed, or the run is cancelled.
    Hang,
}

/// A model whose every call takes the next script: `stream` plays it, `complete` answers with the
/// `Finished` it holds (the non-streaming path, which never announces a call).
struct Scripted {
    scripts: Mutex<VecDeque<Vec<Step>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Scripted {
    fn new(scripts: Vec<Vec<Step>>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn next_script(&self) -> Vec<Step> {
        self.scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("a script for every call")
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelClient for Scripted {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(request);
        for step in self.next_script() {
            match step {
                Step::Delta(Ok(ModelDelta::Finished(response))) => return Ok(response),
                Step::Delta(Err(error)) => return Err(error),
                Step::Delta(_) => {}
                Step::Wait(_) | Step::Hang => break,
            }
        }
        Err(ModelError::invalid_request("the script has no answer"))
    }

    async fn stream(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.requests.lock().unwrap().push(request);
        let steps = self.next_script();
        Ok(stream::unfold(steps.into_iter(), |mut steps| async move {
            loop {
                match steps.next()? {
                    Step::Delta(item) => return Some((item, steps)),
                    Step::Wait(signal) => signal.notified().await,
                    Step::Hang => std::future::pending::<()>().await,
                }
            }
        })
        .boxed())
    }
}

fn started(id: &str, name: &str) -> Step {
    Step::Delta(Ok(ModelDelta::ToolCallStarted {
        id: id.into(),
        name: name.into(),
    }))
}

fn complete(call: &ToolCall) -> Step {
    Step::Delta(Ok(ModelDelta::ToolCallComplete(call.clone())))
}

fn text(words: &str) -> Step {
    Step::Delta(Ok(ModelDelta::Text(words.into())))
}

fn finished(response: ModelResponse) -> Step {
    Step::Delta(Ok(ModelDelta::Finished(response)))
}

/// A stream that says `words` and ends.
fn answer(words: &str) -> Vec<Step> {
    vec![text(words), finished(ModelResponse::text(words))]
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall::new(id, name, args)
}

/// The turn that asks for `calls` and says `words` before them.
fn asks(calls: Vec<ToolCall>, words: &str) -> ModelResponse {
    ModelResponse {
        message: Message::Assistant {
            extra_content: None,
            content: if words.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::text(words)]
            },
            tool_calls: calls,
            reasoning: None,
            provider: None,
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
        reasoning: None,
    }
}

/// A turn of `calls`, each announced and completed, and then the response.
fn turn(calls: Vec<ToolCall>) -> Vec<Step> {
    let mut steps = Vec::new();
    for call in &calls {
        steps.push(started(&call.id, &call.name));
        steps.push(complete(call));
    }
    steps.push(text("working"));
    steps.push(finished(asks(calls, "working")));
    steps
}

// ---------------------------------------------------------------------------
// A probe that says when it started
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Log {
    events: Mutex<Vec<String>>,
    starts: Mutex<HashMap<String, usize>>,
    ends: Mutex<HashMap<String, usize>>,
    args: Mutex<HashMap<String, Value>>,
}

impl Log {
    fn count(map: &Mutex<HashMap<String, usize>>, id: &str) -> usize {
        map.lock().unwrap().get(id).copied().unwrap_or(0)
    }

    fn starts(&self, id: &str) -> usize {
        Self::count(&self.starts, id)
    }

    fn ends(&self, id: &str) -> usize {
        Self::count(&self.ends, id)
    }

    fn args(&self, id: &str) -> Option<Value> {
        self.args.lock().unwrap().get(id).cloned()
    }
}

/// A tool whose arguments say what the call does: `ms` (how long it takes), `use`
/// (`independent`, `r:<scope>`, `w:<scope>`; none means the tool declares nothing and runs alone),
/// `out` (a fixed answer), `hang` (never returns), `ask` (asks although it declared nothing of the
/// kind), `fast` (answers without awaiting anything), `fail: "transient"` (fails transiently, at
/// once). It says when it began, on `started`.
struct Probe {
    name: &'static str,
    log: Arc<Log>,
    started: Arc<Notify>,
    /// The call stops when the run is cancelled.
    heeds_cancel: bool,
}

impl Probe {
    fn new(log: &Arc<Log>, started: &Arc<Notify>) -> Self {
        Self {
            name: "probe",
            log: log.clone(),
            started: started.clone(),
            heeds_cancel: true,
        }
    }
}

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "probe".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn access(&self, args: &Value) -> ToolAccess {
        match args.get("use").and_then(Value::as_str) {
            Some("independent") => ToolAccess::Independent,
            Some(other) => match other.split_once(':') {
                Some(("r", scope)) => ToolAccess::reads(scope),
                Some(("w", scope)) => ToolAccess::writes(scope),
                _ => ToolAccess::Alone,
            },
            None => ToolAccess::Alone,
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let id = ctx.call_id().to_owned();
        self.log.events.lock().unwrap().push(format!("start:{id}"));
        self.log
            .args
            .lock()
            .unwrap()
            .insert(id.clone(), args.clone());
        *self
            .log
            .starts
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default() += 1;
        self.started.notify_one();
        if args.get("fast").and_then(Value::as_bool) == Some(true) {
            // A call that ends without awaiting anything: its future is ready on its first poll.
            self.log.events.lock().unwrap().push(format!("end:{id}"));
            *self.log.ends.lock().unwrap().entry(id.clone()).or_default() += 1;
            return Ok(ToolOutput::text(
                args.get("out")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("ok:{id}"), str::to_owned),
            ));
        }
        if args.get("fail").and_then(Value::as_str) == Some("transient") {
            // The end is logged, then the failure, with no wait: the stream loop polls the call
            // before the next delta, so a later call must see it.
            self.log.events.lock().unwrap().push(format!("end:{id}"));
            *self.log.ends.lock().unwrap().entry(id.clone()).or_default() += 1;
            return Err(ToolError::Transient("the probe failed".into()));
        }
        let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(0);
        let sleep = tokio::time::sleep(Duration::from_millis(ms));
        let stopped = if self.heeds_cancel {
            tokio::select! {
                () = sleep => false,
                () = ctx.cancelled() => true,
            }
        } else {
            sleep.await;
            false
        };
        // The call ended, whatever it ends with (a cancel stops it where it is).
        self.log.events.lock().unwrap().push(format!("end:{id}"));
        *self.log.ends.lock().unwrap().entry(id.clone()).or_default() += 1;
        if stopped {
            return Err(ToolError::Permanent("stopped".into()));
        }
        if args.get("hang").and_then(Value::as_bool) == Some(true) {
            std::future::pending::<()>().await;
        }
        if args.get("ask").and_then(Value::as_bool) == Some(true) {
            return Err(ToolError::needs_input("which one?"));
        }
        Ok(ToolOutput::text(
            args.get("out")
                .and_then(Value::as_str)
                .map_or_else(|| format!("ok:{id}"), str::to_owned),
        ))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    model: Arc<Scripted>,
    sink: CollectingSink,
    store: DynStore,
}

impl Harness {
    fn new() -> Self {
        Self {
            model: Scripted::new(Vec::new()),
            sink: CollectingSink::new(),
            store: Arc::new(MemoryStore::new()),
        }
    }

    fn script(&self, steps: Vec<Step>) {
        self.model.scripts.lock().unwrap().push_back(steps);
    }

    fn agent(
        &self,
        limit: usize,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> LlmAgent {
        configure(LlmAgent::builder("llm", self.model.clone(), "test-model").max_parallel(limit))
            .build()
    }

    fn runtime(&self, agent: LlmAgent, ttl: Duration) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(agent)
            .event_sink(self.sink.clone())
            .worker_id(format!("w-{}", RunId::new()))
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(ttl)
            .retry(
                RetryPolicy::new(3)
                    .with_backoff(Duration::from_millis(10), Duration::from_millis(10))
                    .with_multiplier(1.0),
            )
            .build()
    }

    /// One task on a fresh runtime, to its end.
    async fn run(&self, agent: LlmAgent, message: &str) -> (Runtime, RunId, RunView) {
        let rt = self.runtime(agent, Duration::from_secs(10));
        let run = rt
            .start("llm", user_message(message), None)
            .await
            .expect("start");
        let worker = Worker::spawn(&rt);
        let view = wait_terminal(&rt, run).await;
        worker.stop().await;
        (rt, run, view)
    }

    async fn journal(&self, run: RunId) -> Vec<(u64, String)> {
        self.store
            .journal_list(run)
            .await
            .expect("journal")
            .into_iter()
            .map(|e| (e.seq, e.name))
            .collect()
    }

    /// The step events of the run, in order.
    fn steps(&self, run: RunId) -> Vec<(String, StepState)> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter_map(|e| match e {
                RunEvent::Step(step) => Some((step.id, step.state)),
                _ => None,
            })
            .collect()
    }

    /// The `agent_text` events of the run, in order.
    fn words(&self, run: RunId) -> Vec<String> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter_map(|e| match e {
                RunEvent::Custom { kind, payload } if kind == AGENT_TEXT_KIND => {
                    payload["text"].as_str().map(str::to_owned)
                }
                _ => None,
            })
            .collect()
    }
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
}

impl Worker {
    fn spawn(rt: &Runtime) -> Self {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = rt.clone();
        let handle = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        });
        Self { stop, handle }
    }

    /// The process dies: the worker's task is aborted, and with it every transition it runs.
    async fn kill(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        self.handle
            .await
            .expect("worker task")
            .expect("worker result");
    }
}

async fn wait_terminal(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn conversation(rt: &Runtime, view: &RunView) -> Conversation {
    let state = rt.state(view.id).await.expect("state").expect("the run");
    serde_json::from_value(state).expect("state is a Conversation")
}

async fn tool_answers(rt: &Runtime, view: &RunView) -> Vec<(String, bool)> {
    conversation(rt, view)
        .await
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. A call starts before the stream ends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_call_starts_before_the_stream_ends() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "independent", "ms": 0}));
    // The stream says the call's arguments are whole, then waits for the tool to begin: a loop
    // that starts the call only after the stream would wait for ever here.
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        Step::Wait(began.clone()),
        text("working"),
        finished(asks(vec![c1], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = timeout(Duration::from_secs(20), h.run(agent, "go"))
        .await
        .expect("the call started while the model was still writing");
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 1, "the call ran once");
    assert_eq!(log.ends("c1"), 1);
    // The step said it was running before the model's words arrived: the call ran beside the stream.
    let events = h.sink.events_for(run);
    let running = events
        .iter()
        .position(|e| matches!(e, RunEvent::Step(step) if step.id == "tool:c1" && step.state == StepState::Running))
        .expect("the step started");
    let first_text = events
        .iter()
        .position(|e| matches!(e, RunEvent::TextDelta { .. }))
        .expect("the turn wrote after the call began");
    assert!(
        running < first_text,
        "the call ran while the stream went on: {events:#?}"
    );
    assert_eq!(
        h.journal(run)
            .await
            .iter()
            .map(|(_, n)| n.as_str())
            .collect::<Vec<_>>(),
        ["model:0", "tool:c1", "model:1"],
        "the turn is written before the call's step"
    );
}

// ---------------------------------------------------------------------------
// 2. A barrier does not start early
// ---------------------------------------------------------------------------

/// The start of `id` comes after the last `TextDelta` of the turn that asked for it and after that
/// turn's `agent_text`.
fn assert_waited_for_the_turn(h: &Harness, run: RunId, id: &str) {
    let events = h.sink.events_for(run);
    let words = events
        .iter()
        .position(|e| matches!(e, RunEvent::Custom { kind, .. } if kind == AGENT_TEXT_KIND))
        .expect("the turn said its words");
    let last_text = events[..words]
        .iter()
        .rposition(|e| matches!(e, RunEvent::TextDelta { .. }))
        .expect("the turn wrote");
    let running = events
        .iter()
        .position(|e| {
            matches!(e, RunEvent::Step(step) if step.id == format!("tool:{id}") && step.state == StepState::Running)
        })
        .expect("the call started");
    assert!(
        last_text < running && words < running,
        "tool:{id} waited for the turn: {events:#?}"
    );
}

#[tokio::test]
async fn a_call_that_asks_to_follow_the_ones_before_it_does_not_start_early() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call(
        "c1",
        "probe",
        json!({"use": "independent", "after_previous": true, "ms": 0}),
    );
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        text("working"),
        finished(asks(vec![c1], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 1);
    assert_waited_for_the_turn(&h, run, "c1");
    // The argument is the loop's: the tool never sees it.
    assert_eq!(
        log.args("c1").expect("the call ran"),
        json!({"use": "independent", "ms": 0})
    );
}

#[tokio::test]
async fn a_tool_that_declares_nothing_does_not_start_early() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"ms": 0}));
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        text("working"),
        finished(asks(vec![c1], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_waited_for_the_turn(&h, run, "c1");
}

#[tokio::test]
async fn a_call_that_repeats_the_one_before_it_does_not_start_early() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let args = json!({"use": "independent", "ms": 0, "tag": 1});
    let (c1, c2) = (call("c1", "probe", args.clone()), call("c2", "probe", args));
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        started("c2", "probe"),
        complete(&c2),
        text("working"),
        finished(asks(vec![c1, c2], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    // The first call may start early; the second repeats its key, which the doom loop needs the
    // result of, so it waits for the turn.
    assert_eq!(log.starts("c1"), 1);
    assert_eq!(log.starts("c2"), 1);
    assert_waited_for_the_turn(&h, run, "c2");
}

/// The gate must not start a call before an earlier call of the turn it conflicts with, when that
/// earlier call did not start early: the turn runs it, in the segment's order, and the early call
/// would have run before it (ADR 0049 decisions 1 and 5, and the amendment's "starting it early
/// cannot change what the turn would have done").
///
/// Here c1 reads `files/x` for long enough to still be running when the turn's calls arrive; c2
/// writes `files` and conflicts with c1, so the gate refuses it; c3 writes `files/y`, which
/// conflicts with c2 but not with c1, so the gate used to admit it — and it ran before c2, whose
/// write the model ordered first.
#[tokio::test]
async fn a_call_that_conflicts_with_an_earlier_refused_call_waits_for_it() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "r:files/x", "ms": 400}));
    let c2 = call("c2", "probe", json!({"use": "w:files", "ms": 0}));
    let c3 = call("c3", "probe", json!({"use": "w:files/y", "ms": 0}));
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        started("c2", "probe"),
        complete(&c2),
        started("c3", "probe"),
        complete(&c3),
        text("working"),
        finished(asks(vec![c1, c2, c3], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    let events = log.events.lock().unwrap().clone();
    let start = |id: &str| {
        events
            .iter()
            .position(|e| e == &format!("start:{id}"))
            .unwrap_or_else(|| panic!("{id} did not start: {events:?}"))
    };
    assert!(
        start("c2") < start("c3"),
        "c3 conflicts with c2 and must not run before it: {events:?}"
    );
    // c2 itself waited for the turn: the premise (c1 still running when the turn arrived) held.
    assert_waited_for_the_turn(&h, run, "c2");
    assert_eq!(log.starts("c2"), 1);
    assert_eq!(log.starts("c3"), 1);
}

// ---------------------------------------------------------------------------
// 3. A cancel mid-stream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_mid_stream_ends_the_started_call_within_the_grace() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "independent", "ms": 60_000}));
    h.script(vec![started("c1", "probe"), complete(&c1), Step::Hang]);
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    while log.starts("c1") == 0 {
        assert!(Instant::now() < deadline, "the call did not start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let cancelled = Instant::now();
    rt.cancel(run, "stop").await.expect("cancel");
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Failed);
    assert!(
        view.error.as_deref().is_some_and(|e| e.contains("cancel")),
        "{:?}",
        view.error
    );
    assert_eq!(log.ends("c1"), 1, "the started call ended");
    assert!(
        cancelled.elapsed() < adam_llm_agent::TOOL_STOP_GRACE + Duration::from_secs(1),
        "the call ended within the grace: {:?}",
        cancelled.elapsed()
    );
    assert_eq!(h.model.calls(), 1, "the model was not asked again");
    let journal = h.journal(run).await;
    assert!(
        journal.iter().all(|(_, name)| !name.starts_with("tool:")),
        "a cancelled turn writes no tool step: {journal:?}"
    );
}

// ---------------------------------------------------------------------------
// 3b. A cancel, for a call that does not stop
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_cancel_drops_a_started_call_that_does_not_stop_after_the_grace() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call(
        "c1",
        "probe",
        json!({"use": "independent", "ms": 3_600_000}),
    );
    h.script(vec![started("c1", "probe"), complete(&c1), Step::Hang]);
    let probe = Probe {
        heeds_cancel: false,
        ..Probe::new(&log, &began)
    };
    let agent = h.agent(10, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    while log.starts("c1") == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let cancelled = tokio::time::Instant::now();
    rt.cancel(run, "stop").await.expect("cancel");
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Failed);
    let took = cancelled.elapsed();
    assert!(
        took >= adam_llm_agent::TOOL_STOP_GRACE
            && took < adam_llm_agent::TOOL_STOP_GRACE + Duration::from_secs(1),
        "the loop waits TOOL_STOP_GRACE for a call that ignores the cancel, not its time limit: {took:?}"
    );
}

// ---------------------------------------------------------------------------
// 3c. A transient failure of an early call stops new starts
// ---------------------------------------------------------------------------

/// ADR 0049 decision 9: a transient failure of one call stops new starts. Here c1 starts early and
/// fails transiently; c2 is announced after it, and must wait for the turn (the retry) rather than
/// start beside it.
#[tokio::test]
async fn a_transient_failure_of_an_early_call_stops_new_starts() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call(
        "c1",
        "probe",
        json!({"use": "independent", "ms": 0, "fail": "transient"}),
    );
    let c2 = call("c2", "probe", json!({"use": "independent", "ms": 0}));
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        started("c2", "probe"),
        complete(&c2),
        text("working"),
        finished(asks(vec![c1, c2.clone()], "working")),
    ]);
    // The retry asks again, with a call that succeeds.
    let retry = call("c1b", "probe", json!({"use": "independent", "ms": 0}));
    h.script(turn(vec![retry, c2.clone()]));
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    // c2 runs at the failed try's turn (the segment starts it beside the failed c1) and again on
    // the retry; what matters is that it never started beside the stream, after c1's failure.
    assert!(log.starts("c2") >= 1);
    assert_waited_for_the_turn(&h, run, "c2");
}

// ---------------------------------------------------------------------------
// 3d. The limit, and a busy stream
// ---------------------------------------------------------------------------

/// The limit bounds the calls that run beside the stream: with two running, a third waits for the
/// turn even though it conflicts with neither.
#[tokio::test]
async fn the_limit_bounds_the_calls_that_start_beside_the_stream() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call(
        "c1",
        "probe",
        json!({"use": "independent", "ms": 400, "tag": 1}),
    );
    let c2 = call(
        "c2",
        "probe",
        json!({"use": "independent", "ms": 400, "tag": 2}),
    );
    let c3 = call(
        "c3",
        "probe",
        json!({"use": "independent", "ms": 0, "tag": 3}),
    );
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        started("c2", "probe"),
        complete(&c2),
        started("c3", "probe"),
        complete(&c3),
        text("working"),
        finished(asks(vec![c1, c2, c3], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(2, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    let events = h.sink.events_for(run);
    let words = events
        .iter()
        .position(|e| matches!(e, RunEvent::Custom { kind, .. } if kind == AGENT_TEXT_KIND))
        .expect("the turn said its words");
    let running = events
        .iter()
        .position(|e| {
            matches!(e, RunEvent::Step(step) if step.id == "tool:c3" && step.state == StepState::Running)
        })
        .expect("c3 started");
    assert!(
        words < running,
        "c3 waited for a slot and started at the turn: {events:#?}"
    );
}

/// A stream that keeps producing deltas does not starve the calls beside it: a call that finishes
/// during the burst is taken (its slot is free), so a later call still starts beside the stream.
#[tokio::test]
async fn a_busy_stream_still_takes_a_finished_call() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call(
        "c1",
        "probe",
        json!({"use": "independent", "fast": true, "tag": 1}),
    );
    let c2 = call(
        "c2",
        "probe",
        json!({"use": "independent", "fast": true, "tag": 2}),
    );
    let c3 = call(
        "c3",
        "probe",
        json!({"use": "independent", "fast": true, "tag": 3}),
    );
    let mut steps = vec![started("c1", "probe"), complete(&c1)];
    // A burst of ready text deltas: nothing in it waits for the runtime.
    for _ in 0..500 {
        steps.push(text("x"));
    }
    steps.push(started("c2", "probe"));
    steps.push(complete(&c2));
    steps.push(started("c3", "probe"));
    steps.push(complete(&c3));
    steps.push(finished(asks(vec![c1, c2, c3], &"x".repeat(500))));
    h.script(steps);
    h.script(answer("done"));
    let agent = h.agent(2, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    let events = h.sink.events_for(run);
    let words = events
        .iter()
        .position(|e| matches!(e, RunEvent::Custom { kind, .. } if kind == AGENT_TEXT_KIND))
        .expect("the turn said its words");
    let running = events
        .iter()
        .position(|e| {
            matches!(e, RunEvent::Step(step) if step.id == "tool:c3" && step.state == StepState::Running)
        })
        .expect("c3 started");
    assert!(
        running < words,
        "c3 started beside the stream, so c1's result was taken during the burst: {events:#?}"
    );
    assert_eq!(log.starts("c3"), 1);
}

// ---------------------------------------------------------------------------
// 4. A stream error
// ---------------------------------------------------------------------------
#[tokio::test]
async fn a_stream_error_ends_the_started_call_and_the_retry_runs_it_again() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "independent", "ms": 0}));
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        Step::Delta(Err(ModelError::transient("connection reset"))),
    ]);
    h.script(turn(vec![c1]));
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 2, "the retry ran the call again");
    assert_eq!(log.ends("c1"), 2, "the failed try's call ended");
    let journal = h.journal(run).await;
    let names: Vec<&str> = journal.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(
        names.iter().filter(|name| **name == "tool:c1").count(),
        1,
        "the failed try journaled no tool step: {journal:?}"
    );
    let retried_model = names
        .iter()
        .enumerate()
        .filter(|(_, name)| **name == "model:0")
        .nth(1)
        .map(|(i, _)| i)
        .expect("the retry asked the model again");
    assert!(
        names
            .iter()
            .position(|name| *name == "tool:c1")
            .expect("the step")
            > retried_model,
        "the only tool step is the retry's: {journal:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. A crash after the call started
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_after_a_complete_call_writes_nothing_and_the_replay_asks_again() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "independent", "ms": 0}));
    h.script(vec![started("c1", "probe"), complete(&c1), Step::Hang]);
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let rt = h.runtime(agent, Duration::from_millis(300));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    while log.starts("c1") == 0 {
        assert!(Instant::now() < deadline, "the call did not start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The process dies while the stream hangs: the call had started, and no step was written.
    worker.kill().await;
    assert!(
        h.journal(run).await.is_empty(),
        "a crash mid-stream writes nothing"
    );

    // Another worker re-asks the model, which asks for the same call again.
    h.script(turn(vec![c1]));
    h.script(answer("done"));
    let rt = h.runtime(
        h.agent(10, |b| b.tool(Probe::new(&log, &began))),
        Duration::from_secs(10),
    );
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 2, "the replay ran the call again");
    assert_eq!(
        h.journal(run)
            .await
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>(),
        ["model:0", "tool:c1", "model:1"]
    );
}

/// ADR 0049's amendment: "a stream error ... **waits for the started calls within their own time
/// limits**, discards their results and fails the transition transiently; the retry asks the model
/// again and runs the calls again." With a call still running when the stream fails, the try must
/// wait for it — the probe logs its end only after its work — and the retry runs it again.
///
/// Red if the loop drops the started futures instead of awaiting them: `ends("c1")` would be 1,
/// because the probe never reached its end.
#[tokio::test]
async fn a_stream_error_waits_for_a_started_call_before_failing_the_try() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "independent", "ms": 200}));
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        Step::Delta(Err(ModelError::transient("connection reset"))),
    ]);
    h.script(turn(vec![c1]));
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 2, "the retry ran the call again");
    assert_eq!(
        log.ends("c1"),
        2,
        "the failed try waited for the call it had started"
    );
    let events: Vec<String> = log.events.lock().unwrap().clone();
    assert_eq!(
        events,
        ["start:c1", "end:c1", "start:c1", "end:c1"],
        "the started call ended before the retry began: {events:?}"
    );
}

/// ADR 0049's amendment: "at any limit the journal is `model:N`, then `tool:<id>` in call order".
/// A turn recorded at 10 — every call announced and completed early — is replayed at 1, where the
/// gate starts nothing early, and the calls run in the same order and journal the same way.
///
/// Red if the replay's order follows the limit instead of the calls: at 1 the turn would have to
/// reverse or interleave them for the assertion to fail.
#[tokio::test]
async fn a_replay_makes_the_same_calls_in_the_same_order_at_a_lower_limit() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let calls: Vec<ToolCall> = (1..=3)
        .map(|n| {
            call(
                &format!("c{n}"),
                "probe",
                json!({"use": "independent", "ms": 0, "tag": n}),
            )
        })
        .collect();
    let mut steps = Vec::new();
    for c in &calls {
        steps.push(started(&c.id, "probe"));
        steps.push(complete(c));
    }
    steps.push(Step::Hang);
    h.script(steps);
    let rt = h.runtime(
        h.agent(10, |b| b.tool(Probe::new(&log, &began))),
        Duration::from_millis(300),
    );
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    while log.starts("c3") == 0 {
        assert!(
            Instant::now() < deadline,
            "the third call did not start early"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    worker.kill().await;
    assert!(
        h.journal(run).await.is_empty(),
        "a crash mid-stream writes nothing"
    );
    let recorded: Vec<String> = log.events.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        6,
        "three calls started and ended in the first try: {recorded:?}"
    );

    // The same turn, at a limit where nothing starts early: the calls run one at a time, in order.
    h.script(turn(calls));
    h.script(answer("done"));
    let rt = h.runtime(
        h.agent(1, |b| b.tool(Probe::new(&log, &began))),
        Duration::from_secs(10),
    );
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(
        h.journal(run)
            .await
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>(),
        ["model:0", "tool:c1", "tool:c2", "tool:c3", "model:1"],
        "the turn, then the calls in call order"
    );
    let replayed: Vec<String> = log.events.lock().unwrap().clone()[6..].to_vec();
    assert_eq!(
        replayed,
        [
            "start:c1", "end:c1", "start:c2", "end:c2", "start:c3", "end:c3"
        ],
        "the replay ran the calls in the same order: {replayed:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. A doom-loop refusal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_doom_loop_refusal_is_not_started_early() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let same = || json!({"use": "independent", "ms": 0, "out": "same"});
    let (c1, c2) = (call("c1", "probe", same()), call("c2", "probe", same()));
    let (c3, c4) = (call("c3", "probe", same()), call("c4", "probe", same()));
    // Two identical calls answer the same thing: the third is refused, and the fourth refusal ends
    // the run with the closing turn.
    h.script(turn(vec![c1, c2]));
    h.script(turn(vec![c3.clone()]));
    h.script(turn(vec![c4.clone()]));
    h.script(answer("I will answer with what I know"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c3"), 0, "a refusal does not run");
    assert_eq!(log.starts("c4"), 0);
    let answers = tool_answers(&rt, &view).await;
    assert!(
        answers[2]
            .0
            .starts_with("Not run: this is the third call in a row"),
        "{answers:?}"
    );
    assert!(answers[2].1 && answers[3].1);
    assert_eq!(
        view.output.as_ref().and_then(|o| o.get("limit")),
        Some(&json!("repeated_call")),
        "{:?}",
        view.output
    );
    // The refused call's step is said as it is today: running, then failed, at the turn.
    let steps: Vec<StepState> = h
        .steps(run)
        .into_iter()
        .filter(|(id, _)| id == "tool:c3")
        .map(|(_, state)| state)
        .collect();
    assert_eq!(steps, [StepState::Running, StepState::Failed]);
    assert_waited_for_the_turn(&h, run, "c3");
}

// ---------------------------------------------------------------------------
// 7. Over max_tool_calls
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_turn_over_max_tool_calls_starts_nothing() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let (c1, c2) = (
        call(
            "c1",
            "probe",
            json!({"use": "independent", "ms": 0, "tag": 1}),
        ),
        call(
            "c2",
            "probe",
            json!({"use": "independent", "ms": 0, "tag": 2}),
        ),
    );
    let (c3, c4) = (
        call(
            "c3",
            "probe",
            json!({"use": "independent", "ms": 0, "tag": 3}),
        ),
        call(
            "c4",
            "probe",
            json!({"use": "independent", "ms": 0, "tag": 4}),
        ),
    );
    h.script(turn(vec![c1, c2]));
    h.script(turn(vec![c3.clone(), c4.clone()]));
    h.script(answer("closing words"));
    let agent = h.agent(10, |b| {
        b.tool(Probe::new(&log, &began))
            .limits(adam_llm_agent::Limits {
                max_tool_calls: 2,
                ..adam_llm_agent::Limits::default()
            })
    });
    let (rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c3"), 0, "nothing starts over the limit");
    assert_eq!(log.starts("c4"), 0);
    let answers = tool_answers(&rt, &view).await;
    assert_eq!(answers.len(), 4);
    assert!(
        answers[2].0.starts_with("not run:") && answers[2].1,
        "{answers:?}"
    );
    assert!(answers[3].0.starts_with("not run:") && answers[3].1);
    assert_eq!(
        view.output.as_ref().and_then(|o| o.get("limit")),
        Some(&json!("max_tool_calls"))
    );
    assert_eq!(h.words(run), ["working", "working", "closing words"]);
}

// ---------------------------------------------------------------------------
// 7b. A turn that goes over the limit after a call started
// ---------------------------------------------------------------------------

/// ADR 0049, amended, *What is not decided here*: a turn whose first call fits under
/// `max_tool_calls` starts it early, and a later call that pushes the turn over means every call
/// is answered "not run" — the early call's work is not undone. The gate's room check is per
/// position, so the call that fits starts and the one that does not waits (and is answered
/// without running). This pins the accepted loss so it cannot grow silently.
#[tokio::test]
async fn a_turn_that_goes_over_max_tool_calls_keeps_the_early_call_it_started() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call(
        "c1",
        "probe",
        json!({"use": "independent", "ms": 0, "tag": 1}),
    );
    let c2 = call(
        "c2",
        "probe",
        json!({"use": "independent", "ms": 0, "tag": 2}),
    );
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        started("c2", "probe"),
        complete(&c2),
        text("working"),
        finished(asks(vec![c1, c2], "working")),
    ]);
    h.script(answer("closing words"));
    let agent = h.agent(10, |b| {
        b.tool(Probe::new(&log, &began))
            .limits(adam_llm_agent::Limits {
                max_tool_calls: 1,
                ..adam_llm_agent::Limits::default()
            })
    });
    let (rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(
        log.starts("c1"),
        1,
        "the call that fit started early; its result is dropped with the turn's"
    );
    assert_eq!(log.starts("c2"), 0, "the call over the limit did not start");
    let answers = tool_answers(&rt, &view).await;
    assert_eq!(answers.len(), 2);
    assert!(
        answers
            .iter()
            .all(|(text, is_error)| text.starts_with("not run:") && *is_error),
        "{answers:?}"
    );
    assert_eq!(h.words(run), ["working", "closing words"]);
}

// ---------------------------------------------------------------------------
// 8. The journal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_journal_is_the_sequential_journal_whatever_the_limit() {
    let mut journals = Vec::new();
    let mut answers = Vec::new();
    for limit in [1usize, 10] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        let began = Arc::new(Notify::new());
        let calls = vec![
            call("c1", "probe", json!({"use": "independent", "ms": 30})),
            call("c2", "probe", json!({"use": "w:files/a", "ms": 20})),
            call("c3", "probe", json!({"use": "r:files/a", "ms": 10})),
            call("c4", "probe", json!({"ms": 5})),
            call("c5", "probe", json!({"use": "independent", "ms": 1})),
        ];
        h.script(turn(calls));
        h.script(answer("done"));
        let agent = h.agent(limit, |b| b.tool(Probe::new(&log, &began)));
        let (rt, run, view) = h.run(agent, "go").await;
        assert_eq!(view.status, RunStatus::Done, "limit {limit}: {view:?}");
        journals.push(h.journal(run).await);
        answers.push(tool_answers(&rt, &view).await);
    }
    assert_eq!(journals[0], journals[1]);
    assert_eq!(answers[0], answers[1]);
    let names: Vec<&str> = journals[0].iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "model:0", "tool:c1", "tool:c2", "tool:c3", "tool:c4", "tool:c5", "model:1"
        ]
    );
    assert_eq!(
        journals[0].iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        (0..7).collect::<Vec<u64>>()
    );
}

// ---------------------------------------------------------------------------
// The batch is the turn's, or it is dropped
// ---------------------------------------------------------------------------

/// A client that announces a call and then finishes with another one (a provider that interleaves
/// indices, or a stream that disagrees with its own answer): the batch is not the turn's, so it is
/// dropped and the call runs at the turn.
#[tokio::test]
async fn a_solo_batch_that_is_not_the_turn_is_dropped_and_the_call_runs_again() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let early = call(
        "c1",
        "probe",
        json!({"use": "independent", "ms": 0, "tag": "early"}),
    );
    let turn = call(
        "c1",
        "probe",
        json!({"use": "independent", "ms": 0, "tag": "turn"}),
    );
    h.script(vec![
        started("c1", "probe"),
        complete(&early),
        text("working"),
        finished(asks(vec![turn], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (_rt, _, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(
        log.starts("c1"),
        2,
        "the early result is not the turn's call: the call runs again"
    );
    assert_eq!(
        log.args("c1").expect("the call ran")["tag"],
        "turn",
        "the turn's call is the one that counts"
    );
}

/// The same, for a segment of two: one call that is not the turn's drops the whole batch.
#[tokio::test]
async fn a_segment_batch_that_is_not_the_turn_is_dropped_whole() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let early: Vec<ToolCall> = ["c1", "c2"]
        .iter()
        .map(|id| {
            call(
                id,
                "probe",
                json!({"use": "independent", "ms": 0, "tag": format!("early-{id}")}),
            )
        })
        .collect();
    let turn: Vec<ToolCall> = ["c1", "c2"]
        .iter()
        .map(|id| {
            call(
                id,
                "probe",
                json!({"use": "independent", "ms": 0, "tag": format!("turn-{id}")}),
            )
        })
        .collect();
    let mut steps = Vec::new();
    for call in &early {
        steps.push(started(&call.id, &call.name));
        steps.push(complete(call));
    }
    steps.push(text("working"));
    steps.push(finished(asks(turn, "working")));
    h.script(steps);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (rt, _, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 2, "the batch was dropped whole");
    assert_eq!(log.starts("c2"), 2);
    assert_eq!(log.args("c1").expect("the call ran")["tag"], "turn-c1");
    assert_eq!(
        tool_answers(&rt, &view).await,
        [("ok:c1".to_owned(), false), ("ok:c2".to_owned(), false)]
    );
}

// ---------------------------------------------------------------------------
// The gate and the stream, unit level
// ---------------------------------------------------------------------------

/// A tool that is cancelled never runs again, and a call the gate refuses is not started at all:
/// the stream's announcements do not change what the turn does.
#[tokio::test]
async fn a_call_that_asks_although_it_declared_independent_is_told_to_run_alone() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call(
        "c1",
        "probe",
        json!({"use": "independent", "ms": 0, "ask": true}),
    );
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        text("working"),
        finished(asks(vec![c1], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (rt, _, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(
        tool_answers(&rt, &view).await,
        [(
            "this tool cannot run alongside others: call it alone".to_owned(),
            true
        )]
    );
}

/// A call that was already running when the turn arrived is not started a second time, and the
/// tool is not asked twice.
#[tokio::test]
async fn an_early_call_is_not_run_again_at_the_turn() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "independent", "ms": 50}));
    let c2 = call("c2", "probe", json!({"ms": 0}));
    // c2 declares nothing, so it is a barrier: c1 is a segment of its own, and its early result is
    // the one the turn records.
    h.script(vec![
        started("c1", "probe"),
        complete(&c1),
        started("c2", "probe"),
        complete(&c2),
        text("working"),
        finished(asks(vec![c1, c2], "working")),
    ]);
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)));
    let (rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 1, "the early call ran once");
    assert_eq!(log.starts("c2"), 1);
    assert_eq!(
        h.journal(run)
            .await
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>(),
        ["model:0", "tool:c1", "tool:c2", "model:1"]
    );
    assert_eq!(
        tool_answers(&rt, &view).await,
        [("ok:c1".to_owned(), false), ("ok:c2".to_owned(), false)]
    );
}

/// The non-streaming path announces nothing, so nothing starts early: a client that cannot stream
/// keeps the old timing.
#[tokio::test]
async fn a_turn_that_does_not_stream_starts_nothing_early() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let began = Arc::new(Notify::new());
    let c1 = call("c1", "probe", json!({"use": "independent", "ms": 0}));
    h.script(turn(vec![c1]));
    h.script(answer("done"));
    let agent = h.agent(10, |b| b.tool(Probe::new(&log, &began)).stream_text(false));
    let (_rt, run, view) = h.run(agent, "go").await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(log.starts("c1"), 1);
    let events = h.sink.events_for(run);
    let words = events
        .iter()
        .position(|e| matches!(e, RunEvent::Custom { kind, .. } if kind == AGENT_TEXT_KIND))
        .expect("the turn said its words");
    let running = events
        .iter()
        .position(|e| {
            matches!(e, RunEvent::Step(step) if step.id == "tool:c1" && step.state == StepState::Running)
        })
        .expect("the call started");
    assert!(words < running, "the call waited for the turn: {events:#?}");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RunEvent::TextDelta { .. }))
    );
}
