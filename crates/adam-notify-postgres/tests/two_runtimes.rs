//! A front and a worker, each with its own pools, `PgNotify` and `Runtime`
//! over one PostgreSQL, the way two processes of the coder run. Every poll
//! interval is 30 s, so anything that happens within seconds happened because
//! a notification arrived.
//!
//! ```sh
//! ADAM_TEST_POSTGRES_URL=postgres://postgres:postgres@localhost:5432/adam_test \
//!     cargo test -p adam-notify-postgres --test two_runtimes
//! ```
//!
//! Skipped when the variable is unset, unless `ADAM_TEST_REQUIRE_DB=1` (then it
//! fails). Each test migrates its own tables (`adam_nt<8 hex>_`) and channels
//! (`adam_n<8 hex>_`) and drops the tables at the end.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{DynStore, RunId, RunStatus};
use adam_notify_postgres::PgNotify;
use adam_runtime::{
    Agent, AgentError, AgentStarter, BroadcastSink, Ctx, Delivery, Inbound, Notifier, RunEvent,
    RunView, Runtime, Signal, Transition,
};
use adam_store_postgres::PgStore;
use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, Row};
use tokio::sync::{Notify, oneshot};
use uuid::Uuid;

type StepResult = Result<Transition<Value>, AgentError>;
type StepFn = dyn for<'a> Fn(&'a mut Ctx, Value) -> BoxFuture<'a, StepResult> + Send + Sync;

/// An agent whose state is JSON (the start payload) and whose step is a closure.
#[derive(Clone)]
struct FnAgent {
    name: String,
    step: Arc<StepFn>,
}

