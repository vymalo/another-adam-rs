//! Messages sent to a run while it works (`steer/v1`), seen from an `LlmAgent`: a message is read at
//! the run's next step, never in the middle of a tool call; a final answer that was written while a
//! message was unread is not the last word, the run takes another turn and the model answers the
//! message; and the same message (the same `Inbound::id`) is read once.
//!
//! Run against `MemoryStore` always and against PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set.
//! Nothing sleeps for a fixed time: the model and the tool are held at gates, so a message is
//! delivered *during* the step, always.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Conversation, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{
    DynModel, Message, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse,
    ToolCall, ToolSpec,
};
use adam_runtime::{CollectingSink, Inbound, RunEvent, RunView, Runtime};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn stores() -> Vec<(&'static str, DynStore)> {
    let mut all: Vec<(&'static str, DynStore)> = vec![("memory", Arc::new(MemoryStore::new()))];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        adam_core::Store::migrate(&store).await.expect("migrate");
        all.push(("postgres", Arc::new(store)));
    }
    all
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

/// A point two tasks meet at: the one that reaches it says so, and waits to be let through.
#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

impl Gate {
    async fn wait_reached(&self) {
        tokio::time::timeout(Duration::from_secs(20), self.reached.notified())
            .await
            .expect("timed out waiting for the gate to be reached");
    }
}

/// Wraps a model: the `hold_on`-th call (0-based) waits at the gate before it is answered. That is
/// the model call of a run that is being written when a person sends a message.
struct GateModel {
    inner: Arc<MockModel>,
    hold_on: usize,
    calls: AtomicUsize,
    gate: Arc<Gate>,
}

impl GateModel {
    async fn hold(&self) {
        if self.calls.fetch_add(1, SeqCst) == self.hold_on {
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

/// A tool that waits at its gate each time it is called, then answers `released`.
struct GateTool {
    gate: Arc<Gate>,
}

#[async_trait]
impl Tool for GateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "gate".into(),
            description: "waits".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.gate.reached.notify_one();
        self.gate.release.notified().await;
        Ok(ToolOutput::text("released"))
    }
}

fn gate_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "gate".into(),
        arguments: json!({}),
    }
}

struct Rig {
    name: String,
    mock: Arc<MockModel>,
    sink: CollectingSink,
}

impl Rig {
    fn new() -> Self {
        Self {
            name: uniq("steer"),
            mock: Arc::new(MockModel::new()),
            sink: CollectingSink::new(),
        }
    }

    fn runtime(&self, store: &DynStore, model: DynModel, tool: Option<GateTool>) -> Runtime {
        let mut builder = LlmAgent::builder(&self.name, model, "m");
        if let Some(tool) = tool {
            builder = builder.tool(tool);
        }
        Runtime::builder(store.clone())
            .agent(builder.build())
            .event_sink(self.sink.clone())
            .poll_interval(Duration::from_millis(10))
            .build()
    }

    /// The texts the agent said as its own words (working text and answers), in order.
    fn said(&self, run: RunId) -> Vec<String> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter_map(|e| match e {
                RunEvent::Custom { kind, payload } if kind == "agent_text" => {
                    payload["text"].as_str().map(str::to_owned)
                }
                _ => None,
            })
            .collect()
    }
}

fn spawn_worker(rt: &Runtime) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        let _ = rt
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    (stop, handle)
}

