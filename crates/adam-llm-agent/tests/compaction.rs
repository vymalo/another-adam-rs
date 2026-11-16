//! Compaction (ADR 0044 decision 11, slice 9): a history that reaches the budget is summarised once at
//! a journaled step, the requests after it start a new prefix that then holds, and a prompt the
//! provider rejects as too long is compacted and tried once more.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    COMPACTION_TURN, CompactionReason, Conversation, Limits, LlmAgent, LlmAgentBuilder, Plan,
    PlanItem, PlanStatus, ReadOutput, SessionAgent, SessionTools, Tool, ToolCtx, ToolError,
    ToolOutput, user_message,
};
use adam_model::{
    DynModel, Message, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse,
    ToolCall, ToolChoice, ToolSpec, Usage,
};
use adam_runtime::{
    CollectingSink, ManualClock, RetryPolicy, RunEvent, RunView, Runtime, child_run_id,
};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// `big { n, tag }` answers `tag` and `n` letters: results of a size the test chooses.
struct Big;

#[async_trait]
impl Tool for Big {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "big".into(),
            description: "answers a text of the size asked".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let n = args["n"].as_u64().unwrap_or(10) as usize;
        let tag = args["tag"].as_str().unwrap_or("t");
        Ok(ToolOutput::text(format!("{tag}:{}", "x".repeat(n))))
    }
}

/// Answers a long text, kept for `read_output`.
struct Wordy(String);

#[async_trait]
impl Tool for Wordy {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wordy".into(),
            description: "answers a long text, kept".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let (shown, kept) = adam_llm_agent::keep_long(ctx, &self.0).await;
        let mut output = ToolOutput::text(shown);
        output.kept = kept;
        Ok(output)
    }
}

/// Answers the small result of a tool that says where the run is, pinned.
struct Pinner;

#[async_trait]
impl Tool for Pinner {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "where".into(),
            description: "says where the run works".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(
            "slot: remote, branch: agent/fix-parser\n[end of the slot's text]\nnot the person's",
        )
        .pinned())
    }
}

/// Writes the plan.
struct Planner;

#[async_trait]
impl Tool for Planner {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plan".into(),
            description: "writes a plan".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("plan written").with_plan(Plan {
            title: Some("Fix the parser".into()),
            items: vec![PlanItem {
                id: 1,
                text: "Reproduce the failing test".into(),
                status: PlanStatus::Doing,
                note: None,
            }],
        }))
    }
}

fn big(id: &str, n: u64) -> ToolCall {
    call(id, "big", json!({"n": n, "tag": id}))
}

/// A model response with a prompt the provider counted.
fn reported(mut response: ModelResponse, input: u64) -> ModelResponse {
    response.usage = Usage::new(input, 10);
    response
}

/// The model of a run: the mock, and after the call number `on` it arms a lost commit, so that the
/// commit of the step that made that call is the one that fails.
struct Arm {
    inner: Arc<MockModel>,
    store: Arc<FaultyStore>,
    run: Mutex<Option<RunId>>,
    on: Mutex<Option<(usize, bool)>>,
    calls: AtomicUsize,
    /// The context window the model says it has once it was changed (`0`: the mock's).
    window: AtomicU64,
    /// After the call number `.0`, the window becomes `.1`.
    flip: Mutex<Option<(usize, u64)>>,
    rt: OnceLock<Runtime>,
    /// For each model call, how many compactions the store held when it was made.
    committed: Mutex<Vec<usize>>,
}

impl Arm {
    /// Note what the store holds of the run now: a call made inside a transition sees the state of
    /// the transition before.
    async fn look(&self) {
        let (Some(rt), Some(run)) = (self.rt.get(), *self.run.lock().unwrap()) else {
            return;
        };
        let view = rt.view(run).await.expect("view").expect("run exists");
        self.committed
            .lock()
            .unwrap()
            .push(conversation(&view).compactions.len());
    }

    fn armed(&self) {
        let n = self.calls.fetch_add(1, SeqCst);
        if let Some((at, window)) = *self.flip.lock().unwrap()
            && n == at
        {
            self.window.store(window, SeqCst);
        }
        let Some((on, after_apply)) = *self.on.lock().unwrap() else {
            return;
        };
        let Some(run) = *self.run.lock().unwrap() else {
            return;
        };
        if n == on {
            if after_apply {
                self.store.fail_run_after_apply(Method::CommitRun, run, 1);
            } else {
                self.store.fail_run(Method::CommitRun, run, 1);
            }
        }
    }
}

#[async_trait]
impl ModelClient for Arm {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.look().await;
        let answer = self.inner.complete(req).await;
        self.armed();
        answer
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        self.look().await;
        let answer = self.inner.stream(req).await;
        self.armed();
        answer
    }

    fn context_window(&self, model: &str) -> Option<u64> {
        match self.window.load(SeqCst) {
            0 => self.inner.context_window(model),
            window => Some(window),
        }
    }
}

struct Rig {
    mock: Arc<MockModel>,
    arm: Arc<Arm>,
    sink: CollectingSink,
    clock: ManualClock,
    rt: Runtime,
}

