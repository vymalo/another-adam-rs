//! `mcp.json` `when`: a server's tools are decided from the task before the run's first model
//! call and never again; a call to a server that was not loaded is refused, and its instructions
//! do not reach the prompt.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::Assembly;
use adam_llm_agent::{Conversation, ToolSet, user_message};
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

/// A call with arguments: a subagent's `message`, for instance.
fn call_with(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
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
        .push_tool_calls(vec![call("c2", "github__search")])
        .push_text("done");
    let (requests, _) =
        run_task(&mock, &assembly(&mock, MCP), "please fix the linear ticket").await;
    assert_eq!(requests.len(), 3, "two calls and the answer");
    for request in &requests {
        assert_eq!(
            names(request),
            [
                "github__search",
                "linear__list_issues",
                "read_output",
                "use_server"
            ],
            "the eager server, the one whose keyword the task holds, and the loader, at every turn"
        );
    }
    for pair in requests.windows(2) {
        assert_eq!(
            head(&pair[0]),
            head(&pair[1]),
            "the system prompt and the tools are the same bytes at every request"
        );
    }
    // The tools ran: the last request holds both answers.
    let text = format!("{:?}", requests[2].messages);
    assert!(text.contains("linear__list_issues-out"), "{text}");
    assert!(text.contains("github__search-out"), "{text}");
}

#[tokio::test]
async fn a_server_with_no_when_is_eager() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(
        names(&requests[0]),
        ["github__search", "read_output", "use_server"]
    );
}

#[tokio::test]
async fn an_empty_task_loads_only_the_eager_servers() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "").await;
    assert_eq!(
        names(&requests[0]),
        ["github__search", "read_output", "use_server"]
    );
}

#[tokio::test]
async fn the_keyword_match_ignores_case() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) =
        run_task(&mock, &assembly(&mock, MCP), "PLEASE FIX THE LINEAR TICKET").await;
    assert_eq!(
        names(&requests[0]),
        [
            "github__search",
            "linear__list_issues",
            "read_output",
            "use_server"
        ]
    );
}

#[tokio::test]
async fn a_call_to_a_server_that_was_not_loaded_is_refused_and_names_the_reason() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "docs__search")])
        .push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(
        names(&requests[0]),
        ["github__search", "read_output", "use_server"]
    );
    let last = requests[1].messages.last().unwrap();
    let text = format!("{last:?}");
    assert!(text.contains("not loaded for this run"), "{text}");
    assert!(
        text.contains("the MCP server `docs`"),
        "the refusal names the server, not only the tool: {text}"
    );
    assert!(
        text.contains("docs, documentation"),
        "the refusal names the keywords: {text}"
    );
    assert!(
        !text.contains("docs__search-out"),
        "the server did not run: {text}"
    );
}

/// A tool whose name is no declared server's is refused at bind, so an unselected server's tool
/// cannot be smuggled in under a name of its own (`McpForeignTool`). The withheld path is the
/// other half: a call to the real name is refused at the call (above).
#[test]
fn a_tool_of_no_declared_server_is_refused_at_bind() {
    let def = common::def(&[
        (
            "agent/instructions.md",
            &common::instructions(&format!("name: root\n{TOOLS}"), "You are root."),
        ),
        ("agent/mcp.json", MCP),
    ]);
    let error = def
        .mcp_tools(
            "root",
            common::tools(&[
                "github__search",
                "linear__list_issues",
                "docs__search",
                "other__search",
            ]),
        )
        .bind(ToolSet::new())
        .unwrap_err();
    assert!(
        matches!(error, adam_assembly::Error::McpForeignTool { .. }),
        "{error}"
    );
}

