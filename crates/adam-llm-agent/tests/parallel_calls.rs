//! The tool calls of one model turn run at the same time (ADR 0049, slices P1 to P4): up to the
//! limit, conflicts keep the model's order, the journal is the sequential one, and the results go back
//! in call order.
//!
//! The tool here, `probe`, declares what a call shares from its own arguments, so each case says
//! what runs beside what.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, Tool, ToolAccess, ToolCtx, ToolError, ToolOutput, Use, user_message,
};
use adam_model::{Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{Artifact, CollectingSink, RetryPolicy, RunEvent, RunView, Runtime, StepState};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

/// What the probes did, in the order it happened.
#[derive(Default)]
struct Log {
    events: Mutex<Vec<String>>,
    running: AtomicUsize,
    max_running: AtomicUsize,
    /// Calls that began, by call id.
    starts: Mutex<HashMap<String, usize>>,
    /// Calls that ran to their end, by call id.
    ends: Mutex<HashMap<String, usize>>,
    /// The arguments each call was given, by call id.
    args: Mutex<HashMap<String, Value>>,
}

impl Log {
    fn count(map: &Mutex<HashMap<String, usize>>, id: &str) -> usize {
        map.lock().unwrap().get(id).copied().unwrap_or(0)
    }

    fn starts(&self, id: &str) -> usize {
        Self::count(&self.starts, id)
    }

    fn ends(&self, id: &str) -> usize {
        Self::count(&self.ends, id)
    }

    fn position(&self, event: &str) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .position(|e| e == event)
            .unwrap_or_else(|| panic!("no event {event}: {:?}", self.events.lock().unwrap()))
    }

    fn max_running(&self) -> usize {
        self.max_running.load(SeqCst)
    }
}

/// A tool whose arguments say what the call does:
/// `ms` (how long it takes), `use` (`independent`, `r:<scope>`, `w:<scope>`; none means the tool
/// declares nothing and runs alone), `out` (a fixed answer), `hang` and `transient` (stall and fail while the
/// switch is armed), `ask` (end with a question although it declared nothing of the kind).
struct Probe {
    name: &'static str,
    log: Arc<Log>,
    /// While set, a call that says `hang` never returns and one that says `transient` fails transiently.
    armed: Arc<AtomicBool>,
    /// The call stops when the run is cancelled.
    heeds_cancel: bool,
    /// For a call that says `poison`: where it writes a journal entry that disagrees with its own.
    store: Option<DynStore>,
    /// What `files` calls saw of the room for files, by call id.
    files_seen: Arc<Mutex<Vec<(String, u64)>>>,
}

impl Probe {
    fn new(log: &Arc<Log>) -> Self {
        Self {
            name: "probe",
            log: log.clone(),
            armed: Arc::new(AtomicBool::new(false)),
            heeds_cancel: true,
            store: None,
            files_seen: Arc::default(),
        }
    }
}

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "probe".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn access(&self, args: &Value) -> ToolAccess {
        match args.get("use").and_then(Value::as_str) {
            Some("independent") => ToolAccess::Independent,
            Some(other) => match other.split_once(':') {
                Some(("r", scope)) => ToolAccess::Uses(vec![Use::read(scope)]),
                Some(("w", scope)) => ToolAccess::Uses(vec![Use::write(scope)]),
                _ => ToolAccess::Alone,
            },
            None => ToolAccess::Alone,
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let id = ctx.call_id().to_owned();
        let log = &self.log;
        log.events.lock().unwrap().push(format!("start:{id}"));
        log.args.lock().unwrap().insert(id.clone(), args.clone());
        *log.starts.lock().unwrap().entry(id.clone()).or_default() += 1;
        let now = log.running.fetch_add(1, SeqCst) + 1;
        log.max_running.fetch_max(now, SeqCst);
        if let Some(want) = args.get("files").and_then(Value::as_u64) {
            // Ask for the room before the first await, as the calls of a segment start in order.
            self.files_seen
                .lock()
                .unwrap()
                .push((id.clone(), ctx.files_left()));
            ctx.spend_files(want);
        }
        let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(0);
        let sleep = tokio::time::sleep(Duration::from_millis(ms));
        let stopped = if self.heeds_cancel {
            tokio::select! {
                () = sleep => false,
                () = ctx.cancelled() => true,
            }
        } else {
            sleep.await;
            false
        };
        log.running.fetch_sub(1, SeqCst);
        if stopped {
            return Err(ToolError::Permanent("stopped".into()));
        }
        if self.armed.load(SeqCst) {
            if args.get("hang").and_then(Value::as_bool) == Some(true) {
                // Never returns: the process is killed while this call runs.
                std::future::pending::<()>().await;
            }
            if args.get("transient").and_then(Value::as_bool) == Some(true) {
                self.armed.store(false, SeqCst);
                return Err(ToolError::Transient("try again".into()));
            }
        }
        if args.get("ask").and_then(Value::as_bool) == Some(true) {
            return Err(ToolError::needs_input("which one?"));
        }
        if args.get("poison").and_then(Value::as_bool) == Some(true)
            && let Some(store) = &self.store
        {
            // The journal now disagrees with this call's own step: its record is refused.
            store
                .journal_put(
                    ctx.run_id(),
                    adam_core::JournalEntry::ok(1, "other", json!(1)),
                )
                .await
                .expect("poison the journal");
        }
        if args.get("report").and_then(Value::as_bool) == Some(true) {
            let mut out = ToolOutput::text("reported");
            out.reported = vec![RunId::new()];
            return Ok(out);
        }
        log.events.lock().unwrap().push(format!("end:{id}"));
        *log.ends.lock().unwrap().entry(id.clone()).or_default() += 1;
        Ok(ToolOutput::text(
            args.get("out")
                .and_then(Value::as_str)
                .map_or_else(|| format!("ok:{id}"), str::to_owned),
        ))
    }
}

/// A tool that asks the person.
struct Asker;

#[async_trait]
impl Tool for Asker {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask".into(),
            description: "ask".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn asks_user(&self) -> bool {
        true
    }

    /// What it says is not heeded: a tool that asks runs alone.
    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input("which environment?"))
    }
}

/// A tool that shares a file of the size it is asked for.
struct Filer;

#[async_trait]
impl Tool for Filer {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "filer".into(),
            description: "filer".into(),
            parameters: json!({"type": "object"}),
        }
    }

    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let kib = args.get("kib").and_then(Value::as_u64).unwrap_or(1) as usize;
        let file = Artifact::file(
            format!("f-{}", ctx.call_id()),
            "application/octet-stream",
            format!("{}.bin", ctx.call_id()),
            vec![7; kib * 1024],
        )
        .expect("a file within the cap");
        if args.get("error").and_then(Value::as_bool) == Some(true) {
            return Ok(ToolOutput::error("bad").with_artifact(file));
        }
        Ok(ToolOutput::text("shared").with_artifact(file))
    }
}

struct Harness {
    mock: Arc<MockModel>,
    sink: CollectingSink,
    store: DynStore,
}

impl Harness {
    fn new() -> Self {
        Self {
            mock: Arc::new(MockModel::new()),
            sink: CollectingSink::new(),
            store: Arc::new(MemoryStore::new()),
        }
    }

    fn agent(
        &self,
        limit: usize,
        configure: impl FnOnce(adam_llm_agent::LlmAgentBuilder) -> adam_llm_agent::LlmAgentBuilder,
    ) -> LlmAgent {
        configure(LlmAgent::builder("llm", self.mock.clone(), "test-model").max_parallel(limit))
            .build()
    }

    fn runtime(&self, agent: LlmAgent, ttl: Duration) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(agent)
            .event_sink(self.sink.clone())
            .worker_id(format!("w-{}", RunId::new()))
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(ttl)
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
                multiplier: 1.0,
            })
            .build()
    }

    /// Run one task to its end on a fresh runtime.
    async fn run(&self, limit: usize, text: &str, log: &Arc<Log>) -> (Runtime, RunId, RunView) {
        let agent = self.agent(limit, |b| b.tool(Probe::new(log)));
        let rt = self.runtime(agent, Duration::from_secs(10));
        let run = rt
            .start("llm", user_message(text), None)
            .await
            .expect("start");
        let worker = Worker::spawn(&rt);
        let view = wait_terminal(&rt, run).await;
        worker.stop().await;
        (rt, run, view)
    }

    async fn journal(&self, run: RunId) -> Vec<(u64, String)> {
        self.store
            .journal_list(run)
            .await
            .expect("journal")
            .into_iter()
            .map(|e| (e.seq, e.name))
            .collect()
    }

    fn steps(&self, run: RunId) -> Vec<(String, StepState)> {
        self.sink
            .events_for(run)
            .into_iter()
            .filter_map(|e| match e {
                RunEvent::Step(step) => Some((step.id, step.state)),
                _ => None,
            })
            .collect()
    }
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
}

