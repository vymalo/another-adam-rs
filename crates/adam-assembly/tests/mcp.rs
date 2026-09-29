//! `mcp.json` tools (feature `mcp`): each agent connects to the servers of its own file, and the
//! tools are called from runs on `MockModel` through the runtime, against a real MCP server
//! (`adam-mcp-testkit`, over streamable HTTP). The memory store always, PostgreSQL when
//! `ADAM_TEST_POSTGRES_URL` is set. Every case has agent names of its own, so cases that share a
//! database never claim each other's runs.
#![cfg(feature = "mcp")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use adam_assembly::{AgentDef, Assembly, Error, ToolClash};
use adam_core::{DynStore, MemoryStore, RunId, Store};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::{FnTool, ToolError, ToolOutput, ToolSet, user_message};
use adam_mcp::McpPolicy;
use adam_mcp_testkit::TestHttpServer;
use adam_model::{Message, MockModel, ModelError, ModelRequest, ToolCall};
use adam_runtime::{CollectingSink, Runtime};
use common::{instructions, spawn_worker, wait_done};
use serde_json::{Value, json};

const TOKEN: &str = "root-tok-3f9a7c21-secret";

// --- harness -------------------------------------------------------------------------------

async fn stores() -> Vec<(&'static str, DynStore)> {
    let mut all: Vec<(&'static str, DynStore)> = vec![("memory", Arc::new(MemoryStore::new()))];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        store.migrate().await.expect("migrate");
        all.push(("postgres", Arc::new(store)));
    }
    all
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new().0.simple())
}

type Files = Vec<(String, String)>;

fn def_of(files: &Files) -> AgentDef {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    common::def(&refs)
}

/// The `mcp.json` of one server `linear` at `url`, with the token from `${var}`, and `extra`
/// members (`"tools": ["echo"]`).
fn mcp_json(url: &str, var: &str, extra: &str) -> String {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(", {extra}")
    };
    format!(
        r#"{{"mcpServers": {{"linear": {{"type": "http", "url": "{url}",
            "headers": {{"Authorization": "Bearer ${{{var}}}"}}{extra}}}}}}}"#
    )
}

/// A root with one `mcp.json`.
fn root_files(root: &str, frontmatter: &str, mcp: &str) -> Files {
    vec![
        (
            "agent/instructions.md".into(),
            instructions(&format!("name: {root}\n{frontmatter}"), "You are the root."),
        ),
        ("agent/mcp.json".into(), mcp.into()),
    ]
}

fn policy() -> McpPolicy {
    McpPolicy::default()
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn assemble(def: AgentDef, model: &Arc<MockModel>) -> Assembly {
    def.bind(ToolSet::new())
        .unwrap()
        .model(model.clone(), "default")
        .unwrap()
}

/// The result of the tool call `id` in the last message of the request.
fn last_tool_result(request: &ModelRequest, id: &str) -> (String, bool) {
    match request.messages.last().unwrap() {
        Message::Tool {
            call_id,
            content,
            is_error,
        } if call_id == id => (content.clone(), *is_error),
        other => panic!("the last message is not the result of `{id}`: {other:?}"),
    }
}

fn info_tools(assembly: &Assembly, agent: &str) -> Vec<String> {
    assembly
        .info()
        .iter()
        .find(|i| i.name == agent)
        .unwrap()
        .tools
        .clone()
}

// --- binding -------------------------------------------------------------------------------

#[tokio::test]
async fn each_agent_gets_its_own_mcp_json() {
    // Two directories, both with a server called `linear`, at different servers with different
    // tokens: two connections, each with its own credentials, and no sharing between them.
    let root_server = TestHttpServer::start(Some("tok-root-aaaa1111")).await;
    let research_server = TestHttpServer::start(Some("tok-research-bbbb2222")).await;
    let root = uniq("root");
    let mut files = root_files(
        &root,
        "tools: ['linear__*']",
        &mcp_json(&root_server.url(), "ROOT_TOKEN", r#""tools": ["echo"]"#),
    );
    files.push((
        "agent/subagents/researcher/instructions.md".into(),
        instructions(
            "description: Researches.\ntools: ['linear__*']",
            "You research.",
        ),
    ));
    files.push((
        "agent/subagents/researcher/mcp.json".into(),
        mcp_json(
            &research_server.url(),
            "RESEARCH_TOKEN",
            r#""tools": ["pid", "echo"]"#,
        ),
    ));
    let def = def_of(&files)
        .env("ROOT_TOKEN", "tok-root-aaaa1111")
        .env("RESEARCH_TOKEN", "tok-research-bbbb2222")
        .connect_mcp(&policy())
        .await
        .unwrap();
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call(
            "c1",
            "linear__echo",
            json!({"text": "from the root"}),
        )])
        .push_text("root done")
        .push_tool_calls(vec![call(
            "c2",
            "linear__echo",
            json!({"text": "from the researcher"}),
        )])
        .push_text("researcher done");
    let assembly = assemble(def, &model);

    // Each agent selected its own server's tools; the subagent inherited nothing.
    assert_eq!(info_tools(&assembly, &root), ["linear__echo", "researcher"]);
    assert_eq!(
        info_tools(&assembly, &format!("{root}/researcher")),
        ["linear__pid", "linear__echo"]
    );
    assert_eq!(root_server.initializations(), 1);
    assert_eq!(research_server.initializations(), 1);

    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = common::runtime_on(&assembly, store);
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    let run = rt
        .start(&format!("{root}/researcher"), user_message("go"), None)
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;

    let requests = model.requests();
    assert_eq!(
        last_tool_result(&requests[1], "c1"),
        ("from the root".into(), false)
    );
    assert_eq!(
        last_tool_result(&requests[3], "c2"),
        ("from the researcher".into(), false)
    );
    // Each server was called once, by the agent whose file names it, with that agent's token.
    assert_eq!((root_server.calls(), research_server.calls()), (1, 1));
    assert!(!root_server.authorizations().is_empty());
    for header in root_server.authorizations() {
        assert_eq!(header, "Bearer tok-root-aaaa1111");
    }
    for header in research_server.authorizations() {
        assert_eq!(header, "Bearer tok-research-bbbb2222");
    }
}

