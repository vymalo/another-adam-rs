//! HTTP-level tests against a local mock server. No network needed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::time::Duration;

use adam_model::{
    Classify, ErrorClass, FinishReason, Message, ModelClient, ModelDelta, ModelError, ModelRequest,
    ToolChoice, ToolSpec, Usage,
};
use adam_model_openai::{
    MaxTokensField, OpenAiCompatible, OpenAiConfig, OpenAiConfigError, ReasoningField,
};
use futures::StreamExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-test-key-123";

fn client_with(server: &MockServer, timeout: Duration) -> OpenAiCompatible {
    let mut config = OpenAiConfig::new(format!("{}/v1", server.uri()), SecretString::from(KEY));
    config.timeout = timeout;
    config
        .extra_headers
        .insert("x-tenant".into(), "acme".into());
    OpenAiCompatible::new(config).expect("client")
}

fn client(server: &MockServer) -> OpenAiCompatible {
    client_with(server, Duration::from_secs(10))
}

fn request() -> ModelRequest {
    let mut req = ModelRequest::new("gw-model");
    req.system = Some("be brief".into());
    req.messages.push(Message::user_text("weather in Paris?"));
    req.tools = vec![ToolSpec {
        name: "weather".into(),
        description: "Get the weather".into(),
        parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    }];
    req.tool_choice = ToolChoice::Auto;
    req
}

async fn mount(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(response)
        .mount(server)
        .await;
}

fn sse(events: &[&str]) -> ResponseTemplate {
    let mut body = String::new();
    for e in events {
        body.push_str("data: ");
        body.push_str(e);
        body.push_str("\n\n");
    }
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

async fn sent_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.expect("recording");
    assert_eq!(requests.len(), 1, "exactly one request, no retries");
    requests[0].body_json().expect("json body")
}

async fn collect(
    stream: impl futures::Stream<Item = Result<ModelDelta, ModelError>>,
) -> Vec<Result<ModelDelta, ModelError>> {
    stream.collect().await
}

// ------------------------------------------------------------ non-streaming --

#[tokio::test]
async fn complete_sends_the_documented_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(header("x-tenant", "acme"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "Sunny."}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 3, "total_tokens": 23}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut req = request();
    req.max_output_tokens = Some(64);
    req.temperature = Some(0.25);
    req.metadata.insert("run_id".into(), "r-1".into());
    let resp = client(&server).complete(req).await.unwrap();

    assert_eq!(resp.message.text(), "Sunny.");
    assert_eq!(resp.finish, FinishReason::Stop);
    assert_eq!(resp.usage, Usage::new(20, 3));

    let body = sent_body(&server).await;
    assert_eq!(body["model"], "gw-model");
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "be brief"})
    );
    assert_eq!(
        body["messages"][1],
        json!({"role": "user", "content": "weather in Paris?"})
    );
    assert_eq!(body["tools"][0]["function"]["name"], "weather");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(body["temperature"], 0.25);
    assert_eq!(body["metadata"], json!({"run_id": "r-1"}));
    assert!(body.get("stream").is_none());
}

#[tokio::test]
async fn max_completion_tokens_can_be_selected() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;
    let mut req = request();
    req.max_output_tokens = Some(7);
    client(&server)
        .with_max_tokens_field(MaxTokensField::MaxCompletionTokens)
        .complete(req)
        .await
        .unwrap();
    let body = sent_body(&server).await;
    assert_eq!(body["max_completion_tokens"], 7);
    assert!(body.get("max_tokens").is_none());
}

#[tokio::test]
async fn complete_returns_tool_calls_with_parsed_arguments() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_9", "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}
                }]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 6}
        })),
    )
    .await;

    let resp = client(&server).complete(request()).await.unwrap();
    assert_eq!(resp.finish, FinishReason::ToolCalls);
    let calls = resp.message.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_9");
    assert_eq!(calls[0].name, "weather");
    assert_eq!(calls[0].arguments, json!({"city": "Paris"}));
}

#[tokio::test]
async fn tool_results_round_trip_into_the_next_request() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "18C"}, "finish_reason": "stop"}]
        })),
    )
    .await;
    let mut req = request();
    req.messages.push(Message::Assistant {
        content: vec![],
        tool_calls: vec![adam_model::ToolCall {
            id: "call_9".into(),
            name: "weather".into(),
            arguments: json!({"city": "Paris"}),
            ..Default::default()
        }],
        reasoning: None,
        provider: None,
    });
    req.messages.push(Message::tool_result("call_9", "18C"));
    client(&server).complete(req).await.unwrap();

    let body = sent_body(&server).await;
    assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_9");
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Paris\"}"
    );
    assert_eq!(
        body["messages"][3],
        json!({"role": "tool", "tool_call_id": "call_9", "content": "18C"})
    );
}

