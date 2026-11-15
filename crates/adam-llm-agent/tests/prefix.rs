//! The prefix holds (ADR 0044, slice 4): every request of a run starts with the one before it, so a
//! provider's prompt cache keeps hitting. `system` and `tools` are the same bytes each time, and the
//! messages of a request are a byte prefix of the next one's, except at the turns where the history
//! was shortened, which the state records (`Conversation::shortenings`).
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    Conversation, Limits, LlmAgent, LlmAgentBuilder, ReadOutput, Tool, ToolCtx, ToolError,
    ToolOutput, user_message,
};
use adam_model::{
    DynModel, Message, MockModel, ModelError, ModelRequest, ToolCall, ToolChoice, ToolSpec,
};
use adam_runtime::{CollectingSink, ManualClock, RetryPolicy, RunView, Runtime};
use adam_store_testkit::fault::{FaultyStore, Method};
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

/// `big { n, tag }` answers `tag` and `n` letters: results of a size the test chooses.
struct Big;

#[async_trait]
impl Tool for Big {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "big".into(),
            description: "answers a text of the size asked".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let n = args["n"].as_u64().unwrap_or(10) as usize;
        let tag = args["tag"].as_str().unwrap_or("t");
        Ok(ToolOutput::text(format!("{tag}:{}", "x".repeat(n))))
    }
}

/// Answers a long text, kept for `read_output`.
struct Wordy(String);

#[async_trait]
impl Tool for Wordy {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wordy".into(),
            description: "answers a long text, kept".into(),
            parameters: json!({"type": "object"}),
        }
    }
    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let (shown, kept) = adam_llm_agent::keep_long(ctx, &self.0).await;
        let mut output = ToolOutput::text(shown);
        output.kept = kept;
        Ok(output)
    }
}

fn big(id: &str, n: u64) -> ToolCall {
    call(id, "big", json!({"n": n, "tag": id}))
}

struct Rig {
    mock: Arc<MockModel>,
    store: Arc<FaultyStore>,
    clock: ManualClock,
    rt: Runtime,
}

impl Rig {
    fn new(
        max_history_tokens: u32,
        configure: impl FnOnce(LlmAgentBuilder) -> LlmAgentBuilder,
    ) -> Self {
        let mock = Arc::new(MockModel::new());
        let model: DynModel = mock.clone();
        let agent = configure(
            LlmAgent::builder("llm", model, "test-model")
                .instructions("be brief")
                .limits(Limits {
                    max_history_tokens,
                    ..Limits::default()
                })
                .tool(Big),
        )
        .build();
        let store = Arc::new(FaultyStore::new(Arc::new(MemoryStore::new())));
        let dynamic: DynStore = store.clone();
        let clock = ManualClock::new();
        let rt = Runtime::builder(dynamic)
            .agent(agent)
            .clock(clock.clone())
            .event_sink(CollectingSink::new())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .lease_ttl(Duration::from_secs(10))
            .retry(RetryPolicy {
                max_attempts: 4,
                initial_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(5),
                multiplier: 1.0,
            })
            .build();
        Self {
            mock,
            store,
            clock,
            rt,
        }
    }

    async fn start(&self) -> RunId {
        self.rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start")
    }

    /// Work `run` to its end: the requests the model got and the state the run ended with.
    async fn finish(&self, run: RunId) -> (Vec<ModelRequest>, Conversation) {
        let (stop, rx) = oneshot::channel::<()>();
        let rt = self.rt.clone();
        let worker = tokio::spawn(async move {
            rt.run_worker(async {
                let _ = rx.await;
            })
            .await
        });
        // A failed commit is retried after a backoff of the runtime's clock, which only this moves.
        let clock = self.clock.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                clock.advance(Duration::from_millis(200));
            }
        });
        let view = wait_terminal(&self.rt, run).await;
        ticker.abort();
        let _ = stop.send(());
        worker.await.expect("worker task").expect("worker result");
        assert_eq!(view.status, RunStatus::Done, "{view:#?}");
        (self.mock.requests(), conversation(&view))
    }
}

async fn wait_terminal(rt: &Runtime, run: RunId) -> RunView {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out; last view: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).expect("state is a Conversation")
}

fn head(request: &ModelRequest) -> String {
    serde_json::to_string(&(&request.system, &request.tools, &request.tool_choice)).unwrap()
}

