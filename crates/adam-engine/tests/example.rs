//! The example embedder (`examples/chat`) driven as a person would: lines typed one at a time,
//! each once the transcript shows what the last one did, over `MemoryStore` and a scripted model.
//! Every wait is bounded, so a hang fails.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

#[path = "../examples/chat/app.rs"]
mod app;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_engine::adam_agent_fs::{Dir, Strictness};
use adam_engine::adam_assembly::AgentDef;
use adam_engine::adam_core::MemoryStore;
use adam_engine::adam_model::{
    DynModel, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse, ToolCall,
};
use adam_engine::{Engine, ListQuery, RunState};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(20);

/// The example's own agent folder.
fn chat() -> AgentDef {
    let dir = Dir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/chat"));
    AgentDef::from_source(&dir, Strictness::Strict)
        .unwrap()
        .remove(0)
}

/// Where the scripted model stops, on its `call`-th call (0-based).
#[derive(Clone, Copy)]
enum Hold {
    /// Before it answers.
    Start,
    /// After the first half of its text: the reader has seen words of an answer not yet written.
    MidText,
}

/// A point the model stops at: it says it got there, and waits to be let through.
#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

/// A `MockModel` that stops once, at `hold` of its `call`-th call.
struct Held {
    inner: Arc<MockModel>,
    call: usize,
    hold: Hold,
    calls: Mutex<usize>,
    gate: Arc<Gate>,
}

impl Held {
    fn this_one(&self) -> bool {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        *calls - 1 == self.call
    }
}

#[async_trait]
impl ModelClient for Held {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        unreachable!("the agent streams: {req:?}")
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let held = self.this_one();
        if held && matches!(self.hold, Hold::Start) {
            self.gate.reached.notify_one();
            self.gate.release.notified().await;
        }
        let mut deltas: Vec<_> = self.inner.stream(req).await?.collect().await;
        if !(held && matches!(self.hold, Hold::MidText)) {
            return Ok(futures::stream::iter(deltas).boxed());
        }
        // The first text in two, the gate between the halves.
        let at = deltas
            .iter()
            .position(|d| matches!(d, Ok(ModelDelta::Text(_))))
            .expect("the held call writes text");
        let Ok(ModelDelta::Text(text)) = deltas.remove(at) else {
            unreachable!()
        };
        let (first, second) = text.split_at(text.len() / 2);
        let mut after = deltas.split_off(at);
        after.insert(0, Ok(ModelDelta::Text(second.to_owned())));
        deltas.push(Ok(ModelDelta::Text(first.to_owned())));
        let gate = self.gate.clone();
        let rest = futures::stream::once(async move {
            gate.reached.notify_one();
            gate.release.notified().await;
            futures::stream::iter(after)
        })
        .flatten();
        Ok(futures::stream::iter(deltas).chain(rest).boxed())
    }
}

/// What the example wrote, shared with the test.
#[derive(Clone, Default)]
struct Transcript(Arc<Mutex<Vec<u8>>>);

impl Write for Transcript {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Transcript {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// Once the transcript satisfies `done`.
    async fn wait(&self, what: &str, done: impl Fn(&str) -> bool) {
        let waited = timeout(WAIT, async {
            while !done(&self.text()) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "never saw {what}; the transcript:\n{}",
            self.text()
        );
    }

    /// Once `needle` is in the transcript `n` times.
    async fn wait_count(&self, needle: &str, n: usize) {
        self.wait(&format!("{needle:?} {n} times"), |t| {
            t.matches(needle).count() >= n
        })
        .await;
    }
}

/// The example running over a fresh `MemoryStore`, its keyboard and its screen.
struct Session {
    mock: Arc<MockModel>,
    gate: Arc<Gate>,
    engine: Engine,
    keys: DuplexStream,
    screen: Transcript,
    app: JoinHandle<std::io::Result<()>>,
}

impl Session {
    async fn new(call: usize, hold: Hold) -> Self {
        let mock = Arc::new(MockModel::new());
        let gate = Arc::new(Gate::default());
        let model: DynModel = Arc::new(Held {
            inner: mock.clone(),
            call,
            hold,
            calls: Mutex::new(0),
            gate: gate.clone(),
        });
        let engine = Engine::builder(Arc::new(MemoryStore::new()), chat(), model, "m")
            .start()
            .await
            .expect("the example's agent starts");
        let (keys, input) = tokio::io::duplex(4096);
        let screen = Transcript::default();
        let app = tokio::spawn(app::run(
            engine.clone(),
            BufReader::new(input),
            screen.clone(),
        ));
        Self {
            mock,
            gate,
            engine,
            keys,
            screen,
            app,
        }
    }