/// The bench's repro (adam-rs#322): Gemini 3 attaches `extra_content.google.thought_signature` to
/// a tool call and refuses the next request with a 400 when it is missing. The member must go back
/// value for value.
#[tokio::test]
async fn a_tool_calls_extra_content_survives_the_next_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_g", "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"},
                    "extra_content": {"google": {"thought_signature": "sig-abc-123"}}
                }]},
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "18C"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let first = client.complete(req.clone()).await.unwrap();
    assert_eq!(first.finish, FinishReason::ToolCalls);

    req.messages.push(first.message);
    req.messages.push(Message::tool_result("call_g", "18C"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    assert_eq!(requests.len(), 2, "two calls");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["extra_content"],
        json!({"google": {"thought_signature": "sig-abc-123"}}),
        "the member the provider attached must go back unchanged: {body}"
    );
}

/// The message-level member of a plain answer is the same thing, for a text turn.
#[tokio::test]
async fn a_message_level_extra_content_survives_the_next_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Paris is sunny.",
                    "extra_content": {"google": {"thought_signature": "msg-sig-456"}}
                },
                "finish_reason": "stop"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let first = client.complete(req.clone()).await.unwrap();
    assert_eq!(first.message.text(), "Paris is sunny.");
    req.messages.push(first.message);
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(
        body["messages"][2]["extra_content"],
        json!({"google": {"thought_signature": "msg-sig-456"}}),
        "{body}"
    );
}

/// A reasoning block (an object, not text) is carried raw and echoed in place of the text when the
/// client is set to echo reasoning.
#[tokio::test]
async fn a_reasoning_block_is_echoed_raw_by_a_client_that_echoes_reasoning() {
    let server = MockServer::start().await;
    let block = json!({"blocks": [{"type": "text", "text": "think"}]});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "done", "reasoning": block},
                "finish_reason": "stop"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server).with_echo_reasoning(Some(ReasoningField::Reasoning));
    let mut req = request();
    let first = client.complete(req.clone()).await.unwrap();
    // A block is not reasoning text: it is not in `Message::reasoning`, only in the carrier.
    assert_eq!(first.reasoning, None);
    assert_eq!(first.message.reasoning(), None);
    req.messages.push(first.message);
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(body["messages"][2]["reasoning"], block, "{body}");
}

#[tokio::test]
async fn a_reasoning_block_is_not_echoed_by_a_client_that_does_not() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "done",
                    "reasoning": {"blocks": [{"type": "text", "text": "think"}]}},
                "finish_reason": "stop"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let first = client.complete(req.clone()).await.unwrap();
    assert!(
        matches!(
            &first.message,
            Message::Assistant {
                provider: None,
                reasoning: None,
                ..
            }
        ),
        "the block is not stored without echo: {:?}",
        first.message
    );
    req.messages.push(first.message);
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert!(body["messages"][2].get("reasoning").is_none(), "{body}");
}

/// A block-only response, with echo off: nothing of it is stored (ADR 0020), so the history is
/// the bytes it always was.
#[tokio::test]
async fn a_reasoning_block_alone_is_not_stored_without_echo() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "done",
                    "reasoning": {"blocks": [{"type": "text", "text": "think"}]}},
                "finish_reason": "stop"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let first = client.complete(req.clone()).await.unwrap();
    assert!(
        matches!(
            &first.message,
            Message::Assistant {
                provider: None,
                reasoning: None,
                ..
            }
        ),
        "no carrier is stored for a block with echo off: {:?}",
        first.message
    );
    req.messages.push(first.message);
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert!(body["messages"][2].get("reasoning").is_none(), "{body}");
}

/// With echo off, a block is cleared but the opaque members stay: they are signatures, not
/// reasoning.
#[tokio::test]
async fn a_reasoning_block_is_cleared_but_members_stay_without_echo() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "done",
                    "reasoning": {"blocks": [{"type": "text", "text": "think"}]},
                    "extra_content": {"google": {"thought_signature": "kept-sig"}}},
                "finish_reason": "stop"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let first = client.complete(req.clone()).await.unwrap();
    let Message::Assistant {
        provider: Some(provider),
        ..
    } = &first.message
    else {
        panic!("the members keep the carrier: {:?}", first.message)
    };
    assert_eq!(provider.reasoning, None, "the block is not stored");
    assert_eq!(
        provider.members["extra_content"],
        json!({"google": {"thought_signature": "kept-sig"}})
    );
    req.messages.push(first.message);
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(
        body["messages"][2]["extra_content"],
        json!({"google": {"thought_signature": "kept-sig"}}),
        "{body}"
    );
    assert!(body["messages"][2].get("reasoning").is_none(), "{body}");
}

