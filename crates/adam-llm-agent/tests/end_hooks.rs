//! The deciding hook `run.ending` (ADR 0050, slice H1c): a hook may keep a run going with a reason
//! the model is asked again with; a cap bounds the continuations; and the decision is journaled in
//! the step `end:<turn>`, so a replay asks no hook.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{HookResult, HookedEnd, Limits, LlmAgent, Plugin, Verdict, user_message};
use adam_model::MockModel;
use adam_runtime::{RunView, Runtime};

struct Harness {
    store: DynStore,
    mock: Arc<MockModel>,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            mock: Arc::new(MockModel::new()),
        }
    }

    fn builder(&self) -> adam_llm_agent::LlmAgentBuilder {
        LlmAgent::builder("llm", self.mock.clone(), "test-model")
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .worker_id("w")
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .build()
    }

    /// Runs one message to its end and returns the view.
    async fn run(&self, agent: &LlmAgent) -> (RunId, RunView) {
        let rt = self.runtime(agent);
        let run = rt
            .start("llm", user_message("start"), None)
            .await
            .expect("start");
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let worker = rt.clone();
        let handle = tokio::spawn(async move {
            worker
                .run_worker(async {
                    let _ = rx.await;
                })
                .await
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let view = loop {
            let view = rt.view(run).await.expect("view").expect("run exists");
            if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
                break view;
            }
            assert!(std::time::Instant::now() < deadline, "timed out: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = stop.send(());
        handle.await.expect("worker task").expect("worker result");
        (run, view)
    }

    async fn steps(&self, run: RunId) -> Vec<String> {
        self.store
            .journal_list(run)
            .await
            .expect("journal")
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }
}

/// Keeps every run going while `wants` more continuations are asked for.
struct KeepsGoing {
    asked: AtomicUsize,
    reason: &'static str,
}

impl Plugin for KeepsGoing {
    fn name(&self) -> &str {
        "keeps-going"
    }

    fn run_ending(&self, _end: &HookedEnd<'_>) -> HookResult {
        self.asked.fetch_add(1, SeqCst);
        Ok(Verdict::Deny(self.reason.to_owned()))
    }
}

/// A continuation asks the model again with the hook's reason, and the run ends on the answer a
/// hook lets through.
#[tokio::test]
async fn a_continuation_asks_the_model_again_with_the_reason() {
    struct OnceThenEnd(AtomicUsize);
    impl Plugin for OnceThenEnd {
        fn name(&self) -> &str {
            "once"
        }
        fn run_ending(&self, end: &HookedEnd<'_>) -> HookResult {
            if self.0.fetch_add(1, SeqCst) == 0 {
                assert_eq!(end.continuations, 0);
                return Ok(Verdict::Deny("run the tests before you finish".into()));
            }
            Ok(Verdict::Continue)
        }
    }
    let h = Harness::new();
    h.mock.push_text("done, I think");
    h.mock.push_text("tests pass");
    let agent = h.builder().plugin(OnceThenEnd(AtomicUsize::new(0))).build();
    let (run, view) = h.run(&agent).await;
    assert_eq!(view.status, RunStatus::Done);
    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2, "the model is asked a second time");
    let second = format!("{:?}", requests[1].messages);
    assert!(
        second.contains("run the tests before you finish"),
        "{second}"
    );
    let output = view.output.expect("output");
    assert_eq!(output["text"], "tests pass");
    assert!(output.get("capped").is_none());
    let steps = h.steps(run).await;
    assert!(steps.iter().any(|s| s.starts_with("end:")), "{steps:?}");
}

/// Past the cap the run ends `capped` instead of asking again.
#[tokio::test]
async fn past_the_cap_the_run_ends_capped() {
    let h = Harness::new();
    for text in ["one", "two", "three", "four"] {
        h.mock.push_text(text);
    }
    let plugin = Arc::new(KeepsGoing {
        asked: AtomicUsize::new(0),
        reason: "again",
    });
    struct Shared(Arc<KeepsGoing>);
    impl Plugin for Shared {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn run_ending(&self, end: &HookedEnd<'_>) -> HookResult {
            self.0.run_ending(end)
        }
    }
    let agent = h
        .builder()
        .limits(Limits {
            max_end_continuations: 2,
            ..Limits::default()
        })
        .plugin(Shared(plugin.clone()))
        .build();
    let (_run, view) = h.run(&agent).await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(
        h.mock.requests().len(),
        3,
        "two continuations, then the end"
    );
    let output = view.output.expect("output");
    assert_eq!(output["capped"], true);
    assert_eq!(output["text"], "three");
}

/// A run with no hook set has no `end:` step.
#[tokio::test]
async fn without_hooks_there_is_no_end_step() {
    let h = Harness::new();
    h.mock.push_text("hello");
    let agent = h.builder().build();
    let (run, view) = h.run(&agent).await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(!h.steps(run).await.iter().any(|s| s.starts_with("end:")));
}
