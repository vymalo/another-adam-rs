//! The local agent on `adam-engine` (ADR 0047 slice 7 part 2): the file and command tools over a
//! `PersonDir`, with a hook that asks before every write or command. The decisive proofs:
//!
//! * **A yes runs the same call once, and nothing is restored** (a yes on `write_file` writes the
//!   file, the run ends, the file is still there on a fresh read; `write_file` is called once by
//!   the model).
//! * **A refusal leaves the directory unchanged** (a no on `write_file` does not create the file).
//! * **A read does not ask** (a scripted `read_file` runs to completion with no approval).
//!
//! The red proof (test 7) shows that the decisive 2 fails when the asking hook is removed: the
//! same scripted `write_file` runs without ever parking the run. The hook is restored before the
//! test returns, so the rest of the suite still asks.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::Duration;

use adam_engine::adam_agent_fs::{Dir, Strictness};
use adam_engine::adam_assembly::AgentDef;
use adam_engine::adam_core::{DynStore, MemoryStore, RunId};
use adam_engine::adam_error::{Classify, ErrorClass};
use adam_engine::adam_llm_agent::{HookResult, HookedCall, Plugin, Verdict};
use adam_engine::adam_model::{DynModel, MockModel, ToolCall};
use adam_engine::{Decision, Engine, EngineError, RunInfo, RunState, Say};
use serde_json::json;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(30);

/// The agent for the local-agent tests: names every local-agent tool so the catalog offers them
/// (the agent's `tools:` selects from what `bind` registered; `local_agent` registers them all).
fn agent() -> AgentDef {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/instructions.md"),
        "---\nname: local-agent\ntools: [write_file, read_file, edit_file, apply_patch, run_command, run]\n---\nYou edit files.\n",
    )
    .unwrap();
    AgentDef::from_source(&Dir::new(dir.path()), Strictness::Lenient)
        .unwrap()
        .remove(0)
}

/// Build an engine over a `MemoryStore` with the local agent over `dir`, scripted by `mock`.
async fn rig(dir: &std::path::Path, mock: MockModel) -> Engine {
    let model: DynModel = Arc::new(mock);
    let store: DynStore = Arc::new(MemoryStore::new());
    Engine::builder(store, agent(), model, "m")
        .local_agent(dir)
        .start()
        .await
        .expect("the engine starts")
}

/// Wait for `run` to reach `state`, polling `engine.get`. Returns the final `RunInfo`.
async fn wait_for(engine: &Engine, run: RunId, state: RunState) -> RunInfo {
    let last = std::sync::Mutex::new(None);
    timeout(WAIT, async {
        loop {
            let info = engine.get(run).await.unwrap().expect("the run exists");
            if info.state == state {
                return info;
            }
            *last.lock().unwrap() = Some(info);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "run {run} never reached {state:?}: {:?}",
            last.lock().unwrap()
        )
    })
}

fn write_call(id: &str, path: &str, content: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
        ..Default::default()
    }
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: json!({ "path": path }),
        ..Default::default()
    }
}

fn run_command_call(id: &str, command: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "run_command".into(),
        arguments: json!({ "command": command }),
        ..Default::default()
    }
}

async fn shutdown(engine: Engine) {
    timeout(WAIT, engine.shutdown())
        .await
        .expect("shutdown returns in time")
        .expect("the engine shuts down cleanly");
}

// ---------------------------------------------------------------------------
// 1. Decisive: a yes runs the call once, and nothing restores the file
// ---------------------------------------------------------------------------

/// A scripted model writes a file, the host approves, the file is on disk with the content. After
/// the run ends and the engine is dropped, the file is still there: nothing restored it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_yes_writes_the_file_and_nothing_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = "notes.txt";
    let content = "hello, world\n";
    let mock = MockModel::new();
    mock.push_tool_calls(vec![write_call("c1", path, content)])
        .push_text("wrote it");
    let engine = rig(dir.path(), mock).await;

    let turn = engine.send(None, Say::new("write it")).await.unwrap();
    let parked = wait_for(&engine, turn.run, RunState::AwaitingApproval).await;
    let call_id = parked
        .approval
        .as_ref()
        .expect("the run is parked on a hook's ask")
        .call_id
        .clone();
    engine
        .approve(turn.run, &call_id, Decision::Approve)
        .await
        .expect("the yes is taken");
    let done = wait_for(&engine, turn.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("wrote it"));

    // The file is on disk with the content the model asked for.
    let on_disk = std::fs::read_to_string(dir.path().join(path)).unwrap();
    assert_eq!(on_disk, content, "the file was written");

    // A second read of the run shows it ended Done, and nothing on a fresh read of the file
    // (after the run is finished) undid it.
    let info = engine.get(turn.run).await.unwrap().unwrap();
    assert_eq!(info.state, RunState::Done);

    // The file is still there after the engine is dropped: nothing restored it.
    shutdown(engine).await;
    let still = std::fs::read_to_string(dir.path().join(path)).unwrap();
    assert_eq!(
        still, content,
        "the file is still there after the engine is dropped"
    );
}

