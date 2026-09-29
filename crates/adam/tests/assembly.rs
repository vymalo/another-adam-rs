//! The facade puts the authoring layer together: `#[tool]` functions, an agent manifest and
//! `AgentDef` from the prelude, run through a real `Runtime`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Arc;
use std::time::{Duration, Instant};

use adam::agent_fs::{AgentFrontmatter, AgentManifest, Instructions, ToolList};
use adam::core::{DynStore, MemoryStore, RunStatus};
use adam::model::{MockModel, ToolCall};
use adam::prelude::*;
use adam::runtime::Runtime;
use adam::{Conversation, user_message};
use serde_json::json;

/// Shared state.
pub struct Units(&'static str);

/// Get the current weather for a city.
#[tool]
pub async fn get_weather(
    units: State<Units>,
    /// The city, for example "Berlin"
    city: String,
) -> Result<String, ToolError> {
    Ok(format!("{city}: 18 {}", units.0))
}

/// Say hello.
#[tool]
pub async fn greet() -> String {
    "hello".to_owned()
}

/// A manifest as `adam-agent-fs` would produce it: a prompt with a var and a `tools:` list.
fn manifest(tools: &[&str]) -> AgentManifest {
    let mut frontmatter = AgentFrontmatter::default();
    frontmatter.tools = Some(ToolList::Named(
        tools.iter().map(|t| (*t).to_owned()).collect(),
    ));
    frontmatter.vars = [("city".to_owned(), "Berlin".to_owned())].into();
    AgentManifest {
        name: "weather".into(),
        path: "agent/instructions.md".into(),
        frontmatter,
        instructions: Instructions {
            body: "Report the weather in {{city}}.".into(),
            parts: vec![],
        },
        skills: vec![],
        subagents: vec![],
        mcp: None,
        schedules: vec![],
    }
}

#[test]
fn the_prelude_binds_a_manifest_to_macro_tools_and_suggests_the_right_name() {
    // The macro named the tool `get_weather`; the file says `get_wether`.
    let error = AgentDef::from_manifest(manifest(&["get_wether"]))
        .unwrap()
        .bind(tools![GetWeather, Greet])
        .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("did you mean `get_weather`?"), "{text}");
    assert!(
        text.starts_with("agent `weather` (agent/instructions.md)"),
        "{text}"
    );
    let _: AssemblyError = error;
}

#[tokio::test]
async fn a_bound_agent_runs_a_macro_tool_through_the_runtime() {
    let model = Arc::new(MockModel::new());
    model.push_tool_calls(vec![ToolCall {
        id: "c1".into(),
        name: "get_weather".into(),
        arguments: json!({"city": "Paris"}),
    }]);
    model.push_text("It is 18 C.");

    let assembly = AgentDef::from_manifest(manifest(&["get_weather"]))
        .unwrap()
        .var("city", "Paris")
        .bind(tools![GetWeather, Greet])
        .unwrap()
        .state(Arc::new(Units("C")))
        .model(model.clone(), "weather-model")
        .unwrap();
    assert_eq!(assembly.info()[0].prompt, "Report the weather in Paris.");
    assert_eq!(assembly.info()[0].tools, ["get_weather"]);

    // A tool that needs state nobody gave fails at startup, not in the middle of a run.
    let missing = AgentDef::from_manifest(manifest(&["get_weather"]))
        .unwrap()
        .bind(tools![GetWeather])
        .unwrap()
        .model(model.clone(), "weather-model")
        .unwrap_err();
    assert!(matches!(missing, AssemblyError::Build { .. }), "{missing}");

    let store: DynStore = Arc::new(MemoryStore::new());
    let rt = assembly
        .register(Runtime::builder(store))
        .poll_interval(Duration::from_millis(20))
        .build();
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
    let run = rt
        .start("weather", user_message("weather?"), None)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let view = loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.status == RunStatus::Done {
            break view;
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert_eq!(view.output.as_ref().unwrap()["text"], "It is 18 C.");
    let conversation: Conversation = serde_json::from_value(view.state).unwrap();
    assert!(
        conversation
            .messages
            .iter()
            .any(|m| m.text() == "Paris: 18 C")
    );
    let _ = stop.send(());
    worker.await.unwrap().unwrap();

    let request = &model.requests()[0];
    assert_eq!(request.model, "weather-model");
    assert_eq!(
        request.system.as_deref(),
        Some("Report the weather in Paris.")
    );
}
