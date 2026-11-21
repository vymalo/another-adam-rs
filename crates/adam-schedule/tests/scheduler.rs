//! The `adam.schedule` agent and `reconcile` on the memory store with a manual clock (ADR 0060,
//! slice 4): two workers fire once, a restart loses nothing, an outage fires one with the count, a
//! removed schedule ends, a changed one restarts, one changed back starts again, a fired one-shot
//! is not fired again, and each firing records its trigger.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use adam_agent_fs::Schedule;
use adam_core::{ConversationScope, DynStore, MemoryStore, RunQuery, RunRecord, RunStatus};
use adam_runtime::{
    Agent, AgentError, Clock, Ctx, Inbound, ManualClock, Runtime, RuntimeError, Transition,
};
use adam_schedule::{
    CHANGED_REASON, REMOVED_REASON, Reconciled, SchedulerAgent, SchedulerState, reconcile,
    scheduler_agent_name, trigger_of,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const ROOT: &str = "coder";

/// A root agent: it finishes at once with the message it was started with.
#[derive(Clone)]
struct Root(&'static str);

#[async_trait]
impl Agent for Root {
    type State = Value;

    fn name(&self) -> &str {
        self.0
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        Ok(json!({ "kind": input.kind, "payload": input.payload }))
    }

    async fn step(&self, _ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        Ok(Transition::Done {
            output: state.clone(),
            state,
        })
    }
}

static WORKERS: AtomicUsize = AtomicUsize::new(0);

fn runtime(store: &DynStore, clock: &ManualClock) -> Runtime {
    runtime_of(ROOT, store, clock)
}

/// The runtime of one agent's process: the agent and its scheduler.
fn runtime_of(agent: &'static str, store: &DynStore, clock: &ManualClock) -> Runtime {
    let n = WORKERS.fetch_add(1, Ordering::SeqCst);
    Runtime::builder(store.clone())
        .agent(SchedulerAgent::new(agent))
        .agent(Root(agent))
        .worker_id(format!("w{n}"))
        .clock(clock.clone())
        .poll_interval(Duration::from_millis(10))
        .lease_ttl(Duration::from_secs(10))
        .build()
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

fn schedule(name: &str, cron: &str, after: Option<&str>, prompt: &str) -> Schedule {
    Schedule {
        name: name.into(),
        cron: cron.into(),
        at: None,
        after: after.map(Into::into),
        timezone: "UTC".into(),
        agent: ROOT.into(),
        prompt: prompt.into(),
        path: PathBuf::from(format!("agent/schedules/{name}.md")),
    }
}

async fn runs(store: &DynStore, agent: &str, prefix: &str) -> Vec<RunRecord> {
    store
        .list_runs(&RunQuery::new(
            agent,
            ConversationScope::Prefix(prefix.to_owned()),
            500,
        ))
        .await
        .unwrap()
}

async fn schedulers(store: &DynStore, name: &str) -> Vec<RunRecord> {
    runs(
        store,
        &scheduler_agent_name(ROOT),
        &format!("adam.schedule:{ROOT}/{name}"),
    )
    .await
    .into_iter()
    .filter(|r| r.conversation_id.as_deref() == Some(&format!("adam.schedule:{ROOT}/{name}")))
    .collect()
}

async fn firings(store: &DynStore, name: &str) -> Vec<RunRecord> {
    runs(store, ROOT, &format!("adam.schedule:{ROOT}/{name}@")).await
}

/// Wait until `check` holds, for at most ten seconds.
async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn parked(store: &DynStore, name: &str) {
    until("the scheduler to park", || async {
        schedulers(store, name)
            .await
            .iter()
            .any(|r| r.status == RunStatus::Parked)
    })
    .await;
}

async fn state_of(rt: &Runtime, rec: &RunRecord) -> SchedulerState {
    let view = rt.view(rec.id).await.unwrap().unwrap();
    serde_json::from_value(view.state).unwrap()
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn an_after_schedule_fires_once_records_its_trigger_and_ends() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = runtime(&store, &clock);
    let worker = spawn_worker(&rt);
    let soon = schedule("soon", "", Some("30m"), "Follow up.");

    let first = reconcile(&rt, ROOT, std::slice::from_ref(&soon))
        .await
        .unwrap();
    assert_eq!(
        first,
        Reconciled {
            started: names(&["soon"]),
            ..Reconciled::default()
        }
    );
    parked(&store, "soon").await;
    let anchor = state_of(&rt, &schedulers(&store, "soon").await[0])
        .await
        .anchor;

    clock.advance(Duration::from_secs(31 * 60));
    until("the firing to finish", || async {
        firings(&store, "soon")
            .await
            .iter()
            .any(|r| r.status == RunStatus::Done)
    })
    .await;
    until("the scheduler to end", || async {
        schedulers(&store, "soon").await[0].status == RunStatus::Done
    })
    .await;

    let fired = firings(&store, "soon").await;
    assert_eq!(fired.len(), 1);
    let trigger = trigger_of(fired[0].conversation_id.as_deref().unwrap()).unwrap();
    let due = adam_core::time::Timestamp(anchor.0 + jiff::SignedDuration::from_mins(30));
    assert_eq!(
        (trigger.agent.as_str(), trigger.name.as_str(), trigger.due),
        (ROOT, "soon", due)
    );
    assert_eq!(fired[0].id, adam_schedule::fired_run_id(ROOT, "soon", due));
    let output = rt.view(fired[0].id).await.unwrap().unwrap().output.unwrap();
    assert_eq!(output["kind"], "message");
    assert_eq!(output["payload"]["text"], "Follow up.");
    assert_eq!(output["payload"]["schedule"]["name"], "soon");
    assert_eq!(output["payload"]["schedule"]["missed"], 0);
    // Nobody answers a firing, nor its children: the key they inherit is set.
    assert_eq!(
        output["payload"]["context"][adam_runtime::UNATTENDED_CONTEXT_KEY],
        true
    );

    let state = state_of(&rt, &schedulers(&store, "soon").await[0]).await;
    assert_eq!(state.last_due, Some(due));
    assert_eq!(state.fired.len(), 1);
    assert!(state.fired[0].created);

    // A fired one-shot is kept, not started again, even much later.
    clock.advance(Duration::from_secs(24 * 3600));
    let again = reconcile(&rt, ROOT, std::slice::from_ref(&soon))
        .await
        .unwrap();
    assert_eq!(
        again,
        Reconciled {
            kept: names(&["soon"]),
            ..Reconciled::default()
        }
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(schedulers(&store, "soon").await.len(), 1);
    assert_eq!(firings(&store, "soon").await.len(), 1);
    worker.stop().await;
}

#[tokio::test]
async fn an_outage_fires_the_latest_due_time_once_with_the_count() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = runtime(&store, &clock);
    let hourly = schedule("hourly", "0 * * * *", None, "Report.");
    reconcile(&rt, ROOT, std::slice::from_ref(&hourly))
        .await
        .unwrap();

    // The first worker parks the scheduler and stops: the process is gone.
    let worker = spawn_worker(&rt);
    parked(&store, "hourly").await;
    worker.stop().await;
    drop(rt);

    // Five hours later another process starts: the schedule is kept, and fires once.
    clock.advance(Duration::from_secs(5 * 3600));
    let rt = runtime(&store, &clock);
    let done = reconcile(&rt, ROOT, std::slice::from_ref(&hourly))
        .await
        .unwrap();
    assert_eq!(
        done,
        Reconciled {
            kept: names(&["hourly"]),
            ..Reconciled::default()
        }
    );
    let worker = spawn_worker(&rt);
    until("the firing", || async {
        !firings(&store, "hourly").await.is_empty()
    })
    .await;
    parked(&store, "hourly").await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let fired = firings(&store, "hourly").await;
    assert_eq!(fired.len(), 1, "one firing for the five due times");
    let state = state_of(&rt, &schedulers(&store, "hourly").await[0]).await;
    assert_eq!(state.fired.len(), 1);
    assert_eq!(state.fired[0].missed, 4);
    assert_eq!(state.fired[0].run, fired[0].id);
    worker.stop().await;
}

#[tokio::test]
async fn two_processes_fire_each_due_time_once() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let (a, b) = (runtime(&store, &clock), runtime(&store, &clock));
    let hourly = schedule("hourly", "0 * * * *", None, "Report.");

    let (ra, rb) = tokio::join!(
        reconcile(&a, ROOT, std::slice::from_ref(&hourly)),
        reconcile(&b, ROOT, std::slice::from_ref(&hourly)),
    );
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    assert_eq!(
        ra.started.len() + rb.started.len(),
        1,
        "one of the two starts the scheduler: {ra:?} {rb:?}"
    );
    let (wa, wb) = (spawn_worker(&a), spawn_worker(&b));
    parked(&store, "hourly").await;

    for hour in 1..=3 {
        clock.advance(Duration::from_secs(3600));
        until("the firing", || async {
            firings(&store, "hourly").await.len() >= hour
        })
        .await;
        parked(&store, "hourly").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let fired = firings(&store, "hourly").await;
    let mut dues: Vec<_> = fired
        .iter()
        .map(|r| {
            trigger_of(r.conversation_id.as_deref().unwrap())
                .unwrap()
                .due
        })
        .collect();
    dues.sort();
    dues.dedup();
    assert_eq!(fired.len(), 3);
    assert_eq!(dues.len(), 3, "no due time fired twice");
    let open = schedulers(&store, "hourly").await;
    assert_eq!(open.iter().filter(|r| r.status.is_open()).count(), 1);
    // The scheduler itself fired three times and moved on each time.
    let open_rec = open.iter().find(|r| r.status.is_open()).unwrap();
    let state = state_of(&a, open_rec).await;
    assert_eq!(state.fired.len(), 3);
    assert_eq!(state.last_due, dues.last().copied());
    assert!(state.fired.iter().all(|f| f.created && f.error.is_none()));
    wa.stop().await;
    wb.stop().await;
}

#[tokio::test]
async fn removed_changed_and_changed_back() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = runtime(&store, &clock);
    let daily = schedule("daily", "0 9 * * *", None, "Digest.");
    let edited = schedule("daily", "0 9 * * *", None, "Digest, shorter.");
    let other = schedule("weekly", "0 8 * * MON", None, "Status.");

    let r = reconcile(&rt, ROOT, &[daily.clone(), other.clone()])
        .await
        .unwrap();
    assert_eq!(r.started, names(&["daily", "weekly"]));

    // Changed: the old scheduler is cancelled, a new one started.
    let r = reconcile(&rt, ROOT, &[edited.clone(), other.clone()])
        .await
        .unwrap();
    assert_eq!(
        r,
        Reconciled {
            started: names(&["daily"]),
            kept: names(&["weekly"]),
            cancelled: names(&["daily"]),
        }
    );
    // Changed back: the first spec's run is cancelled for good, so a third one starts.
    let r = reconcile(&rt, ROOT, &[daily.clone(), other.clone()])
        .await
        .unwrap();
    assert_eq!(r.started, names(&["daily"]));
    assert_eq!(r.cancelled, names(&["daily"]));
    let runs = schedulers(&store, "daily").await;
    assert_eq!(runs.len(), 3);
    assert_eq!(runs.iter().filter(|r| r.status.is_open()).count(), 1);
    for rec in runs.iter().filter(|r| r.status == RunStatus::Failed) {
        let error = rt
            .view(rec.id)
            .await
            .unwrap()
            .unwrap()
            .error
            .unwrap_or_default();
        assert!(error.contains(CHANGED_REASON), "{error}");
    }

    // Removed: its scheduler is cancelled; the other is untouched.
    let r = reconcile(&rt, ROOT, std::slice::from_ref(&daily))
        .await
        .unwrap();
    assert_eq!(
        r,
        Reconciled {
            kept: names(&["daily"]),
            cancelled: names(&["weekly"]),
            ..Reconciled::default()
        }
    );
    let weekly = &schedulers(&store, "weekly").await[0];
    assert_eq!(weekly.status, RunStatus::Failed);
    let error = rt
        .view(weekly.id)
        .await
        .unwrap()
        .unwrap()
        .error
        .unwrap_or_default();
    assert!(error.contains(REMOVED_REASON), "{error}");
    // Nothing to do twice.
    let r = reconcile(&rt, ROOT, std::slice::from_ref(&daily))
        .await
        .unwrap();
    assert_eq!(
        r,
        Reconciled {
            kept: names(&["daily"]),
            ..Reconciled::default()
        }
    );
}

#[tokio::test]
async fn a_bad_schedule_changes_nothing_and_other_agents_are_left_alone() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = runtime(&store, &clock);
    let mut bad = schedule("bad", "0 9 * * *", None, "Go.");
    bad.timezone = "Mars/Olympus".into();
    let good = schedule("good", "0 9 * * *", None, "Go.");
    let err = reconcile(&rt, ROOT, &[good.clone(), bad])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("is not an IANA time zone"),
        "{err}"
    );
    assert!(schedulers(&store, "good").await.is_empty());

    // Another agent's schedules are neither started nor cancelled by this agent's reconcile.
    let mut chat = good.clone();
    chat.agent = "chat".into();
    let r = adam_schedule::reconcile_store(store.clone(), "chat", std::slice::from_ref(&chat))
        .await
        .unwrap();
    assert_eq!(r.started, names(&["good"]));
    // Given only the chat's schedule, the coder's reconcile does nothing: it is not its agent's.
    let r = reconcile(&rt, ROOT, std::slice::from_ref(&chat))
        .await
        .unwrap();
    assert_eq!(r, Reconciled::default());
    let chats = runs(&store, &scheduler_agent_name("chat"), "adam.schedule:chat/").await;
    assert_eq!(chats.len(), 1);
    assert!(chats[0].status.is_open());
}

