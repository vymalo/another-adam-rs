//! `turn_output`, the thread tool with which an agent announces its answer: a whole agent behind A2A,
//! as the screen's orchestrator drives it, and the fake endpoint of `adam-mcp-testkit` that lists the
//! tool. A successful call makes its text the run's answer, so the A2A `completed` status message
//! carries it (a plain A2A reader sees what the orchestrator shows) and not the model's closing line.
//!
//! What these assert is recorded: the task's status message, the run's output and state, the tool
//! results the model was sent and what the endpoint accepted.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, Task, TaskState};
use adam_a2a::{Caller, THREAD_TOOLS_EXTENSION, TaskBackend};
use adam_a2a_runtime::{RuntimeTaskBackend, vymalo_inbound};
use adam_core::{DynStore, MemoryStore, RunId};
use adam_llm_agent::LlmAgent;
use adam_mcp::McpPolicy;
use adam_mcp_testkit::ThreadToolsServer;
use adam_model::{MockModel, ModelRequest, ToolCall};
use adam_runtime::{BroadcastSink, Runtime};
use adam_ui::{TURN_OUTPUT_DELIVERED, Ui};
use serde_json::{Value, json};
use tokio::sync::oneshot;

const SECRET: &str = "sekret-token-4f1c9a";

struct Rig {
    runtime: Runtime,
    backend: RuntimeTaskBackend,
    model: Arc<MockModel>,
}

impl Rig {
    fn new() -> Self {
        let store: DynStore = Arc::new(MemoryStore::new());
        let model = Arc::new(MockModel::new());
        let ui = Ui::new(McpPolicy::default());
        let agent = LlmAgent::builder("answerer", model.clone(), "m")
            .instructions("You are a test agent.")
            .tools(ui.tools())
            .tool_source(ui.source())
            .build();
        let events = BroadcastSink::default();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(events.clone())
            .poll_interval(Duration::from_millis(10))
            .build();
        let backend = RuntimeTaskBackend::new(runtime.clone(), events, "answerer")
            .with_poll_interval(Duration::from_millis(10))
            .with_inbound(vymalo_inbound);
        Self {
            runtime,
            backend,
            model,
        }
    }

    /// Run one message to its end and return the task as a plain A2A client reads it.
    async fn run(&self, server: &ThreadToolsServer, text: &str) -> Task {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.runtime.clone();
        let worker = tokio::spawn(async move {
            let _ = rt
                .run_worker(async {
                    let _ = rx.await;
                })
                .await;
        });
        let mut message = Message::new(Role::User, vec![Part::text(text)]);
        message.metadata = Some(
            [(
                THREAD_TOOLS_EXTENSION.to_owned(),
                json!({"url": server.url("thread-1"), "token": SECRET,
                       "expiresAt": "2999-01-01T00:00:00Z"}),
            )]
            .into_iter()
            .collect(),
        );
        let task = self
            .backend
            .submit(Caller::new("token-0"), message, None, None)
            .await
            .unwrap();
        let mut done = None;
        for _ in 0..1000 {
            let got = self
                .backend
                .get(&Caller::new("token-0"), &task.id)
                .await
                .unwrap()
                .unwrap();
            if got.status.state == TaskState::Completed {
                done = Some(got);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.send(()).unwrap();
        worker.await.unwrap();
        done.expect("the task completed")
    }

    async fn state_of(&self, task: &Task) -> (Value, Value) {
        let view = self
            .runtime
            .view(RunId(task.id.parse().unwrap()))
            .await
            .unwrap()
            .unwrap();
        (view.output.unwrap(), view.state)
    }
}

fn turn_output(id: &str, text: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "turn_output".into(),
        arguments: json!({ "text": text }),
    }
}

fn names(request: &ModelRequest) -> Vec<&str> {
    request.tools.iter().map(|t| t.name.as_str()).collect()
}

fn status_text(task: &Task) -> Option<&str> {
    task.status.message.as_ref().and_then(Message::text)
}

const ANSWER: &str = "## Result\n\nThe pull request is **#12**.";

#[tokio::test]
async fn an_announced_answer_is_the_answer_the_completed_status_carries() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    server.enable_turn_output();
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![turn_output("t1", ANSWER)])
        .push_text("There it is.");

    let task = rig.run(&server, "do it").await;

    // A plain A2A reader reads the answer the person was shown, not the closing line.
    assert_eq!(status_text(&task), Some(ANSWER));
    let (output, state) = rig.state_of(&task).await;
    assert_eq!(output["text"], ANSWER);
    assert!(output.get("stream").is_none(), "{output}");
    assert_eq!(state["announced"], ANSWER);
    // What the endpoint accepted: the text, as it was given.
    assert_eq!(server.announcements(), [ANSWER]);
    // The model was offered the tool under its listed name, and told what to do next (not the
    // endpoint's `{"delivered": true}`).
    let requests = rig.model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        names(&requests[0]),
        ["ask_user", "show", "ui_catalog", "turn_output"]
    );
    assert_eq!(
        requests[1].messages.last(),
        Some(&adam_model::Message::tool_result(
            "t1",
            TURN_OUTPUT_DELIVERED
        ))
    );
    assert!(
        TURN_OUTPUT_DELIVERED
            .starts_with("Delivered to the person as your answer. Finish now with one short line")
    );
}

