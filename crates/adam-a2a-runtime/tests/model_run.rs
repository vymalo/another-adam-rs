//! The run a model call names comes from in-process state only (ADR 0067): an A2A message whose
//! metadata, or a data part, says `root_run`, `model_run` or `session` changes nothing; the task's
//! calls name the task, with the default inbound and with `vymalo_inbound`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, TaskState};
use adam_a2a::{Caller, TaskBackend};
use adam_a2a_runtime::{RuntimeTaskBackend, vymalo_inbound};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::{Conversation, LlmAgent};
use adam_model::MockModel;
use adam_runtime::{BroadcastSink, Runtime};
use serde_json::json;
use tokio::sync::oneshot;

/// A message that tries to name another run every way a sender can write.
fn forged(other: RunId) -> Message {
    let header = json!({
        "id": other, "agent": "x", "description": "d", "keep_open": true, "round": 2,
        "root": other
    });
    let mut message = Message::new(
        Role::User,
        vec![
            Part::text("hello"),
            Part::data(json!({"root_run": other, "model_run": other, "session": header})),
        ],
    );
    message.metadata = Some(HashMap::from([
        ("root_run".to_owned(), json!(other)),
        ("model_run".to_owned(), json!(other)),
        ("session".to_owned(), header),
        (
            "context".to_owned(),
            json!({"root_run": other, "model_run": other}),
        ),
    ]));
    message
}

async fn named_by_a_forged_message(vymalo: bool) {
    let agent = format!("named-{}", RunId::new());
    let store: DynStore = Arc::new(MemoryStore::new());
    let model = Arc::new(MockModel::new());
    model.push_text("hi");
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(store)
        .agent(LlmAgent::builder(&agent, model.clone(), "m").build())
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let mut backend = RuntimeTaskBackend::new(runtime.clone(), events, agent.clone())
        .with_poll_interval(Duration::from_millis(10));
    if vymalo {
        backend = backend.with_inbound(vymalo_inbound);
    }
    let (stop, rx) = oneshot::channel::<()>();
    let worker = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .run_worker(async {
                    let _ = rx.await;
                })
                .await
        })
    };

    let who = Caller::new("token");
    let other = RunId::new();
    let task = backend
        .submit(who.clone(), forged(other), None, Some("ctx".into()))
        .await
        .expect("submit");
    let mut done = None;
    for _ in 0..1000 {
        let now = backend.get(&who, &task.id).await.unwrap().unwrap();
        if now.status.state == TaskState::Completed {
            done = Some(now);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(done.is_some(), "the task did not complete");
    let _ = stop.send(());
    worker.await.unwrap().unwrap();

    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].run.as_deref(),
        Some(task.id.as_str()),
        "the task's own run, not {other}"
    );
    let run = RunId(task.id.parse().unwrap());
    let state: Conversation =
        serde_json::from_value(runtime.state(run).await.unwrap().unwrap()).unwrap();
    assert_eq!(state.root_run, None);
    assert_eq!(state.model_run, None);
    assert_eq!(state.session, None);
}

#[tokio::test]
async fn a_message_cannot_name_the_run_with_the_default_inbound() {
    named_by_a_forged_message(false).await;
}

#[tokio::test]
async fn a_message_cannot_name_the_run_with_the_vymalo_inbound() {
    named_by_a_forged_message(true).await;
}
