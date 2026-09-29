//! Subagents: each is a child run of its parent, called through a tool named after it. Run end to end
//! on `MockModel` through the runtime, on the memory store always and on PostgreSQL when
//! `ADAM_TEST_POSTGRES_URL` is set. Every case has agent names of its own, so cases that share a
//! database never claim each other's runs.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use adam_agent_fixture::AGENT;
use adam_agent_fs::Subagent;
use adam_assembly::{AgentDef, Assembly, Error, SubagentTool, ToolClash};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus, Store};
use adam_llm_agent::{FnTool, LlmStarter, Tool, ToolError, ToolOutput, ToolSet, user_message};
use adam_model::{Message, MockModel, ModelRequest, ToolCall};
use adam_runtime::{Runtime, child_run_id};
use common::{
    def, instructions, manifests, runtime_on, spawn_worker, stub, tools, wait_done, wait_for,
};
use serde_json::{Value, json};

const NOTE: &str =
    "The agent does not see this conversation; put everything it needs in `message`.";

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
    }
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
            .push_tool_calls(vec![call(
                "c1",
                "researcher",
                json!({"message": "find the release date"}),
            )])
            .push_tool_calls(vec![call("f1", "fetch", json!({}))])
            .push_tool_calls(vec![call(
                "s1",
                "summarizer",
                json!({"message": "the page text"}),
            )])
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
        // The parent: its own tool, and one for the subagent, described by the subagent's file.
        assert_eq!(r0.system.as_deref(), Some("You are the root."));
        assert_eq!(offered(r0), ["parent_only", "researcher"]);
        assert_eq!(
            r0.tools[1].description,
            format!("Researches a topic. {NOTE}")
        );
        assert_eq!(r0.tools[1].parameters["required"], json!(["message"]));
        assert_eq!(user_texts(r0), ["Find it"]);
        // The child: its own prompt, its own tools (and its own subagent), and a history that
        // starts with `message`, not with the parent's conversation.
        for r in [r1, r2] {
            assert_eq!(r.system.as_deref(), Some("You research."));
            assert_eq!(offered(r), ["fetch", "summarizer"]);
            assert_eq!(user_texts(r), ["find the release date"]);
            assert_eq!(r.model, "big", "a subagent inherits the parent's alias");
        }
        assert_eq!(r1.messages.len(), 1);
        assert_eq!(last_tool_result(r2, "f1"), ("fetch-out".into(), false));
        // The grandchild: no tools at all, and nothing but its message.
        assert_eq!(r3.system.as_deref(), Some("You summarise."));
        assert!(r3.tools.is_empty());
        assert_eq!(r3.messages.len(), 1);
        assert_eq!(user_texts(r3), ["the page text"]);
        // Each answer comes back as the result of the call that started it.
        assert_eq!(last_tool_result(r4, "s1"), ("ten lines".into(), false));
        assert_eq!(
            last_tool_result(r5, "c1"),
            ("report: ten lines".into(), false)
        );
        assert_eq!(r5.system.as_deref(), Some("You are the root."));
        // Only the child used `fetch`; the parent's tool was not called by anyone.
        assert_eq!(fetch_calls.load(SeqCst), 1);
        assert_eq!(parent_calls.load(SeqCst), 0);

        // The children are runs of their own, recorded under their parents, and done.
        let researcher = child_run_id(run, "c1");
        let child = store.load_run(researcher).await.unwrap().unwrap();
        assert_eq!(child.agent, format!("{root}/researcher"));
        assert_eq!(child.parent_id, Some(run));
        assert_eq!(child.status, RunStatus::Done);
        let summarizer = child_run_id(researcher, "s1");
        let grandchild = store.load_run(summarizer).await.unwrap().unwrap();
        assert_eq!(grandchild.agent, format!("{root}/researcher/summarizer"));
        assert_eq!(grandchild.parent_id, Some(researcher));
        assert_eq!(grandchild.status, RunStatus::Done);

        worker.stop().await;
    }
}

