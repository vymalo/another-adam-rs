//! Remote tasks, seen from an `LlmAgent`: a tool starts a task on another system and returns
//! `ToolError::AwaitRemote`, the run parks with a timer, and each time it fires the agent asks the
//! tool how the task stands (`Tool::poll_remote`) until the tool has an answer.
//!
//! Run against `MemoryStore` always and against PostgreSQL when `ADAM_TEST_POSTGRES_URL` is set.
//! Nothing sleeps for a fixed time: the 60 s wait timer is moved with a `ManualClock`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, PendingRemote, PendingWait, RemotePoll, Tool, ToolCtx, ToolError,
    ToolOutput, user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{
    CollectingSink, ManualClock, RetryPolicy, RunEvent, RunView, Runtime, RuntimeBuilder,
    RuntimeError,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

async fn stores() -> Vec<(&'static str, DynStore)> {
    let mut all: Vec<(&'static str, DynStore)> = vec![("memory", Arc::new(MemoryStore::new()))];
    if let Some(url) = adam_core::testing::test_env("ADAM_TEST_POSTGRES_URL") {
        let store = adam_store_postgres::PgStore::connect(&url)
            .await
            .expect("connect to postgres");
        adam_core::Store::migrate(&store).await.expect("migrate");
        all.push(("postgres", Arc::new(store)));
    }
    all
}

fn uniq(prefix: &str) -> String {
    format!("{prefix}-{}", RunId::new())
}

/// What the "remote system" answers to each look, in order; the last answer repeats.
type Script = Arc<Mutex<VecDeque<Result<RemotePoll, ToolError>>>>;

fn script(answers: Vec<Result<RemotePoll, ToolError>>) -> Script {
    Arc::new(Mutex::new(answers.into()))
}

/// A tool that "starts a task" (counting the starts) and waits for it.
struct RemoteTool {
    name: &'static str,
    starts: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    answers: Script,
    timeout_ms: Option<u64>,
    /// Polls without an answer script: the tool does not implement `poll_remote`.
    no_poll: bool,
}

impl RemoteTool {
    fn new(answers: Script) -> Self {
        Self {
            name: "remote",
            starts: Arc::default(),
            polls: Arc::default(),
            answers,
            timeout_ms: None,
            no_poll: false,
        }
    }
}

#[async_trait]
impl Tool for RemoteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.into(),
            description: "starts a remote task".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.starts.fetch_add(1, SeqCst);
        Err(ToolError::AwaitRemote {
            task: format!("task-of-{}", ctx.call_id()),
            timeout_ms: self.timeout_ms,
        })
    }

    async fn poll_remote(&self, _ctx: &ToolCtx, task: &str) -> Result<RemotePoll, ToolError> {
        if self.no_poll {
            return Err(ToolError::Permanent("does not wait on remote tasks".into()));
        }
        assert!(task.starts_with("task-of-"), "{task}");
        self.polls.fetch_add(1, SeqCst);
        let mut answers = self.answers.lock().unwrap();
        if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers.front().cloned().expect("an answer")
        }
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    }
}

struct Rig {
    name: String,
    mock: Arc<MockModel>,
    sink: CollectingSink,
    clock: ManualClock,
}

impl Rig {
    /// The model calls `remote` once (call id `c1`), then answers with `text`.
    fn new(text: &str) -> Self {
        let mock = Arc::new(MockModel::new());
        mock.push_tool_calls(vec![call("c1", "remote")])
            .push_text(text);
        Self {
            name: uniq("parent"),
            mock,
            sink: CollectingSink::new(),
            clock: ManualClock::new(),
        }
    }

    fn agent(&self, tool: RemoteTool) -> LlmAgent {
        let model: DynModel = self.mock.clone();
        LlmAgent::builder(&self.name, model, "m").tool(tool).build()
    }

    fn runtime(&self, store: &DynStore, agent: LlmAgent) -> Runtime {
        self.builder(store).agent(agent).build()
    }

    fn builder(&self, store: &DynStore) -> RuntimeBuilder {
        Runtime::builder(store.clone())
            .worker_id(format!("w-{}", RunId::new()))
            .event_sink(self.sink.clone())
            .clock(self.clock.clone())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
                multiplier: 1.0,
            })
    }
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

async fn wait_for(
    rt: &Runtime,
    run: RunId,
    what: &str,
    pred: impl Fn(&RunView) -> bool,
) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
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

