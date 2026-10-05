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
const TOOLS: [&str; 20] = [
    "prepare_workspace",
    "start_scratch",
    "publish_scratch",
    "request_repository",
    "create_repository",
    "run_command",
    "run",
    "read_file",
    "write_file",
    "apply_patch",
    "edit_file",
    "share_file",
    "delegate_to_opencode",
    "run_checks",
    "rebuild_environment",
    "commit_and_push",
    "open_pull_request",
    "ask_user",
    "show",
    "ui_catalog",
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
    let fx = Fixture::new("hello\n").await.offering_creation();
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

/// With no owner allowed to create repositories (the default: `CREATE_REPO_OWNERS` is empty) the
/// tool is not offered at all, so the model cannot offer a new repository to the person or call
/// the tool with an owner of its own making; everything else is offered, in the same order.
#[tokio::test]
async fn create_repository_is_not_offered_when_no_owner_may_create_one() {
    let fx = Fixture::new("hello\n").await;
    assert!(fx.env.settings.create_repo_owners.is_empty());
    let names: Vec<String> = coder_tools(&fx.env)
        .into_iter()
        .map(|t| t.spec().name)
        .collect();
    let expected: Vec<&str> = TOOLS
        .into_iter()
        .filter(|name| *name != "create_repository")
        .collect();
    assert_eq!(names, expected);
    // And the same fixture with an owner offers it, in its place after `request_repository`.
    let fx = fx.offering_creation();
    let names: Vec<String> = coder_tools(&fx.env)
        .into_iter()
        .map(|t| t.spec().name)
        .collect();
    assert_eq!(names, TOOLS);
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
    let fx = Fixture::new("hello\n").await.offering_creation();
    let mut found = Vec::new();
    for tool in coder_tools(&fx.env) {
        let spec = tool.spec();
        nullable_properties(&spec.name, &spec.parameters, &mut found);
    }
    found.sort();
    assert_eq!(
        found,
        [
            "apply_patch.repo",
            "commit_and_push.repo",
            "create_repository.description",
            "create_repository.private",
            "delegate_to_opencode.repo",
            "edit_file.replace_all",
            "edit_file.repo",
            "open_pull_request.accept_red_checks",
            "open_pull_request.repo",
            "prepare_workspace.base_branch",
            "prepare_workspace.branch",
            "publish_scratch.base_branch",
            "publish_scratch.overwrite",
            "publish_scratch.path",
            "publish_scratch.scratch",
            "read_file.end_line",
            "read_file.repo",
            "read_file.start_line",
            "rebuild_environment.use_default",
            "run.cwd",
            "run.repo",
            "run_checks.cwd",
            "run_checks.repo",
            "run_command.cwd",
            "run_command.repo",
            "share_file.name",
            "share_file.repo",
            "start_scratch.name",
            "write_file.repo",
        ]
    );
}

/// `ask_user`, `request_repository` and `create_repository` are the coder's tools that ask the person, and the redacting
/// wrapper says so too: `adam-assembly` refuses to give such a tool to a subagent (nobody could
/// answer it).
#[tokio::test]
async fn only_the_tools_that_ask_the_person_are_marked_so() {
    let fx = Fixture::new("hello\n").await.offering_creation();
    let asking: Vec<String> = coder_tools(&fx.env)
        .into_iter()
        .filter(|tool| tool.asks_user())
        .map(|tool| tool.spec().name)
        .collect();
    // `request_repository` and `create_repository` ask the person a question of their own writing; a subagent may have
    // neither (it would wait for ever for an answer only the coder's caller can give).
    assert_eq!(
        asking,
        ["request_repository", "create_repository", "ask_user"]
    );
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
