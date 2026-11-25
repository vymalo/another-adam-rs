//! The deciding hook `run.ending` (ADR 0050, slice H1c): a hook may keep a run going when it would
//! end with an answer, the model is asked again with the reason, a deployment cap bounds the
//! continuations, and a replay of the step `end:<turn>` asks no hook.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    HookResult, HookSet, HookedEnd, LlmAgent, Plugin, Tool, ToolCtx, ToolError, ToolOutput,
    Verdict, user_message,
};
use adam_model::{MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, RunView, Runtime};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;

struct Harness {
    store: DynStore,
    mock: Arc<MockModel>,
    sink: CollectingSink,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            mock: Arc::new(MockModel::new()),
            sink: CollectingSink::new(),
        }
    }

    fn runtime(&self, agent: &LlmAgent, worker: &str, lease_ms: u64) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .event_sink(self.sink.clone())
            .worker_id(worker)
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_millis(lease_ms))
            .build()
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

impl Worker {
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(10), self.handle)
            .await
            .expect("worker stops in time")
            .expect("worker task")
            .expect("worker result");
    }
}

async fn wait_for(
    rt: &Runtime,
    run: RunId,
    what: &str,
    pred: impl Fn(&RunView) -> bool,
) -> RunView {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if pred(&view) {
            return view;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "done", |v| v.status == RunStatus::Done).await
}

async fn end_records(store: &DynStore, run: RunId) -> Vec<(String, Value)> {
    store
        .journal_list(run)
        .await
        .expect("journal")
        .into_iter()
        .filter(|e| e.name.starts_with("end:"))
        .map(|e| (e.name, e.payload))
        .collect()
}

/// Keeps the run going `times` times with `reason`, then allows the end. `asked` counts calls.
struct Keeps {
    times: usize,
    reason: &'static str,
    asked: AtomicUsize,
}

impl Plugin for Keeps {
    fn name(&self) -> &str {
        "keeps"
    }

    fn run_ending(&self, _end: &HookedEnd<'_>) -> HookResult {
        if self.asked.fetch_add(1, SeqCst) < self.times {
            Ok(Verdict::Deny(self.reason.into()))
        } else {
            Ok(Verdict::Continue)
        }
    }
}

struct Stalls {
    reached: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Stalls {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stalls".into(),
            description: "stalls once".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("stalled out"))
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
        ..Default::default()
    }
}

fn last_user_text(h: &Harness, request: usize) -> String {
    let calls = h.mock.requests();
    format!("{:?}", calls[request].messages.last().unwrap())
}

#[tokio::test]
async fn a_continuation_asks_the_model_again_with_the_reason() {
    let h = Harness::new();
    let plugin = Arc::new(Keeps {
        times: 1,
        reason: "add the tests",
        asked: 0.into(),
    });
    let agent = LlmAgent::builder("llm", h.mock.clone(), "test-model")
        .hooks(HookSet::new().with(plugin.clone()))
        .build();
    h.mock.push_text("first answer").push_text("second answer");
    let rt = h.runtime(&agent, "w", 10_000);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let done = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        done.output.as_ref().unwrap()["text"],
        json!("second answer")
    );
    assert_eq!(h.mock.requests().len(), 2, "the model was asked again");
    assert!(
        last_user_text(&h, 1).contains("add the tests"),
        "{}",
        last_user_text(&h, 1)
    );
    let records = end_records(&h.store, run).await;
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0].1["verdict"], json!("continue"));
    assert_eq!(records[1].1["verdict"], json!("allow"));
}

#[tokio::test]
async fn past_the_cap_the_run_ends_capped() {
    let h = Harness::new();
    let plugin = Arc::new(Keeps {
        times: usize::MAX,
        reason: "again",
        asked: 0.into(),
    });
    let agent = LlmAgent::builder("llm", h.mock.clone(), "test-model")
        .hooks(HookSet::new().with(plugin))
        .max_run_continuations(2)
        .build();
    for i in 0..6 {
        h.mock.push_text(format!("answer {i}"));
    }
    let rt = h.runtime(&agent, "w", 10_000);
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = spawn_worker(&rt);
    let done = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(done.output.as_ref().unwrap()["text"], json!("answer 2"));
    assert_eq!(
        h.mock.requests().len(),
        3,
        "two continuations, then the end"
    );
    let records = end_records(&h.store, run).await;
    let verdicts: Vec<_> = records.iter().map(|r| r.1["verdict"].clone()).collect();
    assert_eq!(
        verdicts,
        vec![json!("continue"), json!("continue"), json!("capped")]
    );
}

#[tokio::test]
async fn a_replay_of_the_end_step_asks_no_hook() {
    let h = Harness::new();
    let plugin = Arc::new(Keeps {
        times: 1,
        reason: "go on",
        asked: 0.into(),
    });
    let reached = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = LlmAgent::builder("llm", h.mock.clone(), "test-model")
        .tool(Stalls {
            reached: reached.clone(),
            calls: calls.clone(),
        })
        .hooks(HookSet::new().with(plugin.clone()))
        .build();
    h.mock
        .push_text("first")
        .push_tool_calls(vec![call("c1", "stalls")])
        .push_text("second");
    let (rt_a, rt_b) = (h.runtime(&agent, "a", 300), h.runtime(&agent, "b", 300));
    let run = rt_a.start("llm", user_message("go"), None).await.unwrap();
    let doomed = spawn_worker(&rt_a);
    tokio::time::timeout(Duration::from_secs(20), reached.notified())
        .await
        .expect("the call starts");
    doomed.handle.abort();
    let _ = doomed.handle.await;
    let survivor = spawn_worker(&rt_b);
    let done = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(done.output.as_ref().unwrap()["text"], json!("second"));
    assert_eq!(
        plugin.asked.load(SeqCst),
        2,
        "one ask per end:<turn> step (the continued end and the final one), none on the replay"
    );
}
