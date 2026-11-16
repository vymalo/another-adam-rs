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
use adam_runtime::{CollectingSink, Inbound, RunEvent, RunView, Runtime};
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
        match payload["text"].as_str() {
            Some("ok") => return AnswerCheck::Accept,
            Some("read") => return AnswerCheck::Read("read as a value".into()),
            Some("end") => {
                return AnswerCheck::End(
                    Refusal::new(
                        "no more questions",
                        Some(json!([{"ignored": true}])),
                        "gave_up",
                    )
                    .about("q1"),
                );
            }
            Some("whole") => {
                return AnswerCheck::Refuse(Refusal::new(
                    "the whole answer",
                    Some(json!([{"form": "again", "attempt": 1}])),
                    "unknown",
                ));
            }
            _ => {}
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

/// A runtime over `store` whose one agent has `pick` and answers with `mock`, with `sink` as its
/// events.
fn runtime_on(store: &DynStore, mock: &Arc<MockModel>, sink: &CollectingSink) -> Runtime {
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "test-model")
        .tool(Pick)
        .build();
    Runtime::builder(store.clone())
        .agent(agent)
        .event_sink(sink.clone())
        .poll_interval(Duration::from_millis(20))
        .build()
}

fn message(id: &str, text: &str) -> Inbound {
    Inbound::new("message", json!({ "text": text })).with_id(id)
}

fn spawn_worker(rt: &Runtime) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
        .unwrap();
    });
    (stop, handle)
}

fn refusals(sink: &CollectingSink, run: RunId) -> Vec<Value> {
    sink.events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Custom { kind, payload } if kind == "answer_refused" => Some(payload),
            _ => None,
        })
        .collect()
}

async fn started() -> Rig {
    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "pick", json!({"note": "n"}))])
        .push_text("thanks");
    let sink = CollectingSink::new();
    let rt = runtime_on(&store, &mock, &sink);
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

#[tokio::test]
async fn a_refusal_of_the_whole_answer_is_journaled_with_a_star_for_its_question() {
    let rig = started().await;
    let (stop, worker) = spawn_worker(&rig.rt);
    until(&rig.rt, rig.run, "parked", |v| v.waiting).await;
    rig.rt
        .deliver(rig.run, user_message("whole"))
        .await
        .unwrap();
    until(&rig.rt, rig.run, "asked again", asking_again(1)).await;
    let _ = stop.send(());
    worker.await.unwrap();
    assert_eq!(
        refusals(&rig.sink, rig.run),
        [json!({"call_id": "c1", "question": "*", "rule": "unknown"})]
    );
}