/// A budget of 1200 tokens is 4800 characters: four results of 1,200 letters reach it.
const BUDGET: u32 = 1200;

impl Rig {
    fn new(max_history_tokens: u32) -> Self {
        Self::with(MockModel::new(), max_history_tokens, |b| b)
    }

    fn with(
        mock: MockModel,
        max_history_tokens: u32,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        let mock = Arc::new(mock);
        let store = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
        let arm = Arc::new(Arm {
            inner: mock.clone(),
            store: store.clone(),
            run: Mutex::new(None),
            on: Mutex::new(None),
            calls: AtomicUsize::new(0),
            window: AtomicU64::new(0),
            flip: Mutex::new(None),
            rt: OnceLock::new(),
            committed: Mutex::new(Vec::new()),
        });
        let model: DynModel = arm.clone();
        let agent = configure(
            LlmAgent::builder("llm", model, "test-model")
                .instructions("be brief")
                .limits(Limits {
                    max_history_tokens,
                    ..Limits::default()
                })
                .tool(Big),
        )
        .build();
        let dynamic: DynStore = store;
        let clock = ManualClock::new();
        let sink = CollectingSink::new();
        let rt = Runtime::builder(dynamic)
            .agent(agent)
            .clock(clock.clone())
            .event_sink(sink.clone())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .lease_ttl(Duration::from_secs(10))
            .retry(RetryPolicy {
                max_attempts: 4,
                initial_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(5),
                multiplier: 1.0,
            })
            .build();
        let _ = arm.rt.set(rt.clone());
        Self {
            mock,
            arm,
            sink,
            clock,
            rt,
        }
    }

    fn lose_the_commit_after_call(&self, call: usize, after_apply: bool) {
        *self.arm.on.lock().unwrap() = Some((call, after_apply));
    }

    /// After the model call number `call` (0-based) the model says its window is `window`.
    fn change_the_window_after_call(&self, call: usize, window: u64) {
        *self.arm.flip.lock().unwrap() = Some((call, window));
    }

    async fn start_in(&self, conversation: Option<&str>) -> RunId {
        let run = self
            .rt
            .start("llm", user_message("go"), conversation)
            .await
            .expect("start");
        *self.arm.run.lock().unwrap() = Some(run);
        run
    }

    async fn start(&self) -> RunId {
        self.start_in(None).await
    }

    /// A failed commit is retried after a backoff of the runtime's clock, which only this moves.
    fn ticker(&self) -> tokio::task::JoinHandle<()> {
        let clock = self.clock.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                clock.advance(Duration::from_millis(200));
            }
        })
    }

    /// Work `run` to its end, whatever it is.
    async fn work(&self, run: RunId) -> RunView {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.rt.clone();
        let worker = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
            .expect("worker result");
        });
        let ticker = self.ticker();
        let view = wait_terminal(&self.rt, run).await;
        ticker.abort();
        let _ = stop.send(());
        worker.await.expect("worker task");
        view
    }

    /// Work `run` to a `Done`: the requests the model got and the state the run ended with.
    async fn finish(&self, run: RunId) -> (Vec<ModelRequest>, Conversation) {
        let view = self.work(run).await;
        assert_eq!(view.status, RunStatus::Done, "{view:#?}");
        (self.mock.requests(), conversation(&view))
    }
}

async fn wait_terminal(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

fn is_summary(request: &ModelRequest) -> bool {
    request.tool_choice == ToolChoice::None
        && request
            .messages
            .last()
            .is_some_and(|m| m.text().ends_with(COMPACTION_TURN))
}

fn open_messages(request: &ModelRequest) -> String {
    let mut json = serde_json::to_string(&request.messages).unwrap();
    assert_eq!(json.pop(), Some(']'));
    json
}

/// The task: the first text part of the first message, which a compaction keeps verbatim.
fn first_part(message: &Message) -> String {
    match message {
        Message::User { content } => content[0].as_text().to_owned(),
        other => panic!("not a user message: {other:?}"),
    }
}

/// Four results of 1,200 letters, the summary, and what the run does after it.
fn script_one_compaction(mock: &MockModel, summary: &str) {
    mock.push_tool_calls(vec![big("c0", 1200)])
        .push_tool_calls(vec![big("c1", 1200)])
        .push_tool_calls(vec![big("c2", 1200)])
        .push_tool_calls(vec![big("c3", 1200)])
        .push_text(summary)
        .push_tool_calls(vec![big("c4", 100)])
        .push_tool_calls(vec![big("c5", 100)])
        .push_text("done");
}

/// The requests that are model turns, and the summary requests.
fn split(requests: &[ModelRequest]) -> (Vec<&ModelRequest>, Vec<&ModelRequest>) {
    requests.iter().partition(|r| !is_summary(r))
}

#[tokio::test]
async fn a_long_run_is_summarised_once_and_keeps_the_prefix_after() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "SUMMARY: four results of x");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;

    let (turns, summaries) = split(&requests);
    assert_eq!(summaries.len(), 1, "one summary for the whole run");
    assert_eq!(turns.len(), 7);
    assert_eq!(state.turns, 7, "the summary call is no turn");
    assert_eq!(state.compactions.len(), 1);
    let at = state.compactions[0].turn as usize;
    assert_eq!(at, 4);
    assert_eq!(state.compactions[0].reason, CompactionReason::Budget);
    // Before the compaction every request extends the one before; after it, the same, from the
    // first request of the new prefix.
    for pair in turns[..at].windows(2) {
        assert!(open_messages(pair[1]).starts_with(&open_messages(pair[0])));
    }
    assert!(!open_messages(turns[at]).starts_with(&open_messages(turns[at - 1])));
    for pair in turns[at..].windows(2) {
        assert!(open_messages(pair[1]).starts_with(&open_messages(pair[0])));
    }
    // The system prompt and the tools are the same bytes in every request that is not a summary.
    let head = |r: &ModelRequest| serde_json::to_string(&(&r.system, &r.tools)).unwrap();
    assert!(turns.iter().all(|r| head(r) == head(turns[0])));
    // The history holds the summary, and the marker is what the coder and the continued runs read.
    assert!(state.is_omission_marker(0, 1));
    assert!(state.omitted_turns >= 2);
    let marker = state.messages[0].text();
    assert!(marker.contains("SUMMARY: four results of x"), "{marker}");
    assert!(state.summary.as_deref().unwrap().contains("SUMMARY: four"));
}

