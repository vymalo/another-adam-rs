//! Subagents: each is a child run of its parent, a round of a session the parent starts with
//! `subagent_start` (ADR 0089 slice C4: no tool is named after a subagent; the one-shot call is
//! `mode: one_shot` and `wait: true`). Run end to end on `MockModel` through the runtime, on the memory store always and on PostgreSQL when
//! `ADAM_TEST_POSTGRES_URL` is set. Every case has agent names of its own, so cases that share a
//! database never claim each other's runs.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

#[cfg(feature = "remote")]
use adam_agent_fixture::AGENT;
use adam_agent_fs::Subagent;
use adam_assembly::{AgentDef, Assembly, Error, ToolClash};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus, Store};
use adam_llm_agent::{
    FnTool, LlmStarter, SESSION_TOOLS, SessionTools, ToolError, ToolOutput, ToolSet, user_message,
};
use adam_model::{Message, MockModel, ModelRequest, ToolCall};
use adam_runtime::{
    Inbound, RESUME_KIND, Runtime, SessionState, child_run_id, session_conversation_id,
};
use common::{
    def, instructions, manifests, plain, runtime_on, spawn_worker, stub, tools, wait_done, wait_for,
};
use serde_json::{Value, json};

// --- harness -------------------------------------------------------------------------------

/// A store per backend that is available: memory, and PostgreSQL when the variable is set.
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

/// An agent name no other case uses.
fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new().0.simple())
}

type Files = Vec<(String, String)>;

fn def_of(files: &Files) -> AgentDef {
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    def(&refs)
}

/// root (`parent_only`) → researcher (`fetch`) → summarizer (no tools).
fn nested(root: &str) -> Files {
    vec![
        (
            "agent/instructions.md".into(),
            instructions(
                &format!("name: {root}\nmodel: big\ntools: [parent_only]"),
                "You are the root.",
            ),
        ),
        (
            "agent/subagents/researcher/instructions.md".into(),
            instructions(
                "description: Researches a topic.\ntools: [fetch]",
                "You research.",
            ),
        ),
        (
            "agent/subagents/researcher/subagents/summarizer.md".into(),
            instructions("description: Summarises text.", "You summarise."),
        ),
    ]
}

/// A root that calls the subagent `helper` (`tools: [fetch]`, plus `extra` frontmatter).
fn with_helper(root: &str, root_frontmatter: &str, helper_frontmatter: &str) -> Files {
    vec![
        (
            "agent/instructions.md".into(),
            instructions(
                &format!("name: {root}\n{root_frontmatter}"),
                "You are the root.",
            ),
        ),
        (
            "agent/subagents/helper.md".into(),
            instructions(
                &format!("description: Helps.\n{helper_frontmatter}"),
                "You help.",
            ),
        ),
    ]
}

