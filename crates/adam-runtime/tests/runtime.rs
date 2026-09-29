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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, NewRun, RunId, RunStatus};
use adam_runtime::{
    Agent, AgentError, AgentStarter, BroadcastSink, Classify, Clock, CollectingSink, Ctx, Inbound,
    LocalNotifier, MAX_RETRY_AFTER, ManualClock, RetryPolicy, RunEvent, RunView, Runtime,
    RuntimeBuilder, RuntimeError, Transition,
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

fn phase(state: &Value) -> u64 {
    state.get("phase").and_then(Value::as_u64).unwrap_or(0)
}

fn count(c: &AtomicUsize) -> usize {
    c.load(SeqCst)
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
    pub async fn deliver_during_step_is_merged(store: DynStore) {
        let name = uniq("merge");
        let phase0_runs = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let (phase0_runs, started) = (phase0_runs.clone(), started.clone());
                move |ctx, state| {
                    let (phase0_runs, started) = (phase0_runs.clone(), started.clone());
                    async move {
                        if phase(&state) == 0 {
                            phase0_runs.fetch_add(1, SeqCst);
                            started.notify_one();
                            tokio::time::sleep(Duration::from_millis(300)).await;
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
    pub async fn message_during_parking_step_wakes_the_run(store: DynStore) {
        let name = uniq("nosleep");
        let started = Arc::new(Notify::new());
        let agent = fn_agent(
            &name,
            step_fn({
                let started = started.clone();
                move |ctx, state| {
                    let started = started.clone();
                    async move {
                        if phase(&state) == 0 {
                            started.notify_one();
                            tokio::time::sleep(Duration::from_millis(300)).await;
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
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!(1)));
    }

    /// A parked run with `wake_at` is not stepped before it, and is after.
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
        let rt = runtime(&store, &agent);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);

        let parked = wait_for(&rt, run, "parked on a timer", |v| {
            v.status == RunStatus::Parked
        })
        .await;
        let wake_at = parked.wake_at.expect("timer set");
        assert!(!parked.waiting, "a timer is not 'waiting for input'");
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Read first, then the time: if the time is still before `wake_at`,
        // the read was too, so a second step would be a real early wake-up.
        // (On a loaded machine the 200 ms sleep can overshoot the timer.)
        let (steps_seen, status) = (
            count(&steps),
            rt.view(run).await.expect("view").expect("run").status,
        );
        if adam_core::store::now() < wake_at {
            assert_eq!(steps_seen, 1, "not stepped before wake_at");
            assert_eq!(status, RunStatus::Parked);
        }

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
    pub async fn cancel_running_drops_the_workers_commit(store: DynStore) {
        let name = uniq("cancel-running");
        let started = Arc::new(Notify::new());
        let finished = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, finished) = (started.clone(), finished.clone());
                move |_ctx, state| {
                    let (started, finished) = (started.clone(), finished.clone());
                    async move {
                        started.notify_one();
                        tokio::time::sleep(Duration::from_millis(400)).await;
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
        rt.cancel(run, "abort").await.expect("cancel");
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
    pub async fn lease_renewal_keeps_the_lease(store: DynStore) {
        let name = uniq("renew");
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let calls = calls.clone();
                move |_ctx, state| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, SeqCst);
                        // Three lease periods: without renewal the run would be
                        // taken over at least twice.
                        tokio::time::sleep(Duration::from_millis(3000)).await;
                        Ok(Transition::Done {
                            state,
                            output: json!("slow but ours"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        // Renewal runs every ttl/3 (~333 ms), leaving ~667 ms for a slow store
        // round trip before the lease could lapse. A 300 ms lease left ~200 ms
        // and flaked on loaded CI runners.
        let short = |w: &str| {
            builder(&store, w, &agent)
                .lease_ttl(Duration::from_millis(1000))
                .build()
        };
        let (a, b) = (short("renew-a"), short("renew-b"));
        let run = a.start(&name, inbound(), None).await.expect("start");
        let wa = spawn_worker(&a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let wb = spawn_worker(&b);
        let done = wait_done(&a, run).await;
        wa.stop().await;
        wb.stop().await;
        assert_eq!(done.output, Some(json!("slow but ours")));
        assert_eq!(count(&calls), 1, "nobody took the run over");
    }

    /// Without renewal a second worker takes over, and the stale commit of the
    /// first is rejected by the version CAS.
    pub async fn stale_commit_is_rejected_without_renewal(store: DynStore) {
        let name = uniq("stale");
        let invocations = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let slow_finished = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (invocations, started, slow_finished) =
                    (invocations.clone(), started.clone(), slow_finished.clone());
                move |_ctx, state| {
                    let (invocations, started, slow_finished) =
                        (invocations.clone(), started.clone(), slow_finished.clone());
                    async move {
                        if invocations.fetch_add(1, SeqCst) == 0 {
                            started.notify_one();
                            tokio::time::sleep(Duration::from_millis(900)).await;
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
        let a = builder(&store, "stale-a", &agent)
            .lease_ttl(Duration::from_millis(200))
            .lease_renewal(false)
            .event_sink(sink.clone())
            .build();
        let b = builder(&store, "stale-b", &agent)
            .event_sink(sink.clone())
            .build();
        let run = a.start(&name, inbound(), None).await.expect("start");
        let wa = spawn_worker(&a);
        notified(&started, "the slow step").await;
        let wb = spawn_worker(&b);

        let done = wait_done(&b, run).await;
        assert_eq!(done.output, Some(json!("fast")));
        let committed_version = done.version;

        // Wait for the slow step to finish and attempt its commit.
        let deadline = Instant::now() + Duration::from_secs(10);
        while count(&slow_finished) == 0 {
            assert!(Instant::now() < deadline, "slow step never finished");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        wa.stop().await;
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
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(50),
                multiplier: 2.0,
            })
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        // Each retry is 5 s away on the manual clock; step the clock until
        // the run gives up.
        let deadline = Instant::now() + Duration::from_secs(20);
        let failed = loop {
            let v = rt.view(run).await.expect("view").expect("run");
            if v.status == RunStatus::Failed {
                break v;
            }
            clock.advance(Duration::from_secs(6));
            assert!(Instant::now() < deadline, "never gave up: {v:#?}");
            tokio::time::sleep(Duration::from_millis(30)).await;
        };
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
        let rt = builder(&store, &uniq("w"), &agent)
            .lease_ttl(Duration::from_millis(300))
            .build();
        faults.fail(Method::CommitRun, 1);
        let started = Instant::now();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
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
        assert!(
            started.elapsed() >= Duration::from_millis(290),
            "the run was retried before its lease expired: {:?}",
            started.elapsed()
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
    pub async fn renew_failure_lets_another_worker_take_over(store: DynStore) {
        let (faults, faulty_store) = faulty(store.clone());
        let name = uniq("renew-fail");
        let invocations = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let slow_finished = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (invocations, started, slow_finished) =
                    (invocations.clone(), started.clone(), slow_finished.clone());
                move |_ctx, state| {
                    let (invocations, started, slow_finished) =
                        (invocations.clone(), started.clone(), slow_finished.clone());
                    async move {
                        if invocations.fetch_add(1, SeqCst) == 0 {
                            started.notify_one();
                            tokio::time::sleep(Duration::from_millis(900)).await;
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
        let a = builder(&faulty_store, "renew-fail-a", &agent)
            .lease_ttl(Duration::from_millis(300))
            .build();
        let b = builder(&store, "renew-fail-b", &agent).build();
        let run = a.start(&name, inbound(), None).await.expect("start");
        let wa = spawn_worker(&a);
        notified(&started, "the slow step").await;
        let wb = spawn_worker(&b);

        let done = wait_done(&b, run).await;
        assert_eq!(done.output, Some(json!("fast")));
        let committed_version = done.version;

        let deadline = Instant::now() + Duration::from_secs(10);
        while count(&slow_finished) == 0 {
            assert!(Instant::now() < deadline, "the slow step never finished");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        wa.stop().await;
        wb.stop().await;

        assert!(
            faults.injected(Method::RenewLease) >= 1,
            "the renewal was attempted and failed"
        );
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