/// The messages as the JSON array a provider is sent, without its closing bracket.
fn open_messages(request: &ModelRequest) -> String {
    let mut json = serde_json::to_string(&request.messages).unwrap();
    assert_eq!(json.pop(), Some(']'));
    json
}

/// The requests that do not start with the one before: their indices (the turn each was made at).
fn breaks(requests: &[ModelRequest]) -> Vec<u32> {
    let mut found = Vec::new();
    for (n, pair) in requests.windows(2).enumerate() {
        let (a, b) = (&pair[0], &pair[1]);
        assert_eq!(
            head(a),
            head(b),
            "system and tools are the same bytes at request {}",
            n + 1
        );
        if !open_messages(b).starts_with(&open_messages(a)) {
            found.push(u32::try_from(n + 1).unwrap());
        }
    }
    found
}

/// Six turns: five tool calls and the answer. The third result is big, and the fourth makes the
/// history go over the budget with it no longer the newest.
async fn six_turns(max_history_tokens: u32) -> (Vec<ModelRequest>, Conversation) {
    let rig = Rig::new(max_history_tokens, |b| b);
    rig.mock
        .push_tool_calls(vec![big("c0", 200)])
        .push_tool_calls(vec![big("c1", 200)])
        .push_tool_calls(vec![big("c2", 3000)])
        .push_tool_calls(vec![big("c3", 600)])
        .push_tool_calls(vec![big("c4", 200)])
        .push_text("done");
    let run = rig.start().await;
    rig.finish(run).await
}

#[tokio::test]
async fn each_request_starts_with_the_one_before() {
    let (requests, state) = six_turns(925).await;
    assert_eq!(requests.len(), 6);
    assert_eq!(state.turns, 6);
    let cut = breaks(&requests);
    assert_eq!(
        cut, state.shortenings,
        "the breaks are the recorded shortenings"
    );
    assert_eq!(cut.len(), 1, "exactly one shortening: {cut:?}");
}

#[tokio::test]
async fn without_a_budget_nothing_is_shortened_and_the_state_has_no_member_for_it() {
    let (requests, state) = six_turns(0).await;
    assert_eq!(breaks(&requests), Vec::<u32>::new());
    assert!(state.shortenings.is_empty());
    let written = serde_json::to_value(&state).unwrap();
    assert!(written.get("shortenings").is_none(), "{written}");
}

#[tokio::test]
async fn a_shortening_is_sent_the_same_way_after() {
    let (requests, state) = six_turns(925).await;
    let [at] = state.shortenings[..] else {
        panic!("one shortening: {:?}", state.shortenings);
    };
    let at = at as usize;
    // The request that shortened is the history as the state now holds it, and every later request
    // carries those messages, byte for byte.
    for later in &requests[at..] {
        assert_eq!(
            serde_json::to_string(&later.messages[..]).unwrap()
                [..open_messages(&requests[at]).len()],
            open_messages(&requests[at]),
        );
    }
    assert_eq!(
        state.messages[..requests[at].messages.len()],
        requests[at].messages[..],
        "the request is the state's messages"
    );
    // What was cut is the oldest of the outputs, with its marker; the newest is whole.
    let texts: Vec<String> = state
        .messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .map(Message::text)
        .collect();
    assert!(
        texts[0].contains(adam_llm_agent::TRUNCATION_MARKER_PREFIX),
        "{}",
        texts[0]
    );
    assert!(
        texts[2].contains(adam_llm_agent::TRUNCATION_MARKER_PREFIX),
        "{}",
        &texts[2][..80]
    );
    assert_eq!(
        texts[4],
        format!("c4:{}", "x".repeat(200)),
        "the newest result is whole"
    );
    // The request before it was the long history, which the shortening replaced.
    assert!(requests[at - 1].messages.len() < requests[at].messages.len());
    let old = &requests[at - 1].messages[2];
    assert_eq!(old.text(), format!("c0:{}", "x".repeat(200)));
}

#[tokio::test]
async fn a_state_written_before_shortenings_loads_and_one_without_them_writes_none() {
    let old = json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}], "turns": 1});
    let state: Conversation = serde_json::from_value(old).unwrap();
    assert!(state.shortenings.is_empty());
    assert!(
        serde_json::to_value(&state)
            .unwrap()
            .get("shortenings")
            .is_none()
    );
    let mut state = state;
    state.shortenings.push(4);
    assert_eq!(
        serde_json::to_value(&state).unwrap()["shortenings"],
        json!([4])
    );
}

