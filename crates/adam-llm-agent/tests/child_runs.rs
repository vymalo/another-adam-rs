//! Child runs, seen from an `LlmAgent` parent: a tool starts a child run and returns
//! `ToolError::AwaitRun`, the parent parks, and the child's outcome becomes the tool result.
//!
//! Run against `MemoryStore` always, against PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set and
//! against MongoDB when `ADAM_TEST_MONGODB_URI` is set. Nothing here sleeps for a fixed time:
//! steps are held at gates, the 60 s wait timer is moved with a `ManualClock`, and the few polls
//! wait for a state with a deadline.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, NewRun, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, LlmAgentBuilder, LlmStarter, PendingRun, PendingWait, Tool, ToolCtx,
    ToolError, ToolOutput, user_message,
};
use adam_model::{
    DynModel, Message, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse,
    ToolCall, ToolSpec,
};
use adam_runtime::{
    Clock, CollectingSink, Inbound, ManualClock, RUN_FINISHED_KIND, RetryPolicy, RunEvent, RunView,
    Runtime, RuntimeBuilder, RuntimeError, StepEvent, StepKind, StepState, child_run_id,
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
        parameters: json!({"type": "object", "properties": {"message": {"type": "string"}}}),
    }
}

type RtCell = Arc<OnceLock<Runtime>>;

/// A gate a step waits at until the test lets it go.
#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

/// The parent's "subagent" tool: starts a child run under the id derived from the call, then
/// returns `AwaitRun`. This is what the S9 `SubagentTool` will do.
struct SpawnTool {
    child_agent: String,
    rt: RtCell,
    /// Times the tool body ran (a replayed journal entry does not run it).
    calls: Arc<AtomicUsize>,
    /// After starting the child, stop here on the first attempt.
    hold: Option<Arc<Gate>>,
    /// After starting the child (and the gate), fail the first attempt transiently.
    blip: bool,
}

#[async_trait]
impl Tool for SpawnTool {
    fn spec(&self) -> ToolSpec {
        spec("sub")
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        let rt = self
            .rt
            .get()
            .ok_or_else(|| ToolError::Permanent("no runtime".into()))?;
        let message = args["message"].as_str().unwrap_or("go").to_owned();
        let child = ctx.child_run_id();
        rt.start_child(
            ctx.run_id(),
            child,
            &self.child_agent,
            user_message(message),
        )
        .await
        .map_err(|e| ToolError::from_classified(&e))?;
        if ctx.attempt() == 0
            && let Some(gate) = &self.hold
        {
            gate.reached.notify_one();
            gate.release.notified().await;
        }
        if self.blip && ctx.attempt() == 0 {
            return Err(ToolError::Transient("blip".into()));
        }
        Err(ToolError::AwaitRun { run: child })
    }
}

/// Wraps a model: the `hold_on`-th call (0-based) waits at a gate, the first time, before it is
/// answered. That is a worker stuck in the middle of a step.
struct GateModel {
    inner: Arc<MockModel>,
    hold_on: usize,
    calls: AtomicUsize,
    armed: AtomicBool,
    gate: Arc<Gate>,
}

impl GateModel {
    /// Holds the call this model was told to hold, until the gate is released.
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

    // The agent streams its model calls: the gate holds those too.
    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.hold().await;
        self.inner.stream(req).await
    }
}

/// Stops the child's model turn until released (a tool that waits).
struct GateTool {
    gate: Arc<Gate>,
}

#[async_trait]
impl Tool for GateTool {
    fn spec(&self) -> ToolSpec {
        spec("gate")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.gate.reached.notify_one();
        self.gate.release.notified().await;
        Ok(ToolOutput::text("released"))
    }
}

/// A tool that always asks the user.
struct AskTool;

#[async_trait]
impl Tool for AskTool {
    fn spec(&self) -> ToolSpec {
        spec("ask")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input("which environment?"))
    }
}

