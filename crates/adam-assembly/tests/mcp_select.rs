//! `mcp.json` `when`: a server's tools are decided from the task before the run's first model
//! call and never again; a call to a server that was not loaded is refused, and its instructions
//! do not reach the prompt.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::Assembly;
use adam_llm_agent::{ToolSet, user_message};
use adam_model::{MockModel, ModelRequest, ToolCall};
use adam_runtime::RunView;
use serde_json::json;

/// `github` has no `when` (eager); `linear` and `docs` load only when the task mentions one of
/// their keywords.
const MCP: &str = r#"{"mcpServers": {
  "github": {"type": "http", "url": "https://mcp.example/github", "tools": ["search"]},
  "linear": {"type": "http", "url": "https://mcp.linear.app/mcp", "tools": ["list_issues"],
             "when": {"keywords": ["linear", "ticket"]}},
  "docs": {"type": "http", "url": "https://mcp.example/docs", "tools": ["search"],
           "when": {"keywords": ["docs", "documentation"]}}
}}"#;

/// The same servers, none with a `when`: every tool is loaded, as before the key existed.
const MCP_NO_WHEN: &str = r#"{"mcpServers": {
  "github": {"type": "http", "url": "https://mcp.example/github", "tools": ["search"]},
  "linear": {"type": "http", "url": "https://mcp.linear.app/mcp", "tools": ["list_issues"]},
  "docs": {"type": "http", "url": "https://mcp.example/docs", "tools": ["search"]}
}}"#;

const TOOLS: &str = "tools: ['github__*', 'linear__*', 'docs__*']";

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
        ..Default::default()
    }
}

fn assembly(mock: &Arc<MockModel>, mcp: &str) -> Assembly {
    common::def(&[
        (
            "agent/instructions.md",
            &common::instructions(&format!("name: root\n{TOOLS}"), "You are root."),
        ),
        ("agent/mcp.json", mcp),
    ])
    .mcp_tools(
        "root",
        common::tools(&["github__search", "linear__list_issues", "docs__search"]),
    )
    .bind(ToolSet::new())
    .unwrap()
    .model(mock.clone(), "default")
    .unwrap()
}

/// The tools the model was shown on request `n`, by name, sorted.
fn names(request: &ModelRequest) -> Vec<String> {
    let mut names: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
    names.sort();
    names
}

fn has(request: &ModelRequest, tool: &str) -> bool {
    names(request).iter().any(|name| name == tool)
}

/// The bytes a provider caches the start of: the system prompt and the tools.
fn head(request: &ModelRequest) -> String {
    serde_json::to_string(&(&request.system, &request.tools)).unwrap()
}

/// Run one task on a fresh runtime to its end: the requests and the view.
async fn run_task(
    mock: &Arc<MockModel>,
    assembly: &Assembly,
    task: &str,
) -> (Vec<ModelRequest>, RunView) {
    let runtime = common::runtime(assembly);
    let run = runtime
        .start("root", user_message(task), None)
        .await
        .unwrap();
    let worker = common::spawn_worker(&runtime);
    let view = common::wait_done(&runtime, run).await;
    worker.stop().await;
    (mock.requests(), view)
}

#[tokio::test]
async fn a_keyword_that_matches_loads_the_server_and_one_that_does_not_is_withheld() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "linear__list_issues")])
        .push_text("done");
    let (requests, _) =
        run_task(&mock, &assembly(&mock, MCP), "please fix the linear ticket").await;
    assert_eq!(
        names(&requests[0]),
        ["github__search", "linear__list_issues", "read_output"],
        "the eager server and the one whose keyword the task holds"
    );
    assert_eq!(
        names(&requests[1]),
        names(&requests[0]),
        "the same tools after the call"
    );
    assert_eq!(
        head(&requests[0]),
        head(&requests[1]),
        "the system prompt and the tools are the same bytes at every request"
    );
    // The tool ran: the second request holds its answer.
    let last = requests[1].messages.last().unwrap();
    assert!(
        format!("{last:?}").contains("linear__list_issues-out"),
        "the loaded tool ran: {last:?}"
    );
}

#[tokio::test]
async fn a_server_with_no_when_is_eager() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(names(&requests[0]), ["github__search", "read_output"]);
}

#[tokio::test]
async fn an_empty_task_loads_only_the_eager_servers() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "").await;
    assert_eq!(names(&requests[0]), ["github__search", "read_output"]);
}