/// A streamed delta's opaque member on a tool call reaches the finished call and the next request.
#[tokio::test]
async fn streamed_tool_call_extra_content_survives_the_next_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_s","type":"function","extra_content":{"google":{"thought_signature":"stream-sig"}},"function":{"name":"weather","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"Paris\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "18C"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let stream = client.stream(req.clone()).await.unwrap();
    let deltas: Vec<ModelDelta> = collect(stream)
        .await
        .into_iter()
        .map(|item| item.unwrap())
        .collect();
    let Some(ModelDelta::Finished(response)) = deltas.last() else {
        panic!("{deltas:?}")
    };
    assert_eq!(
        response.message.tool_calls()[0].opaque,
        Some(std::collections::BTreeMap::from([(
            "extra_content".into(),
            json!({"google": {"thought_signature": "stream-sig"}})
        )])),
        "the finished call keeps the member"
    );

    req.messages.push(response.message.clone());
    req.messages.push(Message::tool_result("call_s", "18C"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["extra_content"],
        json!({"google": {"thought_signature": "stream-sig"}}),
        "{body}"
    );
}

/// A final chunk shaped as a whole message instead of a delta: its opaque members are kept. No
/// provider we test sends one; the path is defensive and this is its proof.
#[tokio::test]
async fn a_message_shaped_chunk_keeps_its_opaque_members() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}]}"#,
            r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"hi","extra_content":{"google":{"thought_signature":"final-sig"}}},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let stream = client.stream(req.clone()).await.unwrap();
    let deltas: Vec<ModelDelta> = collect(stream)
        .await
        .into_iter()
        .map(|item| item.unwrap())
        .collect();
    let Some(ModelDelta::Finished(response)) = deltas.last() else {
        panic!("{deltas:?}")
    };
    assert_eq!(response.message.text(), "hi");
    req.messages.push(response.message.clone());
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(
        body["messages"][2]["extra_content"],
        json!({"google": {"thought_signature": "final-sig"}}),
        "{body}"
    );
}

/// A delta can carry a message-level member: it is kept and sent back, like a `message`-shaped
/// chunk's.
#[tokio::test]
async fn a_delta_message_level_extra_content_survives_the_next_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"hi","extra_content":{"google":{"thought_signature":"delta-sig"}}}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let stream = client.stream(req.clone()).await.unwrap();
    let deltas: Vec<ModelDelta> = collect(stream)
        .await
        .into_iter()
        .map(|item| item.unwrap())
        .collect();
    let Some(ModelDelta::Finished(response)) = deltas.last() else {
        panic!("{deltas:?}")
    };
    assert_eq!(response.message.text(), "hi");
    req.messages.push(response.message.clone());
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(
        body["messages"][2]["extra_content"],
        json!({"google": {"thought_signature": "delta-sig"}}),
        "{body}"
    );
}

/// Output-only members (`refusal`, `audio`, `function_call`, `annotations`) are never opaque: a
/// streamed turn that carries one stores no carrier, and its next request is byte-identical.
#[tokio::test]
async fn an_output_only_delta_member_is_not_carried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"hi","refusal":null,"audio":null,"function_call":null,"annotations":[]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let stream = client.stream(req.clone()).await.unwrap();
    let deltas: Vec<ModelDelta> = collect(stream)
        .await
        .into_iter()
        .map(|item| item.unwrap())
        .collect();
    let Some(ModelDelta::Finished(response)) = deltas.last() else {
        panic!("{deltas:?}")
    };
    assert!(
        matches!(&response.message, Message::Assistant { provider: None, .. }),
        "an output-only member is no carrier: {:?}",
        response.message
    );
    req.messages.push(response.message.clone());
    req.messages.push(Message::user_text("thanks"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(
        body["messages"][2],
        json!({"role": "assistant", "content": "hi"}),
        "no output-only member goes back: {body}"
    );
}

/// The stamp is the request's own alias, not a constant: a first call under an alias other than
/// the fixture's must still match on the second call.
#[tokio::test]
async fn the_client_stamps_the_requests_own_model_alias() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_a", "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"},
                    "extra_content": {"google": {"thought_signature": "alias-sig"}}
                }]},
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "18C"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    req.model = "model-a".into();
    let first = client.complete(req.clone()).await.unwrap();
    let Message::Assistant {
        provider: Some(provider),
        ..
    } = &first.message
    else {
        panic!("a stamped assistant message: {:?}", first.message)
    };
    assert_eq!(
        provider.model, "model-a",
        "the request's alias, not a constant"
    );

    req.messages.push(first.message);
    req.messages.push(Message::tool_result("call_a", "18C"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(body["model"], "model-a");
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["extra_content"],
        json!({"google": {"thought_signature": "alias-sig"}}),
        "a constant stamp would not match this alias: {body}"
    );
}

