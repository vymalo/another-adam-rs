//! ADR 0045 slice 7: the workspace a session's rounds read.
//!
//! A round a **person** starts is its own parent (`SessionHeader::root` is the run its model calls
//! name, ADR 0067), so it has no task of its own and the tools cannot find a worktree by the root
//! run. The session's header names the run whose workspace the rounds read instead
//! (`SessionHeader::workspace`): the latest task of the context the person opened the session in,
//! which the A2A front resolves from the store. These tests are the mechanism (`a tool reads the
//! header's workspace, not the root's`), the guarantee it protects (a round cannot write, even when
//! it is handed a write tool) and the whole chain over the A2A front (a person's `explorer` reads a
//! file of the parent's worktree, in round 1 and again in round 2).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use a2a::{Message, Part, Role, TaskState};
use adam_a2a::{CHILD_SESSIONS_EXTENSION, Caller, TaskBackend};
use adam_a2a_runtime::{ChildSessionAgent, ChildSessions, RuntimeTaskBackend};
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::{AGENT_NAME, Coder, CoderAgent, RuntimeOptions, coder_tools};
use adam_core::{DynStore, MemoryStore};
use adam_llm_agent::{Tool as _, ToolCtx};
use adam_model::{DynModel, MockModel};
use adam_runtime::Runtime;
use common::{Fixture, call};
use serde_json::json;

/// The notes of `run` say the fixture's repository may be worked on: what the coder's own message
/// naming it does.
async fn name_the_repository(fx: &Fixture, run: &str) {
    let mut notes = fx.env.notes.load(run).await.unwrap();
    notes.name_repos(named_in(
        &without_untrusted(&fx.remote_url()),
        &fx.env.settings.default_repo_host,
    ));
    fx.env.notes.save(run, &notes).await.unwrap();
}

fn sink() -> adam_runtime::DynEventSink {
    std::sync::Arc::new(adam_runtime::BroadcastSink::default())
}

/// The mechanism, without the front: a tool reads the workspace the run's **session header** names,
/// not the workspace of the root run the context belongs to. A round a person started has a root of
/// its own, so this is the only way it can see its parent's worktree.
#[tokio::test]
async fn a_read_reads_the_workspace_of_the_header_not_the_root_run() {
    let fx = Fixture::new().await;
    // The task: its run prepares the repository.
    let task = ToolCtx::detached("conformance", "call-1", sink()).with_state(fx.env.clone());
    name_the_repository(&fx, &task.root_run_id().to_string()).await;
    let prepared = PrepareWorkspace
        .call(
            &task,
            json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
        )
        .await
        .unwrap();
    assert!(!prepared.is_error, "{prepared:?}");

    // The round: another run, its own root, the header naming the task's workspace.
    let round = ToolCtx::detached("read_file", "call-2", sink()).with_state(fx.env.clone());
    assert_ne!(round.run_id(), task.run_id());
    assert_eq!(
        round.root_run_id(),
        round.run_id(),
        "a round a person started is its own root"
    );
    let read = coder_tools(&fx.env)
        .get("read_file")
        .expect("the coder reads files")
        .clone();

    // Without the header's workspace the round has none: the tools say so.
    let none = read
        .call(&round, json!({"path": "README.md"}))
        .await
        .unwrap();
    assert!(none.is_error, "{none:?}");
    assert!(
        !none.content.contains("widgets"),
        "no worktree to read yet: {}",
        none.content
    );

    // With it, the same call reads the task's worktree.
    let naming = round.with_workspace_run(task.workspace_run_id());
    assert_eq!(naming.workspace_run_id(), task.run_id());
    let found = read
        .call(&naming, json!({"path": "README.md"}))
        .await
        .unwrap();
    assert!(!found.is_error, "{found:?}");
    assert!(found.content.contains("widgets"), "{}", found.content);
    // A reader writes no notes of the task, and none of its own.
    assert!(
        fx.env
            .notes
            .load_existing(&task.root_run_id().to_string())
            .await
            .unwrap()
            .is_some(),
        "the task's notes are its own"
    );
}

/// A round a person starts — `explorer` or `reviewer`, whose folders give them read tools only —
/// cannot write, even when the deployment or a test hands it a write tool: the round works under
/// `plan` (it has no notes of a task, and its message names no tier), and the tier gate refuses the
/// call by name. Seen red: the refusal is what the test asserts, so a gate that stopped firing
/// would fail it, and the round is never offered a write tool by its folder.
#[tokio::test]
async fn a_round_a_person_started_cannot_write_even_with_a_write_tool() {
    let fx = Fixture::new().await;
    let round = ToolCtx::detached("write_file", "call-1", sink()).with_state(fx.env.clone());
    let tools = coder_tools(&fx.env);
    let write = tools.get("write_file").expect("a write tool").clone();

    let refused = write
        .call(&round, json!({"path": "README.md", "content": "no"}))
        .await
        .unwrap();
    assert!(refused.is_error, "{refused:?}");
    assert!(
        refused
            .content
            .contains("`write_file` needs the edits access tier"),
        "{}",
        refused.content
    );
    assert!(
        refused.content.contains("this message is under plan"),
        "{}",
        refused.content
    );

    // A read is not refused by the tier: it fails only because the round has no worktree yet.
    let read = tools.get("read_file").expect("a read tool").clone();
    let seen = read
        .call(&round, json!({"path": "README.md"}))
        .await
        .unwrap();
    assert!(
        !seen.content.contains("access tier"),
        "the tier does not hold a read back: {}",
        seen.content
    );
}

