//! The prefix holds (ADR 0044, slice 4): every request of a run starts with the one before it, so a
//! provider's prompt cache keeps hitting. `system` and `tools` are the same bytes each time, and the
//! messages of a request are a byte prefix of the next one's, except at the turns where the history
//! was shortened, which the state records (`Conversation::shortenings`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus, RunUpdate, Store as _};
use adam_llm_agent::{
    COMPACTION_TURN, Conversation, ImageStore, InboundImage, Limits, LlmAgent, LlmAgentBuilder,
    MAX_SOURCE_TOOLS, MemoryImages, ReadOutput, SourceCtx, Tool, ToolAccess, ToolCtx, ToolError,
    ToolOutput, ToolSource, Withheld, keep_image, user_message, user_message_with_images,
};
use adam_model::{
    DynModel, ImageLimits, ImageOrigin, InputModalities, Message, MockModel, ModelClient,
    ModelDelta, ModelError, ModelRequest, ModelResponse, ToolCall, ToolChoice, ToolSpec,
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
    /// What the deployment says the model can read, instead of the mock's, once set.
    modalities: Mutex<Option<InputModalities>>,
    /// Run after each call is answered, with the call's number (0-based).
    hook: Mutex<Option<Hook>>,
}

/// What runs after a model call: given the call's number.
type Hook = Arc<dyn Fn(usize) + Send + Sync>;

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
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(n);
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
    fn input_modalities(&self, model: &str) -> InputModalities {
        match &*self.modalities.lock().unwrap() {
            Some(set) => set.clone(),
            None => self.inner.input_modalities(model),
        }
    }

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
        Self::build_with(MockModel::new(), max_history_tokens, summaries, configure)
    }

    /// A rig whose model can see images within `limits`.
    fn seeing(
        max_history_tokens: u32,
        limits: ImageLimits,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        Self::build_with(
            MockModel::new().seeing_images(limits),
            max_history_tokens,
            false,
            configure,
        )
    }

    fn build_with(
        mock: MockModel,
        max_history_tokens: u32,
        summaries: bool,
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
            refuse_summaries: !summaries,
            modalities: Mutex::new(None),
            hook: Mutex::new(None),
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
            .retry(
                RetryPolicy::new(4)
                    .with_backoff(Duration::from_millis(5), Duration::from_millis(5))
                    .with_multiplier(1.0),
            )
            .build();
        Self {
            mock,
            arm,
            store,
            clock,
            rt,
        }
    }

    /// Run `hook` after each model call is answered, with the call's number (0-based).
    fn after_each_call(&self, hook: impl Fn(usize) + Send + Sync + 'static) {
        *self.arm.hook.lock().unwrap() = Some(Arc::new(hook));
    }

    /// From the model call after number `call` on, the deployment says the model reads `set`.
    fn change_settings_after_call(&self, call: usize, set: InputModalities) {
        let arm = self.arm.clone();
        self.after_each_call(move |n| {
            if n == call {
                *arm.modalities.lock().unwrap() = Some(set.clone());
            }
        });
    }

    /// Work `run` to the end, whichever it is: the requests the model got and the run as it ended.
    async fn finish_any(&self, run: RunId) -> (Vec<ModelRequest>, RunView) {
        let (stop, worker) = self.worker();
        let ticker = self.ticker();
        let view = wait_terminal(&self.rt, run).await;
        ticker.abort();
        let _ = stop.send(());
        worker.await.expect("worker task");
        (self.of_run(run, self.mock.requests()).await, view)
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

    /// Start a run whose task is `task`, not the `go` of [`start`](Self::start): a source that
    /// selects from the task reads this text.
    async fn start_task(&self, task: &str) -> RunId {
        let run = self
            .rt
            .start("llm", user_message(task), None)
            .await
            .expect("start");
        *self.arm.run.lock().unwrap() = Some(run);
        run
    }

    /// Replace the stored state of `run` with a journal written before `loaded` existed: the
    /// transcript kept, `turns` and `loaded_servers` set, so the worker resumes at that turn.
    async fn seed_stored_state(
        &self,
        run: RunId,
        turns: u32,
        loaded_servers: &[&str],
        history: usize,
    ) {
        let record = self
            .store
            .load_run(run)
            .await
            .expect("the stored run")
            .expect("the run exists");
        let mut state = record.state.clone();
        let mut conversation: Conversation =
            serde_json::from_value(state["agent"].clone()).expect("the agent state");
        conversation.turns = turns;
        for n in 0..history {
            let id = format!("old{n}");
            conversation.messages.push(Message::Assistant {
                content: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: id.clone(),
                    name: "big".into(),
                    arguments: json!({"n": 10, "tag": "old"}),
                    ..Default::default()
                }],
                reasoning: None,
                provider: None,
            });
            conversation.messages.push(Message::Tool {
                call_id: id,
                content: format!("old{n}:{}", "x".repeat(1200)),
                is_error: false,
                images: Vec::new(),
            });
        }
        conversation.loaded_servers = loaded_servers.iter().map(|s| (*s).to_owned()).collect();
        // `loaded_servers` is decoded but never written by the live state: the journal the test
        // writes is an old one, so the key goes in by hand.
        let mut agent = serde_json::to_value(&conversation).expect("the state");
        agent["loaded_servers"] = json!(loaded_servers);
        state["agent"] = agent;
        self.store
            .commit_run(run, record.version, RunUpdate::new(record.status, state))
            .await
            .expect("the seeded state");
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
        (
            self.of_run(run, self.mock.requests()).await,
            conversation(&self.rt, &view).await,
        )
    }

    /// `requests` with the run each names cleared (ADR 0067), so two runs that make the same calls
    /// compare equal byte for byte, though their ids differ. Every call names a run this rig
    /// started, which its own store holds (its model may have served an earlier run first), and
    /// the last one `run`.
    async fn of_run(&self, run: RunId, requests: Vec<ModelRequest>) -> Vec<ModelRequest> {
        assert_eq!(
            requests.last().and_then(|r| r.run.clone()),
            Some(run.to_string()),
            "the last call names its run"
        );
        let mut cleared = Vec::with_capacity(requests.len());
        for mut request in requests {
            let named = request.run.take().expect("every call names a run");
            let id = RunId(named.parse().expect("a run id"));
            let view = self.rt.view(id).await.unwrap();
            assert!(view.is_some(), "{named} is a run of this rig's store");
            cleared.push(request);
        }
        cleared
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

async fn conversation(rt: &Runtime, view: &RunView) -> Conversation {
    let state = rt.state(view.id).await.expect("state").expect("the run");
    serde_json::from_value(state).expect("state is a Conversation")
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
///
/// `appended` is [`Conversation::tools_appended`]: the turns at whose request the head grew by an
/// MCP server the run loaded. There, and **only** there, `system` and `tools` may differ from the
/// request before, and only by a **suffix**: the server's instructions follow the ones that were
/// there and its tools follow the tools that were there, so every byte before them is the one the
/// provider cached. Every other pair must have the same head, to the byte.
fn breaks(requests: &[ModelRequest], appended: &[u32]) -> Vec<u32> {
    let requests: Vec<&ModelRequest> = requests.iter().filter(|r| !is_summary(r)).collect();
    let mut found = Vec::new();
    for (n, pair) in requests.windows(2).enumerate() {
        let (a, b) = (&pair[0], &pair[1]);
        let turn = u32::try_from(n + 1).unwrap();
        if appended.contains(&turn) {
            assert_eq!(
                a.tool_choice, b.tool_choice,
                "the choice does not move at the turn a load appended tools: {turn}"
            );
            assert!(
                b.tools.len() >= a.tools.len(),
                "the tools array at turn {turn} did not shrink"
            );
            assert_eq!(
                b.tools[..a.tools.len()],
                a.tools[..],
                "the tools array at turn {turn} is the one before it plus a suffix"
            );
            let (old, new) = (
                a.system.as_deref().unwrap_or_default(),
                b.system.as_deref().unwrap_or_default(),
            );
            assert!(
                new.starts_with(old),
                "the system prompt at turn {turn} is the one before it plus a suffix"
            );
        } else {
            assert_eq!(
                head(a),
                head(b),
                "system and tools are the same bytes at request {}",
                n + 1
            );
        }
        if !open_messages(b).starts_with(&open_messages(a)) {
            found.push(turn);
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
    let cut = breaks(&requests, &state.tools_appended);
    assert_eq!(
        cut, state.shortenings,
        "the breaks are the recorded shortenings"
    );
    assert_eq!(cut.len(), 1, "exactly one shortening: {cut:?}");
}

#[tokio::test]
async fn without_a_budget_nothing_is_shortened_and_the_state_has_no_member_for_it() {
    let (requests, state) = six_turns(0).await;
    assert_eq!(breaks(&requests, &state.tools_appended), Vec::<u32>::new());
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
    let (requests, state) = (rig.mock.requests(), conversation(&rig.rt, &view).await);
    assert_eq!(requests.len(), 6, "{requests:#?}");
    // The first try's request (3) has no message and no cut; the replay answered from the journal,
    // which asked nothing; the model answers again at request 4, the first with the message, and
    // that is the first with the cut.
    assert!(!requests[3].messages.iter().any(|m| m.text() == message));
    assert!(requests[4].messages.iter().any(|m| m.text() == message));
    assert_eq!(
        breaks(&requests[..4], &state.tools_appended),
        Vec::<u32>::new()
    );
    // The message keeps the history at the budget, so more cuts follow (the README says when); the
    // first is at request 4 and every one is recorded at the turn of its request.
    let cut = breaks(&requests, &state.tools_appended);
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
    assert_eq!(breaks(&requests, &state.tools_appended), state.shortenings);
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
        breaks(&shown, &[]),
        Vec::<u32>::new(),
        "the closing request extends the one before"
    );
    assert_eq!(
        requests[2].messages.last().map(Message::text),
        Some(adam_llm_agent::CLOSING_TURN.to_owned())
    );
    assert!(
        !conversation(&rt, &view)
            .await
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
    let (requests, state) = rig.finish(run).await;
    assert_eq!(
        names_of(&requests[0]),
        ["big", "handle", "alpha", "bravo", "charlie", "delta"]
    );
    assert_eq!(
        breaks(&requests, &state.tools_appended),
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
        breaks(&requests, &state.tools_appended),
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
    // The head of every request that is a model turn is the same bytes, whatever side of the
    // compaction it is on.
    assert!(
        turns.iter().all(|r| head(r) == head(turns[0])),
        "system, tools and tool choice never move"
    );
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

/// The summary request takes what the sources add to a request, as a model turn does: the tools
/// they offer, in the same order, so the head is the bytes the provider has cached.
#[tokio::test]
async fn the_summary_request_has_the_tools_of_the_sources_too() {
    let names = ["delta", "alpha", "charlie"].map(str::to_owned).to_vec();
    let rig = Rig::summarising(SIX_TURNS_BUDGET, |b| {
        b.tool_source(Shuffled::new(names, false))
    });
    rig.mock
        .push_tool_calls(vec![big("c0", 1200)])
        .push_tool_calls(vec![big("c1", 1200)])
        .push_tool_calls(vec![big("c2", 1200)])
        .push_tool_calls(vec![big("c3", 1200)])
        .push_text("SUMMARY")
        .push_tool_calls(vec![big("c4", 100)])
        .push_text("done");
    let run = rig.start_in(Some("alice:thread-7")).await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1);
    let at = requests
        .iter()
        .position(is_summary)
        .expect("a summary request");
    let (before, summary) = (&requests[at - 1], &requests[at]);
    assert_eq!(
        names_of(summary),
        ["big", "handle", "alpha", "charlie", "delta"],
        "the tools of the source, in name order"
    );
    assert_eq!(summary.tools, before.tools);
    assert_eq!(summary.system, before.system);
    assert_eq!(summary.prompt_cache_key, before.prompt_cache_key);
    assert!(summary.prompt_cache_key.is_some());
}

// ---------------------------------------------------------------------------------------------
// Images (ADR 0044)
// ---------------------------------------------------------------------------------------------

fn png(width: u32, height: u32, tag: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0]);
    v.extend_from_slice(&tag.to_be_bytes());
    v.extend(std::iter::repeat_n(0xAB, 200));
    v
}

fn limits(max_count: usize) -> ImageLimits {
    ImageLimits {
        max_bytes: 1024 * 1024,
        max_count,
        max_side: 4096,
        sources: [ImageOrigin::Person, ImageOrigin::Workspace].into(),
    }
}

/// `snap { n }` answers an image of its own: a picture of `n`.
struct Snap;

#[async_trait]
impl Tool for Snap {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "snap".into(),
            description: "answers an image".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let n = args["n"].as_u64().unwrap_or(0) as u32;
        let origin = match args["origin"].as_str() {
            Some("mcp") => ImageOrigin::Mcp,
            _ => ImageOrigin::Workspace,
        };
        let pad = args["pad"].as_u64().unwrap_or(0) as usize;
        let mut bytes = png(8, 8, n);
        bytes.resize(bytes.len() + pad, n as u8);
        let outcome = keep_image(ctx, &bytes, None, Some(&format!("shot{n}.png")), origin).await;
        let said = outcome
            .stand_in
            .clone()
            .unwrap_or_else(|| format!("shot{n}.png is an image, shown to you after this result"));
        let output = if args["fail"].as_bool() == Some(true) {
            ToolOutput::error(said)
        } else {
            ToolOutput::text(said)
        };
        Ok(output.with_image(outcome))
    }
}

fn snap(n: u32) -> ToolCall {
    call(&format!("s{n}"), "snap", json!({"n": n}))
}

/// `snap` of an image from `origin` and `pad` bytes more than a header.
fn snap_with(n: u32, origin: &str, pad: u64) -> ToolCall {
    call(
        &format!("s{n}"),
        "snap",
        json!({"n": n, "origin": origin, "pad": pad}),
    )
}

fn with_store(builder: LlmAgentBuilder) -> LlmAgentBuilder {
    builder
        .state(Arc::new(ImageStore(Arc::new(MemoryImages::new()))))
        .tool(Snap)
}

fn looking(png_tag: u32) -> adam_runtime::Inbound {
    user_message_with_images(
        "what is this?",
        &[InboundImage::new(
            Some("me.png"),
            Some("image/png"),
            &png(64, 64, png_tag),
        )],
    )
}

fn images_of(messages: &[Message]) -> Vec<&adam_model::ImageRef> {
    messages.iter().flat_map(Message::images).collect()
}

/// A person's picture and three of a tool's: every request starts with the one before it, and the
/// bytes of every image a request names are in its `image_data`, the same bytes each time.
#[tokio::test]
async fn images_keep_the_prefix() {
    let rig = Rig::seeing(0, limits(8), with_store);
    rig.mock
        .push_tool_calls(vec![snap(1)])
        .push_tool_calls(vec![snap(2)])
        .push_tool_calls(vec![snap(3)])
        .push_text("a cat");
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 4);
    assert_eq!(breaks(&requests, &state.tools_appended), Vec::<u32>::new());
    assert_eq!(images_of(&state.messages).len(), 4);
    for request in &requests {
        for image in images_of(&request.messages) {
            let bytes = request
                .image_data
                .get(&image.sha256)
                .unwrap_or_else(|| panic!("no bytes for {}", image.sha256));
            assert_eq!(adam_llm_agent::sha256_hex(bytes), image.sha256);
        }
    }
    // The first image is the person's, in the first message after its words; what a tool returned
    // is on the tool's message.
    let Message::User { content } = &requests[0].messages[0] else {
        panic!("a user message");
    };
    assert_eq!(content[0].as_text(), "what is this?");
    assert!(matches!(content[1], adam_model::ContentPart::Image(_)));
    assert_eq!(requests[3].image_data.len(), 4);
    // An agent with a store keeps no byte in its state.
    assert!(state.images.is_empty());
    assert!(state.pending_images.is_empty());
}

/// A model that cannot see is given a line in place of every image, and the run completes: an
/// image never fails a request.
#[tokio::test]
async fn a_text_only_model_gets_a_stand_in_and_completes() {
    let rig = Rig::new(0, with_store);
    rig.mock.push_tool_calls(vec![snap(1)]).push_text("ok");
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert!(images_of(&state.messages).is_empty());
    assert!(requests.iter().all(|r| r.image_data.is_empty()));
    let Message::User { content } = &state.messages[0] else {
        panic!("a user message");
    };
    assert_eq!(content.len(), 2, "{content:?}");
    assert!(
        content[1]
            .as_text()
            .starts_with("[image not shown: this model cannot see images; me.png (png"),
        "{content:?}"
    );
    let tool = state
        .messages
        .iter()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert!(
        tool.text()
            .starts_with("[image not shown: this model cannot see images; shot1.png"),
        "{}",
        tool.text()
    );
}

/// The ninth image of a run is a stand-in, and the eight before it stay as they were.
#[tokio::test]
async fn the_ninth_image_is_a_stand_in_and_nothing_before_changes() {
    let rig = Rig::seeing(0, limits(8), with_store);
    for n in 1..=8 {
        rig.mock.push_tool_calls(vec![snap(n)]);
    }
    rig.mock.push_text("done");
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert_eq!(breaks(&requests, &state.tools_appended), Vec::<u32>::new());
    assert_eq!(images_of(&state.messages).len(), 8);
    let last = state
        .messages
        .iter()
        .rfind(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert!(last.images().is_empty());
    assert!(
        last.text()
            .contains("is past the 8 images this run sends to the model"),
        "{}",
        last.text()
    );
    // The request before the stand-in holds the first eight; the last request the same eight.
    assert_eq!(images_of(&requests[7].messages).len(), 8);
    assert_eq!(
        images_of(&requests[8].messages),
        images_of(&requests[7].messages)
    );
}

/// The commit of a step that kept an image is lost (before it applied, or after): the run replays
/// the same references and is sent the same requests.
#[tokio::test]
async fn a_lost_commit_replays_the_same_image_refs() {
    let run_it = |lose: Option<(usize, bool)>| async move {
        let rig = Rig::seeing(0, limits(8), with_store);
        rig.mock
            .push_tool_calls(vec![snap(1)])
            .push_tool_calls(vec![snap(2)])
            .push_text("done");
        if let Some((call, after)) = lose {
            rig.lose_the_commit_after_call(call, after);
        }
        let run = rig.rt.start("llm", looking(7), None).await.expect("start");
        *rig.arm.run.lock().unwrap() = Some(run);
        let out = rig.finish(run).await;
        let injected = rig.store.injected(Method::CommitRun);
        (out, injected)
    };
    let ((base_requests, base), _) = run_it(None).await;
    for call in [0usize, 1] {
        for after in [false, true] {
            let ((requests, state), injected) = run_it(Some((call, after))).await;
            let label = format!("after call {call}, applied: {after}");
            assert_eq!(injected, 1, "{label}");
            assert_eq!(state.messages, base.messages, "{label}");
            assert_eq!(requests.len(), base_requests.len(), "{label}");
            for (got, want) in requests.iter().zip(&base_requests) {
                assert_eq!(
                    serde_json::to_string(got).unwrap(),
                    serde_json::to_string(want).unwrap(),
                    "{label}"
                );
                assert_eq!(got.image_data.len(), want.image_data.len(), "{label}");
            }
        }
    }
}

/// An old result's images go when the history is shortened, once, as a line that names each; the
/// newest result keeps its image and so does the person's.
#[tokio::test]
async fn a_shortening_replaces_old_tool_images_once() {
    // Three images are 12,000 characters of the estimate: a budget of 4,000 tokens is 16,000, and
    // the fourth image takes the history over it.
    let rig = Rig::seeing(4000, limits(8), with_store);
    for n in 1..=5 {
        rig.mock.push_tool_calls(vec![snap(n)]);
    }
    rig.mock.push_text("done");
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert!(!state.shortenings.is_empty(), "{:?}", state.shortenings);
    assert_eq!(breaks(&requests, &state.tools_appended), state.shortenings);
    let tools: Vec<&Message> = state
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .collect();
    let removed = |m: &Message| {
        m.text()
            .matches("[image removed from the history: shot")
            .count()
    };
    assert!(tools[0].images().is_empty());
    assert_eq!(removed(tools[0]), 1, "{}", tools[0].text());
    assert!(!tools.last().unwrap().images().is_empty());
    // The person's picture is never shortened.
    assert_eq!(state.messages[0].images().len(), 1);
    // Once: no result has the line twice.
    assert!(tools.iter().all(|m| removed(m) <= 1));
}

/// A message that is only a picture is a message, and starts a run.
#[tokio::test]
async fn an_image_only_message_starts_a_run() {
    let rig = Rig::seeing(0, limits(8), with_store);
    rig.mock.push_text("a cat");
    let run = rig
        .rt
        .start(
            "llm",
            user_message_with_images("", &[InboundImage::new(None, None, &png(32, 32, 1))]),
            None,
        )
        .await
        .expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    let Message::User { content } = &requests[0].messages[0] else {
        panic!("a user message");
    };
    assert_eq!(content.len(), 1, "{content:?}");
    assert!(matches!(content[0], adam_model::ContentPart::Image(_)));
    assert_eq!(requests[0].image_data.len(), 1);
    assert_eq!(state.messages[0].text(), "");
}

/// Whether `text` holds `image` in a form a durable place could: its bytes as they are, or all of
/// them in base64.
fn holds(text: &str, image: &[u8]) -> bool {
    use base64::Engine as _;
    let whole = base64::engine::general_purpose::STANDARD.encode(image);
    text.contains(&whole) || text.as_bytes().windows(image.len()).any(|w| w == image)
}

/// The record of a run as the store keeps it: its state, whole, the inbox with it.
async fn stored_state(rig: &Rig, run: RunId) -> String {
    let record = rig.store.load_run(run).await.unwrap().unwrap();
    serde_json::to_string(&record.state).unwrap()
}

/// The coder's way (an agent with a store): the bytes of an image are in the message that carried
/// it until the first step, and after that in no durable place: not in the journal, not in the
/// state, not in the stored record. Looked for as the bytes themselves and as all of them in base64.
#[tokio::test]
async fn the_journal_holds_no_image_bytes() {
    let rig = Rig::seeing(0, limits(8), with_store);
    rig.mock
        .push_tool_calls(vec![snap(1)])
        .push_tool_calls(vec![snap(2)])
        .push_text("done");
    let run = rig.rt.start("llm", looking(5), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (person, first, second) = (png(64, 64, 5), png(8, 8, 1), png(8, 8, 2));
    // The start message holds the person's image until a worker has taken the first step.
    assert!(holds(&stored_state(&rig, run).await, &person));
    let (_, state) = rig.finish(run).await;
    let entries = rig.store.journal_list(run).await.unwrap();
    assert!(entries.iter().any(|e| e.name.starts_with("image:")));
    let journal =
        serde_json::to_string(&entries.iter().map(|e| &e.payload).collect::<Vec<_>>()).unwrap();
    let state_json = serde_json::to_string(&state).unwrap();
    let record = stored_state(&rig, run).await;
    for image in [&person, &first, &second] {
        assert!(!holds(&journal, image), "the journal holds image bytes");
        assert!(!holds(&state_json, image), "the state holds image bytes");
        assert!(
            !holds(&record, image),
            "the stored record holds image bytes"
        );
    }
    assert!(state.pending_images.is_empty());
    // The references are there.
    assert!(journal.contains(&images_of(&state.messages)[0].sha256));
}

/// A folder agent (no store) is the other way, said exactly: the bytes of the images its tools keep
/// are in the journal of the step and in the state; the person's are in the state only, never in the
/// journal. The control of the test above: this is where the bytes are, by design.
#[tokio::test]
async fn a_folder_agent_journals_the_images_its_tools_keep_and_no_other() {
    let rig = Rig::seeing(0, limits(8), |b| b.tool(Snap));
    rig.mock.push_tool_calls(vec![snap(1)]).push_text("done");
    let run = rig.rt.start("llm", looking(2), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (_, state) = rig.finish(run).await;
    let (person, shot) = (png(64, 64, 2), png(8, 8, 1));
    let entries = rig.store.journal_list(run).await.unwrap();
    let journal =
        serde_json::to_string(&entries.iter().map(|e| &e.payload).collect::<Vec<_>>()).unwrap();
    assert!(holds(&journal, &shot), "the tool step journals its image");
    assert!(!holds(&journal, &person), "a person's image is a reference");
    let state_json = serde_json::to_string(&state).unwrap();
    assert!(holds(&state_json, &shot) && holds(&state_json, &person));
    // What the step journaled is counted, to the padding of its base64.
    assert!(
        (shot.len()..=shot.len() + 2).contains(&state.journaled_image_bytes),
        "{}",
        state.journaled_image_bytes
    );
}

/// An agent with no store keeps the bytes in its own state, within what a run keeps there, and the
/// model is sent them.
#[tokio::test]
async fn state_images_are_kept_in_the_state_within_the_bound() {
    let rig = Rig::seeing(0, limits(8), |b| b.tool(Snap));
    rig.mock.push_tool_calls(vec![snap(1)]).push_text("done");
    let run = rig.rt.start("llm", looking(2), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert_eq!(state.images.len(), 2, "{:?}", state.images.keys());
    // The control of the test above: here the bytes are in the state, by design.
    {
        use base64::Engine as _;
        let piece = base64::engine::general_purpose::STANDARD.encode(&png(64, 64, 2)[..39]);
        assert!(serde_json::to_string(&state).unwrap().contains(&piece));
    }
    assert!(state.kept_bytes() < adam_llm_agent::MAX_RUN_KEPT_BYTES);
    assert_eq!(requests[1].image_data.len(), 2);
    for image in images_of(&state.messages) {
        let kept = &state.images[&image.sha256];
        assert_eq!(kept.media_type, "image/png");
    }
    // Written before the field existed, or with no image: no member.
    let plain = serde_json::to_value(Conversation::new("x")).unwrap();
    assert!(plain.get("images").is_none() && plain.get("pending_images").is_none());
}

/// A compaction summarises the images of the turns it drops away with the rest: they leave the
/// history, and the bytes a run kept in its state for them with them. The person's picture is in the
/// first message, which is never dropped, and the turn that stays keeps its image. The break is the
/// compaction's, recorded, and nothing else moves.
#[tokio::test]
async fn a_compaction_summarises_the_images_of_dropped_turns_and_keeps_the_rest() {
    let rig = Rig::build_with(MockModel::new().seeing_images(limits(8)), 3500, true, |b| {
        b.tool(Snap)
    });
    rig.mock
        .push_tool_calls(vec![snap(1)])
        .push_tool_calls(vec![snap(2)])
        .push_tool_calls(vec![snap(3)])
        .push_text("SUMMARY: two pictures")
        .push_tool_calls(vec![snap(4)])
        .push_text("done");
    let run = rig.rt.start("llm", looking(9), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1, "{:?}", state.compactions);
    let mut expected = state.shortenings.clone();
    expected.push(state.compactions[0].turn);
    expected.sort_unstable();
    assert_eq!(breaks(&requests, &state.tools_appended), expected);
    // The task's picture, the picture of the turn that stayed, and the one made after.
    let names: Vec<&str> = images_of(&state.messages)
        .iter()
        .map(|i| i.name.as_deref().unwrap())
        .collect();
    assert_eq!(names, ["me.png", "shot3.png", "shot4.png"], "{names:?}");
    // The state keeps the bytes of what the history names and no more.
    assert_eq!(state.images.len(), 3, "{:?}", state.images.keys());
    // The summary request starts with the request before it and is sent the bytes of all four
    // pictures of the history it summarises.
    let at = requests.iter().position(is_summary).unwrap();
    assert!(open_messages(&requests[at]).starts_with(&open_messages(&requests[at - 1])));
    assert_eq!(requests[at].image_data.len(), 4);
    // After it the summarised turns' images are not sent.
    let last = requests.last().unwrap();
    assert_eq!(last.image_data.len(), 3);
}

// ---------------------------------------------------------------------------------------------
// What is sent follows the settings now; a request the provider refuses; the bounds of a run
// ---------------------------------------------------------------------------------------------

/// The texts of every part of a message, joined.
fn said(message: &Message) -> String {
    match message {
        Message::User { content } | Message::Assistant { content, .. } => content
            .iter()
            .map(adam_model::ContentPart::as_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Message::Tool { .. } => message.text(),
    }
}

/// The tool message that answers the call `id` of a request.
fn tool_answer<'a>(request: &'a ModelRequest, id: &str) -> &'a Message {
    request
        .messages
        .iter()
        .find(|m| matches!(m, Message::Tool { call_id, .. } if call_id == id))
        .unwrap_or_else(|| panic!("no answer to {id}"))
}

fn limits_with(sources: &[ImageOrigin], max_bytes: u64) -> ImageLimits {
    ImageLimits {
        max_bytes,
        max_count: 8,
        max_side: 4096,
        sources: sources.iter().copied().collect(),
    }
}

/// An image of a source the deployment stops sending between two turns is a line that says why, in
/// every request after, and nothing about it is recorded: the history still holds the reference.
#[tokio::test]
async fn a_source_removed_between_two_turns_is_said_and_not_recorded() {
    let all = limits_with(
        &[
            ImageOrigin::Person,
            ImageOrigin::Workspace,
            ImageOrigin::Mcp,
        ],
        1 << 20,
    );
    let rig = Rig::seeing(0, all, with_store);
    rig.mock
        .push_tool_calls(vec![snap_with(1, "mcp", 0)])
        .push_tool_calls(vec![snap_with(2, "workspace", 0)])
        .push_tool_calls(vec![snap_with(3, "workspace", 0)])
        .push_text("done");
    rig.change_settings_after_call(
        1,
        InputModalities::with_images(limits_with(
            &[ImageOrigin::Person, ImageOrigin::Workspace],
            1 << 20,
        )),
    );
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 4);
    // Before the change the image is sent, with its bytes.
    let before = tool_answer(&requests[1], "s1");
    assert_eq!(before.images().len(), 1);
    assert!(
        requests[1]
            .image_data
            .get(&before.images()[0].sha256)
            .is_some()
    );
    // After it: a line that says the true reason, and no bytes are read for it.
    let sha = before.images()[0].sha256.clone();
    for later in [&requests[2], &requests[3]] {
        let after = tool_answer(later, "s1");
        assert!(after.images().is_empty());
        assert!(
            after
                .text()
                .contains("this deployment no longer sends images from mcp"),
            "{}",
            after.text()
        );
        assert!(later.image_data.get(&sha).is_none());
    }
    // The same settings say the same thing: the later requests agree on it.
    assert_eq!(
        tool_answer(&requests[2], "s1"),
        tool_answer(&requests[3], "s1")
    );
    // The images of the sources still sent are sent.
    assert_eq!(tool_answer(&requests[3], "s2").images().len(), 1);
    // Nothing was recorded: the reference is in the history.
    let kept = state
        .messages
        .iter()
        .find(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "s1"))
        .unwrap();
    assert_eq!(kept.images().len(), 1);
}

/// A byte limit lowered between two turns is said the same way.
#[tokio::test]
async fn a_lowered_byte_limit_is_said_and_not_recorded() {
    let both = [ImageOrigin::Person, ImageOrigin::Workspace];
    let rig = Rig::seeing(0, limits_with(&both, 4096), with_store);
    rig.mock
        .push_tool_calls(vec![snap_with(1, "workspace", 2000)])
        .push_tool_calls(vec![snap_with(2, "workspace", 100)])
        .push_tool_calls(vec![snap_with(3, "workspace", 100)])
        .push_text("done");
    rig.change_settings_after_call(1, InputModalities::with_images(limits_with(&both, 1024)));
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    // The first image was admitted under the old limit and is over the new one.
    assert_eq!(tool_answer(&requests[1], "s1").images().len(), 1);
    for later in [&requests[2], &requests[3]] {
        let after = tool_answer(later, "s1");
        assert!(after.images().is_empty());
        assert!(
            after
                .text()
                .contains("it is over the 1.0 KiB this deployment sends"),
            "{}",
            after.text()
        );
    }
    // The person's image and the two small ones (under the new limit) are sent.
    assert_eq!(
        images_of(&requests[3].messages).len(),
        3,
        "{:#?}",
        requests[3].messages
    );
    // None of it was recorded.
    assert_eq!(images_of(&state.messages).len(), 4);
}

/// A keeper that counts the images it is asked to read.
struct Counting {
    inner: MemoryImages,
    loads: AtomicUsize,
}

#[async_trait]
impl adam_llm_agent::KeptImages for Counting {
    async fn keep(
        &self,
        scope: &str,
        sha256: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<(), adam_llm_agent::ImageKeepError> {
        self.inner.keep(scope, sha256, media_type, bytes).await
    }

    async fn load(
        &self,
        scope: &str,
        sha256: &str,
    ) -> Result<Vec<u8>, adam_llm_agent::ImageKeepError> {
        self.loads.fetch_add(1, SeqCst);
        self.inner.load(scope, sha256).await
    }
}

/// A model that stops seeing between two turns is sent a line for every image and the keeper is not
/// asked for a byte.
#[tokio::test]
async fn a_model_that_stops_seeing_is_sent_lines_and_no_bytes_are_read() {
    let keeper = Arc::new(Counting {
        inner: MemoryImages::new(),
        loads: AtomicUsize::new(0),
    });
    let rig = Rig::seeing(0, limits(8), {
        let keeper = keeper.clone();
        move |b| b.state(Arc::new(ImageStore(keeper))).tool(Snap)
    });
    rig.mock
        .push_tool_calls(vec![snap(1)])
        .push_tool_calls(vec![snap(2)])
        .push_text("done");
    let loads = Arc::new(AtomicUsize::new(usize::MAX));
    {
        let (keeper, loads) = (keeper.clone(), loads.clone());
        let arm = rig.arm.clone();
        rig.after_each_call(move |n| {
            if n == 0 {
                *arm.modalities.lock().unwrap() = Some(InputModalities::text_only());
                loads.store(keeper.loads.load(SeqCst), SeqCst);
            }
        });
    }
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, _) = rig.finish(run).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].image_data.len(), 1);
    for later in &requests[1..] {
        assert!(later.image_data.is_empty());
        assert!(images_of(&later.messages).is_empty());
        assert!(
            said(&later.messages[0]).contains("this model cannot see images"),
            "{}",
            said(&later.messages[0])
        );
    }
    // The bytes were read for the first request only.
    assert_eq!(keeper.loads.load(SeqCst), loads.load(SeqCst));
    assert!(loads.load(SeqCst) >= 1);
}