/// The frontier fallback, or a person switching model mid-run: the old model's members must never
/// reach the new one.
#[tokio::test]
async fn opaque_members_are_dropped_when_the_run_changes_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_m", "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"},
                    "extra_content": {"google": {"thought_signature": "model-a-sig"}}
                }]},
                "finish_reason": "tool_calls"
            }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "18C"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let client = client(&server);
    let mut req = request();
    let first = client.complete(req.clone()).await.unwrap();
    req.model = "another-model".into();
    req.messages.push(first.message);
    req.messages.push(Message::tool_result("call_m", "18C"));
    client.complete(req).await.unwrap();

    let requests = server.received_requests().await.expect("recording");
    let body: Value = requests[1].body_json().expect("json body");
    assert_eq!(body["model"], "another-model");
    assert!(
        body["messages"][2]["tool_calls"][0]
            .get("extra_content")
            .is_none(),
        "the old model's member reached the new one: {body}"
    );
}

/// The same alias at another endpoint is another model as far as a signature is concerned.
#[tokio::test]
async fn opaque_members_are_dropped_at_another_endpoint() {
    let first_server = MockServer::start().await;
    mount(
        &first_server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_e", "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"},
                    "extra_content": {"google": {"thought_signature": "endpoint-a-sig"}}
                }]},
                "finish_reason": "tool_calls"
            }]
        })),
    )
    .await;
    let second_server = MockServer::start().await;
    mount(
        &second_server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "18C"}, "finish_reason": "stop"}]
        })),
    )
    .await;

    let mut req = request();
    let first = client(&first_server).complete(req.clone()).await.unwrap();
    req.messages.push(first.message);
    req.messages.push(Message::tool_result("call_e", "18C"));
    client(&second_server).complete(req).await.unwrap();

    let body = sent_body(&second_server).await;
    assert!(
        body["messages"][2]["tool_calls"][0]
            .get("extra_content")
            .is_none(),
        "another endpoint received the first endpoint's member: {body}"
    );
}

#[tokio::test]
async fn malformed_tool_call_json_is_kept_as_an_invalid_call() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{
                "message": {"tool_calls": [{"id": "c", "type": "function",
                    "function": {"name": "weather", "arguments": "{\"city\": "}}]},
                "finish_reason": "tool_calls"
            }]
        })),
    )
    .await;
    let resp = client(&server).complete(request()).await.unwrap();
    let call = &resp.message.tool_calls()[0];
    assert_eq!(call.invalid_arguments.as_deref(), Some("{\"city\": "));
    assert_eq!(call.arguments, json!(adam_model::INVALID_ARGUMENTS_MARKER));
    assert_eq!(call.name, "weather");
}

#[tokio::test]
async fn an_invalid_call_is_echoed_with_empty_object_arguments() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        })),
    )
    .await;
    let mut req = request();
    req.messages.push(Message::Assistant {
        content: vec![],
        tool_calls: vec![adam_model::ToolCall::with_invalid_arguments(
            "c",
            "weather",
            "{\"city\": ",
        )],
        reasoning: None,
        provider: None,
    });
    req.messages.push(Message::tool_error("c", "not JSON"));
    client(&server).complete(req).await.unwrap();
    let body = sent_body(&server).await;
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{}"
    );
    assert!(!body["messages"].to_string().contains("city"));
}

#[tokio::test]
async fn unparseable_body_is_a_protocol_error() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_string("<html>oops</html>"),
    )
    .await;
    let err = client(&server).complete(request()).await.unwrap_err();
    assert!(matches!(err, ModelError::Protocol { .. }), "{err:?}");
}

// ---------------------------------------------------------------- streaming --

fn text_events() -> Vec<&'static str> {
    vec![
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"Sun"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"ny."}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":3}}"#,
        "[DONE]",
    ]
}

