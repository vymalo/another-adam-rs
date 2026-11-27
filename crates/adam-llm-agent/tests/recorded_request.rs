//! The model step records the final request's system prompt and tools as content-addressed blobs
//! (ADR 0085 decision 7): the run's state names the last call's ([`Conversation::request`]), the
//! same content is stored once however many turns and runs send it, a replay of a recorded call
//! names the same blobs without storing anything, and a store that refuses the bytes never fails
//! the run. A real `Runtime` with a worker, a scripted `MockModel` and a `FaultyStore` over
//! `MemoryStore`, which counts the blob calls.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, RecordedRequest, RequestContent, SourceCtx, Tool, ToolCtx, ToolError,
    ToolOutput, ToolSource, user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{ManualClock, RetryPolicy, RunView, Runtime, blob_id};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

const INSTRUCTIONS: &str = "You are a test agent.";

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
    }
}

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn spec(&self) -> ToolSpec {
        spec("echo")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("echo-out"))
    }
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "echo".into(),
        arguments: json!({"x": "y"}),
        ..Default::default()
    }
}

/// A source that offers nothing on the run's first model call and one more tool, `late`, on every
/// call after it: the tools of a run change once, as when a group is loaded.
#[derive(Default)]
struct Grows {
    listed: AtomicUsize,
}

#[async_trait]
impl ToolSource for Grows {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        if self.listed.fetch_add(1, Ordering::SeqCst) == 0 {
            Vec::new()
        } else {
            vec![spec("late")]
        }
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        (name == "late").then(|| Ok(ToolOutput::text("late-out")))
    }
}

struct Rig {
    faulty: Arc<FaultyStore>,
    store: DynStore,
    mock: Arc<MockModel>,
    agent: LlmAgent,
}

impl Rig {
    fn new() -> Self {
        Self::with(|builder| builder)
    }

    /// A rig whose agent is built by `build` from the default builder.
    fn with(
        build: impl FnOnce(adam_llm_agent::LlmAgentBuilder) -> adam_llm_agent::LlmAgentBuilder,
    ) -> Self {
        let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = build(
            LlmAgent::builder("llm", model, "test-model")
                .instructions(INSTRUCTIONS)
                .tool(Echo),
        )
        .build();
        Self {
            store: faulty.clone(),
            faulty,
            mock,
            agent,
        }
    }

    fn runtime(&self) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(self.agent.clone())
            .worker_id("w")
            .clock(ManualClock::new())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(1))
            .retry(
                RetryPolicy::new(3)
                    .with_backoff(Duration::from_millis(10), Duration::from_millis(10))
                    .with_multiplier(1.0),
            )
            .build()
    }

    async fn start(&self, rt: &Runtime, text: &str) -> RunId {
        rt.start("llm", user_message(text), None)
            .await
            .expect("start")
    }

    /// Step `run` to its end on a worker of `rt`.
    async fn finish(&self, rt: &Runtime, run: RunId) -> RunView {
        let (stop, rx) = oneshot::channel::<()>();
        let worker = {
            let rt = rt.clone();
            tokio::spawn(async move {
                rt.run_worker(async {
                    let _ = rx.await;
                })
                .await
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let view = loop {
            let view = rt.view(run).await.expect("view").expect("run exists");
            if view.status == RunStatus::Done {
                break view;
            }
            assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = stop.send(());
        worker.await.expect("worker task").expect("worker result");
        view
    }
}

async fn conversation(rt: &Runtime, run: RunId) -> Conversation {
    let state = rt.state(run).await.expect("state").expect("the run");
    serde_json::from_value(state).expect("state is a Conversation")
}

/// A purge far in the future: it collects every blob no run references.
async fn purge_unreferenced(store: &DynStore) -> u64 {
    let far = adam_core::store::now() + jiff::SignedDuration::from_hours(24);
    store.purge_blobs(far).await.expect("purge")
}

/// Two turns with the same prompt and tools: the state names the last call's request, its blobs
/// decode to exactly what the model was sent, and each distinct content was stored once (one put
/// and one existence check per blob, on the first turn; the second turn knew them from the state).
/// The commit referenced them, so a purge keeps them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_call_names_its_prompt_and_tools_stored_once() {
    let rig = Rig::new();
    rig.mock.push_tool_calls(vec![call("c1")]).push_text("done");
    let rt = rig.runtime();
    let run = rig.start(&rt, "go").await;
    let view = rig.finish(&rt, run).await;
    assert_eq!(view.output.expect("an output")["text"], "done");

    let state = conversation(&rt, run).await;
    let request = state
        .request
        .clone()
        .expect("the last call recorded its request");
    assert_eq!(request.turn, 1, "the second call is the last");
    // The same prompt and tools on both turns: one distinct request, the first call's.
    assert_eq!(
        state.requests,
        [RecordedRequest {
            turn: 0,
            ..request.clone()
        }],
        "one entry while nothing changes"
    );
    let sent = rig.mock.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].tools, sent[1].tools, "the same tools on both turns");
    let content = request
        .load(rig.store.as_ref())
        .await
        .expect("read")
        .expect("the blobs are there");
    assert_eq!(
        content,
        RequestContent {
            system: sent[1].system.clone(),
            tools: sent[1].tools.clone(),
        }
    );
    assert!(
        content.tools.iter().any(|tool| tool.name == "echo"),
        "the agent's own tool is there"
    );
    // The bytes by hand, as an exporter that does not link this crate reads them.
    let tools = rig.store.get_blob(&request.tools).await.unwrap().unwrap();
    assert_eq!(tools, RecordedRequest::tools_bytes(&sent[1].tools).unwrap());
    assert_eq!(
        serde_json::from_slice::<Vec<ToolSpec>>(&tools).unwrap(),
        sent[1].tools
    );

    assert_eq!(rig.faulty.calls(Method::PutBlob), 2, "one put per content");
    assert_eq!(
        rig.faulty.calls(Method::BlobStored),
        2,
        "the second turn did not even ask"
    );
    assert_eq!(
        purge_unreferenced(&rig.store).await,
        0,
        "the run references both"
    );
    assert!(rig.store.get_blob(&request.tools).await.unwrap().is_some());

    // A second run with the same prompt and tools stores nothing new: it finds the bytes there.
    rig.mock.push_text("again");
    let second = rig.start(&rt, "again").await;
    rig.finish(&rt, second).await;
    let again = conversation(&rt, second).await.request.expect("recorded");
    assert_eq!(again.system, request.system);
    assert_eq!(again.tools, request.tools);
    assert_eq!(again.turn, 0);
    assert_eq!(rig.faulty.calls(Method::PutBlob), 2, "nothing put again");
}

