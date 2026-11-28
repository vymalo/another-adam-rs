//! A replay sends no request to the program (ADR 0050, slice P1).
//!
//! The first call's step is journaled, then the worker dies inside the second call's step. The
//! survivor replays the journal: the program is asked about the first call once (its record decides,
//! the adapter is never called again) and about the second call twice (it was never recorded). The
//! program's own log is the evidence: it is outside the process, so no in-process counter can stand
//! in for it.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_hook_exec::{Config, DecidePoint, HookExec, Points};
use adam_llm_agent::{HookSet, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{MockModel, ToolCall, ToolSpec};
use adam_runtime::{RetryPolicy, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object"}),
    }
}

struct Worker {
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
}

fn spawn_worker(rt: &Runtime) -> Worker {
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
    });
    Worker { stop, handle }
}

async fn wait_status(rt: &Runtime, run: RunId, wanted: RunStatus) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if view.status == wanted {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {wanted:?}; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Answers `"<name>-out"` and counts its calls.
struct Echo {
    name: &'static str,
    calls: Arc<AtomicUsize>,
}

impl Echo {
    fn new(name: &'static str) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        spec(self.name)
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        Ok(ToolOutput::text(format!("{}-out", self.name)))
    }
}

/// Blocks until the worker is aborted the first time it is called, and answers at once after that:
/// the worker that dies in it never records its step, the survivor runs it again.
struct Stalls {
    reached: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Stalls {
    fn spec(&self) -> ToolSpec {
        spec("b")
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("b-out"))
    }
}

/// The program the adapter runs: the protocol's test program, logging every request it receives.
fn program(mode: &str, log: &std::path::Path) -> Config {
    Config::new(
        "test",
        PathBuf::from(env!("CARGO_BIN_EXE_adam-hook-exec-test-program")),
    )
    .args([mode])
    .points(Points {
        decide: vec![DecidePoint::ToolBefore],
        observe: Vec::new(),
    })
    .env("ADAM_HOOK_TEST_LOG", log.to_string_lossy())
}

/// How many requests the program logged for `tool`.
fn requests_for(log: &std::path::Path, tool: &str) -> usize {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter(|line| line.contains(&format!("\"tool\":\"{tool}\"")))
        .count()
}

#[tokio::test]
async fn a_replay_sends_no_request_for_the_recorded_step() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let log = dir.path().join("requests.log");
    let exec = HookExec::start(program("continue", &log)).expect("the program starts");

    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    let (a, a_calls) = Echo::new("a");
    let b_calls = Arc::new(AtomicUsize::new(0));
    let reached = Arc::new(Notify::new());
    let agent = LlmAgent::builder("llm", mock.clone(), "test-model")
        .tool(a)
        .tool(Stalls {
            reached: reached.clone(),
            calls: b_calls.clone(),
        })
        .hooks(HookSet::new().with(Arc::new(exec)))
        .build();
    mock.push_tool_calls(vec![call("c1", "a", json!({})), call("c2", "b", json!({}))])
        .push_text("both done");

    let runtime = |worker: &'static str| {
        Runtime::builder(store.clone())
            .agent(agent.clone())
            .worker_id(worker)
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_millis(300))
            .build()
    };
    let (rt_a, rt_b) = (runtime("crash-a"), runtime("crash-b"));
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "the second call starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;
    assert_eq!(a_calls.load(SeqCst), 1, "the first call ran once");

    let survivor = spawn_worker(&rt_b);
    wait_status(&rt_b, run, RunStatus::Done).await;
    let _ = survivor.stop.send(());
    tokio::time::timeout(Duration::from_secs(10), survivor.handle)
        .await
        .expect("the survivor stops")
        .expect("the worker task")
        .expect("the worker result");

    assert_eq!(
        a_calls.load(SeqCst),
        1,
        "the recorded tool did not run twice"
    );
    assert_eq!(b_calls.load(SeqCst), 2, "the in-flight tool ran again");
    assert_eq!(
        requests_for(&log, "a"),
        1,
        "the recorded step was replayed: the program was not asked again"
    );
    assert_eq!(
        requests_for(&log, "b"),
        2,
        "the step that was never recorded was asked again"
    );
}

/// A program that dies on its first event makes the transition transient; after the runtime's
/// tries the run fails, and the tool never ran.
#[tokio::test]
async fn a_run_fails_when_its_program_never_answers() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let log = dir.path().join("requests.log");
    let exec = HookExec::start(program("die-after:0", &log)).expect("the program starts");

    let store: DynStore = Arc::new(MemoryStore::new());
    let mock = Arc::new(MockModel::new());
    let (a, a_calls) = Echo::new("a");
    let agent = LlmAgent::builder("llm", mock.clone(), "test-model")
        .tool(a)
        .hooks(HookSet::new().with(Arc::new(exec)))
        .build();
    // The same call on every try (ANR-1466): a transient hook failure fails the step and the runtime
    // tries the transition again at a **fresh seq**, which asks the model again (ADR 0050:29).
    // Scripting a bare text as the next answer let the model "surrender" on the first retry, so the
    // run ended `Done` and never reached the failure this test is about.
    // `RetryPolicy::new(2)` counts **every** try, the first included (`adam_runtime::retry`), so it is
    // two tries: the first and one retry, and two scripted answers is exactly what it consumes.
    for _ in 0..2 {
        mock.push_tool_calls(vec![call("c1", "a", json!({}))]);
    }

    let rt = Runtime::builder(store.clone())
        .agent(agent.clone())
        .worker_id("dead-program")
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_millis(300))
        .retry(RetryPolicy::new(2).with_backoff(Duration::ZERO, Duration::ZERO))
        .build();
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_status(&rt, run, RunStatus::Failed).await;
    let _ = worker.stop.send(());
    tokio::time::timeout(Duration::from_secs(10), worker.handle)
        .await
        .expect("the worker stops")
        .expect("the worker task")
        .expect("the worker result");

    assert_eq!(
        a_calls.load(SeqCst),
        0,
        "a hook that cannot decide never lets the call through"
    );
    assert!(
        requests_for(&log, "a") >= 1,
        "the program was asked, and never answered"
    );
}