#[tokio::test]
async fn stream_text_deltas_concatenate_and_end_with_finished() {
    let server = MockServer::start().await;
    mount(&server, sse(&text_events())).await;

    let stream = client(&server).stream(request()).await.unwrap();
    let items = collect(stream).await;
    let deltas: Vec<ModelDelta> = items.into_iter().map(|i| i.unwrap()).collect();

    let text: String = deltas
        .iter()
        .filter_map(|d| match d {
            ModelDelta::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    let Some(ModelDelta::Finished(resp)) = deltas.last() else {
        panic!("stream must end with Finished: {deltas:?}")
    };
    assert_eq!(text, "Sunny.");
    assert_eq!(resp.message.text(), text);
    assert_eq!(resp.finish, FinishReason::Stop);
    assert_eq!(resp.usage, Usage::new(20, 3));
    assert_eq!(
        deltas
            .iter()
            .filter(|d| matches!(d, ModelDelta::Finished(_)))
            .count(),
        1
    );

    let body = sent_body(&server).await;
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[tokio::test]
async fn stream_assembles_tool_calls_from_deltas() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"weather","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"ci"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\":\"Paris\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":9}}"#,
            "[DONE]",
        ]),
    )
    .await;

    let stream = client(&server).stream(request()).await.unwrap();
    let deltas: Vec<ModelDelta> = collect(stream)
        .await
        .into_iter()
        .map(|i| i.unwrap())
        .collect();
    assert_eq!(
        deltas[0],
        ModelDelta::ToolCallStarted {
            id: "call_1".into(),
            name: "weather".into()
        }
    );
    let Some(ModelDelta::Finished(resp)) = deltas.last() else {
        panic!("{deltas:?}")
    };
    assert_eq!(resp.finish, FinishReason::ToolCalls);
    assert_eq!(
        resp.message.tool_calls()[0].arguments,
        json!({"city": "Paris"})
    );
    assert_eq!(resp.message.tool_calls()[0].id, "call_1");
}

#[tokio::test]
async fn stream_malformed_tool_arguments_are_kept_as_an_invalid_call() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"weather","arguments":"{\"city\": "}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]),
    )
    .await;
    let items = collect(client(&server).stream(request()).await.unwrap()).await;
    let Some(Ok(ModelDelta::Finished(resp))) = items.last() else {
        panic!("{items:?}")
    };
    let call = &resp.message.tool_calls()[0];
    assert_eq!(call.invalid_arguments.as_deref(), Some("{\"city\": "));
    assert_eq!(call.arguments, json!(adam_model::INVALID_ARGUMENTS_MARKER));
}

#[tokio::test]
async fn a_truncated_stream_is_transient_not_a_fabricated_response() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse(&[r#"{"choices":[{"index":0,"delta":{"content":"partial"}}]}"#]),
    )
    .await;
    let items = collect(client(&server).stream(request()).await.unwrap()).await;
    assert!(matches!(items[0], Ok(ModelDelta::Text(_))));
    assert!(
        matches!(items.last(), Some(Err(ModelError::Transient { .. }))),
        "{items:?}"
    );
    assert!(
        !items
            .iter()
            .any(|i| matches!(i, Ok(ModelDelta::Finished(_))))
    );
}

#[tokio::test]
async fn in_band_stream_error_is_surfaced() {
    let server = MockServer::start().await;
    mount(
        &server,
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"content":"Hi"}}]}"#,
            r#"{"error":{"message":"upstream exploded","type":"server_error"}}"#,
        ]),
    )
    .await;
    let items = collect(client(&server).stream(request()).await.unwrap()).await;
    assert!(
        matches!(items.last(), Some(Err(ModelError::Transient { .. }))),
        "{items:?}"
    );
}

#[tokio::test]
async fn stream_http_errors_fail_before_the_stream_starts() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(429).insert_header("Retry-After", "3"),
    )
    .await;
    let Err(err) = client(&server).stream(request()).await else {
        panic!("expected an error")
    };
    assert_eq!(err.class(), ErrorClass::RateLimited);
    assert_eq!(err.retry_after(), Some(Duration::from_secs(3)));
}

// ------------------------------------------------------------- error mapping --

async fn status_error(template: ResponseTemplate) -> ModelError {
    let server = MockServer::start().await;
    mount(&server, template.set_body_string("")).await;
    client(&server).complete(request()).await.unwrap_err()
}

