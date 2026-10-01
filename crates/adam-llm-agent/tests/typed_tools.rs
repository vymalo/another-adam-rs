//! The typed tool helpers end to end: shared state, `try_build`, `ToolSet`,
//! `tools!`, `FnTool`, and (feature `schema`) schemas from `JsonSchema`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_core::{DynStore, MemoryStore, RunId, RunStatus};
use adam_llm_agent::{
    BuildError, Conversation, DynTool, FnTool, Json, LlmAgent, LlmAgentBuilder, StateKey, Tool,
    ToolCtx, ToolError, ToolOutput, ToolSet, parse_args, tools, user_message,
};
use adam_model::{DynModel, Message, MockModel, ToolCall, ToolSpec};
use adam_runtime::{CollectingSink, RunView, Runtime};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn no_params(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("the {name} tool"),
        parameters: json!({"type": "object", "properties": {}}),
    }
}

/// A shared dependency.
struct Greeting(&'static str);

#[derive(Deserialize)]
struct GreetArgs {
    name: String,
}

/// A tool written by hand with every typed helper.
struct Greet;

#[async_trait]
impl Tool for Greet {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "greet".into(),
            description: "Greet someone.".into(),
            parameters: json!({"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}),
        }
    }
    fn required_state(&self) -> Vec<StateKey> {
        vec![StateKey::of::<Greeting>()]
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let args: GreetArgs = match parse_args("greet", args) {
            Ok(args) => args,
            Err(refusal) => return Ok(refusal),
        };
        let greeting = ctx.require_state::<Greeting>()?;
        Ok(ToolOutput::text(format!("{}, {}!", greeting.0, args.name)))
    }
}

/// Answers `"<name>-out"`, needs nothing (a tool as written before state existed).
struct Plain(&'static str);

#[async_trait]
impl Tool for Plain {
    fn spec(&self) -> ToolSpec {
        no_params(self.0)
    }
    async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("{}-out", self.0)))
    }
}

fn builder(mock: &Arc<MockModel>) -> LlmAgentBuilder {
    let model: DynModel = mock.clone();
    LlmAgent::builder("typed", model, "test-model")
}

/// Run `agent` to its end on a scripted model and return the finished run.
async fn run_to_end(agent: &LlmAgent) -> RunView {
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store)
        .agent(agent.clone())
        .event_sink(CollectingSink::new())
        .worker_id("w")
        .poll_interval(Duration::from_millis(20))
        .build();
    let run: RunId = rt.start("typed", user_message("go"), None).await.unwrap();
    let (stop, rx) = tokio::sync::oneshot::channel::<()>();
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
        let view = rt.view(run).await.unwrap().unwrap();
        if view.status == RunStatus::Done || view.status == RunStatus::Failed {
            break view;
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    view
}

fn tool_results(view: &RunView) -> Vec<(String, bool)> {
    let state: Conversation = serde_json::from_value(view.state.clone()).unwrap();
    state
        .messages
        .into_iter()
        .filter_map(|m| match m {
            Message::Tool {
                content, is_error, ..
            } => Some((content, is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn state_given_to_the_builder_reaches_the_tool_in_a_run() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "greet", json!({"name": "Ada"}))])
        .push_text("done");
    let agent = builder(&mock)
        .state(Arc::new(Greeting("Hello")))
        .tools(tools![Greet])
        .try_build()
        .unwrap();
    let view = run_to_end(&agent).await;
    assert_eq!(view.status, RunStatus::Done);
    assert_eq!(tool_results(&view), vec![("Hello, Ada!".to_owned(), false)]);
}

#[tokio::test]
async fn bad_arguments_reach_the_model_as_an_error_result() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "greet", json!({"nome": "Ada"}))])
        .push_text("sorry");
    let agent = builder(&mock)
        .state(Arc::new(Greeting("Hello")))
        .tool(Greet)
        .build();
    let view = run_to_end(&agent).await;
    assert_eq!(view.status, RunStatus::Done, "the run goes on");
    let results = tool_results(&view);
    assert_eq!(results.len(), 1);
    assert!(results[0].1, "marked as an error");
    assert!(
        results[0].0.contains("invalid arguments for `greet`"),
        "{results:?}"
    );
    assert!(results[0].0.contains("missing field `name`"), "{results:?}");
}

