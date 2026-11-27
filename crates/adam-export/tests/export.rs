//! `export_runs` and the `Exporter` end to end: an `LlmAgent` on a scripted `MockModel` and the in-memory store, its
//! runs exported as JSON Lines through a filter.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_error::{Classify, ErrorClass};
use adam_export::{
    ExportError, ExportOptions, Exporter, Filter, JsonLines, Pseudonyms, RuntimeSource, Samples,
    Summary, export_runs,
};
use adam_llm_agent::{
    LlmAgent, LlmAgentBuilder, Tool, ToolCtx, ToolError, ToolOutput, user_message,
};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::{Runtime, RuntimeBuilder};
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

/// The subagent `coder/helper` as its parent calls it: a one-shot session whose answer is the
/// call's result (what `adam_assembly::SubagentTool` does, ADR 0087 slice C1).
struct Helper;

#[async_trait]
impl Tool for Helper {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "helper".into(),
            description: "ask the helper".into(),
            parameters: json!({"type": "object", "properties": {"message": {"type": "string"}}}),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let message = args["message"].as_str().unwrap_or_default();
        let run = ctx.start_one_shot_session(HELPER, message, None).await?;
        Err(ToolError::AwaitRun { run })
    }
}

const HELPER: &str = "coder/helper";

impl Harness {
    fn new() -> Self {
        Self::on(|coder| coder, |runtime, _| runtime)
    }

    /// The harness with the subagent `coder/helper` on the same model, and its tool on `coder`.
    fn with_helper() -> Self {
        Self::on(
            |coder| coder.tool(Helper),
            |runtime, model| {
                runtime.agent(
                    LlmAgent::builder(HELPER, model, "test-model")
                        .instructions("help")
                        .build(),
                )
            },
        )
    }

    /// The harness with what `coder` and `runtime` add to the agent `coder` and to the runtime.
    fn on(
        coder: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
        runtime: impl FnOnce(RuntimeBuilder, DynModel) -> RuntimeBuilder,
    ) -> Self {
        let store: DynStore = Arc::new(MemoryStore::new());
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = coder(
            LlmAgent::builder("coder", model.clone(), "test-model")
                .instructions("be brief")
                .tool(ReadFile),
        )
        .build();
        let runtime = runtime(Runtime::builder(store), model)
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
    export_with(runtime, filter, &options()).await
}

async fn export_with(
    runtime: &Runtime,
    filter: &Filter,
    options: &ExportOptions,
) -> (Summary, Vec<Value>) {
    let mut out = Vec::new();
    let summary = export_runs(runtime, &["coder".to_owned()], filter, options, &mut out)
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
            ..Summary::default()
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
    // The run recorded its last model call (ADR 0085 slice E3): the record carries the system
    // prompt and the tools that call was sent.
    assert_eq!(record["tools_source"], "recorded");
    assert_eq!(record["compacted"], false);
    assert_eq!(record["tools"][0]["type"], "function");
    assert_eq!(record["tools"][0]["function"]["name"], "read_file");

    let all = record["messages"].as_array().expect("messages");
    assert_eq!(all[0]["role"], "system");
    assert!(
        all[0]["content"]
            .as_str()
            .is_some_and(|text| text.contains("be brief")),
        "{}",
        all[0]
    );
    let messages = &all[1..];
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
            ..Summary::default()
        }
    );

    // A failed status lists nothing: the store filters by status before a state is read.
    let (summary, _) = export(&h.runtime, &Filter::default().statuses([RunStatus::Failed])).await;
    assert_eq!(summary, Summary::default());

    h.stop().await;
}

