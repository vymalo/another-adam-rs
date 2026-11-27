//! A round of a session reads its limits from the session header (ADR 0054, slice 1c): each of
//! `max_turns`, `max_tool_calls`, `max_output_tokens` and `max_parallel_tool_calls` narrows the run at
//! the call it governs, never raises what the agent has, and `max_output_tokens` is never `0`.
//! (`context_window` is in `compaction.rs`, `time_limit` and `max_rounds` in `sessions.rs`.)
//!
//! The runs here start a round the way the A2A front does: a first message that carries a header.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, Limits, LlmAgent, Tool, ToolAccess, ToolCtx, ToolError, ToolOutput, user_message,
};
use adam_model::{Message, MockModel, ModelRequest, ToolCall, ToolChoice, ToolSpec};
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

/// `finish { text }` hands `text` over as the answer and ends the turn with it
/// ([`ToolOutput::final_answer`]); `announce { text }` only announces it.
struct Hands {
    ends_turn: bool,
}

#[async_trait]
impl Tool for Hands {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.ends_turn { "finish" } else { "announce" }.into(),
            description: "Hand over the answer.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let text = args["text"].as_str().unwrap_or_default().to_owned();
        Ok(if self.ends_turn {
            ToolOutput::text("Delivered.").final_answer(text)
        } else {
            ToolOutput::text("Announced.").announcing(text)
        })
    }
}

