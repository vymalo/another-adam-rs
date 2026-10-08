//! `RunEvent::Usage`: one report per completed model call, under an id that a replay says again and a
//! call made again does not; a subagent's calls reported on the task of the root run under the root's
//! step; and the task's totals, one entry per provider and model, its children's included.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, JournalEntry, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Conversation, LlmAgent, Tool, ToolCtx, ToolError, ToolOutput, user_message};
use adam_model::{DynModel, Message, MockModel, ModelResponse, ToolCall, ToolSpec, Usage};
use adam_runtime::{CollectingSink, RetryPolicy, RunEvent, RunView, Runtime, UsageEvent};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::oneshot;

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
    }
}

/// A response that asks for `calls` and took `usage`.
fn calls_with(calls: Vec<ToolCall>, usage: Usage) -> ModelResponse {
    let mut response = ModelResponse::tool_calls(calls);
    response.usage = usage;
    response
}

/// A text answer that took `usage`.
fn text_with(text: &str, usage: Usage) -> ModelResponse {
    let mut response = ModelResponse::text(text);
    response.usage = usage;
    response
}

/// Starts the agent `target` as a child run and waits for it, as `SubagentTool` does.
struct Spawn {
    target: String,
}

#[async_trait]
impl Tool for Spawn {
    fn spec(&self) -> ToolSpec {
        spec("spawn")
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let run = ctx.start_child(&self.target, "go").await?;
        Err(ToolError::AwaitRun { run })
    }
}

/// Fails its first try transiently, then answers.
struct Blip {
    tries: AtomicUsize,
}

#[async_trait]
impl Tool for Blip {
    fn spec(&self) -> ToolSpec {
        spec("blip")
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.tries.fetch_add(1, SeqCst) == 0 {
            return Err(ToolError::Transient("blip".into()));
        }
        Ok(ToolOutput::text("fine"))
    }
}

async fn wait_finished(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).unwrap()
}

fn reports(sink: &CollectingSink, run: RunId) -> Vec<UsageEvent> {
    sink.events_for(run)
        .into_iter()
        .filter_map(|e| match e {
            RunEvent::Usage(u) => Some(u),
            _ => None,
        })
        .collect()
}

/// Runs `rt`'s worker until `run` is finished.
async fn run_to_end(rt: &Runtime, run: RunId) -> RunView {
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
    let view = wait_finished(rt, run).await;
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    view
}

/// Every completed call is one report: its tokens, the alias, the provider and the window the model
/// client says; the agent's own calls name no step; the totals and the run's own usage sum them.
#[tokio::test]
async fn every_model_call_is_reported_once_with_its_tokens_and_counted_in_the_totals() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let sink = CollectingSink::new();
    let mock = Arc::new(
        MockModel::new()
            .with_provider("openai")
            .with_context_window("big", 131_072),
    );
    mock.push_response(calls_with(
        vec![call("c1", "blip")],
        Usage::new(41_250, 812)
            .with_reasoning_tokens(300)
            .with_cached_input_tokens(38_000),
    ))
    .push_response(text_with("done", Usage::new(42_000, 20)))
    .push_text("never");
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "big")
        .tool(Blip {
            tries: AtomicUsize::new(1),
        })
        .build();
    let rt = Runtime::builder(store)
        .poll_interval(Duration::from_millis(20))
        .event_sink(sink.clone())
        .agent(agent)
        .build();
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    assert_eq!(view.status, RunStatus::Done);

    let reports = reports(&sink, run);
    assert_eq!(reports.len(), 2, "{reports:#?}");
    for (turn, report) in reports.iter().enumerate() {
        let prefix = format!("{run}-c{turn}-");
        assert!(report.call.starts_with(&prefix), "{}", report.call);
        assert_eq!(report.call.len(), prefix.len() + 8);
        assert_eq!(report.model, "big");
        assert_eq!(report.provider.as_deref(), Some("openai"));
        assert_eq!(report.context_window, Some(131_072));
        assert_eq!(report.step, None, "the agent's own call");
    }
    assert_eq!(
        reports[0].usage,
        Usage::new(41_250, 812)
            .with_reasoning_tokens(300)
            .with_cached_input_tokens(38_000)
    );
    assert_eq!(reports[1].usage, Usage::new(42_000, 20));

    let state = conversation(&view);
    let want = Usage::new(83_250, 832)
        .with_reasoning_tokens(300)
        .with_cached_input_tokens(38_000);
    assert_eq!(state.usage, want);
    let totals = state.usage_totals.entries();
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0].provider.as_deref(), Some("openai"));
    assert_eq!(totals[0].model, "big");
    assert_eq!(totals[0].usage, want);
}

