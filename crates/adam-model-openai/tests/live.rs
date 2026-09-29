//! Optional test against a real OpenAI-compatible endpoint.
//!
//! Runs only when `ADAM_TEST_OPENAI_BASE_URL` and `ADAM_TEST_OPENAI_API_KEY`
//! are set (and optionally `ADAM_TEST_OPENAI_MODEL`, default `gpt-4o-mini`);
//! otherwise it passes without doing anything.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::time::Duration;

use adam_model::{
    FinishReason, Message, ModelClient, ModelDelta, ModelRequest, ToolChoice, ToolSpec,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use futures::TryStreamExt;
use secrecy::SecretString;
use serde_json::json;

fn live_client() -> Option<(OpenAiCompatible, String)> {
    let base_url = std::env::var("ADAM_TEST_OPENAI_BASE_URL").ok()?;
    let key = std::env::var("ADAM_TEST_OPENAI_API_KEY").ok()?;
    let model = std::env::var("ADAM_TEST_OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let mut config = OpenAiConfig::new(base_url, SecretString::from(key));
    config.timeout = Duration::from_secs(60);
    Some((OpenAiCompatible::new(config).expect("client"), model))
}

fn weather_request(model: &str) -> ModelRequest {
    let mut req = ModelRequest::new(model);
    req.messages.push(Message::user_text(
        "What is the weather in Paris? Use the tool.",
    ));
    req.tools = vec![ToolSpec {
        name: "get_weather".into(),
        description: "Get the current weather for a city".into(),
        parameters: json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }];
    req.tool_choice = ToolChoice::Required;
    req.max_output_tokens = Some(200);
    req
}

#[tokio::test]
async fn live_complete_and_stream_with_tools() {
    let Some((client, model)) = live_client() else {
        eprintln!("skipping: ADAM_TEST_OPENAI_BASE_URL / ADAM_TEST_OPENAI_API_KEY not set");
        return;
    };

    let resp = client
        .complete(weather_request(&model))
        .await
        .expect("complete");
    assert_eq!(resp.finish, FinishReason::ToolCalls);
    assert_eq!(resp.message.tool_calls()[0].name, "get_weather");
    assert!(resp.message.tool_calls()[0].arguments.get("city").is_some());

    let deltas: Vec<ModelDelta> = client
        .stream(weather_request(&model))
        .await
        .expect("stream")
        .try_collect()
        .await
        .expect("deltas");
    let Some(ModelDelta::Finished(resp)) = deltas.last() else {
        panic!("stream must end with Finished: {deltas:?}")
    };
    assert_eq!(resp.finish, FinishReason::ToolCalls);
    assert_eq!(resp.message.tool_calls()[0].name, "get_weather");
}