/// A parent, its child, and everything a case needs to watch them.
struct Rig {
    parent_name: String,
    child_name: String,
    parent_mock: Arc<MockModel>,
    child_mock: Arc<MockModel>,
    cell: RtCell,
    spawned: Arc<AtomicUsize>,
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
            cell: RtCell::default(),
            spawned: Arc::default(),
            sink: CollectingSink::new(),
            clock: ManualClock::new(),
        }
    }

    /// The model asks the child once (call id `c1`), then answers with `text`.
    fn parent_script(&self, text: &str) {
        self.parent_mock
            .push_tool_calls(vec![call("c1", "sub", json!({"message": "review it"}))])
            .push_text(text);
    }

    fn spawn_tool(&self, hold: Option<Arc<Gate>>, blip: bool) -> SpawnTool {
        SpawnTool {
            child_agent: self.child_name.clone(),
            rt: self.cell.clone(),
            calls: self.spawned.clone(),
            hold,
            blip,
        }
    }

    fn parent_with(
        &self,
        tool: SpawnTool,
        f: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> LlmAgent {
        let model: DynModel = self.parent_mock.clone();
        f(LlmAgent::builder(&self.parent_name, model, "m").tool(tool)).build()
    }

    fn parent(&self) -> LlmAgent {
        self.parent_with(self.spawn_tool(None, false), |b| b)
    }

    fn child_with(&self, f: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder) -> LlmAgent {
        let model: DynModel = self.child_mock.clone();
        f(LlmAgent::builder(&self.child_name, model, "m")).build()
    }

    /// A child that answers `text` at once.
    fn child(&self, text: &str) -> LlmAgent {
        self.child_mock.push_text(text);
        self.child_with(|b| b)
    }

    fn builder(&self, store: &DynStore, worker: &str) -> RuntimeBuilder {
        Runtime::builder(store.clone())
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

    /// One runtime for both.
    fn together(&self, store: &DynStore, parent: &LlmAgent, child: &LlmAgent) -> Runtime {
        let rt = self
            .builder(store, "w")
            .agent(parent.clone())
            .agent(child.clone())
            .build();
        self.cell.set(rt.clone()).ok().expect("set once");
        rt
    }

    /// The runtime that runs the parent and can start the child but does not step it.
    fn front(&self, store: &DynStore, parent: &LlmAgent) -> Runtime {
        let rt = self
            .builder(store, "front")
            .agent(parent.clone())
            .starter(LlmStarter::new(&self.child_name))
            .build();
        self.cell.set(rt.clone()).ok().expect("set once");
        rt
    }

    /// The runtime that steps the child.
    fn back(&self, store: &DynStore, child: &LlmAgent) -> Runtime {
        self.builder(store, "back").agent(child.clone()).build()
    }

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

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "done", |v| v.status == RunStatus::Done).await
}

/// Parked on the wait timer (not on a question).
async fn wait_parked_on_timer(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "parked on the wait timer", |v| {
        v.status == RunStatus::Parked && v.wake_at.is_some()
    })
    .await
}

