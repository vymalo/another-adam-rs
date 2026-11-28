//! The compiled hooks around a tool call (ADR 0050, slice H1): installed on the builder, run inside
//! the call's journaled step `tool:<call id>`, and their verdict recorded in that step.
//!
//! Four cases are the slice's proof: a replay past the step's journal write asks no hook again; a
//! rewrite that would change what the call touches is denied; a record written before hooks existed
//! still decodes, and a build without them reads a record written now; and each call of a turn of
//! four that run at once gets its own hooks.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, Decision, HookDecision, HookResult, HookSet, HookedCall, HookedModelRequest,
    HookedOutput, HookedText, LlmAgent, Plugin, RecordedEvent, Tool, ToolAccess, ToolCtx,
    ToolError, ToolOutput, user_message,
};
use adam_model::{Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{RunView, Runtime, RuntimeBuilder};
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

struct Harness {
    store: DynStore,
    mock: Arc<MockModel>,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            mock: Arc::new(MockModel::new()),
        }
    }

    fn agent(&self) -> adam_llm_agent::LlmAgentBuilder {
        LlmAgent::builder("llm", self.mock.clone(), "test-model")
    }

    fn runtime_with(
        &self,
        agent: &LlmAgent,
        worker: &str,
        f: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Runtime {
        f(Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .worker_id(worker)
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10)))
        .build()
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        self.runtime_with(agent, "w", |b| b)
    }
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

async fn wait_failed(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "failed", |v| v.status == RunStatus::Failed).await
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

fn short_lease(b: RuntimeBuilder) -> RuntimeBuilder {
    b.lease_ttl(Duration::from_millis(300))
}

/// The result message of the call `id`.
fn tool_of<'a>(messages: &'a [Message], id: &str) -> &'a Message {
    messages
        .iter()
        .find(|message| matches!(message, Message::Tool { call_id, .. } if call_id == id))
        .unwrap_or_else(|| panic!("no result for {id}: {messages:?}"))
}

/// Whether a tool message reports a failure.
fn is_error(message: &Message) -> bool {
    matches!(message, Message::Tool { is_error: true, .. })
}

/// The `tool:<call id>` entry of `run`, as the step records it.
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

/// A plugin that lets every call through, counting the calls of each tool before and after it.
#[derive(Default)]
struct Counts {
    before: Mutex<HashMap<String, usize>>,
    after: Mutex<HashMap<String, usize>>,
}

impl Counts {
    fn of(map: &Mutex<HashMap<String, usize>>, tool: &str) -> usize {
        map.lock().unwrap().get(tool).copied().unwrap_or(0)
    }

    fn before(&self, tool: &str) -> usize {
        Self::of(&self.before, tool)
    }

    fn after(&self, tool: &str) -> usize {
        Self::of(&self.after, tool)
    }
}

impl Plugin for Counts {
    fn name(&self) -> &str {
        "counts"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        *self
            .before
            .lock()
            .unwrap()
            .entry(call.tool.to_owned())
            .or_default() += 1;
        Ok(Decision::Continue)
    }

    fn after_tool(&self, call: &HookedCall<'_>, _output: &mut HookedText<'_>) -> HookResult {
        *self
            .after
            .lock()
            .unwrap()
            .entry(call.tool.to_owned())
            .or_default() += 1;
        Ok(Decision::Continue)
    }
}

/// A plugin that marks every call's arguments and every answer, keeping nothing else.
struct Marks;

impl Plugin for Marks {
    fn name(&self) -> &str {
        "marks"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        if let Some(object) = call.arguments.as_object_mut() {
            object.insert("marked".into(), Value::Bool(true));
        }
        Ok(Decision::Continue)
    }

    fn after_tool(&self, _call: &HookedCall<'_>, output: &mut HookedText<'_>) -> HookResult {
        output.text.push('!');
        Ok(Decision::Continue)
    }
}

/// A plugin that refuses, or stops, with a fixed reason.
struct Refuses {
    verdict: fn(String) -> Decision,
    reason: &'static str,
}