#[tokio::test]
async fn a_message_wakes_a_scheduler_without_firing() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = runtime(&store, &clock);
    let worker = spawn_worker(&rt);
    let daily = schedule("daily", "0 9 * * *", None, "Digest.");
    reconcile(&rt, ROOT, std::slice::from_ref(&daily))
        .await
        .unwrap();
    parked(&store, "daily").await;
    let rec = schedulers(&store, "daily").await.remove(0);
    let before = rec.version;
    rt.deliver(rec.id, Inbound::new("message", json!({"text": "fire now"})))
        .await
        .unwrap();
    until("the scheduler to step and park again", || async {
        let r = schedulers(&store, "daily").await.remove(0);
        r.version > before && r.status == RunStatus::Parked
    })
    .await;
    assert!(firings(&store, "daily").await.is_empty());
    // And it stays parked: the message was taken, so it does not wake it again and again.
    let settled = schedulers(&store, "daily").await.remove(0).version;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later = schedulers(&store, "daily").await.remove(0);
    assert_eq!((later.version, later.status), (settled, RunStatus::Parked));
    // The message is read and gone, not left pending in the run.
    assert_eq!(rt.view(later.id).await.unwrap().unwrap().pending_inbox, 0);
    worker.stop().await;
}

#[tokio::test]
async fn a_removed_schedule_added_back_unchanged_starts_again() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = runtime(&store, &clock);
    let daily = schedule("daily", "0 9 * * *", None, "Digest.");
    reconcile(&rt, ROOT, std::slice::from_ref(&daily))
        .await
        .unwrap();
    let r = reconcile(&rt, ROOT, &[]).await.unwrap();
    assert_eq!(r.cancelled, names(&["daily"]));
    // The same spec as the cancelled run: a cancelled scheduler is never kept.
    let r = reconcile(&rt, ROOT, std::slice::from_ref(&daily))
        .await
        .unwrap();
    assert_eq!(
        r,
        Reconciled {
            started: names(&["daily"]),
            ..Reconciled::default()
        }
    );
    let runs = schedulers(&store, "daily").await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs.iter().filter(|r| r.status.is_open()).count(), 1);
}

