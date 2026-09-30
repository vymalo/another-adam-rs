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
    /// The history, oldest first. Nothing is removed from it while the run goes on: history
    /// limits only shape what is sent to the model. A run that continues another starts from a
    /// bounded copy of the earlier history instead ([`Conversation::continued`]: old tool
    /// outputs shortened, and as a last resort old turns left out).
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
    /// How many turns of earlier conversation were left out to keep a continued run's carried
    /// history within [`MAX_CARRIED_BYTES`], summed over every continuation in the chain. While it
    /// is not zero the **second text part of the first user message** is a marker that says so
    /// (it starts with [`OMITTED_MARKER_PREFIX`]); this count, and not the text, is what says the
    /// part is a marker. Absent from state written before it existed, and not written while it is
    /// zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_turns: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// The most serialized JSON, in bytes, of messages that a continued conversation carries from the
/// conversation it continues ([`Conversation::continued`]): 256 KiB.
///
/// Why a cap at all: every continuation copies the carried history into the new run's state, which
/// is one JSON value in the store, so without a bound each task in a long-lived context would
/// store, and each model call would re-read, everything said before. Why this size: it is about
/// what `Limits::max_history_tokens` lets through to the model (100 000 tokens of about 4
/// characters). Bytes of JSON, not tokens: the cap has to be checkable without a tokenizer, and it
/// over-counts the JSON's own punctuation, which only makes it stricter.
///
/// What is given up to meet it, in this order, and only as far as needed:
///
/// 1. **Old tool outputs are shortened**, oldest first, each keeping its head and ending in the
///    [`TRUNCATION_MARKER_PREFIX`](crate::TRUNCATION_MARKER_PREFIX) marker: what the model
///    already does to a long history when it is sent ([`Limits::max_history_tokens`](crate::Limits)),
///    and the output of a tool is the bulk of a coding run. No call loses its result.
/// 2. **Whole old turns are dropped**, oldest first (a turn is a user message and everything that
///    followed it up to the next user message), and one marker text
///    ([`OMITTED_MARKER_PREFIX`]) says how many. This is what the model is *not* already shown in
///    full, so it is the last resort.
///
/// Two things are never given up: **the first user message of the chain** (the task the whole
/// conversation is about, kept verbatim as the first text part of the first message) and **the
/// newest prior turn** (what the new message most likely follows up on), except for step 1
/// shortening its tool outputs. A newest turn whose own assistant text and tool-call arguments are
/// larger than the cap is therefore carried over the cap: one run's own limits bound it, and the
/// next continuation shortens or drops it.
pub const MAX_CARRIED_BYTES: usize = 256 * 1024;

/// How the text of the marker that stands in for dropped turns begins. The marker is the second
/// text part of the first user message ([`Conversation::omitted_turns`] says it is there), so a
/// reader that looks at what the user said, such as the coder's rule about which repositories a
/// task named, can skip it by this prefix.
pub const OMITTED_MARKER_PREFIX: &str = "[earlier conversation omitted";

fn omission_marker(turns: u32) -> Message {
    Message::user_text(format!(
        "{OMITTED_MARKER_PREFIX}: {turns} earlier turn(s) left out to keep the carried history within its limit]"
    ))
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

/// Undoes what the last continuation did to the first message when it dropped turns: the marker
/// (its second part, which `omitted` says is there) is taken out, and whatever was merged into the
/// message after it goes back to being a user message of its own, so that the turns are again
/// separate ones that can be dropped. A count that finds no marker where it should be (state
/// edited by hand) counts for nothing.
fn split_head(prior: &mut Vec<Message>, omitted: u32) -> u32 {
    if omitted == 0 {
        return 0;
    }
    let Some(Message::User { content }) = prior.first_mut() else {
        return 0;
    };
    if !content
        .get(1)
        .is_some_and(|part| part.as_text().starts_with(OMITTED_MARKER_PREFIX))
    {
        return 0;
    }
    let rest = content.split_off(2);
    content.truncate(1);
    if !rest.is_empty() {
        prior.insert(1, Message::User { content: rest });
    }
    omitted
}

/// Adjacent user messages become one message with all their parts, in order. Chat templates that
/// require the roles to alternate (and some providers) reject two user messages in a row, which
/// the carried history would otherwise have after a marker, after a prior run that ended before
/// the model answered, and after user messages that were waiting behind a tool result.
fn merge_adjacent_users(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            Message::User { content } => match out.last_mut() {
                Some(Message::User { content: before }) => before.extend(content),
                _ => out.push(Message::User { content }),
            },
            other => out.push(other),
        }
    }
    out
}

