//! A killed process resumes its run (ADR 0047 slice 4's proof): the test runs its own binary again
//! as a child, which starts an engine on a durable store and sends a message whose model blocks
//! after the run's first step is committed; the parent kills the child with `SIGKILL`, opens the
//! same store, and its engine finishes the run without running that step again.
//!
//! The store comes from [`durable_store`] alone. Until the SQLite store of ADR 0053 slice 3 lands
//! it panics and the test is ignored; then the helper and the `#[ignore]` are the lines that change.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_engine::adam_agent_fs::{Dir, Strictness};
use adam_engine::adam_assembly::AgentDef;
use adam_engine::adam_core::{DynStore, RunId};
use adam_engine::adam_model::{
    DynModel, Message as ModelMessage, MockModel, ModelClient, ModelDelta, ModelError,
    ModelRequest, ModelResponse, ToolCall,
};
use adam_engine::adam_runtime::{RunEvent, StepState};
use adam_engine::{Engine, EngineEvent, Limits, RunState, Say};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::timeout;

/// Set (to the directory of the store) in the child only: the test is then the child's half.
const CHILD: &str = "ADAM_ENGINE_TEST_RESUME_CHILD";
const TEST: &str = "a_killed_process_resumes_its_run";
const WAIT: Duration = Duration::from_secs(60);
/// The child's lease: what the parent waits at most before it may claim the killed child's run.
const CHILD_LEASE: Duration = Duration::from_secs(2);

/// The store both processes open at `path`: it must outlive the process that wrote it.
fn durable_store(path: &Path) -> DynStore {
    panic!(
        "needs a durable store: adam-store-sqlite (ADR 0053 slice 3, #362); un-ignore in the PR \
         that adds it (it would open {})",
        path.display()
    )
}

#[test]
#[ignore = "needs a durable store: adam-store-sqlite (ADR 0053 slice 3, #362); un-ignore in the PR that adds it"]
fn a_killed_process_resumes_its_run() {
    match std::env::var_os(CHILD) {
        Some(dir) => child(PathBuf::from(dir)),
        None => parent(),
    }
}

fn store_path(dir: &Path) -> PathBuf {
    dir.join("adam.sqlite")
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

fn chat() -> AgentDef {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/instructions.md"),
        "---\nname: chat\ndescription: Talks with the person.\n---\nYou chat.\n",
    )
    .unwrap();
    AgentDef::from_source(&Dir::new(dir.path()), Strictness::Lenient)
        .unwrap()
        .remove(0)
}

fn plan() -> ToolCall {
    ToolCall {
        id: "plan-1".into(),
        name: "plan_work".into(),
        arguments: json!({ "title": "Greet", "items": ["Say hello"] }),
        ..Default::default()
    }
}

/// `name` in `dir`, written whole: a reader sees it complete or not at all.
fn write_file(dir: &Path, name: &str, content: &str) {
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, content).unwrap();
    std::fs::rename(tmp, dir.join(name)).unwrap();
}

/// The child's model: its first call asks for the plan tool, so the run's first step calls it and
/// commits; its second call, which starts only once that step is committed, says so in the file
/// `committed` and never answers.
struct Blocking {
    first: MockModel,
    calls: AtomicUsize,
    dir: PathBuf,
}

#[async_trait]
impl ModelClient for Blocking {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        unreachable!("the agent streams: {req:?}")
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            return self.first.stream(req).await;
        }
        write_file(&self.dir, "committed", "the first step is committed");
        std::future::pending().await
    }
}