impl Worker {
    fn spawn(rt: &Runtime) -> Self {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = rt.clone();
        let handle = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        });
        Self { stop, handle }
    }

    /// The process dies: the worker's task is aborted, and with it every transition it runs.
    async fn kill(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        self.handle
            .await
            .expect("worker task")
            .expect("worker result");
    }
}

async fn wait_terminal(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn conversation(rt: &Runtime, view: &RunView) -> Conversation {
    let state = rt.state(view.id).await.expect("state").expect("the run");
    serde_json::from_value(state).expect("state is a Conversation")
}

async fn tool_answers(rt: &Runtime, view: &RunView) -> Vec<(String, bool)> {
    conversation(rt, view)
        .await
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// The calls of one turn, `n` of them `c1 ..`, each of `ms` and the use `use_`, with a tag that
/// makes their arguments differ.
fn turn(n: usize, ms: u64, use_: &str) -> Vec<ToolCall> {
    (1..=n)
        .map(|i| {
            call(
                &format!("c{i}"),
                "probe",
                json!({"ms": ms, "use": use_, "tag": i}),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn four_calls_take_the_time_of_one_and_at_a_limit_of_one_the_time_of_four() {
    for (limit, expected) in [(10usize, 1_000u128), (1, 4_000)] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(turn(4, 1_000, "independent"));
        h.mock.push_text("done");
        let started = tokio::time::Instant::now();
        let (_rt, _, view) = h.run(limit, "go", &log).await;
        let took = started.elapsed().as_millis();
        assert_eq!(view.status, RunStatus::Done);
        assert!(
            (expected..expected + 400).contains(&took),
            "limit {limit}: {took} ms of virtual time, expected about {expected}"
        );
        assert_eq!(log.max_running(), if limit == 1 { 1 } else { 4 });
    }
}

#[tokio::test]
async fn four_calls_overlap_on_the_real_clock() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(turn(4, 400, "independent"));
    h.mock.push_text("done");
    let (_rt, _, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(log.max_running(), 4, "the four calls were running at once");
}

#[tokio::test(start_paused = true)]
async fn calls_over_the_limit_queue_and_start_in_call_order() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(turn(5, 100, "independent"));
    h.mock.push_text("done");
    let (rt, _, view) = h.run(2, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(log.max_running(), 2);
    let starts: Vec<String> = log
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.starts_with("start:"))
        .cloned()
        .collect();
    assert_eq!(
        starts,
        ["start:c1", "start:c2", "start:c3", "start:c4", "start:c5"]
    );
    assert_eq!(
        tool_answers(&rt, &view).await.len(),
        5,
        "no call was refused for the queue"
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_declares_nothing_runs_alone_at_any_limit() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(turn(3, 50, "undeclared"));
    h.mock.push_text("done");
    let (_rt, _, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(log.max_running(), 1, "the default is Alone");
}

// ---------------------------------------------------------------------------
// Order
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn conflicting_calls_keep_the_models_order_and_the_others_run_beside_them() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("w1", "probe", json!({"ms": 50, "use": "w:files/a"})),
        call("r1", "probe", json!({"ms": 10, "use": "r:files/a"})),
        call("r2", "probe", json!({"ms": 10, "use": "r:files/b"})),
    ]);
    h.mock.push_text("done");
    let (_rt, _, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(
        log.position("end:w1") < log.position("start:r1"),
        "the read of the edited file waits for the edit"
    );
    assert!(
        log.position("start:r2") < log.position("end:w1"),
        "the read of another file runs beside the edit"
    );
}

#[tokio::test(start_paused = true)]
async fn results_come_back_in_call_order_whatever_order_the_calls_finish_in() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"ms": 300, "use": "independent"})),
        call("c2", "probe", json!({"ms": 200, "use": "independent"})),
        call("c3", "probe", json!({"ms": 100, "use": "independent"})),
    ]);
    h.mock.push_text("done");
    let (rt, run, view) = h.run(10, "go", &log).await;
    assert_eq!(
        tool_answers(&rt, &view).await,
        [
            ("ok:c1".to_owned(), false),
            ("ok:c2".to_owned(), false),
            ("ok:c3".to_owned(), false)
        ],
        "the messages are in the order of the calls"
    );
    assert!(log.position("end:c3") < log.position("end:c2"));
    assert!(log.position("end:c2") < log.position("end:c1"));

    // The steps are said as they happen: all three run before any ends, and they end as they finish.
    let steps = h.steps(run);
    assert_eq!(
        steps,
        [
            ("tool:c1".to_owned(), StepState::Running),
            ("tool:c2".to_owned(), StepState::Running),
            ("tool:c3".to_owned(), StepState::Running),
            ("tool:c3".to_owned(), StepState::Completed),
            ("tool:c2".to_owned(), StepState::Completed),
            ("tool:c1".to_owned(), StepState::Completed),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_journal_is_the_sequential_journal_whatever_the_limit() {
    let mut journals = Vec::new();
    let mut answers = Vec::new();
    for limit in [1usize, 3, 10] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(vec![
            call("c1", "probe", json!({"ms": 30, "use": "independent"})),
            call("c2", "probe", json!({"ms": 20, "use": "w:files/a"})),
            call("c3", "probe", json!({"ms": 10, "use": "r:files/a"})),
            call("c4", "probe", json!({"ms": 5})),
            call("c5", "probe", json!({"ms": 1, "use": "independent"})),
        ]);
        h.mock.push_text("done");
        let (rt, run, view) = h.run(limit, "go", &log).await;
        assert_eq!(view.status, RunStatus::Done);
        journals.push(h.journal(run).await);
        answers.push(tool_answers(&rt, &view).await);
    }
    assert_eq!(journals[0], journals[1]);
    assert_eq!(journals[0], journals[2]);
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0], answers[2]);
    let names: Vec<&str> = journals[0].iter().map(|(_, n)| n.as_str()).collect();
    assert!(names[0].starts_with("model:"), "{names:?}");
    assert_eq!(
        &names[1..6],
        ["tool:c1", "tool:c2", "tool:c3", "tool:c4", "tool:c5"],
        "one step per call, at consecutive seqs, in call order"
    );
    assert!(names[6].starts_with("model:"), "{names:?}");
    let seqs: Vec<u64> = journals[0].iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, (0..7).collect::<Vec<u64>>());
}

// ---------------------------------------------------------------------------
// A crash in the middle of a segment
// ---------------------------------------------------------------------------

/// What a run left behind: its journal, and how often each call began and ran to its end.
struct Replayed {
    journal: Vec<(u64, String)>,
    answers: Vec<(String, bool)>,
    starts: Vec<usize>,
    ends: Vec<usize>,
}

/// A turn of three calls whose third dies with the process after the first two are recorded; the
/// run is stepped again by another worker (another limit) once its lease has run out.
async fn crash_and_replay(first_limit: usize, second_limit: usize) -> Replayed {
    crash_with_hang(first_limit, second_limit, "c3").await
}