#[tokio::test]
async fn pattern_selects_mcp_tools() {
    let server = TestHttpServer::start(Some(TOKEN)).await;
    let root = uniq("root");
    let files = root_files(
        &root,
        "tools: ['linear__pi*', 'linear__ec*']",
        &mcp_json(&server.url(), "LINEAR_TOKEN", ""),
    );
    let model = Arc::new(MockModel::new());
    model.push_text("ok");
    let assembly = assemble(
        def_of(&files)
            .env("LINEAR_TOKEN", TOKEN)
            .connect_mcp(&policy())
            .await
            .unwrap(),
        &model,
    );
    // The server lists `echo`, `fail`, `big`, `mixed`, `env`, `pid`, ...: the patterns take two,
    // in the order the file lists them. (`a.b` cannot be shown to a model and was left out.)
    assert_eq!(
        info_tools(&assembly, &root),
        ["linear__pid", "linear__echo"]
    );

    let rt = common::runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    let offered: Vec<String> = model.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(offered, ["linear__pid", "linear__echo"]);
    let echo = &model.requests()[0].tools[1];
    assert_eq!(echo.description, "Answers with the text it is given.");
    assert_eq!(echo.parameters["required"], json!(["text"]));
}

#[tokio::test]
async fn mcp_tool_named_like_registered_is_clash() {
    let server = TestHttpServer::start(Some(TOKEN)).await;
    let root = uniq("root");
    let files = root_files(&root, "", &mcp_json(&server.url(), "LINEAR_TOKEN", ""));
    let registered = ToolSet::new().tool(common::stub("linear__echo"));
    let error = def_of(&files)
        .env("LINEAR_TOKEN", TOKEN)
        .connect_mcp(&policy())
        .await
        .unwrap()
        .bind(registered)
        .unwrap_err();
    assert!(
        matches!(&error, Error::McpToolClash { tool, origin } if tool == "linear__echo"
            && origin.file.to_string_lossy() == "agent/mcp.json"),
        "{error}"
    );
}

#[tokio::test]
async fn subagent_named_like_mcp_tool_is_clash() {
    let server = TestHttpServer::start(Some(TOKEN)).await;
    // By pattern, and with no `tools:` at all (a root then gets every tool, its MCP tools
    // included): the clash is named as the MCP tool's either way, never as a skill's.
    for frontmatter in ["tools: ['linear__*']", ""] {
        let root = uniq("root");
        let mut files = root_files(
            &root,
            frontmatter,
            &mcp_json(&server.url(), "LINEAR_TOKEN", ""),
        );
        files.push((
            "agent/subagents/linear__echo.md".into(),
            instructions("description: Echoes.", "Echo."),
        ));
        let error = def_of(&files)
            .env("LINEAR_TOKEN", TOKEN)
            .connect_mcp(&policy())
            .await
            .unwrap()
            .bind(ToolSet::new())
            .unwrap_err();
        assert!(
            matches!(&error, Error::SubagentToolClash { tool, clash, .. }
                if tool == "linear__echo" && clash == &ToolClash::McpTool { server: "linear".into() }),
            "{frontmatter:?}: {error}"
        );
    }
}

