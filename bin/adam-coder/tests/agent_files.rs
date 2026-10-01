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
//!
//! The same files can be read from a folder at startup (`ADAM_AGENT_DIR`, `AgentFiles::load`):
//! the last half of this file runs the coder on a copy of `agent/` in a temp dir, edited the way a
//! deployment would edit a mounted folder.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_coder::{
    AGENT_NAME, AgentFiles, AgentFilesError, Coder, CoderAgent, RuntimeOptions, agent_card,
    agent_card_from, coder_tools,
};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::{Limits, user_message};
use adam_model::{DynModel, Message, MockModel, ModelRequest, ToolCall};
use adam_runtime::Runtime;
use common::{Fixture, edit_instructions, folder};
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
const TOOLS: [&str; 7] = [
    "prepare_workspace",
    "run_command",
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
        // The rule that lets a rework update the same pull request (`prepare_workspace`'s
        // `branch`, which the tool only accepts for a branch this conversation pushed).
        "`branch` set to the branch `commit_and_push` reported",
        "`open_pull_request` at the end, and it reports (and updates)",
        // Looking around is `run_command`, never a check; a missing toolchain is reported and
        // waited on; a question is answered, not turned into a coding workflow.
        "never to look around",
        "that is not a failing check",
        "toolchain is missing and what you needed it for",
        "and do not try to install it",
        "A tool that the **project** brings itself",
        "Do not start the coding workflow",
        "`base_branch` out to start from the repository's default branch",
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

// ------------------------------------------------------------------ run-time folders

fn files_of(folder: &tempfile::TempDir) -> AgentFiles {
    AgentFiles::load(Some(folder.path())).expect("the folder loads")
}

fn coder_from(files: &AgentFiles, fx: &Fixture, mock: &Arc<MockModel>) -> CoderAgent {
    let model: DynModel = mock.clone();
    let tools = coder_tools(&fx.env);
    CoderAgent::try_from_files(files, model, "test-model", fx.env.clone(), tools)
        .expect("the files assemble")
}

/// Start a run on `coder` with `text` and wait until it waits for the person (the model stopped
/// with nothing to deliver, so the run asks).
async fn run_to_a_question(coder: &Coder, text: &str) -> RunId {
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let worker = {
        let runtime = coder.runtime.clone();
        tokio::spawn(async move {
            runtime
                .run_worker(async {
                    let _ = rx.await;
                })
                .await
        })
    };
    let run = coder
        .runtime
        .start(AGENT_NAME, user_message(text), None)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = coder.runtime.view(run).await.unwrap().expect("run exists");
        match view.status {
            RunStatus::Parked if view.waiting => break,
            RunStatus::Done | RunStatus::Failed => panic!("the run ended: {view:#?}"),
            _ => {}
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    run
}

fn options() -> RuntimeOptions {
    RuntimeOptions {
        poll_interval: Duration::from_millis(20),
        ..RuntimeOptions::default()
    }
}

/// A folder that is a copy of the shipped one is the embedded agent: the same digest, the same
/// assembly (prompt, tools, limits) and the same card.
#[tokio::test]
async fn a_copy_of_the_shipped_folder_is_the_embedded_agent() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    let tmp = folder();
    let files = files_of(&tmp);
    assert_eq!(files.describe().source, "folder");
    assert_eq!(files.describe().agent, AGENT_NAME);
    assert_eq!(
        files.describe().digest,
        AgentFiles::load(None).unwrap().describe().digest
    );
    assert!(files.warnings().is_empty(), "{:?}", files.warnings());

    let from_folder = coder_from(&files, &fx, &mock);
    let embedded = coder_from(&AgentFiles::Embedded, &fx, &mock);
    assert_eq!(from_folder.assembly().info(), embedded.assembly().info());
    assert!(from_folder.subagents().is_empty());

    let url: url::Url = "https://agents.example.com/coder/".parse().unwrap();
    assert_eq!(
        format!("{:?}", agent_card_from(&files, &url).unwrap()),
        format!("{:?}", agent_card(&url))
    );
}

/// The point of the feature: edit `instructions.md` in the folder and the model is sent the
/// edited prompt, with no rebuild. The limit is still the process's (`max_check_cycles`).
#[tokio::test]
async fn editing_the_folder_changes_what_the_model_is_sent() {
    let fx = Fixture::with("hello\n", |s| s.max_check_cycles = 5).await;
    let tmp = folder();
    edit_instructions(&tmp, |text| format!("{text}\nAlways answer in French.\n"));
    let files = files_of(&tmp);

    let mock = Arc::new(MockModel::new());
    mock.push_text("Bonjour.");
    let agent = coder_from(&files, &fx, &mock);
    let coder = Coder::new(Arc::new(MemoryStore::new()), agent, &options());
    run_to_a_question(&coder, "hi").await;

    let system = mock.requests()[0].system.clone().unwrap();
    assert!(system.ends_with("\n\nAlways answer in French."), "{system}");
    assert!(system.contains("at most 5 times"), "{system}");
    assert_eq!(
        system,
        format!("{}\n\nAlways answer in French.", expected_prompt(5)),
        "the shipped prompt, and the edit"
    );
    // The embedded copy is what it was: the folder changed this process only.
    assert_eq!(
        coder_from(&AgentFiles::Embedded, &fx, &mock)
            .assembly()
            .info()[0]
            .prompt,
        expected_prompt(5)
    );
}

/// The card comes from the folder too, so a control plane that has no model serves it.
#[tokio::test]
async fn the_card_comes_from_the_folder() {
    let tmp = folder();
    edit_instructions(&tmp, |text| {
        text.replacen("name: adam-coder", "name: Cody", 1)
    });
    let url: url::Url = "https://agents.example.com/coder/".parse().unwrap();
    let card = agent_card_from(&files_of(&tmp), &url).unwrap();
    assert_eq!(card.name, "Cody");
    assert_eq!(card.url, url);
    assert_ne!(card.name, agent_card(&url).name);
}

/// A folder is the coder's: another name would strand the runs stored under `coder`.
#[tokio::test]
async fn a_folder_for_another_agent_is_refused_naming_the_field() {
    let tmp = folder();
    edit_instructions(&tmp, |text| text.replacen("name: coder", "name: other", 1));
    let error = AgentFiles::load(Some(tmp.path())).unwrap_err();
    assert!(matches!(error, AgentFilesError::Name { .. }), "{error}");
    let text = error.to_string();
    assert!(
        text.contains("name: coder") && text.contains("`other`"),
        "{text}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid);
}

/// The process supplies `max_check_cycles`, so a folder must declare it; one that does not fails
/// at startup, naming the var, and so does a tool the coder does not have.
#[tokio::test]
async fn a_folder_that_disagrees_with_the_code_fails_at_assembly() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    let model = || -> DynModel { mock.clone() };

    let tmp = folder();
    edit_instructions(&tmp, |text| {
        text.replacen("  max_check_cycles: 3\n", "", 1)
            .replace("{{max_check_cycles}}", "three")
    });
    let files = files_of(&tmp);
    let error = CoderAgent::try_from_files(
        &files,
        model(),
        "test-model",
        fx.env.clone(),
        coder_tools(&fx.env),
    )
    .err()
    .expect("a folder without the var is refused");
    assert!(error.to_string().contains("max_check_cycles"), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);

    let tmp = folder();
    edit_instructions(&tmp, |text| {
        text.replacen("limits:", "tools: [run_comand]\nlimits:", 1)
    });
    let error = CoderAgent::try_from_files(
        &files_of(&tmp),
        model(),
        "test-model",
        fx.env.clone(),
        coder_tools(&fx.env),
    )
    .err()
    .expect("a tool the coder does not have is refused");
    let text = error.to_string();
    assert!(
        text.contains("run_comand") && text.contains("did you mean `run_command`"),
        "{text}"
    );
}

/// `tools:` in the folder narrows the coder's tools.
#[tokio::test]
async fn a_folder_may_narrow_the_tools() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    let tmp = folder();
    edit_instructions(&tmp, |text| {
        text.replacen("limits:", "tools: [run_command, ask_user]\nlimits:", 1)
    });
    let agent = coder_from(&files_of(&tmp), &fx, &mock);
    assert_eq!(
        agent.assembly().info()[0].tools,
        ["run_command", "ask_user"]
    );
}

