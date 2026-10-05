//! A conversation can attach a server the agent already has: the endpoint then lists a tool the
//! agent owns, the agent's own wins, and the omission is logged at debug level, not as a warning at
//! every model turn. Any other clash is still a warning. A whole agent against the fake endpoint.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, TaskState};
use adam_a2a::{Caller, THREAD_TOOLS_EXTENSION, TaskBackend};
use adam_a2a_runtime::{RuntimeTaskBackend, vymalo_inbound};
use adam_core::{DynStore, MemoryStore};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput};
use adam_mcp::McpPolicy;
use adam_mcp_testkit::{LogCapture, ThreadToolsServer};
use adam_model::{MockModel, ToolSpec};
use adam_runtime::{BroadcastSink, Runtime};
use adam_ui::Ui;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const SECRET: &str = "sekret-token-4f1c9a";

/// A tool of the agent's own with a fixed name.
struct Own(&'static str);

#[async_trait]
impl Tool for Own {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.0.into(),
            description: "Own.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("own"))
    }
}

#[tokio::test]
async fn a_relayed_tool_the_agent_has_is_a_debug_line_and_another_clash_is_a_warning() {
    let logs = LogCapture::start();
    let server = ThreadToolsServer::start(&[SECRET]).await;
    // The same server the agent connects itself (`websearch__web_search`), and a plain name the
    // agent has too.
    server.add_tool(
        "websearch__web_search",
        "Search.",
        json!({"type": "object"}),
        "found",
    );
    server.add_tool("plain_tool", "Plain.", json!({"type": "object"}), "plain");

    let store: DynStore = Arc::new(MemoryStore::new());
    let model = Arc::new(MockModel::new());
    model.push_text("Hello.");
    let ui = Ui::new(McpPolicy::default());
    let agent = LlmAgent::builder("answerer", model.clone(), "m")
        .instructions("You are a test agent.")
        .tool(Own("websearch__web_search"))
        .tool(Own("plain_tool"))
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
    let (stop, rx) = oneshot::channel::<()>();
    let rt = runtime.clone();
    let worker = tokio::spawn(async move {
        let _ = rt
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    let mut message = Message::new(Role::User, vec![Part::text("hi")]);
    message.metadata = Some(
        [(
            THREAD_TOOLS_EXTENSION.to_owned(),
            json!({"url": server.url("thread-1"), "token": SECRET,
                   "expiresAt": "2999-01-01T00:00:00Z"}),
        )]
        .into_iter()
        .collect(),
    );
    let task = backend
        .submit(Caller::new("token-0"), message, None, None)
        .await
        .unwrap();
    for _ in 0..1000 {
        let got = backend
            .get(&Caller::new("token-0"), &task.id)
            .await
            .unwrap()
            .unwrap();
        if got.status.state == TaskState::Completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stop.send(()).unwrap();
    worker.await.unwrap();

    let text = logs.text();
    let line_of = |tool: &str| {
        text.lines()
            .find(|l| l.contains(tool) && l.contains("left out"))
            .unwrap_or_else(|| panic!("no line about {tool}:\n{text}"))
            .to_owned()
    };
    let relayed = line_of("websearch__web_search");
    assert!(relayed.contains("DEBUG"), "{relayed}");
    let plain = line_of("plain_tool");
    assert!(plain.contains("WARN"), "{plain}");
}