fn hand_over(id: &str, tool: &str, text: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: tool.into(),
        arguments: json!({ "text": text }),
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
            .tool(Hands { ends_turn: true })
            .tool(Hands { ends_turn: false })
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

    /// Run a round of a session whose header holds `config` (`None`: no config) to its end, which
    /// must be `Done`.
    async fn round(&self, config: Option<Value>) -> (Vec<ModelRequest>, Conversation) {
        let (view, requests, state) = self.round_to_its_end(config).await;
        assert_eq!(view.status, RunStatus::Done, "{view:#?}");
        (requests, state)
    }

    /// [`round`](Self::round), whichever way it ends: the run's view, the requests, the state.
    async fn round_to_its_end(
        &self,
        config: Option<Value>,
    ) -> (RunView, Vec<ModelRequest>, Conversation) {
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
        let state = self.rt.state(run).await.unwrap().unwrap();
        (
            view,
            self.mock.requests(),
            serde_json::from_value(state).expect("a Conversation"),
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
// report_schema: the final answer (ADR 0054, slice 9)
// ---------------------------------------------------------------------------

fn verdict_schema() -> Value {
    json!({
        "type": "object",
        "required": ["verdict"],
        "properties": {"verdict": {"type": "string", "enum": ["ok", "bad"]}},
        "additionalProperties": false,
    })
}

fn report_config(extra: Value) -> Value {
    let mut config = json!({"reportSchema": verdict_schema()});
    if let (Some(config), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
        config.extend(extra.clone());
    }
    config
}

fn last_user_text(request: &ModelRequest) -> String {
    match request.messages.last() {
        Some(message @ Message::User { .. }) => message.text(),
        other => panic!("the request ends with a user turn, not {other:?}"),
    }
}

#[tokio::test]
async fn a_matching_answer_is_the_report_and_costs_no_extra_turn() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text(r#"{"verdict": "ok"}"#);
    let (view, requests, state) = rig.round_to_its_end(Some(report_config(json!({})))).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    assert_eq!(requests.len(), 1, "no corrective turn");
    assert_eq!(state.turns, 1);
    assert_eq!(state.report_corrections, 0);
    assert_eq!(
        view.output.as_ref().and_then(|o| o["text"].as_str()),
        Some(r#"{"verdict": "ok"}"#),
        "the report is the answer as the model wrote it"
    );
}

#[tokio::test]
async fn a_mismatch_gets_two_corrective_turns_and_then_fails_the_round_with_the_reason() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("I am done, all good.");
    rig.mock.push_text(r#"{"verdict": "maybe"}"#);
    rig.mock.push_text(r#"{"verdict": 5}"#);
    // Never reached: a third correction would be a fourth call.
    rig.mock.push_text(r#"{"verdict": "ok"}"#);
    let (view, requests, state) = rig.round_to_its_end(Some(report_config(json!({})))).await;

    assert_eq!(view.status, RunStatus::Failed, "{view:#?}");
    let error = view.error.expect("a reason");
    assert!(
        error.starts_with("report does not match report_schema"),
        "{error}"
    );
    assert!(
        error.contains("is not of type"),
        "the last problems: {error}"
    );
    assert_eq!(requests.len(), 3, "the answer and exactly two corrections");
    assert_eq!(state.turns, 3, "each correction is a model call");
    assert_eq!(state.report_corrections, 2);

    let first = last_user_text(&requests[1]);
    assert!(
        first.contains("does not match the report schema"),
        "{first}"
    );
    assert!(first.contains("not one JSON value"), "{first}");
    assert!(
        first.contains("The schema:") && first.contains("additionalProperties"),
        "the first correction tells the schema: {first}"
    );
    let second = last_user_text(&requests[2]);
    assert!(second.contains("maybe"), "its problems: {second}");
    assert!(!second.contains("The schema:"), "told once: {second}");
}

#[tokio::test]
async fn a_corrected_answer_ends_the_round_as_the_report() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text(r#"{"verdict": "maybe"}"#);
    rig.mock.push_text(r#"{"verdict": "bad"}"#);
    let (view, requests, state) = rig.round_to_its_end(Some(report_config(json!({})))).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    assert_eq!(requests.len(), 2);
    assert_eq!(state.report_corrections, 1);
    assert_eq!(
        view.output.as_ref().and_then(|o| o["text"].as_str()),
        Some(r#"{"verdict": "bad"}"#)
    );
}

#[tokio::test]
async fn the_problems_a_correction_tells_are_the_first_three() {
    let rig = Rig::new(Limits::default(), 10);
    let schema = json!({
        "type": "object",
        "properties": {
            "a": {"type": "integer"}, "b": {"type": "integer"},
            "c": {"type": "integer"}, "d": {"type": "integer"},
        },
    });
    rig.mock
        .push_text(r#"{"a": "x", "b": "x", "c": "x", "d": "x"}"#);
    rig.mock.push_text(r#"{"a": 1}"#);
    let (_, requests, _) = rig
        .round_to_its_end(Some(json!({"reportSchema": schema})))
        .await;
    let told = last_user_text(&requests[1]);
    assert!(
        told.contains("1. ") && told.contains("2. ") && told.contains("3. "),
        "{told}"
    );
    assert!(!told.contains("4. "), "at most three: {told}");
}

#[tokio::test]
async fn a_correction_is_a_turn_of_max_turns_and_an_answer_at_the_closing_turn_must_still_match() {
    // One turn allowed: the correction is the closing turn, which calls nothing.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("not json");
    rig.mock.push_text(r#"{"verdict": "ok"}"#);
    let (view, requests, _) = rig
        .round_to_its_end(Some(report_config(json!({"maxTurns": 1}))))
        .await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    assert_eq!(requests.len(), 2);
    assert!(
        matches!(requests[1].tool_choice, ToolChoice::None),
        "the correction used the turn the limit counts, so the second call is the closing turn"
    );

    // A closing answer that does not match is no report: the round fails, with no turn to correct.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("not json");
    rig.mock.push_text(r#"{"verdict": "maybe"}"#);
    let (view, requests, state) = rig
        .round_to_its_end(Some(report_config(json!({"maxTurns": 1}))))
        .await;
    assert_eq!(view.status, RunStatus::Failed, "{view:#?}");
    assert!(
        view.error
            .as_deref()
            .is_some_and(|e| e.starts_with("report does not match report_schema")),
        "{view:#?}"
    );
    assert_eq!(requests.len(), 2, "no further turn");
    assert_eq!(state.turns, 2);
}

#[tokio::test]
async fn a_round_without_a_report_schema_ends_with_whatever_it_answers() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock.push_text("not json at all");
    let (requests, state) = rig.round(Some(json!({"maxTurns": 5}))).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(state.report_corrections, 0);
}

#[tokio::test]
async fn a_tools_final_answer_is_held_to_the_report_schema_and_never_ends_the_round_unchecked() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock
        .push_tool_calls(vec![hand_over("f1", "finish", "not json")]);
    rig.mock
        .push_tool_calls(vec![hand_over("f2", "finish", r#"{"verdict": "maybe"}"#)]);
    rig.mock
        .push_tool_calls(vec![hand_over("f3", "finish", "still not json")]);
    // Never reached: two corrections, then the round fails.
    rig.mock.push_text(r#"{"verdict": "ok"}"#);
    let (view, requests, state) = rig.round_to_its_end(Some(report_config(json!({})))).await;
    assert_eq!(view.status, RunStatus::Failed, "{view:#?}");
    assert!(
        view.error
            .as_deref()
            .is_some_and(|e| e.starts_with("report does not match report_schema")),
        "{view:#?}"
    );
    assert_eq!(
        requests.len(),
        3,
        "the hand-over and exactly two corrections"
    );
    assert_eq!(state.report_corrections, 2);
    assert!(
        last_user_text(&requests[1]).contains("does not match the report schema"),
        "the first hand-over was corrected, not accepted"
    );

    // A hand-over that matches ends the round with no model call after it.
    let rig = Rig::new(Limits::default(), 10);
    rig.mock
        .push_tool_calls(vec![hand_over("f1", "finish", "not json")]);
    rig.mock
        .push_tool_calls(vec![hand_over("f2", "finish", r#"{"verdict": "ok"}"#)]);
    let (view, requests, state) = rig.round_to_its_end(Some(report_config(json!({})))).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    assert_eq!(requests.len(), 2);
    assert_eq!(state.report_corrections, 1);
    assert_eq!(
        view.output.as_ref().and_then(|o| o["text"].as_str()),
        Some(r#"{"verdict": "ok"}"#)
    );
}

#[tokio::test]
async fn a_corrected_answer_replaces_the_announced_one_that_did_not_match() {
    let rig = Rig::new(Limits::default(), 10);
    rig.mock
        .push_tool_calls(vec![hand_over("a1", "announce", "not json")]);
    rig.mock.push_text("That is my answer.");
    rig.mock.push_text(r#"{"verdict": "bad"}"#);
    let (view, requests, state) = rig.round_to_its_end(Some(report_config(json!({})))).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    assert_eq!(requests.len(), 3);
    assert_eq!(state.report_corrections, 1);
    assert_eq!(state.announced, None, "the old announcement is gone");
    assert_eq!(
        view.output.as_ref().and_then(|o| o["text"].as_str()),
        Some(r#"{"verdict": "bad"}"#),
        "the corrected words are the report, not the announcement checked again"
    );
}