#[tokio::test]
async fn unconnected_and_failing_servers_fail_startup_naming_the_agent_and_the_file() {
    let server = TestHttpServer::start(Some(TOKEN)).await;
    let root = uniq("root");
    let mut files = root_files(&root, "", &mcp_json(&server.url(), "LINEAR_TOKEN", ""));
    files.push((
        "agent/subagents/researcher/instructions.md".into(),
        instructions("description: Researches.", "You research."),
    ));
    // The subagent's server is `stdio`, which this policy does not allow.
    files.push((
        "agent/subagents/researcher/mcp.json".into(),
        r#"{"mcpServers": {"fs": {"command": "mcp-server-filesystem"}}}"#.into(),
    ));

    // No `connect_mcp`: refused at bind, and the message says what to call.
    let error = def_of(&files)
        .env("LINEAR_TOKEN", TOKEN)
        .bind(ToolSet::new())
        .unwrap_err();
    assert!(matches!(error, Error::McpNotConnected { .. }), "{error}");
    assert!(
        error.to_string().contains("AgentDef::connect_mcp"),
        "{error}"
    );
    assert!(!error.to_string().contains("enable the feature"), "{error}");

    // The root connects; the subagent's file fails, and the error says whose it is.
    let error = def_of(&files)
        .env("LINEAR_TOKEN", TOKEN)
        .connect_mcp(&policy())
        .await
        .unwrap_err();
    let Error::Mcp { origin, source, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, format!("{root}/researcher"));
    assert_eq!(
        origin.file.to_string_lossy(),
        "agent/subagents/researcher/mcp.json"
    );
    assert!(matches!(
        source.downcast_ref::<adam_mcp::Error>(),
        Some(adam_mcp::Error::StdioNotAllowed { .. })
    ));
    assert_eq!(error.class(), ErrorClass::Invalid);

    // An unset variable, before any request.
    let requests = server.requests();
    let error = def_of(&files[..2].to_vec())
        .connect_mcp(&policy())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Mcp { source, .. }
            if matches!(source.downcast_ref::<adam_mcp::Error>(),
                Some(adam_mcp::Error::Var { var, .. }) if var == "LINEAR_TOKEN")),
        "{error}"
    );
    assert_eq!(server.requests(), requests);

    // A server that is down.
    let mut down = TestHttpServer::start(None).await;
    let url = down.url();
    down.stop().await;
    let files = root_files(&uniq("root"), "", &mcp_json(&url, "LINEAR_TOKEN", ""));
    let error = def_of(&files)
        .env("LINEAR_TOKEN", TOKEN)
        .connect_mcp(&policy())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Mcp { source, .. }
            if matches!(source.downcast_ref::<adam_mcp::Error>(),
                Some(adam_mcp::Error::Connect { .. }))),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Transient);
    assert!(!error.to_string().contains(TOKEN), "{error}");
}

// --- runs ----------------------------------------------------------------------------------

#[tokio::test]
async fn parent_run_calls_mcp_tool_on_memory_and_postgres() {
    for (backend, store) in stores().await {
        let server = TestHttpServer::start(Some(TOKEN)).await;
        let root = uniq("root");
        let files = root_files(
            &root,
            "tools: ['linear__echo']",
            &mcp_json(&server.url(), "LINEAR_TOKEN", ""),
        );
        let def = def_of(&files)
            .env("LINEAR_TOKEN", TOKEN)
            .connect_mcp(&policy())
            .await
            .unwrap();
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![call(
                "c1",
                "linear__echo",
                json!({"text": "hello over MCP"}),
            )])
            .push_text("all done");
        let debug_def = format!("{def:?}");
        let assembly = assemble(def, &model);

        let sink = CollectingSink::new();
        let rt = assembly
            .register(Runtime::builder(store.clone()))
            .poll_interval(std::time::Duration::from_millis(20))
            .event_sink(sink.clone())
            .build();
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        let view = wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(
            view.output.as_ref().unwrap()["text"],
            "all done",
            "{backend}"
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 2, "{backend}");
        assert_eq!(
            last_tool_result(&requests[1], "c1"),
            ("hello over MCP".into(), false),
            "{backend}"
        );
        assert_eq!(server.calls(), 1, "{backend}");

        // The call is a journaled step of the run, and the token is nowhere: not in the journal,
        // the state, the result, the events, nor in what `Debug` shows of the definition or the
        // assembly.
        let journal = store.journal_list(run).await.unwrap();
        let entry = journal.iter().find(|e| e.name == "tool:c1");
        assert!(entry.is_some_and(|e| e.ok), "{backend}: {journal:?}");
        let everything = format!(
            "{journal:?}{:?}{:?}{:?}{debug_def}{assembly:?}{:?}",
            view.state,
            view.output,
            sink.events(),
            assembly.info(),
        );
        assert!(!everything.contains(TOKEN), "{backend}: the token leaked");
    }
}

