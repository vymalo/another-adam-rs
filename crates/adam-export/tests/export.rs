//! `export_runs` end to end: an `LlmAgent` on a scripted `MockModel` and the in-memory store, its
//! runs exported as JSON Lines through a filter.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_export::{ExportOptions, Filter, Pseudonyms, Summary, export_runs};
use adam_llm_agent::{LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::Runtime;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const GITHUB_TOKEN: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
const DEPLOYMENT_SECRET: &str = "corp-secret-42";

/// Reads a `Cargo.toml` that depends on axum, with two secrets planted in it.
struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "read a file of the workspace".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!(
            "     1\t[package]\n     2\tname = \"api\"\n     3\t[dependencies]\n     4\taxum = \"0.8\"\n     5\t# token {GITHUB_TOKEN} and {DEPLOYMENT_SECRET}\n"
        )))
    }
}

struct Harness {
    runtime: Runtime,
    mock: Arc<MockModel>,
    stop: oneshot::Sender<()>,
    worker: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
}

impl Harness {
    fn new() -> Self {
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = LlmAgent::builder("coder", model, "test-model")
            .instructions("be brief")
            .tool(ReadFile)
            .build();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .worker_id("w")
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .build();
        let (stop, rx) = oneshot::channel::<()>();
        let rt = runtime.clone();
        let worker = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        });
        Self {
            runtime,
            mock,
            stop,
            worker,
        }
    }

    async fn run_to_done(&self, text: &str, conversation: &str) -> RunId {
        let run = self
            .runtime
            .start("coder", user_message(text), Some(conversation))
            .await
            .expect("start");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = self.runtime.view(run).await.expect("view").expect("run");
            if view.status == RunStatus::Done {
                return run;
            }
            assert!(
                view.status != RunStatus::Failed,
                "the run failed: {view:#?}"
            );
            assert!(Instant::now() < deadline, "timed out: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.worker)
            .await
            .expect("worker stops in time")
            .expect("worker task")
            .expect("worker result");
    }
}

fn options() -> ExportOptions {
    ExportOptions::new(Pseudonyms::new(b"export key"))
        .scrub(|text: &str| text.replace(DEPLOYMENT_SECRET, "[deployment secret]"))
}

async fn export(runtime: &Runtime, filter: &Filter) -> (Summary, Vec<Value>) {
    let mut out = Vec::new();
    let summary = export_runs(runtime, &["coder".to_owned()], filter, &options(), &mut out)
        .await
        .expect("export");
    let text = String::from_utf8(out).expect("utf-8");
    let lines = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("a line is a JSON object"))
        .collect();
    (summary, lines)
}

#[tokio::test]
async fn a_stack_filter_keeps_the_run_that_read_the_manifest() {
    let h = Harness::new();
    h.mock
        .push_tool_calls(vec![ToolCall::new(
            "c1",
            "read_file",
            json!({"path": "Cargo.toml"}),
        )])
        .push_text("added a route");
    let axum_run = h.run_to_done("add a route", "conv-axum").await;
    h.mock.push_text("hello");
    let chat_run = h.run_to_done("hi", "conv-chat").await;

    let (summary, lines) = export(&h.runtime, &Filter::default().require("dep", "axum")).await;
    assert_eq!(
        summary,
        Summary {
            seen: 2,
            written: 1,
            filtered: 1,
            not_a_conversation: 0,
            unreadable: 0,
        }
    );
    let [record] = lines.as_slice() else {
        panic!("one record: {lines:#?}");
    };
    let raw = record.to_string();
    assert_eq!(record["schema"], "adam.export/v1");
    assert_eq!(record["agent"], "coder");
    assert_eq!(record["status"], "done");
    assert_eq!(
        record["id"],
        Pseudonyms::new(b"export key").of("run", &axum_run.to_string())
    );
    for id in [axum_run.to_string(), chat_run.to_string()] {
        assert!(!raw.contains(&id), "a run id leaked: {raw}");
    }
    assert!(
        !raw.contains("conv-axum"),
        "a conversation id leaked: {raw}"
    );
    assert!(!raw.contains(GITHUB_TOKEN), "a token leaked: {raw}");
    assert!(!raw.contains(DEPLOYMENT_SECRET), "a secret leaked: {raw}");
    assert!(raw.contains("[deployment secret]"), "{raw}");
    assert_eq!(record["facets"]["dep"], json!(["axum"]));
    assert_eq!(record["facets"]["lang"], json!(["toml"]));
    assert_eq!(record["tools_source"], "none");
    assert_eq!(record["compacted"], false);

    let messages = record["messages"].as_array().expect("messages");
    let roles: Vec<&str> = messages
        .iter()
        .map(|message| message["role"].as_str().expect("a role"))
        .collect();
    assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
    assert_eq!(messages[0]["content"], "add a route");
    let call = &messages[1]["tool_calls"][0];
    assert_eq!(call["id"], "c1");
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "read_file");
    assert_eq!(call["function"]["arguments"], r#"{"path":"Cargo.toml"}"#);
    assert_eq!(messages[2]["tool_call_id"], "c1");
    assert!(
        messages[2]["content"]
            .as_str()
            .is_some_and(|text| text.contains("axum = \"0.8\"")),
        "{}",
        messages[2]
    );
    assert_eq!(messages[3]["content"], "added a route");

    let (summary, lines) = export(&h.runtime, &Filter::default().require("dep", "actix-web")).await;
    assert!(lines.is_empty(), "{lines:#?}");
    assert_eq!(
        summary,
        Summary {
            seen: 2,
            written: 0,
            filtered: 2,
            not_a_conversation: 0,
            unreadable: 0,
        }
    );

    // A failed status lists nothing: the store filters by status before a state is read.
    let (summary, _) = export(&h.runtime, &Filter::default().statuses([RunStatus::Failed])).await;
    assert_eq!(summary, Summary::default());

    h.stop().await;
}
