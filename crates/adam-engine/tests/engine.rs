//! The engine's API (ADR 0047 slice 4) over `MemoryStore` and a scripted model: send, steer,
//! answer, cancel and list, a run's events and a slow reader of them, a dropped engine's run resumed
//! by the next at once, two engines with their own worker ids on one store, and what `start`
//! refuses. Every wait is bounded, so a hang fails.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_engine::adam_agent_fs::{Dir, Strictness};
use adam_engine::adam_assembly::{AgentDef, EffortSource, Error as AssemblyError};
use adam_engine::adam_core::{DynStore, MemoryStore, RunId, Store, TurnKeys};
use adam_engine::adam_error::{Classify, ErrorClass};
use adam_engine::adam_llm_agent::{FnTool, ToolError, ToolOutput, ToolSet};
use adam_engine::adam_model::{
    ContentPart, DynModel, InputModalities, Message as ModelMessage, MockModel, ModelClient,
    ModelDelta, ModelError, ModelProfile, ModelProfiles, ModelRequest, ModelResponse,
    ReasoningEffort, ReasoningWire, ToolCall,
};
use adam_engine::adam_runtime::{Artifact, REPLAY_EVENTS_PER_RUN, RunEvent, StepState};
use adam_engine::{
    ConversationId, Delivered, Engine, EngineError, EngineEvent, FiringQuery, Limits, ListQuery,
    RunEvents, RunInfo, RunState, Say, SessionState, StartSession, WithheldFile, default_secrets,
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

    fn provider(&self) -> Option<&str> {
        self.inner.provider()
    }

    fn context_window(&self, model: &str) -> Option<u64> {
        self.inner.context_window(model)
    }

    fn input_modalities(&self, model: &str) -> InputModalities {
        self.inner.input_modalities(model)
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

/// A device's envelope given with a `Say` reaches the model calls of the run it starts; the next run
/// of the conversation is its own prompt, and a `Say` with none carries none. The
/// host's client sends and signs it (`SignedRequests`); this checks the engine hands it over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_say_carries_the_persons_envelope_to_every_model_call() {
    use adam_engine::adam_model::PromptStatement;
    const ENVELOPE: &str =
        "eyJhbGciOiJFZERTQSIsIm5vbmNlIjoibi0xIn0.eyJ0eXAiOiJwcm9tcHQvdjEifQ.c2ln";
    let rig = Rig::new(None).await;
    rig.mock
        .push_text("one")
        .push_text("two")
        .push_text("three");
    let first = rig
        .engine
        .send(
            None,
            Say::new("hi").with_prompt_statement(PromptStatement::envelope(ENVELOPE).unwrap()),
        )
        .await
        .unwrap();
    rig.wait(first.run, RunState::Done).await;
    let second = rig
        .engine
        .send(Some(&first.conversation), Say::new("and then"))
        .await
        .unwrap();
    rig.wait(second.run, RunState::Done).await;
    let plain = rig
        .engine
        .send(None, Say::new("no envelope"))
        .await
        .unwrap();
    rig.wait(plain.run, RunState::Done).await;

    let carried: Vec<Option<(Option<String>, Option<String>)>> = rig
        .mock
        .requests()
        .iter()
        .map(|r| {
            r.prompt_statement.as_ref().map(|p| {
                (
                    p.statement().map(str::to_owned),
                    p.user().map(str::to_owned),
                )
            })
        })
        .collect();
    let envelope = Some((None, Some(ENVELOPE.to_owned())));
    assert_eq!(carried, vec![envelope, None, None]);
    assert_eq!(
        rig.mock.requests()[0]
            .prompt_statement
            .as_ref()
            .unwrap()
            .prompt_jti()
            .unwrap(),
        "n-1"
    );
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

/// ADR 0068 decision 9: a store that outlives the process refuses to start without a turn key, and
/// a store that dies with the process signs with one made at start that dies with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_memory_store_signs_with_an_ephemeral_turn_key() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let engine = Engine::builder(store, chat(), MockModel::new().into_dyn(), "m")
        .start()
        .await
        .expect("a memory store needs no key of the host's");
    assert_eq!(engine.turn_keys().kind(), "ephemeral");
    assert!(engine.turn_keys().current().await.is_ok());
    engine.shutdown().await.unwrap();
}

/// The other half of decision 9: the file a run outlives the process in needs the host's key, and
/// `start` names the builder method rather than making one that would die with the process.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_that_outlives_the_process_needs_a_turn_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adam.sqlite");
    let store: DynStore = Arc::new(adam_store_sqlite::SqliteStore::open(&path).unwrap());
    let refused = Engine::builder(store.clone(), chat(), MockModel::new().into_dyn(), "m")
        .start()
        .await
        .expect_err("a durable store without a turn key");
    assert!(matches!(refused, EngineError::NoTurnKeys), "{refused:?}");
    assert_eq!(refused.class(), ErrorClass::Invalid);

    let keys = <dyn TurnKeys>::ephemeral().unwrap();
    let engine = Engine::builder(store, chat(), MockModel::new().into_dyn(), "m")
        .turn_keys(keys)
        .start()
        .await
        .expect("the host's key is enough");
    engine.shutdown().await.unwrap();
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

/// A host starts, talks to, lists and closes a child session (`child-sessions/v1`, #279), as a
/// client of the binaries does: the helpers are the local subagents; a session's first round is
/// in its own conversation and the parent takes no turn; a message to an idle kept-open session is
/// its next round; `sessions` lists it from the parent; a close ends it, and a closed session takes
/// no message. An unknown helper is refused, and an agent with no subagent has no helper.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_starts_lists_and_closes_a_child_session() {
    let def = agent(&[
        ("agent/instructions.md", CHAT),
        (
            "agent/subagents/planner.md",
            "---\ndescription: Plans the work.\n---\nYou plan.\n",
        ),
    ]);
    let mock = Arc::new(MockModel::new());
    mock.push_text("Hello.")
        .push_text("Plan one.")
        .push_text("Plan two.");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), def, model, "m")
        .start()
        .await
        .unwrap();
    let helpers: Vec<(&str, &str)> = engine
        .helpers()
        .iter()
        .map(|h| (h.name.as_str(), h.description.as_str()))
        .collect();
    assert_eq!(helpers, [("planner", "Plans the work.")]);

    let wait = |run: RunId, state: RunState| {
        let engine = engine.clone();
        async move {
            timeout(WAIT, async {
                loop {
                    let info = engine.get(run).await.unwrap().expect("the run");
                    if info.state == state {
                        return info;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{run} never reached {state:?}"))
        }
    };
    let parent = engine.send(None, Say::new("hi")).await.unwrap();
    wait(parent.run, RunState::Done).await;

    let unknown = engine
        .start_session(
            &parent.conversation,
            StartSession::new("nobody", "Plan it"),
            Say::new("plan"),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown.class(), ErrorClass::Invalid, "{unknown:?}");

    let start = StartSession::new("planner", "Plan the trip").keep_open();
    let first = engine
        .start_session(
            &parent.conversation,
            start.clone(),
            Say::with_id("start-1", "make a plan"),
        )
        .await
        .unwrap();
    assert_eq!(first.delivered, Delivered::Started);
    // Sent again, the same start is the session it started.
    let again = engine
        .start_session(
            &parent.conversation,
            start,
            Say::with_id("start-1", "make a plan"),
        )
        .await
        .unwrap();
    assert_eq!(
        (again.delivered, again.run),
        (Delivered::Repeated, first.run)
    );
    assert_ne!(
        first.conversation, parent.conversation,
        "a conversation of its own"
    );
    assert_eq!(
        wait(first.run, RunState::Done).await.output.as_deref(),
        Some("Plan one.")
    );
    let parent_runs = engine
        .list(ListQuery::new().conversation(parent.conversation.clone()))
        .await
        .unwrap();
    assert_eq!(
        parent_runs.items.len(),
        1,
        "the parent took no turn: {parent_runs:?}"
    );

    let listed = engine.sessions(&parent.conversation).await.unwrap();
    let [session] = listed.as_slice() else {
        panic!("one session: {listed:?}");
    };
    assert_eq!(session.conversation, first.conversation);
    assert_eq!(
        (session.helper.as_str(), session.description.as_str()),
        ("planner", "Plan the trip")
    );
    assert!(session.keep_open);
    assert_eq!(session.state, SessionState::Idle);
    assert_eq!((session.round, session.latest), (1, Some(first.run)));

    let second = engine
        .send(
            Some(&first.conversation),
            Say::with_id("round-2", "and the return"),
        )
        .await
        .unwrap();
    assert_eq!(second.delivered, Delivered::Continued);
    assert_ne!(second.run, first.run);
    let resent = engine
        .send(
            Some(&first.conversation),
            Say::with_id("round-2", "and the return"),
        )
        .await
        .unwrap();
    assert_eq!(
        (resent.delivered, resent.run),
        (Delivered::Repeated, second.run)
    );
    assert_eq!(
        wait(second.run, RunState::Done).await.output.as_deref(),
        Some("Plan two.")
    );
    let rounds = engine
        .list(ListQuery::new().conversation(first.conversation.clone()))
        .await
        .unwrap();
    assert_eq!(rounds.items.len(), 2, "{rounds:?}");

    // A session's conversation holds no session, and a close goes to a session's conversation.
    let nested = engine.sessions(&first.conversation).await.unwrap_err();
    assert_eq!(nested.class(), ErrorClass::Invalid, "{nested:?}");
    let not_a_session = engine
        .close_session(&parent.conversation, false)
        .await
        .unwrap_err();
    assert_eq!(
        not_a_session.class(),
        ErrorClass::Invalid,
        "{not_a_session:?}"
    );
    engine
        .close_session(&first.conversation, false)
        .await
        .unwrap();
    let closed = engine.sessions(&parent.conversation).await.unwrap();
    assert_eq!(closed[0].state, SessionState::Closed, "{closed:?}");
    let refused = engine
        .send(Some(&first.conversation), Say::new("one more"))
        .await
        .unwrap_err();
    assert!(matches!(refused, EngineError::Refused(_)), "{refused:?}");
    assert_eq!(refused.class(), ErrorClass::Rejected);
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();

    let lone = Engine::builder(
        Arc::new(MemoryStore::new()),
        chat(),
        MockModel::new().into_dyn(),
        "m",
    )
    .start()
    .await
    .unwrap();
    assert!(lone.helpers().is_empty());
    let none = lone
        .start_session(
            &parent.conversation,
            StartSession::new("planner", "Plan it"),
            Say::new("plan"),
        )
        .await
        .unwrap_err();
    assert_eq!(none.class(), ErrorClass::Invalid, "{none:?}");
    // With no helper a close is refused, never sent to the agent as a message.
    let closed_nothing = lone
        .close_session(&first.conversation, false)
        .await
        .unwrap_err();
    assert_eq!(
        closed_nothing.class(),
        ErrorClass::Invalid,
        "{closed_nothing:?}"
    );
    timeout(WAIT, lone.shutdown()).await.unwrap().unwrap();
}

/// The host's session limits hold (`Limits::sessions`, #279): with one session open at most, a
/// second start in the same conversation is refused while the first is open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hosts_session_limits_refuse_a_session_too_many() {
    let def = agent(&[
        ("agent/instructions.md", CHAT),
        (
            "agent/subagents/planner.md",
            "---\ndescription: Plans the work.\n---\nYou plan.\n",
        ),
    ]);
    let mock = Arc::new(MockModel::new());
    mock.push_text("Hello.").push_text("Plan one.");
    let model: DynModel = mock.clone();
    let mut limits = Limits::default();
    limits.sessions =
        adam_engine::adam_runtime::SessionLimits::new(1, 1, 20).expect("limits in bounds");
    let engine = Engine::builder(Arc::new(MemoryStore::new()), def, model, "m")
        .limits(limits)
        .start()
        .await
        .unwrap();
    let parent = engine.send(None, Say::new("hi")).await.unwrap();
    let first = engine
        .start_session(
            &parent.conversation,
            StartSession::new("planner", "One").keep_open(),
            Say::new("plan"),
        )
        .await
        .unwrap();
    // Its round ends and it stays open, idle: `max_open`, not `max_working`, refuses the next.
    timeout(WAIT, async {
        while engine
            .get(first.run)
            .await
            .unwrap()
            .expect("the round")
            .state
            != RunState::Done
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the first round ends");
    let second = engine
        .start_session(
            &parent.conversation,
            StartSession::new("planner", "Two"),
            Say::new("plan again"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&second, EngineError::Refused(why) if why.contains("open")),
        "{second:?}"
    );
    assert_eq!(second.class(), ErrorClass::Rejected);
    let listed = engine.sessions(&parent.conversation).await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].conversation, first.conversation);
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

// ------------------------------------------------------------------ S0: the lean first request

/// The first request's bytes: the system prompt plus each tool schema as the OpenAI wire wraps it.
fn first_request_bytes(req: &ModelRequest) -> (usize, usize) {
    let system = req.system.as_deref().map_or(0, str::len);
    let tools = req
        .tools
        .iter()
        .map(|t| {
            serde_json::to_vec(&json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            }))
            .expect("a tool schema serialises")
            .len()
        })
        .sum();
    (system, tools)
}

/// S0 of the lean-first-request plan (`plans/2026-11-21-lean-first-request.md`): an embedded
/// engine's first request is held to the owner's **5,000-token** budget for a lazy agent
/// (ADR 0066), with the same assembly as a plain folder agent. The message is not counted: it is
/// the same "Hi" in every measurement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engines_first_request_stays_under_the_lean_budget() {
    const BUDGET_TOKENS: usize = 5000;

    let rig = Rig::new(None).await;
    rig.mock
        .push_text("Hello. What would you like me to work on?");
    let turn = rig.engine.send(None, Say::new("Hi")).await.unwrap();
    rig.wait(turn.run, RunState::Done).await;

    let first = &rig.mock.requests()[0];
    let (system, tools) = first_request_bytes(first);
    let total = system + tools;
    let tokens = total / 4;
    println!(
        "engine first request: system {system} B + tools {tools} B = {total} B \
         (~{tokens} tokens at chars/4); lean budget {BUDGET_TOKENS} tokens \
         (~{} B), margin {} tokens",
        BUDGET_TOKENS * 4,
        BUDGET_TOKENS.saturating_sub(tokens)
    );
    assert!(
        tokens <= BUDGET_TOKENS,
        "the engine's first request is ~{tokens} tokens (chars/4), over the {BUDGET_TOKENS}-token \
         budget for a lazy agent (ADR 0066)"
    );
    rig.stop().await;
}

