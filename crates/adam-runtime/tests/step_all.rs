//! `Ctx::reserve_steps` and `Ctx::step_all` as an agent uses them: steps that run at the same
//! time are recorded at the seqs a sequential run gives them, so a replay with any concurrency
//! finds them. Run against `MemoryStore`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_runtime::{Agent, AgentError, Ctx, Inbound, Runtime, Transition};
use async_trait::async_trait;
use serde_json::{Value, json};

/// How the agent runs its three steps.
#[derive(Clone, Copy)]
enum Mode {
    /// `step_all`, the first step the slowest.
    StepAll,
    /// One `Ctx::step` after the other: what a sequential loop writes.
    Sequential,
}

struct Batch {
    name: String,
    mode: Mode,
    /// Times a step's work really ran, by step.
    ran: Arc<[AtomicUsize; 3]>,
    /// The order the steps finished in.
    finished: Arc<Mutex<Vec<usize>>>,
}

impl Batch {
    fn new(mode: Mode) -> Self {
        Self {
            name: format!("batch-{}", RunId::new()),
            mode,
            ran: Arc::default(),
            finished: Arc::default(),
        }
    }
}

#[async_trait]
impl Agent for Batch {
    type State = Value;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        Ok(input.payload)
    }

    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        let delays = [60u64, 30, 1];
        let work = |i: usize| {
            let ran = self.ran.clone();
            let finished = self.finished.clone();
            let ms = delays[i];
            move || async move {
                ran[i].fetch_add(1, SeqCst);
                tokio::time::sleep(Duration::from_millis(ms)).await;
                finished.lock().unwrap().push(i);
                Ok::<usize, String>(i)
            }
        };
        let out: Vec<Result<usize, String>> = match self.mode {
            Mode::StepAll => {
                let steps = (0..3).map(|i| (format!("tool:{i}"), work(i))).collect();
                ctx.step_all(steps, 10).await?
            }
            Mode::Sequential => {
                let mut out = Vec::new();
                for i in 0..3 {
                    out.push(ctx.step(&format!("tool:{i}"), work(i)).await?);
                }
                out
            }
        };
        let after: Result<u8, String> = ctx.step("after", || async { Ok(7) }).await?;
        Ok(Transition::Done {
            state,
            output: json!({ "out": out, "after": after }),
        })
    }
}

async fn run_to_done(store: &DynStore, agent: Batch) -> (RunId, Value) {
    let rt = Runtime::builder(store.clone())
        .agent(agent.clone_parts())
        .poll_interval(Duration::from_millis(5))
        .build();
    let run = rt
        .start(&agent.name, Inbound::new("start", json!({})), None)
        .await
        .expect("start");
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
    let output = loop {
        let view = rt.view(run).await.expect("view").expect("run");
        if view.status == RunStatus::Done {
            break view.output.expect("output");
        }
        assert_ne!(view.status, RunStatus::Failed, "{view:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let _ = stop.send(());
    worker.await.expect("worker task").expect("worker");
    (run, output)
}

impl Batch {
    /// A second handle on the same agent, with the same counters, to register with a runtime.
    fn clone_parts(&self) -> Batch {
        Batch {
            name: self.name.clone(),
            mode: self.mode,
            ran: self.ran.clone(),
            finished: self.finished.clone(),
        }
    }
}

async fn journal(store: &DynStore, run: RunId) -> Vec<(u64, String)> {
    store
        .journal_list(run)
        .await
        .expect("journal")
        .into_iter()
        .map(|e| (e.seq, e.name))
        .collect()
}

#[tokio::test]
async fn steps_that_finish_in_reverse_are_recorded_at_seqs_in_call_order() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let agent = Batch::new(Mode::StepAll);
    let finished = agent.finished.clone();
    let (run, output) = run_to_done(&store, agent).await;
    assert_eq!(
        *finished.lock().unwrap(),
        vec![2, 1, 0],
        "the work finished in reverse"
    );
    assert_eq!(output["out"], json!([{"Ok": 0}, {"Ok": 1}, {"Ok": 2}]));
    assert_eq!(
        journal(&store, run).await,
        vec![
            (0, "tool:0".to_owned()),
            (1, "tool:1".to_owned()),
            (2, "tool:2".to_owned()),
            (3, "after".to_owned()),
        ],
        "the step after the batch is at k+n"
    );
}

#[tokio::test]
async fn the_journal_of_a_batch_is_the_journal_of_the_sequential_run() {
    let batch: DynStore = Arc::new(MemoryStore::new());
    let sequential: DynStore = Arc::new(MemoryStore::new());
    let (a, _) = run_to_done(&batch, Batch::new(Mode::StepAll)).await;
    let (b, _) = run_to_done(&sequential, Batch::new(Mode::Sequential)).await;
    assert_eq!(journal(&batch, a).await, journal(&sequential, b).await);
}