#[tokio::test]
async fn a_missing_state_under_build_is_an_error_result_when_the_tool_runs() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "greet", json!({"name": "Ada"}))])
        .push_text("done");
    // `build` stays permissive, for compatibility: no check, no panic.
    let agent = builder(&mock).tool(Greet).build();
    let view = run_to_end(&agent).await;
    let results = tool_results(&view);
    assert!(results[0].1);
    assert!(results[0].0.contains("missing shared state"), "{results:?}");
    assert!(results[0].0.contains("Greeting"), "{results:?}");
}

#[test]
fn try_build_rejects_a_missing_state_and_names_the_tool_and_the_type() {
    let mock = Arc::new(MockModel::new());
    let err = builder(&mock)
        .tool(Plain("a"))
        .tool(Greet)
        .try_build()
        .unwrap_err();
    match &err {
        BuildError::MissingState { tool, state } => {
            assert_eq!(tool, "greet");
            assert!(state.ends_with("Greeting"), "{state}");
        }
        other => panic!("expected MissingState, got {other:?}"),
    }
    assert!(err.to_string().contains("tool `greet` needs shared state"));
    // A state of another type does not count.
    let err = builder(&mock)
        .state(Arc::new(0_u8))
        .tool(Greet)
        .try_build()
        .unwrap_err();
    assert!(matches!(err, BuildError::MissingState { .. }));
    // The right one does, whatever the order of the calls.
    assert!(
        builder(&mock)
            .tool(Greet)
            .state(Arc::new(Greeting("Hi")))
            .try_build()
            .is_ok()
    );
}

#[test]
fn try_build_rejects_duplicate_names_but_build_keeps_the_last() {
    let mock = Arc::new(MockModel::new());
    let err = builder(&mock)
        .tool(Plain("a"))
        .tool(Plain("b"))
        .tool(Plain("a"))
        .try_build()
        .unwrap_err();
    assert_eq!(err, BuildError::DuplicateTool { name: "a".into() });
    assert_eq!(err.to_string(), "two tools are called `a`");
    let agent = builder(&mock).tool(Plain("a")).tool(Plain("a")).build();
    assert!(format!("{agent:?}").contains(r#"["a"]"#));
}

#[test]
fn an_agent_without_extras_builds_as_before() {
    let mock = Arc::new(MockModel::new());
    assert!(builder(&mock).try_build().is_ok());
    assert!(builder(&mock).tool(Plain("a")).try_build().is_ok());
}

#[test]
fn a_tool_context_carries_state_for_unit_tests() {
    let sink: adam_runtime::DynEventSink = Arc::new(CollectingSink::new());
    let ctx = ToolCtx::detached("greet", "c1", sink);
    assert!(ctx.state::<Greeting>().is_none());
    assert!(ctx.require_state::<Greeting>().is_err());
    let ctx = ctx.with_state(Arc::new(Greeting("Yo")));
    assert_eq!(ctx.state::<Greeting>().unwrap().0, "Yo");
    // A clone shares the value.
    assert_eq!(ctx.clone().require_state::<Greeting>().unwrap().0, "Yo");
}

#[test]
fn a_tool_set_keeps_order_and_composes() {
    let set = tools![Plain("a"), Plain("b"),];
    assert_eq!(set.names(), ["a", "b"]);
    assert_eq!(set.len(), 2);
    assert!(!set.is_empty());
    assert!(set.get("b").is_some() && set.get("z").is_none());
    assert_eq!(format!("{set:?}"), r#"["a", "b"]"#);

    let both = tools![Plain("c")].extend(set.clone());
    assert_eq!(both.names(), ["c", "a", "b"]);
    assert!(tools![].is_empty());

    let collected: ToolSet = set.iter().cloned().collect();
    assert_eq!(collected.names(), ["a", "b"]);
    assert_eq!((&collected).into_iter().count(), 2);
}

/// Middleware: a tool that wraps another and shouts its output.
struct Shout(DynTool);

#[async_trait]
impl Tool for Shout {
    fn spec(&self) -> ToolSpec {
        self.0.spec()
    }
    fn required_state(&self) -> Vec<StateKey> {
        self.0.required_state()
    }
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let mut out = self.0.call(ctx, args).await?;
        out.content = out.content.to_uppercase();
        Ok(out)
    }
}

#[tokio::test]
async fn wrap_applies_middleware_to_every_tool_and_keeps_their_needs() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "greet", json!({"name": "Ada"}))])
        .push_text("done");
    let set = tools![Greet].wrap(|tool| Arc::new(Shout(tool)));
    // The wrapper forwards `required_state`, so `try_build` still checks it.
    assert!(builder(&mock).tools(set.clone()).try_build().is_err());
    let agent = builder(&mock)
        .state(Arc::new(Greeting("Hello")))
        .tools(set)
        .try_build()
        .unwrap();
    let view = run_to_end(&agent).await;
    assert_eq!(tool_results(&view), vec![("HELLO, ADA!".to_owned(), false)]);
}