/// [`crash_and_replay`] with the call `hanging` (one of `c1`, `c2`, `c3`) the one that never returns.
async fn crash_with_hang(first_limit: usize, second_limit: usize, hanging: &str) -> Replayed {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(
        ["c1", "c2", "c3"]
            .iter()
            .map(|id| {
                if *id == hanging {
                    call(
                        id,
                        "probe",
                        json!({"ms": 1, "use": "independent", "hang": true}),
                    )
                } else {
                    call(
                        id,
                        "probe",
                        json!({"ms": 5, "use": "independent", "tag": id}),
                    )
                }
            })
            .collect(),
    );
    h.mock.push_text("done");

    let probe = Probe::new(&log);
    probe.armed.store(true, SeqCst);
    let armed = probe.armed.clone();
    let agent = h.agent(first_limit, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_millis(300));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    // The process dies while the third call hangs, after the first two recorded their outcomes: the
    // worker is aborted, which drops the transition and leaves the run with its lease.
    let deadline = Instant::now() + Duration::from_secs(20);
    while ["c1", "c2", "c3"]
        .iter()
        .any(|id| (*id == hanging && log.starts(id) == 0) || (*id != hanging && log.ends(id) == 0))
    {
        assert!(Instant::now() < deadline, "the turn did not get that far");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    worker.kill().await;
    armed.store(false, SeqCst);
    let recorded: Vec<String> = h.journal(run).await.into_iter().map(|(_, n)| n).collect();
    let expected: Vec<String> = ["c1", "c2", "c3"]
        .iter()
        .filter(|id| **id != hanging)
        .map(|id| format!("tool:{id}"))
        .collect();
    assert_eq!(
        recorded[1..],
        expected[..],
        "only the calls that returned were recorded before the crash"
    );

    // Another worker, another limit, takes the run when the lease has lapsed.
    let probe = Probe {
        armed: armed.clone(),
        ..Probe::new(&log)
    };
    let agent = h.agent(second_limit, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    Replayed {
        journal: h.journal(run).await,
        answers: tool_answers(&rt, &view).await,
        starts: ["c1", "c2", "c3"].map(|id| log.starts(id)).to_vec(),
        ends: ["c1", "c2", "c3"].map(|id| log.ends(id)).to_vec(),
    }
}

#[tokio::test]
async fn a_replay_after_a_crash_mid_batch_runs_only_the_calls_that_were_not_recorded() {
    for (first, second) in [(10, 10), (1, 10), (10, 1)] {
        let replay = crash_and_replay(first, second).await;
        assert_eq!(
            replay.starts[0], 1,
            "c1 recorded: not run again ({first} then {second})"
        );
        assert_eq!(
            replay.starts[1], 1,
            "c2 recorded: not run again ({first} then {second})"
        );
        assert_eq!(replay.ends, [1, 1, 1]);
        assert_eq!(
            replay.starts[2], 2,
            "c3 began twice: it never recorded the first time"
        );
        assert_eq!(
            replay.answers,
            [
                ("ok:c1".to_owned(), false),
                ("ok:c2".to_owned(), false),
                ("ok:c3".to_owned(), false)
            ],
            "the messages are in call order"
        );
        let names: Vec<&str> = replay.journal.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(&names[1..4], ["tool:c1", "tool:c2", "tool:c3"]);
        assert_eq!(
            replay.journal.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (0..5).collect::<Vec<u64>>(),
            "the seqs of the sequential journal ({first} then {second})"
        );
    }
}

#[tokio::test]
async fn a_journal_written_at_one_limit_replays_at_the_other_and_equals_an_uninterrupted_run() {
    let uninterrupted = {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(vec![
            call("c1", "probe", json!({"ms": 5, "use": "independent"})),
            call("c2", "probe", json!({"ms": 5, "use": "independent"})),
            call("c3", "probe", json!({"ms": 5, "use": "independent"})),
        ]);
        h.mock.push_text("done");
        let (_rt, run, _) = h.run(1, "go", &log).await;
        h.journal(run).await
    };
    for (first, second) in [(1, 10), (10, 1)] {
        let replay = crash_and_replay(first, second).await;
        assert_eq!(replay.journal, uninterrupted, "{first} then {second}");
    }
}

// ---------------------------------------------------------------------------
// Cancel, failure, doom loop, questions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_starts_no_further_call_and_the_run_fails_cancelled() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(turn(4, 60_000, "independent"));
    h.mock.push_text("never asked");
    let agent = h.agent(2, |b| b.tool(Probe::new(&log)));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    while log.starts("c1") + log.starts("c2") < 2 {
        assert!(Instant::now() < deadline, "two calls did not start");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    rt.cancel(run, "stop").await.expect("cancel");
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Failed);
    assert!(
        view.error.as_deref().is_some_and(|e| e.contains("cancel")),
        "{:?}",
        view.error
    );
    assert_eq!(log.starts("c1"), 1);
    assert_eq!(log.starts("c2"), 1);
    assert_eq!(
        log.starts("c3"),
        0,
        "calls that had not started never start"
    );
    assert_eq!(log.starts("c4"), 0);
    assert_eq!(h.mock.remaining(), 1, "the model was not asked again");
}

#[tokio::test(start_paused = true)]
async fn a_call_that_does_not_stop_is_dropped_after_the_grace_period() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(turn(2, 3_600_000, "independent"));
    h.mock.push_text("never asked");
    let probe = Probe {
        heeds_cancel: false,
        ..Probe::new(&log)
    };
    let agent = h.agent(10, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    while log.starts("c1") + log.starts("c2") < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let cancelled = tokio::time::Instant::now();
    rt.cancel(run, "stop").await.expect("cancel");
    let view = wait_terminal(&rt, run).await;
    // The worker returns once the transition has ended: the loop waited out the grace period and
    // dropped the call that does not stop.
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Failed);
    let took = cancelled.elapsed();
    assert!(
        took >= adam_llm_agent::TOOL_STOP_GRACE
            && took < adam_llm_agent::TOOL_STOP_GRACE + Duration::from_secs(1),
        "the loop waits TOOL_STOP_GRACE for a call that ignores the cancel, not more: {took:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_transient_failure_stops_new_starts_waits_for_the_running_calls_and_retries_the_turn() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"ms": 100, "use": "independent"})),
        call(
            "c2",
            "probe",
            json!({"ms": 10, "use": "independent", "transient": true}),
        ),
        call("c3", "probe", json!({"ms": 10, "use": "independent"})),
        call("c4", "probe", json!({"ms": 10, "use": "independent"})),
    ]);
    // The retry asks the model again, and it asks for the same calls.
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"ms": 100, "use": "independent"})),
        call("c2", "probe", json!({"ms": 10, "use": "independent"})),
        call("c3", "probe", json!({"ms": 10, "use": "independent"})),
        call("c4", "probe", json!({"ms": 10, "use": "independent"})),
    ]);
    h.mock.push_text("done");
    let probe = Probe::new(&log);
    probe.armed.store(true, SeqCst);
    let agent = h.agent(2, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done, "{view:?}");
    assert_eq!(
        log.starts("c1"),
        2,
        "ran in the failed try and again in the retry"
    );
    assert_eq!(log.starts("c2"), 2);
    assert_eq!(
        log.starts("c3"),
        1,
        "the calls that had not started when it failed never started in that try"
    );
    assert_eq!(log.starts("c4"), 1);
    assert_eq!(
        log.ends("c1"),
        2,
        "the failed try waited for the call that was running"
    );
    assert!(log.position("end:c1") < log.position("start:c3"));
    assert_eq!(tool_answers(&rt, &view).await.len(), 4);
}

#[tokio::test(start_paused = true)]
async fn three_identical_calls_in_one_turn_are_refused_as_today() {
    let mut answers = Vec::new();
    for limit in [1usize, 10] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        let same = || json!({"ms": 10, "use": "independent", "out": "same"});
        h.mock.push_tool_calls(vec![
            call(
                "c1",
                "probe",
                json!({"ms": 10, "use": "independent", "out": "first"}),
            ),
            call("c2", "probe", same()),
            call("c3", "probe", same()),
            call("c4", "probe", same()),
            call(
                "c5",
                "probe",
                json!({"ms": 10, "use": "independent", "out": "last"}),
            ),
        ]);
        h.mock.push_text("I will answer with what I know");
        let (rt, _, view) = h.run(limit, "go", &log).await;
        assert_eq!(view.status, RunStatus::Done);
        let got = tool_answers(&rt, &view).await;
        assert_eq!(got.len(), 5);
        assert_eq!(got[1], ("same".to_owned(), false));
        assert_eq!(got[2], ("same".to_owned(), false));
        assert!(got[3].1, "the third identical call is refused: {got:?}");
        assert!(
            got[3]
                .0
                .starts_with("Not run: this is the third call in a row of `probe`")
        );
        assert_eq!(log.ends("c4"), 0, "it did not run");
        answers.push(got);
    }
    assert_eq!(answers[0], answers[1], "the same answers at every limit");
}