impl Plugin for Refuses {
    fn name(&self) -> &str {
        "refuses"
    }

    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        Ok((self.verdict)(self.reason.to_owned()))
    }
}

/// A plugin that rewrites the `use` argument, which the `Probe` tool reads its [`ToolAccess`] from.
struct RewritesUse(&'static str);

impl Plugin for RewritesUse {
    fn name(&self) -> &str {
        "rewrites-use"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        if let Some(object) = call.arguments.as_object_mut() {
            object.insert("use".into(), Value::String(self.0.to_owned()));
        }
        Ok(Decision::Continue)
    }
}

/// Answers `"<name>-out"` and counts its calls.
struct Echo {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

impl Echo {
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
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        spec(self.name)
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        Ok(ToolOutput::text(format!("{}-out", self.name)))
    }
}

/// Blocks until the run is cancelled the first time it is called, and answers at once after that:
/// the worker that dies in it never records its step, the survivor runs it again.
struct Stalls {
    reached: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Stalls {
    fn spec(&self) -> ToolSpec {
        spec("b")
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("b-out"))
    }
}

/// A tool that declares its access from its `use` argument, so a rewrite can change it: `independent`
/// shares nothing, anything else runs alone. It counts whether it ever ran.
struct Probe {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        spec("probe")
    }

    fn access(&self, args: &Value) -> ToolAccess {
        if args.get("use").and_then(Value::as_str) == Some("independent") {
            ToolAccess::Independent
        } else {
            ToolAccess::Alone
        }
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        Ok(ToolOutput::text(serde_json::to_string(&args).unwrap()))
    }
}

// ---------------------------------------------------------------------------
// 1. A replay past the step's journal write asks no hook again
// ---------------------------------------------------------------------------

/// The first call's step is journaled, then the worker dies inside the second call's step. The
/// survivor replays the journal: the first call's hooks are not asked again (its counter stays at
/// 1), while the second call, which was never recorded, runs — and hooks — again (2).
#[tokio::test]
async fn a_replay_past_the_steps_journal_write_asks_no_hook_again() {
    let h = Harness::new();
    let (a, a_calls) = Echo::new("a");
    let b_calls = Arc::new(AtomicUsize::new(0));
    let reached = Arc::new(Notify::new());
    let counts = Arc::new(Counts::default());
    let agent = h
        .agent()
        .tool(a)
        .tool(Stalls {
            reached: reached.clone(),
            calls: b_calls.clone(),
        })
        .hooks(HookSet::new().with(counts.clone()))
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
    notified(&reached, "the second call starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;
    assert_eq!(a_calls.load(SeqCst), 1);

    let survivor = spawn_worker(&rt_b);
    let view = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(
        counts.before("a"),
        1,
        "the recorded step is replayed: its hooks are not asked again"
    );
    assert_eq!(
        counts.after("a"),
        1,
        "the recorded step's after hook is replayed too"
    );
    assert_eq!(
        counts.before("b"),
        2,
        "the call that was never recorded runs (and is hooked) again"
    );
    assert_eq!(
        a_calls.load(SeqCst),
        1,
        "the recorded tool did not run twice"
    );
    assert_eq!(b_calls.load(SeqCst), 2, "the in-flight tool ran again");
    assert_eq!(
        h.mock.requests().len(),
        2,
        "the recorded model turn is replayed, the closing turn asked"
    );
    let messages = conversation(&rt_b, &view).await.messages;
    assert_eq!(messages[2].text(), "a-out");
    assert_eq!(messages[3].text(), "b-out");
}

// ---------------------------------------------------------------------------
// 2. A rewrite that changes Tool::access is denied
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rewrite_that_changes_what_the_call_touches_is_denied() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Probe {
            calls: calls.clone(),
        })
        .plugin(RewritesUse("alone"))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "probe", json!({"use": "independent"}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        calls.load(SeqCst),
        0,
        "a call whose access a hook changed never runs"
    );
    let messages = conversation(&rt, &view).await.messages;
    assert_eq!(
        messages.len(),
        4,
        "the call, its refusal, and the closing exchange: {messages:?}"
    );
    let refusal = messages[2].text();
    assert!(
        refusal.contains("the schedule of the turn was not made from"),
        "{refusal}"
    );
    let record = step_record(&h.store, run, "tool:c1").await;
    assert!(record.output.is_error, "{record:?}");
    assert_eq!(record.hooks.len(), 1, "{record:?}");
    assert_eq!(record.hooks[0].point, HookDecision::TOOL_BEFORE);
    assert_eq!(record.hooks[0].verdict, HookDecision::FAILED);
    assert!(record.args.is_none(), "the refused arguments did not run");
    assert_eq!(
        h.mock.requests().len(),
        2,
        "the run goes on and the model reads the refusal"
    );
}

