//! A tool can announce the run's answer (`ToolOutput::announcing`): when the run finishes, the output's
//! `text` is the last announcement and not the model's closing words. A real `Runtime` with a worker, a
//! scripted `MockModel` and `MemoryStore`, as in `llm_agent.rs`.
//!
//! What these assert is recorded: the run's output and state, the tool results the model was sent and the
//! events of the run (never the progress lines).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Conversation, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{
    CollectingSink, RunEvent, RunView, Runtime, RuntimeBuilder, StepOutput, StepState,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

const TOLD: &str = "Delivered to the person as your answer. Finish now with one short line.";

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

/// `announce { text }`: succeeds and announces `text` as the answer, like `turn_output` of
/// `adam-ui`; with `"refuse": true` it fails (an error result) and announces nothing, though it
/// set the words.
struct Announce {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Announce {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "announce".into(),
            description: "Announce the answer.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        let text = args["text"].as_str().unwrap_or_default().to_owned();
        if args["refuse"] == json!(true) {
            let mut refused = ToolOutput::error("this turn is over");
            refused.answer = Some(text);
            return Ok(refused);
        }
        Ok(ToolOutput::text(TOLD).announcing(text))
    }
}

/// `slow`: hangs the first time it is called, so that a worker can be killed inside it.
struct Hangs {
    first: Arc<AtomicBool>,
    reached: Arc<Notify>,
}

#[async_trait]
impl Tool for Hangs {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "slow".into(),
            description: "Slow.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.first.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("slow-out"))
    }
}

/// `ask`: asks the person a question, which parks the run.
struct Asks;

#[async_trait]
impl Tool for Asks {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask".into(),
            description: "Ask.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input("which one?"))
    }
}

struct Rig {
    store: DynStore,
    sink: CollectingSink,
    mock: Arc<MockModel>,
    announced: Arc<AtomicUsize>,
}

impl Rig {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            sink: CollectingSink::new(),
            mock: Arc::new(MockModel::new()),
            announced: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn agent(&self) -> adam_llm_agent::LlmAgentBuilder {
        let model: DynModel = self.mock.clone();
        LlmAgent::builder("llm", model, "test-model").tool(Announce {
            calls: self.announced.clone(),
        })
    }

    fn runtime(&self, agent: &LlmAgent, worker: &str, short_lease: bool) -> Runtime {
        let mut builder: RuntimeBuilder = Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .event_sink(self.sink.clone())
            .worker_id(worker)
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10));
        if short_lease {
            builder = builder.lease_ttl(Duration::from_millis(300));
        }
        builder.build()
    }
}

fn spawn_worker(
    rt: &Runtime,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
) {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
    });
    (stop, handle)
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
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn finish(rig: &Rig, first: &str) -> RunView {
    let agent = rig.agent().build();
    let rt = rig.runtime(&agent, "w", false);
    let run = rt
        .start("llm", user_message(first), None)
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    let view = wait_for(&rt, run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    handle.await.unwrap().unwrap();
    view
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

fn text_of(view: &RunView) -> &str {
    view.output.as_ref().unwrap()["text"].as_str().unwrap()
}

#[tokio::test]
async fn an_announced_answer_is_the_runs_answer_and_the_closing_line_is_not() {
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call(
            "c1",
            "announce",
            json!({"text": "## The answer\n\nIt is **42**."}),
        )])
        .push_text("There it is.");
    let view = finish(&rig, "what is it?").await;

    let output = view.output.clone().unwrap();
    assert_eq!(output["text"], "## The answer\n\nIt is **42**.");
    assert!(
        output.get("stream").is_none(),
        "the output's text is not the streamed words, so it names no stream: {output}"
    );
    // The model was told what the tool says, and nothing of the endpoint's.
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages.last(),
        Some(&Message::tool_result("c1", TOLD))
    );
    // The state keeps it, and the history keeps the closing line (it was said).
    let state = conversation(&view);
    assert_eq!(
        state.announced.as_deref(),
        Some("## The answer\n\nIt is **42**.")
    );
    assert_eq!(
        state.messages.last(),
        Some(&Message::assistant_text("There it is."))
    );
    // The closing line is said as words of the turn (with the stream it was sent as), once, and the
    // call's step ends `completed` with what the model was told.
    let events = rig.sink.events_for(view.id);
    let said: Vec<&Value> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Custom { kind, payload } if kind == "agent_text" => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(said.len(), 1);
    assert_eq!(said[0]["text"], "There it is.");
    assert!(said[0]["stream"].is_string(), "{:?}", said[0]);
    let ends: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Step(s) if s.state == StepState::Completed => s.output.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(
        ends[0],
        StepOutput::new(TOLD, false),
        "the step carries what the model was told"
    );
}

#[tokio::test]
async fn the_last_announcement_wins() {
    let rig = Rig::new();
    // Two calls in one model message, then a third in the next turn.
    rig.mock
        .push_tool_calls(vec![
            call("c1", "announce", json!({"text": "first"})),
            call("c2", "announce", json!({"text": "second"})),
        ])
        .push_tool_calls(vec![call("c3", "announce", json!({"text": "third"}))])
        .push_text("Done.");
    let view = finish(&rig, "go").await;
    assert_eq!(text_of(&view), "third");
    assert_eq!(rig.announced.load(SeqCst), 3);
    assert_eq!(conversation(&view).announced.as_deref(), Some("third"));
}

