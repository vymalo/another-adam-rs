//! The chat-completions wire format: request building, non-streaming response
//! parsing, and assembly of streamed chunks. Pure functions, no I/O.

use std::collections::BTreeMap;

use adam_model::{
    ContentPart, FinishReason, Message, ModelDelta, ModelError, ModelRequest, ModelResponse,
    ToolCall, ToolChoice, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::MaxTokensField;
use crate::errors::from_error_object;

// ---------------------------------------------------------------- request --

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a BTreeMap<String, String>>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    /// Serialized even when `None` (as `null`): assistant messages that only
    /// carry tool calls have no content, and some servers insist on the key.
    content: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// Serialize `req` as a chat-completions body.
pub(crate) fn build_request(
    req: &ModelRequest,
    stream: bool,
    max_tokens_field: MaxTokensField,
) -> Result<Vec<u8>, ModelError> {
    if let Some(t) = req.temperature
        && !t.is_finite()
    {
        return Err(ModelError::invalid_request("temperature must be finite"));
    }
    if req.tools.is_empty() && matches!(req.tool_choice, ToolChoice::Required | ToolChoice::Tool(_))
    {
        return Err(ModelError::invalid_request(
            "tool_choice requires at least one tool",
        ));
    }

    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if let Some(system) = req.system.as_deref().filter(|s| !s.is_empty()) {
        messages.push(text_message("system", system));
    }
    for message in &req.messages {
        messages.push(wire_message(message)?);
    }

    let tools: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            // Some servers reject a tool without a schema; `null` means "none".
            let parameters = if t.parameters.is_null() {
                json!({"type": "object", "properties": {}})
            } else {
                t.parameters.clone()
            };
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": parameters,
                },
            })
        })
        .collect();

    // `tool_choice` is only valid alongside `tools`.
    let tool_choice = (!tools.is_empty()).then(|| match &req.tool_choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({"type": "function", "function": {"name": name}}),
    });

    let (max_tokens, max_completion_tokens) = match max_tokens_field {
        MaxTokensField::MaxTokens => (req.max_output_tokens, None),
        MaxTokensField::MaxCompletionTokens => (None, req.max_output_tokens),
    };

    let wire = WireRequest {
        model: &req.model,
        messages,
        tools,
        tool_choice,
        max_tokens,
        max_completion_tokens,
        temperature: req.temperature,
        stream: stream.then_some(true),
        stream_options: stream.then(|| json!({"include_usage": true})),
        metadata: (!req.metadata.is_empty()).then_some(&req.metadata),
    };
    serde_json::to_vec(&wire)
        .map_err(|e| ModelError::invalid_request("request is not serializable").with_source(e))
}

fn text_message<'a>(role: &'static str, text: &str) -> WireMessage<'a> {
    WireMessage {
        role,
        content: Some(Value::String(text.to_owned())),
        tool_calls: Vec::new(),
        tool_call_id: None,
    }
}

/// Message content: **one string**, the text parts joined with a blank line, for every message
/// whose parts are all text (today, every one).
///
/// A user message that has several text parts (a continued conversation merges the messages that
/// would otherwise sit next to each other, see `Conversation::continued`) is stored with its parts
/// and sent as the text it says. A plain string is the form of `content` that every
/// OpenAI-compatible server takes; an array of typed parts is not taken by all of them, nor by
/// every chat template (*unverified* for each server: chat templates that insist on a string
/// content are the reason), so it is kept for a part that is not text. [`ContentPart`] has only
/// `Text` today, so this match fails to compile when a variant is added, which is where to send
/// that part, and the text parts around it, as typed parts.
fn content_value(parts: &[ContentPart]) -> Option<Value> {
    if parts.is_empty() {
        return None;
    }
    let text = parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(Value::String(text))
}