// ---------------------------------------------------------------------------
// 3. Old and new step records decode both ways
// ---------------------------------------------------------------------------

/// A record written before hooks existed is a `ToolOutput` and reads as one with no decision; a
/// record written now is still read by a build without hooks, as its output.
#[test]
fn old_and_new_step_records_decode_both_ways() {
    let old = ToolOutput::text("answer");
    let old_json = serde_json::to_value(&old).unwrap();
    assert!(
        old_json.get("hooks").is_none(),
        "an old record holds no verdict"
    );
    let read: HookedOutput = serde_json::from_value(old_json).unwrap();
    assert_eq!(read.output, old);
    assert!(read.hooks.is_empty());
    assert_eq!(read.args, None);

    let new = HookedOutput {
        output: ToolOutput::error("no"),
        hooks: vec![
            HookDecision::new(HookDecision::TOOL_BEFORE, HookDecision::DENY, Some("no")),
            HookDecision::new(
                HookDecision::TOOL_AFTER,
                HookDecision::FAILED,
                Some("broken"),
            )
            .by("a-plugin"),
        ],
        args: Some(json!({"path": "a"})),
        events: vec![RecordedEvent {
            name: "demo.published".into(),
            version: 1,
            data: json!({"n": 1}),
        }],
        approval: None,
        action: None,
    };
    let new_json = serde_json::to_value(&new).unwrap();
    // A build that predates hooks reads the record as the output it flattened, and ignores the rest.
    let as_old: ToolOutput = serde_json::from_value(new_json.clone()).unwrap();
    assert_eq!(as_old, new.output);
    // And this build reads back exactly what it wrote.
    let back: HookedOutput = serde_json::from_value(new_json).unwrap();
    assert_eq!(back, new);
    assert_eq!(back.hooks[1].plugin.as_deref(), Some("a-plugin"));
}

// ---------------------------------------------------------------------------
// 4. Each call of a turn of four that run at once runs its own hooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_call_of_a_concurrent_turn_runs_its_own_hooks() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(Counts::default());
    let agent = h
        .agent()
        .max_parallel(4)
        .tool(Probe {
            calls: calls.clone(),
        })
        .hooks(HookSet::new().with(counts.clone()).with(Arc::new(Marks)))
        .build();
    let ids = ["c1", "c2", "c3", "c4"];
    h.mock
        .push_tool_calls(
            ids.iter()
                .enumerate()
                .map(|(i, id)| call(id, "probe", json!({"use": "independent", "n": i})))
                .collect(),
        )
        .push_text("all done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(calls.load(SeqCst), 4, "every call ran");
    assert_eq!(counts.before("probe"), 4, "one before hook per call");
    assert_eq!(counts.after("probe"), 4, "one after hook per call");
    let messages = conversation(&rt, &view).await.messages;
    for (i, id) in ids.iter().enumerate() {
        let text = tool_of(&messages, id).text();
        assert!(text.ends_with('!'), "the after hook rewrote {id}: {text}");
        let given: Value = serde_json::from_str(text.trim_end_matches('!'))
            .expect("the tool echoed its arguments");
        assert_eq!(given["n"], json!(i), "the call kept its own arguments");
        assert_eq!(given["marked"], json!(true), "the before hook rewrote {id}");
    }
}