fn counting(name: &str, calls: &Arc<AtomicUsize>) -> FnTool {
    let calls = Arc::clone(calls);
    let out = format!("{name}-out");
    FnTool::raw(
        name,
        format!("the {name} tool"),
        json!({"type": "object", "properties": {}}),
        move |_ctx, _args| {
            calls.fetch_add(1, SeqCst);
            let out = out.clone();
            async move { Ok::<_, ToolError>(ToolOutput::text(out)) }
        },
    )
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// The one-shot call of before ADR 0089 slice C4, as a model writes it now: `subagent_start` on
/// `agent` with `prompt`, `mode: one_shot` and `wait: true`, so the call's result is the child's
/// first report and the parent parks until then.
fn delegate(id: &str, agent: &str, prompt: &str) -> ToolCall {
    call(
        id,
        "subagent_start",
        json!({
            "agent": agent,
            "description": "the task",
            "prompt": prompt,
            "mode": "one_shot",
            "wait": true,
        }),
    )
}

/// `own`, then the six session tools, as an agent with subagents is offered them.
fn with_sessions(own: &[&str]) -> Vec<String> {
    own.iter()
        .chain(SESSION_TOOLS.iter())
        .map(|name| (*name).to_owned())
        .collect()
}

/// The report a `subagent_start` that waited answered with, for the call `id` in the last message
/// of the request: the session's header line, `report: done` and the child's text. Panics on an
/// error result or another status.
fn answer(request: &ModelRequest, id: &str) -> String {
    let (text, is_error) = last_tool_result(request, id);
    assert!(!is_error, "{text}");
    assert!(text.starts_with("session "), "{text}");
    assert!(text.contains("\nreport: done\n"), "{text}");
    text
}

fn assemble(files: &Files, tools: ToolSet, model: &Arc<MockModel>) -> Assembly {
    def_of(files)
        .bind(tools)
        .unwrap()
        .model(model.clone(), "default")
        .unwrap()
}

fn offered(request: &ModelRequest) -> Vec<&str> {
    request.tools.iter().map(|t| t.name.as_str()).collect()
}

/// The result of the tool call `id` in the last message of the request.
fn last_tool_result(request: &ModelRequest, id: &str) -> (String, bool) {
    match request.messages.last().unwrap() {
        Message::Tool {
            call_id,
            content,
            is_error,
            ..
        } if call_id == id => (content.clone(), *is_error),
        other => panic!("the last message is not the result of `{id}`: {other:?}"),
    }
}

fn user_texts(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .map(Message::text)
        .collect()
}

// --- the chain -----------------------------------------------------------------------------

#[tokio::test]
async fn a_parent_calls_its_subagent_which_calls_its_own_and_the_parent_goes_on() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![delegate("c1", "researcher", "find the release date")])
            .push_tool_calls(vec![call("f1", "fetch", json!({}))])
            .push_tool_calls(vec![delegate("s1", "summarizer", "the page text")])
            .push_text("ten lines")
            .push_text("report: ten lines")
            .push_text("the answer is in the report");

        let parent_calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        let registered = ToolSet::new()
            .tool(counting("parent_only", &parent_calls))
            .tool(counting("fetch", &fetch_calls));
        let assembly = assemble(&nested(&root), registered, &model);
        let rt = runtime_on(&assembly, store.clone());
        let worker = spawn_worker(&rt);

        let run = rt
            .start(&root, user_message("Find it"), None)
            .await
            .unwrap();
        let view = wait_done(&rt, run).await;
        assert_eq!(
            view.output.as_ref().unwrap()["text"],
            "the answer is in the report",
            "{backend}"
        );

        let requests = model.requests();
        assert_eq!(requests.len(), 6, "{backend}");
        let [r0, r1, r2, r3, r4, r5] = &requests[..] else {
            unreachable!()
        };
        // The parent: its own tool and the session tools, whose roster is the subagent described
        // by its file. No tool is named after the subagent.
        // The root's instructions, then the section about the session tools (ADR 0054 decision 9).
        let root_system = format!(
            "You are the root.\n\n{}",
            SessionTools::guide_for(["researcher"])
        );
        assert_eq!(plain(&r0.system).as_deref(), Some(root_system.as_str()));
        assert_eq!(offered(r0), with_sessions(&["parent_only"]));
        assert!(
            r0.tools[1]
                .description
                .ends_with("Subagents:\n- researcher: Researches a topic."),
            "{}",
            r0.tools[1].description
        );
        assert_eq!(
            r0.tools[1].parameters["properties"]["agent"]["enum"],
            json!(["researcher"])
        );
        assert_eq!(user_texts(r0), ["Find it"]);
        // The child: its own prompt, its own tools (and the session tools for its own subagent),
        // and a history that starts with `prompt`, not with the parent's conversation.
        for r in [r1, r2] {
            let child_system = format!(
                "You research.\n\n{}",
                SessionTools::guide_for(["summarizer"])
            );
            assert_eq!(plain(&r.system).as_deref(), Some(child_system.as_str()));
            assert_eq!(offered(r), with_sessions(&["fetch"]));
            assert_eq!(
                r.tools[1].parameters["properties"]["agent"]["enum"],
                json!(["summarizer"])
            );
            assert_eq!(user_texts(r), ["find the release date"]);
            assert_eq!(r.model, "big", "a subagent inherits the parent's alias");
        }
        assert_eq!(r1.messages.len(), 1);
        assert_eq!(last_tool_result(r2, "f1"), ("fetch-out".into(), false));
        // The grandchild: no tools at all, and nothing but its message.
        assert_eq!(plain(&r3.system).as_deref(), Some("You summarise."));
        assert!(r3.tools.is_empty());
        assert_eq!(r3.messages.len(), 1);
        assert_eq!(user_texts(r3), ["the page text"]);
        // Each answer comes back as the result of the call that started it.
        assert!(answer(r4, "s1").contains("ten lines"));
        assert!(answer(r5, "c1").contains("report: ten lines"));
        assert_eq!(plain(&r5.system).as_deref(), Some(root_system.as_str()));
        // Only the child used `fetch`; the parent's tool was not called by anyone.
        assert_eq!(fetch_calls.load(SeqCst), 1);
        assert_eq!(parent_calls.load(SeqCst), 0);

        // The children are runs of their own, recorded under their parents, and done: each a
        // round of a session of its parent's conversation, the grandchild's in its parent's.
        let researcher = child_run_id(run, "c1");
        let child = store.load_run(researcher).await.unwrap().unwrap();
        assert_eq!(child.agent, format!("{root}/researcher"));
        assert_eq!(child.parent_id, Some(run));
        assert_eq!(child.status, RunStatus::Done);
        let researching = session_conversation_id(&format!("run:{run}"), researcher);
        assert_eq!(child.conversation_id.as_deref(), Some(researching.as_str()));
        let summarizer = child_run_id(researcher, "s1");
        let grandchild = store.load_run(summarizer).await.unwrap().unwrap();
        assert_eq!(grandchild.agent, format!("{root}/researcher/summarizer"));
        assert_eq!(grandchild.parent_id, Some(researcher));
        assert_eq!(grandchild.status, RunStatus::Done);
        assert_eq!(
            grandchild.conversation_id,
            Some(session_conversation_id(&researching, summarizer)),
            "{backend}"
        );

        worker.stop().await;
    }
}

// The fixture has a remote subagent (`a2a:`).
#[cfg(feature = "remote")]
#[tokio::test]
async fn the_embedded_fixture_runs_a_subagent_end_to_end() {
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![delegate("c1", "reviewer", "Review this diff")])
        .push_text("LGTM")
        .push_text("Reviewed: LGTM");
    let assembly = common::with_fixture_mcp(common::with_fixture_token(
        AgentDef::from_manifest(AGENT).unwrap(),
    ))
    .bind(tools(&[
        "prepare_workspace",
        "run_checks",
        "ask_user",
        "read_diff",
        "list_files",
        "fetch_page",
    ]))
    .unwrap()
    .model(model.clone(), "gateway-default")
    .unwrap();
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt
        .start("coder", user_message("Fix it"), None)
        .await
        .unwrap();
    let view = wait_done(&rt, run).await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "Reviewed: LGTM");
    let requests = model.requests();
    // The reviewer, from the fixture's own file: its prompt, its two tools, and no cap of its own,
    // so its request asks the 128,000 fallback (ADR 0064).
    assert_eq!(
        plain(&requests[1].system).as_deref(),
        Some("You review diffs. Report findings as a numbered list.")
    );
    assert_eq!(offered(&requests[1]), ["read_diff", "list_files"]);
    assert_eq!(requests[1].max_output_tokens, Some(128_000));
    assert_eq!(user_texts(&requests[1]), ["Review this diff"]);
    worker.stop().await;
}

// --- isolation -----------------------------------------------------------------------------

