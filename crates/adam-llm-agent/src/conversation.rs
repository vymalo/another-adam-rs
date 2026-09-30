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
    /// The run whose conversation this one carries on, for a run started with
    /// `Runtime::start_with_id_continuing` (see [`Conversation::continued`]); `None` for a run that
    /// began from nothing. Only for whoever reads the durable state: the loop never looks at it.
    /// Absent from state written before it existed, and not written while it is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_from: Option<RunId>,
}

/// The most serialized JSON, in bytes, of messages that a continued conversation carries from the
/// conversation it continues ([`Conversation::continued`]): 256 KiB.
///
/// Beyond it the oldest whole turns are dropped, a turn being a user message and everything that
/// followed it up to the next user message, and one marker message
/// ([`OMITTED_MARKER_PREFIX`]) stands in for them. The newest prior turn is never dropped, even
/// when it alone is larger (one run's own limits bound it, and the next continuation drops it).
///
/// Why a cap at all: every continuation copies the carried history into the new run's state, which
/// is one JSON value in the store, so without a bound each task in a long-lived context would
/// store, and each model call would re-read, everything said before. Why this size: it is about
/// what `Limits::max_history_tokens` lets through to the model (100 000 tokens of about 4
/// characters), so the cap removes history the model would not be shown whole anyway. Bytes of
/// JSON, not tokens: the cap has to be checkable without a tokenizer, and it over-counts the
/// JSON's own punctuation, which only makes it stricter.
pub const MAX_CARRIED_BYTES: usize = 256 * 1024;

/// How the text of the marker message that replaces dropped turns begins. The marker is a user
/// message (a conversation may start with one, unlike with an assistant message), so a reader that
/// looks at what the user said, such as the coder's rule about which repositories a task named,
/// can recognise it by this prefix and skip it.
pub const OMITTED_MARKER_PREFIX: &str = "[earlier conversation omitted";

fn omission_marker() -> Message {
    Message::user_text(format!(
        "{OMITTED_MARKER_PREFIX} to keep the carried history within its limit]"
    ))
}

fn is_omission_marker(message: &Message) -> bool {
    matches!(message, Message::User { content }
        if content.first().is_some_and(|p| p.as_text().starts_with(OMITTED_MARKER_PREFIX)))
}

fn json_len(message: &Message) -> usize {
    serde_json::to_vec(message).map_or(0, |bytes| bytes.len())
}

/// Removes a last assistant message whose tool calls did not all get a result, with anything
/// after it (results that did arrive, for a turn that never finished).
///
/// A run that ended mid-turn (failed on a limit, was cancelled, or died while parked on a
/// question) leaves such a message, and a provider rejects a history in which a call has no
/// result. The loop answers every call of a message before it asks the model again, so only the
/// last message with calls can be in this state.
fn drop_unanswered_calls(messages: &mut Vec<Message>) {
    let Some(at) = messages.iter().rposition(
        |m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()),
    ) else {
        return;
    };
    let Message::Assistant { tool_calls, .. } = &messages[at] else {
        return;
    };
    let answered = |id: &str| {
        messages[at + 1..]
            .iter()
            .any(|m| matches!(m, Message::Tool { call_id, .. } if call_id == id))
    };
    if !tool_calls.iter().all(|call| answered(&call.id)) {
        messages.truncate(at);
    }
}

/// `prior` followed by `new`, with the oldest whole turns of `prior` dropped (and a marker put in
/// their place) while the JSON of all of it exceeds `cap` bytes. See [`MAX_CARRIED_BYTES`].
fn carry(prior: Vec<Message>, new: Message, cap: usize) -> Vec<Message> {
    let sizes: Vec<usize> = prior.iter().map(json_len).collect();
    let mut total = sizes.iter().sum::<usize>() + json_len(&new);
    // Where each turn starts. What precedes the first start (an earlier marker) belongs to the
    // first turn, so it goes with it.
    let starts: Vec<usize> = prior
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::User { .. }) && !is_omission_marker(m))
        .map(|(i, _)| i)
        .collect();
    let marker = omission_marker();
    let marker_len = json_len(&marker);
    let mut dropped = 0;
    let mut next_turn = 1;
    if total > cap {
        // Never the last turn: it is the one the new message most likely follows up on.
        while total + marker_len > cap && next_turn < starts.len() {
            let end = starts[next_turn];
            total -= sizes[dropped..end].iter().sum::<usize>();
            dropped = end;
            next_turn += 1;
        }
    }
    let mut out = Vec::with_capacity(prior.len() - dropped + 2);
    if dropped > 0 {
        out.push(marker);
    }
    out.extend(prior.into_iter().skip(dropped));
    out.push(new);
    out
}

