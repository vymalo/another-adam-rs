//! The chat-completions wire format: request building, non-streaming response
//! parsing, and assembly of streamed chunks. Pure functions, no I/O.

use std::collections::BTreeMap;

use adam_model::{
    ContentPart, FinishReason, ImageData, ImageRef, Message, ModelDelta, ModelError, ModelProfiles,
    ModelRequest, ModelResponse, ProviderTurn, ReasoningEffort, ReasoningWire, ToolCall,
    ToolChoice, Usage, strip_adam_member,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::errors::from_error_object;
use crate::{MaxTokensField, ReasoningField};

/// The members of a body that the runtime owns: `MODEL_EXTRA_BODY` may not set them.
pub(crate) const RESERVED_BODY_KEYS: [&str; 5] =
    ["model", "messages", "tools", "tool_choice", "stream"];

/// What a client adds to every request beside the [`ModelRequest`] itself.
#[derive(Clone, Copy, Default)]
pub(crate) struct RequestOptions<'a> {
    pub(crate) max_tokens_field: MaxTokensField,
    /// Members merged into the body (they win over what the runtime wrote, `prompt_cache_key` included, except
    /// [`RESERVED_BODY_KEYS`], which are refused when the client is built, and except the members a typed
    /// reasoning level's wire duplicates: the level wins there, ADR 0063 decision 6).
    pub(crate) extra_body: Option<&'a serde_json::Map<String, Value>>,
    /// Send the reasoning an assistant message keeps, under this name; `None` sends none.
    pub(crate) echo_reasoning: Option<ReasoningField>,
    /// Send [`ModelRequest::prompt_cache_key`] as the body's `prompt_cache_key` (when the request has one).
    pub(crate) prompt_cache_key: bool,
    /// The client's stable id of the endpoint it speaks to. A provider's opaque members go back
    /// only to the endpoint (and model alias) that produced them.
    pub(crate) endpoint: &'a str,
    /// The deployment's per-model profiles (`MODEL_ALIASES_FILE`): the request's model profile
    /// says how its [`reasoning_effort`](ModelRequest::reasoning_effort) is sent. `None` or a
    /// model with no profile sends nothing.
    pub(crate) profiles: Option<&'a ModelProfiles>,
}

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
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
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
    /// The assistant's reasoning, under the name the provider wants (`reasoning_content` or
    /// `reasoning`), only for a client set to echo it.
    #[serde(flatten)]
    reasoning: Option<BTreeMap<&'static str, Value>>,
    /// The assistant message's opaque members, only for a turn bound to the request's model alias
    /// and endpoint. Empty contributes no member at all.
    #[serde(flatten)]
    opaque: BTreeMap<String, Value>,
}

/// The members the runtime owns on an assistant message: an opaque member of the same name is
/// dropped, the modelled one wins. The output-only members (`refusal`, `audio`, `function_call`,
/// `annotations`) are here too: they are response-only, a provider never needs them back, and
/// keeping one would change every request that carries it.
const MESSAGE_MODELLED_KEYS: [&str; 9] = [
    "role",
    "content",
    "tool_calls",
    "reasoning",
    "reasoning_content",
    "refusal",
    "audio",
    "function_call",
    "annotations",
];

/// The members the runtime owns on a tool call: an opaque member of the same name is dropped, the
/// modelled one wins.
const CALL_MODELLED_KEYS: [&str; 3] = ["id", "type", "function"];

/// Serialize `req` as a chat-completions body.
pub(crate) fn build_request(
    req: &ModelRequest,
    stream: bool,
    options: RequestOptions<'_>,
) -> Result<Vec<u8>, ModelError> {
    let max_tokens_field = options.max_tokens_field;
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
    // The images the tool messages of one run of tool messages return go in one user message after
    // the run, so each body's `messages` starts with the one before it: what the placement depends
    // on is only what came before.
    let mut tool_images: Vec<(&str, &ImageRef)> = Vec::new();
    for message in &req.messages {
        if !matches!(message, Message::Tool { .. }) && !tool_images.is_empty() {
            messages.push(tool_images_message(
                std::mem::take(&mut tool_images),
                &req.image_data,
            ));
        }
        if let Message::Tool {
            call_id, images, ..
        } = message
        {
            tool_images.extend(images.iter().map(|image| (call_id.as_str(), image)));
        }
        messages.push(wire_message(
            message,
            options.echo_reasoning,
            &req.model,
            options.endpoint,
            &req.image_data,
        )?);
    }
    if !tool_images.is_empty() {
        messages.push(tool_images_message(tool_images, &req.image_data));
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
        top_p: req.top_p,
        stream: stream.then_some(true),
        stream_options: stream.then(|| json!({"include_usage": true})),
        metadata: (!req.metadata.is_empty()).then_some(&req.metadata),
        prompt_cache_key: req
            .prompt_cache_key
            .as_deref()
            .filter(|key| options.prompt_cache_key && !key.is_empty()),
    };
    let not_serializable =
        |e| ModelError::invalid_request("request is not serializable").with_source(e);
    let extra = options.extra_body.filter(|extra| !extra.is_empty());
    let call_extra = req.extra_body.as_ref().filter(|extra| !extra.is_empty());
    if let Some(key) = RESERVED_BODY_KEYS
        .iter()
        .find(|key| call_extra.is_some_and(|extra| extra.contains_key(**key)))
    {
        return Err(ModelError::invalid_request(format!(
            "the request's extra body sets `{key}`, which the client owns"
        )));
    }
    if let Some(p) = req.top_p
        && !p.is_finite()
    {
        return Err(ModelError::invalid_request("top_p must be finite"));
    }
    let reasoning = reasoning_member(req, options.profiles)?;
    if extra.is_none() && call_extra.is_none() && reasoning.is_none() {
        return serde_json::to_vec(&wire).map_err(not_serializable);
    }
    // The deployment's own members (a flag that makes a gateway emit reasoning) are merged over
    // the body, at its top level. A typed level then removes what its wire would duplicate and
    // writes the model's own member (ADR 0063 decision 6).
    let mut body = serde_json::to_value(&wire).map_err(not_serializable)?;
    if let Value::Object(members) = &mut body {
        if let Some(extra) = extra {
            for (key, value) in extra {
                members.insert(key.clone(), value.clone());
            }
        }
        if let Some(call_extra) = call_extra {
            for (key, value) in call_extra {
                members.insert(key.clone(), value.clone());
            }
        }
        if let Some(reasoning) = reasoning {
            reasoning.apply(members);
        }
    }
    serde_json::to_vec(&body).map_err(not_serializable)
}

/// The member a typed level writes, once the model's profile says how (ADR 0063 decision 2).
enum ReasoningMember {
    /// `{"reasoning_effort": "<level>"}`.
    Effort(&'static str),
    /// `{"reasoning": {"effort": "<level>"}}`.
    Object(&'static str),
    /// `{"thinking": {"type": "enabled", "budget_tokens": <n>}}`.
    Thinking(u32),
}

/// What a request's [`reasoning_effort`](ModelRequest::reasoning_effort) writes, from the profile
/// of its model: `None` when the request has no level, the model has no profile, or the profile
/// declares no wire. The assembly refuses a level the model does not take, so the client is not a
/// second validator; it does refuse the two things `parse_json` guarantees and a hand-built profile
/// can break: a `thinking_budget` profile with no budget for the level, and `none` with that wire,
/// which has no disabled form.
fn reasoning_member(
    req: &ModelRequest,
    profiles: Option<&ModelProfiles>,
) -> Result<Option<ReasoningMember>, ModelError> {
    let Some(level) = req.reasoning_effort else {
        return Ok(None);
    };
    let Some(profile) = profiles.and_then(|profiles| profiles.profile(&req.model)) else {
        return Ok(None);
    };
    let Some(wire) = profile.reasoning_wire else {
        return Ok(None);
    };
    Ok(Some(match wire {
        ReasoningWire::ReasoningEffort => ReasoningMember::Effort(level.as_str()),
        ReasoningWire::ReasoningObject => ReasoningMember::Object(level.as_str()),
        ReasoningWire::ThinkingBudget => {
            // `none` is the level that means "do not reason", and the shape has no disabled form:
            // the loader refuses the pairing, and a hand-built profile reaches this, the one place
            // the invariant can be total.
            if level == ReasoningEffort::None {
                return Err(ModelError::invalid_request(format!(
                    "the model {:?} cannot send reasoning effort {:?} through the `thinking_budget` wire: the wire has no disabled form",
                    req.model,
                    level.as_str()
                )));
            }
            let budget = profile
                .reasoning_budgets
                .as_ref()
                .and_then(|budgets| budgets.get(&level).copied());
            let Some(budget) = budget else {
                return Err(ModelError::invalid_request(format!(
                    "the model {:?} has no thinking budget for reasoning effort {:?}",
                    req.model,
                    level.as_str()
                )));
            };
            ReasoningMember::Thinking(budget)
        }
    }))
}

impl ReasoningMember {
    /// Write the member, removing only what the extra body holds of the same fact: both
    /// `reasoning_effort` and `reasoning.effort` for the first two wires (an emptied `reasoning`
    /// object that held the effort goes), and `thinking.type` and `thinking.budget_tokens` for the
    /// third, whose object's other members stay (ADR 0063, amended 2026-11-28). Other members stay.
    fn apply(self, members: &mut serde_json::Map<String, Value>) {
        match self {
            Self::Effort(level) => {
                members.remove("reasoning_effort");
                let emptied = match members.get_mut("reasoning") {
                    Some(Value::Object(reasoning)) => {
                        // Only a member that held the effort "becomes empty": an object the extra
                        // body wrote empty stays.
                        let had_effort = reasoning.remove("effort").is_some();
                        had_effort && reasoning.is_empty()
                    }
                    _ => false,
                };
                if emptied {
                    members.remove("reasoning");
                }
                members.insert("reasoning_effort".to_owned(), json!(level));
            }
            Self::Object(level) => {
                members.remove("reasoning_effort");
                // A member that is not an object cannot hold `effort`: the typed level wins.
                let mut reasoning = members
                    .get("reasoning")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                reasoning.remove("effort");
                reasoning.insert("effort".to_owned(), json!(level));
                members.insert("reasoning".to_owned(), Value::Object(reasoning));
            }
            Self::Thinking(budget) => {
                // Only the two members the wire writes are the extra body's duplicate of the same
                // fact: a sibling such as `clear_thinking` (GLM keeps earlier turns' reasoning with
                // it) is the extra body's own and stays (ADR 0063, amended 2026-11-28). A member
                // that is not an object cannot hold them, so the typed level replaces it.
                let mut thinking = members
                    .get("thinking")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                // The two `insert`s below are the removal: they overwrite whatever the extra body
                // wrote of the same two members.
                thinking.insert("type".to_owned(), json!("enabled"));
                thinking.insert("budget_tokens".to_owned(), json!(budget));
                members.insert("thinking".to_owned(), Value::Object(thinking));
            }
        }
    }
}

fn text_message<'a>(role: &'static str, text: &str) -> WireMessage<'a> {
    WireMessage {
        role,
        content: Some(Value::String(text.to_owned())),
        tool_calls: Vec::new(),
        tool_call_id: None,
        reasoning: None,
        opaque: BTreeMap::new(),
    }
}

