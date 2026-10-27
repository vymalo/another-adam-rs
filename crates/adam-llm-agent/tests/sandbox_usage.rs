//! `Conversation::sandbox_usage`: the time a call's commands ran in a sandbox, reported through the tool
//! context, journaled with the call's result and kept in the run's state; a child's added to its
//! parent's when it answers (ADR 0043).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Conversation, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{DynModel, MockModel, ModelResponse, ToolCall, ToolSpec};
use adam_runtime::{RetryPolicy, RunView, Runtime, SandboxEnvironment, SandboxUsage};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const MIB: u64 = 1024 * 1024;

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

/// Runs a command in a sandbox of the shape its argument says, for `ms` milliseconds, and reports it.
struct Command;

#[async_trait]
impl Tool for Command {
    fn spec(&self) -> ToolSpec {
        spec("command")
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let cpu = args["cpu"].as_u64();
        let memory = args["memory"].as_u64();
        let ms = args["ms"].as_u64().unwrap_or(0);
        ctx.report_sandbox(&SandboxUsage::ran_command(cpu, memory, ms));
        Ok(ToolOutput::text("ran"))
    }
}

/// Reports a command and then fails permanently, as a command stopped by a cancel does.
struct Stopped;

#[async_trait]
impl Tool for Stopped {
    fn spec(&self) -> ToolSpec {
        spec("stopped")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        ctx.report_sandbox(&SandboxUsage::ran_command(Some(250), None, 700));
        Err(ToolError::Permanent("the command was stopped".into()))
    }
}

/// Reports a command, fails transiently the first time and then reports another.
struct Flaky {
    tries: AtomicUsize,
}

#[async_trait]
impl Tool for Flaky {
    fn spec(&self) -> ToolSpec {
        spec("flaky")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.tries.fetch_add(1, SeqCst) == 0 {
            ctx.report_sandbox(&SandboxUsage::ran_command(Some(250), None, 9_000));
            return Err(ToolError::Transient("blip".into()));
        }
        ctx.report_sandbox(&SandboxUsage::ran_command(Some(250), None, 100));
        Ok(ToolOutput::text("fine"))
    }
}

/// Starts the agent `target` as a child run and waits for it, as `SubagentTool` does.
struct Spawn {
    target: String,
}

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> ToolSpec {
        spec("spawn")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx.start_child(&self.target, "go").await?;
        Err(ToolError::AwaitRun { run })
    }
}

fn command(id: &str, cpu: u64, memory: u64, ms: u64) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "command".into(),
        arguments: json!({"cpu": cpu, "memory": memory, "ms": ms}),
    }
}

async fn wait_finished(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).unwrap()
}

async fn run_to_end(rt: &Runtime, run: RunId) -> RunView {
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
    let view = wait_finished(rt, run).await;
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    view
}

fn runtime(store: DynStore, agents: Vec<LlmAgent>) -> Runtime {
    let mut builder = Runtime::builder(store)
        .poll_interval(Duration::from_millis(20))
        .retry(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            multiplier: 1.0,
        });
    for agent in agents {
        builder = builder.agent(agent);
    }
    builder.build()
}

fn agent(name: &str, model: &Arc<MockModel>) -> adam_llm_agent::LlmAgentBuilder {
    let model: DynModel = model.clone();
    LlmAgent::builder(name, model, "m")
}

/// Two commands of one shape are one entry, a command of another shape is another; the environment
/// is the one the commands ran in.
#[tokio::test]
async fn the_time_of_commands_is_counted_per_shape_in_the_runs_state() {
    let mock = Arc::new(MockModel::new());
    mock.push_response(ModelResponse::tool_calls(vec![
        command("c1", 250, 512 * MIB, 1_000),
        command("c2", 250, 512 * MIB, 2_500),
    ]))
    .push_response(ModelResponse::tool_calls(vec![command(
        "c3",
        1000,
        2048 * MIB,
        40,
    )]))
    .push_text("done");
    let rt = runtime(
        Arc::new(MemoryStore::new()),
        vec![agent("llm", &mock).tool(Command).build()],
    );
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");

    let usage = conversation(&view).sandbox_usage;
    assert_eq!(usage.environment(), Some(SandboxEnvironment::Kubernetes));
    let [small, big] = usage.entries() else {
        panic!("two shapes: {usage:?}");
    };
    assert_eq!(
        (small.cpu_millis, small.memory_bytes),
        (Some(250), Some(512 * MIB))
    );
    assert_eq!((small.commands, small.milliseconds), (2, 3_500));
    assert_eq!(
        (big.cpu_millis, big.memory_bytes),
        (Some(1000), Some(2048 * MIB))
    );
    assert_eq!((big.commands, big.milliseconds), (1, 40));
}