#[tokio::test]
async fn the_last_announcement_wins() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    server.enable_turn_output();
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![turn_output("t1", "a first try")])
        .push_tool_calls(vec![turn_output("t2", ANSWER)])
        .push_text("Done.");

    let task = rig.run(&server, "do it").await;

    assert_eq!(status_text(&task), Some(ANSWER));
    // The endpoint kept both (the log is append-only: the reader's rule is last wins), the run
    // answers with the last.
    assert_eq!(server.announcements(), ["a first try", ANSWER]);
    assert_eq!(rig.state_of(&task).await.0["text"], ANSWER);
}

#[tokio::test]
async fn a_refused_call_changes_nothing_and_the_model_reads_the_error() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    server.enable_turn_output();
    server.end_turn();
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![turn_output("t1", ANSWER)])
        .push_text("The turn was over; the answer is in my last words.");

    let task = rig.run(&server, "do it").await;

    assert_eq!(
        status_text(&task),
        Some("The turn was over; the answer is in my last words.")
    );
    assert!(server.announcements().is_empty());
    assert_eq!(
        rig.model.requests()[1].messages.last(),
        Some(&adam_model::Message::tool_error("t1", "this turn is over"))
    );
    assert!(rig.state_of(&task).await.1.get("announced").is_none());
}

#[tokio::test]
async fn an_oversize_answer_is_refused_by_the_endpoint_and_announces_nothing() {
    let server = ThreadToolsServer::start(&[SECRET]).await;
    server.enable_turn_output();
    let rig = Rig::new();
    rig.model
        .push_tool_calls(vec![turn_output("t1", &"x".repeat(65_537))])
        .push_text("Too long; here is the short form.");

    let task = rig.run(&server, "do it").await;

    assert_eq!(
        status_text(&task),
        Some("Too long; here is the short form.")
    );
    assert_eq!(
        rig.model.requests()[1].messages.last(),
        Some(&adam_model::Message::tool_error(
            "t1",
            "text must be at most 65536 bytes"
        ))
    );
}

#[tokio::test]
async fn an_endpoint_without_the_tool_changes_nothing() {
    // No `turn_output` listed (an orchestrator without it): the tool is not offered and the closing
    // words are the answer, as they always were.
    let server = ThreadToolsServer::start(&[SECRET]).await;
    let rig = Rig::new();
    rig.model.push_text("Just the words.");

    let task = rig.run(&server, "do it").await;

    assert_eq!(status_text(&task), Some("Just the words."));
    assert_eq!(
        names(&rig.model.requests()[0]),
        ["ask_user", "show", "ui_catalog"]
    );
    assert!(server.calls().is_empty());
    let (output, state) = rig.state_of(&task).await;
    assert_eq!(output["text"], "Just the words.");
    assert!(state.get("announced").is_none());
}