#[tokio::test]
async fn rate_limit_with_retry_after_seconds() {
    let err = status_error(ResponseTemplate::new(429).insert_header("Retry-After", "3")).await;
    assert_eq!(err.class(), ErrorClass::RateLimited);
    assert_eq!(err.retry_after(), Some(Duration::from_secs(3)));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn rate_limit_with_retry_after_http_date() {
    let at = std::time::SystemTime::now() + Duration::from_secs(60);
    let date = httpdate::fmt_http_date(at);
    let err =
        status_error(ResponseTemplate::new(429).insert_header("Retry-After", date.as_str())).await;
    let ModelError::RateLimited {
        retry_after: Some(d),
    } = err
    else {
        panic!("{err:?}")
    };
    assert!(
        d > Duration::from_secs(50) && d <= Duration::from_secs(60),
        "{d:?}"
    );
}

#[tokio::test]
async fn rate_limit_without_retry_after() {
    let err = status_error(ResponseTemplate::new(429)).await;
    assert!(
        matches!(err, ModelError::RateLimited { retry_after: None }),
        "{err:?}"
    );
}

#[tokio::test]
async fn server_errors_are_transient() {
    for status in [500, 502, 503, 504] {
        let err = status_error(ResponseTemplate::new(status)).await;
        assert!(
            matches!(err, ModelError::Transient { .. }),
            "{status}: {err:?}"
        );
        assert!(err.is_retryable());
    }
    let err = status_error(ResponseTemplate::new(408)).await;
    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
}

#[tokio::test]
async fn auth_errors() {
    for status in [401, 403] {
        let err = status_error(ResponseTemplate::new(status)).await;
        assert!(matches!(err, ModelError::Auth(_)), "{status}: {err:?}");
        assert!(!err.is_retryable());
    }
}

#[tokio::test]
async fn other_client_errors_are_invalid_request() {
    for status in [400, 404, 409, 422] {
        let err = status_error(ResponseTemplate::new(status)).await;
        assert!(
            matches!(err, ModelError::InvalidRequest { .. }),
            "{status}: {err:?}"
        );
    }
}

#[tokio::test]
async fn context_length_errors() {
    for status in [400, 413] {
        let server = MockServer::start().await;
        mount(
            &server,
            ResponseTemplate::new(status).set_body_json(json!({"error": {
                "message": "This model's maximum context length is 8192 tokens.",
                "type": "invalid_request_error",
                "code": "context_length_exceeded"
            }})),
        )
        .await;
        let err = client(&server).complete(request()).await.unwrap_err();
        assert!(
            matches!(err, ModelError::ContextLength(_)),
            "{status}: {err:?}"
        );
        assert!(!err.is_retryable());
    }
}

#[tokio::test]
async fn error_message_carries_the_server_detail_but_never_the_key() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(401)
            .set_body_json(json!({"error": {"message": "Incorrect API key provided"}})),
    )
    .await;
    let err = client(&server).complete(request()).await.unwrap_err();
    let shown = format!("{err} / {err:?}");
    assert!(shown.contains("Incorrect API key provided"), "{shown}");
    assert!(!shown.contains(KEY), "{shown}");
}

#[tokio::test]
async fn slow_server_times_out_as_transient() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200)
            .set_delay(Duration::from_secs(3))
            .set_body_json(
                json!({"choices": [{"message": {"content": "late"}, "finish_reason": "stop"}]}),
            ),
    )
    .await;
    let client = client_with(&server, Duration::from_millis(200));
    let err = client.complete(request()).await.unwrap_err();
    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
    let Err(err) = client.stream(request()).await else {
        panic!("expected a timeout")
    };
    assert!(matches!(err, ModelError::Transient { .. }), "{err:?}");
}

/// A connection that cannot be made is `Unreachable` (ADR 0067 decision 3), still of class
/// `Transient`, so the runtime retries it as before.
#[tokio::test]
async fn connection_refused_is_unreachable_and_transient() {
    // Bind then drop, so the port is very likely closed.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let config = OpenAiConfig::new(format!("http://{addr}/v1"), SecretString::from(KEY));
    let client = OpenAiCompatible::new(config).unwrap();
    let err = client.complete(request()).await.unwrap_err();
    assert!(matches!(err, ModelError::Unreachable { .. }), "{err:?}");
    assert_eq!(err.class(), ErrorClass::Transient);
    assert!(err.is_retryable());

    // The transport error is the source, not flattened into the message; the chain has it.
    let source = std::error::Error::source(&err).expect("a transport source");
    assert!(
        source.downcast_ref::<reqwest::Error>().is_some(),
        "{source:?}"
    );
    assert_eq!(err.to_string(), "the model endpoint could not be reached");
    assert!(adam_error::report(&err).len() > err.to_string().len());
}

#[tokio::test]
async fn no_retries_on_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let _ = client(&server).complete(request()).await;
    // `expect(1)` is verified when the server drops.
}

#[tokio::test]
async fn invalid_requests_never_reach_the_network() {
    let server = MockServer::start().await;
    let mut req = ModelRequest::new("m");
    req.tool_choice = ToolChoice::Required; // no tools
    let err = client(&server).complete(req).await.unwrap_err();
    assert!(matches!(err, ModelError::InvalidRequest { .. }));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn usable_through_dyn_model() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(
            json!({"choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]}),
        ),
    )
    .await;
    let model: adam_model::DynModel = std::sync::Arc::new(client(&server));
    assert_eq!(
        model.complete(request()).await.unwrap().message.text(),
        "ok"
    );
}

// ------------------------------------------------------------ source chains --

/// A body that is not JSON is a protocol error that keeps the parser's error as its source.
#[tokio::test]
async fn a_malformed_body_keeps_the_parser_error_as_its_source() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_string("not json"),
    )
    .await;
    let err = client(&server).complete(request()).await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Corrupt, "{err:?}");
    let source = std::error::Error::source(&err).expect("a parser source");
    assert!(
        source.downcast_ref::<serde_json::Error>().is_some(),
        "{source:?}"
    );
}