/// A keeper is asked for an image's bytes once: the turns after the first are served from the bytes
/// the first one checked, not read (and hashed) again for every request that names them.
#[tokio::test]
async fn the_bytes_of_an_image_are_read_once_for_the_run() {
    let keeper = Arc::new(Counting {
        inner: MemoryImages::new(),
        loads: AtomicUsize::new(0),
    });
    let rig = Rig::seeing(0, limits(8), {
        let keeper = keeper.clone();
        move |b| b.state(Arc::new(ImageStore(keeper))).tool(Snap)
    });
    rig.mock
        .push_tool_calls(vec![snap(1)])
        .push_tool_calls(vec![snap(2)])
        .push_text("done");
    let run = rig.rt.start("llm", looking(4), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, _) = rig.finish(run).await;
    assert_eq!(requests.len(), 3);
    let person = adam_llm_agent::sha256_hex(&png(64, 64, 4));
    for request in &requests {
        assert!(
            images_of(&request.messages)
                .iter()
                .any(|image| image.sha256 == person),
            "the person's image is in every request"
        );
        assert!(request.image_data.get(&person).is_some());
    }
    // The person's and the two tool images, each read once: six reads before the cache.
    assert_eq!(keeper.loads.load(SeqCst), 3);
}

/// The bytes of an image changed between two turns are the bytes already checked for its hash: the
/// reference stays in the request, the bytes it names are sent from the cache, and what was written
/// over the file is never sent. The file is read once, not again after the change.
#[tokio::test]
async fn an_image_changed_between_turns_is_sent_from_the_bytes_already_checked() {
    let keeper = Arc::new(Counting {
        inner: MemoryImages::new(),
        loads: AtomicUsize::new(0),
    });
    let rig = Rig::seeing(0, limits(8), {
        let keeper = keeper.clone();
        move |b| b.state(Arc::new(ImageStore(keeper))).tool(Big)
    });
    rig.mock
        .push_tool_calls(vec![call("c1", "big", json!({"n": 10}))])
        .push_text("done");
    {
        let (keeper, arm) = (keeper.clone(), rig.arm.clone());
        rig.after_each_call(move |n| {
            if n == 0 {
                let run = arm.run.lock().unwrap().unwrap();
                let sha = adam_llm_agent::sha256_hex(&png(64, 64, 4));
                let scope = adam_llm_agent::image_scope(None, &run.to_string());
                keeper
                    .inner
                    .tamper(&scope, &sha, b"changed behind its back");
            }
        });
    }
    let run = rig.rt.start("llm", looking(4), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, _) = rig.finish(run).await;
    assert_eq!(requests[0].image_data.len(), 1);
    // The reference is still in the second request, and its bytes are the ones the first request
    // checked: the changed file is not served, and it is not read again.
    assert_eq!(images_of(&requests[1].messages).len(), 1);
    assert_eq!(requests[1].image_data.len(), 1);
    let person = adam_llm_agent::sha256_hex(&png(64, 64, 4));
    let bytes = requests[1]
        .image_data
        .get(&person)
        .expect("the person's bytes");
    assert_eq!(bytes.as_ref(), png(64, 64, 4).as_slice());
    assert_ne!(bytes.as_ref(), b"changed behind its back");
    assert_eq!(
        keeper.loads.load(SeqCst),
        1,
        "the file was read once, before the change"
    );
}

