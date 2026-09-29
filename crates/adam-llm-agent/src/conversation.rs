//! The agent's durable state and the inbound message format.

use adam_model::{Message, ToolCall, Usage};
use adam_runtime::Inbound;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Recommended [`Inbound::kind`] of a user message. The agent does not inspect
/// the kind: anything whose payload parses (see [`user_message`]) is a user
/// message.
pub const MESSAGE_KIND: &str = "message";

/// An inbound user message: kind [`MESSAGE_KIND`], payload `{"text": "..."}`.
///
/// This is the format of [`LlmAgent::init`](crate::LlmAgent) input, of every
/// message delivered to a running run with `Runtime::deliver`, and of the
/// answer to a [`ToolError::NeedsInput`](crate::ToolError::NeedsInput)
/// question. A bare JSON string payload is accepted too.
pub fn user_message(text: impl Into<String>) -> Inbound {
    Inbound::new(MESSAGE_KIND, json!({ "text": text.into() }))
}

/// The text of an inbound payload: `{"text": "..."}` or a bare JSON string.
pub(crate) fn parse_user_text(payload: &Value) -> Result<String, String> {
    match payload {
        Value::String(s) => Ok(s.clone()),
        Value::Object(o) => match o.get("text") {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err("payload object has no string field \"text\"".into()),
        },
        _ => Err("payload must be {\"text\": \"...\"} or a JSON string".into()),
    }
}

/// A tool call that asked the user a question and waits for the answer.
///
/// Stored in [`Conversation::pending_question`], so it is visible in the
/// durable run view (`RunView::state`) while the run is parked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingQuestion {
    /// The tool call the answer will be returned for.
    pub call_id: String,
    /// The tool that asked.
    pub tool: String,
    /// The question.
    pub question: String,
}

/// Name and media type of an artifact a tool produced (the content itself
/// lives in `RunView::artifacts`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Artifact name.
    pub name: String,
    /// Media type, if known.
    pub mime_type: Option<String>,
}

/// The state of an [`LlmAgent`](crate::LlmAgent) run: the persisted history
/// plus the bookkeeping the loop needs to resume from any commit.
///
/// It is the run's durable record: `Runtime::view(run).state` deserializes into
/// it. Every field has a serde default, so state written by an older version
/// still loads.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Conversation {
    /// The full history, oldest first. Never truncated here: history limits
    /// only shape what is sent to the model.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Model calls made so far.
    #[serde(default)]
    pub turns: u32,
    /// Tool calls requested by the model so far.
    #[serde(default)]
    pub tool_calls: u32,
    /// Token usage summed over all model calls.
    #[serde(default)]
    pub usage: Usage,
    /// Calls of the last assistant message that have no result yet, in order.
    /// Only non-empty while a step is in flight or the run is parked on
    /// [`pending_question`](Self::pending_question).
    #[serde(default)]
    pub pending_calls: Vec<ToolCall>,
    /// Set while the run is parked waiting for the user's answer.
    #[serde(default)]
    pub pending_question: Option<PendingQuestion>,
    /// User messages that arrived while tool results were still owed. They are
    /// appended once the last owed result is in (a user message between an
    /// assistant tool call and its result would break the protocol).
    #[serde(default)]
    pub deferred: Vec<Message>,
    /// Artifacts produced so far, summarised in the final output.
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
}

impl Conversation {
    /// A conversation that starts with one user message.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user_text(text)],
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_bare_string_payloads() {
        assert_eq!(
            parse_user_text(&json!({"text": "hi", "x": 1})).unwrap(),
            "hi"
        );
        assert_eq!(parse_user_text(&json!("hi")).unwrap(), "hi");
        assert!(parse_user_text(&json!({"text": 3})).is_err());
        assert!(parse_user_text(&json!({})).is_err());
        assert!(parse_user_text(&json!(null)).is_err());
        assert!(parse_user_text(&json!([1])).is_err());
    }

    #[test]
    fn user_message_has_the_documented_shape() {
        let m = user_message("hello");
        assert_eq!(m.kind, MESSAGE_KIND);
        assert_eq!(m.payload, json!({"text": "hello"}));
        assert_eq!(parse_user_text(&m.payload).unwrap(), "hello");
    }

    #[test]
    fn old_state_without_new_fields_still_loads() {
        let c: Conversation = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c, Conversation::default());
        let c: Conversation = serde_json::from_value(json!({"messages": []})).unwrap();
        assert!(c.messages.is_empty());
    }
}