/// A client that says no provider and a call answered without usage: the report has no provider
/// and zeros, so a screen still knows a call happened.
#[tokio::test]
async fn a_call_without_usage_is_reported_with_zeros_and_no_provider() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let sink = CollectingSink::new();
    let mock = Arc::new(MockModel::new());
    mock.push_text("hello");
    let model: DynModel = mock;
    let rt = Runtime::builder(store)
        .poll_interval(Duration::from_millis(20))
        .event_sink(sink.clone())
        .agent(LlmAgent::builder("llm", model, "m").build())
        .build();
    let run = rt.start("llm", user_message("hi"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    let reports = reports(&sink, run);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].usage, Usage::default());
    assert_eq!(reports[0].provider, None);
    assert_eq!(reports[0].context_window, None);
    let totals = conversation(&view).usage_totals;
    assert_eq!(totals.entries().len(), 1);
    assert_eq!(totals.entries()[0].usage, Usage::default());
}

/// `top` spawns `mid`, which spawns `leaf`. Every call of `mid` and `leaf` is reported **on `top`**,
/// the task, under the step of `top`'s call that started the chain; none on the children's own runs.
/// `top`'s totals hold every call of the three, one entry per provider and model; its own `usage`
/// only its own.
#[tokio::test]
async fn a_subagents_calls_are_reported_on_the_task_under_the_roots_step_and_counted_in_its_totals()
{
    let store: DynStore = Arc::new(MemoryStore::new());
    let sink = CollectingSink::new();
    let model = |window: Option<(&str, u64)>| {
        let mock = MockModel::new().with_provider("openai");
        Arc::new(match window {
            Some((alias, n)) => mock.with_context_window(alias, n),
            None => mock,
        })
    };
    let (top_model, mid_model, leaf_model) = (
        model(Some(("big", 1000))),
        model(Some(("big", 1000))),
        model(None),
    );
    top_model
        .push_response(calls_with(vec![call("t1", "spawn")], Usage::new(100, 10)))
        .push_response(text_with("top done", Usage::new(200, 20)));
    mid_model
        .push_response(calls_with(vec![call("m1", "spawn")], Usage::new(10, 1)))
        .push_response(text_with("mid done", Usage::new(20, 2)));
    leaf_model.push_response(text_with(
        "leaf done",
        Usage::new(1, 1).with_reasoning_tokens(1),
    ));
    let agent = |name: &str, model: Arc<MockModel>, alias: &str, target: Option<&str>| {
        let model: DynModel = model;
        let mut builder = LlmAgent::builder(name, model, alias);
        if let Some(target) = target {
            builder = builder.tool(Spawn {
                target: target.into(),
            });
        }
        builder.build()
    };
    let rt = Runtime::builder(store)
        .poll_interval(Duration::from_millis(20))
        .event_sink(sink.clone())
        .agent(agent("top", top_model, "big", Some("mid")))
        .agent(agent("mid", mid_model, "big", Some("leaf")))
        .agent(agent("leaf", leaf_model, "small", None))
        .build();
    let top = rt.start("top", user_message("go"), None).await.unwrap();
    let view = run_to_end(&rt, top).await;
    assert_eq!(view.status, RunStatus::Done);
    let mid = adam_runtime::child_run_id(top, "t1");
    let leaf = adam_runtime::child_run_id(mid, "m1");

    assert!(
        reports(&sink, mid).is_empty(),
        "a child's calls are the task's"
    );
    assert!(reports(&sink, leaf).is_empty());
    let reports = reports(&sink, top);
    assert_eq!(reports.len(), 5, "{reports:#?}");
    let of = |run: RunId| -> Vec<&UsageEvent> {
        reports
            .iter()
            .filter(|r| r.call.starts_with(&run.to_string()))
            .collect()
    };
    let (own, mids, leafs) = (of(top), of(mid), of(leaf));
    assert_eq!((own.len(), mids.len(), leafs.len()), (2, 2, 1));
    assert!(own.iter().all(|r| r.step.is_none()));
    // The leaf is the mid's child, and still reported under the top's step: the task knows that one.
    for report in mids.iter().chain(&leafs) {
        assert_eq!(report.step.as_deref(), Some("tool:t1"), "{report:?}");
    }
    assert!(
        own.iter()
            .chain(&mids)
            .all(|r| r.context_window == Some(1000))
    );
    assert_eq!(leafs[0].model, "small");
    assert_eq!(
        leafs[0].context_window, None,
        "no window was said for that alias"
    );
    let mut ids: Vec<&str> = reports.iter().map(|r| r.call.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 5, "unique within the task");

    let state = conversation(&view);
    assert_eq!(state.usage, Usage::new(300, 30), "its own calls");
    let totals = state.usage_totals.entries();
    assert_eq!(totals.len(), 2, "{totals:#?}");
    assert_eq!(totals[0].model, "big");
    assert_eq!(totals[0].usage, Usage::new(330, 33));
    assert_eq!(totals[1].model, "small");
    assert_eq!(totals[1].usage, Usage::new(1, 1).with_reasoning_tokens(1));
    let mid_state = conversation(&rt.view(mid).await.unwrap().unwrap());
    assert_eq!(mid_state.root_step.as_deref(), Some("tool:t1"));
    assert_eq!(
        mid_state.usage_totals.entries().len(),
        2,
        "the mid counts its leaf"
    );
    let leaf_state = conversation(&rt.view(leaf).await.unwrap().unwrap());
    assert_eq!(
        leaf_state.root_step.as_deref(),
        Some("tool:t1"),
        "handed down"
    );
}

