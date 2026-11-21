//! The engine's API (ADR 0047 slice 4) over `MemoryStore` and a scripted model: send, steer,
//! answer, cancel and list, a run's events and a slow reader of them, a dropped engine's run resumed
//! by the next at once, two engines with their own worker ids on one store, and what `start`
//! refuses. Every wait is bounded, so a hang fails.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_engine::adam_agent_fs::{Dir, Strictness};
use adam_engine::adam_assembly::{AgentDef, Error as AssemblyError};
use adam_engine::adam_core::{DynStore, MemoryStore, RunId, Store};
use adam_engine::adam_error::{Classify, ErrorClass};
use adam_engine::adam_llm_agent::{FnTool, ToolError, ToolOutput, ToolSet};
use adam_engine::adam_model::{
    ContentPart, DynModel, Message as ModelMessage, MockModel, ModelClient, ModelDelta, ModelError,
    ModelRequest, ModelResponse, ToolCall,
};
use adam_engine::adam_runtime::{Artifact, REPLAY_EVENTS_PER_RUN, RunEvent, StepState};
use adam_engine::{
    ConversationId, Delivered, Engine, EngineError, EngineEvent, FiringQuery, Limits, ListQuery,
    RunEvents, RunInfo, RunState, Say, WithheldFile, default_secrets,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

const SEARCH_MCP: &str =
    r#"{"mcpServers": {"search": {"type": "http", "url": "https://search.example.com/mcp"}}}"#;

const CHAT: &str = "---\nname: chat\ndescription: Talks with the person.\n---\nYou chat.\n";

/// The agent of a folder written from `files` (path, content).
fn agent(files: &[(&str, &str)]) -> AgentDef {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    AgentDef::from_source(&Dir::new(dir.path()), Strictness::Lenient)
        .unwrap()
        .remove(0)
}

fn chat() -> AgentDef {
    agent(&[("agent/instructions.md", CHAT)])
}

/// A point the model stops at: it says it got there, and waits to be let through.
#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

impl Gate {
    async fn wait_reached(&self) {
        timeout(WAIT, self.reached.notified())
            .await
            .expect("the model never reached the gate");
    }
}

/// A scripted model whose `hold_on`-th call (0-based) waits at the gate.
struct GateModel {
    inner: Arc<MockModel>,
    hold_on: Option<usize>,
    calls: AtomicUsize,
    gate: Arc<Gate>,
}

impl GateModel {
    async fn hold(&self) {
        if self.hold_on == Some(self.calls.fetch_add(1, SeqCst)) {
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

/// An engine over a fresh `MemoryStore` and a scripted model, its `hold_on`-th call held.
struct Rig {
    mock: Arc<MockModel>,
    gate: Arc<Gate>,
    engine: Engine,
}

impl Rig {
    async fn new(hold_on: Option<usize>) -> Self {
        Self::with_limits(hold_on, Limits::default()).await
    }

    async fn with_limits(hold_on: Option<usize>, limits: Limits) -> Self {
        Self::on_store(Arc::new(MemoryStore::new()), hold_on, limits, |_| {}).await
    }

    /// An engine over `store`, which another engine may share, its model's replies written by
    /// `script` before the worker starts: a run already in the store may be claimed at once.
    async fn on_store(
        store: DynStore,
        hold_on: Option<usize>,
        limits: Limits,
        script: impl FnOnce(&MockModel),
    ) -> Self {
        Self::try_on_store(store, hold_on, limits, script)
            .await
            .expect("the chat agent starts")
    }

    /// [`on_store`](Self::on_store), with `start`'s refusal.
    async fn try_on_store(
        store: DynStore,
        hold_on: Option<usize>,
        limits: Limits,
        script: impl FnOnce(&MockModel),
    ) -> Result<Self, EngineError> {
        let mock = Arc::new(MockModel::new());
        script(&mock);
        let gate = Arc::new(Gate::default());
        let model: DynModel = Arc::new(GateModel {
            inner: mock.clone(),
            hold_on,
            calls: AtomicUsize::new(0),
            gate: gate.clone(),
        });
        let engine = Engine::builder(store, chat(), model, "m")
            .limits(limits)
            .start()
            .await?;
        Ok(Self { mock, gate, engine })
    }

    /// The run once it is in `state`.
    async fn wait(&self, run: RunId, state: RunState) -> RunInfo {
        timeout(WAIT, async {
            loop {
                let info = self.engine.get(run).await.unwrap().expect("the run exists");
                if info.state == state {
                    return info;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("run {run} never reached {state:?}"))
    }

    async fn stop(self) {
        timeout(WAIT, self.engine.shutdown())
            .await
            .expect("the worker stops in time")
            .expect("the worker ends cleanly");
    }
}

fn ask(question: &str) -> ToolCall {
    ToolCall {
        id: "ask-1".into(),
        name: "ask_user".into(),
        arguments: json!({ "question": question }),
        ..Default::default()
    }
}

/// The second `send` of a conversation is a new run in it, and the agent starts from the first
/// run's state: the model's second request holds the first exchange.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_starts_then_continues() {
    let rig = Rig::new(None).await;
    rig.mock.push_text("hello, Ada").push_text("you said hi");

    let first = rig
        .engine
        .send(None, Say::new("hi, I am Ada"))
        .await
        .unwrap();
    assert_eq!(first.delivered, Delivered::Started);
    let done = rig.wait(first.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("hello, Ada"));
    assert_eq!(done.conversation, first.conversation);

    let second = rig
        .engine
        .send(Some(&first.conversation), Say::new("what did I say?"))
        .await
        .unwrap();
    assert_eq!(
        second.delivered,
        Delivered::Continued,
        "the first run ended"
    );
    assert_ne!(second.run, first.run);
    assert_eq!(second.conversation, first.conversation);
    let done = rig.wait(second.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("you said hi"));

    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2);
    let messages = &requests[1].messages;
    for said in [
        ModelMessage::user_text("hi, I am Ada"),
        ModelMessage::assistant_text("hello, Ada"),
    ] {
        assert!(messages.contains(&said), "{said:?} is not in {messages:?}");
    }
    assert_eq!(
        messages.last(),
        Some(&ModelMessage::user_text("what did I say?"))
    );

    // A conversation this engine never started is not one to continue.
    let unknown = rig
        .engine
        .send(Some(&ConversationId::new("nobody's")), Say::new("hello?"))
        .await
        .unwrap_err();
    assert_eq!(unknown.class(), ErrorClass::NotFound, "{unknown}");
    rig.stop().await;
}

/// A message sent while the model writes the run's answer is read before the run ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steer_reaches_a_working_run() {
    let rig = Rig::new(Some(0)).await;
    rig.mock
        .push_text("a draft in English")
        .push_text("une réponse en français");

    let turn = rig
        .engine
        .send(None, Say::new("write a line"))
        .await
        .unwrap();
    rig.gate.wait_reached().await;
    let steered = rig
        .engine
        .steer(turn.run, Say::new("in French, please"))
        .await
        .unwrap();
    assert_eq!(steered.run, turn.run);
    assert_eq!(steered.state, RunState::Working);
    rig.gate.release.notify_one();

    let done = rig.wait(turn.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("une réponse en français"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2, "the run took one more step");
    assert_eq!(
        requests[1].messages.last(),
        Some(&ModelMessage::user_text("in French, please"))
    );
    rig.stop().await;
}

/// `ask_user` parks the run with its question; `answer` resumes it to the end. An answer to a run
/// that works is refused as the run's state forbids it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answer_resumes_a_question() {
    let rig = Rig::new(Some(1)).await;
    rig.mock
        .push_tool_calls(vec![ask("Which colour?")])
        .push_text("blue it is");

    let turn = rig
        .engine
        .send(None, Say::new("pick for me"))
        .await
        .unwrap();
    let waiting = rig.wait(turn.run, RunState::Waiting).await;
    assert_eq!(waiting.question.as_deref(), Some("Which colour?"));

    let answered = rig.engine.answer(turn.run, Say::new("blue")).await.unwrap();
    assert_eq!(answered.state, RunState::Working);
    rig.gate.wait_reached().await;

    let refused = rig
        .engine
        .answer(turn.run, Say::new("red after all"))
        .await
        .unwrap_err();
    assert_eq!(refused.class(), ErrorClass::Rejected, "{refused}");
    assert!(
        matches!(
            refused,
            EngineError::NotWaiting {
                state: RunState::Working,
                ..
            }
        ),
        "{refused:?}"
    );
    rig.gate.release.notify_one();

    let done = rig.wait(turn.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("blue it is"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2, "the refused answer reached nothing");
    let result = requests[1].messages.last().unwrap().text();
    assert!(result.contains("blue"), "the tool's result: {result}");
    assert!(!result.contains("red"), "the tool's result: {result}");
    rig.stop().await;
}

/// A waiting run is cancelled, a second cancel answers it as it is, and it takes no answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_ends_a_waiting_run() {
    let rig = Rig::new(None).await;
    rig.mock.push_tool_calls(vec![ask("Shall I go on?")]);

    let turn = rig.engine.send(None, Say::new("start")).await.unwrap();
    rig.wait(turn.run, RunState::Waiting).await;

    let cancelled = rig.engine.cancel(turn.run).await.unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    assert!(cancelled.error.is_some(), "the reason is said");
    let again = rig.engine.cancel(turn.run).await.unwrap();
    assert_eq!(again, cancelled, "a second cancel changes nothing");
    assert_eq!(
        rig.engine.get(turn.run).await.unwrap().unwrap().state,
        RunState::Cancelled
    );

    let late = rig
        .engine
        .answer(turn.run, Say::new("yes"))
        .await
        .unwrap_err();
    assert_eq!(late.class(), ErrorClass::Rejected, "{late}");
    let unknown = rig.engine.cancel(RunId::new()).await.unwrap_err();
    assert_eq!(unknown.class(), ErrorClass::NotFound, "{unknown}");
    assert_eq!(rig.mock.requests().len(), 1);
    rig.stop().await;
}

/// The runs, the most recently updated first, a page at a time, and one conversation's alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_pages_newest_first() {
    let rig = Rig::new(None).await;
    let mut runs = Vec::new();
    for n in 0..3 {
        rig.mock.push_text(format!("answer {n}"));
        let turn = rig
            .engine
            .send(None, Say::new(format!("question {n}")))
            .await
            .unwrap();
        rig.wait(turn.run, RunState::Done).await;
        runs.push(turn);
    }

    let first = rig
        .engine
        .list(ListQuery::new().page_size(2))
        .await
        .unwrap();
    assert_eq!(first.total, 3);
    let ids: Vec<RunId> = first.items.iter().map(|i| i.run).collect();
    assert_eq!(ids, [runs[2].run, runs[1].run]);
    assert_eq!(first.items[0].output.as_deref(), Some("answer 2"));
    let token = first.next_page_token.expect("a second page");

    let second = rig
        .engine
        .list(ListQuery::new().page_size(2).after(token.clone()))
        .await
        .unwrap();
    let ids: Vec<RunId> = second.items.iter().map(|i| i.run).collect();
    assert_eq!(ids, [runs[0].run]);
    assert_eq!(second.next_page_token, None);

    let one = rig
        .engine
        .list(ListQuery::new().conversation(runs[1].conversation.clone()))
        .await
        .unwrap();
    let ids: Vec<RunId> = one.items.iter().map(|i| i.run).collect();
    assert_eq!(ids, [runs[1].run]);

    // A token belongs to the query it came from.
    let foreign = rig
        .engine
        .list(
            ListQuery::new()
                .conversation(runs[1].conversation.clone())
                .after(token),
        )
        .await
        .unwrap_err();
    assert_eq!(foreign.class(), ErrorClass::Invalid, "{foreign}");
    rig.stop().await;
}

/// After `shutdown` no message is taken (nothing would step its run), reads still work, and a
/// second `shutdown` does nothing. A message with no text is refused before anything is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_refuses_new_work_and_reads_go_on() {
    let rig = Rig::new(None).await;
    rig.mock.push_text("hello");
    let turn = rig.engine.send(None, Say::new("hi")).await.unwrap();
    rig.wait(turn.run, RunState::Done).await;

    let empty = rig.engine.send(None, Say::new("  ")).await.unwrap_err();
    assert_eq!(empty.class(), ErrorClass::Invalid, "{empty}");
    let page = rig.engine.list(ListQuery::new()).await.unwrap();
    assert_eq!(page.total, 1, "the refused message wrote nothing");

    let clone = rig.engine.clone();
    clone.shutdown().await.unwrap();
    let refused = rig
        .engine
        .send(None, Say::new("hi again"))
        .await
        .unwrap_err();
    assert!(matches!(refused, EngineError::Stopped), "{refused:?}");
    assert_eq!(refused.class(), ErrorClass::Rejected);
    for refused in [
        rig.engine.steer(turn.run, Say::new("more")).await,
        rig.engine.answer(turn.run, Say::new("yes")).await,
    ] {
        let refused = refused.unwrap_err();
        assert!(matches!(refused, EngineError::Stopped), "{refused:?}");
    }
    assert_eq!(
        rig.engine.get(turn.run).await.unwrap().unwrap().state,
        RunState::Done
    );
    rig.stop().await;
}

/// The agent as `start` would build it, refused.
async fn refused(def: AgentDef, alias: &str) -> EngineError {
    let store: DynStore = Arc::new(MemoryStore::new());
    Engine::builder(store, def, MockModel::new().into_dyn(), alias)
        .start()
        .await
        .expect_err("start refuses it")
}

/// `start` fails closed: an empty model alias, a tool nobody registered, an MCP server and a
/// remote subagent (no MCP or A2A client is linked, whatever features the build unifies) build no
/// engine.
#[tokio::test]
async fn start_refuses_what_it_cannot_build() {
    let alias = refused(chat(), "").await;
    assert!(
        matches!(&alias, EngineError::Agent(e) if matches!(**e, AssemblyError::ModelAlias { .. })),
        "{alias:?}"
    );

    let tool = refused(
        agent(&[(
            "agent/instructions.md",
            "---\nname: chat\ntools: [run_checks]\n---\nYou chat.\n",
        )]),
        "m",
    )
    .await;
    assert!(matches!(tool, EngineError::Agent(_)), "{tool:?}");

    let mcp = refused(
        agent(&[
            ("agent/instructions.md", CHAT),
            ("agent/mcp.json", SEARCH_MCP),
        ]),
        "m",
    )
    .await;
    assert!(
        matches!(&mcp, EngineError::Invalid(why) if why.contains("runs no MCP server")),
        "{mcp:?}"
    );

    let remote = refused(
        agent(&[
            ("agent/instructions.md", CHAT),
            (
                "agent/subagents/billing.md",
                "---\ndescription: Billing.\na2a: https://billing.example.com/card\n---\n",
            ),
        ]),
        "m",
    )
    .await;
    assert!(
        matches!(&remote, EngineError::Invalid(why) if why.contains("runs no remote subagent")),
        "{remote:?}"
    );

    // A local subagent's own `mcp.json` is refused too.
    let nested = refused(
        agent(&[
            ("agent/instructions.md", CHAT),
            (
                "agent/subagents/researcher/instructions.md",
                "---\ndescription: Researches.\n---\nYou research.\n",
            ),
            ("agent/subagents/researcher/mcp.json", SEARCH_MCP),
        ]),
        "m",
    )
    .await;
    assert!(
        matches!(&nested, EngineError::Invalid(why) if why.contains("chat/researcher")),
        "{nested:?}"
    );
    let store: DynStore = Arc::new(MemoryStore::new());
    let mut limits = Limits::default();
    limits.lease_ttl = Duration::ZERO;
    let lease = Engine::builder(store, chat(), MockModel::new().into_dyn(), "m")
        .limits(limits)
        .start()
        .await
        .expect_err("a lease of zero is refused");
    assert!(matches!(lease, EngineError::Invalid(_)), "{lease:?}");
    let store: DynStore = Arc::new(MemoryStore::new());
    let mut limits = Limits::default();
    limits.event_capacity = 0;
    let ring = Engine::builder(store.clone(), chat(), MockModel::new().into_dyn(), "m")
        .limits(limits.clone())
        .start()
        .await
        .expect_err("a ring of zero events is refused");
    assert!(matches!(ring, EngineError::Invalid(_)), "{ring:?}");
    limits.event_capacity = (1 << 16) + 1;
    let huge = Engine::builder(store, chat(), MockModel::new().into_dyn(), "m")
        .limits(limits)
        .start()
        .await
        .expect_err("a ring over 65 536 events is refused");
    assert!(matches!(huge, EngineError::Invalid(_)), "{huge:?}");

    for error in [&alias, &tool, &mcp, &remote, &nested, &lease, &ring, &huge] {
        assert_eq!(error.class(), ErrorClass::Invalid, "{error}");
    }
    assert_eq!(format!("{:?}", default_secrets()), "SecretSource(none)");
}

/// Outside a tokio runtime there is nowhere to run the worker: refused, not a panic.
#[test]
fn start_outside_a_tokio_runtime_is_refused() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let started = futures::executor::block_on(
        Engine::builder(store, chat(), MockModel::new().into_dyn(), "m").start(),
    );
    let error = started.expect_err("no runtime");
    assert!(matches!(error, EngineError::NoRuntime), "{error:?}");
    assert_eq!(error.class(), ErrorClass::Invalid);
}

/// A `send` to a conversation whose run works steers it, and the same `Say` twice is read once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_working_run_steers_it() {
    let rig = Rig::new(Some(0)).await;
    rig.mock
        .push_text("a draft in English")
        .push_text("une réponse en français");
    let turn = rig
        .engine
        .send(None, Say::new("write a line"))
        .await
        .unwrap();
    rig.gate.wait_reached().await;

    let again = Say::with_id("steer-1", "in French, please");
    for _ in 0..2 {
        let steered = rig
            .engine
            .send(Some(&turn.conversation), again.clone())
            .await
            .unwrap();
        assert_eq!(steered.delivered, Delivered::Steered);
        assert_eq!(steered.run, turn.run);
    }
    rig.gate.release.notify_one();

    let done = rig.wait(turn.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("une réponse en français"));
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2);
    let steer = ModelMessage::user_text("in French, please");
    assert_eq!(
        requests[1].messages.iter().filter(|m| **m == steer).count(),
        1,
        "the message is read once"
    );
    rig.stop().await;
}

