//! The prefix holds (ADR 0044, slice 4): every request of a run starts with the one before it, so a
//! provider's prompt cache keeps hitting. `system` and `tools` are the same bytes each time, and the
//! messages of a request are a byte prefix of the next one's, except at the turns where the history
//! was shortened, which the state records (`Conversation::shortenings`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    COMPACTION_TURN, Conversation, Limits, LlmAgent, LlmAgentBuilder, MAX_SOURCE_TOOLS, ReadOutput,
    Tool, ToolCtx, ToolError, ToolOutput, ToolSource, user_message,
};
use adam_model::{
    DynModel, Message, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse,
    ToolCall, ToolChoice, ToolSpec,
};
use adam_runtime::{CollectingSink, ManualClock, RetryPolicy, RunView, Runtime};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::oneshot;

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

/// Answers a small structured result, of the kind that holds an id nothing else repeats.
struct Handle;

#[async_trait]
impl Tool for Handle {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "handle".into(),
            description: "answers a small result with an id".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(
            r#"{"session":"3f9c2a17","state":"working"}"#,
        ))
    }
}

fn big(id: &str, n: u64) -> ToolCall {
    call(id, "big", json!({"n": n, "tag": id}))
}

/// The model of a run: the mock, and after the call number `on` it arms a lost commit, so that the
/// commit of the step that made that call is the one that fails.
struct Arm {
    inner: Arc<MockModel>,
    store: Arc<FaultyStore>,
    run: Mutex<Option<RunId>>,
    on: Mutex<Option<(usize, bool)>>,
    calls: AtomicUsize,
    /// Answer a summary request with a refusal that no script has to know about: the history is then
    /// shortened, as it was before compaction existed (the tests of the shortening rely on it).
    refuse_summaries: bool,
}

/// Whether `request` is the summary request of a compaction: no tool may be called, and it ends with
/// the instruction.
fn is_summary(request: &ModelRequest) -> bool {
    request.tool_choice == ToolChoice::None
        && request
            .messages
            .last()
            .is_some_and(|m| m.text().ends_with(COMPACTION_TURN))
}

impl Arm {
    fn armed(&self) {
        let n = self.calls.fetch_add(1, SeqCst);
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
        if self.refuse_summaries && is_summary(&req) {
            return Err(ModelError::invalid_request("this test makes no summaries"));
        }
        let answer = self.inner.complete(req).await;
        self.armed();
        answer
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        if self.refuse_summaries && is_summary(&req) {
            return Err(ModelError::invalid_request("this test makes no summaries"));
        }
        let answer = self.inner.stream(req).await;
        self.armed();
        answer
    }
}

struct Rig {
    mock: Arc<MockModel>,
    arm: Arc<Arm>,
    store: Arc<FaultyStore>,
    clock: ManualClock,
    rt: Runtime,
}

impl Rig {
    fn new(
        max_history_tokens: u32,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        Self::build(max_history_tokens, false, configure)
    }

    /// A rig whose model summarises: the scripted responses include the summaries.
    fn summarising(
        max_history_tokens: u32,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        Self::build(max_history_tokens, true, configure)
    }

    fn build(
        max_history_tokens: u32,
        summaries: bool,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        let mock = Arc::new(MockModel::new());
        let store = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
        let arm = Arc::new(Arm {
            inner: mock.clone(),
            store: store.clone(),
            run: Mutex::new(None),
            on: Mutex::new(None),
            calls: AtomicUsize::new(0),
            refuse_summaries: !summaries,
        });
        let model: DynModel = arm.clone();
        let agent = configure(
            LlmAgent::builder("llm", model, "test-model")
                .instructions("be brief")
                .limits(Limits {
                    max_history_tokens,
                    ..Limits::default()
                })
                .tool(Big)
                .tool(Handle),
        )
        .build();
        let dynamic: DynStore = store.clone();
        let clock = ManualClock::new();
        let rt = Runtime::builder(dynamic)
            .agent(agent)
            .clock(clock.clone())
            .event_sink(CollectingSink::new())
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
        Self {
            mock,
            arm,
            store,
            clock,
            rt,
        }
    }

