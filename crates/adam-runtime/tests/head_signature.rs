//! The runtime signs a run's head at every commit and checks it on every decode (ADR 0068
//! decision 4, slice T3a-2). Each case runs against `MemoryStore`: once as a store that keeps
//! transcript bodies (a v5 head and rows), once as one that does not (the v4 state, the whole
//! state signed, the chain over the agent's own messages).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping
#![allow(dead_code)] // `common` holds helpers other suites use

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::turn::{
    ChainHash, ConversationHash, CurrentKey, DynTurnKeys, KeyId, KeySet, Signature, TurnKeyError,
    TurnKeys, verify_head,
};
use adam_core::{
    BodyRow, BodyWrite, DynStore, MemoryStore, RunId, RunStatus, RunUpdate, head_body,
};
use adam_error::{Classify, ErrorClass};
use adam_runtime::{Agent, AgentError, Ctx, Inbound, Runtime, RuntimeError, Transition};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Chat {
    messages: Vec<Value>,
    turns: u32,
}

/// An agent that parks after each turn, so a test can tamper with the run and wake it.
#[derive(Clone)]
struct ChatAgent {
    name: String,
    /// Declare a split (a v5 head and body rows), or keep the whole state.
    splits: bool,
    /// Give each assistant row a turn signature (opaque: the loop's own signing is T3a-1).
    signed_rows: bool,
    /// Steps taken.
    steps: Arc<AtomicUsize>,
}

