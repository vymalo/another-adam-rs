//! The model's words as they are written: a model turn that streams sends them as `TextDelta` events,
//! says which stream they were in `agent_text` and in the output, fails exactly as a call that does
//! not stream, and replays from the journal without calling the model or sending a piece. A real
//! `Runtime` on the in-memory store, a scripted model.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    LlmAgent, LlmAgentBuilder, Tool, ToolCtx, ToolError, ToolOutput, user_message,
};
use adam_model::{
    ContentPart, DynModel, FinishReason, Message, MockModel, ModelClient, ModelDelta, ModelError,
    ModelRequest, ModelResponse, ToolCall, ToolSpec, Usage,
};
use adam_runtime::{
    AGENT_TEXT_KIND, CollectingSink, MAX_STREAM_ID_BYTES, MAX_TEXT_DELTA_BYTES, RetryPolicy,
    RunEvent, RunView, Runtime, RuntimeBuilder,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A model whose every call is the next scripted stream (`stream` pops one; `complete` answers with
/// the assembled response of it, which is what a model that does not stream would give).
struct ScriptedStreams {
    script: Mutex<VecDeque<Vec<Result<ModelDelta, ModelError>>>>,
    calls: Mutex<Vec<bool>>,
}

impl ScriptedStreams {
    fn new(script: Vec<Vec<Result<ModelDelta, ModelError>>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn pop(&self, streaming: bool) -> Vec<Result<ModelDelta, ModelError>> {
        self.calls.lock().unwrap().push(streaming);
        self.script.lock().unwrap().pop_front().expect("a script")
    }
}

#[async_trait]
impl ModelClient for ScriptedStreams {
    async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, ModelError> {
        let mut items = self.pop(false);
        // What it would have answered, or why it failed.
        match items.pop().expect("a non-empty script") {
            Ok(ModelDelta::Finished(response)) => Ok(response),
            Err(error) => Err(error),
            Ok(other) => panic!("a script ends with Finished or an error, not {other:?}"),
        }
    }

    async fn stream(
        &self,
        _: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        Ok(stream::iter(self.pop(true)).boxed())
    }
}

fn texts(pieces: &[&str]) -> Vec<Result<ModelDelta, ModelError>> {
    let whole: String = pieces.concat();
    pieces
        .iter()
        .map(|p| Ok(ModelDelta::Text((*p).to_owned())))
        .chain([Ok(ModelDelta::Finished(ModelResponse::text(whole)))])
        .collect()
}

struct Harness {
    store: DynStore,
    sink: CollectingSink,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            sink: CollectingSink::new(),
        }
    }

    fn agent(model: DynModel) -> LlmAgentBuilder {
        LlmAgent::builder("llm", model, "test-model")
    }

    fn runtime(
        &self,
        agent: LlmAgent,
        f: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Runtime {
        f(Runtime::builder(self.store.clone())
            .agent(agent)
            .event_sink(self.sink.clone())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10)))
        .build()
    }

    /// The events of the run, the runtime's own status events left out.
    fn events(&self, run: RunId) -> Vec<RunEvent> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter(|e| !matches!(e, RunEvent::Status { .. }))
            .collect()
    }

    /// Runs `agent` on one message to the end of the run, whichever it is.
    async fn run(&self, agent: LlmAgent, retry: bool, message: &str) -> (RunId, RunView) {
        let rt = self.runtime(agent, |b| {
            if retry {
                b.retry(RetryPolicy {
                    max_attempts: 3,
                    initial_backoff: Duration::from_millis(10),
                    max_backoff: Duration::from_millis(10),
                    multiplier: 1.0,
                })
            } else {
                b
            }
        });
        let run = rt
            .start("llm", user_message(message), None)
            .await
            .expect("start");
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
        let deadline = Instant::now() + Duration::from_secs(20);
        let view = loop {
            let view = rt.view(run).await.expect("view").expect("the run exists");
            if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
                break view;
            }
            assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = stop.send(());
        worker.await.expect("worker task").expect("worker result");
        (run, view)
    }
}

/// One piece of streamed text: `(stream, offset, text, last, abandoned)`.
type Piece = (String, u64, String, bool, bool);

fn pieces(events: &[RunEvent]) -> Vec<Piece> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::TextDelta {
                stream,
                offset,
                text,
                last,
                abandoned,
            } => Some((stream.clone(), *offset, text.clone(), *last, *abandoned)),
            _ => None,
        })
        .collect()
}

