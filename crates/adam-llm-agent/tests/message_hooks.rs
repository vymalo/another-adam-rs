//! The deciding hook `message.received` (ADR 0050, slices H1 and H1b): a person's message may be
//! allowed, rewritten, or refused with a reason, and the decision is journaled in a step of its own
//! that exists **only when a hook set is installed**.
//!
//! Five cases are the slice's proof: a refused start fails the run with the reason and the `Rejected`
//! class; a refused steer is left out and the task goes on; a replay asks no hook again; a journal
//! written without the step (no hook configured) replays unchanged; and a message's files reach a
//! hook by reference, never by value.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    HookDecision, HookResult, HookSet, HookedMessage, LlmAgent, MessageDecision, MessageFile,
    Plugin, Tool, ToolCtx, ToolError, ToolOutput, Verdict, user_message,
};
use adam_model::{MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, Inbound, RunEvent, RunView, Runtime, RuntimeBuilder};
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

async fn wait_failed(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "failed", |v| v.status == RunStatus::Failed).await
}

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn short_lease(b: RuntimeBuilder) -> RuntimeBuilder {
    b.lease_ttl(Duration::from_millis(300))
}

/// A message as the A2A backend delivers it: kind `message`, the sender's message id as the id.
fn steer(id: &str, text: &str) -> Inbound {
    Inbound::new("message", json!({ "text": text })).with_id(id)
}

