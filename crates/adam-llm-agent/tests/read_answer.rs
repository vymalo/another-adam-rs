//! `Tool::read_answer`: the tool that asked reads the answer before the model does. A refused answer
//! keeps the run parked on the question the tool asks in its place, gives the model nothing and is
//! journaled as `answer_refused`; an accepted one is the result of the call, as it always was. A real
//! `Runtime` with a worker, a scripted `MockModel` and `MemoryStore`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    AnswerCheck, Conversation, LlmAgent, PendingWait, Refusal, Tool, ToolCtx, ToolError,
    ToolOutput, user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, RunEvent, RunView, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// `pick`: asks "which?" with a form, and reads an answer that is not "ok" as a refusal that asks
/// again on a form of its own. The call's own arguments and the form it drew come back to it.
struct Pick;

#[async_trait]
impl Tool for Pick {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "pick".into(),
            description: "Ask which.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn asks_user(&self) -> bool {
        true
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input_with_ui(
            "which?",
            json!([{"form": "first"}]),
        ))
    }

    fn read_answer(
        &self,
        call_id: &str,
        args: &Value,
        ui: Option<&Value>,
        payload: &Value,
    ) -> AnswerCheck {
        if payload["text"] == "ok" {
            return AnswerCheck::Accept;
        }
        let attempt = ui.map_or(0, |ui| ui[0]["attempt"].as_u64().unwrap_or(0)) + 1;
        AnswerCheck::Refuse(
            Refusal::new(
                format!("{} was refused ({}). which?", payload["text"], args["note"]),
                Some(json!([{"form": "again", "attempt": attempt, "call": call_id}])),
                "not_ok",
            )
            .about("q1"),
        )
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

struct Rig {
    mock: Arc<MockModel>,
    sink: CollectingSink,
    rt: Runtime,
    run: RunId,
}

async fn started() -> Rig {
    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "test-model")
        .tool(Pick)
        .build();
    mock.push_tool_calls(vec![call("c1", "pick", json!({"note": "n"}))])
        .push_text("thanks");
    let sink = CollectingSink::new();
    let rt = Runtime::builder(store)
        .agent(agent)
        .event_sink(sink.clone())
        .poll_interval(Duration::from_millis(20))
        .build();
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    Rig {
        mock,
        sink,
        rt,
        run,
    }
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

async fn until(rt: &Runtime, run: RunId, what: &str, ok: impl Fn(&RunView) -> bool) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if ok(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "the run never got to {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn asking_again(attempt: u64) -> impl Fn(&RunView) -> bool {
    move |view: &RunView| {
        matches!(
            conversation(view).pending_wait,
            Some(PendingWait::Question(q))
                if q.ui.as_ref().is_some_and(|ui| ui[0]["attempt"] == attempt)
        )
    }
}

#[tokio::test]
async fn a_refused_answer_keeps_the_run_parked_on_the_new_question_and_the_model_hears_nothing() {
    let rig = started().await;
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rig.rt.clone();
    let worker = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
    });
    let rt = &rig.rt;
    until(rt, rig.run, "parked", |v| v.waiting).await;

    rt.deliver(rig.run, user_message("nope")).await.unwrap();
    let view = until(rt, rig.run, "asked again", asking_again(1)).await;
    let state = conversation(&view);
    let Some(PendingWait::Question(q)) = &state.pending_wait else {
        panic!("not parked on a question: {:?}", state.pending_wait);
    };
    assert_eq!(q.call_id, "c1");
    assert_eq!(q.tool, "pick");
    assert_eq!(q.question, "\"nope\" was refused (\"n\"). which?");
    assert_eq!(
        q.ui,
        Some(json!([{"form": "again", "attempt": 1, "call": "c1"}]))
    );
    assert_eq!(view.status, RunStatus::Parked);
    // The call is still owed its result: the refused answer is nobody's result.
    assert_eq!(state.pending_calls.len(), 1);
    assert!(
        !state
            .messages
            .iter()
            .any(|m| matches!(m, Message::Tool { .. })),
        "the model was given a tool result: {:?}",
        state.messages
    );
    assert_eq!(
        rig.mock.requests().len(),
        1,
        "the model was not asked again"
    );
    assert!(rig.sink.events_for(rig.run).contains(&RunEvent::Custom {
        kind: "answer_refused".into(),
        payload: json!({"call_id": "c1", "question": "q1", "rule": "not_ok"}),
    }));

    // A second refusal asks again from the question it was shown, and an accepted answer is the
    // result of the call, with nothing of the refused ones in the history.
    rt.deliver(rig.run, user_message("still not"))
        .await
        .unwrap();
    until(rt, rig.run, "asked a second time", asking_again(2)).await;
    rt.deliver(rig.run, user_message("ok")).await.unwrap();
    let done = until(rt, rig.run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    let state = conversation(&done);
    assert!(
        state.messages.contains(&Message::tool_result("c1", "ok")),
        "{:?}",
        state.messages
    );
    let said: Vec<String> = state.messages.iter().map(Message::text).collect();
    assert!(
        !said
            .iter()
            .any(|t| t.contains("nope") || t.contains("still not")),
        "a refused answer reached the history: {said:?}"
    );
    assert_eq!(done.output.expect("output")["text"], "thanks");
    let refusals = rig
        .sink
        .events_for(rig.run)
        .into_iter()
        .filter(|e| matches!(e, RunEvent::Custom { kind, .. } if kind == "answer_refused"))
        .count();
    assert_eq!(refusals, 2);
}

#[test]
fn a_tool_that_does_not_read_answers_accepts_every_one() {
    struct Plain;
    #[async_trait]
    impl Tool for Plain {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "plain".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
            }
        }
        async fn call(&self, _: &ToolCtx, _: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("x"))
        }
    }
    assert_eq!(
        Plain.read_answer("c", &json!({}), None, &json!({"text": "anything"})),
        AnswerCheck::Accept
    );
}