/// A subagent has its own `mcp.json` and its own selector: it selects on **its** task, not on the
/// parent's, so a keyword the parent's task holds does not load the child's server.
#[tokio::test]
async fn a_subagent_selects_on_its_own_task_not_the_parents() {
    const CHILD_MCP: &str = r#"{"mcpServers": {
      "docs": {"type": "http", "url": "https://mcp.example/docs", "tools": ["search"],
               "when": {"keywords": ["docs"]}}
    }}"#;
    fn child_assembly(mock: &Arc<MockModel>) -> Assembly {
        common::def(&[
            (
                "agent/instructions.md",
                &common::instructions("name: root", "You are root."),
            ),
            (
                "agent/subagents/helper/instructions.md",
                &common::instructions("description: Helps.\ntools: ['docs__search']", "You help."),
            ),
            ("agent/subagents/helper/mcp.json", CHILD_MCP),
        ])
        .mcp_tools("root/helper", common::tools(&["docs__search"]))
        .bind(ToolSet::new())
        .unwrap()
        .model(mock.clone(), "default")
        .unwrap()
    }
    async fn child_tools(parent_task: &str, child_task: &str) -> Vec<String> {
        let mock = Arc::new(MockModel::new());
        mock.push_tool_calls(vec![call_with(
            "s1",
            "helper",
            json!({"message": child_task}),
        )])
        .push_text("child done")
        .push_text("parent done");
        let assembly = child_assembly(&mock);
        let runtime = common::runtime(&assembly);
        let run = runtime
            .start("root", user_message(parent_task), None)
            .await
            .unwrap();
        let worker = common::spawn_worker(&runtime);
        common::wait_done(&runtime, run).await;
        worker.stop().await;
        let requests = mock.requests();
        let child = requests
            .iter()
            .find(|r| common::plain(&r.system).as_deref() == Some("You help."))
            .expect("the child's request");
        names(child)
    }
    assert_eq!(
        child_tools("read the docs", "fix a typo").await,
        ["read_output", "use_server"],
        "the parent's task does not select the child's server"
    );
    assert_eq!(
        child_tools("fix a typo", "read the docs").await,
        ["docs__search", "read_output", "use_server"],
        "the child's own task selects it"
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

/// `AgentInfo` is what the dev reload compares and what the tests compare an embedded agent with:
/// a `when` change must make it differ, or two definitions that behave differently compare equal
/// (and a reload reports that nothing changed).
#[test]
fn a_when_change_is_visible_in_the_agent_info() {
    let mock = Arc::new(MockModel::new());
    let before = assembly(&mock, MCP);
    let after = assembly(
        &mock,
        &MCP.replace(
            r#""keywords": ["docs", "documentation"]"#,
            r#""keywords": ["docs"]"#,
        ),
    );
    assert_eq!(
        before.info()[0].when,
        [
            (
                "docs".to_owned(),
                vec!["docs".into(), "documentation".into()]
            ),
            ("linear".to_owned(), vec!["linear".into(), "ticket".into()]),
        ],
        "the triggered servers and their keywords, in name order"
    );
    assert_ne!(
        before.info(),
        after.info(),
        "a `when` change makes different agents"
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

// --- the fallback: `use_server` loads a server during the run -----------------------------------

/// The tools a request offered, by name, in the order the model saw them.
fn tool_names(request: &ModelRequest) -> Vec<String> {
    request.tools.iter().map(|t| t.name.clone()).collect()
}

fn state_of(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

/// A call to `use_server` appends the server's tools to the **next** request, once, and the state
/// records the list and the turn the head moved. The tools array is the one before it plus a
/// suffix: a load never inserts into the cached start of a request (ADR 0044, decision 1).
#[tokio::test]
async fn use_server_appends_a_triggered_servers_tools_to_the_next_call() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "use_server", json!({"name": "docs"}))])
        .push_tool_calls(vec![call("c2", "docs__search")])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(requests.len(), 3, "the load, the call and the answer");
    assert_eq!(
        tool_names(&requests[0]),
        ["github__search", "use_server", "read_output"],
        "the triggered server's tools are not shown before the run loads it"
    );
    assert_eq!(
        tool_names(&requests[1]),
        [
            "github__search",
            "use_server",
            "read_output",
            "docs__search"
        ],
        "the next request is the one before it plus the server's tools, at the end"
    );
    assert_eq!(
        &requests[1].tools[..requests[0].tools.len()],
        &requests[0].tools[..],
        "the array grew by a suffix, never by an insertion"
    );
    assert_eq!(
        requests[2].tools, requests[1].tools,
        "the tools do not move again, and the server's appears once"
    );
    let state = state_of(&view);
    assert_eq!(state.loaded_servers, ["docs"]);
    assert_eq!(
        state.tools_appended,
        [1],
        "the turn of the request that grew"
    );
    let text = format!("{:?}", requests[2].messages);
    assert!(text.contains("docs__search-out"), "the tool ran: {text}");
    assert!(
        !text.contains("not loaded for this run"),
        "the loaded tool was not refused: {text}"
    );
}