#[tokio::test]
async fn an_at_that_passed_before_the_scheduler_started_fires_late_once() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = runtime(&store, &clock);
    let worker = spawn_worker(&rt);
    let mut past = schedule("launch", "", None, "Announce.");
    past.at = Some("2020-01-01T00:00:00Z".into());
    reconcile(&rt, ROOT, std::slice::from_ref(&past))
        .await
        .unwrap();
    until("the scheduler to end", || async {
        schedulers(&store, "launch")
            .await
            .first()
            .is_some_and(|r| r.status == RunStatus::Done)
    })
    .await;
    let fired = firings(&store, "launch").await;
    assert_eq!(fired.len(), 1);
    let due = trigger_of(fired[0].conversation_id.as_deref().unwrap())
        .unwrap()
        .due;
    assert_eq!(due.0.to_string(), "2020-01-01T00:00:00Z");
    let state = state_of(&rt, &schedulers(&store, "launch").await[0]).await;
    assert_eq!(state.fired.len(), 1);
    worker.stop().await;
}

/// A runtime that cannot start the root agent: the scheduler records the refusal and goes on to
/// the next due time; it does not fail.
#[tokio::test]
async fn a_refused_firing_is_recorded_and_the_schedule_goes_on() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let rt = Runtime::builder(store.clone())
        .agent(SchedulerAgent::new(ROOT))
        .worker_id("schedules-only")
        .clock(clock.clone())
        .poll_interval(Duration::from_millis(10))
        .build();
    let worker = spawn_worker(&rt);
    let hourly = schedule("hourly", "0 * * * *", None, "Report.");
    reconcile(&rt, ROOT, std::slice::from_ref(&hourly))
        .await
        .unwrap();
    parked(&store, "hourly").await;
    for hour in 1..=2 {
        clock.advance(Duration::from_secs(3600));
        until("the refusal to be recorded", || async {
            let rec = schedulers(&store, "hourly").await.remove(0);
            rec.status == RunStatus::Parked
                && serde_json::from_value::<SchedulerState>(
                    rt.view(rec.id).await.unwrap().unwrap().state,
                )
                .unwrap()
                .fired
                .len()
                    == hour
        })
        .await;
    }
    let rec = schedulers(&store, "hourly").await.remove(0);
    let state = state_of(&rt, &rec).await;
    assert!(state.fired.iter().all(|f| !f.created), "{state:?}");
    let error = state.fired[0].error.as_deref().unwrap_or_default();
    assert!(error.contains("unknown agent"), "{error}");
    assert!(firings(&store, "hourly").await.is_empty());
    worker.stop().await;
}