#[tokio::test]
async fn committed_call_not_repeated() {
    // The turn that made the call committed. A later transition that fails and is retried (the
    // model blips) replays only itself: the MCP server is not called again, and the recorded
    // result is what the model sees.
    for (backend, store) in stores().await {
        let server = TestHttpServer::start(Some(TOKEN)).await;
        let root = uniq("root");
        let files = root_files(
            &root,
            "tools: ['linear__echo']",
            &mcp_json(&server.url(), "LINEAR_TOKEN", ""),
        );
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![call("c1", "linear__echo", json!({"text": "once"}))])
            .push_error(ModelError::transient("blip"))
            .push_text("done");
        let def = def_of(&files)
            .env("LINEAR_TOKEN", TOKEN)
            .connect_mcp(&policy())
            .await
            .unwrap();
        let assembly = assemble(def, &model);
        let rt = common::runtime_on(&assembly, store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            server.calls(),
            1,
            "{backend}: one call, however the run went on"
        );
        let requests = model.requests();
        assert_eq!(
            requests.len(),
            3,
            "{backend}: the model was asked again, and only it"
        );
        assert_eq!(
            last_tool_result(&requests[2], "c1"),
            ("once".into(), false),
            "{backend}"
        );
    }
}

#[tokio::test]
async fn transient_later_in_turn_calls_again() {
    // Documents what at-least-once means. Two calls in one turn: the MCP call, then a registered
    // tool that fails transiently once. The transition fails before it commits, so it runs again,
    // from its start, and the MCP call is made a second time: MCP has no idempotency key.
    for (backend, store) in stores().await {
        let server = TestHttpServer::start(Some(TOKEN)).await;
        let root = uniq("root");
        let files = root_files(&root, "", &mcp_json(&server.url(), "LINEAR_TOKEN", ""));
        let attempts = Arc::new(AtomicUsize::new(0));
        let flaky = {
            let attempts = Arc::clone(&attempts);
            FnTool::raw(
                "flaky",
                "Fails once, transiently.",
                json!({"type": "object", "properties": {}}),
                move |_ctx, _args| {
                    let first = attempts.fetch_add(1, SeqCst) == 0;
                    async move {
                        if first {
                            Err(ToolError::Transient("try again".into()))
                        } else {
                            Ok(ToolOutput::text("flaky-out"))
                        }
                    }
                },
            )
        };
        let both = || {
            vec![
                call("c1", "linear__echo", json!({"text": "twice"})),
                call("c2", "flaky", json!({})),
            ]
        };
        let model = Arc::new(MockModel::new());
        // The retry runs the whole transition again, the model turn included.
        model
            .push_tool_calls(both())
            .push_tool_calls(both())
            .push_text("done");
        let def = def_of(&files)
            .env("LINEAR_TOKEN", TOKEN)
            .connect_mcp(&policy())
            .await
            .unwrap();
        let assembly = def
            .bind(ToolSet::new().tool(flaky))
            .unwrap()
            .model(model.clone(), "default")
            .unwrap();
        let rt = common::runtime_on(&assembly, store);
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(attempts.load(SeqCst), 2, "{backend}");
        assert_eq!(
            server.calls(),
            2,
            "{backend}: the MCP call was made on both attempts"
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 3, "{backend}");
        let results: Vec<(String, bool)> = requests[2]
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Tool {
                    content, is_error, ..
                } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            [("twice".to_owned(), false), ("flaky-out".to_owned(), false)],
            "{backend}"
        );
    }
}