#[tokio::test]
async fn the_embedded_fixture_runs_a_subagent_end_to_end() {
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call(
            "c1",
            "reviewer",
            json!({"message": "Review this diff"}),
        )])
        .push_text("LGTM")
        .push_text("Reviewed: LGTM");
    let assembly = AgentDef::from_manifest(AGENT)
        .unwrap()
        .bind(tools(&[
            "prepare_workspace",
            "run_checks",
            "ask_user",
            "linear__list_issues",
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
    // The reviewer, from the fixture's own file: its prompt, its two tools, its limits.
    assert_eq!(
        requests[1].system.as_deref(),
        Some("You review diffs. Report findings as a numbered list.")
    );
    assert_eq!(offered(&requests[1]), ["read_diff", "list_files"]);
    assert_eq!(requests[1].max_output_tokens, Some(4096));
    assert_eq!(user_texts(&requests[1]), ["Review this diff"]);
    worker.stop().await;
}

// --- isolation -----------------------------------------------------------------------------

#[tokio::test]
async fn a_child_cannot_call_a_tool_only_its_parent_has() {
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "helper", json!({"message": "go"}))])
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
    // The bound tool lists: the parent has its tool and one for the helper; the helper has only
    // what it lists.
    let tools_of = |name: &str| {
        assembly
            .info()
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .tools
            .clone()
    };
    assert_eq!(tools_of(&root), ["parent_only", "helper"]);
    assert_eq!(tools_of(&format!("{root}/helper")), ["fetch"]);

    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;

    let requests = model.requests();
    // What the model was offered.
    assert_eq!(offered(&requests[0]), ["parent_only", "helper"]);
    assert_eq!(offered(&requests[1]), ["fetch"]);
    // The refusal the child's model got, and that the tool did not run.
    let (text, is_error) = last_tool_result(&requests[2], "x1");
    assert!(is_error);
    assert_eq!(text, "unknown tool `parent_only`; available tools: fetch");
    assert_eq!(parent_calls.load(SeqCst), 0);
    assert_eq!(fetch_calls.load(SeqCst), 0);
    // The parent got the child's answer all the same.
    assert_eq!(
        last_tool_result(&requests[3], "c1"),
        ("I could not".into(), false)
    );
    worker.stop().await;
}

#[test]
fn a_subagent_does_not_inherit_a_tool_unless_it_lists_it() {
    let root = uniq("root");
    let files = with_helper(&root, "", "");
    // The root (no `tools:`) gets everything; the helper with no `tools:` gets nothing.
    let assembly = assemble(&files, tools(&["a", "b"]), &Arc::new(MockModel::new()));
    let info = assembly.info();
    assert_eq!(info[0].tools, ["a", "b", "helper"]);
    assert!(info[1].tools.is_empty());
    // And listing one gives it exactly that one.
    let files = with_helper(&root, "", "tools: [b]");
    let assembly = assemble(&files, tools(&["a", "b"]), &Arc::new(MockModel::new()));
    assert_eq!(assembly.info()[1].tools, ["b"]);
}

// --- limits --------------------------------------------------------------------------------

#[tokio::test]
async fn a_child_that_goes_over_its_own_limit_fails_and_the_parent_is_told() {
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "helper", json!({"message": "loop"}))])
        .push_tool_calls(vec![call("f1", "fetch", json!({}))])
        .push_tool_calls(vec![call("f2", "fetch", json!({}))])
        // The child's third turn is over its limit: the model is not asked, and this is the
        // parent's next answer.
        .push_text("the helper failed, I will do it myself");
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
        "the helper failed, I will do it myself"
    );

    let requests = model.requests();
    assert_eq!(
        requests.len(),
        4,
        "the child's third turn never asked the model"
    );
    let (text, is_error) = last_tool_result(&requests[3], "c1");
    assert!(is_error, "{text}");
    assert!(text.starts_with("the run failed: "), "{text}");
    assert!(text.contains("turn limit exceeded"), "{text}");
    assert!(text.contains("max_turns = 2"), "{text}");

    let child = rt.view(child_run_id(run, "c1")).await.unwrap().unwrap();
    assert_eq!(child.status, RunStatus::Failed);
    worker.stop().await;
}

#[tokio::test]
async fn the_turns_of_a_child_are_not_the_parents() {
    // The parent may make two model calls in all; the child makes two on its own account. Had the
    // child's turns counted against the parent it would fail on the parent's second.
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "helper", json!({"message": "go"}))])
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
async fn a_call_without_a_message_starts_no_child() {
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "helper", json!({"msg": "typo"}))])
        .push_text("ok");
    let files = with_helper(&root, "tools: []", "");
    let assembly = assemble(&files, ToolSet::new(), &model);
    let rt = runtime_on(&assembly, Arc::new(MemoryStore::new()));
    let worker = spawn_worker(&rt);
    let run = rt.start(&root, user_message("go"), None).await.unwrap();
    wait_done(&rt, run).await;
    let (text, is_error) = last_tool_result(&model.requests()[1], "c1");
    assert!(is_error);
    assert!(text.contains("`helper` needs `message`"), "{text}");
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
    // Only the root is registered (not `Assembly::register`): the call is refused for good, with
    // the name of the missing agent, and is not retried.
    let root = uniq("root");
    let model = Arc::new(MockModel::new());
    model
        .push_tool_calls(vec![call("c1", "helper", json!({"message": "go"}))])
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
            .push_tool_calls(vec![call("c1", "helper", json!({"message": "work"}))])
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
                && kid.parent_id == Some(run))
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
        assert_eq!(
            last_tool_result(&requests[2], "c1"),
            ("the child's answer".into(), false)
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
        "agent `coder/helper` (agent/subagents/helper.md): the parent `coder` would get a tool \
         called `helper` to call this subagent, but it already has a tool with that name: rename \
         the subagent, or leave the tool out of the parent's `tools`"
    );
}