/// A request the provider refuses for itself, with images no earlier request carried: they are
/// recorded as lines, once, and the request is sent again. Those an earlier request carried stay.
#[tokio::test]
async fn a_refused_request_loses_the_images_it_alone_carried() {
    let rig = Rig::seeing(0, limits(8), with_store);
    rig.mock
        .push_tool_calls(vec![snap(1)])
        .push_error(ModelError::invalid_request(
            "the image could not be decoded",
        ))
        .push_text("done");
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 3);
    // Request 1 carried the person's image (which request 0 got through) and the tool's.
    assert_eq!(images_of(&requests[1].messages).len(), 2);
    // Request 2: the tool's image is a line, the person's is untouched.
    assert_eq!(images_of(&requests[2].messages).len(), 1);
    assert_eq!(
        images_of(&requests[2].messages)[0].name.as_deref(),
        Some("me.png")
    );
    let line = tool_answer(&requests[2], "s1").text();
    assert!(
        line.contains("shot1.png (png")
            && line.contains("the model's service refused the request that carried it"),
        "{line}"
    );
    // It is the history now: recorded, with a stand-in, in the state.
    assert_eq!(images_of(&state.messages).len(), 1);
    assert!(
        state
            .messages
            .iter()
            .any(|m| m.text().contains("the model's service refused"))
    );
}