/// A run whose tools reported nothing writes nothing: its state is what it was before the member
/// existed, and a state from before the member loads.
#[tokio::test]
async fn a_run_that_reports_nothing_keeps_no_sandbox_usage_and_old_state_loads() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("hello");
    let rt = runtime(
        Arc::new(MemoryStore::new()),
        vec![agent("llm", &mock).build()],
    );
    let run = rt.start("llm", user_message("hi"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    assert!(view.state.get("sandbox_usage").is_none(), "{}", view.state);

    let mut old = view.state.clone();
    old.as_object_mut().unwrap().remove("sandbox_usage");
    let loaded: Conversation = serde_json::from_value(old).unwrap();
    assert!(loaded.sandbox_usage.is_empty());
}

/// A call that ends in an error after its command ran (a cancel stops the command and the tool returns
/// an error) is still counted: the pod was held.
#[tokio::test]
async fn a_command_whose_call_ended_in_an_error_is_counted_too() {
    let mock = Arc::new(MockModel::new());
    mock.push_response(ModelResponse::tool_calls(vec![call("c1", "stopped")]))
        .push_text("done");
    let rt = runtime(
        Arc::new(MemoryStore::new()),
        vec![agent("llm", &mock).tool(Stopped).build()],
    );
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    let usage = conversation(&view).sandbox_usage;
    assert_eq!(usage.entries().len(), 1, "{usage:?}");
    assert_eq!(usage.entries()[0].milliseconds, 700);
}

/// A call that fails transiently is tried again, and only the try that is kept is counted.
#[tokio::test]
async fn a_try_that_failed_transiently_is_not_counted_twice() {
    let mock = Arc::new(MockModel::new());
    mock.push_response(ModelResponse::tool_calls(vec![call("c1", "flaky")]))
        .push_response(ModelResponse::tool_calls(vec![call("c1", "flaky")]))
        .push_text("done");
    let rt = runtime(
        Arc::new(MemoryStore::new()),
        vec![
            agent("llm", &mock)
                .tool(Flaky {
                    tries: AtomicUsize::new(0),
                })
                .build(),
        ],
    );
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    let usage = conversation(&view).sandbox_usage;
    assert_eq!(usage.entries().len(), 1, "{usage:?}");
    assert_eq!(
        (usage.entries()[0].commands, usage.entries()[0].milliseconds),
        (1, 100)
    );
}

/// A call whose result the journal already holds is not made again, and what its record says is
/// counted: the same as before the restart.
#[tokio::test]
async fn a_replayed_call_counts_what_its_record_holds() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    // The model is not called either: its answer is recorded too.
    let rt = runtime(
        store.clone(),
        vec![agent("llm", &mock).tool(Command).build()],
    );
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    store
        .journal_put(
            run,
            JournalEntry::ok(
                0,
                "model:0",
                json!({
                    "message": {"role": "assistant", "content": [],
                                "tool_calls": [{"id": "c1", "name": "command", "arguments": {}}]},
                    "finish": "tool_calls",
                    "usage": {"input_tokens": 1, "output_tokens": 1},
                }),
            ),
        )
        .await
        .unwrap();
    store
        .journal_put(
            run,
            JournalEntry::ok(
                1,
                "tool:c1",
                json!({
                    "content": "ran",
                    "sandbox": {"environment": "kubernetes",
                                "usage": [{"cpu_millis": 250, "memory_bytes": 536870912,
                                           "commands": 3, "milliseconds": 4200}]},
                }),
            ),
        )
        .await
        .unwrap();
    mock.push_text("done");
    let view = run_to_end(&rt, run).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    let usage = conversation(&view).sandbox_usage;
    assert_eq!(usage.entries().len(), 1, "{usage:?}");
    assert_eq!(
        (usage.entries()[0].commands, usage.entries()[0].milliseconds),
        (3, 4_200),
        "counted from the record, once, though the tool never ran"
    );
}

/// A subagent's commands run in the root run's sandbox: they are the task's, added to the parent's
/// when the child answers, and the shape that both used is one entry.
#[tokio::test]
async fn a_subagents_commands_are_counted_with_its_parent() {
    let parent_model = Arc::new(MockModel::new());
    parent_model
        .push_response(ModelResponse::tool_calls(vec![
            command("p1", 250, 512 * MIB, 100),
            call("t1", "spawn"),
        ]))
        .push_text("parent done");
    let child_model = Arc::new(MockModel::new());
    child_model
        .push_response(ModelResponse::tool_calls(vec![
            command("k1", 250, 512 * MIB, 50),
            command("k2", 500, 512 * MIB, 7),
        ]))
        .push_text("child done");
    let rt = runtime(
        Arc::new(MemoryStore::new()),
        vec![
            agent("parent", &parent_model)
                .tool(Command)
                .tool(Spawn {
                    target: "child".into(),
                })
                .build(),
            agent("child", &child_model).tool(Command).build(),
        ],
    );
    let run = rt.start("parent", user_message("go"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");

    let usage = conversation(&view).sandbox_usage;
    let [shared, other] = usage.entries() else {
        panic!("two shapes: {usage:?}");
    };
    assert_eq!(
        (shared.cpu_millis, shared.commands, shared.milliseconds),
        (Some(250), 2, 150)
    );
    assert_eq!(
        (other.cpu_millis, other.commands, other.milliseconds),
        (Some(500), 1, 7)
    );
}
