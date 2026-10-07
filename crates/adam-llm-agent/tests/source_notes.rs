//! What a tool source says about the tools it lists (`ToolNote`): a call of a tool whose system
//! reports its own steps gets no step of the agent's, the note reaches the call (and a retry of the
//! journaled step sees the same call id), and a source can add words to the instructions of the turn.
//! A real `Runtime` with a worker, a scripted `MockModel` and `MemoryStore`, as in
//! `context_and_sources.rs`.
//!
//! What these assert is recorded: the run's events, state and output, and what the model was sent.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, Listing, LlmAgent, LlmAgentBuilder, SourceCtx, ToolCtx, ToolError, ToolNote,
    ToolOutput, ToolSource, user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{
    CollectingSink, ManualClock, RetryPolicy, RunEvent, RunView, Runtime, StepState,
};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
    }
}

struct Rig {
    store: DynStore,
    sink: CollectingSink,
    mock: Arc<MockModel>,
}

impl Rig {
    fn new() -> Self {
        Self::with_store(Arc::new(MemoryStore::new()))
    }

    fn with_store(store: DynStore) -> Self {
        Self {
            store,
            sink: CollectingSink::new(),
            mock: Arc::new(MockModel::new()),
        }
    }

    fn agent(&self) -> LlmAgentBuilder {
        let model: DynModel = self.mock.clone();
        LlmAgent::builder("llm", model, "test-model").instructions("You are a test agent.")
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .event_sink(self.sink.clone())
            .worker_id("w")
            .clock(ManualClock::new())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(1))
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
                multiplier: 1.0,
            })
            .build()
    }

    /// Run one message to its end.
    async fn run(&self, agent: &LlmAgent, text: &str) -> (RunId, RunView) {
        let rt = self.runtime(agent);
        let run = rt
            .start("llm", user_message(text), None)
            .await
            .expect("start");
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
        let deadline = Instant::now() + Duration::from_secs(20);
        let view = loop {
            let view = rt.view(run).await.expect("view").expect("run exists");
            if view.status == RunStatus::Done {
                break view;
            }
            assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = stop.send(());
        worker.await.expect("worker task").expect("worker result");
        (run, view)
    }

    /// The step reports of the run: `(id, state)`.
    fn steps(&self, run: RunId) -> Vec<(String, StepState)> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter_map(|e| match e {
                RunEvent::Step(step) => Some((step.id, step.state)),
                _ => None,
            })
            .collect()
    }
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

/// A source with two tools: `relay__search`, whose system reports its own steps and may take 125 s,
/// and `plain`, which says nothing. It records the notes and the call ids its calls were given.
struct Relay {
    calls: Arc<Mutex<Vec<Seen>>>,
    instructions: Option<String>,
    /// When set, the first call of `relay__search` makes the next journal write fail before it
    /// reaches the store: the step that records the call is lost, so it runs again.
    arm: Mutex<Option<Arc<FaultyStore>>>,
}

#[derive(Debug, Clone, PartialEq)]
struct Seen {
    tool: String,
    call_id: String,
    run: RunId,
    note: Option<ToolNote>,
}

impl Relay {
    fn new() -> (Self, Arc<Mutex<Vec<Seen>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                instructions: None,
                arm: Mutex::new(None),
            },
            calls,
        )
    }
}

#[async_trait]
impl ToolSource for Relay {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        vec![spec("relay__search"), spec("plain")]
    }

    async fn listing(&self, ctx: &SourceCtx) -> Listing {
        Listing::new(self.specs(ctx).await)
            .with_note(
                ToolNote::new("relay__search")
                    .reporting_steps()
                    .with_timeout(Duration::from_secs(125)),
            )
            // A note that says nothing is not kept; one for a tool that is not listed is dropped.
            .with_note(ToolNote::new("plain"))
            .with_note(ToolNote::new("never-listed").reporting_steps())
    }

    async fn instructions(&self, _ctx: &SourceCtx) -> Option<String> {
        self.instructions.clone()
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        if name != "relay__search" && name != "plain" {
            return None;
        }
        self.calls.lock().unwrap().push(Seen {
            tool: name.to_owned(),
            call_id: ctx.call_id().to_owned(),
            run: ctx.run_id(),
            note: ctx.note().cloned(),
        });
        if name == "relay__search"
            && let Some(faulty) = self.arm.lock().unwrap().take()
        {
            faulty.fail(Method::JournalPut, 1);
        }
        Some(Ok(ToolOutput::text(format!("{name}-out"))))
    }
}