/// A `send` to a conversation whose run waits is its answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_waiting_run_answers_it() {
    let rig = Rig::new(None).await;
    rig.mock
        .push_tool_calls(vec![ask("Which colour?")])
        .push_text("blue it is");
    let turn = rig
        .engine
        .send(None, Say::new("pick for me"))
        .await
        .unwrap();
    rig.wait(turn.run, RunState::Waiting).await;

    let answered = rig
        .engine
        .send(Some(&turn.conversation), Say::new("blue"))
        .await
        .unwrap();
    assert_eq!(answered.delivered, Delivered::Answered);
    assert_eq!(answered.run, turn.run);
    let done = rig.wait(turn.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("blue it is"));
    rig.stop().await;
}

/// A `Say` sent again after it started a run starts nothing and says so, in a new conversation and
/// in one it continued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_say_starts_nothing() {
    let rig = Rig::new(None).await;
    rig.mock.push_text("hello").push_text("again");
    let hello = Say::new("hi");
    let first = rig.engine.send(None, hello.clone()).await.unwrap();
    rig.wait(first.run, RunState::Done).await;
    let repeat = rig.engine.send(None, hello).await.unwrap();
    assert_eq!(repeat.delivered, Delivered::Repeated);
    assert_eq!(repeat.run, first.run);
    assert_eq!(repeat.info.state, RunState::Done);

    let next = Say::new("and?");
    let second = rig
        .engine
        .send(Some(&first.conversation), next.clone())
        .await
        .unwrap();
    assert_eq!(second.delivered, Delivered::Continued);
    rig.wait(second.run, RunState::Done).await;
    let repeat = rig
        .engine
        .send(Some(&first.conversation), next)
        .await
        .unwrap();
    assert_eq!(repeat.delivered, Delivered::Repeated);
    assert_eq!(repeat.run, second.run);

    assert_eq!(rig.engine.list(ListQuery::new()).await.unwrap().total, 2);
    assert_eq!(rig.mock.requests().len(), 2, "no message was read twice");
    rig.stop().await;
}

