//! Approvals in the embedded engine (ADR 0047 slice 5, over ADR 0050 slice H8): a host installs the
//! hook that asks (`EngineBuilder::plugin`), sees the run in `RunState::AwaitingApproval` with the
//! call it is asked about, hears `EngineEvent::ApprovalAsked` and `ApprovalAnswered`, and answers
//! with `Engine::approve`.
//!
//! The slice's proofs: a yes runs the same call once, across restarts of the engine over one
//! SQLite file; a replay never asks twice (the commit of the park is lost and the transition runs
//! again); a no refuses the call and the run goes on; and an answer for another call, or for a run
//! that awaits no approval, is refused with its class. Every wait is bounded, so a hang fails.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_engine::adam_a2a_runtime::task_id_for;
use adam_engine::adam_agent_fs::{Dir, Strictness};
use adam_engine::adam_assembly::AgentDef;
use adam_engine::adam_core::{DynStore, MemoryStore, RunId};
use adam_engine::adam_error::{Classify, ErrorClass};
use adam_engine::adam_llm_agent::{
    Decision as PluginDecision, FnTool, HookResult, HookedCall, Plugin, REDACTED_MARKER, StepIo,
    ToolError, ToolOutput, ToolSet,
};
use adam_engine::adam_model::{
    DynModel, InputModalities, Message as ModelMessage, MockModel, ModelClient, ModelDelta,
    ModelError, ModelRequest, ModelResponse, ToolCall,
};
use adam_engine::{
    ApprovalRequest, Decision, Delivered, Engine, EngineBuilder, EngineError, EngineEvent, Limits,
    RunEvents, RunInfo, RunState, Say,
};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(30);

/// What the hook asks, in its own words. It names the secret on purpose: the host reads it through
/// the steps' redactor.
const QUESTION: &str = "may I write the file? (token hunter2-hunter2)";

/// The one secret of these tests, which the host's redactor knows.
const SECRET: &str = "hunter2-hunter2";

/// The agent: the host's `effect` tool and the engine's `ask_user`.
fn agent() -> AgentDef {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/instructions.md"),
        "---\nname: chat\ntools: [effect, ask_user]\n---\nYou chat.\n",
    )
    .unwrap();
    AgentDef::from_source(&Dir::new(dir.path()), Strictness::Lenient)
        .unwrap()
        .remove(0)
}

/// What the host's doubles count: how often its hook asked, and how often the tool ran.
#[derive(Clone, Default)]
struct Counters {
    asked: Arc<AtomicUsize>,
    runs: Arc<AtomicUsize>,
}

/// The host's hook: asks the person before every call of `effect`.
struct AsksBeforeEffect(Arc<AtomicUsize>);

impl Plugin for AsksBeforeEffect {
    fn name(&self) -> &str {
        "asks"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        if call.tool != "effect" {
            return Ok(PluginDecision::Continue);
        }
        self.0.fetch_add(1, SeqCst);
        Ok(PluginDecision::Ask(QUESTION.into()))
    }
}

/// The host's tool with a side effect: it counts its runs.
fn effect(runs: Arc<AtomicUsize>) -> FnTool {
    FnTool::raw(
        "effect",
        "Write the file.",
        json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        move |_ctx, _args| {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, SeqCst);
                Ok::<_, ToolError>(ToolOutput::text("written"))
            }
        },
    )
}

fn effect_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "effect".into(),
        arguments: json!({ "path": "notes.txt", "token": SECRET }),
        ..Default::default()
    }
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

    fn input_modalities(&self, model: &str) -> InputModalities {
        self.inner.input_modalities(model)
    }
}

/// An engine over `store` with the host's hook and tool, whose model is `mock` with its
/// `hold_on`-th call held at `gate`; `more` adds to the builder.
struct Rig {
    engine: Engine,
    mock: Arc<MockModel>,
    gate: Arc<Gate>,
}