/// The first request of a run carries the person's picture and the provider refuses it: the picture
/// is a line from then on, in the history, with the bytes a folder agent kept for it gone.
#[tokio::test]
async fn a_refused_first_request_makes_the_person_picture_a_line() {
    for store in [true, false] {
        let rig = Rig::seeing(0, limits(8), move |b| if store { with_store(b) } else { b });
        rig.mock
            .push_error(ModelError::invalid_request("unsupported image"))
            .push_text("ok");
        let run = rig.rt.start("llm", looking(0), None).await.expect("start");
        *rig.arm.run.lock().unwrap() = Some(run);
        let (requests, state) = rig.finish(run).await;
        assert_eq!(requests.len(), 2, "{store}");
        assert_eq!(images_of(&requests[0].messages).len(), 1);
        assert!(images_of(&requests[1].messages).is_empty(), "{store}");
        assert!(requests[1].image_data.is_empty());
        assert!(
            said(&requests[1].messages[0]).contains("me.png (png"),
            "{}",
            said(&requests[1].messages[0])
        );
        assert!(images_of(&state.messages).is_empty());
        assert!(state.images.is_empty(), "no bytes are kept for a line");
    }
}

/// With no image that is new, a refusal is the failure it always was; one for another reason after
/// images went through is too.
#[tokio::test]
async fn a_refusal_with_no_new_image_fails_the_run() {
    let rig = Rig::seeing(0, limits(8), with_store);
    rig.mock
        .push_tool_calls(vec![big("c0", 10)])
        .push_error(ModelError::invalid_request("the request is not valid"));
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, view) = rig.finish_any(run).await;
    assert_eq!(view.status, RunStatus::Failed, "{view:#?}");
    assert!(
        view.error
            .as_deref()
            .is_some_and(|e| e.contains("the request is not valid")),
        "{:?}",
        view.error
    );
    // The person's picture was carried by a request that got through: it was not touched.
    assert_eq!(requests.len(), 2);
    assert_eq!(
        images_of(&conversation(&rig.rt, &view).await.messages).len(),
        1
    );
}