/// A steer to a run that ended is refused for its state; an unknown run is `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_to_an_ended_run_is_refused() {
    let rig = Rig::new(None).await;
    rig.mock.push_text("done");
    let turn = rig.engine.send(None, Say::new("hi")).await.unwrap();
    rig.wait(turn.run, RunState::Done).await;
    let late = rig
        .engine
        .steer(turn.run, Say::new("wait"))
        .await
        .unwrap_err();
    assert!(matches!(late, EngineError::Finished { .. }), "{late:?}");
    assert_eq!(late.class(), ErrorClass::Rejected);
    assert_eq!(rig.engine.get(RunId::new()).await.unwrap(), None);
    rig.stop().await;
}

/// A `shutdown` dropped before it is done (a timeout around it) aborts the worker: the step in
/// flight stops, its lease is not renewed, and the run is free to be claimed again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_shutdown_stops_the_worker() {
    let mut limits = Limits::default();
    limits.lease_ttl = Duration::from_millis(300);
    let rig = Rig::with_limits(Some(0), limits).await;
    rig.mock.push_text("never said");
    let turn = rig.engine.send(None, Say::new("hi")).await.unwrap();
    rig.gate.wait_reached().await;
    assert_eq!(
        rig.engine.get(turn.run).await.unwrap().unwrap().state,
        RunState::Working
    );

    let gave_up = timeout(Duration::from_millis(50), rig.engine.shutdown()).await;
    assert!(gave_up.is_err(), "the step in flight holds the worker");
    // No worker renews the lease any more: it runs out and the run reads as not taken.
    rig.wait(turn.run, RunState::Submitted).await;
    // A later shutdown hears that the worker is gone.
    timeout(WAIT, rig.engine.shutdown())
        .await
        .expect("the worker is gone")
        .unwrap();
}

