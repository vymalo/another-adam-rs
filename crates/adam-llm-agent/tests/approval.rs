//! A hook's `Ask` parks the run for a person's approval (ADR 0050, decision 12, slice H8).
//!
//! The four decisive cases of the slice: an approved call runs once across a crash (its step
//! `tool:<call id>:approved` is journaled, and a replay past it runs nothing); a replay never asks
//! twice (the step that asked is recorded, so neither the hook nor the person is asked again); a
//! child run reads `Ask` as `Deny`; and a no refuses the call with the person's words while the run
//! goes on.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, HookDecision, HookResult, HookedCall, HookedOutput, LlmAgent, NOT_APPROVABLE,
    Plugin, Tool, ToolCtx, ToolError, ToolOutput, Verdict, approval_step, approves, user_message,
};
use adam_model::{Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{Inbound, RunView, Runtime, RuntimeBuilder};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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
        parameters: json!({"type": "object"}),
    }
}

fn runtime(store: &DynStore, agent: &LlmAgent, worker: &str) -> Runtime {
    runtime_with(store, agent, worker, |b| b)
}

fn runtime_with(
    store: &DynStore,
    agent: &LlmAgent,
    worker: &str,
    f: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
) -> Runtime {
    f(Runtime::builder(store.clone())
        .agent(agent.clone())
        .worker_id(worker)
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_secs(10)))
    .build()
}

fn short_lease(b: RuntimeBuilder) -> RuntimeBuilder {
    b.lease_ttl(Duration::from_millis(300))
}

struct Worker {
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
}

fn spawn_worker(rt: &Runtime) -> Worker {
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
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
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if pred(&view) {
            return view;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "done", |v| v.status == RunStatus::Done).await
}

/// The run is parked on the approval of the call `id`.
async fn wait_asking(rt: &Runtime, run: RunId, id: &str) -> RunView {
    wait_for(rt, run, "parked on the approval", |v| {
        v.status == RunStatus::Parked
            && v.waiting
            && v.state.pointer("/approval/call_id").and_then(Value::as_str) == Some(id)
    })
    .await
}

async fn conversation(rt: &Runtime, view: &RunView) -> Conversation {
    let state = rt.state(view.id).await.expect("state").expect("the run");
    serde_json::from_value(state).expect("state is a Conversation")
}

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn tool_of<'a>(messages: &'a [Message], id: &str) -> &'a Message {
    messages
        .iter()
        .find(|message| matches!(message, Message::Tool { call_id, .. } if call_id == id))
        .unwrap_or_else(|| panic!("no result for {id}: {messages:?}"))
}

fn is_error(message: &Message) -> bool {
    matches!(message, Message::Tool { is_error: true, .. })
}