/// Per turn (ADR 0085, slice E5): the run of two model calls is two samples, each the request of
/// its call (the recorded system prompt and tools, the history before the answer) and the answer
/// last, `train_on: "last"`. A token budget leaves out the samples over it, whole.
#[tokio::test]
async fn a_turn_export_writes_one_sample_per_answer() {
    let h = Harness::new();
    h.mock
        .push_tool_calls(vec![ToolCall::new(
            "c1",
            "read_file",
            json!({"path": "Cargo.toml"}),
        )])
        .push_text("added a route");
    h.run_to_done("add a route", "conv-axum").await;

    let turns = options().samples(Samples::Turn);
    let (summary, lines) = export_with(&h.runtime, &Filter::default(), &turns).await;
    assert_eq!(
        summary,
        Summary {
            seen: 1,
            written: 2,
            ..Summary::default()
        }
    );
    let [first, second] = lines.as_slice() else {
        panic!("two samples: {lines:#?}");
    };
    for (n, sample) in [first, second].into_iter().enumerate() {
        assert_eq!(sample["schema"], "adam.export/v1");
        assert_eq!(sample["train_on"], "last");
        assert_eq!(sample["tools_source"], "recorded");
        assert_eq!(sample["tools"][0]["function"]["name"], "read_file");
        assert_eq!(
            sample["sample"],
            json!({"turn": n, "index": n, "of": 2, "exact": true})
        );
        assert_eq!(sample["turns"], 2, "the run's own count");
        assert_eq!(sample["facets"]["dep"], json!(["axum"]), "the run's facets");
        assert_eq!(sample["messages"][0]["role"], "system");
        let raw = sample.to_string();
        assert!(!raw.contains(GITHUB_TOKEN), "a token leaked: {raw}");
        assert!(!raw.contains(DEPLOYMENT_SECRET), "a secret leaked: {raw}");
    }
    let roles = |sample: &Value| -> Vec<String> {
        sample["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|m| m["role"].as_str().expect("a role").to_owned())
            .collect()
    };
    assert_eq!(roles(first), ["system", "user", "assistant"]);
    assert_eq!(first["messages"][2]["tool_calls"][0]["id"], "c1");
    assert_eq!(
        roles(second),
        ["system", "user", "assistant", "tool", "assistant"]
    );
    assert_eq!(second["messages"][3]["tool_call_id"], "c1");
    assert_eq!(second["messages"][4]["content"], "added a route");

    // The run-mode record says every assistant message is a target, and has no `sample`.
    let (_, run) = export(&h.runtime, &Filter::default()).await;
    assert_eq!(run[0]["train_on"], "assistant");
    assert!(run[0].get("sample").is_none());

    // A budget between the two samples keeps the first only.
    let tokens = |sample: &Value| -> u32 {
        let record: adam_export::Record = serde_json::from_value(sample.clone()).expect("a record");
        u32::try_from(adam_export::estimated_tokens(&record)).expect("small")
    };
    let (small, big) = (tokens(first), tokens(second));
    assert!(small < big);
    let (summary, lines) = export_with(
        &h.runtime,
        &Filter::default(),
        &turns.clone().max_tokens(Some(small)),
    )
    .await;
    assert_eq!((summary.written, summary.over_budget), (1, 1));
    assert_eq!(lines[0]["sample"]["turn"], 0);

    h.stop().await;
}

/// A subagent's one-shot session (ADR 0087 slice C1) has a session's conversation id, so the
/// exporter reaches it: it is a record of its own, linked to its parent's by `parent`.
#[tokio::test]
async fn a_subagents_run_is_a_record_of_its_own_linked_to_its_parent() {
    let h = Harness::with_helper();
    h.mock
        .push_tool_calls(vec![ToolCall::new(
            "c1",
            "helper",
            json!({"message": "summarise the manifest"}),
        )])
        .push_text("it uses axum")
        .push_text("the helper says it uses axum");
    let parent = h.run_to_done("what does it use?", "conv-help").await;

    let mut out = Vec::new();
    let agents = ["coder".to_owned(), HELPER.to_owned()];
    let summary = export_runs(
        &h.runtime,
        &agents,
        &Filter::default(),
        &options(),
        &mut out,
    )
    .await
    .expect("export");
    assert_eq!((summary.seen, summary.written), (2, 2), "{summary:?}");
    let records: Vec<Value> = String::from_utf8(out)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON object"))
        .collect();
    let by_agent = |agent: &str| {
        records
            .iter()
            .find(|record| record["agent"] == agent)
            .unwrap_or_else(|| panic!("a record of {agent}: {records:#?}"))
    };
    let (coder, helper) = (by_agent("coder"), by_agent(HELPER));
    assert_eq!(
        coder["id"],
        Pseudonyms::new(b"export key").of("run", &parent.to_string())
    );
    assert!(coder.get("parent").is_none(), "{coder}");
    assert_eq!(helper["parent"], coder["id"]);
    assert!(helper["conversation"].is_string(), "{helper}");
    assert_ne!(helper["conversation"], coder["conversation"]);
    let roles: Vec<&str> = helper["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["role"].as_str())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant"]);
    assert_eq!(helper["messages"][1]["content"], "summarise the manifest");
    assert_eq!(helper["messages"][2]["content"], "it uses axum");
    h.stop().await;
}

