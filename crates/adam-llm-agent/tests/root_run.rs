//! `ToolCtx::root_run_id`: a tool in a child run knows the run whose work it serves, however deep
//! the chain, and a run that is nobody's child is its own root.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Conversation, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::{RunView, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {"message": {"type": "string"}}}),
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

/// Records the run and the root run a call sees.
struct Probe {
    seen: Arc<Mutex<Vec<(String, RunId, RunId)>>>,
    agent: String,
}

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        spec("probe")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.seen
            .lock()
            .unwrap()
            .push((self.agent.clone(), ctx.run_id(), ctx.root_run_id()));
        Ok(ToolOutput::text("seen"))
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.status == RunStatus::Done {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).unwrap()
}

/// `top` probes, then spawns `mid`; `mid` probes, then spawns `leaf`; `leaf` probes.
#[tokio::test]
async fn a_child_of_a_child_sees_the_top_run_and_a_top_run_sees_itself() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe = |agent: &str| Probe {
        seen: seen.clone(),
        agent: agent.into(),
    };

    let (top_model, mid_model, leaf_model) = (
        Arc::new(MockModel::new()),
        Arc::new(MockModel::new()),
        Arc::new(MockModel::new()),
    );
    top_model
        .push_tool_calls(vec![call("t1", "probe")])
        .push_tool_calls(vec![call("t2", "spawn")])
        .push_text("top done");
    mid_model
        .push_tool_calls(vec![call("m1", "probe")])
        .push_tool_calls(vec![call("m2", "spawn")])
        .push_text("mid done");
    leaf_model
        .push_tool_calls(vec![call("l1", "probe")])
        .push_text("leaf done");

    let agent = |name: &str, model: Arc<MockModel>, target: Option<&str>| {
        let model: DynModel = model;
        let mut builder = LlmAgent::builder(name, model, "m").tool(probe(name));
        if let Some(target) = target {
            builder = builder.tool(Spawn {
                target: target.into(),
            });
        }
        builder.build()
    };
    let rt = Runtime::builder(store)
        .poll_interval(Duration::from_millis(20))
        .agent(agent("top", top_model, Some("mid")))
        .agent(agent("mid", mid_model, Some("leaf")))
        .agent(agent("leaf", leaf_model, None))
        .build();

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

    let top = rt.start("top", user_message("go"), None).await.unwrap();
    let view = wait_done(&rt, top).await;
    assert_eq!(conversation(&view).root_run, None, "nobody's child");
    let mid = adam_runtime::child_run_id(top, "t2");
    let leaf = adam_runtime::child_run_id(mid, "m2");
    let mid_state = conversation(&wait_done(&rt, mid).await);
    let leaf_state = conversation(&wait_done(&rt, leaf).await);

    let _ = stop.send(());
    worker.await.unwrap().unwrap();

    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by_key(|(agent, ..)| ["top", "mid", "leaf"].iter().position(|a| a == agent));
    assert_eq!(
        seen,
        [
            ("top".to_owned(), top, top),
            ("mid".to_owned(), mid, top),
            ("leaf".to_owned(), leaf, top),
        ]
    );
    // It is in the stored state of the children, so a restart or another worker reads the same.
    assert_eq!(mid_state.root_run, Some(top));
    assert_eq!(leaf_state.root_run, Some(top), "the top, not the parent");
}

#[test]
fn a_detached_context_is_its_own_root() {
    let ctx = ToolCtx::detached("t", "c", Arc::new(adam_runtime::NoopSink));
    assert_eq!(ctx.root_run_id(), ctx.run_id());
    let root = RunId::new();
    assert_eq!(ctx.with_root_run(root).root_run_id(), root);
}