/// An unknown server is refused with the names the folder declares, and nothing moves.
#[tokio::test]
async fn use_server_refuses_an_unknown_server() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "use_server", json!({"name": "nope"}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    let last = requests[1].messages.last().expect("the result");
    let text = last.text();
    assert!(text.contains("no MCP server `nope`"), "{text}");
    assert!(text.contains("github"), "it names the servers: {text}");
    assert!(text.contains("docs"), "it names the servers: {text}");
    let state = state_of(&view);
    assert!(
        state.loaded_servers.is_empty(),
        "{:?}",
        state.loaded_servers
    );
    assert!(state.tools_appended.is_empty());
    assert_eq!(requests[1].tools, requests[0].tools);
}

/// A server the task already loaded is refused: its tools are in the list, and loading it again
/// would move them.
#[tokio::test]
async fn use_server_refuses_a_server_the_task_already_loaded() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with(
        "c1",
        "use_server",
        json!({"name": "linear"}),
    )])
    .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix the linear ticket").await;
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("already loaded"), "{text}");
    assert!(
        text.contains("linear__list_issues"),
        "it names them: {text}"
    );
    let state = state_of(&view);
    assert!(state.loaded_servers.is_empty());
    assert!(state.tools_appended.is_empty());
    assert_eq!(requests[1].tools, requests[0].tools);
}

/// A server with no `when` is eager: there is nothing to load, and the tool says so.
#[tokio::test]
async fn use_server_refuses_an_eager_server() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with(
        "c1",
        "use_server",
        json!({"name": "github"}),
    )])
    .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("always loaded"), "{text}");
    assert!(
        requests[0].tools.iter().any(|t| t.name == "github__search"),
        "the eager server's tool is already there"
    );
    let state = state_of(&view);
    assert!(state.loaded_servers.is_empty());
    assert_eq!(requests[1].tools, requests[0].tools);
}

/// A triggered server that offered no tool when the agent started (an `optional` server that was
/// not reachable, or an allow-list that selects nothing) is refused, and nothing is recorded: the
/// run does not wait for a server that will never answer.
#[tokio::test]
async fn use_server_refuses_a_server_that_offered_no_tools() {
    const UNREACHABLE: &str = r#"{"mcpServers": {
      "github": {"type": "http", "url": "https://mcp.example/github", "tools": ["search"]},
      "docs": {"type": "http", "url": "https://mcp.example/docs", "tools": ["search"],
               "when": {"keywords": ["docs"]}}
    }}"#;
    let mock = Arc::new(MockModel::new());
    let assembly = common::def(&[
        (
            "agent/instructions.md",
            &common::instructions("name: root\ntools: ['github__search']", "You are root."),
        ),
        ("agent/mcp.json", UNREACHABLE),
    ])
    .mcp_tools("root", common::tools(&["github__search"]))
    .bind(ToolSet::new())
    .unwrap()
    .model(mock.clone(), "default")
    .unwrap();
    mock.push_tool_calls(vec![call_with("c1", "use_server", json!({"name": "docs"}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly, "fix a typo").await;
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("offered no tools"), "{text}");
    assert!(text.contains("`docs`"), "{text}");
    let state = state_of(&view);
    assert!(state.loaded_servers.is_empty());
    assert!(state.tools_appended.is_empty());
    assert_eq!(requests[1].tools, requests[0].tools);
}

/// A server the run loaded is refused the second time: the list is the state, and a second load
/// would append the same tools twice.
#[tokio::test]
async fn use_server_refuses_a_server_it_already_loaded() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "use_server", json!({"name": "docs"}))])
        .push_tool_calls(vec![call_with("c2", "use_server", json!({"name": "docs"}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    let text = requests[2].messages.last().expect("the result").text();
    assert!(text.contains("already loaded"), "{text}");
    assert_eq!(
        requests[2].tools, requests[1].tools,
        "the refused load did not append anything"
    );
    let state = state_of(&view);
    assert_eq!(state.loaded_servers, ["docs"]);
    assert_eq!(state.tools_appended, [1], "one turn, not two");
}

/// A load costs one full re-prefill, and the result says so; no number is invented (the cost is
/// the next call's `cached_input_tokens`, which only the provider reports).
#[tokio::test]
async fn use_server_says_the_next_call_re_reads_the_prompt() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "use_server", json!({"name": "docs"}))])
        .push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("re-reads"), "{text}");
    assert!(!text.contains("token"), "the tool invents no cost: {text}");
    // The step the person sees carries the same words (the loop's step output is the result).
    assert!(text.contains("docs__search"), "it names the tools: {text}");
}

