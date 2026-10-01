//! A model's words over A2A (`text-stream/v1`): what a client that activated the extension reads (chunks
//! while the model writes, the whole text said once under the stream's id) and what one that did not
//! reads (the whole reply at the end, as always), over a real `Runtime` on the in-memory store running a
//! real `LlmAgent` with a scripted model, and over HTTP through the server.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, TaskState};
use adam_a2a::{
    A2aServer, AgentCardConfig, AuthConfig, BackendError, Caller, ExtensionConfig,
    TEXT_STREAM_EXTENSION, TaskBackend, TaskEvent,
};
use adam_a2a_runtime::RuntimeTaskBackend;
use adam_core::MemoryStore;
use adam_llm_agent::LlmAgent;
use adam_model::{
    ContentPart, DynModel, FinishReason, Message as ModelMessage, ModelClient, ModelDelta,
    ModelError, ModelRequest, ModelResponse, ToolCall, Usage,
};
use adam_runtime::{BroadcastSink, Runtime};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// What a model does in one call: deltas, each after a pause (a model that takes time to write), then
/// the end (the assembled response or a failure, which the last item is).
type Script = Vec<(Duration, Result<ModelDelta, ModelError>)>;

/// A model whose every call is the next script.
struct Dribbling {
    script: Mutex<VecDeque<Script>>,
}

#[async_trait]
impl ModelClient for Dribbling {
    async fn complete(&self, _: ModelRequest) -> Result<ModelResponse, ModelError> {
        let mut items = self.script.lock().unwrap().pop_front().expect("a script");
        match items.pop().expect("a non-empty script").1 {
            Ok(ModelDelta::Finished(response)) => Ok(response),
            Err(error) => Err(error),
            Ok(other) => panic!("a script ends with Finished or an error, not {other:?}"),
        }
    }

    async fn stream(
        &self,
        _: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let items = self.script.lock().unwrap().pop_front().expect("a script");
        Ok(stream::iter(items)
            .then(|(pause, item)| async move {
                tokio::time::sleep(pause).await;
                item
            })
            .boxed())
    }
}

const PAUSE: Duration = Duration::from_millis(130);

/// The words `pieces`, one every [`PAUSE`], then the assembled answer.
fn writes(pieces: &[&str]) -> Script {
    let whole: String = pieces.concat();
    pieces
        .iter()
        .map(|p| (PAUSE, Ok(ModelDelta::Text((*p).to_owned()))))
        .chain([(
            Duration::ZERO,
            Ok(ModelDelta::Finished(ModelResponse::text(whole))),
        )])
        .collect()
}

struct Rig {
    runtime: Runtime,
    backend: RuntimeTaskBackend,
}

fn rig(
    script: Vec<Script>,
    build: impl FnOnce(adam_llm_agent::LlmAgentBuilder) -> LlmAgent,
) -> Rig {
    let model: DynModel = Arc::new(Dribbling {
        script: Mutex::new(script.into()),
    });
    let agent = build(LlmAgent::builder("llm", model, "m"));
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(Arc::new(MemoryStore::new()))
        .agent(agent)
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let backend = RuntimeTaskBackend::new(runtime.clone(), events, "llm")
        .with_poll_interval(Duration::from_millis(10));
    Rig { runtime, backend }
}

fn start(script: Vec<Script>) -> Rig {
    rig(script, |b| b.build())
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Rig {
    fn worker(&self) -> Worker {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.runtime.clone();
        let handle = tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        Worker { stop, handle }
    }
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        self.handle.await.unwrap();
    }
}