/// A lost commit after the refusal replays the same decision: the same lines, the same requests.
#[tokio::test]
async fn a_lost_commit_replays_a_refusal_the_same() {
    let run_it = |lose: Option<bool>| async move {
        let rig = Rig::seeing(0, limits(8), with_store);
        rig.mock
            .push_error(ModelError::invalid_request("unsupported image"))
            .push_text("ok");
        if let Some(after) = lose {
            rig.lose_the_commit_after_call(0, after);
        }
        let run = rig.rt.start("llm", looking(0), None).await.expect("start");
        *rig.arm.run.lock().unwrap() = Some(run);
        rig.finish(run).await
    };
    let (_, base) = run_it(None).await;
    for after in [false, true] {
        let (requests, state) = run_it(Some(after)).await;
        assert_eq!(state.messages, base.messages, "applied: {after}");
        assert!(images_of(&requests.last().unwrap().messages).is_empty());
    }
}

/// Deliver `text` and a picture to a run that has not started working.
async fn deliver(rig: &Rig, run: RunId, text: &str, id: &str, tag: u32) {
    let inbound = user_message_with_images(
        text,
        &[InboundImage::new(
            Some(&format!("{text}.png")),
            Some("image/png"),
            &png(16, 16, tag),
        )],
    )
    .with_id(id);
    rig.rt.deliver(run, inbound).await.expect("deliver");
}

/// The start message and the messages in the inbox are admitted in order under one count, whichever
/// agent: the third picture of a run that admits two is a line.
#[tokio::test]
async fn the_start_message_and_the_inbox_share_the_count() {
    for store in [true, false] {
        let rig = Rig::seeing(0, limits(2), move |b| if store { with_store(b) } else { b });
        rig.mock.push_text("ok");
        let run = rig.rt.start("llm", looking(1), None).await.expect("start");
        deliver(&rig, run, "m1", "id-1", 2).await;
        deliver(&rig, run, "m2", "id-2", 3).await;
        *rig.arm.run.lock().unwrap() = Some(run);
        let (_, state) = rig.finish(run).await;
        let names: Vec<String> = images_of(&state.messages)
            .iter()
            .map(|i| i.name.clone().unwrap())
            .collect();
        assert_eq!(names, ["me.png", "m1.png"], "{store}");
        let last = state
            .messages
            .iter()
            .rfind(|m| said(m).starts_with("m2"))
            .unwrap();
        assert!(
            said(last).contains("past the 2 images"),
            "{store}: {}",
            said(last)
        );
    }
}

/// A message that is in the inbox twice is admitted once.
#[tokio::test]
async fn a_message_twice_in_the_inbox_is_admitted_once() {
    let rig = Rig::seeing(0, limits(2), with_store);
    rig.mock.push_text("ok");
    let run = rig
        .rt
        .start("llm", user_message("start"), None)
        .await
        .expect("start");
    deliver(&rig, run, "m1", "same", 2).await;
    deliver(&rig, run, "m1", "same", 2).await;
    deliver(&rig, run, "m2", "id-2", 3).await;
    *rig.arm.run.lock().unwrap() = Some(run);
    let (_, state) = rig.finish(run).await;
    let names: Vec<String> = images_of(&state.messages)
        .iter()
        .map(|i| i.name.clone().unwrap())
        .collect();
    // The count is two, and the duplicate took none of it.
    assert_eq!(names, ["m1.png", "m2.png"]);
}

/// A tool that fails and returns an image: the model is shown no image, and nothing is kept.
#[tokio::test]
async fn the_images_of_a_failed_result_are_neither_shown_nor_kept() {
    let rig = Rig::seeing(0, limits(8), |b| b.tool(Snap));
    rig.mock
        .push_tool_calls(vec![call("f1", "snap", json!({"n": 1, "fail": true}))])
        .push_text("done");
    let run = rig
        .rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    let tool = state
        .messages
        .iter()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert!(tool.images().is_empty());
    assert!(state.images.is_empty());
    assert!(requests[1].image_data.is_empty());
    assert_eq!(
        state.journaled_image_bytes,
        0,
        "nothing was kept: {:?}",
        state.images.keys()
    );
}