/// A folder with no triggered server has no `use_server`: the fixed tool is there only when there
/// is something to load.
#[tokio::test]
async fn a_folder_with_no_triggered_server_has_no_use_server() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP_NO_WHEN), "fix a typo").await;
    assert!(
        !has(&requests[0], "use_server"),
        "{:?}",
        names(&requests[0])
    );
}

/// The list is carried into a run that continues the conversation: its first request offers the
/// tools the run before ended with, at the end, and loading them again is refused.
#[tokio::test]
async fn a_continued_run_keeps_the_servers_the_run_loaded() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "use_server", json!({"name": "docs"}))])
        .push_text("first")
        .push_text("second");
    let assembly = assembly(&mock, MCP);
    let runtime = common::runtime(&assembly);
    let run = runtime
        .start("root", user_message("fix a typo"), None)
        .await
        .unwrap();
    let worker = common::spawn_worker(&runtime);
    common::wait_done(&runtime, run).await;
    let next = runtime
        .start_continuing("root", user_message("and now?"), None, run)
        .await
        .unwrap();
    let view = common::wait_done(&runtime, next).await;
    worker.stop().await;
    let state = state_of(&view);
    assert_eq!(
        state.loaded_servers,
        ["docs"],
        "the continuation carries the list"
    );
    let requests = mock.requests();
    let first = requests
        .iter()
        .find(|r| r.messages.len() == 1)
        .expect("the first request of the first run");
    let after = requests.last().unwrap();
    let before: Vec<String> = first
        .tools
        .iter()
        .filter(|t| t.name != "docs__search")
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(
        tool_names(after),
        before
            .into_iter()
            .chain(["docs__search".to_owned()])
            .collect::<Vec<_>>(),
        "the continuation's first request ends with the loaded server's tools"
    );
}

/// A load offers only the tools this agent's `tools:` selected: the server may offer more, and the
/// agent was never given them, so appending them would show tools no call could run.
#[tokio::test]
async fn use_server_loads_only_the_tools_the_agent_selects() {
    const MCP_TWO: &str = r#"{"mcpServers": {
      "docs": {"type": "http", "url": "https://mcp.example/docs", "tools": ["search", "read"],
               "when": {"keywords": ["docs"]}}
    }}"#;
    let mock = Arc::new(MockModel::new());
    let assembly = common::def(&[
        (
            "agent/instructions.md",
            &common::instructions("name: root\ntools: ['docs__search']", "You are root."),
        ),
        ("agent/mcp.json", MCP_TWO),
    ])
    .mcp_tools("root", common::tools(&["docs__search", "docs__read"]))
    .bind(ToolSet::new())
    .unwrap()
    .model(mock.clone(), "default")
    .unwrap();
    mock.push_tool_calls(vec![call_with("c1", "use_server", json!({"name": "docs"}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly, "fix a typo").await;
    assert_eq!(
        tool_names(&requests[1]),
        ["use_server", "read_output", "docs__search"],
        "the selected tool is appended; the one `tools:` left out is not"
    );
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("docs__search"), "{text}");
    assert!(!text.contains("docs__read"), "{text}");
    assert_eq!(state_of(&view).loaded_servers, ["docs"]);
}
