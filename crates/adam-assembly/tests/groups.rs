//! Tool groups and their loaders: a server whose `when` did not match the task is one group, loaded
//! by `load__group__<server>`; its tools are withheld until the task holds a keyword or the run
//! loads the group, and its instructions are the group's note.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::Assembly;
use adam_core::RunId;
use adam_llm_agent::{Conversation, MESSAGE_KIND, ToolSet, user_message};
use adam_model::{MockModel, ModelRequest, ToolCall};
use adam_runtime::{Inbound, RunView, SessionConfig, SessionHeader};
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

/// The same agent with a `tools_loading` key in its file.
fn assembly_loading(mock: &Arc<MockModel>, mcp: &str, loading: &str) -> Assembly {
    let front = format!("name: root\n{TOOLS}\ntools_loading: {loading}");
    common::def(&[
        (
            "agent/instructions.md",
            &common::instructions(&front, "You are root."),
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
    let (requests, view) =
        run_task(&mock, &assembly(&mock, MCP), "please fix the linear ticket").await;
    assert_eq!(requests.len(), 3, "two calls and the answer");
    let state = state_of(&view);
    assert!(
        state.tools_appended.is_empty(),
        "a group the task loads is in the first request, so no turn records a growth: {:?}",
        state.tools_appended
    );
    assert_eq!(
        state
            .loaded
            .iter()
            .map(|one| one.loader.as_str())
            .collect::<Vec<_>>(),
        ["load__group__linear"],
        "the task-loaded group is journaled, for the recap"
    );
    assert_eq!(
        tool_names(&requests[0]),
        [
            "github__search",
            "linear__list_issues",
            "load__group__docs",
            "load__group__linear",
            "read_output"
        ],
        "a tool the task's keyword loaded stands where the agent's own list puts it, not at the end"
    );
    for request in &requests {
        assert_eq!(
            names(request),
            [
                "github__search",
                "linear__list_issues",
                "load__group__docs",
                "load__group__linear",
                "read_output"
            ],
            "the eager server, the one whose keyword the task holds, and the loaders, at every turn"
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
        [
            "github__search",
            "load__group__docs",
            "load__group__linear",
            "read_output"
        ]
    );
    assert!(
        !has(&requests[0], "docs__search"),
        "a triggered server's tool is withheld: {:?}",
        names(&requests[0])
    );
}

#[tokio::test]
async fn an_eager_folder_keeps_a_when_servers_tools_in_every_request() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let assembly = assembly_loading(&mock, MCP, "eager");
    assert_eq!(
        assembly.info()[0].tools_loading,
        adam_agent_fs::ToolsLoading::Eager,
        "the setting is observable in the assembly"
    );
    assert!(
        assembly.info()[0].when.is_empty(),
        "an eager folder withholds nothing, so no server is a keyword-triggered group: {:?}",
        assembly.info()[0].when
    );
    let (requests, view) = run_task(&mock, &assembly, "fix a typo").await;
    assert_eq!(requests.len(), 1, "the answer alone");
    assert_eq!(
        names(&requests[0]),
        [
            "docs__search",
            "github__search",
            "linear__list_issues",
            "read_output"
        ],
        "an eager folder withholds nothing: no loader, and both `when` servers' tools are offered"
    );
    let state = state_of(&view);
    assert!(
        state.tools_appended.is_empty() && state.loaded.is_empty(),
        "nothing was withheld, so nothing was loaded: {:?} {:?}",
        state.tools_appended,
        state.loaded
    );
}

#[tokio::test]
async fn an_empty_task_loads_only_the_eager_servers() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "").await;
    assert_eq!(
        names(&requests[0]),
        [
            "github__search",
            "load__group__docs",
            "load__group__linear",
            "read_output"
        ]
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
            "load__group__docs",
            "load__group__linear",
            "read_output"
        ]
    );
}

