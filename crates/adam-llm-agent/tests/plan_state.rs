//! The plan is part of the conversation's state (ADR 0044 decision 5): a tool writes it through
//! `ToolOutput::with_plan`, the loop applies it from a result that is not an error, a replay takes
//! the journaled output, and a task that continues another keeps it. Nothing of it is ever in the
//! system prompt or in an earlier message (decision 1). A real `Runtime` with a worker, a scripted
//! `MockModel` and `MemoryStore`, as in `announced_answer.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, MAX_PLAN_ITEMS, Plan, PlanItem, PlanStatus, Tool, ToolCtx, ToolError,
    ToolOutput, user_message,
};
use adam_model::{DynModel, MockModel, ModelRequest, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, RunView, Runtime, RuntimeBuilder};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

fn plan_of(texts: &[&str]) -> Plan {
    Plan {
        title: None,
        items: texts
            .iter()
            .enumerate()
            .map(|(n, text)| PlanItem {
                id: u32::try_from(n + 1).unwrap(),
                text: (*text).to_owned(),
                status: PlanStatus::Pending,
                note: None,
            })
            .collect(),
    }
}

/// `plan_set { items: [text], fail?: bool }`: appends the items to the plan it reads from the
/// context and returns the new plan; with `fail` the result is an error that still carries a plan.
/// It records what plan it read at each call.
struct PlanSet {
    calls: Arc<AtomicUsize>,
    saw: Arc<Mutex<Vec<Option<Plan>>>>,
}

#[async_trait]
impl Tool for PlanSet {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "plan_set".into(),
            description: "Append items to the plan.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        let before = ctx.plan().cloned();
        self.saw.lock().unwrap().push(before.clone());
        let mut plan = before.unwrap_or_default();
        for text in args["items"].as_array().unwrap() {
            let id = u32::try_from(plan.items.len() + 1).unwrap();
            plan.items.push(PlanItem {
                id,
                text: text.as_str().unwrap().to_owned(),
                status: PlanStatus::Pending,
                note: None,
            });
        }
        let text = format!("plan has {} items", plan.items.len());
        if args["fail"] == json!(true) {
            let mut refused = ToolOutput::error("refused");
            refused.plan = Some(Box::new(plan));
            return Ok(refused);
        }
        let mut out = ToolOutput::text(text).with_plan(plan.clone());
        if args["draw"] == json!(true) {
            // What a plan tool does with a screen: one artifact, under one id, whatever changed.
            out = out.with_artifact(
                adam_runtime::Artifact::new("plan", None, json!({"items": plan.items.len()}))
                    .with_id("plan-test"),
            );
        }
        Ok(out)
    }
}

/// `slow`: hangs the first time it is called, so that a worker can be killed inside it.
struct Hangs {
    first: Arc<AtomicBool>,
    reached: Arc<Notify>,
}

#[async_trait]
impl Tool for Hangs {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "slow".into(),
            description: "Slow.".into(),
            parameters: json!({"type": "object"}),
        }
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.first.swap(false, SeqCst) {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("slow-out"))
    }
}

struct Rig {
    store: DynStore,
    sink: CollectingSink,
    mock: Arc<MockModel>,
    calls: Arc<AtomicUsize>,
    saw: Arc<Mutex<Vec<Option<Plan>>>>,
}

impl Rig {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            sink: CollectingSink::new(),
            mock: Arc::new(MockModel::new()),
            calls: Arc::new(AtomicUsize::new(0)),
            saw: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn agent(&self) -> adam_llm_agent::LlmAgentBuilder {
        let model: DynModel = self.mock.clone();
        LlmAgent::builder("llm", model, "test-model")
            .instructions("You are a test agent.")
            .tool(PlanSet {
                calls: self.calls.clone(),
                saw: self.saw.clone(),
            })
    }

    fn runtime(&self, agent: &LlmAgent, worker: &str, short_lease: bool) -> Runtime {
        let mut builder: RuntimeBuilder = Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .event_sink(self.sink.clone())
            .worker_id(worker)
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10));
        if short_lease {
            builder = builder.lease_ttl(Duration::from_millis(300));
        }
        builder.build()
    }
}