// --------------------------------------------------------------- reasoning --

fn plain_client(server: &MockServer) -> OpenAiCompatible {
    OpenAiCompatible::new(OpenAiConfig::new(
        format!("{}/v1", server.uri()),
        SecretString::from(KEY),
    ))
    .expect("client")
}

#[tokio::test]
async fn a_streamed_reasoning_arrives_before_the_answer_under_either_name() {
    // DeepSeek, GLM and LiteLLM say `reasoning_content`; OpenRouter, Ollama and current vLLM `reasoning`.
    for name in ["reasoning_content", "reasoning"] {
        let server = MockServer::start().await;
        let thought =
            |text: &str| json!({"choices": [{"index": 0, "delta": {name: text}}]}).to_string();
        let thought_1 = thought("The user asks ");
        let thought_2 = thought("for the weather.");
        mount(
            &server,
            sse(&[
                &thought_1,
                &thought_2,
                r#"{"choices":[{"index":0,"delta":{"content":"Sunny."}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
        )
        .await;
        let items = collect(plain_client(&server).stream(request()).await.unwrap()).await;
        let items: Vec<_> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(
            items[..3],
            [
                ModelDelta::Reasoning("The user asks ".into()),
                ModelDelta::Reasoning("for the weather.".into()),
                ModelDelta::Text("Sunny.".into()),
            ],
            "{name}"
        );
        let ModelDelta::Finished(response) = items.last().unwrap() else {
            panic!("{items:?}")
        };
        assert_eq!(
            response.reasoning.as_deref(),
            Some("The user asks for the weather.")
        );
        // The answer is the answer; the history does not keep the reasoning.
        assert_eq!(response.message.text(), "Sunny.");
        assert_eq!(response.message.reasoning(), None);
    }
}

#[tokio::test]
async fn a_completions_reasoning_is_the_responses_not_the_messages() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"role": "assistant", "content": "Sunny.",
                                     "reasoning_content": "Look it up."}, "finish_reason": "stop"}]
        })),
    )
    .await;
    let response = plain_client(&server).complete(request()).await.unwrap();
    assert_eq!(response.reasoning.as_deref(), Some("Look it up."));
    assert_eq!(response.message.text(), "Sunny.");
    assert_eq!(response.message.reasoning(), None);
}

/// A history that holds reasoning (a client that echoes it kept it) is sent without it by a client that
/// does not, and with it, under the chosen name, by one that does.
#[tokio::test]
async fn reasoning_goes_back_only_through_a_client_set_to_echo_it() {
    let history = || {
        let mut req = request();
        req.messages.push(Message::Assistant {
            content: vec![],
            tool_calls: vec![adam_model::ToolCall {
                id: "call_1".into(),
                name: "weather".into(),
                arguments: json!({}),
                ..Default::default()
            }],
            reasoning: Some("I should call the tool.".into()),
            provider: None,
        });
        req.messages.push(Message::tool_result("call_1", "18C"));
        req
    };
    let ok = || {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "18C"}, "finish_reason": "stop"}]
        }))
    };

    let server = MockServer::start().await;
    mount(&server, ok()).await;
    plain_client(&server).complete(history()).await.unwrap();
    let body = sent_body(&server).await.to_string();
    assert!(
        !body.contains("reasoning") && !body.contains("I should call"),
        "{body}"
    );

    for (field, name) in [
        (ReasoningField::ReasoningContent, "reasoning_content"),
        (ReasoningField::Reasoning, "reasoning"),
    ] {
        let server = MockServer::start().await;
        mount(&server, ok()).await;
        plain_client(&server)
            .with_echo_reasoning(Some(field))
            .complete(history())
            .await
            .unwrap();
        let body = sent_body(&server).await;
        assert_eq!(
            body["messages"][2][name], "I should call the tool.",
            "{name}"
        );
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "call_1");
    }
}