#[tokio::test]
async fn a_call_to_a_group_that_was_not_loaded_is_refused_and_names_the_loader() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "docs__search")])
        .push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(
        names(&requests[0]),
        [
            "github__search",
            "load__group__docs",
            "load__group__linear",
            "read_output"
        ]
    );
    let last = requests[1].messages.last().unwrap();
    let text = format!("{last:?}");
    assert!(text.contains("not loaded for this run"), "{text}");
    assert!(
        text.contains("the group `docs`"),
        "the refusal names the group, not only the tool: {text}"
    );
    assert!(
        text.contains("load__group__docs"),
        "the refusal names the loader: {text}"
    );
    assert!(
        text.contains("docs, documentation"),
        "the refusal names the keywords: {text}"
    );
    assert!(
        !text.contains("docs__search-out"),
        "the tool did not run: {text}"
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
        ["load__group__docs", "read_output"],
        "the parent's task does not select the child's server"
    );
    assert_eq!(
        child_tools("fix a typo", "read the docs").await,
        ["docs__search", "load__group__docs", "read_output"],
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

// --- the loaders: a group loads during the run ------------------------------------------------

/// The tools a request offered, by name, in the order the model saw them.
fn tool_names(request: &ModelRequest) -> Vec<String> {
    request.tools.iter().map(|t| t.name.clone()).collect()
}

fn state_of(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

/// A call to a group's loader appends the group's tools to the **next** request, once, and the
/// state records the loader and the turn the head moved. The tools array is the one before it plus
/// a suffix: a load never inserts into the cached start of a request (ADR 0044, decision 1).
#[tokio::test]
async fn a_group_load_appends_its_tools_and_records_the_turn() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_tool_calls(vec![call("c2", "docs__search")])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(requests.len(), 3, "the load, the call and the answer");
    assert_eq!(
        tool_names(&requests[0]),
        [
            "github__search",
            "load__group__docs",
            "load__group__linear",
            "read_output"
        ],
        "the group's tools are not shown before the run loads it"
    );
    assert_eq!(
        tool_names(&requests[1]),
        [
            "github__search",
            "load__group__docs",
            "load__group__linear",
            "read_output",
            "docs__search"
        ],
        "the next request is the one before it plus the group's tools, at the end"
    );
    assert_eq!(
        &requests[1].tools[..requests[0].tools.len()],
        &requests[0].tools[..],
        "the array grew by a suffix, never by an insertion"
    );
    assert_eq!(
        requests[2].tools, requests[1].tools,
        "the tools do not move again, and the group's tool appears once"
    );
    let state = state_of(&view);
    assert_eq!(state.loaded.len(), 1, "{:?}", state.loaded);
    assert_eq!(state.loaded[0].loader, "load__group__docs");
    assert_eq!(state.loaded[0].tools, ["docs__search"]);
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

// --- a session's own tools, and the loader of the group they select -----------------------------

/// A round's session header, as `subagent_start` writes one (ADR 0054, slice 3): the config the
/// round runs with. No parent, and the round closes when it ends.
fn narrowed_session(tools: &[&str]) -> SessionHeader {
    // `SessionConfig` is `#[non_exhaustive]`: another crate sets its fields one by one.
    #[allow(clippy::field_reassign_with_default)]
    let mut config = SessionConfig::default();
    config.tools = Some(tools.iter().map(|tool| (*tool).to_owned()).collect());
    SessionHeader {
        id: RunId::new(),
        agent: "root".into(),
        description: "a narrowed round".into(),
        keep_open: false,
        closed: false,
        closing: false,
        deleted: false,
        round: 1,
        root: None,
        workspace: None,
        visible: false,
        config: Some(config),
        spent: 0,
        editing: false,
        edits: Vec::new(),
    }
}

/// Run `task` as a round of a session narrowed to `tools`: the requests and the view. The header is
/// the one the session tools write, so the loop reads the same header a real round carries.
async fn run_narrowed(
    mock: &Arc<MockModel>,
    assembly: &Assembly,
    task: &str,
    tools: &[&str],
) -> (Vec<ModelRequest>, RunView) {
    let runtime = common::runtime(assembly);
    let payload = json!({"text": task, "session": narrowed_session(tools)});
    let run = runtime
        .start("root", Inbound::new(MESSAGE_KIND, payload), None)
        .await
        .unwrap();
    let worker = common::spawn_worker(&runtime);
    let view = common::wait_done(&runtime, run).await;
    worker.stop().await;
    (mock.requests(), view)
}

/// A lazy `docs` server whose two tools the agent's folder gives it, for the session tests.
fn docs_assembly(mock: &Arc<MockModel>) -> Assembly {
    const MCP_DOCS: &str = r#"{"mcpServers": {
      "docs": {"type": "http", "url": "https://mcp.example/docs", "tools": ["search", "read"],
               "when": {"keywords": ["docs"]}}
    }}"#;
    common::def(&[
        (
            "agent/instructions.md",
            &common::instructions("name: root\ntools: ['docs__*']", "You are root."),
        ),
        ("agent/mcp.json", MCP_DOCS),
    ])
    .mcp_tools("root", common::tools(&["docs__search", "docs__read"]))
    .bind(ToolSet::new())
    .unwrap()
    .model(mock.clone(), "default")
    .unwrap()
}