fn user(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

/// What a client reads of a task, in order, until the stream ends.
struct Read {
    /// The chunks: `(artifact id, text, offset, append, last chunk, abandoned)`.
    chunks: Vec<Chunk>,
    /// The status updates, in order: `(state, message)`, with the index in `order` of each.
    statuses: Vec<(TaskState, Option<Message>)>,
    /// `"chunk"` or `"status"`, in the order the events came.
    order: Vec<&'static str>,
}

#[derive(Debug)]
struct Chunk {
    id: String,
    text: String,
    offset: Value,
    append: Option<bool>,
    last: Option<bool>,
    abandoned: Option<bool>,
}

async fn read(rig: &Rig, caller: Caller, text: &str) -> Read {
    let task = rig
        .backend
        .submit(caller.clone(), user(text), None, None)
        .await
        .expect("submit");
    let mut stream: BoxStream<'static, Result<TaskEvent, BackendError>> =
        rig.backend.subscribe(&caller, &task.id);
    // The snapshot first: the subscription is attached before anything is stepped.
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a snapshot in time")
            .expect("an event")
            .expect("not an error"),
        TaskEvent::Snapshot(_)
    ));
    let worker = rig.worker();
    let mut read = Read {
        chunks: Vec::new(),
        statuses: Vec::new(),
        order: Vec::new(),
    };
    loop {
        match tokio::time::timeout(Duration::from_secs(20), stream.next()).await {
            Ok(Some(item)) => match item.expect("not an error") {
                TaskEvent::Status(update) => {
                    read.order.push("status");
                    read.statuses
                        .push((update.status.state, update.status.message));
                }
                TaskEvent::Artifact(update) => {
                    read.order.push("chunk");
                    let artifact = update.artifact;
                    let entry = artifact
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get(TEXT_STREAM_EXTENSION))
                        .expect("a chunk names its offset under the extension")
                        .clone();
                    let text = match &artifact.parts[0].content {
                        PartContent::Text(t) => t.clone(),
                        other => panic!("a chunk is text: {other:?}"),
                    };
                    assert_eq!(artifact.parts.len(), 1);
                    assert_eq!(artifact.name.as_deref(), Some("reply"));
                    assert_eq!(
                        artifact.extensions,
                        Some(vec![TEXT_STREAM_EXTENSION.to_owned()])
                    );
                    read.chunks.push(Chunk {
                        id: artifact.artifact_id,
                        text,
                        offset: entry["offset"].clone(),
                        append: update.append,
                        last: update.last_chunk,
                        abandoned: entry.get("abandoned").and_then(Value::as_bool),
                    });
                }
                other => panic!("unexpected {other:?}"),
            },
            Ok(None) => break,
            Err(_) => panic!("timed out; read so far: {:?}", read.chunks),
        }
    }
    worker.stop().await;
    read
}

fn activated() -> Caller {
    Caller::new("token-0").with_extensions([TEXT_STREAM_EXTENSION])
}

fn marker(message: &Message) -> Option<&Value> {
    message.metadata.as_ref()?.get(TEXT_STREAM_EXTENSION)
}