/// A before hook's rewrite reaches the tool, and an after hook's rewrite reaches the model and the
/// step's record — one call, both points.
#[tokio::test]
async fn a_rewrite_at_each_point_is_what_runs_and_what_the_model_reads() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Probe {
            calls: calls.clone(),
        })
        .plugin(Marks)
        .build();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "probe",
            json!({"use": "independent", "n": 1}),
        )])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let messages = conversation(&rt, &view).await.messages;
    let answer = messages[2].text();
    assert!(answer.ends_with('!'), "{answer}");
    let given: Value = serde_json::from_str(answer.trim_end_matches('!')).unwrap();
    assert_eq!(
        given["marked"],
        json!(true),
        "the tool ran the rewritten arguments"
    );
    let record = step_record(&h.store, run, "tool:c1").await;
    assert_eq!(
        record.args,
        Some(json!({"use": "independent", "n": 1, "marked": true})),
        "the rewritten arguments are in the record"
    );
    assert!(
        record.output.content.ends_with('!'),
        "the rewritten answer is in the record: {record:?}"
    );
    assert!(record.hooks.is_empty(), "a continue is not a decision");
}

// ---------------------------------------------------------------------------
// Deny and Stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_denial_is_the_calls_result_and_the_run_goes_on() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Probe {
            calls: calls.clone(),
        })
        .plugin(Refuses {
            verdict: Decision::Deny,
            reason: "not from this machine",
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "probe", json!({"use": "independent"}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(calls.load(SeqCst), 0, "a denied call does not run");
    let messages = conversation(&rt, &view).await.messages;
    assert_eq!(messages[2].text(), "not from this machine");
    assert!(is_error(&messages[2]));
    let record = step_record(&h.store, run, "tool:c1").await;
    assert!(record.output.is_error);
    assert_eq!(record.hooks[0].verdict, HookDecision::DENY);
    assert_eq!(
        record.hooks[0].reason.as_deref(),
        Some("not from this machine")
    );
}

#[tokio::test]
async fn a_stop_answers_the_call_and_fails_the_run() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Probe {
            calls: calls.clone(),
        })
        .plugin(Refuses {
            verdict: Decision::Stop,
            reason: "enough of this",
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "probe", json!({"use": "independent"}))]);
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    assert_eq!(calls.load(SeqCst), 0, "a stopped call does not run");
    let error = view.error.clone().unwrap_or_default();
    assert!(
        error.contains("stopped by a plugin: enough of this"),
        "{error}"
    );
    let record = step_record(&h.store, run, "tool:c1").await;
    assert!(record.output.is_error);
    assert_eq!(record.hooks[0].verdict, HookDecision::STOP);
    assert!(
        record.hooks[0].reason.as_deref() == Some("enough of this"),
        "{record:?}"
    );
    assert_eq!(
        h.mock.requests().len(),
        1,
        "the run ended, no closing turn was asked"
    );
    assert_eq!(
        view.status,
        RunStatus::Failed,
        "the step end is said as a failed step"
    );
}

/// An ask parks the run for the person's approval (ADR 0050, slice H8): the call is not run, its
/// step records the ask and its question, and observers read `approval.asked` and, once the person
/// answers, `approval.answered`, keyed by the asking step and by the message that answered.
#[tokio::test]
async fn an_ask_parks_the_run_and_observers_read_the_approval() {
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let watches = Arc::new(Watches::default());
    let agent = h
        .agent()
        .tool(Probe {
            calls: calls.clone(),
        })
        .plugin(Refuses {
            verdict: Decision::Ask,
            reason: "may I?",
        })
        .plugin(SharedWatches(watches.clone()))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "probe", json!({"use": "independent"}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let parked = wait_for(&rt, run, "parked on the approval", |v| {
        v.status == RunStatus::Parked && v.waiting
    })
    .await;
    assert_eq!(calls.load(SeqCst), 0, "nothing runs before the answer");
    let state = conversation(&rt, &parked).await;
    let approval = state.approval.expect("the call waits for approval");
    assert_eq!(
        (approval.call_id.as_str(), approval.approved),
        ("c1", false)
    );
    let record = step_record(&h.store, run, "tool:c1").await;
    assert_eq!(record.hooks[0].verdict, HookDecision::ASK);
    assert_eq!(record.awaits_approval(), Some("may I?"));

    let answer = user_message("yes");
    let answer_id = answer.id.clone();
    rt.deliver(run, answer).await.expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(calls.load(SeqCst), 1, "approved, the call ran once");
    let messages = conversation(&rt, &view).await.messages;
    assert!(!is_error(tool_of(&messages, "c1")));

    let seen = watches.0.lock().unwrap().clone();
    let got: Vec<(String, String)> = seen
        .iter()
        .map(|e| (e.kind.name().to_owned(), e.header.key.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "tool.started".into(),
                format!("{run}/tool:c1/tool.started/0")
            ),
            (
                "approval.asked".into(),
                format!("{run}/tool:c1/approval.asked/0")
            ),
            (
                "approval.answered".into(),
                format!("{run}/{answer_id}/approval.answered/0")
            ),
            (
                "tool.completed".into(),
                format!("{run}/tool:c1/tool.completed/0")
            ),
        ]
    );
    assert_eq!(seen[1].fields["question"], json!("may I?"));
    assert_eq!(seen[2].fields["approved"], json!(true));
}