/// A folder agent keeps the bytes of the images its history names and no others: a shortening that
/// removes the images of old results takes their bytes out of the state with them.
#[tokio::test]
async fn a_shortening_frees_the_bytes_of_the_images_it_removes() {
    let rig = Rig::seeing(4000, limits(8), |b| b.tool(Snap));
    for n in 1..=5 {
        rig.mock.push_tool_calls(vec![snap(n)]);
    }
    rig.mock.push_text("done");
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (_, state) = rig.finish(run).await;
    assert!(!state.shortenings.is_empty());
    let named: std::collections::BTreeSet<&str> = images_of(&state.messages)
        .iter()
        .map(|i| i.sha256.as_str())
        .collect();
    let kept: std::collections::BTreeSet<&str> = state.images.keys().map(String::as_str).collect();
    assert_eq!(kept, named);
    assert!(kept.len() < 6, "some of the six images were removed");
}

/// An agent with a store keeps images outside the room of the run's state; one without keeps them in
/// it, and the image that does not fit is a line.
#[tokio::test]
async fn the_room_of_the_state_bounds_a_folder_agent_and_no_other() {
    let big = |n| snap_with(n, "workspace", 900_000);
    let wide = ImageLimits {
        max_count: 32,
        ..limits(32)
    };
    // With a store: six images of 900 KB are 5.4 MB, over what a state may hold, and all are kept.
    let rig = Rig::seeing(0, wide.clone(), with_store);
    for n in 1..=6 {
        rig.mock.push_tool_calls(vec![big(n)]);
    }
    rig.mock.push_text("done");
    let run = rig
        .rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (_, state) = rig.finish(run).await;
    assert_eq!(images_of(&state.messages).len(), 6);
    assert!(state.images.is_empty());
    // Without one: the fifth does not fit in 4 MiB.
    let rig = Rig::seeing(0, wide, |b| b.tool(Snap));
    for n in 1..=6 {
        rig.mock.push_tool_calls(vec![big(n)]);
    }
    rig.mock.push_text("done");
    let run = rig
        .rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (_, state) = rig.finish(run).await;
    assert_eq!(images_of(&state.messages).len(), 4);
    assert!(state.kept_bytes() <= adam_llm_agent::MAX_RUN_KEPT_BYTES);
    let fifth = state
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .nth(4)
        .unwrap();
    assert!(fifth.text().contains("does not fit"), "{}", fifth.text());
}

/// What a folder agent's tool steps journal is bounded by the run's budget of file bytes, counted
/// over the run: a shortening frees the state, never the journal, so the image after the budget is
/// a line that says the run has kept as many as it may.
#[tokio::test]
async fn a_folder_agent_journals_at_most_a_runs_budget_of_image_bytes() {
    let wide = ImageLimits {
        max_count: 32,
        ..limits(32)
    };
    // A small budget of history: the old images are shortened away, and the state has room again.
    let rig = Rig::seeing(4000, wide, |b| b.tool(Snap));
    for n in 1..=8 {
        rig.mock
            .push_tool_calls(vec![snap_with(n, "workspace", 900_000)]);
    }
    rig.mock.push_text("done");
    let run = rig
        .rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (_, state) = rig.finish(run).await;
    assert!(!state.shortenings.is_empty(), "the history was shortened");
    let cap = adam_llm_agent::MAX_RUN_JOURNALED_IMAGE_BYTES;
    assert!(
        state.journaled_image_bytes <= cap + 900_300,
        "{}",
        state.journaled_image_bytes
    );
    assert!(
        state.journaled_image_bytes > state.kept_bytes(),
        "the journal's count is not the state's: {} against {}",
        state.journaled_image_bytes,
        state.kept_bytes()
    );
    let tools: Vec<&Message> = state
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .collect();
    let refused = tools
        .iter()
        .filter(|m| m.text().contains("kept as many image bytes as it may"))
        .count();
    assert!(refused >= 1, "an image past the budget is a line");
    assert!(tools.len() >= 7);
}

/// The summary of a prompt the provider rejected as too long says each image in a few words and
/// carries no bytes: the request that follows a rejection is not the place to send more of them.
#[tokio::test]
async fn the_summary_after_a_rejection_describes_images_and_sends_none() {
    let rig = Rig::build_with(MockModel::new().seeing_images(limits(8)), 0, true, |b| {
        b.tool(Snap)
    });
    for n in 1..=4 {
        rig.mock.push_tool_calls(vec![snap(n)]);
    }
    rig.mock
        .push_error(ModelError::ContextLength("prompt is too long".into()))
        .push_text("SUMMARY after the rejection")
        .push_text("done");
    let run = rig.rt.start("llm", looking(0), None).await.expect("start");
    *rig.arm.run.lock().unwrap() = Some(run);
    let (requests, state) = rig.finish(run).await;
    let at = requests
        .iter()
        .position(is_summary)
        .expect("a summary request");
    assert_eq!(state.compactions.len(), 1);
    assert_eq!(
        state.compactions[0].reason,
        adam_llm_agent::CompactionReason::Overflow
    );
    let summary = &requests[at];
    assert!(summary.image_data.is_empty());
    assert!(images_of(&summary.messages).is_empty());
    let described = summary.messages.iter().map(said).collect::<String>();
    assert!(described.contains("[image: me.png (png"), "{described}");
    // The request the provider rejected did carry them.
    assert!(!requests[at - 1].image_data.is_empty());
}

/// A source that decides once, from the run, what it offers: `big` is loaded only when the run's
/// context says so, and the context does not change. The tools array is the same bytes at every
/// request, so the prefix a provider cached still holds (ADR 0044, decision 1).
struct TaskGate;

#[async_trait]
impl ToolSource for TaskGate {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let loaded = ctx
            .context("load")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if loaded {
            return Vec::new();
        }
        tools
            .iter()
            .filter(|tool| **tool == "big")
            .map(|tool| Withheld::new(*tool, "big is not loaded for this run"))
            .collect()
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        None
    }
}

/// A tool a source holds back from the start stays held back: the head of every request is the
/// same bytes, whatever the run does with the tools it was shown.
#[tokio::test]
async fn a_tool_a_source_holds_back_from_the_start_keeps_the_head_of_every_request() {
    let rig = Rig::new(100_000, |b| b.tool_source(TaskGate));
    rig.mock
        .push_tool_calls(vec![call("c0", "handle", json!({}))])
        .push_tool_calls(vec![call("c1", "handle", json!({}))])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(breaks(&requests, &state.tools_appended), Vec::<u32>::new());
    assert_eq!(
        requests[0]
            .tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["handle"],
        "the held-back tool is not shown, and the head does not move"
    );
}

/// A source that loads a tool only when the run's task asks for it, as an `mcp.json` server's
/// `when` does: the task is read from the state, so a replay selects the same.
struct TaskSelects;

#[async_trait]
impl ToolSource for TaskSelects {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let loads = ctx
            .task()
            .is_some_and(|task| task.to_lowercase().contains("big"));
        if loads {
            return Vec::new();
        }
        tools
            .iter()
            .filter(|tool| **tool == "big")
            .map(|tool| Withheld::new(*tool, "big is not loaded for this run"))
            .collect()
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        None
    }
}

