//! Custom events over A2A (ADR 0050, slice H5): a tool of a real `LlmAgent` emits two events, one
//! declared for clients and one for hooks only; a client subscribed to the task reads the first as
//! a data part (`{"kind": <name>, "payload": ...}`, the `RunEvent::Custom` the A2A layer already
//! serves) and never the second. The runtime is a real `Runtime` on the in-memory store, the backend
//! a `RuntimeTaskBackend`, and the model a scripted one.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, TaskState};
use adam_a2a::{BackendError, Caller, TaskBackend, TaskEvent};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::MemoryStore;
use adam_llm_agent::{EventSpec, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::{MockModel, ToolCall, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// Emits one event for clients and one for hooks, and answers `done`.
struct Announces;

#[async_trait]
impl Tool for Announces {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "announce".into(),
            description: "announce".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn events(&self) -> Vec<EventSpec> {
        vec![
            EventSpec::new("demo.published").published().with_version(2),
            EventSpec::new("demo.kept"),
        ]
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        ctx.emit_event("demo.published", json!({"title": "release"}))
            .expect("declared for clients");
        ctx.emit_event("demo.kept", json!({"title": "internal"}))
            .expect("declared for hooks");
        Ok(ToolOutput::text("done"))
    }
}

type Events = BoxStream<'static, Result<TaskEvent, BackendError>>;

/// The data parts of every working message a subscriber reads while one run goes to its end.
async fn data_parts_read_by_a_subscriber() -> Vec<Value> {
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "announce".into(),
            arguments: json!({}),
            ..Default::default()
        }])
        .push_text("all done");
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(Arc::new(MemoryStore::new()))
        .agent(
            LlmAgent::builder("announcer", model, "m")
                .tool(Announces)
                .try_build()
                .expect("the declarations are valid"),
        )
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, "announcer")
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
    // Subscribed before the worker starts, so nothing is missed.
    let mut stream: Events = backend.subscribe(&caller, &task.id);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        TaskEvent::Snapshot(_)
    ));
    let (stop, rx) = oneshot::channel::<()>();
    let rt = runtime.clone();
    let worker = tokio::spawn(async move {
        let _ = rt
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    let mut parts = Vec::new();
    let mut last = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(item)) => {
                // Only the status updates carry what this test reads.
                if let TaskEvent::Status(update) = item.expect("stream item was an error") {
                    last = Some(update.status.state.clone());
                    if let Some(message) = update.status.message {
                        for part in message.parts {
                            if let PartContent::Data(data) = part.content {
                                parts.push(data);
                            }
                        }
                    }
                }
            }
            Ok(None) => break,
            Err(_) => panic!("timed out; got {parts:?}"),
        }
    }
    let _ = stop.send(());
    worker.await.unwrap();
    assert_eq!(last, Some(TaskState::Completed));
    parts
}

#[tokio::test]
async fn an_a2a_client_sees_a_clients_event_and_not_a_hooks_one() {
    let parts = data_parts_read_by_a_subscriber().await;
    let kinds: Vec<&str> = parts
        .iter()
        .filter_map(|part| part["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"demo.published"),
        "the Clients event is a data part: {parts:?}"
    );
    assert!(
        !kinds.contains(&"demo.kept"),
        "the Hooks event stays in the process: {parts:?}"
    );
    assert!(
        !parts.iter().any(|p| p.to_string().contains("internal")),
        "nor does its data leak into another event: {parts:?}"
    );
    let published = parts
        .iter()
        .find(|part| part["kind"] == "demo.published")
        .unwrap();
    assert_eq!(published["payload"]["version"], 2);
    assert_eq!(published["payload"]["data"], json!({"title": "release"}));
    let key = published["payload"]["key"].as_str().unwrap();
    assert!(
        key.ends_with("/tool:c1/demo.published/0"),
        "the key a consumer dedupes on: {key}"
    );
}