/// A model call that failed and is made again (the runtime retries it with the committed state)
/// is sent the same bytes, shortening included.
#[tokio::test]
async fn a_retried_model_call_sends_the_same_request_after_a_shortening() {
    let rig = Rig::new(925, |b| b);
    rig.mock
        .push_tool_calls(vec![big("c0", 200)])
        .push_tool_calls(vec![big("c1", 200)])
        .push_tool_calls(vec![big("c2", 3000)])
        .push_tool_calls(vec![big("c3", 600)])
        .push_error(ModelError::transient("blip"))
        .push_tool_calls(vec![big("c4", 200)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 7, "one call was made twice");
    let (failed, retried) = (&requests[4], &requests[5]);
    assert_eq!(
        serde_json::to_string(failed).unwrap(),
        serde_json::to_string(retried).unwrap(),
        "the same request, byte for byte"
    );
    assert_eq!(state.shortenings, [4], "recorded once");
    assert!(
        state
            .messages
            .iter()
            .any(|m| m.text().contains(adam_llm_agent::TRUNCATION_MARKER_PREFIX))
    );
}

/// A commit lost at any point (before it applied, or after, with the acknowledgement lost) leaves
/// the run to be stepped again from the state before it: it ends with the history, the shortenings
/// and the requests of a run that lost nothing.
#[tokio::test]
async fn a_lost_commit_replays_the_same_requests() {
    let baseline = six_turns(925).await;
    for n in [1, 3, 5, 6, 8] {
        for after in [false, true] {
            let rig = Rig::new(925, |b| b);
            rig.mock
                .push_tool_calls(vec![big("c0", 200)])
                .push_tool_calls(vec![big("c1", 200)])
                .push_tool_calls(vec![big("c2", 3000)])
                .push_tool_calls(vec![big("c3", 600)])
                .push_tool_calls(vec![big("c4", 200)])
                .push_text("done");
            let run = rig.start().await;
            if after {
                rig.store.fail_run_after_apply(Method::CommitRun, run, n);
            } else {
                rig.store.fail_run(Method::CommitRun, run, n);
            }
            let (requests, state) = rig.finish(run).await;
            let label = format!("commit {n}, after apply: {after}");
            assert_eq!(state.messages, baseline.1.messages, "{label}");
            assert_eq!(state.shortenings, baseline.1.shortenings, "{label}");
            assert_eq!(requests.len(), baseline.0.len(), "{label}");
            for (got, want) in requests.iter().zip(&baseline.0) {
                assert_eq!(
                    serde_json::to_string(got).unwrap(),
                    serde_json::to_string(want).unwrap(),
                    "{label}"
                );
            }
        }
    }
}

/// A kept output and the reads of it: the long text is in the state beside the history, and the
/// history a read adds extends the one before; a shortening of the shown first page does not touch
/// what `read_output` reads.
#[tokio::test]
async fn a_kept_output_and_its_reads_keep_the_prefix() {
    let text: String = (0..7000).map(|i| format!("row-{i:06}\n")).collect();
    // A budget the pages of a read go over: one shortening, at a read.
    let rig = Rig::new(2600, |b| b.tool(Wordy(text.clone())).tool(ReadOutput));
    let run = rig.start().await;
    let id = adam_llm_agent::output_id(&run.to_string(), "c1");
    rig.mock
        .push_tool_calls(vec![call("c1", "wordy", json!({}))]);
    for (n, offset) in [2001, 4001, 6001].into_iter().enumerate() {
        rig.mock.push_tool_calls(vec![call(
            &format!("r{n}"),
            "read_output",
            json!({"id": id, "offset": offset}),
        )]);
    }
    rig.mock.push_text("done");
    let (requests, state) = rig.finish(run).await;
    assert_eq!(requests.len(), 5);
    assert_eq!(breaks(&requests), state.shortenings);
    assert!(
        !state.shortenings.is_empty(),
        "the pages go over the budget"
    );
    assert_eq!(
        state.kept_outputs.get(&id),
        Some(&text),
        "what is kept is whole"
    );
    // The last page of the output is in the newest result, whole.
    let last = state
        .messages
        .iter()
        .rev()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert!(last.text().contains("row-006999"), "{}", last.text());
}

/// The closing turn is appended to the request and not kept, so a run that goes on after it (a
/// message that arrives, which reopens the run) does not start with the closing turn's request:
/// the one place the rule does not hold. The request that has the closing turn still starts with the
/// one before it.
#[tokio::test]
async fn the_closing_turn_is_the_end_of_the_prefix() {
    let rig = Rig::new(0, |b| b);
    let model: DynModel = rig.mock.clone();
    let agent = LlmAgent::builder("llm", model, "test-model")
        .instructions("be brief")
        .limits(Limits {
            max_turns: 2,
            ..Limits::default()
        })
        .tool(Big)
        .build();
    let rt = {
        let store: DynStore = Arc::new(MemoryStore::new());
        Runtime::builder(store)
            .agent(agent)
            .event_sink(CollectingSink::new())
            .worker_id("w")
            .poll_interval(Duration::from_millis(10))
            .build()
    };
    rig.mock
        .push_tool_calls(vec![big("c0", 50)])
        .push_tool_calls(vec![big("c1", 50)])
        .push_text("here is what I did");
    let run = rt.start("llm", user_message("go"), None).await.unwrap();
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
    let view = wait_terminal(&rt, run).await;
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    assert_eq!(view.status, RunStatus::Done);
    let requests = rig.mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].tool_choice, ToolChoice::None);
    let mut shown = requests.clone();
    for r in &mut shown {
        r.tool_choice = ToolChoice::Auto;
    }
    assert_eq!(
        breaks(&shown),
        Vec::<u32>::new(),
        "the closing request extends the one before"
    );
    assert_eq!(
        requests[2].messages.last().map(Message::text),
        Some(adam_llm_agent::CLOSING_TURN.to_owned())
    );
    assert!(
        !conversation(&view)
            .messages
            .iter()
            .any(|m| m.text() == adam_llm_agent::CLOSING_TURN),
        "the closing turn is not in the history"
    );
}

