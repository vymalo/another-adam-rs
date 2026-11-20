//! The engine's API (ADR 0047 slice 4) over `MemoryStore` and a scripted model: send, steer,
//! answer, cancel and list, and what `start` refuses. Every wait is bounded, so a hang fails.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_engine::adam_agent_fs::{Dir, Strictness};
use adam_engine::adam_assembly::{AgentDef, Error as AssemblyError};
use adam_engine::adam_core::{DynStore, MemoryStore, RunId};
use adam_engine::adam_error::{Classify, ErrorClass};
use adam_engine::adam_model::{
    DynModel, Message as ModelMessage, MockModel, ModelClient, ModelDelta, ModelError,
    ModelRequest, ModelResponse, ToolCall,
};
use adam_engine::{
    ConversationId, Delivered, Engine, EngineError, Limits, ListQuery, RunInfo, RunState, Say,
    default_secrets,
};
use async_trait::async_trait;
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
        let mock = Arc::new(MockModel::new());
        let gate = Arc::new(Gate::default());
        let model: DynModel = Arc::new(GateModel {
            inner: mock.clone(),
            hold_on,
            calls: AtomicUsize::new(0),
            gate: gate.clone(),
        });
        let store: DynStore = Arc::new(MemoryStore::new());
        let engine = Engine::builder(store, chat(), model, "m")
            .limits(limits)
            .start()
            .await
            .expect("the chat agent starts");
        Self { mock, gate, engine }
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

    for error in [&alias, &tool, &mcp, &remote, &nested, &lease] {
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