/// The history a continued run starts from: `prior`, then `tail` (user messages that are never
/// cut), within `cap` bytes of JSON where that can be done without giving up what
/// [`MAX_CARRIED_BYTES`] says is never given up. Returns the messages and the total number of turns
/// left out so far (`omitted` and what this call drops).
fn carry(
    mut prior: Vec<Message>,
    omitted: u32,
    tail: Vec<Message>,
    cap: usize,
) -> (Vec<Message>, u32) {
    let omitted = split_head(&mut prior, omitted);
    let mut sizes: Vec<usize> = prior.iter().map(json_len).collect();
    let tail_len: usize = tail.iter().map(json_len).sum();
    let mut total = sizes.iter().sum::<usize>() + tail_len;
    // What the marker can take, for any count.
    let marker_len = json_len(&omission_marker(u32::MAX));
    let reserve = |omitted_now: u32| if omitted_now > 0 { marker_len } else { 0 };

    // 1. Shorten tool outputs, oldest first, as far as needed.
    for (message, size) in prior.iter_mut().zip(sizes.iter_mut()) {
        let needed = total + reserve(omitted);
        if needed <= cap {
            break;
        }
        let Message::Tool { content, .. } = &mut *message else {
            continue;
        };
        // Two characters more than needed: the marker starts with a newline, which JSON writes
        // as two bytes, and this way one output is enough where one can be.
        let Some(shortened) = crate::history::shorten_output(content, needed - cap + 2) else {
            continue;
        };
        *content = shortened;
        let now = json_len(message);
        total = total - *size + now;
        *size = now;
    }

    // 2. Drop whole turns, oldest first, never the first user message or the newest turn. The
    //    turns are consecutive, so what goes is one stretch after the first user message.
    let starts: Vec<usize> = prior
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::User { .. }))
        .map(|(i, _)| i)
        .collect();
    let mut dropped_turns = 0;
    if let Some(&first) = starts.first() {
        let mut cut = first + 1;
        for &end in &starts[1..] {
            if total + reserve(omitted + dropped_turns) <= cap {
                break;
            }
            let size: usize = sizes[cut..end].iter().sum();
            if end > cut {
                total -= size;
                dropped_turns += 1;
            }
            cut = end;
        }
        prior.drain(first + 1..cut);
        let omitted_now = omitted + dropped_turns;
        if omitted_now > 0 {
            prior.insert(first + 1, omission_marker(omitted_now));
        }
    }
    prior.extend(tail);
    (merge_adjacent_users(prior), omitted + dropped_turns)
}