/// The child's half: an engine on the store in `dir`, one message, and a wait to be killed.
fn child(dir: PathBuf) {
    runtime().block_on(async {
        let first = MockModel::new();
        first.push_tool_calls(vec![plan()]);
        let model: DynModel = Arc::new(Blocking {
            first,
            calls: AtomicUsize::new(0),
            dir: dir.clone(),
        });
        let mut limits = Limits::default();
        limits.lease_ttl = CHILD_LEASE;
        let engine = Engine::builder(durable_store(&store_path(&dir)), chat(), model, "m")
            .limits(limits)
            .start()
            .await
            .unwrap();
        let turn = engine.send(None, Say::new("plan it")).await.unwrap();
        write_file(&dir, "run", &turn.run.to_string());
        // Killed long before this: it only keeps a lost child from living on.
        tokio::time::sleep(WAIT * 2).await;
        panic!("the child was never killed");
    });
}

/// A point the parent's model stops at: it says it got there, and waits to be let through.
#[derive(Default)]
struct Gate {
    reached: Notify,
    release: Notify,
}

/// The parent's model: a `MockModel` whose first call waits at the gate.
struct Gated {
    inner: Arc<MockModel>,
    gate: Arc<Gate>,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelClient for Gated {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        unreachable!("the agent streams: {req:?}")
    }

    async fn stream(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.gate.reached.notify_one();
            self.gate.release.notified().await;
        }
        self.inner.stream(req).await
    }
}

/// Wait for the file `name` in `dir` while the child lives.
fn wait_for(dir: &Path, name: &str, child: &mut std::process::Child) -> String {
    let started = Instant::now();
    loop {
        if let Ok(content) = std::fs::read_to_string(dir.join(name)) {
            return content;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("the child ended ({status}) before it wrote {name}");
        }
        assert!(started.elapsed() < WAIT, "the child never wrote {name}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The parent's half: start the child, kill it once its step is committed, and finish its run.
fn parent() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--include-ignored", "--nocapture"])
        .env(CHILD, dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(dir.path(), "committed", &mut child);
    let run = RunId(uuid::Uuid::parse_str(&wait_for(dir.path(), "run", &mut child)).unwrap());
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "the child was killed: {status}");

    runtime().block_on(async {
        let mock = Arc::new(MockModel::new());
        mock.push_text("finished after the kill");
        let gate = Arc::new(Gate::default());
        let model: DynModel = Arc::new(Gated {
            inner: mock.clone(),
            gate: gate.clone(),
            calls: AtomicUsize::new(0),
        });
        let engine = Engine::builder(durable_store(&store_path(dir.path())), chat(), model, "m")
            .start()
            .await
            .unwrap();
        // The parent's first model call waits at the gate: its stream starts with all it did since
        // it claimed the run, so a step it ran again would be seen.
        timeout(WAIT, gate.reached.notified())
            .await
            .expect("the parent claims the killed child's run");
        let mut events = engine.events(run).await.unwrap();
        gate.release.notify_one();
        let mut heard = Vec::new();
        while let Some(event) = timeout(WAIT, events.next()).await.expect("an event") {
            heard.push(event.unwrap());
        }
        let reran = heard.iter().any(|e| {
            matches!(e, EngineEvent::Live(RunEvent::Step(step))
                if step.id == "tool:plan-1" && step.state == StepState::Running)
        });
        assert!(!reran, "the committed step ran again: {heard:?}");
        match heard.last() {
            Some(EngineEvent::Ended(info)) => {
                assert_eq!(info.state, RunState::Done);
                assert_eq!(info.output.as_deref(), Some("finished after the kill"));
            }
            other => panic!("the stream ended on {other:?}"),
        }

        let requests = mock.requests();
        assert_eq!(requests.len(), 1, "the parent called its model for the next step only");
        let messages = &requests[0].messages;
        let results = messages
            .iter()
            .filter(|m| matches!(m, ModelMessage::Tool { call_id, .. } if call_id == "plan-1"))
            .count();
        assert_eq!(results, 1, "the tool ran once: {messages:?}");
        assert!(
            matches!(messages.last(), Some(ModelMessage::Tool { call_id, is_error: false, .. }) if call_id == "plan-1"),
            "the run went on from the committed step: {messages:?}"
        );
        timeout(WAIT, engine.shutdown())
            .await
            .expect("the worker stops")
            .unwrap();
    });
}