/// A hook that panics never lets the call through: the step records a failure and the call is an
/// error result. The class is the plugin's business; the loop only honours it.
#[tokio::test]
async fn a_panicking_hook_denies_the_call() {
    struct Panics;
    impl Plugin for Panics {
        fn name(&self) -> &str {
            "panics"
        }
        fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
            panic!("on purpose")
        }
    }
    let h = Harness::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Probe {
            calls: calls.clone(),
        })
        .plugin(Panics)
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "probe", json!({"use": "independent"}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(calls.load(SeqCst), 0);
    let messages = conversation(&rt, &view).await.messages;
    assert!(is_error(&messages[2]));
    assert!(
        messages[2].text().contains("panicked"),
        "{}",
        messages[2].text()
    );
    let record = step_record(&h.store, run, "tool:c1").await;
    assert_eq!(record.hooks[0].verdict, HookDecision::FAILED);
    assert_eq!(record.hooks[0].plugin.as_deref(), Some("panics"));
}

// ---------------------------------------------------------------------------
// Observers (ADR 0050, slice H2)
// ---------------------------------------------------------------------------

/// An observer that records what it is offered, as `(name, key)` and the whole event.
#[derive(Default)]
struct Watches(Mutex<Vec<adam_llm_agent::HookEvent>>);

impl Plugin for Watches {
    fn name(&self) -> &str {
        "watches"
    }

    fn observe(&self, event: &adam_llm_agent::HookEvent) -> Result<(), adam_llm_agent::HookError> {
        self.0.lock().unwrap().push(event.clone());
        Ok(())
    }
}

struct SharedWatches(Arc<Watches>);

impl Plugin for SharedWatches {
    fn name(&self) -> &str {
        "watches"
    }

    fn observe(&self, event: &adam_llm_agent::HookEvent) -> Result<(), adam_llm_agent::HookError> {
        self.0.observe(event)
    }
}

/// An observer that panics at every event.
struct PanicsWhenWatching;

impl Plugin for PanicsWhenWatching {
    fn name(&self) -> &str {
        "panics-when-watching"
    }

    fn observe(&self, _event: &adam_llm_agent::HookEvent) -> Result<(), adam_llm_agent::HookError> {
        panic!("on purpose")
    }
}

/// Runs one call of `e` that answers `e-out` under `agent`, and returns the model-visible messages.
async fn run_one_call(h: &Harness, agent: &LlmAgent, args: Value) -> (RunId, Vec<Message>) {
    h.mock
        .push_tool_calls(vec![call("c1", "e", args)])
        .push_text("done");
    let rt = h.runtime(agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    (run, conversation(&rt, &view).await.messages)
}

#[tokio::test]
async fn an_observer_sees_a_calls_events_in_order_with_stable_keys() {
    let h = Harness::new();
    let (echo, _) = Echo::new("e");
    let watches = Arc::new(Watches::default());
    let agent = h
        .agent()
        .tool(echo)
        .plugin(SharedWatches(watches.clone()))
        .build();
    let (run, _) = run_one_call(&h, &agent, json!({"q": 1})).await;

    let seen = watches.0.lock().unwrap().clone();
    let got: Vec<(String, String)> = seen
        .iter()
        .map(|e| (e.kind.name().to_owned(), e.header.key.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "tool.started".into(),
                format!("{run}/tool:c1/tool.started/0")
            ),
            (
                "tool.completed".into(),
                format!("{run}/tool:c1/tool.completed/0")
            ),
        ]
    );
    assert_eq!(seen[1].fields["output"], json!("e-out"));
}

