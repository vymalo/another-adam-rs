//! `#[tool]` end to end: the generated tools are described to the model, called
//! by it with JSON, and answer through a real `LlmAgent` on the in-memory store.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam::core::{DynStore, MemoryStore, RunId, RunStatus};
use adam::error::{Classify, ErrorClass};
use adam::model::{DynModel, Message, MockModel, ToolCall};
use adam::prelude::*;
use adam::runtime::{CollectingSink, RunView, Runtime};
use adam::{BuildError, Conversation, PendingWait, StateKey, user_message};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

// ------------------------------------------------------------------ tools

/// Shared state.
pub struct Units(&'static str);

/// Get the current weather for a city. Say which city; the answer is
/// in the agent's configured units.
#[tool]
pub async fn get_weather(
    units: State<Units>,
    /// The city, for example "Berlin"
    city: String,
    /// Days of forecast to include
    days: Option<u32>,
) -> Result<String, ToolError> {
    if city.trim().is_empty() {
        return Err(ToolError::Permanent("city is empty".into()));
    }
    Ok(format!(
        "{city}: 18 {} (+{} days)",
        units.0,
        days.unwrap_or_default()
    ))
}

/// Ask the person who gave you the task a question and wait for the answer.
#[tool(asks_user)]
async fn ask_user(
    /// What you need to know
    question: String,
) -> Result<ToolOutput, ToolError> {
    match question.trim() {
        "" => Ok(ToolOutput::error("question is required")),
        q => Err(ToolError::needs_input(q.to_owned())),
    }
}

/// Say who is calling.
#[tool]
async fn whoami(ctx: &ToolCtx) -> String {
    format!("{} / {}", ctx.tool_name(), ctx.call_id())
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// What to look for
    query: String,
    #[serde(default)]
    limit: Option<u8>,
}

/// Search, with an existing arguments struct.
#[tool(name = "web_search", type = Searcher)]
async fn search(#[args] args: SearchArgs) -> Json<Vec<String>> {
    let n = usize::from(args.limit.unwrap_or(2));
    Json((0..n).map(|i| format!("{} #{i}", args.query)).collect())
}

/// Only known fields.
#[tool(strict)]
async fn strictly(
    /// A name
    name: String,
) -> String {
    name
}

/// Renamed and defaulted arguments.
#[tool]
async fn tag(
    /// The label
    #[serde(rename = "label")]
    text: String,
    /// How many times
    #[serde(default = "one")]
    times: u8,
) -> String {
    text.repeat(usize::from(times))
}

fn one() -> u8 {
    1
}

#[derive(Debug, thiserror::Error)]
#[error("upstream is {0:?}")]
struct Upstream(ErrorClass);

impl Classify for Upstream {
    fn class(&self) -> ErrorClass {
        self.0
    }
}

/// Fails as told.
#[tool(classify)]
async fn flaky(
    /// A class: "transient" or "invalid"
    class: String,
) -> Result<String, Upstream> {
    Err(Upstream(if class == "transient" {
        ErrorClass::Transient
    } else {
        ErrorClass::Invalid
    }))
}

/// No arguments at all.
#[tool]
async fn ping() -> &'static str {
    "pong"
}

// ------------------------------------------------------------------ specs

#[test]
fn the_spec_is_what_the_model_reads() {
    let spec = GetWeather.spec();
    assert_eq!(spec.name, "get_weather");
    assert_eq!(
        spec.description,
        "Get the current weather for a city. Say which city; the answer is in the agent's configured units."
    );
    assert_eq!(
        spec.parameters,
        json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "The city, for example \"Berlin\""},
                "days": {
                    "type": ["integer", "null"],
                    "format": "uint32",
                    "minimum": 0,
                    "description": "Days of forecast to include"
                }
            },
            "required": ["city"]
        })
    );
}