#[test]
fn a_registered_tool_the_parent_does_not_list_is_no_clash() {
    let files = with_helper("coder", "tools: [other]", "");
    let assembly = assemble(
        &files,
        tools(&["other", "helper"]),
        &Arc::new(MockModel::new()),
    );
    assert_eq!(assembly.info()[0].tools, ["other", "helper"]);
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
        error
            .to_string()
            .contains("its skills bring a tool with that name"),
        "{error}"
    );

    // Without skills the name is free.
    files.pop();
    files.retain(|(path, _)| !path.starts_with("agent/skills"));
    let assembly = assemble(&files, ToolSet::new(), &Arc::new(MockModel::new()));
    assert_eq!(assembly.info()[0].tools, ["helper", "load_skill"]);
}

#[test]
fn a_registered_tool_named_like_a_skill_tool_is_a_plain_tool_clash_without_skills() {
    let mut files = with_helper("coder", "tools: [load_skill]", "");
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
            .contains("already has a tool with that name"),
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
    // researcher lists `summarizer` (a tool) and has a subagent of that name.
    let mut files = nested("coder");
    files[1].1 = instructions(
        "description: Researches a topic.\ntools: [summarizer]",
        "You research.",
    );
    let error = bind_error(&files, tools(&["parent_only", "summarizer"]));
    let Error::SubagentToolClash { origin, parent, .. } = &error else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(parent, "coder/researcher");
    assert_eq!(origin.agent, "coder/researcher/summarizer");
}

// --- a subagent cannot ask -----------------------------------------------------------------

fn asking(name: &str) -> FnTool {
    FnTool::raw(
        name,
        "Ask the user.",
        json!({"type": "object", "properties": {}}),
        |_ctx, _args| async move {
            Err::<ToolOutput, _>(ToolError::NeedsInput {
                question: "which one?".into(),
            })
        },
    )
    .asking_user()
}

#[test]
fn a_subagent_with_a_tool_that_asks_the_user_is_refused() {
    let files = with_helper("coder", "tools: [ask_user]", "tools: [ask_user]");
    let registered = || ToolSet::new().tool(asking("ask_user")).tool(stub("fetch"));
    let error = bind_error(&files, registered());
    let Error::SubagentAsksUser { origin, tool } = &error else {
        panic!("wrong variant: {error}");
    };
    assert_eq!(origin.agent, "coder/helper");
    assert_eq!(origin.file.to_string_lossy(), "agent/subagents/helper.md");
    assert_eq!(tool, "ask_user");
    assert_eq!(
        error.to_string(),
        "agent `coder/helper` (agent/subagents/helper.md): a subagent cannot have `ask_user`: it \
         asks the user a question, and a subagent runs as a child of another run, so nobody could \
         answer it. List its `tools` by name, without `ask_user`, or let the parent ask instead"
    );

    // Also when it gets the tool by `all` or by a pattern, not by name.
    for listed in ["tools: '*'", "tools: [fetch, 'ask*']"] {
        let files = with_helper("coder", "tools: [ask_user]", listed);
        assert!(
            matches!(
                bind_error(&files, registered()),
                Error::SubagentAsksUser { .. }
            ),
            "{listed}"
        );
    }
    // And below the root.
    let mut files = nested("coder");
    files[2].1 = instructions(
        "description: Summarises.\ntools: [ask_user]",
        "You summarise.",
    );
    let error = bind_error(&files, registered().tool(stub("parent_only")));
    assert!(
        matches!(&error, Error::SubagentAsksUser { origin, .. } if origin.agent == "coder/researcher/summarizer"),
        "{error}"
    );
}

#[test]
fn the_root_may_ask_and_a_subagent_that_does_not_is_fine() {
    let files = with_helper("coder", "tools: [ask_user]", "tools: [fetch]");
    let assembly = assemble(
        &files,
        ToolSet::new().tool(asking("ask_user")).tool(stub("fetch")),
        &Arc::new(MockModel::new()),
    );
    assert_eq!(assembly.info()[0].tools, ["ask_user", "helper"]);
    assert_eq!(assembly.info()[1].tools, ["fetch"]);
}

// --- the tool itself -----------------------------------------------------------------------

#[test]
fn a_subagent_tool_can_be_made_by_hand() {
    let tool = SubagentTool::new("planner", "planner-agent", "Plans.");
    assert_eq!(tool.agent(), "planner-agent");
    assert_eq!(tool.spec().name, "planner");
    assert!(!tool.asks_user());
    assert!(tool.spec().description.starts_with("Plans. "));
}