async fn rig(
    store: DynStore,
    counters: &Counters,
    hold_on: Option<usize>,
    script: impl FnOnce(&MockModel),
    more: impl FnOnce(EngineBuilder) -> EngineBuilder,
) -> Rig {
    let mock = Arc::new(MockModel::new());
    script(&mock);
    let gate = Arc::new(Gate::default());
    let model: DynModel = Arc::new(GateModel {
        inner: mock.clone(),
        hold_on,
        calls: AtomicUsize::new(0),
        gate: gate.clone(),
    });
    let builder = Engine::builder(store, agent(), model, "m")
        .tools(ToolSet::new().tool(effect(counters.runs.clone())))
        .plugin(AsksBeforeEffect(counters.asked.clone()));
    let engine = more(builder).start().await.expect("the engine starts");
    Rig { engine, mock, gate }
}

/// The run once it is in `state`.
async fn wait(engine: &Engine, run: RunId, state: RunState) -> RunInfo {
    let last = std::sync::Mutex::new(None);
    timeout(WAIT, async {
        loop {
            let info = engine.get(run).await.unwrap().expect("the run exists");
            if info.state == state {
                return info;
            }
            *last.lock().unwrap() = Some(info);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "run {run} never reached {state:?}: {:?}",
            last.lock().unwrap()
        )
    })
}

/// The events of `events` up to and including the first one `last` accepts.
async fn read_until(
    events: &mut RunEvents,
    last: impl Fn(&EngineEvent) -> bool,
) -> Vec<EngineEvent> {
    let mut got = Vec::new();
    loop {
        match timeout(WAIT, events.next()).await {
            Ok(Some(Ok(event))) => {
                let done = last(&event);
                got.push(event);
                if done {
                    return got;
                }
            }
            Ok(Some(Err(error))) => panic!("the stream failed with {error:?} after {got:?}"),
            Ok(None) => panic!("the stream ended after {got:?}"),
            Err(_) => panic!("the stream said nothing more after {got:?}"),
        }
    }
}

/// Every event `events` says within `quiet`, the stream still open.
async fn read_for(events: &mut RunEvents, quiet: Duration) -> Vec<EngineEvent> {
    let mut got = Vec::new();
    let until = tokio::time::Instant::now() + quiet;
    while let Ok(next) = tokio::time::timeout_at(until, events.next()).await {
        match next {
            Some(Ok(event)) => got.push(event),
            other => panic!("the stream ended with {other:?} after {got:?}"),
        }
    }
    got
}

fn asked_of(events: &[EngineEvent]) -> Vec<&ApprovalRequest> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ApprovalAsked(request) => Some(request),
            _ => None,
        })
        .collect()
}

fn answered_of(events: &[EngineEvent]) -> Vec<(&str, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::ApprovalAnswered { call_id, approved } => {
                Some((call_id.as_str(), *approved))
            }
            _ => None,
        })
        .collect()
}

/// The results the model was given for the call `id` in `request`.
fn results_for<'a>(request: &'a ModelRequest, id: &str) -> Vec<&'a ModelMessage> {
    request
        .messages
        .iter()
        .filter(|m| matches!(m, ModelMessage::Tool { call_id, .. } if call_id == id))
        .collect()
}

/// The request is the approval of `effect_call("c1")`, with `arguments` as shown.
fn assert_request(request: &ApprovalRequest, question: &str, arguments: &Value) {
    assert_eq!(request.call_id, "c1");
    assert_eq!(request.tool, "effect");
    assert_eq!(request.question, question);
    assert_eq!(
        request.arguments.as_ref().map(|a| Value::Object(a.clone())),
        Some(arguments.clone())
    );
}

// ---------------------------------------------------------------------------
// 1. A yes runs the same call once, across restarts (SQLite)
// ---------------------------------------------------------------------------

