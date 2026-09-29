//! Behavioural suite of the runtime, run against `MemoryStore` always,
//! against PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set and against MongoDB
//! when `ADAM_TEST_MONGODB_URI` is set:
//!
//! ```sh
//! ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
//! ADAM_TEST_MONGODB_URI=mongodb://localhost:27017 \
//!     cargo test -p adam-runtime
//! ```
//!
//! Every case takes a fresh agent name from [`uniq`], so cases can share one
//! database and run in parallel without cleanup.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use adam_core::{ClaimScope, DynStore, JournalEntry, MemoryStore, NewRun, RunId, RunStatus};
use adam_runtime::{
    Agent, AgentError, AgentStarter, BroadcastSink, ChildStatus, Classify, Clock, CollectingSink,
    Ctx, Inbound, LocalNotifier, MAX_RETRY_AFTER, ManualClock, RUN_FINISHED_KIND, RetryPolicy,
    RunEvent, RunView, Runtime, RuntimeBuilder, RuntimeError, Transition, child_run_id,
};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn memory_store() -> Option<DynStore> {
    Some(Arc::new(MemoryStore::new()))
}

async fn postgres_store() -> Option<DynStore> {
    let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
    let store = adam_store_postgres::PgStore::connect(&url)
        .await
        .expect("connect to postgres");
    adam_core::Store::migrate(&store).await.expect("migrate");
    Some(Arc::new(store))
}

async fn mongodb_store() -> Option<DynStore> {
    let uri = std::env::var("ADAM_TEST_MONGODB_URI").ok()?;
    let db = std::env::var("ADAM_TEST_MONGODB_DB").unwrap_or_else(|_| "adam_test".into());
    let store = adam_store_mongodb::MongoStore::connect(&uri, &db)
        .await
        .expect("connect to mongodb")
        // Own collections, so migrating here never races the conformance
        // suite's cases that run in another test binary at the same time.
        .with_collection_prefix("adam_rt_")
        .expect("collection prefix");
    adam_core::Store::migrate(&store).await.expect("migrate");
    Some(Arc::new(store))
}

/// `store` behind a [`FaultyStore`]: the handle to script faults and the
/// `DynStore` the runtime is built on.
fn faulty(store: DynStore) -> (Arc<FaultyStore>, DynStore) {
    let faulty = Arc::new(FaultyStore::new(store));
    let dynamic: DynStore = faulty.clone();
    (faulty, dynamic)
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", uuid_like())
}

fn uuid_like() -> String {
    RunId::new().to_string()
}

fn inbound() -> Inbound {
    Inbound::new("start", json!({}))
}

type StepResult = Result<Transition<Value>, AgentError>;
type StepFn = dyn for<'a> Fn(&'a mut Ctx, Value) -> BoxFuture<'a, StepResult> + Send + Sync;

fn step_fn<F>(f: F) -> Arc<StepFn>
where
    F: for<'a> Fn(&'a mut Ctx, Value) -> BoxFuture<'a, StepResult> + Send + Sync + 'static,
{
    Arc::new(f)
}

/// An agent whose state is JSON (initially the start payload) and whose step
/// is a closure, so each case states its behaviour inline.
#[derive(Clone)]
struct FnAgent {
    name: String,
    step: Arc<StepFn>,
}

fn fn_agent(name: &str, step: Arc<StepFn>) -> FnAgent {
    FnAgent {
        name: name.to_owned(),
        step,
    }
}

#[async_trait]
impl Agent for FnAgent {
    type State = Value;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        Ok(input.payload)
    }

    async fn step(&self, ctx: &mut Ctx, state: Value) -> StepResult {
        (self.step)(ctx, state).await
    }
}

/// An agent whose state is a number, to meet a starter whose state is not.
struct CountAgent(String);

#[async_trait]
impl Agent for CountAgent {
    type State = u32;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, _input: Inbound) -> Result<u32, AgentError> {
        Ok(0)
    }

    async fn step(&self, _ctx: &mut Ctx, state: u32) -> Result<Transition<u32>, AgentError> {
        Ok(Transition::Done {
            state,
            output: json!(state),
        })
    }
}

/// A start-only registration whose state is the start payload, like
/// [`FnAgent`].
struct JsonStarter(String);

impl AgentStarter for JsonStarter {
    type State = Value;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        Ok(input.payload)
    }
}

fn builder(store: &DynStore, worker: &str, agent: &FnAgent) -> RuntimeBuilder {
    Runtime::builder(store.clone())
        .agent(agent.clone())
        .worker_id(worker)
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_secs(10))
}

fn runtime(store: &DynStore, agent: &FnAgent) -> Runtime {
    builder(store, &uniq("w"), agent).build()
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), RuntimeError>>,
}

fn spawn_worker(rt: &Runtime) -> Worker {
    let (stop, rx) = oneshot::channel::<()>();
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

/// A clock that stands still until [`advance`](Self::advance)d, for asserting
/// on exact schedules. It starts a minute ahead of the wall clock so that
/// timestamps the store stamps in real time (a new run is due at its
/// `updated_at`) are never in its future.
#[derive(Clone)]
struct FrozenClock(Arc<Mutex<chrono::DateTime<chrono::Utc>>>);

impl FrozenClock {
    fn new() -> Self {
        let start = adam_core::store::now() + chrono::Duration::minutes(1);
        Self(Arc::new(Mutex::new(start)))
    }

    fn advance(&self, by: Duration) {
        let by = chrono::Duration::from_std(by).expect("in range");
        *self.0.lock().expect("lock") += by;
    }
}

impl Clock for FrozenClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        *self.0.lock().expect("lock")
    }
}

async fn wait_for(
    rt: &Runtime,
    run: RunId,
    what: &str,
    pred: impl Fn(&RunView) -> bool,
) -> RunView {
    wait_for_within(rt, run, Duration::from_secs(20), what, pred).await
}

async fn wait_for_within(
    rt: &Runtime,
    run: RunId,
    within: Duration,
    what: &str,
    pred: impl Fn(&RunView) -> bool,
) -> RunView {
    let deadline = Instant::now() + within;
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if pred(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "done", |v| v.status == RunStatus::Done).await
}

async fn wait_failed(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "failed", |v| v.status == RunStatus::Failed).await
}

async fn wait_waiting(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "parked and waiting", |v| v.waiting).await
}

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Polls `cond` until it holds. It fails, naming `what`, after 20 s: a bound
/// for a bug, never a duration the test relies on.
async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// An agent that finishes at once: a probe for [`claim_pass`].
fn probe_agent() -> FnAgent {
    fn_agent(
        &uniq("probe"),
        step_fn(|_ctx, state| {
            async move {
                Ok(Transition::Done {
                    state,
                    output: json!(null),
                })
            }
            .boxed()
        }),
    )
}

/// Returns once a worker of `rt` has claimed a run that was started *after*
/// this call began, that is, after a whole claim pass that saw everything due
/// at the time. It is how a test proves "a worker looked and left the run
/// alone" without sleeping: `rt` must have `probe` registered, and a worker
/// running.
async fn claim_pass(rt: &Runtime, probe: &FnAgent) {
    let run = rt
        .start(&probe.name, inbound(), None)
        .await
        .expect("start the probe");
    wait_done(rt, run).await;
}

fn phase(state: &Value) -> u64 {
    state.get("phase").and_then(Value::as_u64).unwrap_or(0)
}

fn count(c: &AtomicUsize) -> usize {
    c.load(SeqCst)
}

// ---------------------------------------------------------------------------
// Child runs: a parent that starts a child and waits for it
// ---------------------------------------------------------------------------

/// The runtime a parent's step starts its child on. A step is built before the runtime it runs on,
/// so the runtime is put here afterwards.
type RtCell = Arc<OnceLock<Runtime>>;

/// What a test learns about a parent from the outside.
#[derive(Default)]
struct Watch {
    /// Times the child was really started (a replayed step does not count).
    effects: AtomicUsize,
    /// Steps of the parent, all phases.
    steps: AtomicUsize,
    /// Steps after the park: how often the parent resumed.
    resumes: AtomicUsize,
    /// The first step of the parent stops here until released (see [`Hold`]).
    hold: Option<Hold>,
}

/// A gate on the first step of the parent, after it has started its child.
#[derive(Default)]
struct Hold {
    reached: Notify,
    release: Notify,
    first: AtomicBool,
}

impl Hold {
    fn new() -> Self {
        Self {
            first: AtomicBool::new(true),
            ..Self::default()
        }
    }
}

const WAIT: chrono::Duration = chrono::Duration::seconds(60);

/// A parent: in its first step it starts a child of `child_agent` in a journaled step, as a tool
/// does, then parks on a 60 s timer; on the next step it reads the child's finished message, or
/// (woken without one) the child itself, and finishes with `{"via": "notice" | "read", "status"}`.
fn parent_of(name: &str, child_agent: &str, cell: &RtCell, watch: &Arc<Watch>) -> FnAgent {
    let (child_agent, cell, watch) = (child_agent.to_owned(), cell.clone(), watch.clone());
    fn_agent(
        name,
        step_fn(move |ctx, state| {
            let (child_agent, cell, watch) = (child_agent.clone(), cell.clone(), watch.clone());
            async move {
                watch.steps.fetch_add(1, SeqCst);
                let parent = ctx.run_id();
                let child = child_run_id(parent, "call-1");
                if phase(&state) == 0 {
                    let rt = cell.get().expect("the runtime is set").clone();
                    let effects = watch.clone();
                    let started: Result<bool, String> = ctx
                        .step("start-child", move || async move {
                            effects.effects.fetch_add(1, SeqCst);
                            rt.start_child(
                                parent,
                                child,
                                &child_agent,
                                Inbound::new("start", json!({"n": 1})),
                            )
                            .await
                            .map_err(|e| e.to_string())
                        })
                        .await?;
                    started.map_err(AgentError::permanent)?;
                    if let Some(hold) = &watch.hold
                        && hold.first.swap(false, SeqCst)
                    {
                        hold.reached.notify_one();
                        hold.release.notified().await;
                    }
                    return Ok(Transition::Park {
                        state: json!({"phase": 1}),
                        wake_at: Some(ctx.now() + WAIT),
                    });
                }
                watch.resumes.fetch_add(1, SeqCst);
                let notice = ctx
                    .take_inbox()
                    .iter()
                    .filter_map(ChildStatus::from_notice)
                    .find(|(run, _)| *run == child);
                let (via, status) = match notice {
                    Some((_, status)) => ("notice", status),
                    None => match ctx.child_status(child).await? {
                        Some(status) if status.is_finished() => ("read", status),
                        Some(_) => {
                            return Ok(Transition::Park {
                                state,
                                wake_at: Some(ctx.now() + WAIT),
                            });
                        }
                        None => ("gone", ChildStatus::vanished()),
                    },
                };
                Ok(Transition::Done {
                    state,
                    output: json!({"via": via, "status": status}),
                })
            }
            .boxed()
        }),
    )
}

/// A child that finishes at once with `output`.
fn child_done(name: &str, output: Value) -> FnAgent {
    fn_agent(
        name,
        step_fn(move |_ctx, state| {
            let output = output.clone();
            async move { Ok(Transition::Done { state, output }) }.boxed()
        }),
    )
}

/// A child that fails at once with `error`.
fn child_failing(name: &str, error: &str) -> FnAgent {
    let error = error.to_owned();
    fn_agent(
        name,
        step_fn(move |_ctx, state| {
            let error = error.clone();
            async move { Ok(Transition::Fail { state, error }) }.boxed()
        }),
    )
}

/// A child that waits for a message that never comes.
fn child_parked(name: &str) -> FnAgent {
    fn_agent(
        name,
        step_fn(|_ctx, state| {
            async move {
                Ok(Transition::Park {
                    state,
                    wake_at: None,
                })
            }
            .boxed()
        }),
    )
}

/// A child that says it started, waits to be released, then reports whether it was cancelled.
fn child_gated(name: &str, started: &Arc<Notify>, release: &Arc<Notify>) -> FnAgent {
    let (started, release) = (started.clone(), release.clone());
    fn_agent(
        name,
        step_fn(move |ctx, state| {
            let (started, release) = (started.clone(), release.clone());
            async move {
                started.notify_one();
                release.notified().await;
                Ok(Transition::Done {
                    state,
                    output: json!({"saw_cancel": ctx.is_cancelled()}),
                })
            }
            .boxed()
        }),
    )
}

/// One runtime that holds a parent and its child, on a manual clock.
fn family(
    store: &DynStore,
    parent: &FnAgent,
    child: &FnAgent,
    cell: &RtCell,
    clock: &ManualClock,
) -> Runtime {
    let rt = builder(store, &uniq("w"), parent)
        .agent(child.clone())
        .clock(clock.clone())
        .build();
    cell.set(rt.clone()).ok().expect("set once");
    rt
}