#[tokio::test]
async fn a_server_that_went_away_is_an_error_result_and_the_run_goes_on() {
    let mut server = TestHttpServer::start(Some(TOKEN)).await;
    let root = uniq("root");
    let files = root_files(
        &root,
        "tools: ['linear__echo']",
        &mcp_json(&server.url(), "LINEAR_TOKEN", ""),
    );
    let def = def_of(&files)
        .env("LINEAR_TOKEN", TOKEN)
        .connect_mcp(&policy())
        .await
        .unwrap();
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "linear__echo", json!({"text": "hi"}))])
        .push_text("I could not reach the tracker.");
    let assembly = assemble(def, &model);
    server.stop().await;

    let rt = common::runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        view.output.as_ref().unwrap()["text"],
        "I could not reach the tracker."
    );
    let (text, is_error) = last_tool_result(&model.requests()[1], "c1");
    assert!(is_error, "{text}");
    assert!(text.contains("linear"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
}

// --- dev reload ----------------------------------------------------------------------------

#[cfg(feature = "dev")]
mod dev {
    use std::time::Duration;

    use adam_assembly::{Error, LiveAssembly, ReloadError};

    use super::*;

    #[tokio::test]
    async fn reload_keeps_connections_and_refuses_changed_mcp_json() {
        let server = TestHttpServer::start(Some(TOKEN)).await;
        let name = uniq("dev");
        let dir = tempfile::tempdir().unwrap();
        let agent =
            |prompt: &str| instructions(&format!("name: {name}\ntools: ['linear__*']"), prompt);
        let mcp = |tools: &str| {
            mcp_json(
                &server.url(),
                "LINEAR_TOKEN",
                &format!(r#""tools": {tools}"#),
            )
        };
        common::write(
            dir.path(),
            &[
                ("agent/instructions.md", &agent("Old prompt.")),
                ("agent/mcp.json", &mcp(r#"["echo"]"#)),
            ],
        );
        let model = Arc::new(MockModel::new());
        let live = LiveAssembly::builder(dir.path(), model.clone(), "alias")
            // The variable is given in code, before the connection is made.
            .configure(|def| def.env("LINEAR_TOKEN", TOKEN))
            .connect_mcp(&policy())
            .await
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(live.info()[0].tools, ["linear__echo"]);
        assert_eq!(server.initializations(), 1);

        // An edit that is not about MCP reloads, and reuses the connection: no new session.
        common::write(
            dir.path(),
            &[("agent/instructions.md", &agent("New prompt."))],
        );
        let reloaded = live.reload().unwrap();
        assert_eq!(reloaded.generation, 2);
        assert_eq!(live.info()[0].prompt, "New prompt.");
        assert_eq!(live.info()[0].tools, ["linear__echo"]);
        assert_eq!(
            server.initializations(),
            1,
            "a reload does not connect again"
        );

        // The tool works through the reloaded agent.
        model
            .push_tool_calls(vec![call(
                "c1",
                "linear__echo",
                json!({"text": "after a reload"}),
            )])
            .push_text("done");
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = live
            .register(Runtime::builder(store))
            .poll_interval(Duration::from_millis(20))
            .build();
        let worker = spawn_worker(&rt);
        let run = rt.start(&name, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            last_tool_result(&model.requests()[1], "c1"),
            ("after a reload".into(), false)
        );
        assert_eq!(server.initializations(), 1);

        // An edit of `mcp.json` is refused: tools are discovered once, at startup. The last good
        // version stays, and the message says to restart.
        common::write(
            dir.path(),
            &[("agent/mcp.json", &mcp(r#"["echo", "pid"]"#))],
        );
        let refused = live.reload().unwrap_err();
        assert!(
            matches!(&*refused, ReloadError::Load(Error::McpChanged { origin })
                if origin.file.to_string_lossy() == "agent/mcp.json"),
            "{refused}"
        );
        assert!(
            refused.to_string().contains("restart the process"),
            "{refused}"
        );
        assert!(matches!(
            live.last_error().as_deref(),
            Some(ReloadError::Load(Error::McpChanged { .. }))
        ));
        assert_eq!(live.info()[0].tools, ["linear__echo"]);
        assert_eq!(live.info()[0].prompt, "New prompt.");
        assert_eq!(server.initializations(), 1);

        // Putting the file back makes the next reload succeed again.
        common::write(dir.path(), &[("agent/mcp.json", &mcp(r#"["echo"]"#))]);
        live.reload().unwrap();
        assert!(live.last_error().is_none());
    }
}
