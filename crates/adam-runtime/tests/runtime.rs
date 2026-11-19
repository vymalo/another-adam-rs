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

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use adam_core::{ClaimScope, DynStore, JournalEntry, MemoryStore, NewRun, RunId, RunStatus};
use adam_runtime::{
    Agent, AgentError, AgentStarter, Artifact, BroadcastSink, CHILD_REPORTED_KIND, ChildStatus,
    Classify, Clock, CollectingSink, Ctx, Inbound, LocalNotifier, MAX_RETRY_AFTER, ManualClock,
    RUN_FINISHED_KIND, RetryPolicy, RoundOutcome, RunEvent, RunView, Runtime, RuntimeBuilder,
    RuntimeError, SessionState, Transition, child_run_id, parse_session_conversation_id,
    session_conversation_id,
};
use adam_store_testkit::counting::CountingStore;
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
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

/// A person's message that arrives while a run steps.
fn late() -> Inbound {
    Inbound::new("message", json!({"text": "late"}))
}

/// The id of a message the agent of the "finishing" cases says it has read (`Ctx::read_already`).
const READ_BEFORE: &str = "read-before";

/// A copy of a message the run has read, delivered again under the same id.
fn copy_of_a_read_message() -> Inbound {
    Inbound::new("message", json!({"text": "copy"})).with_id(READ_BEFORE)
}

/// The notice of a child that finished, as the runtime sends it to the parent.
fn finished_notice() -> Inbound {
    Inbound::new(RUN_FINISHED_KIND, json!({"status": "done", "output": "42"}))
        .with_id(RunId::new().to_string())
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
    /// What `Agent::on_cancelled` was called with, in order.
    cancelled: Arc<Mutex<Vec<(RunId, Value)>>>,
}

fn fn_agent(name: &str, step: Arc<StepFn>) -> FnAgent {
    FnAgent {
        name: name.to_owned(),
        step,
        cancelled: Arc::default(),
    }
}