/// The commit of the transition that made the call is lost: the transition runs again from the
/// journal, the recorded call is replayed without the model, and the state names the same request,
/// which the replayed commit references although nothing was stored again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_call_names_the_same_request_without_the_model() {
    let rig = Rig::new();
    rig.mock.push_text("done");
    let rt = rig.runtime();
    let run = rig.start(&rt, "go").await;
    rig.faulty.fail_run(Method::CommitRun, run, 1);
    rig.finish(&rt, run).await;
    assert_eq!(
        rig.faulty.injected(Method::CommitRun),
        1,
        "the first commit was lost"
    );
    assert_eq!(rig.mock.requests().len(), 1, "the replay called no model");

    let request = conversation(&rt, run)
        .await
        .request
        .expect("the replay kept the recorded request");
    let sent = rig.mock.last_request().expect("the one call");
    let tools = RecordedRequest::tools_bytes(&sent.tools).unwrap();
    assert_eq!(
        request,
        RecordedRequest {
            turn: 0,
            system: sent.system.as_deref().map(|s| blob_id(s.as_bytes())),
            tools: blob_id(&tools),
        }
    );
    assert_eq!(
        conversation(&rt, run).await.requests,
        std::slice::from_ref(&request),
        "the replay wrote the same list"
    );
    assert_eq!(
        rig.faulty.calls(Method::PutBlob),
        2,
        "the replay stored nothing"
    );
    assert_eq!(
        purge_unreferenced(&rig.store).await,
        0,
        "the replayed commit named the blobs"
    );
    let content = request.load(rig.store.as_ref()).await.unwrap().unwrap();
    assert_eq!(content.system, sent.system);
    assert!(
        content
            .system
            .as_deref()
            .is_some_and(|system| system.contains(INSTRUCTIONS)),
        "the agent's instructions are in the prompt"
    );
    assert_eq!(content.tools, sent.tools);
}

/// A store that refuses every blob does not fail the run: the calls go on and the state records
/// no request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_put_that_fails_does_not_fail_the_run() {
    let rig = Rig::new();
    rig.faulty.fail_always(Method::PutBlob);
    rig.mock.push_tool_calls(vec![call("c1")]).push_text("done");
    let rt = rig.runtime();
    let run = rig.start(&rt, "go").await;
    let view = rig.finish(&rt, run).await;
    assert_eq!(view.output.expect("an output")["text"], "done");
    assert!(
        rig.faulty.injected(Method::PutBlob) >= 2,
        "every turn tried"
    );
    let state = conversation(&rt, run).await;
    assert_eq!(state.request, None);
    assert!(
        state.requests.is_empty(),
        "nothing recorded, nothing listed"
    );
}