/// The names of the journal's steps, in seq order.
async fn steps(store: &DynStore, run: RunId) -> Vec<String> {
    store
        .journal_list(run)
        .await
        .expect("journal")
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

async fn step_record(store: &DynStore, run: RunId, step: &str) -> HookedOutput {
    let journal = store.journal_list(run).await.expect("journal");
    let entry = journal
        .iter()
        .find(|entry| entry.name == step)
        .unwrap_or_else(|| panic!("no step {step} in {journal:?}"));
    serde_json::from_value(entry.payload.clone())
        .unwrap_or_else(|e| panic!("the record of {step} does not decode as HookedOutput: {e}"))
}

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// Asks a person to approve every call of `effect`, and counts how often it was asked.
struct AsksForEffect {
    asked: Arc<AtomicUsize>,
}

impl Plugin for AsksForEffect {
    fn name(&self) -> &str {
        "asks"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        if call.tool != "effect" {
            return Ok(Verdict::Continue);
        }
        self.asked.fetch_add(1, SeqCst);
        Ok(Verdict::Ask("may I write the file?".into()))
    }
}

/// Marks the arguments of every call, before the plugin that asks: what a person approves runs
/// with the arguments the hooks left.
struct Marks;

impl Plugin for Marks {
    fn name(&self) -> &str {
        "marks"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        if let Some(object) = call.arguments.as_object_mut() {
            object.insert("marked".into(), Value::Bool(true));
        }
        Ok(Verdict::Continue)
    }
}

/// A side effect: counts its runs and keeps the arguments it ran with.
struct Effect {
    runs: Arc<AtomicUsize>,
    ran_with: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Tool for Effect {
    fn spec(&self) -> ToolSpec {
        spec("effect")
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, SeqCst);
        self.ran_with.lock().unwrap().push(args);
        Ok(ToolOutput::text("effect-out"))
    }
}

/// Blocks the first time it is called, so the worker dies in it: the step before it is journaled,
/// this one never is, and the survivor runs it again and answers at once.
struct Stalls {
    reached: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Stalls {
    fn spec(&self) -> ToolSpec {
        spec("after")
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("after-out"))
    }
}

struct Rig {
    mock: Arc<MockModel>,
    asked: Arc<AtomicUsize>,
    runs: Arc<AtomicUsize>,
    ran_with: Arc<Mutex<Vec<Value>>>,
}

impl Rig {
    fn new() -> Self {
        Self {
            mock: Arc::new(MockModel::new()),
            asked: Arc::new(AtomicUsize::new(0)),
            runs: Arc::new(AtomicUsize::new(0)),
            ran_with: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn agent(&self) -> adam_llm_agent::LlmAgentBuilder {
        LlmAgent::builder("llm", self.mock.clone(), "test-model")
            .tool(Effect {
                runs: self.runs.clone(),
                ran_with: self.ran_with.clone(),
            })
            .plugin(Marks)
            .plugin(AsksForEffect {
                asked: self.asked.clone(),
            })
    }
}

// ---------------------------------------------------------------------------
// 1. An approved call runs once across a crash
// ---------------------------------------------------------------------------

/// The person approves; the worker runs the approved call (its step `tool:c1:approved` is
/// journaled) and dies in the next call of the same transition. The survivor replays the
/// transition: the approved step is recorded, so the side effect stays at one, and the hook is
/// never asked again.
#[tokio::test]
async fn an_approved_call_runs_once_across_a_crash() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let rig = Rig::new();
    let reached = Arc::new(Notify::new());
    let after_calls = Arc::new(AtomicUsize::new(0));
    let agent = rig
        .agent()
        .tool(Stalls {
            reached: reached.clone(),
            calls: after_calls.clone(),
        })
        .build();
    rig.mock
        .push_tool_calls(vec![
            call("c1", "effect", json!({"path": "a.txt"})),
            call("c2", "after", json!({})),
        ])
        .push_text("both done");
    let (rt_a, rt_b) = (
        runtime_with(&store, &agent, "crash-a", short_lease),
        runtime_with(&store, &agent, "crash-b", short_lease),
    );
    let run = rt_a
        .start("llm", user_message("write it"), None)
        .await
        .expect("start");
    let doomed = spawn_worker(&rt_a);
    let parked = wait_asking(&rt_a, run, "c1").await;
    assert_eq!(rig.runs.load(SeqCst), 0, "nothing runs before the yes");
    let question = parked
        .state
        .pointer("/pending_wait/question")
        .and_then(Value::as_str)
        .expect("the question the person reads");
    assert!(question.contains("may I write the file?"), "{question}");
    assert!(question.contains("`effect`"), "{question}");

    rt_a.deliver(run, user_message("yes"))
        .await
        .expect("deliver");
    notified(&reached, "the call after the approved one starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;
    assert_eq!(rig.runs.load(SeqCst), 1, "the approved call ran");

    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(
        rig.runs.load(SeqCst),
        1,
        "the replay found `tool:c1:approved` recorded and ran the call no second time"
    );
    assert_eq!(rig.asked.load(SeqCst), 1, "the hook was asked once");
    assert_eq!(
        after_calls.load(SeqCst),
        2,
        "the call that was never recorded ran again"
    );
    assert_eq!(
        rig.ran_with.lock().unwrap().as_slice(),
        &[json!({"path": "a.txt", "marked": true})],
        "the approved call ran with the arguments the hooks left"
    );
    let journal = steps(&store, run).await;
    let approved = approval_step("c1");
    assert_eq!(
        journal.iter().filter(|name| **name == "tool:c1").count(),
        1,
        "{journal:?}"
    );
    assert_eq!(
        journal.iter().filter(|name| **name == approved).count(),
        1,
        "{journal:?}"
    );
    let asked = step_record(&store, run, "tool:c1").await;
    assert_eq!(asked.awaits_approval(), Some("may I write the file?"));
    assert_eq!(asked.hooks[0].verdict, HookDecision::ASK);
    assert_eq!(asked.args, Some(json!({"path": "a.txt", "marked": true})));
    let ran = step_record(&store, run, &approved).await;
    assert_eq!(ran.output.content, "effect-out");
    assert_eq!(ran.awaits_approval(), None);

    let state = conversation(&rt_b, &view).await;
    assert_eq!(tool_of(&state.messages, "c1").text(), "effect-out");
    assert_eq!(tool_of(&state.messages, "c2").text(), "after-out");
    assert!(state.approval.is_none(), "the approval is spent");
    assert_eq!(rig.mock.requests().len(), 2);
}

// ---------------------------------------------------------------------------
// 2. A replay never asks twice
// ---------------------------------------------------------------------------

/// The commit of the transition that parked on the approval is lost: the transition runs again from
/// the journal, finds `tool:c1` recorded with its ask, and parks on the same question without asking
/// the hook. Once the person has said yes, the run never parks on the approval again.
#[tokio::test]
async fn a_replay_never_asks_twice() {
    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = faulty.clone();
    let rig = Rig::new();
    let agent = rig.agent().build();
    rig.mock
        .push_tool_calls(vec![call("c1", "effect", json!({"path": "a.txt"}))])
        .push_text("written");
    let rt = runtime_with(&store, &agent, "w", short_lease);
    let run = rt
        .start("llm", user_message("write it"), None)
        .await
        .expect("start");
    // The first commit of the run is the park: it never reaches the store.
    faulty.fail_run(Method::CommitRun, run, 1);
    let worker = spawn_worker(&rt);
    wait_asking(&rt, run, "c1").await;
    assert_eq!(
        faulty.injected(Method::CommitRun),
        1,
        "the commit of the park was lost, and the transition ran again"
    );
    assert_eq!(
        rig.asked.load(SeqCst),
        1,
        "the replay read the ask from `tool:c1`, and asked no hook"
    );
    assert_eq!(
        rig.mock.requests().len(),
        1,
        "the model turn was replayed too"
    );

    rt.deliver(run, user_message("Yes."))
        .await
        .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(rig.runs.load(SeqCst), 1);
    assert_eq!(rig.asked.load(SeqCst), 1, "the yes asked nothing again");
    let state = conversation(&rt, &view).await;
    assert_eq!(tool_of(&state.messages, "c1").text(), "effect-out");
    let journal = steps(&store, run).await;
    for name in [
        "model:0".to_owned(),
        "tool:c1".to_owned(),
        approval_step("c1"),
    ] {
        assert_eq!(
            journal.iter().filter(|step| **step == name).count(),
            1,
            "{name} once: {journal:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. A child reads Ask as Deny
// ---------------------------------------------------------------------------

/// A child run cannot ask the person (ADR 0045 decision 9): the hook's ask is recorded as a denial,
/// the call is answered with why and with the question, nothing parks, and the run goes on.
#[tokio::test]
async fn a_child_reads_ask_as_deny() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let rig = Rig::new();
    let agent = rig.agent().build();
    rig.mock
        .push_tool_calls(vec![call("c1", "effect", json!({"path": "a.txt"}))])
        .push_text("did without it");
    let rt = runtime(&store, &agent, "w");
    // The payload a parent's `start_child` writes: it names the run at the root of the chain.
    let child_input = Inbound::new(
        adam_llm_agent::MESSAGE_KIND,
        json!({"text": "write it", "root_run": RunId::new().to_string()}),
    );
    let run = rt.start("llm", child_input, None).await.expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        rig.runs.load(SeqCst),
        0,
        "a child's ask never runs the call"
    );
    assert_eq!(rig.asked.load(SeqCst), 1);
    let state = conversation(&rt, &view).await;
    assert!(state.approval.is_none() && state.pending_wait.is_none());
    let answer = tool_of(&state.messages, "c1");
    assert!(is_error(answer));
    assert!(
        answer.text().starts_with(NOT_APPROVABLE),
        "{}",
        answer.text()
    );
    assert!(
        answer.text().contains("may I write the file?"),
        "{}",
        answer.text()
    );
    let record = step_record(&store, run, "tool:c1").await;
    assert_eq!(record.hooks.len(), 1, "{record:?}");
    assert_eq!(record.hooks[0].verdict, HookDecision::DENY, "{record:?}");
    assert_eq!(record.awaits_approval(), None);
    assert!(!steps(&store, run).await.contains(&approval_step("c1")));
}

// ---------------------------------------------------------------------------
// 4. No refuses the call with the reason
// ---------------------------------------------------------------------------

/// The person answers anything but a clear yes: the call is answered with the refusal and their
/// words, it never runs, and the run goes on to the model's next turn.
#[tokio::test]
async fn no_refuses_the_call_with_the_reason() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let rig = Rig::new();
    let agent = rig.agent().build();
    rig.mock
        .push_tool_calls(vec![call("c1", "effect", json!({"path": "a.txt"}))])
        .push_text("left it alone");
    let rt = runtime(&store, &agent, "w");
    let run = rt
        .start("llm", user_message("write it"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_asking(&rt, run, "c1").await;
    rt.deliver(run, user_message("no, not that file"))
        .await
        .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(rig.runs.load(SeqCst), 0, "a refused call never runs");
    let state = conversation(&rt, &view).await;
    let answer = tool_of(&state.messages, "c1");
    assert!(is_error(answer), "{answer:?}");
    assert!(
        answer
            .text()
            .contains("the person did not approve the call"),
        "{}",
        answer.text()
    );
    assert!(
        answer.text().contains("no, not that file"),
        "{}",
        answer.text()
    );
    assert!(state.approval.is_none());
    assert!(!steps(&store, run).await.contains(&approval_step("c1")));
    assert_eq!(
        rig.mock.requests().len(),
        2,
        "the model read the refusal and answered"
    );
    assert_eq!(
        view.output
            .as_ref()
            .and_then(|o| o.get("text"))
            .and_then(Value::as_str),
        Some("left it alone")
    );
}

// ---------------------------------------------------------------------------
// Around the four
// ---------------------------------------------------------------------------

/// Only a clear yes approves.
#[test]
fn only_a_clear_yes_approves() {
    for yes in [
        "yes", "Yes.", " YES! ", "y", "approve", "Approved", "allow", "ok", "okay",
    ] {
        assert!(approves(yes), "{yes:?}");
    }
    for no in [
        "",
        "no",
        "yes, but not that file",
        "maybe",
        "what does it do?",
        "nope",
        "yess",
    ] {
        assert!(!approves(no), "{no:?}");
    }
}

/// A record of an ask written before H8 holds no `approval`: it reads as an unrun call, as the
/// build that wrote it treated it, and a record written now still reads as an error result in a
/// build that predates the slice.
#[test]
fn an_ask_written_before_the_slice_reads_as_an_unrun_call() {
    let old = json!({
        "content": "awaiting approval: may I?",
        "is_error": true,
        "hooks": [{"point": "tool.before", "verdict": "ask", "reason": "may I?"}],
    });
    let read: HookedOutput = serde_json::from_value(old).unwrap();
    assert_eq!(read.awaits_approval(), None, "an old ask parks nothing");
    assert!(read.output.is_error);

    let mut new = HookedOutput::plain(ToolOutput::error("awaiting approval: may I?"));
    new.hooks = vec![HookDecision::new(
        HookDecision::TOOL_BEFORE,
        HookDecision::ASK,
        Some("may I?"),
    )];
    new.approval = Some("may I?".into());
    let json = serde_json::to_value(&new).unwrap();
    let as_old: ToolOutput = serde_json::from_value(json.clone()).unwrap();
    assert!(
        as_old.is_error,
        "a build before the slice reads the call as unrun"
    );
    let back: HookedOutput = serde_json::from_value(json).unwrap();
    assert_eq!(back, new);
}