fn spawn_worker(
    rt: &Runtime,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
) {
    let (stop, rx) = oneshot::channel::<()>();
    let rt = rt.clone();
    let handle = tokio::spawn(async move {
        rt.run_worker(async {
            let _ = rx.await;
        })
        .await
    });
    (stop, handle)
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
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn finish(rig: &Rig, first: &str) -> RunView {
    let agent = rig.agent().build();
    let rt = rig.runtime(&agent, "w", false);
    let run = rt
        .start("llm", user_message(first), None)
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    let view = wait_for(&rt, run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    handle.await.unwrap().unwrap();
    view
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

#[tokio::test]
async fn a_plan_written_by_a_tool_is_in_the_state() {
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call("c1", "plan_set", json!({"items": ["a", "b"]}))])
        .push_text("Done.");
    let view = finish(&rig, "go").await;
    assert_eq!(conversation(&view).plan, Some(plan_of(&["a", "b"])));
    assert_eq!(
        view.state["plan"]["items"][0],
        json!({"id": 1, "text": "a", "status": "pending"}),
        "the shape of the state: ids, texts and statuses, no empty members"
    );
    // The model was told the tool's own words, and nothing else of the plan.
    let requests = rig.mock.requests();
    assert_eq!(
        requests[1].messages.last(),
        Some(&adam_model::Message::tool_result("c1", "plan has 2 items"))
    );
}

#[tokio::test]
async fn an_error_result_changes_no_plan() {
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call("c1", "plan_set", json!({"items": ["a"]}))])
        .push_tool_calls(vec![call(
            "c2",
            "plan_set",
            json!({"items": ["x"], "fail": true}),
        )])
        .push_text("Done.");
    let view = finish(&rig, "go").await;
    assert_eq!(
        conversation(&view).plan,
        Some(plan_of(&["a"])),
        "the plan an error result carried is not applied"
    );

    // And with no plan before: still none, and the member is not in the state.
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call(
            "c1",
            "plan_set",
            json!({"items": ["x"], "fail": true}),
        )])
        .push_text("Done.");
    let view = finish(&rig, "go").await;
    assert_eq!(conversation(&view).plan, None);
    assert!(view.state.get("plan").is_none());
}

#[tokio::test]
async fn two_plan_calls_in_one_turn_see_each_other() {
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![
            call("c1", "plan_set", json!({"items": ["a"]})),
            call("c2", "plan_set", json!({"items": ["b"]})),
        ])
        .push_text("Done.");
    let view = finish(&rig, "go").await;
    assert_eq!(conversation(&view).plan, Some(plan_of(&["a", "b"])));
    let saw = rig.saw.lock().unwrap().clone();
    assert_eq!(saw, vec![None, Some(plan_of(&["a"]))]);
}

#[tokio::test]
async fn a_plan_survives_a_replay_from_the_journal() {
    let rig = Rig::new();
    let first = Arc::new(AtomicBool::new(true));
    let reached = Arc::new(Notify::new());
    let agent = rig
        .agent()
        .tool(Hangs {
            first: first.clone(),
            reached: reached.clone(),
        })
        .build();
    // The plan is recorded, then the worker dies inside the second tool of the message.
    rig.mock
        .push_tool_calls(vec![
            call("c1", "plan_set", json!({"items": ["a", "b"], "draw": true})),
            call("c2", "slow", json!({})),
        ])
        .push_text("Closing line.");
    let rt_a = rig.runtime(&agent, "crash-a", true);
    let rt_b = rig.runtime(&agent, "crash-b", true);
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let (_stop, doomed) = spawn_worker(&rt_a);
    tokio::time::timeout(Duration::from_secs(20), reached.notified())
        .await
        .expect("the slow tool started");
    doomed.abort();
    assert!(doomed.await.expect_err("aborted").is_cancelled());
    assert_eq!(rig.calls.load(SeqCst), 1);

    let (stop, survivor) = spawn_worker(&rt_b);
    let view = wait_for(&rt_b, run, "done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    survivor.await.unwrap().unwrap();

    assert_eq!(
        rig.calls.load(SeqCst),
        1,
        "the recorded result, plan included, is replayed and the tool does not run again"
    );
    assert_eq!(conversation(&view).plan, Some(plan_of(&["a", "b"])));
    assert_eq!(
        view.artifacts.len(),
        1,
        "the replay holds the plan's artifact once: {:?}",
        view.artifacts
    );
    assert_eq!(view.artifacts[0].id.as_deref(), Some("plan-test"));
}

#[tokio::test]
async fn a_continued_run_starts_with_the_plan_before() {
    // The state alone.
    let mut prior = Conversation::new("first");
    prior.plan = Some(plan_of(&["a", "b"]));
    let next = prior.continued("second", RunId::new());
    assert_eq!(next.plan, prior.plan);

    // And through a runtime: the tool of the second run reads it from its context.
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call("c1", "plan_set", json!({"items": ["a", "b"]}))])
        .push_text("first done")
        .push_tool_calls(vec![call("c2", "plan_set", json!({"items": ["c"]}))])
        .push_text("second done");
    let agent = rig.agent().build();
    let rt = rig.runtime(&agent, "w", false);
    let first = rt
        .start("llm", user_message("one"), Some("ctx"))
        .await
        .expect("start");
    let (stop, handle) = spawn_worker(&rt);
    wait_for(&rt, first, "first done", |v| v.status == RunStatus::Done).await;
    let second = rt
        .start_continuing("llm", user_message("two"), Some("ctx"), first)
        .await
        .expect("continue");
    let view = wait_for(&rt, second, "second done", |v| v.status == RunStatus::Done).await;
    let _ = stop.send(());
    handle.await.unwrap().unwrap();
    assert_eq!(rig.saw.lock().unwrap()[1], Some(plan_of(&["a", "b"])));
    assert_eq!(conversation(&view).plan, Some(plan_of(&["a", "b", "c"])));
}

