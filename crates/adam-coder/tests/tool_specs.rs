//! The specs the model sees, pinned against JSON snapshots.
//!
//! `fixtures/tool-specs/<tool>.json` holds each tool's `ToolSpec` as it was when the tools were
//! written by hand. A spec is the model's contract (and a change to it changes what a replayed run
//! would have been shown), so any difference is deliberate: review the diff, then regenerate with
//! `ADAM_UPDATE_SNAPSHOTS=1 cargo test -p adam-coder --test tool_specs`.
//!
//! One difference from the hand-written JSON is expected and normalised away: a schema derived from
//! an `Option<T>` says `"type": ["T", "null"]` where the hand-written one said `"type": "T"`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::PathBuf;

use std::sync::Arc;

use adam_coder::{ToolEnv, coder_tools};
use adam_llm_agent::{BuildError, LlmAgent};
use adam_model::MockModel;
use common::Fixture;
use serde_json::{Value, json};

/// The tools in the order the model is offered them.
const TOOLS: [&str; 6] = [
    "prepare_workspace",
    "delegate_to_opencode",
    "run_checks",
    "commit_and_push",
    "open_pull_request",
    "ask_user",
];

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tool-specs")
        .join(format!("{name}.json"))
}

/// `{"type": ["string", "null"]}` becomes `{"type": "string"}`, everywhere in `schema`.
fn without_nullable(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get("type")
                && let [Value::String(t), Value::String(null)] = types.as_slice()
                && null == "null"
            {
                let t = t.clone();
                map.insert("type".into(), Value::String(t));
            }
            map.values_mut().for_each(without_nullable);
        }
        Value::Array(items) => items.iter_mut().for_each(without_nullable),
        _ => {}
    }
}

fn spec_json(tool: &dyn adam_llm_agent::Tool) -> Value {
    let spec = tool.spec();
    let mut value = serde_json::to_value(&spec).unwrap();
    without_nullable(&mut value["parameters"]);
    value
}

#[tokio::test]
async fn every_spec_equals_its_snapshot() {
    let fx = Fixture::new("hello\n").await;
    let tools: Vec<_> = coder_tools(&fx.env).into_iter().collect();
    let names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
    assert_eq!(names, TOOLS, "the tools, in the order they are offered");

    let update = std::env::var_os("ADAM_UPDATE_SNAPSHOTS").is_some();
    for tool in &tools {
        let actual = spec_json(tool.as_ref());
        let name = actual["name"].as_str().unwrap().to_owned();
        let path = snapshot_path(&name);
        if update {
            let mut text = serde_json::to_string_pretty(&actual).unwrap();
            text.push('\n');
            std::fs::write(&path, text).unwrap();
            continue;
        }
        let expected: Value = serde_json::from_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("no snapshot for `{name}` at {}: {e}", path.display())),
        )
        .unwrap();
        assert_eq!(
            actual,
            expected,
            "the spec of `{name}` differs from {}",
            path.display()
        );
    }
}

#[test]
fn nullable_types_normalise_to_the_plain_type() {
    let mut schema = json!({
        "type": "object",
        "properties": {
            "a": {"type": ["string", "null"]},
            "b": {"type": "boolean"},
            "c": {"type": ["integer", "string"]}
        }
    });
    without_nullable(&mut schema);
    assert_eq!(schema["properties"]["a"]["type"], "string");
    assert_eq!(schema["properties"]["b"]["type"], "boolean");
    assert_eq!(
        schema["properties"]["c"]["type"],
        json!(["integer", "string"])
    );
}

/// Every `"type": [T, "null"]` in `schema`, as `path` (`prefix.property`).
fn nullable_properties(prefix: &str, schema: &Value, found: &mut Vec<String>) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, property) in properties {
        if property["type"]
            .as_array()
            .is_some_and(|types| types.iter().any(|t| t == "null"))
        {
            found.push(format!("{prefix}.{name}"));
        }
    }
}

/// The reviewed difference between the hand-written specs and the derived ones: exactly the
/// optional arguments say `null`, and `required` (the contract with the model) is what it is.
#[tokio::test]
async fn only_the_optional_arguments_are_nullable() {
    let fx = Fixture::new("hello\n").await;
    let mut found = Vec::new();
    for tool in coder_tools(&fx.env) {
        let spec = tool.spec();
        nullable_properties(&spec.name, &spec.parameters, &mut found);
    }
    found.sort();
    assert_eq!(
        found,
        [
            "open_pull_request.accept_red_checks",
            "prepare_workspace.base_branch",
            "prepare_workspace.branch",
            "run_checks.cwd",
        ]
    );
}

/// `ask_user` is the one coder tool that asks the person, and the redacting wrapper says so too:
/// `adam-assembly` refuses to give such a tool to a subagent (nobody could answer it).
#[tokio::test]
async fn only_ask_user_asks_the_user() {
    let fx = Fixture::new("hello\n").await;
    let asking: Vec<String> = coder_tools(&fx.env)
        .into_iter()
        .filter(|tool| tool.asks_user())
        .map(|tool| tool.spec().name)
        .collect();
    assert_eq!(asking, ["ask_user"]);
}

/// The tools read `ToolEnv` from the agent's state, and the agent says so when it is missing.
#[tokio::test]
async fn the_tools_need_the_env_as_agent_state() {
    let fx = Fixture::new("hello\n").await;
    let model = || -> adam_model::DynModel { Arc::new(MockModel::new()) };

    let without = LlmAgent::builder("coder", model(), "m")
        .tools(coder_tools(&fx.env))
        .try_build();
    assert!(
        matches!(&without, Err(BuildError::MissingState { .. })),
        "{:?}",
        without.err()
    );

    let with = LlmAgent::builder("coder", model(), "m")
        .state::<ToolEnv>(fx.env.clone())
        .tools(coder_tools(&fx.env))
        .try_build();
    assert!(with.is_ok(), "{:?}", with.err());
}
