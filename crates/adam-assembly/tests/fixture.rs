//! The fixture agent of `adam-agent-fixture` (the valid directory of `adam-agent-fs`), embedded
//! at build time and read from disk, bound the same way and run end to end.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_agent_fixture::AGENT;
use adam_agent_fs::{Dir, Strictness};
use adam_assembly::{AgentDef, Assembly};
use adam_llm_agent::{ToolSet, user_message};
use adam_model::{MockModel, ToolCall};
use common::{runtime, spawn_worker, tools, wait_done};
use serde_json::json;

/// The registered tools the fixture's `tools:` lists name (the root's and the subagents'); the
/// root's `linear__*` names the tools of an MCP server, which come from its own `mcp.json`.
fn fixture_tools() -> ToolSet {
    tools(&[
        "prepare_workspace",
        "run_checks",
        "ask_user",
        "read_diff",
        "list_files",
        "fetch_page",
    ])
}

fn from_dir() -> AgentDef {
    let dir = Dir::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../adam-agent-fs/tests/fixtures/valid"
    ))
    .default_name("fixture");
    let mut defs = AgentDef::from_source(&dir, Strictness::Strict).unwrap();
    assert_eq!(defs.len(), 1);
    defs.remove(0)
}

fn assemble(def: AgentDef, model: Arc<MockModel>) -> Assembly {
    common::with_fixture_mcp(common::with_fixture_token(def))
        .bind(fixture_tools())
        .unwrap()
        .model(model, "gateway-default")
        .unwrap()
}

#[test]
fn the_embedded_manifest_and_the_directory_bind_to_equal_agents() {
    let embedded = assemble(
        AgentDef::from_manifest(AGENT).unwrap(),
        Arc::new(MockModel::new()),
    );
    let read = assemble(from_dir(), Arc::new(MockModel::new()));
    // The embedded agent by value is the same definition as by reference.
    let by_value = assemble(
        AgentDef::from_manifest(*AGENT).unwrap(),
        Arc::new(MockModel::new()),
    );
    assert_eq!(by_value.info(), embedded.info());

    assert_eq!(embedded.manifest(), read.manifest());
    assert_eq!(embedded.info(), read.info());
    assert_eq!(embedded.remotes(), read.remotes());
    assert_eq!(embedded.agents().len(), read.agents().len());

    // Depth first, subagents in name order; one LlmAgent per local definition.
    let names: Vec<&str> = embedded.info().iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "coder",
            "coder/legacy",
            "coder/researcher",
            "coder/researcher/summarizer",
            "coder/reviewer"
        ]
    );
    let parents: Vec<Option<&str>> = embedded
        .info()
        .iter()
        .map(|i| i.parent.as_deref())
        .collect();
    assert_eq!(
        parents,
        [
            None,
            Some("coder"),
            Some("coder"),
            Some("coder/researcher"),
            Some("coder")
        ]
    );
    // The remote subagent is data: no LlmAgent, but the file's target is kept.
    let [remote] = embedded.remotes() else {
        panic!("one remote expected");
    };
    assert_eq!(remote.parent, "coder");
    assert_eq!(remote.agent.name, "billing");
    assert_eq!(
        remote.agent.url,
        "https://billing.example.com/.well-known/agent-card.json"
    );
}

#[test]
fn the_fixture_binds_each_agent_as_its_files_say() {
    let assembly = assemble(
        AgentDef::from_manifest(AGENT).unwrap(),
        Arc::new(MockModel::new()),
    );
    let info = |name: &str| assembly.info().iter().find(|i| i.name == name).unwrap();

    let coder = info("coder");
    assert_eq!(coder.model_alias, "coder-large");
    assert_eq!(
        coder.tools,
        [
            "prepare_workspace",
            "run_checks",
            "ask_user",
            "linear__list_issues",
            "load_skill",
            "read_skill_file",
            // One tool per subagent, named after it, after everything else: `billing` is the
            // fixture's remote (A2A) subagent, a tool of the same shape.
            "billing",
            "legacy",
            "researcher",
            "reviewer"
        ]
    );
    assert_eq!(coder.skills, ["release-notes", "triage"]);
    assert!(coder.preloaded.is_empty());
    assert_eq!(coder.limits.max_turns, 200);
    assert_eq!(coder.limits.max_tool_calls, 400);
    assert_eq!(coder.limits.max_output_tokens, 8192);
    assert_eq!(coder.limits.max_history_tokens, 100_000);
    // The instructions, then the skills catalog (asserted whole in `tests/skills.rs`).
    let instructions = "You are the coder agent. You turn one coding task into a verified pull request.\n\
         Stop after at most 3 failed check cycles.\n\
         Strict mode is true.\n\n\
         ## Style\n\nKeep commits small.";
    assert!(
        coder
            .prompt
            .starts_with(&format!("{instructions}\n\nThe following skills")),
        "{}",
        coder.prompt
    );
    assert_eq!(
        coder.description.as_deref(),
        Some("Turns a coding task into a verified pull request.")
    );
    assert_eq!(coder.file.to_string_lossy(), "agent/instructions.md");

    // `model: inherit` and no `model:` both take the parent's alias.
    let reviewer = info("coder/reviewer");
    assert_eq!(reviewer.model_alias, "coder-large");
    assert_eq!(reviewer.tools, ["read_diff", "list_files"]);
    assert_eq!(reviewer.limits.max_turns, 20);
    // What the frontmatter leaves out keeps the loop's default.
    assert_eq!(reviewer.limits.max_tool_calls, 200);
    assert_eq!(
        reviewer.prompt,
        "You review diffs. Report findings as a numbered list."
    );

    // A subagent that lists no tools has none, and one that nests keeps its own.
    assert!(info("coder/legacy").tools.is_empty());
    // A subagent has its own skills (`web-search`, no files) and inherits none of the parent's.
    assert_eq!(
        info("coder/researcher").tools,
        ["fetch_page", "load_skill", "summarizer"]
    );
    assert_eq!(info("coder/researcher").skills, ["web-search"]);
    assert!(info("coder/reviewer").skills.is_empty());
    assert!(info("coder/researcher/summarizer").tools.is_empty());
    assert_eq!(
        info("coder/researcher/summarizer").parent.as_deref(),
        Some("coder/researcher")
    );
}

