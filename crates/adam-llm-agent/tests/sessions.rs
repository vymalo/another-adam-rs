//! Child sessions seen from an `LlmAgent` parent (ADR 0045, slice 2): the five tools that run a
//! subagent in the background (`subagent_start`, `_send`, `_read`, `_close`, `_list`), the wait a
//! parent's turn makes for the sessions it started, and the interleavings that must not change what
//! the model is told.
//!
//! Run against `MemoryStore` always, against PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set and
//! against MongoDB when `ADAM_TEST_MONGODB_URI` is set. Nothing here sleeps for a fixed time: the
//! child's model call is held at a gate, the 60 s wait timer is moved with a `ManualClock`, and the
//! few polls wait for a state with a deadline.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, DynTool, LlmAgent, LlmAgentBuilder, PendingSession, PendingWait, SessionAgent,
    SessionTools, Tool, ToolCtx, ToolError, ToolOutput, user_message,
};
use adam_model::{
    DynModel, Message, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse,
    ToolCall, Usage,
};
use adam_runtime::{
    CHILD_REPORTED_KIND, CollectingSink, Inbound, ManualClock, RetryPolicy, RunView, Runtime,
    RuntimeBuilder, RuntimeError, SessionHeader, SessionLimits, SessionState, child_run_id,
};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn memory_store() -> Option<DynStore> {
    Some(Arc::new(MemoryStore::new()))
}

async fn postgres_store() -> Option<DynStore> {
    let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
    let store = adam_store_postgres::PgStore::connect(&url)
        .await
        .expect("connect to postgres");
    adam_core::Store::migrate(&store).await.expect("migrate");
    Some(Arc::new(store))
}

async fn mongodb_store() -> Option<DynStore> {
    let uri = adam_core::testing::test_env("ADAM_TEST_MONGODB_URI")?;
    let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let store = adam_store_mongodb::MongoStore::connect(&uri, &db)
        .await
        .expect("connect to mongodb")
        .with_collection_prefix("adam_llm_")
        .expect("collection prefix");
    adam_core::Store::migrate(&store).await.expect("migrate");
    Some(Arc::new(store))
}

fn faulty(store: DynStore) -> (Arc<FaultyStore>, DynStore) {
    let faulty = Arc::new(FaultyStore::new(store));
    let dynamic: DynStore = faulty.clone();
    (faulty, dynamic)
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall::new(id, name, args)
}

/// A gate a step waits at until the test lets it go.
#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

/// Wraps a model: the `hold_on`-th call (0-based) waits at a gate, the first time, before it is
/// answered. That is a child whose round is still working.
struct GateModel {
    inner: Arc<MockModel>,
    hold_on: usize,
    calls: AtomicUsize,
    armed: AtomicBool,
    gate: Arc<Gate>,
}

impl GateModel {
    async fn hold(&self) {
        let n = self.calls.fetch_add(1, SeqCst);
        if n == self.hold_on && self.armed.swap(false, SeqCst) {
            self.gate.reached.notify_one();
            self.gate.release.notified().await;
        }
    }
}

#[async_trait]
impl ModelClient for GateModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.hold().await;
        self.inner.complete(req).await
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.hold().await;
        self.inner.stream(req).await
    }
}

/// A tool that calls another and then fails its first try transiently: the retry runs the tool
/// again, as a replay would, and must find what the first try did.
///
/// It wraps the tools whose calls are committed before they run (start, send and close: the ones
/// with an effect). A read is not one of them: its turn and its call are one transition, so a try
/// of it that fails asks the model again, which a scripted model cannot answer twice.
struct Blip {
    inner: DynTool,
}

#[async_trait]
impl Tool for Blip {
    fn spec(&self) -> adam_model::ToolSpec {
        self.inner.spec()
    }
    fn paged(&self) -> bool {
        self.inner.paged()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let out = self.inner.call(ctx, args).await?;
        if ctx.attempt() == 0 {
            return Err(ToolError::Transient("blip".into()));
        }
        Ok(out)
    }
    async fn poll_remote(
        &self,
        ctx: &ToolCtx,
        task: &str,
    ) -> Result<adam_llm_agent::RemotePoll, ToolError> {
        self.inner.poll_remote(ctx, task).await
    }
}

/// A tool that does what `inner` does for the one call `call`, tells the test, and then fails its
/// first try transiently: the retry runs the call again after the effect, as a lost record would.
struct BlipOn {
    inner: DynTool,
    call: String,
    attempted: Arc<Notify>,
}

#[async_trait]
impl Tool for BlipOn {
    fn spec(&self) -> adam_model::ToolSpec {
        self.inner.spec()
    }
    fn paged(&self) -> bool {
        self.inner.paged()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let out = self.inner.call(ctx, args).await?;
        if ctx.call_id() == self.call && ctx.attempt() == 0 {
            self.attempted.notify_one();
            return Err(ToolError::Transient("blip".into()));
        }
        Ok(out)
    }
    async fn poll_remote(
        &self,
        ctx: &ToolCtx,
        task: &str,
    ) -> Result<adam_llm_agent::RemotePoll, ToolError> {
        self.inner.poll_remote(ctx, task).await
    }
}

/// A tool the parent calls that does not return until the test lets it: the parent does nothing
/// while a round runs to its end, and nobody has read it.
#[derive(Default)]
struct HoldTool {
    reached: Notify,
    release: Notify,
}

struct HoldHandle(Arc<HoldTool>);

#[async_trait]
impl Tool for HoldHandle {
    fn spec(&self) -> adam_model::ToolSpec {
        adam_model::ToolSpec {
            name: "hold".into(),
            description: "waits for the test".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.0.reached.notify_one();
        self.0.release.notified().await;
        Ok(ToolOutput::text("held"))
    }
}

/// A tool that fails its first try transiently: a later call of the same turn that fails the try.
struct Flaky;

#[async_trait]
impl Tool for Flaky {
    fn spec(&self) -> adam_model::ToolSpec {
        adam_model::ToolSpec {
            name: "flaky".into(),
            description: "fails once".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if ctx.attempt() == 0 {
            return Err(ToolError::Transient("blip".into()));
        }
        Ok(ToolOutput::text("steady"))
    }
}

struct Rig {
    parent_name: String,
    child_name: String,
    parent_mock: Arc<MockModel>,
    child_mock: Arc<MockModel>,
    child_gate: Arc<Gate>,
    /// Hold the child's n-th model call at the gate.
    hold_child_on: Option<usize>,
    flaky: bool,
    max_tool_calls: Option<u32>,
    /// The history budget of the parent, in tokens.
    history_tokens: Option<u32>,
    /// The time limit of each round of `helper`.
    within: Option<Duration>,
    /// A tool the parent can call that waits for the test.
    hold: Option<Arc<HoldTool>>,
    /// Hold the parent's n-th model call (0-based) at the gate.
    hold_parent_on: Option<(usize, Arc<Gate>)>,
    /// Build the parent as a build before session reports was: it never lists sessions to report.
    reports_off: bool,
    /// The limits the session tools hold (`None`: the defaults).
    limits: Option<SessionLimits>,
    /// How long a kept-open session may sit idle (`None`: the runtime's default).
    idle_ttl: Option<Duration>,
    sink: CollectingSink,
    clock: ManualClock,
}

impl Rig {
    fn new() -> Self {
        Self {
            parent_name: uniq("parent"),
            child_name: uniq("child"),
            parent_mock: Arc::new(MockModel::new()),
            child_mock: Arc::new(MockModel::new()),
            child_gate: Arc::default(),
            hold_child_on: None,
            flaky: false,
            max_tool_calls: None,
            history_tokens: None,
            within: None,
            hold: None,
            hold_parent_on: None,
            reports_off: false,
            limits: None,
            idle_ttl: None,
            sink: CollectingSink::new(),
            clock: ManualClock::new(),
        }
    }

    fn holding(mut self, n: usize) -> Self {
        self.hold_child_on = Some(n);
        self
    }

    /// Give the parent the `hold` tool.
    fn with_hold(mut self) -> Self {
        self.hold = Some(Arc::default());
        self
    }

    /// Hold the parent's n-th model call at the returned gate.
    fn holding_parent(mut self, n: usize) -> (Self, Arc<Gate>) {
        let gate = Arc::new(Gate::default());
        self.hold_parent_on = Some((n, gate.clone()));
        (self, gate)
    }

    /// The parent of a build that wrote journals before session reports existed.
    fn without_reports(mut self) -> Self {
        self.reports_off = true;
        self
    }

    fn roster(&self) -> SessionTools {
        let tools = SessionTools::new([
            SessionAgent {
                name: "helper".into(),
                agent: self.child_name.clone(),
                description: "Does a piece of work.".into(),
                within: self.within,
                remote: false,
            },
            SessionAgent {
                name: "far".into(),
                agent: "far".into(),
                description: "Lives on another system.".into(),
                within: None,
                remote: true,
            },
        ]);
        match self.limits {
            Some(limits) => tools.with_limits(limits),
            None => tools,
        }
    }

    fn parent_with(&self, wrap: impl Fn(DynTool) -> DynTool) -> LlmAgent {
        let model: DynModel = match &self.hold_parent_on {
            Some((n, gate)) => Arc::new(GateModel {
                inner: self.parent_mock.clone(),
                hold_on: *n,
                calls: AtomicUsize::new(0),
                armed: AtomicBool::new(true),
                gate: gate.clone(),
            }),
            None => self.parent_mock.clone(),
        };
        let mut builder: LlmAgentBuilder = LlmAgent::builder(&self.parent_name, model, "m")
            .session_agents([self.child_name.clone()]);
        if self.reports_off {
            builder = builder.without_session_reports();
        }
        for tool in self.roster().tools() {
            builder = builder.dyn_tool(wrap(tool));
        }
        if self.flaky {
            builder = builder.tool(Flaky);
        }
        if let Some(hold) = &self.hold {
            builder = builder.tool(HoldHandle(hold.clone()));
        }
        if self.max_tool_calls.is_some() || self.history_tokens.is_some() {
            let defaults = adam_llm_agent::Limits::default();
            builder = builder.limits(adam_llm_agent::Limits {
                max_tool_calls: self.max_tool_calls.unwrap_or(defaults.max_tool_calls),
                max_history_tokens: self.history_tokens.unwrap_or(defaults.max_history_tokens),
                ..defaults
            });
        }
        builder.build()
    }

    fn parent(&self) -> LlmAgent {
        self.parent_with(|t| t)
    }

    fn child(&self) -> LlmAgent {
        let model: DynModel = match self.hold_child_on {
            Some(n) => Arc::new(GateModel {
                inner: self.child_mock.clone(),
                hold_on: n,
                calls: AtomicUsize::new(0),
                armed: AtomicBool::new(true),
                gate: self.child_gate.clone(),
            }),
            None => self.child_mock.clone(),
        };
        LlmAgent::builder(&self.child_name, model, "m").build()
    }

    fn builder(&self, store: &DynStore, worker: &str) -> RuntimeBuilder {
        let builder = Runtime::builder(store.clone());
        let builder = match self.idle_ttl {
            Some(ttl) => builder.session_idle_ttl(ttl),
            None => builder,
        };
        builder
            .worker_id(format!("{worker}-{}", RunId::new()))
            .event_sink(self.sink.clone())
            .clock(self.clock.clone())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
                multiplier: 1.0,
            })
    }

    fn together(&self, store: &DynStore, parent: &LlmAgent, child: &LlmAgent) -> Runtime {
        self.builder(store, "w")
            .agent(parent.clone())
            .agent(child.clone())
            .build()
    }

    /// The runtime that steps the parent only.
    fn front(&self, store: &DynStore, parent: &LlmAgent) -> Runtime {
        self.builder(store, "front")
            .agent(parent.clone())
            .starter(adam_llm_agent::LlmStarter::new(&self.child_name))
            .build()
    }

    /// The runtime that steps the child only.
    fn back(&self, store: &DynStore, child: &LlmAgent) -> Runtime {
        self.builder(store, "back").agent(child.clone()).build()
    }

    /// Start the parent in `conversation` with `script`, a list of model turns (calls, or `None`
    /// for a text answer, which the text list gives in order), built from the session id, which
    /// the first call of the run (`c1`, a start) makes.
    async fn start(&self, rt: &Runtime, conversation: &str) -> RunId {
        rt.start(&self.parent_name, user_message("go"), Some(conversation))
            .await
            .expect("start")
    }
}

/// An agent that keeps messages for later, as a round that talks to a remote does: its state says
/// `queues_messages`, and it finishes when a message says `finish`.
struct Queues(String);

#[async_trait]
impl adam_runtime::Agent for Queues {
    type State = Value;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Value, adam_runtime::AgentError> {
        Ok(json!({
            "session": input.payload["session"],
            "queues_messages": true,
            "read_ids": [],
        }))
    }

    async fn step(
        &self,
        ctx: &mut adam_runtime::Ctx,
        mut state: Value,
    ) -> Result<adam_runtime::Transition<Value>, adam_runtime::AgentError> {
        let mut finish = false;
        for message in ctx.take_inbox() {
            if message.kind != adam_llm_agent::MESSAGE_KIND {
                continue;
            }
            finish |= message.payload["text"] == "finish";
            if let Some(ids) = state["read_ids"].as_array_mut() {
                ids.push(json!(message.id));
            }
        }
        if finish {
            return Ok(adam_runtime::Transition::Done {
                state,
                output: json!({"text": "queue done"}),
            });
        }
        Ok(adam_runtime::Transition::Park {
            state,
            wake_at: None,
        })
    }
}

fn start_call(id: &str, keep_open: bool) -> ToolCall {
    call(
        id,
        "subagent_start",
        json!({"agent": "helper", "description": "look at it", "prompt": "do the work",
               "keep_open": keep_open}),
    )
}

fn read_call(id: &str, session: RunId, wait: bool) -> ToolCall {
    call(
        id,
        "subagent_read",
        json!({"session": session.to_string(), "wait": wait}),
    )
}

fn send_call(id: &str, session: RunId, message: &str) -> ToolCall {
    call(
        id,
        "subagent_send",
        json!({"session": session.to_string(), "message": message}),
    )
}

fn close_call(id: &str, session: RunId, cancel: bool) -> ToolCall {
    call(
        id,
        "subagent_close",
        json!({"session": session.to_string(), "cancel": cancel}),
    )
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), RuntimeError>>,
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

/// The session `session` of `conversation` once `pred` holds of it.
async fn session_until(
    rt: &Runtime,
    conversation: &str,
    session: RunId,
    pred: impl Fn(&adam_runtime::SessionView) -> bool,
) -> adam_runtime::SessionView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(view) = rt.session_view(conversation, session).await.unwrap()
            && pred(&view)
        {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the session"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "done", |v| v.status == RunStatus::Done).await
}

/// Parked on the wait timer, with a session wait recorded.
async fn wait_waiting_on_a_session(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "parked on a session", |v| {
        v.status == RunStatus::Parked
            && v.wake_at.is_some()
            && matches!(conversation(v).pending_wait, Some(PendingWait::Session(_)))
    })
    .await
}

async fn reached(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

/// The tool results the history holds, in order.
fn tool_results(state: &Conversation) -> Vec<(String, String, bool)> {
    state
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                call_id,
                content,
                is_error,
                ..
            } => Some((call_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn result_of<'a>(results: &'a [(String, String, bool)], id: &str) -> &'a (String, String, bool) {
    results
        .iter()
        .find(|(call, _, _)| call == id)
        .unwrap_or_else(|| panic!("no result for {id}: {results:#?}"))
}

fn as_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

/// The tokens in the totals of a run: input and output, over every provider and model.
fn tokens(state: &Conversation) -> (u64, u64) {
    state
        .usage_totals
        .entries()
        .iter()
        .fold((0, 0), |(i, o), e| {
            (i + e.usage.input_tokens, o + e.usage.output_tokens)
        })
}

/// A text answer that took `input` and `output` tokens.
fn answer_with(text: &str, input: u64, output: u64) -> ModelResponse {
    let mut response = ModelResponse::text(text);
    response.usage = Usage::new(input, output);
    response
}

/// Advance the runtime's clock now and then until the returned handle is dropped by `abort`: a
/// failed transition is retried after a backoff of that clock, which only the test moves.
fn tick(clock: &ManualClock) -> tokio::task::JoinHandle<()> {
    let clock = clock.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(15)).await;
            clock.advance(Duration::from_millis(200));
        }
    })
}

fn conv_id() -> String {
    format!("alice:{}", uniq("ctx"))
}