#[tokio::test]
async fn a_refused_announcement_changes_nothing_and_the_closing_words_are_the_answer() {
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call(
            "c1",
            "announce",
            json!({"text": "too late", "refuse": true}),
        )])
        .push_text("The turn was over, so here it is: 42.");
    let view = finish(&rig, "go").await;
    assert_eq!(text_of(&view), "The turn was over, so here it is: 42.");
    assert_eq!(conversation(&view).announced, None);
    // The model saw the error.
    assert_eq!(
        rig.mock.requests()[1].messages.last(),
        Some(&Message::tool_error("c1", "this turn is over"))
    );
}

#[tokio::test]
async fn a_refusal_after_an_announcement_keeps_the_announcement() {
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call("c1", "announce", json!({"text": "kept"}))])
        .push_tool_calls(vec![call(
            "c2",
            "announce",
            json!({"text": "refused", "refuse": true}),
        )])
        .push_text("Done.");
    let view = finish(&rig, "go").await;
    assert_eq!(text_of(&view), "kept");
}

#[tokio::test]
async fn with_no_announcement_the_closing_words_are_the_answer_as_before() {
    let rig = Rig::new();
    rig.mock.push_text("Just the words.");
    let view = finish(&rig, "go").await;
    assert_eq!(text_of(&view), "Just the words.");
    let output = view.output.clone().unwrap();
    assert!(
        output["stream"].is_string(),
        "an unannounced answer still names the stream it was sent as: {output}"
    );
    assert_eq!(conversation(&view).announced, None);
    // The state of a run that announced nothing does not carry the member at all.
    assert!(view.state.get("announced").is_none());
}

#[tokio::test]
async fn an_announced_answer_survives_a_replay_from_the_journal() {
    let rig = Rig::new();
    let first = Arc::new(AtomicBool::new(true));
    let reached = Arc::new(Notify::new());
    let agent = rig
        .agent()
        .tool(Hangs {
            first: first.clone(),
            reached: reached.clone(),
        })
        .build();
    // The announcement is recorded, then the worker dies inside the second tool of the message.
    rig.mock
        .push_tool_calls(vec![
            call("c1", "announce", json!({"text": "the announced answer"})),
            call("c2", "slow", json!({})),
        ])
        .push_text("Closing line.");
    let rt_a = rig.runtime(&agent, "crash-a", true);
    let rt_b = rig.runtime(&agent, "crash-b", true);
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let (_stop, doomed) = spawn_worker(&rt_a);
    tokio::time::timeout(Duration::from_secs(20), reached.notified())
        .await
        .expect("the slow tool started");
    doomed.abort();
    assert!(doomed.await.expect_err("aborted").is_cancelled());
    assert_eq!(rig.announced.load(SeqCst), 1);

    let (stop, survivor) = spawn_worker(&rt_b);
    let view = wait_for(&rt_b, run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    survivor.await.unwrap().unwrap();

    assert_eq!(
        rig.announced.load(SeqCst),
        1,
        "the recorded result, announcement included, is replayed and the tool does not run again"
    );
    assert_eq!(text_of(&view), "the announced answer");
    assert_eq!(rig.mock.requests().len(), 2);
}

#[tokio::test]
async fn a_message_that_reaches_the_run_ends_the_turn_and_the_announcement_with_it() {
    let rig = Rig::new();
    let agent = rig.agent().tool(Asks).build();
    rig.mock
        .push_tool_calls(vec![call("c1", "announce", json!({"text": "old answer"}))])
        .push_tool_calls(vec![call("c2", "ask", json!({}))])
        .push_text("The new closing words.");
    let rt = rig.runtime(&agent, "w", false);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    let parked = wait_for(&rt, run, "parked on the question", |v| v.waiting).await;
    assert_eq!(
        conversation(&parked).announced.as_deref(),
        Some("old answer"),
        "an announcement stands while the turn goes on"
    );
    rt.deliver(run, user_message("this one"))
        .await
        .expect("deliver");
    let view = wait_for(&rt, run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    handle.await.unwrap().unwrap();
    assert_eq!(text_of(&view), "The new closing words.");
    assert_eq!(conversation(&view).announced, None);
}

#[test]
fn an_older_journal_and_state_read_without_the_new_members() {
    let output: ToolOutput = serde_json::from_value(json!({"content": "x"})).unwrap();
    assert_eq!(output.answer, None);
    assert!(
        serde_json::to_value(&output)
            .unwrap()
            .get("answer")
            .is_none(),
        "not written while there is none"
    );
    let announcing = ToolOutput::text("x").announcing("a");
    let back: ToolOutput =
        serde_json::from_value(serde_json::to_value(&announcing).unwrap()).unwrap();
    assert_eq!(back.answer.as_deref(), Some("a"));
    let state: Conversation = serde_json::from_value(json!({"messages": []})).unwrap();
    assert_eq!(state.announced, None);
}
