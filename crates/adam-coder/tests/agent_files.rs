//! The coder's prompt, limits and A2A card live in `agent/instructions.md` (slice S6b of the
//! authoring layer), not in Rust. These tests pin what the files must keep producing:
//!
//! * `fixtures/agent/prompt.txt` is the system prompt as it was when it was a Rust constant
//!   (`instructions.rs`), with its one placeholder spelled `{{max_check_cycles}}`; it follows
//!   `agent/instructions.md` whenever the prompt is changed on purpose (the body of that file,
//!   after the front matter, is this file);
//! * the limits, the tool order, the step names and what the model is sent are the ones the
//!   hand-written agent had, so a run journaled by the previous version replays;
//! * the card is the one `agent_card()` used to build (`fixtures/agent/card.json`, pinned by a
//!   unit test in `app.rs`; here the assembled agent is compared with it).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::{AGENT_NAME, CoderAgent, agent_card};
use adam_core::{DynStore, MemoryStore, RunStatus};
use adam_llm_agent::{Limits, user_message};
use adam_model::{DynModel, MockModel, ToolCall};
use adam_runtime::Runtime;
use common::Fixture;
use serde_json::json;

/// The prompt as it was before the move, with `{{max_check_cycles}}` where the limit goes.
const GOLDEN_PROMPT: &str = include_str!("fixtures/agent/prompt.txt");

/// What the model is sent for `cycles`: the old prompt with the limit put in. The one difference
/// from the old constant is the file's final newline, which the loader drops from every body
/// (`adam-agent-fs` trims trailing whitespace), so it is dropped here too.
fn expected_prompt(cycles: u32) -> String {
    let old = GOLDEN_PROMPT.replace("{{max_check_cycles}}", &cycles.to_string());
    assert!(
        old.ends_with(".\n"),
        "the old prompt ended with one newline"
    );
    old.trim_end().to_owned()
}

/// The tools in the order the model is offered them.
const TOOLS: [&str; 6] = [
    "prepare_workspace",
    "delegate_to_opencode",
    "run_checks",
    "commit_and_push",
    "open_pull_request",
    "ask_user",
];

async fn coder(cycles: u32) -> (Fixture, CoderAgent) {
    let fx = Fixture::with("hello\n", |s| s.max_check_cycles = cycles).await;
    let model: DynModel = Arc::new(MockModel::new());
    let agent = CoderAgent::new(model, "test-model", fx.env.clone());
    (fx, agent)
}

fn prompt_of(agent: &CoderAgent) -> &str {
    &agent.assembly().info()[0].prompt
}

/// What the rules in the tools rely on the model having been told (this used to test the
/// `instructions()` function; now it tests the prompt the agent was assembled with).
#[tokio::test]
async fn the_prompt_carries_the_rules_the_code_relies_on() {
    let (_fx, agent) = coder(3).await;
    let text = prompt_of(&agent);
    for needle in [
        "Never open a pull request while the last check run failed",
        "accept_red_checks: true",
        "ask_user",
        "CLAUDE.md",
        "justfile",
        "small, focused commits",
        "verification section",
    ] {
        assert!(text.contains(needle), "prompt lost: {needle}");
    }
}

#[tokio::test]
async fn the_limit_is_templated_in_from_the_settings() {
    let (_fx, agent) = coder(7).await;
    let text = prompt_of(&agent);
    assert!(text.contains("at most 7 times"), "{text}");
    assert!(!text.contains("{{"), "unreplaced placeholder");
}

/// The old prompt, whatever the limit, byte for byte but for its final newline (see
/// [`expected_prompt`]): the file did not change the wording, and the var replaced what
/// `str::replace` on `{{MAX_CHECK_CYCLES}}` did.
#[tokio::test]
async fn the_prompt_equals_the_one_that_was_a_rust_constant() {
    assert_eq!(
        GOLDEN_PROMPT.matches("{{max_check_cycles}}").count(),
        1,
        "one placeholder"
    );
    for cycles in [1, 3, 7, 25] {
        let (_fx, agent) = coder(cycles).await;
        assert_eq!(
            prompt_of(&agent),
            expected_prompt(cycles),
            "with {cycles} check cycles"
        );
    }
}