fn lines(out: Vec<u8>) -> Vec<Value> {
    String::from_utf8(out)
        .expect("utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON object"))
        .collect()
}

/// Two stores into one dataset (ADR 0085, *Amended*: ports and adapters): each record names its
/// source by label, the summary adds both up, and the ids are pseudonyms under the one key, so a
/// run's id is the same whether its store is exported alone or with the other.
#[tokio::test]
async fn two_stores_export_into_one_sink_with_their_labels_and_linked_ids() {
    let (alpha, beta) = (Harness::new(), Harness::new());
    alpha.mock.push_text("hello from alpha");
    let alpha_run = alpha.run_to_done("hi", "conv-a").await;
    beta.mock.push_text("hello from beta");
    let beta_run = beta.run_to_done("hi", "conv-b").await;

    let exporter = Exporter::new(Filter::default(), options());
    let a = RuntimeSource::new(alpha.runtime.clone())
        .only(["coder"])
        .named("alpha");
    let b = RuntimeSource::new(beta.runtime.clone())
        .only(["coder"])
        .named("beta");
    let mut sink = JsonLines::new(Vec::new());
    let summary = exporter.run(&[&a, &b], &mut sink).await.expect("export");
    assert_eq!((summary.seen, summary.written), (2, 2), "{summary:?}");
    let records = lines(sink.into_inner());
    let pseudonym = |run: RunId| Pseudonyms::new(b"export key").of("run", &run.to_string());
    let [first, second] = records.as_slice() else {
        panic!("two records: {records:#?}");
    };
    assert_eq!(first["source"], "alpha");
    assert_eq!(first["id"], pseudonym(alpha_run));
    assert_eq!(second["source"], "beta");
    assert_eq!(second["id"], pseudonym(beta_run));

    // One unnamed source: no `source` field, the same id.
    let alone = RuntimeSource::new(alpha.runtime.clone()).only(["coder"]);
    let mut sink = JsonLines::new(Vec::new());
    exporter.run(&[&alone], &mut sink).await.expect("export");
    let records = lines(sink.into_inner());
    let [record] = records.as_slice() else {
        panic!("one record: {records:#?}");
    };
    assert!(record.get("source").is_none(), "{record}");
    assert_eq!(record["id"], first["id"], "the ids link across exports");

    // Several sources must each be named: nothing is read otherwise.
    let mut sink = JsonLines::new(Vec::new());
    let e = exporter
        .run(&[&a, &alone], &mut sink)
        .await
        .expect_err("an unnamed source among several");
    assert!(matches!(e, ExportError::Labels(_)), "{e:?}");
    assert_eq!(e.class(), ErrorClass::Invalid);
    assert!(sink.into_inner().is_empty());

    alpha.stop().await;
    beta.stop().await;
}

/// The convenience path writes what the `Exporter` writes from one unnamed `RuntimeSource` into
/// `JsonLines`, byte for byte.
#[tokio::test]
async fn export_runs_is_the_exporter_over_one_unnamed_runtime_source() {
    let h = Harness::new();
    h.mock.push_text("hello");
    h.run_to_done("hi", "conv-chat").await;
    let agents = ["coder".to_owned()];

    let mut convenience = Vec::new();
    let summary = export_runs(
        &h.runtime,
        &agents,
        &Filter::default(),
        &options(),
        &mut convenience,
    )
    .await
    .expect("export");
    let mut sink = JsonLines::new(Vec::new());
    let source = RuntimeSource::new(h.runtime.clone()).only(agents);
    let piped = Exporter::new(Filter::default(), options())
        .run(&[&source], &mut sink)
        .await
        .expect("export");
    assert_eq!(summary, piped);
    assert_eq!(summary.written, 1);
    assert_eq!(convenience, sink.into_inner());
    h.stop().await;
}