/// Parked on the wait timer by a commit newer than `version`: the run has been stepped since.
async fn wait_parked_since(rt: &Runtime, run: RunId, version: u64) -> RunView {
    wait_for(rt, run, "parked on the wait timer again", |v| {
        v.status == RunStatus::Parked && v.wake_at.is_some() && v.version > version
    })
    .await
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

fn tool_results(state: &Conversation) -> Vec<(String, String, bool)> {
    state
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool {
                call_id,
                content,
                is_error,
            } => Some((call_id.clone(), content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn ends(rig: &Rig, run: RunId) -> Vec<String> {
    rig.sink
        .events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Custom { kind, payload } if kind == "tool_end" => {
                Some(payload["status"].as_str().unwrap().to_owned())
            }
            RunEvent::Custom { kind, .. } if kind == "awaiting_remote" => Some(kind),
            _ => None,
        })
        .collect()
}

fn ready(text: &str) -> Result<RemotePoll, ToolError> {
    Ok(RemotePoll::Ready(ToolOutput::text(text)))
}

const WORKING: Result<RemotePoll, ToolError> = Ok(RemotePoll::Working);

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_run_polls_on_the_timer_until_the_task_is_over_and_starts_it_once() {
    for (backend, store) in stores().await {
        let rig = Rig::new("all done");
        let tool = RemoteTool::new(script(vec![WORKING, WORKING, ready("remote says 42")]));
        let (starts, polls) = (tool.starts.clone(), tool.polls.clone());
        let rt = rig.runtime(&store, rig.agent(tool));
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);

        // The call parks the run on a timer 60 s away, on a clock nobody moves: no look yet.
        let parked = wait_for(&rt, run, "parked on the remote task", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;
        assert_eq!(polls.load(SeqCst), 0, "{backend}: nothing looked yet");
        let PendingWait::Remote(PendingRemote {
            call_id,
            tool,
            task,
            deadline,
        }) = conversation(&parked).pending_wait.expect("a wait")
        else {
            panic!("{backend}: not a remote wait: {:?}", parked.state);
        };
        assert_eq!(
            (call_id.as_str(), tool.as_str(), task.as_str(), deadline),
            ("c1", "remote", "task-of-c1", None),
            "{backend}"
        );

        // Two timers later the task is still going; the third look finds the answer.
        rig.clock.advance(Duration::from_secs(61));
        let parked = wait_parked_since(&rt, run, parked.version).await;
        rig.clock.advance(Duration::from_secs(61));
        wait_parked_since(&rt, run, parked.version).await;
        assert_eq!(
            rig.mock.requests().len(),
            1,
            "{backend}: the model has not spoken since"
        );
        rig.clock.advance(Duration::from_secs(61));
        let done = wait_done(&rt, run).await;
        worker.stop().await;

        assert_eq!(done.output.unwrap()["text"], "all done", "{backend}");
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "remote says 42".into(), false)],
            "{backend}"
        );
        assert_eq!(state.pending_wait, None, "{backend}");
        assert_eq!(starts.load(SeqCst), 1, "{backend}: started once");
        assert_eq!(polls.load(SeqCst), 3, "{backend}");
        assert_eq!(
            ends(&rig, run),
            ["awaiting_remote", "waiting", "ok"],
            "{backend}"
        );
        assert_eq!(rig.mock.requests().len(), 2, "{backend}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_process_keeps_polling_without_starting_the_task_again() {
    for (backend, store) in stores().await {
        let rig = Rig::new("resumed");
        let first = RemoteTool::new(script(vec![WORKING]));
        let (first_starts, first_polls) = (first.starts.clone(), first.polls.clone());
        let rt = rig.runtime(&store, rig.agent(first));
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let parked = wait_for(&rt, run, "parked", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;
        rig.clock.advance(Duration::from_secs(61));
        wait_parked_since(&rt, run, parked.version).await;
        assert_eq!(first_polls.load(SeqCst), 1);
        worker.stop().await;
        drop(rt);
        assert_eq!(first_starts.load(SeqCst), 1);

        // The next process has a tool of its own; it never sees a start, only the polls.
        let second = RemoteTool::new(script(vec![ready("late answer")]));
        let (second_starts, second_polls) = (second.starts.clone(), second.polls.clone());
        let rt = rig.runtime(&store, rig.agent(second));
        let worker = spawn_worker(&rt);
        rig.clock.advance(Duration::from_secs(61));
        let done = wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(done.output.unwrap()["text"], "resumed", "{backend}");
        assert_eq!(
            second_starts.load(SeqCst),
            0,
            "{backend}: not started again"
        );
        assert_eq!(second_polls.load(SeqCst), 1, "{backend}");
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "late answer".into(), false)],
            "{backend}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_that_failed_is_an_error_result_and_the_run_goes_on() {
    for (backend, store) in stores().await {
        let rig = Rig::new("noted");
        let tool = RemoteTool::new(script(vec![Ok(RemotePoll::Ready(ToolOutput::error(
            "the remote agent failed: out of budget",
        )))]));
        let polls = tool.polls.clone();
        let rt = rig.runtime(&store, rig.agent(tool));
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_for(&rt, run, "parked", |v| v.status == RunStatus::Parked).await;
        rig.clock.advance(Duration::from_secs(61));
        wait_done(&rt, run).await;
        worker.stop().await;
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![(
                "c1".into(),
                "the remote agent failed: out of budget".into(),
                true
            )],
            "{backend}"
        );
        assert_eq!(polls.load(SeqCst), 1);
        assert_eq!(
            ends(&rig, run),
            ["awaiting_remote", "waiting", "error"],
            "{backend}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permanent_poll_error_is_an_error_result_and_a_transient_one_is_retried() {
    for (backend, store) in stores().await {
        // Permanent: the model is told.
        let rig = Rig::new("noted");
        let tool = RemoteTool::new(script(vec![Err(ToolError::Permanent(
            "the task is gone".into(),
        ))]));
        let rt = rig.runtime(&store, rig.agent(tool));
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_for(&rt, run, "parked", |v| v.status == RunStatus::Parked).await;
        rig.clock.advance(Duration::from_secs(61));
        wait_done(&rt, run).await;
        worker.stop().await;
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "the task is gone".into(), true)],
            "{backend}"
        );

        // Transient: the step is retried, and the second look finds the answer.
        let rig = Rig::new("fine");
        let tool = RemoteTool::new(script(vec![
            Err(ToolError::Transient("connection reset".into())),
            ready("second try"),
        ]));
        let polls = tool.polls.clone();
        let rt = rig.runtime(&store, rig.agent(tool));
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_for(&rt, run, "parked", |v| v.status == RunStatus::Parked).await;
        rig.clock.advance(Duration::from_secs(61));
        wait_done(&rt, run).await;
        worker.stop().await;
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "second try".into(), false)],
            "{backend}"
        );
        assert_eq!(polls.load(SeqCst), 2, "{backend}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_that_outlives_its_timeout_is_an_error_result_without_another_look() {
    for (backend, store) in stores().await {
        let rig = Rig::new("gave up");
        let mut tool = RemoteTool::new(script(vec![WORKING]));
        tool.timeout_ms = Some(90_000);
        let polls = tool.polls.clone();
        let rt = rig.runtime(&store, rig.agent(tool));
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);
        let parked = wait_for(&rt, run, "parked", |v| {
            v.status == RunStatus::Parked && v.wake_at.is_some()
        })
        .await;
        let PendingWait::Remote(wait) = conversation(&parked).pending_wait.unwrap() else {
            panic!("not a remote wait");
        };
        assert!(wait.deadline.is_some(), "{backend}");

        // Inside the limit: one more look. Past it: no look, an error result.
        rig.clock.advance(Duration::from_secs(61));
        wait_parked_since(&rt, run, parked.version).await;
        rig.clock.advance(Duration::from_secs(61));
        wait_done(&rt, run).await;
        worker.stop().await;
        assert_eq!(polls.load(SeqCst), 1, "{backend}");
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        let results = tool_results(&state);
        assert_eq!(results.len(), 1, "{backend}");
        assert!(results[0].2, "{backend}: an error result");
        assert!(
            results[0].1.contains("did not finish in the time allowed"),
            "{backend}: {results:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tool_that_cannot_be_polled_gives_an_error_result() {
    for (backend, store) in stores().await {
        let rig = Rig::new("noted");
        let mut tool = RemoteTool::new(script(vec![WORKING]));
        tool.no_poll = true;
        let rt = rig.runtime(&store, rig.agent(tool));
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_for(&rt, run, "parked", |v| v.status == RunStatus::Parked).await;
        rig.clock.advance(Duration::from_secs(61));
        wait_done(&rt, run).await;
        worker.stop().await;
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        assert_eq!(
            tool_results(&state),
            vec![("c1".into(), "does not wait on remote tasks".into(), true)],
            "{backend}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_poll_refuses_and_a_user_message_queues_behind_the_result() {
    for (backend, store) in stores().await {
        // A plain tool (default `poll_remote`) that returned AwaitRemote by mistake.
        struct Plain;
        #[async_trait]
        impl Tool for Plain {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "remote".into(),
                    description: "x".into(),
                    parameters: json!({"type": "object", "properties": {}}),
                }
            }
            async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
                Err(ToolError::AwaitRemote {
                    task: "t".into(),
                    timeout_ms: None,
                })
            }
        }
        let rig = Rig::new("done");
        let model: DynModel = rig.mock.clone();
        let agent = LlmAgent::builder(&rig.name, model, "m").tool(Plain).build();
        let rt = rig.runtime(&store, agent);
        let run = rt.start(&rig.name, user_message("go"), None).await.unwrap();
        let worker = spawn_worker(&rt);
        wait_for(&rt, run, "parked", |v| v.status == RunStatus::Parked).await;
        // A message wakes the run: it looks once (and the default refuses), then answers.
        rt.deliver(run, user_message("meanwhile")).await.unwrap();
        wait_done(&rt, run).await;
        worker.stop().await;
        let state = conversation(&rt.view(run).await.unwrap().unwrap());
        let results = tool_results(&state);
        assert_eq!(results.len(), 1, "{backend}");
        assert!(results[0].2 && results[0].1.contains("does not wait on remote tasks"));
        // The message did not land between the call and its result.
        let roles: Vec<&str> = state
            .messages
            .iter()
            .map(|m| match m {
                Message::User { .. } => "user",
                Message::Assistant { .. } => "assistant",
                Message::Tool { .. } => "tool",
            })
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "tool", "user", "assistant"],
            "{backend}"
        );
    }
}
