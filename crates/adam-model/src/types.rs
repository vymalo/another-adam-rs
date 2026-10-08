use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One request to a model.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ModelRequest {
    /// Model alias understood by the gateway.
    pub model: String,
    /// System prompt, sent ahead of `messages`.
    #[serde(default)]
    pub system: Option<String>,
    /// Conversation so far.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Tools the model may call.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// How the model may use `tools`.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Upper bound on generated tokens.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Forwarded as request metadata where the backend supports it.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl ModelRequest {
    /// A request for `model` with no messages, tools or limits.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }
}

/// How the model may use the tools of a request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// The model decides whether to call a tool.
    #[default]
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call some tool.
    Required,
    /// The model must call the named tool.
    Tool(String),
}

/// One turn of a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// Input from the user.
    User {
        /// The content of the message.
        content: Vec<ContentPart>,
    },
    /// Output from the model.
    Assistant {
        /// Text the model produced.
        content: Vec<ContentPart>,
        /// Tools the model asked to run.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        /// What the model thought before it wrote the above, **kept only when the model's client
        /// is set to send it back** (a provider in thinking mode that requires it, DeepSeek's with
        /// tools). `None` otherwise: a client that does not echo reasoning never puts it here, so
        /// it is never in the stored history and never sent. It is not [`Message::text`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    /// The result of running a tool the model asked for.
    Tool {
        /// The [`ToolCall::id`] this answers.
        call_id: String,
        /// What the tool returned (or the error text).
        content: String,
        /// Whether the tool failed.
        #[serde(default)]
        is_error: bool,
    },
}

impl Message {
    /// A user message with a single text part.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentPart::text(text)],
        }
    }

    /// An assistant message with a single text part and no tool calls.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            reasoning: None,
        }
    }

    /// A successful tool result.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    /// A failed tool result.
    pub fn tool_error(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            call_id: call_id.into(),
            content: content.into(),
            is_error: true,
        }
    }

    /// The text of the message: all text parts concatenated (for a tool
    /// message, its content).
    pub fn text(&self) -> String {
        match self {
            Self::User { content } | Self::Assistant { content, .. } => {
                content.iter().map(ContentPart::as_text).collect()
            }
            Self::Tool { content, .. } => content.clone(),
        }
    }

    /// The reasoning an assistant message keeps for the model (see [`Message::Assistant`]);
    /// `None` for other roles and for a client that does not echo it.
    pub fn reasoning(&self) -> Option<&str> {
        match self {
            Self::Assistant { reasoning, .. } => reasoning.as_deref(),
            _ => None,
        }
    }

    /// The tool calls of an assistant message; empty for other roles.
    pub fn tool_calls(&self) -> &[ToolCall] {
        match self {
            Self::Assistant { tool_calls, .. } => tool_calls,
            _ => &[],
        }
    }
}

/// A piece of message content. Only text for now; images come later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
}

impl ContentPart {
    /// A text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// The text of this part.
    pub fn as_text(&self) -> &str {
        match self {
            Self::Text { text } => text,
        }
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name, unique within a request.
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the tool's arguments.
    pub parameters: Value,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Identifier the tool result must echo ([`Message::Tool::call_id`]).
    pub id: String,
    /// Name of the tool to run.
    pub name: String,
    /// Parsed JSON arguments.
    pub arguments: Value,
}

/// A finished model turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    /// The assembled message; always [`Message::Assistant`].
    pub message: Message,
    /// Why the model stopped.
    pub finish: FinishReason,
    /// Token accounting.
    pub usage: Usage,
    /// What the model thought on the way to `message`, when the provider says it
    /// (`reasoning_content` or `reasoning`): for people to read, **not** part of the answer and not
    /// of the history. An agent shows it and drops it; it reaches the history only through
    /// [`Message::Assistant::reasoning`], and only for a client set to echo it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

impl ModelResponse {
    /// A response with only text: [`FinishReason::Stop`], zero usage.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            message: Message::assistant_text(text),
            finish: FinishReason::Stop,
            usage: Usage::default(),
            reasoning: None,
        }
    }

    /// A response asking for tool calls: [`FinishReason::ToolCalls`], no text,
    /// zero usage.
    pub fn tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            message: Message::Assistant {
                content: Vec::new(),
                tool_calls,
                reasoning: None,
            },
            finish: FinishReason::ToolCalls,
            usage: Usage::default(),
            reasoning: None,
        }
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural end of the turn.
    Stop,
    /// The model wants tools run.
    ToolCalls,
    /// The output token limit was reached.
    Length,
    /// The provider's content filter stopped the output.
    ContentFilter,
    /// Any other provider-specific reason.
    Other(String),
}