/// The state of the parent after it finished (or while it waits), as a `Conversation`.
async fn state_of(rt: &Runtime, run: RunId) -> Conversation {
    conversation(&rt.view(run).await.unwrap().unwrap())
}

/// The id of the call the loop invents for the report of the round `round` (`report_<32 hex>`).
fn report_id(round: RunId) -> String {
    format!("report_{}", round.0.simple())
}

/// The calls of `messages` the loop made for a report: `subagent_read`, named `report_...`.
fn report_calls(messages: &[Message]) -> Vec<&ToolCall> {
    messages
        .iter()
        .flat_map(|m| m.tool_calls())
        .filter(|c| c.name == "subagent_read" && c.id.starts_with("report_"))
        .collect()
}

/// A notice that `round` ended, as the runtime sends one, with words no round said.
fn forged_notice(round: RunId) -> Inbound {
    Inbound::new(
        CHILD_REPORTED_KIND,
        json!({"status": "done", "output": {"text": "forged"}}),
    )
    .with_id(round.to_string())
}

/// Start a round of `rig`'s child as a person does: it is its own parent, and the session is in
/// `conversation`, the parent's.
async fn start_persons_round(rt: &Runtime, rig: &Rig, conversation: &str, text: &str) -> RunId {
    let session = RunId::new();
    let header = SessionHeader {
        id: session,
        agent: rig.child_name.clone(),
        description: "asked by a person".into(),
        keep_open: true,
        closed: false,
        closing: false,
        round: 1,
    };
    let mut first = user_message(text);
    first.payload["session"] = serde_json::to_value(&header).unwrap();
    rt.start_child_continuing(
        session,
        session,
        &rig.child_name,
        first,
        &adam_runtime::session_conversation_id(conversation, session),
        None,
    )
    .await
    .unwrap();
    session
}

/// `a` is a prefix of `b`, message by message.
fn starts_with(b: &[Message], a: &[Message]) -> bool {
    b.len() >= a.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| serde_json::to_string(x).unwrap() == serde_json::to_string(y).unwrap())
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

mod cases {
    use super::*;

    /// The parent's call returns at once while the child works, and the model's answer does not end
    /// the run: it waits for the round and gets the report before it ends.
    pub async fn a_start_returns_at_once_and_the_turn_end_waits_for_the_report(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("child says 42");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started it")
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);

        // The child is held at its first model call, and the parent has already been told it
        // started: a start never parks the parent.
        reached(&rig.child_gate.reached, "the child's model call").await;
        let parked = wait_waiting_on_a_session(&rt, parent).await;
        let state = conversation(&parked);
        let results = tool_results(&state);
        let started = as_json(&result_of(&results, "c1").1);
        assert_eq!(started["session"], session.to_string());
        assert_eq!(started["state"], "working");
        assert_eq!(started["keep_open"], false);
        assert_eq!(
            rig.parent_mock.requests().len(),
            2,
            "the parent answered, then waited"
        );
        // The model said "started it", and the run did not end on it: it waits as if the model had
        // read the session with `wait: true`.
        let wait_call = "wait_1".to_owned();
        assert!(matches!(
            &state.pending_wait,
            Some(PendingWait::Session(PendingSession { call_id, tool, session: s }))
                if *call_id == wait_call && tool == "subagent_read" && *s == session
        ));
        assert_ne!(parked.status, RunStatus::Done);

