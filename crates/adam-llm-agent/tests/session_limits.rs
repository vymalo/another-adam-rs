//! A round of a session reads its limits from the session header (ADR 0054, slice 1c): each of
//! `max_turns`, `max_tool_calls`, `max_output_tokens` and `max_parallel_tool_calls` narrows the run at
//! the call it governs, never raises what the agent has, and `max_output_tokens` is never `0`.
//! (`context_window` is in `compaction.rs`, `time_limit` and `max_rounds` in `sessions.rs`.)
//!
//! Slice 6 adds `token_budget`: `spent`, the session's tokens so far, is read from the header and
//! carried in it, and a round whose spend reaches the budget ends at its closing turn with the
//! words `token budget reached`.
//!
//! The runs here start a round the way the A2A front does: a first message that carries a header.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    CLOSING_TURN, Conversation, LimitKind, Limits, LlmAgent, Tool, ToolAccess, ToolCtx, ToolError,
    ToolOutput, user_message,
};
use adam_model::{
    Message, MockModel, ModelRequest, ModelResponse, ToolCall, ToolChoice, ToolSpec, Usage,
};
use adam_runtime::{RetryPolicy, RunView, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};

/// A tool that takes a moment, runs beside its like, and counts how many run at once.
struct Probe {
    running: AtomicUsize,
    most: AtomicUsize,
    started: Mutex<Vec<String>>,
}

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "probe".into(),
            description: "probe".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.started.lock().unwrap().push(ctx.call_id().to_owned());
        let now = self.running.fetch_add(1, SeqCst) + 1;
        self.most.fetch_max(now, SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.running.fetch_sub(1, SeqCst);
        Ok(ToolOutput::text(format!("ok:{}", ctx.call_id())))
    }
}

fn probe_call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "probe".into(),
        arguments: json!({ "tag": id }),
        ..Default::default()
    }
}

struct Rig {
    mock: Arc<MockModel>,
    probe: Arc<Probe>,
    rt: Runtime,
}

struct Shared(Arc<Probe>);

#[async_trait]
impl Tool for Shared {
    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }
    fn access(&self, args: &Value) -> ToolAccess {
        self.0.access(args)
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.0.call(ctx, args).await
    }
}

impl Rig {
    fn new(limits: Limits, max_parallel: usize) -> Self {
        let mock = Arc::new(MockModel::new());
        let probe = Arc::new(Probe {
            running: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            started: Mutex::default(),
        });
        let agent = LlmAgent::builder("llm", mock.clone(), "test-model")
            .limits(limits)
            .max_parallel(max_parallel)
            .tool(Shared(probe.clone()))
            .build();
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store)
            .agent(agent)
            .worker_id(format!("w-{}", RunId::new()))
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .retry(
                RetryPolicy::new(3)
                    .with_backoff(Duration::from_millis(10), Duration::from_millis(10))
                    .with_multiplier(1.0),
            )
            .build();
        Self { mock, probe, rt }
    }

    /// Run a round of a session whose header holds `config` (`None`: no config) to its end.
    async fn round(&self, config: Option<Value>) -> (Vec<ModelRequest>, Conversation) {
        let (requests, state, _) = self.round_with(config, 0, RunStatus::Done).await;
        (requests, state)
    }

    /// [`round`](Self::round), ending in `want`: a run that fails at a limit still reports its
    /// state and the requests it made.
    async fn round_until(
        &self,
        config: Option<Value>,
        want: RunStatus,
    ) -> (Vec<ModelRequest>, Conversation, RunView) {
        self.round_with(config, 0, want).await
    }

    /// A round whose session header already says `spent` tokens were spent before it, ending in
    /// `want`: what a later round of a session is.
    async fn round_with(
        &self,
        config: Option<Value>,
        spent: u64,
        want: RunStatus,
    ) -> (Vec<ModelRequest>, Conversation, RunView) {
        let mut first = user_message("go");
        let session = RunId::new();
        let mut header = json!({
            "id": session,
            "agent": "llm",
            "description": "a round",
            "keep_open": false,
            "round": 1,
        });
        if let Some(config) = config {
            header["config"] = config;
        }
        if spent > 0 {
            header["spent"] = json!(spent);
        }
        first.payload["session"] = header;
        // A round of a session is a child run, which is where its header is read from.
        let run = session;
        self.rt
            .start_child_continuing(
                session,
                session,
                "llm",
                first,
                &adam_runtime::session_conversation_id("conversation", session),
                None,
            )
            .await
            .expect("start");
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let rt = self.rt.clone();
        let worker = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let view = loop {
            let view = self.rt.view(run).await.unwrap().unwrap();
            if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
                break view;
            }
            assert!(Instant::now() < deadline, "timed out: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = stop.send(());
        worker.await.unwrap().unwrap();
        assert_eq!(view.status, want, "{view:#?}");
        let state = self.rt.state(run).await.unwrap().unwrap();
        (
            self.mock.requests(),
            serde_json::from_value(state).expect("a Conversation"),
            view,
        )
    }
}