#[tokio::test]
async fn a_panicking_observer_leaves_the_run_unchanged() {
    let plain = {
        let h = Harness::new();
        let (echo, _) = Echo::new("e");
        let agent = h.agent().tool(echo).build();
        run_one_call(&h, &agent, json!({})).await.1
    };
    let h = Harness::new();
    let (echo, _) = Echo::new("e");
    let agent = h.agent().tool(echo).plugin(PanicsWhenWatching).build();
    let (_, watched) = run_one_call(&h, &agent, json!({})).await;
    let texts = |m: &[Message]| m.iter().map(Message::text).collect::<Vec<_>>();
    assert_eq!(texts(&plain), texts(&watched));
}

#[tokio::test]
async fn no_secret_reaches_an_observer() {
    let h = Harness::new();
    let (echo, _) = Echo::new("e");
    let watches = Arc::new(Watches::default());
    let agent = h
        .agent()
        .tool(echo)
        .step_io(
            adam_llm_agent::StepIo::default()
                .redact(|s| s.replace("sk-secret-token", "[redacted]")),
        )
        .plugin(SharedWatches(watches.clone()))
        .build();
    run_one_call(&h, &agent, json!({"auth": "sk-secret-token"})).await;
    let seen = watches.0.lock().unwrap().clone();
    let all = seen
        .iter()
        .map(|e| e.to_value().to_string())
        .collect::<String>();
    assert!(all.contains("[redacted]"), "{all}");
    assert!(!all.contains("sk-secret-token"), "{all}");
}

#[tokio::test]
async fn a_denied_call_is_a_denied_event_with_its_reason() {
    let h = Harness::new();
    let (echo, calls) = Echo::new("e");
    let watches = Arc::new(Watches::default());
    let agent = h
        .agent()
        .tool(echo)
        .plugin(Refuses {
            verdict: Decision::Deny,
            reason: "not from here",
        })
        .plugin(SharedWatches(watches.clone()))
        .build();
    run_one_call(&h, &agent, json!({})).await;
    assert_eq!(calls.load(SeqCst), 0);
    let seen = watches.0.lock().unwrap().clone();
    let names: Vec<_> = seen.iter().map(|e| e.kind.name()).collect();
    assert_eq!(names, ["tool.started", "tool.denied"]);
    assert_eq!(seen[1].fields["reason"], json!("not from here"));
}

// ---------------------------------------------------------------------------
// model.request (ADR 0050, slice H3)
// ---------------------------------------------------------------------------

/// Sets the three things the point may change, and counts its calls; or stops.
struct ModelHook {
    asked: Arc<AtomicUsize>,
    stop: bool,
}

impl Plugin for ModelHook {
    fn name(&self) -> &str {
        "model-hook"
    }

    fn model_request(&self, request: &mut HookedModelRequest<'_>) -> HookResult {
        self.asked.fetch_add(1, SeqCst);
        if self.stop {
            return Ok(Decision::Stop("no more model calls".into()));
        }
        *request.temperature = Some(0.25);
        *request.max_output_tokens = Some(321);
        request.metadata.insert("tenant".into(), "t-1".into());
        Ok(Decision::Continue)
    }
}

