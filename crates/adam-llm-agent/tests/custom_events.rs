//! Custom events (ADR 0050, slice H5): a tool declares them, emits them, and the step records them;
//! delivery reads the record. Three cases are the slice's proof: an undeclared name, or one in a core
//! namespace, is refused; a replay delivers an event again with the same key; and an event declared
//! for clients goes out as a `RunEvent::Custom` while one declared for hooks stays in the process.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    BuildError, DeclarationError, EventSpec, HookError, HookEvent, HookSet, HookedOutput, LlmAgent,
    Plugin, Subscriptions, Tool, ToolAccess, ToolCtx, ToolError, ToolOutput, user_message,
};
use adam_model::{MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, RunEvent, RunView, Runtime, RuntimeBuilder};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        ..Default::default()
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object"}),
    }
}

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

    fn agent(&self) -> adam_llm_agent::LlmAgentBuilder {
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
            .worker_id(worker)
            .event_sink(self.sink.clone())
            .poll_interval(Duration::from_millis(20))
            .lease_ttl(Duration::from_secs(10)))
        .build()
    }

    fn runtime(&self, agent: &LlmAgent) -> Runtime {
        self.runtime_with(agent, "w", |b| b)
    }

    /// Runs `agent` to the end on one worker.
    async fn run(&self, agent: &LlmAgent) -> (Runtime, RunId) {
        let rt = self.runtime(agent);
        let run = rt
            .start("llm", user_message("go"), None)
            .await
            .expect("start");
        let worker = spawn_worker(&rt);
        wait_done(&rt, run).await;
        worker.stop().await;
        (rt, run)
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

async fn wait_done(rt: &Runtime, run: RunId) -> RunView {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let view = rt.view(run).await.expect("view").expect("run exists");
        if view.status == RunStatus::Done {
            return view;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for done; last view: {view:#?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn notified(n: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), n.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// The `tool:<call id>` entry of `run`, as the step records it.
async fn step_record(store: &DynStore, run: RunId, step: &str) -> HookedOutput {
    let journal = store.journal_list(run).await.expect("journal");
    let entry = journal
        .iter()
        .find(|entry| entry.name == step)
        .unwrap_or_else(|| panic!("no step {step} in {journal:?}"));
    serde_json::from_value(entry.payload.clone()).expect("a HookedOutput")
}

/// The custom events a run sent to clients, as `(kind, payload)`.
fn sent(sink: &CollectingSink, run: RunId) -> Vec<(String, Value)> {
    sink.events_for(run)
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::Custom { kind, payload } if kind.contains('.') => Some((kind, payload)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// The events the `emit` tool declares: one for clients, one for hooks only.
fn declared() -> Vec<EventSpec> {
    vec![
        EventSpec::new("demo.published").published().with_version(3),
        EventSpec::new("demo.kept"),
    ]
}

/// Emits what its arguments say, `{"emit": [["name", data], ...]}`, and answers with one line per
/// emit: `ok` or the refusal. Counts its calls. Runs beside other calls of its turn.
struct Emits {
    name: &'static str,
    declares: Vec<EventSpec>,
    calls: Arc<AtomicUsize>,
}

impl Emits {
    fn new(name: &'static str, declares: Vec<EventSpec>) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                name,
                declares,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

#[async_trait]
impl Tool for Emits {
    fn spec(&self) -> ToolSpec {
        spec(self.name)
    }

    fn events(&self) -> Vec<EventSpec> {
        self.declares.clone()
    }

    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, SeqCst);
        let mut lines = Vec::new();
        for emit in args["emit"].as_array().cloned().unwrap_or_default() {
            let name = emit[0].as_str().unwrap().to_owned();
            lines.push(match ctx.emit_event(&name, emit[1].clone()) {
                Ok(()) => "ok".to_owned(),
                Err(error) => format!("refused: {error}"),
            });
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

/// What an observer saw of a custom event.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    name: String,
    key: String,
    data: Value,
}

/// An observer with chosen subscriptions that records every event it is offered, core ones too.
struct Watches {
    id: &'static str,
    subs: Subscriptions,
    seen: Mutex<Vec<Seen>>,
}

impl Watches {
    fn new(id: &'static str, subs: Subscriptions) -> Arc<Self> {
        Arc::new(Self {
            id,
            subs,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The names it was offered, in order.
    fn names(&self) -> Vec<String> {
        self.seen().into_iter().map(|s| s.name).collect()
    }
}

impl Plugin for Watches {
    fn name(&self) -> &str {
        self.id
    }

    fn subscriptions(&self) -> Subscriptions {
        self.subs.clone()
    }

    fn observe(&self, event: &HookEvent) -> Result<(), HookError> {
        self.seen.lock().unwrap().push(Seen {
            name: event.name().to_owned(),
            key: event.header.key.clone(),
            data: event.fields.get("data").cloned().unwrap_or(Value::Null),
        });
        Ok(())
    }
}

/// Blocks the first time it is called: the worker that dies in it never records its step.
struct Stalls {
    reached: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Stalls {
    fn spec(&self) -> ToolSpec {
        spec("b")
    }

    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        if self.calls.fetch_add(1, SeqCst) == 0 {
            self.reached.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(ToolOutput::text("b-out"))
    }
}

fn arguments(emits: &[(&str, Value)]) -> Value {
    json!({"emit": emits.iter().map(|(n, d)| json!([n, d])).collect::<Vec<_>>()})
}

// ---------------------------------------------------------------------------
// 1. An undeclared name, or one in a reserved namespace, is refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_undeclared_event_is_refused_and_is_neither_recorded_nor_sent() {
    let h = Harness::new();
    let watcher = Watches::new("watcher", Subscriptions::all());
    let (tool, _) = Emits::new("emit", declared());
    let agent = h
        .agent()
        .tool(tool)
        .hooks(HookSet::new().with(watcher.clone()))
        .try_build()
        .expect("the declarations are valid");
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "emit",
            arguments(&[
                ("demo.published", json!({"n": 1})),
                ("demo.undeclared", json!({"n": 2})),
                // A core name the tool never declared (it could not: the build refuses it).
                ("tool.completed", json!({"n": 3})),
                ("demo.kept", json!({"n": 4})),
            ]),
        )])
        .push_text("done");
    let (rt, run) = h.run(&agent).await;

    // The tool was told, per emit.
    let state = rt.state(run).await.unwrap().unwrap();
    let state: adam_llm_agent::Conversation = serde_json::from_value(state).unwrap();
    let answer = state
        .messages
        .iter()
        .find(|m| matches!(m, adam_model::Message::Tool { .. }))
        .map(|m| m.text())
        .expect("the call's answer");
    let lines: Vec<&str> = answer.lines().collect();
    assert_eq!(lines[0], "ok");
    assert!(
        lines[1].contains("`demo.undeclared` is not declared"),
        "{lines:?}"
    );
    assert!(
        lines[2].contains("`tool.completed` is not declared"),
        "{lines:?}"
    );
    assert_eq!(lines[3], "ok");

    // The step recorded the two that were accepted, and only those.
    let record = step_record(&h.store, run, "tool:c1").await;
    let recorded: Vec<(&str, u32)> = record
        .events
        .iter()
        .map(|e| (e.name.as_str(), e.version))
        .collect();
    assert_eq!(recorded, [("demo.published", 3), ("demo.kept", 1)]);

    // An observer that asked for everything saw the two, and no `tool.completed` that is custom.
    let custom: Vec<_> = watcher
        .seen()
        .into_iter()
        .filter(|s| s.name.starts_with("demo."))
        .collect();
    assert_eq!(
        custom.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        ["demo.published", "demo.kept"]
    );
    assert_eq!(
        watcher
            .names()
            .iter()
            .filter(|n| *n == "tool.completed")
            .count(),
        1,
        "the core event of the call, once; the refused custom one is not a second"
    );
}

#[tokio::test]
async fn a_name_in_a_core_namespace_is_a_build_error_whoever_declares_it() {
    let h = Harness::new();
    for name in [
        "tool.custom",
        "run.x",
        "adam.tick",
        "Hook.x",
        "usage.counted",
    ] {
        let (tool, _) = Emits::new("emit", vec![EventSpec::new(name)]);
        let error = h.agent().tool(tool).try_build().expect_err(name);
        assert!(
            matches!(&error, BuildError::Event(DeclarationError::Reserved { .. })),
            "{name}: {error:?}"
        );
        let error = h
            .agent()
            .event(EventSpec::new(name))
            .try_build()
            .expect_err(name);
        assert!(
            matches!(&error, BuildError::Event(DeclarationError::Reserved { .. })),
            "{name} declared by the agent: {error:?}"
        );
    }
    // Not `ns.name`.
    let error = h
        .agent()
        .event(EventSpec::new("nonamespace"))
        .try_build()
        .expect_err("a name with one segment");
    assert!(matches!(
        error,
        BuildError::Event(DeclarationError::BadName { .. })
    ));
    // Two declarations of one name that differ.
    let (a, _) = Emits::new("a", vec![EventSpec::new("demo.x")]);
    let (b, _) = Emits::new("b", vec![EventSpec::new("demo.x").published()]);
    let error = h.agent().tool(a).tool(b).try_build().expect_err("conflict");
    assert!(matches!(
        error,
        BuildError::Event(DeclarationError::Conflict { .. })
    ));
    // The same declaration made twice is one.
    let (a, _) = Emits::new("a", vec![EventSpec::new("demo.x")]);
    let (b, _) = Emits::new("b", vec![EventSpec::new("demo.x")]);
    h.agent().tool(a).tool(b).try_build().expect("one event");
}

#[tokio::test]
async fn a_subscription_nobody_can_satisfy_is_a_build_error() {
    let h = Harness::new();
    let dead = Watches::new("dead", Subscriptions::of(["nothing.*"]).unwrap());
    let error = h
        .agent()
        .hooks(HookSet::new().with(dead))
        .try_build()
        .expect_err("`nothing` is neither core nor declared");
    assert!(matches!(
        error,
        BuildError::Event(DeclarationError::DeadSubscription { .. })
    ));
    // Once something declares the namespace, the same subscription is live.
    let live = Watches::new(
        "live",
        Subscriptions::of(["demo.*", "tool.completed"]).unwrap(),
    );
    h.agent()
        .event(EventSpec::new("demo.x"))
        .hooks(HookSet::new().with(live))
        .try_build()
        .expect("`demo` is declared and `tool.completed` is a core event");
}

#[tokio::test]
async fn a_tool_emits_only_what_it_or_the_agent_declared() {
    let h = Harness::new();
    let watcher = Watches::new("watcher", Subscriptions::all());
    let (a, _) = Emits::new("a", vec![EventSpec::new("demo.from_a")]);
    let (b, _) = Emits::new("b", vec![EventSpec::new("demo.from_b")]);
    let agent = h
        .agent()
        .tool(a)
        .tool(b)
        .event(EventSpec::new("demo.shared"))
        .hooks(HookSet::new().with(watcher.clone()))
        .try_build()
        .unwrap();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "a",
            arguments(&[
                ("demo.from_a", json!({})),
                ("demo.from_b", json!({})),
                ("demo.shared", json!({})),
            ]),
        )])
        .push_text("done");
    let (_, run) = h.run(&agent).await;
    let record = step_record(&h.store, run, "tool:c1").await;
    let names: Vec<&str> = record.events.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["demo.from_a", "demo.shared"], "not another tool's");
}

// ---------------------------------------------------------------------------
// 2. A replay delivers the event again with the same key
// ---------------------------------------------------------------------------

/// The first call's step is journaled, then the worker dies inside the second call's step. The
/// survivor replays the journal: the first call does not run again, and its event reaches the
/// observer a second time under the same key, because delivery reads the record.
#[tokio::test]
async fn a_replay_delivers_the_event_again_with_the_same_key() {
    let h = Harness::new();
    let watcher = Watches::new("watcher", Subscriptions::of(["demo.*"]).unwrap());
    let (a, a_calls) = Emits::new("a", declared());
    // `Alone` makes the two calls run one after the other, so the first is recorded before the
    // second starts.
    struct AloneEmits(Emits);
    #[async_trait]
    impl Tool for AloneEmits {
        fn spec(&self) -> ToolSpec {
            self.0.spec()
        }
        fn events(&self) -> Vec<EventSpec> {
            self.0.events()
        }
        async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
            self.0.call(ctx, args).await
        }
    }
    let b_calls = Arc::new(AtomicUsize::new(0));
    let reached = Arc::new(Notify::new());
    let agent = h
        .agent()
        .tool(AloneEmits(a))
        .tool(Stalls {
            reached: reached.clone(),
            calls: b_calls.clone(),
        })
        .hooks(HookSet::new().with(watcher.clone()))
        .try_build()
        .unwrap();
    h.mock
        .push_tool_calls(vec![
            call(
                "c1",
                "a",
                arguments(&[
                    ("demo.published", json!({"n": 1})),
                    ("demo.kept", json!({"n": 2})),
                ]),
            ),
            call("c2", "b", json!({})),
        ])
        .push_text("both done");

    let short = |b: RuntimeBuilder| b.lease_ttl(Duration::from_millis(300));
    let (rt_a, rt_b) = (
        h.runtime_with(&agent, "crash-a", short),
        h.runtime_with(&agent, "crash-b", short),
    );
    let run = rt_a
        .start("llm", user_message("go"), None)
        .await
        .expect("start");
    let doomed = spawn_worker(&rt_a);
    notified(&reached, "the second call starting").await;
    doomed.handle.abort();
    let _ = doomed.handle.await;
    assert_eq!(
        watcher.seen().len(),
        2,
        "the first worker delivered both events once: {:?}",
        watcher.seen()
    );

    let survivor = spawn_worker(&rt_b);
    wait_done(&rt_b, run).await;
    survivor.stop().await;

    assert_eq!(
        a_calls.load(SeqCst),
        1,
        "the recorded tool did not run twice"
    );
    let seen = watcher.seen();
    let key = |i: usize| format!("{run}/tool:c1/{}/{i}", ["demo.published", "demo.kept"][i]);
    assert_eq!(
        seen.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        [key(0), key(1), key(0), key(1)],
        "delivered again, in order, under the same keys: {seen:?}"
    );
    assert_eq!(seen[0].data, seen[2].data);
    assert_eq!(seen[0].data, json!({"n": 1}));
    assert_eq!(
        step_record(&h.store, run, "tool:c1").await.events.len(),
        2,
        "the record is what was delivered from"
    );
}

// ---------------------------------------------------------------------------
// 3. Clients see a `Clients` event and not a `Hooks` one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_clients_event_goes_out_as_a_custom_run_event_and_a_hooks_one_stays_in() {
    let h = Harness::new();
    let watcher = Watches::new("watcher", Subscriptions::of(["demo.*"]).unwrap());
    let (tool, _) = Emits::new("emit", declared());
    let agent = h
        .agent()
        .tool(tool)
        .hooks(HookSet::new().with(watcher.clone()))
        .try_build()
        .unwrap();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "emit",
            arguments(&[
                ("demo.published", json!({"n": 1})),
                ("demo.kept", json!({"n": 2})),
            ]),
        )])
        .push_text("done");
    let (_, run) = h.run(&agent).await;

    let out = sent(&h.sink, run);
    assert_eq!(
        out.len(),
        1,
        "only the Clients event reaches clients: {out:?}"
    );
    assert_eq!(out[0].0, "demo.published");
    assert_eq!(
        out[0].1,
        json!({
            "version": 3,
            "key": format!("{run}/tool:c1/demo.published/0"),
            "data": {"n": 1},
        })
    );
    assert_eq!(
        watcher.names(),
        ["demo.published", "demo.kept"],
        "observers read both"
    );
}