#[tokio::test]
async fn a_raw_fn_tool_runs_in_an_agent_and_converts_its_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let lookup = FnTool::raw(
        "lookup",
        "Look a key up.",
        json!({"type": "object", "properties": {"key": {"type": "string"}}}),
        move |_ctx, args| {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, SeqCst);
                Ok::<_, ToolError>(Json(json!({"found": args["key"]})))
            }
        },
    );
    assert_eq!(lookup.spec().name, "lookup");
    assert!(format!("{lookup:?}").contains("lookup"));

    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "lookup", json!({"key": "k"}))])
        .push_text("done");
    let agent = builder(&mock).tool(lookup).try_build().unwrap();
    let view = run_to_end(&agent).await;
    assert_eq!(calls.load(SeqCst), 1);
    assert_eq!(
        tool_results(&view),
        vec![(r#"{"found":"k"}"#.to_owned(), false)]
    );
}

#[tokio::test]
async fn a_fn_tool_error_is_a_tool_error() {
    let sink: adam_runtime::DynEventSink = Arc::new(CollectingSink::new());
    let ctx = ToolCtx::detached("ask", "c1", sink);
    let ask = FnTool::raw(
        "ask",
        "Ask.",
        json!({"type": "object"}),
        |_ctx, _args| async { Err::<String, _>(ToolError::needs_input("which one?")) },
    );
    let err = ask.call(&ctx, json!({})).await.unwrap_err();
    assert_eq!(err, ToolError::needs_input("which one?"));
}

#[cfg(feature = "schema")]
mod schema {
    use schemars::JsonSchema;

    use super::*;

    #[derive(Deserialize, JsonSchema)]
    struct EchoArgs {
        /// The text to echo
        text: String,
    }

    #[tokio::test]
    async fn a_typed_fn_tool_has_a_schema_and_never_sees_bad_arguments() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let echo = FnTool::builder("echo")
            .description("Echo the text back.")
            .args::<EchoArgs>()
            .handler(move |_ctx, a| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, SeqCst);
                    Ok::<_, ToolError>(a.text)
                }
            });

        let spec = echo.spec();
        assert_eq!(spec.name, "echo");
        assert_eq!(spec.description, "Echo the text back.");
        assert_eq!(spec.parameters["required"], json!(["text"]));
        assert_eq!(
            spec.parameters["properties"]["text"]["description"],
            "The text to echo"
        );

        let sink: adam_runtime::DynEventSink = Arc::new(CollectingSink::new());
        let ctx = ToolCtx::detached("echo", "c1", sink);
        let ok = echo.call(&ctx, json!({"text": "hi"})).await.unwrap();
        assert_eq!(ok, ToolOutput::text("hi"));
        let refused = echo.call(&ctx, json!({"text": 3})).await.unwrap();
        assert!(refused.is_error);
        assert!(refused.content.starts_with("invalid arguments for `echo`"));
        assert_eq!(
            calls.load(SeqCst),
            1,
            "the closure ran once, for the good call"
        );
    }
}