fn fn_agent<F>(name: &str, f: F) -> FnAgent
where
    F: for<'a> Fn(&'a mut Ctx, Value) -> BoxFuture<'a, StepResult> + Send + Sync + 'static,
{
    FnAgent {
        name: name.to_owned(),
        step: Arc::new(f),
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

/// What a front registers: the name and the initial state, no `step`.
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

fn inbound() -> Inbound {
    Inbound::new("start", json!({}))
}

fn phase(state: &Value) -> u64 {
    state.get("phase").and_then(Value::as_u64).unwrap_or(0)
}

fn short() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// One database, its own tables and channels, and an admin pool for the test.
struct Env {
    url: String,
    tables: String,
    channels: String,
    admin: PgPool,
}

impl Env {
    async fn new() -> Option<Self> {
        let url = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL")?;
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect");
        Some(Self {
            url,
            tables: format!("adam_nt{}_", short()),
            channels: format!("adam_n{}_", short()),
            admin,
        })
    }

    async fn pool(&self, application_name: &str) -> PgPool {
        let options = PgConnectOptions::from_str(&self.url)
            .expect("url")
            .application_name(application_name);
        PgPoolOptions::new()
            .max_connections(6)
            .connect_with(options)
            .await
            .expect("connect")
    }

    async fn cleanup(&self) {
        let p = &self.tables;
        let drop = format!("DROP TABLE IF EXISTS {p}journal, {p}runs, {p}meta");
        let _ = sqlx::query(AssertSqlSafe(drop)).execute(&self.admin).await;
    }
}

/// One process: its pools, its `PgNotify`, its runtime, and (for a worker) the
/// loop that steps runs.
struct Node {
    rt: Runtime,
    notify: PgNotify,
    /// Where the events of the other process arrive.
    local: BroadcastSink,
    /// Application name of the pool behind `notify`, to find its backends.
    application_name: String,
    stops: Vec<oneshot::Sender<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

enum Role<'a> {
    Front(&'a str),
    Worker(&'a FnAgent),
}

async fn node(env: &Env, role: Role<'_>) -> Node {
    let application_name = format!("adam-nt-{}", short());
    let notify_pool = env.pool(&application_name).await;
    let store_pool = env.pool(&format!("{application_name}-store")).await;
    let store = PgStore::from_pool(store_pool)
        .with_table_prefix(&env.tables)
        .expect("table prefix");
    adam_core::Store::migrate(&store).await.expect("migrate");
    let store: DynStore = Arc::new(store);

    let local = BroadcastSink::default();
    let notify = PgNotify::new(notify_pool, local.clone())
        .with_channel_prefix(&env.channels)
        .expect("channel prefix");
    let builder = Runtime::builder(store)
        .event_sink(notify.event_sink())
        .notifier(notify.notifier())
        .poll_interval(Duration::from_secs(30));
    let (rt, is_worker) = match role {
        Role::Front(name) => (builder.starter(JsonStarter(name.to_owned())).build(), false),
        Role::Worker(agent) => (builder.agent(agent.clone()).build(), true),
    };

    let mut node = Node {
        rt: rt.clone(),
        notify: notify.clone(),
        local,
        application_name,
        stops: Vec::new(),
        tasks: Vec::new(),
    };
    let (stop, stopped) = oneshot::channel::<()>();
    node.stops.push(stop);
    node.tasks.push(tokio::spawn(async move {
        let stopped = async {
            let _ = stopped.await;
        };
        notify.run(stopped).await.expect("notify.run");
    }));
    node.notify.wait_listening().await;
    if is_worker {
        let (stop, stopped) = oneshot::channel::<()>();
        node.stops.push(stop);
        node.tasks.push(tokio::spawn(async move {
            rt.run_worker(async {
                let _ = stopped.await;
            })
            .await
            .expect("run_worker");
        }));
        // Let the worker find nothing and go idle on its 30 s poll.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    node
}

impl Node {
    async fn stop(self) {
        for stop in self.stops {
            let _ = stop.send(());
        }
        for task in self.tasks {
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("stops in time")
                .expect("task");
        }
    }
}

async fn wait_for(
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_done(rt: &Runtime, run: RunId, what: &str) -> RunView {
    wait_for(rt, run, Duration::from_secs(5), what, |v| {
        v.status == RunStatus::Done
    })
    .await
}

/// (a) A run the front starts, and a message it delivers, wake an idle worker
/// of another process despite its 30 s poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_and_a_deliver_wake_an_idle_worker() {
    let Some(env) = Env::new().await else { return };
    let name = format!("nt-wake-{}", short());
    let agent = fn_agent(&name, |ctx, state| {
        Box::pin(async move {
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
        })
    });
    let worker = node(&env, Role::Worker(&agent)).await;
    let front = node(&env, Role::Front(&name)).await;

    let run = front.rt.start(&name, inbound(), None).await.expect("start");
    wait_for(
        &front.rt,
        run,
        Duration::from_secs(5),
        "parked despite a 30s poll",
        |v| v.waiting,
    )
    .await;
    front.rt.deliver(run, inbound()).await.expect("deliver");
    let done = wait_done(&front.rt, run, "done after the deliver").await;
    assert_eq!(done.output, Some(json!(1)));

    // The worker is idle again: a second start wakes it just the same.
    let second = front
        .rt
        .start(&name, Inbound::new("start", json!({"phase": 1})), None)
        .await
        .expect("second start");
    wait_done(&front.rt, second, "done after a start to an idle worker").await;

    front.stop().await;
    worker.stop().await;
    env.cleanup().await;
}

/// (b) What a worker's step emits reaches a subscriber attached to the front's
/// sink exactly once, and before the status that follows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worker_events_reach_the_front_once() {
    let Some(env) = Env::new().await else { return };
    let name = format!("nt-events-{}", short());
    let agent = fn_agent(&name, |ctx, _state| {
        Box::pin(async move {
            ctx.emit(RunEvent::Progress {
                message: "halfway".into(),
            })
            .await;
            Ok(Transition::Park {
                state: json!({"phase": 1}),
                wake_at: None,
            })
        })
    });
    let worker = node(&env, Role::Worker(&agent)).await;
    let front = node(&env, Role::Front(&name)).await;

    let run = RunId::new();
    let mut sub = front.local.subscribe_run(run);
    front
        .rt
        .start_with_id(run, &name, inbound(), None)
        .await
        .expect("start");

    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout(deadline - Instant::now(), sub.recv())
            .await
            .expect("the parked status in time")
            .expect("the sink is open");
        let parked = matches!(
            event,
            RunEvent::Status {
                status: RunStatus::Parked,
                ..
            }
        );
        seen.push(event);
        if parked {
            break;
        }
    }
    // Nothing repeats afterwards either.
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(400), sub.recv()).await {
        seen.push(event);
    }

    let progress: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, RunEvent::Progress { message } if message == "halfway"))
        .map(|(i, _)| i)
        .collect();
    let parked: Vec<usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e,
                RunEvent::Status {
                    status: RunStatus::Parked,
                    ..
                }
            )
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(progress.len(), 1, "progress exactly once: {seen:#?}");
    assert_eq!(parked.len(), 1, "one parked status: {seen:#?}");
    assert!(progress[0] < parked[0], "progress first: {seen:#?}");

    front.stop().await;
    worker.stop().await;
    env.cleanup().await;
}

