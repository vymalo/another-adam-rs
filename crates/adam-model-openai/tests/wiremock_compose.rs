//! `OpenAiCompatible` against the `mock-openai` WireMock of `compose.yaml`.
//!
//! Runs only when `ADAM_TEST_MOCK_OPENAI_URL` is set (the mock's root, for
//! example `http://127.0.0.1:8081`, without `/v1`); otherwise every test
//! passes without doing anything. Start the mock with
//! `docker compose up -d --wait mock-openai`; the scenario switches these
//! tests use are documented in the README ("Local development").
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::BTreeMap;
use std::time::Duration;

use adam_model::{
    FinishReason, Message, ModelClient, ModelDelta, ModelError, ModelRequest, ToolCall, ToolChoice,
    ToolSpec, Usage,
};
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use futures::TryStreamExt;
use secrecy::SecretString;
use serde_json::json;

fn mock_url() -> Option<String> {
    std::env::var("ADAM_TEST_MOCK_OPENAI_URL")
        .ok()
        .map(|u| u.trim_end_matches('/').to_owned())
        .filter(|u| !u.is_empty())
}

/// A client for the mock, with an optional `X-Mock-Scenario` header.
fn client(base_url: &str, scenario: Option<&str>) -> OpenAiCompatible {
    let mut config = OpenAiConfig::new(base_url, SecretString::from("mock-api-key"));
    config.timeout = Duration::from_secs(20);
    config.extra_headers = BTreeMap::new();
    if let Some(scenario) = scenario {
        config
            .extra_headers
            .insert("X-Mock-Scenario".into(), scenario.into());
    }
    OpenAiCompatible::new(config).expect("client")
}

fn request(prompt: &str) -> ModelRequest {
    let mut req = ModelRequest::new("mock-model");
    req.system = Some("be brief".into());
    req.messages.push(Message::user_text(prompt));
    req.max_output_tokens = Some(64);
    req
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: "get_weather".into(),
        description: "Get the current weather for a city".into(),
        parameters: json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }),
    }
}

fn tool_request(prompt: &str) -> ModelRequest {
    let mut req = request(prompt);
    req.tools = vec![weather_tool()];
    req.tool_choice = ToolChoice::Auto;
    req
}

async fn stream_of(client: &OpenAiCompatible, req: ModelRequest) -> Vec<ModelDelta> {
    client
        .stream(req)
        .await
        .expect("stream")
        .try_collect()
        .await
        .expect("deltas")
}

#[tokio::test]
async fn text_answer_complete_and_stream_on_both_paths() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    // The mock serves `/v1/chat/completions` and `/chat/completions`.
    for base in [format!("{root}/v1"), root.clone()] {
        let client = client(&base, None);

        let resp = client.complete(request("hi")).await.expect("complete");
        assert_eq!(resp.finish, FinishReason::Stop, "{base}");
        assert!(resp.message.tool_calls().is_empty());
        assert!(!resp.message.text().is_empty());
        assert_eq!(
            resp.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 14
            }
        );

        let deltas = stream_of(&client, request("hi")).await;
        let text: String = deltas
            .iter()
            .filter_map(|d| match d {
                ModelDelta::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("Hello from the mock model."), "{text}");
        let Some(ModelDelta::Finished(done)) = deltas.last() else {
            panic!("stream must end with Finished: {deltas:?}")
        };
        assert_eq!(done.finish, FinishReason::Stop);
        assert_eq!(done.message.text(), text);
        // The usage chunk (empty `choices`) that follows the finish chunk.
        assert_eq!(done.usage.output_tokens, 14);
    }
}