/// Engine A parks on the approval and is dropped; engine B, over the same SQLite file, shows the
/// same approval without asking the hook again, takes the yes, runs the call and is dropped while
/// its next model call waits; engine C finishes the run. The tool ran once, the hook asked once,
/// and C's model reads the call's result once.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_yes_runs_the_same_call_once_across_restarts() {
    use adam_engine::adam_core::TurnKeys;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adam.sqlite");
    let keys = <dyn TurnKeys>::ephemeral().expect("a key");
    let counters = Counters::default();
    // The store of one engine, and a way to know it was let go: the file's lock is the process's
    // open store, and the next engine opens the file once the last one is gone.
    let open = || {
        let sqlite = Arc::new(adam_store_sqlite::SqliteStore::open(&path).expect("the file"));
        let gone = Arc::downgrade(&sqlite);
        let store: DynStore = sqlite;
        (store, gone)
    };
    let released = |gone: std::sync::Weak<adam_store_sqlite::SqliteStore>| async move {
        timeout(WAIT, async {
            while gone.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the dropped engine let its store go");
    };
    let with_keys = |keys: adam_engine::adam_core::DynTurnKeys| {
        move |builder: EngineBuilder| builder.turn_keys(keys)
    };

    // A: the model calls the tool; the hook asks; the run parks.
    let (store, gone) = open();
    let a = rig(
        store,
        &counters,
        None,
        |mock| {
            mock.push_tool_calls(vec![effect_call("c1")]);
        },
        with_keys(keys.clone()),
    )
    .await;
    let turn = a.engine.send(None, Say::new("write it")).await.unwrap();
    let parked = wait(&a.engine, turn.run, RunState::AwaitingApproval).await;
    let arguments = json!({ "path": "notes.txt", "token": SECRET });
    assert_request(parked.approval.as_ref().unwrap(), QUESTION, &arguments);
    let prompt = parked.question.as_deref().unwrap();
    assert!(prompt.contains("Approve the call to `effect`?"), "{prompt}");
    assert_eq!(counters.runs.load(SeqCst), 0, "nothing runs before the yes");
    assert_eq!(counters.asked.load(SeqCst), 1);
    drop(a);
    released(gone).await;

    // B: the same approval, from the store; the hook is not asked again.
    let (store, gone) = open();
    let b = rig(
        store,
        &counters,
        Some(0),
        |mock| {
            mock.push_text("never said by B");
        },
        with_keys(keys.clone()),
    )
    .await;
    let again = b.engine.get(turn.run).await.unwrap().unwrap();
    assert_eq!(again.state, RunState::AwaitingApproval);
    assert_eq!(
        again.approval, parked.approval,
        "the same call, after a restart"
    );
    let mut events = b.engine.events(turn.run).await.unwrap();
    let first = timeout(WAIT, events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(&first, EngineEvent::ApprovalAsked(request) if Some(request) == parked.approval.as_ref()),
        "a stream opened on a waiting run starts with its approval: {first:?}"
    );

    b.engine
        .approve(turn.run, "c1", Decision::Approve)
        .await
        .expect("the yes is taken");
    // The call ran; the model call after it waits at the gate.
    b.gate.wait_reached().await;
    assert_eq!(counters.runs.load(SeqCst), 1, "the approved call ran");
    let heard = read_until(&mut events, |e| {
        matches!(e, EngineEvent::ApprovalAnswered { .. })
    })
    .await;
    assert_eq!(answered_of(&heard), [("c1", true)]);
    let Rig {
        engine,
        gate: _held,
        ..
    } = b;
    drop(engine);
    drop(events);
    released(gone).await;

    // C: the run goes on from the store; the approved call is not run again.
    let (store, gone) = open();
    let c = rig(
        store,
        &counters,
        None,
        |mock| {
            mock.push_text("written, done");
        },
        with_keys(keys),
    )
    .await;
    let done = wait(&c.engine, turn.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("written, done"));
    assert_eq!(
        counters.runs.load(SeqCst),
        1,
        "the approved call ran once across the restarts"
    );
    assert_eq!(counters.asked.load(SeqCst), 1, "the hook asked once");
    let requests = c.mock.requests();
    assert_eq!(
        requests.len(),
        1,
        "C called its model for the next step only"
    );
    let results = results_for(&requests[0], "c1");
    assert_eq!(
        results.len(),
        1,
        "one result for the call: {:?}",
        requests[0].messages
    );
    assert_eq!(results[0].text(), "written");
    timeout(WAIT, c.engine.shutdown()).await.unwrap().unwrap();
    drop(c);
    released(gone).await;
}

// ---------------------------------------------------------------------------
// 2. A replay never asks twice
// ---------------------------------------------------------------------------

/// The commit of the transition that parks on the approval is lost: the transition runs again from
/// the journal and parks on the same approval without asking the hook, and the stream says the
/// approval once. After the yes the call runs once, and nothing asks again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replay_never_asks_twice() {
    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = faulty.clone();
    let counters = Counters::default();
    let rig = rig(
        store,
        &counters,
        None,
        |mock| {
            mock.push_tool_calls(vec![effect_call("c1")])
                .push_text("written");
        },
        // The faulty store does not say it dies with the process: it needs a turn key. A lost
        // commit leaves the run leased: a short lease lets the worker claim it again soon.
        |builder| {
            let mut limits = Limits::default();
            limits.lease_ttl = Duration::from_millis(300);
            builder
                .limits(limits)
                .turn_keys(<dyn adam_engine::adam_core::TurnKeys>::ephemeral().expect("a key"))
        },
    )
    .await;
    // The run's id is the one the engine derives from the message: its first commit, the park,
    // never reaches the store.
    let say = Say::with_id("say-1", "write it");
    let run = task_id_for("chat", "adam-engine", None, &say.id);
    faulty.fail_run(Method::CommitRun, run, 1);
    let turn = rig.engine.send(None, say).await.unwrap();
    assert_eq!(turn.run, run);
    // Read at once: the ask of the attempt whose commit is lost is heard while the store does not
    // have the park yet, and must not be taken for an answer.
    let mut events = rig.engine.events(run).await.unwrap();
    let mut heard = read_until(&mut events, |e| matches!(e, EngineEvent::ApprovalAsked(_))).await;
    let parked = wait(&rig.engine, run, RunState::AwaitingApproval).await;
    assert_eq!(
        faulty.injected(Method::CommitRun),
        1,
        "the park's commit was lost, and the transition ran again"
    );
    assert_eq!(
        counters.asked.load(SeqCst),
        1,
        "the replay read the ask from the call's step, and asked no hook"
    );
    assert_eq!(
        rig.mock.requests().len(),
        1,
        "the model turn was replayed too"
    );
    // Longer than the stream's poll of the store, while the run waits: the replayed ask, and every
    // read of the parked run, offer the approval no second time.
    heard.extend(read_for(&mut events, Duration::from_millis(2_500)).await);

    let info = rig
        .engine
        .approve(run, "c1", Decision::Approve)
        .await
        .unwrap();
    assert_ne!(info.state, RunState::AwaitingApproval);
    heard.extend(read_until(&mut events, |e| matches!(e, EngineEvent::Ended(_))).await);
    assert_eq!(
        asked_of(&heard),
        [parked.approval.as_ref().unwrap()],
        "the stream said the approval once: {heard:?}"
    );
    assert_eq!(answered_of(&heard), [("c1", true)]);
    let Some(EngineEvent::Ended(done)) = heard.last() else {
        unreachable!()
    };
    assert_eq!(done.state, RunState::Done);
    assert_eq!(counters.runs.load(SeqCst), 1, "the call ran once");
    assert_eq!(
        counters.asked.load(SeqCst),
        1,
        "the yes asked nothing again"
    );
    timeout(WAIT, rig.engine.shutdown()).await.unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// 3. A no refuses the call and the run goes on
// ---------------------------------------------------------------------------

/// The person refuses with a reason: the call never runs, the model reads that it was refused with
/// the person's words, and the run goes on to its answer. What the host was shown went through its
/// redactor: the arguments, the hook's question, and every event of the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_no_refuses_the_call_and_the_run_goes_on() {
    let counters = Counters::default();
    let rig = rig(
        Arc::new(MemoryStore::new()),
        &counters,
        None,
        |mock| {
            mock.push_tool_calls(vec![effect_call("c1")])
                .push_text("I left the file alone.");
        },
        |builder| {
            builder.step_io(StepIo::default().redact(|text| text.replace(SECRET, REDACTED_MARKER)))
        },
    )
    .await;
    let turn = rig.engine.send(None, Say::new("write it")).await.unwrap();
    let mut events = rig.engine.events(turn.run).await.unwrap();
    let parked = wait(&rig.engine, turn.run, RunState::AwaitingApproval).await;
    let shown = QUESTION.replace(SECRET, REDACTED_MARKER);
    assert_request(
        parked.approval.as_ref().unwrap(),
        &shown,
        &json!({ "path": "notes.txt", "token": REDACTED_MARKER }),
    );
    assert!(!parked.question.as_deref().unwrap().contains(SECRET));
    let mut heard = read_until(&mut events, |e| matches!(e, EngineEvent::ApprovalAsked(_))).await;
    assert_eq!(asked_of(&heard), [parked.approval.as_ref().unwrap()]);

    rig.engine
        .approve(turn.run, "c1", Decision::refuse_because("not that file"))
        .await
        .unwrap();
    heard.extend(read_until(&mut events, |e| matches!(e, EngineEvent::Ended(_))).await);
    assert_eq!(asked_of(&heard).len(), 1, "{heard:?}");
    assert_eq!(answered_of(&heard), [("c1", false)]);
    let Some(EngineEvent::Ended(done)) = heard.last() else {
        unreachable!()
    };
    assert_eq!(done.output.as_deref(), Some("I left the file alone."));
    assert_eq!(counters.runs.load(SeqCst), 0, "a refused call never runs");
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2);
    let results = results_for(&requests[1], "c1");
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0], ModelMessage::Tool { is_error: true, .. }),
        "{results:?}"
    );
    assert!(
        results[0].text().contains("They answered: not that file"),
        "{results:?}"
    );
    let said = format!("{heard:?}");
    assert!(!said.contains(SECRET), "a secret reached the host: {said}");
    timeout(WAIT, rig.engine.shutdown()).await.unwrap().unwrap();
}