/// Token accounting for one call, or for several summed.
///
/// The accounting is AG-UI 1.0's `TokenUsage`: `input_tokens` and `output_tokens` are **totals**,
/// and the optional counts are **parts** of them, never additions. `reasoning_tokens` is part of
/// `output_tokens`; `cached_input_tokens` (read from a provider's cache) and
/// `cache_write_input_tokens` (written to it) are disjoint parts of `input_tokens`. A part is
/// `None` when the provider did not say. [`accounted`](Self::accounted) brings a provider that
/// reports a part beside a smaller total back to this rule.
///
/// Build one with [`Usage::new`] and the `with_*` methods; the struct is `#[non_exhaustive]`, so a
/// later count is not a breaking change. Every member but the two totals is left out of the JSON
/// while it is `None`, so a usage recorded before the parts existed reads the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Tokens in the prompt: every prompt token the call was charged for, cached ones included.
    pub input_tokens: u64,
    /// Tokens generated, reasoning included.
    pub output_tokens: u64,
    /// The part of [`output_tokens`](Self::output_tokens) spent on reasoning, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// The part of [`input_tokens`](Self::input_tokens) read from the provider's cache, when the
    /// provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// The part of [`input_tokens`](Self::input_tokens) written to the provider's cache on this call,
    /// when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_input_tokens: Option<u64>,
}

impl Usage {
    /// `input` prompt tokens and `output` generated ones, no parts.
    pub fn new(input: u64, output: u64) -> Self {
        Self {
            input_tokens: input,
            output_tokens: output,
            ..Self::default()
        }
    }

    /// With `tokens` of the output spent on reasoning.
    #[must_use]
    pub fn with_reasoning_tokens(mut self, tokens: u64) -> Self {
        self.reasoning_tokens = Some(tokens);
        self
    }

    /// With `tokens` of the input read from the provider's cache.
    #[must_use]
    pub fn with_cached_input_tokens(mut self, tokens: u64) -> Self {
        self.cached_input_tokens = Some(tokens);
        self
    }

    /// With `tokens` of the input written to the provider's cache.
    #[must_use]
    pub fn with_cache_write_input_tokens(mut self, tokens: u64) -> Self {
        self.cache_write_input_tokens = Some(tokens);
        self
    }

    /// `input_tokens + output_tokens`, saturating.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// The same counts under the accounting of the type: a provider that reports a part beside a
    /// smaller total has the part **added in** (AG-UI's rule for a producer), so that every part
    /// fits in its total. Reasoning above the output is added to the output; cache reads and writes
    /// above the input are added to the input. Counts that already follow the rule are unchanged,
    /// so applying it twice changes nothing.
    #[must_use]
    pub fn accounted(mut self) -> Self {
        if let Some(reasoning) = self.reasoning_tokens
            && reasoning > self.output_tokens
        {
            self.output_tokens = self.output_tokens.saturating_add(reasoning);
        }
        let cache = self
            .cached_input_tokens
            .unwrap_or(0)
            .saturating_add(self.cache_write_input_tokens.unwrap_or(0));
        if cache > self.input_tokens {
            self.input_tokens = self.input_tokens.saturating_add(cache);
        }
        self
    }

    /// The counts of two calls together, each saturating. A part one of them did not report counts
    /// as zero beside one the other did, and stays `None` when neither did.
    #[must_use]
    pub fn saturating_add(self, other: Self) -> Self {
        let part = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
        };
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            reasoning_tokens: part(self.reasoning_tokens, other.reasoning_tokens),
            cached_input_tokens: part(self.cached_input_tokens, other.cached_input_tokens),
            cache_write_input_tokens: part(
                self.cache_write_input_tokens,
                other.cache_write_input_tokens,
            ),
        }
    }
}