async fn stop(worker: (oneshot::Sender<()>, tokio::task::JoinHandle<()>)) {
    let _ = worker.0.send(());
    tokio::time::timeout(Duration::from_secs(10), worker.1)
        .await
        .expect("worker stops in time")
        .expect("worker task");
}

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if view.status == RunStatus::Done {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for done; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A message as the A2A backend delivers it: kind `message`, the sender's message id as the id.
fn steer(id: &str, text: &str) -> Inbound {
    Inbound::new("message", json!({ "text": text })).with_id(id)
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("the run's state is a Conversation")
}

fn answer(view: &RunView) -> String {
    view.output.as_ref().unwrap()["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

/// A message sent while a tool runs waits for the tool, and is the first thing the model reads
/// after its result: at the next step, never in the middle of the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_while_a_tool_runs_is_read_at_the_next_model_turn() {
    for (backend, store) in stores().await {
        let rig = Rig::new();
        rig.mock
            .push_tool_calls(vec![gate_call("c1")])
            .push_text("done");
        let gate = Arc::new(Gate::default());
        let rt = rig.runtime(
            &store,
            rig.mock.clone(),
            Some(GateTool { gate: gate.clone() }),
        );
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);

        gate.wait_reached().await;
        rt.deliver(run, steer("m-1", "you were wrong since line 1"))
            .await
            .unwrap();
        gate.release.notify_one();
        let done = wait_done(&rt, run).await;
        stop(worker).await;

        let requests = rig.mock.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        assert_eq!(
            requests[1].messages.last(),
            Some(&Message::user_text("you were wrong since line 1")),
            "{backend}: the model's next request carries the steered text"
        );
        let before = &requests[1].messages[requests[1].messages.len() - 2];
        assert_eq!(
            before,
            &Message::tool_result("c1", "released"),
            "{backend}: after the call's result, not in the middle of the call"
        );
        assert_eq!(answer(&done), "done", "{backend}");
        assert_eq!(done.pending_inbox, 0, "{backend}");
    }
}

/// A message sent while the model writes its final answer is answered: the answer is not the last
/// word, the run takes another turn, and the second answer reflects the message. The run never
/// finishes with the first one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_during_the_final_model_call_is_answered() {
    for (backend, store) in stores().await {
        let rig = Rig::new();
        rig.mock
            .push_text("the colour is red")
            .push_text("the colour is blue");
        let gate = Arc::new(Gate::default());
        let model: DynModel = Arc::new(GateModel {
            inner: rig.mock.clone(),
            hold_on: 0,
            calls: AtomicUsize::new(0),
            gate: gate.clone(),
        });
        let rt = rig.runtime(&store, model, None);
        let run = rt
            .start(&rig.name, user_message("which colour?"), None)
            .await
            .unwrap();
        let worker = spawn_worker(&rt);

        gate.wait_reached().await;
        rt.deliver(run, steer("m-1", "I meant the sea"))
            .await
            .unwrap();
        gate.release.notify_one();
        let done = wait_done(&rt, run).await;
        stop(worker).await;

        assert_eq!(answer(&done), "the colour is blue", "{backend}");
        assert_eq!(done.pending_inbox, 0, "{backend}");
        let requests = rig.mock.requests();
        assert_eq!(requests.len(), 2, "{backend}: another model turn");
        assert_eq!(
            requests[1].messages,
            [
                Message::user_text("which colour?"),
                Message::assistant_text("the colour is red"),
                Message::user_text("I meant the sea"),
            ],
            "{backend}: the second request has the first answer and the message after it"
        );
        let state = conversation(&done);
        assert_eq!(state.turns, 2, "{backend}");
        assert_eq!(
            rig.said(run),
            ["the colour is red", "the colour is blue"],
            "{backend}: the first answer was said as the words of a turn that went on"
        );
    }
}

/// Without a message nothing changes: the final answer ends the run in one model turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_message_the_final_answer_ends_the_run() {
    for (backend, store) in stores().await {
        let rig = Rig::new();
        rig.mock.push_text("the colour is red");
        let rt = rig.runtime(&store, rig.mock.clone(), None);
        let run = rt
            .start(&rig.name, user_message("which colour?"), None)
            .await
            .unwrap();
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        stop(worker).await;
        assert_eq!(answer(&done), "the colour is red", "{backend}");
        assert_eq!(rig.mock.requests().len(), 1, "{backend}");
    }
}