#[tokio::test]
async fn a_child_cannot_call_a_tool_only_its_parent_has() {
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![delegate("c1", "helper", "go")])
        // The helper tries the parent's tool.
        .push_tool_calls(vec![call("x1", "parent_only", json!({}))])
        .push_text("I could not")
        .push_text("done");
    let parent_calls = Arc::new(AtomicUsize::new(0));
    let fetch_calls = Arc::new(AtomicUsize::new(0));
    let files = with_helper(&root, "tools: [parent_only]", "tools: [fetch]");
    let assembly = assemble(
        &files,
        ToolSet::new()
            .tool(counting("parent_only", &parent_calls))
            .tool(counting("fetch", &fetch_calls)),
        &model,
    );
    // The bound tool lists: the parent has its tool and the session tools, which start the helper;
    // the helper has only what it lists.
    let tools_of = |name: &str| {
        assembly
            .info()
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .tools
            .clone()
    };
    assert_eq!(tools_of(&root), with_sessions(&["parent_only"]));
    assert_eq!(tools_of(&format!("{root}/helper")), ["fetch"]);

    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;

    let requests = model.requests();
    // What the model was offered.
    assert_eq!(offered(&requests[0]), with_sessions(&["parent_only"]));
    assert_eq!(offered(&requests[1]), ["fetch"]);
    // The refusal the child's model got, and that the tool did not run.
    let (text, is_error) = last_tool_result(&requests[2], "x1");
    assert!(is_error);
    assert_eq!(text, "unknown tool `parent_only`; available tools: fetch");
    assert_eq!(parent_calls.load(SeqCst), 0);
    assert_eq!(fetch_calls.load(SeqCst), 0);
    // The parent got the child's answer all the same.
    assert!(answer(&requests[3], "c1").contains("I could not"));
    worker.stop().await;
}

#[test]
fn a_subagent_does_not_inherit_a_tool_unless_it_lists_it() {
    let root = uniq("root");
    let files = with_helper(&root, "", "");
    // The root (no `tools:`) gets everything; the helper with no `tools:` gets nothing.
    let assembly = assemble(&files, tools(&["a", "b"]), &Arc::new(MockModel::new()));
    let info = assembly.info();
    assert_eq!(info[0].tools, with_sessions(&["a", "b"]));
    assert!(info[1].tools.is_empty());
    // And listing one gives it exactly that one.
    let files = with_helper(&root, "", "tools: [b]");
    let assembly = assemble(&files, tools(&["a", "b"]), &Arc::new(MockModel::new()));
    assert_eq!(assembly.info()[1].tools, ["b"]);
}

// --- limits --------------------------------------------------------------------------------

#[tokio::test]
async fn a_child_that_goes_over_its_own_limit_answers_from_its_closing_turn() {
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![delegate("c1", "helper", "loop")])
        .push_tool_calls(vec![call("f1", "fetch", json!({}))])
        .push_tool_calls(vec![call("f2", "fetch", json!({}))])
        // The child's third call is its closing turn: no tool may be called, and this is its answer.
        .push_text("two pages read, the third is left")
        .push_text("the helper got part of it, I will do the rest");
    let files = with_helper(
        &root,
        "tools: [fetch]",
        "tools: [fetch]\nlimits: { max_turns: 2 }",
    );
    let assembly = assemble(&files, tools(&["fetch"]), &model);
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    let view = wait_done(&rt, run).await;
    assert_eq!(
        view.output.as_ref().unwrap()["text"],
        "the helper got part of it, I will do the rest"
    );

    let requests = model.requests();
    assert_eq!(
        requests.len(),
        5,
        "the child's closing turn asked the model"
    );
    assert_eq!(requests[3].tool_choice, adam_model::ToolChoice::None);
    let text = answer(&requests[4], "c1");
    assert!(text.contains("two pages read, the third is left"), "{text}");
    assert!(text.contains("`max_turns` limit"), "{text}");
    assert!(text.contains("may be incomplete"), "{text}");

    let child = rt.view(child_run_id(run, "c1")).await.unwrap().unwrap();
    assert_eq!(child.status, RunStatus::Done);
    assert_eq!(child.output.as_ref().unwrap()["limited"], true);
    worker.stop().await;
}

/// The subagent's `limits.max_minutes` reaches the runtime: the round is created with the deadline
/// (a minute here), 30 minutes when its file says nothing, and none for `0`.
#[tokio::test]
async fn a_subagents_max_minutes_becomes_the_deadline_of_its_run() {
    for (frontmatter, limit) in [
        ("tools: []\nlimits: { max_minutes: 1 }", Some(60)),
        ("tools: []", Some(30 * 60)),
        ("tools: []\nlimits: { max_minutes: 0 }", None),
    ] {
        let root = uniq("root");
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![delegate("c1", "helper", "go")])
            .push_text("the helper's answer")
            .push_text("done");
        let files = with_helper(&root, "tools: []", frontmatter);
        let assembly = assemble(&files, tools(&[]), &model);
        let store: DynStore = Arc::new(MemoryStore::new());
        let rt = runtime_on(&assembly, store.clone());
        let worker = spawn_worker(&rt);
        let run = rt.start(&root, user_message("go"), None).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;

        let child = store
            .load_run(child_run_id(run, "c1"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            child.state.get("limit_secs").and_then(Value::as_u64),
            limit,
            "{frontmatter}: {}",
            child.state
        );
        assert_eq!(child.state.get("deadline").is_some(), limit.is_some());
    }
}

#[tokio::test]
async fn the_turns_of_a_child_are_not_the_parents() {
    // The parent may make two model calls in all; the child makes two on its own account. Had the
    // child's turns counted against the parent it would fail on the parent's second.
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![delegate("c1", "helper", "go")])
        .push_tool_calls(vec![call("f1", "fetch", json!({}))])
        .push_text("helped")
        .push_text("done");
    let files = with_helper(
        &root,
        "tools: []\nlimits: { max_turns: 2, max_tool_calls: 1 }",
        "tools: [fetch]\nlimits: { max_turns: 2, max_tool_calls: 1 }",
    );
    let assembly = assemble(&files, tools(&["fetch"]), &model);
    let root_info = &assembly.info()[0];
    assert_eq!(root_info.limits.max_turns, 2);
    assert_eq!(assembly.info()[1].limits.max_tool_calls, 1);
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    let view = wait_done(&rt, run).await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "done");
    worker.stop().await;
}