/// A plan that changes twenty times is one artifact in the run and one reference in its output,
/// holding the newest drawing, in the place of the first.
#[tokio::test]
async fn twenty_changes_of_the_plan_leave_one_artifact_and_one_reference() {
    let rig = Rig::new();
    for turn in 0..4 {
        rig.mock.push_tool_calls(
            (0..5)
                .map(|n| {
                    call(
                        &format!("c{turn}-{n}"),
                        "plan_set",
                        json!({"items": [format!("item {turn}-{n}")], "draw": true}),
                    )
                })
                .collect(),
        );
    }
    rig.mock.push_text("Done.");
    let view = finish(&rig, "go").await;
    assert_eq!(conversation(&view).plan.unwrap().items.len(), 20);
    assert_eq!(view.artifacts.len(), 1, "{:?}", view.artifacts);
    assert_eq!(view.artifacts[0].id.as_deref(), Some("plan-test"));
    assert_eq!(view.artifacts[0].data, json!({"items": 20}), "the newest");
    let output = view.output.expect("an output");
    assert_eq!(output["artifacts"].as_array().unwrap().len(), 1, "{output}");
    assert_eq!(output["artifacts"][0]["id"], "plan-test");
}

#[test]
fn a_state_with_a_member_it_does_not_know_still_loads() {
    let state: Conversation = serde_json::from_value(json!({
        "messages": [], "turns": 2, "tool_calls": 1,
        "plan": {"title": "t", "items": [], "later": 1},
        "a_member_of_a_later_version": {"x": 1}
    }))
    .unwrap();
    assert_eq!(state.plan.unwrap().title.as_deref(), Some("t"));
    // An artifact reference from before ids existed, and one from after.
    let old: adam_llm_agent::ArtifactRef =
        serde_json::from_value(json!({"name": "n", "mime_type": null})).unwrap();
    assert_eq!(old.id, None);
    assert!(serde_json::to_value(&old).unwrap().get("id").is_none());
}

#[test]
fn an_older_state_and_journal_without_a_plan_load() {
    let state: Conversation =
        serde_json::from_value(json!({"messages": [], "turns": 2, "tool_calls": 1})).unwrap();
    assert_eq!(state.plan, None);
    assert!(
        serde_json::to_value(&state).unwrap().get("plan").is_none(),
        "no plan, no member"
    );
    // A tool output as the journal recorded it before.
    let out: ToolOutput = serde_json::from_str(r#"{"content":"ok"}"#).unwrap();
    assert_eq!(out, ToolOutput::text("ok"));
    assert!(out.plan.is_none());
    assert!(
        serde_json::to_value(&out).unwrap().get("plan").is_none(),
        "no plan, no member"
    );
    // And one with a plan round-trips.
    let with = ToolOutput::text("ok").with_plan(plan_of(&["a"]));
    let back: ToolOutput = serde_json::from_value(serde_json::to_value(&with).unwrap()).unwrap();
    assert_eq!(back, with);
    assert_eq!(MAX_PLAN_ITEMS, 50);
}

/// What decision 1 asks of the loop, written here for plan calls: across requests the system prompt
/// and the tool specs are byte-identical, and each request's messages start with the previous
/// request's (the plan is only ever appended, as a tool result).
fn assert_prefix_holds(requests: &[ModelRequest]) {
    for pair in requests.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        assert_eq!(
            serde_json::to_string(&a.system).unwrap(),
            serde_json::to_string(&b.system).unwrap(),
            "the system prompt is the same on every request"
        );
        assert_eq!(
            serde_json::to_string(&a.tools).unwrap(),
            serde_json::to_string(&b.tools).unwrap(),
            "the tool specs are the same, in the same order"
        );
        assert!(b.messages.len() > a.messages.len());
        assert_eq!(
            serde_json::to_string(&a.messages).unwrap(),
            serde_json::to_string(&b.messages[..a.messages.len()]).unwrap(),
            "a request is the previous one plus what happened since"
        );
    }
}

#[tokio::test]
async fn the_request_prefix_holds_across_plan_calls() {
    let rig = Rig::new();
    rig.mock
        .push_tool_calls(vec![call("c1", "plan_set", json!({"items": ["a", "b"]}))])
        .push_tool_calls(vec![
            call("c2", "plan_set", json!({"items": ["c"]})),
            call("c3", "plan_set", json!({"items": ["d"]})),
        ])
        .push_tool_calls(vec![call(
            "c4",
            "plan_set",
            json!({"items": ["e"], "fail": true}),
        )])
        .push_text("Done.");
    let view = finish(&rig, "go").await;
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 4);
    assert_prefix_holds(&requests);
    for request in &requests {
        let system = request.system.clone().unwrap_or_default();
        assert!(
            !system.contains("plan has"),
            "nothing of the plan is in the system prompt: {system}"
        );
    }
    assert_eq!(
        conversation(&view).plan,
        Some(plan_of(&["a", "b", "c", "d"]))
    );
}