// ------------------------------- ADR 0063 decision 8: the engine's profiles and its level

/// The deployment's profiles as its `MODEL_ALIASES_FILE` writes them, parsed by the same loader:
/// one model `m`, its levels and wire, and the optional own level, cap and `quick` preset.
fn profiles(own_level: Option<&str>, cap: Option<u32>, quick: Option<&str>) -> ModelProfiles {
    let own = own_level.map_or_else(String::new, |level| {
        format!(r#", "reasoningEffort": "{level}""#)
    });
    let cap = cap.map_or_else(String::new, |cap| format!(r#", "maxOutputTokens": {cap}"#));
    let presets = quick.map_or_else(String::new, |level| {
        format!(r#""presets": {{ "quick": {{ "reasoningEffort": "{level}" }} }},"#)
    });
    ModelProfiles::parse_json(&format!(
        r#"{{ {presets}
            "profiles": {{
                "m": {{ "reasoningEfforts": ["low", "high"], "reasoningWire": "reasoning_effort"{own}{cap} }}
            }}
        }}"#
    ))
    .expect("the deployment's profiles parse")
}

/// Send `text` and follow its run to its end.
async fn say_to_the_end(engine: &Engine, text: &str) -> RunInfo {
    let turn = engine.send(None, Say::new(text)).await.unwrap();
    let mut events = engine.events(turn.run).await.unwrap();
    let mut last = None;
    while let Some(event) = timeout(WAIT, events.next()).await.expect("an event") {
        last = Some(event.unwrap());
    }
    match last {
        Some(EngineEvent::Ended(info)) => info,
        other => panic!("the stream ended on {other:?}"),
    }
}

/// The deployment's profiles reach an embedded engine's agent (ADR 0063 decision 8): the model's
/// own level is the resolved default, and the profile's cap is clamped to what the window leaves
/// and to the model's own documented limit. The mock is used directly, not through `Rig`, whose
/// `GateModel` forwards no `context_window`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_deployments_profiles_reach_an_engines_agent() {
    // The window leaves 200,000 − 20,000 margin − the 100,000 default history: 80,000.
    let mock = Arc::new(MockModel::new().with_context_window("m", 200_000));
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .models(profiles(Some("low"), Some(100_000), None))
        .start()
        .await
        .expect("the agent starts with its model's profile");
    let done = say_to_the_end(&engine, "hi").await;
    assert_eq!(done.state, RunState::Done);
    let requests = mock.requests();
    let first = requests.first().expect("one model call");
    assert_eq!(
        first.reasoning_effort,
        Some(ReasoningEffort::Low),
        "the profile's own level is the request's"
    );
    assert_eq!(
        first.max_output_tokens,
        Some(80_000),
        "the window clamps the profile's cap"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();

    // The model's documented limit clamps it too: 64,000 asked, 32,000 allowed. The loader
    // refuses a profile whose own cap is above its limit, so this is a hand-built `ModelProfiles`
    // — the case `output_cap` names for one.
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .models(ModelProfiles {
            profiles: BTreeMap::from([(
                "m".to_owned(),
                ModelProfile {
                    reasoning_efforts: Some(vec![ReasoningEffort::Low, ReasoningEffort::High]),
                    reasoning_wire: Some(ReasoningWire::ReasoningEffort),
                    max_output_tokens: Some(64_000),
                    max_output_tokens_limit: Some(32_000),
                    ..ModelProfile::default()
                },
            )]),
            presets: BTreeMap::new(),
        })
        .start()
        .await
        .expect("the agent starts with its model's profile");
    say_to_the_end(&engine, "hi").await;
    assert_eq!(
        mock.requests()[0].max_output_tokens,
        Some(32_000),
        "the model's limit clamps the profile's cap"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// The builder's level is the **root's** (ADR 0063 decision 8), between the folder and the preset:
/// a folder that sets none takes the builder's over its `model_preset`'s, and a folder that sets
/// one wins over the builder's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_builders_level_is_the_roots_and_the_file_wins() {
    // (a) No folder key: the builder's `high` wins over the `quick` preset's `low`.
    let presetted = agent(&[(
        "agent/instructions.md",
        "---\nname: chat\ndescription: Talks with the person.\nmodel_preset: quick\n---\nYou chat.\n",
    )]);
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), presetted, model, "m")
        .models(profiles(None, None, Some("low")))
        .reasoning_effort(ReasoningEffort::High)
        .start()
        .await
        .expect("the preset is declared and the builder's level is one of the model's");
    say_to_the_end(&engine, "hi").await;
    assert_eq!(
        mock.requests()[0].reasoning_effort,
        Some(ReasoningEffort::High),
        "the builder's level wins over the preset's"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();

    // (b) The folder's `low` wins over the builder's `high`.
    let keyed = agent(&[(
        "agent/instructions.md",
        "---\nname: chat\ndescription: Talks with the person.\nreasoning_effort: low\n---\nYou chat.\n",
    )]);
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), keyed, model, "m")
        .models(profiles(None, None, None))
        .reasoning_effort(ReasoningEffort::High)
        .start()
        .await
        .expect("the folder's level is one of the model's");
    say_to_the_end(&engine, "hi").await;
    assert_eq!(
        mock.requests()[0].reasoning_effort,
        Some(ReasoningEffort::Low),
        "the folder's own key wins over the builder's"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// A level the model's profile does not take stops `start` (ADR 0063 decision 4 and 8):
/// `EngineError::Agent` (class `Invalid`), whether the builder set it or a folder did with no
/// profiles at all, which pins the binaries' fail-closed behaviour as the engine's too. The
/// refusal reports the value as `reasoning_effort` on the agent's file even when the builder set
/// it (`EffortSource::Folder`), the ruled compromise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_level_the_models_profile_does_not_take_stops_start() {
    let refused = Engine::builder(
        Arc::new(MemoryStore::new()),
        chat(),
        MockModel::new().into_dyn(),
        "m",
    )
    .models(profiles(None, None, None))
    .reasoning_effort(ReasoningEffort::Max)
    .start()
    .await
    .expect_err("the model takes low and high, not max");
    let EngineError::Agent(source) = &refused else {
        panic!("the agent is refused: {refused:?}");
    };
    let AssemblyError::ReasoningEffort {
        effort,
        set_by,
        model,
        allowed,
        ..
    } = &**source
    else {
        panic!("the refusal is about the effort: {source}");
    };
    assert_eq!(effort, "max");
    assert_eq!(
        *set_by,
        EffortSource::Folder,
        "the builder's value is reported as the agent's own `reasoning_effort`"
    );
    assert_eq!(model, "m");
    assert_eq!(allowed.as_slice(), ["low", "high"]);
    assert_eq!(refused.class(), ErrorClass::Invalid);

    // The folder's own key, with no profiles: nothing declares a level, so nothing may set one.
    let keyed = agent(&[(
        "agent/instructions.md",
        "---\nname: chat\ndescription: Talks with the person.\nreasoning_effort: low\n---\nYou chat.\n",
    )]);
    let unprofiled = Engine::builder(
        Arc::new(MemoryStore::new()),
        keyed,
        MockModel::new().into_dyn(),
        "m",
    )
    .start()
    .await
    .expect_err("no profile declares a level");
    let EngineError::Agent(source) = &unprofiled else {
        panic!("the agent is refused: {unprofiled:?}");
    };
    let AssemblyError::ReasoningEffort { effort, set_by, .. } = &**source else {
        panic!("the refusal is about the effort: {source}");
    };
    assert_eq!(effort, "low");
    assert_eq!(*set_by, EffortSource::Folder);
    assert_eq!(unprofiled.class(), ErrorClass::Invalid);
}

/// The builder's level wins over the profile's own `reasoningEffort` (ADR 0063 decision 8): the
/// profile's is the model's default, not a ceiling on the host's explicit choice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_builders_level_wins_over_the_profiles_own() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .models(profiles(Some("low"), None, None))
        .reasoning_effort(ReasoningEffort::High)
        .start()
        .await
        .expect("the builder's level is one of the model's");
    say_to_the_end(&engine, "hi").await;
    assert_eq!(
        mock.requests()[0].reasoning_effort,
        Some(ReasoningEffort::High),
        "the builder's level wins over the profile's own"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// A folder that names a preset with no profiles to declare it stops `start` (ADR 0064):
/// `EngineError::Agent` (`UnknownModelPreset`, class `Invalid`), as on a server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preset_the_profiles_do_not_declare_stops_start() {
    let presetted = agent(&[(
        "agent/instructions.md",
        "---\nname: chat\ndescription: Talks with the person.\nmodel_preset: quick\n---\nYou chat.\n",
    )]);
    let refused = Engine::builder(
        Arc::new(MemoryStore::new()),
        presetted,
        MockModel::new().into_dyn(),
        "m",
    )
    .start()
    .await
    .expect_err("no profiles declare quick");
    let EngineError::Agent(source) = &refused else {
        panic!("the agent is refused: {refused:?}");
    };
    assert!(
        matches!(**source, AssemblyError::UnknownModelPreset { .. }),
        "{source}"
    );
    assert_eq!(refused.class(), ErrorClass::Invalid);
}

/// The builder's level is the **root's only** (ADR 0063 decision 8): a subagent never inherits it,
/// since every subagent has its own file. A root at `high` and a child whose model takes only
/// `none` both start, and the child's request sends no level.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_builders_level_never_reaches_a_subagent() {
    let def = agent(&[
        ("agent/instructions.md", CHAT),
        (
            "agent/subagents/worker.md",
            "---\ndescription: Does a piece of the work.\nmodel: child\n---\nYou work.\n",
        ),
    ]);
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "worker".into(),
        arguments: json!({ "message": "do the piece" }),
        ..Default::default()
    }]);
    mock.push_text("the piece is done").push_text("all done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), def, model, "m")
        .models(
            ModelProfiles::parse_json(
                r#"{
                    "profiles": {
                        "m": { "reasoningEfforts": ["high"], "reasoningWire": "reasoning_effort" },
                        "child": { "reasoningEfforts": ["none"], "reasoningWire": "reasoning_effort" }
                    }
                }"#,
            )
            .expect("the deployment's profiles parse"),
        )
        .reasoning_effort(ReasoningEffort::High)
        .start()
        .await
        .expect("the root's level is the root's alone");
    let done = say_to_the_end(&engine, "hi").await;
    assert_eq!(done.state, RunState::Done);
    let requests = mock.requests();
    let child = requests
        .iter()
        .find(|request| request.model == "child")
        .expect("the subagent's model call");
    assert_eq!(
        child.reasoning_effort, None,
        "the builder's level never reaches the subagent"
    );
    let roots: Vec<_> = requests
        .iter()
        .filter(|request| request.model == "m")
        .collect();
    assert_eq!(roots.len(), 2, "the parent's two turns: {requests:?}");
    assert!(
        roots
            .iter()
            .all(|request| request.reasoning_effort == Some(ReasoningEffort::High)),
        "the root's level on every root call: {roots:?}"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

// ------------------------------- ADR 0064 amended 2026-11-23: a model per Say, and a fallback

/// Two profiled models: `m` (the engine's own) and `lite`, with its own cap and level.
fn two_models() -> ModelProfiles {
    ModelProfiles::parse_json(
        r#"{ "profiles": {
            "m": { "reasoningEfforts": ["low", "high"], "reasoningWire": "reasoning_effort" },
            "lite": { "reasoningEfforts": ["low"], "reasoningWire": "reasoning_effort",
                      "reasoningEffort": "low", "maxOutputTokens": 4096 }
        } }"#,
    )
    .expect("the profiles parse")
}