// --- a bad call ----------------------------------------------------------------------------

#[tokio::test]
async fn a_start_without_a_prompt_starts_no_child() {
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call(
            "c1",
            "subagent_start",
            json!({"agent": "helper", "description": "d", "msg": "typo", "wait": true}),
        )])
        .push_text("ok");
    let files = with_helper(&root, "tools: []", "");
    let assembly = assemble(&files, ToolSet::new(), &model);
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    let (text, is_error) = last_tool_result(&model.requests()[1], "c1");
    assert!(is_error);
    assert!(text.contains("`subagent_start` needs `prompt`"), "{text}");
    assert!(
        rt.store()
            .load_run(child_run_id(run, "c1"))
            .await
            .unwrap()
            .is_none()
    );
    worker.stop().await;
}

#[tokio::test]
async fn a_runtime_that_does_not_know_the_child_says_so_and_the_parent_goes_on() {
    // Only the root is registered (not `Assembly::register`): the start is refused for good, with
    // the name of the missing agent, and is not retried.
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![delegate("c1", "helper", "go")])
        .push_text("no helper here");
    let files = with_helper(&root, "tools: []", "");
    let assembly = assemble(&files, ToolSet::new(), &model);
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store)
        .agent(assembly.root().clone())
        .poll_interval(std::time::Duration::from_millis(20))
        .build();
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    let view = wait_done(&rt, run).await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "no helper here");
    let (text, is_error) = last_tool_result(&model.requests()[1], "c1");
    assert!(is_error);
    assert!(
        text.contains(&format!("unknown agent \"{root}/helper\"")),
        "{text}"
    );
    worker.stop().await;
}

// --- a parent that restarts while it waits -------------------------------------------------

#[tokio::test]
async fn a_parent_restarted_mid_wait_still_gets_the_childs_answer() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let child = format!("{root}/helper");
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![delegate("c1", "helper", "work")])
            .push_text("the child's answer")
            .push_text("all done");
        let files = with_helper(&root, "tools: []", "");
        let assembly = assemble(&files, ToolSet::new(), &model);

        // The first process steps the parent and can start the child, but does not step it: the
        // child stays queued and the parent parks on it.
        let first = Runtime::builder(store.clone())
            .agent(assembly.root().clone())
            .starter(LlmStarter::new(&child))
            .poll_interval(std::time::Duration::from_millis(20))
            .build();
        let worker = spawn_worker(&first);
        let run = first.start(&root, user_message("go"), None).await.unwrap();
        let child_id = child_run_id(run, "c1");
        wait_for("the parent to park on its child", || async {
            let parent = store.load_run(run).await.unwrap()?;
            let kid = store.load_run(child_id).await.unwrap()?;
            (parent.status == RunStatus::Parked
                && kid.status == RunStatus::Runnable
                && kid.parent_id == Some(run)
                // A round of a session of the parent (ADR 0089).
                && kid.conversation_id
                    == Some(session_conversation_id(&format!("run:{run}"), child_id)))
            .then_some(())
        })
        .await;
        assert_eq!(
            model.requests().len(),
            1,
            "{backend}: only the parent has spoken"
        );

        // The process goes away.
        worker.stop().await;
        drop(first);

        // A new one, from the same definition, steps the child; its notice wakes the parent,
        // which answers the call and finishes.
        let second = runtime_on(&assembly, store.clone());
        let worker = spawn_worker(&second);
        let view = wait_done(&second, run).await;
        assert_eq!(
            view.output.as_ref().unwrap()["text"],
            "all done",
            "{backend}"
        );
        let requests = model.requests();
        assert_eq!(
            requests.len(),
            3,
            "{backend}: one turn each, nothing asked twice"
        );
        assert_eq!(user_texts(&requests[1]), ["work"]);
        assert!(answer(&requests[2], "c1").contains("the child's answer"));
        worker.stop().await;
    }
}

// --- the one-shot call is a session that closes after its round (ADR 0089 slices C1 and C4) --

/// The conversation the sessions of a run that has none belong to (`ChildStarter`).
fn sessions_of(run: RunId) -> String {
    format!("run:{run}")
}

/// Whether a request carries a report the loop told on its own (a `report_*` call).
fn told_a_report(request: &ModelRequest) -> bool {
    request
        .messages
        .iter()
        .flat_map(Message::tool_calls)
        .any(|c| c.id.starts_with("report_"))
}

#[tokio::test]
async fn a_one_shot_start_that_waits_is_a_session_closed_after_its_round() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let agent = format!("{root}/helper");
        let run = RunId::new();
        let session = child_run_id(run, "c1");
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![delegate("c1", "helper", "work\nhard")])
            .push_text("the child's answer")
            // The parent's next turn sends to the session the call started: it is closed.
            .push_tool_calls(vec![call(
                "s1",
                "subagent_send",
                json!({"session": session.to_string(), "message": "and more"}),
            )])
            .push_text("done");
        let files = with_helper(&root, "tools: []", "");
        let assembly = assemble(&files, ToolSet::new(), &model);
        let rt = runtime_on(&assembly, store.clone());
        let worker = spawn_worker(&rt);
        rt.start_with_id(run, &root, user_message("go"), None)
            .await
            .unwrap();
        let view = wait_done(&rt, run).await;
        assert_eq!(view.output.as_ref().unwrap()["text"], "done", "{backend}");

        // The child is round 1 of a session of the parent, under the id the call derives.
        let conversation = sessions_of(run);
        let child = store.load_run(session).await.unwrap().unwrap();
        assert_eq!(child.agent, agent);
        assert_eq!(child.parent_id, Some(run));
        assert_eq!(
            child.conversation_id.as_deref(),
            Some(session_conversation_id(&conversation, session).as_str()),
            "{backend}"
        );
        // `one_shot`: closed now that its round has ended.
        let read = rt
            .session_view(&conversation, session)
            .await
            .unwrap()
            .expect("the session");
        assert_eq!(
            (read.state, read.closed, read.round),
            (SessionState::Closed, true, 1)
        );
        let header = read.header.expect("a header");
        assert_eq!(header.mode, adam_runtime::SessionMode::OneShot);
        assert_eq!(header.description, "the task");
        // A listing shows it, as it shows any session.
        let listed = rt
            .list_sessions(&conversation, &[agent.as_str()], 0, 10)
            .await
            .unwrap();
        assert_eq!(listed.total, 1, "{backend}");
        assert_eq!(listed.sessions[0].id, session);

        let requests = model.requests();
        assert_eq!(
            requests.len(),
            4,
            "{backend}: one turn each, no report told"
        );
        // The child's answer is the call's result, once: never told again as a report.
        let report = answer(&requests[2], "c1");
        assert!(
            report.starts_with(&format!(
                "session {session} (helper, \"the task\"): round 1, closed"
            )),
            "{report}"
        );
        assert!(report.contains("the child's answer"), "{report}");
        let (text, is_error) = last_tool_result(&requests[3], "s1");
        assert!(is_error, "{text}");
        assert!(
            text.contains("is closed and takes no more messages"),
            "{text}"
        );
        assert!(!requests.iter().any(told_a_report), "{backend}");
        worker.stop().await;
    }
}