impl FnAgent {
    fn hook_calls(&self) -> Vec<(RunId, Value)> {
        self.cancelled.lock().expect("lock").clone()
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

    async fn on_cancelled(&self, run: RunId, state: Value) {
        self.cancelled.lock().expect("lock").push((run, state));
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

/// A small file for the artifact tests.
const SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'/>";

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

/// A child that says it started and works until its token fires (the run was cancelled), then
/// says so.
fn child_listening(name: &str, started: &Arc<Notify>, saw: &Arc<AtomicBool>) -> FnAgent {
    let (started, saw) = (started.clone(), saw.clone());
    fn_agent(
        name,
        step_fn(move |ctx, _state| {
            let (started, saw) = (started.clone(), saw.clone());
            async move {
                started.notify_one();
                ctx.cancelled().await;
                saw.store(true, SeqCst);
                Err(AgentError::permanent("stopped"))
            }
            .boxed()
        }),
    )
}

/// A child that starts a child of its own, `leaf_agent`, in a journaled step, says it started, and
/// works until its token fires.
fn child_with_child(
    name: &str,
    leaf_agent: &str,
    cell: &RtCell,
    started: &Arc<Notify>,
    saw: &Arc<AtomicBool>,
) -> FnAgent {
    let (leaf_agent, cell, started, saw) = (
        leaf_agent.to_owned(),
        cell.clone(),
        started.clone(),
        saw.clone(),
    );
    fn_agent(
        name,
        step_fn(move |ctx, _state| {
            let (leaf_agent, cell, started, saw) = (
                leaf_agent.clone(),
                cell.clone(),
                started.clone(),
                saw.clone(),
            );
            async move {
                let me = ctx.run_id();
                let leaf = child_run_id(me, "call-1");
                let rt = cell.get().expect("the runtime is set").clone();
                let made: Result<bool, String> = ctx
                    .step("start-leaf", move || async move {
                        rt.start_child(me, leaf, &leaf_agent, inbound())
                            .await
                            .map_err(|e| e.to_string())
                    })
                    .await?;
                made.map_err(AgentError::permanent)?;
                started.notify_one();
                ctx.cancelled().await;
                saw.store(true, SeqCst);
                Err(AgentError::permanent("stopped"))
            }
            .boxed()
        }),
    )
}

/// Two instants of a clock that is the wall clock plus an offset are the same within a second or two.
fn near(a: chrono::DateTime<chrono::Utc>, b: chrono::DateTime<chrono::Utc>) {
    assert!(
        (a - b).num_milliseconds().abs() < 2_000,
        "{a} is not near {b}"
    );
}

/// A child that parks on a timer of `minutes` minutes, and does so again at each wake.
fn child_parked_on_a_timer(name: &str, minutes: i64) -> FnAgent {
    fn_agent(
        name,
        step_fn(move |ctx, state| {
            async move {
                Ok(Transition::Park {
                    state,
                    wake_at: Some(ctx.now() + chrono::Duration::minutes(minutes)),
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

// ---------------------------------------------------------------------------
// Continuing another run
// ---------------------------------------------------------------------------

/// A state that remembers what it continued, so a continuation is visible in the stored run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Chain {
    text: String,
    /// The texts of the runs before this one, oldest first.
    before: Vec<String>,
}

fn said(input: &Inbound) -> String {
    input.payload["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn say(text: &str) -> Inbound {
    Inbound::new("message", json!({ "text": text }))
}

fn chain_start(input: &Inbound) -> Chain {
    Chain {
        text: said(input),
        before: Vec::new(),
    }
}

fn chain_continued(input: &Inbound, prior: &Chain) -> Chain {
    let mut before = prior.before.clone();
    before.push(prior.text.clone());
    Chain {
        text: said(input),
        before,
    }
}

fn chain_of(view: &RunView) -> Chain {
    serde_json::from_value(view.state.clone()).expect("the run holds a Chain")
}

/// An agent that finishes at its first step and carries `before` over a continuation.
struct ChainAgent(String);

#[async_trait]
impl Agent for ChainAgent {
    type State = Chain;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Chain, AgentError> {
        Ok(chain_start(&input))
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Chain,
        _prior_run: RunId,
    ) -> Result<Chain, AgentError> {
        Ok(chain_continued(&input, prior))
    }

    async fn step(&self, _ctx: &mut Ctx, state: Chain) -> Result<Transition<Chain>, AgentError> {
        Ok(Transition::Done {
            output: json!({"text": state.text}),
            state,
        })
    }
}

/// A starter that refuses every input, continuing or not.
struct RefusingStarter(String);

impl AgentStarter for RefusingStarter {
    type State = Chain;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, _input: Inbound) -> Result<Chain, AgentError> {
        Err(AgentError::permanent("unusable start message: no"))
    }

    fn init_continuing(
        &self,
        _input: Inbound,
        _prior: &Chain,
        _prior_run: RunId,
    ) -> Result<Chain, AgentError> {
        Err(AgentError::permanent("unusable start message: no"))
    }
}

/// The start-only half of [`ChainAgent`], what an A2A front registers.
struct ChainStarter(String);

impl AgentStarter for ChainStarter {
    type State = Chain;

    fn name(&self) -> &str {
        &self.0
    }

    fn init(&self, input: Inbound) -> Result<Chain, AgentError> {
        Ok(chain_start(&input))
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Chain,
        _prior_run: RunId,
    ) -> Result<Chain, AgentError> {
        Ok(chain_continued(&input, prior))
    }
}

/// The state of a session round: what it said, what the rounds before it said, and the messages
/// that were steered into it while it worked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Round {
    text: String,
    before: Vec<String>,
    steered: Vec<String>,
}

/// A session round's agent. It carries `before` over a continuation like [`ChainAgent`]; when
/// `waits` it parks until a message arrives (a steer) and finishes with it, otherwise it finishes
/// at its first step.
struct RoundAgent {
    name: String,
    waits: bool,
}

#[async_trait]
impl Agent for RoundAgent {
    type State = Round;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Round, AgentError> {
        Ok(Round {
            text: said(&input),
            before: Vec::new(),
            steered: Vec::new(),
        })
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Round,
        _prior_run: RunId,
    ) -> Result<Round, AgentError> {
        let mut before = prior.before.clone();
        before.push(prior.text.clone());
        Ok(Round {
            text: said(&input),
            before,
            steered: Vec::new(),
        })
    }

    async fn step(&self, ctx: &mut Ctx, mut state: Round) -> Result<Transition<Round>, AgentError> {
        if self.waits {
            let inbox = ctx.take_inbox();
            if inbox.is_empty() {
                return Ok(Transition::Park {
                    state,
                    wake_at: None,
                });
            }
            state.steered.extend(inbox.iter().map(said));
        }
        Ok(Transition::Done {
            output: json!({"text": state.text}),
            state,
        })
    }
}

/// A session round that parks on a 60 s timer while `park` is set (a round waiting for a child of
/// its own, as an `LlmAgent` does) and finishes at its first step once it is not.
struct TimedRound {
    name: String,
    park: Arc<AtomicBool>,
}

#[async_trait]
impl Agent for TimedRound {
    type State = Round;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Round, AgentError> {
        Ok(Round {
            text: said(&input),
            before: Vec::new(),
            steered: Vec::new(),
        })
    }

    fn init_continuing(
        &self,
        input: Inbound,
        prior: &Round,
        _prior_run: RunId,
    ) -> Result<Round, AgentError> {
        let mut before = prior.before.clone();
        before.push(prior.text.clone());
        Ok(Round {
            text: said(&input),
            before,
            steered: Vec::new(),
        })
    }

    async fn step(&self, ctx: &mut Ctx, state: Round) -> Result<Transition<Round>, AgentError> {
        if self.park.load(SeqCst) {
            return Ok(Transition::Park {
                state,
                wake_at: Some(ctx.now() + WAIT),
            });
        }
        Ok(Transition::Done {
            output: json!({"text": state.text}),
            state,
        })
    }
}

fn round_of(view: &RunView) -> Round {
    serde_json::from_value(view.state.clone()).expect("the run holds a Round")
}

/// A parent that records every message it is sent and finishes at the first one.
fn collecting_parent(name: &str, seen: &Arc<Mutex<Vec<Inbound>>>) -> FnAgent {
    let seen = seen.clone();
    fn_agent(
        name,
        step_fn(move |ctx, state| {
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
        }),
    )
}

/// The documented shape of a session conversation id, held beyond the doctest.
#[test]
fn the_session_conversation_id_has_the_documented_shape() {
    let session = RunId::new();
    for parent in ["alice:ctx1", "a/s/b", "100%", "/", ""] {
        let id = session_conversation_id(parent, session);
        assert!(id.ends_with(&format!("/s/{session}")), "{id}");
        assert_eq!(
            parse_session_conversation_id(&id),
            Some((parent.to_owned(), session))
        );
        assert!(id.starts_with(&adam_runtime::session_conversation_prefix(parent)));
    }
    assert_eq!(
        session_conversation_id("alice:ctx1", session),
        format!("alice:ctx1/s/{session}")
    );
    assert_ne!(
        session_conversation_id("alice:ctx1", session),
        session_conversation_id("alice:ctx10", session)
    );
}

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

    /// One agent for the cases below: phase 0 holds at a gate (so `deliver` is delivered *during*
    /// the step), says what `Ctx::arrived` reported, and returns `Done`; phase 1 reads the inbox and
    /// returns `Done` with the texts it read (a child's notice has none). `reopen` is whether it
    /// asked to go on. It says it has read the message `READ_BEFORE`.
    async fn finishing_agent(
        store: &DynStore,
        name: &str,
        reopen: bool,
        deliver: &[Inbound],
    ) -> (Runtime, RunId, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let phase0_runs = Arc::new(AtomicUsize::new(0));
        let arrived = Arc::new(AtomicUsize::new(usize::MAX));
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let agent = fn_agent(
            name,
            step_fn({
                let (phase0_runs, arrived, started, gate) = (
                    phase0_runs.clone(),
                    arrived.clone(),
                    started.clone(),
                    gate.clone(),
                );
                move |ctx, state| {
                    let (phase0_runs, arrived, started, gate) = (
                        phase0_runs.clone(),
                        arrived.clone(),
                        started.clone(),
                        gate.clone(),
                    );
                    async move {
                        if phase(&state) == 0 {
                            phase0_runs.fetch_add(1, SeqCst);
                            if reopen {
                                ctx.reopen_on_arrival();
                            }
                            ctx.read_already(&[READ_BEFORE.to_owned()]);
                            started.notify_one();
                            notified(&gate, "the message to be delivered").await;
                            arrived.store(ctx.arrived().await.expect("arrived"), SeqCst);
                            return Ok(Transition::Done {
                                state: json!({"phase": 1}),
                                output: json!("first"),
                            });
                        }
                        let texts: Vec<Value> = ctx
                            .take_inbox()
                            .into_iter()
                            .filter(|m| m.kind != RUN_FINISHED_KIND)
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
        let rt = runtime(store, &agent);
        let run = rt.start(name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "step 0").await;
        for message in deliver {
            rt.deliver(run, message.clone()).await.expect("deliver");
        }
        gate.notify_one(); // only now may the step return and commit
        wait_done(&rt, run).await;
        worker.stop().await;
        (rt, run, phase0_runs, arrived)
    }

    /// An agent that asked to go on is not finished past a message that arrived while it stepped:
    /// the `Done` is committed as a step that goes on, the next transition reads the message, and
    /// only the `Done` after it ends the run. The first transition is not replayed, and
    /// `Ctx::arrived` said one message had come meanwhile.
    pub async fn a_done_that_asked_to_reopen_goes_on_when_a_message_arrived(store: DynStore) {
        let name = uniq("reopen");
        let (rt, run, phase0_runs, arrived) = finishing_agent(&store, &name, true, &[late()]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(
            view.output,
            Some(json!(["late"])),
            "the run finished after reading the message, not with the first answer"
        );
        assert_eq!(view.pending_inbox, 0, "nothing is left unread");
        assert_eq!(
            count(&phase0_runs),
            1,
            "the first transition is not replayed"
        );
        assert_eq!(arrived.load(SeqCst), 1);
    }

    /// Asking to go on costs nothing when nothing arrived: the `Done` ends the run as always.
    pub async fn a_done_that_asked_to_reopen_finishes_when_nothing_arrived(store: DynStore) {
        let name = uniq("reopen-quiet");
        let (rt, run, phase0_runs, arrived) = finishing_agent(&store, &name, true, &[]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!("first")));
        assert_eq!(count(&phase0_runs), 1);
        assert_eq!(arrived.load(SeqCst), 0);
    }

    /// A child's notice that lands while the run finishes is not news: the run read its children
    /// already (from the notice or the store), and the same notice a moment later, after the
    /// commit, is refused. `Ctx::arrived` does not count it and the commit does not reopen the run,
    /// so a parent that settled its wait at its timer does not take another turn (a model call)
    /// because the child's notice raced the timer.
    pub async fn a_childs_notice_that_lands_while_a_run_finishes_does_not_reopen_it(
        store: DynStore,
    ) {
        let name = uniq("reopen-notice");
        let (rt, run, phase0_runs, arrived) =
            finishing_agent(&store, &name, true, &[finished_notice()]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!("first")), "the first answer stands");
        assert_eq!(count(&phase0_runs), 1);
        assert_eq!(arrived.load(SeqCst), 0, "a notice is not an arrival");
        assert_eq!(view.pending_inbox, 1, "it stays in a run that is over");
        let after = rt.deliver(run, finished_notice()).await;
        assert!(
            matches!(after, Err(RuntimeError::Finished { .. })),
            "the same notice after the commit is refused: {after:?}"
        );
    }

    /// A notice beside a message does not hide it: the message counts and reopens the run, alone.
    pub async fn a_message_beside_a_childs_notice_still_reopens_the_run(store: DynStore) {
        let name = uniq("reopen-both");
        let (rt, run, phase0_runs, arrived) =
            finishing_agent(&store, &name, true, &[finished_notice(), late()]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!(["late"])));
        assert_eq!(count(&phase0_runs), 1);
        assert_eq!(arrived.load(SeqCst), 1, "only the message counts");
    }

    /// A copy of a message the run has read, delivered again while the run finishes, is not news:
    /// `Ctx::arrived` does not count it and the commit does not reopen the run, so the run does not
    /// take a turn (a model call) with nothing new to answer.
    pub async fn a_copy_of_a_message_the_run_read_does_not_reopen_it(store: DynStore) {
        let name = uniq("reopen-copy");
        let (rt, run, phase0_runs, arrived) =
            finishing_agent(&store, &name, true, &[copy_of_a_read_message()]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!("first")), "the first answer stands");
        assert_eq!(count(&phase0_runs), 1);
        assert_eq!(arrived.load(SeqCst), 0, "a copy is not an arrival");
        assert_eq!(view.pending_inbox, 1, "it stays in a run that is over");
    }

    /// A copy beside a new message does not hide it: the new one counts and reopens the run.
    pub async fn a_message_beside_a_copy_still_reopens_the_run(store: DynStore) {
        let name = uniq("reopen-copy-both");
        let (rt, run, phase0_runs, arrived) =
            finishing_agent(&store, &name, true, &[copy_of_a_read_message(), late()]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!(["copy", "late"])));
        assert_eq!(count(&phase0_runs), 1);
        assert_eq!(arrived.load(SeqCst), 1, "only the new message counts");
    }

    /// An agent that did not ask is committed exactly as before: it finishes, and the message that
    /// arrived meanwhile stays unread in a run that is over.
    pub async fn a_done_that_did_not_ask_finishes_past_a_message(store: DynStore) {
        let name = uniq("reopen-off");
        let (rt, run, _, _) = finishing_agent(&store, &name, false, &[late()]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!("first")));
        assert_eq!(view.pending_inbox, 1);
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
                &[],
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

    /// A header is one store call: the runtime asks the store for it and reads neither the lease
    /// nor the record beside it (an adapter answers it from a projection; the trait's default is
    /// two reads inside the store, not beside it). A view reads both, because its state is the
    /// record.
    pub async fn a_header_is_one_store_call(store: DynStore) {
        let (faulty, store) = faulty(store);
        let rt = Runtime::builder(store.clone()).build();
        let name = uniq("header");
        let run = store
            .create_run(NewRun::new(&name, json!({"turn": 0})))
            .await
            .unwrap();
        let before = (
            faulty.calls(Method::RunHeader),
            faulty.calls(Method::LoadRun),
            faulty.calls(Method::LeaseUntil),
        );
        let header = rt.header(run.id).await.expect("header").expect("the run");
        assert_eq!((header.id, header.version), (run.id, 1));
        assert_eq!(
            faulty.calls(Method::RunHeader),
            before.0 + 1,
            "one header read"
        );
        assert_eq!(
            faulty.calls(Method::LoadRun),
            before.1,
            "the runtime reads no record beside it"
        );
        assert_eq!(
            faulty.calls(Method::LeaseUntil),
            before.2,
            "nor a lease beside it"
        );
        assert!(rt.header(RunId::new()).await.expect("header").is_none());
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
    /// A worker that takes a run says so before it steps it, and `RunView::claimed` is true from
    /// then until the lease ends: nothing is committed while the first step is in the air, so the
    /// record alone cannot tell a run being stepped from one nobody has taken. Only the first claim
    /// is announced: after a commit the run is known to be in progress.
    pub async fn a_claimed_run_says_so_before_its_first_commit(store: DynStore) {
        let name = uniq("claimed");
        let (started, gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, gate) = (started.clone(), gate.clone());
                move |_ctx, state| {
                    let (started, gate) = (started.clone(), gate.clone());
                    async move {
                        if phase(&state) == 0 {
                            started.notify_one();
                            notified(&gate, "the test to look at the run").await;
                            return Ok(Transition::Continue(json!({"phase": 1})));
                        }
                        Ok(Transition::Done {
                            state,
                            output: json!("finished"),
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
        let before = rt.view(run).await.expect("view").expect("run");
        assert_eq!(
            (before.status, before.version, before.claimed),
            (RunStatus::Runnable, 1, false),
            "created, and nobody has taken it"
        );

        let worker = spawn_worker(&rt);
        notified(&started, "the first step").await;
        let during = rt.view(run).await.expect("view").expect("run");
        assert_eq!(
            (during.status, during.version, during.claimed),
            (RunStatus::Runnable, 1, true),
            "a worker holds it, and nothing is committed yet"
        );
        gate.notify_one();
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert!(!done.claimed, "a finished run is nobody's");

        let claimed = |e: &RunEvent| matches!(e, RunEvent::Status { status: RunStatus::Runnable, detail: Some(d) } if d == "claimed");
        let events = sink.events_for(run);
        assert_eq!(
            events.iter().filter(|e| claimed(e)).count(),
            1,
            "the first claim is announced once, not the claim of the second step: {events:?}"
        );
        assert!(
            events.get(1).is_some_and(claimed),
            "announced right after the start, before anything the step did: {events:?}"
        );
    }

    /// A lease that ran out (the worker died, or the step outlived it) is no claim: the run reads
    /// as unclaimed again until a worker takes it.
    pub async fn a_lease_that_ran_out_is_not_a_claim(store: DynStore) {
        let name = uniq("claim-lapse");
        let (started, gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, gate) = (started.clone(), gate.clone());
                move |_ctx, state| {
                    let (started, gate) = (started.clone(), gate.clone());
                    async move {
                        started.notify_one();
                        notified(&gate, "the clock to move past the lease").await;
                        Ok(Transition::Done {
                            state,
                            output: json!("late"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .lease_renewal(false)
            .clock(clock.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the step").await;
        assert!(rt.view(run).await.unwrap().unwrap().claimed);

        clock.advance(Duration::from_secs(11)); // the 10 s lease lapses under the step
        let lapsed = rt.view(run).await.unwrap().unwrap();
        assert_eq!(
            (lapsed.status, lapsed.version, lapsed.claimed),
            (RunStatus::Runnable, 1, false)
        );
        gate.notify_one();
        wait_done(&rt, run).await;
        worker.stop().await;
    }

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
                            id: None,
                            name: "report".into(),
                            mime_type: Some("text/markdown".into()),
                            data: json!("# hi"),
                            file: None,
                        })
                        .await;
                        ctx.emit(RunEvent::from(
                            Artifact::file("chart", "image/svg+xml", "chart.svg", SVG.to_vec())
                                .expect("a small file"),
                        ))
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
                RunEvent::Status {
                    status: RunStatus::Runnable,
                    detail: Some("claimed".into())
                },
                RunEvent::Progress {
                    message: "working".into()
                },
                RunEvent::Artifact {
                    id: None,
                    name: "report".into(),
                    mime_type: Some("text/markdown".into()),
                    data: json!("# hi"),
                    file: None,
                },
                RunEvent::from(
                    Artifact::file("chart", "image/svg+xml", "chart.svg", SVG.to_vec())
                        .expect("a small file")
                ),
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
        assert_eq!(view.artifacts.len(), 2);
        assert_eq!(view.artifacts[0].name, "report");
        assert_eq!(view.artifacts[0].data, json!("# hi"));
        assert_eq!(view.artifacts[0].file, None);
        // A file comes back from the store byte for byte, with its name and type.
        let chart = &view.artifacts[1];
        assert_eq!(chart.mime_type.as_deref(), Some("image/svg+xml"));
        let file = chart.file.as_ref().expect("the file");
        assert_eq!(
            (file.filename.as_str(), file.bytes.as_slice()),
            ("chart.svg", SVG)
        );
        // The step that made it also put its bytes under their digest (ADR 0055): a store that
        // keeps blobs answers them by id, and the digest is the one `blob_id` computes.
        if store.keeps_blobs() {
            let id = adam_runtime::blob_id(SVG);
            assert_eq!(
                store.get_blob(&id).await.expect("read the blob"),
                Some(SVG.to_vec()),
                "the commit referenced the file's bytes"
            );
        }
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

    /// A worker does not claim a run it is still stepping, even when the lease on it has lapsed.
    /// The claim would lease the run a second time to the worker that holds it: its snapshot is
    /// older than what the running step commits next (so a step could start on it, and run
    /// twice for one wake-up), and the release at the end of the first step would clear the new
    /// lease. Another worker may still take the run over once the lease has expired (see
    /// `renew_failure_lets_another_worker_take_over`).
    ///
    /// The step waits at a gate while the clock moves past its lease and whole claim passes
    /// follow; the store has handed the run out once, at the start, and the step ran once.
    pub async fn a_step_that_outlives_its_lease_is_not_claimed_again_by_its_worker(
        store: DynStore,
    ) {
        let (faults, store) = faulty(store);
        let name = uniq("lapsed");
        let (started, gate) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let steps = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let (started, gate, steps) = (started.clone(), gate.clone(), steps.clone());
                move |_ctx, state| {
                    let (started, gate, steps) = (started.clone(), gate.clone(), steps.clone());
                    async move {
                        steps.fetch_add(1, SeqCst);
                        started.notify_one();
                        notified(&gate, "the clock to move past the lease").await;
                        Ok(Transition::Done {
                            state,
                            output: json!("once"),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let probe = probe_agent();
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .agent(probe.clone())
            .lease_renewal(false)
            .clock(clock.clone())
            .build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the step").await;
        assert_eq!(faults.claimed(run), 1);

        clock.advance(Duration::from_secs(11)); // the 10 s lease lapses under the step
        // A worker has claimed a run that started after the clock moved, so a whole claim pass
        // has seen the lapsed lease; twice, so that a claim that returned the run and then
        // handed it back would have come round again.
        claim_pass(&rt, &probe).await;
        claim_pass(&rt, &probe).await;
        assert_eq!(faults.claimed(run), 1, "the store handed the run out again");

        gate.notify_one();
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!("once")));
        assert_eq!(count(&steps), 1, "the step ran once");
        assert_eq!(faults.claimed(run), 1);
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
                // The signal is what tells `a`, not its own watch: keep the watch's fallback
                // slower than the assertion's timeout, or it would pass without the notifier.
                .cancel_poll_interval(Duration::from_secs(30))
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

    // -----------------------------------------------------------------------
    // The hook a cancel runs (`Agent::on_cancelled`)
    // -----------------------------------------------------------------------

    /// An agent that parks with a state worth telling the hook about.
    fn parks_with_a_task(name: &str) -> FnAgent {
        fn_agent(
            name,
            step_fn(|_ctx, _state| {
                async move {
                    Ok(Transition::Park {
                        state: json!({"task": "t-1"}),
                        wake_at: None,
                    })
                }
                .boxed()
            }),
        )
    }

    /// A cancel runs the hook once, with the state the run last committed, and asking again, or
    /// for a run that has failed, runs nothing more.
    pub async fn a_cancel_runs_the_agents_hook_once_with_its_last_state(store: DynStore) {
        let name = uniq("hook");
        let agent = parks_with_a_task(&name);
        let rt = builder(&store, &uniq("w"), &agent).build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, run).await;
        rt.cancel(run, "stop").await.expect("cancel");
        wait_until("the hook", || agent.hook_calls().len() == 1).await;
        assert_eq!(agent.hook_calls(), vec![(run, json!({"task": "t-1"}))]);
        rt.cancel(run, "again").await.expect("cancel twice");
        // A negative check: the hook is spawned, so "never called again" can only be shown by
        // waiting a while; the first call was awaited above.
        tokio::time::sleep(Duration::from_millis(150)).await;
        worker.stop().await;
        assert_eq!(agent.hook_calls().len(), 1, "once per cancel commit");
        let state = store.load_run(run).await.unwrap().expect("the run").state;
        assert_eq!(
            super::common::v4::V4Envelope::decode(&state)
                .expect("a v4 envelope")
                .v,
            1
        );
    }

    /// A run that finished is not cancelled, so the hook does not run.
    pub async fn a_finished_run_runs_no_hook(store: DynStore) {
        let name = uniq("hook-done");
        let agent = child_done(&name, json!("ok"));
        let rt = builder(&store, &uniq("w"), &agent).build();
        let run = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        wait_done(&rt, run).await;
        rt.cancel(run, "too late").await.expect("cancel");
        // A negative check, as above: nothing to wait for, so a short wait.
        tokio::time::sleep(Duration::from_millis(150)).await;
        worker.stop().await;
        assert!(agent.hook_calls().is_empty());
    }

    /// A process that registered only the starter has no agent to call: it cancels, and the
    /// hook runs where the full agent is registered.
    pub async fn a_process_with_only_a_starter_runs_no_hook(store: DynStore) {
        let name = uniq("hook-front");
        let agent = parks_with_a_task(&name);
        let front = Runtime::builder(store.clone())
            .starter(JsonStarter(name.clone()))
            .worker_id(uniq("front"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let full = builder(&store, &uniq("w"), &agent).build();
        let (first, second) = (
            front.start(&name, inbound(), None).await.expect("start"),
            front.start(&name, inbound(), None).await.expect("start"),
        );
        let worker = spawn_worker(&full);
        wait_waiting(&full, first).await;
        wait_waiting(&full, second).await;
        front.cancel(first, "from the front").await.expect("cancel");
        // A negative check, as above; the second cancel below proves the hook path works.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(agent.hook_calls().is_empty(), "the front has no agent");
        full.cancel(second, "from the worker")
            .await
            .expect("cancel");
        wait_until("the hook", || agent.hook_calls().len() == 1).await;
        worker.stop().await;
        assert_eq!(agent.hook_calls()[0].0, second);
    }

    /// A child that follows a parent that was cancelled, or a time limit, is cancelled by the
    /// runtime and the hook runs for it too.
    pub async fn a_parent_cascade_and_a_deadline_run_the_hook(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let (started, saw) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
        let parent_agent = parent_of(&name, &child_name, &cell, &watch);
        let child_agent = child_listening(&child_name, &started, &saw);
        let clock = ManualClock::new();
        let rt = family(&store, &parent_agent, &child_agent, &cell, &clock);
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&started, "the child").await;
        wait_for(&rt, parent, "the parent parked", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;
        let limited = child_run_id(parent, "limited");
        rt.start_child_within(
            parent,
            limited,
            &child_name,
            inbound(),
            Some(Duration::from_secs(30)),
        )
        .await
        .expect("limited child");
        notified(&started, "the limited child").await;
        clock.advance(Duration::from_secs(31));
        wait_until("the limited child's hook", || {
            child_agent.hook_calls().iter().any(|(r, _)| *r == limited)
        })
        .await;

        rt.cancel(parent, "changed my mind").await.expect("cancel");
        let child = child_run_id(parent, "call-1");
        wait_until("the child's hook", || {
            child_agent.hook_calls().iter().any(|(r, _)| *r == child)
        })
        .await;
        worker.stop().await;
    }

    /// Cancelling a parent that waits cancels the child it waits for (ADR 0044 slice 10): the child
    /// is running, its worker sees the parent has ended within one poll and fails it, and what the
    /// child would have answered is not kept.
    pub async fn cancelling_a_waiting_parent_cancels_its_child(store: DynStore) {
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
        let ended = wait_failed(&rt, child).await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: the parent run ended")
        );

        // The child's step, still held at its gate, ends and its answer is dropped.
        release.notify_one();
        worker.stop().await;
        let child_after = rt.view(child).await.unwrap().unwrap();
        assert_eq!(child_after.status, RunStatus::Failed);
        assert_eq!(child_after.output, None, "the answer was not kept");
        let after = rt.view(parent).await.unwrap().unwrap();
        assert_eq!(after.status, RunStatus::Failed);
        assert_eq!(after.error.as_deref(), Some("cancelled: changed my mind"));
        assert_eq!(
            after.version, cancelled.version,
            "nothing touched the parent"
        );
        assert_eq!(count(&watch.resumes), 0);
    }

    /// A running child and the child it started are both told: each worker sees its parent has
    /// ended within one poll, fires the step's token and fails the run, the grandchild after the
    /// child.
    pub async fn a_running_child_and_its_child_are_told_within_one_poll(store: DynStore) {
        let (root_name, mid_name, leaf_name) = (uniq("root"), uniq("mid"), uniq("leaf"));
        let (cell, watch) = (RtCell::default(), Arc::new(Watch::default()));
        let (mid_started, leaf_started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (mid_saw, leaf_saw) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let root_agent = parent_of(&root_name, &mid_name, &cell, &watch);
        let mid_agent = child_with_child(&mid_name, &leaf_name, &cell, &mid_started, &mid_saw);
        let leaf_agent = child_listening(&leaf_name, &leaf_started, &leaf_saw);
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &root_agent)
            .agent(mid_agent)
            .agent(leaf_agent)
            .clock(clock.clone())
            .build();
        cell.set(rt.clone()).ok().expect("set once");
        let root = rt.start(&root_name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        notified(&mid_started, "the child").await;
        notified(&leaf_started, "the grandchild").await;
        let mid = child_run_id(root, "call-1");
        let leaf = child_run_id(mid, "call-1");
        assert_eq!(
            store.load_run(leaf).await.unwrap().unwrap().parent_id,
            Some(mid)
        );

        rt.cancel(root, "changed my mind").await.expect("cancel");
        for run in [mid, leaf] {
            let ended = wait_failed(&rt, run).await;
            assert_eq!(
                ended.error.as_deref(),
                Some("cancelled: the parent run ended")
            );
        }
        wait_until("both steps to see their token fire", || {
            mid_saw.load(SeqCst) && leaf_saw.load(SeqCst)
        })
        .await;
        worker.stop().await;
        assert_eq!(
            rt.view(root).await.unwrap().unwrap().error.as_deref(),
            Some("cancelled: changed my mind")
        );
    }

    /// A parent that ended makes no new child step: one started after the cancel is cancelled when
    /// it is claimed, before its step runs, and cancelling it again is a no-op, as is cancelling the
    /// parent twice.
    pub async fn a_child_created_after_the_cancel_never_steps(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let steps = Arc::new(AtomicUsize::new(0));
        let child_agent = {
            let steps = steps.clone();
            fn_agent(
                &child_name,
                step_fn(move |_ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        Ok(Transition::Done {
                            state,
                            output: json!("stepped"),
                        })
                    }
                    .boxed()
                }),
            )
        };
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_agent)
            .build();
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        rt.cancel(parent, "stop").await.expect("cancel");
        let child = child_run_id(parent, "late");
        assert!(
            rt.start_child(parent, child, &child_name, inbound())
                .await
                .expect("start child")
        );
        let worker = spawn_worker(&rt);
        let ended = wait_failed(&rt, child).await;
        worker.stop().await;
        assert_eq!(count(&steps), 0, "the child never stepped");
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: the parent run ended")
        );

        // Asking again changes nothing, for the parent and for the cancelled child.
        let parent_before = rt.view(parent).await.unwrap().unwrap();
        rt.cancel(parent, "again").await.expect("second cancel");
        rt.cancel(child, "again")
            .await
            .expect("cancel of a cancelled child");
        let (parent_after, child_after) = (
            rt.view(parent).await.unwrap().unwrap(),
            rt.view(child).await.unwrap().unwrap(),
        );
        assert_eq!(parent_after.version, parent_before.version);
        assert_eq!(parent_after.error.as_deref(), Some("cancelled: stop"));
        assert_eq!(child_after.version, ended.version);
        assert_eq!(
            child_after.error.as_deref(),
            Some("cancelled: the parent run ended")
        );
    }

    /// A parent that is `Done`, or still working, changes nothing for its child; one that failed for
    /// another reason than a cancel does; one that is not in the store is unknown and does not.
    pub async fn a_child_follows_a_parent_that_failed_but_not_one_that_is_done(store: DynStore) {
        let (name, child_name, failing_name) = (uniq("parent"), uniq("child"), uniq("failing"));
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_parked(&child_name))
            .agent(child_failing(&failing_name, "it broke"))
            .build();

        // A finished parent: the child goes on.
        let done_name = uniq("done");
        let rt_done = builder(&store, &uniq("w"), &child_done(&done_name, json!("ok")))
            .agent(child_parked(&child_name))
            .build();
        let done_parent = rt_done.start(&done_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt_done);
        wait_done(&rt_done, done_parent).await;
        let kept = child_run_id(done_parent, "kept");
        rt_done
            .start_child(done_parent, kept, &child_name, inbound())
            .await
            .unwrap();
        wait_waiting(&rt_done, kept).await;
        worker.stop().await;
        assert_eq!(
            rt_done.view(kept).await.unwrap().unwrap().status,
            RunStatus::Parked,
            "a Done parent does not end its child"
        );

        // A parent that failed on its own: the child follows.
        let failed_parent = rt.start(&failing_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_failed(&rt, failed_parent).await;
        let follows = child_run_id(failed_parent, "follows");
        rt.start_child(failed_parent, follows, &child_name, inbound())
            .await
            .unwrap();
        let ended = wait_failed(&rt, follows).await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: the parent run ended")
        );
        assert_eq!(
            rt.view(failed_parent)
                .await
                .unwrap()
                .unwrap()
                .error
                .as_deref(),
            Some("it broke")
        );

        // A parent that cannot be loaded is unknown, and cancels nothing.
        let orphan = RunId::new();
        rt.start_child(RunId::new(), orphan, &child_name, inbound())
            .await
            .unwrap();
        wait_waiting(&rt, orphan).await;
        worker.stop().await;
        assert_eq!(
            rt.view(orphan).await.unwrap().unwrap().status,
            RunStatus::Parked,
            "a parent that is not in the store is not a parent that ended"
        );
    }

    /// A cancelled parent fails the open round of a session, which is waiting on its timer, when it
    /// wakes; the session itself stays usable: a later parent run continues it from that round.
    pub async fn a_cancelled_parent_fails_the_open_round_and_the_session_continues(
        store: DynStore,
    ) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let park = Arc::new(AtomicBool::new(true));
        let clock = ManualClock::new();
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(TimedRound {
                name: agent.clone(),
                park: park.clone(),
            })
            .clock(clock.clone())
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let session = child_run_id(parent, "call-1");
        let conversation = session_conversation_id(&uniq("u:ctx"), session);
        rt.start_child_continuing(parent, session, &agent, say("one"), &conversation, None)
            .await
            .unwrap();
        wait_for(&rt, session, "the round parked on its timer", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;

        rt.cancel(parent, "changed my mind").await.unwrap();
        // The round is parked: it learns at its next wake.
        clock.advance(Duration::from_secs(61));
        let ended = wait_failed(&rt, session).await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: the parent run ended")
        );

        // The session is not closed: a new parent run continues it from the failed round.
        park.store(false, SeqCst);
        let next_parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let second = child_run_id(next_parent, "call-1");
        assert!(
            rt.start_child_continuing(
                next_parent,
                second,
                &agent,
                say("two"),
                &conversation,
                Some(session)
            )
            .await
            .unwrap()
        );
        let view = wait_done(&rt, second).await;
        worker.stop().await;
        assert_eq!(round_of(&view).text, "two");
        assert_eq!(round_of(&view).before, vec!["one"]);
    }

    /// A child started with a time limit is failed when the runtime's clock passes its deadline
    /// (`cancelled: time limit: ...`): a running one within one poll, its token fired, and a parked
    /// one, which would wait for a message that never comes, at the wake the runtime set at the
    /// deadline. A child with no limit parks with no wake.
    pub async fn a_child_past_its_deadline_fails_and_wakes_at_it(store: DynStore) {
        let (name, parked_name, running_name, free_name) = (
            uniq("parent"),
            uniq("parked"),
            uniq("running"),
            uniq("free"),
        );
        let (started, saw) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_parked_on_a_timer(&parked_name, 50))
            .agent(child_parked(&free_name))
            .agent(child_listening(&running_name, &started, &saw))
            .clock(clock.clone())
            .build();
        let parent = rt.start(&name, inbound(), None).await.expect("start");
        let worker = spawn_worker(&rt);
        let limit = Duration::from_secs(30 * 60);
        let deadline = clock.now() + chrono::Duration::minutes(30);

        let parked = child_run_id(parent, "parked");
        rt.start_child_within(parent, parked, &parked_name, inbound(), Some(limit))
            .await
            .expect("parked child");
        let view = wait_for(&rt, parked, "the child parked", |v| {
            v.status == RunStatus::Parked
        })
        .await;
        assert!(
            view.wake_at.unwrap() > deadline,
            "the view says what the agent asked for: 50 min"
        );
        let stored = store.load_run(parked).await.unwrap().unwrap();
        near(
            stored.wake_at.unwrap(),
            clock.now() + chrono::Duration::seconds(60),
        );
        assert!(
            deadline > stored.wake_at.unwrap(),
            "the runtime looks at it within the poll"
        );
        let free = child_run_id(parent, "free");
        rt.start_child(parent, free, &free_name, inbound())
            .await
            .expect("child with no limit");
        let view = wait_waiting(&rt, free).await;
        assert_eq!(view.wake_at, None, "no limit, no wake of the agent's");

        let running = child_run_id(parent, "running");
        rt.start_child_within(parent, running, &running_name, inbound(), Some(limit))
            .await
            .expect("running child");
        notified(&started, "the running child").await;

        clock.advance(Duration::from_secs(31 * 60));
        for run in [parked, running] {
            let ended = wait_failed(&rt, run).await;
            assert_eq!(
                ended.error.as_deref(),
                Some("cancelled: time limit: the run did not finish within 30 min")
            );
        }
        wait_until("the running child's token to fire", || saw.load(SeqCst)).await;
        worker.stop().await;
        assert_eq!(
            rt.view(free).await.unwrap().unwrap().status,
            RunStatus::Parked
        );
        // The children's ends are news for the parent, so it may be runnable again when the worker stops: what
        // holds is that no deadline ended it.
        let parent = rt.view(parent).await.unwrap().unwrap();
        assert!(
            matches!(parent.status, RunStatus::Parked | RunStatus::Runnable),
            "the parent has no deadline: {:?}",
            parent.status
        );
        assert_eq!(parent.error, None, "the parent has no deadline");
    }

    /// State written before deadlines existed has no `deadline` and runs as it did.
    pub async fn an_envelope_without_a_deadline_decodes(store: DynStore) {
        let name = uniq("old");
        let rt = runtime(&store, &child_done(&name, json!("ok")));
        let id = RunId::new();
        store
            .create_run(
                NewRun::new(
                    &name,
                    json!({
                        "v": 1, "agent": {}, "inbox": [], "seq": 0, "attempt": 0, "rev": 0,
                        "output": null, "error": null, "artifacts": []
                    }),
                )
                .with_id(id),
            )
            .await
            .expect("create");
        let worker = spawn_worker(&rt);
        let done = wait_done(&rt, id).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!("ok")));
    }

    /// A child that waits with no timer (it asked a question) is looked at by the runtime once a
    /// minute: when its parent has been cancelled it fails, without anyone answering.
    pub async fn a_child_parked_on_a_question_is_cancelled_when_its_parent_is(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_parked(&child_name))
            .clock(clock.clone())
            .build();
        let parent = rt.start(&name, inbound(), None).await.unwrap();
        let child = child_run_id(parent, "q");
        rt.start_child(parent, child, &child_name, inbound())
            .await
            .unwrap();
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, child).await;

        rt.cancel(parent, "changed my mind").await.unwrap();
        clock.advance(Duration::from_secs(61));
        let ended = wait_failed(&rt, child).await;
        worker.stop().await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: the parent run ended")
        );
    }

    /// The runs under a parked child are reached one level at a time, each at its next look.
    pub async fn a_grandchild_under_a_parked_child_follows(store: DynStore) {
        let (name, mid_name, leaf_name) = (uniq("root"), uniq("mid"), uniq("leaf"));
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_parked(&mid_name))
            .agent(child_parked(&leaf_name))
            .clock(clock.clone())
            .build();
        let root = rt.start(&name, inbound(), None).await.unwrap();
        let (mid, leaf) = (child_run_id(root, "m"), RunId::new());
        rt.start_child(root, mid, &mid_name, inbound())
            .await
            .unwrap();
        rt.start_child(mid, leaf, &leaf_name, inbound())
            .await
            .unwrap();
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, mid).await;
        wait_waiting(&rt, leaf).await;

        rt.cancel(root, "stop").await.unwrap();
        clock.advance(Duration::from_secs(61));
        wait_failed(&rt, mid).await;
        clock.advance(Duration::from_secs(61));
        let ended = wait_failed(&rt, leaf).await;
        worker.stop().await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: the parent run ended")
        );
    }

    /// The look at the parent costs the agent nothing: a healthy parked child is woken again and
    /// again, its agent is not stepped, nothing is journaled and no model would be called.
    pub async fn a_healthy_parked_child_is_woken_without_stepping_the_agent(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let steps = Arc::new(AtomicUsize::new(0));
        let child_agent = {
            let steps = steps.clone();
            fn_agent(
                &child_name,
                step_fn(move |ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        let _: Result<u32, String> = ctx.step("work", || async { Ok(1) }).await?;
                        Ok(Transition::Park {
                            state,
                            wake_at: None,
                        })
                    }
                    .boxed()
                }),
            )
        };
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_agent)
            .clock(clock.clone())
            .build();
        let parent = rt.start(&name, inbound(), None).await.unwrap();
        let child = child_run_id(parent, "c");
        rt.start_child(parent, child, &child_name, inbound())
            .await
            .unwrap();
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, child).await;
        assert_eq!(count(&steps), 1);
        let mut version = rt.view(child).await.unwrap().unwrap().version;

        for _ in 0..5 {
            clock.advance(Duration::from_secs(61));
            let seen = version;
            version = wait_for(&rt, child, "woken and parked again", |v| v.version > seen)
                .await
                .version;
        }
        worker.stop().await;
        assert_eq!(count(&steps), 1, "the agent was not stepped");
        assert!(store.journal_get(child, 0).await.unwrap().is_some());
        assert!(
            store.journal_get(child, 1).await.unwrap().is_none(),
            "and nothing was journaled"
        );
        assert_eq!(
            rt.view(child).await.unwrap().unwrap().status,
            RunStatus::Parked
        );
    }

    /// P starts a session round R and finishes `Done`; retention purges P; R still works and
    /// finishes: a parent that is not in the store is unknown, not ended.
    pub async fn a_done_parent_that_was_purged_leaves_its_open_child_running(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let clock = ManualClock::new();
        let rt = Runtime::builder(store.clone())
            .agent(child_done(&parent_name, json!("done")))
            .agent(RoundAgent {
                name: agent.clone(),
                waits: true,
            })
            .clock(clock.clone())
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_done(&rt, parent).await;
        let round = child_run_id(parent, "call-1");
        let conversation = session_conversation_id(&uniq("u:ctx"), round);
        rt.start_child_continuing(parent, round, &agent, say("one"), &conversation, None)
            .await
            .unwrap();
        wait_waiting(&rt, round).await;

        let purged = store
            .purge_finished(
                &parent_name,
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .await
            .unwrap();
        // A concurrent sweeper may have taken a row this call then skips (`SKIP LOCKED`), so the
        // count is not exact; the parent being gone is what this case needs.
        assert!(purged >= 1, "purged {purged}");
        assert!(store.load_run(parent).await.unwrap().is_none());
        // Several looks at the missing parent: the round is not cancelled.
        for _ in 0..3 {
            // The version is read before the clock moves: read after, a look that had already
            // happened would be the one waited for, and no other comes until the clock moves again.
            let seen = rt.view(round).await.unwrap().unwrap().version;
            clock.advance(Duration::from_secs(61));
            wait_for(&rt, round, "looked at", |v| v.version > seen).await;
        }
        assert_eq!(
            rt.view(round).await.unwrap().unwrap().status,
            RunStatus::Parked
        );
        rt.deliver(round, say("steer")).await.unwrap();
        let done = wait_done(&rt, round).await;
        worker.stop().await;
        assert_eq!(done.output, Some(json!({"text": "one"})));
    }

    /// The limit is for runaway work: the time a child waits for a person is not counted. A child
    /// with 30 minutes that waits 2 hours for an answer is not failed, and has what was left (all of
    /// it) when it works again.
    pub async fn a_child_waiting_for_a_person_keeps_its_time_for_when_it_works(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let (started, saw) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
        let child_agent = {
            let (started, saw) = (started.clone(), saw.clone());
            fn_agent(
                &child_name,
                step_fn(move |ctx, state| {
                    let (started, saw) = (started.clone(), saw.clone());
                    async move {
                        if ctx.take_inbox().is_empty() {
                            return Ok(Transition::Park {
                                state,
                                wake_at: None,
                            });
                        }
                        started.notify_one();
                        ctx.cancelled().await;
                        saw.store(true, SeqCst);
                        Err(AgentError::permanent("stopped"))
                    }
                    .boxed()
                }),
            )
        };
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_agent)
            .clock(clock.clone())
            .build();
        let parent = rt.start(&name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let child = child_run_id(parent, "q");
        rt.start_child_within(
            parent,
            child,
            &child_name,
            inbound(),
            Some(Duration::from_secs(30 * 60)),
        )
        .await
        .unwrap();
        wait_waiting(&rt, child).await;

        // Two hours of waiting for the answer.
        for _ in 0..2 {
            // The version is read before the clock moves, so the look it causes is the one waited for.
            let seen = rt.view(child).await.unwrap().unwrap().version;
            clock.advance(Duration::from_secs(60 * 60));
            wait_for(&rt, child, "looked at", |v| v.version > seen).await;
        }
        assert_eq!(
            rt.view(child).await.unwrap().unwrap().status,
            RunStatus::Parked,
            "waiting for a person is not time against the limit"
        );

        // The answer comes: the child works, and has its 30 minutes from now.
        rt.deliver(child, say("the answer")).await.unwrap();
        notified(&started, "the child to work").await;
        clock.advance(Duration::from_secs(20 * 60));
        let seen = rt.view(child).await.unwrap().unwrap();
        assert_eq!(
            seen.status,
            RunStatus::Runnable,
            "twenty minutes of work is within the limit"
        );
        assert!(!saw.load(SeqCst));
        clock.advance(Duration::from_secs(11 * 60));
        let ended = wait_failed(&rt, child).await;
        worker.stop().await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: time limit: the run did not finish within 30 min")
        );
        assert!(saw.load(SeqCst));
    }

    /// A wake that comes before the deadline (the clock of the store ahead of this one) parks the
    /// child again, to the deadline plus a second, and does not step the agent or loop.
    pub async fn a_wake_before_the_deadline_parks_again_without_stepping(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let steps = Arc::new(AtomicUsize::new(0));
        let child_agent = {
            let steps = steps.clone();
            fn_agent(
                &child_name,
                step_fn(move |ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        Ok(Transition::Park {
                            state,
                            wake_at: Some(ctx.now() + chrono::Duration::minutes(50)),
                        })
                    }
                    .boxed()
                }),
            )
        };
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_agent)
            .clock(clock.clone())
            .build();
        let parent = rt.start(&name, inbound(), None).await.unwrap();
        let child = child_run_id(parent, "c");
        let start = clock.now();
        rt.start_child_within(
            parent,
            child,
            &child_name,
            inbound(),
            Some(Duration::from_secs(1800)),
        )
        .await
        .unwrap();
        let worker = spawn_worker(&rt);
        wait_for(&rt, child, "parked", |v| v.status == RunStatus::Parked).await;
        let deadline = start + chrono::Duration::seconds(1800);

