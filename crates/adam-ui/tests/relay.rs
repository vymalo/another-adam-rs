//! The thread tools the orchestrator reports (`thread-tools/v1`: `_meta["thread-tools/v1"]` of a
//! tool, the request's `_meta {callId, parentStepId}`) and the agents a person mentioned
//! (`mentions/v1`), against the fake endpoint of `adam-mcp-testkit`: a long call honours the time
//! the tool says and the cap, the request carries a `callId` that a retried step repeats, a tool the
//! orchestrator reports gets no step of the agent's (and the others do), and the mentions reach the
//! model's instructions only when there are some.
//!
//! What these assert is recorded: the endpoint's requests (with their `_meta`), the run's events and
//! output, what the model was sent, and the time a call took.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_a2a_runtime::{CONTEXT_MENTIONS, CONTEXT_THREAD_TOOLS};
use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{Listing, LlmAgent, SourceCtx, ToolCtx, ToolError, ToolOutput, ToolSource};
use adam_mcp::McpPolicy;
use adam_mcp_testkit::{ThreadToolsServer, wait_until};
use adam_model::{DynModel, MockModel, ToolCall, ToolSpec};
use adam_runtime::{
    CancelToken, CollectingSink, Inbound, ManualClock, NoopSink, RetryPolicy, RunEvent, Runtime,
    StepState,
};
use adam_store_testkit::fault::{FaultyStore, Method};
use adam_ui::{META_KEY, ThreadTools, ThreadToolsClient};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

const TOKEN: &str = "sekret-token-4f1c9a";

fn grant(server: &ThreadToolsServer) -> Map<String, Value> {
    json!({CONTEXT_THREAD_TOOLS: {
        "url": server.url("thread-1"), "token": TOKEN, "expiresAt": "2999-01-01T00:00:00Z"}})
    .as_object()
    .cloned()
    .unwrap()
}

fn source(policy: McpPolicy) -> ThreadTools {
    ThreadTools::new(Arc::new(ThreadToolsClient::new(policy)))
}

/// The tools of the test endpoint: `relay__search` (the orchestrator reports its steps and says it
/// may take 125 s) and `plain__tool` (says nothing).
fn endpoint_tools(server: &ThreadToolsServer) {
    server.add_tool_with_meta(
        "relay__search",
        "Search.",
        json!({"type": "object"}),
        "found",
        json!({META_KEY: {"reportsStep": true, "timeoutSecs": 125}}),
    );
    server.add_tool("plain__tool", "Plain.", json!({"type": "object"}), "plain");
}

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

/// The note a listing made about `tool`, put on a call as the agent does.
async fn ctx_for(source: &ThreadTools, server: &ThreadToolsServer, tool: &str) -> ToolCtx {
    let listing: Listing = source.listing(&SourceCtx::detached(grant(server))).await;
    let note = listing.notes.into_iter().find(|n| n.tool == tool);
    ToolCtx::detached(tool, "call_1", Arc::new(NoopSink))
        .with_context(grant(server))
        .with_note(note)
}

// ---------------------------------------------------------------------------------------------
// What the listing says, what the call waits for
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_listing_reads_what_each_tool_says_about_itself() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    endpoint_tools(&server);
    // Malformed and partial entries say nothing a caller can use.
    server.add_tool_with_meta(
        "a__zero",
        "x",
        json!({"type": "object"}),
        "x",
        json!({META_KEY: {"timeoutSecs": 0}}),
    );
    server.add_tool_with_meta(
        "a__text",
        "x",
        json!({"type": "object"}),
        "x",
        json!({META_KEY: {"reportsStep": "yes", "timeoutSecs": "60"}}),
    );
    server.add_tool_with_meta(
        "a__only_time",
        "x",
        json!({"type": "object"}),
        "x",
        json!({META_KEY: {"timeoutSecs": 900}}),
    );
    server.add_tool_with_meta(
        "a__other_key",
        "x",
        json!({"type": "object"}),
        "x",
        json!({"other/v1": {"reportsStep": true}}),
    );
    let listing = source(McpPolicy::default())
        .listing(&SourceCtx::detached(grant(&server)))
        .await;
    assert_eq!(
        listing.specs.len(),
        7,
        "every tool is offered, notes or not"
    );
    let notes: Vec<(String, bool, Option<u64>)> = listing
        .notes
        .iter()
        .map(|n| (n.tool.clone(), n.reports_step, n.timeout_ms))
        .collect();
    assert_eq!(
        notes,
        [
            ("relay__search".to_owned(), true, Some(125_000)),
            ("a__only_time".to_owned(), false, Some(900_000)),
        ],
        "only what is well formed and says something"
    );
}

