//! A source may hold tools back: they are not shown to the model, and a call to one is refused when
//! it would run, whoever offers the tool. Scripted `MockModel`, `MemoryStore` and a real `Runtime`
//! with a worker, as in `context_and_sources.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    LlmAgent, LlmAgentBuilder, SourceCtx, Tool, ToolCtx, ToolError, ToolOutput, ToolSource,
    Withheld, user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{Inbound, RunView, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

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

    fn agent(&self) -> LlmAgentBuilder {
        let model: DynModel = self.mock.clone();
        LlmAgent::builder("llm", model, "test-model")
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .worker_id("w")
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .build()
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

async fn wait_for(rt: &Runtime, run: RunId, pred: impl Fn(&RunView) -> bool) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if pred(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

fn message_with(text: &str, context: Value) -> Inbound {
    Inbound::new("message", json!({"text": text, "context": context}))
}

fn names_of(request: &adam_model::ModelRequest) -> Vec<String> {
    request.tools.iter().map(|t| t.name.clone()).collect()
}

/// A tool that counts its calls.
struct Counted(&'static str, Arc<AtomicUsize>);

#[async_trait]
impl Tool for Counted {
    fn spec(&self) -> ToolSpec {
        spec(self.0)
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.1.fetch_add(1, SeqCst);
        Ok(ToolOutput::text(format!("{} ran", self.0)))
    }
}

/// Parks the run on a question, so that a later message can change the context.
struct AskOnce;

#[async_trait]
impl Tool for AskOnce {
    fn spec(&self) -> ToolSpec {
        spec("ask")
    }
    fn asks_user(&self) -> bool {
        true
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input("which one?"))
    }
}

/// Holds back the tools the context names under `held`, with a reason that says so; offers `extra`,
/// a tool of its own that it can hold back too.
struct Holds(Arc<AtomicUsize>);

#[async_trait]
impl ToolSource for Holds {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        vec![spec("extra")]
    }
    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let held: Vec<&str> = ctx
            .context("held")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        // Of the tools asked about, the ones the context names; and one that was not asked about,
        // which the agent must not take for held.
        tools
            .iter()
            .filter(|tool| held.contains(tool))
            .map(|tool| Withheld::new(*tool, format!("{tool} is held back for this message")))
            .chain(std::iter::once(Withheld::new(
                "never-asked-about",
                "not among the tools",
            )))
            .collect()
    }
    async fn call(
        &self,
        _ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        (name == "extra").then(|| {
            self.0.fetch_add(1, SeqCst);
            Ok(ToolOutput::text("extra ran"))
        })
    }
}

#[tokio::test]
async fn a_held_back_tool_is_not_shown_and_a_call_to_it_is_refused_whoever_offers_it() {
    let h = Harness::new();
    let (writes, extras) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let reads = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Counted("write", writes.clone()))
        .tool(Counted("read", reads.clone()))
        .tool_source(Holds(extras.clone()))
        .build();
    // The model calls all three, though it was shown only `read`.
    h.mock
        .push_tool_calls(vec![
            call("c1", "write"),
            call("c2", "extra"),
            call("c3", "read"),
        ])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start(
            "llm",
            message_with("hi", json!({"held": ["write", "extra"]})),
            None,
        )
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    wait_for(&rt, run, |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    handle.await.expect("worker task").expect("worker result");

    let first = &h.mock.requests()[0];
    assert_eq!(names_of(first), ["read"], "what is held back is not shown");
    assert_eq!(
        (writes.load(SeqCst), extras.load(SeqCst), reads.load(SeqCst)),
        (0, 0, 1),
        "the tools that were held back did not run, the other did"
    );
    // The model is told, as the result of each refused call.
    let second = &h.mock.requests()[1];
    let results: Vec<&Message> = second
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .collect();
    assert_eq!(
        results,
        [
            &Message::tool_error("c1", "write is held back for this message"),
            &Message::tool_error("c2", "extra is held back for this message"),
            &Message::tool_result("c3", "read ran"),
        ]
    );
}

#[tokio::test]
async fn a_limit_that_changes_holds_from_the_next_turn_and_the_next_call() {
    let h = Harness::new();
    let writes = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Counted("write", writes.clone()))
        .tool(AskOnce)
        .tool_source(Holds(Arc::new(AtomicUsize::new(0))))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "ask")])
        // After the answer the limit is gone: the call is let through, and the tool is shown again.
        .push_tool_calls(vec![call("c2", "write")])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", message_with("hi", json!({"held": ["write"]})), None)
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    wait_for(&rt, run, |v| v.waiting).await;
    rt.deliver(run, message_with("the answer", json!({"held": null})))
        .await
        .expect("deliver");
    wait_for(&rt, run, |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    handle.await.expect("worker task").expect("worker result");

    let requests = h.mock.requests();
    assert_eq!(names_of(&requests[0]), ["ask", "extra"]);
    assert_eq!(names_of(&requests[1]), ["write", "ask", "extra"]);
    assert_eq!(writes.load(SeqCst), 1);
}

#[tokio::test]
async fn a_source_that_holds_nothing_back_changes_nothing() {
    let h = Harness::new();
    let writes = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool(Counted("write", writes.clone()))
        .tool_source(Holds(Arc::new(AtomicUsize::new(0))))
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "write")])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    wait_for(&rt, run, |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    handle.await.expect("worker task").expect("worker result");
    assert_eq!(names_of(&h.mock.requests()[0]), ["write", "extra"]);
    assert_eq!(writes.load(SeqCst), 1);
}