/// A tool a source selects from the task is the same tool after a lost commit: the task is the
/// first message of the journaled state, so the replayed request is the one the run would have
/// made (a selection stored only in memory would drop `big` here).
#[tokio::test]
async fn a_task_selected_tool_survives_a_lost_commit() {
    fn push_script(mock: &MockModel) {
        mock.push_tool_calls(vec![big("c0", 10)])
            .push_tool_calls(vec![call("c1", "handle", json!({}))])
            .push_text("done");
    }
    let baseline = {
        let rig = Rig::new(100_000, |b| b.tool_source(TaskSelects));
        push_script(&rig.mock);
        let run = rig.start_task("use big please").await;
        rig.finish(run).await
    };
    assert!(
        baseline.0[0].tools.iter().any(|t| t.name == "big"),
        "the task loads `big` in the baseline"
    );
    for call in [0, 1] {
        let rig = Rig::new(100_000, |b| b.tool_source(TaskSelects));
        push_script(&rig.mock);
        rig.lose_the_commit_after_call(call, false);
        let run = rig.start_task("use big please").await;
        let (requests, state) = rig.finish(run).await;
        let label = format!("lost commit after call {call}");
        assert_eq!(rig.store.injected(Method::CommitRun), 1, "{label}");
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

/// A group of tools the run loads during itself, as `adam-assembly`'s `Groups` does: withheld
/// until the run's state names the group's loader, then appended after every other tool. `big` and
/// `handle` are the agent's own tools; `docs__*` are this group's.
struct Deferred {
    group: &'static str,
    specs: Vec<ToolSpec>,
    note: &'static str,
    keywords: &'static [&'static str],
}

impl Deferred {
    fn new(
        group: &'static str,
        tools: &[&'static str],
        note: &'static str,
        keywords: &'static [&'static str],
    ) -> Self {
        Self {
            group,
            specs: tools
                .iter()
                .map(|name| ToolSpec {
                    name: (*name).to_owned(),
                    description: format!("the {name} tool"),
                    parameters: json!({"type": "object", "properties": {}}),
                })
                .collect(),
            note,
            keywords,
        }
    }

    fn loader(&self) -> String {
        format!("load__group__{}", self.group)
    }

    fn matches(&self, task: Option<&str>) -> bool {
        task.is_some_and(|task| {
            let task = task.to_lowercase();
            self.keywords
                .iter()
                .any(|word| !word.is_empty() && task.contains(&word.to_lowercase()))
        })
    }

    fn loaded(&self, ctx: &SourceCtx) -> bool {
        self.matches(ctx.task()) || ctx.loaded().iter().any(|one| one.loader == self.loader())
    }
}

#[async_trait]
impl ToolSource for Deferred {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn appended(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        // A task-loaded group is not appended: its tools were never withheld.
        if self.matches(ctx.task()) || !self.loaded(ctx) {
            Vec::new()
        } else {
            self.specs.clone()
        }
    }

    async fn loaders(&self, ctx: &SourceCtx) -> Vec<adam_llm_agent::Loader> {
        if self.matches(ctx.task()) {
            return Vec::new();
        }
        vec![adam_llm_agent::Loader {
            name: self.loader(),
            holds: self.specs.iter().map(|spec| spec.name.clone()).collect(),
            note: self.note.to_owned(),
            note_is_server: true,
        }]
    }

    async fn loaded_by_task(&self, ctx: &SourceCtx) -> Vec<adam_llm_agent::Loaded> {
        if self.matches(ctx.task()) {
            vec![adam_llm_agent::Loaded {
                loader: self.loader(),
                tools: self.specs.iter().map(|spec| spec.name.clone()).collect(),
                note: self.note.to_owned(),
                note_is_server: true,
            }]
        } else {
            Vec::new()
        }
    }

    async fn loaded_tools(&self, ctx: &SourceCtx) -> Option<Vec<String>> {
        Some(if self.loaded(ctx) {
            self.specs.iter().map(|spec| spec.name.clone()).collect()
        } else {
            Vec::new()
        })
    }

    /// The group's words reach the prompt only when the run has loaded it, and then they follow
    /// everything: the trait's append-words mechanism.
    async fn appended_instructions(&self, ctx: &SourceCtx) -> Option<String> {
        self.loaded(ctx)
            .then(|| format!("## {}\n\n> {} words", self.group, self.group))
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        if self.loaded(ctx) {
            return Vec::new();
        }
        self.specs
            .iter()
            .filter(|spec| tools.contains(&spec.name.as_str()))
            .map(|spec| {
                Withheld::new(
                    spec.name.clone(),
                    format!("`{}` is not loaded for this run", spec.name),
                )
            })
            .collect()
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        None
    }
}

/// The `load__group__<name>` tool of `adam-assembly`, in miniature: a fixed tool whose result says
/// what the run has loaded, which the loop applies to the state.
struct LoadGroup {
    group: &'static str,
    tools: Vec<String>,
    note: &'static str,
}

impl LoadGroup {
    fn loader(&self) -> String {
        format!("load__group__{}", self.group)
    }
}

#[async_trait]
impl Tool for LoadGroup {
    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.loader(),
            description: format!("loads the `{}` group", self.group),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let loader = self.loader();
        if ctx.loaded().iter().any(|one| one.loader == loader) {
            return Ok(ToolOutput::error("already loaded"));
        }
        let mut loaded: Vec<adam_llm_agent::Loaded> = ctx.loaded().to_vec();
        loaded.push(adam_llm_agent::Loaded {
            loader,
            tools: self.tools.clone(),
            note: self.note.to_owned(),
            note_is_server: false,
        });
        let mut out = ToolOutput::text(format!("loaded the group {}\n{}", self.group, self.note));
        out.loaded = loaded;
        Ok(out)
    }
}

/// The agent with deferred groups: their tools are own tools, withheld until the run loads them.
fn with_groups(
    builder: LlmAgentBuilder,
    groups: &[(
        &'static str,
        &'static [&'static str],
        &'static str,
        &'static [&'static str],
    )],
) -> LlmAgentBuilder {
    let mut builder = builder.tool(Fixed("big")).tool(Fixed("handle"));
    for (group, tools, note, keywords) in groups {
        for tool in *tools {
            builder = builder.tool(Fixed(tool));
        }
        builder = builder
            .tool(LoadGroup {
                group,
                tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
                note,
            })
            .tool_source(Deferred::new(group, tools, note, keywords));
    }
    builder
}

fn with_deferred(builder: LlmAgentBuilder) -> LlmAgentBuilder {
    with_groups(
        builder,
        &[(
            "docs",
            &["docs__search", "docs__read"],
            "the docs words",
            &[],
        )],
    )
}

/// An own tool that answers `<name>-out`, for a group's tools.
struct Fixed(&'static str);

#[async_trait]
impl Tool for Fixed {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.to_owned(),
            description: format!("the {} tool", self.0),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("{}-out", self.0)))
    }
}

/// The tools a request offered, by name, in the order the model saw them.
fn tool_names(request: &ModelRequest) -> Vec<String> {
    request.tools.iter().map(|t| t.name.clone()).collect()
}

/// A group load appends the group's tools to the next request, once, and the state records the
/// loader and the turn the head moved: the tools array is the one before it plus a suffix, never an
/// insertion, and the messages still start with the ones before.
#[tokio::test]
async fn a_group_load_appends_its_tools_and_records_the_turn() {
    let rig = Rig::new(0, with_deferred);
    rig.mock
        .push_tool_calls(vec![call("c1", "load__group__docs", json!({}))])
        .push_tool_calls(vec![call("c2", "docs__search", json!({}))])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(
        tool_names(&requests[0]),
        ["big", "handle", "load__group__docs"],
        "the group's tools are not shown before the run loads it"
    );
    assert_eq!(
        tool_names(&requests[1]),
        [
            "big",
            "handle",
            "load__group__docs",
            "docs__search",
            "docs__read"
        ],
        "the next request is the one before it plus the group's tools"
    );
    assert_eq!(
        &requests[1].tools[..requests[0].tools.len()],
        &requests[0].tools[..],
        "the array grew by a suffix, never by an insertion"
    );
    assert_eq!(
        requests[2].tools, requests[1].tools,
        "the tools do not move again, and appear once"
    );
    assert_eq!(
        state.loaded.len(),
        1,
        "the load is one entry: {:?}",
        state.loaded
    );
    assert_eq!(state.loaded[0].loader, "load__group__docs");
    assert_eq!(state.loaded[0].tools, ["docs__search", "docs__read"]);
    assert_eq!(state.loaded[0].note, "the docs words");
    assert_eq!(
        state.loaders[0].name, "load__group__docs",
        "the loaders are recorded on the first turn"
    );
    assert_eq!(
        state.tools_appended,
        [1],
        "the turn of the request that grew"
    );
    assert_eq!(
        breaks(&requests, &state.tools_appended),
        Vec::<u32>::new(),
        "the messages' prefix rule is unchanged"
    );
    // The call to the loaded tool ran: its result is in the last request.
    let text = format!("{:?}", requests[2].messages);
    assert!(text.contains("docs__search-out"), "{text}");
    assert!(
        text.contains("the docs words"),
        "the note is in the load's result: {text}"
    );
}

/// A run that never loads a group records no break: the head is the same bytes at every request
/// and the state has no member for a load.
#[tokio::test]
async fn a_run_that_never_loads_a_group_records_no_break() {
    let rig = Rig::new(0, with_deferred);
    rig.mock
        .push_tool_calls(vec![big("c0", 5)])
        .push_tool_calls(vec![call("c1", "handle", json!({}))])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert!(state.loaded.is_empty());
    assert!(state.tools_appended.is_empty());
    assert_eq!(breaks(&requests, &state.tools_appended), Vec::<u32>::new());
    let written = serde_json::to_value(&state).unwrap();
    assert!(written.get("loaded").is_none(), "{written}");
    assert!(written.get("tools_appended").is_none(), "{written}");
}

/// The load is state: a run whose commit is lost after the load replays the same list, the same
/// requests and the same recorded turn, and the group's tools appear once. A resumed run builds the
/// same bytes.
#[tokio::test]
async fn a_killed_run_resumes_with_the_same_bytes() {
    fn push_script(mock: &MockModel) {
        mock.push_tool_calls(vec![call("c1", "load__group__docs", json!({}))])
            .push_tool_calls(vec![call("c2", "docs__search", json!({}))])
            .push_text("done");
    }
    let baseline = {
        let rig = Rig::new(0, with_deferred);
        push_script(&rig.mock);
        let run = rig.start().await;
        rig.finish(run).await
    };
    assert_eq!(baseline.1.loaded.len(), 1);
    for call in [0, 1] {
        for after_apply in [false, true] {
            let rig = Rig::new(0, with_deferred);
            push_script(&rig.mock);
            rig.lose_the_commit_after_call(call, after_apply);
            let run = rig.start().await;
            let (requests, state) = rig.finish(run).await;
            let label = format!("after call {call}, applied before the loss: {after_apply}");
            assert_eq!(rig.store.injected(Method::CommitRun), 1, "{label}");
            assert_eq!(state.loaded, baseline.1.loaded, "{label}");
            assert_eq!(state.tools_appended, [1], "{label}");
            assert_eq!(state.messages, baseline.1.messages, "{label}");
            for (got, want) in requests.iter().zip(&baseline.0) {
                assert_eq!(
                    serde_json::to_string(got).unwrap(),
                    serde_json::to_string(want).unwrap(),
                    "{label}: the resumed request is the baseline's bytes"
                );
            }
            let last = requests.last().unwrap();
            assert!(
                last.tools.iter().any(|t| t.name == "docs__search"),
                "{label}: the replay kept the load"
            );
            assert_eq!(
                breaks(&requests, &state.tools_appended),
                Vec::<u32>::new(),
                "{label}"
            );
        }
    }
}