fn wire_message(message: &Message) -> Result<WireMessage<'_>, ModelError> {
    Ok(match message {
        Message::User { content } => WireMessage {
            role: "user",
            // An empty user message is still a message; send an empty string.
            content: Some(content_value(content).unwrap_or_else(|| Value::String(String::new()))),
            tool_calls: Vec::new(),
            tool_call_id: None,
        },
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut wire_calls = Vec::with_capacity(tool_calls.len());
            for call in tool_calls {
                let arguments = serde_json::to_string(&call.arguments).map_err(|e| {
                    ModelError::invalid_request(format!(
                        "arguments of tool call {} are not serializable",
                        call.id
                    ))
                    .with_source(e)
                })?;
                wire_calls.push(json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": arguments},
                }));
            }
            let content = content_value(content);
            WireMessage {
                role: "assistant",
                // With tool calls, `null` content is the canonical form; without,
                // send an empty string so the message stays well-formed.
                content: content
                    .or_else(|| tool_calls.is_empty().then(|| Value::String(String::new()))),
                tool_calls: wire_calls,
                tool_call_id: None,
            }
        }
        // The chat-completions format has no error flag on tool messages, so
        // `is_error` is not sent: tools should put the failure in `content`.
        Message::Tool {
            call_id, content, ..
        } => WireMessage {
            role: "tool",
            content: Some(Value::String(content.clone())),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id),
        },
    })
}

// --------------------------------------------------------------- response --

/// Parse a non-streaming response body.
pub(crate) fn parse_completion(body: &[u8]) -> Result<ModelResponse, ModelError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ModelError::protocol("response is not JSON").with_source(e))?;
    if value.get("choices").is_none() && value.get("error").is_some() {
        return Err(from_error_object(&value));
    }
    let completion: Completion = serde_json::from_value(value)
        .map_err(|e| ModelError::protocol("unexpected response shape").with_source(e))?;

    let choice = completion
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| ModelError::protocol("response has no choices"))?;
    let message = choice
        .message
        .ok_or_else(|| ModelError::protocol("choice has no message"))?;
    let finish = choice
        .finish_reason
        .ok_or_else(|| ModelError::protocol("choice has no finish_reason"))?;

    let mut tool_calls = Vec::new();
    for (i, call) in message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        let name = call.function.name.unwrap_or_default();
        let id = call
            .id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| fallback_id(i));
        let arguments = match call.function.arguments {
            None | Some(Value::Null) => json!({}),
            // Some servers send the arguments as an object instead of a string.
            Some(Value::String(raw)) => parse_arguments(&name, &raw)?,
            Some(other) => other,
        };
        check_name(&name)?;
        tool_calls.push(ToolCall {
            id,
            name,
            arguments,
        });
    }

    Ok(build_response(
        content_text(message.content),
        tool_calls,
        &finish,
        completion.usage.map(Usage::from).unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Option<ChoiceMessage>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<Value>,
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: Option<String>,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: Option<String>,
    arguments: Option<Value>,
}

#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl From<WireUsage> for Usage {
    fn from(u: WireUsage) -> Self {
        Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        }
    }
}

/// Text of a message `content`: a string, or (rarely) an array of typed parts.
fn content_text(content: Option<Value>) -> String {
    match content {
        Some(Value::String(s)) => s,
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

fn fallback_id(index: usize) -> String {
    format!("call_{index}")
}

fn check_name(name: &str) -> Result<(), ModelError> {
    if name.is_empty() {
        return Err(ModelError::protocol("tool call has no name"));
    }
    Ok(())
}

/// Parse tool-call arguments. Empty means "no arguments"; anything else must
/// be valid JSON.
fn parse_arguments(name: &str, raw: &str) -> Result<Value, ModelError> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|e| {
        ModelError::protocol(format!(
            "arguments of tool call `{name}` are not valid JSON"
        ))
        .with_source(e)
    })
}

fn map_finish(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_owned()),
    }
}

fn build_response(
    text: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: &str,
    usage: Usage,
) -> ModelResponse {
    let mut finish = map_finish(finish_reason);
    // Some servers (Ollama, several gateways) report `stop` for a turn that
    // ended in tool calls; the calls are what matters to the agent loop.
    if !tool_calls.is_empty() && finish == FinishReason::Stop {
        finish = FinishReason::ToolCalls;
    }
    let content = if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::text(text)]
    };
    ModelResponse {
        message: Message::Assistant {
            content,
            tool_calls,
        },
        finish,
        usage,
    }
}

// ------------------------------------------------------------- streaming --

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    /// Position of the call within the message; deltas of one call share it.
    #[serde(default)]
    index: Option<usize>,
    id: Option<String>,
    function: Option<DeltaFunction>,
}

#[derive(Deserialize, Default)]
struct DeltaFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    arguments: String,
    started: bool,
}