/// One item of a streamed completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDelta {
    /// A piece of generated text.
    Text(String),
    /// A piece of the model's reasoning (`reasoning_content`, `reasoning`), which a provider in
    /// thinking mode sends before the answer. Not part of the answer: see
    /// [`ModelResponse::reasoning`]. The pieces concatenate to it.
    Reasoning(String),
    /// The model started a tool call. Arguments arrive only in
    /// [`ModelDelta::Finished`], once they parse as JSON.
    ToolCallStarted {
        /// Call identifier.
        id: String,
        /// Tool name.
        name: String,
    },
    /// The final item: the fully assembled response.
    Finished(ModelResponse),
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::json;

    use super::*;

    fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: T) {
        let json = serde_json::to_string(&value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, back, "via {json}");
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "weather".into(),
            arguments: json!({"city": "Paris", "days": [1, 2]}),
        }
    }

    fn response() -> ModelResponse {
        ModelResponse {
            message: Message::Assistant {
                content: vec![ContentPart::text("checking")],
                tool_calls: vec![tool_call()],
                reasoning: None,
            },
            finish: FinishReason::ToolCalls,
            usage: Usage::new(12, 34)
                .with_reasoning_tokens(30)
                .with_cached_input_tokens(10),
            reasoning: Some("the user wants the weather".into()),
        }
    }

    #[test]
    fn request_roundtrip() {
        let mut req = ModelRequest::new("gpt");
        req.system = Some("be brief".into());
        req.messages = vec![
            Message::user_text("hi"),
            Message::assistant_text("hello"),
            Message::tool_result("c", "ok"),
        ];
        req.tools = vec![ToolSpec {
            name: "weather".into(),
            description: "Get weather".into(),
            parameters: json!({"type": "object", "properties": {}}),
        }];
        req.tool_choice = ToolChoice::Tool("weather".into());
        req.max_output_tokens = Some(256);
        req.temperature = Some(0.5);
        req.metadata.insert("run".into(), "r1".into());
        roundtrip(req);
        roundtrip(ModelRequest::default());
    }

    #[test]
    fn message_roundtrip() {
        roundtrip(Message::user_text("hi"));
        roundtrip(Message::assistant_text("hi"));
        roundtrip(Message::Assistant {
            content: vec![],
            tool_calls: vec![tool_call()],
            reasoning: None,
        });
        roundtrip(Message::Assistant {
            content: vec![],
            tool_calls: vec![tool_call()],
            reasoning: Some("thinking".into()),
        });
        roundtrip(Message::tool_result("c", "out"));
        roundtrip(Message::tool_error("c", "boom"));
    }

    #[test]
    fn message_wire_shape_is_stable() {
        assert_eq!(
            serde_json::to_value(Message::user_text("hi")).unwrap(),
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})
        );
        assert_eq!(
            serde_json::to_value(Message::assistant_text("yo")).unwrap(),
            json!({"role": "assistant", "content": [{"type": "text", "text": "yo"}]})
        );
        assert_eq!(
            serde_json::to_value(Message::tool_error("c1", "bad")).unwrap(),
            json!({"role": "tool", "call_id": "c1", "content": "bad", "is_error": true})
        );
        // Reasoning is written only when there is some: a history stored before it existed, and
        // one from a client that does not echo it, are the same bytes.
        assert_eq!(
            serde_json::to_value(Message::assistant_text("yo")).unwrap(),
            serde_json::to_value(Message::Assistant {
                content: vec![ContentPart::text("yo")],
                tool_calls: Vec::new(),
                reasoning: None,
            })
            .unwrap()
        );
        assert_eq!(
            serde_json::to_value(Message::Assistant {
                content: vec![],
                tool_calls: Vec::new(),
                reasoning: Some("r".into()),
            })
            .unwrap(),
            json!({"role": "assistant", "content": [], "reasoning": "r"})
        );
        // History persisted without optional fields still loads.
        let m: Message = serde_json::from_value(
            json!({"role": "assistant", "content": [{"type": "text", "text": "x"}]}),
        )
        .unwrap();
        assert_eq!(m, Message::assistant_text("x"));
    }

    #[test]
    fn tool_choice_roundtrip_and_shape() {
        for c in [
            ToolChoice::Auto,
            ToolChoice::None,
            ToolChoice::Required,
            ToolChoice::Tool("t".into()),
        ] {
            roundtrip(c);
        }
        assert_eq!(
            serde_json::to_value(ToolChoice::Auto).unwrap(),
            json!("auto")
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::Tool("t".into())).unwrap(),
            json!({"tool": "t"})
        );
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }

    #[test]
    fn leaf_types_roundtrip() {
        roundtrip(ContentPart::text("x"));
        roundtrip(ToolSpec {
            name: "n".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        });
        roundtrip(tool_call());
        roundtrip(Usage::new(u64::MAX, 0));
        roundtrip(
            Usage::new(5, 6)
                .with_reasoning_tokens(1)
                .with_cached_input_tokens(2)
                .with_cache_write_input_tokens(3),
        );
        roundtrip(response());
    }

    #[test]
    fn usage_writes_a_part_only_when_there_is_one_and_reads_the_old_shape() {
        assert_eq!(
            serde_json::to_value(Usage::new(3, 4)).unwrap(),
            json!({"input_tokens": 3, "output_tokens": 4})
        );
        assert_eq!(
            serde_json::to_value(Usage::new(3, 4).with_reasoning_tokens(2)).unwrap(),
            json!({"input_tokens": 3, "output_tokens": 4, "reasoning_tokens": 2})
        );
        let old: Usage =
            serde_json::from_value(json!({"input_tokens": 3, "output_tokens": 4})).unwrap();
        assert_eq!(old, Usage::new(3, 4));
        assert_eq!(old.reasoning_tokens, None);
        assert_eq!(Usage::new(u64::MAX, 1).total_tokens(), u64::MAX);
    }

    #[test]
    fn a_part_above_its_total_is_added_in_and_one_within_it_is_left() {
        // Reasoning beside a smaller completion count: the producer adds it in.
        let beside = Usage::new(10, 5).with_reasoning_tokens(300).accounted();
        assert_eq!(beside.output_tokens, 305);
        assert_eq!(beside.reasoning_tokens, Some(300));
        // Within the total: a part, nothing changes.
        let within = Usage::new(10, 500).with_reasoning_tokens(300);
        assert_eq!(within.accounted(), within);
        // Cache reads and writes beside a smaller input are added in, together.
        let cached = Usage::new(100, 1)
            .with_cached_input_tokens(80)
            .with_cache_write_input_tokens(40)
            .accounted();
        assert_eq!(cached.input_tokens, 220);
        let fits = Usage::new(100, 1).with_cached_input_tokens(80);
        assert_eq!(fits.accounted(), fits);
        // Twice is once.
        assert_eq!(beside.accounted(), beside);
        assert_eq!(cached.accounted(), cached);
    }

    #[test]
    fn usages_add_part_by_part_and_saturate() {
        let a = Usage::new(1, 2).with_reasoning_tokens(1);
        let b = Usage::new(10, 20).with_cached_input_tokens(5);
        let sum = a.saturating_add(b);
        assert_eq!(sum.input_tokens, 11);
        assert_eq!(sum.output_tokens, 22);
        assert_eq!(sum.reasoning_tokens, Some(1));
        assert_eq!(sum.cached_input_tokens, Some(5));
        assert_eq!(sum.cache_write_input_tokens, None);
        let full = Usage::new(u64::MAX, u64::MAX).with_reasoning_tokens(u64::MAX);
        let over = full.saturating_add(full);
        assert_eq!(over.input_tokens, u64::MAX);
        assert_eq!(over.reasoning_tokens, Some(u64::MAX));
    }

    #[test]
    fn finish_reason_roundtrip() {
        for f in [
            FinishReason::Stop,
            FinishReason::ToolCalls,
            FinishReason::Length,
            FinishReason::ContentFilter,
            FinishReason::Other("weird".into()),
        ] {
            roundtrip(f);
        }
        assert_eq!(
            serde_json::to_value(FinishReason::ToolCalls).unwrap(),
            json!("tool_calls")
        );
    }

    #[test]
    fn delta_roundtrip() {
        roundtrip(ModelDelta::Text("hi".into()));
        roundtrip(ModelDelta::Reasoning("hm".into()));
        roundtrip(ModelDelta::ToolCallStarted {
            id: "c".into(),
            name: "n".into(),
        });
        roundtrip(ModelDelta::Finished(response()));
    }

    #[test]
    fn helpers() {
        let m = Message::Assistant {
            content: vec![ContentPart::text("a"), ContentPart::text("b")],
            tool_calls: vec![tool_call()],
            reasoning: Some("why".into()),
        };
        // Reasoning is never part of the text.
        assert_eq!(m.text(), "ab");
        assert_eq!(m.reasoning(), Some("why"));
        assert_eq!(Message::user_text("u").reasoning(), None);
        assert_eq!(m.tool_calls().len(), 1);
        assert_eq!(Message::user_text("u").tool_calls(), &[]);
        assert_eq!(Message::tool_result("c", "out").text(), "out");

        let r = ModelResponse::text("t");
        assert_eq!(r.finish, FinishReason::Stop);
        let r = ModelResponse::tool_calls(vec![tool_call()]);
        assert_eq!(r.finish, FinishReason::ToolCalls);
        assert_eq!(r.message.tool_calls()[0].name, "weather");
    }
}