#[tokio::test(start_paused = true)]
async fn the_limit_on_tool_calls_is_counted_as_before() {
    for limit in [1usize, 10] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(turn(4, 10, "independent"));
        h.mock.push_text("closing words");
        let agent = h.agent(limit, |b| {
            b.tool(Probe::new(&log)).limits(adam_llm_agent::Limits {
                max_tool_calls: 3,
                ..adam_llm_agent::Limits::default()
            })
        });
        let rt = h.runtime(agent, Duration::from_secs(10));
        let run = rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start");
        let worker = Worker::spawn(&rt);
        let view = wait_terminal(&rt, run).await;
        worker.stop().await;
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(
            log.max_running(),
            0,
            "a turn over the limit has none of its calls run"
        );
        assert_eq!(tool_answers(&rt, &view).await.len(), 4);
        assert!(
            tool_answers(&rt, &view)
                .await
                .iter()
                .all(|(_, error)| *error)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_question_in_the_middle_of_a_turn_runs_alone_and_the_rest_runs_after_the_answer() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"ms": 10, "use": "independent"})),
        call("c2", "ask", json!({})),
        call("c3", "probe", json!({"ms": 10, "use": "independent"})),
    ]);
    h.mock.push_text("deploying");
    let agent = h.agent(10, |b| b.tool(Probe::new(&log)).tool(Asker));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("deploy"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.waiting {
            break;
        }
        assert!(Instant::now() < deadline, "the run did not park");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(log.ends("c1"), 1, "the call before the question finished");
    assert_eq!(
        log.starts("c3"),
        0,
        "the call after it waits for the answer"
    );
    rt.deliver(run, user_message("prod"))
        .await
        .expect("deliver");
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(
        tool_answers(&rt, &view).await,
        [
            ("ok:c1".to_owned(), false),
            ("prod".to_owned(), false),
            ("ok:c3".to_owned(), false)
        ],
        "the results are in call order"
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_that_asks_though_it_declared_nothing_of_the_kind_is_told_to_run_alone() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"ms": 10, "use": "independent"})),
        call(
            "c2",
            "probe",
            json!({"ms": 10, "use": "independent", "ask": true}),
        ),
        call("c3", "probe", json!({"ms": 10, "use": "independent"})),
    ]);
    h.mock.push_text("done");
    let (rt, run, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done, "the run did not park");
    let answers = tool_answers(&rt, &view).await;
    assert_eq!(answers[0], ("ok:c1".to_owned(), false));
    assert_eq!(
        answers[1],
        (
            "this tool cannot run alongside others: call it alone".to_owned(),
            true
        )
    );
    assert_eq!(answers[2], ("ok:c3".to_owned(), false));
    let ends: Vec<StepState> = h
        .steps(run)
        .into_iter()
        .filter(|(id, state)| id == "tool:c2" && state.is_end())
        .map(|(_, state)| state)
        .collect();
    assert_eq!(ends, [StepState::Failed]);
}

// ---------------------------------------------------------------------------
// The budgets of a segment, the builder
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_file_the_run_has_no_room_for_is_refused_when_the_results_are_applied_and_its_end_is_corrected()
 {
    let h = Harness::new();
    h.mock.push_tool_calls(vec![
        call("c1", "filer", json!({"kib": 4096, "tag": 1})),
        call("c2", "filer", json!({"kib": 4096, "tag": 2})),
    ]);
    h.mock.push_text("done");
    let agent = h.agent(10, |b| b.tool(Filer));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    let answers = tool_answers(&rt, &view).await;
    assert_eq!(answers[0], ("shared".to_owned(), false));
    assert!(
        answers[1].1 && answers[1].0.contains("Not shared"),
        "{answers:?}"
    );
    let c2: Vec<StepState> = h
        .steps(run)
        .into_iter()
        .filter(|(id, _)| id == "tool:c2")
        .map(|(_, state)| state)
        .collect();
    assert_eq!(
        c2,
        [StepState::Running, StepState::Completed, StepState::Failed],
        "the end said as it happened, and the corrected end"
    );
}

#[test]
fn the_limit_is_ten_by_default_and_clamped_to_one_to_thirty_two() {
    let model = Arc::new(MockModel::new());
    let limit = |n: Option<usize>| {
        let builder = LlmAgent::builder("a", model.clone(), "m");
        match n {
            Some(n) => builder.max_parallel(n),
            None => builder,
        }
        .build()
        .max_parallel()
    };
    assert_eq!(limit(None), 10);
    assert_eq!(limit(Some(0)), 1);
    assert_eq!(limit(Some(1)), 1);
    assert_eq!(limit(Some(7)), 7);
    assert_eq!(limit(Some(32)), 32);
    assert_eq!(limit(Some(1000)), 32);
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// A source of tools that sleep: `slow_*` tools say they share nothing, the others say nothing.
struct Sleepers {
    log: Arc<Log>,
}

#[async_trait]
impl adam_llm_agent::ToolSource for Sleepers {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
        ["slow_a", "slow_b", "plain"]
            .iter()
            .map(|name| ToolSpec {
                name: (*name).into(),
                description: "sleeps".into(),
                parameters: json!({"type": "object"}),
            })
            .collect()
    }

    fn access(&self, name: &str, _args: &Value) -> ToolAccess {
        if name.starts_with("slow_") {
            ToolAccess::Independent
        } else {
            ToolAccess::Alone
        }
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        let now = self.log.running.fetch_add(1, SeqCst) + 1;
        self.log.max_running.fetch_max(now, SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.log.running.fetch_sub(1, SeqCst);
        Some(Ok(ToolOutput::text(format!("{name}:{}", ctx.call_id()))))
    }
}

#[tokio::test(start_paused = true)]
async fn the_calls_of_a_source_run_together_when_the_source_says_they_share_nothing() {
    for (names, together) in [(["slow_a", "slow_b"], 2usize), (["plain", "plain"], 1)] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(vec![
            call("c1", names[0], json!({"tag": 1})),
            call("c2", names[1], json!({"tag": 2})),
        ]);
        h.mock.push_text("done");
        let agent = h.agent(10, |b| b.tool_source(Sleepers { log: log.clone() }));
        let rt = h.runtime(agent, Duration::from_secs(10));
        let run = rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start");
        let worker = Worker::spawn(&rt);
        let view = wait_terminal(&rt, run).await;
        worker.stop().await;
        assert_eq!(view.status, RunStatus::Done);
        assert_eq!(log.max_running(), together, "{names:?}");
        assert_eq!(
            tool_answers(&rt, &view).await,
            [
                (format!("{}:c1", names[0]), false),
                (format!("{}:c2", names[1]), false)
            ]
        );
    }
}

// ---------------------------------------------------------------------------
// Fixes of the review
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_recorded_call_above_a_gap_is_not_run_again_and_the_journal_is_the_sequential_one() {
    let uninterrupted = {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(turn(3, 5, "independent"));
        h.mock.push_text("done");
        let (_rt, run, _) = h.run(1, "go", &log).await;
        h.journal(run).await
    };
    for second in [1usize, 10] {
        // c1 is slow and never returns; c2 and c3 are recorded above its gap.
        let replay = crash_with_hang(10, second, "c1").await;
        assert_eq!(
            replay.starts,
            [2, 1, 1],
            "c2 and c3 are not run again (limit {second})"
        );
        assert_eq!(replay.ends, [1, 1, 1]);
        assert_eq!(
            replay.journal.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (0..5).collect::<Vec<u64>>()
        );
        assert_eq!(replay.journal, uninterrupted, "limit {second}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_call_that_reports_rounds_beside_other_calls_is_told_to_run_alone() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"ms": 10, "use": "independent"})),
        call(
            "c2",
            "probe",
            json!({"ms": 10, "use": "independent", "report": true}),
        ),
    ]);
    h.mock.push_text("done");
    let (rt, run, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    let answers = tool_answers(&rt, &view).await;
    assert_eq!(answers[0], ("ok:c1".to_owned(), false));
    assert_eq!(
        answers[1],
        (
            "this tool cannot run alongside others: call it alone".to_owned(),
            true
        )
    );
    let ends: Vec<StepState> = h
        .steps(run)
        .into_iter()
        .filter(|(id, state)| id == "tool:c2" && state.is_end())
        .map(|(_, state)| state)
        .collect();
    assert_eq!(ends, [StepState::Failed], "its end is said once");
    let names: Vec<String> = h.journal(run).await.into_iter().map(|(_, n)| n).collect();
    assert!(
        names.iter().all(|n| !n.starts_with("usage:")),
        "no usage step for it: {names:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn concurrent_calls_see_the_files_each_other_took() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    // The run may share 6 MiB of files; each call wants 4 MiB.
    let four_mib = 4 * 1024 * 1024u64;
    h.mock.push_tool_calls(vec![
        call(
            "c1",
            "probe",
            json!({"ms": 10, "use": "independent", "files": four_mib, "tag": 1}),
        ),
        call(
            "c2",
            "probe",
            json!({"ms": 10, "use": "independent", "files": four_mib, "tag": 2}),
        ),
    ]);
    h.mock.push_text("done");
    let probe = Probe::new(&log);
    let seen = probe.files_seen.clone();
    let agent = h.agent(10, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0], ("c1".to_owned(), 6 * 1024 * 1024));
    assert_eq!(
        seen[1],
        ("c2".to_owned(), 2 * 1024 * 1024),
        "the second call is told what the first left"
    );
}

#[tokio::test(start_paused = true)]
async fn a_permanent_error_wins_over_a_transient_one_whichever_finished_first() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        // c1 poisons its own journal record when it ends, after c2 failed transiently.
        call(
            "c1",
            "probe",
            json!({"ms": 100, "use": "independent", "poison": true}),
        ),
        call(
            "c2",
            "probe",
            json!({"ms": 10, "use": "independent", "transient": true}),
        ),
    ]);
    h.mock.push_text("done");
    let probe = Probe {
        store: Some(h.store.clone()),
        ..Probe::new(&log)
    };
    probe.armed.store(true, SeqCst);
    let agent = h.agent(10, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Failed, "{view:?}");
    assert!(
        view.error
            .as_deref()
            .is_some_and(|e| e.contains("non-deterministic")),
        "{:?}",
        view.error
    );
}