/// Assembles streamed chunks into a [`ModelResponse`], keyed by tool-call
/// index. Feed it each parsed SSE payload with [`Assembler::push`]; call
/// [`Assembler::finish`] when the stream ends.
#[derive(Default)]
pub(crate) struct Assembler {
    text: String,
    calls: BTreeMap<usize, PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl Assembler {
    /// Consume one SSE `data:` payload (not `[DONE]`), returning the deltas to
    /// forward to the caller.
    pub(crate) fn push(&mut self, data: &str) -> Result<Vec<ModelDelta>, ModelError> {
        let value: Value = serde_json::from_str(data)
            .map_err(|e| ModelError::protocol("stream chunk is not JSON").with_source(e))?;
        if value.get("choices").is_none() && value.get("error").is_some() {
            return Err(from_error_object(&value));
        }
        let chunk: Chunk = serde_json::from_value(value)
            .map_err(|e| ModelError::protocol("unexpected stream chunk shape").with_source(e))?;

        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.into());
        }

        let mut out = Vec::new();
        // Only the first choice is used (`n` is never requested).
        for choice in chunk.choices.into_iter().filter(|c| c.index == 0) {
            if let Some(delta) = choice.delta {
                if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
                    self.text.push_str(&text);
                    out.push(ModelDelta::Text(text));
                }
                for (position, call) in delta.tool_calls.unwrap_or_default().into_iter().enumerate()
                {
                    if let Some(started) = self.push_tool_call(position, call) {
                        out.push(started);
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
        Ok(out)
    }

    fn push_tool_call(&mut self, position: usize, delta: DeltaToolCall) -> Option<ModelDelta> {
        // Servers that omit `index` send whole calls, one per position.
        let index = delta.index.unwrap_or(position);
        let call = self.calls.entry(index).or_default();
        if let Some(id) = delta.id.filter(|id| !id.is_empty()) {
            call.id.get_or_insert(id);
        }
        if let Some(function) = delta.function {
            if let Some(name) = function.name {
                call.name.push_str(&name);
            }
            if let Some(args) = function.arguments {
                call.arguments.push_str(&args);
            }
        }
        if !call.started && !call.name.is_empty() {
            call.started = true;
            let id = call.id.get_or_insert_with(|| fallback_id(index)).clone();
            return Some(ModelDelta::ToolCallStarted {
                id,
                name: call.name.clone(),
            });
        }
        None
    }

    /// Finish the stream. Fails if the server never said why it stopped,
    /// rather than fabricating a response from a truncated stream.
    pub(crate) fn finish(self) -> Result<ModelResponse, ModelError> {
        let reason = self
            .finish_reason
            .ok_or_else(|| ModelError::protocol("stream ended without a finish_reason"))?;
        let mut tool_calls = Vec::with_capacity(self.calls.len());
        for (index, call) in self.calls {
            check_name(&call.name)?;
            let arguments = parse_arguments(&call.name, &call.arguments)?;
            tool_calls.push(ToolCall {
                id: call.id.unwrap_or_else(|| fallback_id(index)),
                name: call.name,
                arguments,
            });
        }
        Ok(build_response(
            self.text,
            tool_calls,
            &reason,
            self.usage.unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use adam_model::ToolSpec;

    use super::*;

    fn request() -> ModelRequest {
        let mut req = ModelRequest::new("gw-model");
        req.system = Some("sys".into());
        req.messages = vec![
            Message::user_text("hi"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "weather".into(),
                    arguments: json!({"city": "Paris"}),
                }],
            },
            Message::tool_result("c1", "sunny"),
        ];
        req.tools = vec![ToolSpec {
            name: "weather".into(),
            description: "Get weather".into(),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        }];
        req
    }

    fn body(req: &ModelRequest, stream: bool, f: MaxTokensField) -> Value {
        serde_json::from_slice(&build_request(req, stream, f).unwrap()).unwrap()
    }

    #[test]
    fn request_shape() {
        let mut req = request();
        req.max_output_tokens = Some(100);
        req.temperature = Some(0.7);
        req.metadata.insert("run".into(), "r1".into());
        let v = body(&req, false, MaxTokensField::MaxTokens);
        assert_eq!(
            v,
            json!({
                "model": "gw-model",
                "messages": [
                    {"role": "system", "content": "sys"},
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}}]},
                    {"role": "tool", "content": "sunny", "tool_call_id": "c1"},
                ],
                "tools": [{"type": "function", "function": {
                    "name": "weather", "description": "Get weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}],
                "tool_choice": "auto",
                "max_tokens": 100,
                "temperature": 0.7,
                "metadata": {"run": "r1"},
            })
        );
    }

    #[test]
    fn stream_flags_and_max_completion_tokens() {
        let mut req = request();
        req.max_output_tokens = Some(5);
        let v = body(&req, true, MaxTokensField::MaxCompletionTokens);
        assert_eq!(v["stream"], json!(true));
        assert_eq!(v["stream_options"], json!({"include_usage": true}));
        assert_eq!(v["max_completion_tokens"], json!(5));
        assert!(v.get("max_tokens").is_none());
        let v = body(&req, false, MaxTokensField::MaxTokens);
        assert!(v.get("stream").is_none() && v.get("stream_options").is_none());
    }

    #[test]
    fn optional_fields_are_omitted() {
        let v = body(&ModelRequest::new("m"), false, MaxTokensField::MaxTokens);
        assert_eq!(v, json!({"model": "m", "messages": []}));
    }

    #[test]
    fn tool_choice_variants() {
        let mut req = request();
        for (choice, expected) in [
            (ToolChoice::None, json!("none")),
            (ToolChoice::Required, json!("required")),
            (
                ToolChoice::Tool("weather".into()),
                json!({"type": "function", "function": {"name": "weather"}}),
            ),
        ] {
            req.tool_choice = choice;
            assert_eq!(
                body(&req, false, MaxTokensField::MaxTokens)["tool_choice"],
                expected
            );
        }
    }

    #[test]
    fn invalid_requests_are_rejected_locally() {
        let mut req = ModelRequest::new("m");
        req.tool_choice = ToolChoice::Required;
        assert!(matches!(
            build_request(&req, false, MaxTokensField::MaxTokens),
            Err(ModelError::InvalidRequest { .. })
        ));
        let mut req = ModelRequest::new("m");
        req.temperature = Some(f32::NAN);
        assert!(matches!(
            build_request(&req, false, MaxTokensField::MaxTokens),
            Err(ModelError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn multi_part_text_content_is_one_string_and_a_null_schema_is_an_object() {
        let mut req = ModelRequest::new("m");
        req.messages = vec![Message::User {
            content: vec![ContentPart::text("a"), ContentPart::text("b")],
        }];
        req.tools = vec![ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: Value::Null,
        }];
        let v = body(&req, false, MaxTokensField::MaxTokens);
        // The parts are sent as one string, joined with a blank line; the request still holds them.
        assert_eq!(v["messages"][0]["content"], json!("a\n\nb"));
        assert_eq!(
            v["tools"][0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn parses_text_completion() {
        let r = parse_completion(
            br#"{"choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#,
        )
        .unwrap();
        assert_eq!(r.message.text(), "hi");
        assert_eq!(r.finish, FinishReason::Stop);
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 3,
                output_tokens: 4
            }
        );
    }

    #[test]
    fn parses_tool_calls() {
        let r = parse_completion(
            br#"{"choices":[{"message":{"content":null,"tool_calls":[
                {"id":"a","type":"function","function":{"name":"w","arguments":"{\"x\":1}"}},
                {"id":"b","type":"function","function":{"name":"n","arguments":""}}]},
                "finish_reason":"tool_calls"}]}"#,
        )
        .unwrap();
        assert_eq!(r.finish, FinishReason::ToolCalls);
        let calls = r.message.tool_calls();
        assert_eq!(calls[0].arguments, json!({"x": 1}));
        assert_eq!(calls[1].arguments, json!({}));
        assert_eq!(r.usage, Usage::default());
    }

    #[test]
    fn stop_with_tool_calls_is_normalised() {
        let r = parse_completion(
            br#"{"choices":[{"message":{"tool_calls":[
                {"id":"a","function":{"name":"w","arguments":{"x":1}}}]},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
        assert_eq!(r.finish, FinishReason::ToolCalls);
        assert_eq!(r.message.tool_calls()[0].arguments, json!({"x": 1}));
    }

