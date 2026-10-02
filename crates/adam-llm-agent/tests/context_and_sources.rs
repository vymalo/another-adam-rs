//! What a message says about its sender (the run's inbound context), a question that carries an
//! interface, and tools that come from a source read at every model turn: a scripted `MockModel`,
//! `MemoryStore` and a real `Runtime` with a worker, as in `llm_agent.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, LlmAgent, LlmAgentBuilder, MAX_CONTEXT_BYTES, MAX_SOURCE_TOOLS, PendingQuestion,
    PendingWait, SourceCtx, Tool, ToolCtx, ToolError, ToolOutput, ToolSource, user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{Inbound, ManualClock, RetryPolicy, RunView, Runtime};
use adam_store_testkit::fault::{FaultyStore, Method};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    store: DynStore,
    mock: Arc<MockModel>,
}

impl Harness {
    fn new() -> Self {
        Self {
            store: Arc::new(MemoryStore::new()),
            mock: Arc::new(MockModel::new()),
        }
    }

    fn agent(&self) -> LlmAgentBuilder {
        let model: DynModel = self.mock.clone();
        LlmAgent::builder("llm", model, "test-model")
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        Runtime::builder(self.store.clone())
            .agent(agent.clone())
            .worker_id("w")
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10))
            .build()
    }
}

struct Worker {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), adam_runtime::RuntimeError>>,
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

async fn wait_waiting(rt: &Runtime, run: RunId) -> RunView {
    wait_for(rt, run, "parked and waiting", |v| v.waiting).await
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
    }
}

/// A message as an A2A front makes it: the text, and what the sender says about itself.
fn message_with(text: &str, context: Value) -> Inbound {
    Inbound::new("message", json!({"text": text, "context": context}))
}

/// Answers with what it reads of the context under `key`, as JSON.
struct ReadsContext {
    name: &'static str,
    key: &'static str,
    seen: Arc<Mutex<Vec<Option<Value>>>>,
}

impl ReadsContext {
    fn new(name: &'static str, key: &'static str) -> (Self, Arc<Mutex<Vec<Option<Value>>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                name,
                key,
                seen: seen.clone(),
            },
            seen,
        )
    }
}