        // Five seconds before the deadline a wake is due that the deadline did not set.
        clock.advance(Duration::from_secs(1795));
        // The worker may look at the parked child at the same moment (the clock just moved): the
        // write is tried again on the version it left, and the count starts from that version.
        let version = loop {
            let rec = store.load_run(child).await.unwrap().unwrap();
            if rec.status != RunStatus::Parked {
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
            let mut update = adam_core::RunUpdate::new(RunStatus::Parked, rec.state.clone());
            update.wake_at = Some(clock.now());
            match store.commit_run(child, rec.version, update).await {
                Ok(_) => break rec.version,
                Err(adam_core::StoreError::Conflict { .. }) => {}
                Err(err) => panic!("the wake was not written: {err}"),
            }
        };
        let parked = wait_for(&rt, child, "parked again", |v| v.version >= version + 2).await;
        assert_eq!(parked.status, RunStatus::Parked);
        let rec = store.load_run(child).await.unwrap().unwrap();
        near(
            rec.wake_at.unwrap(),
            deadline + chrono::Duration::seconds(1),
        );
        assert_eq!(count(&steps), 1, "the agent was not stepped");
        assert_eq!(rec.version, version + 2, "and there was no loop");
        assert_eq!(
            super::common::v4::V4Envelope::decode(&rec.state)
                .expect("a v4 envelope")
                .v,
            1
        );

        clock.advance(Duration::from_secs(7));
        let ended = wait_failed(&rt, child).await;
        worker.stop().await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: time limit: the run did not finish within 30 min")
        );
    }

    /// A child that retries with a wake an hour away and has 30 minutes is failed at 30 minutes:
    /// the wake of a runnable run is clamped to the deadline too.
    pub async fn a_runnable_wake_past_the_deadline_is_clamped_to_it(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let child_agent = fn_agent(
            &child_name,
            step_fn(|_ctx, _state| {
                async move {
                    Err(AgentError::transient_after(
                        "later",
                        Duration::from_secs(3600),
                    ))
                }
                .boxed()
            }),
        );
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_agent)
            .clock(clock.clone())
            .build();
        let parent = rt.start(&name, inbound(), None).await.unwrap();
        let child = child_run_id(parent, "c");
        let start = clock.now();
        rt.start_child_within(
            parent,
            child,
            &child_name,
            inbound(),
            Some(Duration::from_secs(1800)),
        )
        .await
        .unwrap();
        let worker = spawn_worker(&rt);
        let retrying = wait_for(&rt, child, "retry scheduled", |v| v.attempt == 1).await;
        near(
            retrying.wake_at.unwrap(),
            start + chrono::Duration::seconds(1800),
        );
        clock.advance(Duration::from_secs(31 * 60));
        let ended = wait_failed(&rt, child).await;
        worker.stop().await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: time limit: the run did not finish within 30 min")
        );
    }

    /// The deadline is fixed when the child is made: a child that parks on a 20 minute timer again
    /// and again still fails at 30 minutes, at its second wake.
    pub async fn a_deadline_is_set_once_and_not_moved_by_later_commits(store: DynStore) {
        let (name, child_name) = (uniq("parent"), uniq("child"));
        let steps = Arc::new(AtomicUsize::new(0));
        let child_agent = {
            let steps = steps.clone();
            fn_agent(
                &child_name,
                step_fn(move |ctx, state| {
                    let steps = steps.clone();
                    async move {
                        steps.fetch_add(1, SeqCst);
                        Ok(Transition::Park {
                            state,
                            wake_at: Some(ctx.now() + chrono::Duration::minutes(20)),
                        })
                    }
                    .boxed()
                }),
            )
        };
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &child_parked(&name))
            .agent(child_agent)
            .clock(clock.clone())
            .build();
        let parent = rt.start(&name, inbound(), None).await.unwrap();
        let child = child_run_id(parent, "c");
        rt.start_child_within(
            parent,
            child,
            &child_name,
            inbound(),
            Some(Duration::from_secs(1800)),
        )
        .await
        .unwrap();
        let worker = spawn_worker(&rt);
        wait_for(&rt, child, "first park", |v| v.status == RunStatus::Parked).await;
        clock.advance(Duration::from_secs(21 * 60));
        wait_for(&rt, child, "second park", |v| {
            count(&steps) == 2 && v.status == RunStatus::Parked
        })
        .await;
        // 31 minutes in: past the first deadline, not past one recomputed at the second commit.
        clock.advance(Duration::from_secs(10 * 60));
        let ended = wait_failed(&rt, child).await;
        worker.stop().await;
        assert_eq!(
            count(&steps),
            2,
            "the third wake failed the run, it did not step"
        );
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: time limit: the run did not finish within 30 min")
        );
    }

    /// A session round takes the time limit of `start_child_continuing_within`, and the session stays
    /// usable: the next round continues from the failed one.
    pub async fn a_round_with_a_limit_fails_past_it(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let park = Arc::new(AtomicBool::new(true));
        let clock = ManualClock::new();
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(TimedRound {
                name: agent.clone(),
                park: park.clone(),
            })
            .clock(clock.clone())
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let session = child_run_id(parent, "call-1");
        let conversation = session_conversation_id(&uniq("u:ctx"), session);
        rt.start_child_continuing_within(
            parent,
            session,
            &agent,
            say("one"),
            &conversation,
            None,
            Some(Duration::from_secs(600)),
        )
        .await
        .unwrap();
        wait_for(&rt, session, "parked", |v| v.status == RunStatus::Parked).await;
        clock.advance(Duration::from_secs(601));
        let ended = wait_failed(&rt, session).await;
        assert_eq!(
            ended.error.as_deref(),
            Some("cancelled: time limit: the run did not finish within 10 min")
        );
        park.store(false, SeqCst);
        let second = child_run_id(parent, "call-2");
        rt.start_child_continuing(
            parent,
            second,
            &agent,
            say("two"),
            &conversation,
            Some(session),
        )
        .await
        .unwrap();
        let view = wait_done(&rt, second).await;
        worker.stop().await;
        assert_eq!(round_of(&view).before, vec!["one"]);
    }

    /// The cancel watch of a running child reads its parent about once a second, not on every poll.
    pub async fn the_watch_reads_the_parent_about_once_a_second(store: DynStore) {
        let (faulty, dynamic) = faulty(store);
        let (name, child_name, alone_name) = (uniq("parent"), uniq("child"), uniq("alone"));
        let (started, saw) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
        let (alone_started, alone_saw) =
            (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)));
        let rt = builder(&dynamic, &uniq("w"), &child_parked(&name))
            .agent(child_listening(&child_name, &started, &saw))
            .agent(child_listening(&alone_name, &alone_started, &alone_saw))
            .poll_interval(Duration::from_millis(10))
            // The watch's own interval, so the window has polls to count (the production default
            // would make the window one read).
            .cancel_poll_interval(Duration::from_millis(10))
            .build();
        let parent = rt.start(&name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, parent).await;

        // The same running step, with a parent and with none: the reads that differ are the parent's.
        let window = Duration::from_millis(2500);
        let mut reads = Vec::new();
        for with_parent in [false, true] {
            let id = RunId::new();
            if with_parent {
                rt.start_child(parent, id, &child_name, inbound())
                    .await
                    .unwrap();
                notified(&started, "the child").await;
            } else {
                rt.start_with_id(id, &alone_name, inbound(), None)
                    .await
                    .unwrap();
                notified(&alone_started, "the run").await;
            }
            let before = faulty.calls(Method::RunHeader);
            tokio::time::sleep(window).await;
            reads.push(faulty.calls(Method::RunHeader) - before);
            rt.cancel(id, "done measuring").await.unwrap();
        }
        worker.stop().await;
        let extra = reads[1].saturating_sub(reads[0]);
        // Without the limit every poll of the window would read the parent too, which doubles the reads; with it
        // about 3 do. The two windows are measured one after the other, and on a real database the number of
        // polls in a window varies by a few percent, so the bound is a share of the polls and not a count.
        assert!(
            extra <= reads[0] / 4,
            "the parent was read {extra} times more than a run with none: {reads:?}"
        );
    }

    /// The cancel watch of a step reads the run's header on its own interval: a 2.5 s step at the
    /// 1 s default is read about twice, not once per worker poll (the 20 ms idle poll would be
    /// ~125 reads). It never reads the state: the header is a projection.
    pub async fn the_cancel_watch_polls_at_its_own_interval(store: DynStore) {
        let (faulty, dynamic) = faulty(store);
        let name = uniq("watch-poll");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, state| {
                async move {
                    tokio::time::sleep(Duration::from_millis(2500)).await;
                    Ok(Transition::Done {
                        state,
                        output: json!("done"),
                    })
                }
                .boxed()
            }),
        );
        let rt = builder(&dynamic, &uniq("w"), &agent)
            .cancel_poll_interval(Duration::from_secs(1))
            .build();
        let worker = spawn_worker(&rt);
        let run = rt.start(&name, inbound(), None).await.expect("start");
        wait_done(&rt, run).await;
        worker.stop().await;
        let reads = faulty.calls(Method::RunHeader);
        assert!(
            (2..=5).contains(&reads),
            "the watch read the header {reads} times over a 2.5 s step at a 1 s interval"
        );
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

    /// A new run continues a finished one from a front that only holds the starter: the state comes
    /// from the prior run's last commit, the run is an ordinary new run, a busy conversation is
    /// refused as for any start, and asking again is idempotent even when the prior run has been
    /// purged meanwhile.
    pub async fn a_run_continues_a_finished_run_from_a_start_only_front(store: DynStore) {
        let name = uniq("chain");
        let conversation = uniq("conv");
        let back = Runtime::builder(store.clone())
            .agent(ChainAgent(name.clone()))
            .worker_id(uniq("back"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let front = Runtime::builder(store.clone())
            .starter(ChainStarter(name.clone()))
            .worker_id(uniq("front"))
            .build();

        let first = RunId::new();
        assert!(
            front
                .start_with_id(first, &name, say("one"), Some(&conversation))
                .await
                .unwrap()
        );
        let worker = spawn_worker(&back);
        wait_done(&back, first).await;
        // Stop the worker before `second` exists: `ChainAgent` finishes in one step, so a worker
        // that is still polling can claim and finish `second` between the insert below and the
        // read of its status, and the `Runnable` assertion would then see `Done`.
        worker.stop().await;

        let second = RunId::new();
        assert!(
            front
                .start_with_id_continuing(second, &name, say("two"), Some(&conversation), first)
                .await
                .unwrap()
        );
        let view = front.view(second).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Runnable);
        assert_eq!(view.conversation_id.as_deref(), Some(conversation.as_str()));
        assert_eq!(
            chain_of(&view),
            Chain {
                text: "two".into(),
                before: vec!["one".into()]
            }
        );
        // A worker steps it like any run; the prior run is untouched.
        let worker = spawn_worker(&back);
        wait_done(&back, second).await;
        assert_eq!(
            chain_of(&back.view(first).await.unwrap().unwrap()).text,
            "one"
        );
        worker.stop().await;

        // A third continues the second: the chain grows.
        let third = RunId::new();
        assert!(
            front
                .start_with_id_continuing(third, &name, say("three"), Some(&conversation), second)
                .await
                .unwrap()
        );
        let expected = Chain {
            text: "three".into(),
            before: vec!["one".into(), "two".into()],
        };
        assert_eq!(
            chain_of(&front.view(third).await.unwrap().unwrap()),
            expected
        );

        // One open run per conversation, as for any start: a fourth, while the third is open,
        // is refused and the third is untouched.
        let fourth = RunId::new();
        let busy = front
            .start_with_id_continuing(fourth, &name, say("four"), Some(&conversation), second)
            .await;
        assert!(
            matches!(busy, Err(RuntimeError::ConversationBusy { .. })),
            "{busy:?}"
        );
        assert!(front.view(fourth).await.unwrap().is_none());

        // Asking again is idempotent, also once the runs it came from are gone (a retry after the
        // prior was purged); with a different id and a prior that is gone, it is not found.
        let fifth = RunId::new();
        assert!(
            front
                .start_with_id_continuing(fifth, &name, say("five"), None, second)
                .await
                .unwrap()
        );
        assert!(
            !front
                .start_with_id_continuing(fifth, &name, say("five"), None, second)
                .await
                .unwrap()
        );
        let purged = store
            .purge_finished(&name, chrono::Utc::now() + chrono::Duration::hours(1))
            .await
            .expect("purge");
        assert!(purged >= 2, "purged {purged}");
        assert!(store.load_run(second).await.unwrap().is_none());
        assert!(
            !front
                .start_with_id_continuing(fifth, &name, say("five"), None, second)
                .await
                .unwrap()
        );
        assert_eq!(
            chain_of(&front.view(fifth).await.unwrap().unwrap()),
            Chain {
                text: "five".into(),
                before: vec!["one".into(), "two".into()]
            }
        );
        let gone = front
            .start_with_id_continuing(RunId::new(), &name, say("x"), None, second)
            .await;
        assert!(
            matches!(gone, Err(RuntimeError::NotFound(id)) if id == second),
            "{gone:?}"
        );
    }

    /// The runtime refuses what it can check and leaves no run behind: a prior that does not
    /// exist, a prior of another agent, an input the agent refuses.
    pub async fn continuing_checks_the_prior_run_and_creates_nothing_when_it_refuses(
        store: DynStore,
    ) {
        let (name, other) = (uniq("chain"), uniq("other"));
        let rt = Runtime::builder(store.clone())
            .starter(ChainStarter(name.clone()))
            .starter(JsonStarter(other.clone()))
            .worker_id(uniq("front"))
            .build();
        let foreign = RunId::new();
        assert!(
            rt.start_with_id(foreign, &other, inbound(), None)
                .await
                .unwrap()
        );

        let ghost = RunId::new();
        let id = RunId::new();
        let missing = rt
            .start_with_id_continuing(id, &name, say("x"), None, ghost)
            .await;
        assert!(
            matches!(missing, Err(RuntimeError::NotFound(g)) if g == ghost),
            "{missing:?}"
        );
        let wrong_agent = rt
            .start_with_id_continuing(id, &name, say("x"), None, foreign)
            .await
            .expect_err("another agent's run is not this agent's to continue");
        assert_eq!(wrong_agent.class(), adam_runtime::ErrorClass::Invalid);
        assert!(!wrong_agent.is_retryable());
        assert!(
            matches!(&wrong_agent, RuntimeError::WrongAgent { run, agent } if *run == foreign && *agent == name),
            "{wrong_agent:?}"
        );
        let unknown_agent = rt
            .start_with_id_continuing(id, "nobody", say("x"), None, foreign)
            .await;
        assert!(matches!(unknown_agent, Err(RuntimeError::UnknownAgent(_))));
        assert!(
            store.load_run(id).await.unwrap().is_none(),
            "nothing was created"
        );
        assert!(matches!(
            rt.start_continuing(&name, say("x"), None, ghost).await,
            Err(RuntimeError::NotFound(g)) if g == ghost
        ));

        // The agent's own refusal of the input is the caller's error, not a fallback.
        let prior = RunId::new();
        assert!(
            rt.start_with_id(prior, &name, say("one"), None)
                .await
                .unwrap()
        );
        let refusing = Runtime::builder(store.clone())
            .starter(RefusingStarter(name.clone()))
            .worker_id(uniq("refusing"))
            .build();
        let refused = refusing
            .start_with_id_continuing(id, &name, say("x"), None, prior)
            .await;
        assert!(
            matches!(
                refused,
                Err(RuntimeError::Agent(AgentError::Permanent { .. }))
            ),
            "{refused:?}"
        );
        assert!(store.load_run(id).await.unwrap().is_none());

        // A repeat of a request that already started its run answers `false` before the prior is
        // read or `init_continuing` runs: a prior that is wrong, or gone, changes nothing.
        let started = RunId::new();
        assert!(
            rt.start_with_id_continuing(started, &name, say("two"), None, prior)
                .await
                .unwrap()
        );
        for other_prior in [foreign, ghost] {
            assert!(
                !rt.start_with_id_continuing(started, &name, say("two"), None, other_prior)
                    .await
                    .unwrap(),
                "a repeat is recognised before the prior is looked at"
            );
        }
        // Whereas a request that has not started anything is still refused for it.
        assert!(matches!(
            rt.start_with_id_continuing(RunId::new(), &name, say("x"), None, foreign)
                .await,
            Err(RuntimeError::WrongAgent { .. })
        ));
    }

    /// A prior state the agent cannot decode (another shape under the same name) does not stop a
    /// new run from starting: it starts as `init` says, and the run is fine.
    pub async fn a_prior_state_that_does_not_decode_starts_the_run_fresh(store: DynStore) {
        let name = uniq("chain");
        let old_shape = Runtime::builder(store.clone())
            .starter(JsonStarter(name.clone()))
            .worker_id(uniq("old"))
            .build();
        let prior = RunId::new();
        assert!(
            old_shape
                .start_with_id(prior, &name, Inbound::new("m", json!({"text": 7})), None)
                .await
                .unwrap()
        );

        let rt = Runtime::builder(store.clone())
            .starter(ChainStarter(name.clone()))
            .worker_id(uniq("front"))
            .build();
        let id = RunId::new();
        assert!(
            rt.start_with_id_continuing(id, &name, say("two"), None, prior)
                .await
                .unwrap()
        );
        assert_eq!(
            chain_of(&rt.view(id).await.unwrap().unwrap()),
            Chain {
                text: "two".into(),
                before: Vec::new()
            }
        );
    }

    /// An agent that does not override `init_continuing` gets what `init` makes of the input, so
    /// nothing changes for it, whatever the prior run held.
    pub async fn an_agent_without_an_override_starts_fresh_when_asked_to_continue(store: DynStore) {
        let name = uniq("plain");
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
        let prior = rt
            .start(&name, Inbound::new("m", json!({"k": 1})), None)
            .await
            .unwrap();
        let id = RunId::new();
        assert!(
            rt.start_with_id_continuing(id, &name, Inbound::new("m", json!({"k": 2})), None, prior)
                .await
                .unwrap()
        );
        assert_eq!(rt.view(id).await.unwrap().unwrap().state, json!({"k": 2}));
        let by_start = rt
            .start_continuing(&name, Inbound::new("m", json!({"k": 3})), None, prior)
            .await
            .unwrap();
        assert_eq!(
            rt.view(by_start).await.unwrap().unwrap().state,
            json!({"k": 3})
        );
    }

    /// `start_continuing` is `start` for a continuation: it delivers to the conversation's open
    /// run (and does not even read the prior), and creates a continuing run when none is open.
    pub async fn start_continuing_delivers_to_an_open_run_and_otherwise_continues(store: DynStore) {
        let name = uniq("chain");
        let conversation = uniq("conv");
        let rt = Runtime::builder(store.clone())
            .agent(ChainAgent(name.clone()))
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let open = rt
            .start(&name, say("one"), Some(&conversation))
            .await
            .unwrap();
        // The conversation is busy: the message goes to the open run; the prior is not needed.
        let delivered = rt
            .start_continuing(&name, say("more"), Some(&conversation), RunId::new())
            .await
            .unwrap();
        assert_eq!(delivered, open);
        assert_eq!(rt.view(open).await.unwrap().unwrap().pending_inbox, 1);

        rt.cancel(open, "done with it").await.unwrap();
        let next = rt
            .start_continuing(&name, say("two"), Some(&conversation), open)
            .await
            .unwrap();
        assert_ne!(next, open);
        assert_eq!(
            chain_of(&rt.view(next).await.unwrap().unwrap()),
            Chain {
                text: "two".into(),
                before: vec!["one".into()]
            }
        );
    }

    // -- child sessions (ADR 0045, slice 4) -------------------------------

    /// A session whose latest round ended and that nobody wrote to for the idle TTL reads closed,
    /// in a listing and in a single read, and its report is still there to read.
    pub async fn a_session_idle_past_its_ttl_reads_closed(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let clock = ManualClock::new();
        let ttl = Duration::from_secs(3600);
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(RoundAgent {
                name: agent.clone(),
                waits: false,
            })
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .clock(clock.clone())
            .session_idle_ttl(ttl)
            .build();
        assert_eq!(rt.session_idle_ttl(), ttl);
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let session = child_run_id(parent, "call-1");
        let ctx = uniq("u:ctx");
        rt.start_child_continuing(
            parent,
            session,
            &agent,
            say("one"),
            &session_conversation_id(&ctx, session),
            None,
        )
        .await
        .unwrap();
        wait_done(&rt, session).await;
        worker.stop().await;

        let read = |rt: &Runtime| {
            let (rt, ctx, agent) = (rt.clone(), ctx.clone(), agent.clone());
            async move {
                let listed = rt.list_sessions(&ctx, &[&agent], 0, 10).await.unwrap();
                let view = rt.session_view(&ctx, session).await.unwrap().unwrap();
                (listed.sessions[0].clone(), view)
            }
        };
        let (listed, view) = read(&rt).await;
        assert_eq!(listed.state, SessionState::Idle);
        assert!(!listed.closed && !listed.idle_expired);
        assert!(!view.closed && !view.idle_expired);

        clock.advance(ttl - Duration::from_secs(60));
        let (listed, view) = read(&rt).await;
        assert_eq!(listed.state, SessionState::Idle, "just inside the TTL");
        assert!(!view.idle_expired);

        clock.advance(Duration::from_secs(60));
        let (listed, view) = read(&rt).await;
        assert_eq!(listed.state, SessionState::Closed, "at the TTL");
        assert!(listed.closed && listed.idle_expired);
        assert_eq!(view.state, SessionState::Closed);
        assert!(view.closed && view.idle_expired);
        assert!(listed.report.ended, "the round's report stays readable");
        assert_eq!(view.latest.status, RunStatus::Done);
    }

    /// A round that works never expires, however long it has worked: the TTL is for idle sessions.
    pub async fn a_working_round_never_expires(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let clock = ManualClock::new();
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(RoundAgent {
                name: agent.clone(),
                waits: true,
            })
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .clock(clock.clone())
            .session_idle_ttl(Duration::from_secs(60))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let session = child_run_id(parent, "call-1");
        let ctx = uniq("u:ctx");
        rt.start_child_continuing(
            parent,
            session,
            &agent,
            say("one"),
            &session_conversation_id(&ctx, session),
            None,
        )
        .await
        .unwrap();
        wait_waiting(&rt, session).await;
        clock.advance(Duration::from_secs(30 * 24 * 3600));
        let listed = rt.list_sessions(&ctx, &[&agent], 0, 10).await.unwrap();
        assert_eq!(listed.sessions[0].state, SessionState::Working);
        assert!(!listed.sessions[0].closed && !listed.sessions[0].idle_expired);
        let view = rt.session_view(&ctx, session).await.unwrap().unwrap();
        assert!(!view.closed && !view.idle_expired);
        worker.stop().await;
    }

    // -- child sessions (ADR 0045, slice 1) -------------------------------

    /// Round 2 starts from round 1's conversation, round 3 from round 2's, and every round is a
    /// child of the parent with the session's conversation.
    pub async fn a_session_round_carries_the_previous_rounds_conversation(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(RoundAgent {
                name: agent.clone(),
                waits: false,
            })
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);

        let session = child_run_id(parent, "call-1");
        let conversation = session_conversation_id(&uniq("u:ctx"), session);
        assert!(
            rt.start_child_continuing(parent, session, &agent, say("one"), &conversation, None)
                .await
                .unwrap()
        );
        wait_done(&rt, session).await;
        let second = child_run_id(parent, "call-2");
        assert!(
            rt.start_child_continuing(
                parent,
                second,
                &agent,
                say("two"),
                &conversation,
                Some(session)
            )
            .await
            .unwrap()
        );
        wait_done(&rt, second).await;
        let third = child_run_id(parent, "call-3");
        rt.start_child_continuing(
            parent,
            third,
            &agent,
            say("three"),
            &conversation,
            Some(second),
        )
        .await
        .unwrap();
        let view = wait_done(&rt, third).await;
        worker.stop().await;

        let round = round_of(&view);
        assert_eq!(round.text, "three");
        assert_eq!(round.before, vec!["one", "two"]);
        for id in [session, second, third] {
            let rec = store.load_run(id).await.unwrap().unwrap();
            assert_eq!(rec.parent_id, Some(parent));
            assert_eq!(rec.conversation_id.as_deref(), Some(conversation.as_str()));
            assert_eq!(rec.agent, agent);
        }
        assert_eq!(
            round_of(&rt.view(second).await.unwrap().unwrap()).before,
            vec!["one"]
        );

        // A prior of another agent is refused, and nothing is created.
        let stranger = rt.start(&parent_name, inbound(), None).await.unwrap();
        let refused = RunId::new();
        let err = rt
            .start_child_continuing(
                parent,
                refused,
                &agent,
                say("x"),
                &conversation,
                Some(stranger),
            )
            .await;
        assert!(
            matches!(err, Err(RuntimeError::WrongAgent { .. })),
            "{err:?}"
        );
        assert!(store.load_run(refused).await.unwrap().is_none());
    }

    /// A second round cannot start while one is open: the error is `Rejected` and tells the caller
    /// apart (`ConversationBusy`), nothing is created, and `deliver` to the open round is read by it
    /// as a steered message. Once it ends the next round starts.
    pub async fn a_round_is_refused_while_one_is_open_and_deliver_steers(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(RoundAgent {
                name: agent.clone(),
                waits: true,
            })
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let session = child_run_id(parent, "call-1");
        let conversation = session_conversation_id(&uniq("u:ctx"), session);
        rt.start_child_continuing(parent, session, &agent, say("one"), &conversation, None)
            .await
            .unwrap();
        wait_waiting(&rt, session).await;

        let second = child_run_id(parent, "call-2");
        let err = rt
            .start_child_continuing(
                parent,
                second,
                &agent,
                say("two"),
                &conversation,
                Some(session),
            )
            .await
            .expect_err("the session has an open round");
        assert!(
            matches!(err, RuntimeError::ConversationBusy { .. }),
            "{err:?}"
        );
        assert_eq!(err.class(), adam_runtime::ErrorClass::Rejected);
        assert!(store.load_run(second).await.unwrap().is_none());

        rt.deliver(session, say("steer")).await.unwrap();
        let view = wait_done(&rt, session).await;
        assert_eq!(round_of(&view).steered, vec!["steer"]);

        assert!(
            rt.start_child_continuing(
                parent,
                second,
                &agent,
                say("two"),
                &conversation,
                Some(session)
            )
            .await
            .expect("the round ended, so the next one starts")
        );
        worker.stop().await;
    }

    /// Asking again with the same ids (the tool ran again after a crash before its journal write)
    /// finds the round it started: one row, the first input, and a prior that is wrong or gone
    /// changes nothing. It holds while the round is open too.
    pub async fn a_repeated_session_start_returns_the_same_round(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(RoundAgent {
                name: agent.clone(),
                waits: true,
            })
            .worker_id(uniq("w"))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let session = child_run_id(parent, "call-1");
        let parent_conversation = uniq("u:ctx");
        let conversation = session_conversation_id(&parent_conversation, session);

        assert!(
            rt.start_child_continuing(parent, session, &agent, say("first"), &conversation, None)
                .await
                .unwrap()
        );
        assert!(
            !rt.start_child_continuing(parent, session, &agent, say("second"), &conversation, None)
                .await
                .expect("a repeat is not an error, open round or not"),
        );
        let view = rt.view(session).await.unwrap().unwrap();
        assert_eq!(round_of(&view).text, "first");
        let listed = rt
            .list_sessions(&parent_conversation, &[&agent], 0, 10)
            .await
            .unwrap();
        assert_eq!(listed.total, 1, "one session, one round");
        assert_eq!(listed.sessions[0].round, 1);
    }

    /// The parent hears of the end of a round as `adam.child.reported`, and of a one-shot child as
    /// `adam.run.finished` exactly as before: the kind follows the conversation, the id and the
    /// payload are the same.
    pub async fn the_notice_kind_depends_on_the_childs_conversation(store: DynStore) {
        let (agent, one_shot) = (uniq("round"), uniq("oneshot"));
        let (session_seen, shot_seen) = (
            Arc::new(Mutex::new(Vec::<Inbound>::new())),
            Arc::new(Mutex::new(Vec::<Inbound>::new())),
        );
        let (session_parent_name, shot_parent_name) = (uniq("sp"), uniq("op"));
        let rt = Runtime::builder(store.clone())
            .agent(collecting_parent(&session_parent_name, &session_seen))
            .agent(collecting_parent(&shot_parent_name, &shot_seen))
            .agent(RoundAgent {
                name: agent.clone(),
                waits: false,
            })
            .agent(child_done(&one_shot, json!({"n": 1})))
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let session_parent = rt
            .start(&session_parent_name, inbound(), None)
            .await
            .unwrap();
        let shot_parent = rt.start(&shot_parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, session_parent).await;
        wait_waiting(&rt, shot_parent).await;

        let round = child_run_id(session_parent, "call-1");
        let conversation = session_conversation_id(&uniq("u:ctx"), round);
        rt.start_child_continuing(
            session_parent,
            round,
            &agent,
            say("hi"),
            &conversation,
            None,
        )
        .await
        .unwrap();
        let shot = child_run_id(shot_parent, "call-1");
        rt.start_child(shot_parent, shot, &one_shot, inbound())
            .await
            .unwrap();
        wait_done(&rt, session_parent).await;
        wait_done(&rt, shot_parent).await;
        worker.stop().await;

        let session_seen = session_seen.lock().unwrap();
        assert_eq!(session_seen.len(), 1);
        assert_eq!(session_seen[0].kind, CHILD_REPORTED_KIND);
        assert_eq!(session_seen[0].id, round.to_string());
        assert_eq!(
            session_seen[0].payload,
            json!({"status": "done", "output": {"text": "hi"}})
        );
        let (reported, status) = ChildStatus::from_report(&session_seen[0]).expect("a report");
        assert_eq!(reported, round);
        assert!(status.is_finished());
        assert!(
            ChildStatus::from_notice(&session_seen[0]).is_none(),
            "a report is not a finished notice"
        );

        let shot_seen = shot_seen.lock().unwrap();
        assert_eq!(shot_seen.len(), 1);
        assert_eq!(shot_seen[0].kind, RUN_FINISHED_KIND);
        assert_eq!(shot_seen[0].id, shot.to_string());
        assert_eq!(
            shot_seen[0].payload,
            json!({"status": "done", "output": {"n": 1}})
        );
        assert!(ChildStatus::from_report(&shot_seen[0]).is_none());
    }

    /// The listing of a parent's sessions: the latest round of each, working first and then the
    /// most recently updated, paged; sessions of a parent whose id only starts like this one's
    /// (`ctx1` and `ctx10`), a name that is not a session id and a session's own children are not
    /// in it.
    pub async fn the_sessions_of_a_parent_are_listed_sorted_paged_and_scoped(store: DynStore) {
        let (parent_name, working, idle) = (uniq("parent"), uniq("working"), uniq("idle"));
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(RoundAgent {
                name: working.clone(),
                waits: true,
            })
            .agent(RoundAgent {
                name: idle.clone(),
                waits: false,
            })
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let owner = uniq("owner");
        let (ctx1, ctx10) = (format!("{owner}:ctx1"), format!("{owner}:ctx10"));
        let pause = || tokio::time::sleep(Duration::from_millis(15));

        // Two idle sessions (the first has two rounds), then one working, then a stranger's.
        let first = child_run_id(parent, "a1");
        let first_conv = session_conversation_id(&ctx1, first);
        rt.start_child_continuing(parent, first, &idle, say("a"), &first_conv, None)
            .await
            .unwrap();
        wait_done(&rt, first).await;
        pause().await;
        let first_2 = child_run_id(parent, "a2");
        rt.start_child_continuing(parent, first_2, &idle, say("a2"), &first_conv, Some(first))
            .await
            .unwrap();
        wait_done(&rt, first_2).await;
        pause().await;
        let second = child_run_id(parent, "b1");
        let second_conv = session_conversation_id(&ctx1, second);
        rt.start_child_continuing(parent, second, &idle, say("b"), &second_conv, None)
            .await
            .unwrap();
        wait_done(&rt, second).await;
        pause().await;
        let busy = child_run_id(parent, "c1");
        let busy_conv = session_conversation_id(&ctx1, busy);
        rt.start_child_continuing(parent, busy, &working, say("c"), &busy_conv, None)
            .await
            .unwrap();
        wait_waiting(&rt, busy).await;
        let other = child_run_id(parent, "d1");
        rt.start_child_continuing(
            parent,
            other,
            &idle,
            say("d"),
            &session_conversation_id(&ctx10, other),
            None,
        )
        .await
        .unwrap();
        // Not sessions of ctx1 itself: a name that is no session id, and a session's own child.
        rt.start(&idle, say("e"), Some(&format!("{ctx1}/s/not-a-session")))
            .await
            .unwrap();
        rt.start(
            &idle,
            say("f"),
            Some(&format!("{busy_conv}/s/{}", RunId::new())),
        )
        .await
        .unwrap();
        wait_done(&rt, other).await;
        worker.stop().await;

        let agents = [working.as_str(), idle.as_str()];
        let all = rt.list_sessions(&ctx1, &agents, 0, 10).await.unwrap();
        assert_eq!(all.total, 3);
        let ids: Vec<RunId> = all.sessions.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![busy, second, first], "working first, then newest");
        assert_eq!(all.sessions[0].state, SessionState::Working);
        assert_eq!(all.sessions[0].agent, working);
        assert_eq!(all.sessions[0].round, 1);
        assert_eq!(all.sessions[1].state, SessionState::Idle);
        assert_eq!(all.sessions[2].round, 2, "the latest of two rounds");
        assert_eq!(all.sessions[2].latest_run, first_2);
        assert_eq!(all.sessions[2].conversation_id, first_conv);

        let page = rt.list_sessions(&ctx1, &agents, 1, 1).await.unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(
            page.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![second]
        );
        assert!(
            rt.list_sessions(&ctx1, &agents, 3, 10)
                .await
                .unwrap()
                .sessions
                .is_empty()
        );

        let stranger = rt.list_sessions(&ctx10, &agents, 0, 10).await.unwrap();
        assert_eq!(
            stranger.sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![other],
            "ctx10 is not ctx1"
        );
        assert!(
            rt.list_sessions(&format!("{owner}:ctx"), &agents, 0, 10)
                .await
                .unwrap()
                .sessions
                .is_empty(),
            "a shorter context id does not list its longer neighbours' sessions"
        );
    }

    /// A round whose parent is the round itself (a person started it: no run of an agent did) reads
    /// as started outside a run, which a round of a parent run does not; the runtime never
    /// cancels it because another run ended.
    pub async fn a_round_that_is_its_own_parent_is_listed_reads_as_started_outside_and_is_never_cancelled_with_a_parent(
        store: DynStore,
    ) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let rt = rounds_runtime(&store, &parent_name, &[(&agent, true)]);
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let ctx = uniq("u:ctx");

        let by_agent = child_run_id(parent, "a");
        let by_agent_conv = session_conversation_id(&ctx, by_agent);
        rt.start_child_continuing(parent, by_agent, &agent, say("a"), &by_agent_conv, None)
            .await
            .unwrap();
        let alone = RunId::new();
        let alone_conv = session_conversation_id(&ctx, alone);
        rt.start_child_continuing(alone, alone, &agent, say("b"), &alone_conv, None)
            .await
            .unwrap();
        wait_waiting(&rt, by_agent).await;
        wait_waiting(&rt, alone).await;

        let listed = rt
            .list_sessions(&ctx, &[agent.as_str()], 0, 10)
            .await
            .unwrap();
        let summary = |id: RunId| listed.sessions.iter().find(|s| s.id == id).unwrap();
        assert!(!summary(by_agent).started_outside_a_run());
        assert!(summary(alone).started_outside_a_run());
        assert_eq!(summary(alone).started_by, Some(alone));
        let view = rt.session_view(&ctx, alone).await.unwrap().unwrap();
        assert!(view.started_outside_a_run());
        assert!(
            !rt.session_view(&ctx, by_agent)
                .await
                .unwrap()
                .unwrap()
                .started_outside_a_run()
        );

        // The parent run ends; both rounds are woken. The parent's round is cancelled when it
        // steps, the person's round is not: it steps and finishes.
        rt.cancel(parent, "done").await.unwrap();
        rt.deliver(by_agent, say("again")).await.unwrap();
        rt.deliver(alone, say("again")).await.unwrap();
        wait_failed(&rt, by_agent).await;
        let again = wait_done(&rt, alone).await;
        assert_eq!(
            again.error, None,
            "the parent's end does not cancel a round that is its own parent"
        );
        worker.stop().await;
    }

    /// A round's terminal commit lands but telling the parent fails: the parent has no notice and
    /// learns of the round by reading it (`Ctx::child_status`) at its next step, as it does for a
    /// one-shot child.
    pub async fn a_lost_round_report_is_recovered_by_reading_the_round(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let (faulty, dynamic) = faulty(store);
        let session = Arc::new(Mutex::new(None::<RunId>));
        let parent_agent = fn_agent(
            &parent_name,
            step_fn({
                let session = session.clone();
                move |ctx, state| {
                    let session = session.clone();
                    async move {
                        if ctx.take_inbox().is_empty() {
                            return Ok(Transition::Park {
                                state,
                                wake_at: None,
                            });
                        }
                        let round = session.lock().unwrap().expect("the round was started");
                        let status = ctx.child_status(round).await?;
                        Ok(Transition::Done {
                            state,
                            output: json!({"status": status}),
                        })
                    }
                    .boxed()
                }
            }),
        );
        let rt = builder(&dynamic, &uniq("w"), &parent_agent)
            .agent(RoundAgent {
                name: agent.clone(),
                waits: false,
            })
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, parent).await;

        faulty.fail_run(Method::CommitRun, parent, 1);
        let round = child_run_id(parent, "call-1");
        *session.lock().unwrap() = Some(round);
        let conversation = session_conversation_id(&uniq("u:ctx"), round);
        rt.start_child_continuing(parent, round, &agent, say("hi"), &conversation, None)
            .await
            .unwrap();
        wait_done(&rt, round).await;
        wait_injected(&faulty, Method::CommitRun, 1, "the report to fail").await;
        assert_eq!(
            rt.view(parent).await.unwrap().unwrap().pending_inbox,
            0,
            "the report was lost"
        );

        rt.deliver(parent, inbound()).await.unwrap();
        let done = wait_done(&rt, parent).await;
        worker.stop().await;
        assert_eq!(
            done.output.unwrap()["status"],
            json!({"status": "done", "output": {"text": "hi"}})
        );
    }

    // -- child sessions: what a caller may not make of the entry points --------

    /// A runtime with a parent that only waits and the agent `name` for rounds.
    fn rounds_runtime(store: &DynStore, parent_name: &str, rounds: &[(&str, bool)]) -> Runtime {
        let mut builder = Runtime::builder(store.clone())
            .agent(child_parked(parent_name))
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20));
        for (name, waits) in rounds {
            builder = builder.agent(RoundAgent {
                name: (*name).to_owned(),
                waits: *waits,
            });
        }
        builder.build()
    }

    /// A finished first round of `agent` under `conversation`, started by `parent`.
    async fn finished_first_round(
        rt: &Runtime,
        parent: RunId,
        agent: &str,
        conversation_of: impl Fn(RunId) -> String,
        key: &str,
    ) -> (RunId, String) {
        let session = child_run_id(parent, key);
        let conversation = conversation_of(session);
        rt.start_child_continuing(parent, session, agent, say("one"), &conversation, None)
            .await
            .unwrap();
        let worker = spawn_worker(rt);
        wait_done(rt, session).await;
        worker.stop().await;
        (session, conversation)
    }

    /// `prior` is checked: a finished round of another session, of another agent, a run that is
    /// not a round of the session at all, and a conversation id that is not a session's are each
    /// refused, and nothing is created. A still-open prior is the busy case.
    pub async fn a_prior_must_be_a_finished_round_of_this_session(store: DynStore) {
        let (parent_name, agent, other_agent) = (uniq("parent"), uniq("round"), uniq("other"));
        let rt = rounds_runtime(
            &store,
            &parent_name,
            &[(&agent, false), (&other_agent, true)],
        );
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let alice = uniq("alice:ctx");
        let (a_session, a_conv) = finished_first_round(
            &rt,
            parent,
            &agent,
            |s| session_conversation_id(&alice, s),
            "a",
        )
        .await;
        let other_parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let bob = uniq("bob:ctx");
        let b_session = child_run_id(other_parent, "b");
        let b_conv = session_conversation_id(&bob, b_session);

        let next = RunId::new();
        // Bob's session continues from Alice's finished round: another conversation.
        for (conversation, id, prior, what) in [
            (&b_conv, b_session, None, "round 1 of Bob's"),
            (
                &b_conv,
                next,
                Some(a_session),
                "Alice's round as Bob's prior",
            ),
        ] {
            let res = rt
                .start_child_continuing(other_parent, id, &agent, say("x"), conversation, prior)
                .await;
            if prior.is_none() {
                assert!(res.unwrap(), "{what}");
            } else {
                let err = res.expect_err(what);
                assert_eq!(err.class(), adam_runtime::ErrorClass::Invalid, "{err}");
                assert!(matches!(err, RuntimeError::InvalidSession { .. }), "{err}");
                assert!(store.load_run(id).await.unwrap().is_none(), "{what}");
            }
        }

        // Another agent's run is not this agent's state to read.
        let foreign = rt
            .start(&other_agent, say("f"), Some(&a_conv))
            .await
            .expect("another agent in Alice's conversation");
        let err = rt
            .start_child_continuing(parent, next, &agent, say("x"), &a_conv, Some(foreign))
            .await
            .expect_err("another agent's prior");
        assert_eq!(err.class(), adam_runtime::ErrorClass::Invalid);
        assert!(store.load_run(next).await.unwrap().is_none());

        // A conversation id that is not a session's.
        let err = rt
            .start_child_continuing(parent, next, &agent, say("x"), "alice:plain", None)
            .await
            .expect_err("not a session conversation id");
        assert!(matches!(err, RuntimeError::InvalidSession { .. }), "{err}");
        assert!(store.load_run(next).await.unwrap().is_none());

        // The id of round 1 is the session id, and only round 1's.
        let err = rt
            .start_child_continuing(parent, next, &agent, say("x"), &a_conv, None)
            .await
            .expect_err("round 1 is the session id");
        assert!(matches!(err, RuntimeError::InvalidSession { .. }), "{err}");
        let err = rt
            .start_child_continuing(
                parent,
                a_session,
                &agent,
                say("x"),
                &a_conv,
                Some(a_session),
            )
            .await
            .expect_err("a later round is not the session id");
        assert!(matches!(err, RuntimeError::InvalidSession { .. }), "{err}");

        // A top-level run in a session-shaped conversation is not a round: continuing it is refused.
        let spoof_session = RunId::new();
        let spoof_conv = session_conversation_id(&alice, spoof_session);
        let spoof = rt.start(&agent, say("s"), Some(&spoof_conv)).await.unwrap();
        rt.cancel(spoof, "over").await.unwrap();
        let err = rt
            .start_child_continuing(parent, next, &agent, say("x"), &spoof_conv, Some(spoof))
            .await
            .expect_err("a root run is not a round");
        assert!(matches!(err, RuntimeError::InvalidSession { .. }), "{err}");
        assert!(store.load_run(next).await.unwrap().is_none());

        // An open prior of this session is the busy case, not an invalid one.
        let c_session = child_run_id(parent, "c");
        let c_conv = session_conversation_id(&alice, c_session);
        rt.start_child_continuing(parent, c_session, &other_agent, say("c"), &c_conv, None)
            .await
            .unwrap();
        let err = rt
            .start_child_continuing(
                parent,
                next,
                &other_agent,
                say("x"),
                &c_conv,
                Some(c_session),
            )
            .await
            .expect_err("the round is open");
        assert!(err.is_busy(), "{err}");
        assert!(store.load_run(next).await.unwrap().is_none());
    }

    /// A repeat is a repeat only of the same round: a run with that id that has another agent,
    /// another conversation or (for round 1) another parent is refused, not answered `false`.
    pub async fn a_repeat_is_only_a_repeat_of_the_same_round(store: DynStore) {
        let (parent_name, agent, other_agent) = (uniq("parent"), uniq("round"), uniq("other"));
        let rt = rounds_runtime(
            &store,
            &parent_name,
            &[(&agent, false), (&other_agent, false)],
        );
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let ctx = uniq("u:ctx");
        let (session, conv) = finished_first_round(
            &rt,
            parent,
            &agent,
            |s| session_conversation_id(&ctx, s),
            "a",
        )
        .await;

        // The same round, again: `false`, for round 1 and for a later round.
        assert!(
            !rt.start_child_continuing(parent, session, &agent, say("y"), &conv, None)
                .await
                .unwrap()
        );
        let second = child_run_id(parent, "second");
        assert!(
            rt.start_child_continuing(parent, second, &agent, say("two"), &conv, Some(session))
                .await
                .unwrap()
        );
        assert!(
            !rt.start_child_continuing(parent, second, &agent, say("y"), &conv, Some(session))
                .await
                .unwrap()
        );

        // Another parent for round 1, another agent, or another conversation: refused.
        let stranger = rt.start(&parent_name, inbound(), None).await.unwrap();
        for (who, id, name, conversation, prior) in [
            (stranger, session, &agent, conv.clone(), None),
            (parent, session, &other_agent, conv.clone(), None),
            (parent, second, &other_agent, conv.clone(), Some(session)),
        ] {
            let err = rt
                .start_child_continuing(who, id, name, say("y"), &conversation, prior)
                .await
                .expect_err("an existing run that is not this round");
            assert_eq!(err.class(), adam_runtime::ErrorClass::Invalid, "{err}");
        }
        // The parent's own run id, used as a session id: it exists and is not a round.
        let own_conv = session_conversation_id(&ctx, parent);
        let err = rt
            .start_child_continuing(parent, parent, &agent, say("y"), &own_conv, None)
            .await
            .expect_err("the parent is not a round");
        assert_eq!(err.class(), adam_runtime::ErrorClass::Invalid, "{err}");
        let view = rt.view(parent).await.unwrap().unwrap();
        assert_eq!(view.agent, parent_name, "nothing was changed");
    }

    /// A session has one agent: a round of another agent is refused whether it continues a round
    /// or claims to be round 1, so one open round per session follows from the store's index.
    pub async fn a_session_keeps_one_agent(store: DynStore) {
        let (parent_name, agent, other_agent) = (uniq("parent"), uniq("round"), uniq("other"));
        let rt = rounds_runtime(
            &store,
            &parent_name,
            &[(&agent, true), (&other_agent, true)],
        );
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let ctx = uniq("u:ctx");
        let session = child_run_id(parent, "a");
        let conv = session_conversation_id(&ctx, session);
        rt.start_child_continuing(parent, session, &agent, say("one"), &conv, None)
            .await
            .unwrap();
        let worker = spawn_worker(&rt);
        wait_waiting(&rt, session).await;

        // Round 1 is open: the other agent cannot take its place by naming the session id, nor
        // continue it.
        let err = rt
            .start_child_continuing(parent, session, &other_agent, say("x"), &conv, None)
            .await
            .expect_err("another agent as round 1");
        assert_eq!(err.class(), adam_runtime::ErrorClass::Invalid, "{err}");
        let next = RunId::new();
        let err = rt
            .start_child_continuing(parent, next, &other_agent, say("x"), &conv, Some(session))
            .await
            .expect_err("another agent's round 2");
        assert!(store.load_run(next).await.unwrap().is_none(), "{err}");
        // Nothing of the other agent's is in the conversation.
        let none = store
            .open_run_for_conversation(&other_agent, &conv)
            .await
            .unwrap();
        assert!(none.is_none());
        worker.stop().await;
    }

    /// A session conversation id longer than the bound is refused and names its length.
    pub async fn a_session_conversation_id_has_a_length_limit(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let rt = rounds_runtime(&store, &parent_name, &[(&agent, false)]);
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let session = child_run_id(parent, "a");
        let long = "x".repeat(adam_runtime::MAX_SESSION_CONVERSATION_BYTES);
        let conv = session_conversation_id(&long, session);
        let err = rt
            .start_child_continuing(parent, session, &agent, say("x"), &conv, None)
            .await
            .expect_err("too long");
        assert_eq!(err.class(), adam_runtime::ErrorClass::Invalid);
        assert!(err.to_string().contains(&conv.len().to_string()), "{err}");
        assert!(store.load_run(session).await.unwrap().is_none());
        // Just under the bound it works.
        let fits = session_conversation_id(
            &"x".repeat(adam_runtime::MAX_SESSION_CONVERSATION_BYTES - 39),
            session,
        );
        assert_eq!(fits.len(), adam_runtime::MAX_SESSION_CONVERSATION_BYTES);
        assert!(
            rt.start_child_continuing(parent, session, &agent, say("x"), &fits, None)
                .await
                .unwrap()
        );
    }

    /// The parents' ids hold `/s/`, `%` and `/`, or only start alike: each lists its own sessions
    /// and no other's, and a top-level run in a session-shaped conversation is not a session.
    pub async fn session_listings_do_not_mix_awkward_parent_ids(store: DynStore) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let rt = rounds_runtime(&store, &parent_name, &[(&agent, false)]);
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let owner = uniq("owner");
        let parents: Vec<String> = ["ctx1", "ctx10", "a/s/b", "a%2Fs", "a%", "a/", "a"]
            .iter()
            .map(|c| format!("{owner}:{c}"))
            .collect();
        let mut sessions = Vec::new();
        for (i, conversation) in parents.iter().enumerate() {
            let session = child_run_id(parent, &format!("s{i}"));
            rt.start_child_continuing(
                parent,
                session,
                &agent,
                say("x"),
                &session_conversation_id(conversation, session),
                None,
            )
            .await
            .unwrap();
            sessions.push(session);
        }
        // A client's own context that spells a session's conversation id, started at the top.
        let loose = RunId::new();
        rt.start(
            &agent,
            say("loose"),
            Some(&session_conversation_id(&parents[0], loose)),
        )
        .await
        .unwrap();

        for (conversation, session) in parents.iter().zip(&sessions) {
            let listed = rt
                .list_sessions(conversation, &[&agent], 0, 10)
                .await
                .unwrap();
            let ids: Vec<RunId> = listed.sessions.iter().map(|s| s.id).collect();
            assert_eq!(ids, vec![*session], "{conversation:?}");
        }
    }

    /// The agents named twice count once, the page is capped, and the listing reads past one scan
    /// page of the store (more than 200 rounds of one agent).
    pub async fn a_session_listing_dedupes_agents_caps_the_page_and_reads_every_page(
        store: DynStore,
    ) {
        let (parent_name, agent) = (uniq("parent"), uniq("round"));
        let rt = rounds_runtime(&store, &parent_name, &[(&agent, false)]);
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let ctx = uniq("u:ctx");
        let mut made = HashSet::new();
        for i in 0..201 {
            let session = child_run_id(parent, &format!("s{i}"));
            rt.start_child_continuing(
                parent,
                session,
                &agent,
                say("x"),
                &session_conversation_id(&ctx, session),
                None,
            )
            .await
            .unwrap();
            made.insert(session);
        }
        let all = rt
            .list_sessions(&ctx, &[&agent, &agent], 0, 1000)
            .await
            .unwrap();
        assert_eq!(all.total, 201, "every round was read, none twice");
        assert_eq!(all.sessions.len(), adam_runtime::MAX_SESSION_PAGE);
        assert!(all.sessions.iter().all(|s| s.round == 1));
        let tail = rt.list_sessions(&ctx, &[&agent], 200, 1000).await.unwrap();
        assert_eq!(tail.sessions.len(), 1);
        let mut seen: HashSet<RunId> = all.sessions.iter().map(|s| s.id).collect();
        for offset in [100, 200] {
            let page = rt
                .list_sessions(&ctx, &[&agent], offset, 100)
                .await
                .unwrap();
            seen.extend(page.sessions.iter().map(|s| s.id));
        }
        assert_eq!(seen, made, "the pages together are every session once");
    }

    /// A listing says how the latest round ended, and a working session comes before a newer idle
    /// one.
    pub async fn a_session_listing_says_how_the_round_ended(store: DynStore) {
        let parent_name = uniq("parent");
        let (done, failed, cancelled, working) = (
            uniq("done"),
            uniq("failed"),
            uniq("cancelled"),
            uniq("working"),
        );
        let rt = Runtime::builder(store.clone())
            .agent(child_parked(&parent_name))
            .agent(RoundAgent {
                name: done.clone(),
                waits: false,
            })
            .agent(child_failing(&failed, "boom"))
            .agent(child_parked(&cancelled))
            .agent(child_parked(&working))
            .worker_id(uniq("w"))
            .poll_interval(Duration::from_millis(20))
            .build();
        let parent = rt.start(&parent_name, inbound(), None).await.unwrap();
        let ctx = uniq("u:ctx");
        let worker = spawn_worker(&rt);
        let mut ids = HashMap::new();
        // The working one is created first, so every idle one is newer than it.
        for name in [&working, &cancelled, &done, &failed] {
            let session = child_run_id(parent, name);
            rt.start_child_continuing(
                parent,
                session,
                name,
                say("x"),
                &session_conversation_id(&ctx, session),
                None,
            )
            .await
            .unwrap();
            ids.insert(name.clone(), session);
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        wait_waiting(&rt, ids[&working]).await;
        wait_waiting(&rt, ids[&cancelled]).await;
        rt.cancel(ids[&cancelled], "enough").await.unwrap();
        wait_done(&rt, ids[&done]).await;
        wait_failed(&rt, ids[&failed]).await;
        worker.stop().await;

        let names = [
            working.as_str(),
            cancelled.as_str(),
            done.as_str(),
            failed.as_str(),
        ];
        let listed = rt.list_sessions(&ctx, &names, 0, 10).await.unwrap();
        let outcome = |name: &String| {
            listed
                .sessions
                .iter()
                .find(|s| s.id == ids[name])
                .map(|s| (s.state, s.outcome))
                .expect("listed")
        };
        assert_eq!(outcome(&working), (SessionState::Working, None));
        assert_eq!(
            outcome(&done),
            (SessionState::Idle, Some(RoundOutcome::Done))
        );
        assert_eq!(
            outcome(&failed),
            (SessionState::Idle, Some(RoundOutcome::Failed))
        );
        assert_eq!(
            outcome(&cancelled),
            (SessionState::Idle, Some(RoundOutcome::Cancelled))
        );
        assert_eq!(
            listed.sessions[0].id, ids[&working],
            "a working session comes before newer idle ones"
        );
    }

    /// A report that lands while a run finishes is not news, as a finished notice is not: no
    /// agent reads it before slice 2.
    pub async fn a_report_that_lands_while_a_run_finishes_does_not_reopen_it(store: DynStore) {
        let name = uniq("reopen-report");
        let report = Inbound::new(
            CHILD_REPORTED_KIND,
            json!({"status": "done", "output": "42"}),
        )
        .with_id(RunId::new().to_string());
        let (rt, run, phase0_runs, arrived) = finishing_agent(&store, &name, true, &[report]).await;
        let view = rt.view(run).await.unwrap().unwrap();
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(view.output, Some(json!("first")), "the first answer stands");
        assert_eq!(count(&phase0_runs), 1);
        assert_eq!(arrived.load(SeqCst), 0, "a report is not an arrival yet");
    }

    /// A few MiB of transcript, carried through two commits: the state a commit lands is exactly
    /// the one the agent returned, and the turns before it are untouched. A store that truncates,
    /// or a decode that loses a member, fails here; the copy count is the benchmark's business.
    pub async fn a_large_state_lands_unchanged(store: DynStore) {
        const MESSAGES: usize = 48;
        const TEXT: usize = 64 * 1024;
        let name = uniq("large-state");
        let text = "x".repeat(TEXT);
        let transcript: Vec<Value> = (0..MESSAGES)
            .map(|i| {
                json!({
                    "role": if i % 2 == 0 { "user" } else { "assistant" },
                    "text": text,
                })
            })
            .collect();
        let start = json!({"messages": transcript});
        let bytes = serde_json::to_vec(&start)
            .expect("the state serialises")
            .len();
        assert!(
            bytes > 3 * 1024 * 1024,
            "the state is a few MiB, not {bytes} bytes"
        );

        let turns = Arc::new(AtomicUsize::new(0));
        let agent = fn_agent(
            &name,
            step_fn({
                let turns = turns.clone();
                move |_ctx, mut state| {
                    let turns = turns.clone();
                    async move {
                        let messages = state["messages"].as_array_mut().expect("a transcript");
                        let at = messages.len();
                        messages.push(json!({"role": "assistant", "text": format!("turn {at}")}));
                        if turns.fetch_add(1, SeqCst) == 0 {
                            Ok(Transition::Continue(state))
                        } else {
                            Ok(Transition::Done {
                                state,
                                output: json!("finished"),
                            })
                        }
                    }
                    .boxed()
                }
            }),
        );
        let rt = runtime(&store, &agent);
        let worker = spawn_worker(&rt);
        let run = rt
            .start(&name, Inbound::new("start", start.clone()), None)
            .await
            .expect("start");
        let view = wait_done(&rt, run).await;
        worker.stop().await;

        let appended: Vec<Value> = (MESSAGES..MESSAGES + 2)
            .map(|i| json!({"role": "assistant", "text": format!("turn {i}")}))
            .collect();
        let messages = view.state["messages"].as_array().expect("the transcript");
        assert_eq!(messages.len(), MESSAGES + 2, "both turns were appended");
        assert_eq!(
            &messages[..MESSAGES],
            start["messages"]
                .as_array()
                .expect("a transcript")
                .as_slice(),
            "the transcript it started with is the one that landed"
        );
        assert_eq!(&messages[MESSAGES..], appended.as_slice());
        assert_eq!(view.output, Some(json!("finished")));
    }

    /// A permanent failure commits the state the run already had. The step took the agent state
    /// by move, so this is the path that reads it back from the record instead of a drained
    /// envelope: a regression here writes `"agent": null` over the run's state.
    pub async fn a_permanent_failure_keeps_the_agents_state(store: DynStore) {
        let name = uniq("perm-state");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, _state| {
                async move { Err(AgentError::permanent("no such customer")) }.boxed()
            }),
        );
        let rt = runtime(&store, &agent);
        let run = rt
            .start(&name, Inbound::new("start", json!({"k": 1})), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        let failed = wait_failed(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            failed.state,
            json!({"k": 1}),
            "the failure kept the agent's state"
        );
    }

    /// The same holds when the retries run out: the last failure commits the state the run had,
    /// not `null` from the envelope the step drained.
    pub async fn exhausted_retries_keep_the_agents_state(store: DynStore) {
        let name = uniq("retry-state");
        let agent = fn_agent(
            &name,
            step_fn(|_ctx, _state| {
                async move { Err(AgentError::transient("still limited")) }.boxed()
            }),
        );
        let clock = ManualClock::new();
        let rt = builder(&store, &uniq("w"), &agent)
            .clock(clock.clone())
            .retry(RetryPolicy {
                max_attempts: 2,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(50),
                multiplier: 2.0,
            })
            .build();
        let run = rt
            .start(&name, Inbound::new("start", json!({"k": 2})), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        // The first failure schedules a retry; the run may also exhaust both attempts before a
        // reader sees attempt 1, so accept either, then move the clock for a scheduled retry.
        wait_for(&rt, run, "the first failure", |v| {
            v.attempt >= 1 || v.status == RunStatus::Failed
        })
        .await;
        clock.advance(Duration::from_secs(1));
        let failed = wait_failed(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            failed.state,
            json!({"k": 2}),
            "the exhausted retry kept the agent's state"
        );
        let state = store.load_run(run).await.unwrap().expect("the run").state;
        assert_eq!(
            super::common::v4::V4Envelope::decode(&state)
                .expect("a v4 envelope")
                .v,
            1
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
                a_done_that_asked_to_reopen_goes_on_when_a_message_arrived,
                a_done_that_asked_to_reopen_finishes_when_nothing_arrived,
                a_done_that_did_not_ask_finishes_past_a_message,
                a_childs_notice_that_lands_while_a_run_finishes_does_not_reopen_it,
                a_message_beside_a_childs_notice_still_reopens_the_run,
                a_copy_of_a_message_the_run_read_does_not_reopen_it,
                a_message_beside_a_copy_still_reopens_the_run,
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
                a_header_is_one_store_call,
                a_starter_only_runtime_starts_and_a_full_runtime_steps,
                a_claimed_run_says_so_before_its_first_commit,
                a_lease_that_ran_out_is_not_a_claim,
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
                a_step_that_outlives_its_lease_is_not_claimed_again_by_its_worker,
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
                cancelling_a_waiting_parent_cancels_its_child,
                a_running_child_and_its_child_are_told_within_one_poll,
                a_child_created_after_the_cancel_never_steps,
                a_child_follows_a_parent_that_failed_but_not_one_that_is_done,
                a_cancelled_parent_fails_the_open_round_and_the_session_continues,
                a_child_past_its_deadline_fails_and_wakes_at_it,
                an_envelope_without_a_deadline_decodes,
                a_child_parked_on_a_question_is_cancelled_when_its_parent_is,
                a_grandchild_under_a_parked_child_follows,
                a_healthy_parked_child_is_woken_without_stepping_the_agent,
                a_done_parent_that_was_purged_leaves_its_open_child_running,
                a_child_waiting_for_a_person_keeps_its_time_for_when_it_works,
                a_wake_before_the_deadline_parks_again_without_stepping,
                a_runnable_wake_past_the_deadline_is_clamped_to_it,
                a_deadline_is_set_once_and_not_moved_by_later_commits,
                a_round_with_a_limit_fails_past_it,
                the_watch_reads_the_parent_about_once_a_second,
                the_cancel_watch_polls_at_its_own_interval,
                a_parent_that_loses_its_lease_does_not_start_or_resume_twice,
                child_status_reads_only_the_callers_children,
                a_purged_child_reads_as_gone,
                the_notice_is_an_ordinary_inbound_with_the_childs_id,
                a_run_continues_a_finished_run_from_a_start_only_front,
                continuing_checks_the_prior_run_and_creates_nothing_when_it_refuses,
                a_prior_state_that_does_not_decode_starts_the_run_fresh,
                an_agent_without_an_override_starts_fresh_when_asked_to_continue,
                start_continuing_delivers_to_an_open_run_and_otherwise_continues,
                a_session_round_carries_the_previous_rounds_conversation,
                a_round_is_refused_while_one_is_open_and_deliver_steers,
                a_repeated_session_start_returns_the_same_round,
                the_notice_kind_depends_on_the_childs_conversation,
                the_sessions_of_a_parent_are_listed_sorted_paged_and_scoped,
                a_lost_round_report_is_recovered_by_reading_the_round,
                a_prior_must_be_a_finished_round_of_this_session,
                a_repeat_is_only_a_repeat_of_the_same_round,
                a_session_keeps_one_agent,
                a_session_conversation_id_has_a_length_limit,
                session_listings_do_not_mix_awkward_parent_ids,
                a_session_listing_dedupes_agents_caps_the_page_and_reads_every_page,
                a_session_listing_says_how_the_round_ended,
                a_session_idle_past_its_ttl_reads_closed,
                a_working_round_never_expires,
                a_report_that_lands_while_a_run_finishes_does_not_reopen_it,
                a_large_state_lands_unchanged,
                a_permanent_failure_keeps_the_agents_state,
                exhausted_retries_keep_the_agents_state,
                a_round_that_is_its_own_parent_is_listed_reads_as_started_outside_and_is_never_cancelled_with_a_parent,
                a_cancel_runs_the_agents_hook_once_with_its_last_state,
                a_finished_run_runs_no_hook,
                a_process_with_only_a_starter_runs_no_hook,
                a_parent_cascade_and_a_deadline_run_the_hook,
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

// ---------------------------------------------------------------------------
// The shadow: the head and the body written beside the state
// ---------------------------------------------------------------------------

/// A state shaped like a conversation: the messages split from the counters, as
/// `adam_llm_agent::Conversation` splits them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Transcript {
    messages: Vec<String>,
    turns: u32,
}

/// An agent that splits [`Transcript`]; `wrong` drops the last row from the split, which the
/// runtime's reconstruction check must refuse.
#[derive(Clone)]
struct TranscriptAgent {
    name: String,
    steps: u32,
    /// Park once the state has this many turns (`None`: never).
    park_at: Option<u32>,
    /// Drop the last row: a split that does not reconstruct the state it was taken from.
    wrong: bool,
    /// A file artifact the first step emits, for the blob gate.
    file: Option<Artifact>,
    /// Replace the transcript on the second step: a compaction, not an append.
    compact: bool,
}

impl TranscriptAgent {
    fn new(name: &str, steps: u32) -> Self {
        Self {
            name: name.to_owned(),
            steps,
            park_at: None,
            wrong: false,
            file: None,
            compact: false,
        }
    }

    fn with_file(mut self, file: Artifact) -> Self {
        self.file = Some(file);
        self
    }
}

#[async_trait]
impl Agent for TranscriptAgent {
    type State = Transcript;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Transcript, AgentError> {
        Ok(Transcript {
            messages: vec![input.payload["text"].as_str().unwrap_or("start").to_owned()],
            turns: 0,
        })
    }

    fn split(&self, state: &Transcript) -> Option<(Transcript, Vec<Value>)> {
        let mut head = state.clone();
        let mut rows: Vec<Value> = std::mem::take(&mut head.messages)
            .into_iter()
            .map(|message| json!(message))
            .collect();
        if self.wrong {
            rows.pop();
        }
        Some((head, rows))
    }

    fn join(&self, head: Transcript, rows: Vec<Value>) -> Result<Transcript, AgentError> {
        let messages = rows
            .into_iter()
            .map(|row| {
                serde_json::from_value::<String>(row).map_err(|e| {
                    AgentError::permanent("a transcript row is not a message").with_source(e)
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Transcript { messages, ..head })
    }

    async fn step(
        &self,
        ctx: &mut Ctx,
        mut state: Transcript,
    ) -> Result<Transition<Transcript>, AgentError> {
        if state.turns == 0
            && let Some(file) = &self.file
        {
            ctx.emit(RunEvent::from(file.clone())).await;
        }
        if self.compact && state.turns == 1 {
            state.messages.clear();
            state.messages.push("compacted".into());
        }
        state.turns += 1;
        state.messages.push(format!("turn {}", state.turns));
        if self.park_at == Some(state.turns) {
            return Ok(Transition::Park {
                state,
                wake_at: None,
            });
        }
        if state.turns >= self.steps {
            Ok(Transition::Done {
                state,
                output: json!("done"),
            })
        } else {
            Ok(Transition::Continue(state))
        }
    }
}

fn transcript_runtime(store: &DynStore, agent: &TranscriptAgent) -> Runtime {
    Runtime::builder(store.clone())
        .agent(agent.clone())
        .worker_id(uniq("w"))
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_secs(10))
        .build()
}

/// A correct split writes the head and the body beside the full v4 state, and the store serves
/// the head at the version it was written at.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_step_that_splits_writes_the_head_and_the_body_beside_the_state() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let recording = Arc::new(common::recording::RecordingStore::new(inner.clone()));
    let store: DynStore = recording.clone();
    let agent = TranscriptAgent::new(&uniq("split"), 2);
    let rt = transcript_runtime(&store, &agent);
    let run = rt.start(&agent.name, inbound(), None).await.expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let commits = recording.commits();
    assert_eq!(commits.len(), 2, "one commit per transition");
    let first = &commits[0];
    let head = first.head.as_ref().expect("the step commit writes a head");
    let body = first.body.as_ref().expect("and the rows the head names");
    assert_eq!(head["v"], 5);
    // The runtime leaves the generation null; the store resolves and patches it.
    assert_eq!(head["body"]["generation"], Value::Null);
    let generation = body
        .generation
        .expect("the first split starts a generation");
    assert_eq!(head["body"]["len"], 2);
    assert!(
        head["agent"]["messages"].as_array().unwrap().is_empty(),
        "the head keeps no message"
    );
    assert_eq!(
        body.rows.iter().map(|row| row.idx).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(body.rows[0].message, json!("start"));
    // `state` is still the complete v4 envelope, beside the head.
    assert_eq!(first.state["v"], 1);
    assert_eq!(
        first.state["agent"],
        json!({"messages": ["start", "turn 1"], "turns": 1})
    );

    // The done commit appends to the same generation: the head grows, the rows are not rewritten.
    let last = commits.last().expect("the done commit");
    let last_head = last.head.as_ref().expect("the done commit writes a head");
    let last_body = last.body.as_ref().expect("and a body");
    assert_eq!(
        last_body.generation, None,
        "an append does not start a generation"
    );
    assert_eq!(last_body.rows.len(), 3, "the final transcript is complete");
    assert_eq!(last_head["body"]["len"], 3);
    let served = inner.load_head(run).await.expect("load").expect("the run");
    assert_eq!(
        served.state["body"]["generation"],
        json!(generation.to_string())
    );
    assert_eq!(served.state["body"]["len"], 3);
    assert_eq!(served.state["agent"], last_head["agent"]);
    assert_eq!(
        inner.body_list(run, generation).await.expect("body"),
        last_body.rows,
        "one generation holds the whole transcript, not one set per commit"
    );
}

/// A split whose `join` does not restore the state is refused: the commit writes the state only,
/// the run stays v4, and no head is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_split_that_does_not_reconstruct_the_state_writes_the_state_only() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let recording = Arc::new(common::recording::RecordingStore::new(inner.clone()));
    let store: DynStore = recording.clone();
    let mut agent = TranscriptAgent::new(&uniq("wrong-split"), 1);
    agent.wrong = true;
    let rt = transcript_runtime(&store, &agent);
    let run = rt.start(&agent.name, inbound(), None).await.expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let commits = recording.commits();
    assert!(!commits.is_empty());
    assert!(
        commits
            .iter()
            .all(|update| update.head.is_none() && update.body.is_none()),
        "a split that does not join is never written"
    );
    // The run finished on its full state, in the v4 format, and has no head to serve.
    let record = inner.load_run(run).await.expect("load").expect("the run");
    assert_eq!(record.status, RunStatus::Done);
    assert_eq!(record.state["v"], 1);
    assert_eq!(
        record.state["agent"]["messages"].as_array().unwrap().len(),
        2
    );
    let served = inner.load_head(run).await.expect("load").expect("the run");
    assert_eq!(served.state["v"], 1, "there is no head to serve");
    for state in recording.states() {
        assert_eq!(state["v"], 1, "every state written stays v4");
    }
}

/// Every `state` the runtime writes is a v4 envelope, and a v5 head is not one: the fixture that
/// S3 inverts rejects it here. A delivery re-encodes what it read and writes no head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_state_written_is_a_v4_envelope_and_a_head_is_not_one() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let recording = Arc::new(common::recording::RecordingStore::new(inner.clone()));
    let store: DynStore = recording.clone();
    let mut agent = TranscriptAgent::new(&uniq("fixture"), 2);
    agent.park_at = Some(1);
    let rt = transcript_runtime(&store, &agent);
    let run = rt.start(&agent.name, inbound(), None).await.expect("start");
    let worker = spawn_worker(&rt);
    wait_waiting(&rt, run).await;
    worker.stop().await;
    let parked_head = inner
        .load_head(run)
        .await
        .expect("load")
        .expect("the run")
        .state;
    let parked_generation = uuid::Uuid::parse_str(
        parked_head["body"]["generation"]
            .as_str()
            .expect("the park's generation"),
    )
    .expect("a uuid");

    // A delivery: a commit that re-encodes the state it read, with no head.
    rt.deliver(run, late()).await.expect("deliver");
    let delivered = recording.commits().last().cloned().expect("the delivery");
    assert!(
        delivered.head.is_none() && delivered.body.is_none(),
        "a delivery writes no head"
    );
    let served = inner.load_head(run).await.expect("load").expect("the run");
    assert_eq!(
        served.state["v"], 1,
        "the head the park wrote is stale after a headless commit"
    );

    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    let served = inner.load_head(run).await.expect("load").expect("the run");
    let done_generation = uuid::Uuid::parse_str(
        served.state["body"]["generation"]
            .as_str()
            .expect("the done generation"),
    )
    .expect("a uuid");
    assert_ne!(
        done_generation, parked_generation,
        "a stale head's generation is not appended to: the done commit starts a new one"
    );
    assert_eq!(
        inner
            .body_list(run, done_generation)
            .await
            .expect("body")
            .len(),
        3,
        "the new generation holds the whole transcript"
    );

    let states = recording.states();
    assert!(
        states.len() >= 4,
        "a create, the park, the delivery and the done commit: {}",
        states.len()
    );
    for state in &states {
        let envelope = common::v4::V4Envelope::decode(state)
            .expect("every state written in E is a v4 envelope");
        assert_eq!(envelope.v, 1);
    }
    // The head the park wrote is a v5 document, and the v4 fixture rejects it.
    let head = recording
        .commits()
        .iter()
        .find_map(|update| update.head.clone())
        .expect("a step wrote a head");
    assert_eq!(head["v"], 5);
    assert!(
        common::v4::V4Envelope::decode(&head).is_err(),
        "a v5 head is not a v4 envelope"
    );
}

/// The read path is unchanged in the expand release: a view reads the state, never the head and
/// never the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_view_reads_the_state_and_never_the_head() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let counting = Arc::new(CountingStore::new(inner.clone()));
    let store: DynStore = counting.clone();
    let agent = TranscriptAgent::new(&uniq("view"), 2);
    let rt = transcript_runtime(&store, &agent);
    let run = rt.start(&agent.name, inbound(), None).await.expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    counting.reset();
    let view = rt.view(run).await.expect("view").expect("the run");
    assert_eq!(counting.calls(Method::LeaseUntil), 1);
    assert_eq!(counting.calls(Method::LoadRun), 1);
    assert_eq!(
        counting.calls(Method::LoadHead),
        0,
        "the view does not read the head in S2a"
    );
    assert_eq!(counting.calls(Method::BodyList), 0, "and it reads no body");
    assert_eq!(
        view.state["messages"].as_array().unwrap().len(),
        3,
        "the whole agent state is served"
    );
    // The head exists and is current; the runtime simply did not ask for it.
    let served = inner.load_head(run).await.expect("load").expect("the run");
    assert_eq!(served.state["v"], 5);
}

/// A run created with a file artifact in its state but no blob (a pre-E run; the runtime's start
/// paths do not copy artifacts, so this is a direct `create_run`) uploads it on its first commit,
/// before the head that names it: a head never names a digest that is not there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pre_e_file_artifact_is_uploaded_before_the_head_names_it() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let recording = Arc::new(common::recording::RecordingStore::new(inner.clone()));
    let store: DynStore = recording.clone();
    let agent = TranscriptAgent::new(&uniq("seeded"), 1);
    let file =
        Artifact::file("chart", "image/svg+xml", "chart.svg", SVG.to_vec()).expect("a small file");
    let state = json!({
        "v": 1,
        "agent": {"messages": ["start"], "turns": 0},
        "inbox": [], "seq": 0, "attempt": 0, "rev": 0,
        "output": null, "error": null,
        "artifacts": [serde_json::to_value(&file).expect("an artifact is JSON")],
    });
    let run = store
        .create_run(NewRun::new(&agent.name, state))
        .await
        .expect("create");
    let rt = transcript_runtime(&store, &agent);
    let worker = spawn_worker(&rt);
    wait_done(&rt, run.id).await;
    worker.stop().await;

    let digest = adam_runtime::blob_id(SVG);
    let first = recording.commits().into_iter().next().expect("a commit");
    assert!(
        first.blobs.contains(&digest),
        "the seeded file's digest is referenced by the first commit"
    );
    let head = first.head.expect("a head");
    assert_eq!(
        head["artifacts"][0]["file"]["digest"],
        json!(digest.to_string()),
        "the head names the digest"
    );
    assert_eq!(
        inner.get_blob(&digest).await.expect("read the blob"),
        Some(SVG.to_vec()),
        "the bytes were uploaded before the head named them"
    );
}