/// A model step whose journal records the call: the run replays it, calling no model, and reports
/// the call under the id the record holds; a journal written before reports existed is reported
/// under the run and the turn, which a second replay would say again.
#[tokio::test]
async fn a_replayed_call_is_reported_under_the_id_its_record_holds() {
    for (recorded, want) in [(Some("the-recorded-id"), None), (None, Some("-c0"))] {
        let store: DynStore = Arc::new(MemoryStore::new());
        let sink = CollectingSink::new();
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let rt = Runtime::builder(store.clone())
            .poll_interval(Duration::from_millis(20))
            .event_sink(sink.clone())
            .agent(LlmAgent::builder("llm", model, "m").build())
            .build();
        let run = rt.start("llm", user_message("hi"), None).await.unwrap();
        let mut payload = json!({
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            "finish": "stop",
            "usage": {"input_tokens": 3, "output_tokens": 5, "reasoning_tokens": 2},
        });
        if let Some(id) = recorded {
            payload["call"] = json!(id);
        }
        store
            .journal_put(run, JournalEntry::ok(0, "model:0", payload))
            .await
            .unwrap();
        let view = run_to_end(&rt, run).await;
        assert_eq!(view.status, RunStatus::Done);
        assert!(mock.requests().is_empty(), "replayed, not called");
        let reports = reports(&sink, run);
        assert_eq!(reports.len(), 1);
        match (recorded, want) {
            (Some(id), _) => assert_eq!(reports[0].call, id),
            (None, Some(suffix)) => assert_eq!(reports[0].call, format!("{run}{suffix}")),
            _ => unreachable!(),
        }
        assert_eq!(reports[0].usage, Usage::new(3, 5).with_reasoning_tokens(2));
    }
}

/// A transition that fails transiently after its model call is tried again from a fresh journal
/// position: the model is called again, which is **another call with another id**. The first try's
/// call was reported live; its state was abandoned with the try, so the totals hold the calls of the
/// tries that were kept.
#[tokio::test]
async fn a_call_made_again_after_a_transient_failure_is_another_call() {
    let store: DynStore = Arc::new(MemoryStore::new());
    let sink = CollectingSink::new();
    let mock = Arc::new(MockModel::new());
    mock.push_response(calls_with(vec![call("c1", "blip")], Usage::new(10, 1)))
        .push_response(calls_with(vec![call("c1", "blip")], Usage::new(20, 2)))
        .push_response(text_with("done", Usage::new(30, 3)));
    let model: DynModel = mock.clone();
    let agent = LlmAgent::builder("llm", model, "m")
        .tool(Blip {
            tries: AtomicUsize::new(0),
        })
        .build();
    let rt = Runtime::builder(store)
        .poll_interval(Duration::from_millis(20))
        .event_sink(sink.clone())
        .retry(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            multiplier: 1.0,
        })
        .agent(agent)
        .build();
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
    let view = run_to_end(&rt, run).await;
    assert_eq!(view.status, RunStatus::Done, "{view:#?}");
    assert_eq!(mock.requests().len(), 3);
    let reports = reports(&sink, run);
    assert_eq!(reports.len(), 3, "{reports:#?}");
    assert!(reports[0].call.starts_with(&format!("{run}-c0-")));
    assert!(
        reports[1].call.starts_with(&format!("{run}-c0-")),
        "the same turn, tried again"
    );
    assert_ne!(reports[0].call, reports[1].call, "another call, another id");
    let state = conversation(&view);
    assert_eq!(state.usage_totals.entries()[0].usage, Usage::new(50, 5));
    // The history is the one of the try that was kept.
    assert!(matches!(
        state.messages.last(),
        Some(Message::Assistant { .. })
    ));
}