#[async_trait]
impl Tool for ReadsContext {
    fn spec(&self) -> ToolSpec {
        spec(self.name)
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let value = ctx.context(self.key).cloned();
        self.seen.lock().unwrap().push(value.clone());
        Ok(ToolOutput::text(
            value.map_or_else(|| "(none)".to_owned(), |v| v.to_string()),
        ))
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_context_of_the_start_message_reaches_a_tool() {
    let h = Harness::new();
    let (tool, seen) = ReadsContext::new("who", "vymalo.screen");
    let agent = h.agent().tool(tool).build();
    h.mock
        .push_tool_calls(vec![call("c1", "who", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start(
            "llm",
            message_with("hi", json!({"vymalo.screen": {"width": 3}, "other": true})),
            None,
        )
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(*seen.lock().unwrap(), [Some(json!({"width": 3}))]);
    let state = conversation(&view);
    assert_eq!(
        Value::Object(state.context),
        json!({"vymalo.screen": {"width": 3}, "other": true}),
        "the context is part of the durable state"
    );
    // What the tool read is the result the model got.
    assert_eq!(
        h.mock.requests()[1].messages[2],
        Message::tool_result("c1", r#"{"width":3}"#)
    );
}

#[tokio::test]
async fn a_later_message_replaces_a_key_and_null_deletes_one() {
    let h = Harness::new();
    let (tool, seen) = ReadsContext::new("who", "a");
    let ask = AskOnce;
    let agent = h.agent().tool(tool).tool(ask).build();
    h.mock
        .push_tool_calls(vec![call("c1", "ask", json!({}))])
        .push_tool_calls(vec![call("c2", "who", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start(
            "llm",
            message_with("hi", json!({"a": 1, "b": 2, "c": 3})),
            None,
        )
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_waiting(&rt, run).await;
    // The answer to the question brings a new `a`, takes `b` away and adds `d`.
    rt.deliver(
        run,
        message_with("the answer", json!({"a": 10, "b": null, "d": 4})),
    )
    .await
    .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(*seen.lock().unwrap(), [Some(json!(10))]);
    assert_eq!(
        Value::Object(conversation(&view).context),
        json!({"a": 10, "c": 3, "d": 4})
    );
}

/// Asks once, with no interface.
struct AskOnce;

#[async_trait]
impl Tool for AskOnce {
    fn spec(&self) -> ToolSpec {
        spec("ask")
    }
    fn asks_user(&self) -> bool {
        true
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input("which one?"))
    }
}

#[tokio::test]
async fn a_message_without_context_keeps_what_the_run_has() {
    let h = Harness::new();
    let (tool, seen) = ReadsContext::new("who", "a");
    let agent = h.agent().tool(tool).tool(AskOnce).build();
    h.mock
        .push_tool_calls(vec![call("c1", "ask", json!({}))])
        .push_tool_calls(vec![call("c2", "who", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", message_with("hi", json!({"a": 1})), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_waiting(&rt, run).await;
    rt.deliver(run, user_message("plain answer"))
        .await
        .expect("deliver");
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(*seen.lock().unwrap(), [Some(json!(1))]);
}

#[tokio::test]
async fn a_run_that_continues_another_starts_with_its_context_and_the_new_message_wins() {
    let h = Harness::new();
    let (tool, seen) = ReadsContext::new("who", "a");
    let agent = h.agent().tool(tool).build();
    h.mock
        .push_text("answer one")
        .push_tool_calls(vec![call("c1", "who", json!({}))])
        .push_text("answer two");
    let rt = h.runtime(&agent);
    let worker = spawn_worker(&rt);

    let first = rt
        .start(
            "llm",
            message_with("first", json!({"a": "old", "keep": true})),
            Some("conv"),
        )
        .await
        .expect("start");
    wait_done(&rt, first).await;

    // A second task of the same conversation: with no context of its own it has the first's;
    // with one, the new keys win.
    let second = RunId::new();
    assert!(
        rt.start_with_id_continuing(
            second,
            "llm",
            message_with("second", json!({"a": "new"})),
            Some("conv"),
            first
        )
        .await
        .expect("continue")
    );
    let view = wait_done(&rt, second).await;
    worker.stop().await;

    assert_eq!(*seen.lock().unwrap(), [Some(json!("new"))]);
    assert_eq!(
        Value::Object(conversation(&view).context),
        json!({"a": "new", "keep": true})
    );
}

#[test]
fn a_continued_run_without_context_of_its_own_has_the_one_before() {
    let mut prior = Conversation::new("first");
    prior.merge_context(json!({"a": 1}).as_object().unwrap());
    let next = prior.continued("second", RunId::new());
    assert_eq!(Value::Object(next.context), json!({"a": 1}));
}

#[tokio::test]
async fn an_expired_entry_leaves_the_state_and_a_live_one_stays() {
    let h = Harness::new();
    let (tool, seen) = ReadsContext::new("who", "grant");
    let agent = h.agent().tool(tool).build();
    h.mock
        .push_tool_calls(vec![call("c1", "who", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start(
            "llm",
            message_with(
                "hi",
                json!({
                    "grant": {"token": "t", "expiresAt": "2020-01-01T00:00:00Z"},
                    "live": {"token": "u", "expiresAt": "2999-01-01T00:00:00Z"},
                    "plain": {"token": "v"},
                    "odd": {"expiresAt": "not a time"},
                }),
            ),
            None,
        )
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(
        *seen.lock().unwrap(),
        [None],
        "the tool never saw the expired grant"
    );
    let mut kept: Vec<String> = conversation(&view).context.keys().cloned().collect();
    kept.sort();
    assert_eq!(kept, ["live", "odd", "plain"]);
}

#[tokio::test]
async fn a_context_over_the_limit_is_dropped_and_the_text_is_still_read() {
    let h = Harness::new();
    let agent = h.agent().build();
    h.mock.push_text("done");
    let rt = h.runtime(&agent);
    let big = "x".repeat(MAX_CONTEXT_BYTES);
    let run = rt
        .start("llm", message_with("hi", json!({"big": big})), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    let state = conversation(&view);
    assert!(state.context.is_empty());
    assert_eq!(state.messages[0], Message::user_text("hi"));
}

#[test]
fn merging_context_follows_the_rules_and_keeps_the_limit() {
    let mut c = Conversation::new("t");
    let obj = |v: Value| v.as_object().cloned().unwrap();
    assert!(c.merge_context(&obj(json!({"a": 1, "b": {"x": 1}}))));
    assert!(c.merge_context(&obj(json!({"a": null, "b": {"y": 2}}))));
    assert_eq!(Value::Object(c.context.clone()), json!({"b": {"y": 2}}));
    // Over the limit: nothing changes.
    let big = obj(json!({"big": "x".repeat(MAX_CONTEXT_BYTES)}));
    assert!(!c.merge_context(&big));
    assert_eq!(Value::Object(c.context.clone()), json!({"b": {"y": 2}}));
    // Just within it.
    let room = MAX_CONTEXT_BYTES
        - serde_json::to_vec(&json!({"b": {"y": 2}, "k": ""}))
            .unwrap()
            .len();
    assert!(c.merge_context(&obj(json!({"k": "x".repeat(room)}))));
    assert!(serde_json::to_vec(&c.context).unwrap().len() <= MAX_CONTEXT_BYTES);
    assert!(!c.merge_context(&obj(json!({"k": "x".repeat(room + 1)}))));
}

#[test]
fn state_written_before_context_existed_loads_and_does_not_write_the_field() {
    let c: Conversation = serde_json::from_value(json!({"messages": []})).unwrap();
    assert!(c.context.is_empty());
    assert!(serde_json::to_value(&c).unwrap().get("context").is_none());
    let mut c = c;
    c.merge_context(json!({"k": 1}).as_object().unwrap());
    let stored = serde_json::to_value(&c).unwrap();
    assert_eq!(stored["context"], json!({"k": 1}));
    assert_eq!(serde_json::from_value::<Conversation>(stored).unwrap(), c);
}

#[test]
fn a_detached_tool_context_reads_the_context_it_is_given() {
    use adam_runtime::NoopSink;
    let ctx = ToolCtx::detached("t", "c1", Arc::new(NoopSink));
    assert_eq!(ctx.context("k"), None);
    let ctx = ctx.with_context(json!({"k": [1]}).as_object().cloned().unwrap());
    assert_eq!(ctx.context("k"), Some(&json!([1])));
    assert_eq!(ctx.context_map().len(), 1);
}

// ---------------------------------------------------------------------------
// A question that carries an interface
// ---------------------------------------------------------------------------

/// Asks with an interface.
struct AskWithUi;

#[async_trait]
impl Tool for AskWithUi {
    fn spec(&self) -> ToolSpec {
        spec("ask")
    }
    fn asks_user(&self) -> bool {
        true
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Err(ToolError::needs_input_with_ui(
            "which one?",
            json!([{"version": "v0.9.1", "createSurface": {"surfaceId": "s", "catalogId": "c"}}]),
        ))
    }
}

#[tokio::test]
async fn a_question_keeps_its_interface_while_the_run_waits() {
    let h = Harness::new();
    let agent = h.agent().tool(AskWithUi).build();
    h.mock
        .push_tool_calls(vec![call("c1", "ask", json!({}))])
        .push_text("ok");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let parked = wait_waiting(&rt, run).await;
    let ui = json!([{"version": "v0.9.1", "createSurface": {"surfaceId": "s", "catalogId": "c"}}]);
    assert_eq!(
        conversation(&parked).pending_wait,
        Some(PendingWait::Question(PendingQuestion {
            call_id: "c1".into(),
            tool: "ask".into(),
            question: "which one?".into(),
            ui: Some(ui.clone()),
            stream: None,
        }))
    );
    // Where an A2A front reads it from, without knowing the type.
    assert_eq!(parked.state.pointer("/pending_wait/ui"), Some(&ui));
    rt.deliver(run, user_message("the first"))
        .await
        .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert!(conversation(&view).pending_wait.is_none());
}

#[test]
fn journals_and_state_written_before_the_interface_existed_still_decode() {
    let old: ToolError = serde_json::from_str(r#"{"NeedsInput":{"question":"q"}}"#).unwrap();
    assert_eq!(old, ToolError::needs_input("q"));
    assert_eq!(
        serde_json::to_string(&old).unwrap(),
        r#"{"NeedsInput":{"question":"q"}}"#,
        "a question with no interface keeps its frozen shape"
    );
    let with = ToolError::needs_input_with_ui("q", json!([{"a": 1}]));
    let json = serde_json::to_string(&with).unwrap();
    assert_eq!(json, r#"{"NeedsInput":{"question":"q","ui":[{"a":1}]}}"#);
    assert_eq!(serde_json::from_str::<ToolError>(&json).unwrap(), with);

    let state: Conversation = serde_json::from_value(json!({
        "pending_wait": {"call_id": "c1", "tool": "ask", "question": "which?"}
    }))
    .unwrap();
    assert_eq!(
        state.pending_wait,
        Some(PendingWait::Question(PendingQuestion {
            call_id: "c1".into(),
            tool: "ask".into(),
            question: "which?".into(),
            ui: None,
            stream: None,
        }))
    );
    let stored = serde_json::to_value(&state).unwrap();
    assert!(stored["pending_wait"].get("ui").is_none());
    let with: Conversation = serde_json::from_value(json!({
        "pending_wait": {"call_id": "c1", "tool": "ask", "question": "which?", "ui": [1]}
    }))
    .unwrap();
    assert!(matches!(
        with.pending_wait,
        Some(PendingWait::Question(PendingQuestion { ui: Some(_), .. }))
    ));
}

// ---------------------------------------------------------------------------
// Tool sources
// ---------------------------------------------------------------------------

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// Offers the tools named in the context under `offer` (an array of names), and answers each by
/// saying its own name and what it was given.
struct ContextSource {
    listed: Arc<AtomicUsize>,
    called: Calls,
}

impl ContextSource {
    fn new() -> (Self, Arc<AtomicUsize>, Calls) {
        let listed = Arc::new(AtomicUsize::new(0));
        let called = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                listed: listed.clone(),
                called: called.clone(),
            },
            listed,
            called,
        )
    }

    fn names(map: &Map<String, Value>) -> Vec<String> {
        map.get("offer")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    }
}

#[async_trait]
impl ToolSource for ContextSource {
    async fn specs(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        self.listed.fetch_add(1, SeqCst);
        Self::names(ctx.context_map())
            .iter()
            .map(|name| spec(name))
            .collect()
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        if !Self::names(ctx.context_map()).iter().any(|n| n == name) {
            return None;
        }
        self.called.lock().unwrap().push((name.to_owned(), args));
        Some(Ok(ToolOutput::text(format!("{name}-out"))))
    }
}

fn names_of(request: &adam_model::ModelRequest) -> Vec<String> {
    request.tools.iter().map(|t| t.name.clone()).collect()
}

#[tokio::test]
async fn a_source_is_read_at_every_model_turn_and_its_tools_are_called() {
    let h = Harness::new();
    let (source, listed, called) = ContextSource::new();
    let (own, _) = ReadsContext::new("own", "nothing");
    let agent = h
        .agent()
        .tool(own)
        .tool_source(source)
        .tool(AskOnce)
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "ask", json!({}))])
        // After the answer, the context offers one more tool: the next turn lists it.
        .push_tool_calls(vec![call("c2", "relay__search", json!({"q": "rust"}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start(
            "llm",
            message_with("hi", json!({"offer": ["relay__search"]})),
            None,
        )
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_waiting(&rt, run).await;
    rt.deliver(
        run,
        message_with(
            "answer",
            json!({"offer": ["relay__search", "relay__fetch"]}),
        ),
    )
    .await
    .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        names_of(&requests[0]),
        ["own", "ask", "relay__search"],
        "the agent's own tools first, then the source's"
    );
    assert_eq!(
        names_of(&requests[1]),
        ["own", "ask", "relay__search", "relay__fetch"],
        "read again at the next turn: a tool offered since is there"
    );
    assert_eq!(listed.load(SeqCst), 3, "once per model call");
    assert_eq!(
        *called.lock().unwrap(),
        [("relay__search".to_owned(), json!({"q": "rust"}))]
    );
    // The source's answer is the tool result, as for any tool.
    assert_eq!(
        requests[2].messages.last(),
        Some(&Message::tool_result("c2", "relay__search-out"))
    );
    assert_eq!(view.output.expect("output")["text"], "done");
}

#[tokio::test]
async fn an_own_tool_wins_a_name_clash_and_a_name_nobody_has_is_an_error_result() {
    let h = Harness::new();
    let (source, _, called) = ContextSource::new();
    let (own, _) = ReadsContext::new("clash", "nothing");
    let agent = h.agent().tool(own).tool_source(source).build();
    h.mock
        .push_tool_calls(vec![call("c1", "clash", json!({}))])
        .push_tool_calls(vec![call("c2", "ghost", json!({}))])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", message_with("hi", json!({"offer": ["clash"]})), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    let requests = h.mock.requests();
    assert_eq!(
        names_of(&requests[0]),
        ["clash"],
        "the source's `clash` is left out"
    );
    assert_eq!(
        requests[1].messages[2],
        Message::tool_result("c1", "(none)"),
        "the call went to the agent's own tool"
    );
    assert!(called.lock().unwrap().is_empty());
    // The unknown name is an error result the model reads; the run goes on.
    let state = conversation(&view);
    let ghost = state
        .messages
        .iter()
        .find(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "c2"))
        .expect("the ghost call has a result");
    assert!(
        matches!(ghost, Message::Tool { is_error: true, content, .. }
            if content.contains("unknown tool `ghost`") && content.contains("clash")),
        "{ghost:?}"
    );
}

#[tokio::test]
async fn a_source_that_offers_too_many_is_cut_at_the_limit_in_order() {
    let h = Harness::new();
    let (source, _, _) = ContextSource::new();
    let agent = h.agent().tool_source(source).build();
    h.mock.push_text("done");
    let offer: Vec<String> = (0..MAX_SOURCE_TOOLS + 5).map(|i| format!("t{i}")).collect();
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", message_with("hi", json!({ "offer": offer })), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    let names = names_of(&h.mock.requests()[0]);
    assert_eq!(names.len(), MAX_SOURCE_TOOLS);
    assert_eq!(names.first().map(String::as_str), Some("t0"));
}

#[tokio::test]
async fn an_agent_with_a_source_that_offers_nothing_asks_the_model_exactly_as_before() {
    let h = Harness::new();
    let (source, listed, _) = ContextSource::new();
    let (own, _) = ReadsContext::new("own", "k");
    let agent = h.agent().tool(own).tool_source(source).build();
    h.mock.push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(names_of(&h.mock.requests()[0]), ["own"]);
    assert_eq!(listed.load(SeqCst), 1);
}

/// A source that offers nothing of its own and rewrites how the tools of this turn are described:
/// the description of `own` says what the run's context holds, and nothing else changes.
struct Describes;

#[async_trait]
impl ToolSource for Describes {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        vec![spec("extra")]
    }
    async fn refine(&self, ctx: &SourceCtx, specs: &mut [ToolSpec]) {
        let word = ctx
            .context("word")
            .and_then(Value::as_str)
            .unwrap_or("none");
        for spec in specs.iter_mut() {
            spec.description = format!("{} (the word is {word})", spec.description);
        }
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

/// A source may rewrite the descriptions of the tools the model is shown, the agent's own and the
/// sources', from what it knows of this run; the names and the schemas are the tools' own. The model
/// is asked with the result, and a replay, which asks no model, refines nothing.
#[tokio::test]
async fn a_source_refines_the_descriptions_of_the_tools_the_model_is_shown() {
    let h = Harness::new();
    let agent = h
        .agent()
        .tool(ReadsContext::new("own", "word").0)
        .tool_source(Describes)
        .build();
    h.mock.push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", message_with("hi", json!({"word": "green"})), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_done(&rt, run).await;
    worker.stop().await;
    let request = &h.mock.requests()[0];
    assert_eq!(names_of(request), ["own", "extra"]);
    let described: Vec<&str> = request
        .tools
        .iter()
        .map(|t| t.description.as_str())
        .collect();
    assert!(
        described.iter().all(|d| d.ends_with("(the word is green)")),
        "{described:?}"
    );
    assert_eq!(request.tools[1].parameters, spec("extra").parameters);
}

/// A source with no state: it offers `ping` and answers `pong`, to check the call is journaled.
struct Ping(Arc<AtomicUsize>);

#[async_trait]
impl ToolSource for Ping {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        vec![spec("ping")]
    }
    async fn call(
        &self,
        _ctx: &ToolCtx,
        name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        (name == "ping").then(|| {
            self.0.fetch_add(1, SeqCst);
            Ok(ToolOutput::text("pong"))
        })
    }
}

#[tokio::test]
async fn a_source_tool_can_ask_the_person_and_the_answer_is_its_result() {
    struct Asker;
    #[async_trait]
    impl ToolSource for Asker {
        async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
            vec![spec("ask_remote")]
        }
        async fn call(
            &self,
            _ctx: &ToolCtx,
            name: &str,
            _args: Value,
        ) -> Option<Result<ToolOutput, ToolError>> {
            (name == "ask_remote").then(|| Err(ToolError::needs_input("which one?")))
        }
    }
    let h = Harness::new();
    let pings = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool_source(Asker)
        .tool_source(Ping(pings.clone()))
        .build();
    h.mock
        .push_tool_calls(vec![
            call("c1", "ask_remote", json!({})),
            call("c2", "ping", json!({})),
        ])
        .push_text("done");
    let rt = h.runtime(&agent);
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let parked = wait_waiting(&rt, run).await;
    assert!(matches!(
        conversation(&parked).pending_wait,
        Some(PendingWait::Question(PendingQuestion { tool, .. })) if tool == "ask_remote"
    ));
    assert_eq!(pings.load(SeqCst), 0, "the calls after the parked one wait");
    rt.deliver(run, user_message("the first"))
        .await
        .expect("deliver");
    let view = wait_done(&rt, run).await;
    worker.stop().await;
    assert_eq!(pings.load(SeqCst), 1);
    let state = conversation(&view);
    assert_eq!(state.messages[2], Message::tool_result("c1", "the first"));
    assert_eq!(state.messages[3], Message::tool_result("c2", "pong"));
}

// ---------------------------------------------------------------------------
// Replay and the limits of a source's tools
// ---------------------------------------------------------------------------

/// A runtime that retries after 10 ms, keeps its time in `clock` and holds a lease for a second
/// (a run whose step failed is claimed again once its lease has run out).
fn quick_runtime(store: &DynStore, agent: &LlmAgent, clock: &ManualClock) -> Runtime {
    Runtime::builder(store.clone())
        .agent(agent.clone())
        .worker_id("w")
        .clock(clock.clone())
        .poll_interval(Duration::from_millis(20))
        .lease_ttl(Duration::from_secs(1))
        .retry(RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            multiplier: 1.0,
        })
        .build()
}

async fn wait_injected(faulty: &FaultyStore, method: Method, at_least: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while faulty.injected(method) < at_least {
        assert!(Instant::now() < deadline, "the fault was never injected");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A run that fails right after the model step was recorded takes the recorded answer on its
/// next attempt: the model is not called again and the sources are not read again, because only
/// the model's answer is journaled, not the tools it was given.
#[tokio::test]
async fn a_replayed_model_step_neither_calls_the_model_nor_reads_the_sources() {
    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = faulty.clone();
    let h = Harness {
        store: store.clone(),
        mock: Arc::new(MockModel::new()),
    };
    let (source, listed, _) = ContextSource::new();
    let agent = h.agent().tool_source(source).build();
    h.mock.push_text("done");
    // The first journal entry is `model:0`: it is written, and the write then reports a failure,
    // as a crash right after it would.
    faulty.fail_after_apply(Method::JournalPut, 1);
    let rt = quick_runtime(&store, &agent, &ManualClock::new());
    let run = rt
        .start(
            "llm",
            message_with("hi", json!({"offer": ["relay__x"]})),
            None,
        )
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_injected(&faulty, Method::JournalPut, 1).await;
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(view.output.expect("output")["text"], "done");
    assert_eq!(h.mock.requests().len(), 1, "the recorded answer was used");
    assert_eq!(
        listed.load(SeqCst),
        1,
        "the source was read once, not again"
    );
}

/// A call to a source's tool is recorded by its step like any tool call: a run that fails right
/// after it was recorded does not make the call again.
#[tokio::test]
async fn a_source_tool_that_was_recorded_is_not_called_again_by_a_replay() {
    /// Offers `relay__x`; the first time it is called, the next journal write (the step that
    /// records this very call) is applied and then reported as failed.
    struct Arms {
        faulty: Arc<FaultyStore>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ToolSource for Arms {
        async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
            vec![spec("relay__x")]
        }
        async fn call(
            &self,
            _ctx: &ToolCtx,
            name: &str,
            _args: Value,
        ) -> Option<Result<ToolOutput, ToolError>> {
            (name == "relay__x").then(|| {
                if self.calls.fetch_add(1, SeqCst) == 0 {
                    self.faulty.fail_after_apply(Method::JournalPut, 1);
                }
                Ok(ToolOutput::text("recorded"))
            })
        }
    }
    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = faulty.clone();
    let h = Harness {
        store: store.clone(),
        mock: Arc::new(MockModel::new()),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let agent = h
        .agent()
        .tool_source(Arms {
            faulty: faulty.clone(),
            calls: calls.clone(),
        })
        .build();
    h.mock
        .push_tool_calls(vec![call("c1", "relay__x", json!({}))])
        .push_text("done");
    let rt = quick_runtime(&store, &agent, &ManualClock::new());
    let run = rt
        .start("llm", user_message("hi"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(faulty.injected(Method::JournalPut), 1, "the fault struck");
    assert_eq!(calls.load(SeqCst), 1, "the tool ran once");
    assert_eq!(view.output.clone().expect("output")["text"], "done");
    assert_eq!(
        conversation(&view).messages[2],
        Message::tool_result("c1", "recorded"),
        "and its recorded answer is the result"
    );
}

/// A source's tool that starts a task of another system cannot be asked how it stands (the agent
/// asks the tool that started it, and a source's tool is not known by then): the call ends with an
/// error result, and the run goes on.
#[tokio::test]
async fn a_source_tool_cannot_wait_on_a_remote_task_and_the_call_is_an_error_result() {
    struct Starts;
    #[async_trait]
    impl ToolSource for Starts {
        async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
            vec![spec("start_task")]
        }
        async fn call(
            &self,
            _ctx: &ToolCtx,
            name: &str,
            _args: Value,
        ) -> Option<Result<ToolOutput, ToolError>> {
            (name == "start_task").then(|| {
                Err(ToolError::AwaitRemote {
                    task: "t-1".into(),
                    timeout_ms: None,
                })
            })
        }
    }
    let h = Harness::new();
    let agent = h.agent().tool_source(Starts).build();
    h.mock
        .push_tool_calls(vec![call("c1", "start_task", json!({}))])
        .push_text("noted");
    let clock = ManualClock::new();
    let rt = quick_runtime(&h.store, &agent, &clock);
    let run = rt
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let worker = spawn_worker(&rt);
    wait_for(&rt, run, "parked on the wait timer", |v| {
        v.status == RunStatus::Parked && v.wake_at.is_some()
    })
    .await;
    // The timer fires: the agent looks, and there is nobody to ask.
    clock.advance(Duration::from_secs(61));
    let view = wait_done(&rt, run).await;
    worker.stop().await;

    assert_eq!(view.output.clone().expect("output")["text"], "noted");
    let state = conversation(&view);
    assert!(
        matches!(&state.messages[2], Message::Tool { call_id, content, is_error: true }
            if call_id == "c1" && content.contains("`start_task`") && content.contains("is gone")),
        "{:?}",
        state.messages[2]
    );
}