fn text_of(message: &Message) -> &str {
    message.text().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The client that activated the extension
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_activated_client_reads_chunks_while_the_model_writes_then_the_whole_text_under_the_streams_id()
 {
    let rig = start(vec![writes(&["Fib", "onacci ", "in ", "Rust."])]);
    let read = read(&rig, activated(), "fibonacci?").await;

    // At least two chunks, all before the status that ends the turn, whose text they add up to.
    assert!(read.chunks.len() >= 2, "{:?}", read.chunks);
    let last_status = read.order.iter().rposition(|k| *k == "status").unwrap();
    let last_chunk = read.order.iter().rposition(|k| *k == "chunk").unwrap();
    assert!(
        last_chunk < last_status,
        "chunks before the end of the turn: {:?}",
        read.order
    );
    let (state, message) = read.statuses.last().unwrap();
    assert_eq!(*state, TaskState::Completed);
    let message = message.as_ref().expect("the answer");
    assert_eq!(text_of(message), "Fibonacci in Rust.");

    // One stream; each chunk begins where the one before ended (whole numbers, bytes), only the first
    // is not an append and only the last ends it, with nothing abandoned.
    let stream = &read.chunks[0].id;
    let mut text = String::new();
    for (n, chunk) in read.chunks.iter().enumerate() {
        assert_eq!(&chunk.id, stream);
        assert_eq!(chunk.offset, json!(text.len()), "chunk {n}");
        assert_eq!(chunk.append, Some(n > 0), "chunk {n}");
        assert_eq!(chunk.last, Some(n + 1 == read.chunks.len()), "chunk {n}");
        assert_eq!(chunk.abandoned, None);
        text.push_str(&chunk.text);
    }
    assert_eq!(text, "Fibonacci in Rust.");
    // The turn's status says the whole text is that stream's.
    assert_eq!(marker(message), Some(&json!({"streamId": stream})));
}

#[tokio::test]
async fn an_answer_that_ends_a_turn_is_stated_once_and_not_as_a_status_of_its_own_before() {
    let rig = start(vec![writes(&["Hello ", "there."])]);
    let read = read(&rig, activated(), "hi").await;
    // The only message that carries the words is the one that ends the turn (no `working` status states
    // them: that is for words before a tool call).
    let marked: Vec<&Message> = read
        .statuses
        .iter()
        .filter_map(|(_, m)| m.as_ref())
        .filter(|m| marker(m).is_some())
        .collect();
    assert_eq!(marked.len(), 1);
    assert_eq!(text_of(marked[0]), "Hello there.");
    assert_eq!(read.statuses.last().unwrap().0, TaskState::Completed);
}

#[tokio::test]
async fn the_words_before_a_tool_call_are_stated_as_a_working_status_with_the_streams_id() {
    use adam_llm_agent::{Tool, ToolCtx, ToolError, ToolOutput};
    struct Look;
    #[async_trait]
    impl Tool for Look {
        fn spec(&self) -> adam_model::ToolSpec {
            adam_model::ToolSpec {
                name: "look".into(),
                description: "look".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }
        }
        async fn call(&self, _: &ToolCtx, _: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("a file"))
        }
    }
    let first = ModelResponse {
        message: ModelMessage::Assistant {
            content: vec![ContentPart::text("Let me look.")],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "look".into(),
                arguments: json!({}),
            }],
        },
        finish: FinishReason::ToolCalls,
        usage: Usage::default(),
    };
    let script = vec![
        vec![
            (PAUSE, Ok(ModelDelta::Text("Let me look.".into()))),
            (Duration::ZERO, Ok(ModelDelta::Finished(first))),
        ],
        writes(&["It is a file."]),
    ];
    let rig = rig(script, |b| b.tool(Look).build());
    let read = read(&rig, activated(), "what is here?").await;

    // Two streams, the words before the call and the answer, each its own chunks.
    let ids: Vec<&String> = read
        .chunks
        .iter()
        .map(|c| &c.id)
        .fold(Vec::new(), |mut ids, id| {
            if !ids.contains(&id) {
                ids.push(id);
            }
            ids
        });
    assert_eq!(ids.len(), 2, "{:?}", read.chunks);
    // The words before the call are said whole by a `working` status that names the stream, and whose
    // message id is the stream's.
    let said: Vec<&Message> = read
        .statuses
        .iter()
        .filter(|(state, _)| *state == TaskState::Working)
        .filter_map(|(_, m)| m.as_ref())
        .filter(|m| marker(m).is_some())
        .collect();
    assert_eq!(said.len(), 1, "{:?}", read.statuses);
    assert_eq!(text_of(said[0]), "Let me look.");
    assert_eq!(said[0].message_id, *ids[0]);
    assert_eq!(marker(said[0]), Some(&json!({"streamId": ids[0]})));
    // The answer is the one the turn ends with.
    let (state, answer) = read.statuses.last().unwrap();
    assert_eq!(*state, TaskState::Completed);
    let answer = answer.as_ref().unwrap();
    assert_eq!(text_of(answer), "It is a file.");
    assert_eq!(marker(answer), Some(&json!({"streamId": ids[1]})));
}

#[tokio::test]
async fn a_model_that_fails_in_the_middle_abandons_the_stream_and_fails_the_task_as_ever() {
    let broken = || {
        vec![vec![
            (PAUSE, Ok(ModelDelta::Text("Fib".into()))),
            (PAUSE, Ok(ModelDelta::Text("onacci".into()))),
            (
                Duration::ZERO,
                Err(ModelError::invalid_request("the provider said no")),
            ),
        ]]
    };
    let streamed = read(&start(broken()), activated(), "fibonacci?").await;
    let plain = read(&start(broken()), Caller::new("token-0"), "fibonacci?").await;

    // The outcome is the same either way.
    let (state, message) = streamed.statuses.last().unwrap();
    assert_eq!(*state, TaskState::Failed);
    assert_eq!(
        text_of(message.as_ref().unwrap()),
        "model call failed: invalid request: the provider said no"
    );
    let (plain_state, plain_message) = plain.statuses.last().unwrap();
    assert_eq!(plain_state, state);
    assert_eq!(
        plain_message.as_ref().map(text_of),
        message.as_ref().map(text_of)
    );

    // What was written reached the activated client, and the stream ends abandoned.
    let text: String = streamed.chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(text, "Fibonacci");
    let last = streamed.chunks.last().unwrap();
    assert_eq!((last.last, last.abandoned), (Some(true), Some(true)));
    assert!(
        streamed.chunks[..streamed.chunks.len() - 1]
            .iter()
            .all(|c| c.abandoned.is_none())
    );
    // No words are stated whole for a stream that was abandoned.
    assert!(
        streamed
            .statuses
            .iter()
            .filter_map(|(_, m)| m.as_ref())
            .all(|m| marker(m).is_none())
    );
    assert!(plain.chunks.is_empty());
}