impl ChatAgent {
    fn new(prefix: &str, splits: bool) -> Self {
        Self {
            name: format!("{prefix}-{}", Uuid::new_v4().simple()),
            splits,
            signed_rows: false,
            steps: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Agent for ChatAgent {
    type State = Chat;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Chat, AgentError> {
        Ok(Chat {
            messages: vec![json!({"role": "user", "text": input.payload["text"]})],
            turns: 0,
        })
    }

    fn split(&self, state: &Chat) -> Option<(Chat, Vec<Value>)> {
        if !self.splits {
            return None;
        }
        let mut head = state.clone();
        let rows = std::mem::take(&mut head.messages);
        Some((head, rows))
    }

    fn join(&self, head: Chat, rows: Vec<Value>) -> Result<Chat, AgentError> {
        Ok(Chat {
            messages: rows,
            ..head
        })
    }

    async fn step(&self, _ctx: &mut Ctx, mut state: Chat) -> Result<Transition<Chat>, AgentError> {
        self.steps.fetch_add(1, SeqCst);
        state.turns += 1;
        let mut row = json!({"role": "assistant", "text": format!("turn {}", state.turns)});
        if self.signed_rows {
            row["extra_content"] = json!({"adam": {"turn_signature": "opaque"}});
        }
        state.messages.push(row);
        Ok(Transition::Park {
            state,
            wake_at: None,
        })
    }
}

fn hello(text: &str) -> Inbound {
    Inbound::new("message", json!({"text": text}))
}

fn keys() -> DynTurnKeys {
    <dyn TurnKeys>::ephemeral().expect("ephemeral keys")
}

fn runtime(store: &DynStore, agent: &ChatAgent, keys: Option<&DynTurnKeys>) -> Runtime {
    let mut builder = Runtime::builder(store.clone())
        .agent(agent.clone())
        .worker_id(format!("w-{}", Uuid::new_v4().simple()))
        .poll_interval(Duration::from_millis(10))
        .lease_ttl(Duration::from_secs(10));
    if let Some(keys) = keys {
        builder = builder.turn_keys(keys.clone());
    }
    builder.build()
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
    pred: impl Fn(&adam_runtime::RunView) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run");
        if pred(&view) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Parked after `turns` steps and waiting for a message.
async fn parked_after(rt: &Runtime, run: RunId, turns: u32) {
    wait_for(rt, run, "the run to park", |v| {
        v.status == RunStatus::Parked && v.waiting && v.state["turns"] == turns
    })
    .await;
}

/// Start a run, step it once and let it park; the store holds its signed head.
async fn parked_run(rt: &Runtime, agent: &ChatAgent, conversation: &str) -> RunId {
    let run = rt
        .start(&agent.name, hello("hi"), Some(conversation))
        .await
        .expect("start");
    let worker = spawn_worker(rt);
    parked_after(rt, run, 1).await;
    worker.stop().await;
    run
}

fn assert_corrupt(result: Result<Option<Value>, RuntimeError>) -> RuntimeError {
    let err = result.expect_err("the run is refused");
    assert_eq!(err.class(), ErrorClass::Corrupt, "{err}");
    assert!(err.to_string().contains("run "), "{err}");
    err
}

/// Replace the stored document of `run` with `f` applied to it, keeping its status and wake.
async fn tamper(store: &DynStore, run: RunId, f: impl FnOnce(&mut Value)) {
    let rec = store.load_run(run).await.unwrap().unwrap();
    let mut doc = rec.state.clone();
    f(&mut doc);
    let mut update = if doc["v"] == 5 {
        RunUpdate::new(rec.status, Value::Null).head(doc)
    } else {
        RunUpdate::new(rec.status, doc)
    };
    update.wake_at = rec.wake_at;
    store.commit_run(run, rec.version, update).await.unwrap();
}

/// Append `row` to the transcript of `run` and write its head back unchanged, so the store names
/// one more row and the signature does not.
async fn append_row(store: &DynStore, run: RunId, row: Value) {
    let rec = store.load_run(run).await.unwrap().unwrap();
    let (generation, len) = head_body(&rec.state).expect("a head");
    let mut rows: Vec<BodyRow> = store.body_list(run, generation).await.unwrap();
    rows.push(BodyRow {
        idx: len,
        message: row,
    });
    let mut update = RunUpdate::new(rec.status, Value::Null).head(rec.state.clone());
    update.body = Some(BodyWrite {
        generation: None,
        rows,
        writer: Uuid::new_v4(),
    });
    update.wake_at = rec.wake_at;
    store.commit_run(run, rec.version, update).await.unwrap();
}

fn memory() -> DynStore {
    Arc::new(MemoryStore::new())
}

fn without_bodies() -> DynStore {
    Arc::new(common::recording::RecordingStore::new(memory()).without_bodies())
}

/// Keys whose key set can be taken down: while `down` is set, or for the next `failures` reads,
/// `key_set` answers `Unavailable` (class `Transient`); signing is untouched.
struct Flaky {
    inner: DynTurnKeys,
    down: AtomicBool,
    failures: AtomicUsize,
    reads: AtomicUsize,
}

impl Flaky {
    fn over(inner: &DynTurnKeys) -> Arc<Self> {
        Arc::new(Self {
            inner: inner.clone(),
            down: AtomicBool::new(false),
            failures: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl TurnKeys for Flaky {
    async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
        self.inner.current().await
    }

    async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
        self.inner.sign(kid, input).await
    }

    async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
        self.reads.fetch_add(1, SeqCst);
        let once = self
            .failures
            .fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if once || self.down.load(SeqCst) {
            return Err(TurnKeyError::Unavailable(Box::new(std::io::Error::other(
                "the key set is down",
            ))));
        }
        self.inner.key_set().await
    }
}

// ---------------------------------------------------------------------------
// What is written
// ---------------------------------------------------------------------------

/// The head the runtime wrote verifies under the documented payload, taken by hand from the store:
/// the run, the hash of its conversation, the number of rows, the chain over them and the head
/// without its signature, its seal and the generation the store resolved.
#[tokio::test]
async fn the_head_is_signed_over_the_run_the_conversation_the_rows_and_the_state() {
    let store = memory();
    let keys = keys();
    let agent = ChatAgent::new("sign", true);
    let rt = runtime(&store, &agent, Some(&keys));
    let run = parked_after_start(&rt, &agent, "conv-1").await;

    let rec = store.load_run(run).await.unwrap().unwrap();
    assert_eq!(rec.state["v"], 5);
    let jws = rec.state["signature"].as_str().expect("the head is signed");
    let (generation, len) = head_body(&rec.state).expect("a head");
    assert_eq!(len, 2, "the person's row and the assistant's");
    assert_eq!(rec.state["seal"]["len"], 2);
    let rows: Vec<Value> = store
        .body_list(run, generation)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.message)
        .collect();
    let cnv = ConversationHash::of(Some("conv-1"), run);
    let chain = ChainHash::over(run, &cnv, &rows);
    assert_eq!(rec.state["seal"]["chain"], chain.to_string());

    let mut head = rec.state.clone();
    for member in ["signature", "seal"] {
        head.as_object_mut().unwrap().remove(member);
    }
    head["body"].as_object_mut().unwrap().remove("generation");
    let set = keys.key_set().await.unwrap();
    verify_head(&set, run, &cnv, len, &chain, &head, jws).expect("the documented payload verifies");

    // Each member of the payload is bound.
    let other = ConversationHash::of(Some("conv-2"), run);
    assert!(verify_head(&set, run, &other, len, &chain, &head, jws).is_err());
    assert!(verify_head(&set, RunId::new(), &cnv, len, &chain, &head, jws).is_err());
    assert!(verify_head(&set, run, &cnv, len + 1, &chain, &head, jws).is_err());
    let mut changed = head.clone();
    changed["rev"] = json!(99);
    assert!(verify_head(&set, run, &cnv, len, &chain, &changed, jws).is_err());
}

async fn parked_after_start(rt: &Runtime, agent: &ChatAgent, conversation: &str) -> RunId {
    parked_run(rt, agent, conversation).await
}

/// A delivery and the next step keep the head verifying: the delivery signs the document it
/// changed, and the step signs what it writes.
#[tokio::test]
async fn a_delivery_and_a_step_keep_the_run_readable() {
    for store in [memory(), without_bodies()] {
        let keys = keys();
        let agent = ChatAgent::new("round", store.keeps_bodies());
        let rt = runtime(&store, &agent, Some(&keys));
        let run = parked_run(&rt, &agent, "c").await;
        let before = store.load_run(run).await.unwrap().unwrap().state["signature"].clone();
        assert!(before.is_string(), "the first commit signed the run");

        rt.deliver(run, hello("again")).await.expect("deliver");
        let delivered = store.load_run(run).await.unwrap().unwrap();
        assert_ne!(
            delivered.state["signature"], before,
            "the inbox changed, so the signature did"
        );
        rt.state(run)
            .await
            .expect("a delivered run is readable")
            .expect("the run");

        let worker = spawn_worker(&rt);
        parked_after(&rt, run, 2).await;
        worker.stop().await;
        rt.state(run)
            .await
            .expect("a stepped run is readable")
            .expect("the run");
        assert_eq!(agent.steps.load(SeqCst), 2);
    }
}

/// A store that keeps no bodies holds the whole state, signed; one that does holds the head. Both
/// round-trip their signature, and a cancel (which ends a run) signs the document it writes.
#[tokio::test]
async fn both_kinds_of_store_round_trip_the_signature() {
    for (store, v) in [(memory(), 5), (without_bodies(), 1)] {
        let keys = keys();
        let agent = ChatAgent::new("both", v == 5);
        let rt = runtime(&store, &agent, Some(&keys));
        let run = parked_run(&rt, &agent, "c").await;
        let rec = store.load_run(run).await.unwrap().unwrap();
        assert_eq!(rec.state["v"], v);
        assert!(rec.state["signature"].is_string());
        assert_eq!(
            rec.state.get("seal").is_some(),
            v == 5,
            "only a head carries a seal"
        );
        rt.state(run).await.expect("reads").expect("run");

        rt.cancel(run, "stop").await.expect("cancel");
        let cancelled = store.load_run(run).await.unwrap().unwrap();
        assert_eq!(cancelled.status, RunStatus::Failed);
        assert!(
            cancelled.state["signature"].is_string(),
            "the cancel signed what it wrote"
        );
        rt.state(run)
            .await
            .expect("a cancelled run reads")
            .expect("run");
    }
}

// ---------------------------------------------------------------------------
// What is refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_changed_head_field_is_refused_corrupt() {
    for splits in [true, false] {
        let store = if splits { memory() } else { without_bodies() };
        let keys = keys();
        let agent = ChatAgent::new("field", splits);
        let rt = runtime(&store, &agent, Some(&keys));
        let run = parked_run(&rt, &agent, "c").await;
        rt.state(run).await.expect("intact").expect("run");

        // The agent's own state, then a member of the runtime's.
        tamper(&store, run, |doc| doc["agent"]["turns"] = json!(7)).await;
        assert_corrupt(rt.state(run).await);
    }
}

#[tokio::test]
async fn a_member_of_the_runtime_changed_is_refused_corrupt() {
    let store = memory();
    let keys = keys();
    let agent = ChatAgent::new("rev", true);
    let rt = runtime(&store, &agent, Some(&keys));
    let run = parked_run(&rt, &agent, "c").await;
    tamper(&store, run, |doc| doc["error"] = json!("forged")).await;
    assert_corrupt(rt.state(run).await);
    // A delivery does not launder it: it reads the head it changes and refuses first.
    let err = rt.deliver(run, hello("x")).await.expect_err("refused");
    assert_eq!(err.class(), ErrorClass::Corrupt);
}

/// Appending a row makes the **store** raise the head's `body.len` (the head is written back
/// otherwise unchanged), so what this exercises is a `len` that no longer matches the signed one,
/// and a chain that no longer matches the rows: either alone refuses the run.
#[tokio::test]
async fn a_row_appended_past_len_is_refused_corrupt() {
    let store = memory();
    let keys = keys();
    let agent = ChatAgent::new("append", true);
    let rt = runtime(&store, &agent, Some(&keys));
    let run = parked_run(&rt, &agent, "c").await;
    rt.state(run).await.expect("intact").expect("run");

    append_row(&store, run, json!({"role": "user", "text": "forged"})).await;
    assert_corrupt(rt.state(run).await);
}

#[tokio::test]
async fn a_row_changed_in_the_transcript_is_refused_corrupt() {
    for splits in [true, false] {
        let store = if splits { memory() } else { without_bodies() };
        let keys = keys();
        let agent = ChatAgent::new("row", splits);
        let rt = runtime(&store, &agent, Some(&keys));
        let run = parked_run(&rt, &agent, "c").await;
        if splits {
            // The rows are not in the head: replace the first of the generation.
            let rec = store.load_run(run).await.unwrap().unwrap();
            let (generation, _) = head_body(&rec.state).unwrap();
            let mut rows = store.body_list(run, generation).await.unwrap();
            rows[0].message = json!({"role": "user", "text": "someone else"});
            // A compaction writes a new generation: it is the one way the store lets a row change.
            let mut update = RunUpdate::new(rec.status, Value::Null).head(rec.state.clone());
            update.body = Some(BodyWrite {
                generation: Some(Uuid::new_v4()),
                rows,
                writer: Uuid::new_v4(),
            });
            update.wake_at = rec.wake_at;
            store.commit_run(run, rec.version, update).await.unwrap();
        } else {
            tamper(&store, run, |doc| {
                doc["agent"]["messages"][0] = json!({"role": "user", "text": "someone else"});
            })
            .await;
        }
        assert_corrupt(rt.state(run).await);
    }
}

#[tokio::test]
async fn a_run_moved_to_another_run_and_conversation_is_refused_corrupt() {
    let store = memory();
    let keys = keys();
    let agent = ChatAgent::new("moved", true);
    let rt = runtime(&store, &agent, Some(&keys));
    let first = parked_run(&rt, &agent, "conv-a").await;
    let second = parked_run(&rt, &agent, "conv-b").await;
    rt.state(second).await.expect("intact").expect("run");

    // The second run's document and rows replaced by the first's: both signed by adam.
    let source = store.load_run(first).await.unwrap().unwrap();
    let (generation, _) = head_body(&source.state).unwrap();
    let rows = store.body_list(first, generation).await.unwrap();
    let target = store.load_run(second).await.unwrap().unwrap();
    let mut update = RunUpdate::new(target.status, Value::Null).head(source.state.clone());
    update.body = Some(BodyWrite {
        generation: Some(Uuid::new_v4()),
        rows,
        writer: Uuid::new_v4(),
    });
    update.wake_at = target.wake_at;
    store
        .commit_run(second, target.version, update)
        .await
        .unwrap();

    rt.state(first)
        .await
        .expect("the original is intact")
        .expect("run");
    assert_corrupt(rt.state(second).await);
}

/// A head with no signature is adopted when the run has no signed turn (release N of decision 11),
/// and signed at its next commit. One that carries a signed turn is refused.
#[tokio::test]
async fn an_unsigned_head_is_adopted_unless_it_covers_a_signed_turn() {
    let store = memory();
    let keys = keys();
    let agent = ChatAgent::new("adopt", true);
    let plain = runtime(&store, &agent, None);
    let run = parked_run(&plain, &agent, "legacy").await;
    assert!(
        store
            .load_run(run)
            .await
            .unwrap()
            .unwrap()
            .state
            .get("signature")
            .is_none(),
        "no keys, no signature"
    );

    // The same store read by a runtime with keys: adopted, then signed by the next step.
    let signing = runtime(&store, &agent, Some(&keys));
    signing.state(run).await.expect("adopted").expect("run");
    signing.deliver(run, hello("more")).await.expect("deliver");
    assert!(
        store
            .load_run(run)
            .await
            .unwrap()
            .unwrap()
            .state
            .get("signature")
            .is_none(),
        "a delivery does not sign what it found unsigned"
    );
    let worker = spawn_worker(&signing);
    parked_after(&signing, run, 2).await;
    worker.stop().await;
    assert!(store.load_run(run).await.unwrap().unwrap().state["signature"].is_string());
    signing.state(run).await.expect("signed now").expect("run");

    // A run whose rows carry a turn signature must keep its head signature.
    let mut signed_agent = ChatAgent::new("strip", true);
    signed_agent.signed_rows = true;
    let rt = runtime(&store, &signed_agent, Some(&keys));
    let run = parked_run(&rt, &signed_agent, "c").await;
    rt.state(run).await.expect("intact").expect("run");
    tamper(&store, run, |doc| {
        let doc = doc.as_object_mut().unwrap();
        doc.remove("signature");
        doc.remove("seal");
    })
    .await;
    assert_corrupt(rt.state(run).await);
}

/// A run signed by other keys is refused: its key is not in this runtime's set.
#[tokio::test]
async fn a_head_signed_by_a_key_outside_the_set_is_refused_corrupt() {
    let store = memory();
    let agent = ChatAgent::new("foreign", true);
    let rt = runtime(&store, &agent, Some(&keys()));
    let run = parked_run(&rt, &agent, "c").await;
    let other = runtime(&store, &agent, Some(&keys()));
    assert_corrupt(other.state(run).await);
}

/// Without keys the runtime is as it was: nothing signed, nothing required, and a signature in the
/// state is not read (a write without keys drops it).
#[tokio::test]
async fn without_keys_nothing_is_signed_or_required() {
    let store = memory();
    let keys = keys();
    let agent = ChatAgent::new("nokeys", true);
    let signing = runtime(&store, &agent, Some(&keys));
    let run = parked_run(&signing, &agent, "c").await;
    assert!(store.load_run(run).await.unwrap().unwrap().state["signature"].is_string());

    let plain = runtime(&store, &agent, None);
    plain
        .state(run)
        .await
        .expect("read without keys")
        .expect("run");
    plain.deliver(run, hello("x")).await.expect("deliver");
    let doc = store.load_run(run).await.unwrap().unwrap().state;
    assert!(
        doc.get("signature").is_none() && doc.get("seal").is_none(),
        "{doc}"
    );
}

/// A refused head fails the run before the agent runs, with the model never asked: the step
/// counter does not move.
#[tokio::test]
async fn a_refused_run_fails_before_its_agent_is_stepped() {
    let store = memory();
    let keys = keys();
    let agent = ChatAgent::new("nostep", true);
    let rt = runtime(&store, &agent, Some(&keys));
    let run = parked_run(&rt, &agent, "c").await;
    assert_eq!(agent.steps.load(SeqCst), 1);

    append_row(&store, run, json!({"role": "user", "text": "forged"})).await;
    // A delivery refuses it too (the head now names another length); the run is made runnable
    // by the store, as a lost lease would.
    let err = rt.deliver(run, hello("wake")).await.expect_err("refused");
    assert_eq!(err.class(), ErrorClass::Corrupt);
    let rec = store.load_run(run).await.unwrap().unwrap();
    let mut update = RunUpdate::new(RunStatus::Runnable, Value::Null).head(rec.state.clone());
    update.wake_at = None;
    store.commit_run(run, rec.version, update).await.unwrap();
    let worker = spawn_worker(&rt);
    wait_for(&rt, run, "the run to fail", |v| {
        v.status == RunStatus::Failed
    })
    .await;
    worker.stop().await;
    assert_eq!(agent.steps.load(SeqCst), 1, "the agent was not stepped");
}

// ---------------------------------------------------------------------------
// What is not a forgery
// ---------------------------------------------------------------------------

/// A key set that cannot be read says nothing about the run: the claim leaves the run (lease kept)
/// instead of failing it, and the run is stepped when the keys are back.
#[tokio::test]
async fn a_key_outage_at_claim_leaves_the_run_and_it_completes_later() {
    for store in [memory(), without_bodies()] {
        let keys = keys();
        let agent = ChatAgent::new("outage", true);
        let first = runtime(&store, &agent, Some(&keys));
        let run = parked_run(&first, &agent, "c").await;
        first.deliver(run, hello("again")).await.expect("deliver");

        let flaky = Flaky::over(&keys);
        flaky.failures.store(1, SeqCst);
        let flaky_keys: DynTurnKeys = flaky.clone();
        let rt = Runtime::builder(store.clone())
            .agent(agent.clone())
            .worker_id(format!("w-{}", Uuid::new_v4().simple()))
            .poll_interval(Duration::from_millis(10))
            .lease_ttl(Duration::from_millis(300))
            .turn_keys(flaky_keys)
            .build();
        let worker = spawn_worker(&rt);
        parked_after(&rt, run, 2).await;
        worker.stop().await;

        assert!(flaky.reads.load(SeqCst) >= 2, "the first read failed");
        assert_eq!(
            agent.steps.load(SeqCst),
            2,
            "stepped once, after the outage"
        );
        let view = rt.view(run).await.unwrap().unwrap();
        assert_ne!(view.status, RunStatus::Failed, "{view:#?}");
        assert!(view.error.is_none(), "{view:#?}");
        rt.state(run).await.expect("readable").expect("run");
    }
}

/// A front end that hosts no agent still reads a signed v4 run and delivers to it: it cannot chain
/// the agent's rows, so it checks none (a documented gap), and what it writes is unsigned and
/// adopted by the worker's next read.
#[tokio::test]
async fn a_front_end_without_the_agent_reads_and_delivers_to_a_signed_v4_run() {
    let store = without_bodies();
    let keys = keys();
    // A splitting agent on a store with no bodies: a v4 state chained over the agent's own rows.
    let agent = ChatAgent::new("front", true);
    let worker_rt = runtime(&store, &agent, Some(&keys));
    let run = parked_run(&worker_rt, &agent, "c").await;
    let rec = store.load_run(run).await.unwrap().unwrap();
    assert_eq!(rec.state["v"], 1, "a v4 state, the whole run");
    assert!(rec.state["signature"].is_string());

    let front = Runtime::builder(store.clone())
        .turn_keys(keys.clone())
        .build();
    let state = front
        .state(run)
        .await
        .expect("a front end reads a signed v4 run it cannot chain")
        .expect("run");
    assert_eq!(state["turns"], 1);
    front
        .deliver(run, hello("from the front"))
        .await
        .expect("a front end delivers to it");
    let delivered = store.load_run(run).await.unwrap().unwrap();
    assert!(
        delivered.state.get("signature").is_none(),
        "it could not sign what it could not chain"
    );

    // The worker adopts it (no signed turn) and the run goes on.
    let worker = spawn_worker(&worker_rt);
    parked_after(&worker_rt, run, 2).await;
    worker.stop().await;
    assert!(store.load_run(run).await.unwrap().unwrap().state["signature"].is_string());
}

/// A state that does not decode as the agent's is `Corrupt`, never an empty transcript.
#[tokio::test]
async fn a_v4_state_the_agent_cannot_decode_is_refused_corrupt() {
    let store = without_bodies();
    let keys = keys();
    let agent = ChatAgent::new("undecodable", true);
    let rt = runtime(&store, &agent, Some(&keys));
    let run = parked_run(&rt, &agent, "c").await;
    tamper(&store, run, |doc| {
        doc["agent"] = json!({"messages": "not rows"})
    })
    .await;
    assert_corrupt(rt.state(run).await);
}

// ---------------------------------------------------------------------------
// A cancel
// ---------------------------------------------------------------------------

/// A cancel during a key outage fails and writes nothing (it does not strip a good signature); a
/// cancel of a corrupt run still ends it.
#[tokio::test]
async fn a_cancel_during_a_key_outage_fails_and_a_cancel_of_a_corrupt_run_ends_it() {
    for store in [memory(), without_bodies()] {
        let keys = keys();
        let agent = ChatAgent::new("cancel", store.keeps_bodies());
        let flaky = Flaky::over(&keys);
        let flaky_keys: DynTurnKeys = flaky.clone();
        let rt = runtime(&store, &agent, Some(&flaky_keys));
        let run = parked_run(&rt, &agent, "c").await;
        let before = store.load_run(run).await.unwrap().unwrap();

        flaky.down.store(true, SeqCst);
        let err = rt.cancel(run, "stop").await.expect_err("keys are down");
        assert_eq!(err.class(), ErrorClass::Transient, "{err}");
        assert!(matches!(err, RuntimeError::TurnKeys(_)), "{err}");
        let after = store.load_run(run).await.unwrap().unwrap();
        assert_eq!(after.version, before.version, "nothing was written");
        assert_eq!(after.status, before.status);
        assert_eq!(after.state, before.state);

        // Keys back: the same cancel lands, signed.
        flaky.down.store(false, SeqCst);
        rt.cancel(run, "stop").await.expect("cancel");
        let cancelled = store.load_run(run).await.unwrap().unwrap();
        assert_eq!(cancelled.status, RunStatus::Failed);
        assert!(cancelled.state["signature"].is_string());

        // A corrupt run is cancelled too, and the head it writes is unsigned.
        let corrupt = parked_run(&rt, &agent, "c2").await;
        tamper(&store, corrupt, |doc| doc["error"] = json!("forged")).await;
        rt.cancel(corrupt, "stop")
            .await
            .expect("a corrupt run can be cancelled");
        let ended = store.load_run(corrupt).await.unwrap().unwrap();
        assert_eq!(ended.status, RunStatus::Failed);
        assert!(ended.state.get("signature").is_none(), "{}", ended.state);
    }
}

// ---------------------------------------------------------------------------
// A child's result
// ---------------------------------------------------------------------------

/// Finishes at its first step with the output `"real"`.
#[derive(Clone)]
struct Finisher {
    name: String,
    splits: bool,
}

#[async_trait]
impl Agent for Finisher {
    type State = Chat;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Chat, AgentError> {
        Ok(Chat {
            messages: vec![json!({"role": "user", "text": input.payload["text"]})],
            turns: 0,
        })
    }

    fn split(&self, state: &Chat) -> Option<(Chat, Vec<Value>)> {
        if !self.splits {
            return None;
        }
        let mut head = state.clone();
        let rows = std::mem::take(&mut head.messages);
        Some((head, rows))
    }

    fn join(&self, head: Chat, rows: Vec<Value>) -> Result<Chat, AgentError> {
        Ok(Chat {
            messages: rows,
            ..head
        })
    }

    async fn step(&self, _ctx: &mut Ctx, state: Chat) -> Result<Transition<Chat>, AgentError> {
        Ok(Transition::Done {
            state,
            output: json!("real"),
        })
    }
}

/// Waits for `go`, then reads its child both ways and finishes with what it was told.
#[derive(Clone)]
struct Parent {
    name: String,
}

#[async_trait]
impl Agent for Parent {
    type State = Value;

    fn name(&self) -> &str {
        &self.name
    }

    fn init(&self, input: Inbound) -> Result<Value, AgentError> {
        Ok(input.payload)
    }

    async fn step(&self, ctx: &mut Ctx, state: Value) -> Result<Transition<Value>, AgentError> {
        let inbox = ctx.take_inbox();
        if !inbox.iter().any(|i| i.kind == "go") {
            return Ok(Transition::Park {
                state,
                wake_at: None,
            });
        }
        let child: RunId = serde_json::from_value(state["child"].clone()).unwrap();
        let status = ctx.child_status(child).await?;
        let look = ctx.child_look(child).await?;
        Ok(Transition::Done {
            state,
            output: json!({
                "status": status,
                "look": look.map(|look| json!({"status": look.status, "head": look.head})),
            }),
        })
    }
}

/// A finished child whose head was changed after it was signed is not handed to its parent as its
/// output: the parent is told the child failed, by `child_status` and by `child_look` alike. The
/// control, an untouched child, reaches the parent whole.
#[tokio::test]
async fn a_tampered_finished_child_is_not_passed_to_its_parent() {
    for splits in [true, false] {
        let store = if splits { memory() } else { without_bodies() };
        let keys = keys();
        let child_agent = Finisher {
            name: format!("kid-{}", Uuid::new_v4().simple()),
            splits,
        };
        let parent_agent = Parent {
            name: format!("parent-{}", Uuid::new_v4().simple()),
        };
        let rt = Runtime::builder(store.clone())
            .agent(child_agent.clone())
            .agent(parent_agent.clone())
            .worker_id(format!("w-{}", Uuid::new_v4().simple()))
            .poll_interval(Duration::from_millis(10))
            .turn_keys(keys.clone())
            .build();

        let mut parents = Vec::new();
        for _ in 0..2 {
            let child = RunId::new();
            let parent = rt
                .start(
                    &parent_agent.name,
                    Inbound::new("start", json!({"child": child})),
                    None,
                )
                .await
                .expect("start");
            rt.start_child(parent, child, &child_agent.name, hello("work"))
                .await
                .expect("child");
            parents.push((parent, child));
        }
        let worker = spawn_worker(&rt);
        for (parent, child) in &parents {
            wait_for(&rt, *child, "the child to finish", |v| {
                v.status == RunStatus::Done
            })
            .await;
            wait_for(&rt, *parent, "the parent to wait", |v| v.waiting).await;
        }

        // The second child is changed after it was signed; the first is the control.
        let (_, forged) = parents[1];
        tamper(&store, forged, |doc| doc["output"] = json!("forged")).await;

        for (parent, _) in &parents {
            rt.deliver(*parent, Inbound::new("go", json!({})))
                .await
                .expect("go");
        }
        for (parent, _) in &parents {
            wait_for(&rt, *parent, "the parent to finish", |v| {
                v.status == RunStatus::Done
            })
            .await;
        }
        worker.stop().await;

        let control = rt
            .view(parents[0].0)
            .await
            .unwrap()
            .unwrap()
            .output
            .unwrap();
        assert_eq!(control["status"]["status"], "done", "{control}");
        assert_eq!(control["status"]["output"], "real", "{control}");
        assert_eq!(control["look"]["status"]["output"], "real", "{control}");

        let told = rt
            .view(parents[1].0)
            .await
            .unwrap()
            .unwrap()
            .output
            .unwrap();
        for status in [&told["status"], &told["look"]["status"]] {
            assert_eq!(status["status"], "failed", "{told}");
            assert!(status.get("output").is_none(), "{told}");
            assert!(
                status["error"].as_str().unwrap().contains("refused"),
                "{told}"
            );
        }
        assert!(!told.to_string().contains("forged"), "{told}");
    }
}