/// A head never names a digest whose bytes are not stored: a failed upload refuses the split, and
/// the commit writes the state only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_split_whose_file_is_not_stored_writes_the_state_only() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let faulty = Arc::new(FaultyStore::new(inner.clone()));
    faulty.fail_always(Method::PutBlob);
    let recording = Arc::new(common::recording::RecordingStore::new(faulty.clone()));
    let store: DynStore = recording.clone();
    let file =
        Artifact::file("chart", "image/svg+xml", "chart.svg", SVG.to_vec()).expect("a small file");
    let agent = TranscriptAgent::new(&uniq("no-blob"), 1).with_file(file);
    let rt = transcript_runtime(&store, &agent);
    let run = rt.start(&agent.name, inbound(), None).await.expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let commits = recording.commits();
    assert!(!commits.is_empty());
    assert!(
        commits
            .iter()
            .all(|update| update.head.is_none() && update.body.is_none()),
        "a head is not written when a file artifact's bytes are missing"
    );
    let record = inner.load_run(run).await.expect("load").expect("the run");
    assert_eq!(record.state["v"], 1);
    assert_eq!(
        inner
            .load_head(run)
            .await
            .expect("load")
            .expect("the run")
            .state["v"],
        1,
        "there is no head to serve"
    );
}

/// A transcript that no longer starts with the previous one is a compaction: the commit starts a
/// new generation with the whole transcript, and the old one stays readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_compaction_starts_a_new_generation() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let recording = Arc::new(common::recording::RecordingStore::new(inner.clone()));
    let store: DynStore = recording.clone();
    let mut agent = TranscriptAgent::new(&uniq("compact"), 2);
    agent.compact = true;
    let rt = transcript_runtime(&store, &agent);
    let run = rt.start(&agent.name, inbound(), None).await.expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let commits = recording.commits();
    assert_eq!(commits.len(), 2);
    let first = commits[0]
        .body
        .as_ref()
        .expect("a body")
        .generation
        .expect("a generation");
    let second = commits[1]
        .body
        .as_ref()
        .expect("a body")
        .generation
        .expect("a generation");
    assert_ne!(first, second, "a compaction starts a new generation");
    let served = inner.load_head(run).await.expect("load").expect("the run");
    assert_eq!(
        served.state["body"]["generation"],
        json!(second.to_string()),
        "the head names the new generation"
    );
    assert_eq!(
        inner.body_list(run, first).await.expect("body").len(),
        2,
        "the old generation is still readable"
    );
    assert_eq!(
        inner.body_list(run, second).await.expect("body").len(),
        2,
        "the compaction wrote the whole transcript under the new generation"
    );
}

