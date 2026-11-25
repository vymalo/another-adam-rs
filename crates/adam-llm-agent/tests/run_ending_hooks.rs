//! The deciding hook `run.ending` (ADR 0050, slice H1c): a hook may keep a run going with a reason,
//! the model is asked again with it, a cap bounds the continuations, and the decision is journaled
//! in the step `end:<turn>`, which exists only when a hook set is installed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    HookResult, HookSet, HookedEnding, LlmAgent, Plugin, Tool, ToolCtx, ToolError, ToolOutput,
    Verdict, user_message,
};
use adam_model::{MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, RunView, Runtime, RuntimeBuilder};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

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

    fn builder(&self) -> adam_llm_agent::LlmAgentBuilder {
        LlmAgent::builder("llm", self.mock.clone(), "test-model")
    }

    fn runtime_with(
        &self,
        agent: &LlmAgent,
        worker: &str,
        f: impl FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    ) -> Runtime {
        f(Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .event_sink(self.sink.clone())
            .worker_id(worker)
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10)))
        .build()
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        self.runtime_with(agent, "w", |b| b)
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

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn short_lease(b: RuntimeBuilder) -> RuntimeBuilder {
    b.lease_ttl(Duration::from_millis(300))
}

/// Blocks until the worker that reached it dies, then answers at once: the replay runs it again.
struct Stalls {
    reached: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Stalls {
    fn spec(&self) -> ToolSpec {
        spec("stalls")
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("stalled out"))
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
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
async fn step_names(store: &DynStore, run: RunId) -> Vec<String> {
    store
        .journal_list(run)
        .await
        .expect("journal")
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

async fn end_record(store: &DynStore, run: RunId, name: &str) -> Option<Value> {
    let journal = store.journal_list(run).await.expect("journal");
    journal
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.payload.clone())
}

/// Keeps the run going with `reason` for as long as it is asked, and counts the asks.
struct KeepsGoing {
    reason: &'static str,
    asked: AtomicUsize,
}

impl Plugin for KeepsGoing {
    fn name(&self) -> &str {
        "keeps-going"
    }

    fn run_ending(&self, _ending: &HookedEnding<'_>) -> HookResult {
        self.asked.fetch_add(1, SeqCst);
        Ok(Verdict::Deny(self.reason.to_owned()))
    }
}

/// Keeps the run going the first time it is asked, then lets it end.
struct KeepsOnce {
    asked: AtomicUsize,
}

impl Plugin for KeepsOnce {
    fn name(&self) -> &str {
        "keeps-once"
    }

    fn run_ending(&self, _ending: &HookedEnding<'_>) -> HookResult {
        if self.asked.fetch_add(1, SeqCst) == 0 {
            Ok(Verdict::Deny("run the tests first".into()))
        } else {
            Ok(Verdict::Continue)
        }
    }
}

#[tokio::test]
async fn a_continuation_asks_the_model_again_with_the_reason() {
    let h = Harness::new();
    let hook = Arc::new(KeepsOnce {
        asked: AtomicUsize::new(0),
    });
    let agent = h.builder().hooks(HookSet::new().with(hook.clone())).build();
    h.mock.push_text("first answer").push_text("second answer");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        view.output.as_ref().unwrap()["text"],
        json!("second answer")
    );
    assert!(view.output.as_ref().unwrap().get("capped").is_none());
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2, "the model was asked twice");
    assert_eq!(
        requests[1].messages.last(),
        Some(&adam_model::Message::user_text("run the tests first")),
        "the second call ends with the hook's reason"
    );
    assert_eq!(hook.asked.load(SeqCst), 2);
    let names = step_names(&h.store, run).await;
    assert!(
        names.contains(&"end:0".to_owned()) && names.contains(&"end:1".to_owned()),
        "{names:?}"
    );
}

#[tokio::test]
async fn past_the_cap_the_run_ends_capped_and_the_step_says_why() {
    let h = Harness::new();
    let hook = Arc::new(KeepsGoing {
        reason: "not yet",
        asked: AtomicUsize::new(0),
    });
    let agent = h
        .builder()
        .end_continuations(2)
        .hooks(HookSet::new().with(hook.clone()))
        .build();
    h.mock
        .push_text("a")
        .push_text("b")
        .push_text("c")
        .push_text("never");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let output = view.output.as_ref().unwrap();
    assert_eq!(output["text"], json!("c"));
    assert_eq!(output["capped"], json!(true));
    assert_eq!(output["capped_reason"], json!("not yet"));
    assert_eq!(
        h.mock.requests().len(),
        3,
        "two continuations, then the end"
    );
    let record = end_record(&h.store, run, "end:2")
        .await
        .expect("the last end step");
    assert_eq!(record["decision"], json!("capped"));
    assert_eq!(record["reason"], json!("not yet"));
}

#[tokio::test]
async fn a_cap_of_zero_lets_no_hook_keep_a_run_going() {
    let h = Harness::new();
    let hook = Arc::new(KeepsGoing {
        reason: "no",
        asked: AtomicUsize::new(0),
    });
    let agent = h
        .builder()
        .end_continuations(0)
        .hooks(HookSet::new().with(hook))
        .build();
    h.mock.push_text("only");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.output.as_ref().unwrap()["capped"], json!(true));
    assert_eq!(h.mock.requests().len(), 1);
}

#[tokio::test]
async fn a_replay_of_an_end_step_asks_no_hook_again() {
    let h = Harness::new();
    let hook = Arc::new(KeepsOnce {
        asked: AtomicUsize::new(0),
    });
    let reached = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .builder()
        .tool(Stalls {
            reached: reached.clone(),
            calls: calls.clone(),
        })
        .hooks(HookSet::new().with(hook.clone()))
        .build();
    h.mock
        .push_text("first answer")
        .push_tool_calls(vec![call("c1", "stalls")])
        .push_text("done");
    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "crash-a", short_lease),
        h.runtime_with(&agent, "crash-b", short_lease),
    );
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "the call starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;

    let survivor = spawn_worker(&rt_b);
    let done = wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(done.output.as_ref().unwrap()["text"], json!("done"));
    assert_eq!(
        hook.asked.load(SeqCst),
        2,
        "end:0 once and end:2 once: the replay of end:0 asks nothing"
    );
}

#[tokio::test]
async fn no_hook_set_takes_no_end_step() {
    let h = Harness::new();
    let agent = h.builder().build();
    h.mock.push_text("plain");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    assert!(
        step_names(&h.store, run)
            .await
            .iter()
            .all(|n| !n.starts_with("end:"))
    );
}

#[test]
fn the_point_is_a_word() {
    assert_eq!(adam_llm_agent::RUN_ENDING, "run.ending");
}
