//! `doom_loop`: the third call in a row with the same tool, the same arguments and the same result
//! as the two before it is not run, and a model that does it again is ended with the closing turn.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    CLOSING_TURN, Conversation, LlmAgent, LlmAgentBuilder, Tool, ToolCtx, ToolError, ToolOutput,
    user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolChoice, ToolSpec};
use adam_runtime::{CollectingSink, RunView, Runtime};
use async_trait::async_trait;
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

/// Counts its calls and answers with the next entry of `answers` (the last one again when they
/// are used up).
struct Scripted {
    name: &'static str,
    calls: Arc<AtomicUsize>,
    answers: Mutex<Vec<&'static str>>,
}

impl Scripted {
    fn new(name: &'static str, answers: &[&'static str]) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name,
                calls: calls.clone(),
                answers: Mutex::new(answers.iter().rev().copied().collect()),
            },
            calls,
        )
    }
}

#[async_trait]
impl Tool for Scripted {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: format!("the {} tool", self.name),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        let mut answers = self.answers.lock().unwrap();
        let answer = if answers.len() > 1 {
            answers.pop().unwrap()
        } else {
            answers.last().copied().unwrap_or("done")
        };
        Ok(ToolOutput::text(answer))
    }
}

struct Harness {
    mock: Arc<MockModel>,
    rt: Runtime,
}

impl Harness {
    fn new(configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder) -> Self {
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent: LlmAgent = configure(LlmAgent::builder("llm", model, "test-model")).build();
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = Runtime::builder(store)
            .agent(agent)
            .event_sink(CollectingSink::new())
            .worker_id("w")
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .build();
        Self { mock, rt }
    }

    async fn run(&self, text: &str) -> RunView {
        let run = self
            .rt
            .start("llm", user_message(text), None)
            .await
            .expect("start");
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.rt.clone();
        let worker = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        });
        let view = wait_terminal(&self.rt, run).await;
        let _ = stop.send(());
        worker.await.expect("worker task").expect("worker result");
        view
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

fn tool_answers(state: &Conversation) -> Vec<(String, bool)> {
    state
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_third_identical_call_with_the_same_result_is_not_run() {
    let (a, calls) = Scripted::new("a", &["same"]);
    let h = Harness::new(|b| b.tool(a));
    for i in 1..=3 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({"q": 1}))]);
    }
    h.mock.push_text("I will answer with what I know");
    let view = h.run("go").await;

    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(calls.load(SeqCst), 2, "the third call is not run");
    let answers = tool_answers(&conversation(&view));
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0], ("same".to_owned(), false));
    assert_eq!(answers[1], ("same".to_owned(), false));
    assert!(answers[2].1, "the refusal is an error result");
    assert_eq!(
        answers[2].0,
        "Not run: this is the third call in a row of `a` with the same arguments, and the last two \
         gave the same result. Calling it again will not change it. Change your approach: other \
         arguments, another tool, or answer with what you know."
    );
    let output = view.output.expect("output");
    assert_eq!(output["text"], "I will answer with what I know");
    assert!(output.get("limit").is_none(), "one refusal is no limit");
}

#[tokio::test]
async fn a_changing_result_or_a_call_between_is_run() {
    // A poll whose answer changes runs every time.
    let (poll, poll_calls) = Scripted::new("poll", &["1", "2", "3", "4"]);
    let h = Harness::new(|b| b.tool(poll));
    for i in 1..=4 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "poll", json!({}))]);
    }
    h.mock.push_text("done");
    let view = h.run("poll").await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(poll_calls.load(SeqCst), 4);

    // The same call with another call in between runs: a, a, b, a, a.
    let (a, a_calls) = Scripted::new("a", &["same"]);
    let (b_tool, b_calls) = Scripted::new("b", &["other"]);
    let h = Harness::new(|b| b.tool(a).tool(b_tool));
    for (i, name) in ["a", "a", "b", "a", "a"].into_iter().enumerate() {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), name, json!({}))]);
    }
    h.mock.push_text("done");
    let view = h.run("interleave").await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(a_calls.load(SeqCst), 4);
    assert_eq!(b_calls.load(SeqCst), 1);
    assert!(
        tool_answers(&conversation(&view)).iter().all(|(_, e)| !e),
        "no call was refused"
    );

    // The same arguments but another tool, or other arguments of the same tool, are other calls.
    let (a, a_calls) = Scripted::new("a", &["same"]);
    let h = Harness::new(|b| b.tool(a));
    for (i, args) in [json!({"q": 1}), json!({"q": 2}), json!({"q": 1})]
        .into_iter()
        .enumerate()
    {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", args)]);
    }
    h.mock.push_text("done");
    let view = h.run("args").await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(a_calls.load(SeqCst), 3);
}