        rig.child_gate.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "the helper said 42");
        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        let report = &result_of(&results, &wait_call).1;
        assert!(report.contains("report: done"), "{report}");
        assert!(report.contains("child says 42"), "{report}");
        assert!(report.ends_with("[end: 1 lines]"), "{report}");
        assert_eq!(state.pending_wait, None);
        // The synthetic call is in the model's message exactly once, and its id is one every
        // provider accepts.
        let waits: Vec<&ToolCall> = state
            .messages
            .iter()
            .flat_map(|m| m.tool_calls())
            .filter(|c| c.name == "subagent_read")
            .collect();
        assert_eq!(waits.len(), 1, "{waits:?}");
        assert!(
            waits[0]
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
        // Three model calls: the start, the answer that waited, the answer that read the report.
        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(matches!(
            requests[2].messages.last(),
            Some(Message::Tool { call_id, .. }) if *call_id == wait_call
        ));
        assert_eq!(rig.child_mock.requests().len(), 1, "one child ran");
    }

    /// The requests of a parent that starts a session, waits for it and reads its report each start
    /// with the one before: the synthetic `wait_<turn>` call and the report are appended like any
    /// other call and result (ADR 0044, slice 4).
    pub async fn a_session_report_keeps_the_prefix(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("child says 42");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started it")
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        assert!(state.shortenings.is_empty());
        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3);
        for pair in requests.windows(2) {
            assert_eq!(
                serde_json::to_string(&(&pair[0].system, &pair[0].tools)).unwrap(),
                serde_json::to_string(&(&pair[1].system, &pair[1].tools)).unwrap(),
                "system and tools are the same bytes"
            );
            let (a, b) = (
                serde_json::to_string(&pair[0].messages).unwrap(),
                serde_json::to_string(&pair[1].messages).unwrap(),
            );
            assert!(
                b.starts_with(a.trim_end_matches(']')),
                "a request starts with the one before it:\n{a}\n{b}"
            );
        }
        assert!(matches!(
            requests[2].messages.last(),
            Some(Message::Tool { call_id, .. }) if call_id == "wait_1"
        ));
    }

    /// A history far over budget leaves the result of `subagent_start` alone: it is a small
    /// structured result that holds the session's id, which nothing else repeats.
    pub async fn the_start_result_survives_a_history_far_over_budget(store: DynStore) {
        let mut rig = Rig::new().holding(0);
        rig.history_tokens = Some(1);
        rig.child_mock.push_text("child says 42");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started it")
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        let started = as_json(&result_of(&results, "c1").1);
        assert_eq!(started["session"], session.to_string());
        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[2].messages[2], state.messages[2],
            "the third request still shows the start result as it was"
        );
    }

    /// A read that does not wait answers "working" and parks nothing; the model carries on.
    pub async fn a_read_without_wait_answers_working_and_parks_nothing(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("late answer");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", session, false)])
            .push_text("it is still working")
            .push_text("done: late answer");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        let parked = wait_waiting_on_a_session(&rt, parent).await;
        let results = tool_results(&conversation(&parked));
        let working = &result_of(&results, "c2").1;
        assert!(working.contains("round 1, working"), "{working}");
        assert!(working.contains("report: none yet"), "{working}");
        assert!(
            working.contains("wait: true"),
            "it says what to do instead of polling"
        );
        assert!(working.ends_with("[end: 0 lines]"));
        assert!(!result_of(&results, "c2").2, "working is not an error");
        // Three model calls so far: the read did not park the run.
        assert_eq!(rig.parent_mock.requests().len(), 3);

        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
    }

    /// A read with `wait` parks on the session, and the parent resumes once, with the report.
    pub async fn a_read_with_wait_parks_and_resumes_once_with_the_report(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("line one\nline two");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_text("all read");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        let parked = wait_waiting_on_a_session(&rt, parent).await;
        assert!(matches!(
            conversation(&parked).pending_wait,
            Some(PendingWait::Session(PendingSession { ref call_id, .. })) if call_id == "c2"
        ));
        assert_eq!(
            rig.parent_mock.requests().len(),
            2,
            "no model call while it waits"
        );

        rig.child_gate.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "all read");
        let results = tool_results(&state_of(&rt, parent).await);
        let report = &result_of(&results, "c2").1;
        assert!(report.contains("1\tline one"), "{report}");
        assert!(report.contains("2\tline two"), "{report}");
        assert!(report.ends_with("[end: 2 lines]"), "{report}");
        assert_eq!(rig.parent_mock.requests().len(), 3, "resumed once");
    }

    /// By default a session closes when its round has ended: its report can still be read, and a
    /// send is refused in words the model can act on.
    pub async fn a_session_closes_by_default_and_refuses_a_send(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("first and only");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![send_call("c3", session, "more please")])
            .push_text("it refused");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        assert!(result_of(&results, "c2").1.contains("first and only"));
        assert!(result_of(&results, "c2").1.contains("closed"));
        let (_, refusal, is_error) = result_of(&results, "c3");
        assert!(*is_error);
        assert!(refusal.contains("is closed"), "{refusal}");
        assert!(
            refusal.contains("subagent_start"),
            "it says what to do: {refusal}"
        );
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Closed);
        assert_eq!(
            list.sessions[0].round, 1,
            "nothing was started by the refused send"
        );
    }

    /// A kept-open session takes a second round that continues the first: the child's second
    /// request holds the first round's exchange.
    pub async fn a_kept_open_session_continues_in_round_two(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock
            .push_text("answer one")
            .push_text("answer two");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![send_call("c3", session, "and now this")])
            .push_tool_calls(vec![read_call("c4", session, true)])
            .push_text("two rounds");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        assert!(result_of(&results, "c2").1.contains("answer one"));
        assert!(result_of(&results, "c2").1.contains("idle"));
        let sent = as_json(&result_of(&results, "c3").1);
        assert_eq!(sent["delivered"], "new_round");
        assert_eq!(sent["round"], 2);
        assert!(result_of(&results, "c4").1.contains("answer two"));
        assert!(result_of(&results, "c4").1.contains("round 2"));

        let requests = rig.child_mock.requests();
        assert_eq!(requests.len(), 2);
        let second = &requests[1].messages;
        assert!(
            second.contains(&Message::user_text("do the work")),
            "round 1's prompt: {second:#?}"
        );
        assert!(
            second.iter().any(|m| m.text() == "answer one"),
            "round 1's answer"
        );
        assert_eq!(second.last(), Some(&Message::user_text("and now this")));
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Idle);
        assert_eq!(list.sessions[0].round, 2);
        assert!(list.sessions[0].keep_open);
        assert_eq!(list.sessions[0].description, "look at it");
    }

    /// A message to a working round is steered into it.
    pub async fn a_message_to_a_working_round_is_steered(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock
            .push_text("first try")
            .push_text("changed course");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![send_call("c2", session, "do it differently")])
            .push_tool_calls(vec![read_call("c3", session, true)])
            .push_text("steered");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        // The send is answered while the round is held.
        wait_for(&rt, parent, "the steer to be answered", |v| {
            tool_results(&conversation(v))
                .iter()
                .any(|(id, _, _)| id == "c2")
        })
        .await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let sent = as_json(&result_of(&results, "c2").1);
        assert_eq!(sent["delivered"], "steered");
        assert_eq!(sent["round"], 1);
        assert!(result_of(&results, "c3").1.contains("changed course"));
        // The round read the steer: its second model call has it as the newest message.
        let requests = rig.child_mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].messages.last(),
            Some(&Message::user_text("do it differently"))
        );
    }

    /// Closing an idle session takes one round that calls no model; the session is then closed.
    pub async fn closing_an_idle_session_takes_a_round_that_calls_no_model(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("kept answer");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![close_call("c3", session, false)])
            .push_tool_calls(vec![close_call("c4", session, false)])
            .push_tool_calls(vec![send_call("c5", session, "too late")])
            .push_tool_calls(vec![read_call("c6", session, true)])
            .push_text("closed");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        assert_eq!(as_json(&result_of(&results, "c3").1)["state"], "closed");
        assert_eq!(
            as_json(&result_of(&results, "c4").1)["state"],
            "closed",
            "closing twice does nothing"
        );
        assert!(result_of(&results, "c5").2 && result_of(&results, "c5").1.contains("is closed"));
        // The report of the last real round is still what a read gives.
        assert!(result_of(&results, "c6").1.contains("kept answer"));
        assert!(result_of(&results, "c6").1.contains("closed"));
        assert_eq!(
            rig.child_mock.requests().len(),
            1,
            "the close called no model"
        );
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Closed);
        assert_eq!(list.sessions[0].round, 2, "a close is a round");
    }

    /// Closing a working session without `cancel` leaves its round to finish; the session takes no
    /// more messages, and the parent's turn waits for the round and reads its report.
    pub async fn closing_a_working_session_lets_the_round_finish(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("finished anyway");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![close_call("c2", session, false)])
            .push_tool_calls(vec![send_call("c3", session, "one more")])
            .push_text("closed while it worked")
            .push_text("and it finished");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        let closed = as_json(&result_of(&results, "c2").1);
        assert_eq!(closed["state"], "closed");
        assert!(closed["note"].as_str().unwrap().contains("left to finish"));
        assert!(
            result_of(&results, "c3").2,
            "a send after the close is refused"
        );
        assert!(result_of(&results, "c3").1.contains("is closed"));
        let wait_call = "wait_3".to_owned();
        assert!(
            result_of(&results, &wait_call)
                .1
                .contains("finished anyway"),
            "{results:#?}"
        );
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Closed);
        assert_eq!(
            list.sessions[0].round, 1,
            "the round that was working is the last"
        );
    }

    /// Closing with `cancel` stops the working round; the session is closed and the parent has
    /// nothing to wait for.
    pub async fn closing_with_cancel_stops_the_round(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("never said");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![close_call("c2", session, true)])
            .push_tool_calls(vec![read_call("c3", session, true)])
            .push_text("stopped it");
        let worker = spawn_worker(&rt);
        // The parent may cancel the round before the child's first model call: nothing waits for
        // the gate, and a release that finds nobody is kept as a permit.
        let done = wait_done(&rt, parent).await;
        assert_eq!(done.output.expect("output")["text"], "stopped it");
        rig.child_gate.release.notify_one();
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        assert_eq!(as_json(&result_of(&results, "c2").1)["state"], "closed");
        let read = &result_of(&results, "c3").1;
        assert!(
            read.contains("round 2, closed"),
            "the close was a round: {read}"
        );
        let round = rt.view(session).await.unwrap().unwrap();
        assert_eq!(round.status, RunStatus::Failed);
        assert_eq!(
            round.error.as_deref(),
            Some("cancelled: the session was closed with cancel")
        );
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Closed);
    }

    /// The listing puts open sessions first (working, then idle, then closed) and pages with the
    /// convention's last line.
    pub async fn the_list_orders_open_sessions_first_and_pages(store: DynStore) {
        // The third child call is held: session three is the one that is working.
        let rig = Rig::new().holding(2);
        rig.child_mock
            .push_text("one")
            .push_text("two")
            .push_text("three");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let s1 = child_run_id(parent, "c1");
        let s2 = child_run_id(parent, "c3");
        let s3 = child_run_id(parent, "c5");
        let start = |id: &str, keep: bool, what: &str| {
            call(
                id,
                "subagent_start",
                json!({"agent": "helper", "description": what, "prompt": "p", "keep_open": keep}),
            )
        };
        rig.parent_mock
            .push_tool_calls(vec![start("c1", false, "closes by itself")])
            .push_tool_calls(vec![read_call("c2", s1, true)])
            .push_tool_calls(vec![start("c3", true, "kept idle")])
            .push_tool_calls(vec![read_call("c4", s2, true)])
            .push_tool_calls(vec![start("c5", true, "still working")])
            .push_tool_calls(vec![call("c6", "subagent_list", json!({}))])
            .push_tool_calls(vec![call("c7", "subagent_list", json!({"limit": 2}))])
            .push_tool_calls(vec![call("c8", "subagent_list", json!({"offset": 3}))])
            .push_tool_calls(vec![call("c9", "subagent_list", json!({"offset": -1}))])
            .push_tool_calls(vec![call("c10", "subagent_list", json!({"offset": 9}))])
            .push_text("listed")
            .push_text("the third finished");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the third child").await;
        wait_for(&rt, parent, "the listings", |v| {
            tool_results(&conversation(v))
                .iter()
                .any(|(id, _, _)| id == "c10")
        })
        .await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let all = &result_of(&results, "c6").1;
        let lines: Vec<&str> = all.lines().collect();
        assert_eq!(lines.len(), 4, "{all}");
        assert!(
            lines[0].starts_with(&s3.to_string()) && lines[0].contains("working"),
            "{all}"
        );
        assert!(
            lines[1].starts_with(&s2.to_string()) && lines[1].contains("idle"),
            "{all}"
        );
        assert!(
            lines[2].starts_with(&s1.to_string()) && lines[2].contains("closed"),
            "{all}"
        );
        assert!(lines[0].contains("helper working round 1 keep_open=true \"still working\""));
        assert_eq!(lines[3], "[end: 3 sessions]");
        let first = &result_of(&results, "c7").1;
        assert_eq!(first.lines().count(), 3);
        assert!(
            first.ends_with("[more: 1 sessions left; next page: offset=3]"),
            "{first}"
        );
        let last = &result_of(&results, "c8").1;
        assert!(
            last.starts_with(&s1.to_string()) && last.ends_with("[end: 3 sessions]"),
            "{last}"
        );
        let from_end = &result_of(&results, "c9").1;
        assert!(from_end.starts_with(&s1.to_string()), "{from_end}");
        let past = result_of(&results, "c10");
        assert!(past.2 && past.1.contains("past the end"), "{past:?}");
    }

    /// An unknown id, a malformed id and another conversation's session are refused in words, and
    /// a missing or mistyped argument is too. Nothing is started by any of them.
    pub async fn a_session_that_is_not_ours_is_refused_in_words(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("alice's work");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let alices = rig.start(&rt, &conv_id()).await;
        let alices_session = child_run_id(alices, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", alices_session, true)])
            .push_text("alice done");
        let worker = spawn_worker(&rt);
        wait_done(&rt, alices).await;
        // Another conversation reaches for alice's session, a made-up one, and a bad one.
        let nobody = RunId::new();
        rig.parent_mock
            .push_tool_calls(vec![
                read_call("b1", alices_session, false),
                send_call("b2", alices_session, "hello"),
                close_call("b3", alices_session, true),
                read_call("b4", nobody, true),
                call("b5", "subagent_read", json!({"session": "not-an-id"})),
                call("b6", "subagent_read", json!({})),
                call(
                    "b7",
                    "subagent_send",
                    json!({"session": nobody.to_string()}),
                ),
                call(
                    "b8",
                    "subagent_read",
                    json!({"session": nobody.to_string(), "wait": "yes"}),
                ),
                call(
                    "b9",
                    "subagent_start",
                    json!({"agent": "nope", "description": "d", "prompt": "p"}),
                ),
                call(
                    "b10",
                    "subagent_start",
                    json!({"agent": "helper", "description": "x".repeat(81), "prompt": "p"}),
                ),
                call(
                    "b11",
                    "subagent_start",
                    json!({"agent": "helper", "description": "d", "prompt": " "}),
                ),
            ])
            .push_text("all refused");
        let bobs = rig.start(&rt, &conv_id()).await;
        wait_done(&rt, bobs).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, bobs).await);
        for id in ["b1", "b2", "b3", "b4"] {
            let (_, text, is_error) = result_of(&results, id);
            assert!(*is_error && text.contains("no session"), "{id}: {text}");
        }
        for (id, want) in [
            ("b5", "not a session id"),
            ("b6", "needs `session`"),
            ("b7", "needs `message`"),
            ("b8", "`wait` must be true or false"),
            ("b9", "no subagent `nope`"),
            ("b10", "at most 80"),
            ("b11", "needs `prompt`"),
        ] {
            let (_, text, is_error) = result_of(&results, id);
            assert!(*is_error && text.contains(want), "{id}: {text}");
        }
        assert_eq!(rig.child_mock.requests().len(), 1, "only alice's round ran");
    }

    /// A subagent that runs on another system starts a round like any other: the round is a run of
    /// the agent registered under its name, which is the one that talks to the remote.
    pub async fn a_remote_subagent_starts_a_round_of_its_registered_agent(store: DynStore) {
        let rig = Rig::new();
        let far_mock = Arc::new(MockModel::new());
        far_mock.push_text("the far answer");
        let far_model: DynModel = far_mock.clone();
        let far = LlmAgent::builder("far", far_model, "m").build();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig
            .builder(&store, "w")
            .agent(parent_agent.clone())
            .agent(child_agent)
            .agent(far)
            .build();
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![call(
                "c1",
                "subagent_start",
                json!({"agent": "far", "description": "d", "prompt": "p"}),
            )])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_text("read");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let started = as_json(&result_of(&results, "c1").1);
        assert_eq!(started["agent"], "far");
        assert!(!result_of(&results, "c1").2, "{results:#?}");
        assert!(result_of(&results, "c2").1.contains("the far answer"));
        let listed = rt.list_sessions(&conv, &["far"], 0, 10).await.unwrap();
        assert_eq!(listed.sessions.len(), 1);
        assert_eq!(listed.sessions[0].agent, "far");
        assert_eq!(far_mock.requests().len(), 1);
    }

    /// A round whose agent keeps messages until its remote can take them says so: the send answers
    /// `queued`, not `steered`.
    pub async fn a_send_to_a_round_that_queues_says_queued(store: DynStore) {
        let rig = Rig::new();
        let (parent_agent, queues) = (rig.parent(), Queues(rig.child_name.clone()));
        let rt = rig
            .builder(&store, "w")
            .agent(parent_agent)
            .agent(queues)
            .build();
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![send_call("c2", session, "hold on")])
            .push_tool_calls(vec![send_call("c3", session, "finish")])
            .push_tool_calls(vec![read_call("c4", session, true)])
            .push_text("queued twice");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        for id in ["c2", "c3"] {
            assert_eq!(as_json(&result_of(&results, id).1)["delivered"], "queued");
        }
        assert!(result_of(&results, "c4").1.contains("queue done"));
    }

    /// A send the round already read (the call is made again) reports how the round took it the
    /// first time: `queued` for a round that keeps messages, not `steered`.
    pub async fn a_replayed_send_to_a_round_that_queues_says_queued(store: DynStore) {
        let rig = Rig::new();
        let (parent_agent, queues) = (rig.parent(), Queues(rig.child_name.clone()));
        let rt = rig
            .builder(&store, "w")
            .agent(parent_agent)
            .agent(queues)
            .build();
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![send_call("c2", session, "hold on")])
            .push_tool_calls(vec![send_call("c2", session, "hold on")])
            .push_tool_calls(vec![send_call("c3", session, "finish")])
            .push_tool_calls(vec![read_call("c4", session, true)])
            .push_text("sent twice");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let deliveries: Vec<String> = tool_results(&state)
            .iter()
            .filter(|(id, ..)| id == "c2")
            .map(|(_, text, _)| {
                as_json(text)["delivered"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert!(!deliveries.is_empty(), "{state:#?}");
        assert!(deliveries.iter().all(|d| d == "queued"), "{deliveries:?}");
    }

    /// A later call of the same turn fails the try, and the model is asked again with other call
    /// ids: the session the first try started is the one the history names, and nothing else starts.
    pub async fn a_failed_try_later_in_the_turn_starts_no_second_session(store: DynStore) {
        let mut rig = Rig::new();
        rig.flaky = true;
        rig.child_mock.push_text("one").push_text("two");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        rig.parent_mock
            .push_tool_calls(vec![
                start_call("c1", false),
                call("f1", "flaky", json!({})),
            ])
            .push_text("done")
            // The round may still work at the end of the turn, and is then waited for.
            .push_text("done, and it reported");
        let parent = rig.start(&rt, &conv).await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        let state = state_of(&rt, parent).await;
        assert_eq!(list.sessions.len(), 1, "{:#?}", state.messages);
        let named: Vec<&str> = state
            .messages
            .iter()
            .flat_map(|m| m.tool_calls().iter().map(|c| c.id.as_str()))
            .collect();
        assert!(
            named.contains(&"c1"),
            "the history names the call: {named:?}"
        );
        let owner = child_run_id(parent, "c1");
        assert_eq!(
            list.sessions[0].id, owner,
            "the session is owned by a call in the history"
        );
    }

    /// A try that fails after its effects (a tool that errs after it acted, a commit that is lost
    /// before or after it applies) runs again, and starts nothing twice: one session, the rounds
    /// that were asked for, and a child that ran as often as it was asked.
    pub async fn a_retried_try_starts_nothing_twice(store: DynStore) {
        for fault in
            std::iter::once(None).chain((1..=6).flat_map(|n| [(n, false), (n, true)]).map(Some))
        {
            let rig = Rig::new();
            rig.child_mock
                .push_text("answer one")
                .push_text("answer two");
            let (inner, dynamic) = faulty(store.clone());
            let parent_agent = rig.parent_with(|t| {
                if matches!(
                    t.spec().name.as_str(),
                    "subagent_start" | "subagent_send" | "subagent_close"
                ) {
                    Arc::new(Blip { inner: t })
                } else {
                    t
                }
            });
            let child_agent = rig.child();
            let rt = rig.together(&dynamic, &parent_agent, &child_agent);
            let conv = conv_id();
            let parent = rig.start(&rt, &conv).await;
            let session = child_run_id(parent, "c1");
            rig.parent_mock
                .push_tool_calls(vec![start_call("c1", true)])
                .push_tool_calls(vec![read_call("c2", session, true)])
                .push_tool_calls(vec![send_call("c3", session, "again")])
                .push_tool_calls(vec![read_call("c4", session, true)])
                .push_tool_calls(vec![close_call("c5", session, false)])
                .push_text("finished");
            if let Some((n, after)) = fault {
                if after {
                    inner.fail_run_after_apply(Method::CommitRun, parent, n);
                } else {
                    inner.fail_run(Method::CommitRun, parent, n);
                }
            }
            let worker = spawn_worker(&rt);
            // A failed commit is retried after a backoff of the runtime's clock, which only this
            // moves; it also fires the wait timers, which look and find what is there.
            let clock = rig.clock.clone();
            let ticker = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                    clock.advance(Duration::from_millis(200));
                }
            });
            wait_done(&rt, parent).await;
            ticker.abort();
            worker.stop().await;
            let list = rt
                .list_sessions(&conv, &[&rig.child_name], 0, 10)
                .await
                .unwrap();
            assert_eq!(list.sessions.len(), 1, "{fault:?}: one session");
            assert_eq!(
                list.sessions[0].round, 3,
                "{fault:?}: two rounds and a close"
            );
            assert_eq!(
                rig.child_mock.requests().len(),
                2,
                "{fault:?}: no round ran twice"
            );
            // The retried send answers what the first try did: round 2, made by this call.
            let results = tool_results(&state_of(&rt, parent).await);
            let sent = as_json(&result_of(&results, "c3").1);
            assert_eq!(sent["round"], 2, "{fault:?}: {sent}");
            assert_eq!(sent["delivered"], "new_round", "{fault:?}: {sent}");
        }
    }

    /// Every step replays from its journal: a copy of the journal of a finished run, given to a
    /// new run that starts the same way and whose models say nothing, ends with the history the
    /// original ended with, with no child started and no model asked. That covers the start, the
    /// read that waits, the send, the check at the end of the turn and the wait it makes.
    pub async fn every_step_replays_from_the_journal_to_the_same_history(store: DynStore) {
        let rig = Rig::new().holding(1);
        rig.child_mock
            .push_text("answer one")
            .push_text("answer two");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![send_call("c3", session, "again")])
            .push_text("waiting for round two")
            .push_text("finished");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "round two").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let original = state_of(&rt, parent).await;
        assert!(
            original
                .messages
                .iter()
                .any(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "wait_3")),
            "the run waited at the end of its turn"
        );

        // The same run again, on its journal, with models that have nothing to say.
        let again = Rig::new();
        let (parent_agent, child_agent) = (again.parent(), again.child());
        let again_rt = again.together(&store, &parent_agent, &child_agent);
        let copy_conv = conv_id();
        let copy = again.start(&again_rt, &copy_conv).await;
        let mut seq = 0;
        while let Some(entry) = store.journal_get(parent, seq).await.unwrap() {
            store.journal_put(copy, entry).await.unwrap();
            seq += 1;
        }
        assert!(seq > 8, "the journal has the steps of the run: {seq}");
        let worker = spawn_worker(&again_rt);
        let clock = again.clock.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                clock.advance(Duration::from_secs(61));
            }
        });
        wait_done(&again_rt, copy).await;
        ticker.abort();
        worker.stop().await;
        let replayed = state_of(&again_rt, copy).await;
        let text = |c: &Conversation, run: RunId| {
            serde_json::to_string(&c.messages)
                .unwrap()
                .replace(&run.to_string(), "<run>")
        };
        assert_eq!(text(&replayed, copy), text(&original, parent));
        assert_eq!(again.parent_mock.requests().len(), 0, "no model was asked");
        assert_eq!(again.child_mock.requests().len(), 0, "no child ran");
        let list = again_rt
            .list_sessions(&copy_conv, &[&again.child_name], 0, 10)
            .await
            .unwrap();
        assert!(list.sessions.is_empty());
    }

    /// Closing with cancel must not hide the failure: the read says the round that worked failed.
    pub async fn a_closed_session_reports_the_round_that_worked(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("never said");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![close_call("c2", session, true)])
            .push_tool_calls(vec![read_call("c3", session, true)])
            .push_text("stopped it");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        rig.child_gate.release.notify_one();
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let read = &result_of(&results, "c3").1;
        assert!(read.contains("report: failed"), "{read}");
        assert!(
            read.contains("cancelled: the session was closed with cancel"),
            "{read}"
        );
        assert!(read.contains("closed"), "{read}");
    }

    /// A run that ends where it cannot wait (a tool-call limit) cancels the rounds it started.
    pub async fn a_run_that_cannot_wait_cancels_its_rounds(store: DynStore) {
        let mut rig = Rig::new().holding(0);
        rig.max_tool_calls = Some(1);
        rig.child_mock.push_text("never said");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_text("done");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, parent).await;
        let text = done.output.unwrap()["text"].as_str().unwrap().to_owned();
        assert!(text.contains("were cancelled"), "{text}");
        let round = rt.view(child_run_id(parent, "c1")).await.unwrap().unwrap();
        assert_eq!(round.status, RunStatus::Failed);
        assert_eq!(
            round.error.as_deref(),
            Some("cancelled: the run that started it ended")
        );
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Idle, "kept open");
        rig.child_gate.release.notify_one();
        worker.stop().await;
    }

    /// A close request survives the cancel of the parent: the session never reopens.
    pub async fn a_close_request_survives_a_cancel(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("never said");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![close_call("c2", session, false)])
            .push_text("closing");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rt.cancel(parent, "stop").await.unwrap();
        wait_for(&rt, session, "the round to be cancelled", |v| {
            v.status == RunStatus::Failed
        })
        .await;
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Closed);
        assert!(list.sessions[0].closed);
        // A later task of the conversation is refused when it sends to it.
        rig.parent_mock
            .push_tool_calls(vec![send_call("d1", session, "reopen it")])
            .push_text("refused");
        let later = rig.start(&rt, &conv).await;
        wait_done(&rt, later).await;
        let results = tool_results(&state_of(&rt, later).await);
        let (_, refusal, is_error) = result_of(&results, "d1");
        assert!(*is_error && refusal.contains("is closed"), "{refusal}");
        rig.child_gate.release.notify_one();
        worker.stop().await;
    }

    /// The caps: 16 open sessions, then refused in words; accepted again after a close; a retried
    /// start of the 16th still answers "started"; and 19 sends make 20 rounds, the 20th send is
    /// refused while a close is still accepted.
    pub async fn the_caps_refuse_in_words(store: DynStore) {
        let mut rig = Rig::new();
        rig.flaky = false;
        for n in 0..18 {
            rig.child_mock.push_text(format!("answer {n}"));
        }
        let (parent_agent, child_agent) = (rig.parent_with(|t| t), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let sid = |n: usize| child_run_id(parent, &format!("s{n}"));
        let mut script = Vec::new();
        for n in 0..16 {
            script.push(vec![start_call(&format!("s{n}"), true)]);
            script.push(vec![read_call(&format!("r{n}"), sid(n), true)]);
        }
        script.push(vec![start_call("s16", true)]);
        script.push(vec![close_call("x0", sid(0), false)]);
        script.push(vec![start_call("s17", true)]);
        for calls in script {
            rig.parent_mock.push_tool_calls(calls);
        }
        // The last round may still work at the end of the turn, and is then waited for.
        rig.parent_mock
            .push_text("capped")
            .push_text("capped, and it reported");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let refused = result_of(&results, "s16");
        assert!(
            refused.2 && refused.1.contains("16 open sessions"),
            "{refused:?}"
        );
        assert_eq!(as_json(&result_of(&results, "x0").1)["state"], "closed");
        assert!(!result_of(&results, "s17").2, "after a close there is room");
    }

    pub async fn the_round_cap_refuses_a_send_and_not_a_close(store: DynStore) {
        let rig = Rig::new();
        for n in 0..20 {
            rig.child_mock.push_text(format!("answer {n}"));
        }
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c0");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c0", true)]);
        rig.parent_mock
            .push_tool_calls(vec![read_call("r0", session, true)]);
        for n in 1..20 {
            rig.parent_mock
                .push_tool_calls(vec![send_call(&format!("n{n}"), session, "again")]);
            rig.parent_mock
                .push_tool_calls(vec![read_call(&format!("r{n}"), session, true)]);
        }
        rig.parent_mock
            .push_tool_calls(vec![send_call("n20", session, "one too many")])
            .push_tool_calls(vec![close_call("x", session, false)])
            .push_text("capped");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let refused = result_of(&results, "n20");
        assert!(refused.2 && refused.1.contains("20 rounds"), "{refused:?}");
        assert_eq!(as_json(&result_of(&results, "x").1)["state"], "closed");
    }

    /// The access tier the parent runs under is the round's, the first and the next.
    pub async fn every_round_inherits_the_protected_context(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("one").push_text("two");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let mut input = user_message("go");
        input.payload["context"] =
            json!({ adam_llm_agent::ACCESS_TIER_CONTEXT_KEY: {"tier": "plan"} });
        let parent = rt
            .start(&rig.parent_name, input, Some(&conv))
            .await
            .unwrap();
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![send_call("c3", session, "again")])
            .push_tool_calls(vec![read_call("c4", session, true)])
            .push_text("ok");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        for run in [session, list.sessions[0].latest_run] {
            let c = conversation(&rt.view(run).await.unwrap().unwrap());
            assert_eq!(
                c.context.get(adam_llm_agent::ACCESS_TIER_CONTEXT_KEY),
                Some(&json!({"tier": "plan"})),
                "{run}"
            );
            assert_eq!(
                c.root_run,
                Some(parent),
                "a round keeps the attribution of the run that sent it"
            );
            assert!(c.root_step.is_some());
        }
    }

    /// A report is paged by lines; a failed round reads as failed with its reason; a not-kept
    /// session whose round works is steered, not refused.
    pub async fn reads_page_and_failures_read_as_failed(store: DynStore) {
        let rig = Rig::new().holding(1);
        rig.child_mock.push_text("l1\nl2\nl3\nl4\nl5");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let s1 = child_run_id(parent, "c1");
        let s2 = child_run_id(parent, "c3");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("w1", s1, true)])
            .push_tool_calls(vec![call(
                "p1",
                "subagent_read",
                json!({"session": s1.to_string(), "wait": true, "offset": 2, "limit": 2}),
            )])
            .push_tool_calls(vec![start_call("c3", false)])
            .push_tool_calls(vec![send_call("c4", s2, "steer me")])
            .push_text("done")
            .push_text("really done");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the second child").await;
        // The run parks on the round at the end of its turn while the round is held: only then is
        // it let go, so the wait is always there.
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let page = &result_of(&results, "p1").1;
        assert!(page.contains("     2\tl2\n     3\tl3\n"), "{page}");
        assert!(!page.contains("l1") && !page.contains("l4"), "{page}");
        assert!(
            page.ends_with("[more: 2 lines left; next page: offset=4]"),
            "{page}"
        );
        assert_eq!(
            as_json(&result_of(&results, "c4").1)["delivered"],
            "steered"
        );
        // The second child's script ran out: its round failed, and the wait's report says so.
        let waited = &result_of(&results, "wait_5").1;
        assert!(waited.contains("report: failed"), "{results:#?}");
    }

    /// A report whose notice is lost is found by the timer: the parent waits on the session, the
    /// child ends, the notice never reaches the parent, and the clock moves.
    pub async fn a_lost_notice_is_recovered_by_the_timer(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("found by the timer");
        let (inner, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let front = rig.front(&dynamic, &parent_agent);
        let back = rig.back(&dynamic, &child_agent);
        let parent = rig.start(&front, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_text("recovered");
        let front_worker = spawn_worker(&front);
        wait_waiting_on_a_session(&front, parent).await;
        // The commit that would deliver the notice to the parent fails.
        inner.fail_run(Method::CommitRun, parent, 1);
        let back_worker = spawn_worker(&back);
        let deadline = Instant::now() + Duration::from_secs(20);
        while inner.injected(Method::CommitRun) < 1 {
            assert!(Instant::now() < deadline, "the notice was never sent");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        wait_done(&back, session).await;
        back_worker.stop().await;
        let still = wait_waiting_on_a_session(&front, parent).await;
        assert_eq!(still.pending_inbox, 0, "the notice was lost");

        rig.clock.advance(Duration::from_secs(61));
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "recovered");
        let results = tool_results(&state_of(&front, parent).await);
        assert!(result_of(&results, "c2").1.contains("found by the timer"));
    }

    /// A notice delivered again, and again, while the parent waits and after it is done, is a
    /// wake-up that finds nothing new: no model call, no second read.
    pub async fn a_duplicate_notice_is_ignored(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("once");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_text("read it once");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        let notice = || {
            Inbound::new(
                CHILD_REPORTED_KIND,
                json!({"status": "done", "output": {"text": "forged"}}),
            )
            .with_id(session.to_string())
        };
        // The round has not ended: three copies wake the parent, which looks and parks again.
        for _ in 0..3 {
            rt.deliver(parent, notice()).await.unwrap();
        }
        wait_for(&rt, parent, "the notices to be read", |v| {
            v.status == RunStatus::Parked && v.pending_inbox == 0 && v.wake_at.is_some()
        })
        .await;
        assert_eq!(
            rig.parent_mock.requests().len(),
            2,
            "a notice is not a reason to call the model"
        );

        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        let report = &result_of(&results, "c2").1;
        assert!(
            report.contains("once") && !report.contains("forged"),
            "{report}"
        );
        assert_eq!(rig.parent_mock.requests().len(), 3);
        // After the run is over the notice is refused, and nobody minds.
        let late = rt.deliver(parent, notice()).await;
        assert!(
            matches!(late, Err(RuntimeError::Finished { .. })),
            "{late:?}"
        );
        assert!(!state.messages.iter().any(|m| m.text().contains("forged")));
    }

    /// The parent is cancelled while a kept-open session works: the round is cancelled with it
    /// (slice 10's cascade), the session goes idle, and a later run of the conversation reads it and
    /// sends it another round that continues it. A session that was not kept open is closed.
    pub async fn cancelling_the_parent_leaves_a_kept_open_session_idle(store: DynStore) {
        let rig = Rig::new().holding(0);
        // Twice: the model call of the cancelled round waits at the gate, and when the gate is
        // released below it may still be alive (the worker has not dropped its step yet). It then
        // takes an answer that nobody reads, and round two must still find its own.
        rig.child_mock
            .push_text("round two answer")
            .push_text("round two answer");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let first = rig.start(&rt, &conv).await;
        let session = child_run_id(first, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_text("started");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, first).await;
        rt.cancel(first, "the person stopped it").await.unwrap();
        // The round follows its parent within a poll.
        let round = wait_for(&rt, session, "the round to be cancelled", |v| {
            v.status == RunStatus::Failed
        })
        .await;
        assert_eq!(
            round.error.as_deref(),
            Some("cancelled: the parent run ended")
        );
        rig.child_gate.release.notify_one();
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(
            list.sessions[0].state,
            SessionState::Idle,
            "kept open: usable"
        );

        // A later task of the conversation finds the session by the conversation, not by its task.
        rig.parent_mock
            .push_tool_calls(vec![send_call("d1", session, "carry on")])
            .push_tool_calls(vec![read_call("d2", session, true)])
            .push_text("carried on");
        let second = rig.start(&rt, &conv).await;
        wait_done(&rt, second).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, second).await);
        assert_eq!(as_json(&result_of(&results, "d1").1)["round"], 2);
        assert!(
            result_of(&results, "d2").1.contains("round two answer"),
            "{results:#?}"
        );
        assert!(result_of(&results, "d2").1.contains("round 2, idle"));
        let seen = format!("{:?}", rig.child_mock.requests().last().unwrap().messages);
        assert!(
            seen.contains("do the work") && seen.contains("carry on"),
            "{seen}"
        );
    }

    /// The repeated-call rule: three reads without `wait` that all say "working" are refused on
    /// the third, and reads that wait are never counted when their results differ.
    pub async fn polling_is_refused_and_waiting_is_not(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock
            .push_text("r1")
            .push_text("r2")
            .push_text("r3");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("p1", session, false)])
            .push_tool_calls(vec![read_call("p2", session, false)])
            .push_tool_calls(vec![read_call("p3", session, false)])
            .push_text("polled")
            // Then three waiting reads with the same arguments, whose reports differ.
            .push_tool_calls(vec![read_call("w1", session, true)])
            .push_tool_calls(vec![send_call("s2", session, "next")])
            .push_tool_calls(vec![read_call("w2", session, true)])
            .push_tool_calls(vec![send_call("s3", session, "next")])
            .push_tool_calls(vec![read_call("w3", session, true)])
            .push_text("waited");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        let polled = tool_results(&state_of(&rt, parent).await);
        assert!(!result_of(&polled, "p1").2 && !result_of(&polled, "p2").2);
        let denied = result_of(&polled, "p3");
        assert!(
            denied.2 && denied.1.contains("third call in a row"),
            "{denied:?}"
        );
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        for (id, want) in [("w1", "r1"), ("w2", "r2"), ("w3", "r3")] {
            let (_, text, is_error) = result_of(&results, id);
            assert!(!is_error && text.contains(want), "{id}: {text}");
        }
    }

    /// The specs are the same for every run of an agent and do not change when a session starts:
    /// the only enum is the subagent names, sorted whatever order they were given in.
    pub async fn the_specs_are_byte_stable(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("done");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", session, false)])
            .push_text("ok")
            .push_text("ok");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let specs: Vec<String> = rig
            .parent_mock
            .requests()
            .iter()
            .map(|r| serde_json::to_string(&r.tools).unwrap())
            .collect();
        assert!(specs.len() >= 3);
        assert!(
            specs.windows(2).all(|w| w[0] == w[1]),
            "a session changed a spec"
        );
        let mut tools: Vec<adam_model::ToolSpec> = rig.parent_mock.requests()[0].tools.clone();
        // The loop adds the optional `after_previous` to every schema (ADR 0049): the rest is the tool's.
        for tool in &mut tools {
            let added = tool.parameters["properties"]
                .as_object_mut()
                .and_then(|properties| properties.remove("after_previous"));
            assert_eq!(added.unwrap()["type"], "boolean", "{}", tool.name);
        }
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "subagent_start",
                "subagent_send",
                "subagent_read",
                "subagent_close",
                "subagent_list"
            ]
        );
        assert_eq!(
            tools[0].parameters["properties"]["agent"]["enum"],
            json!(["far", "helper"])
        );
        // Another run of another agent built from the same roster, given in another order, has the
        // same bytes.
        let again = SessionTools::new([
            SessionAgent {
                name: "helper".into(),
                agent: "x/helper".into(),
                description: "Does a piece of work.".into(),
                within: Some(Duration::from_secs(5)),
                remote: false,
            },
            SessionAgent {
                name: "far".into(),
                agent: "far".into(),
                description: "Lives on another system.".into(),
                within: None,
                remote: true,
            },
        ]);
        let again: Vec<adam_model::ToolSpec> = again.tools().iter().map(|t| t.spec()).collect();
        assert_eq!(
            serde_json::to_string(&again).unwrap(),
            serde_json::to_string(&tools).unwrap()
        );
        for tool in rig.roster().tools() {
            assert!(
                tool.time_limit().is_some(),
                "{} declares its limit",
                tool.spec().name
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // The gaps the reviews found
    // -----------------------------------------------------------------------------------------

    /// A start of the 16th session whose try fails after it acted still answers "started", and the
    /// conversation holds exactly 16 sessions: the retry finds the round it made, and does not
    /// count it against the cap it filled.
    pub async fn a_blipped_start_of_the_last_session_still_answers_started(store: DynStore) {
        let rig = Rig::new();
        for n in 0..16 {
            rig.child_mock.push_text(format!("answer {n}"));
        }
        let attempted = Arc::new(Notify::new());
        let blipped = attempted.clone();
        let parent_agent = rig.parent_with(move |tool| {
            if tool.spec().name == "subagent_start" {
                Arc::new(BlipOn {
                    inner: tool,
                    call: "s15".into(),
                    attempted: blipped.clone(),
                })
            } else {
                tool
            }
        });
        let child_agent = rig.child();
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let sid = |n: usize| child_run_id(parent, &format!("s{n}"));
        for n in 0..15 {
            rig.parent_mock
                .push_tool_calls(vec![start_call(&format!("s{n}"), true)])
                .push_tool_calls(vec![read_call(&format!("r{n}"), sid(n), true)]);
        }
        rig.parent_mock
            .push_tool_calls(vec![start_call("s15", true)])
            .push_text("all started")
            .push_text("all done");
        let worker = spawn_worker(&rt);
        let ticker = tick(&rig.clock);
        wait_done(&rt, parent).await;
        ticker.abort();
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let (_, started, is_error) = result_of(&results, "s15");
        assert!(
            !is_error,
            "the retry answers what the first try did: {started}"
        );
        assert_eq!(as_json(started)["session"], sid(15).to_string());
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 100)
            .await
            .unwrap();
        assert_eq!(list.total, 16, "exactly 16 sessions");
        assert_eq!(list.sessions.len(), 16);
        assert_eq!(rig.child_mock.requests().len(), 16, "no round ran twice");
    }

    /// A round past its time limit is failed by the runtime, and the parent that waits on it reads
    /// `report: failed` with the reason in words.
    pub async fn a_round_past_its_deadline_reads_as_failed_with_the_reason(store: DynStore) {
        let mut rig = Rig::new().holding(0);
        rig.within = Some(Duration::from_secs(30 * 60));
        rig.child_mock.push_text("never said");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_text("it ran out of time");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        // The version is read first; only then does the clock move.
        wait_waiting_on_a_session(&rt, parent).await;
        rig.clock.advance(Duration::from_secs(31 * 60));
        // The parent's timer and the round's time limit are both due now. A parent that looks
        // before the runtime has failed the round parks until its next look, a minute later by
        // the clock: time goes on until it has looked again.
        let clock = rig.clock.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                clock.advance(Duration::from_secs(61));
            }
        });
        wait_done(&rt, parent).await;
        ticker.abort();
        rig.child_gate.release.notify_one();
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let (_, report, is_error) = result_of(&results, "c2");
        assert!(
            !is_error,
            "a failed round is a report, not an error: {report}"
        );
        assert!(report.contains("report: failed"), "{report}");
        assert!(
            report.contains("time limit: the run did not finish within 30 min"),
            "{report}"
        );
        assert!(
            report.contains("idle"),
            "a kept-open session stays usable: {report}"
        );
    }

    /// The rounds of a session are purged while the parent is parked on it: the parent wakes with
    /// an error result in words and ends, it does not hang.
    pub async fn a_purged_session_wakes_a_parked_parent_with_an_error(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("gone before it was read");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let front = rig.front(&store, &parent_agent);
        let back = rig.back(&store, &child_agent);
        let parent = rig.start(&front, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_text("it was purged");
        // The parent parks on a round nobody steps yet.
        let front_worker = spawn_worker(&front);
        wait_waiting_on_a_session(&front, parent).await;
        front_worker.stop().await;
        // The round runs to its end and its notice reaches the parent, which nobody steps; then the
        // round's record is purged.
        let back_worker = spawn_worker(&back);
        wait_done(&back, session).await;
        back_worker.stop().await;
        let purged = store
            .purge_finished(
                &rig.child_name,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        assert!(purged >= 1, "the round was purged");
        let front_worker = spawn_worker(&front);
        wait_done(&front, parent).await;
        front_worker.stop().await;
        let results = tool_results(&state_of(&front, parent).await);
        let (_, text, is_error) = result_of(&results, "c2");
        assert!(*is_error, "{text}");
        assert!(text.contains("no session"), "in words: {text}");
        assert!(text.contains("subagent_list"), "it says what to do: {text}");
    }

    /// A round that only closes a session is not work: a run that ends its turn while that round
    /// has not run yet neither waits for it (it would read the last report a second time and ask
    /// the model again) nor cancels it when it ends. The round closes the session when it runs.
    pub async fn a_round_that_only_closes_a_session_is_neither_waited_for_nor_cancelled(
        store: DynStore,
    ) {
        let rig = Rig::new();
        rig.child_mock.push_text("answer one");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let front = rig.front(&store, &parent_agent);
        let back = rig.back(&store, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&front, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![close_call("c3", session, false)])
            .push_text("finished");
        // The parent parks on the round; the round runs to its end; nobody steps the children after.
        let front_worker = spawn_worker(&front);
        wait_waiting_on_a_session(&front, parent).await;
        front_worker.stop().await;
        let back_worker = spawn_worker(&back);
        wait_done(&back, session).await;
        back_worker.stop().await;
        // The parent reads the report, closes the session (a round that only closes it, which
        // nobody steps) and says its last words: the run ends there.
        let front_worker = spawn_worker(&front);
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(
            done.output.expect("output")["text"],
            "finished",
            "nothing was cancelled, so nothing is said about it"
        );
        assert_eq!(
            rig.parent_mock.requests().len(),
            4,
            "no turn after the last"
        );
        let state = state_of(&front, parent).await;
        assert!(
            !state
                .messages
                .iter()
                .flat_map(|m| m.tool_calls())
                .any(|c| c.id.starts_with("wait")),
            "the closing round was not waited for"
        );
        let listed = front
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        let closing = listed.sessions[0].latest_run;
        assert_eq!(listed.sessions[0].round, 2);
        // The round was left to run: it closes the session.
        let back_worker = spawn_worker(&back);
        wait_done(&back, closing).await;
        back_worker.stop().await;
        let listed = front
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(listed.sessions[0].state, SessionState::Closed);
        assert_eq!(rig.child_mock.requests().len(), 1, "it called no model");
    }

    /// The wait at the end of a turn is for the rounds **this task** started. A kept-open session an
    /// earlier task of the conversation left working is neither waited for nor cancelled by this
    /// task's end (ADR 0045 decision 4: "a round it started in that task"; decision 7: a later task
    /// finds the sessions of the conversation and may send to them, which makes the round its own).
    pub async fn the_turn_end_wait_is_for_this_tasks_rounds_only(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock
            .push_text("this task's round")
            .push_text("the stranded one");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        // An earlier task of the conversation: it ended, and a round it started still works.
        rig.parent_mock.push_text("the earlier task");
        let earlier = rig.start(&rt, &conv).await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, earlier).await;
        let stranded = child_run_id(earlier, "old");
        let header = SessionHeader {
            id: stranded,
            agent: rig.child_name.clone(),
            description: "left working".into(),
            keep_open: true,
            closed: false,
            closing: false,
            round: 1,
        };
        let mut first = user_message("old work");
        first.payload["session"] = serde_json::to_value(&header).unwrap();
        rt.start_child_continuing_within(
            earlier,
            stranded,
            &rig.child_name,
            first,
            &adam_runtime::session_conversation_id(&conv, stranded),
            None,
            None,
        )
        .await
        .unwrap();
        reached(&rig.child_gate.reached, "the stranded round's model call").await;

        // This task starts a round of its own, answers, and ends with the other one still working.
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started mine")
            .push_text("mine said it");
        let this = rig.start(&rt, &conv).await;
        wait_done(&rt, this).await;
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        let other = list.sessions.iter().find(|s| s.id == stranded).unwrap();
        assert_eq!(
            other.state,
            SessionState::Working,
            "not waited for, not cancelled"
        );
        assert_eq!(other.outcome, None);
        rig.child_gate.release.notify_one();
        wait_for(&rt, stranded, "the stranded round to end", |v| {
            v.status == RunStatus::Done
        })
        .await;
        worker.stop().await;
    }

    /// A task whose only session call is a send to a kept-open session of an earlier task still
    /// waits for the round it made: its answer in text does not end the run.
    pub async fn a_task_that_only_sends_waits_for_its_round(store: DynStore) {
        let rig = Rig::new().holding(1);
        rig.child_mock.push_text("round one").push_text("round two");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let first = rig.start(&rt, &conv).await;
        let session = child_run_id(first, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_text("one round done");
        let worker = spawn_worker(&rt);
        wait_done(&rt, first).await;

        rig.parent_mock
            .push_tool_calls(vec![send_call("d1", session, "and now this")])
            .push_text("sent it")
            .push_text("it said round two");
        let second = rig.start(&rt, &conv).await;
        reached(&rig.child_gate.reached, "round two").await;
        let parked = wait_waiting_on_a_session(&rt, second).await;
        assert!(matches!(
            &conversation(&parked).pending_wait,
            Some(PendingWait::Session(PendingSession { call_id, tool, session: s }))
                if call_id == "wait_1" && tool == "subagent_read" && *s == session
        ));
        rig.child_gate.release.notify_one();
        let done = wait_done(&rt, second).await;
        worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "it said round two");
        let results = tool_results(&state_of(&rt, second).await);
        let report = &result_of(&results, "wait_1").1;
        assert!(
            report.contains("round two") && report.contains("round 2"),
            "{report}"
        );
    }

    /// Two reads that wait, on two sessions, in one model turn: each result is for its own call and
    /// its own session, whichever round ends first.
    pub async fn two_waiting_reads_in_one_turn_each_get_their_own_report(store: DynStore) {
        for held in [0, 1] {
            let rig = Rig::new().holding(held);
            rig.child_mock
                .push_text("answer one")
                .push_text("answer two");
            let (parent_agent, child_agent) = (rig.parent(), rig.child());
            let rt = rig.together(&store, &parent_agent, &child_agent);
            let parent = rig.start(&rt, &conv_id()).await;
            let (a, b) = (child_run_id(parent, "c1"), child_run_id(parent, "c2"));
            rig.parent_mock
                .push_tool_calls(vec![start_call("c1", false)])
                .push_tool_calls(vec![start_call("c2", false)])
                .push_tool_calls(vec![read_call("r1", a, true), read_call("r2", b, true)])
                .push_text("both read");
            let worker = spawn_worker(&rt);
            reached(&rig.child_gate.reached, "the held round").await;
            // The parent is parked on a round that works while the other has ended: a wake that
            // is not the held round's end (a notice again) must not give either read the other's
            // report.
            wait_waiting_on_a_session(&rt, parent).await;
            for session in [a, b] {
                let notice = Inbound::new(
                    CHILD_REPORTED_KIND,
                    json!({"status": "done", "output": {"text": "forged"}}),
                )
                .with_id(session.to_string());
                // The run may have ended already: then there is nobody to wake.
                let _ = rt.deliver(parent, notice).await;
            }
            rig.child_gate.release.notify_one();
            wait_done(&rt, parent).await;
            worker.stop().await;
            let results = tool_results(&state_of(&rt, parent).await);
            for (call, session, other) in [("r1", a, b), ("r2", b, a)] {
                let said = rt.view(session).await.unwrap().unwrap().output.unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let said_by_other = rt.view(other).await.unwrap().unwrap().output.unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let (_, report, is_error) = result_of(&results, call);
                assert!(!is_error, "{call}: {report}");
                assert!(
                    report.starts_with(&format!("session {session} ")),
                    "held {held}, {call}: {report}"
                );
                assert!(
                    report.contains(&said) && !report.contains(&said_by_other),
                    "held {held}, {call}: {report}"
                );
                assert!(report.contains("report: done"), "{report}");
            }
        }
    }

    /// A send that steered a working round is tried again: the message reached the child once, and
    /// no round is made for its text. The second try can find the round ended (it reads the id in
    /// the round's state) or still working (it steers again, and the child drops the copy without
    /// a turn of its own); the worker decides which, and both end the same way.
    pub async fn a_steer_retried_after_the_round_ended_reaches_the_child_once(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock
            .push_text("first try")
            .push_text("changed course");
        let attempted = Arc::new(Notify::new());
        let blipped = attempted.clone();
        let parent_agent = rig.parent_with(move |tool| {
            if tool.spec().name == "subagent_send" {
                Arc::new(BlipOn {
                    inner: tool,
                    call: "c2".into(),
                    attempted: blipped.clone(),
                })
            } else {
                tool
            }
        });
        let child_agent = rig.child();
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![send_call("c2", session, "steer me")])
            .push_tool_calls(vec![read_call("c3", session, true)])
            .push_text("steered once");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        reached(&attempted, "the steer").await;
        // The first try steered the round and failed; before its retry the round reads the message
        // and ends.
        rig.child_gate.release.notify_one();
        wait_for(&rt, session, "the round to end", |v| {
            v.status == RunStatus::Done
        })
        .await;
        let ticker = tick(&rig.clock);
        wait_done(&rt, parent).await;
        ticker.abort();
        worker.stop().await;
        let results = tool_results(&state_of(&rt, parent).await);
        let sent = as_json(&result_of(&results, "c2").1);
        assert_eq!(sent["delivered"], "steered", "{sent}");
        assert_eq!(sent["round"], 1);
        assert!(result_of(&results, "c3").1.contains("changed course"));
        let requests = rig.child_mock.requests();
        assert_eq!(requests.len(), 2, "the message made no round of its own");
        let said = |r: &ModelRequest| {
            r.messages
                .iter()
                .filter(|m| **m == Message::user_text("steer me"))
                .count()
        };
        assert_eq!(said(&requests[1]), 1);
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].round, 1);
    }

    /// Every id the loop invents for a tool call is one every provider accepts
    /// (`^[a-zA-Z0-9_-]{1,64}$`). The loop invents one: the read that waits at the end of a turn
    /// (`wait_<turn>`); a model's own ids are the provider's (and `call_<16 hex>` when a provider
    /// gives none, in `adam-model-openai`).
    pub async fn the_ids_the_loop_invents_are_ones_every_provider_accepts(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("late answer");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started")
            .push_text("read it");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let ids: Vec<&str> = state
            .messages
            .iter()
            .flat_map(|m| m.tool_calls())
            .map(|c| c.id.as_str())
            .collect();
        assert!(ids.iter().any(|id| id.starts_with("wait")), "{ids:?}");
        let results = tool_results(&state);
        assert!(results.iter().any(|(id, _, _)| id.starts_with("wait")));
        for id in ids
            .into_iter()
            .chain(results.iter().map(|(id, _, _)| id.as_str()))
        {
            assert!(
                (1..=64).contains(&id.len())
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "`{id}` is not an id every provider accepts"
            );
        }
    }

    // -----------------------------------------------------------------------------------------
    // The tokens of a round are counted once
    // -----------------------------------------------------------------------------------------

    /// A report read twice counts its tokens once.
    pub async fn a_report_read_twice_counts_its_tokens_once(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock
            .push_response(answer_with("counted", 100, 10));
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![read_call("c3", session, false)])
            .push_text("read twice");
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        assert_eq!(tokens(&state), (100, 10));
        assert!(
            state.rounds_started.is_empty(),
            "{:?}",
            state.rounds_started
        );
    }

    /// A round whose report is never read, in a session that is then closed, is counted.
    pub async fn a_round_never_read_is_counted_when_its_session_closes(store: DynStore) {
        // The parent of a build that does not tell the model of a round that ended: nothing reads
        // it. (One that does reads it at its next model call, and counts it then.)
        let rig = Rig::new().with_hold().without_reports();
        rig.child_mock.push_response(answer_with("unread", 100, 10));
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_tool_calls(vec![close_call("x1", session, false)])
            .push_tool_calls(vec![call("h2", "hold", json!({}))])
            .push_text("closed without reading");
        let worker = spawn_worker(&rt);
        // The round ends while the parent does something else, and nobody reads it.
        reached(&hold.reached, "the parent's other work").await;
        wait_done(&rt, session).await;
        hold.release.notify_one();
        // The close of the idle session is a round of its own; it has ended before the turn does.
        reached(&hold.reached, "the parent's other work, again").await;
        wait_done(&rt, child_run_id(parent, "x1")).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        assert_eq!(as_json(&result_of(&results, "x1").1)["state"], "closed");
        assert!(
            !state.messages.iter().any(|m| m.text().contains("unread")),
            "the report was never read"
        );
        assert_eq!(tokens(&state), (100, 10));
    }

    /// A round whose report is never read, in a run that ends, is counted when the run ends
    /// (`sessions_end`): the parent of a build that does not tell the model of an ended round.
    pub async fn a_round_never_read_is_counted_when_the_parent_ends(store: DynStore) {
        let rig = Rig::new().with_hold().without_reports();
        rig.child_mock.push_response(answer_with("unread", 100, 10));
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("ended without reading");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        wait_done(&rt, session).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        assert_eq!(tokens(&state), (100, 10));
        assert!(state.rounds_started.is_empty());
    }

    /// A store error on the read of a round's usage is tried again, not swallowed: the tokens are
    /// counted once, after the retry.
    pub async fn a_store_error_on_the_usage_read_is_retried(store: DynStore) {
        // As above: nothing reads the round before the run ends.
        let rig = Rig::new().with_hold().without_reports();
        rig.child_mock.push_response(answer_with("unread", 100, 10));
        let hold = rig.hold.clone().unwrap();
        let (inner, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&dynamic, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            // The end of the run is tried again after the failed read, with the model asked again.
            .push_text("ended without reading")
            .push_text("ended without reading, again");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        wait_done(&rt, session).await;
        // The round is read once more, at the end of the parent's run, and that read fails once.
        inner.fail_run(Method::LoadRun, session, 1);
        hold.release.notify_one();
        let ticker = tick(&rig.clock);
        wait_done(&rt, parent).await;
        ticker.abort();
        worker.stop().await;
        assert_eq!(inner.injected(Method::LoadRun), 1, "the read failed once");
        assert_eq!(tokens(&state_of(&rt, parent).await), (100, 10));
    }

    /// The journal of a run that counted rounds, read or not, gives a replay the same totals with
    /// no child running and no model asked.
    pub async fn a_replay_counts_the_same_tokens(store: DynStore) {
        let rig = Rig::new().with_hold().holding(1);
        rig.child_mock
            .push_response(answer_with("round one", 100, 10))
            .push_response(answer_with("round two", 50, 5));
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![send_call("c3", session, "again")])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("the second round was told by the loop");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "round two").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, child_run_id(parent, "c3")).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let original = state_of(&rt, parent).await;
        assert_eq!(tokens(&original), (150, 15), "both rounds, once each");

        let again = Rig::new().with_hold();
        let (parent_agent, child_agent) = (again.parent(), again.child());
        let again_rt = again.together(&store, &parent_agent, &child_agent);
        let copy = again.start(&again_rt, &conv_id()).await;
        let mut seq = 0;
        while let Some(entry) = store.journal_get(parent, seq).await.unwrap() {
            store.journal_put(copy, entry).await.unwrap();
            seq += 1;
        }
        let worker = spawn_worker(&again_rt);
        let clock = again.clock.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                clock.advance(Duration::from_secs(61));
            }
        });
        wait_done(&again_rt, copy).await;
        ticker.abort();
        worker.stop().await;
        let replayed = state_of(&again_rt, copy).await;
        assert_eq!(tokens(&replayed), tokens(&original));
        assert_eq!(replayed.usage_totals, original.usage_totals);
        assert_eq!(again.child_mock.requests().len(), 0, "no child ran");
        assert_eq!(again.parent_mock.requests().len(), 0, "no model was asked");
    }

    /// A report is counted when the model reads it, not only when the run ends: the totals of a run
    /// that is parked on a later round already hold the round it read.
    pub async fn a_report_is_counted_when_it_is_read(store: DynStore) {
        let rig = Rig::new().holding(1);
        rig.child_mock
            .push_response(answer_with("first", 100, 10))
            .push_text("second");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let (a, b) = (child_run_id(parent, "c1"), child_run_id(parent, "c3"));
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![read_call("c2", a, true)])
            .push_tool_calls(vec![start_call("c3", false)])
            .push_tool_calls(vec![read_call("c4", b, true)])
            .push_text("both read");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the second round").await;
        let parked = wait_for(&rt, parent, "parked on the second round", |v| {
            matches!(
                conversation(v).pending_wait,
                Some(PendingWait::Session(ref wait)) if wait.call_id == "c4"
            )
        })
        .await;
        let state = conversation(&parked);
        assert_eq!(
            tokens(&state),
            (100, 10),
            "the first round is counted, once read"
        );
        assert_eq!(
            state.rounds_started,
            vec!["c3".to_owned()],
            "the second is still owed"
        );
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        assert_eq!(tokens(&state), (100, 10));
        assert!(state.rounds_started.is_empty());
    }

    /// The cancel of the parent closes a session that was not kept open, as it leaves a kept-open
    /// one idle: nothing more can be sent to it.
    pub async fn cancelling_the_parent_closes_a_session_that_was_not_kept_open(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("never said");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let first = rig.start(&rt, &conv).await;
        let session = child_run_id(first, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, first).await;
        rt.cancel(first, "the person stopped it").await.unwrap();
        wait_for(&rt, session, "the round to be cancelled", |v| {
            v.status == RunStatus::Failed
        })
        .await;
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Closed);
        rig.parent_mock
            .push_tool_calls(vec![send_call("d1", session, "carry on")])
            .push_text("refused");
        let second = rig.start(&rt, &conv).await;
        wait_done(&rt, second).await;
        let results = tool_results(&state_of(&rt, second).await);
        let (_, refusal, is_error) = result_of(&results, "d1");
        assert!(*is_error && refusal.contains("is closed"), "{refusal}");
        rig.child_gate.release.notify_one();
        worker.stop().await;
    }

    // -----------------------------------------------------------------------
    // Limits, idle sessions and cancel (ADR 0045, slice 4)
    // -----------------------------------------------------------------------

    /// Each limit refuses in words: `max_working` on a start and on a send that would start a
    /// round (and never on a steer into a working round), `max_open` on a start, `max_rounds` on a
    /// send and not on a close.
    pub async fn each_limit_refuses_in_words(store: DynStore) {
        let mut rig = Rig::new().holding(1);
        rig.limits = Some(SessionLimits::new(2, 1, 2).unwrap());
        for text in ["a0", "a1", "a1 again", "a2", "a2 again"] {
            rig.child_mock.push_text(text);
        }
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let (s0, s1) = (child_run_id(parent, "c0"), child_run_id(parent, "c1"));
        for calls in [
            vec![start_call("c0", true)],
            vec![read_call("r0", s0, true)],
            vec![start_call("c1", true)],
            vec![send_call("m0", s0, "more")],
            vec![send_call("st", s1, "steer")],
            vec![start_call("c2", true)],
            vec![read_call("r1", s1, true)],
            vec![start_call("c3", true)],
            vec![close_call("x0", s0, false)],
            vec![send_call("n2", s1, "round two")],
            vec![read_call("r2", s1, true)],
            vec![send_call("n3", s1, "round three")],
            vec![close_call("x1", s1, false)],
        ] {
            rig.parent_mock.push_tool_calls(calls);
        }
        rig.parent_mock.push_text("done").push_text("done again");
        let worker = spawn_worker(&rt);
        // The second round is held in its model call, so it works while the parent refuses; the
        // parent parks at `r1`, after every refusal.
        reached(&rig.child_gate.reached, "the second round's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let results = tool_results(&state_of(&rt, parent).await);
        let text = |id: &str| result_of(&results, id).clone();
        for id in ["m0", "c2"] {
            let (_, words, is_error) = text(id);
            assert!(
                is_error && words.contains("1 subagent rounds are already working"),
                "{id}: {words}"
            );
        }
        let (_, steered, is_error) = text("st");
        assert!(
            !is_error,
            "a steer into a working round starts none: {steered}"
        );
        let (_, words, is_error) = text("c3");
        assert!(is_error && words.contains("2 open sessions"), "{words}");
        assert_eq!(as_json(&text("x0").1)["state"], "closed");
        assert!(!text("n2").2, "{:?}", text("n2"));
        let (_, words, is_error) = text("n3");
        assert!(is_error && words.contains("2 rounds"), "{words}");
        assert_eq!(
            as_json(&text("x1").1)["state"],
            "closed",
            "a close is never refused"
        );
    }

    /// A round that only closes a session works for no one: it calls no model and ends by itself, so
    /// it does not count against `max_working`. The child's worker is stopped after round 1, so the
    /// closing round stays open and the test does not race its claim.
    async fn a_closing_round_is_not_counted_as_working(store: DynStore, then_start: bool) {
        let (mut rig, gate) = Rig::new().holding_parent(4);
        rig.limits = Some(SessionLimits::new(4, 1, 20).unwrap());
        rig.child_mock.push_text("a0").push_text("a1");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let (front, back) = (
            rig.front(&store, &parent_agent),
            rig.back(&store, &child_agent),
        );
        let parent = rig.start(&front, &conv_id()).await;
        let (s0, s1) = (child_run_id(parent, "c0"), child_run_id(parent, "c1"));
        for calls in [
            vec![start_call("c0", true)],
            vec![read_call("r0", s0, true)],
            vec![start_call("c1", true)],
            vec![read_call("r1", s1, true)],
            vec![close_call("x0", s0, false)],
            vec![if then_start {
                start_call("next", true)
            } else {
                send_call("next", s1, "more")
            }],
        ] {
            rig.parent_mock.push_tool_calls(calls);
        }
        rig.parent_mock.push_text("done").push_text("done again");
        let (front_worker, back_worker) = (spawn_worker(&front), spawn_worker(&back));
        reached(&gate.reached, "the parent's call before the close").await;
        back_worker.stop().await;
        gate.release.notify_one();
        wait_waiting_on_a_session(&front, parent).await;
        let results = tool_results(&state_of(&front, parent).await);
        let (_, text, is_error) = result_of(&results, "x0");
        assert!(!*is_error, "{text}");
        let (_, text, is_error) = result_of(&results, "next");
        assert!(
            !*is_error,
            "the closing round of the session just closed is no working round: {text}"
        );
        front_worker.stop().await;
    }

    pub async fn a_close_then_a_start_is_not_refused_by_the_closing_round(store: DynStore) {
        a_closing_round_is_not_counted_as_working(store, true).await;
    }

    pub async fn a_close_then_a_send_is_not_refused_by_the_closing_round(store: DynStore) {
        a_closing_round_is_not_counted_as_working(store, false).await;
    }

    /// A kept-open session nobody has written to for the idle TTL reads closed: a send is refused
    /// as to a closed session and says why, and the session no longer counts toward `max_open`.
    pub async fn an_idle_expired_session_refuses_a_send_as_closed(store: DynStore) {
        let mut rig = Rig::new();
        rig.limits = Some(SessionLimits::new(1, 1, 20).unwrap());
        rig.idle_ttl = Some(Duration::from_secs(3600));
        rig.child_mock.push_text("a0").push_text("a1");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let first = rig.start(&rt, &conv).await;
        let session = child_run_id(first, "c0");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c0", true)])
            .push_tool_calls(vec![read_call("r0", session, true)])
            .push_text("one");
        let worker = spawn_worker(&rt);
        wait_done(&rt, first).await;
        let listed = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(listed.sessions[0].state, SessionState::Idle);

        rig.clock.advance(Duration::from_secs(2 * 3600));
        rig.parent_mock
            .push_tool_calls(vec![call("l0", "subagent_list", json!({}))])
            .push_tool_calls(vec![send_call("d0", session, "back")])
            .push_tool_calls(vec![start_call("c1", true)])
            .push_text("two")
            .push_text("two, and it reported");
        let second = rt
            .start_continuing(&rig.parent_name, user_message("again"), Some(&conv), first)
            .await
            .unwrap();
        wait_done(&rt, second).await;
        worker.stop().await;

        let results = tool_results(&state_of(&rt, second).await);
        assert!(
            result_of(&results, "l0").1.contains("closed"),
            "{results:?}"
        );
        let (_, words, is_error) = result_of(&results, "d0");
        assert!(
            *is_error && words.contains("is closed") && words.contains("idle"),
            "{words}"
        );
        assert!(
            !result_of(&results, "c1").2,
            "an expired session frees its place under max_open"
        );
    }

    /// The report of a round nobody has been told of stays due when its session has since
    /// expired: it is told once, by the next task's first model call, and not again.
    pub async fn an_expired_sessions_report_is_still_told_once(store: DynStore) {
        let mut rig = Rig::new();
        rig.idle_ttl = Some(Duration::from_secs(3600));
        rig.child_mock.push_text("the person's round says 9");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        rig.parent_mock.push_text("first task");
        let first = rig.start(&rt, &conv).await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, first).await;
        let theirs = start_persons_round(&rt, &rig, &conv, "asked by a person").await;
        wait_done(&rt, theirs).await;
        rig.clock.advance(Duration::from_secs(2 * 3600));
        let listed = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert!(listed.sessions[0].idle_expired);
        assert_eq!(listed.sessions[0].state, SessionState::Closed);

        let next = |prior: RunId, text: &'static str| {
            let rt = rt.clone();
            let (name, conv) = (rig.parent_name.clone(), conv.clone());
            async move {
                rt.start_continuing(&name, user_message(text), Some(&conv), prior)
                    .await
                    .unwrap()
            }
        };
        rig.parent_mock.push_text("second task");
        let second = next(first, "second").await;
        wait_done(&rt, second).await;
        rig.parent_mock.push_text("third task");
        let third = next(second, "third").await;
        wait_done(&rt, third).await;
        worker.stop().await;

        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3);
        let told = report_calls(&requests[1].messages);
        assert_eq!(told.len(), 1, "{:?}", requests[1].messages);
        assert_eq!(told[0].id, report_id(theirs));
        assert!(matches!(
            requests[1].messages.last(),
            Some(Message::Tool { content, .. }) if content.contains("round says 9")
        ));
        assert_eq!(
            report_calls(&requests[2].messages).len(),
            1,
            "not told again"
        );
        assert_eq!(state_of(&rt, third).await.reported_rounds, vec![theirs]);
    }

    /// A parent whose model fails for good stops the rounds it started.
    pub async fn a_parent_that_fails_stops_its_rounds(store: DynStore) {
        let rig = Rig::new().holding(0);
        rig.child_mock.push_text("never said");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_error(ModelError::Auth("bad key".into()));
        let worker = spawn_worker(&rt);
        wait_for(&rt, parent, "the parent to fail", |v| {
            v.status == RunStatus::Failed
        })
        .await;
        let round = wait_for(&rt, session, "the round to be cancelled", |v| {
            v.status == RunStatus::Failed
        })
        .await;
        assert_eq!(
            round.error.as_deref(),
            Some("cancelled: the parent run ended")
        );
        let list = rt
            .list_sessions(&conv, &[&rig.child_name], 0, 10)
            .await
            .unwrap();
        assert_eq!(list.sessions[0].state, SessionState::Idle, "kept open");
        rig.child_gate.release.notify_one();
        worker.stop().await;
    }

    /// A cancelled parent frees the working slots: the next task starts a round under
    /// `max_working` 1.
    pub async fn a_cancel_frees_the_working_slots_for_the_next_task(store: DynStore) {
        let mut rig = Rig::new().holding(0);
        rig.limits = Some(SessionLimits::new(2, 1, 20).unwrap());
        rig.child_mock
            .push_text("never said")
            .push_text("c2 says 5");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let first = rig.start(&rt, &conv).await;
        let session = child_run_id(first, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_text("started");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, first).await;
        rt.cancel(first, "the person stopped it").await.unwrap();
        wait_for(&rt, session, "the round to be cancelled", |v| {
            v.status == RunStatus::Failed
        })
        .await;

        rig.parent_mock
            .push_tool_calls(vec![start_call("c2", true)])
            .push_text("started the second")
            .push_text("the second said 5");
        let second = rt
            .start_continuing(&rig.parent_name, user_message("again"), Some(&conv), first)
            .await
            .unwrap();
        wait_done(&rt, second).await;
        let results = tool_results(&state_of(&rt, second).await);
        let (_, started, is_error) = result_of(&results, "c2");
        assert!(
            !*is_error,
            "no slot is held by the cancelled round: {started}"
        );
        rig.child_gate.release.notify_one();
        worker.stop().await;
    }

    // -----------------------------------------------------------------------
    // Reports during a turn (ADR 0045, slice 3)
    // -----------------------------------------------------------------------

    /// A round that ends while the parent does something else is reported to the model by the
    /// next model call: a `subagent_read` call and its result are appended before the request, with
    /// no read by the model and no wait.
    pub async fn a_report_that_ends_mid_turn_reaches_the_next_model_call(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_response(answer_with("the helper said 42", 500, 5));
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        hold.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "the helper said 42");

        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3, "no wait, no extra call");
        let id = report_id(session);
        let sent = &requests[2].messages;
        let n = sent.len();
        match &sent[n - 2] {
            Message::Assistant { tool_calls, .. } => {
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "subagent_read");
                assert_eq!(tool_calls[0].id, id);
                assert_eq!(tool_calls[0].arguments["session"], session.to_string());
            }
            other => panic!("the report's call comes first: {other:?}"),
        }
        match &sent[n - 1] {
            Message::Tool {
                call_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(*call_id, id);
                assert!(!*is_error);
                assert!(content.contains("report: done"), "{content}");
                assert!(content.contains("child says 42"), "{content}");
                assert!(!content.contains("started by the person"), "{content}");
            }
            other => panic!("the report's result is last: {other:?}"),
        }
        let state = state_of(&rt, parent).await;
        assert_eq!(report_calls(&state.messages).len(), 1);
        assert_eq!(state.reported_rounds, vec![session]);
        assert!(
            !state
                .messages
                .iter()
                .flat_map(|m| m.tool_calls())
                .any(|c| c.id.starts_with("wait_")),
            "the report was not waited for"
        );
        // The report counted the round's tokens, as a read does.
        assert!(
            state.rounds_started.is_empty(),
            "{:?}",
            state.rounds_started
        );
        assert_eq!(state.turns, 3);
        // The call that was told the report counted the messages it was given, the report's two
        // included: the next estimate starts from them.
        let prompt = state.last_prompt.expect("the provider counted the prompt");
        assert_eq!(prompt.messages, n, "{prompt:?}");
        // Every session was closed when the report was told: the loop stops looking.
        assert!(!state.sessions_open);
    }

    /// Every request of a run that is told a report in the middle of a turn starts with the one
    /// before it, and the system and the tools are the same bytes: the report is only appended.
    pub async fn reports_during_a_turn_keep_the_prefix(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        assert!(state.shortenings.is_empty());
        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3);
        for pair in requests.windows(2) {
            assert_eq!(
                serde_json::to_string(&(&pair[0].system, &pair[0].tools)).unwrap(),
                serde_json::to_string(&(&pair[1].system, &pair[1].tools)).unwrap(),
                "system and tools are the same bytes"
            );
            assert!(
                starts_with(&pair[1].messages, &pair[0].messages),
                "a request starts with the one before it:\n{:?}\n{:?}",
                pair[0].messages,
                pair[1].messages
            );
        }
        assert_eq!(report_calls(&requests[2].messages).len(), 1);
        // What the model was sent last is the history it now has.
        assert!(starts_with(&state.messages, &requests[2].messages));
    }

    /// A round that ends while the parent's last model call is out is not lost: the turn does not
    /// end on that answer. It is read from the store, as a round that still works is waited for.
    pub async fn a_report_during_the_final_model_call_reopens_the_turn(store: DynStore) {
        let (rig, parent_gate) = Rig::new().holding(0).holding_parent(1);
        rig.child_mock.push_text("child says 42");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started it, and that is all")
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&parent_gate.reached, "the parent's last model call").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        parent_gate.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(
            done.output.expect("output")["text"],
            "the helper said 42",
            "the run did not end on the answer the report missed"
        );
        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        let report = &result_of(&results, "wait_1").1;
        assert!(report.contains("child says 42"), "{report}");
        assert_eq!(rig.parent_mock.requests().len(), 3);
        assert_eq!(state.reported_rounds, vec![session]);
    }

    /// The notice that a round ended is a wake-up: when it is lost (the commit that would deliver
    /// it fails) the next model call finds the report in the store, with no timer.
    pub async fn a_report_is_found_in_the_store_when_its_notice_is_lost(store: DynStore) {
        let rig = Rig::new().with_hold();
        rig.child_mock.push_text("found without a notice");
        let hold = rig.hold.clone().unwrap();
        let (inner, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let front = rig.front(&dynamic, &parent_agent);
        let back = rig.back(&dynamic, &child_agent);
        let parent = rig.start(&front, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("found it");
        let front_worker = spawn_worker(&front);
        reached(&hold.reached, "the parent's other work").await;
        // The commit that would deliver the notice to the parent fails.
        inner.fail_run(Method::CommitRun, parent, 1);
        let back_worker = spawn_worker(&back);
        let deadline = Instant::now() + Duration::from_secs(20);
        while inner.injected(Method::CommitRun) < 1 {
            assert!(Instant::now() < deadline, "the notice was never sent");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        wait_done(&back, session).await;
        back_worker.stop().await;
        assert_eq!(
            front.view(parent).await.unwrap().unwrap().pending_inbox,
            0,
            "the notice was lost"
        );
        hold.release.notify_one();
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "found it");
        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(
            matches!(
                requests[2].messages.last(),
                Some(Message::Tool { content, .. }) if content.contains("found without a notice")
            ),
            "{:?}",
            requests[2].messages.last()
        );
    }

    /// A parent parked on its turn-end wait whose notice is lost is woken by its timer, reads the
    /// report and goes on: the loss costs the timer and one model call, no more.
    pub async fn a_lost_notice_while_the_parent_is_parked_costs_one_turn(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("found by the timer");
        let (inner, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let front = rig.front(&dynamic, &parent_agent);
        let back = rig.back(&dynamic, &child_agent);
        let parent = rig.start(&front, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started it")
            .push_text("the helper said so");
        let front_worker = spawn_worker(&front);
        wait_waiting_on_a_session(&front, parent).await;
        inner.fail_run(Method::CommitRun, parent, 1);
        let back_worker = spawn_worker(&back);
        let deadline = Instant::now() + Duration::from_secs(20);
        while inner.injected(Method::CommitRun) < 1 {
            assert!(Instant::now() < deadline, "the notice was never sent");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        wait_done(&back, session).await;
        back_worker.stop().await;
        let still = wait_waiting_on_a_session(&front, parent).await;
        assert_eq!(still.pending_inbox, 0, "the notice was lost");
        assert_eq!(rig.parent_mock.requests().len(), 2, "parked, no call");

        rig.clock.advance(Duration::from_secs(61));
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "the helper said so");
        assert_eq!(
            rig.parent_mock.requests().len(),
            3,
            "one call after the timer"
        );
        let results = tool_results(&state_of(&front, parent).await);
        assert!(
            result_of(&results, "wait_1")
                .1
                .contains("found by the timer")
        );
    }

    /// A closing round that follows a round the parent has not looked at does not hide that
    /// round's report: the report is of the latest round that worked.
    pub async fn a_closing_round_does_not_hide_the_report_of_the_round_before(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            // The round ends during the hold, and the call after it closes the idle session.
            .push_tool_calls(vec![
                call("h1", "hold", json!({})),
                close_call("x1", session, false),
            ])
            .push_text("closed it")
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let told = tool_results(&state)
            .iter()
            .filter(|(id, text, _)| {
                (id.starts_with("report_") || id.starts_with("wait_"))
                    && text.contains("child says 42")
            })
            .count();
        assert_eq!(told, 1, "{:?}", tool_results(&state));
        assert_eq!(state.reported_rounds, vec![session]);
    }

    /// The journal of a run whose answer was recorded with reports, and that is replayed after a
    /// message arrived (the commit was lost), takes the same steps as the first try: the round is
    /// counted once and the stale answer is dropped.
    pub async fn a_replay_of_a_stale_answer_counts_each_round_once(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock
            .push_response(answer_with("child says 42", 100, 10));
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let original = state_of(&rt, parent).await;
        assert_eq!(tokens(&original), (100, 10));

        let again = Rig::new().with_hold();
        again.parent_mock.push_text("answered again");
        let (parent_agent, child_agent) = (again.parent(), again.child());
        let again_rt = again.together(&store, &parent_agent, &child_agent);
        let copy = again.start(&again_rt, &conv_id()).await;
        let mut seq = 0;
        // What a worker that crashed left: the journal up to the round's count, before the step
        // that decides whether the turn may end.
        while let Some(entry) = store.journal_get(parent, seq).await.unwrap() {
            if entry.name.starts_with("sessions:") {
                break;
            }
            store.journal_put(copy, entry).await.unwrap();
            seq += 1;
        }
        // A message that came in before the worker that replays took the run: every recorded
        // answer predates it.
        again_rt
            .deliver(copy, user_message("one more thing").with_id("steer-1"))
            .await
            .unwrap();
        let worker = spawn_worker(&again_rt);
        let done = wait_for(&again_rt, copy, "the replay to end", |v| {
            v.status.is_terminal()
        })
        .await;
        worker.stop().await;
        assert_eq!(done.status, RunStatus::Done, "{done:?}");
        let replayed = conversation(&done);
        assert_eq!(tokens(&replayed), (100, 10), "counted once");
        assert!(replayed.rounds_started.is_empty());
        // The answer that predated the message was dropped, and the model answered again.
        assert_eq!(again.parent_mock.requests().len(), 1, "one live call");
        assert_eq!(done.output.expect("output")["text"], "answered again");
    }

    /// A notice that comes again, before the report and after it, matches nothing: the loop never
    /// reads one, so there is no second report and no extra model call.
    pub async fn a_duplicate_report_notice_matches_nothing(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("once");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("done");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        // Before the round has ended.
        for _ in 0..2 {
            rt.deliver(parent, forged_notice(session)).await.unwrap();
        }
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        // After it has, still before the parent has looked.
        rt.deliver(parent, forged_notice(session)).await.unwrap();
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        assert_eq!(report_calls(&state.messages).len(), 1);
        assert_eq!(rig.parent_mock.requests().len(), 3, "no extra model call");
        assert!(!state.messages.iter().any(|m| m.text().contains("forged")));
        let late = rt.deliver(parent, forged_notice(session)).await;
        assert!(
            matches!(late, Err(RuntimeError::Finished { .. })),
            "{late:?}"
        );
    }

    /// A round a person started in the conversation is reported to the model, marked as theirs,
    /// without the person's words: it is a tool result, never a message of the person.
    pub async fn a_persons_round_is_reported_as_the_persons(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock
            .push_text("child says 42")
            .push_text("the person's round says 7");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let parent = rig.start(&rt, &conv).await;
        let mine = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("both reports read");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, mine).await;
        let theirs = start_persons_round(&rt, &rig, &conv, "a private thing the person said").await;
        wait_done(&rt, theirs).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        let of_theirs = &result_of(&results, &report_id(theirs)).1;
        assert!(of_theirs.contains("started by the person"), "{of_theirs}");
        assert!(
            of_theirs.contains("the person's round says 7"),
            "{of_theirs}"
        );
        assert!(
            !of_theirs.contains("a private thing"),
            "the person's words are not in it: {of_theirs}"
        );
        let of_mine = &result_of(&results, &report_id(mine)).1;
        assert!(!of_mine.contains("started by the person"), "{of_mine}");
        assert!(of_mine.contains("child says 42"), "{of_mine}");
        // Both are results of calls the loop made, never messages of a user.
        assert!(
            !state
                .messages
                .iter()
                .any(|m| matches!(m, Message::User { .. }) && m.text().contains("private")),
        );
        assert_eq!(rig.parent_mock.requests().len(), 3);
    }

    /// A person's round that ended between two tasks is reported at the first model call of the
    /// task that **continues** the first, once: the task after that is not told again, because
    /// `reported_rounds` is carried.
    pub async fn a_persons_round_is_reported_in_a_later_task(store: DynStore) {
        let rig = Rig::new();
        rig.child_mock.push_text("the person's round says 7");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        rig.parent_mock.push_text("first task");
        let first = rig.start(&rt, &conv).await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, first).await;
        let theirs = start_persons_round(&rt, &rig, &conv, "asked by a person").await;
        wait_done(&rt, theirs).await;

        let next = |prior: RunId, text: &'static str| {
            let rt = rt.clone();
            let (name, conv) = (rig.parent_name.clone(), conv.clone());
            async move {
                rt.start_continuing(&name, user_message(text), Some(&conv), prior)
                    .await
                    .unwrap()
            }
        };
        rig.parent_mock.push_text("second task");
        let second = next(first, "second").await;
        wait_done(&rt, second).await;
        rig.parent_mock.push_text("third task");
        let third = next(second, "third").await;
        wait_done(&rt, third).await;
        worker.stop().await;

        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(report_calls(&requests[0].messages).is_empty());
        let told = report_calls(&requests[1].messages);
        assert_eq!(told.len(), 1, "{:?}", requests[1].messages);
        assert_eq!(told[0].id, report_id(theirs));
        assert!(matches!(
            requests[1].messages.last(),
            Some(Message::Tool { content, .. })
                if content.contains("started by the person") && content.contains("round says 7")
        ));
        assert_eq!(
            report_calls(&requests[2].messages).len(),
            1,
            "the third task has the report in its history and is not told again"
        );
        assert_eq!(
            state_of(&rt, second).await.reported_rounds,
            vec![theirs],
            "told once"
        );
        assert_eq!(
            state_of(&rt, third).await.reported_rounds,
            vec![theirs],
            "carried to the next task"
        );
    }

    /// A round that only cancels what the round before left running (a closing round in a session
    /// that stays open) is the platform's, not the person's: it is not counted against
    /// `max_rounds`, the next `subagent_send` starts an ordinary round (not another cleanup), and a
    /// close that really closes is not taken for one.
    pub async fn a_cleanup_round_is_not_counted_and_the_next_send_is_an_ordinary_round(
        store: DynStore,
    ) {
        let mut rig = Rig::new();
        rig.limits = Some(SessionLimits::new(4, 2, 2).unwrap());
        rig.child_mock
            .push_text("round one says 1")
            .push_text("round three says 3");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        rig.parent_mock.push_text("first task");
        let first = rig.start(&rt, &conv).await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, first).await;
        let session = start_persons_round(&rt, &rig, &conv, "asked by a person").await;
        wait_done(&rt, session).await;

        let cleanup = RunId::new();
        let header = SessionHeader {
            id: session,
            agent: rig.child_name.clone(),
            description: "asked by a person".into(),
            keep_open: true,
            closed: false,
            closing: true,
            round: 2,
        };
        rt.start_child_continuing(
            cleanup,
            cleanup,
            &rig.child_name,
            Inbound::new("message", adam_runtime::closing_round_payload(&header)),
            &adam_runtime::session_conversation_id(&conv, session),
            Some(session),
        )
        .await
        .unwrap();
        wait_done(&rt, cleanup).await;
        let view = rt.session_view(&conv, session).await.unwrap().unwrap();
        assert_eq!((view.round, view.cleanup_rounds), (2, 1));

        // Two rounds are the most, and one of them is ours: the person's second is still allowed.
        rig.parent_mock
            .push_tool_calls(vec![send_call("s1", session, "more")])
            // The parent reads the round it started, so its turn has one model call more whether
            // the round ends before the next call or after it.
            .push_tool_calls(vec![read_call("r1", session, true)])
            .push_text("sent");
        let second = rt
            .start_continuing(&rig.parent_name, user_message("second"), Some(&conv), first)
            .await
            .unwrap();
        wait_done(&rt, second).await;
        let results = tool_results(&state_of(&rt, second).await);
        let (_, words, is_error) = result_of(&results, "s1");
        assert!(
            !is_error,
            "a cleanup round does not use the person's rounds: {words}"
        );
        let view = session_until(&rt, &conv, session, |v| {
            v.round == 3 && v.state == SessionState::Idle
        })
        .await;
        let latest = view.header.clone().expect("a header");
        assert!(
            !latest.closing,
            "the round after a cleanup is an ordinary round"
        );
        assert_eq!(view.cleanup_rounds, 1);

        // A close that closes is not a cleanup round.
        rig.parent_mock
            .push_tool_calls(vec![close_call("x1", session, false)])
            .push_text("closed");
        let third = rt
            .start_continuing(&rig.parent_name, user_message("third"), Some(&conv), second)
            .await
            .unwrap();
        wait_done(&rt, third).await;
        let view = session_until(&rt, &conv, session, |v| v.state == SessionState::Closed).await;
        assert_eq!(
            view.cleanup_rounds, 1,
            "a closing round that closes is not a cleanup"
        );
        worker.stop().await;
    }

    /// A task that does not continue another (a fresh `init`) does not tell the model of a
    /// person's round that ended before it began, however old: nothing records that it was told.
    /// It does tell one that ended after it began.
    pub async fn a_fresh_task_tells_only_the_persons_rounds_that_ended_after_it_began(
        store: DynStore,
    ) {
        let rig = Rig::new().with_hold();
        rig.child_mock
            .push_text("old round says 1")
            .push_text("child says 42")
            .push_text("new round says 7");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        rig.parent_mock.push_text("first task");
        let first = rig.start(&rt, &conv).await;
        let worker = spawn_worker(&rt);
        wait_done(&rt, first).await;
        let old = start_persons_round(&rt, &rig, &conv, "an old one").await;
        wait_done(&rt, old).await;
        // The second task must begin strictly after the old round ended: a round that ends in the
        // same instant as the chain starts is told (the rule is `ended >= chain_started`), and on
        // the memory store both can fall in one clock tick. Wait for the runtime's clock to pass
        // the round's last update; no fixed sleep, and no clock advance (the new round's end must
        // stay after the chain's start).
        let ended = store.load_run(old).await.unwrap().unwrap().updated_at;
        while adam_runtime::Clock::now(&rig.clock) <= ended {
            tokio::task::yield_now().await;
        }

        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("done");
        let second = rig.start(&rt, &conv).await;
        let mine = child_run_id(second, "c1");
        reached(&hold.reached, "the second task's other work").await;
        wait_done(&rt, mine).await;
        let new = start_persons_round(&rt, &rig, &conv, "a new one").await;
        wait_done(&rt, new).await;
        hold.release.notify_one();
        wait_done(&rt, second).await;
        worker.stop().await;

        let state = state_of(&rt, second).await;
        let told: Vec<String> = report_calls(&state.messages)
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert!(
            !told.contains(&report_id(old)),
            "the old round is not told: {told:?}"
        );
        assert!(
            told.contains(&report_id(new)),
            "{told:?}: new {new} mine {mine} old {old}"
        );
        assert!(told.contains(&report_id(mine)), "{told:?}");
        assert_eq!(told.len(), 2, "{told:?}");
    }

    /// A report the model read itself (a read that waited for it) is not appended again.
    pub async fn a_report_read_by_the_model_is_not_appended_again(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("read it myself");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        reached(&hold.reached, "the parent's other work").await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        assert!(
            report_calls(&state.messages).is_empty(),
            "{:?}",
            report_calls(&state.messages)
        );
        assert_eq!(state.reported_rounds, vec![session]);
        let results = tool_results(&state);
        assert!(result_of(&results, "c2").1.contains("child says 42"));
        assert_eq!(rig.parent_mock.requests().len(), 4);
    }

    /// A run that never opens a session asks the store for them at its first model call and no
    /// more: the later calls of its turns cost nothing.
    pub async fn a_run_without_sessions_lists_only_at_its_first_model_call(store: DynStore) {
        let rig = Rig::new().with_hold();
        let hold = rig.hold.clone().unwrap();
        let (inner, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&dynamic, &parent_agent, &child_agent);
        let worker = spawn_worker(&rt);

        rig.parent_mock.push_text("one call");
        let before = inner.calls(Method::CountRuns);
        let one = rig.start(&rt, &conv_id()).await;
        wait_done(&rt, one).await;
        let listing = inner.calls(Method::CountRuns) - before;
        assert!(listing > 0, "the first call looks");

        rig.parent_mock
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_tool_calls(vec![call("h2", "hold", json!({}))])
            .push_text("three calls");
        let before = inner.calls(Method::CountRuns);
        let three = rig.start(&rt, &conv_id()).await;
        for _ in 0..2 {
            reached(&hold.reached, "the parent's other work").await;
            hold.release.notify_one();
        }
        wait_done(&rt, three).await;
        worker.stop().await;
        assert_eq!(
            inner.calls(Method::CountRuns) - before,
            listing,
            "three model calls, one listing"
        );
        assert_eq!(rig.parent_mock.requests().len(), 4);
    }

    /// A report that was due and could not be read (the store failed) keeps the listing on: the next
    /// model call looks again and tells it, instead of leaving it for the end of the turn.
    pub async fn a_report_that_could_not_be_read_is_looked_at_again(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (inner, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&dynamic, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_tool_calls(vec![call("h2", "hold", json!({}))])
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's first other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        // The read of the report that the next model call makes fails once.
        inner.fail_run(Method::LoadRun, session, 1);
        hold.release.notify_one();
        reached(&hold.reached, "the parent's second other work").await;
        assert_eq!(inner.injected(Method::LoadRun), 1, "the read failed once");
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let state = state_of(&rt, parent).await;
        let told: Vec<String> = report_calls(&state.messages)
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(
            told,
            vec![report_id(session)],
            "told by the call that looked again"
        );
        assert!(
            !tool_results(&state)
                .iter()
                .any(|(id, _, _)| id.starts_with("wait_")),
            "it was not left for the turn end"
        );
        assert_eq!(rig.parent_mock.requests().len(), 4, "no extra call");
    }

    /// A report the model read itself without waiting (the round had ended) is not told again by
    /// the next model call, while another session keeps the listing on.
    pub async fn a_report_read_without_waiting_is_not_told_again(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            // The other work first, so that the round has ended when the read runs.
            .push_tool_calls(vec![
                call("h1", "hold", json!({})),
                read_call("c2", session, false),
            ])
            .push_text("read it myself");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        assert!(result_of(&results, "c2").1.contains("child says 42"));
        assert!(
            report_calls(&state.messages).is_empty(),
            "{:?}",
            report_calls(&state.messages)
        );
        assert_eq!(state.reported_rounds, vec![session]);
        assert_eq!(rig.parent_mock.requests().len(), 3);
    }

    /// A person's round that ends while the run's last model call is written is waited for, though
    /// the run never started a session: the listing of the first call saw it work.
    pub async fn a_persons_round_that_ends_during_the_final_model_call_is_reported(
        store: DynStore,
    ) {
        let (rig, parent_gate) = Rig::new().holding(0).holding_parent(0);
        rig.child_mock.push_text("the person's round says 7");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let conv = conv_id();
        let theirs = start_persons_round(&rt, &rig, &conv, "a private thing").await;
        let parent = rig.start(&rt, &conv).await;
        rig.parent_mock
            .push_text("nothing to do")
            .push_text("the person's round said 7");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the person's round").await;
        reached(&parent_gate.reached, "the parent's only model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, theirs).await;
        parent_gate.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;

        assert_eq!(
            done.output.expect("output")["text"],
            "the person's round said 7"
        );
        let state = state_of(&rt, parent).await;
        let results = tool_results(&state);
        let report = &result_of(&results, "wait_0").1;
        assert!(report.contains("started by the person"), "{report}");
        assert!(report.contains("the person's round says 7"), "{report}");
        assert_eq!(state.reported_rounds, vec![theirs]);
        assert_eq!(rig.parent_mock.requests().len(), 2);
    }

    /// A recorded answer that predates a message the run has read is not the last word, nor is its
    /// listing: the reports it was given are left out of the history (the next call finds them
    /// again) and the loop keeps looking, even though the record's listing saw every session closed.
    pub async fn a_stale_answer_leaves_its_reports_and_its_listing_for_the_next_call(
        store: DynStore,
    ) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            // The call that is told the report also calls a tool: the answer is kept when stale.
            .push_tool_calls(vec![call("h2", "hold", json!({}))])
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's first other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        hold.release.notify_one();
        reached(&hold.reached, "the parent's second other work").await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let original = state_of(&rt, parent).await;
        assert_eq!(report_calls(&original.messages).len(), 1);

        let again = Rig::new().with_hold();
        again.parent_mock.push_text("answered again");
        let (inner, dynamic) = faulty(store.clone());
        let (parent_agent, child_agent) = (again.parent(), again.child());
        let again_rt = again.together(&dynamic, &parent_agent, &child_agent);
        let copy = again.start(&again_rt, &conv_id()).await;
        let mut seq = 0;
        // What a worker that crashed left: the journal up to the last model call.
        while let Some(entry) = store.journal_get(parent, seq).await.unwrap() {
            if entry.name == "model:3" {
                break;
            }
            store.journal_put(copy, entry).await.unwrap();
            seq += 1;
        }
        again_rt
            .deliver(copy, user_message("one more thing").with_id("steer-1"))
            .await
            .unwrap();
        let worker = spawn_worker(&again_rt);
        let done = wait_for(&again_rt, copy, "the replay to end", |v| {
            v.status.is_terminal()
        })
        .await;
        worker.stop().await;
        assert_eq!(done.status, RunStatus::Done, "{done:?}");
        let replayed = conversation(&done);
        assert!(
            report_calls(&replayed.messages).is_empty(),
            "the stale answer's reports were not appended: {:?}",
            report_calls(&replayed.messages)
        );
        // The call after it listed (the record said every session was closed, and the loop did not
        // believe a record it left half applied): its listing and the one of the turn's end.
        assert_eq!(inner.calls(Method::CountRuns), 6);
    }

    /// A send starts a round that may end: it turns the listing on, even when the listing before it
    /// failed and left the loop not knowing of the session.
    pub async fn a_send_turns_the_listing_on(store: DynStore) {
        let rig = Rig::new().with_hold();
        rig.child_mock
            .push_text("the first round says 1")
            .push_text("the second round says 2");
        let hold = rig.hold.clone().unwrap();
        let (inner, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&dynamic, &parent_agent, &child_agent);
        let conv = conv_id();
        let worker = spawn_worker(&rt);
        let theirs = start_persons_round(&rt, &rig, &conv, "a first thing").await;
        wait_done(&rt, theirs).await;

        // The listing of the first model call fails: the loop does not know of the session.
        inner.fail(Method::CountRuns, 1);
        rig.parent_mock
            .push_tool_calls(vec![send_call("s1", theirs, "and now this")])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("the second round said 2");
        let parent = rig.start(&rt, &conv).await;
        reached(&hold.reached, "the parent's other work").await;
        assert_eq!(inner.injected(Method::CountRuns), 1);
        let round = child_run_id(parent, "s1");
        wait_done(&rt, round).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;

        let state = state_of(&rt, parent).await;
        let told: Vec<String> = report_calls(&state.messages)
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(
            told,
            vec![report_id(round)],
            "told by the call after the work"
        );
        assert!(
            !tool_results(&state)
                .iter()
                .any(|(id, _, _)| id.starts_with("wait_")),
            "it was not left for the turn end"
        );
    }

    /// The report of a session that was closed is the newest round that worked, not the first: two
    /// rounds, and a closing one after them.
    pub async fn a_closing_round_does_not_hide_the_newest_round_that_worked(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock
            .push_text("answer one")
            .push_text("answer two");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        let second = child_run_id(parent, "c3");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", true)])
            .push_tool_calls(vec![read_call("c2", session, true)])
            .push_tool_calls(vec![send_call("c3", session, "and now this")])
            // The second round ends during the hold, and the call after it closes the idle session.
            .push_tool_calls(vec![
                call("h1", "hold", json!({})),
                close_call("x1", session, false),
            ])
            .push_text("closed it")
            .push_text("the helper said two");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's first model call").await;
        rig.child_gate.release.notify_one();
        reached(&hold.reached, "the parent's other work").await;
        wait_done(&rt, second).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let state = state_of(&rt, parent).await;
        let told: Vec<String> = tool_results(&state)
            .iter()
            .filter(|(id, text, _)| {
                (id.starts_with("report_") || id.starts_with("wait_"))
                    && text.contains("answer two")
            })
            .map(|(id, _, _)| id.clone())
            .collect();
        assert_eq!(told.len(), 1, "{:?}", tool_results(&state));
        assert_eq!(state.reported_rounds, vec![session, second]);
    }

    /// A journal written before reports existed (no listing in its model steps) replays to the
    /// same history under a build that has them, with no model asked and no disagreement.
    pub async fn a_journal_from_before_reports_replays_to_the_same_history(store: DynStore) {
        let rig = Rig::new().holding(0).without_reports();
        rig.child_mock.push_text("child says 42");
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_text("started it")
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&rig.child_gate.reached, "the child's model call").await;
        wait_waiting_on_a_session(&rt, parent).await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let original = state_of(&rt, parent).await;
        assert!(!original.sessions_open, "the old build listed nothing");

        let again = Rig::new();
        let (parent_agent, child_agent) = (again.parent(), again.child());
        let again_rt = again.together(&store, &parent_agent, &child_agent);
        let copy = again.start(&again_rt, &conv_id()).await;
        let mut seq = 0;
        while let Some(entry) = store.journal_get(parent, seq).await.unwrap() {
            store.journal_put(copy, entry).await.unwrap();
            seq += 1;
        }
        assert!(seq > 4, "the journal has the steps of the run: {seq}");
        let worker = spawn_worker(&again_rt);
        let clock = again.clock.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                clock.advance(Duration::from_secs(61));
            }
        });
        wait_done(&again_rt, copy).await;
        ticker.abort();
        worker.stop().await;
        let replayed = state_of(&again_rt, copy).await;
        let text = |c: &Conversation, run: RunId| {
            serde_json::to_string(&c.messages)
                .unwrap()
                .replace(&run.to_string(), "<run>")
        };
        assert_eq!(text(&replayed, copy), text(&original, parent));
        assert_eq!(again.parent_mock.requests().len(), 0, "no model was asked");
        assert_eq!(again.child_mock.requests().len(), 0, "no child ran");
    }

    /// The journal of a run that was told a report in the middle of a turn replays to the same
    /// history: the same report, under the same call id, with no model and no child.
    pub async fn a_replay_appends_the_same_reports(store: DynStore) {
        let rig = Rig::new().with_hold().holding(0);
        rig.child_mock.push_text("child says 42");
        let hold = rig.hold.clone().unwrap();
        let (parent_agent, child_agent) = (rig.parent(), rig.child());
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rig.start(&rt, &conv_id()).await;
        let session = child_run_id(parent, "c1");
        rig.parent_mock
            .push_tool_calls(vec![start_call("c1", false)])
            .push_tool_calls(vec![call("h1", "hold", json!({}))])
            .push_text("the helper said 42");
        let worker = spawn_worker(&rt);
        reached(&hold.reached, "the parent's other work").await;
        reached(&rig.child_gate.reached, "the child's model call").await;
        rig.child_gate.release.notify_one();
        wait_done(&rt, session).await;
        hold.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        let original = state_of(&rt, parent).await;
        assert_eq!(report_calls(&original.messages).len(), 1);

        let again = Rig::new().with_hold();
        let (parent_agent, child_agent) = (again.parent(), again.child());
        let again_rt = again.together(&store, &parent_agent, &child_agent);
        let copy = again.start(&again_rt, &conv_id()).await;
        let mut seq = 0;
        while let Some(entry) = store.journal_get(parent, seq).await.unwrap() {
            store.journal_put(copy, entry).await.unwrap();
            seq += 1;
        }
        let worker = spawn_worker(&again_rt);
        wait_done(&again_rt, copy).await;
        worker.stop().await;
        let replayed = state_of(&again_rt, copy).await;
        let text = |c: &Conversation, run: RunId| {
            serde_json::to_string(&c.messages)
                .unwrap()
                .replace(&run.to_string(), "<run>")
        };
        assert_eq!(text(&replayed, copy), text(&original, parent));
        assert_eq!(replayed.reported_rounds, original.reported_rounds);
        assert_eq!(again.parent_mock.requests().len(), 0, "no model was asked");
        assert_eq!(again.child_mock.requests().len(), 0, "no child ran");
    }
}