#[tokio::test]
async fn two_calls_of_one_turn_are_two_sessions_with_their_own_conversations() {
    let root = uniq("root");
    let agent = format!("{root}/helper");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![
            delegate("c1", "helper", "first"),
            delegate("c2", "helper", "second"),
        ])
        .push_text("one")
        .push_text("two")
        .push_text("both done");
    let files = with_helper(&root, "tools: []", "");
    let assembly = assemble(&files, ToolSet::new(), &model);
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;

    let conversation = sessions_of(run);
    let (first, second) = (child_run_id(run, "c1"), child_run_id(run, "c2"));
    let mut conversations = Vec::new();
    for session in [first, second] {
        let child = rt.store().load_run(session).await.unwrap().unwrap();
        assert_eq!(child.status, RunStatus::Done);
        conversations.push(child.conversation_id.unwrap());
    }
    assert_eq!(
        conversations,
        [
            session_conversation_id(&conversation, first),
            session_conversation_id(&conversation, second),
        ]
    );
    let listed = rt
        .list_sessions(&conversation, &[agent.as_str()], 0, 10)
        .await
        .unwrap();
    assert_eq!(listed.total, 2);
    assert!(
        listed
            .sessions
            .iter()
            .all(|s| s.state == SessionState::Closed)
    );

    let requests = model.requests();
    assert_eq!(requests.len(), 4);
    let last = &requests[3];
    assert!(!told_a_report(last));
    let results: Vec<(&str, String)> = last
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                call_id, content, ..
            } => Some((call_id.as_str(), content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "{results:?}");
    for ((id, report), (want_id, answer)) in results.iter().zip([("c1", "one"), ("c2", "two")]) {
        assert_eq!(*id, want_id);
        assert!(report.contains(&format!("\t{answer}\n")), "{report}");
    }
    worker.stop().await;
}

/// A host's own tool that waits on a child, as a subagent's own tool did before ADR 0089 slice C4
/// (`ToolCtx::start_one_shot_session`, which the assembly no longer offers a tool for): it starts
/// `helper` as a one-shot session of the parent and parks the parent on it.
struct OneShot {
    name: &'static str,
    helper: String,
}

#[async_trait::async_trait]
impl adam_llm_agent::Tool for OneShot {
    fn spec(&self) -> adam_model::ToolSpec {
        adam_model::ToolSpec {
            name: self.name.into(),
            description: "asks the helper once".into(),
            parameters: json!({
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"],
            }),
        }
    }

    async fn call(
        &self,
        ctx: &adam_llm_agent::ToolCtx,
        args: Value,
    ) -> Result<ToolOutput, ToolError> {
        let message = args["message"].as_str().unwrap_or_default();
        let run = ctx
            .start_one_shot_session(&self.helper, message, None)
            .await?;
        Err(ToolError::AwaitRun { run })
    }
}

#[tokio::test]
async fn a_hosts_one_shot_tool_finds_a_child_started_before_it_was_a_session() {
    // A parent journaled the call when its child was a plain child run (no conversation): the
    // call, run again under C1, finds that child and waits for it instead of failing.
    let root = uniq("root");
    let agent = format!("{root}/helper");
    let model = Arc::new(MockModel::new());
    model
        .push_text("the old child's answer")
        .push_tool_calls(vec![call("c1", "ask_helper", json!({"message": "work"}))])
        .push_text("all done");
    let files = with_helper(&root, "tools: [ask_helper]", "");
    let assembly = assemble(
        &files,
        ToolSet::new().tool(OneShot {
            name: "ask_helper",
            helper: agent.clone(),
        }),
        &model,
    );
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = RunId::new();
    let session = child_run_id(run, "c1");
    rt.start_child(run, session, &agent, user_message("work"))
        .await
        .unwrap();
    wait_done(&rt, session).await;
    rt.start_with_id(run, &root, user_message("go"), None)
        .await
        .unwrap();
    // The child ended before its parent existed, so no notice of its end will come: wake the
    // parent instead of waiting for its poll, and it reads the child from the store.
    wait_for("the parent to wait on the old child", || async {
        let parent = rt.store().load_run(run).await.unwrap()?;
        (parent.status == RunStatus::Parked).then_some(())
    })
    .await;
    rt.deliver(run, Inbound::new(RESUME_KIND, json!({})).with_id("wake"))
        .await
        .unwrap();
    let view = wait_done(&rt, run).await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "all done");
    assert_eq!(
        last_tool_result(&model.requests()[2], "c1"),
        ("the old child's answer".into(), false)
    );
    let child = rt.store().load_run(session).await.unwrap().unwrap();
    assert_eq!(
        child.conversation_id, None,
        "the old child is left as it was"
    );
    worker.stop().await;
}