/// A session narrowed to a server's tools keeps that server's group loader (ANR-1304): `tools:
/// ["docs__*"]` holds the group's tools back, as lazy does, and the loader withholds them until it
/// is called, so the session can load the group it asked for. The load appends the tools the list
/// selects and nothing more (ADR 0044, decision 1; ADR 0066).
#[tokio::test]
async fn a_session_narrowed_to_a_servers_tools_keeps_its_group_loader() {
    let mock = Arc::new(MockModel::new());
    let assembly = docs_assembly(&mock);
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_tool_calls(vec![call("c2", "docs__read")])
        .push_text("done");
    let (requests, view) = run_narrowed(&mock, &assembly, "fix a typo", &["docs__*"]).await;
    assert_eq!(requests.len(), 3, "the load, the call and the answer");
    assert!(
        has(&requests[0], "load__group__docs"),
        "the list selects the server's tools, so it keeps its loader: {:?}",
        tool_names(&requests[0])
    );
    assert!(
        !has(&requests[0], "docs__search") && !has(&requests[0], "docs__read"),
        "the group's tools are withheld until the group is loaded: {:?}",
        tool_names(&requests[0])
    );
    assert_eq!(
        tool_names(&requests[1]),
        [
            "load__group__docs",
            "read_output",
            "docs__search",
            "docs__read"
        ],
        "the load appended the group's tools, after every tool the session had"
    );
    assert_eq!(
        &requests[1].tools[..requests[0].tools.len()],
        &requests[0].tools[..],
        "the array grew by a suffix, never by an insertion"
    );
    let state = state_of(&view);
    assert_eq!(state.loaded.len(), 1, "{:?}", state.loaded);
    assert_eq!(state.loaded[0].loader, "load__group__docs");
    assert_eq!(state.loaded[0].tools, ["docs__search", "docs__read"]);
    let text = format!("{:?}", requests[2].messages);
    assert!(
        text.contains("docs__read-out"),
        "the loaded tool ran: {text}"
    );
    assert!(
        !text.contains("is not offered to this session"),
        "nothing the list selects was refused: {text}"
    );
}

/// A session whose list names none of a server's tools keeps no loader for it (ANR-1304): a loader
/// that could load nothing is not handed out, and a call to it is refused without running. It is
/// the guard on the rule above: keeping every loader a server has would hand out one that loads
/// nothing.
#[tokio::test]
async fn a_session_that_selects_nothing_of_a_server_keeps_no_loader() {
    let mock = Arc::new(MockModel::new());
    let assembly = docs_assembly(&mock);
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_text("done");
    let (requests, view) = run_narrowed(&mock, &assembly, "fix a typo", &["read_output"]).await;
    assert_eq!(requests.len(), 2, "the refused call and the answer");
    assert!(
        !has(&requests[0], "load__group__docs"),
        "the list names nothing of `docs`, so its loader is not offered: {:?}",
        tool_names(&requests[0])
    );
    let refused = requests[1].messages.last().expect("the refusal").text();
    assert!(
        refused.contains("`load__group__docs` is not offered to this session"),
        "{refused}"
    );
    assert!(
        !refused.contains("Loaded the group"),
        "the loader did not run: {refused}"
    );
    let state = state_of(&view);
    assert!(state.loaded.is_empty(), "{:?}", state.loaded);
}

/// A loader of a name no group has is an unknown tool, and nothing moves.
#[tokio::test]
async fn a_call_to_an_unknown_loader_is_refused() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "load__group__nope", json!({}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    let last = requests[1].messages.last().expect("the result");
    let text = last.text();
    assert!(text.contains("load__group__nope"), "{text}");
    let state = state_of(&view);
    assert!(state.loaded.is_empty(), "{:?}", state.loaded);
    assert!(state.tools_appended.is_empty());
    assert_eq!(requests[1].tools, requests[0].tools);
}

