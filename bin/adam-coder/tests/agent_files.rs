//! The coder's prompt, limits and A2A card live in `agent/instructions.md` (slice S6b of the
//! authoring layer), not in Rust. These tests pin what the files must keep producing:
//!
//! * `fixtures/agent/prompt.txt` is the **instruction snapshot**: the body of
//!   `agent/instructions.md` (everything after the front matter) with its placeholders spelled
//!   `{{max_check_cycles}}` and `{{display_name}}`. Changing the instructions on purpose means
//!   changing this file in the same commit, so the change of what the model is told is a reviewed
//!   diff (`the_prompt_equals_the_snapshot`). It began as the prompt of the Rust constant the
//!   file replaced, and #55 (the coder's name and plain words) is the first deliberate change;
//! * the limits, the tool order, the step names and what the model is sent are the ones the
//!   hand-written agent had, so a run journaled by the previous version replays;
//! * the card is `fixtures/agent/card.json` (pinned by a unit test in `app.rs`; here the
//!   assembled agent is compared with it).
//!
//! The same files can be read from a folder at startup (`ADAM_AGENT_DIR`, `AgentFiles::load`):
//! the last half of this file runs the coder on a copy of `agent/` in a temp dir, edited the way a
//! deployment would edit a mounted folder.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam::mcp::McpPolicy;
use adam_coder::{
    AGENT_NAME, AgentFiles, AgentFilesError, Coder, CoderAgent, RuntimeOptions, agent_card,
    agent_card_from, coder_tools,
};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::{Limits, user_message};
use adam_mcp_testkit::TestHttpServer;
use adam_model::{
    DynModel, Message, MockModel, ModelClient, ModelDelta, ModelError, ModelRequest, ModelResponse,
    ToolCall,
};
use adam_runtime::Runtime;
use async_trait::async_trait;
// A folder here is the shipped agent without its `mcp.json` (the shipped one names the GitHub
// server, a local process: see `common::plain_folder`); the tests of `mcp.json` write their own.
use common::{Fixture, edit_instructions, plain_folder as folder};
use futures::stream::BoxStream;
use serde_json::json;

/// The instruction snapshot: the body of `agent/instructions.md`, with `{{max_check_cycles}}`
/// where the limit goes and `{{display_name}}` where the name goes.
const SNAPSHOT: &str = include_str!("fixtures/agent/prompt.txt");

/// What the model is sent for `cycles` and the shipped name: the snapshot with both put in. The
/// snapshot's final newline is dropped, as the loader drops it from every body
/// (`adam-agent-fs` trims trailing whitespace).
fn expected_prompt(cycles: u32) -> String {
    let rendered = SNAPSHOT
        .replace("{{max_check_cycles}}", &cycles.to_string())
        .replace("{{display_name}}", "Coder");
    assert!(
        rendered.ends_with(".\n"),
        "the snapshot ends with one newline"
    );
    rendered.trim_end().to_owned()
}