/// The model sees the rewrite, and only the rewrite: the system prompt and the model are untouched.
#[tokio::test]
async fn the_model_sees_the_rewritten_temperature_tokens_and_metadata() {
    let h = Harness::new();
    let asked = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .instructions("be brief")
        .plugin(ModelHook {
            asked: asked.clone(),
            stop: false,
        })
        .build();
    h.mock.push_text("hello");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].temperature, Some(0.25));
    assert_eq!(requests[0].max_output_tokens, Some(321));
    assert_eq!(
        requests[0].metadata.get("tenant").map(String::as_str),
        Some("t-1")
    );
    assert_eq!(requests[0].model, "test-model");
    assert!(
        requests[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .starts_with("be brief")
    );
    assert_eq!(asked.load(SeqCst), 1);
}

/// Sets top-p, a header and an extra body (slice H3b), or a header that looks like a credential.
struct ModelHookB {
    header: &'static str,
}

impl Plugin for ModelHookB {
    fn name(&self) -> &str {
        "model-hook-b"
    }

    fn model_request(&self, request: &mut HookedModelRequest<'_>) -> HookResult {
        *request.top_p = Some(0.5);
        request.headers.insert(self.header.into(), "v-1".into());
        let mut body = serde_json::Map::new();
        body.insert("seed".into(), json!(7));
        *request.extra_body = Some(body);
        Ok(Decision::Continue)
    }
}

/// The model sees top-p, the header and the extra body.
#[tokio::test]
async fn the_model_sees_the_rewritten_top_p_header_and_extra_body() {
    let h = Harness::new();
    let agent = h.agent().plugin(ModelHookB { header: "x-tenant" }).build();
    h.mock.push_text("hello");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].top_p, Some(0.5));
    assert_eq!(
        requests[0].headers.get("x-tenant").map(String::as_str),
        Some("v-1")
    );
    assert_eq!(
        requests[0].extra_body.as_ref().and_then(|b| b.get("seed")),
        Some(&json!(7))
    );
}

/// A header that looks like a credential fails the run and the model is never called.
#[tokio::test]
async fn a_credential_like_header_from_a_hook_is_refused() {
    for name in ["Authorization", "x-api-key", "Cookie", "X_Auth_Token"] {
        let h = Harness::new();
        let agent = h.agent().plugin(ModelHookB { header: name }).build();
        h.mock.push_text("never said");
        let rt = h.runtime(&agent);
        let run = rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let view = wait_failed(&rt, run).await;
        worker.stop().await;
        let error = view.error.clone().unwrap_or_default();
        assert!(error.contains("credential"), "{name}: {error}");
        assert!(h.mock.requests().is_empty(), "{name}: the model was called");
    }
}

/// A replay of a recorded model step asks no hook: the first worker records the turn and dies in
/// the tool, and the survivor replays `model:0` without asking the hook for it again.
#[tokio::test]
async fn a_replay_of_a_recorded_model_step_asks_no_model_hook() {
    let h = Harness::new();
    let asked = Arc::new(AtomicUsize::new(0));
    let (a, _a_calls) = Echo::new("a");
    let reached = Arc::new(Notify::new());
    let agent = h
        .agent()
        .tool(a)
        .tool(Stalls {
            reached: reached.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .plugin(ModelHook {
            asked: asked.clone(),
            stop: false,
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "a", json!({})), call("c2", "b", json!({}))])
        .push_text("done");
    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "crash-a", short_lease),
        h.runtime_with(&agent, "crash-b", short_lease),
    );
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "the second call starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;
    assert_eq!(asked.load(SeqCst), 1, "the first turn asked once");

    let survivor = spawn_worker(&rt_b);
    wait_done(&rt_b, run).await;
    survivor.stop().await;
    // Turn 0 was replayed (not asked again); the following turn asked once.
    assert_eq!(
        asked.load(SeqCst),
        2,
        "only the turn never recorded asks the hook"
    );
}

/// A `Stop` fails the run and the model is never called.
#[tokio::test]
async fn a_model_request_stop_fails_the_run() {
    let h = Harness::new();
    let asked = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .plugin(ModelHook {
            asked: asked.clone(),
            stop: true,
        })
        .build();
    h.mock.push_text("never said");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    let error = view.error.clone().unwrap_or_default();
    assert!(error.contains("no more model calls"), "{error}");
    assert!(h.mock.requests().is_empty(), "the model was not called");
}