/// A refusal whose reason would read as a yes is still a refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_that_says_ok_is_still_a_no() {
    let counters = Counters::default();
    let rig = rig(
        Arc::new(MemoryStore::new()),
        &counters,
        None,
        |mock| {
            mock.push_tool_calls(vec![effect_call("c1")])
                .push_text("fine");
        },
        |builder| builder,
    )
    .await;
    let turn = rig.engine.send(None, Say::new("write it")).await.unwrap();
    wait(&rig.engine, turn.run, RunState::AwaitingApproval).await;
    rig.engine
        .approve(turn.run, "c1", Decision::refuse_because("OK"))
        .await
        .unwrap();
    wait(&rig.engine, turn.run, RunState::Done).await;
    assert_eq!(counters.runs.load(SeqCst), 0);
    let requests = rig.mock.requests();
    assert!(
        results_for(&requests[1], "c1")[0]
            .text()
            .contains("They answered: no: OK"),
        "{:?}",
        requests[1].messages
    );
    timeout(WAIT, rig.engine.shutdown()).await.unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// 4. An answer for another call, or for a run that awaits none, is refused
// ---------------------------------------------------------------------------

/// `approve` names the call it answers: another call's id, a run that works, a run that waits on a
/// plain question, a run that ended and a run nobody has are each refused with their class, and
/// nothing reaches the run. A plain message is still an answer: `send` of a clear yes approves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_for_another_call_or_no_approval_is_refused() {
    let counters = Counters::default();
    let rig = rig(
        Arc::new(MemoryStore::new()),
        &counters,
        // The third call: the model's first turn of the second run, held while it works.
        Some(2),
        |mock| {
            mock.push_tool_calls(vec![effect_call("c1")])
                .push_text("written")
                .push_tool_calls(vec![ToolCall {
                    id: "ask-1".into(),
                    name: "ask_user".into(),
                    arguments: json!({ "question": "which file?" }),
                    ..Default::default()
                }]);
        },
        |builder| builder,
    )
    .await;
    let turn = rig.engine.send(None, Say::new("write it")).await.unwrap();
    wait(&rig.engine, turn.run, RunState::AwaitingApproval).await;

    let other = rig
        .engine
        .approve(turn.run, "c9", Decision::Approve)
        .await
        .unwrap_err();
    assert_eq!(other.class(), ErrorClass::Rejected, "{other}");
    assert!(
        matches!(&other, EngineError::NotAwaitingApproval { call_id, awaited: Some(awaited), state: RunState::AwaitingApproval, .. }
            if call_id == "c9" && awaited == "c1"),
        "{other:?}"
    );
    let still = rig.engine.get(turn.run).await.unwrap().unwrap();
    assert_eq!(
        still.state,
        RunState::AwaitingApproval,
        "nothing was delivered"
    );
    assert_eq!(rig.mock.requests().len(), 1);

    // A clear yes typed as a message is the approval's answer.
    let yes = rig
        .engine
        .send(Some(&turn.conversation), Say::new("Yes."))
        .await
        .unwrap();
    assert_eq!(yes.delivered, Delivered::Answered);
    wait(&rig.engine, turn.run, RunState::Done).await;
    assert_eq!(counters.runs.load(SeqCst), 1);

    let ended = rig
        .engine
        .approve(turn.run, "c1", Decision::Approve)
        .await
        .unwrap_err();
    assert!(matches!(ended, EngineError::Finished { .. }), "{ended:?}");
    assert_eq!(ended.class(), ErrorClass::Rejected);
    let nobody = rig
        .engine
        .approve(RunId::new(), "c1", Decision::Approve)
        .await
        .unwrap_err();
    assert_eq!(nobody.class(), ErrorClass::NotFound, "{nobody}");

    // A run that works awaits no approval.
    let second = rig
        .engine
        .send(Some(&turn.conversation), Say::new("and now?"))
        .await
        .unwrap();
    rig.gate.wait_reached().await;
    let working = rig
        .engine
        .approve(second.run, "c1", Decision::Approve)
        .await
        .unwrap_err();
    assert!(
        matches!(&working, EngineError::NotAwaitingApproval { awaited: None, state, .. }
            if matches!(state, RunState::Working | RunState::Submitted)),
        "{working:?}"
    );
    rig.gate.release.notify_one();

    // Nor does a run that waits on a plain question.
    let question = wait(&rig.engine, second.run, RunState::Waiting).await;
    assert_eq!(question.question.as_deref(), Some("which file?"));
    assert_eq!(question.approval, None);
    let asking = rig
        .engine
        .approve(second.run, "ask-1", Decision::Approve)
        .await
        .unwrap_err();
    assert!(
        matches!(
            &asking,
            EngineError::NotAwaitingApproval {
                awaited: None,
                state: RunState::Waiting,
                ..
            }
        ),
        "{asking:?}"
    );
    assert_eq!(asking.class(), ErrorClass::Rejected);
    assert_eq!(
        rig.engine.get(second.run).await.unwrap().unwrap().state,
        RunState::Waiting,
        "the question still waits for its answer"
    );
    timeout(WAIT, rig.engine.shutdown()).await.unwrap().unwrap();
}