#[tokio::test]
async fn a_tool_whose_system_reports_its_steps_gets_none_of_the_agents_and_the_others_do() {
    let rig = Rig::new();
    let (source, seen) = Relay::new();
    let agent = rig.agent().tool_source(source).build();
    rig.mock
        .push_tool_calls(vec![
            call("c1", "relay__search", json!({"x": "rust"})),
            call("c2", "plain", json!({})),
        ])
        .push_text("done");
    let (run, view) = rig.run(&agent, "go").await;

    // The agent reported the step of `plain` (running, then completed) and nothing for the relayed
    // call: the orchestration layer reports that one.
    assert_eq!(
        rig.steps(run),
        [
            ("tool:c2".to_owned(), StepState::Running),
            ("tool:c2".to_owned(), StepState::Completed),
        ]
    );
    // The call still ran, and its result is the model's.
    let messages = conversation(&view).messages;
    assert!(messages.contains(&Message::tool_result("c1", "relay__search-out")));
    assert!(messages.contains(&Message::tool_result("c2", "plain-out")));
    assert_eq!(view.output.expect("output")["text"], "done");

    // The call of the relayed tool was given what its listing said; the other, nothing.
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen[0].note,
        Some(
            ToolNote::new("relay__search")
                .reporting_steps()
                .with_timeout(Duration::from_secs(125))
        )
    );
    assert_eq!(seen[0].note.as_ref().unwrap().timeout_ms, Some(125_000));
    assert_eq!(seen[1].note, None, "a note that says nothing is not kept");
}

#[tokio::test]
async fn the_notes_are_the_latest_listings_and_are_in_the_state_for_a_later_transition() {
    let rig = Rig::new();
    let (source, _) = Relay::new();
    let agent = rig.agent().tool_source(source).build();
    rig.mock
        .push_tool_calls(vec![call("c1", "relay__search", json!({}))])
        .push_text("done");
    let (_, view) = rig.run(&agent, "go").await;
    // Only the one meaningful note of a listed tool is kept, as written to the durable state.
    assert_eq!(
        view.state["source_notes"],
        json!([{"tool": "relay__search", "reports_step": true, "timeout_ms": 125_000}])
    );
}

#[tokio::test]
async fn an_agents_own_tool_wins_the_name_and_keeps_its_step() {
    struct Own;
    #[async_trait]
    impl adam_llm_agent::Tool for Own {
        fn spec(&self) -> ToolSpec {
            spec("relay__search")
        }
        async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            assert!(ctx.note().is_none(), "an own tool has no source's note");
            Ok(ToolOutput::text("own-out"))
        }
    }
    let rig = Rig::new();
    let (source, seen) = Relay::new();
    let agent = rig.agent().tool(Own).tool_source(source).build();
    rig.mock
        .push_tool_calls(vec![call("c1", "relay__search", json!({}))])
        .push_text("done");
    let (run, _) = rig.run(&agent, "go").await;
    assert_eq!(
        rig.steps(run),
        [
            ("tool:c1".to_owned(), StepState::Running),
            ("tool:c1".to_owned(), StepState::Completed),
        ],
        "the call is the agent's own tool's: it reports its step"
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// A source's tool with a title in its note is drawn under that title, from the first report to the
/// last; the model still calls it by its name.
#[tokio::test]
async fn a_source_tool_with_a_titled_note_is_drawn_under_its_title() {
    struct Titled;
    #[async_trait]
    impl ToolSource for Titled {
        async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
            vec![spec("turn_output")]
        }
        async fn listing(&self, ctx: &SourceCtx) -> Listing {
            Listing::new(self.specs(ctx).await)
                .with_note(ToolNote::new("turn_output").with_label("Send the answer"))
        }
        async fn call(
            &self,
            _ctx: &ToolCtx,
            name: &str,
            _args: Value,
        ) -> Option<Result<ToolOutput, ToolError>> {
            (name == "turn_output").then(|| Ok(ToolOutput::text("out")))
        }
    }
    let rig = Rig::new();
    let agent = rig.agent().tool_source(Titled).build();
    rig.mock
        .push_tool_calls(vec![call("c1", "turn_output", json!({}))])
        .push_text("done");
    let (run, view) = rig.run(&agent, "go").await;
    let labels: Vec<String> = rig
        .sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) => Some(step.label),
            _ => None,
        })
        .collect();
    assert_eq!(labels, ["Send the answer", "Send the answer"]);
    assert_eq!(
        view.state["source_notes"],
        json!([{"tool": "turn_output", "label": "Send the answer"}]),
        "a title alone is a note worth keeping"
    );
}

#[tokio::test]
async fn a_source_without_notes_leaves_every_call_a_step_as_before() {
    struct Plain;
    #[async_trait]
    impl ToolSource for Plain {
        async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
            vec![spec("relay__search")]
        }
        async fn call(
            &self,
            _ctx: &ToolCtx,
            name: &str,
            _args: Value,
        ) -> Option<Result<ToolOutput, ToolError>> {
            (name == "relay__search").then(|| Ok(ToolOutput::text("out")))
        }
    }
    let rig = Rig::new();
    let agent = rig.agent().tool_source(Plain).build();
    rig.mock
        .push_tool_calls(vec![call("c1", "relay__search", json!({}))])
        .push_text("done");
    let (run, view) = rig.run(&agent, "go").await;
    assert_eq!(
        rig.steps(run),
        [
            ("tool:c1".to_owned(), StepState::Running),
            ("tool:c1".to_owned(), StepState::Completed),
        ]
    );
    assert!(
        view.state.get("source_notes").is_none(),
        "nothing is written for no notes"
    );
}