// ---------------------------------------------------------------------------
// Survivors of the mutation pass
// ---------------------------------------------------------------------------

/// A source that offers nothing and answers `Alone` for every name.
struct Silent;

#[async_trait]
impl adam_llm_agent::ToolSource for Silent {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
        Vec::new()
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        None
    }
}

#[tokio::test(start_paused = true)]
async fn the_second_source_decides_when_the_first_does_not_offer_the_tool() {
    // The rule: the first source that answers something other than `Alone` decides, so a source
    // that does not offer the tool (and says `Alone`, the default) does not hide a later one.
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "slow_a", json!({"tag": 1})),
        call("c2", "slow_b", json!({"tag": 2})),
    ]);
    h.mock.push_text("done");
    let agent = h.agent(10, |b| {
        b.tool_source(Silent)
            .tool_source(Sleepers { log: log.clone() })
    });
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(log.max_running(), 2);
    assert_eq!(
        tool_answers(&rt, &view).await,
        [
            ("slow_a:c1".to_owned(), false),
            ("slow_b:c2".to_owned(), false)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_queued_call_starts_when_any_running_call_ends() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call(
            "c1",
            "probe",
            json!({"ms": 300, "use": "independent", "tag": 1}),
        ),
        call(
            "c2",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 2}),
        ),
        call(
            "c3",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 3}),
        ),
    ]);
    h.mock.push_text("done");
    let started = tokio::time::Instant::now();
    let (_rt, _, view) = h.run(2, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(
        log.position("start:c3") < log.position("end:c1"),
        "c3 took the place c2 left, it did not wait for c1"
    );
    let took = started.elapsed().as_millis();
    assert!((300..400).contains(&took), "{took} ms of virtual time");
}

#[tokio::test(start_paused = true)]
async fn an_error_result_whose_file_is_refused_has_its_end_corrected() {
    let h = Harness::new();
    h.mock.push_tool_calls(vec![
        call("c1", "filer", json!({"kib": 4096, "error": true, "tag": 1})),
        call("c2", "filer", json!({"kib": 4096, "error": true, "tag": 2})),
    ]);
    h.mock.push_text("done");
    let agent = h.agent(10, |b| b.tool(Filer));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    let c2: Vec<(StepState, String)> = h
        .sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) if step.id == "tool:c2" => {
                Some((step.state, step.output.map(|o| o.text).unwrap_or_default()))
            }
            _ => None,
        })
        .collect();
    let states: Vec<StepState> = c2.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        states,
        [StepState::Running, StepState::Failed, StepState::Failed]
    );
    assert_eq!(c2[1].1, "bad");
    assert!(
        c2[2].1.contains("Not shared"),
        "the corrected end says why: {:?}",
        c2[2].1
    );
}

#[tokio::test(start_paused = true)]
async fn a_call_with_bad_arguments_in_a_segment_takes_no_seq() {
    let mut runs = Vec::new();
    for limit in [1usize, 10] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        let mut bad = call("c2", "probe", Value::Null);
        bad.invalid_arguments = Some("{oops".into());
        h.mock.push_tool_calls(vec![
            call(
                "c1",
                "probe",
                json!({"ms": 10, "use": "independent", "tag": 1}),
            ),
            bad,
            call(
                "c3",
                "probe",
                json!({"ms": 10, "use": "independent", "tag": 3}),
            ),
        ]);
        h.mock.push_text("done");
        let (rt, run, view) = h.run(limit, "go", &log).await;
        assert_eq!(view.status, RunStatus::Done, "limit {limit}: {view:?}");
        let journal = h.journal(run).await;
        assert!(journal.iter().all(|(_, n)| n != "tool:c2"), "{journal:?}");
        assert_eq!(
            journal.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (0..journal.len() as u64).collect::<Vec<_>>(),
            "contiguous seqs"
        );
        runs.push((journal, tool_answers(&rt, &view).await));
    }
    assert_eq!(
        runs[0], runs[1],
        "the same journal and answers at both limits"
    );
    assert!(runs[0].1[1].1, "the bad call is answered with an error");
}

#[tokio::test(start_paused = true)]
async fn a_session_tool_runs_alone_even_when_it_declares_independent() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call(
            "c1",
            "subagent_read",
            json!({"ms": 50, "use": "independent", "tag": 1}),
        ),
        call(
            "c2",
            "subagent_read",
            json!({"ms": 50, "use": "independent", "tag": 2}),
        ),
    ]);
    h.mock.push_text("done");
    let probe = Probe {
        name: adam_llm_agent::SUBAGENT_READ,
        ..Probe::new(&log)
    };
    let agent = h.agent(10, |b| b.tool(probe));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(log.max_running(), 1, "what the tool declares is not heeded");
}

// ---------------------------------------------------------------------------
// Sync on demand: `after_previous` (ADR 0049 decision 12)
// ---------------------------------------------------------------------------

/// A tool whose own schema forbids every argument but `x`, as a `deny_unknown_fields` one does: it
/// fails on any other.
struct Strict {
    log: Arc<Log>,
}

#[async_trait]
impl Tool for Strict {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "strict".into(),
            description: "strict".into(),
            parameters: json!({
                "type": "object",
                "properties": {"x": {"type": "integer"}},
                "required": ["x"],
                "additionalProperties": false
            }),
        }
    }

    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.log
            .args
            .lock()
            .unwrap()
            .insert(ctx.call_id().to_owned(), args.clone());
        let keys: Vec<&String> = args.as_object().unwrap().keys().collect();
        if keys != ["x"] {
            return Err(ToolError::Permanent(format!("unknown field in {keys:?}")));
        }
        Ok(ToolOutput::text(format!("strict:{}", ctx.call_id())))
    }
}

fn start_position(log: &Log, id: &str) -> usize {
    log.position(&format!("start:{id}"))
}

fn end_position(log: &Log, id: &str) -> usize {
    log.position(&format!("end:{id}"))
}

#[tokio::test(start_paused = true)]
async fn a_call_with_after_previous_waits_for_every_call_before_it_and_holds_the_ones_after() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call(
            "c1",
            "probe",
            json!({"ms": 300, "use": "independent", "tag": 1}),
        ),
        call(
            "c2",
            "probe",
            json!({"ms": 200, "use": "independent", "tag": 2}),
        ),
        call(
            "c3",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 3, "after_previous": true}),
        ),
        call(
            "c4",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 4}),
        ),
        call(
            "c5",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 5}),
        ),
    ]);
    h.mock.push_text("done");
    let (rt, run, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(
        start_position(&log, "c2") < end_position(&log, "c1"),
        "the calls before the barrier ran together"
    );
    assert!(
        end_position(&log, "c1") < start_position(&log, "c3")
            && end_position(&log, "c2") < start_position(&log, "c3"),
        "the barrier waited for both: {:?}",
        log.events.lock().unwrap()
    );
    assert!(
        end_position(&log, "c3") < start_position(&log, "c4"),
        "nothing after the barrier started before it was done"
    );
    assert!(
        start_position(&log, "c5") < end_position(&log, "c4"),
        "the calls after the barrier run together again"
    );
    assert_eq!(log.max_running(), 2);
    assert_eq!(
        tool_answers(&rt, &view)
            .await
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["ok:c1", "ok:c2", "ok:c3", "ok:c4", "ok:c5"],
        "results in call order"
    );
    // The journal is the sequential one: a step per call, in call order.
    let names: Vec<String> = h.journal(run).await.into_iter().map(|(_, n)| n).collect();
    assert_eq!(
        &names[1..6],
        ["tool:c1", "tool:c2", "tool:c3", "tool:c4", "tool:c5"]
    );
}

