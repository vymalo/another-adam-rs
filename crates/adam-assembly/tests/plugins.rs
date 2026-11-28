//! `plugins:` of an agent file (ADR 0050, slice H6a): a name the deployment registered, inherited
//! by subagents; a name nobody registered is a bind error, which every host reports as an invalid
//! configuration (exit 78). The host's own hooks (`BoundDef::hooks`) run in every agent, before the
//! file's.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};

use adam_assembly::Error;
use adam_core::{MemoryStore, RunId, RunStatus};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::{Conversation, Decision, user_message};
use adam_model::{Message, MockModel, ToolCall};
use adam_plugin::HookSet;
use adam_plugin_testkit::{Allows, Answers};
use adam_runtime::Runtime;
use common::{def, instructions, spawn_worker, tools};
use serde_json::json;

const INSTRUCTIONS: &str = "agent/instructions.md";

#[test]
fn a_plugin_nobody_registered_stops_the_agent_with_a_suggestion() {
    let error = def(&[(
        INSTRUCTIONS,
        &instructions("name: coder\nplugins: [audt]", "Hi."),
    )])
    .plugin("audit", Arc::new(Allows::default()))
    .bind(tools(&[]))
    .unwrap_err();
    let Error::UnknownPlugin {
        plugin, suggestion, ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(
        (plugin.as_str(), suggestion.as_deref()),
        ("audt", Some("audit"))
    );
    // An invalid configuration: the hosts exit 78 on this class.
    assert_eq!(error.class(), ErrorClass::Invalid);
    // No plugin registered at all is the same refusal.
    let error = def(&[(
        INSTRUCTIONS,
        &instructions("name: coder\nplugins: [audit]", "Hi."),
    )])
    .bind(tools(&[]))
    .unwrap_err();
    assert!(matches!(error, Error::UnknownPlugin { .. }), "{error}");
}

#[test]
fn a_subagent_runs_its_parents_plugins_and_its_own() {
    let assembly = def(&[
        (
            INSTRUCTIONS,
            &instructions("name: coder\nplugins: [audit]", "Root."),
        ),
        (
            "agent/subagents/child.md",
            &instructions("description: Child.\nplugins: [snapshot]", "Child."),
        ),
        (
            "agent/subagents/plain.md",
            &instructions("description: Plain.", "Plain."),
        ),
    ])
    .plugin("audit", Arc::new(Allows::default()))
    .plugin("snapshot", Arc::new(Allows::default()))
    .bind(tools(&[]))
    .unwrap()
    .model(Arc::new(MockModel::new()), "m")
    .unwrap();
    let plugins_of = |name: &str| {
        assembly
            .info()
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .plugins
            .clone()
    };
    assert_eq!(plugins_of("coder"), ["audit"]);
    assert_eq!(plugins_of("coder/child"), ["audit", "snapshot"]);
    assert_eq!(plugins_of("coder/plain"), ["audit"]);
}

#[test]
fn an_agent_with_no_plugins_key_runs_none() {
    let assembly = def(&[(INSTRUCTIONS, &instructions("name: coder", "Hi."))])
        .plugin("audit", Arc::new(Allows::default()))
        .bind(tools(&[]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assert!(assembly.info()[0].plugins.is_empty());
}

/// The host's hooks (`BoundDef::hooks`) reach the root and a subagent, and run before the plugins a
/// file names (the subagent runs its parent's): the host's counter is asked before the file's
/// plugin denies the call, so the counter sees the root's call and the subagent's.
#[tokio::test]
async fn the_hosts_hooks_run_in_every_agent_before_the_files_plugins() {
    let host = Arc::new(Allows::default());
    let mock = Arc::new(MockModel::new());
    let call = |id: &str| ToolCall {
        id: id.into(),
        name: "stub".into(),
        arguments: json!({}),
        ..Default::default()
    };
    mock.push_tool_calls(vec![call("root-1")])
        .push_text("root done")
        .push_tool_calls(vec![call("child-1")])
        .push_text("child done");
    let assembly = def(&[
        (
            INSTRUCTIONS,
            &instructions("name: coder\ntools: [stub]\nplugins: [deny]", "Root."),
        ),
        (
            "agent/subagents/child.md",
            &instructions("description: Child.\ntools: [stub]", "Child."),
        ),
    ])
    .plugin(
        "deny",
        Arc::new(Answers(Decision::Deny("the file says no".into()))),
    )
    .bind(tools(&["stub"]))
    .unwrap()
    .hooks(HookSet::new().with(host.clone()))
    .model(mock, "m")
    .unwrap();
    // The file's plugins are what `info` lists; the host's are not the file's.
    assert_eq!(assembly.info()[0].plugins, ["deny"]);
    let rt = assembly
        .register(Runtime::builder(Arc::new(MemoryStore::new())))
        .poll_interval(Duration::from_millis(20))
        .build();
    let worker = spawn_worker(&rt);
    let result_of = |state: Conversation, id: &str| {
        state
            .messages
            .iter()
            .find_map(|m| match m {
                Message::Tool { call_id, .. } if call_id == id => Some(m.text()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {id}"))
    };

    let root = rt.start("coder", user_message("go"), None).await.unwrap();
    let state = done(&rt, root).await;
    assert_eq!(
        host.before.load(SeqCst),
        1,
        "the host's hook saw the root's call"
    );
    assert!(
        result_of(state, "root-1").contains("the file says no"),
        "then the file's plugin denied it"
    );

    let child = rt
        .start("coder/child", user_message("go"), None)
        .await
        .unwrap();
    let state = done(&rt, child).await;
    assert_eq!(host.before.load(SeqCst), 2, "and the subagent's");
    assert!(
        result_of(state, "child-1").contains("the file says no"),
        "the subagent runs its parent's plugin after the host's"
    );
    worker.stop().await;
}

/// The run's state once it is done.
async fn done(rt: &Runtime, run: RunId) -> Conversation {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.status == RunStatus::Done {
            let state = rt.state(run).await.unwrap().unwrap();
            return serde_json::from_value(state).unwrap();
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