impl Conversation {
    /// A conversation that starts with one user message.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user_text(text)],
            ..Self::default()
        }
    }

    /// Whether text part `part` of message `message` is the marker that says turns were left out
    /// ([`omitted_turns`](Self::omitted_turns)): the second part of the first message, while turns
    /// have been omitted. Anything else that merely starts like the marker is not one, so a rule
    /// that reads what the user said can skip exactly the marker and nothing the user wrote.
    #[must_use]
    pub fn is_omission_marker(&self, message: usize, part: usize) -> bool {
        self.omitted_turns > 0
            && message == 0
            && part == 1
            && matches!(
                self.messages.first(),
                Some(Message::User { content })
                    if content.get(1).is_some_and(|p| p.as_text().starts_with(OMITTED_MARKER_PREFIX))
            )
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
    /// * **Recorded:** `continued_from`, and in `omitted_turns` how many turns have been left
    ///   out, in this and earlier continuations.
    /// * **Bounded:** by [`MAX_CARRIED_BYTES`]: old tool outputs are shortened first, whole old
    ///   turns are dropped only after that, and the first user message is never dropped.
    /// * **Alternating:** user messages that would end up next to each other (the marker and the
    ///   message after it, the new one after a history that ends with a user message, the ones
    ///   that were waiting) become one message with several text parts, so that the roles
    ///   alternate as chat templates that insist on it need.
    #[must_use]
    pub fn continued(&self, text: impl Into<String>, from: RunId) -> Self {
        self.continued_within(text, from, MAX_CARRIED_BYTES)
    }

    fn continued_within(&self, text: impl Into<String>, from: RunId, cap: usize) -> Self {
        let mut prior = self.messages.clone();
        drop_unanswered_calls(&mut prior);
        // The deferred messages and the new one are the tail: counted, never cut, and not turns of
        // the prior, so the newest turn that is protected is the prior's own.
        let mut tail = self.deferred.clone();
        tail.push(Message::user_text(text));
        let (messages, omitted_turns) = carry(prior, self.omitted_turns, tail, cap);
        Self {
            messages,
            omitted_turns,
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

    /// A user message with these text parts.
    fn user_of(parts: &[&str]) -> Message {
        Message::User {
            content: parts
                .iter()
                .map(|t| adam_model::ContentPart::text(*t))
                .collect(),
        }
    }

    /// The text parts of a user message.
    fn parts_of(m: &Message) -> Vec<String> {
        match m {
            Message::User { content } => content.iter().map(|p| p.as_text().to_owned()).collect(),
            other => panic!("not a user message: {other:?}"),
        }
    }

    /// The roles of a history as a chat template that insists on alternation reads them: it starts
    /// with a user message, a user message follows an assistant message or a tool result (never
    /// another user message), an assistant message follows a user message or a tool result, and a
    /// tool result follows the assistant message that called it or another result.
    #[track_caller]
    fn assert_alternating(messages: &[Message]) {
        for (i, m) in messages.iter().enumerate() {
            let before = i.checked_sub(1).map(|j| &messages[j]);
            let ok = match (before, m) {
                (None, Message::User { .. }) => true,
                (Some(Message::Assistant { .. } | Message::Tool { .. }), Message::User { .. }) => {
                    true
                }
                (Some(Message::User { .. } | Message::Tool { .. }), Message::Assistant { .. }) => {
                    true
                }
                (Some(Message::Assistant { tool_calls, .. }), Message::Tool { call_id, .. }) => {
                    tool_calls.iter().any(|c| c.id == *call_id)
                }
                (Some(Message::Tool { .. }), Message::Tool { .. }) => true,
                _ => false,
            };
            assert!(ok, "message {i} breaks the alternation: {messages:#?}");
        }
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
        // In the order they arrived, as the parts of the one user message that follows the answer.
        assert_eq!(
            next.messages,
            [
                Message::user_text("first task"),
                Message::assistant_text("done"),
                user_of(&["also this", "and this", "second task"]),
            ]
        );
        assert_alternating(&next.messages);
        assert!(next.deferred.is_empty());
    }

    #[test]
    fn a_tool_call_without_its_result_is_dropped_with_the_turn_it_belonged_to() {
        let run = RunId::new();
        let user = |t: &str| Message::user_text(t);

        // Nothing came back at all (a limit failed the run right after the model's turn): what is
        // left ends with the task, so the new message joins it instead of following it.
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1", "c2"]));
        let next = c.continued("more", run);
        assert_eq!(next.messages, [user_of(&["task", "more"])]);
        assert_alternating(&next.messages);

        // Some results came back, not all.
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1", "c2"]));
        c.messages.push(Message::tool_result("c1", "partial"));
        assert_eq!(
            c.continued("more", run).messages,
            [user_of(&["task", "more"])]
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
        assert_eq!(next.messages, [user_of(&["task", "more"])]);
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
        assert_alternating(&next.messages);

        // Answered calls are history, whatever came after them.
        let mut c = Conversation::new("task");
        c.messages.push(calls(&["c1", "c2"]));
        c.messages.push(Message::tool_result("c1", "one"));
        c.messages.push(Message::tool_error("c2", "two"));
        c.messages.push(Message::assistant_text("done"));
        let next = c.continued("more", run);
        assert_eq!(next.messages.len(), 6);
        assert_alternating(&next.messages);
    }

    /// A conversation of `n` turns: a user message, a tool exchange whose result is `size` bytes,
    /// and the answer.
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

    fn convo(messages: Vec<Message>) -> Conversation {
        Conversation {
            messages,
            ..Conversation::default()
        }
    }

    fn total_len(messages: &[Message]) -> usize {
        messages.iter().map(json_len).sum()
    }

    /// Whether a tool output was shortened by the truncation of the history.
    fn shortened(m: &Message) -> bool {
        matches!(m, Message::Tool { content, .. }
            if content.contains(crate::history::TRUNCATION_MARKER_PREFIX))
    }

    /// How many text parts of user messages are the omission marker.
    fn markers(messages: &[Message]) -> usize {
        messages
            .iter()
            .filter(|m| matches!(m, Message::User { .. }))
            .flat_map(parts_of)
            .filter(|t| t.starts_with(OMITTED_MARKER_PREFIX))
            .count()
    }

    /// Every user message text of the history, in order (a part at a time).
    fn said(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter(|m| matches!(m, Message::User { .. }))
            .flat_map(parts_of)
            .collect()
    }

    /// No call without its result in `messages`.
    #[track_caller]
    fn assert_calls_answered(messages: &[Message]) {
        for m in messages {
            if let Message::Assistant { tool_calls, .. } = m {
                for c in tool_calls {
                    assert!(
                        messages.iter().any(
                            |r| matches!(r, Message::Tool { call_id, .. } if *call_id == c.id)
                        ),
                        "{} has no result",
                        c.id
                    );
                }
            }
        }
    }

    #[test]
    fn under_the_cap_nothing_is_shortened_dropped_or_marked() {
        let prior = convo(turns(3, 1000));
        let next = prior.continued_within("next", RunId::new(), 100_000);
        assert_eq!(next.messages.len(), 13);
        assert_eq!(&next.messages[..12], &prior.messages[..]);
        assert_eq!(next.omitted_turns, 0);
        assert_eq!(markers(&next.messages), 0);
        assert!(!next.messages.iter().any(shortened));
        assert_alternating(&next.messages);
    }

    #[test]
    fn over_the_cap_tool_outputs_are_shortened_before_any_turn_is_dropped() {
        // Five turns of about 1280 bytes each, the bulk of it tool output. This cap is over by
        // about 900 bytes, which shortening the oldest output covers: every turn is carried.
        let prior = convo(turns(5, 1000));
        let cap = 5_600;
        assert!(total_len(&prior.messages) > cap + 500);
        let next = prior.continued_within("next", RunId::new(), cap);

        assert_eq!(next.omitted_turns, 0, "no turn had to go");
        assert_eq!(markers(&next.messages), 0);
        assert_eq!(next.messages.len(), 21, "every message is still there");
        assert_eq!(said(&next.messages).len(), 6);
        assert_alternating(&next.messages);
        assert_calls_answered(&next.messages);
        // Only as much as needed, oldest first: the first output lost its tail (keeping its head
        // and ending in the marker the history truncation uses), the others are whole.
        let outputs: Vec<&Message> = next
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .collect();
        assert_eq!(
            outputs.iter().map(|m| shortened(m)).collect::<Vec<_>>(),
            [true, false, false, false, false]
        );
        let Message::Tool { content, .. } = outputs[0] else {
            unreachable!()
        };
        assert!(content.starts_with("xxx"), "the head of the output stays");
        assert!(content.contains(crate::history::TRUNCATION_MARKER_PREFIX));
        assert!(total_len(&next.messages) <= cap);
        // Carried again under the same cap it needs nothing more: no marker on a marker.
        let again = next.continued_within("more", RunId::new(), cap + 100);
        assert_eq!(again.omitted_turns, 0);
        assert_eq!(markers(&again.messages), 0);
    }

    #[test]
    fn whole_turns_go_only_when_shortening_is_not_enough() {
        // With every output shortened to its marker five turns still take about 1700 bytes.
        let prior = convo(turns(5, 1000));
        let cap = 1_200;
        let next = prior.continued_within("next", RunId::new(), cap);

        assert!(next.omitted_turns >= 1 && next.omitted_turns < 5);
        assert_eq!(markers(&next.messages), 1);
        assert_alternating(&next.messages);
        assert_calls_answered(&next.messages);
        // The first user message is kept verbatim, as the first text part of the first message;
        // the marker is the second, and says how many turns it stands for.
        let head = parts_of(&next.messages[0]);
        assert_eq!(head[0], "task 0");
        assert!(head[1].starts_with(OMITTED_MARKER_PREFIX));
        assert!(
            head[1].contains(&format!(": {} earlier", next.omitted_turns)),
            "{}",
            head[1]
        );
        // The third part is what was the next kept user message, a turn start, unchanged: turns
        // 0 (its body) up to the one before it are the omitted ones.
        assert_eq!(head[2], format!("task {}", next.omitted_turns), "{head:?}");
        // The newest prior turn and the new message are there; the survivors' outputs had been
        // shortened first (they are all at their floor), the newest turn's included.
        let users = said(&next.messages);
        assert!(users.contains(&"task 4".to_owned()));
        assert_eq!(users.last().unwrap(), "next");
        assert!(
            next.messages
                .iter()
                .filter(|m| matches!(m, Message::Tool { .. }))
                .all(shortened)
        );
        assert!(total_len(&next.messages) <= cap);
    }

    #[test]
    fn a_huge_single_turn_keeps_the_task_through_two_continuations() {
        // One coding run: a task, forty tool exchanges of 100 KB each, an answer. Far over the
        // cap, and the old rule would have dropped the whole turn (the task with it) at the
        // second rework.
        let big_run = |task: &str, tag: &str| {
            let mut m = vec![Message::user_text(task)];
            for i in 0..40 {
                let id = format!("{tag}-{i}");
                m.push(calls(&[&id]));
                m.push(Message::tool_result(id, "o".repeat(100_000)));
            }
            m.push(Message::assistant_text(format!("done {tag}")));
            m
        };
        let task = "implement X in repo R";
        let first = convo(big_run(task, "a"));
        assert!(total_len(&first.messages) > 10 * MAX_CARRIED_BYTES);

        // First rework.
        let second = first.continued("rework it", RunId::new());
        assert_eq!(second.omitted_turns, 0, "one turn alone is never dropped");
        assert_eq!(second.messages[0], Message::user_text(task));
        assert_alternating(&second.messages);
        assert_calls_answered(&second.messages);
        assert!(total_len(&second.messages) <= MAX_CARRIED_BYTES);
        assert_eq!(said(&second.messages), [task, "rework it"]);
        assert!(second.messages.iter().any(shortened));

        // The rework ran, and is as big; second rework.
        let mut worked = second.clone();
        worked
            .messages
            .extend(big_run("x", "b").into_iter().skip(1));
        let third = worked.continued("and once more", RunId::new());
        assert_eq!(
            third.messages[0],
            Message::user_text(task),
            "the task survives"
        );
        assert_eq!(said(&third.messages), [task, "rework it", "and once more"]);
        assert_alternating(&third.messages);
        assert_calls_answered(&third.messages);
        assert!(total_len(&third.messages) <= MAX_CARRIED_BYTES);
        assert!(third.continued_from.is_some());
    }

    #[test]
    fn the_first_user_message_survives_a_long_chain_under_a_small_cap() {
        let task = "the original task";
        let mut c = convo(vec![
            Message::user_text(task),
            Message::assistant_text("ok"),
        ]);
        let mut last_omitted = 0;
        for round in 0..12 {
            let next = c.continued_within(format!("rework {round}"), RunId::new(), 3_000);
            assert_eq!(parts_of(&next.messages[0])[0], task, "round {round}");
            assert!(next.omitted_turns >= last_omitted, "round {round}");
            assert_eq!(
                next.omitted_turns > 0,
                markers(&next.messages) == 1,
                "round {round}"
            );
            assert!(markers(&next.messages) <= 1, "markers do not stack");
            assert_alternating(&next.messages);
            assert_calls_answered(&next.messages);
            assert_eq!(
                said(&next.messages).last().unwrap(),
                &format!("rework {round}")
            );
            last_omitted = next.omitted_turns;
            // The rework runs: a tool exchange with a big output and an answer.
            c = next;
            c.messages.push(calls(&[&format!("r{round}")]));
            c.messages
                .push(Message::tool_result(format!("r{round}"), "y".repeat(2_000)));
            c.messages
                .push(Message::assistant_text(format!("done {round}")));
        }
        assert!(last_omitted > 0, "the chain was long enough to drop turns");
    }

    #[test]
    fn continuing_again_replaces_the_marker_instead_of_stacking_them() {
        let prior = convo(turns(6, 1000));
        let once = prior.continued_within("next", RunId::new(), 1_200);
        assert_eq!(markers(&once.messages), 1);
        let first_count = once.omitted_turns;
        assert!(first_count >= 1);

        // Still over a smaller cap: more turns go, there is still one marker, and it counts all.
        let twice = once.continued_within("and again", RunId::new(), 800);
        assert_eq!(markers(&twice.messages), 1);
        assert!(twice.omitted_turns > first_count);
        let head = parts_of(&twice.messages[0]);
        assert_eq!(head[0], "task 0");
        assert!(head[1].starts_with(OMITTED_MARKER_PREFIX));
        assert!(head[1].contains(&format!(": {} earlier", twice.omitted_turns)));
        assert_eq!(said(&twice.messages).last().unwrap(), "and again");
        assert_alternating(&twice.messages);

        // Under a cap that fits, the marker of an earlier omission stays, with its count, and the
        // turns merged behind it are turns of their own again.
        let small = once.continued_within("tiny", RunId::new(), 1_000_000);
        assert_eq!(small.omitted_turns, first_count);
        assert_eq!(markers(&small.messages), 1);
        // "next" was the last message and a user message: "tiny" joins it.
        assert_eq!(small.messages.len(), once.messages.len());
        assert_eq!(parts_of(small.messages.last().unwrap()), ["next", "tiny"]);
        assert_eq!(
            small.messages[..small.messages.len() - 1],
            once.messages[..once.messages.len() - 1]
        );
        assert_alternating(&small.messages);
    }

    #[test]
    fn the_newest_turn_is_carried_even_when_its_own_text_is_over_the_cap() {
        // Its tool outputs are shortened like any other, but what the assistant said is not
        // touched, and nothing drops the turn: the cap is overshot by it alone.
        let mut newest = vec![Message::user_text("newest")];
        newest.push(Message::assistant_text("a".repeat(20_000)));
        let mut messages = turns(3, 10_000);
        messages.extend(newest);
        let prior = convo(messages);
        let next = prior.continued_within("next", RunId::new(), 1_000);

        // What the assistant said in the newest turn is whole, and the turn's user message is
        // the third part of the first message (behind the task and the marker).
        let marker = omission_marker(3).text();
        assert_eq!(
            parts_of(&next.messages[0]),
            ["task 0", marker.as_str(), "newest"]
        );
        assert_eq!(
            next.messages[1],
            Message::assistant_text("a".repeat(20_000))
        );
        assert_eq!(next.messages.last().unwrap(), &Message::user_text("next"));
        // The older turns are gone but for the task; the marker says how many.
        assert_eq!(next.omitted_turns, 3);
        assert_eq!(said(&next.messages)[0], "task 0");
        assert_alternating(&next.messages);

        // One turn only: nothing to drop at all, and its output is shortened to fit what it can.
        let prior = convo(turns(1, 10_000));
        let next = prior.continued_within("next", RunId::new(), 1_000);
        assert_eq!(next.omitted_turns, 0);
        assert_eq!(next.messages.len(), 5);
        assert!(shortened(&next.messages[2]));
    }

    #[test]
    fn deferred_messages_do_not_make_the_priors_last_turn_droppable() {
        // The waiting messages and the new one are the tail, not turns of the prior: the newest
        // prior turn is still the one that is protected, and the older ones go instead.
        let mut prior = convo(turns(4, 1_000));
        prior.deferred = vec![Message::user_text("waiting")];
        let next = prior.continued_within("next", RunId::new(), 1_000);

        assert!(next.omitted_turns >= 1);
        let users = said(&next.messages);
        assert!(users.contains(&"task 3".to_owned()), "{users:?}");
        assert_eq!(&users[users.len() - 2..], ["waiting", "next"]);
        assert!(
            next.messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { content, .. } if content.first().is_some_and(|p| p.as_text() == "answer 3")))
        );
        assert_alternating(&next.messages);
    }

    #[test]
    fn the_roles_alternate_in_every_shape_a_continuation_can_take() {
        let run = RunId::new();
        // The prior ended on a user message that nothing answered (failed before the model did).
        let next = convo(vec![Message::user_text("task")]).continued("more", run);
        assert_eq!(next.messages, [user_of(&["task", "more"])]);
        assert_alternating(&next.messages);

        // A prior that ends with results and leftover user messages (a question never answered,
        // messages that arrived meanwhile).
        let mut c = convo(vec![Message::user_text("task"), calls(&["c1"])]);
        c.messages.push(Message::tool_result("c1", "out"));
        c.messages.push(calls(&["c2"]));
        c.deferred = vec![Message::user_text("d1"), Message::user_text("d2")];
        let next = c.continued("more", run);
        assert_alternating(&next.messages);
        assert_eq!(
            said(&next.messages),
            ["task", "d1", "d2", "more"],
            "{:#?}",
            next.messages
        );

        // Marker, first kept message, deferred and new one, all at once.
        let mut c = convo(turns(6, 1_000));
        c.deferred = vec![Message::user_text("d1")];
        let next = c.continued_within("more", run, 1_200);
        assert!(next.omitted_turns > 0);
        assert_alternating(&next.messages);
        assert_eq!(said(&next.messages).last().unwrap(), "more");
    }

    #[test]
    fn a_user_message_that_starts_like_the_marker_is_only_a_user_message() {
        let run = RunId::new();
        let lookalike = format!("{OMITTED_MARKER_PREFIX} by me, please ignore]");

        // In the middle of a conversation it starts a turn like any other: over the cap it is
        // dropped as one, counted, and no earlier turn is merged into it.
        let mut messages = turns(2, 1_000);
        messages.push(Message::user_text(lookalike.clone()));
        messages.push(Message::assistant_text("noted"));
        messages.extend(turns(2, 1_000));
        let next = convo(messages).continued_within("next", run, 1_200);
        assert!(next.omitted_turns >= 1);
        assert_eq!(
            markers(&next.messages),
            1 + usize::from(said(&next.messages).contains(&lookalike))
        );

        // As the new message on a history that ends with the task it becomes the second part of
        // the first message. Nothing has been omitted, so it is not a marker, and a later
        // continuation keeps it.
        let once = convo(vec![Message::user_text("task")]).continued(lookalike.clone(), run);
        assert_eq!(once.omitted_turns, 0);
        assert_eq!(parts_of(&once.messages[0]), ["task", &lookalike]);
        let twice = once.continued("more", run);
        assert_eq!(
            parts_of(&twice.messages[0]),
            ["task", lookalike.as_str(), "more"]
        );
        assert_eq!(twice.omitted_turns, 0);
    }

    #[test]
    fn only_the_marker_is_the_marker() {
        let next = convo(turns(6, 1_000)).continued_within("next", RunId::new(), 1_200);
        assert!(next.omitted_turns > 0);
        assert!(next.is_omission_marker(0, 1));
        // The task, and the message that follows the marker, are the user's.
        assert!(!next.is_omission_marker(0, 0));
        assert!(!next.is_omission_marker(0, 2));
        assert!(!next.is_omission_marker(1, 1));
        // Without omitted turns nothing is a marker, whatever it says.
        let lookalike = format!("{OMITTED_MARKER_PREFIX} by me]");
        let once = convo(vec![Message::user_text("task")]).continued(lookalike, RunId::new());
        assert_eq!(once.omitted_turns, 0);
        assert!(!once.is_omission_marker(0, 1));
    }

    #[test]
    fn a_count_without_a_marker_counts_for_nothing() {
        // State edited by hand: it says turns were omitted but the first message has no marker.
        let mut c = convo(turns(2, 10));
        c.omitted_turns = 4;
        let next = c.continued("more", RunId::new());
        assert_eq!(next.omitted_turns, 0);
        assert_eq!(markers(&next.messages), 0);
        assert_alternating(&next.messages);
    }

    #[test]
    fn omitted_turns_is_only_stored_when_there_are_some() {
        let c = convo(turns(1, 10));
        assert!(
            serde_json::to_value(&c)
                .unwrap()
                .get("omitted_turns")
                .is_none()
        );
        let old: Conversation = serde_json::from_str(OLD_PARKED_ON_A_QUESTION).unwrap();
        assert_eq!(old.omitted_turns, 0);

        let next = convo(turns(6, 1_000)).continued_within("next", RunId::new(), 1_200);
        let stored = serde_json::to_value(&next).unwrap();
        assert_eq!(stored["omitted_turns"], json!(next.omitted_turns));
        assert_eq!(
            serde_json::from_value::<Conversation>(stored).unwrap(),
            next
        );
    }

    #[test]
    fn the_default_cap_is_256_kib_of_json() {
        assert_eq!(MAX_CARRIED_BYTES, 262_144);
        let prior = convo(turns(3, 100_000));
        assert!(total_len(&prior.messages) > MAX_CARRIED_BYTES);
        let next = prior.continued("next", RunId::new());
        // Shortening the oldest outputs is enough: nothing is dropped, nothing marked.
        assert_eq!(next.omitted_turns, 0);
        assert_eq!(markers(&next.messages), 0);
        assert_eq!(said(&next.messages), ["task 0", "task 1", "task 2", "next"]);
        assert!(total_len(&next.messages) <= MAX_CARRIED_BYTES);
        assert!(shortened(&next.messages[2]));
        assert!(!shortened(&next.messages[10]));
    }
}