    #[test]
    fn malformed_completions_are_protocol_errors() {
        for body in [
            &b"not json"[..],
            br#"{"choices":[]}"#,
            br#"{"choices":[{"message":{"content":"x"}}]}"#,
            br#"{"choices":[{"message":{"tool_calls":[{"id":"a","function":{"name":"w","arguments":"{oops"}}]},"finish_reason":"tool_calls"}]}"#,
            br#"{"choices":[{"message":{"tool_calls":[{"id":"a","function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
            br#"{"choices":"nope"}"#,
        ] {
            assert!(
                matches!(parse_completion(body), Err(ModelError::Protocol { .. })),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn error_body_with_200_is_mapped() {
        let e = parse_completion(br#"{"error":{"message":"busy","type":"rate_limit_error"}}"#);
        assert!(matches!(e, Err(ModelError::RateLimited { .. })));
    }

    #[test]
    fn finish_reasons() {
        assert_eq!(map_finish("length"), FinishReason::Length);
        assert_eq!(map_finish("content_filter"), FinishReason::ContentFilter);
        assert_eq!(map_finish("function_call"), FinishReason::ToolCalls);
        assert_eq!(map_finish("weird"), FinishReason::Other("weird".into()));
    }

    fn feed(asm: &mut Assembler, chunks: &[&str]) -> Vec<ModelDelta> {
        chunks.iter().flat_map(|c| asm.push(c).unwrap()).collect()
    }

    #[test]
    fn assembles_interleaved_tool_calls_by_index() {
        let mut asm = Assembler::default();
        let deltas = feed(
            &mut asm,
            &[
                r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Let "}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"me look"}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","type":"function","function":{"name":"one","arguments":""}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"b","type":"function","function":{"name":"two","arguments":"{\"k\""}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":":true}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
            ],
        );
        assert_eq!(
            deltas,
            vec![
                ModelDelta::Text("Let ".into()),
                ModelDelta::Text("me look".into()),
                ModelDelta::ToolCallStarted {
                    id: "a".into(),
                    name: "one".into()
                },
                ModelDelta::ToolCallStarted {
                    id: "b".into(),
                    name: "two".into()
                },
            ]
        );
        let r = asm.finish().unwrap();
        assert_eq!(r.message.text(), "Let me look");
        assert_eq!(r.finish, FinishReason::ToolCalls);
        assert_eq!(
            r.usage,
            Usage {
                input_tokens: 10,
                output_tokens: 5
            }
        );
        let calls = r.message.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            (calls[0].id.as_str(), calls[0].arguments.clone()),
            ("a", json!({"x": 1}))
        );
        assert_eq!(
            (calls[1].id.as_str(), calls[1].arguments.clone()),
            ("b", json!({"k": true}))
        );
    }

    #[test]
    fn missing_call_id_gets_a_stable_fallback() {
        let mut asm = Assembler::default();
        let deltas = feed(
            &mut asm,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"t","arguments":"{}"}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            ],
        );
        assert_eq!(
            deltas,
            vec![ModelDelta::ToolCallStarted {
                id: "call_0".into(),
                name: "t".into()
            }]
        );
        let r = asm.finish().unwrap();
        assert_eq!(r.message.tool_calls()[0].id, "call_0");
        assert_eq!(r.finish, FinishReason::ToolCalls);
    }