    /// Lose the commit of the step that makes the model call number `call` (0-based), before it
    /// applies or, with `after_apply`, after it did.
    fn lose_the_commit_after_call(&self, call: usize, after_apply: bool) {
        *self.arm.on.lock().unwrap() = Some((call, after_apply));
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

    fn worker(&self) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.rt.clone();
        let worker = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
            .expect("worker result");
        });
        (stop, worker)
    }

    /// Work `run` to its end: the requests the model got and the state the run ended with.
    async fn finish(&self, run: RunId) -> (Vec<ModelRequest>, Conversation) {
        let (stop, worker) = self.worker();
        let ticker = self.ticker();
        let view = wait_terminal(&self.rt, run).await;
        ticker.abort();
        let _ = stop.send(());
        worker.await.expect("worker task");
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

fn head(request: &ModelRequest) -> String {
    serde_json::to_string(&(&request.system, &request.tools, &request.tool_choice)).unwrap()
}

/// The messages as the JSON array a provider is sent, without its closing bracket.
fn open_messages(request: &ModelRequest) -> String {
    let mut json = serde_json::to_string(&request.messages).unwrap();
    assert_eq!(json.pop(), Some(']'));
    json
}

/// The requests that do not start with the one before: their indices (the turn each was made at).
/// A summary request (a compaction's) is no turn and has its own rule (`the_summary_request_...`); it
/// is left out here, so the index of a request is the number of model turns made before it.
fn breaks(requests: &[ModelRequest]) -> Vec<u32> {
    let requests: Vec<&ModelRequest> = requests.iter().filter(|r| !is_summary(r)).collect();
    let mut found = Vec::new();
    for (n, pair) in requests.windows(2).enumerate() {
        let (a, b) = (&pair[0], &pair[1]);
        assert_eq!(
            head(a),
            head(b),
            "system and tools are the same bytes at request {}",
            n + 1
        );
        if !open_messages(b).starts_with(&open_messages(a)) {
            found.push(u32::try_from(n + 1).unwrap());
        }
    }
    found
}

/// A budget of 1200 tokens is 4800 characters. The results are 700, 700, 3000, 600 and 700
/// letters, so the history is about 4,500 characters at request 3 (under it, the 3000 result being
/// the newest) and about 5,200 at request 4, where the 3000 result is no longer the newest: the
/// first request over the budget, and so the only one that shortens (to 3,600).
const SIX_TURNS_BUDGET: u32 = 1200;

fn push_six_turns(mock: &MockModel) {
    mock.push_tool_calls(vec![big("c0", 700)])
        .push_tool_calls(vec![big("c1", 700)])
        .push_tool_calls(vec![big("c2", 3000)])
        .push_tool_calls(vec![big("c3", 600)])
        .push_tool_calls(vec![big("c4", 700)])
        .push_text("done");
}

/// Six turns: five tool calls and the answer.
async fn six_turns(max_history_tokens: u32) -> (Vec<ModelRequest>, Conversation) {
    let rig = Rig::new(max_history_tokens, |b| b);
    push_six_turns(&rig.mock);
    let run = rig.start().await;
    rig.finish(run).await
}

#[tokio::test]
async fn each_request_starts_with_the_one_before() {
    let (requests, state) = six_turns(SIX_TURNS_BUDGET).await;
    assert_eq!(requests.len(), 6);
    assert_eq!(state.turns, 6);
    let cut = breaks(&requests);
    assert_eq!(
        cut, state.shortenings,
        "the breaks are the recorded shortenings"
    );
    assert_eq!(cut.len(), 1, "exactly one shortening: {cut:?}");
}

#[tokio::test]
async fn without_a_budget_nothing_is_shortened_and_the_state_has_no_member_for_it() {
    let (requests, state) = six_turns(0).await;
    assert_eq!(breaks(&requests), Vec::<u32>::new());
    assert!(state.shortenings.is_empty());
    let written = serde_json::to_value(&state).unwrap();
    assert!(written.get("shortenings").is_none(), "{written}");
}

#[tokio::test]
async fn a_shortening_is_sent_the_same_way_after() {
    let (requests, state) = six_turns(SIX_TURNS_BUDGET).await;
    let [at] = state.shortenings[..] else {
        panic!("one shortening: {:?}", state.shortenings);
    };
    let at = at as usize;
    // The request that shortened is the history as the state now holds it, and every later request
    // carries those messages, byte for byte.
    for later in &requests[at..] {
        assert!(
            open_messages(later).starts_with(&open_messages(&requests[at])),
            "a later request carries the shortened messages as they were sent"
        );
    }
    assert_eq!(
        state.messages[..requests[at].messages.len()],
        requests[at].messages[..],
        "the request is the state's messages"
    );
    // What was cut is the oldest of the outputs, with its marker; the newest is whole.
    let texts: Vec<String> = state
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .map(Message::text)
        .collect();
    assert!(
        texts[0].contains(adam_llm_agent::TRUNCATION_MARKER_PREFIX),
        "{}",
        texts[0]
    );
    assert!(
        texts[2].contains(adam_llm_agent::TRUNCATION_MARKER_PREFIX),
        "{}",
        &texts[2][..80]
    );
    assert_eq!(
        texts[4],
        format!("c4:{}", "x".repeat(700)),
        "the newest result is whole"
    );
    // The request before it was the long history, which the shortening replaced.
    assert_eq!(
        requests[at - 1].messages[2].text(),
        format!("c0:{}", "x".repeat(700))
    );
}

#[tokio::test]
async fn a_state_written_before_shortenings_loads_and_one_without_them_writes_none() {
    let old = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}], "turns": 1});
    let state: Conversation = serde_json::from_value(old).unwrap();
    assert!(state.shortenings.is_empty());
    assert!(
        serde_json::to_value(&state)
            .unwrap()
            .get("shortenings")
            .is_none()
    );
    let mut state = state;
    state.shortenings.push(4);
    assert_eq!(
        serde_json::to_value(&state).unwrap()["shortenings"],
        json!([4])
    );
}