async fn wait_injected(faulty: &FaultyStore, method: Method, at_least: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while faulty.injected(method) < at_least {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
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

/// The report that the call `id` of the tool `sub` is in the state `status` names: `waiting`, `ok`
/// (completed) or `error` (failed).
fn tool_end(id: &str, status: &str) -> RunEvent {
    let state = match status {
        "waiting" => StepState::Waiting,
        "ok" => StepState::Completed,
        "error" => StepState::Failed,
        other => panic!("no such status {other}"),
    };
    RunEvent::Step(StepEvent::new(
        format!("tool:{id}"),
        StepKind::Tool,
        "sub",
        state,
    ))
}

/// The tool results the parent's history holds, in order.
fn tool_results(state: &Conversation) -> Vec<(String, String, bool)> {
    state
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                call_id,
                content,
                is_error,
            } => Some((call_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// The notice a finished child sends its parent, written out by hand.
fn notice(child: RunId, payload: Value) -> Inbound {
    Inbound::new(RUN_FINISHED_KIND, payload).with_id(child.to_string())
}

fn child_said(text: &str) -> Value {
    json!({"status": "done", "output": {"text": text, "artifacts": []}})
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

mod cases {
    use super::*;

    /// The child finishes and the parent resumes exactly once, with the child's answer as the tool
    /// result. The wait timer is an hour of a clock nobody moves: the message did it.
    pub async fn the_childs_answer_becomes_the_tool_result(store: DynStore) {
        let rig = Rig::new();
        rig.parent_script("the reviewer approved");
        let (parent_agent, child_agent) = (rig.parent(), rig.child("child says 42"));
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, parent).await;
        worker.stop().await;

        assert_eq!(
            done.output.expect("output")["text"],
            "the reviewer approved"
        );
        let state = conversation(&rt.view(parent).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "child says 42".into(), false)]
        );
        assert_eq!(state.pending_wait, None);
        assert!(state.pending_calls.is_empty());
        // The model saw the answer, once: two model calls, the second after the tool result.
        let requests = rig.parent_mock.requests();
        assert_eq!(requests.len(), 2, "the parent's model was asked twice");
        assert_eq!(
            requests[1].messages.last(),
            Some(&Message::tool_result("c1", "child says 42"))
        );
        assert_eq!(rig.spawned.load(SeqCst), 1, "the tool ran once");
        assert_eq!(rig.child_mock.requests().len(), 1, "one child ran");
        let child = child_run_id(parent, "c1");
        assert_eq!(
            rig.child_mock.requests()[0].messages,
            vec![Message::user_text("review it")]
        );
        let record = store.load_run(child).await.unwrap().unwrap();
        assert_eq!(record.parent_id, Some(parent));
        assert_eq!(record.status, RunStatus::Done);
        // What the run announced.
        let events = rig.events(parent);
        assert!(events.contains(&custom(
            "awaiting_run",
            json!({"call_id": "c1", "run": child.to_string()})
        )));
        let waiting = events.iter().position(|e| *e == tool_end("c1", "waiting"));
        let finished = events.iter().position(|e| *e == tool_end("c1", "ok"));
        assert!(waiting < finished && finished.is_some(), "{events:#?}");
    }

    /// While the child works the run is parked on its timer, which A2A reads as `working`, and a
    /// user message that arrives meanwhile queues behind the owed tool result.
    pub async fn a_message_while_waiting_queues_behind_the_result(store: DynStore) {
        let rig = Rig::new();
        let gate = Arc::new(Gate::default());
        rig.child_mock
            .push_tool_calls(vec![call("g1", "gate", json!({}))])
            .push_text("late answer");
        rig.parent_script("thanks");
        let parent_agent = rig.parent();
        let child_agent = rig.child_with(|b| b.tool(GateTool { gate: gate.clone() }));
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        notified(&gate.reached, "the child's tool").await;
        let parked = wait_parked_on_timer(&rt, parent).await;
        assert!(!parked.waiting, "waiting on a child is not input-required");
        assert!(matches!(
            conversation(&parked).pending_wait,
            Some(PendingWait::Run(PendingRun { ref call_id, ref tool, .. })) if call_id == "c1" && tool == "sub"
        ));

        rt.deliver(parent, user_message("and be quick"))
            .await
            .expect("deliver");
        // The parent wakes, looks, finds the child busy and parks again with the message queued.
        wait_for(&rt, parent, "the message absorbed", |v| {
            v.pending_inbox == 0 && conversation(v).deferred.len() == 1
        })
        .await;
        gate.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;

        let messages = conversation(&done).messages;
        assert_eq!(messages[2], Message::tool_result("c1", "late answer"));
        assert_eq!(messages[3], Message::user_text("and be quick"));
        assert_eq!(rig.parent_mock.requests().len(), 2);
    }

    /// A duplicate of the message is dropped, whether it arrives in the same batch, after the
    /// result is in but the run goes on, or after the run is over.
    pub async fn a_duplicate_notice_is_ignored(store: DynStore) {
        let rig = Rig::new();
        let slow = Arc::new(Gate::default());
        // Two calls in one turn: the child, then a tool that waits, so the run stays open after
        // the child's answer is in.
        rig.parent_mock
            .push_tool_calls(vec![
                call("c1", "sub", json!({"message": "review it"})),
                call("c2", "gate", json!({})),
            ])
            .push_text("all done");
        let parent_agent = rig.parent_with(rig.spawn_tool(None, false), |b| {
            b.tool(GateTool { gate: slow.clone() })
        });
        let child_agent = rig.child("child says 42");
        let front = rig.front(&store, &parent_agent);
        let back = rig.back(&store, &child_agent);
        let parent = front
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let child = child_run_id(parent, "c1");

        let front_worker = spawn_worker(&front);
        wait_parked_on_timer(&front, parent).await;
        front_worker.stop().await;

        // The child finishes and tells the parent; then two more copies arrive before the
        // parent looks, so its inbox holds the message three times.
        let back_worker = spawn_worker(&back);
        wait_done(&back, child).await;
        back_worker.stop().await;
        let copy = notice(child, child_said("child says 42"));
        front.deliver(parent, copy.clone()).await.expect("dup 1");
        front.deliver(parent, copy.clone()).await.expect("dup 2");
        assert_eq!(front.view(parent).await.unwrap().unwrap().pending_inbox, 3);

        let front_worker = spawn_worker(&front);
        notified(&slow.reached, "the parent to reach its second tool").await;
        // The run is open and busy: a late copy lands in its inbox now.
        front.deliver(parent, copy.clone()).await.expect("dup 3");
        slow.release.notify_one();
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;

        let state = conversation(&done.clone());
        assert_eq!(
            tool_results(&state),
            vec![
                ("c1".into(), "child says 42".into(), false),
                ("c2".into(), "released".into(), false),
            ],
            "one result per call, however many messages"
        );
        assert_eq!(
            rig.parent_mock.requests().len(),
            2,
            "the model is not asked again"
        );
        assert_eq!(done.pending_inbox, 0);
        // After the run is over a copy is refused, and that is all that happens.
        let late = front.deliver(parent, copy).await;
        assert!(
            matches!(late, Err(RuntimeError::Finished { .. })),
            "{late:?}"
        );
    }

    /// The child's process dies between its commit and the message: the parent's commit fails when
    /// the message is delivered. The timer brings the parent back, and it reads the answer.
    pub async fn a_lost_notice_is_recovered_when_the_timer_fires(store: DynStore) {
        let rig = Rig::new();
        rig.parent_script("recovered");
        let (faulty, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child("child says 42"));
        let front = rig.front(&dynamic, &parent_agent);
        let back = rig.back(&dynamic, &child_agent);
        let parent = front
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let child = child_run_id(parent, "c1");
        let front_worker = spawn_worker(&front);
        let parked = wait_parked_on_timer(&front, parent).await;

        // The message is the parent's next commit; it fails once.
        faulty.fail_run(Method::CommitRun, parent, 1);
        let back_worker = spawn_worker(&back);
        wait_done(&back, child).await;
        wait_injected(&faulty, Method::CommitRun, 1, "the message to fail").await;
        back_worker.stop().await;

        let still = front.view(parent).await.unwrap().unwrap();
        assert_eq!(
            (still.status, still.pending_inbox, still.version),
            (RunStatus::Parked, 0, parked.version),
            "the parent knows nothing"
        );
        assert_eq!(rig.parent_mock.requests().len(), 1);

        // The timer is 60 s away on the manual clock. Move it.
        rig.clock.advance(Duration::from_secs(61));
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "recovered");
        let state = conversation(&front.view(parent).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "child says 42".into(), false)]
        );
        assert_eq!(rig.parent_mock.requests().len(), 2);
        assert_eq!(rig.spawned.load(SeqCst), 1);
    }

    /// The other way to lose the message: the child's terminal commit is applied but its
    /// acknowledgement is lost, the worker treats it as failed and sends nothing.
    pub async fn a_lost_terminal_ack_is_recovered_when_the_timer_fires(store: DynStore) {
        let rig = Rig::new();
        rig.parent_script("recovered");
        let (faulty, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child("child says 42"));
        let front = rig.front(&dynamic, &parent_agent);
        let back = rig.back(&dynamic, &child_agent);
        let parent = front
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let child = child_run_id(parent, "c1");
        let front_worker = spawn_worker(&front);
        wait_parked_on_timer(&front, parent).await;

        // The child's only transition is its terminal one (a model turn without tool calls),
        // so its first commit is the one whose acknowledgement is lost.
        faulty.fail_run_after_apply(Method::CommitRun, child, 1);
        let back_worker = spawn_worker(&back);
        wait_injected(&faulty, Method::CommitRun, 1, "the child's ack to be lost").await;
        wait_done(&back, child).await;
        back_worker.stop().await;
        let still = front.view(parent).await.unwrap().unwrap();
        assert_eq!((still.status, still.pending_inbox), (RunStatus::Parked, 0));

        rig.clock.advance(Duration::from_secs(61));
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "recovered");
        let state = conversation(&front.view(parent).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "child says 42".into(), false)]
        );
    }

    /// A failing child is an error result: the model sees why and the run goes on.
    pub async fn a_failing_child_is_an_error_result(store: DynStore) {
        let rig = Rig::new();
        rig.parent_script("I will fix it myself");
        rig.child_mock
            .push_error(ModelError::Auth("bad key".into()));
        let (parent_agent, child_agent) = (rig.parent(), rig.child_with(|b| b));
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, parent).await;
        worker.stop().await;

        assert_eq!(done.output.expect("output")["text"], "I will fix it myself");
        let results = tool_results(&conversation(&rt.view(parent).await.unwrap().unwrap()));
        assert_eq!(results.len(), 1);
        let (id, content, is_error) = &results[0];
        assert_eq!(id, "c1");
        assert!(is_error, "the result is marked as an error");
        assert!(
            content.starts_with("the run failed: model call failed"),
            "{content}"
        );
        assert!(content.contains("bad key"), "{content}");
        assert!(rig.events(parent).contains(&tool_end("c1", "error")));
    }

    /// A child that is cancelled from outside tells the parent, which reports it to the model.
    pub async fn a_cancelled_child_is_an_error_result(store: DynStore) {
        let rig = Rig::new();
        rig.parent_script("ok, no review");
        rig.child_mock
            .push_tool_calls(vec![call("q1", "ask", json!({}))]);
        let (parent_agent, child_agent) = (rig.parent(), rig.child_with(|b| b.tool(AskTool)));
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let child = child_run_id(parent, "c1");
        wait_parked_on_timer(&rt, parent).await;
        wait_for(&rt, child, "the child asking", |v| v.waiting).await;

        rt.cancel(child, "not needed").await.expect("cancel");
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(done.output.expect("output")["text"], "ok, no review");
        assert_eq!(
            tool_results(&conversation(&rt.view(parent).await.unwrap().unwrap())),
            vec![(
                "c1".into(),
                "the run failed: cancelled: not needed".into(),
                true
            )]
        );
    }

    /// The parent is cancelled while it waits: the child is not (v1 has no cascade), it finishes
    /// on its own, and its message to the finished parent changes nothing.
    pub async fn cancelling_the_parent_leaves_the_child_running(store: DynStore) {
        let rig = Rig::new();
        let gate = Arc::new(Gate::default());
        rig.parent_script("never asked twice");
        rig.child_mock
            .push_tool_calls(vec![call("g1", "gate", json!({}))])
            .push_text("nobody listens");
        let parent_agent = rig.parent();
        let child_agent = rig.child_with(|b| b.tool(GateTool { gate: gate.clone() }));
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let child = child_run_id(parent, "c1");
        notified(&gate.reached, "the child's tool").await;
        wait_parked_on_timer(&rt, parent).await;

        rt.cancel(parent, "changed my mind").await.expect("cancel");
        let cancelled = rt.view(parent).await.unwrap().unwrap();
        assert_eq!(
            cancelled.error.as_deref(),
            Some("cancelled: changed my mind")
        );
        assert!(rt.view(child).await.unwrap().unwrap().status.is_open());

        gate.release.notify_one();
        let child_done = wait_done(&rt, child).await;
        worker.stop().await;
        assert_eq!(child_done.output.expect("output")["text"], "nobody listens");
        let after = rt.view(parent).await.unwrap().unwrap();
        assert_eq!(after.status, RunStatus::Failed);
        assert_eq!(
            after.version, cancelled.version,
            "the parent was not touched"
        );
        assert_eq!(rig.parent_mock.requests().len(), 1, "and never asked again");
    }

    /// Only the child the run waits for can answer it: a message for another run, and one that is
    /// not a well-formed finished message at all, are not the answer and not user text either.
    pub async fn only_the_awaited_childs_notice_answers(store: DynStore) {
        let rig = Rig::new();
        let gate = Arc::new(Gate::default());
        rig.parent_script("patient");
        rig.child_mock
            .push_tool_calls(vec![call("g1", "gate", json!({}))])
            .push_text("the real answer");
        let parent_agent = rig.parent();
        let child_agent = rig.child_with(|b| b.tool(GateTool { gate: gate.clone() }));
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        notified(&gate.reached, "the child's tool").await;
        wait_parked_on_timer(&rt, parent).await;

        let stranger = notice(RunId::new(), child_said("forged"));
        let garbage = Inbound::new(RUN_FINISHED_KIND, json!("garbage")).with_id("not-a-run");
        let unfinished = notice(child_run_id(parent, "c1"), json!({"status": "parked"}));
        for bogus in [stranger, garbage, unfinished] {
            rt.deliver(parent, bogus).await.expect("deliver");
        }
        // The parent wakes for them, finds nothing that answers, and goes back to waiting.
        let view = wait_for(&rt, parent, "the messages consumed", |v| {
            v.pending_inbox == 0 && v.status == RunStatus::Parked && v.version > 3
        })
        .await;
        let state = conversation(&view);
        assert!(matches!(state.pending_wait, Some(PendingWait::Run(_))));
        assert!(tool_results(&state).is_empty(), "no result yet");
        assert!(state.deferred.is_empty(), "nor user text");
        assert_eq!(state.messages.len(), 2);

        gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(
            tool_results(&conversation(&rt.view(parent).await.unwrap().unwrap())),
            vec![("c1".into(), "the real answer".into(), false)]
        );
    }

    /// The child finishes while the parent's tool is still running, before the parent could
    /// park: the message waits in the inbox, and the parent finishes without its timer.
    pub async fn a_notice_that_beats_the_park_is_used(store: DynStore) {
        let rig = Rig::new();
        let hold = Arc::new(Gate::default());
        rig.parent_script("early bird");
        let parent_agent = rig.parent_with(rig.spawn_tool(Some(hold.clone()), false), |b| b);
        let child_agent = rig.child("child says 42");
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let child = child_run_id(parent, "c1");

        notified(&hold.reached, "the tool to start the child").await;
        wait_done(&rt, child).await;
        wait_for(&rt, parent, "the message in the parent's inbox", |v| {
            v.pending_inbox == 1
        })
        .await;
        hold.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;

        assert_eq!(done.output.expect("output")["text"], "early bird");
        assert_eq!(
            tool_results(&conversation(&rt.view(parent).await.unwrap().unwrap())),
            vec![("c1".into(), "child says 42".into(), false)]
        );
        assert_eq!(rig.parent_mock.requests().len(), 2);
    }

    /// The tool fails transiently after it started the child and the child answered meanwhile.
    /// The retry finds the message in its inbox before the wait is recorded, and uses it: the tool
    /// starts nothing twice and the run does not wait for its timer.
    pub async fn a_retry_finds_the_notice_before_the_wait_is_recorded(store: DynStore) {
        let rig = Rig::new();
        let hold = Arc::new(Gate::default());
        // The model is asked again on the retry: steps are at-least-once across transient retries.
        rig.parent_mock
            .push_tool_calls(vec![call("c1", "sub", json!({"message": "review it"}))])
            .push_tool_calls(vec![call("c1", "sub", json!({"message": "review it"}))])
            .push_text("retried");
        let parent_agent = rig.parent_with(rig.spawn_tool(Some(hold.clone()), true), |b| b);
        let child_agent = rig.child("child says 42");
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let child = child_run_id(parent, "c1");

        notified(&hold.reached, "the first attempt").await;
        wait_done(&rt, child).await;
        wait_for(&rt, parent, "the message in the parent's inbox", |v| {
            v.pending_inbox == 1
        })
        .await;
        hold.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;

        assert_eq!(done.output.expect("output")["text"], "retried");
        assert_eq!(rig.spawned.load(SeqCst), 2, "the tool ran on both attempts");
        assert_eq!(rig.child_mock.requests().len(), 1, "but one child ran");
        assert_eq!(
            tool_results(&conversation(&rt.view(parent).await.unwrap().unwrap())),
            vec![("c1".into(), "child says 42".into(), false)]
        );
        assert!(
            !rig.events(parent).contains(&tool_end("c1", "waiting")),
            "the parent never had to wait"
        );
    }

    /// The timer fires while the child still works: the parent looks, sees it is busy and parks
    /// again on a timer one interval later. `wait_poll` sets the interval.
    pub async fn the_parent_looks_at_the_child_when_the_timer_fires(store: DynStore) {
        let rig = Rig::new();
        let gate = Arc::new(Gate::default());
        rig.parent_script("patient");
        rig.child_mock
            .push_tool_calls(vec![call("g1", "gate", json!({}))])
            .push_text("done at last");
        let parent_agent = rig.parent_with(rig.spawn_tool(None, false), |b| {
            b.wait_poll(Duration::from_secs(5))
        });
        let child_agent = rig.child_with(|b| b.tool(GateTool { gate: gate.clone() }));
        let rt = rig.together(&store, &parent_agent, &child_agent);
        let parent = rt
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        notified(&gate.reached, "the child's tool").await;
        let first = wait_parked_on_timer(&rt, parent).await;

        rig.clock.advance(Duration::from_secs(6));
        let second = wait_for(&rt, parent, "a later timer", |v| {
            v.status == RunStatus::Parked && v.wake_at > first.wake_at
        })
        .await;
        // The new timer is one interval from the clock's now, not from the old timer.
        let until = second.wake_at.unwrap() - rig.clock.now();
        assert!(
            until > chrono::Duration::seconds(3) && until <= chrono::Duration::seconds(5),
            "one interval from now, got {until}"
        );
        assert_eq!(
            rig.parent_mock.requests().len(),
            1,
            "the model was not asked"
        );
        assert!(second.version > first.version, "and the look was committed");

        gate.release.notify_one();
        wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(rig.spawned.load(SeqCst), 1);
    }

    /// The child was purged before the parent looked and its message was lost: the model is told,
    /// and the run goes on.
    pub async fn a_child_that_is_gone_is_an_error_result(store: DynStore) {
        let rig = Rig::new();
        rig.parent_script("moving on");
        let (faulty, dynamic) = faulty(store);
        let (parent_agent, child_agent) = (rig.parent(), rig.child("child says 42"));
        let front = rig.front(&dynamic, &parent_agent);
        let back = rig.back(&dynamic, &child_agent);
        let parent = front
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let child = child_run_id(parent, "c1");
        let front_worker = spawn_worker(&front);
        wait_parked_on_timer(&front, parent).await;
        faulty.fail_run(Method::CommitRun, parent, 1);
        let back_worker = spawn_worker(&back);
        wait_done(&back, child).await;
        wait_injected(&faulty, Method::CommitRun, 1, "the message to fail").await;
        back_worker.stop().await;
        dynamic
            .purge_finished(
                &rig.child_name,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .expect("purge");

        rig.clock.advance(Duration::from_secs(61));
        wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(
            tool_results(&conversation(&front.view(parent).await.unwrap().unwrap())),
            vec![(
                "c1".into(),
                "the run failed: the child run no longer exists".into(),
                true
            )]
        );
    }

    /// Fencing during the resume. The worker that answers the call is stuck in its model call and
    /// loses its lease. The consumption of the message was never committed, so the worker that
    /// takes over finds it in the inbox, answers the call the same way and finishes the run; the
    /// first worker's late commit is refused. One result, one child, one finished run.
    pub async fn a_worker_that_loses_its_lease_while_answering_changes_nothing(store: DynStore) {
        let rig = Rig::new();
        let gate = Arc::new(Gate::default());
        // The blocked call and the one that replaces it each need an answer to give.
        rig.parent_mock
            .push_tool_calls(vec![call("c1", "sub", json!({"message": "review it"}))])
            .push_text("answered once")
            .push_text("answered once");
        let model: DynModel = Arc::new(GateModel {
            inner: rig.parent_mock.clone(),
            hold_on: 1,
            calls: AtomicUsize::new(0),
            armed: AtomicBool::new(true),
            gate: gate.clone(),
        });
        let agent = |model: &DynModel| {
            LlmAgent::builder(&rig.parent_name, model.clone(), "m")
                .tool(rig.spawn_tool(None, false))
                .build()
        };
        let (parent_a, parent_b) = (agent(&model), agent(&model));
        let child_agent = rig.child("child says 42");
        let a = rig
            .builder(&store, "a")
            .lease_ttl(Duration::from_millis(200))
            .lease_renewal(false)
            .agent(parent_a)
            .agent(child_agent)
            .build();
        rig.cell.set(a.clone()).ok().expect("set once");
        let b = rig.builder(&store, "b").agent(parent_b).build();

        let parent = a
            .start(&rig.parent_name, user_message("go"), None)
            .await
            .expect("start");
        let wa = spawn_worker(&a);
        notified(&gate.reached, "worker A to be stuck answering the call").await;
        let stuck = a.view(parent).await.unwrap().unwrap();
        assert_eq!(stuck.pending_inbox, 1, "the message is not consumed yet");

        // B takes the parent over when A's lease runs out, and replays the resume.
        let wb = spawn_worker(&b);
        let done = wait_done(&b, parent).await;
        let version = done.version;
        gate.release.notify_one();
        wa.stop().await;
        wb.stop().await;

        let after = b.view(parent).await.unwrap().unwrap();
        assert_eq!(after.version, version, "A's late commit changed nothing");
        assert_eq!(
            after.output.clone().expect("output")["text"],
            "answered once"
        );
        assert_eq!(
            tool_results(&conversation(&after)),
            vec![("c1".into(), "child says 42".into(), false)]
        );
        assert_eq!(rig.spawned.load(SeqCst), 1, "the tool ran once");
        assert_eq!(rig.child_mock.requests().len(), 1, "one child ran");
    }

    /// A run parked on a question by a build that called the field `pending_question` resumes under
    /// this one: the answer still becomes the tool result.
    pub async fn a_run_parked_by_the_old_field_name_resumes(store: DynStore) {
        let rig = Rig::new();
        rig.parent_mock.push_text("deploying to prod");
        let parent_agent = rig.parent_with(rig.spawn_tool(None, false), |b| b.tool(AskTool));
        let old_state = json!({
            "v": 1,
            "seq": 2,
            "agent": {
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "deploy"}]},
                    {"role": "assistant", "content": [],
                     "tool_calls": [{"id": "c1", "name": "ask", "arguments": {}}]}
                ],
                "turns": 1,
                "tool_calls": 1,
                "pending_calls": [{"id": "c1", "name": "ask", "arguments": {}}],
                "pending_question": {
                    "call_id": "c1", "tool": "ask", "question": "which environment?"
                }
            }
        });
        let run = store
            .create_run(NewRun::new(&rig.parent_name, old_state).status(RunStatus::Parked))
            .await
            .expect("create")
            .id;
        let rt = rig.builder(&store, "w").agent(parent_agent).build();
        let view = rt.view(run).await.unwrap().unwrap();
        assert!(view.waiting);
        assert_eq!(
            conversation(&view).pending_wait,
            Some(PendingWait::Question(adam_llm_agent::PendingQuestion {
                call_id: "c1".into(),
                tool: "ask".into(),
                question: "which environment?".into(),
                ui: None,
                stream: None,
            }))
        );

        let worker = spawn_worker(&rt);
        rt.deliver(run, user_message("prod"))
            .await
            .expect("deliver");
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            done.output.clone().expect("output")["text"],
            "deploying to prod"
        );
        assert_eq!(
            rig.parent_mock.requests()[0].messages.last(),
            Some(&Message::tool_result("c1", "prod"))
        );
        let state = conversation(&done);
        assert_eq!(state.pending_wait, None);
        assert!(
            done.state.get("pending_question").is_none(),
            "stored again, the field has its new name"
        );
    }

    /// A journal written before `AwaitRun` existed replays: the recorded old-format error is
    /// decoded and acted on without running the tool again.
    pub async fn an_old_journal_entry_replays(store: DynStore) {
        let rig = Rig::new();
        let ran = Arc::new(AtomicUsize::new(0));
        struct Counted(Arc<AtomicUsize>);
        #[async_trait]
        impl Tool for Counted {
            fn spec(&self) -> ToolSpec {
                spec("ask")
            }
            async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
                self.0.fetch_add(1, SeqCst);
                Ok(ToolOutput::text("must not run"))
            }
        }
        let parent_agent = rig.parent_with(rig.spawn_tool(None, false), |b| {
            b.tool(Counted(ran.clone()))
        });
        let state = json!({
            "v": 1,
            "seq": 1,
            "agent": {
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "deploy"}]},
                    {"role": "assistant", "content": [],
                     "tool_calls": [{"id": "c1", "name": "ask", "arguments": {}}]}
                ],
                "turns": 1,
                "tool_calls": 1,
                "pending_calls": [{"id": "c1", "name": "ask", "arguments": {}}]
            }
        });
        let run = store
            .create_run(NewRun::new(&rig.parent_name, state))
            .await
            .expect("create")
            .id;
        // The entry as an older build wrote it, by hand.
        store
            .journal_put(
                run,
                JournalEntry::err(
                    1,
                    "tool:c1",
                    json!({"NeedsInput": {"question": "which one?"}}),
                ),
            )
            .await
            .expect("journal");
        let rt = rig.builder(&store, "w").agent(parent_agent).build();
        let worker = spawn_worker(&rt);
        let view = wait_for(&rt, run, "parked on the recorded question", |v| v.waiting).await;
        worker.stop().await;
        assert_eq!(ran.load(SeqCst), 0, "the recorded result was replayed");
        assert_eq!(
            conversation(&view).pending_wait,
            Some(PendingWait::Question(adam_llm_agent::PendingQuestion {
                call_id: "c1".into(),
                tool: "ask".into(),
                question: "which one?".into(),
                ui: None,
                stream: None,
            }))
        );
    }
}

// ---------------------------------------------------------------------------
// Instantiate every case per store
// ---------------------------------------------------------------------------

macro_rules! child_suite {
    ($module:ident, $make:path) => {
        mod $module {
            child_suite!(@cases $make;
                the_childs_answer_becomes_the_tool_result,
                a_message_while_waiting_queues_behind_the_result,
                a_duplicate_notice_is_ignored,
                a_lost_notice_is_recovered_when_the_timer_fires,
                a_lost_terminal_ack_is_recovered_when_the_timer_fires,
                a_failing_child_is_an_error_result,
                a_cancelled_child_is_an_error_result,
                cancelling_the_parent_leaves_the_child_running,
                only_the_awaited_childs_notice_answers,
                a_notice_that_beats_the_park_is_used,
                a_retry_finds_the_notice_before_the_wait_is_recorded,
                the_parent_looks_at_the_child_when_the_timer_fires,
                a_child_that_is_gone_is_an_error_result,
                a_worker_that_loses_its_lease_while_answering_changes_nothing,
                a_run_parked_by_the_old_field_name_resumes,
                an_old_journal_entry_replays,
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

child_suite!(memory, super::memory_store);
child_suite!(postgres, super::postgres_store);
child_suite!(mongodb, super::mongodb_store);