/// Message content: **one string**, the text parts joined with a blank line, for every message
/// whose parts are all text.
///
/// A user message that has several text parts (a continued conversation merges the messages that
/// would otherwise sit next to each other, see `Conversation::continued`) is stored with its parts
/// and sent as the text it says. A plain string is the form of `content` that every
/// OpenAI-compatible server takes; an array of typed parts is not taken by all of them, nor by
/// every chat template (*unverified* for each server: chat templates that insist on a string
/// content are the reason), so an array is sent only for a message that holds an image. The
/// [`ContentPart`] match fails to compile when a variant is added, which is where to send it.
fn content_value(parts: &[ContentPart], data: &ImageData) -> Option<Value> {
    if parts.is_empty() {
        return None;
    }
    if !parts
        .iter()
        .any(|part| matches!(part, ContentPart::Image(_)))
    {
        let text = parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => text.as_str(),
                ContentPart::Image(_) => "",
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        return Some(Value::String(text));
    }
    let mut typed = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ContentPart::Text { text } if text.is_empty() => {}
            ContentPart::Text { text } => typed.push(json!({"type": "text", "text": text})),
            ContentPart::Image(image) => typed.push(image_part(image, data)),
        }
    }
    Some(Value::Array(typed))
}

/// An image as a typed part: a data URL with the bytes, or, with no bytes to send, the line that
/// says so. No `detail` member is sent.
fn image_part(image: &ImageRef, data: &ImageData) -> Value {
    match data.get(&image.sha256) {
        Some(bytes) => json!({
            "type": "image_url",
            "image_url": {
                "url": format!(
                    "data:{};base64,{}",
                    image.media_type,
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                ),
            },
        }),
        None => json!({"type": "text", "text": image.missing_line()}),
    }
}

/// The user message that follows a run of tool messages whose results returned images.
fn tool_images_message<'a>(images: Vec<(&str, &ImageRef)>, data: &ImageData) -> WireMessage<'a> {
    let names = images
        .iter()
        .map(|(call, image)| format!("{call} {}", image.name.as_deref().unwrap_or("image")))
        .collect::<Vec<_>>()
        .join("; ");
    let mut parts = vec![json!({
        "type": "text",
        "text": format!("Images from tool results: {names}"),
    })];
    parts.extend(images.iter().map(|(_, image)| image_part(image, data)));
    WireMessage {
        role: "user",
        content: Some(Value::Array(parts)),
        tool_calls: Vec::new(),
        tool_call_id: None,
        reasoning: None,
        opaque: BTreeMap::new(),
    }
}

fn wire_message<'a>(
    message: &'a Message,
    echo: Option<ReasoningField>,
    model: &str,
    endpoint: &str,
    data: &ImageData,
) -> Result<WireMessage<'a>, ModelError> {
    Ok(match message {
        Message::User { content } => WireMessage {
            role: "user",
            // An empty user message is still a message; send an empty string.
            content: Some(
                content_value(content, data).unwrap_or_else(|| Value::String(String::new())),
            ),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning: None,
            opaque: BTreeMap::new(),
        },
        Message::Assistant {
            content,
            tool_calls,
            reasoning,
            provider,
            ..
        } => {
            // The provider's opaque members go back only to the model alias and endpoint that
            // produced them: a run that switched model drops them.
            let bound = provider
                .as_ref()
                .filter(|turn| turn.is_bound_to(model, endpoint));
            let mut wire_calls = Vec::with_capacity(tool_calls.len());
            for call in tool_calls {
                // A call whose text was not JSON goes back as `{}`: the text is not echoed.
                let arguments = if call.invalid_arguments.is_some() {
                    "{}".to_owned()
                } else {
                    serde_json::to_string(&call.arguments).map_err(|e| {
                        ModelError::invalid_request(format!(
                            "arguments of tool call {} are not serializable",
                            call.id
                        ))
                        .with_source(e)
                    })?
                };
                let mut object = serde_json::Map::with_capacity(3);
                object.insert("id".into(), Value::String(call.id.clone()));
                object.insert("type".into(), Value::String("function".into()));
                object.insert(
                    "function".into(),
                    json!({"name": call.name, "arguments": arguments}),
                );
                // The call's own members ride along only when its turn is bound; the modelled
                // keys win on collision.
                if bound.is_some()
                    && let Some(opaque) = &call.opaque
                {
                    for (key, value) in opaque {
                        if !CALL_MODELLED_KEYS.contains(&key.as_str()) {
                            object.entry(key.clone()).or_insert_with(|| value.clone());
                        }
                    }
                }
                wire_calls.push(Value::Object(object));
            }
            let content = content_value(content, data);
            WireMessage {
                role: "assistant",
                // With tool calls, `null` content is the canonical form; without,
                // send an empty string so the message stays well-formed.
                content: content
                    .or_else(|| tool_calls.is_empty().then(|| Value::String(String::new()))),
                tool_calls: wire_calls,
                tool_call_id: None,
                // Reasoning goes back only when the client was set to send it (a provider that
                // requires it); otherwise it is never in a request, whatever the message holds. A
                // raw reasoning block goes back unchanged in place of the text, and only to the
                // model that produced it.
                reasoning: echo.and_then(|field| {
                    let name = field.wire_name();
                    match bound.and_then(|turn| turn.reasoning.as_ref()) {
                        Some(raw) => Some(BTreeMap::from([(name, raw.clone())])),
                        None => reasoning
                            .as_deref()
                            .map(|text| BTreeMap::from([(name, Value::String(text.to_owned()))])),
                    }
                }),
                opaque: message_members(bound),
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
            reasoning: None,
            opaque: BTreeMap::new(),
        },
    })
}