/// The recorded `message:` step with `name`, when the journal holds one.
async fn message_record(store: &DynStore, run: RunId, name: &str) -> Option<MessageDecision> {
    let journal = store.journal_list(run).await.expect("journal");
    let entry = journal.iter().find(|entry| entry.name == name)?;
    Some(
        serde_json::from_value(entry.payload.clone())
            .unwrap_or_else(|e| panic!("the record of {name} does not decode: {e}")),
    )
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

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// A plugin that refuses a message whose text contains `needle`, and allows every other one.
struct RefusesText {
    needle: &'static str,
    reason: &'static str,
}

impl Plugin for RefusesText {
    fn name(&self) -> &str {
        "refuses-text"
    }

    fn message_received(&self, message: &mut HookedMessage<'_>) -> HookResult {
        if message.text.contains(self.needle) {
            return Ok(Verdict::Deny(self.reason.to_owned()));
        }
        Ok(Verdict::Continue)
    }
}

/// A plugin that rewrites a message's text, keeping everything else.
struct Rewrites {
    from: &'static str,
    to: &'static str,
}

impl Plugin for Rewrites {
    fn name(&self) -> &str {
        "rewrites"
    }

    fn message_received(&self, message: &mut HookedMessage<'_>) -> HookResult {
        *message.text = message.text.replace(self.from, self.to);
        Ok(Verdict::Continue)
    }
}

/// A plugin that counts how often a message is decided, and what it saw of one.
#[derive(Default)]
struct Watches {
    decisions: AtomicUsize,
    texts: Mutex<Vec<String>>,
    files: Mutex<Vec<MessageFile>>,
}

impl Plugin for Watches {
    fn name(&self) -> &str {
        "watches"
    }

    fn message_received(&self, message: &mut HookedMessage<'_>) -> HookResult {
        self.decisions.fetch_add(1, SeqCst);
        self.texts.lock().unwrap().push(message.text.clone());
        self.files.lock().unwrap().extend_from_slice(message.files);
        Ok(Verdict::Continue)
    }
}

/// Blocks until it is released the first time it is called, and answers at once after that.
struct Waits {
    reached: Arc<Notify>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Waits {
    fn spec(&self) -> ToolSpec {
        spec("waits")
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        self.reached.notify_one();
        self.release.notified().await;
        self.calls.fetch_add(1, SeqCst);
        Ok(ToolOutput::text("waited"))
    }
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

fn png() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    v.extend_from_slice(&[0, 0, 0, 8, 0, 0, 0, 8, 8, 2, 0, 0, 0]);
    v.extend(std::iter::repeat_n(0xAB, 200));
    v
}

// ---------------------------------------------------------------------------
// 1. A refused start fails the run with the reason
// ---------------------------------------------------------------------------

/// H1b: a refused start never reaches a model, and the run fails with the plugin's reason — a
/// `Rejected` failure, not a crash of the loop.
#[tokio::test]
async fn a_refused_start_fails_the_run_with_the_reason_and_the_rejected_class() {
    let h = Harness::new();
    let agent = h
        .builder()
        .plugin(RefusesText {
            needle: "forbidden",
            reason: "this task does not accept that",
        })
        .build();
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("a forbidden request"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_failed(&rt, run).await;
    worker.stop().await;

    assert!(
        h.mock.requests().is_empty(),
        "a refused message never reaches a model"
    );
    let error = view.error.as_deref().unwrap_or_default();
    assert!(
        error.contains("this task does not accept that"),
        "the reason a person reads is the run's: {error}"
    );
    assert!(
        error.contains("refuses-text"),
        "the failure names the plugin: {error}"
    );
    let record = message_record(&h.store, run, "message:start")
        .await
        .expect("the decision is journaled");
    assert_eq!(
        record.hook.as_ref().map(|h| h.verdict.as_str()),
        Some("deny")
    );
    assert_eq!(
        record.hook.as_ref().and_then(|h| h.reason.as_deref()),
        Some("this task does not accept that")
    );
}

/// The start's decision is journaled, so it is not made again on the transition that follows: a
/// message the hook allows reaches the model once, and the hook is asked once.
#[tokio::test]
async fn an_allowed_start_reaches_the_model_once_and_the_hook_is_asked_once() {
    let h = Harness::new();
    let watches = Arc::new(Watches::default());
    let agent = h
        .builder()
        .hooks(HookSet::new().with(watches.clone()))
        .build();
    h.mock.push_text("answered");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("hello"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(watches.decisions.load(SeqCst), 1);
    assert_eq!(watches.texts.lock().unwrap().as_slice(), ["hello"]);
    assert_eq!(
        h.mock.requests()[0].messages.first(),
        Some(&adam_model::Message::user_text("hello"))
    );
    assert_eq!(view.output.as_ref().unwrap()["text"], json!("answered"));
}

/// A rewrite is what the model reads and what the history holds.
#[tokio::test]
async fn a_rewritten_message_is_what_the_model_reads() {
    let h = Harness::new();
    let agent = h
        .builder()
        .plugin(Rewrites {
            from: "colour",
            to: "hue",
        })
        .build();
    h.mock.push_text("answered");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("which colour?"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let done = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        h.mock.requests()[0].messages.first(),
        Some(&adam_model::Message::user_text("which hue?")),
        "the model reads the rewritten text"
    );
    let state = rt.state(done.id).await.expect("state").expect("the run");
    let state: adam_llm_agent::Conversation =
        serde_json::from_value(state).expect("a conversation");
    assert_eq!(
        state.messages.first(),
        Some(&adam_model::Message::user_text("which hue?")),
        "and the history holds it"
    );
    let record = message_record(&h.store, run, "message:start")
        .await
        .expect("the rewrite is journaled");
    assert_eq!(record.text.as_deref(), Some("which hue?"));
    assert!(record.hook.is_none(), "a rewrite is not a refusal");
}

// ---------------------------------------------------------------------------
// 2. A refused steer is left out and the task goes on
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_steer_is_left_out_and_the_task_goes_on() {
    let h = Harness::new();
    h.mock
        .push_tool_calls(vec![call("c1", "waits")])
        .push_text("done");
    let reached = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let agent = h
        .builder()
        .plugin(RefusesText {
            needle: "delete",
            reason: "that is not this task's to do",
        })
        .tool(Waits {
            reached: reached.clone(),
            release: release.clone(),
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .build();
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);

    notified(&reached, "the tool starting").await;
    rt.deliver(run, steer("m-1", "delete the branch"))
        .await
        .expect("deliver");
    release.notify_one();
    let done = wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    assert_eq!(requests.len(), 2, "the task carries on");
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|m| m.text().contains("delete")),
        "the refused message never reaches a model: {:?}",
        requests[1].messages
    );
    let state = rt.state(done.id).await.expect("state").expect("the run");
    let state: adam_llm_agent::Conversation =
        serde_json::from_value(state).expect("a conversation");
    assert!(
        state.read_ids.iter().any(|id| id == "m-1"),
        "the refused message is read once and not asked again: {:?}",
        state.read_ids
    );
    let record = message_record(&h.store, run, "message:m-1")
        .await
        .expect("the decision is journaled");
    assert_eq!(
        record.hook.as_ref().map(|h| h.verdict.as_str()),
        Some("deny")
    );
    assert_eq!(
        record.hook.as_ref().and_then(|h| h.plugin.as_deref()),
        Some("refuses-text")
    );
    let refused = h
        .sink
        .events_for(run)
        .into_iter()
        .find_map(|event| match event {
            RunEvent::Progress { message } if message.contains("refused") => Some(message),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a refusal is never silent: {:?}", h.sink.events_for(run)));
    assert!(refused.contains("refuses-text"), "{refused}");
    assert!(
        refused.contains("that is not this task's to do"),
        "{refused}"
    );
}

// ---------------------------------------------------------------------------
// 3. A replay asks no hook again
// ---------------------------------------------------------------------------

/// The message's step is journaled, then the worker dies inside the tool call. The survivor
/// replays the journal: the message hook is not asked a second time (its counter stays at 1).
#[tokio::test]
async fn a_replay_past_the_messages_step_asks_no_hook_again() {
    let h = Harness::new();
    let watches = Arc::new(Watches::default());
    let reached = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .builder()
        .tool(Stalls {
            reached: reached.clone(),
            calls: calls.clone(),
        })
        .hooks(HookSet::new().with(watches.clone()))
        .build();
    h.mock
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
        watches.decisions.load(SeqCst),
        1,
        "the recorded message step is replayed: its hook is not asked again"
    );
    assert_eq!(
        step_names(&h.store, run)
            .await
            .iter()
            .filter(|name| name.starts_with("message:"))
            .count(),
        1,
        "one message step, however many transitions the run took"
    );
}

// ---------------------------------------------------------------------------
// 4. A journal written without the step replays unchanged
// ---------------------------------------------------------------------------

/// No hook is configured, so no `message:` step is taken: the journal is what it was before the
/// point existed, and a run that crashes and is replayed walks its steps unchanged. This is the
/// case that fails if the step is taken unconditionally or a hook is required.
#[tokio::test]
async fn a_journal_recorded_before_message_hooks_replays_unchanged_with_none_configured() {
    let h = Harness::new();
    let reached = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .builder()
        .tool(Stalls {
            reached: reached.clone(),
            calls: calls.clone(),
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "stalls")])
        .push_text("done");

    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "before-a", short_lease),
        h.runtime_with(&agent, "before-b", short_lease),
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
    let names = step_names(&h.store, run).await;
    assert!(
        !names.iter().any(|name| name.starts_with("message:")),
        "no hook is configured, so no message step is taken: {names:?}"
    );
    assert!(
        names.iter().any(|name| name == "model:0"),
        "the steps the run always took are the same: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. A message's files reach the hook by reference
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_messages_files_reach_the_hook_by_reference() {
    use adam_llm_agent::{InboundImage, user_message_with_images};
    use adam_model::{ImageLimits, ImageOrigin};
    let h = Harness {
        store: Arc::new(MemoryStore::new()),
        mock: Arc::new(MockModel::new().seeing_images(ImageLimits {
            max_bytes: 1 << 20,
            max_count: 4,
            max_side: 4096,
            sources: [ImageOrigin::Person].into(),
        })),
        sink: CollectingSink::new(),
    };
    let watches = Arc::new(Watches::default());
    let agent = h
        .builder()
        .hooks(HookSet::new().with(watches.clone()))
        .build();
    h.mock.push_text("seen");
    let rt = h.runtime(&agent);
    let image = InboundImage::new(Some("shot.png"), Some("image/png"), &png());
    let run = rt
        .start(
            "llm",
            user_message_with_images("look", std::slice::from_ref(&image)),
            None,
        )
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;

    let seen = watches.files.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let file = &seen[0];
    assert_eq!(file.name.as_deref(), Some("shot.png"));
    assert_eq!(file.media_type.as_deref(), Some("image/png"));
    assert!(file.size > 0, "the size is said, not the bytes: {file:?}");
    let reference = file.reference.as_deref().expect("a reference, not bytes");
    assert_eq!(reference.len(), 64, "{reference}");

    // The reference is the one the image step recorded, and no step holds the bytes.
    let journal = h.store.journal_list(run).await.expect("journal");
    let admitted = journal
        .iter()
        .find(|entry| entry.name == "image:start")
        .expect("the image is admitted");
    assert_eq!(
        admitted.payload[0]["image"]["sha256"],
        json!(reference),
        "the hook saw the reference the image step recorded"
    );
    let decision = message_record(&h.store, run, "message:start")
        .await
        .expect("the decision is journaled");
    assert!(decision.hook.is_none(), "the message was allowed");
    let bytes = serde_json::to_string(&decision).expect("json");
    assert!(
        !bytes.contains(&image.data),
        "the decision holds no bytes of the file"
    );
}

/// The point's verdict constants are the words the journal records, so a later build reads them.
#[test]
fn the_point_is_a_word_in_the_journal() {
    assert_eq!(HookDecision::MESSAGE_RECEIVED, "message.received");
}