/// A `Say` that names a model with a profile is answered by it, with that profile's cap and level;
/// one that names a model without a profile is refused before anything is written, and so is any
/// alias when the engine was given no profiles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_say_names_a_model_and_the_engine_refuses_one_with_no_profile() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .models(two_models())
        .start()
        .await
        .expect("starts");

    let refused = engine
        .send(None, Say::new("hi").with_model("unknown"))
        .await
        .expect_err("a model with no profile is refused");
    let EngineError::Invalid(why) = &refused else {
        panic!("expected Invalid, got {refused:?}");
    };
    assert!(why.contains("`unknown`") && why.contains("lite"), "{why}");
    assert!(mock.requests().is_empty(), "nothing reached the model");

    let turn = engine
        .send(None, Say::new("hi").with_model("lite"))
        .await
        .expect("a profiled model is accepted");
    let mut events = engine.events(turn.run).await.unwrap();
    while timeout(WAIT, events.next())
        .await
        .expect("an event")
        .is_some()
    {}
    let requests = mock.requests();
    assert_eq!(requests[0].model, "lite");
    assert_eq!(requests[0].max_output_tokens, Some(4096));
    assert_eq!(requests[0].reasoning_effort, Some(ReasoningEffort::Low));
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();

    // No profiles at all: only the agent's own alias is a choice.
    let model: DynModel = Arc::new(MockModel::new());
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .start()
        .await
        .expect("starts");
    let refused = engine
        .send(None, Say::new("hi").with_model("lite"))
        .await
        .expect_err("no profiles, no choice");
    assert!(matches!(refused, EngineError::Invalid(_)), "{refused:?}");
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// A folder's `model_fallback` reaches the engine's agent: the locked model's call is made again
/// to the fallback. A fallback that is the agent's own model stops `start`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folders_model_fallback_switches_once_in_an_engine() {
    let with = |fallback: &str| {
        agent(&[(
            "agent/instructions.md",
            &format!(
                "---\nname: chat\ndescription: Talks with the person.\nmodel_fallback: {fallback}\n---\nYou chat.\n"
            ),
        )])
    };
    let mock = Arc::new(MockModel::new());
    mock.push_error(adam_model::ModelError::ModelLocked { model: "m".into() })
        .push_text("done");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), with("lite"), model, "m")
        .models(two_models())
        .start()
        .await
        .expect("starts");
    let done = say_to_the_end(&engine, "hi").await;
    assert_eq!(done.state, RunState::Done);
    let models: Vec<String> = mock.requests().iter().map(|r| r.model.clone()).collect();
    assert_eq!(models, ["m", "lite"]);
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();

    let model: DynModel = Arc::new(MockModel::new());
    let refused = Engine::builder(Arc::new(MemoryStore::new()), with("m"), model, "m")
        .models(two_models())
        .start()
        .await
        .expect_err("a fallback that is the agent's own model");
    assert!(matches!(refused, EngineError::Agent(_)), "{refused:?}");
}