/// An echoing client keeps what the model thought in the message it returns, so that the next request
/// carries it; a client that does not, returns the message without it.
#[tokio::test]
async fn an_echoing_client_keeps_the_reasoning_in_the_message_it_returns() {
    let sse_body = || {
        sse(&[
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"Think."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"Done."},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ])
    };
    let server = MockServer::start().await;
    mount(&server, sse_body()).await;
    let client = plain_client(&server).with_echo_reasoning(Some(ReasoningField::ReasoningContent));
    let items = collect(client.stream(request()).await.unwrap()).await;
    let Some(Ok(ModelDelta::Finished(response))) = items.into_iter().last() else {
        panic!("a final message")
    };
    assert_eq!(response.message.reasoning(), Some("Think."));
    assert_eq!(response.message.text(), "Done.");
    assert_eq!(response.reasoning.as_deref(), Some("Think."));
}

#[tokio::test]
async fn the_extra_body_is_sent_with_every_request_streamed_or_not() {
    let extra = json!({"reasoning_effort": "medium", "thinking": {"type": "enabled"}});
    let extra = extra.as_object().unwrap().clone();
    for streaming in [false, true] {
        let server = MockServer::start().await;
        if streaming {
            mount(
                &server,
                sse(&[
                    r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
                    "[DONE]",
                ]),
            )
            .await;
        } else {
            mount(
                &server,
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}]
                })),
            )
            .await;
        }
        let client = plain_client(&server)
            .with_extra_body(extra.clone())
            .unwrap();
        if streaming {
            collect(client.stream(request()).await.unwrap()).await;
        } else {
            client.complete(request()).await.unwrap();
        }
        let body = sent_body(&server).await;
        assert_eq!(body["reasoning_effort"], "medium", "streaming {streaming}");
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["model"], "gw-model");
        assert_eq!(body["messages"][1]["content"], "weather in Paris?");
    }
}

#[test]
fn an_extra_body_may_not_set_what_the_client_owns() {
    for key in ["model", "messages", "tools", "tool_choice", "stream"] {
        let extra = json!({ key: 1, "ok": true }).as_object().unwrap().clone();
        let client = OpenAiCompatible::new(OpenAiConfig::new(
            "https://gw.example/v1",
            SecretString::from(KEY),
        ))
        .unwrap();
        let error = client.with_extra_body(extra).unwrap_err();
        assert!(
            matches!(&error, OpenAiConfigError::ReservedBodyKey(k) if k == key),
            "{key}"
        );
        assert_eq!(error.class(), ErrorClass::Invalid);
    }
}

#[tokio::test]
async fn the_prompt_cache_key_is_sent_only_by_a_client_built_to_send_it() {
    for (on, streaming) in [(true, false), (false, false), (true, true), (false, true)] {
        let server = MockServer::start().await;
        if streaming {
            mount(
                &server,
                sse(&[
                    r#"{"choices":[{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
                    "[DONE]",
                ]),
            )
            .await;
        } else {
            mount(
                &server,
                ResponseTemplate::new(200).set_body_json(json!({
                    "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}]
                })),
            )
            .await;
        }
        let client = client(&server).with_prompt_cache_key(on);
        let mut req = request();
        req.prompt_cache_key = Some("0123456789abcdef0123456789abcdef".into());
        if streaming {
            collect(client.stream(req).await.unwrap()).await;
        } else {
            client.complete(req).await.unwrap();
        }
        let body = sent_body(&server).await;
        if on {
            assert_eq!(
                body["prompt_cache_key"], "0123456789abcdef0123456789abcdef",
                "on, streaming {streaming}"
            );
        } else {
            assert!(
                body.get("prompt_cache_key").is_none(),
                "off, streaming {streaming}: {body}"
            );
        }
    }
}

#[tokio::test]
async fn a_body_over_the_cap_is_a_protocol_error() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200).set_body_raw(vec![b'x'; (16 << 20) + 1], "application/json"),
    )
    .await;
    let err = client(&server)
        .complete(request())
        .await
        .expect_err("over 16 MiB");
    assert!(matches!(err, ModelError::Protocol { .. }), "{err:?}");
    assert!(err.to_string().contains("over 16777216 bytes"), "{err}");
    assert_eq!(err.class(), ErrorClass::Corrupt);
}

#[tokio::test]
async fn a_stream_line_over_the_cap_is_a_protocol_error() {
    let server = MockServer::start().await;
    let mut body = b"data: ".to_vec();
    body.extend_from_slice(&vec![b'x'; 1 << 20]);
    mount(
        &server,
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
    )
    .await;
    let mut stream = client(&server)
        .stream(request())
        .await
        .expect("the response headers");
    let err = stream
        .next()
        .await
        .expect("one item")
        .expect_err("over 1 MiB");
    assert!(matches!(err, ModelError::Protocol { .. }), "{err:?}");
    assert!(err.to_string().contains("over 1048576 bytes"), "{err}");
    assert!(stream.next().await.is_none(), "the stream stops after it");
}