// ---------------------------------------------------------------------------
// The client that did not
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_client_that_did_not_activate_the_extension_reads_the_whole_reply_at_the_end_as_ever() {
    let rig = start(vec![writes(&["Fib", "onacci ", "in ", "Rust."])]);
    let read = read(&rig, Caller::new("token-0"), "fibonacci?").await;
    assert!(
        read.chunks.is_empty(),
        "no chunk for a client that did not ask: {:?}",
        read.chunks
    );
    // The reply is the status that ends the turn, whole.
    let (state, message) = read.statuses.last().unwrap();
    assert_eq!(*state, TaskState::Completed);
    let message = message.as_ref().expect("the answer");
    assert_eq!(text_of(message), "Fibonacci in Rust.");
    assert_eq!(message.parts.len(), 1);
    // The words were not stated as a status of their own either.
    assert!(
        read.statuses[..read.statuses.len() - 1]
            .iter()
            .filter_map(|(_, m)| m.as_ref())
            .all(|m| marker(m).is_none())
    );
}

#[tokio::test]
async fn the_result_of_a_blocking_send_is_the_task_it_always_was() {
    // `message/send` is a submit and polls of the task: no chunk is in the task, and the answer is the
    // status message. (The marker on it is data under the extension's own key.)
    let rig = start(vec![writes(&["Fib", "onacci ", "in ", "Rust."])]);
    let caller = activated();
    let task = rig
        .backend
        .submit(caller.clone(), user("fibonacci?"), None, None)
        .await
        .unwrap();
    let worker = rig.worker();
    let done = loop {
        let task = rig.backend.get(&caller, &task.id).await.unwrap().unwrap();
        if task.status.state == TaskState::Completed {
            break task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    worker.stop().await;
    assert_eq!(
        done.status.message.as_ref().map(text_of),
        Some("Fibonacci in Rust.")
    );
    assert!(
        done.artifacts.is_none(),
        "chunks are never in the task: {:?}",
        done.artifacts
    );
    assert!(done.history.is_none());
    // The completed status says which stream the text was: what a poll reads, as a stream does.
    let streamed = done
        .status
        .message
        .as_ref()
        .and_then(marker)
        .and_then(|m| m["streamId"].as_str())
        .expect("a stream id");
    assert!(streamed.contains("-m0-"), "{streamed}");
}

// ---------------------------------------------------------------------------
// Over HTTP, through the server and the SDK
// ---------------------------------------------------------------------------

/// The `data:` events of a `SendStreamingMessage` answered by `rig`'s server, named or not naming the
/// extension in the header.
async fn over_http(rig: &Rig, header: Option<&str>) -> (String, Vec<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let card = AgentCardConfig::new(
        "llm",
        "Streams its replies",
        format!("http://{addr}/").parse().unwrap(),
        "0.1.0",
    )
    .with_extension(ExtensionConfig::text_stream());
    let app = A2aServer::router(
        card,
        Arc::new(rig.backend.clone()),
        AuthConfig::AllowAnonymous,
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let body = json!({"jsonrpc": "2.0", "id": "1", "method": "SendStreamingMessage",
        "params": {"message": {"messageId": "m-http", "role": "ROLE_USER",
                               "parts": [{"text": "fibonacci?"}]}}})
    .to_string();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(header) = header {
        request.push_str(&format!("A2A-Extensions: {header}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);
    stream.write_all(request.as_bytes()).await.unwrap();
    let worker = rig.worker();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut bytes))
        .await
        .expect("the stream ends in time")
        .unwrap();
    worker.stop().await;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a response");
    let events = body
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| serde_json::from_str(d.trim()).expect("JSON"))
        .collect();
    (head.to_owned(), events)
}

#[tokio::test]
async fn over_http_the_header_activates_the_stream_and_the_chunks_carry_their_offsets() {
    let rig = start(vec![writes(&["Fib", "onacci ", "in ", "Rust."])]);
    let (head, events) = over_http(&rig, Some(TEXT_STREAM_EXTENSION)).await;
    assert!(
        head.to_lowercase()
            .contains(&format!("a2a-extensions: {TEXT_STREAM_EXTENSION}").to_lowercase()),
        "the response says which extension it activated: {head}"
    );
    let updates: Vec<&Value> = events
        .iter()
        .filter_map(|e| e["result"].get("artifactUpdate"))
        .collect();
    assert!(updates.len() >= 2, "{events:#?}");
    // On the wire the SDK writes a number of metadata as a float, whatever it was: a whole number,
    // which the orchestration layer reads as the integer it is.
    let mut offset = 0u64;
    let mut text = String::new();
    for update in &updates {
        let entry = &update["artifact"]["metadata"][TEXT_STREAM_EXTENSION];
        let wire = entry["offset"].as_f64().expect("a number");
        assert_eq!(wire.fract(), 0.0);
        assert_eq!(wire as u64, offset, "{update}");
        let piece = update["artifact"]["parts"][0]["text"]
            .as_str()
            .expect("text");
        offset += piece.len() as u64;
        text.push_str(piece);
        assert_eq!(update["artifact"]["name"], "reply");
        assert_eq!(
            update["artifact"]["extensions"],
            json!([TEXT_STREAM_EXTENSION])
        );
    }
    assert_eq!(text, "Fibonacci in Rust.");
    assert_eq!(updates.last().unwrap()["lastChunk"], true);
    let done = events
        .iter()
        .filter_map(|e| e["result"].get("statusUpdate"))
        .next_back()
        .expect("a status");
    assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(
        done["status"]["message"]["metadata"][TEXT_STREAM_EXTENSION]["streamId"],
        updates[0]["artifact"]["artifactId"]
    );
}

#[tokio::test]
async fn over_http_without_the_header_there_is_no_chunk() {
    let rig = start(vec![writes(&["Fib", "onacci ", "in ", "Rust."])]);
    let (head, events) = over_http(&rig, None).await;
    assert!(!head.to_lowercase().contains("a2a-extensions"), "{head}");
    assert!(
        events
            .iter()
            .all(|e| e["result"].get("artifactUpdate").is_none())
    );
    let done = events
        .iter()
        .filter_map(|e| e["result"].get("statusUpdate"))
        .next_back()
        .expect("a status");
    assert_eq!(
        done["status"]["message"]["parts"][0]["text"],
        "Fibonacci in Rust."
    );
}

// ---------------------------------------------------------------------------
// A reply that was turned into a question
// ---------------------------------------------------------------------------

/// Parks on a question that is the words it streamed (what the coder does with a reply that delivers
/// nothing): the question names the stream.
struct AsksWhatItSaid;

#[async_trait]
impl adam_runtime::Agent for AsksWhatItSaid {
    type State = Value;

    fn name(&self) -> &str {
        "asks"
    }

    fn init(&self, _: adam_runtime::Inbound) -> Result<Value, adam_runtime::AgentError> {
        Ok(json!({}))
    }

    async fn step(
        &self,
        ctx: &mut adam_runtime::Ctx,
        _state: Value,
    ) -> Result<adam_runtime::Transition<Value>, adam_runtime::AgentError> {
        for (offset, piece, last) in [
            (0, "Hi! I'm Coder. ", false),
            (15, "Which repository?", true),
        ] {
            ctx.emit(adam_runtime::RunEvent::TextDelta {
                stream: "s-ask".into(),
                offset,
                text: piece.into(),
                last,
                abandoned: false,
            })
            .await;
        }
        Ok(adam_runtime::Transition::Park {
            state: json!({"pending_wait": {"call_id": "stop00001", "tool": "ask_user",
                "question": "Hi! I'm Coder. Which repository?", "stream": "s-ask"}}),
            wake_at: None,
        })
    }
}

#[tokio::test]
async fn a_question_that_is_the_streamed_reply_is_stated_under_its_stream() {
    let events = BroadcastSink::default();
    let runtime = Runtime::builder(Arc::new(MemoryStore::new()))
        .agent(AsksWhatItSaid)
        .event_sink(events.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let rig = Rig {
        backend: RuntimeTaskBackend::new(runtime.clone(), events, "asks")
            .with_poll_interval(Duration::from_millis(10)),
        runtime,
    };
    let read = read(&rig, activated(), "hi").await;

    // The pieces, then the status that ends the turn: the question, which names the stream.
    let text: String = read.chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(text, "Hi! I'm Coder. Which repository?");
    let (state, message) = read.statuses.last().unwrap();
    assert_eq!(*state, TaskState::InputRequired);
    let message = message.as_ref().expect("the question");
    assert_eq!(text_of(message), "Hi! I'm Coder. Which repository?");
    assert_eq!(marker(message), Some(&json!({"streamId": "s-ask"})));
    assert!(read.chunks.iter().all(|c| c.id == "s-ask"));
}