/// (c) A cancel issued by the front reaches the step the worker is running
/// within seconds, not after a 30 s poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_reaches_the_running_step() {
    let Some(env) = Env::new().await else { return };
    let name = format!("nt-cancel-{}", short());
    let started = Arc::new(Notify::new());
    let observed = Arc::new(Notify::new());
    let agent = fn_agent(&name, {
        let (started, observed) = (started.clone(), observed.clone());
        move |ctx, state| {
            let (started, observed) = (started.clone(), observed.clone());
            Box::pin(async move {
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
            })
        }
    });
    let worker = node(&env, Role::Worker(&agent)).await;
    let front = node(&env, Role::Front(&name)).await;

    let run = front.rt.start(&name, inbound(), None).await.expect("start");
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("the step starts on the worker");
    front
        .rt
        .cancel(run, "from the front")
        .await
        .expect("cancel");
    tokio::time::timeout(Duration::from_secs(2), observed.notified())
        .await
        .expect("the step sees the cancel within 2 s");
    let view = front.rt.view(run).await.expect("view").expect("run");
    assert_eq!(view.status, RunStatus::Failed);
    assert_eq!(view.error.as_deref(), Some("cancelled: from the front"));

    front.stop().await;
    worker.stop().await;
    env.cleanup().await;
}

/// (d) Killing the listener's connection makes subscribers resync, a run
/// started during the outage still completes, and signals flow again after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_connection_is_healed_with_a_resync() {
    let Some(env) = Env::new().await else { return };
    let name = format!("nt-reconnect-{}", short());
    let agent = fn_agent(&name, |_ctx, state| {
        Box::pin(async move {
            Ok(Transition::Done {
                output: state.clone(),
                state,
            })
        })
    });
    let worker = node(&env, Role::Worker(&agent)).await;
    let front = node(&env, Role::Front(&name)).await;
    let mut deliveries = worker.notify.notifier().subscribe();

    // Kill every backend of the worker's notification pool, the listener's
    // included, and start a run while it is down.
    let killed = sqlx::query(
        "SELECT pg_terminate_backend(pid) AS ok FROM pg_stat_activity
         WHERE application_name = $1 AND pid <> pg_backend_pid()",
    )
    .bind(&worker.application_name)
    .fetch_all(&env.admin)
    .await
    .expect("terminate")
    .iter()
    .filter(|row| row.get::<bool, _>("ok"))
    .count();
    assert!(killed >= 1, "the listener's backend was found and killed");
    let run = front.rt.start(&name, inbound(), None).await.expect("start");

    let resync = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(delivery) = deliveries.next().await {
            if delivery == Delivery::Resync {
                return;
            }
        }
        panic!("the subscription ended");
    })
    .await;
    assert!(
        resync.is_ok(),
        "a resync within 5 s of the connection dying"
    );
    worker.notify.wait_listening().await;
    wait_done(&front.rt, run, "the run started during the outage").await;

    // Listening again: a later signal still arrives.
    let later = Signal::Runnable {
        run: RunId::new(),
        agent: "someone".into(),
    };
    front.notify.notifier().publish(later.clone()).await;
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(delivery) = deliveries.next().await {
            if delivery == Delivery::Signal(later.clone()) {
                return true;
            }
        }
        false
    })
    .await;
    assert_eq!(got, Ok(true), "a signal after the reconnect arrives");

    front.stop().await;
    worker.stop().await;
    env.cleanup().await;
}