#[tokio::test]
async fn a_tool_may_end_the_question_and_the_model_reads_an_error_and_goes_on() {
    let rig = started().await;
    let (stop, worker) = spawn_worker(&rig.rt);
    until(&rig.rt, rig.run, "parked", |v| v.waiting).await;
    rig.rt.deliver(rig.run, user_message("end")).await.unwrap();
    let done = until(&rig.rt, rig.run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    worker.await.unwrap();
    let state = conversation(&done);
    assert_eq!(
        state
            .messages
            .iter()
            .find(|m| matches!(m, Message::Tool { .. })),
        Some(&Message::tool_error("c1", "no more questions"))
    );
    assert!(state.pending_wait.is_none() && state.pending_calls.is_empty());
    assert_eq!(rig.mock.requests().len(), 2, "the model answered once more");
    assert_eq!(
        refusals(&rig.sink, rig.run),
        [json!({"call_id": "c1", "question": "q1", "rule": "gave_up"})]
    );
    assert_eq!(done.output.expect("output")["text"], "thanks");
}

#[tokio::test]
async fn a_tool_may_say_what_the_answer_reads_as() {
    let rig = started().await;
    let (stop, worker) = spawn_worker(&rig.rt);
    until(&rig.rt, rig.run, "parked", |v| v.waiting).await;
    rig.rt.deliver(rig.run, user_message("read")).await.unwrap();
    let done = until(&rig.rt, rig.run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    worker.await.unwrap();
    assert!(
        conversation(&done)
            .messages
            .contains(&Message::tool_result("c1", "read as a value"))
    );
    assert!(refusals(&rig.sink, rig.run).is_empty());
}

#[tokio::test]
async fn messages_that_arrive_together_are_read_in_turn_and_a_copy_of_one_is_read_once() {
    let rig = started().await;
    let (stop, worker) = spawn_worker(&rig.rt);
    until(&rig.rt, rig.run, "parked", |v| v.waiting).await;
    let _ = stop.send(());
    worker.await.unwrap();
    // Four messages sit in the inbox of a run nobody is stepping: a refused one and a copy of it,
    // the one that fits, and what comes after the question is answered.
    for inbound in [
        message("m-1", "nope"),
        message("m-1", "nope"),
        message("m-2", "ok"),
        message("m-3", "and one more thing"),
    ] {
        rig.rt.deliver(rig.run, inbound).await.unwrap();
    }
    let (stop, worker) = spawn_worker(&rig.rt);
    let done = until(&rig.rt, rig.run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    worker.await.unwrap();
    let state = conversation(&done);
    assert_eq!(
        refusals(&rig.sink, rig.run),
        [json!({"call_id": "c1", "question": "q1", "rule": "not_ok"})],
        "refused once, though it came twice"
    );
    assert_eq!(state.read_ids, ["m-1", "m-2", "m-3"]);
    assert!(state.messages.contains(&Message::tool_result("c1", "ok")));
    // Nothing is judged once the question is answered: the next message is a message.
    assert!(
        state
            .messages
            .contains(&Message::user_text("and one more thing")),
        "{:?}",
        state.messages
    );
    assert!(
        !state.messages.iter().any(|m| m.text().contains("nope")),
        "a refused answer reached the history"
    );
    assert_eq!(rig.mock.requests().len(), 2);
}

#[tokio::test]
async fn a_run_that_is_loaded_again_refuses_from_the_question_it_stored() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "pick", json!({"note": "n"}))])
        .push_text("thanks");
    let sink = CollectingSink::new();
    let first = runtime_on(&store, &mock, &sink);
    let run = first
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let (stop, worker) = spawn_worker(&first);
    until(&first, run, "parked", |v| v.waiting).await;
    first.deliver(run, message("m-1", "nope")).await.unwrap();
    until(&first, run, "asked again", asking_again(1)).await;
    let _ = stop.send(());
    worker.await.unwrap();
    drop(first);

    // A new process: the runtime and the agent are new, the question is read from the store.
    let second = runtime_on(&store, &mock, &sink);
    let (stop, worker) = spawn_worker(&second);
    second
        .deliver(run, message("m-2", "still not"))
        .await
        .unwrap();
    let view = until(&second, run, "asked a second time", asking_again(2)).await;
    // The same message again, after the restart, is read as it was: once.
    second
        .deliver(run, message("m-2", "still not"))
        .await
        .unwrap();
    second.deliver(run, message("m-3", "ok")).await.unwrap();
    until(&second, run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    worker.await.unwrap();
    let Some(PendingWait::Question(q)) = conversation(&view).pending_wait else {
        panic!("not parked on a question")
    };
    assert_eq!(q.question, "\"still not\" was refused (\"n\"). which?");
    assert_eq!(refusals(&sink, run).len(), 2, "{:?}", refusals(&sink, run));
}

/// An answer that carries a picture gives the call's result the picture, through a step of its own
/// that keeps it: the model is sent the answer's words and, after the result, the image. A model
/// that cannot see is told in a line, and the answer stands.
#[tokio::test]
async fn an_answer_with_a_picture_gives_the_result_its_image() {
    use adam_llm_agent::{ImageStore, InboundImage, MemoryImages, user_message_with_images};
    use adam_model::{ImageLimits, ImageOrigin};
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&[0, 0, 0, 16, 0, 0, 0, 16, 8, 2, 0, 0, 0, 1, 2, 3]);
    for sees in [true, false] {
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(if sees {
            MockModel::new().seeing_images(ImageLimits {
                max_bytes: 1 << 20,
                max_count: 8,
                max_side: 4096,
                sources: [ImageOrigin::Person].into(),
            })
        } else {
            MockModel::new()
        });
        mock.push_tool_calls(vec![call("c1", "pick", json!({"note": "n"}))])
            .push_text("thanks");
        let model: DynModel = mock.clone();
        let agent = LlmAgent::builder("llm", model, "test-model")
            .state(Arc::new(ImageStore(Arc::new(MemoryImages::new()))))
            .tool(Pick)
            .build();
        let sink = CollectingSink::new();
        let rt = Runtime::builder(store)
            .agent(agent)
            .event_sink(sink)
            .poll_interval(Duration::from_millis(20))
            .build();
        let run = rt.start("llm", user_message("go"), None).await.unwrap();
        let (stop, worker) = spawn_worker(&rt);
        until(&rt, run, "parked", |v| v.waiting).await;
        rt.deliver(
            run,
            user_message_with_images(
                "ok",
                &[InboundImage::new(
                    Some("proof.png"),
                    Some("image/png"),
                    &png,
                )],
            ),
        )
        .await
        .unwrap();
        let done = until(&rt, run, "done", |v| v.status == RunStatus::Done).await;
        let _ = stop.send(());
        worker.await.unwrap();
        let state = conversation(&done);
        let result = state
            .messages
            .iter()
            .find(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "c1"))
            .unwrap();
        if sees {
            assert_eq!(result.text(), "ok");
            assert_eq!(result.images().len(), 1);
            let last = mock.requests().pop().unwrap();
            assert_eq!(last.image_data.len(), 1);
        } else {
            assert!(result.images().is_empty());
            assert!(
                result
                    .text()
                    .starts_with("ok\n[image not shown: this model cannot see images"),
                "{}",
                result.text()
            );
        }
    }
}