/// The tools in the order the model is offered them.
const TOOLS: [&str; 17] = [
    "prepare_workspace",
    "start_scratch",
    "publish_scratch",
    "request_repository",
    "create_repository",
    "run_command",
    "read_file",
    "write_file",
    "apply_patch",
    "delegate_to_opencode",
    "run_checks",
    "rebuild_environment",
    "commit_and_push",
    "open_pull_request",
    "ask_user",
    "show",
    "ui_catalog",
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

/// #55: what the instructions tell the model about who it is. The needles are single lines of the
/// prompt (it is wrapped by hand), and the test that follows pins the whole text.
#[tokio::test]
async fn the_prompt_gives_the_agent_a_name_and_asks_for_plain_words() {
    let (_fx, agent) = coder(3).await;
    let text = prompt_of(&agent);
    for needle in [
        // The two persona lines the mocks read, first.
        "Your name is Coder.\nIn one sentence: I take a repository you name,",
        "and never say",
        "A greeting gets a greeting.",
        "\"list your tools\".**",
        "cannot do, and why:",
        "you can create one only where this deployment allows it",
        "Do not\n  list the tools.",
        // A greeting is answered, not treated as a missing task.
        "so do not call a tool for it",
    ] {
        assert!(text.contains(needle), "prompt lost: {needle}\n{text}");
    }
    assert!(text.starts_with("Your name is Coder."), "{text}");
    assert!(!text.contains("{{"), "unreplaced placeholder");
    // The old instruction that made a greeting a task with something missing is gone.
    assert!(!text.contains("A greeting or a\n   vague request is not a task"));
}

#[tokio::test]
async fn the_limit_is_templated_in_from_the_settings() {
    let (_fx, agent) = coder(7).await;
    let text = prompt_of(&agent);
    assert!(text.contains("at most 7 times"), "{text}");
    assert!(!text.contains("{{"), "unreplaced placeholder");
}

/// The instruction snapshot, whatever the limit, byte for byte but for its final newline (see
/// [`expected_prompt`]). A change to `agent/instructions.md` that is not in the snapshot fails
/// here, and the diff of the snapshot is what a reviewer reads.
#[tokio::test]
async fn the_prompt_equals_the_snapshot() {
    assert_eq!(
        SNAPSHOT.matches("{{max_check_cycles}}").count(),
        1,
        "one limit placeholder"
    );
    assert!(
        SNAPSHOT.starts_with("Your name is {{display_name}}.\nIn one sentence: "),
        "the body opens with the two persona lines the mocks read"
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
    // The assembly knows nothing of the screen, of steps or of streaming: the card of the process
    // adds the extensions the screen's tools need, `steps/v1` and `text-stream/v1`.
    let assembled = adam_ui::with_card_extensions(
        agent
            .assembly()
            .card(url.clone(), env!("CARGO_PKG_VERSION"))
            .unwrap(),
    )
    .with_extension(adam_a2a::ExtensionConfig::steps())
    .with_extension(adam_a2a::ExtensionConfig::text_stream());
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

/// The coder assembled from `files`, as the sync path does it: no server of an `mcp.json` is
/// connected, so the agent has the coder's tools and no MCP ones (the embedded copy names the
/// GitHub server, and a definition that names servers and was not given tools is refused:
/// `an_mcp_json_that_was_not_connected_is_refused_at_assembly`).
fn coder_from(files: &AgentFiles, fx: &Fixture, mock: &Arc<MockModel>) -> CoderAgent {
    let model: DynModel = mock.clone();
    let tools = coder_tools(&fx.env);
    let def = files
        .def()
        .expect("the files make a definition")
        .mcp_tools(AGENT_NAME, adam_llm_agent::ToolSet::new());
    CoderAgent::try_from_def(def, model, "test-model", fx.env.clone(), tools)
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
    // The shipped files as they are, `mcp.json` included: the digest covers it.
    let tmp = common::folder();
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
        text.replacen("  name: Coder", "  name: Cody", 1)
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

/// A model that answers the way the `mock-coder` greeting mapping of `dev/wiremock/mock-openai`
/// does: it reads the two persona lines at the top of its system prompt (`Your name is X.`, `In one
/// sentence: Y.`) and greets back with them. It is a scripted model whose script is the prompt, so
/// what a test edits in the folder is what the answer says.
struct PersonaModel {
    answers: std::sync::Mutex<Vec<String>>,
}

impl PersonaModel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            answers: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn answers(&self) -> Vec<String> {
        self.answers.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelClient for PersonaModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        let system = req.system.unwrap_or_default();
        let line = |prefix: &str| {
            system
                .lines()
                .find_map(|l| l.strip_prefix(prefix))
                .map(|rest| rest.trim_end_matches('.').to_owned())
                .ok_or_else(|| ModelError::invalid_request(format!("no `{prefix}` line")))
        };
        let answer = format!(
            "Hi! I'm {}. {}. Which repository should I work on, and what should I change?",
            line("Your name is ")?,
            line("In one sentence: ")?
        );
        self.answers.lock().unwrap().push(answer.clone());
        Ok(ModelResponse::text(answer))
    }

    /// The coder streams its model calls: the greeting is written in two pieces and then whole.
    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let response = self.complete(req).await?;
        let text = response.message.text();
        let middle = text
            .char_indices()
            .nth(text.chars().count() / 2)
            .map_or(text.len(), |(at, _)| at);
        let (head, tail) = (text[..middle].to_owned(), text[middle..].to_owned());
        Ok(futures::StreamExt::boxed(futures::stream::iter([
            Ok(ModelDelta::Text(head)),
            Ok(ModelDelta::Text(tail)),
            Ok(ModelDelta::Finished(response)),
        ])))
    }
}

/// "hi" gets a greeting that carries the name and the one-sentence summary, not a request for a
/// task and not a tool call, and the run waits for the answer; editing the persona lines in the
/// mounted folder changes what is said, with no rebuild. (The model here is scripted by the
/// prompt; what a live model does with it is unverified.)
#[tokio::test]
async fn a_greeting_gets_a_greeting_and_the_folder_changes_what_it_says() {
    let fx = Fixture::new("hello\n").await;

    let greet = |files: AgentFiles| {
        let fx = &fx;
        async move {
            let model = PersonaModel::new();
            let dynamic: DynModel = model.clone();
            let def = files
                .def()
                .unwrap()
                .mcp_tools(AGENT_NAME, adam_llm_agent::ToolSet::new());
            let agent = CoderAgent::try_from_def(
                def,
                dynamic,
                "test-model",
                fx.env.clone(),
                coder_tools(&fx.env),
            )
            .unwrap();
            let coder = Coder::new(Arc::new(MemoryStore::new()), agent, &options());
            let run = run_to_a_question(&coder, "hi").await;
            let view = coder.runtime.view(run).await.unwrap().unwrap();
            assert!(view.artifacts.is_empty(), "no tool ran: {view:#?}");
            let answers = model.answers();
            assert_eq!(answers.len(), 1, "one turn, no tool calls: {answers:?}");
            // The greeting is the question the run waits on, and the question says which stream the
            // words were (the model streams its calls), so that a client that read the pieces knows
            // this text for what they were.
            assert!(view.state.to_string().contains(&answers[0]), "{view:#?}");
            assert_eq!(view.state["pending_wait"]["question"], answers[0]);
            let stream = view.state["pending_wait"]["stream"]
                .as_str()
                .expect("the question names the stream of its words");
            assert!(stream.starts_with(&format!("{run}-m0-")), "{stream}");
            answers[0].clone()
        }
    };

    let shipped = greet(AgentFiles::Embedded).await;
    assert!(
        shipped.starts_with("Hi! I'm Coder. I take a repository you name"),
        "{shipped}"
    );
    assert!(shipped.ends_with("Which repository should I work on, and what should I change?"));

    let tmp = folder();
    edit_instructions(&tmp, |text| {
        text.replacen("display_name: Coder", "display_name: Cody", 1)
            .replacen(
                "In one sentence: I take a repository you name, make the change you ask for, run the project's own checks and open a pull request.",
                "In one sentence: I only fix typos.",
                1,
            )
    });
    let edited = greet(files_of(&tmp)).await;
    assert_eq!(
        edited,
        "Hi! I'm Cody. I only fix typos. Which repository should I work on, and what should I change?"
    );
    assert!(
        fx.agent_branches().is_empty(),
        "nothing was prepared or pushed"
    );
}

/// `display_name` is the one var of the persona: a folder that renames it renames the prompt.
#[tokio::test]
async fn the_display_name_var_is_the_name_in_the_prompt() {
    let fx = Fixture::new("hello\n").await;
    let mock = Arc::new(MockModel::new());
    let tmp = folder();
    edit_instructions(&tmp, |text| {
        text.replacen("display_name: Coder", "display_name: Cody", 1)
    });
    let agent = coder_from(&files_of(&tmp), &fx, &mock);
    let prompt = &agent.assembly().info()[0].prompt;
    assert!(
        prompt.starts_with("Your name is Cody.\nIn one sentence: "),
        "{prompt}"
    );
    assert!(prompt.contains("You are Cody."), "{prompt}");
    assert!(!prompt.contains("Coder"), "{prompt}");
}

// ------------------------------------------------------------------ mcp.json tools

/// The token the test MCP server wants, and the variable a folder's `mcp.json` reads it from.
const MCP_TOKEN: &str = "mcp-tok-7d1c4e90-secret";
const MCP_TOKEN_VAR: &str = "TEST_MCP_TOKEN";

/// `agent/mcp.json` of `folder`: one server `test` at `url` that takes the token from
/// `${TEST_MCP_TOKEN}` and offers only `echo`.
fn write_mcp_json(folder: &tempfile::TempDir, url: &str) {
    std::fs::write(
        folder.path().join("agent/mcp.json"),
        format!(
            r#"{{"mcpServers": {{"test": {{"type": "http", "url": "{url}",
                "headers": {{"Authorization": "Bearer ${{{MCP_TOKEN_VAR}}}"}},
                "tools": ["echo"]}}}}}}"#
        ),
    )
    .unwrap();
}

