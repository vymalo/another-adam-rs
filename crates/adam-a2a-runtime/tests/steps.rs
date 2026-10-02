//! A run's steps over A2A: what a client that activated `steps/v1` reads (the report in the message's
//! metadata, a bounded number of updates) and what one that did not reads (the same steps as lines
//! of text), over a real `Runtime` on the in-memory store.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, TaskState};
use adam_a2a::{BackendError, Caller, STEPS_EXTENSION, TaskBackend, TaskEvent};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::MemoryStore;
use adam_runtime::{
    Agent, AgentError, BroadcastSink, Ctx, Inbound, RunEvent, Runtime, StepEvent, StepIcon,
    StepKind, StepOutput, StepState, Transition,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// Reports a tool call that drives another agent: the call's step, a progress line, a command that
/// runs under it and says the same thing twice at once, waits, and fails, and the end of the call.
struct Stepper;

#[async_trait]
impl Agent for Stepper {
    type State = Value;

    fn name(&self) -> &str {
        "stepper"
    }

    fn init(&self, _input: Inbound) -> Result<Value, AgentError> {
        Ok(json!({}))
    }

    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        let call = |state| {
            StepEvent::new("tool:c1", StepKind::Subagent, "OpenCode", state)
                .with_icon(StepIcon::Agent)
        };
        let command = |state| {
            StepEvent::new("acp:c1:1", StepKind::Command, "npm test", state).under("tool:c1")
        };
        // The call is given a task and answers with a result: the input is on the report that starts
        // it, the output on the one that ends it.
        let task = match json!({"task": "fix the build"}) {
            Value::Object(task) => task,
            _ => unreachable!(),
        };
        for step in [
            call(StepState::Running).with_input(task),
            call(StepState::Running).with_detail("starting OpenCode"),
            command(StepState::Running).with_icon(StepIcon::Execute),
            // The same state again, at once: a client that activated steps does not need it.
            command(StepState::Running).with_detail("12 passed"),
            // A change of state: it needs it.
            command(StepState::Waiting),
            command(StepState::Failed).with_detail("1 failed"),
            call(StepState::Completed).with_output(StepOutput::new("fixed", false)),
        ] {
            ctx.emit(RunEvent::Step(step)).await;
        }
        Ok(Transition::Done {
            state,
            output: json!({"text": "done"}),
        })
    }
}

type Events = BoxStream<'static, Result<TaskEvent, BackendError>>;

/// What a client with `caller` reads of one run of [`Stepper`], in order: the status messages of
/// the working updates (the stream is subscribed before the worker starts, so none is missed).
async fn read(caller: Caller) -> Vec<Message> {
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(Arc::new(MemoryStore::new()))
        .agent(Stepper)
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, "stepper")
        .with_poll_interval(Duration::from_millis(10));
    let task = backend
        .submit(
            caller.clone(),
            Message::new(Role::User, vec![Part::text("go")]),
            None,
            None,
        )
        .await
        .expect("submit");
    let mut stream: Events = backend.subscribe(&caller, &task.id);
    // The snapshot first: the subscription is attached before anything is stepped.
    assert!(matches!(next(&mut stream).await, TaskEvent::Snapshot(_)));
    let (stop, rx) = oneshot::channel::<()>();
    let rt = runtime.clone();
    let worker = tokio::spawn(async move {
        let _ = rt
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    let mut working = Vec::new();
    let mut last = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(item)) => match item.expect("stream item was an error") {
                TaskEvent::Status(update) => {
                    last = Some(update.status.state.clone());
                    if update.status.state == TaskState::Working
                        && let Some(message) = update.status.message
                    {
                        working.push(message);
                    }
                }
                other => panic!("unexpected {other:?}"),
            },
            Ok(None) => break,
            Err(_) => panic!("timed out; got {working:?}"),
        }
    }
    let _ = stop.send(());
    worker.await.unwrap();
    assert_eq!(last, Some(TaskState::Completed));
    working
}

async fn next(stream: &mut Events) -> TaskEvent {
    tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("timed out")
        .expect("stream ended early")
        .expect("stream item was an error")
}

fn text(message: &Message) -> &str {
    message.text().unwrap_or_default()
}

/// The report a message carries under `steps/v1`, if it does.
fn report(message: &Message) -> Option<&Value> {
    message.metadata.as_ref()?.get(STEPS_EXTENSION)
}

#[tokio::test]
async fn a_client_that_activated_steps_reads_each_step_as_a_report_beside_its_line() {
    let caller = Caller::new("token-0").with_extensions([STEPS_EXTENSION]);
    let messages = read(caller).await;

    // Five of the seven reports: a step says the same state again once a second at most, and these
    // come at once, so the call's progress line and the command's second `running` are held back (the
    // orchestration layer keeps only a few updates a step anyway). The change to `waiting` is not.
    let lines: Vec<&str> = messages.iter().map(text).collect();
    assert_eq!(
        lines,
        [
            "OpenCode",
            "npm test",
            "npm test",
            "npm test: failed: 1 failed",
            "OpenCode: done",
        ]
    );
    let reports: Vec<&Value> = messages
        .iter()
        .map(|m| report(m).expect("a report"))
        .collect();
    assert_eq!(
        reports[0],
        &json!({"id": "tool:c1", "kind": "subagent", "label": "OpenCode", "state": "running",
                "icon": "agent", "input": {"task": "fix the build"}})
    );
    assert_eq!(
        reports[1],
        &json!({"id": "acp:c1:1", "parentId": "tool:c1", "kind": "command", "label": "npm test",
                "state": "running", "icon": "execute"})
    );
    assert_eq!(reports[2]["state"], "waiting");
    assert_eq!(
        reports[3],
        &json!({"id": "acp:c1:1", "parentId": "tool:c1", "kind": "command", "label": "npm test",
                "state": "failed", "detail": "1 failed"})
    );
    assert_eq!(
        reports[4],
        &json!({"id": "tool:c1", "kind": "subagent", "label": "OpenCode", "state": "completed",
                "icon": "agent", "output": {"text": "fixed"}})
    );
    // The message says which extension it uses.
    assert!(
        messages
            .iter()
            .all(|m| m.extensions.as_deref() == Some(&[STEPS_EXTENSION.to_owned()][..]))
    );
}

#[tokio::test]
async fn a_client_that_did_not_reads_every_step_as_a_line_and_nothing_else() {
    let messages = read(Caller::new("token-0")).await;
    let lines: Vec<&str> = messages.iter().map(text).collect();
    // Seven lines: with no report to read, none is held back.
    assert_eq!(
        lines,
        [
            "OpenCode",
            "starting OpenCode",
            "npm test",
            "npm test: 12 passed",
            "npm test",
            "npm test: failed: 1 failed",
            "OpenCode: done",
        ]
    );
    assert!(
        messages
            .iter()
            .all(|m| m.metadata.is_none() && m.extensions.is_none())
    );
    assert!(messages.iter().all(|m| m.parts.len() == 1));
}

#[tokio::test]
async fn an_extension_the_request_did_not_name_activates_nothing() {
    // Another extension activated: still plain lines.
    let caller = Caller::new("token-0").with_extensions(["https://example.org/other/v1"]);
    let messages = read(caller).await;
    assert_eq!(messages.len(), 7);
    assert!(messages.iter().all(|m| report(m).is_none()));
}