/// The loop says the cache key of every request: the hex of a SHA-256 of the conversation id, 32
/// characters, the same for every request of the conversation, and none for a run with no
/// conversation id; the id itself is not in it.
#[tokio::test]
async fn the_loop_gives_every_request_of_a_conversation_the_same_cache_key() {
    let rig = Rig::new(0, |b| b);
    rig.mock
        .push_tool_calls(vec![big("c0", 5)])
        .push_text("done");
    let run = rig
        .rt
        .start("llm", user_message("go"), Some("alice:thread-7"))
        .await
        .unwrap();
    let (requests, _) = rig.finish(run).await;
    let keys: Vec<&str> = requests
        .iter()
        .map(|r| r.prompt_cache_key.as_deref().expect("a key"))
        .collect();
    assert_eq!(keys[0], keys[1]);
    assert_eq!(keys[0].len(), 32);
    assert!(
        keys[0]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert!(!keys[0].contains("alice"));

    let rig = Rig::new(0, |b| b);
    rig.mock.push_text("done");
    let run = rig.start().await;
    let (requests, _) = rig.finish(run).await;
    assert_eq!(
        requests[0].prompt_cache_key, None,
        "no conversation, no key"
    );
}

/// The tools a source offers are in the model's request by name, whatever order it listed them in.
#[tokio::test]
async fn the_tools_of_a_source_are_offered_in_name_order() {
    struct Shuffled(std::sync::atomic::AtomicBool);
    #[async_trait]
    impl adam_llm_agent::ToolSource for Shuffled {
        async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<ToolSpec> {
            let flip = self.0.fetch_xor(true, std::sync::atomic::Ordering::SeqCst);
            let mut names = ["delta", "alpha", "charlie", "bravo"];
            if flip {
                names.reverse();
            }
            names
                .iter()
                .map(|n| ToolSpec {
                    name: (*n).into(),
                    description: format!("the {n} tool"),
                    parameters: json!({"type": "object"}),
                })
                .collect()
        }
        async fn call(
            &self,
            _ctx: &ToolCtx,
            _name: &str,
            _args: Value,
        ) -> Option<Result<ToolOutput, ToolError>> {
            Some(Ok(ToolOutput::text("ok")))
        }
    }
    let rig = Rig::new(0, |b| b.tool_source(Shuffled(Default::default())));
    rig.mock
        .push_tool_calls(vec![big("c0", 5)])
        .push_tool_calls(vec![big("c1", 5)])
        .push_text("done");
    let run = rig.start().await;
    let (requests, _) = rig.finish(run).await;
    let names = |r: &ModelRequest| r.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>();
    assert_eq!(
        names(&requests[0]),
        ["big", "alpha", "bravo", "charlie", "delta"]
    );
    assert_eq!(
        breaks(&requests),
        Vec::<u32>::new(),
        "the listing flipped, the request did not"
    );
}