/// The same message, sent twice (the orchestration layer repeats one after a lost lease), is read
/// once: both while it is still in the inbox and transitions after it was read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_message_is_read_once() {
    for (backend, store) in stores().await {
        let rig = Rig::new();
        rig.mock
            .push_tool_calls(vec![gate_call("c1")])
            .push_tool_calls(vec![gate_call("c2")])
            .push_text("done");
        let gate = Arc::new(Gate::default());
        let rt = rig.runtime(
            &store,
            rig.mock.clone(),
            Some(GateTool { gate: gate.clone() }),
        );
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);

        // Twice in the inbox at once.
        gate.wait_reached().await;
        rt.deliver(run, steer("m-1", "use blue")).await.unwrap();
        rt.deliver(run, steer("m-1", "use blue")).await.unwrap();
        gate.release.notify_one();
        // Read at the next step; the second call of the tool is where the run is now, and the same
        // message arrives a third time, long after it was read.
        gate.wait_reached().await;
        rt.deliver(run, steer("m-1", "use blue")).await.unwrap();
        // Another message is another message.
        rt.deliver(run, steer("m-2", "use blue")).await.unwrap();
        gate.release.notify_one();
        let done = wait_done(&rt, run).await;
        stop(worker).await;

        let state = conversation(&done);
        let said: Vec<&Message> = state
            .messages
            .iter()
            .filter(|m| **m == Message::user_text("use blue"))
            .collect();
        assert_eq!(said.len(), 2, "{backend}: m-1 once and m-2 once");
        assert_eq!(state.read_ids, ["m-1", "m-2"], "{backend}");
        assert_eq!(done.pending_inbox, 0, "{backend}");
    }
}

/// An answer that was recorded before a message the next transition read (the commit that
/// followed the recording was lost, and the message came in between) did not read it: it is
/// dropped, and the model answers again with the message in front of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_recorded_before_a_message_was_read_is_not_the_last_word() {
    for (backend, store) in stores().await {
        let rig = Rig::new();
        rig.mock.push_text("the colour is blue");
        let rt = rig.runtime(&store, rig.mock.clone(), None);
        let run = rt
            .start(&rig.name, user_message("which colour?"), None)
            .await
            .unwrap();
        // What the worker that crashed left: the model's answer to a history of one message.
        let mut recorded = serde_json::to_value(ModelResponse::text("the colour is red")).unwrap();
        recorded["seen"] = json!(1);
        store
            .journal_put(run, JournalEntry::ok(0, "model:0", recorded))
            .await
            .unwrap();
        rt.deliver(run, steer("m-1", "I meant the sea"))
            .await
            .unwrap();

        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        stop(worker).await;

        assert_eq!(answer(&done), "the colour is blue", "{backend}");
        let requests = rig.mock.requests();
        assert_eq!(
            requests.len(),
            1,
            "{backend}: the recorded answer was not asked again"
        );
        assert_eq!(
            requests[0].messages,
            [
                Message::user_text("which colour?"),
                Message::user_text("I meant the sea"),
            ],
            "{backend}: the stale answer is not in the history"
        );
        assert_eq!(rig.said(run), ["the colour is blue"], "{backend}");
    }
}

/// A journal written before the history length was recorded reads as current: its answer ends the
/// run, as it always did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journal_from_before_the_history_length_reads_as_current() {
    for (backend, store) in stores().await {
        let rig = Rig::new();
        let rt = rig.runtime(&store, rig.mock.clone(), None);
        let run = rt
            .start(&rig.name, user_message("which colour?"), None)
            .await
            .unwrap();
        let old = serde_json::to_value(ModelResponse::text("the colour is red")).unwrap();
        store
            .journal_put(run, JournalEntry::ok(0, "model:0", old))
            .await
            .unwrap();
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        stop(worker).await;
        assert_eq!(answer(&done), "the colour is red", "{backend}");
        assert!(rig.mock.requests().is_empty(), "{backend}");
    }
}

/// What the dedupe state holds, with older state (no `read_ids`) loading as none.
#[test]
fn state_from_before_the_read_ids_loads() {
    let old: Conversation = serde_json::from_value(json!({"messages": [], "turns": 1})).unwrap();
    assert!(old.read_ids.is_empty());
    let json = serde_json::to_value(&old).unwrap();
    assert!(json.get("read_ids").is_none(), "not written while empty");
}