#[tokio::test]
async fn the_summary_request_extends_the_previous_request() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "S");
    let run = rig.start_in(Some("alice:thread-7")).await;
    let (requests, _) = rig.finish(run).await;
    let at = requests
        .iter()
        .position(is_summary)
        .expect("a summary request");
    let (before, summary) = (&requests[at - 1], &requests[at]);
    assert_eq!(summary.system, before.system);
    assert_eq!(summary.tools, before.tools);
    assert!(before.prompt_cache_key.is_some());
    assert_eq!(
        summary.prompt_cache_key, before.prompt_cache_key,
        "the provider is asked to serve it from the same cache"
    );
    assert!(
        open_messages(summary).starts_with(&open_messages(before)),
        "served from the cache"
    );
    assert_eq!(summary.tool_choice, ToolChoice::None);
    assert_eq!(summary.max_output_tokens, Some(4096));
    assert_eq!(summary.messages.last().unwrap().text(), COMPACTION_TURN);
}

#[tokio::test]
async fn the_summary_is_replayed_from_the_journal_not_recomputed() {
    for after_apply in [false, true] {
        let rig = Rig::new(BUDGET);
        script_one_compaction(&rig.mock, "S");
        // Calls 0 to 3 are the turns and call 4 is the summary: its step's commit is the one lost.
        rig.lose_the_commit_after_call(4, after_apply);
        let run = rig.start().await;
        let (requests, state) = rig.finish(run).await;
        let (turns, summaries) = split(&requests);
        assert_eq!(
            summaries.len(),
            1,
            "the summary is not asked again (after_apply {after_apply})"
        );
        assert_eq!(turns.len(), 7);
        assert_eq!(state.compactions.len(), 1);
    }
}

#[tokio::test]
async fn the_task_plan_pinned_results_and_kept_pointers_survive() {
    let text: String = (0..7000).map(|i| format!("row-{i:06}\n")).collect();
    let rig = Rig::with(MockModel::new(), 2600, |b| {
        b.tool(Wordy(text.clone()))
            .tool(Planner)
            .tool(Pinner)
            .tool(ReadOutput)
    });
    let run = rig.start().await;
    let id = adam_llm_agent::output_id(&run.to_string(), "c1");
    rig.mock
        .push_tool_calls(vec![
            call("p0", "where", json!({})),
            call("c0", "plan", json!({})),
        ])
        .push_tool_calls(vec![call("c1", "wordy", json!({}))])
        .push_tool_calls(vec![big("c2", 50)])
        .push_text("SUMMARY: planned and read")
        .push_tool_calls(vec![big("c3", 50)])
        .push_text("done");
    let (requests, state) = rig.finish(run).await;
    assert_eq!(split(&requests).1.len(), 1);
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(first_part(&state.messages[0]), "go");
    let marker = state.messages[0].text();
    assert!(marker.contains("Plan: Fix the parser"), "{marker}");
    assert!(
        marker.contains("1. [>] Reproduce the failing test"),
        "{marker}"
    );
    assert!(
        marker.contains(&adam_llm_agent::pointer_line(&id)),
        "the pointer, built from the call's own id: {marker}"
    );
    // The pinned result is copied whole, its bracketed line escaped so that it cannot pose as one of
    // the loop's.
    assert!(
        marker.contains("slot: remote, branch: agent/fix-parser\n\\x5bend of the slot's text]"),
        "{marker}"
    );
    assert!(
        state.pinned_calls.is_empty(),
        "no pinned result is left in the history"
    );
    assert_eq!(
        state.kept_outputs.get(&id),
        Some(&text),
        "what is kept is whole"
    );
    assert_eq!(
        state.plan.as_ref().unwrap().title.as_deref(),
        Some("Fix the parser")
    );
}