    async fn type_line(&mut self, line: &str) {
        self.keys
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    /// `/quit`, and the example returns.
    async fn quit(mut self) -> (Transcript, Engine, Arc<MockModel>) {
        self.type_line("/quit").await;
        self.returns().await
    }

    /// The example returns by itself.
    async fn returns(self) -> (Transcript, Engine, Arc<MockModel>) {
        timeout(WAIT, self.app)
            .await
            .expect("the example returns in time")
            .unwrap()
            .expect("the example ends cleanly");
        (self.screen, self.engine, self.mock)
    }
}

fn ask(question: &str) -> ToolCall {
    ToolCall {
        id: "ask-1".into(),
        name: "ask_user".into(),
        arguments: json!({ "question": question }),
        ..Default::default()
    }
}

/// A send, the reply printed while the model still writes it, a question answered with
/// `/answer`, `/list` and `/quit`: the whole transcript, line for line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_example_chats_streams_and_answers_a_question() {
    let mut s = Session::new(0, Hold::MidText).await;
    s.mock
        .push_text("Hello! What shall we talk about?")
        .push_tool_calls(vec![ask("Which colour?")])
        .push_text("blue it is");

    s.type_line("hi").await;
    timeout(WAIT, s.gate.reached.notified())
        .await
        .expect("the model writes half its answer");
    // The first half is on the screen while the model has not written the rest.
    s.screen.wait_count("Hello! What", 1).await;
    assert!(!s.screen.text().contains("[done]"), "{}", s.screen.text());
    s.gate.release.notify_one();
    s.screen.wait_count("[done]", 1).await;

    s.type_line("pick a colour for me").await;
    s.screen.wait_count("? Which colour?", 1).await;
    s.type_line("/answer blue").await;
    s.screen.wait_count("[done]", 2).await;
    s.type_line("/list").await;
    s.screen.wait_count("run(s), the latest first", 1).await;
    let (screen, engine, mock) = s.quit().await;

    let runs = engine.list(ListQuery::new()).await.unwrap().items;
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r.state == RunState::Done), "{runs:?}");
    let id = |i: usize| -> String { runs[i].run.to_string().chars().take(8).collect() };
    let expected = format!(
        "Hello! What shall we talk about?\n\
         [done]\n\
         ? Which colour?\n  (answer with /answer <text>, or just type it)\n\
         [answered]\n\
         blue it is\n\
         [done]\n\
         2 run(s), the latest first:\n  {} Done: blue it is\n  {} Done: Hello! What shall we talk about?\n\
         bye\n",
        id(0),
        id(1),
    );
    assert_eq!(screen.text(), expected);

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let answer = requests[2].messages.last().unwrap().text();
    assert!(answer.contains("blue"), "the tool's result: {answer}");
}