/// A model call that failed and is made again (the runtime retries it with the committed state)
/// is sent the same bytes, shortening included, however many times it is retried.
#[tokio::test]
async fn a_retried_model_call_sends_the_same_request_after_a_shortening() {
    let rig = Rig::new(SIX_TURNS_BUDGET, |b| b);
    rig.mock
        .push_tool_calls(vec![big("c0", 700)])
        .push_tool_calls(vec![big("c1", 700)])
        .push_tool_calls(vec![big("c2", 3000)])
        .push_tool_calls(vec![big("c3", 600)])
        .push_error(ModelError::transient("blip"))
        .push_error(ModelError::transient("blip again"))
        .push_tool_calls(vec![big("c4", 700)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 8, "the call at turn 4 was made three times");
    let first = serde_json::to_string(&requests[4]).unwrap();
    for retried in &requests[5..=6] {
        assert_eq!(
            serde_json::to_string(retried).unwrap(),
            first,
            "the same request, byte for byte"
        );
    }
    assert_eq!(
        state.shortenings,
        [4],
        "recorded once, at the turn it was made"
    );
    assert!(
        state
            .messages
            .iter()
            .any(|m| m.text().contains(adam_llm_agent::TRUNCATION_MARKER_PREFIX))
    );
}

/// The commit of the step that records a shortening is lost (before it applied, or after, with the
/// acknowledgement lost) and so is the one after it: the run is stepped again from the state before
/// and ends with the history, the shortenings and the requests of a run that lost nothing.
#[tokio::test]
async fn a_lost_commit_replays_the_same_requests() {
    let baseline = six_turns(SIX_TURNS_BUDGET).await;
    let [at] = baseline.1.shortenings[..] else {
        panic!("one shortening: {:?}", baseline.1.shortenings);
    };
    // The commit of the shortening's own transition, and the one after it.
    for call in [at as usize, at as usize + 1] {
        for after in [false, true] {
            let rig = Rig::new(SIX_TURNS_BUDGET, |b| b);
            push_six_turns(&rig.mock);
            rig.lose_the_commit_after_call(call, after);
            let run = rig.start().await;
            let (requests, state) = rig.finish(run).await;
            let label = format!("after call {call}, commit applied before the loss: {after}");
            assert_eq!(
                rig.store.injected(Method::CommitRun),
                1,
                "{label}: the fault fired"
            );
            assert_eq!(state.shortenings, [at], "{label}: the cut is recorded once");
            assert_eq!(state.messages, baseline.1.messages, "{label}");
            assert_eq!(requests.len(), baseline.0.len(), "{label}");
            for (got, want) in requests.iter().zip(&baseline.0) {
                assert_eq!(
                    serde_json::to_string(got).unwrap(),
                    serde_json::to_string(want).unwrap(),
                    "{label}"
                );
            }
        }
    }
}

/// A message that arrives between a lost commit and the replay does not move the cut. The
/// shortening is decided from the committed state, before the message is read: the replayed turn
/// finds the cut it would have made without the message (none), the message counts toward the
/// budget one turn later, and `shortenings` names the turn of the first request that carries the
/// cut, which is the first request that carries the message.
#[tokio::test]
async fn a_message_that_arrives_before_a_replay_does_not_move_the_cut() {
    // 850 tokens is 3,400 characters: the history is about 3,100 at request 3, and 5,100 with the
    // message, which is 2,000 characters.
    let rig = Rig::new(850, |b| b);
    rig.mock
        .push_tool_calls(vec![big("c0", 700)])
        .push_tool_calls(vec![big("c1", 700)])
        .push_tool_calls(vec![big("c2", 1500)])
        .push_text("finished") // request 3: the answer whose commit is lost
        .push_tool_calls(vec![big("c3", 100)]) // request 4: the answer to the message
        .push_text("done");
    rig.lose_the_commit_after_call(3, false);
    let run = rig.start().await;
    let (stop, worker) = rig.worker();
    // Nothing moves until the clock does: wait for the lost commit, then the message arrives.
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.store.injected(Method::CommitRun) < 1 {
        assert!(Instant::now() < deadline, "the commit was never lost");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let message = "m".repeat(2000);
    rig.rt
        .deliver(run, adam_llm_agent::user_message(message.clone()))
        .await
        .expect("deliver");
    let ticker = rig.ticker();
    let view = wait_terminal(&rig.rt, run).await;
    ticker.abort();
    let _ = stop.send(());
    worker.await.expect("worker task");
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    let (requests, state) = (rig.mock.requests(), conversation(&view));
    assert_eq!(requests.len(), 6, "{requests:#?}");
    // The first try's request (3) has no message and no cut; the replay answered from the journal,
    // which asked nothing; the model answers again at request 4, the first with the message, and
    // that is the first with the cut.
    assert!(!requests[3].messages.iter().any(|m| m.text() == message));
    assert!(requests[4].messages.iter().any(|m| m.text() == message));
    assert_eq!(breaks(&requests[..4]), Vec::<u32>::new());
    // The message keeps the history at the budget, so more cuts follow (the README says when); the
    // first is at request 4 and every one is recorded at the turn of its request.
    let cut = breaks(&requests);
    assert_eq!(
        cut.first(),
        Some(&4),
        "the first cut is in request 4: {cut:?}"
    );
    assert_eq!(
        state.shortenings, cut,
        "`shortenings` names the turns of the requests, not the replayed one"
    );
}

/// A kept output and the reads of it: the long text is in the state beside the history, and the
/// history a read adds extends the one before; a shortening of the shown first page does not
/// take the id away, and the reads that follow it still work.
#[tokio::test]
async fn a_kept_output_and_its_reads_keep_the_prefix() {
    let text: String = (0..7000).map(|i| format!("row-{i:06}\n")).collect();
    // 2600 tokens is 10,400 characters; the first page alone is 22,000, so the history is over
    // the budget from the first request after it and is cut at the next reads.
    let rig = Rig::new(2600, |b| b.tool(Wordy(text.clone())).tool(ReadOutput));
    let run = rig.start().await;
    let id = adam_llm_agent::output_id(&run.to_string(), "c1");
    rig.mock
        .push_tool_calls(vec![call("c1", "wordy", json!({}))]);
    for (n, offset) in [2001, 4001, 6001].into_iter().enumerate() {
        rig.mock.push_tool_calls(vec![call(
            &format!("r{n}"),
            "read_output",
            json!({"id": id, "offset": offset}),
        )]);
    }
    rig.mock.push_text("done");
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 5);
    assert_eq!(breaks(&requests), state.shortenings);
    assert!(
        !state.shortenings.is_empty(),
        "the pages go over the budget"
    );
    assert_eq!(
        state.kept_outputs.get(&id),
        Some(&text),
        "what is kept is whole"
    );
    // The first result was cut (a page is far over the floor and the budget), and still says
    // how to read the rest.
    let first = state
        .messages
        .iter()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap()
        .text();
    assert!(
        first.contains(adam_llm_agent::TRUNCATION_MARKER_PREFIX),
        "cut: {}",
        &first[..200]
    );
    let pointer = adam_llm_agent::read_pointer(&first, &id).expect("the pointer survived the cut");
    assert!(pointer.contains(&id), "{pointer}");
    // The last page of the output is in the newest result, whole: the read still worked.
    let last = state
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert!(last.text().contains("row-006999"), "{}", last.text());
}

/// A small structured result (a session's id, a plan's, a shared file's) is never shortened, whatever
/// the history is: it holds an id nothing else repeats.
#[tokio::test]
async fn a_small_result_with_an_id_survives_a_history_far_over_budget() {
    let rig = Rig::new(100, |b| b);
    rig.mock
        .push_tool_calls(vec![call("h", "handle", json!({}))])
        .push_tool_calls(vec![big("c0", 3000)])
        .push_tool_calls(vec![big("c1", 3000)])
        .push_tool_calls(vec![big("c2", 3000)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert!(
        !state.shortenings.is_empty(),
        "the history was far over budget"
    );
    let handle = &state.messages[2];
    assert_eq!(
        handle.text(),
        r#"{"session":"3f9c2a17","state":"working"}"#,
        "{handle:?}"
    );
    assert_eq!(requests[4].messages[2], *handle);
}

/// The closing turn is appended to the request and not kept in the history. The request that
/// has it still starts with the one before it; a run reopened after it would not start with the
/// closing request, which this does not do (documented in the README as the one exception).
#[tokio::test]
async fn the_closing_request_extends_the_one_before_and_its_turn_is_not_kept() {
    let rig = Rig::new(0, |b| b);
    let model: DynModel = rig.mock.clone();
    let agent = LlmAgent::builder("llm", model, "test-model")
        .instructions("be brief")
        .limits(Limits {
            max_turns: 2,
            ..Limits::default()
        })
        .tool(Big)
        .build();
    let rt = {
        let store: DynStore = Arc::new(MemoryStore::new());
        Runtime::builder(store)
            .agent(agent)
            .event_sink(CollectingSink::new())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .build()
    };
    rig.mock
        .push_tool_calls(vec![big("c0", 50)])
        .push_tool_calls(vec![big("c1", 50)])
        .push_text("here is what I did");
    let run = rt
        .start("llm", user_message("go"), Some("alice:thread-7"))
        .await
        .unwrap();
    let (stop, rx) = oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        })
    };
    let view = wait_terminal(&rt, run).await;
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    assert_eq!(view.status, RunStatus::Done);
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].tool_choice, ToolChoice::None);
    let mut shown = requests.clone();
    for r in &mut shown {
        r.tool_choice = ToolChoice::Auto;
    }
    assert_eq!(
        breaks(&shown),
        Vec::<u32>::new(),
        "the closing request extends the one before"
    );
    assert_eq!(
        requests[2].messages.last().map(Message::text),
        Some(adam_llm_agent::CLOSING_TURN.to_owned())
    );
    assert!(
        !conversation(&view)
            .messages
            .iter()
            .any(|m| m.text() == adam_llm_agent::CLOSING_TURN),
        "the closing turn is not in the history"
    );
    // It carries the conversation's key like every request of the run.
    assert_eq!(
        requests[2].prompt_cache_key, requests[0].prompt_cache_key,
        "the closing turn's request has the key too"
    );
    assert!(requests[2].prompt_cache_key.is_some());
}