#[tokio::test(start_paused = true)]
async fn after_previous_never_reaches_the_tool_the_doom_loop_key_or_the_step() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "strict", json!({"x": 1, "after_previous": false})),
        call("c2", "strict", json!({"x": 2, "after_previous": true})),
        call("c3", "strict", json!({"x": 3, "after_previous": null})),
    ]);
    h.mock.push_text("done");
    let agent = h.agent(10, |b| b.tool(Strict { log: log.clone() }));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(
        tool_answers(&rt, &view).await,
        [
            ("strict:c1".to_owned(), false),
            ("strict:c2".to_owned(), false),
            ("strict:c3".to_owned(), false)
        ],
        "a strict tool accepted every call"
    );
    let args = log.args.lock().unwrap().clone();
    assert_eq!(args["c1"], json!({"x": 1}));
    assert_eq!(args["c2"], json!({"x": 2}));
    assert_eq!(args["c3"], json!({"x": 3}));
    // The step's input is the call without it; the history keeps what the model said.
    let inputs: Vec<String> = h
        .sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) if step.state == StepState::Running => {
                step.input.map(|v| Value::Object(v).to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(inputs.len(), 3, "{inputs:?}");
    assert!(
        inputs.iter().all(|i| !i.contains("after_previous")),
        "{inputs:?}"
    );
    let said = conversation(&rt, &view)
        .await
        .messages
        .iter()
        .flat_map(|m| m.tool_calls().to_vec())
        .map(|c| c.arguments)
        .collect::<Vec<_>>();
    assert_eq!(said[1], json!({"x": 2, "after_previous": true}));
}

#[tokio::test(start_paused = true)]
async fn the_doom_loop_key_ignores_after_previous() {
    // Three calls with the same tool and arguments, one saying `after_previous: false`, are the
    // same call: the third is refused as it is without the flag.
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"use": "independent", "out": "same"})),
        call(
            "c2",
            "probe",
            json!({"use": "independent", "out": "same", "after_previous": false}),
        ),
        call("c3", "probe", json!({"use": "independent", "out": "same"})),
    ]);
    h.mock.push_text("done");
    let (rt, _, view) = h.run(10, "go", &log).await;
    let answers = tool_answers(&rt, &view).await;
    assert_eq!(answers[0], ("same".to_owned(), false));
    assert_eq!(answers[1], ("same".to_owned(), false));
    assert!(
        answers[2].1,
        "the third identical call is refused: {answers:?}"
    );
    assert_eq!(log.starts("c3"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_bad_after_previous_answers_an_error_takes_no_step_and_runs_nothing() {
    let mut runs = Vec::new();
    for limit in [1usize, 10] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(vec![
            call("c1", "probe", json!({"use": "independent", "tag": 1})),
            call(
                "c2",
                "probe",
                json!({"use": "independent", "tag": 2, "after_previous": "yes"}),
            ),
            call("c3", "probe", json!({"use": "independent", "tag": 3})),
        ]);
        h.mock.push_text("done");
        let (rt, run, view) = h.run(limit, "go", &log).await;
        assert_eq!(view.status, RunStatus::Done);
        let answers = tool_answers(&rt, &view).await;
        assert!(answers[1].1, "{answers:?}");
        assert!(
            answers[1].0.contains("`after_previous`")
                && answers[1].0.contains("true or false")
                && answers[1].0.contains("did not run"),
            "{}",
            answers[1].0
        );
        assert_eq!(log.starts("c2"), 0, "the call did not run");
        let journal = h.journal(run).await;
        assert!(journal.iter().all(|(_, n)| n != "tool:c2"), "{journal:?}");
        assert_eq!(
            journal.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            (0..journal.len() as u64).collect::<Vec<_>>(),
            "contiguous seqs"
        );
        runs.push((journal, answers));
    }
    assert_eq!(runs[0], runs[1], "the same at both limits");
}

#[tokio::test(start_paused = true)]
async fn the_journal_and_the_answers_are_the_same_at_a_limit_of_one_and_ten_with_after_previous() {
    let mut runs = Vec::new();
    for limit in [1usize, 10] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(vec![
            call(
                "c1",
                "probe",
                json!({"ms": 30, "use": "independent", "tag": 1}),
            ),
            call(
                "c2",
                "probe",
                json!({"ms": 20, "use": "independent", "tag": 2, "after_previous": true}),
            ),
            call(
                "c3",
                "probe",
                json!({"ms": 10, "use": "r:files/a", "tag": 3}),
            ),
        ]);
        h.mock.push_text("done");
        let (rt, run, view) = h.run(limit, "go", &log).await;
        assert_eq!(view.status, RunStatus::Done);
        runs.push((h.journal(run).await, tool_answers(&rt, &view).await));
    }
    assert_eq!(runs[0], runs[1]);
}

#[tokio::test]
async fn after_previous_is_in_every_schema_above_a_limit_of_one_and_in_none_at_one() {
    for (limit, injected) in [(10usize, true), (2, true), (1, false)] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_text("done");
        let agent = h.agent(limit, |b| {
            b.tool(Probe::new(&log))
                .tool(Strict { log: log.clone() })
                .tool_source(Sleepers { log: log.clone() })
        });
        let rt = h.runtime(agent, Duration::from_secs(10));
        let run = rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start");
        let worker = Worker::spawn(&rt);
        wait_terminal(&rt, run).await;
        worker.stop().await;
        let request = &h.mock.requests()[0];
        let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["probe", "strict", "plain", "slow_a", "slow_b"],
            "own tools first, then the source's by name"
        );
        for tool in &request.tools {
            let property = &tool.parameters["properties"]["after_previous"];
            if injected {
                assert_eq!(property["type"], "boolean", "{}: {property}", tool.name);
                let required = tool.parameters["required"].as_array();
                assert!(
                    required.is_none_or(|r| !r.iter().any(|v| v == "after_previous")),
                    "{}: never required",
                    tool.name
                );
            } else {
                assert!(property.is_null(), "{}: {property}", tool.name);
            }
        }
        // The schema's own keys are all still there.
        let strict = &request.tools[1].parameters;
        assert_eq!(strict["required"], json!(["x"]));
        assert_eq!(strict["additionalProperties"], json!(false));
        assert_eq!(strict["properties"]["x"], json!({"type": "integer"}));
        let system = request.system.clone().unwrap_or_default();
        assert_eq!(
            system.contains("after_previous"),
            injected,
            "the instruction says it above a limit of one only: {system:?}"
        );
        if injected {
            assert!(
                system.contains("The tool calls of one reply may run at the same time"),
                "a tool that runs alone still does, so the calls only may: {system:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_schema_that_is_not_an_object_is_left_alone() {
    struct Odd;
    #[async_trait]
    impl Tool for Odd {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "odd".into(),
                description: "odd".into(),
                parameters: json!({"type": "string"}),
            }
        }
        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("odd"))
        }
    }
    let h = Harness::new();
    h.mock.push_text("done");
    let agent = h.agent(10, |b| b.tool(Odd));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(
        h.mock.requests()[0].tools[0].parameters,
        json!({"type": "string"})
    );
}

/// A tool whose own schema already has an `after_previous`: it is the tool's, so nothing is injected
/// and nothing is stripped.
struct Owner {
    log: Arc<Log>,
}

#[async_trait]
impl Tool for Owner {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "owner".into(),
            description: "owner".into(),
            parameters: json!({
                "type": "object",
                "properties": {"after_previous": {"type": "string"}}
            }),
        }
    }

    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.log
            .args
            .lock()
            .unwrap()
            .insert(ctx.call_id().to_owned(), args);
        Ok(ToolOutput::text("owner"))
    }
}

