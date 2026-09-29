//! The agent's durable state and the inbound message format.

use adam_core::RunId;
use adam_model::{Message, ToolCall, Usage};
use adam_runtime::Inbound;
use chrono::{DateTime, Utc};
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
/// One kind of [`PendingWait`], stored in [`Conversation::pending_wait`], so it
/// is visible in the durable run view (`RunView::state`) while the run is
/// parked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingQuestion {
    /// The tool call the answer will be returned for.
    pub call_id: String,
    /// The tool that asked.
    pub tool: String,
    /// The question.
    pub question: String,
}

/// A tool call that waits for a child run to finish.
///
/// One kind of [`PendingWait`]. The run is parked with a timer, and the call is
/// answered by the child's `adam.run.finished` message, or, when the timer fires
/// first, by reading the child.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingRun {
    /// The tool call the child's result will be returned for.
    pub call_id: String,
    /// The tool that started the child.
    pub tool: String,
    /// The child run.
    pub run: RunId,
}

/// A tool call that waits for a task on another system to finish.
///
/// One kind of [`PendingWait`]. The run is parked with a timer; each time the timer fires the
/// agent asks the tool that made the call how the task stands
/// ([`Tool::poll_remote`](crate::Tool::poll_remote)) as a journaled step, until it has an answer
/// or `deadline` has passed. See [`ToolError::AwaitRemote`](crate::ToolError::AwaitRemote).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingRemote {
    /// The tool call the task's outcome will be returned for.
    pub call_id: String,
    /// The tool that started the task, and that is asked how it stands.
    pub tool: String,
    /// The tool's own name for the task (for an A2A agent, the remote task id). The agent does
    /// not read it.
    pub task: String,
    /// When the agent gives up waiting and answers the call with an error result, if the tool
    /// asked for a limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
}

/// What a parked run is waiting for, so that one result can complete the call
/// that owes it.
///
/// Serialized without a tag, by its fields: `{call_id, tool, question}` for a
/// [`Question`](Self::Question) (the shape the field had when it could only hold
/// a question), `{call_id, tool, run}` for a [`Run`](Self::Run) and
/// `{call_id, tool, task}` for a [`Remote`](Self::Remote).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PendingWait {
    /// The tool started a child run; its outcome is the tool result.
    Run(PendingRun),
    /// The tool needs the user's answer; the next message is the tool result.
    Question(PendingQuestion),
    /// The tool started a task on another system; polling it gives the tool result.
    Remote(PendingRemote),
}

impl PendingWait {
    /// The tool call the wait is for.
    pub fn call_id(&self) -> &str {
        match self {
            Self::Run(w) => &w.call_id,
            Self::Question(w) => &w.call_id,
            Self::Remote(w) => &w.call_id,
        }
    }
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
    /// [`pending_wait`](Self::pending_wait).
    #[serde(default)]
    pub pending_calls: Vec<ToolCall>,
    /// Set while the run is parked waiting for the user's answer or for a child
    /// run. Before child runs existed this field was `pending_question` and held
    /// only a question; state stored under that name still loads.
    #[serde(default, alias = "pending_question")]
    pub pending_wait: Option<PendingWait>,
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

    /// A `Conversation` as an older version stored it: the field is still called
    /// `pending_question`. Written by hand, not by the current serializer.
    const OLD_PARKED_ON_A_QUESTION: &str = r#"{
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "deploy"}]}
        ],
        "turns": 1,
        "tool_calls": 1,
        "pending_calls": [{"id": "c1", "name": "ask", "arguments": {}}],
        "pending_question": {"call_id": "c1", "tool": "ask", "question": "which environment?"}
    }"#;

    #[test]
    fn a_state_stored_as_pending_question_still_loads() {
        let c: Conversation = serde_json::from_str(OLD_PARKED_ON_A_QUESTION).unwrap();
        assert_eq!(
            c.pending_wait,
            Some(PendingWait::Question(PendingQuestion {
                call_id: "c1".into(),
                tool: "ask".into(),
                question: "which environment?".into(),
            }))
        );
        assert_eq!(c.pending_calls.len(), 1);
        assert_eq!(c.turns, 1);
        // Stored again, it is written under the new name and reads back the same.
        let stored = serde_json::to_value(&c).unwrap();
        assert!(stored.get("pending_question").is_none());
        assert_eq!(
            stored["pending_wait"],
            json!({"call_id": "c1", "tool": "ask", "question": "which environment?"})
        );
        assert_eq!(serde_json::from_value::<Conversation>(stored).unwrap(), c);
    }

    #[test]
    fn a_wait_on_a_child_run_keeps_its_own_shape() {
        let run = RunId::new();
        let literal = json!({
            "pending_wait": {"call_id": "c1", "tool": "reviewer", "run": run.to_string()}
        });
        let c: Conversation = serde_json::from_value(literal.clone()).unwrap();
        assert_eq!(
            c.pending_wait,
            Some(PendingWait::Run(PendingRun {
                call_id: "c1".into(),
                tool: "reviewer".into(),
                run,
            }))
        );
        assert_eq!(
            c.pending_wait.as_ref().map(PendingWait::call_id),
            Some("c1")
        );
        assert_eq!(
            serde_json::to_value(&c).unwrap()["pending_wait"],
            literal["pending_wait"]
        );
        // A null field, as `Conversation::default` serializes, is no wait at all.
        let none: Conversation = serde_json::from_value(json!({"pending_question": null})).unwrap();
        assert_eq!(none.pending_wait, None);
    }

    #[test]
    fn a_wait_on_a_remote_task_keeps_its_own_shape_and_the_others_still_read() {
        let literal = json!({
            "pending_wait": {"call_id": "c1", "tool": "billing", "task": "t-9"}
        });
        let c: Conversation = serde_json::from_value(literal.clone()).unwrap();
        assert_eq!(
            c.pending_wait,
            Some(PendingWait::Remote(PendingRemote {
                call_id: "c1".into(),
                tool: "billing".into(),
                task: "t-9".into(),
                deadline: None,
            }))
        );
        assert_eq!(
            c.pending_wait.as_ref().map(PendingWait::call_id),
            Some("c1")
        );
        // No deadline, no field: the shape is the literal.
        assert_eq!(
            serde_json::to_value(&c).unwrap()["pending_wait"],
            literal["pending_wait"]
        );
        // With a deadline it round-trips.
        let with = json!({
            "pending_wait": {"call_id": "c1", "tool": "billing", "task": "t-9",
                             "deadline": "2026-01-02T03:04:05Z"}
        });
        let c: Conversation = serde_json::from_value(with.clone()).unwrap();
        assert!(matches!(
            &c.pending_wait,
            Some(PendingWait::Remote(r)) if r.deadline.is_some()
        ));
        assert_eq!(
            serde_json::to_value(&c).unwrap()["pending_wait"]["deadline"],
            with["pending_wait"]["deadline"]
        );
        // The untagged variants do not swallow each other.
        let q: Conversation = serde_json::from_value(json!({
            "pending_wait": {"call_id": "c1", "tool": "ask", "question": "?"}
        }))
        .unwrap();
        assert!(matches!(q.pending_wait, Some(PendingWait::Question(_))));
    }

    #[test]
    fn old_state_without_new_fields_still_loads() {
        let c: Conversation = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c, Conversation::default());
        let c: Conversation = serde_json::from_value(json!({"messages": []})).unwrap();
        assert!(c.messages.is_empty());
    }
}