/// Every tool result follows the assistant message that called it, in every request and in the end.
fn assert_paired(messages: &[Message]) {
    let mut open: Vec<&str> = Vec::new();
    for message in messages {
        match message {
            Message::Assistant { tool_calls, .. } => {
                assert!(open.is_empty(), "a call without its result: {open:?}");
                open = tool_calls.iter().map(|c| c.id.as_str()).collect();
            }
            Message::Tool { call_id, .. } => {
                assert!(
                    open.contains(&call_id.as_str()),
                    "result {call_id} follows its call"
                );
                open.retain(|id| id != call_id);
            }
            Message::User { .. } => {}
        }
    }
}

#[tokio::test]
async fn no_call_loses_its_result_across_the_cut() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "S");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    for request in &requests {
        // The history a request carries ends with the owed calls' results, bar the summary's own
        // instruction.
        let messages = if is_summary(request) {
            &request.messages[..request.messages.len() - 1]
        } else {
            &request.messages[..]
        };
        assert_paired(messages);
    }
    assert_paired(&state.messages);
}

#[tokio::test]
async fn the_last_turns_are_kept_verbatim() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "S");
    let run = rig.start().await;
    let (requests, _) = rig.finish(run).await;
    let at = requests.iter().position(is_summary).unwrap();
    let history = &requests[at].messages[..requests[at].messages.len() - 1];
    let first_after = &requests[at + 1];
    let kept = first_after.messages.len() - 1;
    assert!(kept >= 2, "at least one whole turn stays");
    assert!(kept < history.len() / 2, "and the older ones are gone");
    assert_eq!(
        first_after.messages[1..],
        history[history.len() - kept..],
        "the newest turns, byte for byte"
    );
    assert_eq!(first_part(&first_after.messages[0]), "go");
}

#[tokio::test]
async fn without_a_window_the_history_budget_triggers_it() {
    // No window: the model says none, and only `max_history_tokens` is the budget.
    let rig = Rig::new(BUDGET);
    assert_eq!(rig.mock.context_window("test-model"), None);
    script_one_compaction(&rig.mock, "S");
    let run = rig.start().await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1);

    // No window and no budget: nothing is ever compacted.
    let none = Rig::new(0);
    for n in 0..6 {
        none.mock.push_tool_calls(vec![big(&format!("c{n}"), 1200)]);
    }
    none.mock.push_text("done");
    let run = none.start().await;
    let (requests, state) = none.finish(run).await;
    assert_eq!(requests.len(), 7);
    assert!(state.compactions.is_empty());
}

#[tokio::test]
async fn with_a_window_the_reported_tokens_trigger_it() {
    // A window of 26,096 tokens, less 20,000 and the 4,096 of output, leaves a budget of 2,000. The
    // texts are tiny: only what the provider reports says the prompt is big, and a compaction keeps
    // at most eight turns, so ten turns are needed for two to go.
    let script = |mock: &MockModel, input: u64, summary: bool| {
        for n in 0..10 {
            mock.push_response(reported(
                ModelResponse::tool_calls(vec![big(&format!("c{n}"), 20)]),
                input,
            ));
        }
        if summary {
            mock.push_text("SUMMARY");
        }
        mock.push_response(reported(ModelResponse::text("done"), input));
    };
    let reports = Rig::with(
        MockModel::new().with_context_window("test-model", 26_096),
        0,
        |b| b,
    );
    script(&reports.mock, 2_500, true);
    let run = reports.start().await;
    let (requests, state) = reports.finish(run).await;
    assert_eq!(state.compactions.len(), 1, "{:?}", state.compactions);
    assert_eq!(state.compactions[0].turn, 10);
    assert_eq!(state.compactions[0].dropped_turns, 2);
    assert_eq!(split(&requests).1.len(), 1);

    // The same run with prompts that report small ones is never compacted.
    let small = Rig::with(
        MockModel::new().with_context_window("test-model", 26_096),
        0,
        |b| b,
    );
    script(&small.mock, 300, false);
    let run = small.start().await;
    let (_, state) = small.finish(run).await;
    assert!(state.compactions.is_empty());
}

#[tokio::test]
async fn a_failed_summary_call_falls_back_to_recorded_shortenings() {
    let rig = Rig::new(BUDGET);
    rig.mock
        .push_tool_calls(vec![big("c0", 1200)])
        .push_tool_calls(vec![big("c1", 1200)])
        .push_tool_calls(vec![big("c2", 1200)])
        .push_tool_calls(vec![big("c3", 1200)])
        .push_error(ModelError::invalid_request("the summary is refused"))
        .push_tool_calls(vec![big("c4", 100)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert!(state.compactions.is_empty());
    assert_eq!(
        state.shortenings.len(),
        1,
        "the history was shortened instead"
    );
    assert_eq!(state.compact_failed, Some(4));
    assert_eq!(split(&requests).1.len(), 1, "and not asked again at once");
}

#[tokio::test]
async fn the_summary_call_is_reported_under_its_own_call_id() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "S");
    let run = rig.start().await;
    let _ = rig.finish(run).await;
    let calls: Vec<String> = rig
        .sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Usage(u) => Some(u.call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 8, "seven turns and one summary: {calls:?}");
    let own: Vec<&String> = calls.iter().filter(|c| c.contains("-k0-")).collect();
    assert_eq!(own.len(), 1, "{calls:?}");
    let mut sorted = calls.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 8, "every call has an id of its own");
    assert!(own[0].len() <= 57);
}