/// The folder's definition with the servers of its `mcp.json` connected, as `serve` does it. The
/// token is given in code (`AgentDef::env`), so no test touches the process environment.
async fn connected_def(
    files: &AgentFiles,
    policy: &McpPolicy,
) -> Result<adam::AgentDef, Box<adam::AssemblyError>> {
    files
        .def()?
        .env(MCP_TOKEN_VAR, MCP_TOKEN)
        .connect_mcp(policy)
        .await
        .map_err(Box::new)
}

/// A folder with an `mcp.json` gives the coder the tools of its servers, named `<server>__<tool>`
/// after its seven, and a call by the model reaches the server and comes back as the tool's
/// result. The token reaches the server from `${TEST_MCP_TOKEN}`.
#[tokio::test]
async fn a_folder_with_an_mcp_json_gives_the_coder_the_tools_of_its_servers() {
    let fx = Fixture::new("hello\n").await;
    let server = TestHttpServer::start(Some(MCP_TOKEN)).await;
    let tmp = folder();
    write_mcp_json(&tmp, &server.url());
    let files = files_of(&tmp);

    let def = connected_def(&files, &McpPolicy::default()).await.unwrap();
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![ToolCall {
        id: "m1".into(),
        name: "test__echo".into(),
        arguments: json!({"text": "ping"}),
    }])
    .push_text("The server said ping.");
    let model: DynModel = mock.clone();
    let agent = CoderAgent::try_from_def(
        def,
        model,
        "test-model",
        fx.env.clone(),
        coder_tools(&fx.env),
    )
    .expect("the folder assembles with its MCP tools");
    let tools = &agent.assembly().info()[0].tools;
    assert_eq!(tools.len(), TOOLS.len() + 1, "{tools:?}");
    assert_eq!(&tools[..TOOLS.len()], TOOLS);
    assert_eq!(tools.last().map(String::as_str), Some("test__echo"));

    let coder = Coder::new(Arc::new(MemoryStore::new()), agent, &options());
    run_to_a_question(&coder, "echo ping").await;

    assert_eq!(server.calls(), 1, "the call reached the server");
    assert!(
        server
            .authorizations()
            .iter()
            .all(|a| a == &format!("Bearer {MCP_TOKEN}")),
        "{:?}",
        server.authorizations()
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].tools.iter().any(|t| t.name == "test__echo"),
        "the model is offered the tool"
    );
    match requests[1].messages.last().unwrap() {
        Message::Tool {
            call_id,
            content,
            is_error,
        } => {
            assert_eq!(call_id, "m1");
            assert_eq!(content, "ping");
            assert!(!is_error);
        }
        other => panic!("{other:?}"),
    }
}

