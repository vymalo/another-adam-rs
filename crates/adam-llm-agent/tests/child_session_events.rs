//! Child and session events for observers (ADR 0050, slice H7): a scripted run with a one-shot
//! child and a kept-open session gives the events in a golden order, and gives them in the same
//! order again when the script is run again.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    HookError, HookEvent, LlmAgent, Plugin, SessionAgent, SessionTools, Tool, ToolCtx, ToolError,
    ToolOutput, user_message,
};
use adam_model::{MockModel, ToolCall};
use adam_runtime::{Runtime, child_run_id};
use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Default)]
struct Watches(Mutex<Vec<HookEvent>>);

struct Shared(Arc<Watches>);

impl Plugin for Shared {
    fn name(&self) -> &str {
        "watches"
    }

    fn observe(&self, event: &HookEvent) -> Result<(), HookError> {
        self.0.0.lock().unwrap().push(event.clone());
        Ok(())
    }
}

/// Starts the agent `target` as a one-shot child run and waits for it.
struct Spawn(String);

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> adam_model::ToolSpec {
        adam_model::ToolSpec {
            name: "spawn".into(),
            description: "starts a child".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx.start_child(&self.0, "go").await?;
        Err(ToolError::AwaitRun { run })
    }
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// One scripted run: a one-shot child (`c1`), then a kept-open session that is started (`c2`),
/// read (`c3`), sent a second round (`c4`), read (`c5`) and closed (`c6`). Returns the events as
/// `(name, key)` with the run ids made stable, and the events themselves.
async fn scripted_run() -> Vec<(String, String)> {
    let store: adam_core::DynStore = Arc::new(MemoryStore::new());
    let child_name = format!("child-{}", RunId::new());
    let parent_name = format!("parent-{}", RunId::new());
    let parent_mock = Arc::new(MockModel::new());
    let child_mock = Arc::new(MockModel::new());
    child_mock
        .push_text("one-shot done")
        .push_text("round one done")
        .push_text("round two done");
    let watches = Arc::new(Watches::default());

    let roster = SessionTools::new([SessionAgent {
        name: "helper".into(),
        agent: child_name.clone(),
        description: "Does a piece of work.".into(),
        within: None,
        remote: false,
        bounds: adam_runtime::SessionBounds::default(),
    }]);
    let mut builder = LlmAgent::builder(&parent_name, parent_mock.clone(), "m")
        .session_agents([child_name.clone()])
        .tool(Spawn(child_name.clone()))
        .plugin(Shared(watches.clone()));
    for tool in roster.tools() {
        builder = builder.dyn_tool(tool);
    }
    let parent = builder.build();
    let child = LlmAgent::builder(&child_name, child_mock.clone(), "m").build();

    let rt = Runtime::builder(store)
        .agent(parent)
        .agent(child)
        .worker_id(format!("w-{}", RunId::new()))
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_secs(10))
        .build();
    let run = rt
        .start(
            &parent_name,
            user_message("go"),
            Some(&format!("conv-{}", RunId::new())),
        )
        .await
        .expect("start");
    let session = child_run_id(run, "c2");
    let read = |id: &str| {
        call(
            id,
            "subagent_read",
            json!({"session": session.to_string(), "wait": true}),
        )
    };
    parent_mock
        .push_tool_calls(vec![call("c1", "spawn", json!({}))])
        .push_tool_calls(vec![call(
            "c2",
            "subagent_start",
            json!({"agent": "helper", "description": "look", "prompt": "work", "keep_open": true}),
        )])
        .push_tool_calls(vec![read("c3")])
        .push_tool_calls(vec![call(
            "c4",
            "subagent_send",
            json!({"session": session.to_string(), "message": "again"}),
        )])
        .push_tool_calls(vec![read("c5")])
        .push_tool_calls(vec![call(
            "c6",
            "subagent_close",
            json!({"session": session.to_string(), "cancel": false}),
        )])
        .push_text("all done");

    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        })
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = rt.view(run).await.expect("view").expect("run");
        if view.status == RunStatus::Done {
            break;
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    worker.await.expect("worker task").expect("worker result");

    let stable = |s: &str| {
        // A report's key names its round by id, which a replay derives again from the same call.
        let s = match s.find("report_") {
            Some(at) => format!(
                "{}report_<round>{}",
                &s[..at],
                &s[at + "report_".len() + 32..]
            ),
            None => s.to_owned(),
        };
        s.replace(&run.to_string(), "<run>")
            .replace(&session.to_string(), "<session>")
            .replace(&child_run_id(run, "c1").to_string(), "<child>")
    };
    let seen = watches.0.lock().unwrap().clone();
    seen.iter()
        .filter(|e| {
            let n = e.kind.name();
            n.starts_with("child.") || n.starts_with("session.")
        })
        .map(|e| (e.kind.name().to_owned(), stable(&e.header.key)))
        .collect()
}

#[tokio::test]
async fn child_and_session_events_come_in_a_golden_order_and_again_on_a_replay() {
    // A report is told when a round that has ended meets the next model call, so whether it falls
    // before or after the next call of the script depends on timing: the golden order is of the
    // events the script's own calls make, and the reports are checked beside it.
    let without_reports = |events: Vec<(String, String)>| -> Vec<(String, String)> {
        events
            .into_iter()
            .filter(|(n, _)| n != "session.reported")
            .collect()
    };
    let all = scripted_run().await;
    // Whether a round's report is told before the script ends depends on timing too: when one is,
    // its key is the round's own.
    for (name, key) in &all {
        if name == "session.reported" {
            assert!(key.starts_with("<run>/report_<round>/"), "{key}");
        }
    }
    let first = without_reports(all);
    let names: Vec<&str> = first.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "child.started",
            "child.ended",
            "session.opened",
            "session.round",
            "session.closed",
        ],
        "{first:#?}"
    );
    let keys: std::collections::HashSet<_> = first.iter().map(|(_, k)| k.clone()).collect();
    assert_eq!(keys.len(), first.len(), "every key is its own: {first:#?}");

    // The same script, run again, says the same events in the same order under the same keys.
    assert_eq!(without_reports(scripted_run().await), first);
}