/// Two shutdowns at once both return only once the worker has stopped: after the step in flight
/// committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_shutdowns_both_wait_for_the_worker() {
    let rig = Rig::new(Some(0)).await;
    rig.mock.push_text("finished in time");
    let turn = rig.engine.send(None, Say::new("hi")).await.unwrap();
    rig.gate.wait_reached().await;

    let shutdowns: Vec<_> = (0..2)
        .map(|_| {
            let engine = rig.engine.clone();
            tokio::spawn(async move { engine.shutdown().await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        shutdowns.iter().all(|s| !s.is_finished()),
        "neither returns while the step is in flight"
    );
    rig.gate.release.notify_one();
    for shutdown in shutdowns {
        timeout(WAIT, shutdown)
            .await
            .expect("the worker stops")
            .unwrap()
            .unwrap();
        assert_eq!(
            rig.engine.get(turn.run).await.unwrap().unwrap().state,
            RunState::Done,
            "the step committed before the shutdown returned"
        );
    }
}

fn plan() -> ToolCall {
    ToolCall {
        id: "plan-1".into(),
        name: "plan_work".into(),
        arguments: json!({ "title": "Greet", "items": ["Say hello"] }),
        ..Default::default()
    }
}

/// Every item of `events` until it ends, each read within `WAIT`, and the error it ended with.
async fn read_all(events: &mut RunEvents) -> (Vec<EngineEvent>, Option<EngineError>) {
    let mut got = Vec::new();
    loop {
        match timeout(WAIT, events.next()).await {
            Ok(Some(Ok(event))) => got.push(event),
            Ok(Some(Err(error))) => {
                let after = timeout(WAIT, events.next()).await.expect("the stream ends");
                assert!(after.is_none(), "an item after the error: {after:?}");
                return (got, Some(error));
            }
            Ok(None) => return (got, None),
            Err(_) => panic!("the stream neither said more nor ended; so far: {got:?}"),
        }
    }
}

/// Every item of `events` until it ends, none an error.
async fn read_to_the_end(events: &mut RunEvents) -> Vec<EngineEvent> {
    let (got, error) = read_all(events).await;
    assert!(error.is_none(), "{error:?} after {got:?}");
    got
}

/// The stream ended on `Stopped`, and said no `Ended` before it.
fn assert_stopped(got: &[EngineEvent], error: Option<EngineError>) {
    let error = error.unwrap_or_else(|| panic!("the stream ended with no error after {got:?}"));
    assert!(matches!(error, EngineError::Stopped), "{error:?}");
    assert!(
        !got.iter().any(|e| matches!(e, EngineEvent::Ended(_))),
        "{got:?}"
    );
}

/// The text pieces among `events`, joined.
fn text_of(events: &[EngineEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Live(RunEvent::TextDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A reader that keeps up hears the run's tool step and its text live, then `Ended` with the
/// output read from the store, and nothing after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_follow_a_run_to_its_end() {
    let rig = Rig::new(Some(0)).await;
    rig.mock
        .push_tool_calls(vec![plan()])
        .push_text("hello, Ada");
    let turn = rig.engine.send(None, Say::new("greet me")).await.unwrap();
    rig.gate.wait_reached().await;
    let mut events = timeout(WAIT, rig.engine.events(turn.run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events.run(), turn.run);
    rig.gate.release.notify_one();

    let got = read_to_the_end(&mut events).await;
    assert_eq!(text_of(&got), "hello, Ada", "{got:?}");
    assert!(
        got.iter().any(|e| matches!(
            e,
            EngineEvent::Live(RunEvent::Step(step)) if step.id == "tool:plan-1"
        )),
        "the tool's step reached the reader: {got:?}"
    );
    assert!(
        !got.iter()
            .any(|e| matches!(e, EngineEvent::Behind { .. } | EngineEvent::Snapshot(_))),
        "a reader that keeps up loses nothing: {got:?}"
    );
    match got.last() {
        Some(EngineEvent::Ended(info)) => {
            assert_eq!(info.state, RunState::Done);
            assert_eq!(info.output.as_deref(), Some("hello, Ada"));
        }
        other => panic!("the stream ended on {other:?}"),
    }
    let ended = got
        .iter()
        .filter(|e| matches!(e, EngineEvent::Ended(_)))
        .count();
    assert_eq!(ended, 1);
    rig.stop().await;
}

/// A reader that reads only after a long reply is over holds no more than the ring and the replay
/// it started with: it is told it fell behind, then reads the whole answer from the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_reader_stays_bounded() {
    const CAPACITY: usize = 16;
    let mut limits = Limits::default();
    limits.event_capacity = CAPACITY;
    let rig = Rig::with_limits(Some(0), limits).await;
    // 256 KiB: 256 pieces of `MAX_TEXT_DELTA_BYTES` (1 KiB), and the events around them.
    let reply = "1234567\n".repeat(32 * 1024);
    rig.mock.push_text(reply.clone());
    let turn = rig
        .engine
        .send(None, Say::new("write a lot"))
        .await
        .unwrap();
    rig.gate.wait_reached().await;
    let mut events = timeout(WAIT, rig.engine.events(turn.run))
        .await
        .unwrap()
        .unwrap();
    rig.gate.release.notify_one();
    rig.wait(turn.run, RunState::Done).await;

    let got = read_to_the_end(&mut events).await;
    let live = got
        .iter()
        .filter(|e| matches!(e, EngineEvent::Live(_)))
        .count();
    // What the stream started with (at most the replay of the run) and what the ring kept.
    let bound = REPLAY_EVENTS_PER_RUN + CAPACITY;
    assert!(
        live <= bound,
        "{live} live events, over the bound of {bound}"
    );
    let behind = got
        .iter()
        .position(|e| matches!(e, EngineEvent::Behind { skipped } if *skipped > 0))
        .unwrap_or_else(|| panic!("the reader was not told it fell behind: {got:?}"));
    assert!(
        matches!(
            got.get(behind + 1),
            Some(EngineEvent::Snapshot(_) | EngineEvent::Ended(_))
        ),
        "the store is read right after: {:?}",
        got.get(behind + 1)
    );
    match got.last() {
        Some(EngineEvent::Ended(info)) => {
            assert_eq!(info.state, RunState::Done);
            assert_eq!(
                info.output.as_ref().map(String::len),
                Some(reply.len()),
                "the whole answer, from the store"
            );
            assert_eq!(info.output.as_deref(), Some(reply.as_str()));
        }
        other => panic!("the stream ended on {other:?}"),
    }
    assert!(
        text_of(&got).len() < reply.len(),
        "the lost pieces are lost, not held"
    );
    rig.stop().await;
}

/// An unknown run has no events: refused at once. A run that has ended gives `Ended` alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_of_an_unknown_run_and_of_a_finished_one() {
    let rig = Rig::new(None).await;
    let unknown = rig.engine.events(RunId::new()).await.unwrap_err();
    assert!(matches!(unknown, EngineError::NotFound(_)), "{unknown:?}");
    assert_eq!(unknown.class(), ErrorClass::NotFound);

    rig.mock.push_text("done already");
    let turn = rig.engine.send(None, Say::new("hi")).await.unwrap();
    let done = rig.wait(turn.run, RunState::Done).await;
    let mut events = rig.engine.events(turn.run).await.unwrap();
    assert_eq!(
        read_to_the_end(&mut events).await,
        [EngineEvent::Ended(done)]
    );
    rig.stop().await;
}

/// The in-memory stand-in for a killed process, on the lease-expiry path: engine A is dropped
/// while its run's second model call is in flight, and engine B, over the same store with a random
/// worker id of its own (the default) and so no release, finishes the run once A's lease runs out.
/// The step A committed is kept: B's model is called once, for the step after it, and the tool that
/// step called is not run again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_engine_resumes_its_run() {
    let store = Arc::new(MemoryStore::new());
    let mut limits = Limits::default();
    limits.lease_ttl = Duration::from_millis(300);

    let a = Rig::on_store(store.clone(), Some(1), limits.clone(), |mock| {
        mock.push_tool_calls(vec![plan()]).push_text("never said");
    })
    .await;
    let turn = a.engine.send(None, Say::new("plan it")).await.unwrap();
    a.gate.wait_reached().await;
    let mut a_events = a.engine.events(turn.run).await.unwrap();
    let Rig {
        mock: a_mock,
        gate: _held,
        engine,
    } = a;
    drop(engine);
    // A stream does not keep the engine alive: it ends with it, `Stopped`.
    let (heard, error) = read_all(&mut a_events).await;
    assert_stopped(&heard, error);
    assert_eq!(
        a_mock.requests().len(),
        1,
        "A's second call never reached its model"
    );

    // B's first model call waits at its gate: everything B did since it claimed the run is in the
    // replay its stream starts with, so a tool B ran again would be seen.
    let b = Rig::on_store(store, Some(0), limits, |mock| {
        mock.push_text("finished by B");
    })
    .await;
    b.gate.wait_reached().await;
    let mut b_events = b.engine.events(turn.run).await.unwrap();
    b.gate.release.notify_one();
    let heard = read_to_the_end(&mut b_events).await;
    let tool_runs = heard
        .iter()
        .filter(|e| {
            matches!(e, EngineEvent::Live(RunEvent::Step(step))
                if step.id == "tool:plan-1" && step.state == StepState::Running)
        })
        .count();
    assert_eq!(tool_runs, 0, "B ran A's tool again: {heard:?}");
    match heard.last() {
        Some(EngineEvent::Ended(done)) => {
            assert_eq!(done.output.as_deref(), Some("finished by B"));
        }
        other => panic!("B's stream ended on {other:?}"),
    }

    let requests = b.mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "B called its model for the next step only"
    );
    let messages = &requests[0].messages;
    assert!(
        messages.contains(&ModelMessage::user_text("plan it")),
        "{messages:?}"
    );
    let results = messages
        .iter()
        .filter(|m| matches!(m, ModelMessage::Tool { call_id, .. } if call_id == "plan-1"))
        .count();
    assert_eq!(results, 1, "the tool ran once: {messages:?}");
    let last = messages.last().expect("a message");
    assert!(
        matches!(last, ModelMessage::Tool { call_id, is_error: false, .. } if call_id == "plan-1"),
        "B continued from A's committed step, the plan's result: {last:?}"
    );
    b.stop().await;
}

/// A lease far longer than any wait here: a run held by it is claimed in time only if it was
/// released.
const LONG_LEASE: Duration = Duration::from_secs(60 * 60);

/// How long a run released at start may take to be claimed and finished: a few polls.
const AT_ONCE: Duration = Duration::from_secs(5);

/// A restarted engine with a fixed worker id releases the leases it holds, so it claims the run its
/// last process was stepping at once: A, with a lease of an hour, is dropped mid-step (its worker
/// aborted), and B, the same id on the same store, finishes the run within seconds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_engine_claims_its_old_runs_at_once() {
    let store = Arc::new(MemoryStore::new());
    let mut limits = Limits::default();
    limits.lease_ttl = LONG_LEASE;
    limits.worker_id = Some("restarted".to_owned());

    let a = Rig::on_store(store.clone(), Some(1), limits.clone(), |mock| {
        mock.push_tool_calls(vec![plan()]).push_text("never said");
    })
    .await;
    let turn = a.engine.send(None, Say::new("plan it")).await.unwrap();
    a.gate.wait_reached().await;
    let Rig {
        gate: _held,
        engine,
        ..
    } = a;
    drop(engine);
    assert!(
        store.lease_until(turn.run).await.unwrap().is_some(),
        "A's lease outlives A"
    );

    // A's id is free once its aborted worker task has ended, at its next poll.
    let b = timeout(AT_ONCE, async {
        loop {
            let started = Rig::try_on_store(store.clone(), None, limits.clone(), |mock| {
                mock.push_text("finished by B");
            })
            .await;
            match started {
                Err(EngineError::WorkerIdInUse { .. }) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                started => return started.expect("B starts"),
            }
        }
    })
    .await
    .expect("A's worker ends and frees its id");
    let done = timeout(AT_ONCE, b.wait(turn.run, RunState::Done))
        .await
        .expect("B claims A's run at once, not when A's lease of an hour runs out");
    assert_eq!(done.output.as_deref(), Some("finished by B"));
    assert_eq!(b.mock.requests().len(), 1, "B stepped the run once");
    b.stop().await;
}