#[test]
fn names_types_and_argument_shapes() {
    assert_eq!(AskUser.spec().name, "ask_user");
    assert_eq!(Searcher.spec().name, "web_search");
    assert_eq!(
        Ping.spec().parameters,
        json!({"type": "object", "properties": {}})
    );
    // The schema is computed once and cloned out.
    assert_eq!(Ping.spec(), Ping.spec());

    // `#[args]`: the struct's own schema.
    let search = Searcher.spec();
    assert_eq!(
        search.description,
        "Search, with an existing arguments struct."
    );
    assert_eq!(search.parameters["required"], json!(["query"]));
    assert_eq!(
        search.parameters["properties"]["query"]["description"],
        "What to look for"
    );

    // serde attributes on a parameter shape the schema.
    let tag = Tag.spec();
    assert_eq!(tag.parameters["required"], json!(["label"]));
    assert!(tag.parameters["properties"].get("text").is_none());
    assert_eq!(
        tag.parameters["properties"]["times"]["description"],
        "How many times"
    );

    // `strict` closes the object.
    assert_eq!(
        Strictly.spec().parameters["additionalProperties"],
        json!(false)
    );
    assert!(
        AskUser
            .spec()
            .parameters
            .get("additionalProperties")
            .is_none()
    );
}

#[test]
fn state_is_declared_and_only_state() {
    assert_eq!(GetWeather.required_state(), vec![StateKey::of::<Units>()]);
    assert!(AskUser.required_state().is_empty());
    assert!(Whoami.required_state().is_empty());
}

#[test]
fn the_function_is_kept_and_the_tool_is_a_copyable_unit() {
    let tool = GetWeather;
    let copy = tool;
    assert_eq!(format!("{tool:?}{copy:?}"), "GetWeatherGetWeather");
    let _: GetWeather = Default::default();
}

// -------------------------------------------------------------- unit level

fn detached(name: &str) -> ToolCtx {
    let sink: adam::runtime::DynEventSink = Arc::new(CollectingSink::new());
    ToolCtx::detached(name, "call-1", sink)
}

#[tokio::test]
async fn a_tool_is_called_with_json_and_answers() {
    let ctx = detached("get_weather").with_state(Arc::new(Units("C")));
    let out = GetWeather
        .call(&ctx, json!({"city": "Berlin", "days": 2}))
        .await
        .unwrap();
    assert_eq!(out, ToolOutput::text("Berlin: 18 C (+2 days)"));
    // The optional argument may be missing or null; unknown fields are ignored.
    let out = GetWeather
        .call(&ctx, json!({"city": "Oslo", "days": null, "extra": 1}))
        .await
        .unwrap();
    assert_eq!(out, ToolOutput::text("Oslo: 18 C (+0 days)"));
    // The function's own error comes through as the tool's.
    let err = GetWeather
        .call(&ctx, json!({"city": " "}))
        .await
        .unwrap_err();
    assert_eq!(err, ToolError::Permanent("city is empty".into()));
}

#[tokio::test]
async fn bad_arguments_are_an_error_output_for_the_model() {
    let ctx = detached("get_weather").with_state(Arc::new(Units("C")));
    let out = GetWeather.call(&ctx, json!({"town": "x"})).await.unwrap();
    assert!(out.is_error);
    assert_eq!(
        out.content,
        "invalid arguments for `get_weather`: missing field `city`"
    );
    let out = GetWeather.call(&ctx, json!({"city": 3})).await.unwrap();
    assert!(
        out.is_error && out.content.contains("invalid type"),
        "{out:?}"
    );
    // The `null` a model sends for "no arguments".
    assert_eq!(
        Ping.call(&ctx, Value::Null).await.unwrap(),
        ToolOutput::text("pong")
    );
    let out = GetWeather.call(&ctx, Value::Null).await.unwrap();
    assert!(out.is_error);
    // Bad input never reaches the function, so its state is not even asked for.
    let no_state = detached("get_weather");
    let out = GetWeather.call(&no_state, json!({})).await.unwrap();
    assert!(out.content.contains("missing field `city`"), "{out:?}");
}