/// The step of `turn_output` is called "Send the answer", not the tool's name; a tool the endpoint
/// lists with no title keeps its own name.
#[tokio::test]
async fn the_answer_tool_is_drawn_under_a_title() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    server.enable_turn_output();
    server.add_tool("plain__tool", "Plain.", json!({"type": "object"}), "plain");
    let listing = source(McpPolicy::default())
        .listing(&SourceCtx::detached(grant(&server)))
        .await;
    let labels: Vec<(String, Option<String>)> = listing
        .notes
        .iter()
        .map(|n| (n.tool.clone(), n.label.clone()))
        .collect();
    assert_eq!(
        labels,
        [("turn_output".to_owned(), Some("Send the answer".to_owned()))]
    );
}

#[tokio::test]
async fn a_long_call_waits_as_long_as_its_tool_says_not_as_long_as_the_default() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    server.add_tool_with_meta(
        "relay__slow",
        "Slow.",
        json!({"type": "object"}),
        "slow",
        json!({META_KEY: {"reportsStep": true, "timeoutSecs": 5}}),
    );
    server.set_delay("relay__slow", Duration::from_millis(700));
    // The default wait for a tool that says nothing is a fifth of a second here.
    let policy = McpPolicy::default().call_timeout(Duration::from_millis(200));
    let source = source(policy);

    // The tool says it may take 5 s: the call, which takes 0.7 s, is waited for.
    let ctx = ctx_for(&source, &server, "relay__slow").await;
    let started = Instant::now();
    let done = source
        .call(&ctx, "relay__slow", json!({}))
        .await
        .unwrap()
        .unwrap();
    assert!(!done.is_error, "{}", done.content);
    assert!(done.content.starts_with("slow"), "{}", done.content);
    assert!(started.elapsed() >= Duration::from_millis(700));

    // The same call with no note (a tool that says nothing) is given up on after the default.
    let bare =
        ToolCtx::detached("relay__slow", "call_2", Arc::new(NoopSink)).with_context(grant(&server));
    let started = Instant::now();
    let timed_out = source
        .call(&bare, "relay__slow", json!({}))
        .await
        .unwrap()
        .unwrap();
    assert!(timed_out.is_error, "{}", timed_out.content);
    assert!(
        timed_out.content.contains("did not finish") && timed_out.content.contains("200 ms"),
        "{}",
        timed_out.content
    );
    assert!(
        started.elapsed() < Duration::from_millis(650),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_cap_limits_what_a_tool_asks_for() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    server.add_tool_with_meta(
        "relay__hours",
        "Hours.",
        json!({"type": "object"}),
        "never",
        json!({META_KEY: {"reportsStep": true, "timeoutSecs": 3600}}),
    );
    server.set_delay("relay__hours", Duration::from_secs(30));
    // The deployment's cap: a quarter of a second, whatever the tool asks for.
    let policy = McpPolicy::default().thread_tools_max_call(Duration::from_millis(250));
    let source = source(policy);
    let ctx = ctx_for(&source, &server, "relay__hours").await;
    let started = Instant::now();
    let capped = source
        .call(&ctx, "relay__hours", json!({}))
        .await
        .unwrap()
        .unwrap();
    assert!(capped.is_error, "{}", capped.content);
    assert!(capped.content.contains("250 ms"), "{}", capped.content);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    // And the default of the cap is an hour: a tool that asks for 125 s gets 125 s.
    let default = ThreadToolsClient::new(McpPolicy::default());
    let note = adam_llm_agent::ToolNote::new("t").with_timeout(Duration::from_secs(125));
    assert_eq!(default.wait_for(Some(&note)), Duration::from_secs(125));
    let long = adam_llm_agent::ToolNote::new("t").with_timeout(Duration::from_secs(7200));
    assert_eq!(default.wait_for(Some(&long)), Duration::from_secs(3600));
    assert_eq!(default.wait_for(None), Duration::from_secs(60));
}