/// A group the task already loaded is refused: its tools are in the list, and loading it again
/// would move them.
#[tokio::test]
async fn a_loader_of_a_group_the_task_loaded_is_refused() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "load__group__linear", json!({}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix the linear ticket").await;
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("already loaded"), "{text}");
    assert!(
        text.contains("linear__list_issues"),
        "it names them: {text}"
    );
    let state = state_of(&view);
    assert_eq!(
        state
            .loaded
            .iter()
            .map(|one| one.loader.as_str())
            .collect::<Vec<_>>(),
        ["load__group__linear"],
        "the task-loaded group is journaled, for the recap"
    );
    assert!(state.tools_appended.is_empty());
    assert_eq!(requests[1].tools, requests[0].tools);
}

/// A server with no `when` is eager: it has no loader, and its tool is in the first request.
#[tokio::test]
async fn an_eager_server_has_no_loader() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert!(
        requests[0].tools.iter().any(|t| t.name == "github__search"),
        "the eager server's tool is already there"
    );
    assert!(
        !has(&requests[0], "load__group__github"),
        "an eager server has nothing to load: {:?}",
        names(&requests[0])
    );
}

/// A triggered server whose tools the agent's `tools:` did not select has no loader: a group left
/// empty reveals nothing and offers nothing.
#[tokio::test]
async fn a_group_with_no_allowed_member_has_no_loader() {
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
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly, "fix a typo").await;
    assert_eq!(names(&requests[0]), ["github__search", "read_output"]);
    assert!(
        !has(&requests[0], "load__group__docs"),
        "a group with no member the agent has gets no loader: {:?}",
        names(&requests[0])
    );
}

/// A loader the run called is refused the second time: the list is the state, and a second load
/// would append the same tools twice.
#[tokio::test]
async fn a_loader_called_twice_is_refused() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_tool_calls(vec![call_with("c2", "load__group__docs", json!({}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    let text = requests[2].messages.last().expect("the result").text();
    assert!(text.contains("already loaded"), "{text}");
    assert_eq!(
        requests[2].tools, requests[1].tools,
        "the refused load did not append anything"
    );
    let state = state_of(&view);
    assert_eq!(state.loaded.len(), 1);
    assert_eq!(state.tools_appended, [1], "one turn, not two");
}

/// A load costs one full re-prefill, and the result says so; no number is invented (the cost is
/// the next call's `cached_input_tokens`, which only the provider reports).
#[tokio::test]
async fn a_group_load_says_the_next_call_re_reads_the_prompt() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("re-reads"), "{text}");
    assert!(text.contains("slower"), "it says the cost in time: {text}");
    assert!(!text.contains("token"), "the tool invents no cost: {text}");
    assert!(
        !text.contains("saves") && !text.contains("faster"),
        "it claims no saving: {text}"
    );
    // The fixture's server and tools carry no digit, so any digit would be an invented figure.
    assert!(
        !text.chars().any(|c| c.is_ascii_digit()),
        "the tool invents no number: {text}"
    );
    assert!(text.contains("docs__search"), "it names the tools: {text}");
}

/// The load is a step of the run, and the step's output names the group and the tools it added:
/// the cost is visible to the person, not only in the model's history.
#[tokio::test]
async fn the_load_is_a_step_that_names_the_group() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_text("done");
    let assembly = assembly(&mock, MCP);
    let store: adam_core::DynStore = Arc::new(adam_core::MemoryStore::new());
    let sink = adam_runtime::CollectingSink::new();
    let runtime = assembly
        .register(adam_runtime::Runtime::builder(store))
        .poll_interval(std::time::Duration::from_millis(20))
        .event_sink(sink.clone())
        .build();
    let run = runtime
        .start("root", user_message("fix a typo"), None)
        .await
        .unwrap();
    let worker = common::spawn_worker(&runtime);
    common::wait_done(&runtime, run).await;
    worker.stop().await;
    let steps: Vec<adam_runtime::StepEvent> = sink
        .events_for(run)
        .into_iter()
        .filter_map(|event| match event {
            adam_runtime::RunEvent::Step(step) => Some(step),
            _ => None,
        })
        .collect();
    let load = steps
        .iter()
        .find(|step| step.id == "tool:c1" && step.output.is_some())
        .expect("the load's step ended");
    assert_eq!(load.label, "Load a group of tools");
    let output = load.output.as_ref().expect("the step ended");
    assert!(!output.error, "{output:?}");
    assert!(output.text.contains("docs"), "{output:?}");
    assert!(output.text.contains("docs__search"), "{output:?}");
}