/// The loop says the cache key of every request of a run that has a conversation id: the first 32
/// hex digits of the SHA-256 of the id (`printf alice:thread-7 | sha256sum`), so every run of the
/// conversation has the same key; none for a run with no conversation id; the id itself is not in it.
#[tokio::test]
async fn the_loop_gives_every_request_of_a_conversation_the_same_cache_key() {
    let rig = Rig::new(0, |b| b);
    rig.mock
        .push_tool_calls(vec![big("c0", 5)])
        .push_text("done")
        .push_text("done again");
    let first = rig.start_in(Some("alice:thread-7")).await;
    rig.finish(first).await;
    let second = rig.start_in(Some("alice:thread-7")).await;
    let (requests, _) = rig.finish(second).await;
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(
            request.prompt_cache_key.as_deref(),
            Some("de9e4a5612b838c5c8dfe872afe6cf72"),
            "the same key for every request of every run of the conversation"
        );
    }

    let rig = Rig::new(0, |b| b);
    rig.mock.push_text("done");
    let run = rig.start().await;
    let (requests, _) = rig.finish(run).await;
    assert_eq!(
        requests[0].prompt_cache_key, None,
        "no conversation, no key"
    );
}

/// What a source lists, in an order that changes every turn.
struct Shuffled {
    flip: AtomicBool,
    names: Vec<String>,
    /// Descriptions in the opposite order to the names.
    describe_backwards: bool,
}