/// A parent parked on a child that a subagent's own tool started before slice C4 (here a host tool
/// that does what it did) is answered by a process of after it, which offers no such tool: the
/// wait settles from the store, not from the tool.
#[tokio::test]
async fn a_parent_parked_on_a_subagents_own_tool_of_before_is_answered_after_the_upgrade() {
    for (backend, store) in stores().await {
        let root = uniq("root");
        let child = format!("{root}/helper");
        let model = Arc::new(MockModel::new());
        model
            .push_tool_calls(vec![call("c1", "helper_once", json!({"message": "work"}))])
            .push_text("the child's answer")
            .push_text("all done");
        let before = assemble(
            &with_helper(&root, "tools: [helper_once]", ""),
            ToolSet::new().tool(OneShot {
                name: "helper_once",
                helper: child.clone(),
            }),
            &model,
        );
        // The process of before steps the parent and can start the child, but does not step it.
        let first = Runtime::builder(store.clone())
            .agent(before.root().clone())
            .starter(LlmStarter::new(&child))
            .poll_interval(std::time::Duration::from_millis(20))
            .build();
        let worker = spawn_worker(&first);
        let run = first.start(&root, user_message("go"), None).await.unwrap();
        let child_id = child_run_id(run, "c1");
        wait_for("the parent to park on its child", || async {
            let parent = store.load_run(run).await.unwrap()?;
            let kid = store.load_run(child_id).await.unwrap()?;
            (parent.status == RunStatus::Parked && kid.status == RunStatus::Runnable).then_some(())
        })
        .await;
        worker.stop().await;
        drop(first);

        // The process of after: the same folder, and no tool named after the subagent.
        let after = assemble(&with_helper(&root, "tools: []", ""), ToolSet::new(), &model);
        assert_eq!(after.info()[0].tools, with_sessions(&[]));
        let second = runtime_on(&after, store.clone());
        let worker = spawn_worker(&second);
        let view = wait_done(&second, run).await;
        assert_eq!(
            view.output.as_ref().unwrap()["text"],
            "all done",
            "{backend}"
        );
        assert_eq!(
            last_tool_result(&model.requests()[2], "c1"),
            ("the child's answer".into(), false),
            "{backend}"
        );
        worker.stop().await;
    }
}

// --- name clashes --------------------------------------------------------------------------

fn bind_error(files: &Files, registered: ToolSet) -> Error {
    def_of(files).bind(registered).unwrap_err()
}

#[test]
fn a_subagent_named_like_a_tool_of_its_parent_is_refused() {
    // The root has no `tools:`, so it has every registered tool, and one is called `helper`.
    let files = with_helper("coder", "", "");
    let error = bind_error(&files, tools(&["helper"]));
    let Error::SubagentToolClash {
        origin,
        parent,
        tool,
        clash,
    } = &error
    else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(origin.agent, "coder/helper");
    assert_eq!(origin.file.to_string_lossy(), "agent/subagents/helper.md");
    assert_eq!(parent, "coder");
    assert_eq!(tool, "helper");
    assert_eq!(*clash, ToolClash::Tool);
    assert_eq!(
        error.to_string(),
        "agent `coder/helper` (agent/subagents/helper.md): the parent `coder` would read `helper` \
         as two things, a subagent and a tool it already has: rename the subagent, or leave the \
         tool out of the parent's `tools`"
    );
}

#[test]
fn a_tool_named_like_a_session_tool_is_refused() {
    let files = with_helper("coder", "", "");
    let error = bind_error(&files, tools(&["subagent_read"]));
    let Error::SubagentToolClash {
        parent,
        tool,
        clash,
        ..
    } = &error
    else {
        panic!("wrong variant: {error}");
    };
    assert_eq!((parent.as_str(), tool.as_str()), ("coder", "subagent_read"));
    assert_eq!(*clash, ToolClash::Tool);
}

/// ADR 0089 slice C4: an agent that has subagents, the root or a subagent with its own, gets the
/// session tools, which are how it delegates; an agent without one gets none. No agent gets a tool
/// named after a subagent.
#[test]
fn every_agent_with_subagents_gets_the_session_tools_and_no_other() {
    let files = with_helper("coder", "", "");
    let assembly = assemble(&files, ToolSet::new(), &Arc::new(MockModel::new()));
    let info = assembly.info();
    assert_eq!(info[0].tools, with_sessions(&[]));
    assert!(
        info[1].tools.iter().all(|t| !t.starts_with("subagent_")),
        "a subagent's tools: {:?}",
        info[1].tools
    );
    let assembly = assemble(
        &nested("coder"),
        tools(&["parent_only", "fetch"]),
        &Arc::new(MockModel::new()),
    );
    let tools_of = |name: &str| {
        assembly
            .info()
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .tools
            .clone()
    };
    assert_eq!(tools_of("coder"), with_sessions(&["parent_only"]));
    assert_eq!(tools_of("coder/researcher"), with_sessions(&["fetch"]));
    assert!(tools_of("coder/researcher/summarizer").is_empty());
}

#[test]
fn a_registered_tool_the_parent_does_not_list_is_no_clash() {
    let files = with_helper("coder", "tools: [other]", "");
    let assembly = assemble(
        &files,
        tools(&["other", "helper"]),
        &Arc::new(MockModel::new()),
    );
    assert_eq!(assembly.info()[0].tools, with_sessions(&["other"]));
}

#[test]
fn a_subagent_named_like_a_skill_tool_is_refused() {
    let mut files = with_helper("coder", "tools: []", "");
    files.push((
        "agent/subagents/load_skill.md".into(),
        instructions("description: Loads.", "You load."),
    ));
    files.push((
        "agent/skills/notes/SKILL.md".into(),
        "---\nname: notes\ndescription: Notes about things.\n---\nBody.\n".into(),
    ));
    let error = bind_error(&files, ToolSet::new());
    let Error::SubagentToolClash { tool, clash, .. } = &error else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(tool, "load_skill");
    assert_eq!(*clash, ToolClash::SkillTool);
    assert!(
        error.to_string().contains("a tool its skills bring"),
        "{error}"
    );

    // Without skills the name is free.
    files.pop();
    files.retain(|(path, _)| !path.starts_with("agent/skills"));
    let assembly = assemble(&files, ToolSet::new(), &Arc::new(MockModel::new()));
    // No tool is named after a subagent (ADR 0089 slice C4): the root has the session tools.
    assert_eq!(assembly.info()[0].tools, with_sessions(&[]));
}