#[tokio::test]
async fn clients_get_a_clients_event_with_no_plugin_installed() {
    // Delivery to clients does not depend on a hook set.
    let h = Harness::new();
    let (tool, _) = Emits::new("emit", declared());
    let agent = h.agent().tool(tool).try_build().unwrap();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "emit",
            arguments(&[("demo.published", json!({})), ("demo.kept", json!({}))]),
        )])
        .push_text("done");
    let (_, run) = h.run(&agent).await;
    let out = sent(&h.sink, run);
    assert_eq!(
        out.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["demo.published"]
    );
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_observer_gets_custom_events_only_by_subscription_and_keeps_its_core_ones() {
    let h = Harness::new();
    let core_only = Watches::new("core", Subscriptions::core());
    let exact = Watches::new("exact", Subscriptions::of(["demo.kept"]).unwrap());
    let namespace = Watches::new("ns", Subscriptions::of(["demo.*"]).unwrap());
    let other = Watches::new("other", Subscriptions::of(["other.*"]).unwrap());
    let none = Watches::new("none", Subscriptions::none());
    let both = Watches::new("both", Subscriptions::core().with("demo.kept").unwrap());
    let (tool, _) = Emits::new("emit", declared());
    let agent = h
        .agent()
        .tool(tool)
        .event(EventSpec::new("other.thing"))
        .hooks(
            HookSet::new()
                .with(core_only.clone())
                .with(exact.clone())
                .with(namespace.clone())
                .with(other.clone())
                .with(none.clone())
                .with(both.clone()),
        )
        .try_build()
        .unwrap();
    h.mock
        .push_tool_calls(vec![call(
            "c1",
            "emit",
            arguments(&[("demo.published", json!({})), ("demo.kept", json!({}))]),
        )])
        .push_text("done");
    h.run(&agent).await;

    let custom = |w: &Watches| -> Vec<String> {
        w.names()
            .into_iter()
            .filter(|n| n.starts_with("demo."))
            .collect()
    };
    let core = |w: &Watches| -> usize { w.names().iter().filter(|n| !n.contains("demo.")).count() };
    assert!(
        custom(&core_only).is_empty(),
        "the default is the core catalogue"
    );
    assert!(
        core(&core_only) > 0,
        "and it still receives the core events"
    );
    assert_eq!(custom(&exact), ["demo.kept"]);
    assert_eq!(core(&exact), 0, "patterns alone leave the core out");
    assert_eq!(custom(&namespace), ["demo.published", "demo.kept"]);
    assert!(other.names().is_empty(), "another namespace sees nothing");
    assert!(none.names().is_empty());
    assert_eq!(custom(&both), ["demo.kept"]);
    assert_eq!(core(&both), core(&core_only));
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn data_over_4_kib_is_refused_and_a_step_holds_sixteen() {
    let h = Harness::new();
    let (tool, _) = Emits::new("emit", declared());
    let agent = h.agent().tool(tool).try_build().unwrap();
    let mut emits = vec![("demo.published", json!({"text": "x".repeat(5000)}))];
    emits.extend((0..17).map(|n| ("demo.kept", json!({"n": n}))));
    h.mock
        .push_tool_calls(vec![call("c1", "emit", arguments(&emits))])
        .push_text("done");
    let (rt, run) = h.run(&agent).await;
    let record = step_record(&h.store, run, "tool:c1").await;
    assert_eq!(
        record.events.len(),
        16,
        "the oversized one took no slot, the 17th is refused"
    );
    assert!(record.events.iter().all(|e| e.name == "demo.kept"));
    let state: adam_llm_agent::Conversation =
        serde_json::from_value(rt.state(run).await.unwrap().unwrap()).unwrap();
    let answer = state
        .messages
        .iter()
        .find(|m| matches!(m, adam_model::Message::Tool { .. }))
        .map(|m| m.text())
        .unwrap();
    assert!(answer.lines().next().unwrap().contains("the most is 4096"));
    assert!(answer.lines().last().unwrap().contains("at most 16 events"));
}

/// Seventeen calls of a turn each emit sixteen: the transition holds 256, not 17 times 16. A counter
/// per call would let every one through.
#[tokio::test]
async fn a_transition_holds_256_events_across_all_its_calls() {
    let h = Harness::new();
    let (tool, _) = Emits::new("emit", vec![EventSpec::new("demo.kept")]);
    let agent = h.agent().tool(tool).max_parallel(8).try_build().unwrap();
    let burst: Vec<(&str, Value)> = (0..16).map(|n| ("demo.kept", json!({"n": n}))).collect();
    h.mock
        .push_tool_calls(
            (0..17)
                .map(|n| {
                    // Distinct arguments: three identical calls in a row are a doom loop, refused.
                    let mut args = arguments(&burst);
                    args["call"] = json!(n);
                    call(&format!("c{n}"), "emit", args)
                })
                .collect(),
        )
        .push_text("done");
    let (_, run) = h.run(&agent).await;
    let mut total = 0;
    let mut refused_calls = 0;
    for n in 0..17 {
        let record = step_record(&h.store, run, &format!("tool:c{n}")).await;
        total += record.events.len();
        if record.events.len() < 16 {
            refused_calls += 1;
        }
    }
    assert_eq!(total, 256);
    assert_eq!(
        refused_calls, 1,
        "exactly the call that found the budget spent"
    );
}