#[test]
fn limits_do_not_change_the_specs() {
    let agents = || {
        [SessionAgent {
            name: "helper".into(),
            agent: "helper".into(),
            description: "Does a piece of work.".into(),
            within: None,
            remote: false,
        }]
    };
    let specs = |tools: SessionTools| {
        serde_json::to_string(&tools.tools().iter().map(|t| t.spec()).collect::<Vec<_>>()).unwrap()
    };
    let default = specs(SessionTools::new(agents()));
    let limited = SessionLimits::new(3, 2, 5).unwrap();
    assert_eq!(
        default,
        specs(SessionTools::new(agents()).with_limits(limited))
    );
}

/// The five specs, as the model sees them, pinned: a spec is the model's contract, so a change is
/// deliberate. Regenerate with `ADAM_UPDATE_SNAPSHOTS=1 cargo test -p adam-llm-agent --test sessions`.
#[test]
fn the_five_specs_are_pinned() {
    let roster = SessionTools::new([
        SessionAgent {
            name: "reviewer".into(),
            agent: "coder/reviewer".into(),
            description: "Reviews a diff.".into(),
            within: None,
            remote: false,
        },
        SessionAgent {
            name: "explorer".into(),
            agent: "coder/explorer".into(),
            description: "Reads the repository and answers where and how.".into(),
            within: None,
            remote: false,
        },
    ]);
    let specs: Vec<adam_model::ToolSpec> = roster.tools().iter().map(|t| t.spec()).collect();
    let mut text = serde_json::to_string_pretty(&specs).unwrap();
    text.push('\n');
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session-tools.json");
    if std::env::var_os("ADAM_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let pinned = std::fs::read_to_string(&path).expect("tests/fixtures/session-tools.json");
    assert_eq!(
        text, pinned,
        "the session tools' specs changed: review the diff, then regenerate"
    );
}

// ---------------------------------------------------------------------------
// Instantiate every case per store
// ---------------------------------------------------------------------------

macro_rules! session_suite {
    ($module:ident, $make:path) => {
        mod $module {
            session_suite!(@cases $make;
                a_start_returns_at_once_and_the_turn_end_waits_for_the_report,
                a_session_report_keeps_the_prefix,
                the_start_result_survives_a_history_far_over_budget,
                a_read_without_wait_answers_working_and_parks_nothing,
                a_read_with_wait_parks_and_resumes_once_with_the_report,
                a_session_closes_by_default_and_refuses_a_send,
                a_kept_open_session_continues_in_round_two,
                a_message_to_a_working_round_is_steered,
                closing_an_idle_session_takes_a_round_that_calls_no_model,
                closing_a_working_session_lets_the_round_finish,
                closing_with_cancel_stops_the_round,
                the_list_orders_open_sessions_first_and_pages,
                a_session_that_is_not_ours_is_refused_in_words,
                a_remote_subagent_starts_a_round_of_its_registered_agent,
                a_send_to_a_round_that_queues_says_queued,
                a_replayed_send_to_a_round_that_queues_says_queued,
                a_retried_try_starts_nothing_twice,
                a_closed_session_reports_the_round_that_worked,
                a_run_that_cannot_wait_cancels_its_rounds,
                a_close_request_survives_a_cancel,
                the_caps_refuse_in_words,
                the_round_cap_refuses_a_send_and_not_a_close,
                every_round_inherits_the_protected_context,
                reads_page_and_failures_read_as_failed,
                a_failed_try_later_in_the_turn_starts_no_second_session,
                every_step_replays_from_the_journal_to_the_same_history,
                a_lost_notice_is_recovered_by_the_timer,
                a_duplicate_notice_is_ignored,
                cancelling_the_parent_leaves_a_kept_open_session_idle,
                polling_is_refused_and_waiting_is_not,
                the_specs_are_byte_stable,
                a_report_is_counted_when_it_is_read,
                cancelling_the_parent_closes_a_session_that_was_not_kept_open,
                a_blipped_start_of_the_last_session_still_answers_started,
                a_round_past_its_deadline_reads_as_failed_with_the_reason,
                a_purged_session_wakes_a_parked_parent_with_an_error,
                a_round_that_only_closes_a_session_is_neither_waited_for_nor_cancelled,
                the_turn_end_wait_is_for_this_tasks_rounds_only,
                a_task_that_only_sends_waits_for_its_round,
                two_waiting_reads_in_one_turn_each_get_their_own_report,
                a_steer_retried_after_the_round_ended_reaches_the_child_once,
                the_ids_the_loop_invents_are_ones_every_provider_accepts,
                a_report_read_twice_counts_its_tokens_once,
                a_round_never_read_is_counted_when_its_session_closes,
                a_round_never_read_is_counted_when_the_parent_ends,
                a_store_error_on_the_usage_read_is_retried,
                a_replay_counts_the_same_tokens,
                a_report_that_ends_mid_turn_reaches_the_next_model_call,
                reports_during_a_turn_keep_the_prefix,
                a_report_during_the_final_model_call_reopens_the_turn,
                a_report_is_found_in_the_store_when_its_notice_is_lost,
                a_lost_notice_while_the_parent_is_parked_costs_one_turn,
                a_closing_round_does_not_hide_the_report_of_the_round_before,
                a_replay_of_a_stale_answer_counts_each_round_once,
                a_fresh_task_tells_only_the_persons_rounds_that_ended_after_it_began,
                a_duplicate_report_notice_matches_nothing,
                a_persons_round_is_reported_as_the_persons,
                a_persons_round_is_reported_in_a_later_task,
                a_report_read_by_the_model_is_not_appended_again,
                a_run_without_sessions_lists_only_at_its_first_model_call,
                a_journal_from_before_reports_replays_to_the_same_history,
                a_replay_appends_the_same_reports,
                a_report_that_could_not_be_read_is_looked_at_again,
                a_report_read_without_waiting_is_not_told_again,
                a_persons_round_that_ends_during_the_final_model_call_is_reported,
                a_stale_answer_leaves_its_reports_and_its_listing_for_the_next_call,
                a_send_turns_the_listing_on,
                a_closing_round_does_not_hide_the_newest_round_that_worked,
                each_limit_refuses_in_words,
                a_close_then_a_start_is_not_refused_by_the_closing_round,
                a_close_then_a_send_is_not_refused_by_the_closing_round,
                an_idle_expired_session_refuses_a_send_as_closed,
                an_expired_sessions_report_is_still_told_once,
                a_parent_that_fails_stops_its_rounds,
                a_cancel_frees_the_working_slots_for_the_next_task,
                a_cleanup_round_is_not_counted_and_the_next_send_is_an_ordinary_round,
            );
        }
    };
    (@cases $make:path; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                let Some(store) = $make().await else {
                    return;
                };
                super::cases::$case(store).await;
            }
        )+
    };
}

session_suite!(memory, super::memory_store);
session_suite!(postgres, super::postgres_store);
session_suite!(mongodb, super::mongodb_store);