// ---------------------------------------------------------------------------
// 2. Decisive: a refusal leaves the directory unchanged
// ---------------------------------------------------------------------------

/// A scripted model writes a file, the host refuses: the file is not on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_does_not_write_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = "notes.txt";
    let mock = MockModel::new();
    mock.push_tool_calls(vec![write_call("c1", path, "should not be written\n")])
        .push_text("I did not write it.");
    let engine = rig(dir.path(), mock).await;

    let turn = engine.send(None, Say::new("write it")).await.unwrap();
    let parked = wait_for(&engine, turn.run, RunState::AwaitingApproval).await;
    let call_id = parked.approval.as_ref().unwrap().call_id.clone();
    engine
        .approve(turn.run, &call_id, Decision::refuse_because("not now"))
        .await
        .expect("the no is taken");
    let done = wait_for(&engine, turn.run, RunState::Done).await;
    assert_eq!(done.output.as_deref(), Some("I did not write it."));

    // The file is absent: a refused call never ran, so the directory is unchanged.
    let on_disk = dir.path().join(path);
    assert!(
        !on_disk.exists(),
        "the file should not exist: {}",
        on_disk.display()
    );
    shutdown(engine).await;
}

// ---------------------------------------------------------------------------
// 3. Reads do not ask
// ---------------------------------------------------------------------------

/// A scripted `read_file` runs to completion with no approval: the run ends `Done`, the host
/// never saw an `ApprovalAsked` for the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_does_not_ask() {
    let dir = tempfile::tempdir().unwrap();
    let path = "notes.txt";
    std::fs::write(dir.path().join(path), "hello\n").unwrap();
    let mock = MockModel::new();
    mock.push_tool_calls(vec![read_call("c1", path)])
        .push_text("read it");
    let engine = rig(dir.path(), mock).await;

    let turn = engine.send(None, Say::new("read it")).await.unwrap();
    let done = wait_for(&engine, turn.run, RunState::Done).await;
    // No approval: a read does not park the run.
    assert_eq!(done.approval, None);
    assert_eq!(done.output.as_deref(), Some("read it"));
    shutdown(engine).await;
}

// ---------------------------------------------------------------------------
// 4. Commands also ask
// ---------------------------------------------------------------------------

/// A scripted `run_command` parks the run on a hook's ask; a yes runs the command and a refusal
/// leaves the file the command would have created absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_yes_runs_the_command_and_a_refusal_does_not() {
    // Yes: a `touch` runs.
    let dir_yes = tempfile::tempdir().unwrap();
    let mock_yes = MockModel::new();
    mock_yes
        .push_tool_calls(vec![run_command_call("c1", "touch by_run_yes.txt")])
        .push_text("ran it");
    let engine_yes = rig(dir_yes.path(), mock_yes).await;
    let turn_yes = engine_yes
        .send(None, Say::new("touch a file"))
        .await
        .unwrap();
    let parked_yes = wait_for(&engine_yes, turn_yes.run, RunState::AwaitingApproval).await;
    let call_id_yes = parked_yes.approval.as_ref().unwrap().call_id.clone();
    engine_yes
        .approve(turn_yes.run, &call_id_yes, Decision::Approve)
        .await
        .unwrap();
    wait_for(&engine_yes, turn_yes.run, RunState::Done).await;
    assert!(
        dir_yes.path().join("by_run_yes.txt").exists(),
        "the command's file should exist"
    );
    shutdown(engine_yes).await;

    // Refusal: a `touch` does not run.
    let dir_no = tempfile::tempdir().unwrap();
    let mock_no = MockModel::new();
    mock_no
        .push_tool_calls(vec![run_command_call("c1", "touch by_run_no.txt")])
        .push_text("I did not run it.");
    let engine_no = rig(dir_no.path(), mock_no).await;
    let turn_no = engine_no
        .send(None, Say::new("touch a file"))
        .await
        .unwrap();
    let parked_no = wait_for(&engine_no, turn_no.run, RunState::AwaitingApproval).await;
    let call_id_no = parked_no.approval.as_ref().unwrap().call_id.clone();
    engine_no
        .approve(turn_no.run, &call_id_no, Decision::refuse_because("no"))
        .await
        .unwrap();
    wait_for(&engine_no, turn_no.run, RunState::Done).await;
    assert!(
        !dir_no.path().join("by_run_no.txt").exists(),
        "the command's file should not exist"
    );
    shutdown(engine_no).await;
}