#[tokio::test]
async fn the_keyword_match_ignores_case() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) =
        run_task(&mock, &assembly(&mock, MCP), "PLEASE FIX THE LINEAR TICKET").await;
    assert_eq!(
        names(&requests[0]),
        ["github__search", "linear__list_issues", "read_output"]
    );
}

#[tokio::test]
async fn a_call_to_a_server_that_was_not_loaded_is_refused_and_names_the_reason() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "docs__search")])
        .push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(names(&requests[0]), ["github__search", "read_output"]);
    let last = requests[1].messages.last().unwrap();
    let text = format!("{last:?}");
    assert!(text.contains("not loaded for this run"), "{text}");
    assert!(text.contains("docs"), "{text}");
    assert!(
        !text.contains("docs__search-out"),
        "the server did not run: {text}"
    );
}

#[tokio::test]
async fn a_continued_run_selects_the_same_servers() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("first").push_text("second");
    let assembly = assembly(&mock, MCP);
    let runtime = common::runtime(&assembly);
    let run = runtime
        .start("root", user_message("fix the linear ticket"), None)
        .await
        .unwrap();
    let worker = common::spawn_worker(&runtime);
    common::wait_done(&runtime, run).await;
    // The new message names `docs`, but the task the run carries is the one it started with: the
    // selection is recomputed from it, so the tools do not move.
    let next = runtime
        .start_continuing("root", user_message("now the docs too"), None, run)
        .await
        .unwrap();
    common::wait_done(&runtime, next).await;
    worker.stop().await;
    let requests = mock.requests();
    let first = requests
        .iter()
        .find(|r| r.messages.len() == 1)
        .expect("the first request of the first run");
    assert!(has(first, "linear__list_issues"), "{:?}", names(first));
    let after = requests.last().unwrap();
    assert!(
        !has(after, "docs__search"),
        "the continuation did not load docs: {:?}",
        names(after)
    );
    assert!(
        has(after, "linear__list_issues"),
        "the continuation kept linear: {:?}",
        names(after)
    );
}

#[tokio::test]
async fn a_folder_with_no_when_keeps_the_head_it_had() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP_NO_WHEN), "fix a typo").await;
    assert_eq!(names(&requests[0]).len(), 4, "every server is loaded");
    // Captured at `8f6fd2f` (main before `when` existed): a folder with no `when` sends the same
    // head it sent before the key existed.
    assert_eq!(
        head(&requests[0]),
        "[\"You are root.\\n\\nThe tool calls of one reply may run at the same time; give a call `\\\"after_previous\\\": true` when it must start only after every call before it in the reply has finished.\",[{\"name\":\"github__search\",\"description\":\"the github__search tool\",\"parameters\":{\"type\":\"object\",\"properties\":{\"after_previous\":{\"type\":\"boolean\",\"description\":\"Set true to run this call only after every earlier call of this reply has finished.\"}}}},{\"name\":\"linear__list_issues\",\"description\":\"the linear__list_issues tool\",\"parameters\":{\"type\":\"object\",\"properties\":{\"after_previous\":{\"type\":\"boolean\",\"description\":\"Set true to run this call only after every earlier call of this reply has finished.\"}}}},{\"name\":\"docs__search\",\"description\":\"the docs__search tool\",\"parameters\":{\"type\":\"object\",\"properties\":{\"after_previous\":{\"type\":\"boolean\",\"description\":\"Set true to run this call only after every earlier call of this reply has finished.\"}}}},{\"name\":\"read_output\",\"description\":\"Read a kept output page by page: the output of a command, or a long tool result, whose result named its id (o- and 16 hex characters). You get its lines, then a last line that says where the next page starts. The same id and offset give the same page: a third call in a row with the same arguments, after two identical results, is not run. Example: {\\\"id\\\":\\\"o-3f9c2a17b0d4e8a1\\\",\\\"offset\\\":2001}\",\"parameters\":{\"type\":\"object\",\"properties\":{\"id\":{\"type\":\"string\",\"description\":\"The output id from the result that kept it\"},\"offset\":{\"type\":\"integer\",\"description\":\"First line, from 1; -50 = the last 50 lines\"},\"limit\":{\"type\":\"integer\",\"description\":\"Most lines (at most 2000)\"},\"after_previous\":{\"type\":\"boolean\",\"description\":\"Set true to run this call only after every earlier call of this reply has finished.\"}},\"required\":[\"id\"],\"additionalProperties\":false}}]]"
    );
}