    #[test]
    fn malformed_streamed_arguments_are_a_protocol_error() {
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"t","arguments":"{\"x\": "}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        assert!(matches!(asm.finish(), Err(ModelError::Protocol { .. })));
    }

    #[test]
    fn finishing_without_a_reason_is_a_protocol_error() {
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[r#"{"choices":[{"delta":{"content":"partial"}}]}"#],
        );
        assert!(matches!(asm.finish(), Err(ModelError::Protocol { .. })));
    }

    #[test]
    fn garbage_chunks_and_in_band_errors() {
        let mut asm = Assembler::default();
        assert!(matches!(
            asm.push("{nope"),
            Err(ModelError::Protocol { .. })
        ));
        assert!(matches!(
            asm.push(r#"{"error":{"message":"boom"}}"#),
            Err(ModelError::Transient { .. })
        ));
    }

    #[test]
    fn keepalive_chunks_with_null_usage_are_harmless() {
        let mut asm = Assembler::default();
        let deltas = feed(
            &mut asm,
            &[
                r#"{"choices":[{"delta":{"content":""},"finish_reason":null}],"usage":null}"#,
                r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
            ],
        );
        assert_eq!(deltas, vec![ModelDelta::Text("ok".into())]);
        assert_eq!(asm.finish().unwrap().message.text(), "ok");
    }
}