/// A model with a profile that no agent can be bound to (its preset's cap is above its own limit)
/// is no choice: the engine refuses it at `Say` time and writes nothing, instead of answering on
/// the default model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_say_may_not_name_a_model_the_agent_cannot_bind() {
    let mut profiles = two_models();
    profiles.presets.insert(
        "huge".to_owned(),
        adam_engine::adam_model::Preset {
            max_output_tokens: Some(5_000),
            ..Default::default()
        },
    );
    profiles.profiles.insert(
        "tight".to_owned(),
        ModelProfile {
            preset: Some("huge".to_owned()),
            max_output_tokens_limit: Some(1_000),
            ..ModelProfile::default()
        },
    );
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .models(profiles)
        .start()
        .await
        .expect("starts");
    let refused = engine
        .send(None, Say::new("hi").with_model("tight"))
        .await
        .expect_err("a model the agent cannot be bound to is no choice");
    assert!(matches!(refused, EngineError::Invalid(_)), "{refused:?}");
    assert!(mock.requests().is_empty(), "nothing reached the model");
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// After the run switched to its fallback, a message that names a model is refused with an error
/// that says so, never dropped; a message that names none still reaches the run. The pick does not
/// carry into the conversation's next run, which starts on the agent's own model again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_picked_after_the_switch_is_refused_and_the_next_run_starts_on_the_agents_own() {
    let folder = agent(&[(
        "agent/instructions.md",
        "---\nname: chat\ndescription: Talks with the person.\nmodel_fallback: lite\n---\nYou chat.\n",
    )]);
    let mock = Arc::new(MockModel::new());
    mock.push_error(ModelError::ModelLocked { model: "m".into() })
        .push_tool_calls(vec![ask("Which colour?")])
        .push_text("blue it is")
        .push_text("second run");
    let model: DynModel = mock.clone();
    let engine = Engine::builder(Arc::new(MemoryStore::new()), folder, model, "m")
        .models(two_models())
        .start()
        .await
        .expect("starts");
    let turn = engine.send(None, Say::new("pick for me")).await.unwrap();
    timeout(WAIT, async {
        loop {
            let info = engine.get(turn.run).await.unwrap().expect("the run");
            if info.state == RunState::Waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the run waits on its question, on the fallback");

    for refused in [
        engine
            .answer(turn.run, Say::new("blue").with_model("m"))
            .await
            .map(drop),
        engine
            .steer(turn.run, Say::new("blue").with_model("m"))
            .await
            .map(drop),
        engine
            .send(Some(&turn.conversation), Say::new("blue").with_model("m"))
            .await
            .map(drop),
    ] {
        let refused = refused.expect_err("a pick after the switch is refused");
        let EngineError::Refused(why) = &refused else {
            panic!("expected Refused, got {refused:?}");
        };
        assert!(why.contains("`m`") && why.contains("`lite`"), "{why}");
    }
    engine.answer(turn.run, Say::new("blue")).await.unwrap();
    let done = timeout(WAIT, async {
        loop {
            let info = engine.get(turn.run).await.unwrap().expect("the run");
            if info.state == RunState::Done {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("done");
    assert_eq!(done.output.as_deref(), Some("blue it is"));

    engine
        .send(Some(&turn.conversation), Say::new("again"))
        .await
        .unwrap();
    timeout(WAIT, async {
        while mock.requests().len() < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the second run asks");
    let models: Vec<String> = mock.requests().iter().map(|r| r.model.clone()).collect();
    assert_eq!(
        models,
        ["m", "lite", "lite", "m"],
        "the pick does not carry over"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// ADR 0065 slice 6: an engine with no pools reports every pool as unlimited, not as zero: an
/// absent limit is not a limit of zero. Nothing is held and no device was pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_with_no_limits_reports_unlimited_not_zero() {
    let rig = Rig::new(None).await;
    let report = rig.engine.resources();
    for pool in adam_budget::Pool::ALL {
        let line = report
            .pool(pool)
            .unwrap_or_else(|| panic!("{pool} is in the report"));
        assert_eq!(line.limit(), adam_budget::Limit::Unlimited, "{pool}");
        assert_ne!(line.limit(), adam_budget::Limit::AtMost(0), "{pool}");
        assert_eq!(line.in_use(), 0, "{pool}");
    }
    assert!(report.device().is_none(), "the host pushed no device");
    rig.stop().await;
}

/// ADR 0065 decision 9: the report carries what the pools hold, the ceiling each counts against,
/// and the four readings of the host's device.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_report_carries_the_pools_held_and_the_devices_readings() {
    use adam_admission_local::LocalAdmission;
    use adam_budget::{Ask, Budget, DynAdmission, Limit, Pool};
    use adam_device_state::{DeviceReadings, FakeDevice, MemoryPressure, Network, Power, Thermal};

    let admission: DynAdmission = Arc::new(LocalAdmission::new(
        Budget::builder()
            .runs(2)
            .tool_calls(4)
            .model_calls(2)
            .build(),
    ));
    let readings = DeviceReadings::new(
        Power::on_battery(15, false),
        Thermal::Serious,
        Network::Metered,
        MemoryPressure::Warn,
    );
    let device = Arc::new(FakeDevice::new(readings));
    let model: DynModel = Arc::new(MockModel::new());
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .admission(Arc::clone(&admission))
        .device_state(device)
        .start()
        .await
        .unwrap();

    let held = admission
        .admit(Ask::new(Pool::ToolCalls, "one conversation"))
        .await
        .unwrap();
    let report = engine.resources();
    let calls = report.pool(Pool::ToolCalls).expect("tool calls are listed");
    assert_eq!(calls.limit(), Limit::AtMost(4));
    assert_eq!(calls.in_use(), 1);
    assert_eq!(
        report.pool(Pool::Runs).expect("runs are listed").limit(),
        Limit::AtMost(2)
    );
    assert_eq!(
        report
            .pool(Pool::Processes)
            .expect("processes are listed")
            .limit(),
        Limit::Unlimited,
        "a pool the budget leaves alone is unlimited"
    );
    assert_eq!(report.device().copied(), Some(readings));
    drop(held);
    assert_eq!(
        engine
            .resources()
            .pool(Pool::ToolCalls)
            .expect("listed")
            .in_use(),
        0,
        "a dropped permit gives its share back"
    );
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}

/// ADR 0065 decision 9: while a run works, its stream carries the report, at most every two
/// seconds. The run is held in its first model call, so it is working and quiet throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_working_run_hears_the_resource_report() {
    let rig = Rig::new(Some(0)).await;
    rig.mock.push_text("hello back");
    let started = std::time::Instant::now();
    let turn = rig.engine.send(None, Say::new("hello")).await.unwrap();
    rig.gate.wait_reached().await;
    let mut events = rig.engine.events(turn.run).await.unwrap();
    let report = timeout(WAIT, async {
        while let Some(event) = events.next().await {
            if let EngineEvent::Resources(report) = event.unwrap() {
                return report;
            }
        }
        panic!("the stream ended without a report");
    })
    .await
    .expect("a report arrives while the run works");
    assert!(
        started.elapsed() >= Duration::from_millis(1_900),
        "a report is not sent more often than every two seconds, {:?} after the start",
        started.elapsed()
    );
    for pool in adam_budget::Pool::ALL {
        let line = report
            .pool(pool)
            .unwrap_or_else(|| panic!("{pool} is in the report"));
        assert_eq!(line.limit(), adam_budget::Limit::Unlimited, "{pool}");
    }
    rig.gate.release.notify_one();
    rig.wait(turn.run, RunState::Done).await;
    rig.stop().await;
}

/// ADR 0065 decision 8: `set_device_state` lowers the admission from the next ask on. On a low
/// battery a second run waits for the first, plugged in again it does not, and the floor of one
/// run holds: a run on its own is always admitted. The person's switch turns the rule off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_device_state_on_a_low_battery_admits_one_run_at_a_time() {
    use adam_admission_local::LocalAdmission;
    use adam_budget::{Ask, Budget, DeviceRules, DynAdmission, Pool, Refused};
    use adam_device_state::{DeviceReadings, FakeDevice, Power};

    let admission: DynAdmission = Arc::new(LocalAdmission::new(
        Budget::builder()
            .runs(3)
            .tool_calls(4)
            .model_calls(2)
            .build(),
    ));
    let model: DynModel = Arc::new(MockModel::new());
    let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
        .admission(admission)
        .start()
        .await
        .unwrap();
    let asked = engine
        .admission()
        .expect("the engine was given an admission");

    let first = asked.admit(Ask::new(Pool::Runs, "one")).await.unwrap();
    let second = asked.admit(Ask::new(Pool::Runs, "two")).await.unwrap();
    drop((first, second));

    let device = Arc::new(FakeDevice::new(DeviceReadings::NOMINAL));
    engine.set_device_state(device.clone());
    device.set_power(Power::on_battery(12, false));
    let first = asked.admit(Ask::new(Pool::Runs, "one")).await.unwrap();
    let second = asked.admit(Ask::new(Pool::Runs, "two")).await;
    assert!(
        matches!(
            second,
            Err(Refused::Busy {
                pool: Pool::Runs,
                ..
            })
        ),
        "{second:?}"
    );
    assert_eq!(
        engine
            .resources()
            .pool(Pool::Runs)
            .expect("listed")
            .in_use(),
        1
    );

    engine.set_device_rules(DeviceRules::default().low_battery(false));
    let second = asked.admit(Ask::new(Pool::Runs, "two")).await.unwrap();
    drop((first, second));

    engine.set_device_rules(DeviceRules::default());
    device.set_power(Power::OnMains);
    let first = asked.admit(Ask::new(Pool::Runs, "one")).await.unwrap();
    let second = asked.admit(Ask::new(Pool::Runs, "two")).await.unwrap();
    drop((first, second));
    timeout(WAIT, engine.shutdown()).await.unwrap().unwrap();
}