fn tool_texts(state: &Conversation) -> Vec<String> {
    state
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// max_output_tokens: the request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_output_tokens_of_the_header_is_the_cap_of_the_request() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("done");
    let (requests, _) = rig.round(Some(json!({"maxOutputTokens": 512}))).await;
    assert_eq!(requests[0].max_output_tokens, Some(512));

    // No config: the agent's own 4,096.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("done");
    let (requests, _) = rig.round(None).await;
    assert_eq!(requests[0].max_output_tokens, Some(4096));
}

#[tokio::test]
async fn max_output_tokens_of_the_header_never_raises_the_agents_and_is_never_zero() {
    // A header written for a larger folder: the agent's cap wins.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("done");
    let (requests, _) = rig.round(Some(json!({"maxOutputTokens": 9000}))).await;
    assert_eq!(requests[0].max_output_tokens, Some(4096));

    // A folder that leaves the cap to the model (`0`): the session's cap is the cap.
    let to_model = Limits {
        max_output_tokens: 0,
        ..Limits::default()
    };
    let rig = Rig::new(to_model, 10);
    rig.mock.push_text("done");
    let (requests, _) = rig.round(Some(json!({"maxOutputTokens": 1000}))).await;
    assert_eq!(requests[0].max_output_tokens, Some(1000));

    // A header that says `0` narrows nothing (it must not widen a folder's cap to the model's).
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("done");
    let (requests, _) = rig.round(Some(json!({"maxOutputTokens": 0}))).await;
    assert_eq!(requests[0].max_output_tokens, Some(4096));
}

// ---------------------------------------------------------------------------
// max_turns, max_tool_calls: the closing turn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn max_turns_of_the_header_ends_the_round_at_its_closing_turn() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_tool_calls(vec![probe_call("c1")]);
    rig.mock.push_text("closing words");
    let (requests, state) = rig.round(Some(json!({"maxTurns": 1}))).await;
    assert_eq!(requests.len(), 2);
    assert!(
        matches!(requests[1].tool_choice, ToolChoice::None),
        "the second call is the closing turn, with no tool callable"
    );
    assert_eq!(state.turns, 2);

    // The same script with no config: the second call still offers the tools.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_tool_calls(vec![probe_call("c1")]);
    rig.mock.push_text("done");
    let (requests, _) = rig.round(None).await;
    assert!(!matches!(requests[1].tool_choice, ToolChoice::None));
}

#[tokio::test]
async fn max_turns_of_the_header_never_raises_the_agents() {
    let limits = Limits {
        max_turns: 1,
        ..Limits::default()
    };
    let rig = Rig::new(limits, 10);
    rig.mock.push_tool_calls(vec![probe_call("c1")]);
    rig.mock.push_text("closing words");
    let (requests, _) = rig.round(Some(json!({"maxTurns": 40}))).await;
    assert!(matches!(requests[1].tool_choice, ToolChoice::None));
}

#[tokio::test]
async fn max_tool_calls_of_the_header_refuses_a_turn_that_would_go_over_it() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock
        .push_tool_calls(vec![probe_call("c1"), probe_call("c2")]);
    rig.mock.push_text("closing words");
    let (requests, state) = rig.round(Some(json!({"maxToolCalls": 1}))).await;
    assert!(
        rig.probe.started.lock().unwrap().is_empty(),
        "none of the turn's calls ran"
    );
    let texts = tool_texts(&state);
    assert!(
        texts.iter().all(|t| t.contains("max_tool_calls = 1")),
        "{texts:?}"
    );
    assert!(matches!(requests[1].tool_choice, ToolChoice::None));

    // At the limit, one call runs.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_tool_calls(vec![probe_call("c1")]);
    rig.mock.push_text("done");
    rig.round(Some(json!({"maxToolCalls": 1}))).await;
    assert_eq!(rig.probe.started.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// max_parallel_tool_calls
// ---------------------------------------------------------------------------

fn four() -> Vec<ToolCall> {
    (1..=4).map(|i| probe_call(&format!("c{i}"))).collect()
}

#[tokio::test]
async fn max_parallel_tool_calls_of_the_header_narrows_how_many_run_at_once() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_tool_calls(four());
    rig.mock.push_text("done");
    rig.round(Some(json!({"maxParallelToolCalls": 2}))).await;
    assert_eq!(rig.probe.most.load(SeqCst), 2);

    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_tool_calls(four());
    rig.mock.push_text("done");
    rig.round(Some(json!({"maxParallelToolCalls": 1}))).await;
    assert_eq!(rig.probe.most.load(SeqCst), 1);

    // No config: all four at once.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_tool_calls(four());
    rig.mock.push_text("done");
    rig.round(None).await;
    assert_eq!(rig.probe.most.load(SeqCst), 4);
}