/// The members a message carries beyond the modelled ones: a bound turn's own, and nothing else.
/// `Message::extra_content` is adam's namespace and is never sent (a provider's members are
/// bound to their model in `ProviderTurn`, and nowhere else), so a maker set there cannot reach
/// a model after a switch. adam's maker is stripped from the turn's own members too. Nothing
/// left means no `extra_content` member at all.
fn message_members(bound: Option<&ProviderTurn>) -> BTreeMap<String, Value> {
    let mut members: BTreeMap<String, Value> = bound
        .map(|turn| {
            turn.members
                .iter()
                .filter(|(key, _)| !MESSAGE_MODELLED_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    strip_adam_member(&mut members);
    members
}

// --------------------------------------------------------------- response --

/// Parse a non-streaming response body.
pub(crate) fn parse_completion(body: &[u8]) -> Result<ModelResponse, ModelError> {
    let value: Value = serde_json::from_slice(body).map_err(|e| {
        if e.is_eof() {
            ModelError::transient("response body ended early").with_source(e)
        } else {
            ModelError::protocol("response is not JSON").with_source(e)
        }
    })?;
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

    let ChoiceMessage {
        _role,
        content,
        tool_calls: calls,
        reasoning_content,
        reasoning,
        mut opaque,
        ..
    } = message;
    // adam's own maker is never kept from a model's reply: a model could otherwise hand adam a
    // value of the kind adam signs with.
    strip_adam_member(&mut opaque);

    let mut tool_calls = Vec::new();
    for call in calls.unwrap_or_default() {
        let WireToolCall {
            id,
            _type,
            function,
            opaque: mut call_opaque,
        } = call;
        strip_adam_member(&mut call_opaque);
        let name = function.name.unwrap_or_default();
        let id = id.filter(|id| !id.is_empty()).unwrap_or_else(fallback_id);
        let mut parsed = match function.arguments {
            None | Some(Value::Null) => ToolCall::new(id, name, json!({})),
            // Some servers send the arguments as an object instead of a string.
            Some(Value::String(raw)) => match parse_arguments(&raw) {
                Ok(arguments) => ToolCall::new(id, name, arguments),
                Err(raw) => {
                    check_name(&name)?;
                    let mut invalid = ToolCall::with_invalid_arguments(id, name, raw);
                    invalid.opaque = Some(call_opaque).filter(|opaque| !opaque.is_empty());
                    tool_calls.push(invalid);
                    continue;
                }
            },
            Some(arguments) => ToolCall::new(id, name, arguments),
        };
        check_name(&parsed.name)?;
        parsed.opaque = Some(call_opaque).filter(|opaque| !opaque.is_empty());
        tool_calls.push(parsed);
    }

    let calls_opaque = tool_calls.iter().any(|call| {
        call.opaque
            .as_ref()
            .is_some_and(|members| !members.is_empty())
    });
    let mut response = build_response(
        content_text(content),
        tool_calls,
        &finish,
        completion.usage.map(Usage::from).unwrap_or_default(),
    );
    response.reasoning = reasoning_text(&[reasoning_content.as_ref(), reasoning.as_ref()]);
    // The parse does not know the request's model alias or endpoint: the client stamps them. A
    // provider that attached nothing leaves the message exactly as before.
    let raw = raw_reasoning(&[reasoning_content.as_ref(), reasoning.as_ref()]);
    if (!opaque.is_empty() || raw.is_some() || calls_opaque)
        && let Message::Assistant { provider, .. } = &mut response.message
    {
        *provider = Some(ProviderTurn {
            model: String::new(),
            endpoint: String::new(),
            members: opaque,
            reasoning: raw,
        });
    }
    Ok(response)
}

/// The reasoning a provider sent, under whichever of the names it uses: the first that is a string
/// that is not empty. A provider that sends the same text under two names is read once, and a
/// value that is not a string (a structured `reasoning` some gateways add) is not reasoning text.
fn reasoning_text(candidates: &[Option<&Value>]) -> Option<String> {
    candidates
        .iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

/// The raw reasoning value when the provider sent an object or array (a reasoning block) instead
/// of text: the first such value, value for value. A string, a number or `null` is not a block.
fn raw_reasoning(candidates: &[Option<&Value>]) -> Option<Value> {
    candidates
        .iter()
        .flatten()
        .find(|value| matches!(**value, Value::Object(_) | Value::Array(_)))
        .map(|value| (**value).clone())
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
    /// Always `assistant`; modelled so it is not kept as an opaque member.
    #[serde(rename = "role")]
    _role: Option<String>,
    content: Option<Value>,
    tool_calls: Option<Vec<WireToolCall>>,
    /// DeepSeek, GLM, vLLM before 0.12, LiteLLM.
    reasoning_content: Option<Value>,
    /// OpenRouter, Ollama, current vLLM.
    reasoning: Option<Value>,
    /// Output-only members: a provider never needs them back, so they are consumed here and never
    /// kept as opaque (keeping one would change every response that carries it).
    #[serde(rename = "refusal")]
    _refusal: Option<Value>,
    #[serde(rename = "audio")]
    _audio: Option<Value>,
    #[serde(rename = "function_call")]
    _function_call: Option<Value>,
    #[serde(rename = "annotations")]
    _annotations: Option<Value>,
    /// The members this client does not model (`extra_content`, and siblings), value for value.
    #[serde(flatten)]
    opaque: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct WireToolCall {
    id: Option<String>,
    /// Always `function`; modelled so it is not kept as an opaque member.
    #[serde(rename = "type")]
    _type: Option<String>,
    function: WireFunction,
    /// The call's members this client does not model (`extra_content`, and siblings).
    #[serde(flatten)]
    opaque: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct WireFunction {
    name: Option<String>,
    arguments: Option<Value>,
}

/// `usage` of a completion, or of the last chunk of a stream: the totals and, where the provider
/// breaks them down, `completion_tokens_details.reasoning_tokens` ("Tokens generated by the model
/// for reasoning", counted in `completion_tokens`), `prompt_tokens_details.cached_tokens` ("Cached
/// tokens present in the prompt") and `prompt_tokens_details.cache_write_tokens` ("The unadjusted
/// number of prompt tokens written to cache"). *Verified 2026-10-08* in OpenAI's API reference,
/// <https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create>.
/// A detail that is absent, `null` or not a whole number leaves its part `None`.
#[derive(Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<Value>,
    #[serde(default)]
    completion_tokens_details: Option<Value>,
}

impl From<WireUsage> for Usage {
    fn from(u: WireUsage) -> Self {
        let detail = |details: &Option<Value>, name: &str| {
            details
                .as_ref()
                .and_then(|d| d.get(name))
                .and_then(Value::as_u64)
        };
        let mut usage = Usage::new(u.prompt_tokens, u.completion_tokens);
        usage.reasoning_tokens = detail(&u.completion_tokens_details, "reasoning_tokens");
        usage.cached_input_tokens = detail(&u.prompt_tokens_details, "cached_tokens");
        usage.cache_write_input_tokens = detail(&u.prompt_tokens_details, "cache_write_tokens");
        // A gateway that counts a part beside its total, not in it, has the part added in.
        usage.accounted()
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

/// An id for a tool call the provider gave none: `call_` and 16 random hexadecimal digits, new for
/// every call of every response. The ids `call_0`, `call_1`... that it used to be repeat in every
/// turn, and an id is what ties an answer (a person's click on a question, a tool's result) to its
/// call. The response is journaled with these ids by the agent, so a resumed run reads the ones that
/// were made, not new ones.
fn fallback_id() -> String {
    use std::hash::{BuildHasher as _, Hasher as _};
    use std::sync::atomic::{AtomicU64, Ordering};
    static MADE: AtomicU64 = AtomicU64::new(0);
    // `RandomState` is seeded from the system's randomness; the count keeps two ids of one process
    // apart whatever the hash does.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(MADE.fetch_add(1, Ordering::Relaxed));
    format!("call_{:016x}", hasher.finish())
}

fn check_name(name: &str) -> Result<(), ModelError> {
    if name.is_empty() {
        return Err(ModelError::protocol("tool call has no name"));
    }
    Ok(())
}

/// The most of an invalid argument text that is kept (the journal and the state hold it).
const MAX_INVALID_ARGUMENTS: usize = 8 * 1024;

/// Parse tool-call arguments. Empty means "no arguments"; text that is not JSON comes back as
/// `Err(raw)`, cut to [`MAX_INVALID_ARGUMENTS`] bytes on a char boundary, for the agent to answer
/// the call with an error instead of failing the run.
fn parse_arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|_| {
        if raw.len() <= MAX_INVALID_ARGUMENTS {
            return raw.to_owned();
        }
        let mut end = MAX_INVALID_ARGUMENTS;
        while !raw.is_char_boundary(end) {
            end -= 1;
        }
        format!("{} [{} bytes cut]", &raw[..end], raw.len() - end)
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
            extra_content: None,
            content,
            tool_calls,
            reasoning: None,
            provider: None,
        },
        finish,
        usage,
        reasoning: None,
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
    /// Some servers send the last chunk as a whole `message` instead of a `delta`. Only its
    /// opaque members are read: the stream's deltas already carry its text and calls.
    #[serde(default)]
    message: Option<Delta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    /// Always `assistant` on a delta or a message; modelled so it is not kept as an opaque member.
    #[serde(rename = "role")]
    _role: Option<String>,
    content: Option<String>,
    tool_calls: Option<Vec<DeltaToolCall>>,
    /// DeepSeek, GLM, vLLM before 0.12, LiteLLM. A `Value`, not a `String`: a chunk whose reasoning
    /// is some other shape is still a chunk (its content and calls count).
    reasoning_content: Option<Value>,
    /// OpenRouter, Ollama, current vLLM.
    reasoning: Option<Value>,
    /// Output-only members: consumed, never opaque (see [`ChoiceMessage`]).
    #[serde(rename = "refusal")]
    _refusal: Option<Value>,
    #[serde(rename = "audio")]
    _audio: Option<Value>,
    #[serde(rename = "function_call")]
    _function_call: Option<Value>,
    #[serde(rename = "annotations")]
    _annotations: Option<Value>,
    /// Members this client does not model; a delta's and a `message`-shaped chunk's are both read
    /// (through [`STREAM_MESSAGE_MODELLED_KEYS`]).
    #[serde(flatten)]
    opaque: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct DeltaToolCall {
    /// Position of the call within the message; deltas of one call share it.
    #[serde(default)]
    index: Option<usize>,
    id: Option<String>,
    /// Always `function`; modelled so it is not kept as an opaque member.
    #[serde(rename = "type")]
    _type: Option<String>,
    function: Option<DeltaFunction>,
    /// The call's members this client does not model, accumulated per call.
    #[serde(flatten)]
    opaque: BTreeMap<String, Value>,
}

/// The members of an assistant message the streamed format models: a member of a delta or a
/// `message`-shaped chunk with one of these names is not opaque. The keys of
/// [`MESSAGE_MODELLED_KEYS`], so the two paths stay aligned; `tool_call_id` is not one of them: it
/// cannot collide with an assistant message's fields and replaying it is harmless.
const STREAM_MESSAGE_MODELLED_KEYS: [&str; 9] = MESSAGE_MODELLED_KEYS;

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
    /// The call's opaque members, last value wins per key.
    opaque: BTreeMap<String, Value>,
    started: bool,
    /// [`ModelDelta::ToolCallComplete`] was said for it: a client says it once per call.
    announced: bool,
}

impl PartialCall {
    /// The call as the assembled response holds it, with its id fixed (the one the early
    /// `ToolCallComplete` carried is the one [`Assembler::finish`] writes).
    fn value(&mut self) -> Result<ToolCall, ModelError> {
        check_name(&self.name)?;
        let id = self.id.get_or_insert_with(fallback_id).clone();
        let mut call = match parse_arguments(&self.arguments) {
            Ok(arguments) => ToolCall::new(id, self.name.clone(), arguments),
            Err(raw) => ToolCall::with_invalid_arguments(id, self.name.clone(), raw),
        };
        // Cloned, not taken: the early `ToolCallComplete` and the finished response carry the same
        // members.
        let mut opaque = self.opaque.clone();
        strip_adam_member(&mut opaque);
        call.opaque = (!opaque.is_empty()).then_some(opaque);
        Ok(call)
    }

    /// Whether the argument text is whole: it parses as a JSON **object**. Empty text is not: the
    /// parser reads it as `{}` ([`parse_arguments`]), which a provider that sends the name first and
    /// the arguments after has not finished saying.
    fn arguments_are_complete(&self) -> bool {
        !self.arguments.trim().is_empty()
            && matches!(parse_arguments(&self.arguments), Ok(Value::Object(_)))
    }
}

/// Assembles streamed chunks into a [`ModelResponse`], keyed by tool-call
/// index. Feed it each parsed SSE payload with [`Assembler::push`]; call
/// [`Assembler::finish`] when the stream ends.
#[derive(Default)]
pub(crate) struct Assembler {
    text: String,
    reasoning: String,
    /// The raw reasoning value of a block the provider sent instead of text, last wins.
    reasoning_raw: Option<Value>,
    /// The assistant message's opaque members, from a delta or a `message`-shaped chunk.
    message_members: BTreeMap<String, Value>,
    calls: BTreeMap<usize, PartialCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl Assembler {
    /// Consume one SSE `data:` payload (not `[DONE]`), returning the deltas to
    /// forward to the caller.
    pub(crate) fn push(&mut self, data: &str) -> Result<Vec<ModelDelta>, ModelError> {
        self.push_event(data, false)
    }

    /// Like [`Assembler::push`] for the event handed over when the connection closed without its
    /// terminator: a payload cut off mid-JSON is a truncated response, so it is transient.
    pub(crate) fn push_last(&mut self, data: &str) -> Result<Vec<ModelDelta>, ModelError> {
        self.push_event(data, true)
    }

    fn push_event(&mut self, data: &str, last: bool) -> Result<Vec<ModelDelta>, ModelError> {
        let value: Value = serde_json::from_str(data).map_err(|e| {
            if last && e.is_eof() {
                ModelError::transient("stream ended inside a chunk").with_source(e)
            } else {
                ModelError::protocol("stream chunk is not JSON").with_source(e)
            }
        })?;
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
                if let Some(thought) =
                    reasoning_text(&[delta.reasoning_content.as_ref(), delta.reasoning.as_ref()])
                {
                    self.reasoning.push_str(&thought);
                    out.push(ModelDelta::Reasoning(thought));
                }
                if let Some(raw) =
                    raw_reasoning(&[delta.reasoning_content.as_ref(), delta.reasoning.as_ref()])
                {
                    self.reasoning_raw = Some(raw);
                }
                if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
                    self.text.push_str(&text);
                    out.push(ModelDelta::Text(text));
                }
                for (position, call) in delta.tool_calls.unwrap_or_default().into_iter().enumerate()
                {
                    self.push_tool_call(position, call, &mut out);
                }
                // A delta can carry the message-level members too (the same filter as a
                // message-shaped chunk): a provider may attach them to a delta rather than a final
                // `message`.
                for (key, value) in delta.opaque {
                    if !STREAM_MESSAGE_MODELLED_KEYS.contains(&key.as_str()) {
                        self.message_members.insert(key, value);
                    }
                }
            }
            // A chunk shaped as a whole message rather than a delta: its opaque members are the
            // assistant message's, its text and calls are already in the deltas.
            if let Some(message) = choice.message {
                for (key, value) in message.opaque {
                    if !STREAM_MESSAGE_MODELLED_KEYS.contains(&key.as_str()) {
                        self.message_members.insert(key, value);
                    }
                }
                if let Some(raw) = raw_reasoning(&[
                    message.reasoning_content.as_ref(),
                    message.reasoning.as_ref(),
                ]) {
                    self.reasoning_raw = Some(raw);
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(reason);
            }
        }
        Ok(out)
    }

    /// One tool call of a chunk: say that it started when its name is first whole, and that its
    /// arguments are complete the first time they parse as an object.
    fn push_tool_call(&mut self, position: usize, delta: DeltaToolCall, out: &mut Vec<ModelDelta>) {
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
        // Last value wins per key; a member seen once is kept.
        for (key, value) in delta.opaque {
            call.opaque.insert(key, value);
        }
        if !call.started && !call.name.is_empty() {
            call.started = true;
            let id = call.id.get_or_insert_with(fallback_id).clone();
            out.push(ModelDelta::ToolCallStarted {
                id,
                name: call.name.clone(),
            });
        }
        // The arguments are whole the first time they parse as an object: the call can start. A
        // call with no name yet is not a call (`finish` refuses it): its arguments wait for the
        // name, and the announcement is not lost.
        if !call.announced && !call.name.is_empty() && call.arguments_are_complete() {
            call.announced = true;
            if let Ok(value) = call.value() {
                out.push(ModelDelta::ToolCallComplete(value));
            }
        }
    }

    /// The calls whose arguments were never announced while they streamed (an empty argument text,
    /// or one that never parses as an object): the deltas that say them, for the caller to send
    /// immediately before [`ModelDelta::Finished`]. A call with no name is left out: it is not a
    /// call, and [`Assembler::finish`] refuses it.
    pub(crate) fn remaining(&mut self) -> Vec<ModelDelta> {
        let mut out = Vec::new();
        for call in self.calls.values_mut() {
            if call.announced || call.name.is_empty() {
                continue;
            }
            call.announced = true;
            if let Ok(value) = call.value() {
                out.push(ModelDelta::ToolCallComplete(value));
            }
        }
        out
    }

    /// Finish the stream. Fails if the server never said why it stopped,
    /// rather than fabricating a response from a truncated stream.
    pub(crate) fn finish(mut self) -> Result<ModelResponse, ModelError> {
        let reason = self
            .finish_reason
            .ok_or_else(|| ModelError::transient("stream ended without a finish_reason"))?;
        let mut tool_calls = Vec::with_capacity(self.calls.len());
        for call in self.calls.values_mut() {
            tool_calls.push(call.value()?);
        }
        let mut response = build_response(
            self.text,
            tool_calls,
            &reason,
            self.usage.unwrap_or_default(),
        );
        response.reasoning = Some(self.reasoning).filter(|r| !r.is_empty());
        // The parse does not know the request's model alias or endpoint: the client stamps them.
        strip_adam_member(&mut self.message_members);
        let calls_opaque = response
            .message
            .tool_calls()
            .iter()
            .any(|c| c.opaque.as_ref().is_some_and(|members| !members.is_empty()));
        if (!self.message_members.is_empty() || self.reasoning_raw.is_some() || calls_opaque)
            && let Message::Assistant { provider, .. } = &mut response.message
        {
            *provider = Some(ProviderTurn {
                model: String::new(),
                endpoint: String::new(),
                members: self.message_members,
                reasoning: self.reasoning_raw,
            });
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use adam_model::{ModelProfile, ReasoningEffort, ReasoningWire, ToolSpec};

    use super::*;

    fn request() -> ModelRequest {
        let mut req = ModelRequest::new("gw-model");
        req.system = Some("sys".into());
        req.messages = vec![
            Message::user_text("hi"),
            Message::Assistant {
                extra_content: None,
                content: vec![],
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "weather".into(),
                    arguments: json!({"city": "Paris"}),
                    ..Default::default()
                }],
                reasoning: None,
                provider: None,
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

    fn options(max_tokens_field: MaxTokensField) -> RequestOptions<'static> {
        RequestOptions {
            max_tokens_field,
            ..RequestOptions::default()
        }
    }

    fn body(req: &ModelRequest, stream: bool, f: MaxTokensField) -> Value {
        serde_json::from_slice(&build_request(req, stream, options(f)).unwrap()).unwrap()
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

    fn with(extra: Option<Value>, echo: Option<ReasoningField>) -> RequestOptions<'static> {
        let extra = extra.map(|v| match v {
            Value::Object(map) => &*Box::leak(Box::new(map)),
            other => panic!("not an object: {other}"),
        });
        RequestOptions {
            extra_body: extra,
            echo_reasoning: echo,
            ..RequestOptions::default()
        }
    }

    #[test]
    fn a_requests_top_p_and_extra_body_are_sent_and_a_reserved_member_is_refused() {
        let mut req = request();
        req.top_p = Some(0.5);
        let mut body = serde_json::Map::new();
        body.insert("seed".into(), json!(7));
        req.extra_body = Some(body);
        let sent: Value =
            serde_json::from_slice(&build_request(&req, false, RequestOptions::default()).unwrap())
                .unwrap();
        assert_eq!(sent["top_p"], json!(0.5));
        assert_eq!(sent["seed"], json!(7));

        let mut reserved = serde_json::Map::new();
        reserved.insert("model".into(), json!("other"));
        req.extra_body = Some(reserved);
        assert!(build_request(&req, false, RequestOptions::default()).is_err());
    }

    #[test]
    fn an_extra_body_is_merged_over_every_request_at_its_top_level() {
        let req = request();
        let extra = json!({"reasoning_effort": "medium", "chat_template_kwargs": {"enable_thinking": true}});
        for stream in [false, true] {
            let sent: Value = serde_json::from_slice(
                &build_request(&req, stream, with(Some(extra.clone()), None)).unwrap(),
            )
            .unwrap();
            assert_eq!(sent["reasoning_effort"], "medium");
            assert_eq!(
                sent["chat_template_kwargs"],
                json!({"enable_thinking": true})
            );
            // The rest of the body is what it was.
            assert_eq!(sent["model"], "gw-model");
            assert_eq!(sent["messages"].as_array().map(Vec::len), Some(4));
        }
        // It wins over what the client wrote (`max_tokens` here), and an empty one changes nothing.
        let mut req = request();
        req.max_output_tokens = Some(10);
        let sent: Value = serde_json::from_slice(
            &build_request(&req, false, with(Some(json!({"max_tokens": 99})), None)).unwrap(),
        )
        .unwrap();
        assert_eq!(sent["max_tokens"], 99);
        assert_eq!(
            build_request(&req, false, with(Some(json!({})), None)).unwrap(),
            build_request(&req, false, with(None, None)).unwrap()
        );
    }

    fn cached(on: bool, extra: Option<Value>) -> RequestOptions<'static> {
        RequestOptions {
            prompt_cache_key: on,
            ..with(extra, None)
        }
    }

    fn profile_of(efforts: &[ReasoningEffort], wire: ReasoningWire) -> ModelProfile {
        ModelProfile {
            reasoning_efforts: Some(efforts.to_vec()),
            reasoning_wire: Some(wire),
            ..ModelProfile::default()
        }
    }

    fn profiles_of(model: &str, profile: ModelProfile) -> ModelProfiles {
        ModelProfiles {
            profiles: BTreeMap::from([(model.to_owned(), profile)]),
            presets: BTreeMap::new(),
        }
    }

    fn body_of(req: &ModelRequest, options: RequestOptions<'_>) -> Value {
        serde_json::from_slice(&build_request(req, false, options).unwrap()).unwrap()
    }

    /// The ADR's pinned order: the typed level is applied **after** the extra body, so it wins.
    #[test]
    fn a_typed_level_wins_over_the_extra_body_and_keeps_its_other_members() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::Low);
        let profiles = profiles_of(
            "gw-model",
            profile_of(
                &[ReasoningEffort::Low, ReasoningEffort::Medium],
                ReasoningWire::ReasoningEffort,
            ),
        );
        let extra = json!({"reasoning_effort": "medium", "chat_template_kwargs": {"enable_thinking": true}});
        for stream in [false, true] {
            let sent: Value = serde_json::from_slice(
                &build_request(
                    &req,
                    stream,
                    RequestOptions {
                        profiles: Some(&profiles),
                        ..with(Some(extra.clone()), None)
                    },
                )
                .unwrap(),
            )
            .unwrap();
            // Apply the level before the extra-body merge and this is `medium`.
            assert_eq!(sent["reasoning_effort"], "low", "the typed level wins");
            assert_eq!(
                sent["chat_template_kwargs"],
                json!({"enable_thinking": true}),
                "the other members stay"
            );
            assert_eq!(sent["model"], "gw-model");
        }
    }

    #[test]
    fn each_wire_writes_its_own_member() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::High);
        // Every wire's member, so each arm proves what it removed and what it left.
        let extra = json!({
            "reasoning_effort": "medium",
            "reasoning": {"effort": "medium", "summary": "auto"},
            "thinking": {"type": "enabled", "budget_tokens": 999, "clear_thinking": false}
        });
        for wire in ReasoningWire::ALL {
            let mut profile = profile_of(&[ReasoningEffort::High], wire);
            if wire == ReasoningWire::ThinkingBudget {
                profile.reasoning_budgets = Some(BTreeMap::from([(ReasoningEffort::High, 8192)]));
            }
            let profiles = profiles_of("gw-model", profile);
            let sent = body_of(
                &req,
                RequestOptions {
                    profiles: Some(&profiles),
                    ..with(Some(extra.clone()), None)
                },
            );
            match wire {
                ReasoningWire::ReasoningEffort => {
                    assert_eq!(sent["reasoning_effort"], "high", "{sent}");
                    assert_eq!(
                        sent["reasoning"],
                        json!({"summary": "auto"}),
                        "the effort goes, the object's other members stay: {sent}"
                    );
                    assert_eq!(
                        sent["thinking"],
                        json!({"type": "enabled", "budget_tokens": 999, "clear_thinking": false}),
                        "a member of another wire stays: {sent}"
                    );
                }
                ReasoningWire::ReasoningObject => {
                    assert_eq!(
                        sent["reasoning"],
                        json!({"summary": "auto", "effort": "high"}),
                        "{sent}"
                    );
                    assert!(
                        sent.get("reasoning_effort").is_none(),
                        "the other member of the same fact is removed: {sent}"
                    );
                    assert_eq!(
                        sent["thinking"],
                        json!({"type": "enabled", "budget_tokens": 999, "clear_thinking": false}),
                        "{sent}"
                    );
                }
                ReasoningWire::ThinkingBudget => {
                    assert_eq!(
                        sent["thinking"],
                        json!({"type": "enabled", "budget_tokens": 8192, "clear_thinking": false}),
                        "the wire's two members are written, the object's sibling stays: {sent}"
                    );
                    assert_eq!(
                        sent["reasoning_effort"], "medium",
                        "the wire does not write `reasoning_effort`, so the extra body's stays: {sent}"
                    );
                    assert_eq!(
                        sent["reasoning"],
                        json!({"effort": "medium", "summary": "auto"}),
                        "{sent}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_empty_reasoning_object_the_extra_body_wrote_stays() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::Low);
        let profiles = profiles_of(
            "gw-model",
            profile_of(&[ReasoningEffort::Low], ReasoningWire::ReasoningEffort),
        );
        let sent = body_of(
            &req,
            RequestOptions {
                profiles: Some(&profiles),
                ..with(Some(json!({"reasoning": {}})), None)
            },
        );
        assert_eq!(
            sent["reasoning"],
            json!({}),
            "an object that held no effort does not 'become empty': {sent}"
        );
        assert_eq!(sent["reasoning_effort"], "low");
    }

    #[test]
    fn an_emptied_reasoning_object_is_removed_and_its_other_members_stay() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::Low);
        let profiles = profiles_of(
            "gw-model",
            profile_of(&[ReasoningEffort::Low], ReasoningWire::ReasoningEffort),
        );
        let sent = body_of(
            &req,
            RequestOptions {
                profiles: Some(&profiles),
                ..with(Some(json!({"reasoning": {"effort": "medium"}})), None)
            },
        );
        assert_eq!(sent["reasoning_effort"], "low");
        assert!(sent.get("reasoning").is_none(), "{sent}");

        let sent = body_of(
            &req,
            RequestOptions {
                profiles: Some(&profiles),
                ..with(
                    Some(json!({"reasoning": {"effort": "medium", "summary": "auto"}})),
                    None,
                )
            },
        );
        assert_eq!(sent["reasoning"], json!({"summary": "auto"}), "{sent}");
        assert_eq!(sent["reasoning_effort"], "low");
    }

    #[test]
    fn the_reasoning_object_wire_keeps_the_objects_other_members() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::Low);
        let profiles = profiles_of(
            "gw-model",
            profile_of(&[ReasoningEffort::Low], ReasoningWire::ReasoningObject),
        );
        let sent = body_of(
            &req,
            RequestOptions {
                profiles: Some(&profiles),
                ..with(
                    Some(json!({
                        "reasoning_effort": "medium",
                        "reasoning": {"effort": "medium", "summary": "auto"}
                    })),
                    None,
                )
            },
        );
        assert_eq!(
            sent["reasoning"],
            json!({"summary": "auto", "effort": "low"})
        );
        assert!(
            sent.get("reasoning_effort").is_none(),
            "the wire's duplicate is removed: {sent}"
        );
    }

    /// The wire writes `thinking.type` and `thinking.budget_tokens` and leaves the object's other
    /// members alone (ADR 0063, amended 2026-11-28): a provider's sibling such as `clear_thinking`
    /// survives. Red with `members.remove("thinking")`, which dropped it.
    #[test]
    fn the_thinking_budget_wire_keeps_a_thinking_objects_other_members() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::Low);
        let mut profile = profile_of(&[ReasoningEffort::Low], ReasoningWire::ThinkingBudget);
        profile.reasoning_budgets = Some(BTreeMap::from([(ReasoningEffort::Low, 1024)]));
        let profiles = profiles_of("gw-model", profile);
        let sent = body_of(
            &req,
            RequestOptions {
                profiles: Some(&profiles),
                ..with(
                    Some(json!({"thinking": {
                        "type": "enabled",
                        "budget_tokens": 999,
                        "clear_thinking": false
                    }})),
                    None,
                )
            },
        );
        assert_eq!(
            sent["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024, "clear_thinking": false}),
            "{sent}"
        );
    }

    /// A `thinking` that is not an object cannot hold the wire's members: the typed level replaces it.
    #[test]
    fn the_thinking_budget_wire_replaces_a_thinking_that_is_not_an_object() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::Low);
        let mut profile = profile_of(&[ReasoningEffort::Low], ReasoningWire::ThinkingBudget);
        profile.reasoning_budgets = Some(BTreeMap::from([(ReasoningEffort::Low, 1024)]));
        let profiles = profiles_of("gw-model", profile);
        for thinking in [json!("enabled"), json!([1, 2]), json!(null)] {
            let sent = body_of(
                &req,
                RequestOptions {
                    profiles: Some(&profiles),
                    ..with(Some(json!({"thinking": thinking.clone()})), None)
                },
            );
            assert_eq!(
                sent["thinking"],
                json!({"type": "enabled", "budget_tokens": 1024}),
                "{thinking} cannot hold the wire's members: {sent}"
            );
        }
    }

    #[test]
    fn none_is_a_level_that_is_sent_and_an_absent_one_is_not() {
        let profiles = profiles_of(
            "gw-model",
            profile_of(
                &[ReasoningEffort::None, ReasoningEffort::Low],
                ReasoningWire::ReasoningEffort,
            ),
        );
        let mut req = request();
        let absent = build_request(
            &req,
            false,
            RequestOptions {
                profiles: Some(&profiles),
                ..RequestOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            absent,
            build_request(&req, false, RequestOptions::default()).unwrap(),
            "no level sends the bytes of no profile"
        );
        req.reasoning_effort = Some(ReasoningEffort::None);
        let sent = body_of(
            &req,
            RequestOptions {
                profiles: Some(&profiles),
                ..RequestOptions::default()
            },
        );
        assert_eq!(sent["reasoning_effort"], "none", "{sent}");
    }

    #[test]
    fn without_a_profile_a_wire_or_a_level_the_body_is_byte_identical() {
        let req = request();
        let plain = build_request(&req, false, RequestOptions::default()).unwrap();
        // A level and no profiles.
        let mut levelled = request();
        levelled.reasoning_effort = Some(ReasoningEffort::High);
        assert_eq!(
            build_request(&levelled, false, RequestOptions::default()).unwrap(),
            plain
        );
        // A profile with no wire (a cap only).
        let capped = profiles_of(
            "gw-model",
            ModelProfile {
                max_output_tokens: Some(1000),
                ..ModelProfile::default()
            },
        );
        assert_eq!(
            build_request(
                &levelled,
                false,
                RequestOptions {
                    profiles: Some(&capped),
                    ..RequestOptions::default()
                }
            )
            .unwrap(),
            plain
        );
        // A profile with a wire, and no level.
        let wired = profiles_of(
            "gw-model",
            profile_of(&[ReasoningEffort::High], ReasoningWire::ReasoningEffort),
        );
        for stream in [false, true] {
            assert_eq!(
                build_request(
                    &req,
                    stream,
                    RequestOptions {
                        profiles: Some(&wired),
                        ..RequestOptions::default()
                    }
                )
                .unwrap(),
                build_request(&req, stream, RequestOptions::default()).unwrap()
            );
        }
    }

    #[test]
    fn a_thinking_budget_with_no_budget_names_the_model_and_the_level() {
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::High);
        for budgets in [None, Some(BTreeMap::from([(ReasoningEffort::Low, 1024)]))] {
            let mut profile = profile_of(&[ReasoningEffort::High], ReasoningWire::ThinkingBudget);
            profile.reasoning_budgets = budgets;
            let profiles = profiles_of("gw-model", profile);
            let err = build_request(
                &req,
                false,
                RequestOptions {
                    profiles: Some(&profiles),
                    ..RequestOptions::default()
                },
            )
            .expect_err("no budget for the level");
            assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
            assert!(err.to_string().contains("gw-model"), "{err}");
            assert!(err.to_string().contains("high"), "{err}");
        }
    }

    #[test]
    fn a_hand_built_thinking_budget_profile_refuses_none() {
        // `parse_json` refuses this pairing; a hand-built profile (public fields) bypasses it, so
        // the client refuses it too: the wire has no disabled form.
        let mut req = request();
        req.reasoning_effort = Some(ReasoningEffort::None);
        let mut profile = profile_of(
            &[ReasoningEffort::None, ReasoningEffort::Low],
            ReasoningWire::ThinkingBudget,
        );
        profile.reasoning_budgets = Some(BTreeMap::from([
            (ReasoningEffort::None, 1),
            (ReasoningEffort::Low, 1024),
        ]));
        let profiles = profiles_of("gw-model", profile);
        let err = build_request(
            &req,
            false,
            RequestOptions {
                profiles: Some(&profiles),
                ..RequestOptions::default()
            },
        )
        .expect_err("none through thinking_budget");
        assert!(matches!(err, ModelError::InvalidRequest { .. }), "{err:?}");
        assert!(err.to_string().contains("gw-model"), "{err}");
        assert!(err.to_string().contains("none"), "{err}");
        assert!(err.to_string().contains("no disabled form"), "{err}");
    }

    #[test]
    fn prompt_cache_key_is_sent_only_when_set() {
        let mut req = request();
        let key = |req: &ModelRequest, on: bool| -> Option<Value> {
            let sent: Value =
                serde_json::from_slice(&build_request(req, false, cached(on, None)).unwrap())
                    .unwrap();
            sent.get("prompt_cache_key").cloned()
        };
        // No key on the request: nothing is sent, whatever the client is set to.
        assert_eq!(key(&req, true), None);
        assert_eq!(key(&req, false), None);
        req.prompt_cache_key = Some("0123456789abcdef0123456789abcdef".into());
        assert_eq!(
            key(&req, true),
            Some(json!("0123456789abcdef0123456789abcdef"))
        );
        // A client that is not set to send it drops it, and the body is the one without.
        assert_eq!(key(&req, false), None);
        assert_eq!(
            build_request(&req, false, cached(false, None)).unwrap(),
            build_request(&request(), false, cached(false, None)).unwrap()
        );
        // An empty key is no key.
        req.prompt_cache_key = Some(String::new());
        assert_eq!(key(&req, true), None);
        // A request with no key serialises as it did before the field existed (no new member).
        assert!(
            !String::from_utf8(build_request(&request(), true, cached(true, None)).unwrap())
                .unwrap()
                .contains("prompt_cache_key")
        );
    }

    #[test]
    fn an_extra_body_member_of_the_same_name_wins_over_the_cache_key() {
        let mut req = request();
        req.prompt_cache_key = Some("from-the-loop".into());
        let sent: Value = serde_json::from_slice(
            &build_request(
                &req,
                false,
                cached(
                    true,
                    Some(json!({"prompt_cache_key": "from-the-deployment"})),
                ),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(sent["prompt_cache_key"], "from-the-deployment");
        // And with the client off, the deployment's member is still sent as written.
        let sent: Value = serde_json::from_slice(
            &build_request(
                &req,
                false,
                cached(
                    false,
                    Some(json!({"prompt_cache_key": "from-the-deployment"})),
                ),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(sent["prompt_cache_key"], "from-the-deployment");
    }

    /// The body of a request that appends messages to another's starts with the other's bytes
    /// (through the last of the shared messages), and what follows the messages is the same.
    #[test]
    fn appending_messages_keeps_the_body_prefix() {
        let first = request();
        let mut second = request();
        second.messages.push(Message::assistant_text("it is sunny"));
        second.messages.push(Message::user_text("and tomorrow?"));
        for stream in [false, true] {
            let a = String::from_utf8(
                build_request(&first, stream, options(MaxTokensField::MaxTokens)).unwrap(),
            )
            .unwrap();
            let b = String::from_utf8(
                build_request(&second, stream, options(MaxTokensField::MaxTokens)).unwrap(),
            )
            .unwrap();
            let end_a = a.find("],\"tools\":").expect("the messages end");
            let end_b = b.find("],\"tools\":").expect("the messages end");
            assert!(
                b[..end_b].starts_with(&a[..end_a]),
                "the shared messages are the same bytes: {a}\n{b}"
            );
            assert_eq!(
                a[end_a..],
                b[end_b..],
                "what follows the messages is the same"
            );
        }
    }

    fn thinking_history() -> ModelRequest {
        let mut req = request();
        req.messages[1] = Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "weather".into(),
                arguments: json!({"city": "Paris"}),
                ..Default::default()
            }],
            reasoning: Some("I should look it up".into()),
            provider: None,
        };
        req
    }

    #[test]
    fn reasoning_in_a_history_is_never_sent_unless_the_client_echoes_it() {
        let req = thinking_history();
        let plain = String::from_utf8(
            build_request(&req, false, options(MaxTokensField::MaxTokens)).unwrap(),
        )
        .unwrap();
        assert!(!plain.contains("reasoning"), "{plain}");
        assert!(!plain.contains("I should look it up"), "{plain}");

        for (field, name) in [
            (ReasoningField::ReasoningContent, "reasoning_content"),
            (ReasoningField::Reasoning, "reasoning"),
        ] {
            let sent: Value = serde_json::from_slice(
                &build_request(&req, false, with(None, Some(field))).unwrap(),
            )
            .unwrap();
            assert_eq!(sent["messages"][2][name], "I should look it up");
            // Only on the assistant message that holds it.
            assert!(sent["messages"][1].get(name).is_none());
            assert!(sent["messages"][3].get(name).is_none());
        }
    }

    #[test]
    fn a_completion_carries_its_reasoning_under_either_name_and_not_in_the_text() {
        let parse = |message: Value| {
            parse_completion(
                json!({"choices": [{"message": message, "finish_reason": "stop"}]})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap()
        };
        let r = parse(json!({"content": "42", "reasoning_content": "6 times 7"}));
        assert_eq!(r.reasoning.as_deref(), Some("6 times 7"));
        assert_eq!(r.message.text(), "42");
        assert_eq!(r.message.reasoning(), None);
        let r = parse(json!({"content": "42", "reasoning": "six sevens"}));
        assert_eq!(r.reasoning.as_deref(), Some("six sevens"));
        // The same text under both names is read once; empty and null are none; a structure is not text.
        let r = parse(json!({"content": "42", "reasoning_content": "t", "reasoning": "t"}));
        assert_eq!(r.reasoning.as_deref(), Some("t"));
        for none in [
            json!({"content": "42"}),
            json!({"content": "42", "reasoning_content": null, "reasoning": ""}),
            json!({"content": "42", "reasoning": {"summary": []}}),
        ] {
            assert_eq!(parse(none).reasoning, None);
        }
    }

    #[test]
    fn streamed_reasoning_arrives_before_the_answer_and_is_not_the_answer() {
        let mut asm = Assembler::default();
        let mut deltas = Vec::new();
        for chunk in [
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"","reasoning_content":"The user "}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"says hi."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning":" Then greet."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":null,"content":"Hello"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning":{"x":1},"content":"!"},"finish_reason":"stop"}]}"#,
        ] {
            deltas.extend(asm.push(chunk).unwrap());
        }
        assert_eq!(
            deltas,
            vec![
                ModelDelta::Reasoning("The user ".into()),
                ModelDelta::Reasoning("says hi.".into()),
                ModelDelta::Reasoning(" Then greet.".into()),
                ModelDelta::Text("Hello".into()),
                ModelDelta::Text("!".into()),
            ]
        );
        let response = asm.finish().unwrap();
        assert_eq!(response.message.text(), "Hello!");
        assert_eq!(response.message.reasoning(), None);
        assert_eq!(
            response.reasoning.as_deref(),
            Some("The user says hi. Then greet.")
        );
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
            build_request(&req, false, options(MaxTokensField::MaxTokens)),
            Err(ModelError::InvalidRequest { .. })
        ));
        let mut req = ModelRequest::new("m");
        req.temperature = Some(f32::NAN);
        assert!(matches!(
            build_request(&req, false, options(MaxTokensField::MaxTokens)),
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
        assert_eq!(r.usage, Usage::new(3, 4));
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
    fn a_complete_completion_without_a_finish_reason_is_a_protocol_error() {
        for body in [
            &br#"{"choices":[{"message":{"content":"x"}}]}"#[..],
            br#"{"choices":[{"message":{"content":"x"},"finish_reason":null}]}"#,
        ] {
            assert!(
                matches!(parse_completion(body), Err(ModelError::Protocol { .. })),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn a_long_invalid_argument_text_is_cut_on_a_char_boundary() {
        // Multi-byte characters around the cut: 8192 is inside one of them.
        let raw = format!("{{\"a\": {}", "\u{e9}\u{4e2d}".repeat(10_000));
        assert!(raw.len() > 30_000);
        let kept = parse_arguments(&raw).unwrap_err();
        let (text, note) = kept.split_once(" [").expect("a marker");
        assert!(text.len() <= MAX_INVALID_ARGUMENTS && raw.starts_with(text));
        assert_eq!(note, format!("{} bytes cut]", raw.len() - text.len()));
        // And a short one is untouched.
        assert_eq!(parse_arguments("{oops").unwrap_err(), "{oops");
    }

    #[test]
    fn an_invalid_call_is_echoed_as_empty_arguments() {
        let call = adam_model::ToolCall::with_invalid_arguments("c", "w", "{oops");
        let message = Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![call],
            reasoning: None,
            provider: None,
        };
        let wire =
            wire_message(&message, None, "gw-model", "openai:test", &ImageData::new()).unwrap();
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["tool_calls"][0]["function"]["arguments"], "{}");
        let text = json.to_string();
        assert!(
            !text.contains("oops") && !text.contains("valid JSON"),
            "{text}"
        );
    }

    #[test]
    fn a_bound_turns_members_go_back_and_the_modelled_keys_win() {
        let mut provider = ProviderTurn::new("gw-model", "openai:test");
        provider.members = BTreeMap::from([
            ("role".into(), json!("evil")),
            (
                "extra_content".into(),
                json!({"google": {"thought_signature": "msg"}}),
            ),
        ]);
        let call = ToolCall {
            id: "c".into(),
            name: "weather".into(),
            arguments: json!({}),
            opaque: Some(BTreeMap::from([
                ("id".into(), json!("evil")),
                ("type".into(), json!("evil")),
                ("function".into(), json!("evil")),
                (
                    "extra_content".into(),
                    json!({"google": {"thought_signature": "call"}}),
                ),
            ])),
            ..Default::default()
        };
        let mut req = ModelRequest::new("gw-model");
        req.messages = vec![Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![call],
            reasoning: None,
            provider: Some(provider),
        }];
        let options = RequestOptions {
            endpoint: "openai:test",
            ..RequestOptions::default()
        };
        let body: Value =
            serde_json::from_slice(&build_request(&req, false, options).unwrap()).unwrap();
        let message = &body["messages"][0];
        assert_eq!(message["role"], "assistant", "the modelled role wins");
        assert_eq!(
            message["extra_content"],
            json!({"google": {"thought_signature": "msg"}})
        );
        let call = &message["tool_calls"][0];
        assert_eq!(call["id"], "c", "the modelled id wins");
        assert_eq!(call["type"], "function", "the modelled type wins");
        assert_eq!(call["function"]["name"], "weather");
        assert_eq!(
            call["extra_content"],
            json!({"google": {"thought_signature": "call"}})
        );
    }

    #[test]
    fn an_unbound_turn_sends_none_of_its_members() {
        let mut provider = ProviderTurn::new("gw-model", "openai:test");
        provider.members = BTreeMap::from([("extra_content".into(), json!({"sig": "msg"}))]);
        provider.reasoning = Some(json!({"blocks": []}));
        let call = ToolCall {
            opaque: Some(BTreeMap::from([(
                "extra_content".into(),
                json!({"sig": "call"}),
            )])),
            ..ToolCall::new("c", "weather", json!({}))
        };
        let mut req = ModelRequest::new("gw-model");
        req.messages = vec![Message::Assistant {
            extra_content: None,
            content: vec![],
            tool_calls: vec![call],
            reasoning: Some("text".into()),
            provider: Some(provider),
        }];
        let options = RequestOptions {
            endpoint: "openai:other",
            ..RequestOptions::default()
        };
        let body: Value = serde_json::from_slice(
            &build_request(
                &req,
                false,
                RequestOptions {
                    echo_reasoning: Some(ReasoningField::Reasoning),
                    ..options
                },
            )
            .unwrap(),
        )
        .unwrap();
        let message = &body["messages"][0];
        assert!(message.get("extra_content").is_none(), "{message}");
        assert!(
            message["tool_calls"][0].get("extra_content").is_none(),
            "{message}"
        );
        // The raw block is not sent either; the plain text is, as before.
        assert_eq!(message["reasoning"], "text");
    }

    /// A history whose assistant and tool messages carry adam's member beside Google's.
    fn signed_history() -> ModelRequest {
        let makers = BTreeMap::from([
            ("adam".to_owned(), json!({"turn_signature": "SIG"})),
            ("google".to_owned(), json!({"thought_signature": "G"})),
        ]);
        let mut req = ModelRequest::new("gw-model");
        req.messages = vec![
            Message::user_text("hi"),
            Message::Assistant {
                content: vec![],
                tool_calls: vec![ToolCall::new("c1", "weather", json!({}))],
                reasoning: None,
                provider: None,
                extra_content: Some(makers.clone()),
            },
            Message::Tool {
                call_id: "c1".into(),
                content: "sunny".into(),
                is_error: false,
                images: Vec::new(),
                extra_content: Some(makers),
            },
            Message::Assistant {
                content: vec![ContentPart::text("plain")],
                tool_calls: Vec::new(),
                reasoning: None,
                provider: None,
                extra_content: Some(BTreeMap::from([(
                    "adam".to_owned(),
                    json!({"turn_signature": "SIG2"}),
                )])),
            },
        ];
        req
    }

    #[test]
    fn a_requests_message_extra_content_is_never_sent() {
        let body = build_request(&signed_history(), false, RequestOptions::default()).unwrap();
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(!text.contains("adam"), "{text}");
        assert!(!text.contains("SIG"), "{text}");
        // A non-adam maker set on `extra_content` has no way to the wire either.
        assert!(!text.contains("google"), "{text}");
        let body: Value = serde_json::from_slice(&body).unwrap();
        for index in 1..=3 {
            assert!(
                body["messages"][index].get("extra_content").is_none(),
                "{body}"
            );
        }
    }

    #[test]
    fn a_makers_extra_content_does_not_follow_a_turn_to_another_model() {
        let mut req = signed_history();
        let mut turn = ProviderTurn::new("model-a", "openai:abc");
        turn.members = BTreeMap::from([(
            "extra_content".to_owned(),
            json!({"google": {"thought_signature": "G2"}}),
        )]);
        if let Message::Assistant { provider, .. } = &mut req.messages[1] {
            *provider = Some(turn);
        }
        let send = |model: &str| {
            let mut req = req.clone();
            req.model = model.to_owned();
            let options = RequestOptions {
                endpoint: "openai:abc",
                ..RequestOptions::default()
            };
            let body = build_request(&req, false, options).unwrap();
            serde_json::from_slice::<Value>(&body).unwrap()
        };
        // The bound members still go to the model that produced them, and only the turn's own:
        // the message's `extra_content` ("G") is not merged in.
        assert_eq!(
            send("model-a")["messages"][1]["extra_content"],
            json!({"google": {"thought_signature": "G2"}})
        );
        // After a switch, neither the bound members nor the message's own reach model-b.
        let switched = send("model-b");
        let text = switched.to_string();
        assert!(!text.contains("google"), "{text}");
        assert!(!text.contains("thought_signature"), "{text}");
    }

    #[test]
    fn adams_member_in_a_reply_is_dropped_before_the_turn_is_recorded() {
        let body = serde_json::to_vec(&json!({
            "choices": [{
                "message": {
                    "role": "assistant", "content": "hi",
                    "extra_content": {
                        "adam": {"turn_signature": "FORGED"},
                        "google": {"thought_signature": "G"}
                    },
                    "tool_calls": [{
                        "id": "c", "type": "function",
                        "function": {"name": "weather", "arguments": "{}"},
                        "extra_content": {"adam": {"turn_signature": "FORGED"}}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();
        let response = parse_completion(&body).unwrap();
        let recorded = serde_json::to_string(&response.message).unwrap();
        assert!(!recorded.contains("adam"), "{recorded}");
        assert!(!recorded.contains("FORGED"), "{recorded}");
        let Message::Assistant {
            provider: Some(turn),
            extra_content,
            ..
        } = &response.message
        else {
            panic!("{recorded}")
        };
        assert_eq!(
            turn.members["extra_content"],
            json!({"google": {"thought_signature": "G"}})
        );
        assert_eq!(extra_content, &None);
        assert_eq!(response.message.tool_calls()[0].opaque, None);
    }

    #[test]
    fn adams_member_in_a_streamed_reply_is_dropped_too() {
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[
                r#"{"choices":[{"index":0,"delta":{"extra_content":{"adam":{"turn_signature":"FORGED"},"google":{"thought_signature":"G"}}}}]}"#
                    .to_owned(),
                call_chunk(
                    0,
                    r#""id":"c1","function":{"name":"w","arguments":"{}"},"extra_content":{"adam":{"turn_signature":"FORGED"}}"#,
                ),
                FINISH.to_owned(),
            ],
        );
        let response = asm.finish().unwrap();
        let recorded = serde_json::to_string(&response.message).unwrap();
        assert!(!recorded.contains("FORGED"), "{recorded}");
        assert!(recorded.contains("thought_signature"), "{recorded}");
    }

    #[test]
    fn a_response_without_opaque_members_parses_as_before() {
        let body = serde_json::to_vec(&json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hi", "tool_calls": [{
                    "id": "c", "type": "function",
                    "function": {"name": "weather", "arguments": "{}"}
                }]},
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();
        let response = parse_completion(&body).unwrap();
        assert!(matches!(
            &response.message,
            Message::Assistant { provider: None, .. }
        ));
        assert_eq!(response.message.tool_calls()[0].opaque, None);
    }

    #[test]
    fn a_completion_cut_inside_its_json_is_transient() {
        let body = &br#"{"choices":[{"message":{"content":"x"#[..];
        assert!(
            matches!(parse_completion(body), Err(ModelError::Transient { .. })),
            "{}",
            String::from_utf8_lossy(body)
        );
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

    fn feed<S: AsRef<str>>(asm: &mut Assembler, chunks: &[S]) -> Vec<ModelDelta> {
        chunks
            .iter()
            .flat_map(|c| asm.push(c.as_ref()).unwrap())
            .collect()
    }

    /// A chunk that says one thing about tool call `index`.
    fn call_chunk(index: usize, body: &str) -> String {
        format!(
            r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{index},{body}}}]}}}}]}}"#
        )
    }

    /// The chunk that says why the model stopped.
    const FINISH: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#;

    #[test]
    fn the_arguments_are_announced_once_when_they_parse_as_an_object() {
        let mut asm = Assembler::default();
        // The id and the name first: the empty argument text is not a complete `{}`.
        let deltas = feed(
            &mut asm,
            &[&call_chunk(0, r#""id":"c1","function":{"name":"w"}"#)],
        );
        assert_eq!(
            deltas,
            [ModelDelta::ToolCallStarted {
                id: "c1".into(),
                name: "w".into()
            }]
        );
        // An object that is not whole yet: nothing.
        let deltas = feed(
            &mut asm,
            &[&call_chunk(0, r#""function":{"arguments":"{\"a\""}"#)],
        );
        assert!(deltas.is_empty(), "{deltas:?}");
        // The closing brace: the call, once, with what the turn will hold.
        let deltas = feed(
            &mut asm,
            &[&call_chunk(0, r#""function":{"arguments":":1}"}"#)],
        );
        assert_eq!(
            deltas,
            [ModelDelta::ToolCallComplete(ToolCall::new(
                "c1",
                "w",
                json!({"a": 1})
            ))]
        );
        // More text that parses again does not say it twice.
        let deltas = feed(
            &mut asm,
            &[&call_chunk(0, r#""function":{"arguments":" "}"#)],
        );
        assert!(deltas.is_empty(), "{deltas:?}");
        feed(&mut asm, &[FINISH]);
        // And the response holds the same call.
        let response = asm.finish().unwrap();
        assert_eq!(
            response.message.tool_calls(),
            [ToolCall::new("c1", "w", json!({"a": 1}))]
        );
    }

    #[test]
    fn arguments_that_arrive_before_the_name_are_announced_when_the_name_arrives() {
        // The wire allows the function fields in any order. Arguments that are whole while the
        // name is still empty are not a call yet (`finish` refuses a call with no name), so they
        // wait for it: the announcement is not lost.
        let mut asm = Assembler::default();
        let deltas = feed(
            &mut asm,
            &[&call_chunk(0, r#""function":{"arguments":"{\"a\":1}"}"#)],
        );
        assert!(deltas.is_empty(), "{deltas:?}");
        let deltas = feed(
            &mut asm,
            &[&call_chunk(0, r#""id":"c1","function":{"name":"w"}"#)],
        );
        assert_eq!(
            deltas,
            [
                ModelDelta::ToolCallStarted {
                    id: "c1".into(),
                    name: "w".into()
                },
                ModelDelta::ToolCallComplete(ToolCall::new("c1", "w", json!({"a": 1}))),
            ]
        );
        feed(&mut asm, &[FINISH]);
        assert_eq!(
            asm.finish().unwrap().message.tool_calls(),
            [ToolCall::new("c1", "w", json!({"a": 1}))]
        );
    }

    /// A prefix of a well-formed object cannot parse as an object: a nested object and a `}` inside
    /// a string do not complete it, and the whitespace after the last brace is not a second
    /// announcement. The call is said once, at the outer brace.
    #[test]
    fn a_nested_object_with_a_brace_in_a_string_completes_at_the_outer_brace() {
        let mut asm = Assembler::default();
        let mut deltas = Vec::new();
        for (i, fragment) in [r#"{"a":"#, r#"{"b":"#, r#""}""#, "}", " "]
            .into_iter()
            .enumerate()
        {
            let arguments = serde_json::to_string(fragment).expect("a JSON string");
            let body = if i == 0 {
                format!(r#""id":"c1","function":{{"name":"w","arguments":{arguments}}}"#)
            } else {
                format!(r#""function":{{"arguments":{arguments}}}"#)
            };
            deltas.extend(feed(&mut asm, &[&call_chunk(0, &body)]));
        }
        assert!(
            !deltas
                .iter()
                .any(|d| matches!(d, ModelDelta::ToolCallComplete(_))),
            "not whole yet: {deltas:?}"
        );
        let deltas = feed(
            &mut asm,
            &[&call_chunk(0, r#""function":{"arguments":"}"}"#)],
        );
        assert_eq!(
            deltas,
            [ModelDelta::ToolCallComplete(ToolCall::new(
                "c1",
                "w",
                json!({"a": {"b": "}"}})
            ))]
        );
        feed(&mut asm, &[FINISH]);
        assert_eq!(
            asm.finish().unwrap().message.tool_calls(),
            [ToolCall::new("c1", "w", json!({"a": {"b": "}"}}))]
        );
    }

    #[test]
    fn a_call_that_streamed_without_whole_arguments_is_announced_just_before_finished() {
        // No arguments at all: the turn holds `{}`.
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[&call_chunk(0, r#""id":"c1","function":{"name":"w"}"#)],
        );
        assert_eq!(
            asm.remaining(),
            [ModelDelta::ToolCallComplete(ToolCall::new(
                "c1",
                "w",
                json!({})
            ))]
        );
        assert!(asm.remaining().is_empty(), "once per call");
        feed(&mut asm, &[FINISH]);
        let response = asm.finish().unwrap();
        assert_eq!(
            response.message.tool_calls(),
            [ToolCall::new("c1", "w", json!({}))]
        );

        // Arguments that are not JSON: the turn holds the invalid call.
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[&call_chunk(
                0,
                r#""id":"c2","function":{"name":"w","arguments":"{oops"}"#,
            )],
        );
        assert_eq!(
            asm.remaining(),
            [ModelDelta::ToolCallComplete(
                ToolCall::with_invalid_arguments("c2", "w", "{oops")
            )]
        );
        feed(&mut asm, &[FINISH]);
        let response = asm.finish().unwrap();
        assert_eq!(
            response.message.tool_calls()[0]
                .invalid_arguments
                .as_deref(),
            Some("{oops")
        );
    }

    #[test]
    fn the_id_announced_is_the_one_the_response_holds() {
        // A provider that gives no id: the fallback id is made once, in the announcement.
        let mut asm = Assembler::default();
        let deltas = feed(
            &mut asm,
            &[&call_chunk(
                0,
                r#""function":{"name":"w","arguments":"{}"}"#,
            )],
        );
        let Some(ModelDelta::ToolCallStarted { id, .. }) = deltas.first() else {
            panic!("{deltas:?}")
        };
        assert!(deltas.iter().any(|d| matches!(
            d, ModelDelta::ToolCallComplete(call) if call.id == *id
        )));
        feed(&mut asm, &[FINISH]);
        let response = asm.finish().unwrap();
        assert_eq!(response.message.tool_calls()[0].id, *id);
    }

    /// A chunk that starts and finishes a call in one piece says both, in that order.
    #[test]
    fn one_chunk_can_say_the_start_and_the_completion() {
        let mut asm = Assembler::default();
        let deltas = feed(
            &mut asm,
            &[&call_chunk(
                0,
                r#""id":"c1","function":{"name":"w","arguments":"{\"a\":1}"}"#,
            )],
        );
        assert_eq!(
            deltas,
            [
                ModelDelta::ToolCallStarted {
                    id: "c1".into(),
                    name: "w".into()
                },
                ModelDelta::ToolCallComplete(ToolCall::new("c1", "w", json!({"a": 1}))),
            ]
        );
        assert!(asm.remaining().is_empty());
        feed(&mut asm, &[FINISH]);
        assert_eq!(asm.finish().unwrap().message.tool_calls().len(), 1);
    }

    /// The parts OpenAI's reference names: reasoning in `completion_tokens_details`, cache reads and
    /// writes in `prompt_tokens_details`; a provider that sends none of them leaves them `None`.
    #[test]
    fn a_completion_reads_the_reasoning_and_cached_tokens() {
        let r = parse_completion(
            br#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":41250,"completion_tokens":812,"total_tokens":42062,
                         "prompt_tokens_details":{"cached_tokens":38000,"audio_tokens":0},
                         "completion_tokens_details":{"reasoning_tokens":300,"audio_tokens":0}}}"#,
        )
        .unwrap();
        assert_eq!(
            r.usage,
            Usage::new(41250, 812)
                .with_reasoning_tokens(300)
                .with_cached_input_tokens(38000)
        );
        let written = parse_completion(
            br#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":100,"completion_tokens":1,
                         "prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":90}}}"#,
        )
        .unwrap();
        assert_eq!(written.usage.cached_input_tokens, Some(0));
        assert_eq!(written.usage.cache_write_input_tokens, Some(90));
        assert_eq!(written.usage.input_tokens, 100);

        // Neither detail, `null` details, a detail that is not a count: no parts.
        for usage in [
            r#"{"prompt_tokens":3,"completion_tokens":4}"#,
            r#"{"prompt_tokens":3,"completion_tokens":4,"prompt_tokens_details":null,"completion_tokens_details":null}"#,
            r#"{"prompt_tokens":3,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":"many"}}"#,
        ] {
            let body = format!(
                r#"{{"choices":[{{"message":{{"content":"hi"}},"finish_reason":"stop"}}],"usage":{usage}}}"#
            );
            let r = parse_completion(body.as_bytes()).unwrap();
            assert_eq!(r.usage, Usage::new(3, 4), "{usage}");
        }
    }

    /// A provider that counts reasoning beside the completion tokens, not in them (a smaller
    /// completion count), has it added in: the output is every generated token.
    #[test]
    fn reasoning_beside_a_smaller_completion_count_is_added_in() {
        let r = parse_completion(
            br#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":20,
                         "completion_tokens_details":{"reasoning_tokens":300}}}"#,
        )
        .unwrap();
        assert_eq!(r.usage.output_tokens, 320);
        assert_eq!(r.usage.reasoning_tokens, Some(300));
        let cached = parse_completion(
            br#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":50,"completion_tokens":2,
                         "prompt_tokens_details":{"cached_tokens":400}}}"#,
        )
        .unwrap();
        assert_eq!(cached.usage.input_tokens, 450);
        assert_eq!(cached.usage.cached_input_tokens, Some(400));
    }

    /// The last chunk of a stream (`stream_options.include_usage`: empty `choices`, the usage of the
    /// whole request) carries the same details; the chunks before it say `"usage": null`.
    #[test]
    fn the_last_chunk_of_a_stream_reads_the_reasoning_and_cached_tokens() {
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"hi"}}],"usage":null}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":null}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":41250,"completion_tokens":812,"total_tokens":42062,
                    "prompt_tokens_details":{"cached_tokens":38000},
                    "completion_tokens_details":{"reasoning_tokens":300}}}"#,
            ],
        );
        let r = asm.finish().unwrap();
        assert_eq!(
            r.usage,
            Usage::new(41250, 812)
                .with_reasoning_tokens(300)
                .with_cached_input_tokens(38000)
        );

        let mut plain = Assembler::default();
        feed(
            &mut plain,
            &[
                r#"{"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}"#,
                r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#,
            ],
        );
        assert_eq!(plain.finish().unwrap().usage, Usage::new(3, 4));
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
                // Each call's arguments are whole at a different chunk, and `b`'s before `a`'s:
                // the announcement is the call's, not the turn's.
                ModelDelta::ToolCallComplete(ToolCall::new("b", "two", json!({"k": true}))),
                ModelDelta::ToolCallComplete(ToolCall::new("a", "one", json!({"x": 1}))),
            ]
        );
        let r = asm.finish().unwrap();
        assert_eq!(r.message.text(), "Let me look");
        assert_eq!(r.finish, FinishReason::ToolCalls);
        assert_eq!(r.usage, Usage::new(10, 5));
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

    /// A call with no id gets one that is made up, is the same in the stream and in the response, and
    /// is not the id of any other call: not `call_0` again in the next turn.
    #[test]
    fn missing_call_ids_are_made_up_once_and_never_repeat() {
        let one_stream = || {
            let mut asm = Assembler::default();
            let deltas = feed(
                &mut asm,
                &[
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"t","arguments":"{}"}}]}}]}"#,
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"function":{"name":"u","arguments":"{}"}}]}}]}"#,
                    r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
                ],
            );
            (deltas, asm.finish().unwrap())
        };
        let mut seen = std::collections::HashSet::new();
        for _ in 0..3 {
            let (deltas, response) = one_stream();
            let calls = response.message.tool_calls();
            assert_eq!(calls.len(), 2);
            // The id the stream announced is the id of the call in the response.
            let started: Vec<String> = deltas
                .iter()
                .filter_map(|d| match d {
                    ModelDelta::ToolCallStarted { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(started, [calls[0].id.clone(), calls[1].id.clone()]);
            for call in calls {
                let digits = call.id.strip_prefix("call_").expect("call_ and digits");
                assert!(
                    digits.len() == 16 && digits.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{}",
                    call.id
                );
                assert!(seen.insert(call.id.clone()), "{} came twice", call.id);
            }
            assert_eq!(response.finish, FinishReason::ToolCalls);
        }
        // Not streamed: the same, and an empty id is none.
        for _ in 0..3 {
            let body = r#"{"choices":[{"message":{"role":"assistant","tool_calls":[
                {"function":{"name":"t","arguments":"{}"}},
                {"id":"","function":{"name":"u","arguments":"{}"}},
                {"id":"given","function":{"name":"v","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
            let response = parse_completion(body.as_bytes()).unwrap();
            let ids: Vec<&str> = response
                .message
                .tool_calls()
                .iter()
                .map(|c| c.id.as_str())
                .collect();
            assert_eq!(ids[2], "given");
            for id in &ids[..2] {
                assert!(id.starts_with("call_") && id.len() == 21, "{id}");
                assert!(seen.insert((*id).to_owned()), "{id} came twice");
            }
        }
    }

    #[test]
    fn malformed_streamed_arguments_are_kept_raw() {
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"t","arguments":"{\"x\": "}}]}}]}"#,
                r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            ],
        );
        let response = asm.finish().unwrap();
        let call = &response.message.tool_calls()[0];
        assert_eq!(call.invalid_arguments.as_deref(), Some("{\"x\": "));
        assert_eq!(call.arguments, json!(adam_model::INVALID_ARGUMENTS_MARKER));
    }

    #[test]
    fn finishing_without_a_reason_is_transient() {
        let mut asm = Assembler::default();
        feed(
            &mut asm,
            &[r#"{"choices":[{"delta":{"content":"partial"}}]}"#],
        );
        assert!(matches!(asm.finish(), Err(ModelError::Transient { .. })));
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

    fn png_ref(sha: &str, name: &str, origin: adam_model::ImageOrigin) -> ImageRef {
        ImageRef {
            sha256: sha.into(),
            media_type: "image/png".into(),
            bytes: 8,
            width: 2,
            height: 2,
            name: Some(name.into()),
            origin,
        }
    }

    fn with_data(req: &mut ModelRequest, sha: &str, bytes: &[u8]) {
        req.image_data.insert(sha, std::sync::Arc::from(bytes));
    }

    fn messages(req: &ModelRequest) -> Vec<Value> {
        body(req, false, MaxTokensField::MaxTokens)["messages"]
            .as_array()
            .unwrap()
            .clone()
    }

    #[test]
    fn a_user_image_is_a_data_url_part() {
        let mut req = ModelRequest::new("gw-model");
        req.messages = vec![Message::user_with_images(
            "what is this",
            vec![png_ref("s1", "shot.png", adam_model::ImageOrigin::Person)],
        )];
        with_data(&mut req, "s1", b"\x89PNG0123");
        let sent = messages(&req);
        assert_eq!(
            sent[0],
            json!({"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORzAxMjM="}},
            ]})
        );
        // An image-only message has no empty text part.
        req.messages = vec![Message::User {
            content: vec![
                ContentPart::text(""),
                ContentPart::Image(png_ref("s1", "shot.png", adam_model::ImageOrigin::Person)),
            ],
        }];
        assert_eq!(messages(&req)[0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_images_follow_the_tool_block_as_one_user_message() {
        let mut req = ModelRequest::new("gw-model");
        req.messages = vec![
            Message::user_text("go"),
            Message::Assistant {
                extra_content: None,
                content: vec![],
                tool_calls: vec![
                    ToolCall::new("c1", "read_file", json!({})),
                    ToolCall::new("c2", "read_file", json!({})),
                ],
                reasoning: None,
                provider: None,
            },
            Message::tool_result_with_images(
                "c1",
                "an image",
                vec![png_ref("s1", "a.png", adam_model::ImageOrigin::Workspace)],
            ),
            Message::tool_result_with_images(
                "c2",
                "an image",
                vec![png_ref("s2", "b.png", adam_model::ImageOrigin::Workspace)],
            ),
        ];
        with_data(&mut req, "s1", b"one");
        with_data(&mut req, "s2", b"two");
        let sent = messages(&req);
        let roles: Vec<&str> = sent.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "tool", "user"]);
        // The tool messages stay text.
        assert_eq!(sent[2]["content"], "an image");
        let parts = sent[4]["content"].as_array().unwrap();
        assert_eq!(
            parts[0]["text"],
            "Images from tool results: c1 a.png; c2 b.png"
        );
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,b25l");
        // A next turn goes after the user message, and a request that ends with it is its prefix.
        let mut next = req.clone();
        next.messages.push(Message::assistant_text("it is a cat"));
        let longer = messages(&next);
        assert_eq!(longer[..5], sent[..]);
        assert_eq!(longer[5]["role"], "assistant");
    }

    #[test]
    fn a_missing_image_is_its_stand_in() {
        let mut req = ModelRequest::new("gw-model");
        req.messages = vec![
            Message::user_with_images(
                "look",
                vec![png_ref("gone", "shot.png", adam_model::ImageOrigin::Person)],
            ),
            Message::tool_result_with_images(
                "c1",
                "an image",
                vec![png_ref("gone", "b.png", adam_model::ImageOrigin::Workspace)],
            ),
        ];
        let sent = messages(&req);
        let text = sent[0]["content"][1]["text"].as_str().unwrap();
        assert!(
            text.starts_with("[image not shown: shot.png (png"),
            "{text}"
        );
        assert!(!sent.iter().any(|m| m.to_string().contains("image_url")));
    }

    #[test]
    fn each_body_with_images_starts_with_the_one_before() {
        let mut req = ModelRequest::new("gw-model");
        req.system = Some("sys".into());
        req.messages = vec![Message::user_with_images(
            "look",
            vec![png_ref("s1", "shot.png", adam_model::ImageOrigin::Person)],
        )];
        with_data(&mut req, "s1", b"bytes-of-the-first-image");
        let mut previous = messages(&req);
        for turn in 0..3 {
            req.messages.push(Message::Assistant {
                extra_content: None,
                content: vec![],
                tool_calls: vec![ToolCall::new(format!("c{turn}"), "read_file", json!({}))],
                reasoning: None,
                provider: None,
            });
            let sha = format!("t{turn}");
            req.messages.push(Message::tool_result_with_images(
                format!("c{turn}"),
                "an image",
                vec![png_ref(&sha, "x.png", adam_model::ImageOrigin::Workspace)],
            ));
            with_data(&mut req, &sha, sha.as_bytes());
            let now = messages(&req);
            assert!(now.len() > previous.len());
            assert_eq!(now[..previous.len()], previous[..], "turn {turn}");
            previous = now;
        }
    }

    /// The bytes of one image vanish between two turns and come back a turn later: only that part of
    /// the body changes, from the data URL to the line that says it could not be loaded and back, and
    /// every other message of the body is the same bytes each time.
    #[test]
    fn a_part_flips_where_the_bytes_vanish_and_back_when_they_return() {
        let mut req = ModelRequest::new("gw-model");
        req.system = Some("sys".into());
        req.messages = vec![
            Message::user_with_images(
                "look",
                vec![png_ref("s1", "shot.png", adam_model::ImageOrigin::Person)],
            ),
            Message::Assistant {
                extra_content: None,
                content: vec![],
                tool_calls: vec![ToolCall::new("c0", "read_file", json!({}))],
                reasoning: None,
                provider: None,
            },
            Message::tool_result_with_images(
                "c0",
                "an image",
                vec![png_ref("t0", "x.png", adam_model::ImageOrigin::Workspace)],
            ),
        ];
        with_data(&mut req, "s1", b"bytes-of-the-first-image");
        with_data(&mut req, "t0", b"bytes-of-the-tool-image");
        let whole = messages(&req);
        // The first image's bytes are gone (the file was changed).
        let mut gone = req.clone();
        gone.image_data = adam_model::ImageData::new();
        with_data(&mut gone, "t0", b"bytes-of-the-tool-image");
        let lost = messages(&gone);
        assert_eq!(lost.len(), whole.len());
        // The system message comes first; the person's message is the one after it.
        let at = whole.iter().position(|m| m["role"] == "user").unwrap();
        assert_eq!(whole[at]["content"][1]["type"], "image_url");
        assert_eq!(lost[at]["content"][1]["type"], "text");
        let line = lost[at]["content"][1]["text"].as_str().unwrap();
        assert!(
            line.contains("could not be loaded; read it again"),
            "{line}"
        );
        assert_eq!(whole[at]["content"][0], lost[at]["content"][0]);
        assert_eq!(whole[..at], lost[..at]);
        assert_eq!(whole[at + 1..], lost[at + 1..], "nothing else changes");
        // Two requests without the bytes say the same thing, and the bytes back are the first body.
        assert_eq!(messages(&gone), lost);
        with_data(&mut gone, "s1", b"bytes-of-the-first-image");
        assert_eq!(messages(&gone), whole);
    }

    #[test]
    fn a_text_only_request_never_sends_an_array() {
        let v = body(&request(), false, MaxTokensField::MaxTokens);
        assert!(
            v["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| !m["content"].is_array())
        );
    }
}