// ------------------------------------------------------------ the whole chain, over the front

fn options() -> RuntimeOptions {
    RuntimeOptions {
        concurrency: 2,
        poll_interval: Duration::from_millis(20),
        ..RuntimeOptions::default()
    }
}

struct Worker {
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

fn spawn_worker(runtime: &Runtime) -> Worker {
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let runtime = runtime.clone();
    let handle = tokio::spawn(async move {
        let _ = runtime
            .run_worker(async {
                let _ = rx.await;
            })
            .await;
    });
    Worker { stop, handle }
}

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(20), self.handle).await;
    }
}

fn said(text: &str, context: Option<&str>) -> Message {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.context_id = context.map(str::to_owned);
    message
}

fn activated(subject: &str) -> Caller {
    let mut caller = Caller::new(subject);
    caller.extensions = vec![CHILD_SESSIONS_EXTENSION.to_owned()];
    caller
}

/// A coder whose front serves `child-sessions/v1` with its shipped `explorer`.
fn coder_with_sessions(fx: &Fixture, mock: &Arc<MockModel>, store: DynStore) -> Coder {
    let model: DynModel = mock.clone();
    let coder = Coder::new(
        store,
        CoderAgent::new(model, "test-model", fx.env.clone()),
        &options(),
    );
    let sessions = ChildSessions::new(
        [ChildSessionAgent::new(
            "explorer",
            "coder/explorer",
            "Where and how questions about a repository.",
        )],
        4,
    )
    .unwrap();
    let backend = RuntimeTaskBackend::new(
        coder.runtime.clone(),
        adam_runtime::BroadcastSink::default(),
        AGENT_NAME,
    )
    .with_poll_interval(Duration::from_millis(20))
    .with_child_sessions(sessions);
    Coder {
        runtime: coder.runtime,
        backend,
    }
}

async fn until(coder: &Coder, caller: &Caller, id: &str, state: TaskState) -> a2a::Task {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let task = coder.backend.get(caller, id).await.unwrap().unwrap();
        if task.status.state == state {
            return task;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {state:?}; last {:?}",
            task.status.state
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn status_text(task: &a2a::Task) -> String {
    task.status
        .message
        .as_ref()
        .and_then(|m| m.parts.first())
        .and_then(Part::as_text)
        .unwrap_or_default()
        .to_owned()
}

/// What the model read as the result of the call `id`, from every request it was sent.
fn tool_results(mock: &MockModel, id: &str) -> Vec<String> {
    let mut found = Vec::new();
    for request in mock.requests() {
        for message in &request.messages {
            if let adam_model::Message::Tool {
                call_id, content, ..
            } = message
                && call_id == id
            {
                found.push(content.clone());
            }
        }
    }
    found
}

/// The card: a person-started `explorer` round reads the repository of the task the session was
/// opened on, and **round 2 reads the same repository**, because the header carries the workspace
/// from one round to the next.
#[tokio::test]
async fn a_persons_explorer_reads_the_parents_repository_in_every_round() {
    let fx = Fixture::new().await;
    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "p1",
        "prepare_workspace",
        json!({"repo_url": fx.remote_url(), "base_branch": "main"}),
    )])
    .push_text("the repository is ready")
    .push_tool_calls(vec![call("r1", "read_file", json!({"path": "README.md"}))])
    .push_text("it says widgets")
    .push_tool_calls(vec![call("r2", "read_file", json!({"path": "README.md"}))])
    .push_text("still widgets");
    let coder = coder_with_sessions(&fx, &mock, store);
    let worker = spawn_worker(&coder.runtime);
    let alice = activated("alice");

    // The task: the coder prepares the repository the person named.
    let asked = format!("work on {} please", fx.remote_url());
    let task = coder
        .backend
        .submit(
            alice.clone(),
            said(&asked, Some("ctx-ws")),
            None,
            Some("ctx-ws".into()),
        )
        .await
        .unwrap();
    until(&coder, &alice, &task.id, TaskState::Completed).await;

    // The person starts an explorer session in that context.
    let mut start = said("where is the greeting?", Some("ctx-ws"));
    start.metadata = Some(HashMap::from([(
        CHILD_SESSIONS_EXTENSION.to_owned(),
        json!({"start": {"agent": "explorer", "description": "look", "keepOpen": true}}),
    )]));
    let round = coder
        .backend
        .submit(alice.clone(), start, None, Some("ctx-ws".into()))
        .await
        .unwrap();
    let done = until(&coder, &alice, &round.id, TaskState::Completed).await;
    assert_eq!(status_text(&done), "it says widgets");
    let read = tool_results(&mock, "r1");
    assert_eq!(read.len(), 1, "the read ran once: {read:?}");
    assert!(
        read[0].contains("widgets"),
        "round 1 read the task's worktree: {}",
        read[0]
    );

    // Round 2 of the same session: a message to the child context, and the same repository again.
    let child = round.context_id.clone();
    let mut more = said("and again?", Some(&child));
    more.message_id = "m-second".into();
    let second = coder
        .backend
        .submit(alice.clone(), more, None, Some(child))
        .await
        .unwrap();
    assert_ne!(second.id, round.id, "a new round");
    let done = until(&coder, &alice, &second.id, TaskState::Completed).await;
    assert_eq!(status_text(&done), "still widgets");
    let round_two = tool_results(&mock, "r2");
    assert_eq!(round_two.len(), 1, "the read ran once: {round_two:?}");
    assert!(
        round_two[0].contains("widgets"),
        "round 2 reads the workspace round 1 read: {}",
        round_two[0]
    );
    worker.stop().await;
}