/// A folder with no triggered server has no loader: the tools are there only when there is
/// something to load.
#[tokio::test]
async fn a_folder_with_no_triggered_server_has_no_loader() {
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let (requests, _) = run_task(&mock, &assembly(&mock, MCP_NO_WHEN), "fix a typo").await;
    assert!(
        has(&requests[0], "github__search"),
        "the eager server's tool is there: {:?}",
        names(&requests[0])
    );
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|t| t.name.starts_with("load__")),
        "{:?}",
        names(&requests[0])
    );
}

/// A registered tool whose name starts with `load__` is refused at bind: the loaders reserve the
/// prefix, and an MCP tool is `<server>__<tool>`, so the names would collide.
#[test]
fn a_folder_tool_named_load_is_refused_at_bind() {
    let def = common::def(&[
        (
            "agent/instructions.md",
            &common::instructions(
                "name: root\ntools: ['load__group__github']",
                "You are root.",
            ),
        ),
        ("agent/mcp.json", MCP),
    ]);
    let error = def
        .mcp_tools("root", common::tools(&["github__search"]))
        .bind(ToolSet::new().tool(common::stub("load__group__github")))
        .unwrap_err();
    assert!(
        matches!(error, adam_assembly::Error::ReservedLoaderName { .. }),
        "{error}"
    );
}

/// Two loads in one turn: the tools array grows once, at the next request, by both groups in
/// the order the calls ran, and the state records that one turn once.
#[tokio::test]
async fn two_loads_in_one_turn_append_both_in_order_and_record_the_turn_once() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![
        call_with("c1", "load__group__docs", json!({})),
        // `after_previous` puts the second load in its own segment, after the first: its result
        // then carries the whole list, the first group included, as the sequential path does.
        call_with("c2", "load__group__linear", json!({"after_previous": true})),
    ])
    .push_text("done");
    let (requests, view) = run_task(&mock, &assembly(&mock, MCP), "fix a typo").await;
    assert_eq!(
        tool_names(&requests[1]),
        [
            "github__search",
            "load__group__docs",
            "load__group__linear",
            "read_output",
            "docs__search",
            "linear__list_issues"
        ],
        "both groups' tools, in the order the calls ran"
    );
    let state = state_of(&view);
    assert_eq!(
        state
            .loaded
            .iter()
            .map(|one| one.loader.as_str())
            .collect::<Vec<_>>(),
        ["load__group__docs", "load__group__linear"]
    );
    assert_eq!(
        state.tools_appended,
        [1],
        "the array grew once, at one request"
    );
}

/// The loaded set is carried into a run that continues the conversation: its first request offers
/// the tools the run before ended with, at the end, and loading them again is refused.
#[tokio::test]
async fn a_continued_run_keeps_the_groups_the_run_loaded() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
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
        state.loaded[0].loader, "load__group__docs",
        "the continuation carries the loaded set"
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
        "the continuation's first request ends with the loaded group's tools"
    );
}

/// A load offers only the tools this agent's `tools:` selected: the server may offer more, and the
/// agent was never given them, so appending them would show tools no call could run.
#[tokio::test]
async fn a_group_holds_only_the_tools_the_agent_selects() {
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
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_text("done");
    let (requests, view) = run_task(&mock, &assembly, "fix a typo").await;
    assert_eq!(
        tool_names(&requests[1]),
        ["load__group__docs", "read_output", "docs__search"],
        "the selected tool is appended; the one `tools:` left out is not"
    );
    let text = requests[1].messages.last().expect("the result").text();
    assert!(text.contains("docs__search"), "{text}");
    assert!(!text.contains("docs__read"), "{text}");
    assert_eq!(state_of(&view).loaded[0].tools, ["docs__search"]);
}