/// The `agent_text` events: `(text, turn, stream)`.
fn words(events: &[RunEvent]) -> Vec<(String, u64, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Custom { kind, payload } if kind == AGENT_TEXT_KIND => Some((
                payload["text"].as_str().expect("text").to_owned(),
                payload["turn"].as_u64().expect("turn"),
                payload
                    .get("stream")
                    .map(|s| s.as_str().expect("a string").to_owned()),
            )),
            _ => None,
        })
        .collect()
}

/// What the pieces of one stream add up to, checking that they follow each other: each begins where
/// the one before ended, in bytes, and only the last says it is.
fn joined(pieces: &[Piece]) -> String {
    let mut text = String::new();
    for (n, (_, offset, piece, last, _)) in pieces.iter().enumerate() {
        assert_eq!(
            *offset,
            text.len() as u64,
            "piece {n} begins where the one before ended"
        );
        assert!(piece.len() <= MAX_TEXT_DELTA_BYTES);
        assert_eq!(
            *last,
            n + 1 == pieces.len(),
            "only the last piece says it is"
        );
        text.push_str(piece);
    }
    text
}

fn well_formed_id(stream: &str, run: RunId, turn: u32) {
    assert!(
        !stream.is_empty() && stream.len() <= MAX_STREAM_ID_BYTES,
        "{stream}"
    );
    assert!(!stream.chars().any(char::is_control), "{stream}");
    let prefix = format!("{run}-m{turn}-");
    let suffix = stream
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("{stream} is {prefix}<hex>"));
    assert_eq!(suffix.len(), 8, "{stream}");
    assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()), "{stream}");
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

struct Look;

#[async_trait]
impl Tool for Look {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "look".into(),
            description: "look".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("a file"))
    }
}

// ---------------------------------------------------------------------------
// What is sent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_answer_is_sent_in_pieces_that_add_up_to_it_and_its_stream_is_in_the_output() {
    let h = Harness::new();
    let model = ScriptedStreams::new(vec![texts(&["Fib", "onacci ", "in ", "Rust."])]);
    let (run, view) = h
        .run(Harness::agent(model.clone()).build(), false, "fibonacci?")
        .await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(*model.calls.lock().unwrap(), [true], "one call, a stream");

    let events = h.events(run);
    let sent = pieces(&events);
    assert!(sent.len() >= 2, "more than one piece: {sent:?}");
    assert_eq!(joined(&sent), "Fibonacci in Rust.");
    let stream = sent[0].0.clone();
    assert!(
        sent.iter().all(|p| p.0 == stream && !p.4),
        "one stream, not abandoned"
    );
    well_formed_id(&stream, run, 0);

    // The answer that ends the run is said by the run: its output names the stream. The words of the
    // turn are an event too, with no stream: nothing more is to be said of them.
    assert_eq!(words(&events), [("Fibonacci in Rust.".to_owned(), 0, None)]);
    assert_eq!(
        view.output,
        Some(json!({"text": "Fibonacci in Rust.", "artifacts": [], "stream": stream}))
    );
    // The pieces came first: a client that read them has the text before it is said whole.
    let position = |f: &dyn Fn(&RunEvent) -> bool| events.iter().position(f).expect("an event");
    assert!(
        position(&|e| matches!(e, RunEvent::TextDelta { last: true, .. }))
            < position(&|e| matches!(e, RunEvent::Custom { .. }))
    );
}

#[tokio::test]
async fn offsets_are_utf8_bytes_and_a_long_answer_is_cut_into_pieces_of_a_bounded_size() {
    let h = Harness::new();
    // One delta of 3000 bytes of two-byte characters (a model that sends all at once), then more.
    let long = "é".repeat(1500);
    let model = ScriptedStreams::new(vec![texts(&[&long, " 🙂 done"])]);
    let (run, view) = h
        .run(Harness::agent(model).build(), false, "write a lot")
        .await;
    assert_eq!(view.status, RunStatus::Done);
    let sent = pieces(&h.events(run));
    assert!(sent.len() >= 4, "{} pieces", sent.len());
    assert_eq!(joined(&sent), format!("{long} 🙂 done"));
    // Every piece is whole characters: `String` guarantees it, and the sum of their lengths is the end.
    let total: usize = sent.iter().map(|p| p.2.len()).sum();
    assert_eq!(total, 3000 + " 🙂 done".len());
}

