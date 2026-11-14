//! Cancelling an A2A task cancels the subagent run its agent is waiting for (ADR 0044 slice 10):
//! `CancelTask` commits the task's run as cancelled and answers `Canceled` at once, and the child
//! run follows within a poll. Remote A2A tasks a run started are not cancelled by this (ADR 0045
//! slice 5).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::Duration;

use a2a::{Message, Part, Role, TaskState};
use adam_a2a::{Caller, TaskBackend};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::{MemoryStore, RunId, RunStatus};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime, child_run_id};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

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
        ..Default::default()
    }
}

/// Starts the agent `llm/explorer` as a child run and waits for it, as `SubagentTool` does.
struct Spawn;

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> ToolSpec {
        spec("explorer")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx.start_child("llm/explorer", "look around").await?;
        Err(ToolError::AwaitRun { run })
    }
}

/// Says it started, then works until its run is cancelled.
struct Work {
    reached: Arc<Notify>,
    saw_cancel: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for Work {
    fn spec(&self) -> ToolSpec {
        spec("work")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.reached.notify_one();
        ctx.cancelled().await;
        self.saw_cancel.store(true, SeqCst);
        Ok(ToolOutput::text("stopped"))
    }
}

#[tokio::test]
async fn cancel_task_reaches_the_subagent_run() {
    let parent_model = Arc::new(MockModel::new());
    parent_model
        .push_tool_calls(vec![call("c1", "explorer")])
        .push_text("never reached");
    let child_model = Arc::new(MockModel::new());
    child_model
        .push_tool_calls(vec![call("k1", "work")])
        .push_text("never reached");
    let (reached, saw_cancel) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
    let (parent_model, child_model): (DynModel, DynModel) = (parent_model, child_model);
    let llm = LlmAgent::builder("llm", parent_model, "m")
        .tool(Spawn)
        .build();
    let explorer = LlmAgent::builder("llm/explorer", child_model, "m")
        .tool(Work {
            reached: reached.clone(),
            saw_cancel: saw_cancel.clone(),
        })
        .build();
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(Arc::new(MemoryStore::new()))
        .agent(llm)
        .agent(explorer)
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, "llm")
        .with_poll_interval(Duration::from_millis(10));

    let caller = Caller::new("token-0");
    let task = backend
        .submit(
            caller.clone(),
            Message::new(Role::User, vec![Part::text("go")]),
            None,
            None,
        )
        .await
        .expect("submit");
    let (stop, rx) = oneshot::channel::<()>();
    let rt = runtime.clone();
    let worker = tokio::spawn(async move {
        let _ = rt
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    tokio::time::timeout(Duration::from_secs(20), reached.notified())
        .await
        .expect("the subagent's tool started");

    let canceled = backend.cancel(&caller, &task.id).await.expect("cancel");
    assert_eq!(canceled.status.state, TaskState::Canceled);

    let parent = RunId(task.id.parse().expect("a task id is a run id"));
    let child = child_run_id(parent, "c1");
    let mut ended = None;
    for _ in 0..2000 {
        let view = runtime.view(child).await.unwrap().unwrap();
        if view.status == RunStatus::Failed {
            ended = Some(view);
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let ended = ended.expect("the subagent run was cancelled with the task");
    assert_eq!(
        ended.error.as_deref(),
        Some("cancelled: the parent run ended")
    );
    for _ in 0..2000 {
        if saw_cancel.load(SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        saw_cancel.load(SeqCst),
        "the subagent's tool saw its token fire"
    );
    let _ = stop.send(());
    worker.await.unwrap();
}