/// Wait until the store has injected at least `at_least` faults of `method`.
async fn wait_injected(faulty: &FaultyStore, method: Method, at_least: u64, what: &str) {
    wait_until(what, || faulty.injected(method) >= at_least).await;
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

mod cases {
    use super::*;

    /// A step records a side effect, the worker dies before committing,
    /// another worker takes over: the effect does not run again.
    pub async fn crash_safety(store: DynStore) {
        let name = uniq("crash");
        let effects = Arc::new(AtomicUsize::new(0));
        let first = Arc::new(AtomicBool::new(true));
        let reached = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (effects, first, reached) = (effects.clone(), first.clone(), reached.clone());
                move |ctx, state| {
                    let (effects, first, reached) =
                        (effects.clone(), first.clone(), reached.clone());
                    async move {
                        let charged: Result<u32, String> = ctx
                            .step("charge", || async move {
                                effects.fetch_add(1, SeqCst);
                                Ok(7)
                            })
                            .await?;
                        if first.swap(false, SeqCst) {
                            // Side effect recorded; die before committing.
                            reached.notify_one();
                            std::future::pending::<()>().await;
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!(charged.unwrap_or(0)),
                        })
                    }
                    .boxed()
                }
            }),
        );

        let short = |worker: &str| {
            builder(&store, worker, &agent)
                .lease_ttl(Duration::from_millis(300))
                .build()
        };
        let (a, b) = (short("crash-a"), short("crash-b"));
        let run = a.start(&name, inbound(), None).await.expect("start");

        let doomed = spawn_worker(&a);
        notified(&reached, "the side effect").await;
        doomed.handle.abort(); // the worker vanishes, mid-step, lease held
        assert!(doomed.handle.await.expect_err("aborted").is_cancelled());
        assert_eq!(count(&effects), 1);
        assert_eq!(
            a.view(run).await.expect("view").expect("run").status,
            RunStatus::Runnable
        );

        let survivor = spawn_worker(&b);
        let view = wait_done(&b, run).await;
        survivor.stop().await;

        assert_eq!(view.output, Some(json!(7)));
        assert_eq!(count(&effects), 1, "the recorded effect must not run again");
    }

    /// 8 workers over 50 runs: steps of one run are linear, all end `Done`.
    pub async fn no_double_advance(store: DynStore) {
        const RUNS: usize = 50;
        const STEPS: u64 = 5;
        let name = uniq("linear");
        let active = Arc::new(Mutex::new(HashSet::<RunId>::new()));
        let seen = Arc::new(Mutex::new(HashMap::<RunId, Vec<u64>>::new()));
        let violations = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (active, seen, violations) = (active.clone(), seen.clone(), violations.clone());
                move |ctx, state| {
                    let (active, seen, violations) =
                        (active.clone(), seen.clone(), violations.clone());
                    async move {
                        let run = ctx.run_id();
                        let n = state.get("n").and_then(Value::as_u64).unwrap_or(0);
                        if !active.lock().expect("lock").insert(run) {
                            violations.fetch_add(1, SeqCst);
                        }
                        seen.lock().expect("lock").entry(run).or_default().push(n);
                        tokio::time::sleep(Duration::from_millis(3)).await;
                        active.lock().expect("lock").remove(&run);
                        Ok(if n + 1 >= STEPS {
                            Transition::Done {
                                state,
                                output: json!(n),
                            }
                        } else {
                            Transition::Continue(json!({ "n": n + 1 }))
                        })
                    }
                    .boxed()
                }
            }),
        );

        let runtimes: Vec<Runtime> = (0..8)
            .map(|i| {
                builder(&store, &format!("lin-{i}"), &agent)
                    .concurrency(2)
                    .build()
            })
            .collect();
        let mut runs = Vec::new();
        for _ in 0..RUNS {
            runs.push(
                runtimes[0]
                    .start(&name, Inbound::new("start", json!({"n": 0})), None)
                    .await
                    .expect("start"),
            );
        }
        let workers: Vec<Worker> = runtimes.iter().map(spawn_worker).collect();
        for run in &runs {
            wait_done(&runtimes[0], *run).await;
        }
        for w in workers {
            w.stop().await;
        }

        assert_eq!(count(&violations), 0, "two steps of one run overlapped");
        let seen = seen.lock().expect("lock");
        assert_eq!(seen.len(), RUNS);
        for run in &runs {
            assert_eq!(seen[run], (0..STEPS).collect::<Vec<_>>(), "run {run}");
        }
    }

    /// Parks, `deliver` wakes it, the next step sees the message.
    pub async fn park_and_deliver(store: DynStore) {
        let name = uniq("park");
        let steps = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let steps = steps.clone();
                move |ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        if phase(&state) == 0 {
                            return Ok(Transition::Park {
                                state: json!({"phase": 1, "question": "name?"}),
                                wake_at: None,
                            });
                        }
                        let texts: Vec<Value> = ctx
                            .take_inbox()
                            .into_iter()
                            .map(|m| m.payload["text"].clone())
                            .collect();
                        Ok(Transition::Done {
                            state,
                            output: json!(texts),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        let parked = wait_waiting(&rt, run).await;
        assert_eq!(
            parked.state["question"], "name?",
            "view exposes the parked state"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            count(&steps),
            1,
            "a parked run without timer is never stepped"
        );

        rt.deliver(run, Inbound::new("message", json!({"text": "ada"})))
            .await
            .expect("deliver");
        let done = wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(done.output, Some(json!(["ada"])));
        assert_eq!(done.pending_inbox, 0);
        assert_eq!(count(&steps), 2);
    }

    /// A message delivered while a step is in flight neither is lost nor
    /// discards the step: it is merged into the commit.
    ///
    /// The step is held at a gate until the message is delivered, so it is
    /// always delivered *during* the step (a sleep in the step let a loaded
    /// machine deliver it after the step had committed).
    pub async fn deliver_during_step_is_merged(store: DynStore) {
        let name = uniq("merge");
        let phase0_runs = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (phase0_runs, started, gate) =
                    (phase0_runs.clone(), started.clone(), gate.clone());
                move |ctx, state| {
                    let (phase0_runs, started, gate) =
                        (phase0_runs.clone(), started.clone(), gate.clone());
                    async move {
                        if phase(&state) == 0 {
                            phase0_runs.fetch_add(1, SeqCst);
                            started.notify_one();
                            notified(&gate, "the message to be delivered").await;
                            return Ok(Transition::Continue(json!({"phase": 1})));
                        }
                        let texts: Vec<Value> = ctx
                            .take_inbox()
                            .into_iter()
                            .map(|m| m.payload["text"].clone())
                            .collect();
                        Ok(Transition::Done {
                            state,
                            output: json!(texts),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        notified(&started, "step 0").await;
        rt.deliver(run, Inbound::new("message", json!({"text": "late"})))
            .await
            .expect("deliver");
        gate.notify_one(); // only now may the step return and commit
        let done = wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(done.output, Some(json!(["late"])));
        assert_eq!(
            count(&phase0_runs),
            1,
            "the in-flight step must not be replayed"
        );
    }

    /// A message arriving during the step that parks must not be slept through.
    /// The step is held at a gate until the message is delivered.
    pub async fn message_during_parking_step_wakes_the_run(store: DynStore) {
        let name = uniq("nosleep");
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, gate) = (started.clone(), gate.clone());
                move |ctx, state| {
                    let (started, gate) = (started.clone(), gate.clone());
                    async move {
                        if phase(&state) == 0 {
                            started.notify_one();
                            notified(&gate, "the message to be delivered").await;
                            return Ok(Transition::Park {
                                state: json!({"phase": 1}),
                                wake_at: None,
                            });
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!(ctx.take_inbox().len()),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "step 0").await;
        rt.deliver(run, Inbound::new("message", json!({})))
            .await
            .expect("deliver");
        gate.notify_one(); // only now may the step park
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!(1)));
    }

    /// A parked run with `wake_at` is not stepped before it, and is at it.
    ///
    /// The clock is frozen, so the run stays parked until the test moves time
    /// (sampling a 500 ms window for "parked" missed it on a loaded machine,
    /// and a wall-clock sleep only made "not stepped early" hold by chance).
    /// One millisecond short of `wake_at`, a probe run proves a claim pass
    /// really looked at the parked run and left it alone.
    pub async fn timers(store: DynStore) {
        let name = uniq("timer");
        let steps = Arc::new(AtomicUsize::new(0));
        let stepped_at = Arc::new(Mutex::new(Vec::new()));
        let agent = fn_agent(
            &name,
            step_fn({
                let (steps, stepped_at) = (steps.clone(), stepped_at.clone());
                move |ctx, state| {
                    let (steps, stepped_at) = (steps.clone(), stepped_at.clone());
                    async move {
                        steps.fetch_add(1, SeqCst);
                        stepped_at.lock().expect("lock").push(ctx.now());
                        if phase(&state) == 0 {
                            return Ok(Transition::Park {
                                state: json!({"phase": 1}),
                                wake_at: Some(ctx.now() + chrono::Duration::milliseconds(500)),
                            });
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!("woke"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let probe = probe_agent();
        let clock = FrozenClock::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .agent(probe.clone())
            .clock(clock.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        let parked = wait_for(&rt, run, "parked on a timer", |v| {
            v.status == RunStatus::Parked
        })
        .await;
        let wake_at = parked.wake_at.expect("timer set");
        assert!(!parked.waiting, "a timer is not 'waiting for input'");

        clock.advance(Duration::from_millis(499));
        claim_pass(&rt, &probe).await;
        assert_eq!(count(&steps), 1, "not stepped before wake_at");
        let view = rt.view(run).await.expect("view").expect("run");
        assert_eq!(view.status, RunStatus::Parked);

        clock.advance(Duration::from_millis(1));
        wait_done(&rt, run).await;
        worker.stop().await;
        let stepped_at = stepped_at.lock().expect("lock");
        assert_eq!(stepped_at.len(), 2);
        assert!(
            stepped_at[1] >= wake_at,
            "stepped at {} before {wake_at}",
            stepped_at[1]
        );
    }

    /// Same with a timer of one hour, fired by advancing an injected clock.
    pub async fn timers_with_manual_clock(store: DynStore) {
        let name = uniq("clock");
        let steps = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let steps = steps.clone();
                move |ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        if phase(&state) == 0 {
                            return Ok(Transition::Park {
                                state: json!({"phase": 1}),
                                wake_at: Some(ctx.now() + chrono::Duration::hours(1)),
                            });
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!(null),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .clock(clock.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        wait_for(&rt, run, "parked", |v| v.status == RunStatus::Parked).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(count(&steps), 1);
        clock.advance(Duration::from_secs(2 * 3600));
        wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(count(&steps), 2);
    }

    /// Transient errors retry with growing `wake_at`, then fail.
    ///
    /// The clock is frozen, so a scheduled retry stays scheduled until the
    /// test moves time: each backoff is read from the stored `wake_at` at
    /// leisure and is exact, instead of sampling a window of `initial` ms
    /// (150) that a stalled poll on a loaded machine can miss entirely.
    pub async fn retries_back_off_then_fail(store: DynStore) {
        let name = uniq("retry");
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let calls = calls.clone();
                move |ctx, _state| {
                    let calls = calls.clone();
                    async move {
                        let r: Result<u32, String> = ctx
                            .step("flaky", || async move {
                                calls.fetch_add(1, SeqCst);
                                Err("boom".to_owned())
                            })
                            .await?;
                        match r {
                            Ok(_) => Err(AgentError::permanent("unreachable")),
                            Err(e) => Err(AgentError::transient(e)),
                        }
                    }
                    .boxed()
                }
            }),
        );
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(150),
            max_backoff: Duration::from_secs(5),
            multiplier: 2.0,
        };
        let sink = CollectingSink::new();
        let clock = FrozenClock::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .retry(policy)
            .clock(clock.clone())
            .event_sink(sink.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        // After each failed try the run waits, runnable, for its backoff to
        // pass. Time is frozen, so `wake_at - now` is exactly that backoff,
        // and the run cannot be tried again before the test advances time.
        let mut delays: Vec<i64> = Vec::new();
        for failed_tries in 1..=2u32 {
            let v = wait_for(&rt, run, "a retry is scheduled", |v| {
                v.attempt == failed_tries && v.wake_at.is_some()
            })
            .await;
            assert_eq!(v.status, RunStatus::Runnable);
            let delay = v.wake_at.expect("wake_at") - clock.now();
            assert_eq!(
                count(&calls),
                failed_tries as usize,
                "not tried again before its backoff has passed"
            );
            delays.push(delay.num_milliseconds());
            clock.advance(delay.to_std().expect("a backoff in the future"));
        }
        let last = wait_for(&rt, run, "gave up", |v| v.status == RunStatus::Failed).await;
        worker.stop().await;

        assert_eq!(delays, [150, 300], "two retries with doubling backoff");
        assert_eq!(last.attempt, 3);
        let error = last.error.expect("error");
        assert!(
            error.contains("gave up after 3 attempts") && error.contains("boom"),
            "{error}"
        );
        assert_eq!(count(&calls), 3, "each try re-runs its steps");
        let retry_events = sink
            .events_for(run)
            .into_iter()
            .filter(|e| matches!(e, RunEvent::Status { status: RunStatus::Runnable, detail: Some(d) } if d.contains("retrying")))
            .count();
        assert_eq!(retry_events, 2);
    }

    /// A transient error that clears ends `Done`, with the attempt reset.
    pub async fn retry_recovers_and_resets_attempt(store: DynStore) {
        let name = uniq("recover");
        let attempts_seen = Arc::new(Mutex::new(Vec::new()));
        let agent = fn_agent(
            &name,
            step_fn({
                let attempts_seen = attempts_seen.clone();
                move |ctx, state| {
                    let attempts_seen = attempts_seen.clone();
                    async move {
                        attempts_seen.lock().expect("lock").push(ctx.attempt());
                        if ctx.attempt() < 2 {
                            return Err(AgentError::transient("not yet"));
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!("ok"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .retry(RetryPolicy {
                max_attempts: 5,
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(100),
                multiplier: 2.0,
            })
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            done.attempt, 0,
            "a successful transition resets the attempt"
        );
        assert_eq!(*attempts_seen.lock().expect("lock"), vec![0, 1, 2]);
    }

    /// `Permanent` fails at once, without retrying.
    pub async fn permanent_fails_immediately(store: DynStore) {
        let name = uniq("perm");
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let calls = calls.clone();
                move |_ctx, _state| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, SeqCst);
                        Err(AgentError::permanent("no such customer"))
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let failed = wait_failed(&rt, run).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        worker.stop().await;
        assert_eq!(failed.error.as_deref(), Some("no such customer"));
        assert_eq!(count(&calls), 1);
    }

    /// The agent itself may fail a run.
    pub async fn agent_fail_transition(store: DynStore) {
        let name = uniq("agentfail");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(Transition::Fail {
                        state,
                        error: "gave up".into(),
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let failed = wait_failed(&rt, run).await;
        worker.stop().await;
        assert_eq!(failed.error.as_deref(), Some("gave up"));
        assert_eq!(failed.output, None);
    }

    /// A different step name at a recorded seq fails the run, clearly.
    pub async fn non_determinism_fails_the_run(store: DynStore) {
        let name = uniq("nondet");
        let executed = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let executed = executed.clone();
                move |ctx, state| {
                    let executed = executed.clone();
                    async move {
                        let _: Result<u32, String> = ctx
                            .step("new-name", || async move {
                                executed.fetch_add(1, SeqCst);
                                Ok(1)
                            })
                            .await?;
                        Ok(Transition::Done {
                            state,
                            output: json!(null),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        store
            .journal_put(run, JournalEntry::ok(0, "old-name", json!(1)))
            .await
            .expect("seed journal");
        let worker = spawn_worker(&rt);
        let failed = wait_failed(&rt, run).await;
        worker.stop().await;
        let error = failed.error.expect("error");
        assert!(
            error.contains("non-deterministic")
                && error.contains("old-name")
                && error.contains("new-name"),
            "{error}"
        );
        assert_eq!(count(&executed), 0);
    }

    /// A recorded `Err` is replayed as `Err`, without running the closure.
    pub async fn journal_replays_recorded_err(store: DynStore) {
        let name = uniq("replay-err");
        let executed = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let executed = executed.clone();
                move |ctx, state| {
                    let executed = executed.clone();
                    async move {
                        let r: Result<u32, String> = ctx
                            .step("call", || async move {
                                executed.fetch_add(1, SeqCst);
                                Ok(1)
                            })
                            .await?;
                        Ok(Transition::Done {
                            state,
                            output: json!(r.err()),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        store
            .journal_put(run, JournalEntry::err(0, "call", json!("boom")))
            .await
            .expect("seed journal");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!("boom")));
        assert_eq!(count(&executed), 0);
    }

    /// The journal is one sequence across transitions.
    pub async fn journal_seq_spans_transitions(store: DynStore) {
        let name = uniq("seq");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if phase(&state) == 0 {
                        let _: Result<u32, String> = ctx.step("a", || async { Ok(1) }).await?;
                        return Ok(Transition::Continue(json!({"phase": 1})));
                    }
                    let _: Result<u32, String> = ctx.step("b", || async { Ok(2) }).await?;
                    let _ = ctx.now_journaled().await?;
                    Ok(Transition::Done {
                        state,
                        output: json!(null),
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        wait_done(&rt, run).await;
        worker.stop().await;
        let journal = store.journal_list(run).await.expect("journal");
        let seen: Vec<(u64, &str)> = journal.iter().map(|e| (e.seq, e.name.as_str())).collect();
        assert_eq!(seen, vec![(0, "a"), (1, "b"), (2, "ctx.now")]);
    }

    /// Ctx accessors.
    pub async fn ctx_accessors(store: DynStore) {
        let name = uniq("ctx");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    let out = json!({
                        "run": ctx.run_id().to_string(),
                        "agent": ctx.agent(),
                        "conversation": ctx.conversation_id(),
                        "attempt": ctx.attempt(),
                    });
                    Ok(Transition::Done { state, output: out })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt
            .start(&name, inbound(), Some("conv-x"))
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            done.output,
            Some(
                json!({"run": run.to_string(), "agent": name, "conversation": "conv-x", "attempt": 0})
            )
        );
        assert_eq!(done.conversation_id.as_deref(), Some("conv-x"));
    }

    /// A parked run ends `Failed` with the reason.
    pub async fn cancel_parked(store: DynStore) {
        let name = uniq("cancel-parked");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, _state| {
                async move {
                    Ok(Transition::Park {
                        state: json!({"phase": 1}),
                        wake_at: None,
                    })
                }
                .boxed()
            }),
        );
        let sink = CollectingSink::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .event_sink(sink.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, run).await;
        rt.cancel(run, "user pressed stop").await.expect("cancel");
        worker.stop().await;
        let view = rt.view(run).await.expect("view").expect("run");
        assert_eq!(view.status, RunStatus::Failed);
        assert_eq!(view.error.as_deref(), Some("cancelled: user pressed stop"));
        assert!(sink.events_for(run).iter().any(|e| matches!(
            e,
            RunEvent::Status { status: RunStatus::Failed, detail: Some(d) } if d.contains("user pressed stop")
        )));
        // Cancelling again is a no-op, and a message can no longer be delivered.
        rt.cancel(run, "again").await.expect("cancel twice");
        assert_eq!(
            rt.view(run)
                .await
                .expect("view")
                .expect("run")
                .error
                .as_deref(),
            Some("cancelled: user pressed stop")
        );
        let err = rt.deliver(run, inbound()).await.expect_err("finished");
        assert!(matches!(
            err,
            RuntimeError::Finished {
                status: RunStatus::Failed,
                ..
            }
        ));
    }

    /// A runnable run that no worker has touched yet.
    pub async fn cancel_runnable(store: DynStore) {
        let name = uniq("cancel-runnable");
        let steps = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let steps = steps.clone();
                move |_ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        Ok(Transition::Done {
                            state,
                            output: json!(null),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        rt.cancel(run, "changed my mind").await.expect("cancel");
        let worker = spawn_worker(&rt);
        tokio::time::sleep(Duration::from_millis(150)).await;
        worker.stop().await;
        let view = rt.view(run).await.expect("view").expect("run");
        assert_eq!(view.status, RunStatus::Failed);
        assert_eq!(view.error.as_deref(), Some("cancelled: changed my mind"));
        assert_eq!(count(&steps), 0);
    }

    /// A finished run is unaffected.
    pub async fn cancel_done_is_untouched(store: DynStore) {
        let name = uniq("cancel-done");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(Transition::Done {
                        state,
                        output: json!({"answer": 42}),
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let before = wait_done(&rt, run).await;
        worker.stop().await;
        rt.cancel(run, "too late").await.expect("cancel");
        let after = rt.view(run).await.expect("view").expect("run");
        assert_eq!(after, before);
        assert_eq!(after.output, Some(json!({"answer": 42})));
        assert!(matches!(
            rt.cancel(RunId::new(), "x").await,
            Err(RuntimeError::NotFound(_))
        ));
    }

    /// Cancelling a run under a worker: the worker's commit is rejected and
    /// its result dropped.
    ///
    /// The step is held at a gate until the cancel has returned, so the cancel
    /// always lands before the step can commit. (A fixed sleep in the step made
    /// this a race: a cancel delayed past it, by a loaded database, found the
    /// run already `Done`.)
    pub async fn cancel_running_drops_the_workers_commit(store: DynStore) {
        let name = uniq("cancel-running");
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let finished = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, gate, finished) = (started.clone(), gate.clone(), finished.clone());
                move |_ctx, state| {
                    let (started, gate, finished) =
                        (started.clone(), gate.clone(), finished.clone());
                    async move {
                        started.notify_one();
                        notified(&gate, "the cancel to land").await;
                        finished.fetch_add(1, SeqCst);
                        Ok(Transition::Done {
                            state,
                            output: json!("too late"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let sink = CollectingSink::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .event_sink(sink.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the step").await;
        assert_eq!(count(&finished), 0, "the step is still held at the gate");
        rt.cancel(run, "abort").await.expect("cancel");
        gate.notify_one(); // only now may the step return and the worker commit
        worker.stop().await; // waits for the in-flight step to finish
        assert_eq!(count(&finished), 1, "the step ran to its end");
        let view = rt.view(run).await.expect("view").expect("run");
        assert_eq!(view.status, RunStatus::Failed);
        assert_eq!(view.error.as_deref(), Some("cancelled: abort"));
        assert_eq!(view.output, None);
        assert!(
            !sink.events_for(run).iter().any(|e| matches!(
                e,
                RunEvent::Status {
                    status: RunStatus::Done,
                    ..
                }
            )),
            "a dropped result must not be announced"
        );
    }

    /// A step longer than the lease TTL keeps its lease by renewal.
    ///
    /// Time is a frozen clock that the test moves, so the lease can only lapse
    /// if a renewal really fails to extend it: nothing depends on how quickly a
    /// loaded machine schedules the renewer or answers the store. The clock
    /// moves 60% of a TTL at a time, four times (well past one TTL), and after
    /// each move the test waits for renewals that began after it; without them
    /// the second move would have expired the lease.
    pub async fn lease_renewal_keeps_the_lease(store: DynStore) {
        let (faults, store) = faulty(store);
        let name = uniq("renew");
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (calls, started, gate) = (calls.clone(), started.clone(), gate.clone());
                move |_ctx, state| {
                    let (calls, started, gate) = (calls.clone(), started.clone(), gate.clone());
                    async move {
                        calls.fetch_add(1, SeqCst);
                        started.notify_one();
                        notified(&gate, "the lease to outlive several TTLs").await;
                        Ok(Transition::Done {
                            state,
                            output: json!("slow but ours"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let probe = probe_agent();
        let clock = FrozenClock::new();
        let ttl = Duration::from_millis(90); // renewed every 30 ms of real time
        let short = |w: &str| {
            builder(&store, w, &agent)
                .agent(probe.clone())
                .lease_ttl(ttl)
                .clock(clock.clone())
                .build()
        };
        let (a, b) = (short("renew-a"), short("renew-b"));
        let run = a.start(&name, inbound(), None).await.expect("start");
        let wa = spawn_worker(&a);
        notified(&started, "the step").await;
        let wb = spawn_worker(&b);

        for _ in 0..4 {
            clock.advance(ttl * 6 / 10);
            // Renewal n+1 may have read the clock before the move; n+2 began
            // after n+1 finished, so n+2 read it after, and n+3 starting means
            // n+2 has been applied.
            let seen = faults.calls(Method::RenewLease);
            wait_until("renewals after the clock moved", || {
                faults.calls(Method::RenewLease) >= seen + 3 || count(&calls) > 1
            })
            .await;
            assert_eq!(count(&calls), 1, "nobody took the run over");
        }
        // The other worker has looked, at the final time, and left the run.
        claim_pass(&b, &probe).await;
        assert_eq!(count(&calls), 1, "nobody took the run over");
        gate.notify_one();
        let done = wait_done(&a, run).await;
        wa.stop().await;
        wb.stop().await;
        assert_eq!(done.output, Some(json!("slow but ours")));
        assert_eq!(count(&calls), 1, "nobody took the run over");
    }

    /// Without renewal a second worker takes over, and the stale commit of the
    /// first is rejected by the version CAS.
    ///
    /// The first worker's step is held at a gate until the second has
    /// finished the run, so the stale commit always comes second; the lease
    /// lapses because the frozen clock is moved past it. (A 900 ms sleep in
    /// the step let a stalled second worker arrive after the first had
    /// committed, and then the "stale" commit was the valid one.)
    pub async fn stale_commit_is_rejected_without_renewal(store: DynStore) {
        let name = uniq("stale");
        let invocations = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let slow_finished = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (invocations, started, gate, slow_finished) = (
                    invocations.clone(),
                    started.clone(),
                    gate.clone(),
                    slow_finished.clone(),
                );
                move |_ctx, state| {
                    let (invocations, started, gate, slow_finished) = (
                        invocations.clone(),
                        started.clone(),
                        gate.clone(),
                        slow_finished.clone(),
                    );
                    async move {
                        if invocations.fetch_add(1, SeqCst) == 0 {
                            started.notify_one();
                            notified(&gate, "the takeover to finish").await;
                            slow_finished.fetch_add(1, SeqCst);
                            return Ok(Transition::Done {
                                state,
                                output: json!("slow"),
                            });
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!("fast"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let sink = CollectingSink::new();
        let clock = FrozenClock::new();
        let a = builder(&store, "stale-a", &agent)
            .lease_ttl(Duration::from_millis(200))
            .lease_renewal(false)
            .clock(clock.clone())
            .event_sink(sink.clone())
            .build();
        let b = builder(&store, "stale-b", &agent)
            .clock(clock.clone())
            .event_sink(sink.clone())
            .build();
        let run = a.start(&name, inbound(), None).await.expect("start");
        let wa = spawn_worker(&a);
        notified(&started, "the slow step").await;
        clock.advance(Duration::from_millis(250)); // the first lease lapses
        let wb = spawn_worker(&b);

        let done = wait_done(&b, run).await;
        assert_eq!(done.output, Some(json!("fast")));
        assert_eq!(count(&slow_finished), 0, "the slow step is still held");
        let committed_version = done.version;

        // Only now may the slow step finish and attempt its commit.
        gate.notify_one();
        wait_until("the slow step to finish", || count(&slow_finished) == 1).await;
        wa.stop().await; // waits for its commit attempt
        wb.stop().await;

        let after = b.view(run).await.expect("view").expect("run");
        assert_eq!(
            after.output,
            Some(json!("fast")),
            "stale result must not win"
        );
        assert_eq!(
            after.version, committed_version,
            "stale commit changed nothing"
        );
        assert_eq!(count(&invocations), 2);
        let done_events = sink
            .events_for(run)
            .into_iter()
            .filter(|e| {
                matches!(
                    e,
                    RunEvent::Status {
                        status: RunStatus::Done,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(done_events, 1, "only the winning commit is announced");
    }

    /// In-flight steps finish and leases are released on shutdown.
    pub async fn graceful_shutdown(store: DynStore) {
        let name = uniq("shutdown");
        let started = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let started = started.clone();
                move |_ctx, state| {
                    let started = started.clone();
                    async move {
                        if phase(&state) == 0 {
                            started.notify_one();
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            return Ok(Transition::Continue(json!({"phase": 1})));
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!(null),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = builder(&store, "shutdown-a", &agent).concurrency(1).build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the step").await;
        worker.stop().await; // returns only after the step committed

        let view = rt.view(run).await.expect("view").expect("run");
        assert_eq!(
            view.status,
            RunStatus::Runnable,
            "no new claim after shutdown"
        );
        assert_eq!(phase(&view.state), 1, "the in-flight step was committed");

        // The lease was released: another worker can claim it right away.
        let leases = store
            .claim_due(
                std::slice::from_ref(&name),
                "someone-else",
                adam_core::ClaimScope::Any,
                adam_core::store::now(),
                Duration::from_secs(30),
                10,
            )
            .await
            .expect("claim");
        assert_eq!(leases.len(), 1, "lease must have been released");
        assert_eq!(leases[0].run.id, run);
    }

    /// A conversation has one open run; further starts deliver into it.
    pub async fn conversation_delivers_to_the_open_run(store: DynStore) {
        let name = uniq("conv");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if phase(&state) == 0 {
                        return Ok(Transition::Park {
                            state: json!({"phase": 1}),
                            wake_at: None,
                        });
                    }
                    Ok(Transition::Done {
                        state,
                        output: json!(ctx.take_inbox().len()),
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let first = rt
            .start(&name, inbound(), Some("chat-1"))
            .await
            .expect("start");
        let again = rt
            .start(&name, inbound(), Some("chat-1"))
            .await
            .expect("start again");
        let other = rt
            .start(&name, inbound(), Some("chat-2"))
            .await
            .expect("other chat");
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(
            rt.view(first)
                .await
                .expect("view")
                .expect("run")
                .pending_inbox,
            1
        );

        // Racing starts still create exactly one run.
        let racers = futures::future::join_all((0..8).map(|_| {
            let (rt, name) = (rt.clone(), name.clone());
            tokio::spawn(async move { rt.start(&name, inbound(), Some("chat-3")).await })
        }))
        .await;
        let ids: HashSet<RunId> = racers
            .into_iter()
            .map(|r| r.expect("join").expect("start"))
            .collect();
        assert_eq!(ids.len(), 1, "one run for the conversation: {ids:?}");
        let raced = ids.into_iter().next().expect("one id");
        assert_eq!(
            rt.view(raced)
                .await
                .expect("view")
                .expect("run")
                .pending_inbox,
            7
        );

        // Once it finished, the conversation gets a new run.
        rt.cancel(first, "reset").await.expect("cancel");
        let fresh = rt
            .start(&name, inbound(), Some("chat-1"))
            .await
            .expect("restart");
        assert_ne!(fresh, first);
    }

    /// Caller-chosen ids give fire-once semantics.
    pub async fn start_with_id_is_idempotent(store: DynStore) {
        let name = uniq("once");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(Transition::Park {
                        state,
                        wake_at: None,
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let id = RunId::new();
        assert!(
            rt.start_with_id(id, &name, inbound(), None)
                .await
                .expect("first")
        );
        assert!(
            !rt.start_with_id(id, &name, inbound(), None)
                .await
                .expect("second")
        );
        assert_eq!(rt.view(id).await.expect("view").expect("run").agent, name);

        let conv = RunId::new();
        assert!(
            rt.start_with_id(conv, &name, inbound(), Some("c"))
                .await
                .expect("c")
        );
        let busy = rt
            .start_with_id(RunId::new(), &name, inbound(), Some("c"))
            .await;
        assert!(
            matches!(busy, Err(RuntimeError::ConversationBusy { .. })),
            "{busy:?}"
        );
    }

    /// Error surface of the control API.
    pub async fn api_errors(store: DynStore) {
        let name = uniq("errors");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(Transition::Done {
                        state,
                        output: json!(1),
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        assert!(matches!(
            rt.start("nobody", inbound(), None).await,
            Err(RuntimeError::UnknownAgent(n)) if n == "nobody"
        ));
        let ghost = RunId::new();
        assert!(matches!(
            rt.deliver(ghost, inbound()).await,
            Err(RuntimeError::NotFound(id)) if id == ghost
        ));
        assert!(rt.view(ghost).await.expect("view").is_none());
        assert_eq!(rt.agent_names(), vec![name]);
    }

    /// A start-only runtime (`RuntimeBuilder::starter`) creates runs but never
    /// steps them; a runtime with the full agent over the same store does.
    pub async fn a_starter_only_runtime_starts_and_a_full_runtime_steps(store: DynStore) {
        let name = uniq("starter");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(Transition::Done {
                        state,
                        output: json!("stepped"),
                    })
                }
                .boxed()
            }),
        );
        let front = Runtime::builder(store.clone())
            .starter(JsonStarter(name.clone()))
            .worker_id(uniq("front"))
            .poll_interval(Duration::from_millis(20))
            .build();
        assert_eq!(front.agent_names(), vec![name.clone()]);
        assert!(matches!(
            front.start("nobody", inbound(), None).await,
            Err(RuntimeError::UnknownAgent(n)) if n == "nobody"
        ));

        // The front starts a run and keeps it Runnable however long its worker
        // loop polls: it has nothing to step it with.
        let run = front
            .start(&name, Inbound::new("start", json!({"k": 1})), None)
            .await
            .unwrap();
        let idle = spawn_worker(&front);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let view = front.view(run).await.unwrap().unwrap();
        idle.stop().await;
        assert_eq!(view.status, RunStatus::Runnable);
        assert_eq!(view.agent, name);
        assert_eq!(view.state, json!({"k": 1}));
        let version = view.version;

        // A conversation start and an idempotent start work the same way.
        let by_id = RunId::new();
        assert!(
            front
                .start_with_id(by_id, &name, inbound(), Some(&uniq("conv")))
                .await
                .unwrap()
        );

        // A second runtime with the full agent, over the same store, steps it.
        let worker = runtime(&store, &agent);
        let stepping = spawn_worker(&worker);
        let done = wait_done(&worker, run).await;
        wait_done(&worker, by_id).await;
        stepping.stop().await;
        assert_eq!(done.output, Some(json!("stepped")));
        assert!(done.version > version);
        assert_eq!(done.state, json!({"k": 1}));

        // The last registration of a name wins, whichever kind it is.
        let agent_last = Runtime::builder(store.clone())
            .starter(JsonStarter(name.clone()))
            .agent(agent.clone())
            .worker_id(uniq("agent-last"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let run = agent_last.start(&name, inbound(), None).await.unwrap();
        let w = spawn_worker(&agent_last);
        wait_done(&agent_last, run).await;
        w.stop().await;

        let starter_last = Runtime::builder(store.clone())
            .agent(agent)
            .starter(JsonStarter(name.clone()))
            .worker_id(uniq("starter-last"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let run = starter_last.start(&name, inbound(), None).await.unwrap();
        let w = spawn_worker(&starter_last);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let view = starter_last.view(run).await.unwrap().unwrap();
        w.stop().await;
        assert_eq!(view.status, RunStatus::Runnable);

        // A starter whose State is not the agent's: the start succeeds, and the
        // worker's first step fails the run, naming the agent and the cause.
        let typed = uniq("starter-typed");
        let front = Runtime::builder(store.clone())
            .starter(JsonStarter(typed.clone()))
            .worker_id(uniq("typed-front"))
            .build();
        let run = front
            .start(&typed, Inbound::new("start", json!({"k": 1})), None)
            .await
            .unwrap();
        let worker = Runtime::builder(store.clone())
            .agent(CountAgent(typed.clone()))
            .worker_id(uniq("typed-worker"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let w = spawn_worker(&worker);
        let failed = wait_failed(&worker, run).await;
        w.stop().await;
        let error = failed.error.unwrap_or_default();
        assert!(error.contains(&format!("{typed:?}")), "{error}");
        assert!(error.contains("AgentStarter"), "{error}");
    }

    /// Events, durable artifacts, and reconstruction after a restart.
    pub async fn events_and_durable_artifacts(store: DynStore) {
        let name = uniq("events");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if phase(&state) == 0 {
                        ctx.emit(RunEvent::Progress {
                            message: "working".into(),
                        })
                        .await;
                        ctx.emit(RunEvent::Artifact {
                            name: "report".into(),
                            mime_type: Some("text/markdown".into()),
                            data: json!("# hi"),
                        })
                        .await;
                        return Ok(Transition::Continue(json!({"phase": 1})));
                    }
                    ctx.emit(RunEvent::Custom {
                        kind: "k".into(),
                        payload: json!(1),
                    })
                    .await;
                    Ok(Transition::Done {
                        state,
                        output: json!("finished"),
                    })
                }
                .boxed()
            }),
        );
        let sink = CollectingSink::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .event_sink(sink.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        wait_done(&rt, run).await;
        worker.stop().await;

        let events = sink.events_for(run);
        assert_eq!(
            events,
            vec![
                RunEvent::Status {
                    status: RunStatus::Runnable,
                    detail: Some("started".into())
                },
                RunEvent::Progress {
                    message: "working".into()
                },
                RunEvent::Artifact {
                    name: "report".into(),
                    mime_type: Some("text/markdown".into()),
                    data: json!("# hi"),
                },
                RunEvent::Custom {
                    kind: "k".into(),
                    payload: json!(1)
                },
                RunEvent::Status {
                    status: RunStatus::Done,
                    detail: None
                },
            ]
        );
        assert!(sink.events().iter().all(|e| e.agent == name));

        // "Restart": a brand-new runtime over the same store sees it all.
        let fresh = runtime(&store, &agent);
        let view = fresh.view(run).await.expect("view").expect("run");
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!("finished")));
        assert_eq!(view.artifacts.len(), 1);
        assert_eq!(view.artifacts[0].name, "report");
        assert_eq!(view.artifacts[0].data, json!("# hi"));
    }

    /// Live fan-out through `BroadcastSink`.
    pub async fn broadcast_sink_streams_a_run(store: DynStore) {
        let name = uniq("broadcast");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    ctx.emit(RunEvent::Progress {
                        message: "p".into(),
                    })
                    .await;
                    Ok(Transition::Done {
                        state,
                        output: json!(null),
                    })
                }
                .boxed()
            }),
        );
        let sink = BroadcastSink::new(64);
        let rt = builder(&store, &uniq("w"), &agent)
            .event_sink(sink.clone())
            .build();
        let mut all = sink.subscribe();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let mut mine = sink.subscribe_run(run);
        let worker = spawn_worker(&rt);
        let mut seen = Vec::new();
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(10), mine.recv()).await
        {
            let done = matches!(
                event,
                RunEvent::Status {
                    status: RunStatus::Done,
                    ..
                }
            );
            seen.push(event);
            if done {
                break;
            }
        }
        worker.stop().await;
        assert!(
            seen.contains(&RunEvent::Progress {
                message: "p".into()
            }),
            "{seen:?}"
        );
        assert!(matches!(
            seen.last(),
            Some(RunEvent::Status {
                status: RunStatus::Done,
                ..
            })
        ));
        let first = all.recv().await.expect("global subscriber sees events too");
        assert_eq!(first.run, run);
    }

    /// A panicking agent is treated as a transient failure.
    pub async fn panic_is_a_transient_failure(store: DynStore) {
        let name = uniq("panic");
        let first = Arc::new(AtomicBool::new(true));
        let agent = fn_agent(
            &name,
            step_fn({
                let first = first.clone();
                move |_ctx, state| {
                    let first = first.clone();
                    async move {
                        if first.swap(false, SeqCst) {
                            panic!("agent bug");
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!("recovered"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(20),
                max_backoff: Duration::from_millis(50),
                multiplier: 2.0,
            })
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!("recovered")));
    }

    /// `deliver` on the same runtime wakes its workers without waiting for a poll.
    pub async fn deliver_wakes_local_workers_immediately(store: DynStore) {
        let name = uniq("wake");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if phase(&state) == 0 {
                        return Ok(Transition::Park {
                            state: json!({"phase": 1}),
                            wake_at: None,
                        });
                    }
                    Ok(Transition::Done {
                        state,
                        output: json!(ctx.take_inbox().len()),
                    })
                }
                .boxed()
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .poll_interval(Duration::from_secs(30))
            .build();
        let worker = spawn_worker(&rt);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        wait_for_within(
            &rt,
            run,
            Duration::from_secs(5),
            "parked despite a 30s poll",
            |v| v.waiting,
        )
        .await;
        rt.deliver(run, inbound()).await.expect("deliver");
        let done = wait_for_within(
            &rt,
            run,
            Duration::from_secs(5),
            "done despite a 30s poll",
            |v| v.status == RunStatus::Done,
        )
        .await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!(1)));
    }

    /// A state of an unknown envelope version fails the run instead of being
    /// misread.
    pub async fn unknown_envelope_version_is_rejected(store: DynStore) {
        let name = uniq("corrupt");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(Transition::Done {
                        state,
                        output: json!(1),
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let run = RunId::new();
        store
            .create_run(NewRun::new(&name, json!({"v": 99, "agent": null})).with_id(run))
            .await
            .expect("create");
        assert!(matches!(
            rt.view(run).await,
            Err(RuntimeError::Corrupt { .. })
        ));
        assert!(matches!(
            rt.deliver(run, inbound()).await,
            Err(RuntimeError::Corrupt { .. })
        ));
        let worker = spawn_worker(&rt);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rec = store.load_run(run).await.expect("load").expect("run");
            if rec.status == RunStatus::Failed {
                break;
            }
            assert!(Instant::now() < deadline, "run was not failed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        worker.stop().await;
    }

    // -----------------------------------------------------------------------
    // Retry hints (`AgentError::with_retry_after`)
    // -----------------------------------------------------------------------

    /// A retry hint (a rate limit's `Retry-After`) delays the retry at least
    /// that long: not stepped 5 s before the hint is up, stepped after it.
    pub async fn transient_with_minimum_delay_waits_at_least_that_long(store: DynStore) {
        let name = uniq("hint");
        let steps = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let steps = steps.clone();
                move |ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        if ctx.attempt() == 0 {
                            return Err(AgentError::transient_after(
                                "slow down",
                                Duration::from_secs(30),
                            ));
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!("through"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let clock = ManualClock::new();
        let t0 = clock.now();
        let sink = CollectingSink::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .clock(clock.clone())
            .event_sink(sink.clone())
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(50),
                multiplier: 2.0,
            })
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        let scheduled = wait_for(&rt, run, "the retry to be scheduled", |v| {
            v.attempt == 1 && v.status == RunStatus::Runnable
        })
        .await;
        let wake_at = scheduled.wake_at.expect("a retry carries a timer");
        let thirty = chrono::Duration::seconds(30);
        assert!(wake_at >= t0 + thirty, "hint ignored: wake_at {wake_at}");
        assert!(
            wake_at <= clock.now() + thirty,
            "the hint is a minimum, not a bonus on top of the backoff: {wake_at}"
        );

        // 5 s before the hint is up: several polls later, still not stepped.
        let gap = (wake_at - clock.now() - chrono::Duration::seconds(5))
            .to_std()
            .expect("the timer is in the future");
        clock.advance(gap);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(count(&steps), 1, "stepped before the hint elapsed");
        assert_eq!(
            rt.view(run).await.expect("view").expect("run").status,
            RunStatus::Runnable
        );

        clock.advance(Duration::from_secs(10));
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!("through")));
        assert_eq!(count(&steps), 2);
        let retry_events: Vec<String> = sink
            .events_for(run)
            .into_iter()
            .filter_map(|e| match e {
                RunEvent::Status {
                    status: RunStatus::Runnable,
                    detail: Some(d),
                } if d.contains("retrying") => Some(d),
                _ => None,
            })
            .collect();
        assert_eq!(retry_events.len(), 1);
        assert!(
            retry_events[0].contains("retrying in 30s") && retry_events[0].contains("slow down"),
            "{retry_events:?}"
        );
    }

    /// The hint can lengthen the wait, never shorten the policy's backoff, and
    /// it is a floor (`max`), not an addition.
    pub async fn retry_hint_never_shortens_the_backoff(store: DynStore) {
        let name = uniq("hint-short");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if ctx.attempt() == 0 {
                        return Err(AgentError::transient_after(
                            "tiny hint",
                            Duration::from_secs(1),
                        ));
                    }
                    Ok(Transition::Done {
                        state,
                        output: json!(null),
                    })
                }
                .boxed()
            }),
        );
        let clock = ManualClock::new();
        let t0 = clock.now();
        let rt = builder(&store, &uniq("w"), &agent)
            .clock(clock.clone())
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_secs(20),
                max_backoff: Duration::from_secs(60),
                multiplier: 2.0,
            })
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let scheduled = wait_for(&rt, run, "the retry to be scheduled", |v| v.attempt == 1).await;
        let wake_at = scheduled.wake_at.expect("timer");
        let twenty = chrono::Duration::seconds(20);
        assert!(wake_at >= t0 + twenty, "backoff was shortened: {wake_at}");
        assert!(
            wake_at <= clock.now() + twenty,
            "hint and backoff must not add up: {wake_at}"
        );
        clock.advance(Duration::from_secs(25));
        wait_done(&rt, run).await;
        worker.stop().await;
    }

    /// A hostile or broken hint (`Duration::MAX`) neither overflows the
    /// timestamp arithmetic nor parks the run forever: it is capped.
    pub async fn huge_retry_hint_is_capped_and_does_not_panic(store: DynStore) {
        let name = uniq("hint-huge");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if ctx.attempt() == 0 {
                        return Err(AgentError::transient_after("forever", Duration::MAX));
                    }
                    Ok(Transition::Done {
                        state,
                        output: json!(null),
                    })
                }
                .boxed()
            }),
        );
        let clock = ManualClock::new();
        let t0 = clock.now();
        let rt = builder(&store, &uniq("w"), &agent)
            .clock(clock.clone())
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(50),
                multiplier: 2.0,
            })
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let scheduled = wait_for(&rt, run, "the retry to be scheduled", |v| v.attempt == 1).await;
        let cap = chrono::Duration::from_std(MAX_RETRY_AFTER).expect("cap fits");
        let wake_at = scheduled.wake_at.expect("timer");
        assert!(wake_at >= t0 + cap, "capped too low: {wake_at}");
        assert!(wake_at <= clock.now() + cap, "not capped: {wake_at}");
        clock.advance(MAX_RETRY_AFTER + Duration::from_secs(60));
        wait_done(&rt, run).await;
        worker.stop().await;
    }

    /// A hinted retry still counts as an attempt: the budget bounds retries.
    ///
    /// The test moves the clock once per scheduled retry, after seeing it
    /// scheduled, and the lease outlives all of it. (It used to step the clock
    /// by 6 s every 30 ms against a 10 s lease: on a loaded machine the lease
    /// lapsed before the first step even began, the claim loop picked the run
    /// up again from a record read before that step committed, and the step
    /// ran a second time on the same attempt. The stale run was discarded by
    /// the version check, but it made the call count 4.)
    pub async fn hinted_retries_still_exhaust_the_attempt_budget(store: DynStore) {
        let name = uniq("hint-budget");
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let calls = calls.clone();
                move |_ctx, _state| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, SeqCst);
                        Err(AgentError::transient_after(
                            "still limited",
                            Duration::from_secs(5),
                        ))
                    }
                    .boxed()
                }
            }),
        );
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .clock(clock.clone())
            .lease_ttl(Duration::from_secs(24 * 3600))
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(50),
                multiplier: 2.0,
            })
            .build();
        let t0 = clock.now();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        // Each failure schedules a retry at least 5 s away (the hint, not the
        // 10-50 ms policy). Nothing steps before the test moves the clock.
        let hint = chrono::Duration::seconds(5);
        for attempt in 1..3 {
            let scheduled = wait_for(&rt, run, "the retry to be scheduled", |v| {
                v.attempt == attempt
            })
            .await;
            let wake_at = scheduled.wake_at.expect("timer");
            assert!(wake_at >= t0 + hint, "the hint was not honoured: {wake_at}");
            assert_eq!(count(&calls), attempt as usize, "stepped before its time");
            clock.advance(Duration::from_secs(6));
        }
        let failed = wait_failed(&rt, run).await;
        worker.stop().await;
        assert_eq!(count(&calls), 3);
        let error = failed.error.expect("error");
        assert!(
            error.contains("gave up after 3 attempts") && error.contains("still limited"),
            "{error}"
        );
    }

    // -----------------------------------------------------------------------
    // Store outages (`FaultyStore`)
    // -----------------------------------------------------------------------

    /// `claim_due` failing five times in a row does not stop the worker; the
    /// run ends `Done` once the store answers again, and it ran once.
    pub async fn store_outage_during_claim_is_survived(store: DynStore) {
        let (faults, store) = faulty(store);
        let name = uniq("claim-outage");
        let effects = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let effects = effects.clone();
                move |ctx, state| {
                    let effects = effects.clone();
                    async move {
                        let _: Result<u8, String> = ctx
                            .step("effect", || async move {
                                effects.fetch_add(1, SeqCst);
                                Ok(1)
                            })
                            .await?;
                        Ok(Transition::Done {
                            state,
                            output: json!("finished"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        faults.fail(Method::ClaimDue, 5);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        assert!(
            !worker.handle.is_finished(),
            "the worker must outlive the outage"
        );
        worker.stop().await;
        assert_eq!(done.output, Some(json!("finished")));
        assert_eq!(faults.injected(Method::ClaimDue), 5);
        assert_eq!(count(&effects), 1);
    }

    /// A commit that fails leaves the lease to expire; the next claim replays
    /// the journal, so the effect still ran once, and the run finishes.
    ///
    /// The clock is frozen, so the lease lapses only when the test says so.
    /// Before that, a probe run proves a claim pass looked at the run and left
    /// it (an elapsed-time bound could not tell "not yet due" from "not yet
    /// polled").
    pub async fn commit_failure_leaves_run_to_lease_expiry(store: DynStore) {
        let (faults, store) = faulty(store);
        let name = uniq("commit-fail");
        let effects = Arc::new(AtomicUsize::new(0));
        let invocations = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (effects, invocations) = (effects.clone(), invocations.clone());
                move |ctx, state| {
                    let (effects, invocations) = (effects.clone(), invocations.clone());
                    async move {
                        invocations.fetch_add(1, SeqCst);
                        let _: Result<u8, String> = ctx
                            .step("effect", || async move {
                                effects.fetch_add(1, SeqCst);
                                Ok(1)
                            })
                            .await?;
                        Ok(Transition::Done {
                            state,
                            output: json!("finished"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let probe = probe_agent();
        let clock = FrozenClock::new();
        let ttl = Duration::from_millis(300);
        let rt = builder(&store, &uniq("w"), &agent)
            .agent(probe.clone())
            .lease_ttl(ttl)
            .clock(clock.clone())
            .build();
        faults.fail(Method::CommitRun, 1);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        wait_until("the commit to fail", || {
            faults.injected(Method::CommitRun) == 1
        })
        .await;

        // The lease is still ours: a claim pass leaves the run alone.
        claim_pass(&rt, &probe).await;
        assert_eq!(
            count(&invocations),
            1,
            "the run was retried before its lease expired"
        );
        let waiting = rt.view(run).await.expect("view").expect("run");
        assert_eq!(waiting.status, RunStatus::Runnable);

        clock.advance(ttl);
        let done = wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(done.output, Some(json!("finished")));
        assert_eq!(faults.injected(Method::CommitRun), 1);
        assert_eq!(count(&invocations), 2, "stepped again after the failure");
        assert_eq!(
            count(&effects),
            1,
            "the journal kept the effect from re-running"
        );
    }

    /// The commit *was* applied but its acknowledgement was lost: the run
    /// carries on from the committed state, nothing before it re-runs.
    pub async fn commit_ack_lost_is_survived(store: DynStore) {
        let (faults, store) = faulty(store);
        let name = uniq("ack-lost");
        let effects = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let agent = fn_agent(
            &name,
            step_fn({
                let effects = effects.clone();
                move |ctx, _state| {
                    let effects = effects.clone();
                    async move {
                        let (index, phase) = (phase(&_state) as usize, phase(&_state));
                        let _: Result<u8, String> = ctx
                            .step(&format!("effect-{phase}"), || async move {
                                effects[index].fetch_add(1, SeqCst);
                                Ok(1)
                            })
                            .await?;
                        Ok(if phase == 0 {
                            Transition::Continue(json!({"phase": 1}))
                        } else {
                            Transition::Done {
                                state: json!({"phase": 1}),
                                output: json!("finished"),
                            }
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .lease_ttl(Duration::from_millis(300))
            .build();
        faults.fail_after_apply(Method::CommitRun, 1);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(done.output, Some(json!("finished")));
        assert_eq!(faults.injected(Method::CommitRun), 1);
        assert_eq!(count(&effects[0]), 1, "phase 0 was committed, never redone");
        assert_eq!(count(&effects[1]), 1);
        let journal = store.journal_list(run).await.expect("journal");
        let names: Vec<_> = journal.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["effect-0", "effect-1"]);
    }

    /// A journal write that fails after the effect ran is the one place an
    /// effect can run twice (the crash window between doing and recording):
    /// the run still completes, with exactly one more execution.
    pub async fn journal_write_failure_reruns_the_step_after_the_lease_expires(store: DynStore) {
        let (faults, store) = faulty(store);
        let name = uniq("journal-fail");
        let effects = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let effects = effects.clone();
                move |ctx, state| {
                    let effects = effects.clone();
                    async move {
                        let seen: Result<usize, String> = ctx
                            .step(
                                "effect",
                                || async move { Ok(effects.fetch_add(1, SeqCst) + 1) },
                            )
                            .await?;
                        Ok(Transition::Done {
                            state,
                            output: json!(seen.unwrap_or(0)),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .lease_ttl(Duration::from_millis(300))
            .build();
        faults.fail(Method::JournalPut, 1);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(faults.injected(Method::JournalPut), 1);
        assert_eq!(count(&effects), 2, "unrecorded, so it ran again");
        assert_eq!(
            done.output,
            Some(json!(2)),
            "and only the second result was recorded"
        );
    }

    /// A lease that cannot be renewed expires under a slow step; another
    /// worker takes the run over and finishes it, and the slow worker's late
    /// commit is rejected.
    ///
    /// As in the test without renewal, the slow step is held at a gate until
    /// the takeover has committed, and the lease lapses by moving a frozen
    /// clock, after a renewal has been seen to fail.
    pub async fn renew_failure_lets_another_worker_take_over(store: DynStore) {
        let (faults, faulty_store) = faulty(store.clone());
        let name = uniq("renew-fail");
        let invocations = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let slow_finished = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (invocations, started, gate, slow_finished) = (
                    invocations.clone(),
                    started.clone(),
                    gate.clone(),
                    slow_finished.clone(),
                );
                move |_ctx, state| {
                    let (invocations, started, gate, slow_finished) = (
                        invocations.clone(),
                        started.clone(),
                        gate.clone(),
                        slow_finished.clone(),
                    );
                    async move {
                        if invocations.fetch_add(1, SeqCst) == 0 {
                            started.notify_one();
                            notified(&gate, "the takeover to finish").await;
                            slow_finished.fetch_add(1, SeqCst);
                            return Ok(Transition::Done {
                                state,
                                output: json!("slow"),
                            });
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!("fast"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        faults.fail_always(Method::RenewLease);
        let clock = FrozenClock::new();
        let a = builder(&faulty_store, "renew-fail-a", &agent)
            .lease_ttl(Duration::from_millis(300))
            .clock(clock.clone())
            .build();
        let b = builder(&store, "renew-fail-b", &agent)
            .clock(clock.clone())
            .build();
        let run = a.start(&name, inbound(), None).await.expect("start");
        let wa = spawn_worker(&a);
        notified(&started, "the slow step").await;
        wait_until("a renewal to be attempted and fail", || {
            faults.injected(Method::RenewLease) >= 1
        })
        .await;
        clock.advance(Duration::from_millis(350)); // the unrenewed lease lapses
        let wb = spawn_worker(&b);

        let done = wait_done(&b, run).await;
        assert_eq!(done.output, Some(json!("fast")));
        assert_eq!(count(&slow_finished), 0, "the slow step is still held");
        let committed_version = done.version;

        // Only now may the slow step finish and attempt its commit.
        gate.notify_one();
        wait_until("the slow step to finish", || count(&slow_finished) == 1).await;
        wa.stop().await; // waits for its commit attempt
        wb.stop().await;

        let after = b.view(run).await.expect("view").expect("run");
        assert_eq!(after.output, Some(json!("fast")), "the late commit lost");
        assert_eq!(after.version, committed_version);
        assert_eq!(count(&invocations), 2);
    }

    /// A lease that cannot be released simply expires; the run still goes on.
    pub async fn release_failure_is_harmless(store: DynStore) {
        let (faults, store) = faulty(store);
        let name = uniq("release-fail");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(if phase(&state) == 0 {
                        Transition::Continue(json!({"phase": 1}))
                    } else {
                        Transition::Done {
                            state,
                            output: json!("finished"),
                        }
                    })
                }
                .boxed()
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .lease_ttl(Duration::from_millis(200))
            .build();
        faults.fail_always(Method::ReleaseLease);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!("finished")));
        assert!(faults.injected(Method::ReleaseLease) >= 2);
    }

    /// Every API call reports an outage as a retryable store error and leaves
    /// the run exactly as it was; the same call works once the store is back.
    pub async fn api_calls_surface_store_outage_as_retryable_errors(store: DynStore) {
        let (faults, store) = faulty(store);
        let name = uniq("api-outage");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    Ok(Transition::Park {
                        state,
                        wake_at: None,
                    })
                }
                .boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let retryable = |e: RuntimeError| {
            assert!(
                matches!(&e, RuntimeError::Store(s) if adam_store_testkit::fault::is_injected(s)),
                "{e:?}"
            );
            assert!(e.is_retryable(), "{e:?}");
        };

        // start, plain and through a conversation lookup
        faults.fail(Method::CreateRun, 1);
        retryable(rt.start(&name, inbound(), None).await.expect_err("outage"));
        faults.fail(Method::OpenRunForConversation, 1);
        retryable(
            rt.start(&name, inbound(), Some("conv"))
                .await
                .expect_err("outage"),
        );
        faults.fail(Method::CreateRun, 1);
        retryable(
            rt.start_with_id(RunId::new(), &name, inbound(), None)
                .await
                .expect_err("outage"),
        );
        let run = rt.start(&name, inbound(), None).await.expect("recovered");

        // view, deliver, cancel: reads and writes both
        faults.fail(Method::LoadRun, 1);
        retryable(rt.view(run).await.expect_err("outage"));
        faults.fail(Method::LoadRun, 1);
        retryable(rt.deliver(run, inbound()).await.expect_err("outage"));
        faults.fail(Method::CommitRun, 1);
        retryable(rt.deliver(run, inbound()).await.expect_err("outage"));
        faults.fail(Method::CommitRun, 1);
        retryable(rt.cancel(run, "x").await.expect_err("outage"));

        let untouched = rt.view(run).await.expect("view").expect("run");
        assert_eq!(untouched.status, RunStatus::Runnable);
        assert_eq!(untouched.pending_inbox, 0, "a failed deliver adds nothing");
        assert_eq!(untouched.error, None, "a failed cancel cancels nothing");

        rt.deliver(run, inbound()).await.expect("deliver recovers");
        rt.cancel(run, "now for real")
            .await
            .expect("cancel recovers");
        let view = rt.view(run).await.expect("view").expect("run");
        assert_eq!(view.status, RunStatus::Failed);
        assert_eq!(view.error.as_deref(), Some("cancelled: now for real"));
    }

    // -----------------------------------------------------------------------
    // Cancellation signal (`Ctx::cancelled`)
    // -----------------------------------------------------------------------

    /// `Runtime::cancel` reaches the step that is running: `Ctx::cancelled`
    /// (also through a cloned token in a spawned task) resolves, the step
    /// stops early, and its result is still dropped.
    pub async fn cancel_signals_the_step_that_is_running(store: DynStore) {
        let name = uniq("cancel-signal");
        let started = Arc::new(Notify::new());
        let observed = Arc::new(Notify::new());
        let spawned_saw = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, observed, spawned_saw) =
                    (started.clone(), observed.clone(), spawned_saw.clone());
                move |ctx, state| {
                    let (started, observed, spawned_saw) =
                        (started.clone(), observed.clone(), spawned_saw.clone());
                    async move {
                        assert!(!ctx.is_cancelled());
                        let token = ctx.cancel_token();
                        tokio::spawn(async move {
                            token.cancelled().await;
                            spawned_saw.notify_one();
                        });
                        started.notify_one();
                        tokio::select! {
                            () = ctx.cancelled() => {
                                assert!(ctx.is_cancelled());
                                observed.notify_one();
                                Ok(Transition::Done { state, output: json!("stopped early") })
                            }
                            () = tokio::time::sleep(Duration::from_secs(60)) => {
                                Ok(Transition::Done { state, output: json!("ran to the end") })
                            }
                        }
                    }
                    .boxed()
                }
            }),
        );
        let sink = CollectingSink::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .event_sink(sink.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the step").await;

        rt.cancel(run, "stop").await.expect("cancel");
        notified(&observed, "the step to see the cancellation").await;
        notified(&spawned_saw, "the cloned token to fire").await;
        // Would take 60 s (and fail `stop`'s timeout) if the step had not
        // been told.
        worker.stop().await;

        let view = rt.view(run).await.expect("view").expect("run");
        assert_eq!(view.status, RunStatus::Failed);
        assert_eq!(view.error.as_deref(), Some("cancelled: stop"));
        assert_eq!(view.output, None, "the stopped step's result is dropped");
        assert!(
            !sink.events_for(run).iter().any(|e| matches!(
                e,
                RunEvent::Status {
                    status: RunStatus::Done,
                    ..
                }
            )),
            "a dropped result must not be announced"
        );
    }

    /// A cancel issued by another process (another `Runtime` on the same
    /// store) reaches the step through the worker's watch on the run.
    pub async fn cancel_issued_elsewhere_signals_the_step(store: DynStore) {
        let name = uniq("cancel-remote");
        let started = Arc::new(Notify::new());
        let observed = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, observed) = (started.clone(), observed.clone());
                move |ctx, state| {
                    let (started, observed) = (started.clone(), observed.clone());
                    async move {
                        started.notify_one();
                        tokio::select! {
                            () = ctx.cancelled() => {
                                observed.notify_one();
                                Ok(Transition::Done { state, output: json!("stopped early") })
                            }
                            () = tokio::time::sleep(Duration::from_secs(60)) => {
                                Ok(Transition::Done { state, output: json!("ran to the end") })
                            }
                        }
                    }
                    .boxed()
                }
            }),
        );
        let a = builder(&store, "cancel-remote-a", &agent).build();
        let b = builder(&store, "cancel-remote-b", &agent).build();
        let run = a.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&a);
        notified(&started, "the step on a").await;

        b.cancel(run, "from b")
            .await
            .expect("cancel on the other runtime");
        notified(&observed, "a's step to see b's cancellation").await;
        worker.stop().await;
        let view = a.view(run).await.expect("view").expect("run");
        assert_eq!(view.error.as_deref(), Some("cancelled: from b"));
    }

    /// With a shared notifier a run started, and a message delivered, by one
    /// runtime wakes the idle worker of another at once, despite a 30 s poll.
    pub async fn notifier_wakes_a_worker_of_another_runtime(store: DynStore) {
        let name = uniq("notify-wake");
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if phase(&state) == 0 {
                        return Ok(Transition::Park {
                            state: json!({"phase": 1}),
                            wake_at: None,
                        });
                    }
                    Ok(Transition::Done {
                        state,
                        output: json!(ctx.take_inbox().len()),
                    })
                }
                .boxed()
            }),
        );
        let notifier = LocalNotifier::default();
        let slow = |worker: &str| {
            builder(&store, worker, &agent)
                .poll_interval(Duration::from_secs(30))
                .notifier(notifier.clone())
                .build()
        };
        let (front, back) = (slow("notify-front"), slow("notify-back"));
        let worker = spawn_worker(&back);
        // Let the worker find nothing and go idle on its 30 s poll.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let run = front.start(&name, inbound(), None).await.expect("start");
        wait_for_within(
            &back,
            run,
            Duration::from_secs(5),
            "parked despite a 30s poll",
            |v| v.waiting,
        )
        .await;
        front.deliver(run, inbound()).await.expect("deliver");
        let done = wait_for_within(
            &back,
            run,
            Duration::from_secs(5),
            "done despite a 30s poll",
            |v| v.status == RunStatus::Done,
        )
        .await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!(1)));
    }

    /// With a shared notifier a cancel issued by one runtime reaches the step
    /// another is running at once, not after a 30 s poll.
    pub async fn notifier_carries_a_cancel_to_another_runtime(store: DynStore) {
        let name = uniq("notify-cancel");
        let started = Arc::new(Notify::new());
        let observed = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, observed) = (started.clone(), observed.clone());
                move |ctx, state| {
                    let (started, observed) = (started.clone(), observed.clone());
                    async move {
                        started.notify_one();
                        tokio::select! {
                            () = ctx.cancelled() => {
                                observed.notify_one();
                                Ok(Transition::Done { state, output: json!("stopped early") })
                            }
                            () = tokio::time::sleep(Duration::from_secs(60)) => {
                                Ok(Transition::Done { state, output: json!("ran to the end") })
                            }
                        }
                    }
                    .boxed()
                }
            }),
        );
        let notifier = LocalNotifier::default();
        let slow = |worker: &str| {
            builder(&store, worker, &agent)
                .poll_interval(Duration::from_secs(30))
                .notifier(notifier.clone())
                .build()
        };
        let (a, b) = (slow("notify-cancel-a"), slow("notify-cancel-b"));
        let run = a.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&a);
        notified(&started, "the step on a").await;

        b.cancel(run, "from b").await.expect("cancel on b");
        tokio::time::timeout(Duration::from_secs(5), observed.notified())
            .await
            .expect("a's step sees b's cancellation despite a 30s poll");
        worker.stop().await;
        let view = a.view(run).await.expect("view").expect("run");
        assert_eq!(view.error.as_deref(), Some("cancelled: from b"));
    }

    /// The signal is per run: cancelling one run leaves a concurrent run's
    /// token untouched, and a fresh transition starts with a fresh token.
    pub async fn cancel_only_signals_its_own_run(store: DynStore) {
        let name = uniq("cancel-scoped");
        let started = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        let observed = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, observed, release) =
                    (started.clone(), observed.clone(), release.clone());
                move |ctx, state| {
                    let (started, observed, release) =
                        (started.clone(), observed.clone(), release.clone());
                    async move {
                        let which = state["which"].as_u64().unwrap_or(0) as usize;
                        started[which].notify_one();
                        if which == 0 {
                            ctx.cancelled().await;
                            observed.notify_one();
                            return Ok(Transition::Done {
                                state,
                                output: json!("cancelled"),
                            });
                        }
                        release.notified().await;
                        Ok(Transition::Done {
                            state,
                            output: json!({"saw_cancel": ctx.is_cancelled()}),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let victim = rt
            .start(&name, Inbound::new("start", json!({"which": 0})), None)
            .await
            .expect("start");
        let bystander = rt
            .start(&name, Inbound::new("start", json!({"which": 1})), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        notified(&started[0], "the victim's step").await;
        notified(&started[1], "the bystander's step").await;

        rt.cancel(victim, "only you").await.expect("cancel");
        notified(&observed, "the victim to see it").await;
        release.notify_one();
        let done = wait_done(&rt, bystander).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!({"saw_cancel": false})));
        assert_eq!(
            rt.view(victim).await.expect("view").expect("run").status,
            RunStatus::Failed
        );
    }

    const PLACEMENT_WORKERS: [&str; 3] = ["pin-a", "pin-b", "pin-c"];
    const PLACEMENT_STEPS: u64 = 6;

    /// Three runtimes (one worker id each) over one store, and what they stepped.
    struct Placement {
        runtimes: Vec<Runtime>,
        name: String,
        /// Per run, the workers that stepped it, in order.
        seen: Arc<Mutex<HashMap<RunId, Vec<String>>>>,
        /// The runs seeded by `seed_placement`, one per seeded worker, in `PLACEMENT_WORKERS` order.
        seeded: Vec<RunId>,
    }

    impl Placement {
        async fn start(&self) -> RunId {
            self.runtimes[0]
                .start(&self.name, Inbound::new("start", json!({"n": 0})), None)
                .await
                .expect("start")
        }

        fn steps(&self, run: RunId) -> Vec<String> {
            self.seen
                .lock()
                .expect("lock")
                .get(&run)
                .cloned()
                .unwrap_or_default()
        }
    }

    /// Builds the three runtimes and seeds one run for each of the first `seeds` workers: the
    /// worker runs alone, takes its run for exactly one step (which fixes the run's first
    /// claimant, and so its owner under `Pinned`), and is stopped again with the run unfinished.
    /// Which worker first claims a run is otherwise a race that any one worker can win (they all
    /// poll the same store), so leaving it to the scheduler makes "more than one worker took
    /// part" flaky under load. (With `Any` a later seed would also step the earlier runs, so
    /// seed only as many workers as the case needs.)
    async fn seed_placement(store: DynStore, scope: ClaimScope, seeds: usize) -> Placement {
        let name = uniq("placement");
        let seen = Arc::new(Mutex::new(HashMap::<RunId, Vec<String>>::new()));
        // While set, the first step of a run waits, so the seeding worker cannot get past it
        // before it is told to stop.
        let hold = Arc::new(AtomicBool::new(true));
        let runtimes: Vec<Runtime> = PLACEMENT_WORKERS
            .into_iter()
            .map(|worker| {
                // One agent per runtime, so a step knows which worker runs it.
                let agent = fn_agent(
                    &name,
                    step_fn({
                        let seen = seen.clone();
                        let hold = hold.clone();
                        move |ctx, state| {
                            let seen = seen.clone();
                            let hold = hold.clone();
                            async move {
                                let n = state.get("n").and_then(Value::as_u64).unwrap_or(0);
                                seen.lock()
                                    .expect("lock")
                                    .entry(ctx.run_id())
                                    .or_default()
                                    .push(worker.to_owned());
                                let deadline = Instant::now() + Duration::from_secs(30);
                                while n == 0 && hold.load(SeqCst) && Instant::now() < deadline {
                                    tokio::time::sleep(Duration::from_millis(2)).await;
                                }
                                tokio::time::sleep(Duration::from_millis(8)).await;
                                Ok(if n + 1 >= PLACEMENT_STEPS {
                                    Transition::Done {
                                        state,
                                        output: json!(n),
                                    }
                                } else {
                                    Transition::Continue(json!({ "n": n + 1 }))
                                })
                            }
                            .boxed()
                        }
                    }),
                );
                builder(&store, worker, &agent)
                    .claim_scope(scope)
                    .concurrency(2)
                    .build()
            })
            .collect();
        assert!(runtimes.iter().all(|rt| rt.claim_scope() == scope));
        let mut placement = Placement {
            runtimes,
            name,
            seen,
            seeded: Vec::new(),
        };
        for (i, worker) in PLACEMENT_WORKERS.into_iter().enumerate().take(seeds) {
            hold.store(true, SeqCst);
            let run = placement.start().await;
            let running = spawn_worker(&placement.runtimes[i]);
            let deadline = Instant::now() + Duration::from_secs(20);
            while placement.steps(run).is_empty() {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {worker} to take its run"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            // Stop first, then let the step go: the worker finishes and commits it, and (as it
            // was told to stop before) claims nothing more.
            let _ = running.stop.send(());
            hold.store(false, SeqCst);
            tokio::time::timeout(Duration::from_secs(10), running.handle)
                .await
                .expect("worker stops in time")
                .expect("worker task")
                .expect("worker result");
            assert_eq!(
                placement.steps(run),
                vec![worker.to_owned()],
                "the seed step ran on {worker} alone"
            );
            let view = placement.runtimes[0]
                .view(run)
                .await
                .expect("view")
                .expect("run");
            assert_ne!(view.status, RunStatus::Done, "the seeded run is unfinished");
            placement.seeded.push(run);
        }
        hold.store(false, SeqCst);
        placement
    }

    /// With `Pinned`, a multi-step run always steps on the worker that first claimed it, while
    /// the runs are shared out between the workers.
    ///
    /// Each worker owns one unfinished run before the others start, so all three necessarily take
    /// part; then all three run together over those runs and nine new ones, and no run may
    /// change hands, however the scheduler orders them.
    pub async fn pinned_workers_step_a_run_only_on_its_owner(store: DynStore) {
        let placement = seed_placement(store, ClaimScope::Pinned, PLACEMENT_WORKERS.len()).await;
        let mut runs = placement.seeded.clone();
        for _ in 0..9 {
            runs.push(placement.start().await);
        }
        let workers: Vec<Worker> = placement.runtimes.iter().map(spawn_worker).collect();
        for run in &runs {
            wait_done(&placement.runtimes[0], *run).await;
        }
        for w in workers {
            w.stop().await;
        }
        let mut took_part = HashSet::new();
        for (i, run) in runs.iter().enumerate() {
            let steps = placement.steps(*run);
            assert_eq!(steps.len(), PLACEMENT_STEPS as usize, "every step ran once");
            let owner = &steps[0];
            assert!(
                steps.iter().all(|w| w == owner),
                "run {i} moved between workers: {steps:?}"
            );
            if let Some(seed) = PLACEMENT_WORKERS.get(i) {
                assert_eq!(owner, seed, "a seeded run stays with its seeding worker");
            }
            took_part.insert(owner.clone());
        }
        assert_eq!(
            took_part.len(),
            PLACEMENT_WORKERS.len(),
            "the work should be shared, only {took_part:?} took part"
        );
    }

    /// The control: with `Any`, a run one worker started can be finished by another, which is
    /// what pinning prevents (and what forked coder runs on separate disks). Here `pin-a`'s
    /// seeded run is finished by `pin-b` alone, so it must move.
    pub async fn any_workers_let_a_run_move_between_workers(store: DynStore) {
        let placement = seed_placement(store, ClaimScope::Any, 1).await;
        let run = placement.seeded[0];
        let only_b = spawn_worker(&placement.runtimes[1]);
        wait_done(&placement.runtimes[0], run).await;
        only_b.stop().await;
        let steps = placement.steps(run);
        assert_eq!(steps.len(), PLACEMENT_STEPS as usize, "every step ran once");
        assert_eq!(steps[0], "pin-a");
        assert!(
            steps[1..].iter().all(|w| w == "pin-b"),
            "the run should have moved to pin-b: {steps:?}"
        );
    }

    // -- child runs -----------------------------------------------------

    /// The child records its parent, and starting the same child twice starts it once.
    pub async fn start_child_records_the_parent_and_is_idempotent(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let parent_agent = child_parked(&name);
        let rt = builder(&store, &uniq("w"), &parent_agent)
            .agent(child_parked(&child_name))
            .build();
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let id = child_run_id(parent, "call-1");

        let first = Inbound::new("start", json!({"which": "first"}));
        assert!(
            rt.start_child(parent, id, &child_name, first)
                .await
                .expect("first")
        );
        let second = Inbound::new("start", json!({"which": "second"}));
        assert!(
            !rt.start_child(parent, id, &child_name, second)
                .await
                .expect("second"),
            "the same id is the same child"
        );

        let rec = store.load_run(id).await.expect("load").expect("child");
        assert_eq!(rec.parent_id, Some(parent));
        assert_eq!(rec.agent, child_name);
        assert_eq!(rec.conversation_id, None);
        let view = rt.view(id).await.expect("view").expect("child");
        assert_eq!(view.state["which"], "first", "the second input was ignored");
        // A run that is not a child has no parent, and the parent is not a child of anyone.
        assert_eq!(
            store.load_run(parent).await.unwrap().unwrap().parent_id,
            None
        );

        let unknown = rt
            .start_child(parent, RunId::new(), "nobody", inbound())
            .await;
        assert!(
            matches!(unknown, Err(RuntimeError::UnknownAgent(_))),
            "{unknown:?}"
        );
    }

    /// The child finishes, the parent resumes exactly once with its answer, and it got it from
    /// the message: the timer (an hour off, on a clock nobody moves) could not have.
    pub async fn a_finished_child_wakes_its_parent_once(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let child_agent = child_done(&child_name, json!({"answer": 42}));
        let clock = ManualClock::new();
        let rt = family(&store, &parent_agent, &child_agent, &cell, &clock);
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        let done = wait_done(&rt, parent).await;
        worker.stop().await;

        assert_eq!(
            done.output,
            Some(json!({"via": "notice", "status": {"status": "done", "output": {"answer": 42}}}))
        );
        assert_eq!(count(&watch.effects), 1, "one child was started");
        assert_eq!(count(&watch.resumes), 1, "the parent resumed once");
        assert_eq!(count(&watch.steps), 2);
        assert_eq!(done.pending_inbox, 0);
        let child = child_run_id(parent, "call-1");
        let record = store.load_run(child).await.unwrap().unwrap();
        assert_eq!(
            (record.parent_id, record.status),
            (Some(parent), RunStatus::Done)
        );
    }

    /// The child finishes while the parent's first step is still running: the message waits in
    /// the inbox, the step's park turns into a wake-up, and the parent resumes without its timer.
    pub async fn a_notice_that_beats_the_park_is_not_lost(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (cell, watch) = (
            RtCell::default(),
            Arc::new(Watch {
                hold: Some(Hold::new()),
                ..Watch::default()
            }),
        );
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let child_agent = child_done(&child_name, json!("early"));
        let clock = ManualClock::new();
        let rt = family(&store, &parent_agent, &child_agent, &cell, &clock);
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let hold = watch.hold.as_ref().expect("hold");

        // The parent has started its child and is held before it parks.
        notified(&hold.reached, "the parent to start its child").await;
        let child = child_run_id(parent, "call-1");
        wait_done(&rt, child).await;
        wait_for(&rt, parent, "the notice in the parent's inbox", |v| {
            v.pending_inbox == 1
        })
        .await;
        assert_eq!(count(&watch.resumes), 0);

        hold.release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(
            done.output,
            Some(json!({"via": "notice", "status": {"status": "done", "output": "early"}}))
        );
        assert_eq!(count(&watch.resumes), 1);
        assert_eq!(count(&watch.effects), 1);
    }

    /// A child that fails, and one that is cancelled, tell the parent why.
    pub async fn a_failed_or_cancelled_child_tells_its_parent_why(store: DynStore) {
        let (name, failing) = (uniq("parent"), uniq("failing"));
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let parent_agent = parent_of(&name, &failing, &cell, &watch);
        let clock = ManualClock::new();
        let rt = family(
            &store,
            &parent_agent,
            &child_failing(&failing, "the build is red"),
            &cell,
            &clock,
        );
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(
            done.output,
            Some(json!({
                "via": "notice",
                "status": {"status": "failed", "error": "the build is red"}
            }))
        );

        // A child cancelled from outside: the cancel commit is the terminal commit, and it tells.
        let (name, parked) = (uniq("parent"), uniq("parked"));
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let parent_agent = parent_of(&name, &parked, &cell, &watch);
        let rt = family(&store, &parent_agent, &child_parked(&parked), &cell, &clock);
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let child = child_run_id(parent, "call-1");
        wait_for(&rt, parent, "the parent parked", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;
        wait_for(&rt, child, "the child waiting", |v| v.waiting).await;
        rt.cancel(child, "not needed").await.expect("cancel");
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(
            done.output,
            Some(json!({
                "via": "notice",
                "status": {"status": "failed", "error": "cancelled: not needed"}
            }))
        );
    }

    /// The message is lost: the parent's commit fails when the finished child tries to deliver it.
    /// The parent stays parked on its timer, and when the timer fires it reads the child.
    pub async fn a_lost_notice_is_recovered_by_the_timer(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (faulty, dynamic) = faulty(store);
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let clock = ManualClock::new();
        // The front knows how to start the child; a separate runtime steps it.
        let front = builder(&dynamic, &uniq("front"), &parent_agent)
            .starter(JsonStarter(child_name.clone()))
            .clock(clock.clone())
            .build();
        cell.set(front.clone()).ok().expect("set once");
        let child_agent = child_done(&child_name, json!("late but sure"));
        let back = builder(&dynamic, &uniq("back"), &child_agent).build();

        let parent = front.start(&name, inbound(), None).await.expect("start");
        let child = child_run_id(parent, "call-1");
        let front_worker = spawn_worker(&front);
        wait_for(&front, parent, "the parent parked on its timer", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;

        // From now on the parent's next commit fails once: the child's message.
        faulty.fail_run(Method::CommitRun, parent, 1);
        let back_worker = spawn_worker(&back);
        wait_done(&back, child).await;
        wait_injected(&faulty, Method::CommitRun, 1, "the message to fail").await;
        back_worker.stop().await;

        let parked = front.view(parent).await.unwrap().unwrap();
        assert_eq!(parked.status, RunStatus::Parked, "nobody woke the parent");
        assert_eq!(parked.pending_inbox, 0, "and no message is waiting for it");
        assert_eq!(count(&watch.resumes), 0);

        clock.advance(Duration::from_secs(61));
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(
            done.output,
            Some(json!({
                "via": "read",
                "status": {"status": "done", "output": "late but sure"}
            }))
        );
        assert_eq!(count(&watch.resumes), 1);
        assert_eq!(count(&watch.effects), 1);
    }

    /// The child's terminal commit is applied but its acknowledgement is lost, so the worker
    /// believes it failed and sends nothing. Same recovery: the timer, then a read.
    pub async fn a_lost_terminal_ack_sends_no_notice_and_the_timer_recovers(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (faulty, dynamic) = faulty(store);
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let clock = ManualClock::new();
        let front = builder(&dynamic, &uniq("front"), &parent_agent)
            .starter(JsonStarter(child_name.clone()))
            .clock(clock.clone())
            .build();
        cell.set(front.clone()).ok().expect("set once");
        let child_agent = child_done(&child_name, json!("done anyway"));
        let back = builder(&dynamic, &uniq("back"), &child_agent).build();

        let parent = front.start(&name, inbound(), None).await.expect("start");
        let child = child_run_id(parent, "call-1");
        let front_worker = spawn_worker(&front);
        wait_for(&front, parent, "the parent parked on its timer", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;

        faulty.fail_run_after_apply(Method::CommitRun, child, 1);
        let back_worker = spawn_worker(&back);
        wait_injected(&faulty, Method::CommitRun, 1, "the child's ack to be lost").await;
        wait_done(&back, child).await;
        back_worker.stop().await;

        let parked = front.view(parent).await.unwrap().unwrap();
        assert_eq!(
            (parked.status, parked.pending_inbox),
            (RunStatus::Parked, 0)
        );

        clock.advance(Duration::from_secs(61));
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(
            done.output,
            Some(json!({
                "via": "read",
                "status": {"status": "done", "output": "done anyway"}
            }))
        );
        assert_eq!(count(&watch.resumes), 1);
    }

    /// The timer fires while the child still works: the parent looks, sees it is not done, and
    /// parks again on a later timer, without finishing and without starting anything.
    pub async fn a_parent_woken_early_parks_again(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let child_agent = child_gated(&child_name, &started, &release);
        let clock = ManualClock::new();
        let rt = family(&store, &parent_agent, &child_agent, &cell, &clock);
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the child").await;
        let first = wait_for(&rt, parent, "parked on the timer", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;

        clock.advance(Duration::from_secs(61));
        let second = wait_for(&rt, parent, "parked on a later timer", |v| {
            v.status == RunStatus::Parked && v.wake_at > first.wake_at
        })
        .await;
        assert!(second.wake_at > first.wake_at);
        assert_eq!(count(&watch.resumes), 1, "one look at the child, no answer");
        assert_eq!(count(&watch.effects), 1, "and no second child");

        release.notify_one();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(done.output.expect("output")["via"], "notice");
        assert_eq!(count(&watch.resumes), 2);
    }

    /// Cancelling a parent that waits leaves the child running (there is no cascade in v1). The
    /// child finishes, its message finds a finished parent and is dropped without harm.
    pub async fn cancelling_a_waiting_parent_does_not_cancel_its_child(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let child_agent = child_gated(&child_name, &started, &release);
        let clock = ManualClock::new();
        let rt = family(&store, &parent_agent, &child_agent, &cell, &clock);
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the child").await;
        wait_for(&rt, parent, "the parent parked", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;

        rt.cancel(parent, "changed my mind").await.expect("cancel");
        let cancelled = rt.view(parent).await.unwrap().unwrap();
        assert_eq!(cancelled.status, RunStatus::Failed);
        let child = child_run_id(parent, "call-1");
        assert!(
            rt.view(child).await.unwrap().unwrap().status.is_open(),
            "the child is still running"
        );

        release.notify_one();
        let done = wait_done(&rt, child).await;
        assert_eq!(
            done.output,
            Some(json!({"saw_cancel": false})),
            "the child was never told"
        );
        worker.stop().await;
        let after = rt.view(parent).await.unwrap().unwrap();
        assert_eq!(after.status, RunStatus::Failed);
        assert_eq!(after.error.as_deref(), Some("cancelled: changed my mind"));
        assert_eq!(
            after.version, cancelled.version,
            "nothing touched the parent"
        );
        assert_eq!(count(&watch.resumes), 0);
    }

    /// Fencing. The worker that started the child loses its lease before it commits; another
    /// worker replays the step (the journal keeps the child from being started twice) and parks
    /// the parent, and when the first worker finally commits, the store refuses it. The child
    /// then runs, and the parent resumes once.
    pub async fn a_parent_that_loses_its_lease_does_not_start_or_resume_twice(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (cell_a, cell_b) = (RtCell::default(), RtCell::default());
        let watch = Arc::new(Watch {
            hold: Some(Hold::new()),
            ..Watch::default()
        });
        let parent_a = parent_of(&name, &child_name, &cell_a, &watch);
        let parent_b = parent_of(&name, &child_name, &cell_b, &watch);
        let a = builder(&store, "fence-a", &parent_a)
            .starter(JsonStarter(child_name.clone()))
            .lease_ttl(Duration::from_millis(200))
            .lease_renewal(false)
            .build();
        cell_a.set(a.clone()).ok().expect("set once");
        let b = builder(&store, "fence-b", &parent_b)
            .starter(JsonStarter(child_name.clone()))
            .build();
        cell_b.set(b.clone()).ok().expect("set once");
        let child_steps = Arc::new(AtomicUsize::new(0));
        let child_agent = fn_agent(
            &child_name,
            step_fn({
                let child_steps = child_steps.clone();
                move |_ctx, state| {
                    let child_steps = child_steps.clone();
                    async move {
                        child_steps.fetch_add(1, SeqCst);
                        Ok(Transition::Done {
                            state,
                            output: json!("once"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let c = builder(&store, "fence-c", &child_agent).build();

        let parent = a.start(&name, inbound(), None).await.expect("start");
        let wa = spawn_worker(&a);
        let hold = watch.hold.as_ref().expect("hold");
        notified(&hold.reached, "worker A to start the child").await;
        assert_eq!(count(&watch.effects), 1);

        // A is stuck holding a lease that runs out. B takes the run over, replays the step (the
        // start is in the journal) and parks the parent.
        let wb = spawn_worker(&b);
        let parked = wait_for(&b, parent, "B to park the parent", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;
        assert_eq!(
            count(&watch.effects),
            1,
            "the replay found the recorded start"
        );

        // A finally commits its step: the version moved, the store refuses it.
        hold.release.notify_one();
        wa.stop().await;
        let after_a = b.view(parent).await.unwrap().unwrap();
        assert_eq!(
            after_a.version, parked.version,
            "A's stale commit changed nothing"
        );
        assert_eq!(count(&child_steps), 0, "the child has not run yet");

        // Now the child runs, and tells the parent once.
        let wc = spawn_worker(&c);
        let done = wait_done(&b, parent).await;
        wb.stop().await;
        wc.stop().await;
        assert_eq!(done.output.as_ref().expect("output")["via"], "notice");
        assert_eq!(count(&watch.effects), 1, "one child was started");
        assert_eq!(count(&child_steps), 1, "and ran once");
        assert_eq!(count(&watch.resumes), 1, "the parent resumed once");
        let child = child_run_id(parent, "call-1");
        assert_eq!(
            store.load_run(child).await.unwrap().unwrap().parent_id,
            Some(parent)
        );
    }

    /// `Ctx::child_status` reads the caller's own children, and nobody else's: an unrelated run
    /// is an error, a run that does not exist is `None`.
    pub async fn child_status_reads_only_the_callers_children(store: DynStore) {
        let (name, child_name, other_name) = (uniq("parent"), uniq("child"), uniq("other"));
        let agent = fn_agent(
            &name,
            step_fn(|ctx, state| {
                async move {
                    if ctx.take_inbox().is_empty() {
                        return Ok(Transition::Park {
                            state,
                            wake_at: None,
                        });
                    }
                    let child: RunId = serde_json::from_value(state["child"].clone()).unwrap();
                    let other: RunId = serde_json::from_value(state["other"].clone()).unwrap();
                    let own = ctx.child_status(child).await?;
                    let stranger = ctx.child_status(other).await;
                    let missing = ctx.child_status(RunId::new()).await?;
                    Ok(Transition::Done {
                        state,
                        output: json!({
                            "own": own,
                            "stranger_is_permanent": matches!(stranger, Err(AgentError::Permanent { .. })),
                            "missing": missing,
                        }),
                    })
                }
                .boxed()
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .agent(child_parked(&child_name))
            .agent(child_parked(&other_name))
            .build();
        let child = RunId::new();
        let other = RunId::new();
        let parent = rt
            .start(
                &name,
                Inbound::new("start", json!({"child": child, "other": other})),
                None,
            )
            .await
            .expect("start");
        rt.start_child(parent, child, &child_name, inbound())
            .await
            .expect("child");
        // Not a child of the parent: started with no parent at all.
        rt.start_with_id(other, &other_name, inbound(), None)
            .await
            .expect("other");
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, parent).await;
        wait_waiting(&rt, child).await;

        rt.deliver(parent, inbound()).await.expect("wake");
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        let out = done.output.expect("output");
        assert_eq!(out["own"], json!({"status": "parked"}));
        assert_eq!(out["stranger_is_permanent"], true);
        assert_eq!(out["missing"], Value::Null);
    }

    /// A finished child that was purged before its parent looked is reported, not waited for.
    pub async fn a_purged_child_reads_as_gone(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (faulty, dynamic) = faulty(store);
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let clock = ManualClock::new();
        let front = builder(&dynamic, &uniq("front"), &parent_agent)
            .starter(JsonStarter(child_name.clone()))
            .clock(clock.clone())
            .build();
        cell.set(front.clone()).ok().expect("set once");
        let back = builder(&dynamic, &uniq("back"), &child_done(&child_name, json!(1))).build();

        let parent = front.start(&name, inbound(), None).await.expect("start");
        let child = child_run_id(parent, "call-1");
        let front_worker = spawn_worker(&front);
        wait_for(&front, parent, "parked", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;
        faulty.fail_run(Method::CommitRun, parent, 1);
        let back_worker = spawn_worker(&back);
        wait_done(&back, child).await;
        wait_injected(&faulty, Method::CommitRun, 1, "the message to fail").await;
        back_worker.stop().await;

        let purged = dynamic
            .purge_finished(&child_name, chrono::Utc::now() + chrono::Duration::hours(1))
            .await
            .expect("purge");
        assert!(purged >= 1);
        clock.advance(Duration::from_secs(61));
        let done = wait_done(&front, parent).await;
        front_worker.stop().await;
        assert_eq!(done.output.expect("output")["via"], "gone");
    }

    /// A finished-child message only means something to the run it is addressed to: to an
    /// agent that does not read it, it is one more inbox entry, consumed like the rest.
    pub async fn the_notice_is_an_ordinary_inbound_with_the_childs_id(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let seen = Arc::new(Mutex::new(Vec::<Inbound>::new()));
        let agent = fn_agent(
            &name,
            step_fn({
                let seen = seen.clone();
                move |ctx, state| {
                    let seen = seen.clone();
                    async move {
                        let inbox = ctx.take_inbox();
                        if inbox.is_empty() {
                            return Ok(Transition::Park {
                                state,
                                wake_at: None,
                            });
                        }
                        seen.lock().unwrap().extend(inbox);
                        Ok(Transition::Done {
                            state,
                            output: json!(null),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = builder(&store, &uniq("w"), &agent)
            .agent(child_failing(&child_name, "nope"))
            .build();
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, parent).await;
        let child = RunId::new();
        rt.start_child(parent, child, &child_name, inbound())
            .await
            .expect("child");
        wait_done(&rt, parent).await;
        worker.stop().await;

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].kind, RUN_FINISHED_KIND);
        assert_eq!(seen[0].id, child.to_string());
        assert_eq!(
            seen[0].payload,
            json!({"status": "failed", "error": "nope"})
        );
    }
}

// ---------------------------------------------------------------------------
// Instantiate every case per store
// ---------------------------------------------------------------------------

macro_rules! runtime_suite {
    ($module:ident, $make:path) => {
        mod $module {
            runtime_suite!(@cases $make;
                crash_safety,
                no_double_advance,
                park_and_deliver,
                deliver_during_step_is_merged,
                message_during_parking_step_wakes_the_run,
                timers,
                timers_with_manual_clock,
                retries_back_off_then_fail,
                retry_recovers_and_resets_attempt,
                permanent_fails_immediately,
                agent_fail_transition,
                non_determinism_fails_the_run,
                journal_replays_recorded_err,
                journal_seq_spans_transitions,
                ctx_accessors,
                cancel_parked,
                cancel_runnable,
                cancel_done_is_untouched,
                cancel_running_drops_the_workers_commit,
                lease_renewal_keeps_the_lease,
                stale_commit_is_rejected_without_renewal,
                graceful_shutdown,
                conversation_delivers_to_the_open_run,
                start_with_id_is_idempotent,
                api_errors,
                a_starter_only_runtime_starts_and_a_full_runtime_steps,
                events_and_durable_artifacts,
                broadcast_sink_streams_a_run,
                panic_is_a_transient_failure,
                deliver_wakes_local_workers_immediately,
                unknown_envelope_version_is_rejected,
                transient_with_minimum_delay_waits_at_least_that_long,
                retry_hint_never_shortens_the_backoff,
                huge_retry_hint_is_capped_and_does_not_panic,
                hinted_retries_still_exhaust_the_attempt_budget,
                store_outage_during_claim_is_survived,
                commit_failure_leaves_run_to_lease_expiry,
                commit_ack_lost_is_survived,
                journal_write_failure_reruns_the_step_after_the_lease_expires,
                renew_failure_lets_another_worker_take_over,
                release_failure_is_harmless,
                api_calls_surface_store_outage_as_retryable_errors,
                cancel_signals_the_step_that_is_running,
                cancel_issued_elsewhere_signals_the_step,
                cancel_only_signals_its_own_run,
                notifier_wakes_a_worker_of_another_runtime,
                notifier_carries_a_cancel_to_another_runtime,
                pinned_workers_step_a_run_only_on_its_owner,
                any_workers_let_a_run_move_between_workers,
                start_child_records_the_parent_and_is_idempotent,
                a_finished_child_wakes_its_parent_once,
                a_notice_that_beats_the_park_is_not_lost,
                a_failed_or_cancelled_child_tells_its_parent_why,
                a_lost_notice_is_recovered_by_the_timer,
                a_lost_terminal_ack_sends_no_notice_and_the_timer_recovers,
                a_parent_woken_early_parks_again,
                cancelling_a_waiting_parent_does_not_cancel_its_child,
                a_parent_that_loses_its_lease_does_not_start_or_resume_twice,
                child_status_reads_only_the_callers_children,
                a_purged_child_reads_as_gone,
                the_notice_is_an_ordinary_inbound_with_the_childs_id,
            );
        }
    };
    (@cases $make:path; $($case:ident),+ $(,)?) => {
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $case() {
                let Some(store) = $make().await else {
                    eprintln!("skipped: store not configured");
                    return;
                };
                super::cases::$case(store).await;
            }
        )+
    };
}

runtime_suite!(memory, super::memory_store);
runtime_suite!(postgres, super::postgres_store);
runtime_suite!(mongodb, super::mongodb_store);