/// The twelve read tools of the shipped `mcp.json`, in its order: what the model may call of the
/// GitHub MCP server, and all of it (`tools:` is an allow-list, and the server is also started
/// with `--read-only`).
const GITHUB_TOOLS: [&str; 12] = [
    "get_me",
    "search_repositories",
    "get_file_contents",
    "list_branches",
    "list_commits",
    "get_commit",
    "search_code",
    "list_issues",
    "issue_read",
    "search_issues",
    "list_pull_requests",
    "pull_request_read",
];

/// The shipped `mcp.json` is the official GitHub MCP server over stdio, read-only, with the four
/// toolsets the coder reads and the twelve tools above and no others, and it hands the child the
/// credentials the coder already has and nothing else: a token or the App's id, installation and key
/// *file* (never the key itself), and the host. The values are `${VAR:-}`, so an unset variable is
/// an empty one, which the server counts as unset (verified against v1.12.2, ADR 0009).
#[test]
fn the_shipped_mcp_json_names_the_github_server_read_only() {
    use adam::agent_fs::McpServer;

    let def = AgentFiles::Embedded.def().unwrap();
    let config = def
        .manifest()
        .mcp
        .as_ref()
        .expect("the shipped agent has an mcp.json");
    assert_eq!(config.servers.keys().collect::<Vec<_>>(), ["github"]);
    let McpServer::Stdio {
        command,
        args,
        env,
        tools,
    } = &config.servers["github"]
    else {
        panic!(
            "the GitHub server is a local process: {:?}",
            config.servers["github"]
        );
    };
    assert_eq!(command, "github-mcp-server");
    assert_eq!(
        args,
        &[
            "stdio",
            "--read-only",
            "--toolsets",
            "context,repos,issues,pull_requests"
        ]
    );
    assert_eq!(tools.as_deref(), Some(&GITHUB_TOOLS.map(String::from)[..]));
    // Nothing that writes: every name is a read, and none of the verbs of the server's write tools.
    for tool in GITHUB_TOOLS {
        for verb in [
            "create_", "update_", "delete_", "push_", "merge_", "add_", "fork_", "request_",
            "assign_", "dismiss_", "enable_", "disable_", "set_", "remove_", "resolve_", "submit_",
        ] {
            assert!(!tool.starts_with(verb), "{tool} writes");
        }
    }
    // The credentials the coder holds, by the names the server reads, and the host.
    let passed: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        passed,
        [
            ("GITHUB_APP_ID", "${GITHUB_APP_ID:-}"),
            (
                "GITHUB_APP_INSTALLATION_ID",
                "${GITHUB_APP_INSTALLATION_ID:-}"
            ),
            (
                "GITHUB_APP_PRIVATE_KEY_PATH",
                "${GITHUB_APP_PRIVATE_KEY_PATH:-}"
            ),
            ("GITHUB_HOST", "${GITHUB_MCP_HOST:-}"),
            ("GITHUB_PERSONAL_ACCESS_TOKEN", "${GITHUB_TOKEN:-}"),
        ],
        "the key itself (GITHUB_APP_PRIVATE_KEY) is not handed to a child"
    );
}