#[test]
fn a_registered_tool_named_like_a_skill_tool_is_a_plain_tool_clash_without_skills() {
    // The root has no `tools:`, so it has every registered tool, `load_skill` among them.
    let mut files = with_helper("coder", "", "");
    files.push((
        "agent/subagents/load_skill.md".into(),
        instructions("description: Loads.", "You load."),
    ));
    let error = bind_error(&files, tools(&["load_skill"]));
    let Error::SubagentToolClash { tool, clash, .. } = &error else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(tool, "load_skill");
    assert_eq!(
        *clash,
        ToolClash::Tool,
        "no skills, so the tool is a registered one"
    );
    assert!(
        error
            .to_string()
            .contains("a subagent and a tool it already has"),
        "{error}"
    );
}

#[test]
fn two_subagents_with_one_name_are_refused() {
    let files = with_helper("coder", "tools: []", "");
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (_dir, agents) = manifests(&refs);
    // The loader cannot produce this from files; a manifest built by hand can.
    let mut manifest = agents[0].clone();
    let Subagent::Local(twin) = manifest.subagents[0].clone() else {
        panic!("a local subagent");
    };
    manifest.subagents.push(Subagent::Local(twin));
    let error = AgentDef::from_manifest(manifest)
        .unwrap()
        .bind(ToolSet::new())
        .unwrap_err();
    let Error::SubagentToolClash { tool, clash, .. } = &error else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(tool, "helper");
    assert_eq!(
        *clash,
        ToolClash::Subagent {
            file: "agent/subagents/helper.md".into()
        }
    );
    assert!(
        error
            .to_string()
            .contains("another subagent of the parent, in agent/subagents/helper.md"),
        "{error}"
    );
}

#[test]
fn a_clash_is_found_below_the_root_too() {
    // researcher has every registered tool, `summarizer` among them, and a subagent of that name.
    let mut files = nested("coder");
    files[1].1 = instructions(
        "description: Researches a topic.\ntools: '*'",
        "You research.",
    );
    let error = bind_error(&files, tools(&["parent_only", "summarizer"]));
    let Error::SubagentToolClash { origin, parent, .. } = &error else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(parent, "coder/researcher");
    assert_eq!(origin.agent, "coder/researcher/summarizer");
}

// --- a subagent may have an asking tool, and cannot use it at run time -----------------------

fn asking(name: &str) -> FnTool {
    FnTool::raw(
        name,
        "Ask the user.",
        json!({"type": "object", "properties": {}}),
        |_ctx, _args| async move { Err::<ToolOutput, _>(ToolError::needs_input("which one?")) },
    )
    .asking_user()
}

#[test]
fn a_subagent_with_a_tool_that_asks_the_user_is_offered_it() {
    // ADR 0045 decision 9: whether a child may ask is decided per run, not at bind time. A tool
    // that asks is offered to a subagent here; the loop refuses the question in a run nobody can
    // answer (`adam_llm_agent::NOT_ASKABLE`) and lets a round of a kept-open session a client
    // shows ask.
    let files = with_helper("coder", "tools: [fetch]", "tools: [ask_user]");
    let assembly = assemble(
        &files,
        ToolSet::new().tool(asking("ask_user")).tool(stub("fetch")),
        &Arc::new(MockModel::new()),
    );
    assert_eq!(assembly.info()[1].tools, ["ask_user"]);
    // Also through `all` or a pattern, not only by name.
    for listed in ["tools: '*'", "tools: [fetch, 'ask*']"] {
        let files = with_helper("coder", "tools: [fetch]", listed);
        let assembly = assemble(
            &files,
            ToolSet::new().tool(asking("ask_user")).tool(stub("fetch")),
            &Arc::new(MockModel::new()),
        );
        assert!(
            assembly.info()[1].tools.iter().any(|t| t == "ask_user"),
            "{listed}: {:?}",
            assembly.info()[1].tools
        );
    }
}

#[test]
fn the_root_may_ask_and_a_subagent_that_does_not_is_fine() {
    let files = with_helper("coder", "tools: [ask_user]", "tools: [fetch]");
    let assembly = assemble(
        &files,
        ToolSet::new().tool(asking("ask_user")).tool(stub("fetch")),
        &Arc::new(MockModel::new()),
    );
    assert_eq!(assembly.info()[0].tools, with_sessions(&["ask_user"]));
    assert_eq!(assembly.info()[1].tools, ["fetch"]);
}

// --- `session:` in a subagent's file (ADR 0089 slice C3) -----------------------------------

#[test]
fn the_definition_carries_each_subagents_session_block() {
    use adam_llm_agent::SessionDefaults;
    use adam_runtime::SessionMode;

    let root = uniq("root");
    let mut files = with_helper(&root, "", "session:\n  mode: sealed\n  wait: true");
    files.push((
        "agent/subagents/plain.md".into(),
        instructions("description: Plain.", "You do it."),
    ));
    files.push((
        "agent/subagents/far.md".into(),
        instructions(
            "description: Far away.\na2a: https://far.example.com/card\nsession: { mode: one_shot }",
            "",
        ),
    ));
    let defaults = def_of(&files).session_defaults();
    assert_eq!(
        defaults,
        [
            (
                "far".to_owned(),
                SessionDefaults {
                    mode: Some(SessionMode::OneShot),
                    wait: None,
                }
            ),
            (
                "helper".to_owned(),
                SessionDefaults {
                    mode: Some(SessionMode::Sealed),
                    wait: Some(true),
                }
            ),
            ("plain".to_owned(), SessionDefaults::default()),
        ]
    );
}