#[tokio::test]
async fn the_assembled_agent_is_what_the_hand_written_one_was() {
    let (_fx, agent) = coder(3).await;
    let info = &agent.assembly().info()[0];
    assert_eq!(info.name, AGENT_NAME);
    assert_eq!(info.model_alias, "test-model");
    assert_eq!(info.parent, None);
    assert_eq!(
        info.tools, TOOLS,
        "the tools, in the order they are offered"
    );
    assert!(info.skills.is_empty() && info.preloaded.is_empty());
    // What `coder_limits()` returned.
    assert_eq!(
        info.limits,
        Limits {
            max_turns: 200,
            max_tool_calls: 400,
            max_output_tokens: 8192,
            max_history_tokens: 100_000,
        }
    );
    assert_eq!(agent.assembly().agents().len(), 1, "no subagents");
    assert!(agent.assembly().remotes().is_empty());
}

#[tokio::test]
async fn a_model_alias_the_assembly_refuses_is_an_error_not_a_panic_for_try_new() {
    let fx = Fixture::new("hello\n").await;
    let model = || -> DynModel { Arc::new(MockModel::new()) };
    for alias in ["", "two words"] {
        let err = CoderAgent::try_new(model(), alias, fx.env.clone())
            .err()
            .unwrap_or_else(|| panic!("alias {alias:?} was accepted"));
        assert!(
            matches!(*err, adam::AssemblyError::ModelAlias { .. }),
            "{err}"
        );
    }
}

/// The assembled agent's card and the one a control plane serves (`agent_card`, which has no
/// assembly) are the same.
#[tokio::test]
async fn the_assemblys_card_is_the_card_a_control_plane_serves() {
    let (_fx, agent) = coder(3).await;
    let url: url::Url = "https://agents.example.com/coder/".parse().unwrap();
    let assembled = agent
        .assembly()
        .card(url.clone(), env!("CARGO_PKG_VERSION"))
        .unwrap();
    assert_eq!(format!("{assembled:?}"), format!("{:?}", agent_card(&url)));
}

/// A run through a runtime: what the model is sent (the prompt, the tools, the limits) and what
/// the journal records (the step names, which a replay checks).
#[tokio::test]
async fn a_run_sends_the_old_request_and_journals_the_old_step_names() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    // No workspace yet: the tool answers with an error result and has no effect.
    mock.push_tool_calls(vec![ToolCall {
        id: "call-1".into(),
        name: "run_checks".into(),
        arguments: json!({"command": "true"}),
    }]);
    mock.push_text("There is nothing to do.");
    let model: DynModel = mock.clone();
    let agent = CoderAgent::new(model, "test-model", fx.env.clone());

    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store.clone())
        .agent(agent)
        .poll_interval(Duration::from_millis(20))
        .build();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let worker = {
        let rt = rt.clone();
        tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        })
    };
    let run = rt
        .start(AGENT_NAME, user_message("fix it"), None)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().expect("run exists");
        match view.status {
            // The model stopped with text and nothing delivered: that is a question, so the run
            // waits for the person instead of completing.
            RunStatus::Parked if view.waiting => break,
            RunStatus::Done => panic!("a stop that delivered nothing completed: {view:#?}"),
            RunStatus::Failed => panic!("the run failed: {view:#?}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    worker.await.unwrap().unwrap();

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let first = &requests[0];
    assert_eq!(first.model, "test-model");
    assert_eq!(first.system.as_deref(), Some(expected_prompt(3).as_str()));
    assert_eq!(first.max_output_tokens, Some(8192));
    let names: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, TOOLS);

    let journal = store.journal_list(run).await.unwrap();
    let steps: Vec<&str> = journal.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(steps, ["model:0", "tool:call-1", "model:1"]);
    assert!(journal.iter().all(|e| e.ok));
}