/// An empty worker id would name no worker: refused before anything starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_worker_id_is_refused() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let mut limits = Limits::default();
    limits.worker_id = Some(String::new());
    let error = Engine::builder(store, chat(), MockModel::new().into_dyn(), "m")
        .limits(limits)
        .start()
        .await
        .expect_err("an empty worker id is refused");
    assert!(matches!(error, EngineError::Invalid(_)), "{error:?}");
    assert_eq!(error.class(), ErrorClass::Invalid);
}

/// Two live engines in one process on one store, `a` and `b`, each with a lease of an hour: A's
/// run, whose model call is in flight, keeps A's lease after B's start and is not stepped by B, and
/// each engine steps its own run once.
async fn two_engines_keep_their_own_leases(a: Option<&str>, b: Option<&str>) {
    let store = Arc::new(MemoryStore::new());
    let mut limits = Limits::default();
    limits.lease_ttl = LONG_LEASE;
    limits.worker_id = a.map(str::to_owned);
    let a = Rig::on_store(store.clone(), Some(0), limits.clone(), |mock| {
        mock.push_text("said by A");
    })
    .await;
    let run_a = a.engine.send(None, Say::new("to A")).await.unwrap().run;
    a.gate.wait_reached().await;

    limits.worker_id = b.map(str::to_owned);
    let b = Rig::on_store(store.clone(), None, limits, |mock| {
        mock.push_text("said by B");
    })
    .await;
    let run_b = b.engine.send(None, Say::new("to B")).await.unwrap().run;
    let done = b.wait(run_b, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("said by B"));
    assert_eq!(
        b.engine.get(run_a).await.unwrap().unwrap().state,
        RunState::Working
    );
    assert!(
        store.lease_until(run_a).await.unwrap().is_some(),
        "A's run is still leased"
    );

    a.gate.release.notify_one();
    let done = a.wait(run_a, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("said by A"));
    assert_eq!(a.mock.requests().len(), 1, "A stepped its run once");
    let b_requests = b.mock.requests();
    assert_eq!(b_requests.len(), 1, "B stepped its own run only");
    assert!(
        !b_requests[0]
            .messages
            .contains(&ModelMessage::user_text("to A")),
        "B stepped A's run"
    );
    a.stop().await;
    b.stop().await;
}