#[tokio::test]
async fn max_parallel_tool_calls_of_the_header_never_raises_the_agents() {
    let rig = Rig::new(Limits::default(), 2);
    rig.mock.push_tool_calls(four());
    rig.mock.push_text("done");
    rig.round(Some(json!({"maxParallelToolCalls": 8}))).await;
    assert_eq!(rig.probe.most.load(SeqCst), 2);
}

// ---------------------------------------------------------------------------
// token_budget: the closing turn, and `spent` carried in the header
// ---------------------------------------------------------------------------

/// A text answer that took `input` and `output` tokens.
fn answer(text: &str, input: u64, output: u64) -> ModelResponse {
    let mut response = ModelResponse::text(text);
    response.usage = Usage::new(input, output);
    response
}

/// A turn that asks for `calls`, having taken `input` and `output` tokens.
fn tool_turn(calls: Vec<ToolCall>, input: u64, output: u64) -> ModelResponse {
    let mut response = ModelResponse::tool_calls(calls);
    response.usage = Usage::new(input, output);
    response
}

#[tokio::test]
async fn token_budget_of_the_header_ends_the_round_at_its_closing_turn() {
    let rig = Rig::new(Limits::default(), 10);
    // The first call spends past the budget (1,800 of 1,000) and asks for a tool, so a second
    // turn would follow: it is the closing turn instead.
    rig.mock
        .push_response(tool_turn(vec![probe_call("c1")], 900, 900));
    rig.mock.push_response(answer("closing words", 10, 5));
    let (requests, state) = rig.round(Some(json!({"tokenBudget": 1000}))).await;
    assert_eq!(requests.len(), 2);
    assert!(
        matches!(requests[1].tool_choice, ToolChoice::None),
        "the second call is the closing turn, with no tool callable"
    );
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|message| message.text() == CLOSING_TURN),
        "the closing turn carries its own instruction"
    );
    let hit = state.limit.as_ref().expect("the round hit a limit");
    assert_eq!(hit.limit, LimitKind::TokenBudget);
    let output = hit.answer.as_ref().expect("the closing answer");
    assert_eq!(output["limited"], true);
    assert_eq!(output["limit"], "token_budget");
    assert_eq!(output["text"], "closing words");
    // The spend is in the state and in the header it commits: both calls, input and output.
    assert_eq!(state.spent, 1_815);
    let header = state.session.as_ref().expect("a header");
    assert_eq!(header.spent, 1_815);
    assert_eq!(header.spent, state.spent, "one number, one place");

    // The same script with no budget: the second call still offers the tools, and the round is no
    // closing turn.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock
        .push_response(tool_turn(vec![probe_call("c1")], 900, 900));
    rig.mock.push_text("done");
    let (requests, state) = rig.round(None).await;
    assert!(!matches!(requests[1].tool_choice, ToolChoice::None));
    assert!(state.limit.is_none());
}

#[tokio::test]
async fn a_closing_turn_that_says_nothing_fails_with_the_words_token_budget_reached() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock
        .push_response(tool_turn(vec![probe_call("c1")], 1_200, 0));
    rig.mock.push_response(answer("", 10, 5));
    let (requests, state, view) = rig
        .round_until(Some(json!({"tokenBudget": 1000})), RunStatus::Failed)
        .await;
    assert_eq!(requests.len(), 2);
    assert!(matches!(requests[1].tool_choice, ToolChoice::None));
    let error = view.error.as_deref().unwrap_or_default();
    assert!(error.contains("token budget reached"), "{error}");
    assert!(error.contains("of its 1000 tokens"), "{error}");
    // The closing call's tokens count too: the provider took them.
    assert_eq!(state.spent, 1_215);
}

#[tokio::test]
async fn the_spent_a_header_carries_is_counted_before_the_first_call() {
    // A round whose session already spent its 1,000-token budget: its first model call is the
    // closing turn, and no tool runs.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_response(answer("closing words", 10, 5));
    let (requests, state, _) = rig
        .round_with(Some(json!({"tokenBudget": 1000})), 1_000, RunStatus::Done)
        .await;
    assert_eq!(requests.len(), 1);
    assert!(matches!(requests[0].tool_choice, ToolChoice::None));
    assert_eq!(state.spent, 1_015, "the carried 1,000 plus this call");
    assert_eq!(state.session.as_ref().expect("a header").spent, 1_015);
}

#[tokio::test]
async fn a_round_with_no_token_budget_spends_and_is_never_spent() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_response(answer("done", 5_000_000, 5_000_000));
    let (requests, state) = rig.round(None).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(state.spent, 10_000_000, "it counts, without a cap");
    assert!(!state.session.as_ref().expect("a header").is_spent());
}