#[tokio::test]
async fn a_cancel_drops_a_long_call_at_once() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    server.add_tool_with_meta(
        "relay__slow",
        "Slow.",
        json!({"type": "object"}),
        "slow",
        json!({META_KEY: {"reportsStep": true, "timeoutSecs": 600}}),
    );
    server.set_delay("relay__slow", Duration::from_secs(30));
    let source = Arc::new(source(McpPolicy::default()));
    let cancel = CancelToken::new();
    let ctx = ctx_for(&source, &server, "relay__slow")
        .await
        .with_cancel_token(cancel.clone());

    let started = Instant::now();
    let call = {
        let source = Arc::clone(&source);
        tokio::spawn(async move { source.call(&ctx, "relay__slow", json!({})).await })
    };
    wait_until("the call to reach the endpoint", || async {
        server.in_flight() == 1
    })
    .await;
    cancel.cancel();
    let result = call.await.unwrap().unwrap().unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("cancelled"), "{}", result.content);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    // The connection went with it: the endpoint has nothing in flight any more.
    wait_until("the endpoint to have nothing in flight", || async {
        server.in_flight() == 0
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// The request's _meta
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_call_is_sent_with_a_call_id_and_the_step_it_runs_under() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    endpoint_tools(&server);
    let source = source(McpPolicy::default());
    let ctx = ctx_for(&source, &server, "relay__search").await;

    source
        .call(&ctx, "relay__search", json!({"q": "a"}))
        .await
        .unwrap()
        .unwrap();
    // The same call again, as a retried step makes it (the same context): the same call id.
    source
        .call(&ctx.clone(), "relay__search", json!({"q": "a"}))
        .await
        .unwrap()
        .unwrap();
    // Another call of the same run, and the same model call id in another run: other ids.
    let other_call = ToolCtx::detached("relay__search", "call_2", Arc::new(NoopSink))
        .with_context(grant(&server));
    source
        .call(&other_call, "relay__search", json!({}))
        .await
        .unwrap()
        .unwrap();
    let other_run = ToolCtx::detached("relay__search", "call_1", Arc::new(NoopSink))
        .with_context(grant(&server));
    source
        .call(&other_run, "relay__search", json!({}))
        .await
        .unwrap()
        .unwrap();
    // A call made under a step the caller reported says so.
    let nested = ctx.clone().under_step("tool:outer-1");
    source
        .call(&nested, "relay__search", json!({}))
        .await
        .unwrap()
        .unwrap();

    let metas: Vec<Value> = server
        .requests()
        .into_iter()
        .filter(|r| r.name == "relay__search")
        .map(|r| r.meta.expect("every call carries _meta")[META_KEY].clone())
        .collect();
    assert_eq!(metas.len(), 5);
    let ids: Vec<&str> = metas
        .iter()
        .map(|m| m["callId"].as_str().unwrap())
        .collect();
    assert_eq!(ids[0], ids[1], "a retry of the step repeats the call id");
    assert_eq!(ids[0], ids[4]);
    assert_eq!(ids[0], format!("{}:call_1", ctx.run_id()));
    assert_ne!(ids[0], ids[2], "another call, another id");
    assert_ne!(
        ids[0], ids[3],
        "another run, another id, though the model's id is the same"
    );
    assert!(
        metas[0].get("parentStepId").is_none(),
        "a call of the model's own is at the top"
    );
    assert_eq!(metas[4]["parentStepId"], "tool:outer-1");
    assert!(ids.iter().all(|id| id.len() <= 256));
}

#[tokio::test]
async fn a_model_call_id_too_long_for_the_contract_is_replaced_by_its_hash_and_stays_stable() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    endpoint_tools(&server);
    let source = source(McpPolicy::default());
    let long_id = "x".repeat(400);
    let ctx = ToolCtx::detached("relay__search", long_id.clone(), Arc::new(NoopSink))
        .with_context(grant(&server));
    source
        .call(&ctx, "relay__search", json!({}))
        .await
        .unwrap()
        .unwrap();
    source
        .call(&ctx, "relay__search", json!({}))
        .await
        .unwrap()
        .unwrap();
    let longer = ToolCtx::detached("relay__search", format!("{long_id}y"), Arc::new(NoopSink))
        .with_context(grant(&server));
    source
        .call(&longer, "relay__search", json!({}))
        .await
        .unwrap()
        .unwrap();

    let ids: Vec<String> = server
        .requests()
        .into_iter()
        .map(|r| {
            r.meta.unwrap()[META_KEY]["callId"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert!(ids[0].len() <= 256, "{}", ids[0].len());
    assert!(ids[0].contains(":sha256-"), "{}", ids[0]);
    assert_eq!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
}

// ---------------------------------------------------------------------------------------------
// Through a whole agent: steps, and a retried step
// ---------------------------------------------------------------------------------------------

struct Rig {
    runtime: Runtime,
    sink: CollectingSink,
    mock: Arc<MockModel>,
}

impl Rig {
    fn new(store: DynStore, source: impl ToolSource) -> Self {
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = LlmAgent::builder("llm", model, "test-model")
            .instructions("You are a test agent.")
            .tool_source(source)
            .build();
        let sink = CollectingSink::new();
        let runtime = Runtime::builder(store)
            .agent(agent)
            .event_sink(sink.clone())
            .worker_id("w")
            .clock(ManualClock::new())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(1))
            .retry(RetryPolicy {
                max_attempts: 3,
                initial_backoff: Duration::from_millis(10),
                max_backoff: Duration::from_millis(10),
                multiplier: 1.0,
            })
            .build();
        Self {
            runtime,
            sink,
            mock,
        }
    }

    /// Run a message with `context` to its end.
    async fn run(&self, text: &str, context: Map<String, Value>) -> (RunId, Value) {
        let run = self
            .runtime
            .start(
                "llm",
                Inbound::new("message", json!({"text": text, "context": context})),
                None,
            )
            .await
            .expect("start");
        let (stop, rx) = oneshot::channel::<()>();
        let worker = {
            let rt = self.runtime.clone();
            tokio::spawn(async move {
                rt.run_worker(async {
                    let _ = rx.await;
                })
                .await
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let view = loop {
            let view = self.runtime.view(run).await.unwrap().unwrap();
            if view.status == RunStatus::Done {
                break view;
            }
            assert!(Instant::now() < deadline, "timed out; {view:#?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let _ = stop.send(());
        worker.await.unwrap().unwrap();
        (run, view.output.expect("output"))
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

#[tokio::test]
async fn the_agent_reports_no_step_for_a_tool_the_orchestrator_reports_and_one_for_the_others() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    endpoint_tools(&server);
    let rig = Rig::new(Arc::new(MemoryStore::new()), source(McpPolicy::default()));
    rig.mock
        .push_tool_calls(vec![
            call("c1", "relay__search", json!({"q": "rust"})),
            call("c2", "plain__tool", json!({})),
        ])
        .push_text("done");
    let (run, output) = rig.run("go", grant(&server)).await;
    assert_eq!(output["text"], "done");

    // `tool:c1` is the relayed call: the orchestrator reports it, with the id it makes from the
    // `callId`. The agent reports `tool:c2` only.
    assert_eq!(
        rig.steps(run),
        [
            ("tool:c2".to_owned(), StepState::Running),
            ("tool:c2".to_owned(), StepState::Completed),
        ]
    );
    // Both calls reached the endpoint, in order, each with its call id.
    let requests = server.requests();
    let names: Vec<&str> = requests.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["relay__search", "plain__tool"]);
    assert_eq!(
        requests[0].meta.as_ref().unwrap()[META_KEY]["callId"],
        format!("{run}:c1")
    );
    assert_eq!(
        requests[1].meta.as_ref().unwrap()[META_KEY]["callId"],
        format!("{run}:c2")
    );
    // The model read both results.
    let sent = rig.mock.requests();
    assert_eq!(sent.len(), 2);
    let results: Vec<String> = sent[1]
        .messages
        .iter()
        .filter_map(|m| match m {
            adam_model::Message::Tool { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].starts_with("found"), "{results:?}");
    assert!(results[1].starts_with("plain"), "{results:?}");
}

#[tokio::test]
async fn an_endpoint_that_says_nothing_about_its_tools_leaves_every_call_a_step() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    server.add_tool(
        "relay__search",
        "Search.",
        json!({"type": "object"}),
        "found",
    );
    let rig = Rig::new(Arc::new(MemoryStore::new()), source(McpPolicy::default()));
    rig.mock
        .push_tool_calls(vec![call("c1", "relay__search", json!({}))])
        .push_text("done");
    let (run, _) = rig.run("go", grant(&server)).await;
    assert_eq!(
        rig.steps(run),
        [
            ("tool:c1".to_owned(), StepState::Running),
            ("tool:c1".to_owned(), StepState::Completed),
        ]
    );
}

/// Arms a failure of the next journal write when the first call starts: the write that records the
/// call's step is lost before it reaches the store, so the step runs again.
struct Arms {
    inner: ThreadTools,
    faulty: Arc<FaultyStore>,
    armed: AtomicBool,
}

#[async_trait]
impl ToolSource for Arms {
    async fn specs(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        self.inner.specs(ctx).await
    }

    async fn listing(&self, ctx: &SourceCtx) -> Listing {
        self.inner.listing(ctx).await
    }

    async fn call(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        if name == "relay__search" && !self.armed.swap(true, SeqCst) {
            self.faulty.fail(Method::JournalPut, 1);
        }
        self.inner.call(ctx, name, args).await
    }
}

#[tokio::test]
async fn a_step_retried_after_a_lost_write_sends_the_same_call_id_and_still_reports_no_step() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    endpoint_tools(&server);
    let faulty = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
    let store: DynStore = faulty.clone();
    let rig = Rig::new(
        store,
        Arms {
            inner: source(McpPolicy::default()),
            faulty: Arc::clone(&faulty),
            armed: AtomicBool::new(false),
        },
    );
    rig.mock
        .push_tool_calls(vec![call("c1", "relay__search", json!({"q": "rust"}))])
        .push_text("done");
    let (run, output) = rig.run("go", grant(&server)).await;
    assert_eq!(output["text"], "done");
    assert_eq!(
        faulty.injected(Method::JournalPut),
        1,
        "the write was lost once"
    );

    let ids: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.name == "relay__search")
        .map(|r| {
            r.meta.unwrap()[META_KEY]["callId"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "the call was made again by the retried step: {ids:?}"
    );
    assert_eq!(ids[0], ids[1], "with the call id it had");
    assert_eq!(ids[0], format!("{run}:c1"));
    assert!(rig.steps(run).is_empty(), "{:?}", rig.steps(run));
}

// ---------------------------------------------------------------------------------------------
// Mentions in the instructions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_mentioned_agents_are_in_the_instructions_only_when_there_are_some() {
    let server = ThreadToolsServer::start(&[TOKEN]).await;
    endpoint_tools(&server);

    // With mentions: the block follows the agent's own instructions.
    let rig = Rig::new(Arc::new(MemoryStore::new()), source(McpPolicy::default()));
    rig.mock.push_text("done");
    let mut context = grant(&server);
    context.insert(
        CONTEXT_MENTIONS.into(),
        json!({"mentions": [
            {"agentId": "mock-researcher", "name": "Mock researcher", "label": "@researcher",
             "start": 6, "end": 17},
            {"agentId": "mock-coder", "label": "@coder", "start": 23, "end": 29}],
               "coordinate": {"tool": "ask_agent"}}),
    );
    rig.run("first @researcher then @coder", context).await;
    let system = rig.mock.requests()[0].system.clone().unwrap();
    assert!(
        system.starts_with("You are a test agent.\n\n## Mentioned agents\n\n"),
        "{system}"
    );
    assert!(
        system.contains("\"@researcher\": agentId \"mock-researcher\", named \"Mock researcher\"")
    );
    assert!(system.contains("\"@coder\": agentId \"mock-coder\""));
    assert!(system.contains("call the tool `ask_agent`"));
    assert!(
        !system.contains(TOKEN),
        "the grant is never in the instructions"
    );

    // Without mentions the instructions are exactly the agent's own.
    let rig = Rig::new(Arc::new(MemoryStore::new()), source(McpPolicy::default()));
    rig.mock.push_text("done");
    rig.run("hello", grant(&server)).await;
    assert_eq!(
        rig.mock.requests()[0].system.as_deref(),
        Some("You are a test agent.")
    );
}