#[test]
fn an_own_tool_whose_schema_has_after_previous_is_a_build_error_above_a_limit_of_one() {
    let log = Arc::new(Log::default());
    let model = Arc::new(MockModel::new());
    let err = LlmAgent::builder("a", model.clone(), "m")
        .max_parallel(10)
        .tool(Owner { log: log.clone() })
        .try_build()
        .unwrap_err();
    assert!(
        matches!(&err, adam_llm_agent::BuildError::ReservedArgument { tool, .. } if tool == "owner"),
        "{err:?}"
    );
    assert!(err.to_string().contains("after_previous"), "{err}");
    // At a limit of one nothing is injected, so nothing collides.
    LlmAgent::builder("a", model, "m")
        .max_parallel(1)
        .tool(Owner { log })
        .try_build()
        .expect("no collision at a limit of one");
}

/// A source's tool with an `after_previous` of its own: untouched, and its value reaches it.
struct Collides {
    log: Arc<Log>,
}

#[async_trait]
impl adam_llm_agent::ToolSource for Collides {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "mine".into(),
                description: "has its own".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"after_previous": {"type": "string"}}
                }),
            },
            ToolSpec {
                name: "theirs".into(),
                description: "has none".into(),
                parameters: json!({"type": "object"}),
            },
        ]
    }

    fn access(&self, _name: &str, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        self.log
            .args
            .lock()
            .unwrap()
            .insert(ctx.call_id().to_owned(), args);
        Some(Ok(ToolOutput::text(format!("{name}:{}", ctx.call_id()))))
    }
}

#[tokio::test]
async fn a_source_tool_with_its_own_after_previous_keeps_it_and_the_others_still_get_the_flag() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "mine", json!({"after_previous": "later"})),
        call("c2", "theirs", json!({"after_previous": true, "k": 1})),
    ]);
    h.mock.push_text("done");
    let agent = h.agent(10, |b| b.tool_source(Collides { log: log.clone() }));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = Worker::spawn(&rt);
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    let request = &h.mock.requests()[0];
    assert_eq!(
        request.tools[0].parameters["properties"]["after_previous"],
        json!({"type": "string"}),
        "the tool's own property is not replaced"
    );
    assert_eq!(
        request.tools[1].parameters["properties"]["after_previous"]["type"],
        "boolean"
    );
    let args = log.args.lock().unwrap().clone();
    assert_eq!(
        args["c1"],
        json!({"after_previous": "later"}),
        "not stripped"
    );
    assert_eq!(args["c2"], json!({"k": 1}), "stripped");
    assert_eq!(
        tool_answers(&rt, &view).await,
        [
            ("mine:c1".to_owned(), false),
            ("theirs:c2".to_owned(), false)
        ]
    );
}

#[test]
fn read_output_is_independent_and_the_session_list_runs_alone() {
    assert_eq!(
        adam_llm_agent::ReadOutput.access(&json!({})),
        ToolAccess::Independent
    );
    let roster = adam_llm_agent::SessionTools::new([adam_llm_agent::SessionAgent {
        name: "helper".into(),
        agent: "x/helper".into(),
        description: "Does a piece of work.".into(),
        within: None,
        remote: false,
    }]);
    for tool in roster.tools() {
        assert_eq!(
            tool.access(&json!({})),
            ToolAccess::Alone,
            "{}",
            tool.spec().name
        );
    }
}

// ---------------------------------------------------------------------------
// The fixes of the review of PR B
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_call_flagged_invalid_by_the_client_is_told_why_even_when_its_text_holds_after_previous()
{
    let h = Harness::new();
    let log = Arc::new(Log::default());
    let mut bad = call(
        "c1",
        "probe",
        Value::String(adam_model::INVALID_ARGUMENTS_MARKER.into()),
    );
    bad.invalid_arguments = Some(r#"{"after_previous": true}"#.into());
    h.mock.push_tool_calls(vec![bad]);
    h.mock.push_text("done");
    let (rt, _, view) = h.run(10, "go", &log).await;
    let answers = tool_answers(&rt, &view).await;
    assert!(answers[0].1, "{answers:?}");
    assert!(
        answers[0].0.contains("not valid JSON"),
        "the client's reason, not the order's: {}",
        answers[0].0
    );
    assert!(
        !answers[0].0.contains("must be true or false"),
        "{}",
        answers[0].0
    );
}

#[tokio::test(start_paused = true)]
async fn the_order_of_a_turn_is_not_kept_once_its_calls_are_answered() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "probe", json!({"use": "independent", "tag": 1})),
        call(
            "c2",
            "probe",
            json!({"use": "independent", "tag": 2, "after_previous": true}),
        ),
    ]);
    h.mock.push_text("done");
    let (rt, _, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    let state = conversation(&rt, &view).await;
    assert!(state.pending_calls.is_empty());
    assert!(
        state.ordered_calls.is_empty(),
        "the snapshot keeps no order of calls that are answered: {:?}",
        state.ordered_calls
    );
}

/// A source whose tool `mine` has a property `after_previous` of its own, or not: what a server that
/// changed between two processes would list.
struct Mine {
    log: Arc<Log>,
    own: bool,
}