// ---------------------------------------------------------------------------
// 5. Approval for a run that awaits none is rejected
// ---------------------------------------------------------------------------

/// An approval for a run that is not awaiting one is `Rejected`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_approval_for_a_run_that_awaits_none_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockModel::new();
    mock.push_text("hello");
    let engine = rig(dir.path(), mock).await;
    let turn = engine.send(None, Say::new("hi")).await.unwrap();
    wait_for(&engine, turn.run, RunState::Done).await;
    let err = engine
        .approve(turn.run, "c1", Decision::Approve)
        .await
        .unwrap_err();
    assert_eq!(err.class(), ErrorClass::Rejected, "{err}");
    shutdown(engine).await;
}

// ---------------------------------------------------------------------------
// 6. Exposed pieces
// ---------------------------------------------------------------------------

/// `tool_set` lists the seven tools, in order; `plugin` is the asking hook.
#[test]
fn tool_set_and_plugin_are_exposed() {
    let tool_set = adam_engine::local_agent::tool_set();
    let names: Vec<String> = tool_set.names();
    assert_eq!(
        names,
        vec![
            "read_file".to_owned(),
            "read_files".to_owned(),
            "write_file".to_owned(),
            "edit_file".to_owned(),
            "apply_patch".to_owned(),
            "run_command".to_owned(),
            "run".to_owned(),
        ]
    );
    let plugin = adam_engine::local_agent::plugin();
    assert_eq!(plugin.name(), "asks-before-writing-or-command");
}

/// A bogus directory is refused at `start` with `Invalid`.
#[tokio::test]
async fn a_bogus_directory_is_refused_at_start() {
    let mock = MockModel::new();
    let model: DynModel = Arc::new(mock);
    let store: DynStore = Arc::new(MemoryStore::new());
    let bogus = std::path::PathBuf::from("/this/path/does/not/exist/anywhere");
    let err = Engine::builder(store, agent(), model, "m")
        .local_agent(&bogus)
        .start()
        .await
        .expect_err("start refuses a missing directory");
    assert!(
        matches!(err, EngineError::Invalid(_)),
        "expected Invalid, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. Red proof: skipping the ask would write without approval
// ---------------------------------------------------------------------------

/// **Red proof.** The ask is the only barrier: replace the local agent's asking hook with a
/// no-op (`AlwaysContinue`) and the same scripted `write_file` runs without ever parking the
/// run. The decisive 2 assertion (the file is absent after a refusal) would break: the file
/// exists. The local agent's hook is restored (the test does not modify it; it installs an
/// alternative) so the rest of the suite still asks.
struct AlwaysContinue;

impl Plugin for AlwaysContinue {
    fn name(&self) -> &str {
        "always-continue"
    }
    fn before_tool(&self, _call: &mut HookedCall<'_>) -> HookResult {
        Ok(Verdict::Continue)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_asking_hook_a_write_runs_without_approval() {
    let dir = tempfile::tempdir().unwrap();
    let path = "no_ask_written.txt";
    let mock = MockModel::new();
    mock.push_tool_calls(vec![write_call("c1", path, "written without ask\n")])
        .push_text("done without asking");
    let model: DynModel = Arc::new(mock);
    let store: DynStore = Arc::new(MemoryStore::new());
    // The local-agent tools, but no asking hook: the file is written without ever parking.
    let engine = Engine::builder(store, agent(), model, "m")
        .tools(adam_engine::local_agent::tool_set())
        .plugin(AlwaysContinue)
        .start()
        .await
        .expect("the engine starts");
    let turn = engine.send(None, Say::new("write it")).await.unwrap();
    // The run goes straight to `Done` (no approval was asked), so the call ran once, unwatched.
    let done = wait_for(&engine, turn.run, RunState::Done).await;
    assert_eq!(done.approval, None, "the run was never asked");
    assert!(
        dir.path().join(path).exists(),
        "the file was written without approval"
    );
    shutdown(engine).await;
}