/// Two engines with the default (`None`, a random id each) on one store: neither releases the
/// other's leases, so a run is never stepped twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_default_engines_on_one_store_do_not_take_each_others_leases() {
    two_engines_keep_their_own_leases(None, None).await;
}

/// Two engines with fixed ids of their own: B's start releases B's leases only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_engines_with_their_own_ids_keep_their_own_leases() {
    two_engines_keep_their_own_leases(Some("engine-a"), Some("engine-b")).await;
}

/// A second live engine of this process with A's fixed id on A's store is refused (its release
/// would take A's leases); the same id on another store starts, and after A's `shutdown` so does
/// the same id on A's store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_live_engine_with_the_same_id_is_refused() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let mut limits = Limits::default();
    limits.worker_id = Some("app".to_owned());
    let a = Rig::on_store(store.clone(), None, limits.clone(), |_| {}).await;

    let refused = Rig::try_on_store(store.clone(), None, limits.clone(), |_| {})
        .await
        .err()
        .expect("a second live engine with the same id is refused");
    assert!(
        matches!(&refused, EngineError::WorkerIdInUse { id } if id == "app"),
        "{refused:?}"
    );
    assert_eq!(refused.class(), ErrorClass::Rejected);

    let elsewhere = Rig::on_store(Arc::new(MemoryStore::new()), None, limits.clone(), |_| {}).await;
    a.stop().await;
    let again = Rig::on_store(store, None, limits, |_| {}).await;
    again.stop().await;
    elsewhere.stop().await;
}

/// After `shutdown`, a stream on a run that has not ended ends with `Stopped` at once, and so does
/// one opened after it: nothing would step the run, so nothing is polled for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_ends_stopped_after_shutdown() {
    let rig = Rig::new(None).await;
    rig.mock.push_tool_calls(vec![ask("Shall I go on?")]);
    let turn = rig.engine.send(None, Say::new("start")).await.unwrap();
    rig.wait(turn.run, RunState::Waiting).await;
    let mut before = rig.engine.events(turn.run).await.unwrap();

    timeout(WAIT, rig.engine.shutdown())
        .await
        .expect("the worker stops")
        .unwrap();
    let started = std::time::Instant::now();
    let (got, error) = read_all(&mut before).await;
    assert_stopped(&got, error);
    let mut after = rig.engine.events(turn.run).await.unwrap();
    let (got, error) = read_all(&mut after).await;
    assert_stopped(&got, error);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the streams ended at once, not at a poll: {:?}",
        started.elapsed()
    );
}

/// A reader that falls behind a run that is still going gets `Behind`, then a `Snapshot` of the run
/// as it is, then follows it live to `Ended`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_behind_a_running_run_gets_a_snapshot() {
    let mut limits = Limits::default();
    limits.event_capacity = 16;
    let rig = Rig::with_limits(Some(0), limits).await;
    // Many pieces of text, then a question: the run waits, and has not ended.
    let long = "1234567\n".repeat(16 * 1024);
    let mut asking = ModelResponse::tool_calls(vec![ask("Shall I go on?")]);
    if let ModelMessage::Assistant { content, .. } = &mut asking.message {
        *content = vec![ContentPart::text(long)];
    }
    rig.mock.push_response(asking).push_text("done");
    let turn = rig
        .engine
        .send(None, Say::new("write, then ask"))
        .await
        .unwrap();
    rig.gate.wait_reached().await;
    let mut events = rig.engine.events(turn.run).await.unwrap();
    rig.gate.release.notify_one();
    rig.wait(turn.run, RunState::Waiting).await;

    let mut got = Vec::new();
    let snapshot = loop {
        let event = timeout(WAIT, events.next())
            .await
            .expect("an item")
            .expect("the stream goes on")
            .expect("no error");
        if let EngineEvent::Snapshot(info) = &event {
            break info.clone();
        }
        assert!(
            !matches!(event, EngineEvent::Ended(_)),
            "the run has not ended: {got:?}"
        );
        got.push(event);
    };
    assert!(
        matches!(got.last(), Some(EngineEvent::Behind { skipped }) if *skipped > 0),
        "a Snapshot follows Behind: {got:?}"
    );
    assert_eq!(snapshot.state, RunState::Waiting);
    assert_eq!(snapshot.question.as_deref(), Some("Shall I go on?"));

    rig.engine.answer(turn.run, Say::new("yes")).await.unwrap();
    let rest = read_to_the_end(&mut events).await;
    match rest.last() {
        Some(EngineEvent::Ended(info)) => {
            assert_eq!(info.state, RunState::Done);
            assert_eq!(info.output.as_deref(), Some("done"));
        }
        other => panic!("the stream ended on {other:?}"),
    }
    rig.stop().await;
}

// --- parity with the binaries (docs/reference/parity.md) -----------------------------------------