/// A start the store fails with a retryable error is tried again soon, and fires once.
#[tokio::test]
async fn a_firing_the_store_fails_is_tried_again_and_fires_once() {
    let memory: DynStore = Arc::new(MemoryStore::new());
    let faulty = Arc::new(adam_store_testkit::fault::FaultyStore::new(memory));
    let store: DynStore = faulty.clone();
    let clock = ManualClock::new();
    // Start 20 s before an hour, the window where a retry 30 s later passes the next due time.
    let into_hour = clock.now().0.as_second().rem_euclid(3600);
    clock.advance(Duration::from_secs(
        u64::try_from((3600 - 20 - into_hour).rem_euclid(3600)).expect("non-negative"),
    ));
    let rt = runtime(&store, &clock);
    let worker = spawn_worker(&rt);
    let hourly = schedule("hourly", "0 * * * *", None, "Report.");
    reconcile(&rt, ROOT, std::slice::from_ref(&hourly))
        .await
        .unwrap();
    parked(&store, "hourly").await;
    let state = state_of(&rt, &schedulers(&store, "hourly").await[0]).await;
    let spec = adam_schedule::ScheduleSpec::from_schedule(&hourly).unwrap();
    let due = spec.due_after(state.anchor, state.anchor).unwrap();
    let run = adam_schedule::fired_run_id(ROOT, "hourly", due);
    // Only the firing creates a run from here on: fail the next one.
    faulty.fail(adam_store_testkit::fault::Method::CreateRun, 1);

    // To just past the due time, not a whole hour on: the clock starts at the wall clock, and the
    // retry below must still come before the next due time, or it fires that one instead (a run
    // begun 31 s before an hour did).
    let wait = due.0.duration_since(clock.now().0) + jiff::SignedDuration::from_secs(1);
    clock.advance(Duration::try_from(wait).expect("the due time is ahead"));
    until("the failed try", || async {
        faulty.injected(adam_store_testkit::fault::Method::CreateRun) == 1
    })
    .await;
    parked(&store, "hourly").await;
    assert!(firings(&store, "hourly").await.is_empty());
    clock.advance(adam_schedule::FIRE_RETRY + Duration::from_secs(1));
    until("the firing", || async {
        !firings(&store, "hourly").await.is_empty()
    })
    .await;
    parked(&store, "hourly").await;

    let fired = firings(&store, "hourly").await;
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].id, run);
    let state = state_of(&rt, &schedulers(&store, "hourly").await[0]).await;
    assert_eq!(state.fired.len(), 1);
    assert!(state.fired[0].created && state.fired[0].error.is_none());
    assert_eq!(state.last_due, Some(due));
    worker.stop().await;
}