#[tokio::test]
async fn the_words_before_a_tool_call_are_a_stream_of_their_own_and_the_answer_another() {
    let h = Harness::new();
    let first = ModelResponse {
        message: Message::Assistant {
            content: vec![ContentPart::text("Let me look.")],
            tool_calls: vec![call("c1", "look")],
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    };
    let model = ScriptedStreams::new(vec![
        vec![
            Ok(ModelDelta::Text("Let me ".into())),
            Ok(ModelDelta::Text("look.".into())),
            Ok(ModelDelta::ToolCallStarted {
                id: "c1".into(),
                name: "look".into(),
            }),
            Ok(ModelDelta::Finished(first)),
        ],
        texts(&["It is ", "a file."]),
    ]);
    let (run, view) = h
        .run(
            Harness::agent(model).tool(Look).build(),
            false,
            "what is here?",
        )
        .await;
    assert_eq!(view.status, RunStatus::Done);

    let events = h.events(run);
    let sent = pieces(&events);
    let of = |n: usize| -> Vec<Piece> {
        sent.iter()
            .filter(|p| p.0.contains(&format!("-m{n}-")))
            .cloned()
            .collect()
    };
    let (before, after) = (of(0), of(1));
    assert_eq!(joined(&before), "Let me look.");
    assert_eq!(joined(&after), "It is a file.");
    assert_eq!(before.len() + after.len(), sent.len());
    let (s0, s1) = (before[0].0.clone(), after[0].0.clone());
    assert_ne!(s0, s1);
    well_formed_id(&s0, run, 0);
    well_formed_id(&s1, run, 1);
    assert_eq!(
        words(&events),
        [
            ("Let me look.".to_owned(), 0, Some(s0)),
            ("It is a file.".to_owned(), 1, None),
        ]
    );
    // The words before the call are said under their stream; the answer that ends the run is the run's
    // stream, in its output.
    assert_eq!(view.output.expect("output")["stream"], s1);
}

#[tokio::test]
async fn a_turn_with_no_words_opens_no_stream() {
    let h = Harness::new();
    let model = ScriptedStreams::new(vec![
        vec![Ok(ModelDelta::Finished(ModelResponse::tool_calls(vec![
            call("c1", "look"),
        ])))],
        texts(&["Done."]),
    ]);
    let (run, view) = h
        .run(Harness::agent(model).tool(Look).build(), false, "go")
        .await;
    assert_eq!(view.status, RunStatus::Done);
    let events = h.events(run);
    assert!(
        pieces(&events).iter().all(|p| p.0.contains("-m1-")),
        "only turn 1 wrote"
    );
    assert_eq!(words(&events).len(), 1);
}

#[tokio::test]
async fn a_run_that_does_not_stream_says_nothing_of_streams() {
    let h = Harness::new();
    let mock = Arc::new(MockModel::new());
    mock.push_text("Fibonacci in Rust.");
    let (run, view) = h
        .run(
            Harness::agent(mock.clone()).stream_text(false).build(),
            false,
            "fibonacci?",
        )
        .await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(
        mock.calls().iter().all(|c| !c.streaming),
        "complete, not stream"
    );
    let events = h.events(run);
    assert!(pieces(&events).is_empty());
    assert_eq!(words(&events), [("Fibonacci in Rust.".to_owned(), 0, None)]);
    assert_eq!(
        view.output,
        Some(json!({"text": "Fibonacci in Rust.", "artifacts": []}))
    );
}

// ---------------------------------------------------------------------------
// When the model fails
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_model_that_fails_in_the_middle_fails_the_run_as_a_call_that_does_not_stream_does() {
    let broken = || {
        vec![vec![
            Ok(ModelDelta::Text("Fib".into())),
            Ok(ModelDelta::Text("onacci".into())),
            Err(ModelError::invalid_request("the provider said no")),
        ]]
    };
    let streamed = Harness::new();
    let (run, view) = streamed
        .run(
            Harness::agent(ScriptedStreams::new(broken())).build(),
            false,
            "fibonacci?",
        )
        .await;
    assert_eq!(view.status, RunStatus::Failed);

    // The same failure, from a model that is not asked to stream.
    let plain = Harness::new();
    let (plain_run, plain_view) = plain
        .run(
            Harness::agent(ScriptedStreams::new(broken()))
                .stream_text(false)
                .build(),
            false,
            "fibonacci?",
        )
        .await;
    assert_eq!(plain_view.status, RunStatus::Failed);
    assert_eq!(view.error, plain_view.error, "the same failure");
    assert_eq!(
        view.error.as_deref(),
        Some("model call failed: invalid request: the provider said no")
    );

    // What was written is on the screen, and the stream ends abandoned; no words are said whole.
    let events = streamed.events(run);
    let sent = pieces(&events);
    assert_eq!(joined(&sent), "Fibonacci");
    let last = sent.last().expect("a piece");
    assert!(last.3 && last.4, "the last piece is abandoned: {last:?}");
    assert!(sent[..sent.len() - 1].iter().all(|p| !p.4));
    assert!(words(&events).is_empty());
    assert!(plain.events(plain_run).is_empty());
}

#[tokio::test]
async fn a_transient_failure_is_retried_as_another_stream_and_the_first_ends_abandoned() {
    let h = Harness::new();
    let model = ScriptedStreams::new(vec![
        vec![
            Ok(ModelDelta::Text("Fib".into())),
            Err(ModelError::transient("connection reset")),
        ],
        texts(&["Fibonacci ", "in Rust."]),
    ]);
    let (run, view) = h
        .run(Harness::agent(model).build(), true, "fibonacci?")
        .await;
    assert_eq!(view.status, RunStatus::Done);

    let events = h.events(run);
    let sent = pieces(&events);
    let streams: Vec<&str> = {
        let mut ids: Vec<&str> = Vec::new();
        for p in &sent {
            if !ids.contains(&p.0.as_str()) {
                ids.push(&p.0);
            }
        }
        ids
    };
    assert_eq!(streams.len(), 2, "the retry is another stream: {streams:?}");
    let first: Vec<Piece> = sent.iter().filter(|p| p.0 == streams[0]).cloned().collect();
    let second: Vec<Piece> = sent.iter().filter(|p| p.0 == streams[1]).cloned().collect();
    assert_eq!(joined(&second), "Fibonacci in Rust.");
    assert!(
        first.last().expect("a piece").4,
        "the first one is abandoned"
    );
    assert!(second.iter().all(|p| !p.4));
    // Both are of turn 0 (the failed try is abandoned, and the turn runs again), and the answer is the
    // second's.
    well_formed_id(streams[0], run, 0);
    well_formed_id(streams[1], run, 0);
    assert_eq!(words(&events), [("Fibonacci in Rust.".to_owned(), 0, None)]);
    assert_eq!(view.output.expect("output")["stream"], streams[1]);
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// The journal of a model step recorded `payload`: the run replays it, calling no model.
async fn replay(payload: Value) -> (Arc<MockModel>, Harness, RunId, RunView) {
    let h = Harness::new();
    let mock = Arc::new(MockModel::new());
    let agent = Harness::agent(mock.clone()).build();
    let rt = h.runtime(agent, |b| b);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    h.store
        .journal_put(run, JournalEntry::ok(0, "model:0", payload))
        .await
        .expect("seed the journal");
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
    let deadline = Instant::now() + Duration::from_secs(20);
    let view = loop {
        let view = rt.view(run).await.expect("view").expect("the run exists");
        if view.status == RunStatus::Done || view.status == RunStatus::Failed {
            break view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let _ = stop.send(());
    worker.await.expect("worker task").expect("worker result");
    (mock, h, run, view)
}

#[tokio::test]
async fn a_replayed_model_step_calls_no_model_sends_no_piece_and_says_the_same_words() {
    // What a journal written by this version holds: the response, and the stream its words were.
    let recorded = json!({
        "message": {"role": "assistant", "content": [{"type": "text", "text": "Fibonacci in Rust."}]},
        "finish": "stop",
        "usage": {"input_tokens": 3, "output_tokens": 5},
        "stream": "run-m0-a1b2c3d4"
    });
    let (mock, h, run, view) = replay(recorded).await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(mock.requests().is_empty(), "no model call: it was recorded");
    let events = h.events(run);
    assert!(
        pieces(&events).is_empty(),
        "the pieces were not recorded, and are not sent again"
    );
    assert_eq!(words(&events), [("Fibonacci in Rust.".to_owned(), 0, None)]);
    assert_eq!(
        view.output,
        Some(json!({"text": "Fibonacci in Rust.", "artifacts": [], "stream": "run-m0-a1b2c3d4"}))
    );
}

/// A journal written before words could be streamed holds the bare response, which still reads, as
/// one with no stream.
#[tokio::test]
async fn a_journal_written_before_streaming_is_replayed_with_no_stream() {
    let old = json!({
        "message": {"role": "assistant", "content": [{"type": "text", "text": "Fibonacci in Rust."}]},
        "finish": "stop",
        "usage": {"input_tokens": 3, "output_tokens": 5}
    });
    let (mock, h, run, view) = replay(old).await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(mock.requests().is_empty());
    let events = h.events(run);
    assert!(pieces(&events).is_empty());
    assert_eq!(words(&events), [("Fibonacci in Rust.".to_owned(), 0, None)]);
    assert_eq!(
        view.output,
        Some(json!({"text": "Fibonacci in Rust.", "artifacts": []}))
    );
}