/// The step the person sees: running, then completed, with how much was summarised.
#[tokio::test]
async fn the_person_sees_one_step_for_the_summary() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "S");
    let run = rig.start().await;
    let _ = rig.finish(run).await;
    let steps: Vec<_> = rig
        .sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(s) if s.id == "compact:0:4" => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert_eq!(steps[0].label, "Summarise the conversation");
    assert_eq!(steps[0].state, adam_llm_agent::StepState::Running);
    assert_eq!(steps[1].state, adam_llm_agent::StepState::Completed);
    let detail = steps[1].detail.as_deref().unwrap();
    assert!(detail.starts_with("Summarised 3 earlier turns"), "{detail}");
}

#[tokio::test]
async fn a_rejected_prompt_is_compacted_and_retried_once() {
    // No budget at all: only the provider's rejection says the prompt is too long.
    let rig = Rig::new(0);
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 3000)]);
    }
    rig.mock
        .push_error(ModelError::ContextLength("prompt is too long (one)".into()))
        .push_text("SUMMARY after the rejection")
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    // Four turns, the rejected fifth, the summary, the fifth again.
    assert_eq!(requests.len(), 7);
    let (rejected, summary, again) = (&requests[4], &requests[5], &requests[6]);
    assert!(is_summary(summary));
    assert!(
        summary.messages.len() < rejected.messages.len(),
        "the summary request leaves the newest turns out"
    );
    let longest = summary
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { content, .. } => Some(content.chars().count()),
            _ => None,
        })
        .max()
        .unwrap();
    assert!(longest < 2_500, "long outputs are shortened: {longest}");
    assert!(again.messages.len() < rejected.messages.len());
    assert!(
        again.messages[0]
            .text()
            .contains("SUMMARY after the rejection")
    );
    assert_eq!(state.compactions.len(), 1);
    let compaction = &state.compactions[0];
    assert_eq!(compaction.reason, CompactionReason::Overflow);
    assert_eq!(
        compaction.rejected.as_deref().map(|r| r.contains("one")),
        Some(true)
    );
    assert_eq!(state.turns, 5, "the rejected turn was asked again, once");
}

#[tokio::test]
async fn a_second_rejection_fails_naming_both() {
    let rig = Rig::new(0);
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 3000)]);
    }
    rig.mock
        .push_error(ModelError::ContextLength("prompt is too long (one)".into()))
        .push_text("SUMMARY")
        .push_error(ModelError::ContextLength("prompt is too long (two)".into()));
    let run = rig.start().await;
    let view = rig.work(run).await;
    assert_eq!(view.status, RunStatus::Failed, "{view:#?}");
    let error = view.error.expect("an error");
    assert!(error.contains("after compacting the history"), "{error}");
    assert!(
        error.contains("(two)") && error.contains("(one)"),
        "{error}"
    );
    assert_eq!(rig.mock.requests().len(), 7, "no third try");
}

#[tokio::test]
async fn a_run_that_continues_another_and_compacts_again_keeps_what_code_wrote() {
    let text: String = (0..7000).map(|i| format!("row-{i:06}\n")).collect();
    let rig = Rig::with(MockModel::new(), 2600, |b| {
        b.tool(Wordy(text.clone()))
            .tool(Planner)
            .tool(Pinner)
            .tool(ReadOutput)
    });
    let first = rig.start_in(Some("conv")).await;
    let id = adam_llm_agent::output_id(&first.to_string(), "c1");
    rig.mock
        .push_tool_calls(vec![
            call("p0", "where", json!({})),
            call("c0", "plan", json!({})),
        ])
        .push_tool_calls(vec![call("c1", "wordy", json!({}))])
        .push_tool_calls(vec![big("c2", 50)])
        .push_text("SUMMARY A: planned and read")
        .push_tool_calls(vec![big("c3", 50)])
        .push_text("done");
    let (_, before) = rig.finish(first).await;
    assert_eq!(before.compactions.len(), 1);

    // The person writes again, and this run fills the history once more.
    let second = RunId::new();
    assert!(
        rig.rt
            .start_with_id_continuing(second, "llm", user_message("again"), Some("conv"), first)
            .await
            .expect("continue")
    );
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("d{n}"), 3500)]);
    }
    rig.mock
        .push_text("SUMMARY B: the second task")
        .push_text("done again");
    let (requests, state) = rig.finish(second).await;
    assert_eq!(state.continued_from, Some(first));
    assert_eq!(
        split(&requests).1.len(),
        2,
        "one summary for each of the two runs"
    );
    assert_eq!(state.compactions.len(), 2, "{:?}", state.compactions);
    assert_eq!(first_part(&state.messages[0]), "go");
    let marker = state.messages[0].text();
    assert!(marker.contains("SUMMARY B"), "{marker}");
    assert!(marker.contains("Plan: Fix the parser"), "{marker}");
    assert!(
        marker.contains("slot: remote, branch: agent/fix-parser"),
        "the pinned result of the first run: {marker}"
    );
    assert!(
        marker.contains(&adam_llm_agent::pointer_line(&id)),
        "the pointer of the first run: {marker}"
    );
}