#[tokio::test]
async fn missing_state_is_a_permanent_error_naming_the_type() {
    let err = GetWeather
        .call(&detached("get_weather"), json!({"city": "Bonn"}))
        .await
        .unwrap_err();
    match err {
        ToolError::Permanent(message) => {
            assert!(message.contains("missing shared state"), "{message}");
            assert!(message.contains("Units"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn context_arguments_ctx_and_needs_input() {
    let out = Whoami.call(&detached("whoami"), json!({})).await.unwrap();
    assert_eq!(out, ToolOutput::text("whoami / call-1"));

    // Only the tool that says so declares that it asks the user.
    assert!(AskUser.asks_user());
    assert!(!Whoami.asks_user());

    let err = AskUser
        .call(&detached("ask_user"), json!({"question": " why? "}))
        .await
        .unwrap_err();
    assert_eq!(err, ToolError::needs_input("why?"));
    let out = AskUser
        .call(&detached("ask_user"), json!({"question": "  "}))
        .await
        .unwrap();
    assert_eq!(out, ToolOutput::error("question is required"));
}

#[tokio::test]
async fn args_structs_serde_attributes_strict_and_json_output() {
    let c = detached("web_search");
    let out = Searcher
        .call(&c, json!({"query": "rust", "limit": 2}))
        .await
        .unwrap();
    assert_eq!(out, ToolOutput::text(r#"["rust #0","rust #1"]"#));
    let out = Searcher.call(&c, json!({"limit": 2})).await.unwrap();
    assert!(
        out.is_error && out.content.contains("web_search"),
        "{out:?}"
    );

    let out = Tag
        .call(&c, json!({"label": "ab", "times": 2}))
        .await
        .unwrap();
    assert_eq!(out, ToolOutput::text("abab"));
    let out = Tag.call(&c, json!({"label": "ab"})).await.unwrap();
    assert_eq!(out, ToolOutput::text("ab"));
    let out = Tag.call(&c, json!({"text": "ab"})).await.unwrap();
    assert!(out.is_error, "the parameter is called `label` to the model");

    let out = Strictly.call(&c, json!({"name": "n"})).await.unwrap();
    assert_eq!(out, ToolOutput::text("n"));
    let out = Strictly
        .call(&c, json!({"name": "n", "more": 1}))
        .await
        .unwrap();
    assert!(
        out.is_error && out.content.contains("unknown field `more`"),
        "{out:?}"
    );
}

#[tokio::test]
async fn classify_splits_retryable_from_permanent() {
    let c = detached("flaky");
    assert_eq!(
        Flaky
            .call(&c, json!({"class": "transient"}))
            .await
            .unwrap_err(),
        ToolError::Transient("upstream is Transient".into())
    );
    assert_eq!(
        Flaky
            .call(&c, json!({"class": "invalid"}))
            .await
            .unwrap_err(),
        ToolError::Permanent("upstream is Invalid".into())
    );
}

// ------------------------------------------------------- through a real run

fn call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn builder(mock: &Arc<MockModel>) -> adam::LlmAgentBuilder {
    let model: DynModel = mock.clone();
    LlmAgent::builder("generated", model, "test-model")
}

/// Run `agent` until the run stops moving: done, failed, or parked on a question.
async fn run_until_settled(agent: &LlmAgent) -> RunView {
    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = Runtime::builder(store)
        .agent(agent.clone())
        .event_sink(CollectingSink::new())
        .worker_id("w")
        .poll_interval(Duration::from_millis(20))
        .build();
    let run: RunId = rt
        .start("generated", user_message("go"), None)
        .await
        .unwrap();
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
        let conversation: Conversation = serde_json::from_value(view.state.clone()).unwrap();
        let parked = conversation.pending_wait.is_some();
        if matches!(view.status, RunStatus::Done | RunStatus::Failed) || parked {
            break view;
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let _ = stop.send(());
    worker.await.unwrap().unwrap();
    view
}

fn conversation(view: &RunView) -> Conversation {
    serde_json::from_value(view.state.clone()).unwrap()
}

fn tool_results(view: &RunView) -> Vec<(String, bool)> {
    conversation(view)
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
async fn the_model_calls_generated_tools_and_reads_their_output() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![
        call("c1", "get_weather", json!({"city": "Berlin", "days": 1})),
        call("c2", "whoami", json!({})),
        call("c3", "get_weather", json!({"town": "Berlin"})),
        call("c4", "web_search", json!({"query": "q", "limit": 1})),
    ])
    .push_text("done");
    let agent = builder(&mock)
        .state(Arc::new(Units("C")))
        .tools(tools![GetWeather, Whoami, Searcher])
        .try_build()
        .unwrap();
    let view = run_until_settled(&agent).await;
    assert_eq!(view.status, RunStatus::Done);
    let results = tool_results(&view);
    assert_eq!(results.len(), 4, "{results:?}");
    assert_eq!(results[0], ("Berlin: 18 C (+1 days)".to_owned(), false));
    assert_eq!(results[1], ("whoami / c2".to_owned(), false));
    assert!(
        results[2].1 && results[2].0.contains("missing field `city`"),
        "{results:?}"
    );
    assert_eq!(results[3], (r#"["q #0"]"#.to_owned(), false));

    // The model was shown exactly the generated specs.
    let request = mock.requests().into_iter().next().unwrap();
    let shown: Vec<String> = request.tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(shown, ["get_weather", "whoami", "web_search"]);
    assert_eq!(request.tools[0], GetWeather.spec());
}

#[tokio::test]
async fn a_tool_that_needs_input_parks_the_run_with_the_question() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call(
        "c1",
        "ask_user",
        json!({"question": "Which branch?"}),
    )]);
    let agent = builder(&mock).tools(tools![AskUser]).try_build().unwrap();
    let view = run_until_settled(&agent).await;
    let pending = conversation(&view)
        .pending_wait
        .expect("parked on the question");
    assert!(
        matches!(&pending, PendingWait::Question(q) if q.question == "Which branch?"),
        "{pending:?}"
    );
}

#[test]
fn try_build_reports_the_state_a_generated_tool_declares() {
    let mock = Arc::new(MockModel::new());
    let err = builder(&mock)
        .tools(tools![GetWeather])
        .try_build()
        .unwrap_err();
    match err {
        BuildError::MissingState { tool, state } => {
            assert_eq!(tool, "get_weather");
            assert!(state.ends_with("Units"), "{state}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        builder(&mock)
            .state(Arc::new(Units("F")))
            .tools(tools![GetWeather])
            .try_build()
            .is_ok()
    );
}

#[test]
fn duplicate_generated_tools_are_rejected_at_build() {
    let mock = Arc::new(MockModel::new());
    let err = builder(&mock)
        .tools(tools![Ping, Whoami, Ping])
        .try_build()
        .unwrap_err();
    assert_eq!(
        err,
        BuildError::DuplicateTool {
            name: "ping".into()
        }
    );
}

#[tokio::test]
async fn a_permanent_tool_error_is_the_models_to_read() {
    let mock = Arc::new(MockModel::new());
    mock.push_tool_calls(vec![call("c1", "flaky", json!({"class": "invalid"}))])
        .push_text("done");
    let agent = builder(&mock).tools(tools![Flaky]).try_build().unwrap();
    let view = run_until_settled(&agent).await;
    assert_eq!(
        view.status,
        RunStatus::Done,
        "a permanent error is the model's to read"
    );
    let results = tool_results(&view);
    assert_eq!(results.len(), 1);
    assert!(
        results[0].1 && results[0].0.contains("upstream is Invalid"),
        "{results:?}"
    );
}