/// The subagents of a folder are registered beside the coder: the model calls the subagent's tool,
/// a child run answers, and the coder goes on with the child's text.
#[tokio::test]
async fn a_subagent_of_the_folder_is_registered_and_runs_as_a_child() {
    let fx = Fixture::new("hello\n").await;
    let tmp = folder();
    std::fs::create_dir_all(tmp.path().join("agent/subagents")).unwrap();
    std::fs::write(
        tmp.path().join("agent/subagents/reviewer.md"),
        "---\ndescription: Reviews a diff.\ntools: [run_command]\n---\nYou review diffs.\n",
    )
    .unwrap();
    let files = files_of(&tmp);

    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "c1".into(),
        name: "reviewer".into(),
        arguments: json!({"message": "review the diff"}),
    }])
    .push_text("LGTM, nothing to fix.")
    .push_text("The reviewer is happy.");
    let agent = coder_from(&files, &fx, &mock);
    let info = agent.assembly().info();
    assert_eq!(
        info.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        ["coder", "coder/reviewer"]
    );
    assert_eq!(agent.subagents().len(), 1);
    assert_eq!(info[0].tools.last().map(String::as_str), Some("reviewer"));

    let coder = Coder::new(Arc::new(MemoryStore::new()), agent, &options());
    run_to_a_question(&coder, "have it reviewed").await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let offered =
        |r: &ModelRequest| -> Vec<String> { r.tools.iter().map(|t| t.name.clone()).collect() };
    assert!(offered(&requests[0]).contains(&"reviewer".to_owned()));
    // The child: its own prompt and the one tool it picked from the coder's.
    assert_eq!(requests[1].system.as_deref(), Some("You review diffs."));
    assert_eq!(offered(&requests[1]), ["run_command"]);
    // The coder goes on with what the child said.
    match requests[2].messages.last().unwrap() {
        Message::Tool {
            call_id, content, ..
        } => {
            assert_eq!(call_id, "c1");
            assert_eq!(content, "LGTM, nothing to fix.");
        }
        other => panic!("{other:?}"),
    }
}