#[tokio::test]
async fn a_history_over_the_budget_when_the_closing_turn_comes_is_not_compacted() {
    // A window of 26,096 tokens leaves a budget of 2,000; the tenth call, which asks for one tool
    // call too many, reports a prompt over it, and the run goes on to its closing turn.
    let rig = Rig::with(
        MockModel::new().with_context_window("test-model", 26_096),
        0,
        |b| {
            b.limits(Limits {
                max_tool_calls: 9,
                max_history_tokens: 0,
                ..Limits::default()
            })
        },
    );
    for n in 0..9 {
        rig.mock.push_response(reported(
            ModelResponse::tool_calls(vec![big(&format!("c{n}"), 20)]),
            300,
        ));
    }
    rig.mock.push_response(reported(
        ModelResponse::tool_calls(vec![big("c9", 20)]),
        2_500,
    ));
    rig.mock.push_text("closing answer");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 11);
    assert!(split(&requests).1.is_empty(), "no summary request");
    assert!(state.compactions.is_empty());
    assert!(state.limit.is_some(), "the closing turn was reached");
}

#[tokio::test]
async fn a_rejected_closing_turn_is_compacted_and_asked_again() {
    let rig = Rig::with(MockModel::new(), 0, |b| {
        b.limits(Limits {
            max_tool_calls: 3,
            max_history_tokens: 0,
            ..Limits::default()
        })
    });
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 3000)]);
    }
    rig.mock
        .push_error(ModelError::ContextLength(
            "too long for the closing turn".into(),
        ))
        .push_text("SUMMARY of the work")
        .push_text("closing answer");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 7);
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(state.compactions[0].reason, CompactionReason::Overflow);
    assert!(state.limit.is_some());
}

#[tokio::test]
async fn an_empty_summary_is_said_to_be_none() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "  \n ");
    let run = rig.start().await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1);
    assert!(!state.compactions[0].summarised);
    let marker = state.messages[0].text();
    assert!(marker.contains("No summary could be made"), "{marker}");
    assert!(!marker.contains("Summary (written by you"), "{marker}");
}

#[tokio::test]
async fn a_summary_call_that_fails_for_a_moment_is_asked_again() {
    let rig = Rig::new(BUDGET);
    rig.mock
        .push_tool_calls(vec![big("c0", 1200)])
        .push_tool_calls(vec![big("c1", 1200)])
        .push_tool_calls(vec![big("c2", 1200)])
        .push_tool_calls(vec![big("c3", 1200)])
        .push_error(ModelError::transient("the provider hiccuped"))
        .push_text("S")
        .push_tool_calls(vec![big("c4", 100)])
        .push_text("done");
    let run = rig.start().await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1, "{:?}", state.compactions);
    assert_eq!(state.compact_failed, None);
    assert!(state.shortenings.is_empty());
}

#[tokio::test]
async fn the_summary_calls_tokens_are_in_the_runs_usage() {
    let rig = Rig::new(BUDGET);
    rig.mock
        .push_tool_calls(vec![big("c0", 1200)])
        .push_tool_calls(vec![big("c1", 1200)])
        .push_tool_calls(vec![big("c2", 1200)])
        .push_tool_calls(vec![big("c3", 1200)])
        .push_response(reported(ModelResponse::text("S"), 777))
        .push_text("done");
    let run = rig.start().await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(state.usage.input_tokens, 777);
    assert_eq!(state.usage.output_tokens, 10);
}

#[tokio::test]
async fn a_replay_applies_the_cut_the_journal_recorded() {
    // Budget 1,600 tokens: the eleventh result reaches it, and two turns stay. After the summary call
    // the window shrinks to a budget of 800, where one turn stays, and the commit is lost: the
    // replay takes the summary from the journal and the cut it was made for.
    let rig = Rig::with(
        MockModel::new().with_context_window("test-model", 25_696),
        0,
        |b| b,
    );
    for n in 0..11 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 600)]);
    }
    rig.mock
        .push_text("S")
        .push_tool_calls(vec![big("c11", 100)])
        .push_text("done");
    rig.change_the_window_after_call(11, 24_896);
    rig.lose_the_commit_after_call(11, false);
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(
        split(&requests).1.len(),
        1,
        "the summary is not asked again"
    );
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(
        state.compactions[0].dropped_turns, 9,
        "eleven turns, the two the first budget kept stay"
    );
}

#[tokio::test]
async fn a_compaction_is_committed_before_the_model_is_asked_again() {
    let rig = Rig::new(BUDGET);
    script_one_compaction(&rig.mock, "S");
    let run = rig.start().await;
    let _ = rig.finish(run).await;
    let seen = rig.arm.committed.lock().unwrap().clone();
    assert_eq!(seen.len(), 8, "{seen:?}");
    assert_eq!(
        seen[4], 0,
        "the summary call is made before it is committed"
    );
    assert_eq!(
        seen[5], 1,
        "the call after it is made from the committed history: {seen:?}"
    );
}