#[tokio::test]
async fn tool_call_by_keyword_and_by_header_then_final_answer() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let base = format!("{root}/v1");
    let by_keyword = client(&base, None);
    let by_header = client(&base, Some("tool-call"));

    for (client, prompt) in [
        (&by_keyword, "[mock:tool-call] weather in Paris?"),
        (&by_header, "weather in Paris?"),
    ] {
        // Non-streaming: the first declared tool is called with `{}`.
        let resp = client
            .complete(tool_request(prompt))
            .await
            .expect("complete");
        assert_eq!(resp.finish, FinishReason::ToolCalls);
        let calls = resp.message.tool_calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, json!({}));
        assert!(!calls[0].id.is_empty());

        // Streaming: the same call, assembled from argument fragments.
        let deltas = stream_of(client, tool_request(prompt)).await;
        assert!(
            deltas.iter().any(|d| matches!(
                d,
                ModelDelta::ToolCallStarted { name, .. } if name == "get_weather"
            )),
            "{deltas:?}"
        );
        let Some(ModelDelta::Finished(done)) = deltas.last() else {
            panic!("stream must end with Finished: {deltas:?}")
        };
        assert_eq!(done.finish, FinishReason::ToolCalls);
        assert_eq!(done.message.tool_calls()[0].name, "get_weather");
        assert_eq!(done.message.tool_calls()[0].arguments, json!({}));
        assert_eq!(done.usage.input_tokens, 20);

        // Once the history holds a tool result, the mock answers in text even
        // though the keyword / header is still there: the agent loop ends.
        let mut follow_up = tool_request(prompt);
        follow_up.messages.push(Message::Assistant {
            content: vec![],
            tool_calls: vec![ToolCall {
                id: calls[0].id.clone(),
                name: "get_weather".into(),
                arguments: json!({"city": "Paris"}),
            }],
        });
        follow_up
            .messages
            .push(Message::tool_result(calls[0].id.clone(), "sunny, 21C"));
        let resp = client.complete(follow_up.clone()).await.expect("final");
        assert_eq!(resp.finish, FinishReason::Stop);
        assert!(resp.message.tool_calls().is_empty());
        let deltas = stream_of(client, follow_up).await;
        let Some(ModelDelta::Finished(done)) = deltas.last() else {
            panic!("stream must end with Finished: {deltas:?}")
        };
        assert_eq!(done.finish, FinishReason::Stop);
    }
}

#[tokio::test]
async fn error_scenarios_map_onto_model_errors() {
    let Some(root) = mock_url() else {
        eprintln!("skipping: ADAM_TEST_MOCK_OPENAI_URL not set");
        return;
    };
    let base = format!("{root}/v1");
    type Check = fn(&ModelError) -> bool;
    let scenarios: [(&str, Check); 4] = [
        ("rate-limit", |e| {
            matches!(
                e,
                ModelError::RateLimited {
                    retry_after: Some(d)
                } if *d == Duration::from_secs(2)
            )
        }),
        ("server-error", |e| {
            matches!(e, ModelError::Transient { .. })
        }),
        ("unauthorized", |e| matches!(e, ModelError::Auth(_))),
        ("context-length", |e| {
            matches!(e, ModelError::ContextLength(_))
        }),
    ];

    for (scenario, is_expected) in scenarios {
        // Selected by the header ...
        let by_header = client(&base, Some(scenario));
        let err = by_header.complete(request("hi")).await.expect_err(scenario);
        assert!(is_expected(&err), "{scenario} (header): {err:?}");
        let Err(err) = by_header.stream(request("hi")).await else {
            panic!("{scenario} (header): the stream must fail before it starts")
        };
        assert!(is_expected(&err), "{scenario} (header, stream): {err:?}");

        // ... and by the keyword in the prompt.
        let plain = client(&base, None);
        let prompt = format!("please fail with [mock:{scenario}]");
        let err = plain.complete(request(&prompt)).await.expect_err(scenario);
        assert!(is_expected(&err), "{scenario} (keyword): {err:?}");
        let Err(err) = plain.stream(request(&prompt)).await else {
            panic!("{scenario} (keyword): the stream must fail before it starts")
        };
        assert!(is_expected(&err), "{scenario} (keyword, stream): {err:?}");
    }
}