impl Shuffled {
    fn new(names: Vec<String>, describe_backwards: bool) -> Self {
        Self {
            flip: AtomicBool::new(false),
            names,
            describe_backwards,
        }
    }
}

#[async_trait]
impl ToolSource for Shuffled {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
        let flip = self.flip.fetch_xor(true, SeqCst);
        let count = self.names.len();
        let mut specs: Vec<ToolSpec> = self
            .names
            .iter()
            .enumerate()
            .map(|(n, name)| ToolSpec {
                name: name.clone(),
                description: if self.describe_backwards {
                    format!("{:04} the {name} tool", count - n)
                } else {
                    format!("the {name} tool")
                },
                parameters: json!({"type": "object"}),
            })
            .collect();
        if flip {
            specs.reverse();
        }
        specs
    }
    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        Some(Ok(ToolOutput::text("ok")))
    }
}

fn names_of(request: &ModelRequest) -> Vec<String> {
    request.tools.iter().map(|t| t.name.clone()).collect()
}

/// The tools of a source are offered in name order, whatever order it lists them in, and by name
/// and not by description.
#[tokio::test]
async fn the_tools_of_a_source_are_offered_in_name_order() {
    let names = ["delta", "alpha", "charlie", "bravo"]
        .map(str::to_owned)
        .to_vec();
    let rig = Rig::new(0, |b| b.tool_source(Shuffled::new(names, true)));
    rig.mock
        .push_tool_calls(vec![big("c0", 5)])
        .push_tool_calls(vec![big("c1", 5)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, _) = rig.finish(run).await;
    assert_eq!(
        names_of(&requests[0]),
        ["big", "handle", "alpha", "bravo", "charlie", "delta"]
    );
    assert_eq!(
        breaks(&requests),
        Vec::<u32>::new(),
        "the listing flipped, the request did not"
    );
}

/// The sort comes before the cap: a source that lists more tools than `MAX_SOURCE_TOOLS` offers
/// the same ones in every request, the first by name, whatever order it lists them in.
#[tokio::test]
async fn the_tools_that_survive_the_cap_do_not_depend_on_the_listing_order() {
    let names: Vec<String> = (0..MAX_SOURCE_TOOLS + 2)
        .map(|n| format!("t{n:03}"))
        .collect();
    let rig = Rig::new(0, |b| b.tool_source(Shuffled::new(names.clone(), false)));
    rig.mock
        .push_tool_calls(vec![big("c0", 5)])
        .push_tool_calls(vec![big("c1", 5)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, _) = rig.finish(run).await;
    let mut want = vec!["big".to_owned(), "handle".to_owned()];
    want.extend(names.iter().take(MAX_SOURCE_TOOLS).cloned());
    for request in &requests {
        assert_eq!(names_of(request), want);
    }
}

/// Four results of 1,200 letters in a budget of 1,200 tokens (4,800 characters): the history is
/// under it at the third request and over it at the fifth, where the run is compacted, and the
/// request that follows is the first of a new prefix.
#[tokio::test]
async fn a_compaction_is_the_only_break_besides_shortenings() {
    let rig = Rig::summarising(SIX_TURNS_BUDGET, |b| b);
    rig.mock
        .push_tool_calls(vec![big("c0", 1200)])
        .push_tool_calls(vec![big("c1", 1200)])
        .push_tool_calls(vec![big("c2", 1200)])
        .push_tool_calls(vec![big("c3", 1200)])
        .push_text("SUMMARY: the task and four results")
        .push_tool_calls(vec![big("c4", 100)])
        .push_tool_calls(vec![big("c5", 100)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;

    assert_eq!(state.compactions.len(), 1, "{:?}", state.compactions);
    let at = state.compactions[0].turn;
    let mut expected: Vec<u32> = state.shortenings.clone();
    expected.push(at);
    expected.sort_unstable();
    assert_eq!(
        breaks(&requests),
        expected,
        "the breaks are the recorded ones"
    );

    // The summary request is the request before it, then the instruction: served from the cache.
    let summaries: Vec<usize> = requests
        .iter()
        .enumerate()
        .filter(|(_, r)| is_summary(r))
        .map(|(n, _)| n)
        .collect();
    assert_eq!(summaries.len(), 1);
    let summary = &requests[summaries[0]];
    let before = &requests[summaries[0] - 1];
    assert_eq!(summary.system, before.system);
    assert_eq!(summary.tools, before.tools);
    assert!(open_messages(summary).starts_with(&open_messages(before)));
    assert_eq!(summary.max_output_tokens, Some(4096));

    // Every request after the compaction extends the first one after it.
    let turns: Vec<&ModelRequest> = requests.iter().filter(|r| !is_summary(r)).collect();
    let first_after = turns[at as usize];
    for later in &turns[at as usize..] {
        assert!(open_messages(later).starts_with(&open_messages(first_after)));
    }
    // The task stays first, and the marker with the summary is its second part.
    let Message::User { content } = &first_after.messages[0] else {
        panic!("the first message is the person's");
    };
    assert_eq!(content[0].as_text(), "go");
    assert!(
        content[1].as_text().contains("SUMMARY: the task"),
        "{content:?}"
    );
    assert!(state.is_omission_marker(0, 1));
}