/// What a binary's component does at start: reconcile over the store alone, while the runtime that
/// steps the schedulers is another one.
#[tokio::test]
async fn reconcile_store_starts_schedulers_another_runtime_steps() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let soon = schedule("soon", "", Some("1m"), "Go.");
    let r = adam_schedule::reconcile_store(store.clone(), ROOT, std::slice::from_ref(&soon))
        .await
        .unwrap();
    assert_eq!(r.started, names(&["soon"]));
    let rt = runtime(&store, &clock);
    let worker = spawn_worker(&rt);
    parked(&store, "soon").await;
    clock.advance(Duration::from_secs(120));
    until("the firing", || async {
        !firings(&store, "soon").await.is_empty()
    })
    .await;
    worker.stop().await;
    // Removing every schedule cancels the scheduler: the component runs with none, too.
    let r = adam_schedule::reconcile_store(store.clone(), ROOT, &[])
        .await
        .unwrap();
    assert_eq!(r, Reconciled::default(), "a Done one-shot is not cancelled");
}

/// Two agents share one database, each in a process of its own (another-agentic-system runs chat,
/// researcher and browser so): each process steps its own agent's schedulers only, and every
/// firing reaches its own agent.
#[tokio::test]
async fn agents_that_share_a_database_each_fire_their_own_schedules() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let clock = ManualClock::new();
    let (coder, chat) = (
        runtime_of(ROOT, &store, &clock),
        runtime_of("chat", &store, &clock),
    );
    let mut theirs = schedule("hourly", "0 * * * *", None, "Chat report.");
    theirs.agent = "chat".into();
    let ours = schedule("hourly", "0 * * * *", None, "Coder report.");
    reconcile(&coder, ROOT, std::slice::from_ref(&ours))
        .await
        .unwrap();
    reconcile(&chat, "chat", std::slice::from_ref(&theirs))
        .await
        .unwrap();
    let (wc, wh) = (spawn_worker(&coder), spawn_worker(&chat));
    parked(&store, "hourly").await;

    for hour in 1..=2 {
        clock.advance(Duration::from_secs(3600));
        until("both agents' firings", || async {
            firings(&store, "hourly").await.len() >= hour
                && runs(&store, "chat", "adam.schedule:chat/hourly@")
                    .await
                    .len()
                    >= hour
        })
        .await;
    }
    wc.stop().await;
    wh.stop().await;
    for (agent, prompt) in [(ROOT, "Coder report."), ("chat", "Chat report.")] {
        let fired = runs(&store, agent, &format!("adam.schedule:{agent}/hourly@")).await;
        assert_eq!(fired.len(), 2, "{agent}");
        for rec in &fired {
            let view = coder.view(rec.id).await.unwrap().unwrap();
            assert_eq!(view.state["payload"]["text"], prompt, "{agent}");
        }
        let scheduler = runs(
            &store,
            &scheduler_agent_name(agent),
            &format!("adam.schedule:{agent}/"),
        )
        .await;
        assert_eq!(scheduler.len(), 1, "{agent}");
        let state = state_of(&coder, &scheduler[0]).await;
        assert!(
            state.fired.iter().all(|f| f.created && f.error.is_none()),
            "{agent}: {state:?}"
        );
    }
}