/// A tool the agent's `tools:` did not select is in no loader, and a call to it is refused: lazy
/// never widens access, and a forbidden tool's existence is not revealed by a loader.
#[tokio::test]
async fn a_forbidden_tool_is_in_no_loader_and_is_refused() {
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
    mock.push_tool_calls(vec![call_with("c1", "load__group__docs", json!({}))])
        .push_tool_calls(vec![call("c2", "docs__read")])
        .push_text("done");
    let (requests, _) = run_task(&mock, &assembly, "fix a typo").await;
    let load = requests[1]
        .messages
        .last()
        .expect("the load's result")
        .text();
    assert!(load.contains("docs__search"), "{load}");
    assert!(
        !load.contains("docs__read"),
        "the forbidden tool is not in the load's result: {load}"
    );
    let refused = requests[2].messages.last().expect("the refusal").text();
    assert!(refused.contains("docs__read"), "{refused}");
    assert!(
        !refused.contains("docs__read-out"),
        "the forbidden tool did not run: {refused}"
    );
}

/// A `when` server whose name is a group name binds, and gets a loader: `my_server` is legal in
/// `mcp.json` and a group name.
#[tokio::test]
async fn a_when_server_named_my_server_binds() {
    const MCP_UNDERSCORE: &str = r#"{"mcpServers": {
      "my_server": {"type": "http", "url": "https://mcp.example/x", "tools": ["echo"],
                    "when": {"keywords": ["x"]}}
    }}"#;
    let mock = Arc::new(MockModel::new());
    mock.push_text("done");
    let assembly = common::def(&[
        (
            "agent/instructions.md",
            &common::instructions("name: root\ntools: ['my_server__echo']", "You are root."),
        ),
        ("agent/mcp.json", MCP_UNDERSCORE),
    ])
    .mcp_tools("root", common::tools(&["my_server__echo"]))
    .bind(ToolSet::new())
    .unwrap()
    .model(mock.clone(), "default")
    .unwrap();
    let (requests, _) = run_task(&mock, &assembly, "fix a typo").await;
    assert!(
        has(&requests[0], "load__group__my_server"),
        "{:?}",
        names(&requests[0])
    );
}

/// A tool in two groups is refused at bind: a load must append it once, from one group.
#[test]
fn a_tool_in_two_groups_is_refused_at_bind() {
    let given: Vec<(String, adam_llm_agent::DynTool)> =
        vec![("shared".to_owned(), Arc::new(common::stub("shared")))];
    let error = adam_assembly::Groups::bind(
        &adam_assembly::Origin {
            agent: "root".into(),
            file: "agent/instructions.md".into(),
        },
        vec![
            adam_assembly::Group::new("a", "the a group", vec!["shared".to_owned()]),
            adam_assembly::Group::new("b", "the b group", vec!["shared".to_owned()]),
        ],
        &given,
    )
    .unwrap_err();
    assert!(
        matches!(error, adam_assembly::Error::GroupOverlap { .. }),
        "{error}"
    );
    assert!(
        error.to_string().contains("belongs to one group"),
        "{error}"
    );
}

/// A registered `load__` tool is refused at bind even when the agent does not select it: the
/// catalogue is the host's, and the loaders reserve the prefix.
#[test]
fn a_registered_load_tool_is_refused_even_unselected() {
    let def = common::def(&[(
        "agent/instructions.md",
        &common::instructions("name: root\ntools: ['other']", "You are root."),
    )]);
    let error = def
        .bind(
            ToolSet::new()
                .tool(common::stub("other"))
                .tool(common::stub("load__x")),
        )
        .unwrap_err();
    assert!(
        matches!(error, adam_assembly::Error::ReservedLoaderName { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("reserved"), "{error}");
}

/// A subagent is called as a tool named after it, so one named `load__` would collide with a
/// loader: refused at bind.
#[test]
fn a_subagent_named_load_is_refused() {
    let def = common::def(&[
        (
            "agent/instructions.md",
            &common::instructions("name: root", "You are root."),
        ),
        (
            "agent/subagents/load__x/instructions.md",
            &common::instructions("description: Helps.", "You help."),
        ),
    ]);
    let error = def.bind(ToolSet::new()).unwrap_err();
    assert!(
        matches!(error, adam_assembly::Error::ReservedLoaderName { .. }),
        "{error}"
    );
}