/// Run `rig`'s agent on two tool calls and an answer: three model calls, whose tools grow once
/// (the source's `late` from the second call on). With `lose_commit`, the first commit of the run
/// is lost and its transition is replayed from the journal.
async fn three_calls(rig: &Rig, lose_commit: bool) -> (Conversation, usize) {
    rig.mock
        .push_tool_calls(vec![call("c1")])
        .push_tool_calls(vec![call("c2")])
        .push_text("done");
    let rt = rig.runtime();
    let run = rig.start(&rt, "go").await;
    if lose_commit {
        rig.faulty.fail_run(Method::CommitRun, run, 1);
    }
    rig.finish(&rt, run).await;
    (conversation(&rt, run).await, rig.mock.requests().len())
}

/// Tools that change during a run (ADR 0085, slice E5): `requests` keeps one entry per distinct
/// request, with the turn it was first sent at, and a call whose request is the one before
/// appends nothing. A replay of a lost commit writes the same list without calling the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_that_change_mid_run_record_one_request_per_change() {
    let rig = Rig::with(|builder| builder.tool_source(Grows::default()));
    let (state, calls) = three_calls(&rig, false).await;
    assert_eq!(calls, 3);
    let sent = rig.mock.requests();
    assert_ne!(
        sent[0].tools, sent[1].tools,
        "the second call has one more tool"
    );
    assert_eq!(sent[1].tools, sent[2].tools);
    let turns: Vec<u32> = state.requests.iter().map(|r| r.turn).collect();
    assert_eq!(
        turns,
        [0, 1],
        "an entry at the first call and at the change"
    );
    assert_eq!(state.request.as_ref().map(|r| r.turn), Some(2));
    assert_eq!(
        state.requests[1].tools,
        state.request.as_ref().expect("recorded").tools,
        "the last entry is the last call's request"
    );
    for (entry, sent) in state.requests.iter().zip([&sent[0], &sent[1]]) {
        let content = entry
            .load(rig.store.as_ref())
            .await
            .expect("read")
            .expect("the blobs are there");
        assert_eq!(content.tools, sent.tools);
        assert_eq!(content.system, sent.system);
    }

    // The same run with its first commit lost: the replay writes the same list (the ids are
    // content addresses, so they compare across stores) and asks the model nothing more.
    let replayed = Rig::with(|builder| builder.tool_source(Grows::default()));
    let (again, calls) = three_calls(&replayed, true).await;
    assert_eq!(replayed.faulty.injected(Method::CommitRun), 1);
    assert_eq!(calls, 3, "the replay called no model");
    assert_eq!(again.requests, state.requests);
    assert_eq!(again.request, state.request);
}

/// State written before the field existed decodes without it, and a state without a request writes
/// no member: the bytes of such a state are what they were.
#[test]
fn a_state_from_before_decodes_and_writes_the_same_bytes() {
    // What a build before the field wrote: the same members, none of them `request`.
    let old = serde_json::to_value(Conversation {
        messages: vec![Message::user_text("hi")],
        turns: 2,
        ..Conversation::default()
    })
    .unwrap();
    assert!(old.get("request").is_none());
    assert!(old.get("requests").is_none());
    let old_bytes = serde_json::to_vec(&old).unwrap();
    let state: Conversation = serde_json::from_value(old).expect("an older state decodes");
    assert_eq!(state.request, None);
    assert!(state.requests.is_empty());
    assert_eq!(
        serde_json::to_vec(&state).unwrap(),
        old_bytes,
        "re-serialized to the same bytes"
    );
    assert_eq!(state.turns, 2);
    let written = serde_json::to_value(&state).unwrap();
    assert!(written.get("request").is_none(), "no member while None");
    assert!(written.get("requests").is_none(), "no member while empty");
    assert_eq!(
        serde_json::to_vec(&Conversation::default()).unwrap(),
        serde_json::to_vec(&serde_json::from_str::<Conversation>("{}").unwrap()).unwrap()
    );

    let one = RecordedRequest {
        turn: 3,
        system: None,
        tools: blob_id(b"[]"),
    };
    let recorded = Conversation {
        request: Some(one.clone()),
        requests: vec![one],
        ..state
    };
    let back: Conversation =
        serde_json::from_value(serde_json::to_value(&recorded).unwrap()).unwrap();
    assert_eq!(back, recorded);
}