impl Conversation {
    /// A conversation that starts with one user message.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user_text(text)],
            ..Self::default()
        }
    }

    /// The conversation of a new run that carries on this one (the run `from`) with one more
    /// user message: what [`LlmAgent`](crate::LlmAgent) and [`LlmStarter`](crate::LlmStarter)
    /// start a run with when the runtime says it continues another.
    ///
    /// * **Carried:** the history, oldest first, and the user messages that were still waiting
    ///   behind an owed tool result ([`deferred`](Self::deferred)), in the order they arrived,
    ///   then `text` as the newest user message.
    /// * **Dropped:** a last assistant message whose tool calls never got their results, with the
    ///   results that did arrive (the run that made them ended mid-turn, and a model provider
    ///   rejects a call without a result). The wait that message was parked on goes with it:
    ///   `pending_wait` and `pending_calls` are always empty here, so a continued run never
    ///   answers a question or a child run of the run before.
    /// * **Reset, per task:** `turns`, `tool_calls` and `usage` (the [`Limits`](crate::Limits) are
    ///   per run, and this is a new run) and `artifacts` (the final output lists what this run
    ///   produced).
    /// * **Recorded:** `continued_from`.
    /// * **Bounded:** by [`MAX_CARRIED_BYTES`], which drops the oldest whole turns.
    #[must_use]
    pub fn continued(&self, text: impl Into<String>, from: RunId) -> Self {
        self.continued_within(text, from, MAX_CARRIED_BYTES)
    }

    fn continued_within(&self, text: impl Into<String>, from: RunId, cap: usize) -> Self {
        let mut messages = self.messages.clone();
        drop_unanswered_calls(&mut messages);
        messages.extend(self.deferred.iter().cloned());
        Self {
            messages: carry(messages, Message::user_text(text), cap),
            continued_from: Some(from),
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

    #[test]
    fn state_written_before_continuation_existed_loads_without_continued_from() {
        let c: Conversation = serde_json::from_str(OLD_PARKED_ON_A_QUESTION).unwrap();
        assert_eq!(c.continued_from, None);
        // A conversation that continues nothing does not write the field, so its stored shape is
        // what it always was; one that does round-trips it.
        assert!(
            serde_json::to_value(&c)
                .unwrap()
                .get("continued_from")
                .is_none()
        );
        let run = RunId::new();
        let continued = c.continued("and then?", run);
        let stored = serde_json::to_value(&continued).unwrap();
        assert_eq!(stored["continued_from"], json!(run.to_string()));
        assert_eq!(
            serde_json::from_value::<Conversation>(stored).unwrap(),
            continued
        );
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "t".into(),
            arguments: json!({}),
        }
    }

    fn calls(ids: &[&str]) -> Message {
        Message::Assistant {
            content: vec![],
            tool_calls: ids.iter().map(|id| call(id)).collect(),
        }
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        messages.iter().map(Message::text).collect()
    }

    #[test]
    fn a_continuation_carries_the_history_and_starts_the_counters_again() {
        let mut prior = Conversation::new("first task");
        prior.messages.push(calls(&["c1"]));
        prior.messages.push(Message::tool_result("c1", "out"));
        prior.messages.push(Message::assistant_text("done"));
        prior.turns = 7;
        prior.tool_calls = 5;
        prior.usage = Usage {
            input_tokens: 10,
            output_tokens: 20,
        };
        prior.artifacts = vec![ArtifactRef {
            name: "report".into(),
            mime_type: None,
        }];
        let run = RunId::new();

        let next = prior.continued("second task", run);

        let mut expected = prior.messages.clone();
        expected.push(Message::user_text("second task"));
        assert_eq!(next.messages, expected);
        assert_eq!(next.continued_from, Some(run));
        // A fresh run: its own limits and its own summary of what it produced.
        assert_eq!((next.turns, next.tool_calls), (0, 0));
        assert_eq!(next.usage, Usage::default());
        assert!(next.artifacts.is_empty());
        assert!(next.pending_calls.is_empty() && next.pending_wait.is_none());
        assert!(next.deferred.is_empty());
        // The prior conversation is untouched.
        assert_eq!(prior.turns, 7);
        assert_eq!(prior.messages.len(), 4);
    }

    #[test]
    fn user_messages_that_were_waiting_behind_a_result_come_before_the_new_one() {
        let mut prior = Conversation::new("first task");
        prior.messages.push(Message::assistant_text("done"));
        prior.deferred = vec![
            Message::user_text("also this"),
            Message::user_text("and this"),
        ];
        let next = prior.continued("second task", RunId::new());
        assert_eq!(
            texts(&next.messages),
            ["first task", "done", "also this", "and this", "second task"]
        );
        assert!(next.deferred.is_empty());
    }

    #[test]
    fn a_tool_call_without_its_result_is_dropped_with_the_turn_it_belonged_to() {
        let run = RunId::new();
        let user = |t: &str| Message::user_text(t);

        // Nothing came back at all (a limit failed the run right after the model's turn).
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1", "c2"]));
        assert_eq!(
            c.continued("more", run).messages,
            [user("task"), user("more")]
        );

        // Some results came back, not all.
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1", "c2"]));
        c.messages.push(Message::tool_result("c1", "partial"));
        assert_eq!(
            c.continued("more", run).messages,
            [user("task"), user("more")]
        );

        // Parked on a question the run never got an answer to: the wait goes with the call.
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1"]));
        c.pending_calls = vec![call("c1")];
        c.pending_wait = Some(PendingWait::Question(PendingQuestion {
            call_id: "c1".into(),
            tool: "ask".into(),
            question: "which?".into(),
        }));
        let next = c.continued("more", run);
        assert_eq!(next.messages, [user("task"), user("more")]);
        assert!(next.pending_calls.is_empty() && next.pending_wait.is_none());

        // Earlier, finished exchanges stay; only the last, unfinished one goes.
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1"]));
        c.messages.push(Message::tool_result("c1", "out"));
        c.messages.push(calls(&["c2"]));
        let next = c.continued("more", run);
        assert_eq!(
            next.messages,
            [
                user("task"),
                calls(&["c1"]),
                Message::tool_result("c1", "out"),
                user("more"),
            ]
        );

        // Answered calls are history, whatever came after them.
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1", "c2"]));
        c.messages.push(Message::tool_result("c1", "one"));
        c.messages.push(Message::tool_error("c2", "two"));
        c.messages.push(Message::assistant_text("done"));
        assert_eq!(c.continued("more", run).messages.len(), 6);
    }

    /// A conversation of `turns` turns: a user message, a tool exchange whose result is `size`
    /// bytes, and the answer.
    fn turns(n: usize, size: usize) -> Vec<Message> {
        let mut out = Vec::new();
        for i in 0..n {
            out.push(Message::user_text(format!("task {i}")));
            out.push(calls(&[&format!("c{i}")]));
            out.push(Message::tool_result(format!("c{i}"), "x".repeat(size)));
            out.push(Message::assistant_text(format!("answer {i}")));
        }
        out
    }

    fn total_len(messages: &[Message]) -> usize {
        messages.iter().map(json_len).sum()
    }

    fn is_marker(m: &Message) -> bool {
        is_omission_marker(m)
    }

    #[test]
    fn under_the_cap_nothing_is_dropped_and_no_marker_appears() {
        let prior = Conversation {
            messages: turns(3, 1000),
            ..Conversation::default()
        };
        let next = prior.continued_within("next", RunId::new(), 100_000);
        assert_eq!(next.messages.len(), 13);
        assert!(!next.messages.iter().any(is_marker));
        assert_eq!(&next.messages[..12], &prior.messages[..]);
    }

    #[test]
    fn over_the_cap_the_oldest_whole_turns_go_and_one_marker_stands_in() {
        // Five turns of about 1260 bytes each: this cap fits three of them, the new message and
        // the marker, and not four.
        let prior = Conversation {
            messages: turns(5, 1000),
            ..Conversation::default()
        };
        let cap = 4_500;
        let next = prior.continued_within("next", RunId::new(), cap);

        assert!(is_marker(&next.messages[0]));
        assert!(next.messages[0].text().starts_with(OMITTED_MARKER_PREFIX));
        // The first kept message starts a turn: no half turns.
        assert_eq!(next.messages[1].text(), "task 2");
        assert_eq!(next.messages.last().unwrap().text(), "next");
        // Whole turns only: the kept ones are the newest, in order, unchanged.
        let kept = &next.messages[1..next.messages.len() - 1];
        assert_eq!(kept, &prior.messages[8..]);
        assert_eq!(next.messages.iter().filter(|m| is_marker(m)).count(), 1);
        assert!(
            total_len(&next.messages) <= cap,
            "{}",
            total_len(&next.messages)
        );
        // No tool call lost its result in what is kept.
        for m in kept {
            if let Message::Assistant { tool_calls, .. } = m {
                for c in tool_calls {
                    assert!(
                        kept.iter().any(
                            |r| matches!(r, Message::Tool { call_id, .. } if *call_id == c.id)
                        )
                    );
                }
            }
        }
    }

    #[test]
    fn continuing_again_replaces_the_marker_instead_of_stacking_them() {
        let prior = Conversation {
            messages: turns(5, 1000),
            ..Conversation::default()
        };
        let once = prior.continued_within("next", RunId::new(), 4_500);
        assert!(is_marker(&once.messages[0]));
        // Still over this smaller cap: one more turn goes, with the old marker, and there is one
        // new marker.
        let cap = 3_000;
        let twice = once.continued_within("and again", RunId::new(), cap);
        assert_eq!(twice.messages.iter().filter(|m| is_marker(m)).count(), 1);
        assert!(is_marker(&twice.messages[0]));
        assert_eq!(twice.messages[1].text(), "task 3");
        assert_eq!(twice.messages.last().unwrap().text(), "and again");
        assert!(total_len(&twice.messages) <= cap);
        // Under the cap, the marker of an earlier omission stays where it is.
        let small = once.continued_within("tiny", RunId::new(), 1_000_000);
        assert_eq!(small.messages.len(), once.messages.len() + 1);
        assert!(is_marker(&small.messages[0]));
    }

    #[test]
    fn the_newest_turn_is_carried_whole_even_when_it_alone_is_over_the_cap() {
        let prior = Conversation {
            messages: turns(1, 10_000),
            ..Conversation::default()
        };
        let next = prior.continued_within("next", RunId::new(), 1_000);
        assert_eq!(next.messages.len(), 5);
        assert!(!next.messages.iter().any(is_marker));

        // With older turns before it, they go and it stays.
        let prior = Conversation {
            messages: turns(3, 10_000),
            ..Conversation::default()
        };
        let next = prior.continued_within("next", RunId::new(), 1_000);
        assert!(is_marker(&next.messages[0]));
        assert_eq!(&next.messages[1..5], &prior.messages[8..]);
        assert_eq!(next.messages.len(), 6);
    }

    #[test]
    fn the_default_cap_is_256_kib_of_json() {
        assert_eq!(MAX_CARRIED_BYTES, 262_144);
        let prior = Conversation {
            messages: turns(3, 100_000),
            ..Conversation::default()
        };
        assert!(total_len(&prior.messages) > MAX_CARRIED_BYTES);
        let next = prior.continued("next", RunId::new());
        assert!(is_marker(&next.messages[0]));
        assert_eq!(next.messages[1].text(), "task 1");
        assert!(total_len(&next.messages) <= MAX_CARRIED_BYTES);
    }
}
