//! A folder's modes (`agent-mode/v1`, ADR 0039 decision 3) in a real agent: the mode a run's
//! context names narrows the tools the model is shown, its words reach the prompt, a mode that
//! says nothing changes nothing — and a mode never widens what the access tier allows.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::AgentDef;
use adam_llm_agent::{
    ACCESS_TIER_CONTEXT_KEY, MODE_CONTEXT_KEY, SourceCtx, ToolCtx, ToolError, ToolOutput,
    ToolSource, Withheld,
};
use adam_model::{DynModel, MockModel, ToolSpec};
use adam_runtime::{Inbound, Runtime};
use async_trait::async_trait;
use common::{runtime, spawn_worker, tools, wait_done};
use serde_json::{Value, json};

const INSTRUCTIONS: &str = "agent/instructions.md";

fn instructions(frontmatter: &str, body: &str) -> String {
    format!("---\n{frontmatter}\n---\n{body}\n")
}

/// The folder: `build` (the default, inert) and `plan` (read-only, with words).
fn files() -> Vec<(&'static str, String)> {
    vec![
        (
            INSTRUCTIONS,
            instructions(
                "name: helper\ntools: [read_file, list_files, write_file]",
                "You help.",
            ),
        ),
        (
            "agent/modes/build.md",
            "---\ndescription: Changes things.\ndefault: true\n---\n".to_owned(),
        ),
        (
            "agent/modes/plan.md",
            "---\ndescription: Reads and proposes.\ntools: [read_file]\n---\n\
             You are in plan mode: propose, change nothing.\n"
                .to_owned(),
        ),
    ]
}

fn assembly(mock: Arc<MockModel>) -> adam_assembly::Assembly {
    let owned: Vec<(&str, &str)> = files()
        .into_iter()
        .map(|(path, text)| (path, Box::leak(text.into_boxed_str()) as &str))
        .collect();
    let def: AgentDef = common::def(&owned);
    let model: DynModel = mock;
    def.bind(tools(&["read_file", "list_files", "write_file"]))
        .unwrap()
        .tool_source(TierSource)
        .model(model, "m")
        .unwrap()
}

/// What the coder's `TierSource` does, cut down to three tools: `write_file` needs `edits`, the
/// rest are reads. A second source beside the mode's, so the test proves the intersection.
struct TierSource;

/// The tier named in a context, `auto` when it names none.
fn tier_of(context: &serde_json::Map<String, Value>) -> &'static str {
    match context
        .get(ACCESS_TIER_CONTEXT_KEY)
        .and_then(|entry| entry.get("tier"))
        .and_then(Value::as_str)
    {
        Some("plan") => "plan",
        Some("edits") => "edits",
        Some("bypass") => "bypass",
        _ => "auto",
    }
}

#[async_trait]
impl ToolSource for TierSource {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let tier = tier_of(ctx.context_map());
        tools
            .iter()
            .filter(|tool| **tool == "write_file" && tier == "plan")
            .map(|tool| Withheld::new(*tool, format!("`{tool}` needs the edits tier")))
            .collect()
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        None
    }
}

/// Run the agent once, with `context` as the run's inbound context, and answer the tools the model
/// was shown.
async fn shown(context: Value) -> Vec<String> {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let built = assembly(Arc::clone(&mock));
    let rt: Runtime = runtime(&built);
    let worker = spawn_worker(&rt);
    let run = rt
        .start(
            "helper",
            Inbound::new("message", json!({"text": "go", "context": context})),
            None,
        )
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    mock.requests()[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

fn naming(mode: &str) -> Value {
    json!({ MODE_CONTEXT_KEY: { "mode": mode } })
}

#[tokio::test]
async fn the_default_mode_narrows_nothing() {
    assert_eq!(
        shown(json!({})).await,
        ["read_file", "list_files", "write_file"]
    );
    // `build` is the default and says nothing: naming it changes nothing either.
    assert_eq!(
        shown(naming("build")).await,
        ["read_file", "list_files", "write_file"]
    );
}

#[tokio::test]
async fn a_mode_narrows_the_tools_the_model_is_shown() {
    assert_eq!(shown(naming("plan")).await, ["read_file"]);
}

#[tokio::test]
async fn the_tier_still_caps_what_a_mode_allows() {
    // The mode allows `read_file` and nothing else; the tier of the message caps `write_file`
    // whether a mode allows it or not.
    assert_eq!(
        shown(json!({
            MODE_CONTEXT_KEY: { "mode": "build" },
            ACCESS_TIER_CONTEXT_KEY: { "tier": "plan" },
        }))
        .await,
        ["read_file", "list_files"],
        "the tier alone, with the inert default mode"
    );
    // Both at once: the intersection. `list_files` is the tier's to allow and the mode's to refuse.
    assert_eq!(
        shown(json!({
            MODE_CONTEXT_KEY: { "mode": "plan" },
            ACCESS_TIER_CONTEXT_KEY: { "tier": "plan" },
        }))
        .await,
        ["read_file"]
    );
    // A wide tier does not widen a mode: the mode's list is the most it may show.
    assert_eq!(
        shown(json!({
            MODE_CONTEXT_KEY: { "mode": "plan" },
            ACCESS_TIER_CONTEXT_KEY: { "tier": "bypass" },
        }))
        .await,
        ["read_file"]
    );
}

#[tokio::test]
async fn the_modes_words_reach_the_prompt_only_when_a_mode_says_them() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let built = assembly(Arc::clone(&mock));
    let rt = runtime(&built);
    let worker = spawn_worker(&rt);
    let run = rt
        .start(
            "helper",
            Inbound::new("message", json!({"text": "go", "context": naming("plan")})),
            None,
        )
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    let system = mock.requests()[0].system.clone().unwrap_or_default();
    assert!(system.contains("You help."), "{system}");
    assert!(system.contains("**plan**"), "{system}");
    assert!(system.contains("propose, change nothing"), "{system}");

    // The default mode adds no words at all: the prompt is the folder's, byte for byte.
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let built = assembly(Arc::clone(&mock));
    let rt = runtime(&built);
    let worker = spawn_worker(&rt);
    let run = rt
        .start(
            "helper",
            Inbound::new("message", json!({"text": "go"})),
            None,
        )
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    let system = mock.requests()[0].system.clone().unwrap_or_default();
    assert!(system.starts_with("You help."), "{system}");
    assert!(!system.contains("Mode:"), "{system}");
}