#[async_trait]
impl adam_llm_agent::ToolSource for Mine {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
        let parameters = if self.own {
            json!({"type": "object", "properties": {"after_previous": {"type": "string"}}})
        } else {
            json!({"type": "object"})
        };
        vec![ToolSpec {
            name: "mine".into(),
            description: "mine".into(),
            parameters,
        }]
    }

    fn access(&self, _name: &str, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        let id = ctx.call_id().to_owned();
        self.log.args.lock().unwrap().insert(id.clone(), args);
        self.log.events.lock().unwrap().push(format!("start:{id}"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.log.events.lock().unwrap().push(format!("end:{id}"));
        *self.log.ends.lock().unwrap().entry(id.clone()).or_default() += 1;
        Some(Ok(ToolOutput::text(format!("{name}:{id}"))))
    }
}

/// The turn of the restart tests: a call that hangs in the first process, a call to the source's tool
/// with its own `after_previous`, and a call that asks to follow both.
fn ordered_turn(hang: bool) -> Vec<ToolCall> {
    vec![
        call(
            "c1",
            "probe",
            json!({"ms": 30, "use": "independent", "tag": 1, "hang": hang}),
        ),
        call("c2", "mine", json!({"after_previous": "later", "k": 2})),
        call(
            "c3",
            "probe",
            json!({"ms": 5, "use": "independent", "tag": 3, "after_previous": true}),
        ),
    ]
}

/// The start events of the tool steps of `run`, by call id: what each call was given.
fn step_inputs(h: &Harness, run: RunId) -> HashMap<String, Value> {
    h.sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Step(step) if step.state == StepState::Running => {
                Some((step.id, Value::Object(step.input.unwrap_or_default())))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_restart_with_another_limit_and_other_sources_keeps_the_recorded_order() {
    // An uninterrupted run, for the journal it writes.
    let reference = {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(ordered_turn(false));
        h.mock.push_text("done");
        let agent = h.agent(10, |b| {
            b.tool(Probe::new(&log)).tool_source(Mine {
                log: log.clone(),
                own: true,
            })
        });
        let rt = h.runtime(agent, Duration::from_secs(10));
        let run = rt.start("llm", user_message("go"), None).await.unwrap();
        let worker = Worker::spawn(&rt);
        wait_terminal(&rt, run).await;
        worker.stop().await;
        (h.journal(run).await, step_inputs(&h, run))
    };
    for (first, second) in [(10usize, 1usize), (1, 10)] {
        let h = Harness::new();
        let log = Arc::new(Log::default());
        h.mock.push_tool_calls(ordered_turn(true));
        h.mock.push_text("done");
        let probe = Probe::new(&log);
        probe.armed.store(true, SeqCst);
        let armed = probe.armed.clone();
        let agent = h.agent(first, |b| {
            b.tool(probe).tool_source(Mine {
                log: log.clone(),
                own: true,
            })
        });
        let rt = h.runtime(agent, Duration::from_millis(300));
        let run = rt.start("llm", user_message("go"), None).await.unwrap();
        let worker = Worker::spawn(&rt);
        let deadline = Instant::now() + Duration::from_secs(20);
        while log.starts("c1") == 0 {
            assert!(Instant::now() < deadline, "the turn did not start");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // The process dies in the middle of the turn, after its model step was journaled.
        worker.kill().await;
        armed.store(false, SeqCst);
        let before: Vec<String> = h.journal(run).await.into_iter().map(|(_, n)| n).collect();
        assert!(before[0].starts_with("model:"), "{before:?}");
        assert!(!before.contains(&"tool:c3".to_owned()), "{before:?}");

        // Another process: another limit, and a source whose `mine` has no `after_previous` now.
        let probe = Probe {
            armed: armed.clone(),
            ..Probe::new(&log)
        };
        let agent = h.agent(second, |b| {
            b.tool(probe).tool_source(Mine {
                log: log.clone(),
                own: false,
            })
        });
        let rt = h.runtime(agent, Duration::from_secs(10));
        let worker = Worker::spawn(&rt);
        let view = wait_terminal(&rt, run).await;
        worker.stop().await;
        assert_eq!(
            view.status,
            RunStatus::Done,
            "{first} then {second}: {view:?}"
        );
        assert_eq!(h.journal(run).await, reference.0, "{first} then {second}");
        let args = log.args.lock().unwrap().clone();
        assert_eq!(
            args["c2"],
            json!({"after_previous": "later", "k": 2}),
            "{first} then {second}: the recorded turn says the property is the tool's"
        );
        assert_eq!(
            args["c3"],
            json!({"ms": 5, "use": "independent", "tag": 3}),
            "{first} then {second}"
        );
        let last_c1_end = log
            .events
            .lock()
            .unwrap()
            .iter()
            .rposition(|e| e == "end:c1")
            .unwrap();
        let c3_start = start_position(&log, "c3");
        assert!(
            end_position(&log, "c2") < c3_start && last_c1_end < c3_start,
            "{first} then {second}: the barrier held: {:?}",
            log.events.lock().unwrap()
        );
        let inputs = step_inputs(&h, run);
        for id in ["tool:c2", "tool:c3"] {
            assert_eq!(inputs[id], reference.1[id], "{first} then {second}: {id}");
        }
        assert!(
            !inputs["tool:c3"].to_string().contains("after_previous"),
            "{:?}",
            inputs["tool:c3"]
        );
    }
}

#[tokio::test]
async fn a_parked_turn_keeps_its_order_in_the_snapshot_and_a_new_process_honours_it() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call("c1", "ask", json!({})),
        call(
            "c2",
            "probe",
            json!({"ms": 200, "use": "independent", "tag": 2, "after_previous": true}),
        ),
        call(
            "c3",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 3}),
        ),
    ]);
    h.mock.push_text("done");
    // Parked by a process that runs one call at a time.
    let agent = h.agent(1, |b| b.tool(Probe::new(&log)).tool(Asker));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let worker = Worker::spawn(&rt);
    let deadline = Instant::now() + Duration::from_secs(20);
    let parked = loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.waiting {
            break view;
        }
        assert!(Instant::now() < deadline, "the run did not park");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    worker.stop().await;
    assert_eq!(
        parked.state["ordered_calls"],
        json!(["c2"]),
        "in the snapshot"
    );
    let state = conversation(&rt, &parked).await;
    assert_eq!(
        state.pending_calls[1].arguments,
        json!({"ms": 200, "use": "independent", "tag": 2})
    );

    // Answered in a process that runs ten at once: the snapshot still says c2 follows the ones before
    // it, and c3 waits for c2.
    let agent = h.agent(10, |b| b.tool(Probe::new(&log)).tool(Asker));
    let rt = h.runtime(agent, Duration::from_secs(10));
    let worker = Worker::spawn(&rt);
    rt.deliver(run, user_message("prod"))
        .await
        .expect("deliver");
    let view = wait_terminal(&rt, run).await;
    worker.stop().await;
    assert_eq!(view.status, RunStatus::Done);
    assert!(
        end_position(&log, "c2") < start_position(&log, "c3"),
        "the barrier came back from the snapshot: {:?}",
        log.events.lock().unwrap()
    );
}

#[test]
fn of_two_calls_that_read_the_same_room_only_one_keeps_a_file_that_fits_once() {
    let file = || {
        ToolOutput::text("made").with_artifact(
            Artifact::file("f", "application/octet-stream", "f.bin", vec![7; 4 << 20])
                .expect("a file within the cap"),
        )
    };
    let ctx = ToolCtx::detached("t", "c1", Arc::new(adam_runtime::NoopSink));
    let (a_ctx, b_ctx) = (ctx.clone(), ctx.clone());
    // Both read 6 MiB of room before either takes any, as two calls of a segment can.
    assert_eq!((a_ctx.files_left(), b_ctx.files_left()), (6 << 20, 6 << 20));
    let (mut a, mut b) = (file(), file());
    a_ctx.spend_files_of(&mut a);
    b_ctx.spend_files_of(&mut b);
    assert_eq!(a.artifacts.len(), 1, "the first keeps its file");
    assert!(!a.is_error);
    assert!(
        b.artifacts.is_empty(),
        "the second drops it instead of journaling it"
    );
    assert!(b.is_error);
    assert!(
        b.content
            .starts_with("made\nNot shared: f. A run may share at most 6 MiB"),
        "{}",
        b.content
    );
    assert_eq!(ctx.files_left(), 2 << 20);
    assert!(
        !ctx.try_spend_files(3 << 20),
        "nothing taken when it does not fit"
    );
    assert_eq!(ctx.files_left(), 2 << 20);
}

/// Every `after_previous: true` of a turn is a barrier, not only the first: the second waits for the
/// calls between the two, and the calls after it wait for it.
#[tokio::test(start_paused = true)]
async fn every_after_previous_of_a_turn_is_a_barrier() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![
        call(
            "c1",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 1}),
        ),
        call(
            "c2",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 2, "after_previous": true}),
        ),
        call(
            "c3",
            "probe",
            json!({"ms": 300, "use": "independent", "tag": 3}),
        ),
        call(
            "c4",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 4, "after_previous": true}),
        ),
        call(
            "c5",
            "probe",
            json!({"ms": 10, "use": "independent", "tag": 5}),
        ),
    ]);
    h.mock.push_text("done");
    let (_rt, _, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    let events = || log.events.lock().unwrap().clone();
    assert!(
        end_position(&log, "c3") < start_position(&log, "c4"),
        "the second barrier waited for the call before it: {:?}",
        events()
    );
    assert!(
        end_position(&log, "c4") < start_position(&log, "c5"),
        "nothing after the second barrier started before it was done: {:?}",
        events()
    );
}

/// The answer to a call whose `after_previous` is not a boolean quotes the value the model gave.
#[tokio::test]
async fn a_bad_after_previous_is_quoted_in_the_answer() {
    let h = Harness::new();
    let log = Arc::new(Log::default());
    h.mock.push_tool_calls(vec![call(
        "c1",
        "probe",
        json!({"use": "independent", "tag": 1, "after_previous": "yes"}),
    )]);
    h.mock.push_text("done");
    let (rt, _, view) = h.run(10, "go", &log).await;
    assert_eq!(view.status, RunStatus::Done);
    let answers = tool_answers(&rt, &view).await;
    assert_eq!(
        answers[0],
        (
            "`after_previous` of `probe` must be true or false, not \"yes\". The call did not run."
                .to_owned(),
            true
        )
    );
}

/// The line that refuses a file says how much the run had shared before the call: here the 4 MiB the
/// first call took.
#[test]
fn a_refused_file_says_how_much_the_run_has_shared() {
    let file = || {
        ToolOutput::text("made").with_artifact(
            Artifact::file("f", "application/octet-stream", "f.bin", vec![7; 4 << 20])
                .expect("a file within the cap"),
        )
    };
    let ctx = ToolCtx::detached("t", "c1", Arc::new(adam_runtime::NoopSink));
    let (mut a, mut b) = (file(), file());
    ctx.spend_files_of(&mut a);
    ctx.spend_files_of(&mut b);
    assert_eq!(
        b.content,
        "made\nNot shared: f. A run may share at most 6 MiB of files in all, and it has shared \
         4096 KiB so far."
    );
}

/// The reserved name is refused at every limit above one, not only at the default.
#[test]
fn an_own_after_previous_is_a_build_error_at_every_limit_above_one() {
    for limit in [2usize, 32] {
        let log = Arc::new(Log::default());
        let model = Arc::new(MockModel::new());
        let err = LlmAgent::builder("a", model, "m")
            .max_parallel(limit)
            .tool(Owner { log })
            .try_build()
            .unwrap_err();
        assert!(
            matches!(&err, adam_llm_agent::BuildError::ReservedArgument { tool, .. } if tool == "owner"),
            "{limit}: {err:?}"
        );
    }
}