#[tokio::test]
async fn the_fixture_agent_runs_end_to_end_on_a_mock_model() {
    let model = Arc::new(MockModel::new());
    model.push_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "run_checks".into(),
        arguments: json!({}),
    }]);
    model.push_text("All checks pass.");
    model.push_text("LGTM");

    let assembly = assemble(AgentDef::from_manifest(AGENT).unwrap(), model.clone());
    let rt = runtime(&assembly);
    assert_eq!(
        rt.agent_names(),
        [
            "coder",
            "coder/legacy",
            "coder/researcher",
            "coder/researcher/summarizer",
            "coder/reviewer"
        ]
    );
    let worker = spawn_worker(&rt);

    // The root: a tool call, then the answer.
    let run = rt
        .start("coder", user_message("Fix the failing test"), None)
        .await
        .unwrap();
    let view = wait_done(&rt, run).await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "All checks pass.");

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let first = &requests[0];
    assert_eq!(first.model, "coder-large");
    assert_eq!(
        first.system.as_deref(),
        Some(assembly.info()[0].prompt.as_str())
    );
    assert!(
        first
            .system
            .as_deref()
            .unwrap()
            .contains("at most 3 failed check cycles")
    );
    assert_eq!(first.max_output_tokens, Some(8192));
    let offered: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        offered,
        [
            "prepare_workspace",
            "run_checks",
            "ask_user",
            "linear__list_issues",
            "load_skill",
            "read_skill_file",
            "billing",
            "legacy",
            "researcher",
            "reviewer"
        ]
    );
    // The tool ran and its output went back to the model.
    assert_eq!(
        requests[1].messages.last().unwrap().text(),
        "run_checks-out"
    );

    // A subagent is registered too, with its own prompt, tools and alias.
    let run = rt
        .start("coder/reviewer", user_message("Review this diff"), None)
        .await
        .unwrap();
    let view = wait_done(&rt, run).await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "LGTM");
    let reviewer = model.last_request().unwrap();
    assert_eq!(
        reviewer.system.as_deref(),
        Some("You review diffs. Report findings as a numbered list.")
    );
    let offered: Vec<&str> = reviewer.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(offered, ["read_diff", "list_files"]);
    assert_eq!(reviewer.model, "coder-large");
    assert_eq!(reviewer.max_output_tokens, Some(4096));

    worker.stop().await;
}

#[test]
fn the_fixture_does_not_bind_without_its_tools() {
    let error = AgentDef::from_manifest(AGENT)
        .unwrap()
        .bind(tools(&["prepare_workspace", "run_checks", "ask_user"]))
        .unwrap_err();
    // The root's `mcp.json` names servers that nobody connected: fail closed, before `tools:` is
    // even looked at.
    assert!(
        matches!(&error, adam_assembly::Error::McpNotConnected { servers, .. }
            if servers == &["fs".to_owned(), "linear".to_owned()]),
        "{error}"
    );
    assert!(
        error.to_string().starts_with(
            "agent `coder` (agent/mcp.json): `mcp.json` lists the MCP servers `fs`, `linear`"
        ),
        "{error}"
    );

    // With the MCP tools given, the registered tools that are missing are what is found next.
    let error = common::with_fixture_mcp(common::with_fixture_token(
        AgentDef::from_manifest(AGENT).unwrap(),
    ))
    .bind(tools(&["prepare_workspace", "run_checks", "ask_user"]))
    .unwrap_err();
    assert!(
        error.to_string().contains("`tools` names `fetch_page`")
            || error.to_string().contains("`tools` names `read_diff`"),
        "{error}"
    );
}