/// A CAS retry reuses the attempt's body writer and generation: a retry of one transition writes
/// the same rows, not a second set.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cas_retry_reuses_the_body_writer() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let recording = Arc::new(common::recording::RecordingStore::new(inner.clone()));
    let store: DynStore = recording.clone();
    let agent = TranscriptAgent::new(&uniq("retry-writer"), 2);
    let rt = transcript_runtime(&store, &agent);
    recording.conflict_next_commit();
    let run = rt.start(&agent.name, inbound(), None).await.expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let attempts = recording.attempts();
    assert!(attempts.len() >= 3, "the first commit was tried twice");
    let first = attempts[0].body.as_ref().expect("the first attempt's body");
    let retry = attempts[1].body.as_ref().expect("the retry's body");
    assert_eq!(
        first.writer, retry.writer,
        "a retry of one attempt reuses its writer"
    );
    assert_eq!(
        first.generation, retry.generation,
        "and the generation it resolved"
    );
    assert_eq!(
        first.rows, retry.rows,
        "the retry writes the same transcript"
    );
}

/// A file a split uploaded is not sent again by the next split: one `put_blob` for the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_is_uploaded_once_across_splits() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let counting = Arc::new(CountingStore::new(inner.clone()));
    let store: DynStore = counting.clone();
    let agent = TranscriptAgent::new(&uniq("upload-once"), 2);
    let file =
        Artifact::file("chart", "image/svg+xml", "chart.svg", SVG.to_vec()).expect("a small file");
    let state = json!({
        "v": 1,
        "agent": {"messages": ["start"], "turns": 0},
        "inbox": [], "seq": 0, "attempt": 0, "rev": 0,
        "output": null, "error": null,
        "artifacts": [serde_json::to_value(&file).expect("an artifact is JSON")],
    });
    let run = store
        .create_run(NewRun::new(&agent.name, state))
        .await
        .expect("create");
    let rt = transcript_runtime(&store, &agent);
    let worker = spawn_worker(&rt);
    wait_done(&rt, run.id).await;
    worker.stop().await;

    assert_eq!(
        counting.calls(Method::PutBlob),
        1,
        "the file's bytes are sent on the first split only"
    );
    assert!(
        counting.calls(Method::BlobStored) >= 2,
        "the second split checks the digest instead of re-sending it"
    );
    assert_eq!(
        inner
            .get_blob(&adam_runtime::blob_id(SVG))
            .await
            .expect("read the blob"),
        Some(SVG.to_vec())
    );
}