#[tokio::test]
async fn key_order_does_not_matter() {
    let (a, calls) = Scripted::new("a", &["same"]);
    let h = Harness::new(|b| b.tool(a));
    let texts = [
        r#"{"x":1,"y":{"p":true,"q":[1,{"m":1,"n":2}]}}"#,
        r#"{"y":{"q":[1,{"n":2,"m":1}],"p":true},"x":1}"#,
        r#"{ "x": 1, "y": { "p": true, "q": [1, {"n": 2, "m": 1}] } }"#,
    ];
    for (i, text) in texts.into_iter().enumerate() {
        let args: Value = serde_json::from_str(text).unwrap();
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", args)]);
    }
    h.mock.push_text("done");
    let view = h.run("order").await;
    assert_eq!(calls.load(SeqCst), 2, "the third spelling is the same call");
    assert!(tool_answers(&conversation(&view))[2].1);
}

#[tokio::test]
async fn two_refusals_lead_to_the_closing_turn_with_repeated_call() {
    let (a, calls) = Scripted::new("a", &["same"]);
    let h = Harness::new(|b| b.tool(a));
    for i in 1..=4 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({}))]);
    }
    h.mock.push_text("I am stuck on a");
    let view = h.run("loop").await;

    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(calls.load(SeqCst), 2);
    let output = view.output.clone().expect("output");
    assert_eq!(output["limited"], true);
    assert_eq!(output["limit"], "repeated_call");
    assert_eq!(output["text"], "I am stuck on a");
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 5, "four turns and the closing turn");
    let closing = requests.last().unwrap();
    assert_eq!(closing.tool_choice, ToolChoice::None);
    assert_eq!(
        closing.messages.last(),
        Some(&Message::user_text(CLOSING_TURN))
    );
    let answers = tool_answers(&conversation(&view));
    assert_eq!(answers.len(), 4);
    assert!(answers[2].1 && answers[3].1);
}

#[tokio::test]
async fn the_rest_of_a_turn_is_answered_when_the_run_ends_at_the_limit() {
    let (a, calls) = Scripted::new("a", &["same"]);
    let (b_tool, b_calls) = Scripted::new("b", &["other"]);
    let h = Harness::new(|b| b.tool(a).tool(b_tool));
    for i in 1..=3 {
        h.mock
            .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({}))]);
    }
    // The fourth turn: the refused call and one more behind it.
    h.mock
        .push_tool_calls(vec![call("c4", "a", json!({})), call("c5", "b", json!({}))]);
    h.mock.push_text("stuck");
    let view = h.run("loop").await;

    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(calls.load(SeqCst), 2);
    assert_eq!(
        b_calls.load(SeqCst),
        0,
        "the call behind the refusal is not run"
    );
    assert_eq!(view.output.clone().unwrap()["limit"], "repeated_call");
    let answers = tool_answers(&conversation(&view));
    assert_eq!(answers.len(), 5, "every call has a result");
    assert!(answers[4].0.starts_with("not run"), "{answers:?}");
}

#[tokio::test]
async fn a_refusal_replays_the_same() {
    // Two independent runs of the same script decide the same, to the byte, and keep the same
    // counters: the decision reads the committed state and nothing else.
    let mut states = Vec::new();
    for _ in 0..2 {
        let (a, _) = Scripted::new("a", &["same"]);
        let h = Harness::new(|b| b.tool(a));
        for i in 1..=3 {
            h.mock
                .push_tool_calls(vec![call(&format!("c{i}"), "a", json!({"k": "v"}))]);
        }
        h.mock.push_text("done");
        let view = h.run("replay").await;
        states.push(conversation(&view));
    }
    assert_eq!(states[0].messages, states[1].messages);
    assert_eq!(states[0].repeats, states[1].repeats);
    let repeats = states[0].repeats.clone().expect("the last call is kept");
    assert_eq!((repeats.count, repeats.refused), (2, 1));
    // The hashes are kept in the durable state, so they must not change from one release to the
    // next (FNV-1a over the name and the canonical arguments).
    assert_eq!(repeats.key, "a945aedd85bb19ef");
}