#[tokio::test]
async fn a_summary_that_cannot_be_made_after_a_rejection_fails_the_run_with_its_own_error() {
    let rig = Rig::new(0);
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 3000)]);
    }
    rig.mock
        .push_error(ModelError::ContextLength("prompt is too long (one)".into()))
        .push_error(ModelError::invalid_request("the model alias is unknown"));
    let run = rig.start().await;
    let view = rig.work(run).await;
    assert_eq!(view.status, RunStatus::Failed, "{view:#?}");
    let error = view.error.clone().expect("an error");
    assert!(error.contains("the model alias is unknown"), "{error}");
    assert!(error.contains("Invalid"), "{error}");
    let state = conversation(&view);
    assert!(state.compactions.is_empty(), "the history was not dropped");
    assert_eq!(state.messages.len(), 9);
    assert_eq!(rig.mock.requests().len(), 6, "no third try");
}

#[tokio::test]
async fn a_summary_request_that_is_itself_too_long_drops_the_turns_without_one() {
    let rig = Rig::new(0);
    for n in 0..4 {
        rig.mock.push_tool_calls(vec![big(&format!("c{n}"), 3000)]);
    }
    rig.mock
        .push_error(ModelError::ContextLength("prompt is too long (one)".into()))
        .push_error(ModelError::ContextLength(
            "the summary request is too long".into(),
        ))
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 7);
    assert_eq!(state.compactions.len(), 1);
    let compaction = &state.compactions[0];
    assert!(!compaction.summarised);
    assert_eq!(compaction.reason, CompactionReason::Overflow);
    assert!(compaction.rejected.as_deref().unwrap().contains("(one)"));
    let marker = state.messages[0].text();
    assert!(marker.contains("No summary could be made"), "{marker}");
}

/// A tool that waits for a remote task and answers a result that it pins.
struct WaitsAndPins;

#[async_trait]
impl Tool for WaitsAndPins {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "waiter".into(),
            description: "waits for a remote task".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::AwaitRemote {
            task: format!("task-of-{}", ctx.call_id()),
            timeout_ms: None,
        })
    }
    async fn poll_remote(
        &self,
        _ctx: &ToolCtx,
        _task: &str,
    ) -> Result<adam_llm_agent::RemotePoll, ToolError> {
        Ok(adam_llm_agent::RemotePoll::Ready(
            ToolOutput::text("slot: waited-for, branch: agent/late").pinned(),
        ))
    }
}

#[tokio::test]
async fn a_pinned_result_that_came_through_a_wait_survives_a_compaction() {
    let rig = Rig::with(MockModel::new(), BUDGET, |b| b.tool(WaitsAndPins));
    rig.mock
        .push_tool_calls(vec![call("w0", "waiter", json!({}))])
        .push_tool_calls(vec![big("c0", 1200)])
        .push_tool_calls(vec![big("c1", 1200)])
        .push_tool_calls(vec![big("c2", 1200)])
        .push_tool_calls(vec![big("c3", 1200)])
        .push_text("S")
        .push_text("done");
    let run = rig.start().await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1);
    let marker = state.messages[0].text();
    assert!(
        marker.contains("slot: waited-for, branch: agent/late"),
        "{marker}"
    );
    assert!(state.pinned_calls.is_empty());
}

#[tokio::test]
async fn a_shortening_leaves_the_estimate_to_start_again() {
    // Two turns of 3,000 letters are over the budget of 1,200 tokens, but the reports say the prompt
    // is small and there are not two turns to drop: the history is shortened, and the call after it
    // fails for good, so the state it leaves is the one the shortening made.
    let rig = Rig::new(BUDGET);
    rig.mock
        .push_response(reported(
            ModelResponse::tool_calls(vec![big("c0", 3000)]),
            100,
        ))
        .push_response(reported(
            ModelResponse::tool_calls(vec![big("c1", 3000)]),
            100,
        ))
        .push_error(ModelError::invalid_request("refused"));
    let run = rig.start().await;
    let view = rig.work(run).await;
    assert_eq!(view.status, RunStatus::Failed, "{view:#?}");
    let state = conversation(&view);
    assert_eq!(state.shortenings.len(), 1, "{state:#?}");
    assert_eq!(state.last_prompt, None);
}

// ---------------------------------------------------------------------------
// Sessions and subagents
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

/// Wraps a model: the first call waits at a gate before it is answered: a child whose round is
/// still working.
struct GateModel {
    inner: Arc<MockModel>,
    armed: AtomicBool,
    gate: Arc<Gate>,
}