/// A store that keeps no blobs cannot back a file reference: the split is refused, and the state
/// is written alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_without_blobs_clears_the_split() {
    let inner: DynStore = Arc::new(MemoryStore::new());
    let recording = Arc::new(common::recording::RecordingStore::new(inner.clone()).without_blobs());
    let store: DynStore = recording.clone();
    let agent = TranscriptAgent::new(&uniq("no-blob-store"), 1);
    let file =
        Artifact::file("chart", "image/svg+xml", "chart.svg", SVG.to_vec()).expect("a small file");
    let state = json!({
        "v": 1,
        "agent": {"messages": ["start"], "turns": 0},
        "inbox": [], "seq": 0, "attempt": 0, "rev": 0,
        "output": null, "error": null,
        "artifacts": [serde_json::to_value(&file).expect("an artifact is JSON")],
    });
    let run = store
        .create_run(NewRun::new(&agent.name, state))
        .await
        .expect("create");
    let rt = transcript_runtime(&store, &agent);
    let worker = spawn_worker(&rt);
    wait_done(&rt, run.id).await;
    worker.stop().await;

    assert!(
        recording
            .commits()
            .iter()
            .all(|update| update.head.is_none() && update.body.is_none()),
        "a store that keeps no blobs never gets a head naming a file"
    );
    assert_eq!(
        inner
            .get_blob(&adam_runtime::blob_id(SVG))
            .await
            .expect("read the blob"),
        None
    );
}