/// The loader's schema is a constant: the same bytes at every turn of a run, before and after a
/// load, and in a continuation, so the tool list never moves with the group's contents.
#[tokio::test]
async fn the_loader_schemas_are_the_same_bytes_at_every_turn() {
    let rig = Rig::new(0, with_deferred);
    rig.mock
        .push_tool_calls(vec![call("c1", "load__group__docs", json!({}))])
        .push_text("done")
        .push_text("again");
    // The test is about the loader's spec, which the request carries; the rig's model records it.
    let run = rig.start().await;
    let (requests, _) = rig.finish(run).await;
    let spec = |request: &ModelRequest| {
        request
            .tools
            .iter()
            .find(|tool| tool.name == "load__group__docs")
            .map(|tool| serde_json::to_string(tool).unwrap())
            .expect("the loader is offered")
    };
    let first = spec(&requests[0]);
    for request in &requests[1..] {
        assert_eq!(spec(request), first, "the loader's bytes moved");
    }
}

/// Two loads in one reply: the tools array grows once, at the next request, by both groups in the
/// order the calls ran, and the state records that one turn once.
#[tokio::test]
async fn two_loads_in_one_reply_apply_in_order() {
    let rig = Rig::new(0, |builder| {
        with_groups(
            builder,
            &[
                ("docs", &["docs__search"], "the docs words", &[]),
                ("linear", &["linear__list"], "the linear words", &[]),
            ],
        )
    });
    rig.mock
        .push_tool_calls(vec![
            call("c1", "load__group__docs", json!({})),
            call("c2", "load__group__linear", json!({})),
        ])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(
        tool_names(&requests[1]),
        [
            "big",
            "handle",
            "load__group__docs",
            "load__group__linear",
            "docs__search",
            "linear__list"
        ],
        "both groups' tools, in the order the calls ran"
    );
    assert_eq!(
        state
            .loaded
            .iter()
            .map(|one| one.loader.as_str())
            .collect::<Vec<_>>(),
        ["load__group__docs", "load__group__linear"]
    );
    assert_eq!(
        state.tools_appended,
        [1],
        "the array grew once, at one request"
    );
    assert_eq!(breaks(&requests, &state.tools_appended), Vec::<u32>::new());
}

/// Two loaders in **parallel** (no `after_previous`, `max_parallel` above one): each reads the
/// turn-start state, so each result carries only its own entry; the loop merges them by name in
/// call order.
#[tokio::test]
async fn two_loads_in_parallel_apply_in_order() {
    let rig = Rig::new(0, |builder| {
        with_groups(
            builder,
            &[
                ("docs", &["docs__search"], "the docs words", &[]),
                ("linear", &["linear__list"], "the linear words", &[]),
            ],
        )
        .max_parallel(4)
    });
    rig.mock
        .push_tool_calls(vec![
            call("c1", "load__group__docs", json!({})),
            call("c2", "load__group__linear", json!({})),
        ])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(
        state
            .loaded
            .iter()
            .map(|one| one.loader.as_str())
            .collect::<Vec<_>>(),
        ["load__group__docs", "load__group__linear"],
        "the merge follows the model's call order, whatever finished first"
    );
    assert_eq!(
        tool_names(&requests[1]),
        [
            "big",
            "handle",
            "load__group__docs",
            "load__group__linear",
            "docs__search",
            "linear__list"
        ],
        "both groups' tools are appended"
    );
}

/// A load appends to the prompt even when a source **after** the one that grows contributes words
/// of its own: the loaded source's section must follow everything the request before carried,
/// never sit between two parts of it.
#[tokio::test]
async fn a_load_appends_to_a_prompt_another_source_already_grew() {
    struct Always;

    #[async_trait]
    impl ToolSource for Always {
        async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
            Vec::new()
        }

        async fn instructions(&self, _ctx: &SourceCtx) -> Option<String> {
            Some("the person named helper".into())
        }

        async fn call(
            &self,
            _ctx: &ToolCtx,
            _name: &str,
            _args: Value,
        ) -> Option<Result<ToolOutput, ToolError>> {
            None
        }
    }

    let rig = Rig::new(0, |builder| with_deferred(builder).tool_source(Always));
    rig.mock
        .push_tool_calls(vec![call("c1", "load__group__docs", json!({}))])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    let before = requests[0].system.as_deref().unwrap_or_default();
    let after = requests[1].system.as_deref().unwrap_or_default();
    assert!(
        before.ends_with("the person named helper"),
        "the other source's words are in the prompt from the first request: {before:?}"
    );
    assert!(
        after.starts_with(before),
        "the system prompt at the load is the one before it plus a suffix: {after:?}"
    );
    assert!(
        after.ends_with("> docs words"),
        "the loaded group's words follow everything: {after:?}"
    );
    assert_eq!(state.tools_appended, [1]);
    assert_eq!(breaks(&requests, &state.tools_appended), Vec::<u32>::new());
}

/// A seeded resume at turn 4: the restored tools are offered, the requests are a prefix, and no
/// turn records a growth (the seed's carried set already names them).
#[tokio::test]
async fn a_seeded_resume_records_no_growth() {
    let rig = Rig::new(0, with_deferred);
    rig.mock
        .push_tool_calls(vec![call("c1", "docs__search", json!({}))])
        .push_text("done");
    let run = rig.start().await;
    rig.seed_stored_state(run, 4, &["docs"], 0).await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_names(&requests[0]),
        [
            "big",
            "handle",
            "load__group__docs",
            "docs__search",
            "docs__read"
        ],
        "the seeded group's tools are offered"
    );
    assert_eq!(
        &requests[1].tools[..requests[0].tools.len()],
        &requests[0].tools[..],
        "the requests are a prefix"
    );
    assert!(
        state.tools_appended.is_empty(),
        "the seed is not a growth: {:?}",
        state.tools_appended
    );
    assert_eq!(state.loaded[0].loader, "load__group__docs");
    assert_eq!(state.loaded[0].tools, ["docs__search", "docs__read"]);
    assert_eq!(state.loaded[0].note, "the docs words");
    assert!(state.loaded[0].note_is_server);
}

/// A seeded group filled from a `Loader` keeps its note: the loader carries it, so the recap can
/// label the server's words.
#[tokio::test]
async fn a_seeded_group_filled_from_a_loader_keeps_its_note() {
    let rig = Rig::summarising(1000, with_deferred);
    rig.mock.push_text("SUMMARY: seeded").push_text("done");
    let run = rig.start_task("go").await;
    rig.seed_stored_state(run, 4, &["docs"], 8).await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.loaded[0].note, "the docs words");
    assert!(state.loaded[0].note_is_server);
    let marker = state.messages[0].text();
    assert!(
        marker.contains("(the server's words: the docs words)"),
        "{marker}"
    );
}

/// A resume whose first transition compacts seeds and fills before the marker is written: the
/// marker names the seeded group through the real transition.
#[tokio::test]
async fn a_resume_that_compacts_first_names_the_seeded_group() {
    let rig = Rig::summarising(1000, with_deferred);
    rig.mock.push_text("SUMMARY: seeded").push_text("done");
    let run = rig.start_task("go").await;
    rig.seed_stored_state(run, 4, &["docs"], 8).await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1, "the resume compacted first");
    let marker = state.messages[0].text();
    assert!(marker.contains("load__group__docs"), "{marker}");
    assert!(marker.contains("docs__search"), "{marker}");
}

/// A seeded resume whose task also loads a **different** group journals that group too: the
/// recap names every group the run's task matches, not only the seeded one.
#[tokio::test]
async fn a_seeded_resume_names_a_different_task_loaded_group() {
    let rig = Rig::summarising(1000, |builder| {
        with_groups(
            builder,
            &[
                ("docs", &["docs__search"], "the docs words", &[]),
                ("linear", &["linear__list"], "the linear words", &["linear"]),
            ],
        )
    });
    rig.mock.push_text("SUMMARY: seeded").push_text("done");
    let run = rig.start_task("go linear").await;
    rig.seed_stored_state(run, 4, &["docs"], 8).await;
    let (_, state) = rig.finish(run).await;
    assert_eq!(state.compactions.len(), 1);
    let marker = state.messages[0].text();
    assert!(marker.contains("load__group__docs"), "{marker}");
    assert!(marker.contains("load__group__linear"), "{marker}");
    assert!(marker.contains("linear__list"), "{marker}");
}