/// A folder's `schedules/` run in the engine as in `adam-agent` (ADR 0060, #269, #289): the scheduler is
/// registered, the reconcile at `start` starts it, and the firing is a run of the agent, in the
/// host's store.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folders_schedule_fires_the_agent() {
    let def = agent(&[
        ("agent/instructions.md", CHAT),
        (
            "agent/schedules/soon.md",
            "---\nafter: 1s\n---\nSay what is due.\n",
        ),
    ]);
    let mock = Arc::new(MockModel::new());
    mock.push_text("Nothing is due.");
    let model: DynModel = mock.clone();
    let store: DynStore = Arc::new(MemoryStore::new());
    let engine = Engine::builder(store.clone(), def, model, "m")
        .start()
        .await
        .unwrap();
    let fired = timeout(WAIT, async {
        loop {
            let runs = store
                .list_runs(&adam_engine::adam_core::RunQuery::new(
                    "chat",
                    adam_engine::adam_core::ConversationScope::Prefix(
                        "adam.schedule:chat/soon@".to_owned(),
                    ),
                    10,
                ))
                .await
                .unwrap();
            if let Some(run) = runs
                .into_iter()
                .find(|r| r.status == adam_engine::adam_core::RunStatus::Done)
            {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the schedule fired");
    // A firing is nobody's message, so it is in no caller's `list`; the engine shows it to its host
    // all the same (#289): `firings` lists it with its trigger, and `get` and `files` reach it.
    let listed = engine.list(ListQuery::new()).await.unwrap();
    assert!(listed.items.is_empty(), "{listed:?}");
    let firings = engine.firings(FiringQuery::new()).await.unwrap();
    let [firing] = firings.items.as_slice() else {
        panic!("one firing: {firings:?}");
    };
    assert_eq!(firing.info.run, fired.id);
    assert_eq!(firing.info.state, RunState::Done);
    assert_eq!(firing.info.output.as_deref(), Some("Nothing is due."));
    assert_eq!(
        (firing.trigger.agent.as_str(), firing.trigger.name.as_str()),
        ("chat", "soon")
    );
    assert_eq!(firings.total, 1);
    assert_eq!(
        engine.get(fired.id).await.unwrap().as_ref(),
        Some(&firing.info)
    );
    assert!(engine.files(fired.id).await.unwrap().is_empty());
    let asked = mock.requests();
    assert!(
        format!("{:?}", asked[0].messages).contains("Say what is due."),
        "the firing's prompt is the person's message: {asked:?}"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// The runtime's retry policy is the host's (`Limits::retry`, #270): a transient model failure,
/// which names no policy of its own, is given up after the host's two attempts, not five.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hosts_retry_policy_governs_a_transient_failure() {
    let mock = Arc::new(MockModel::new());
    for _ in 0..2 {
        mock.push_error(ModelError::transient("502"));
    }
    mock.push_text("never");
    let model: DynModel = mock.clone();
    let mut limits = Limits::default();
    limits.retry = adam_engine::adam_runtime::RetryPolicy::new(2)
        .with_backoff(Duration::from_millis(5), Duration::from_millis(5));
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .limits(limits)
        .start()
        .await
        .unwrap();
    let turn = engine.send(None, Say::new("hi")).await.unwrap();
    let mut events = engine.events(turn.run).await.unwrap();
    let mut last = None;
    while let Some(event) = timeout(WAIT, events.next()).await.expect("an event") {
        last = Some(event.unwrap());
    }
    let Some(EngineEvent::Ended(info)) = last else {
        panic!("the stream ended on {last:?}");
    };
    assert_eq!(info.state, RunState::Failed);
    assert_eq!(mock.requests().len(), 2, "two attempts, the host's");
    assert_eq!(
        Limits::default().retry,
        adam_engine::adam_runtime::RetryPolicy::default()
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// A folder's `commands/` are expanded in the engine as on the server surfaces (`commands/v1`,
/// #276): `/greet Ada Lovelace` reaches the model as the command's template with `$1` (not the
/// whole argument line), `commands()` lists it, and an
/// unknown `/name` reaches the model as typed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folders_command_is_expanded() {
    let def = agent(&[
        ("agent/instructions.md", CHAT),
        (
            "agent/commands/greet.md",
            "---\ndescription: Greet someone.\nargument-hint: \"<name>\"\n---\nSay hello to $1, warmly.\n",
        ),
    ]);
    let mock = Arc::new(MockModel::new());
    mock.push_text("Hello, Ada!");
    mock.push_text("I do not know that one.");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), def, model, "m")
        .start()
        .await
        .unwrap();
    let listed: Vec<(&str, &str)> = engine
        .commands()
        .iter()
        .map(|c| (c.name.as_str(), c.description.as_str()))
        .collect();
    assert_eq!(listed, [("greet", "Greet someone.")]);
    assert_eq!(
        engine.commands()[0].argument_hint.as_deref(),
        Some("<name>")
    );

    for (said, expected) in [
        ("/greet Ada Lovelace", "Say hello to Ada, warmly."),
        ("/wave Ada", "/wave Ada"),
    ] {
        let turn = engine.send(None, Say::new(said)).await.unwrap();
        let mut events = engine.events(turn.run).await.unwrap();
        while let Some(event) = timeout(WAIT, events.next()).await.expect("an event") {
            event.unwrap();
        }
        let asked = mock.requests();
        let last = asked.last().expect("a model call");
        let text = format!("{:?}", last.messages.last());
        assert!(text.contains(expected), "{said}: {text}");
    }
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// A host's own tool can hand the agent a file, and the host reads it back (#280): the event
/// withholds its bytes and says its digest, `files` lists it, `file` answers its bytes by that
/// digest, and another digest or a run this engine does not have is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_the_agent_hands_over_is_read_by_its_digest() {
    const BYTES: &[u8] = b"the report, line one\nline two\n";
    let report = FnTool::raw(
        "write_report",
        "Write the report as a file.",
        json!({"type": "object", "properties": {}}),
        |_ctx, _args| async move {
            let file = Artifact::file("report", "text/plain", "report.txt", BYTES.to_vec())
                .expect("a file");
            Ok::<_, ToolError>(ToolOutput::text("written").with_artifact(file))
        },
    );
    let def = agent(&[(
        "agent/instructions.md",
        "---\nname: chat\ntools: [write_report]\n---\nYou chat.\n",
    )]);
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "report-1".into(),
        name: "write_report".into(),
        arguments: json!({}),
        ..Default::default()
    }])
    .push_text("Here is the report.");
    // The first model call waits until the reader is subscribed, so its stream holds the file.
    let gate = Arc::new(Gate::default());
    let model: DynModel = Arc::new(GateModel {
        inner: mock.clone(),
        hold_on: Some(0),
        calls: AtomicUsize::new(0),
        gate: gate.clone(),
    });
    let engine = Engine::builder(Arc::new(MemoryStore::new()), def, model, "m")
        .tools(ToolSet::new().tool(report))
        .start()
        .await
        .unwrap();

    let turn = engine.send(None, Say::new("Write it.")).await.unwrap();
    gate.wait_reached().await;
    let mut events = engine.events(turn.run).await.unwrap();
    gate.release.notify_one();
    let mut withheld = Vec::new();
    while let Some(event) = timeout(WAIT, events.next()).await.expect("an event") {
        if let EngineEvent::Live(event) = event.unwrap() {
            withheld.extend(WithheldFile::of(&event));
        }
    }
    let [said] = withheld.as_slice() else {
        panic!("one file event: {withheld:?}");
    };
    assert_eq!(said.len, BYTES.len() as u64);
    let digest = Artifact::file("report", "text/plain", "report.txt", BYTES.to_vec())
        .expect("a file")
        .file_sha256()
        .expect("a digest");
    assert_eq!(said.sha256, digest, "the event says the bytes' SHA-256");

    let files = engine.files(turn.run).await.unwrap();
    let [file] = files.as_slice() else {
        panic!("one file: {files:?}");
    };
    assert_eq!(
        (
            file.name.as_str(),
            file.mime_type.as_deref(),
            file.filename.as_str(),
            file.len
        ),
        (
            "report",
            Some("text/plain"),
            "report.txt",
            BYTES.len() as u64
        )
    );
    assert_eq!(
        file.sha256, said.sha256,
        "the list and the event name it alike"
    );
    let bytes = engine
        .file(turn.run, &said.sha256)
        .await
        .unwrap()
        .expect("its bytes");
    assert_eq!(&*bytes, BYTES);

    assert!(
        engine
            .file(turn.run, &"0".repeat(64))
            .await
            .unwrap()
            .is_none()
    );
    let unknown = RunId::new();
    for refused in [
        engine.files(unknown).await.map(|_| ()).unwrap_err(),
        engine
            .file(unknown, &said.sha256)
            .await
            .map(|_| ())
            .unwrap_err(),
    ] {
        assert_eq!(refused.class(), ErrorClass::NotFound, "{refused:?}");
    }
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// A host's tool named as one of the engine's own is refused at `start` (`DuplicateTool`), never
/// put in the other's place.
#[tokio::test]
async fn a_host_tool_named_as_the_engines_is_refused() {
    let ask = FnTool::raw(
        "ask_user",
        "Not the engine's.",
        json!({"type": "object", "properties": {}}),
        |_ctx, _args| async move { Ok::<_, ToolError>(ToolOutput::text("mine")) },
    );
    let refused = Engine::builder(
        Arc::new(MemoryStore::new()),
        chat(),
        MockModel::new().into_dyn(),
        "m",
    )
    .tools(ToolSet::new().tool(ask))
    .start()
    .await
    .expect_err("start refuses it");
    assert!(
        matches!(&refused, EngineError::Agent(e) if matches!(**e, AssemblyError::DuplicateTool { .. })),
        "{refused:?}"
    );
    assert_eq!(refused.class(), ErrorClass::Invalid);
}

/// A host sees and stops a firing that works (#289): two schedules fire, `firings` pages them and
/// filters by schedule (`a` never lists `ab`'s), a token of another query is refused, `events`
/// streams the working one and `cancel` ends it `Cancelled`, a done one is `Finished` to `cancel`, the
/// schedulers are not reached, and `steer`, `answer` and a `send` to its conversation do not reach a
/// firing (nobody talks to one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_lists_and_cancels_a_working_firing() {
    let def = agent(&[
        ("agent/instructions.md", CHAT),
        ("agent/schedules/a.md", "---\nafter: 1s\n---\nSay hello.\n"),
        ("agent/schedules/ab.md", "---\nafter: 1s\n---\nSay bye.\n"),
    ]);
    let mock = Arc::new(MockModel::new());
    mock.push_text("one").push_text("two");
    let gate = Arc::new(Gate::default());
    let model: DynModel = Arc::new(GateModel {
        inner: mock.clone(),
        hold_on: Some(0),
        calls: AtomicUsize::new(0),
        gate: gate.clone(),
    });
    let store: DynStore = Arc::new(MemoryStore::new());
    let engine = Engine::builder(store.clone(), def, model, "m")
        .start()
        .await
        .unwrap();
    // The first firing's model call waits at the gate: it works until it is cancelled.
    gate.wait_reached().await;
    let all = timeout(WAIT, async {
        loop {
            let page = engine.firings(FiringQuery::new()).await.unwrap();
            // The other firing's model call is not held: wait until it is done, so the one that
            // works is the held one, not one about to finish.
            let state = |s| {
                page.items
                    .iter()
                    .any(|f: &adam_engine::Firing| f.info.state == s)
            };
            if page.total == 2 && state(RunState::Done) && state(RunState::Working) {
                return page;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both schedules fired");
    let working = all
        .items
        .iter()
        .find(|f| f.info.state == RunState::Working)
        .expect("one firing works")
        .clone();

    let first = engine
        .firings(FiringQuery::new().page_size(1))
        .await
        .unwrap();
    let token = first.next_page_token.clone().expect("a second page");
    let second = engine
        .firings(FiringQuery::new().page_size(1).after(token.clone()))
        .await
        .unwrap();
    let mut paged: Vec<RunId> = first
        .items
        .iter()
        .chain(&second.items)
        .map(|f| f.info.run)
        .collect();
    let mut every: Vec<RunId> = all.items.iter().map(|f| f.info.run).collect();
    paged.sort();
    every.sort();
    assert_eq!(paged, every, "two pages of one hold both firings");
    assert!(second.next_page_token.is_none());
    let foreign = engine
        .firings(FiringQuery::new().schedule("a").page_size(1).after(token))
        .await
        .unwrap_err();
    assert_eq!(foreign.class(), ErrorClass::Invalid, "{foreign:?}");
    for name in ["a", "ab"] {
        let only = engine
            .firings(FiringQuery::new().schedule(name))
            .await
            .unwrap();
        let names: Vec<&str> = only.items.iter().map(|f| f.trigger.name.as_str()).collect();
        assert_eq!(
            names,
            [name],
            "the filter {name:?} lists only its own firing"
        );
        assert_eq!(only.total, 1);
    }
    let done = all
        .items
        .iter()
        .find(|f| f.info.state == RunState::Done)
        .expect("one firing is done");
    let ended = engine.cancel(done.info.run).await.unwrap_err();
    assert!(matches!(ended, EngineError::Finished { .. }), "{ended:?}");
    // The schedulers are the engine's own work, not firings: neither listed nor reached.
    let schedulers = store
        .list_runs(&adam_engine::adam_core::RunQuery::new(
            adam_engine::adam_schedule::scheduler_agent_name("chat"),
            adam_engine::adam_core::ConversationScope::Prefix("adam.schedule:chat/".to_owned()),
            10,
        ))
        .await
        .unwrap();
    assert_eq!(schedulers.len(), 2, "{schedulers:?}");
    for scheduler in &schedulers {
        assert!(engine.get(scheduler.id).await.unwrap().is_none());
    }

    let refused = engine
        .steer(working.info.run, Say::new("stop that"))
        .await
        .unwrap_err();
    assert_eq!(refused.class(), ErrorClass::NotFound, "{refused:?}");
    let unanswered = engine
        .answer(working.info.run, Say::new("yes"))
        .await
        .unwrap_err();
    assert_eq!(unanswered.class(), ErrorClass::NotFound, "{unanswered:?}");
    let sent = engine
        .send(Some(&working.info.conversation), Say::new("hello"))
        .await
        .unwrap_err();
    assert_eq!(sent.class(), ErrorClass::NotFound, "{sent:?}");
    let mut events = engine.events(working.info.run).await.unwrap();
    let cancelled = engine.cancel(working.info.run).await.unwrap();
    assert_eq!(cancelled.state, RunState::Cancelled);
    let (heard, error) = read_all(&mut events).await;
    assert!(error.is_none(), "{error:?}");
    assert!(
        matches!(heard.last(), Some(EngineEvent::Ended(info)) if info.state == RunState::Cancelled),
        "{heard:?}"
    );
    // Cancelling it again answers it as it is.
    assert_eq!(
        engine.cancel(working.info.run).await.unwrap().state,
        RunState::Cancelled
    );
    gate.release.notify_one();
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}