/// A deployment that does not allow local processes does not get the GitHub server, and says so
/// at startup, naming the variable that decides (78); the image allows them (`MCP_ALLOW_STDIO`).
#[tokio::test]
async fn the_shipped_mcp_json_starts_no_local_process_unless_the_deployment_allows_it() {
    let error = AgentFiles::Embedded
        .def()
        .unwrap()
        .connect_mcp(&McpPolicy::default())
        .await
        .expect_err("a local process is not allowed by default");
    assert!(error.to_string().contains("local process"), "{error}");
    assert!(error.to_string().contains("github"), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);
}

/// A folder whose `mcp.json` lists servers that were never connected is refused at assembly, not
/// bound without them (fail closed): `try_from_files` is the sync path and cannot connect.
#[tokio::test]
async fn an_mcp_json_that_was_not_connected_is_refused_at_assembly() {
    let fx = Fixture::new("hello\n").await;
    let tmp = folder();
    write_mcp_json(&tmp, "http://127.0.0.1:9/mcp");
    let mock = Arc::new(MockModel::new());
    let model: DynModel = mock.clone();
    let error = CoderAgent::try_from_files(
        &files_of(&tmp),
        model,
        "test-model",
        fx.env.clone(),
        coder_tools(&fx.env),
    )
    .err()
    .expect("servers that were not connected are refused");
    let text = error.to_string();
    assert!(text.contains("mcp.json"), "{text}");
    assert_eq!(error.class(), ErrorClass::Invalid);
}

/// What the deployment's policy refuses is refused before anything starts, with the class that
/// decides the exit code: a local process, a `${VAR}` in a URL and an unset variable are the
/// deployment's mistakes (78); a server that is down may be up later (69).
#[tokio::test]
async fn the_policy_and_the_servers_decide_what_a_folder_may_connect() {
    let tmp = folder();
    std::fs::write(
        tmp.path().join("agent/mcp.json"),
        r#"{"mcpServers": {"local": {"command": "adam-mcp-test-server"}}}"#,
    )
    .unwrap();
    let files = files_of(&tmp);
    let error = connected_def(&files, &McpPolicy::default())
        .await
        .expect_err("a local process is not allowed by default");
    assert!(error.to_string().contains("local process"), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);

    // A `${VAR}` in a URL: refused unless the deployment opts in (MCP_ALLOW_URL_VARS), and then
    // read from the variables like a header is.
    let server = TestHttpServer::start(Some(MCP_TOKEN)).await;
    let tmp = folder();
    write_mcp_json(&tmp, "${TEST_MCP_URL}");
    let files = files_of(&tmp);
    let with_url = |files: &AgentFiles| {
        files
            .def()
            .unwrap()
            .env(MCP_TOKEN_VAR, MCP_TOKEN)
            .env("TEST_MCP_URL", server.url())
    };
    let error = with_url(&files)
        .connect_mcp(&McpPolicy::default())
        .await
        .expect_err("a variable in a URL is refused by default");
    assert!(error.to_string().contains("TEST_MCP_URL"), "{error}");
    assert!(!error.to_string().contains(&server.url()), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);
    with_url(&files)
        .connect_mcp(&McpPolicy::default().allow_url_secrets(true))
        .await
        .expect("and read from the variables once the deployment opts in");

    // A variable nobody set.
    let error = files
        .def()
        .unwrap()
        .env("TEST_MCP_URL", server.url())
        .connect_mcp(&McpPolicy::default().allow_url_secrets(true))
        .await
        .expect_err("the token variable is unset");
    assert!(error.to_string().contains(MCP_TOKEN_VAR), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);

    // A server that is down: transient, so exit 69 and a supervisor retries.
    let tmp = folder();
    write_mcp_json(&tmp, "http://127.0.0.1:1/mcp");
    let error = connected_def(&files_of(&tmp), &McpPolicy::default())
        .await
        .expect_err("nothing listens on port 1");
    assert_eq!(error.class(), ErrorClass::Transient, "{error}");
}