#[tokio::test]
async fn a_start_gets_the_filed_mode_says_it_in_the_roster_and_may_not_widen_it() {
    let root = uniq("root");
    let run = RunId::new();
    let session = child_run_id(run, "s2");
    let start = |id: &str, extra: Value| {
        let mut args = json!({"agent": "helper", "description": "work", "prompt": "do it"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        call(id, "subagent_start", args)
    };
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![start("s1", json!({"mode": "open"}))])
        .push_tool_calls(vec![start("s2", json!({"wait": true}))])
        .push_text("helped")
        .push_text("done");
    let files = with_helper(&root, "tools: []", "session: { mode: sealed }");
    let assembly = assemble(&files, ToolSet::new(), &model);
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = runtime_on(&assembly, store);
    let worker = spawn_worker(&rt);
    rt.start_with_id(run, &root, user_message("go"), None)
        .await
        .unwrap();
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.output.as_ref().unwrap()["text"], "done");

    let requests = model.requests();
    let spec = requests[0]
        .tools
        .iter()
        .find(|t| t.name == "subagent_start")
        .unwrap();
    assert!(
        spec.description
            .contains("\n- helper: Helps. (mode sealed by default, at most sealed)"),
        "{}",
        spec.description
    );
    let (refused, is_error) = last_tool_result(&requests[1], "s1");
    assert!(is_error, "{refused}");
    assert!(
        refused.starts_with("`mode` is open; at most sealed for helper"),
        "{refused}"
    );
    let (report, is_error) = last_tool_result(&requests[3], "s2");
    assert!(!is_error, "{report}");
    assert!(report.contains("helped"), "{report}");
    let read = rt
        .session_view(&sessions_of(run), session)
        .await
        .unwrap()
        .expect("the session");
    assert_eq!(
        read.header.map(|h| h.mode),
        Some(adam_runtime::SessionMode::Sealed),
        "the filed mode, which the call did not name"
    );
    assert_eq!(read.state, SessionState::Idle, "sealed stays open");
}

// --- the named tools are gone (ADR 0089 slice C4) ---------------------------------------------

/// A mode that lists the session tools keeps them (ADR 0089 slice C4): they are made after `bind`
/// resolves the modes, and a mode may name them all the same, so an agent in that mode can still
/// delegate. Before C4 such a mode quietly lost them and kept only the subagents' own tools.
#[tokio::test]
async fn a_mode_that_names_the_session_tools_keeps_them() {
    let root = uniq("root");
    let mut files = with_helper(&root, "tools: [fetch, other]", "");
    files.push((
        "agent/modes/plan.md".into(),
        "---\ntitle: Plan\ndescription: Plans.\ndefault: true\ntools: [fetch, subagent_start, subagent_read]\n---\nPlan.\n"
            .into(),
    ));
    let model = Arc::new(MockModel::new());
    model.push_text("planned");
    let assembly = assemble(&files, tools(&["fetch", "other"]), &model);
    assert_eq!(assembly.info()[0].tools, with_sessions(&["fetch", "other"]));
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt
        .start(&root, user_message("plan it"), None)
        .await
        .unwrap();
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        offered(&model.requests()[0]),
        ["fetch", "subagent_start", "subagent_read"]
    );
}

// --- session limits (ADR 0045, slice 4) -------------------------------------------------------

/// A root and one child, each with the `limits:` line given (empty for none).
fn limits_family(root_limits: &str, child_limits: &str) -> AgentDef {
    let line = |limits: &str| {
        if limits.is_empty() {
            String::new()
        } else {
            format!("\nlimits: {{ {limits} }}")
        }
    };
    def(&[
        (
            "agent/instructions.md",
            &instructions(&format!("name: coder{}", line(root_limits)), "Root."),
        ),
        (
            "agent/subagents/child.md",
            &instructions(
                &format!("description: Child.{}", line(child_limits)),
                "Child.",
            ),
        ),
    ])
}

fn limits_of(def: AgentDef, deployment: Option<adam_runtime::SessionLimits>) -> [u32; 3] {
    let def = match deployment {
        Some(limits) => def.session_limits(limits),
        None => def,
    };
    let pure = def.effective_session_limits();
    let assembly = def
        .bind(tools(&[]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    let bound = assembly.session_limits();
    assert_eq!(pure, bound, "the card's limits are the tools' limits");
    [bound.max_open(), bound.max_working(), bound.max_rounds()]
}

#[test]
fn the_deployments_session_limits_are_lowered_by_the_roots_file_and_never_raised() {
    use adam_runtime::SessionLimits;
    // Nothing said: the defaults.
    assert_eq!(limits_of(limits_family("", ""), None), [16, 4, 20]);
    // The deployment's own.
    let deployment = SessionLimits::new(8, 3, 10).unwrap();
    assert_eq!(
        limits_of(limits_family("", ""), Some(deployment)),
        [8, 3, 10]
    );
    // The root's file lowers each, alone or together.
    assert_eq!(
        limits_of(
            limits_family("max_open_sessions: 4, max_session_rounds: 5", ""),
            Some(deployment)
        ),
        [4, 3, 5]
    );
    // And never raises: the deployment's values stand.
    assert_eq!(
        limits_of(
            limits_family(
                "max_open_sessions: 50, max_working_sessions: 30, max_session_rounds: 100",
                ""
            ),
            Some(deployment)
        ),
        [8, 3, 10]
    );
    // Lowering open lowers working with it.
    assert_eq!(
        limits_of(limits_family("max_open_sessions: 2", ""), Some(deployment)),
        [2, 2, 10]
    );
}

#[test]
fn a_subagents_session_limits_are_ignored() {
    assert_eq!(
        limits_of(
            limits_family(
                "",
                "max_open_sessions: 1, max_working_sessions: 1, max_session_rounds: 2"
            ),
            None
        ),
        [16, 4, 20],
        "only the root's file sets the limits of the sessions it starts"
    );
}