/// A step whose write is lost runs the tool again with the same run and call id (the model's answer,
/// recorded, is replayed): what a source derives a call's identity from does not change between the
/// tries. The relayed call is reported by no step of the agent's, on either try.
#[tokio::test]
async fn a_retried_step_sees_the_same_run_and_call_id_and_the_same_note() {
    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let rig = Rig::with_store(faulty.clone());
    let (source, seen) = Relay::new();
    *source.arm.lock().unwrap() = Some(faulty.clone());
    let agent = rig.agent().tool_source(source).build();
    rig.mock
        .push_tool_calls(vec![call("c1", "relay__search", json!({}))])
        .push_text("done");
    let (run, view) = rig.run(&agent, "go").await;

    assert_eq!(
        faulty.injected(Method::JournalPut),
        1,
        "the write was lost once"
    );
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        2,
        "the step ran again after the lost write: {seen:#?}"
    );
    assert_eq!(
        seen[0], seen[1],
        "both tries saw the same run, call id and note"
    );
    assert_eq!(seen[0].call_id, "c1");
    assert_eq!(seen[0].run, run);
    assert!(seen[0].note.as_ref().is_some_and(|n| n.reports_step));
    assert_eq!(
        rig.mock.requests().len(),
        2,
        "the recorded model answer was replayed"
    );
    assert_eq!(view.output.expect("output")["text"], "done");
    assert!(rig.steps(run).is_empty(), "{:?}", rig.steps(run));
}

#[tokio::test]
async fn a_source_adds_words_to_the_instructions_of_the_turn_and_only_when_it_has_some() {
    let rig = Rig::new();
    let (mut with, _) = Relay::new();
    with.instructions = Some("  ## Mentioned agents\n\n- @coder  ".into());
    let agent = rig.agent().tool_source(with).build();
    rig.mock.push_text("done");
    rig.run(&agent, "go").await;
    assert_eq!(
        rig.mock.requests()[0].system.as_deref(),
        Some("You are a test agent.\n\n## Mentioned agents\n\n- @coder"),
        "after the agent's own, set apart by a blank line, trimmed"
    );

    let rig = Rig::new();
    let (without, _) = Relay::new();
    let agent = rig.agent().tool_source(without).build();
    rig.mock.push_text("done");
    rig.run(&agent, "go").await;
    assert_eq!(
        rig.mock.requests()[0].system.as_deref(),
        Some("You are a test agent."),
        "a source with nothing to say changes nothing"
    );

    // An agent with no instructions of its own gets just the source's.
    let rig = Rig::new();
    let (mut only, _) = Relay::new();
    only.instructions = Some("from the source".into());
    let model: DynModel = rig.mock.clone();
    let agent = LlmAgent::builder("llm", model, "test-model")
        .tool_source(only)
        .build();
    rig.mock.push_text("done");
    rig.run(&agent, "go").await;
    assert_eq!(
        rig.mock.requests()[0].system.as_deref(),
        Some("from the source")
    );
}

#[test]
fn notes_written_by_state_before_they_existed_load_and_none_is_written_for_no_notes() {
    let old: Conversation = serde_json::from_value(json!({"messages": [], "turns": 1})).unwrap();
    assert!(old.source_notes.is_empty());
    assert!(
        serde_json::to_value(&old)
            .unwrap()
            .get("source_notes")
            .is_none()
    );
    let note: ToolNote = serde_json::from_value(json!({"tool": "t"})).unwrap();
    assert_eq!(note, ToolNote::new("t"));
    assert_eq!(serde_json::to_value(&note).unwrap(), json!({"tool": "t"}));
}

#[test]
fn a_tool_context_knows_its_note_and_the_step_it_runs_under() {
    let ctx = ToolCtx::detached("t", "c1", Arc::new(adam_runtime::NoopSink));
    assert_eq!(ctx.note(), None);
    assert_eq!(
        ctx.parent_step_id(),
        None,
        "a call of the model's own is at the top"
    );
    let ctx = ctx
        .with_note(ToolNote::new("t").reporting_steps())
        .under_step("tool:outer");
    assert!(ctx.note().is_some_and(|n| n.reports_step));
    assert_eq!(ctx.parent_step_id(), Some("tool:outer"));
    assert_eq!(ToolNote::new("t").timeout(), None);
    assert_eq!(
        ToolNote::new("t")
            .with_timeout(Duration::from_millis(1500))
            .timeout(),
        Some(Duration::from_millis(1500))
    );
}