impl GateModel {
    async fn hold(&self) {
        if self.armed.swap(false, SeqCst) {
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

fn start_call(id: &str) -> ToolCall {
    call(
        id,
        "subagent_start",
        json!({"agent": "helper", "description": "look at it", "prompt": "do the work"}),
    )
}

struct Family {
    parent_mock: Arc<MockModel>,
    child_mock: Arc<MockModel>,
    gate: Arc<Gate>,
    rt: Runtime,
    parent_name: String,
}

impl Family {
    /// A parent with a big tool and the session tools, and a child that is held at its first call
    /// when `hold`, with `child_history` as its own history budget.
    fn new(parent_history: u32, child_history: u32, hold: bool) -> Self {
        let parent_name = format!("parent-{}", RunId::new());
        let child_name = format!("child-{}", RunId::new());
        let parent_mock = Arc::new(MockModel::new());
        let child_mock = Arc::new(MockModel::new());
        let gate = Arc::new(Gate::default());
        let roster = SessionTools::new([SessionAgent {
            name: "helper".into(),
            agent: child_name.clone(),
            description: "Does a piece of work.".into(),
            within: None,
            remote: false,
        }]);
        let parent_model: DynModel = parent_mock.clone();
        let mut parent = LlmAgent::builder(&parent_name, parent_model, "m")
            .session_agents([child_name.clone()])
            .limits(Limits {
                max_history_tokens: parent_history,
                ..Limits::default()
            })
            .tool(Big);
        for tool in roster.tools() {
            parent = parent.dyn_tool(tool);
        }
        let child_model: DynModel = if hold {
            Arc::new(GateModel {
                inner: child_mock.clone(),
                armed: AtomicBool::new(true),
                gate: gate.clone(),
            })
        } else {
            child_mock.clone()
        };
        let child = LlmAgent::builder(&child_name, child_model, "m")
            .limits(Limits {
                max_history_tokens: child_history,
                ..Limits::default()
            })
            .tool(Big)
            .build();
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store)
            .agent(parent.build())
            .agent(child)
            .worker_id("w")
            .event_sink(CollectingSink::new())
            .clock(ManualClock::new())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .build();
        Self {
            parent_mock,
            child_mock,
            gate,
            rt,
            parent_name,
        }
    }

    async fn start(&self) -> RunId {
        self.rt
            .start(
                &self.parent_name,
                user_message("go"),
                Some(&format!("conv-{}", RunId::new())),
            )
            .await
            .expect("start")
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

fn spawn_worker(rt: &Runtime) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
        .expect("worker result");
    });
    (stop, handle)
}

#[tokio::test]
async fn an_open_session_is_still_waited_for_after_its_start_was_cut() {
    // The parent's tools weigh about a thousand tokens: a budget of 3,200 is reached by the fourth
    // result of 2,400 letters and not by the third.
    let family = Family::new(3200, 0, true);
    family.child_mock.push_text("child says 42");
    let parent = family.start().await;
    // The start is the first turn; the big results then fill the history, which is compacted before
    // the model answers: the start call is among the turns the summary replaces.
    family
        .parent_mock
        .push_tool_calls(vec![start_call("c1")])
        .push_tool_calls(vec![big("b0", 2400)])
        .push_tool_calls(vec![big("b1", 2400)])
        .push_tool_calls(vec![big("b2", 2400)])
        .push_tool_calls(vec![big("b3", 2400)])
        .push_text("SUMMARY: started a helper")
        .push_text("I think I am done")
        .push_text("the helper said 42");
    let (stop, worker) = spawn_worker(&family.rt);
    tokio::time::timeout(Duration::from_secs(20), family.gate.reached.notified())
        .await
        .expect("the child's model call");
    // The parent has compacted, answered, and now waits for the session instead of ending.
    let parked = wait_for(&family.rt, parent, "parked on the session", |v| {
        v.status == RunStatus::Parked
            && !conversation(v).compactions.is_empty()
            && conversation(v).pending_wait.is_some()
    })
    .await;
    let state = conversation(&parked);
    assert!(
        state.compactions[0].sessions,
        "the cut took the start with it"
    );
    assert!(
        !state
            .messages
            .iter()
            .any(|m| m.tool_calls().iter().any(|c| c.name == "subagent_start")),
        "the start call is no longer in the history"
    );
    family.gate.release.notify_one();
    let done = wait_for(&family.rt, parent, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    worker.await.unwrap();
    let state = conversation(&done);
    assert!(
        state
            .messages
            .iter()
            .any(|m| m.text().contains("child says 42"))
    );
}

#[tokio::test]
async fn a_subagent_compacts_against_its_own_budget() {
    // The parent has no budget worth the name; the child has a small one.
    let family = Family::new(100_000, BUDGET, false);
    for n in 0..4 {
        family
            .child_mock
            .push_tool_calls(vec![big(&format!("k{n}"), 1200)]);
    }
    family
        .child_mock
        .push_text("CHILD SUMMARY")
        .push_text("child done");
    let parent = family.start().await;
    family
        .parent_mock
        .push_tool_calls(vec![start_call("c1")])
        .push_text("started")
        .push_text("all done");
    let (stop, worker) = spawn_worker(&family.rt);
    let done = wait_for(&family.rt, parent, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    worker.await.unwrap();
    assert!(
        conversation(&done).compactions.is_empty(),
        "the parent did not compact"
    );
    let child = family
        .rt
        .view(child_run_id(parent, "c1"))
        .await
        .unwrap()
        .expect("the child's run");
    let child = conversation(&child);
    assert_eq!(child.compactions.len(), 1, "the child compacted itself");
    assert!(child.messages[0].text().contains("CHILD SUMMARY"));
}