/// The store is not reachable at first: `reconcile_until_stopped` tries again, reconciles, and
/// returns only when it is stopped.
#[tokio::test]
async fn the_component_tries_a_store_fault_again_and_waits_for_the_stop() {
    use adam_store_testkit::fault::{FaultyStore, Method};
    let memory: DynStore = Arc::new(MemoryStore::new());
    let faulty = Arc::new(FaultyStore::new(memory.clone()));
    faulty.fail(Method::ListRuns, 1);
    let stop = tokio_util::sync::CancellationToken::new();
    let soon = schedule("soon", "", Some("1h"), "Go.");
    let task = tokio::spawn(adam_schedule::reconcile_until_stopped(
        faulty.clone(),
        ROOT.into(),
        vec![soon],
        stop.clone(),
    ));
    // The memory store itself, so that this reading spends no fault.
    until("the scheduler", || async {
        !schedulers(&memory, "soon").await.is_empty()
    })
    .await;
    assert_eq!(faulty.injected(Method::ListRuns), 1);
    assert!(!task.is_finished(), "it waits for the stop");
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("it stops in time")
        .expect("the task")
        .expect("no error");
}

/// A stop while the store keeps failing ends the component at once, with no error; a failure no
/// retry mends is its error.
#[tokio::test]
async fn the_component_stops_while_it_waits_and_returns_what_no_retry_mends() {
    use adam_store_testkit::fault::{FaultyStore, Method};
    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    faulty.fail_always(Method::ListRuns);
    let stop = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(adam_schedule::reconcile_until_stopped(
        faulty.clone(),
        ROOT.into(),
        vec![schedule("soon", "", Some("1h"), "Go.")],
        stop.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("it stops at once")
        .expect("the task")
        .expect("no error");

    let mut bad = schedule("bad", "0 9 * * *", None, "Go.");
    bad.timezone = "Mars/Olympus".into();
    let err = adam_schedule::reconcile_until_stopped(
        Arc::new(MemoryStore::new()),
        ROOT.into(),
        vec![bad],
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, adam_schedule::ReconcileError::Schedule(_)),
        "{err}"
    );
}