/// `/answer` to a run that works is refused and reaches nothing; `/steer` reaches it, which takes
/// another step to read it; `/cancel` ends a waiting run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_example_steers_refuses_an_early_answer_and_cancels() {
    let mut s = Session::new(0, Hold::Start).await;
    s.mock
        .push_text("a long poem")
        .push_text("a short poem")
        .push_tool_calls(vec![ask("About what?")]);

    s.type_line("write me a poem").await;
    timeout(WAIT, s.gate.reached.notified())
        .await
        .expect("the model is called");
    s.type_line("/answer too soon").await;
    s.screen.wait_count("not waiting for an answer", 1).await;
    s.type_line("/steer make it short").await;
    s.screen.wait_count("[steered]", 1).await;
    s.gate.release.notify_one();
    s.screen.wait_count("[done]", 1).await;

    s.type_line("/cancel").await;
    s.screen.wait_count("! no run is open", 1).await;
    s.type_line("ask me something").await;
    s.screen.wait_count("? About what?", 1).await;
    s.type_line("/cancel").await;
    s.screen.wait_count("[cancelled", 1).await;
    let (screen, _engine, mock) = s.quit().await;

    let text = screen.text();
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[0].starts_with("! Rejected: run ") && lines[0].ends_with("not waiting for an answer"),
        "{lines:?}"
    );
    assert_eq!(
        lines[1..6],
        [
            "[steered]",
            "a long poem",
            "a short poem",
            "[done]",
            "! no run is open: type a message first"
        ],
        "{lines:?}"
    );
    assert!(lines[8].starts_with("[cancelled"), "{lines:?}");
    assert_eq!(lines.last(), Some(&"bye"));

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let seen: Vec<String> = requests[1].messages.iter().map(|m| m.text()).collect();
    assert!(
        seen.iter().any(|t| t.contains("make it short")),
        "the steer reached the second call: {seen:?}"
    );
    assert!(
        requests
            .iter()
            .flat_map(|r| &r.messages)
            .all(|m| !m.text().contains("too soon")),
        "the refused answer reached nothing"
    );
}

/// An input that ends right after a send (`printf 'hi\n' | chat`): the example follows the run to
/// its end, prints the whole reply, and only then shuts the engine down. A second run asks a
/// question, which ends the wait: nothing more will be typed to answer it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_end_of_the_input_waits_for_the_run() {
    let mut s = Session::new(0, Hold::Start).await;
    s.mock.push_text("Hello! What shall we talk about?");
    s.type_line("hi").await;
    timeout(WAIT, s.gate.reached.notified())
        .await
        .expect("the model is called");
    // The input ends while the model has not answered, and the example reads that end first.
    s.keys.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    s.gate.release.notify_one();
    let (screen, engine, _mock) = s.returns().await;
    assert_eq!(
        screen.text(),
        "Hello! What shall we talk about?\n[done]\nbye\n"
    );
    let runs = engine.list(ListQuery::new()).await.unwrap().items;
    assert_eq!(runs[0].state, RunState::Done, "{runs:?}");

    let mut s = Session::new(9, Hold::Start).await;
    s.mock.push_tool_calls(vec![ask("Which colour?")]);
    s.type_line("pick a colour").await;
    s.keys.shutdown().await.unwrap();
    let (screen, engine, _mock) = s.returns().await;
    assert_eq!(
        screen.text(),
        "? Which colour?\n  (answer with /answer <text>, or just type it)\nbye\n"
    );
    let runs = engine.list(ListQuery::new()).await.unwrap().items;
    assert_eq!(runs[0].state, RunState::Waiting, "{runs:?}");
}

/// `/quit` while a step is in flight says that the step finishes first, and it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quit_during_a_step_says_it_waits_for_it() {
    let mut s = Session::new(0, Hold::Start).await;
    s.mock.push_text("finished");
    s.type_line("hi").await;
    timeout(WAIT, s.gate.reached.notified())
        .await
        .expect("the model is called");
    s.type_line("/quit").await;
    s.screen
        .wait_count("[stopping: the current step finishes first]", 1)
        .await;
    s.gate.release.notify_one();
    let (screen, _engine, _mock) = s.returns().await;
    assert_eq!(
        screen.text(),
        "[stopping: the current step finishes first]\nbye\n"
    );
}
