//! A run's streamed text ([`RunEvent::TextDelta`](adam_runtime::RunEvent::TextDelta)) as A2A messages:
//! `text-stream/v1`.
//!
//! The reply of a model turn reaches a client **as it is written**, in **chunks**: a
//! `TaskArtifactUpdateEvent` whose artifact is the stream (its id is the stream id `S`), with one text
//! part, the piece, and under the extension's URI in the artifact's `metadata` where the piece begins:
//!
//! ```json
//! {"artifact": {"artifactId": "run-m2-a1b2c3d4", "name": "reply", "parts": [{"text": "onacci "}],
//!               "extensions": ["https://agents.vymalo.com/a2a/extensions/text-stream/v1"],
//!               "metadata": {"https://agents.vymalo.com/a2a/extensions/text-stream/v1": {"offset": 3}}},
//!  "append": true, "lastChunk": false}
//! ```
//!
//! and says **once** what the whole text is, in a status message that carries
//! `{"streamId": "S"}` under the URI in its **metadata**: on a `working` status for words written
//! before a tool call (best effort, [`words_message`]) and on the status that ends the turn when its
//! text is the streamed words (`completed`, or `input-required` for a reply the agent turned into a
//! question; [`streamed_status`], durable). The chunks are transient: they are never in a
//! `GetTask`, and a subscription does not replay them. The contract is the orchestration layer's
//! (`docs/api/text-stream-v1.md` of `vymalo/another-agentic-system`).
//!
//! Only a client whose request activated `text-stream/v1` is sent chunks, and only it is sent the
//! words as a status of their own; the marker on the completed status is for everyone (it is data
//! under a namespaced key, which a client that does not know the extension ignores).

use std::collections::HashMap;

use a2a::{Artifact, Message, Part, Role, TaskArtifactUpdateEvent, TaskState};
use adam_a2a::{TEXT_STREAM_EXTENSION, TEXT_STREAM_KIND_REASONING};
use adam_runtime::{MAX_STREAM_ID_BYTES, RunView};
use serde_json::{Map, Value, json};

/// The name of a stream's artifact: a reply.
const ARTIFACT_NAME: &str = "reply";

/// The name of a reasoning stream's artifact.
const REASONING_ARTIFACT_NAME: &str = "reasoning";

/// Whether `id` is a stream id the contract takes: 1 to [`MAX_STREAM_ID_BYTES`] bytes, no control
/// characters.
pub(crate) fn is_stream_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_STREAM_ID_BYTES && !id.chars().any(char::is_control)
}

/// The chunk of stream `stream` that begins at byte `offset` with `text`: the first piece
/// (`offset` 0) has `append: false`, the next ones `append: true`; the last has `lastChunk: true`, and
/// `abandoned: true` when the model failed before the text was whole. `None` for an id the contract
/// does not take (the piece is dropped: it is best effort).
pub(crate) fn chunk(
    task_id: &str,
    context_id: &str,
    stream: &str,
    offset: u64,
    text: String,
    last: bool,
    abandoned: bool,
) -> Option<TaskArtifactUpdateEvent> {
    build(
        task_id,
        context_id,
        &Piece {
            stream,
            offset,
            text,
            last,
            abandoned,
            reasoning: false,
        },
    )
}

/// The chunk of a **reasoning** stream: the same as [`chunk`], with the artifact named `reasoning`
/// and `"kind": "reasoning"` beside the `offset`, so a reader says it apart from the reply. Its
/// whole text is never stated (reasoning is not an answer: no status message carries it).
pub(crate) fn reasoning_chunk(
    task_id: &str,
    context_id: &str,
    stream: &str,
    offset: u64,
    text: String,
    last: bool,
    abandoned: bool,
) -> Option<TaskArtifactUpdateEvent> {
    build(
        task_id,
        context_id,
        &Piece {
            stream,
            offset,
            text,
            last,
            abandoned,
            reasoning: true,
        },
    )
}

/// One piece of a stream, whichever it is.
struct Piece<'a> {
    stream: &'a str,
    offset: u64,
    text: String,
    last: bool,
    abandoned: bool,
    reasoning: bool,
}

fn build(task_id: &str, context_id: &str, piece: &Piece<'_>) -> Option<TaskArtifactUpdateEvent> {
    if !is_stream_id(piece.stream) {
        return None;
    }
    let mut entry = Map::new();
    entry.insert("offset".into(), json!(piece.offset));
    if piece.reasoning {
        entry.insert("kind".into(), json!(TEXT_STREAM_KIND_REASONING));
    }
    if piece.abandoned {
        entry.insert("abandoned".into(), json!(true));
    }
    Some(TaskArtifactUpdateEvent {
        task_id: task_id.to_owned(),
        context_id: context_id.to_owned(),
        artifact: Artifact {
            artifact_id: piece.stream.to_owned(),
            name: Some(
                if piece.reasoning {
                    REASONING_ARTIFACT_NAME
                } else {
                    ARTIFACT_NAME
                }
                .to_owned(),
            ),
            description: None,
            parts: vec![Part::text(piece.text.clone())],
            metadata: Some(HashMap::from([(
                TEXT_STREAM_EXTENSION.to_owned(),
                Value::Object(entry),
            )])),
            extensions: Some(vec![TEXT_STREAM_EXTENSION.to_owned()]),
        },
        append: Some(piece.offset > 0),
        last_chunk: Some(piece.last),
        metadata: None,
    })
}

/// `{URI: {"streamId": stream}}`: what says that a status message states the whole text of `stream`.
fn marker(stream: &str) -> HashMap<String, Value> {
    HashMap::from([(
        TEXT_STREAM_EXTENSION.to_owned(),
        json!({ "streamId": stream }),
    )])
}

/// The words of a model turn as the status message that states them: one text part, the whole text,
/// the stream's id as the message id, and the marker. For a turn that wrote words and then asked for
/// tools; the answer that ends a turn is stated by its own status ([`streamed_status`]).
pub(crate) fn words_message(stream: &str, text: String) -> Message {
    let mut message = Message::new(Role::Agent, vec![Part::text(text)]);
    message.message_id = stream.to_owned();
    message.metadata = Some(marker(stream));
    message.extensions = Some(vec![TEXT_STREAM_EXTENSION.to_owned()]);
    message
}

/// The stream and the whole text an `agent_text` event
/// ([`AGENT_TEXT_KIND`](adam_runtime::AGENT_TEXT_KIND)) carries, when the turn's words were
/// streamed: `{"text": .., "stream": ..}` with a stream id the contract takes and a text that is not
/// blank.
pub(crate) fn words_of(payload: &Value) -> Option<(&str, &str)> {
    let stream = payload
        .get("stream")?
        .as_str()
        .filter(|s| is_stream_id(s))?;
    let text = payload
        .get("text")?
        .as_str()
        .filter(|t| !t.trim().is_empty())?;
    Some((stream, text))
}

/// The stream the text of a task's status `state` was sent as, when it was: the status that ends the
/// turn states the whole text of the stream the model's words were, once.
///
/// * `completed`: the run's `output.stream`, the answer that ended it (`text` is the output's text);
/// * `input-required`: the question's own `pending_wait.stream`, set by an agent that turns the
///   model's reply into the question (the coder does, for a reply that delivers nothing), when
///   `text` is that question: a custom [`PromptFn`](crate::PromptFn) that says something else says
///   something that was not streamed.
///
/// Only an id the contract takes.
pub(crate) fn streamed_status<'v>(
    view: &'v RunView,
    state: &TaskState,
    text: &str,
) -> Option<&'v str> {
    let stream = match state {
        TaskState::Completed => view.output.as_ref()?.get("stream")?,
        TaskState::InputRequired => {
            let wait = view.state.get("pending_wait")?;
            (wait.get("question")?.as_str()? == text)
                .then(|| wait.get("stream"))
                .flatten()?
        }
        _ => return None,
    };
    stream.as_str().filter(|s| is_stream_id(s))
}

/// Marks the status message of a turn whose text was streamed with the stream's id, so a client
/// that read the chunks reads this text as the whole of them.
pub(crate) fn mark_status(message: &mut Message, stream: &str) {
    message.metadata = Some(marker(stream));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_id_is_one_to_128_printable_bytes() {
        assert!(is_stream_id("run-m0-a1b2c3d4"));
        assert!(is_stream_id(&"s".repeat(MAX_STREAM_ID_BYTES)));
        assert!(!is_stream_id(""));
        assert!(!is_stream_id(&"s".repeat(MAX_STREAM_ID_BYTES + 1)));
        assert!(!is_stream_id("a\nb"));
        assert!(!is_stream_id("a\u{0}b"));
        // The limit is bytes, not characters.
        assert!(!is_stream_id(&"é".repeat(MAX_STREAM_ID_BYTES / 2 + 1)));
    }

    #[test]
    fn a_chunk_is_the_contracts_artifact_update() {
        let first = chunk("t1", "c1", "s1", 0, "Fib".into(), false, false).expect("a chunk");
        assert_eq!(
            (first.task_id.as_str(), first.context_id.as_str()),
            ("t1", "c1")
        );
        assert_eq!(first.artifact.artifact_id, "s1");
        assert_eq!(first.artifact.name.as_deref(), Some("reply"));
        assert_eq!(first.artifact.parts.len(), 1);
        assert_eq!(first.artifact.parts[0], Part::text("Fib"));
        assert_eq!(
            first.artifact.extensions,
            Some(vec![TEXT_STREAM_EXTENSION.to_owned()])
        );
        assert_eq!(
            first.artifact.metadata.expect("metadata")[TEXT_STREAM_EXTENSION],
            json!({"offset": 0})
        );
        assert_eq!((first.append, first.last_chunk), (Some(false), Some(false)));

        let later = chunk("t1", "c1", "s1", 3, "onacci ".into(), false, false).expect("a chunk");
        assert_eq!((later.append, later.last_chunk), (Some(true), Some(false)));
        assert_eq!(
            later.artifact.metadata.expect("metadata")[TEXT_STREAM_EXTENSION],
            json!({"offset": 3})
        );
    }

    #[test]
    fn a_reasoning_chunk_is_a_chunk_of_its_own_stream_marked_with_its_kind() {
        let first = reasoning_chunk("t1", "c1", "s1-r", 0, "The user".into(), false, false)
            .expect("a chunk");
        assert_eq!(first.artifact.artifact_id, "s1-r");
        assert_eq!(first.artifact.name.as_deref(), Some("reasoning"));
        assert_eq!(first.artifact.parts, vec![Part::text("The user")]);
        assert_eq!(
            first.artifact.metadata.expect("metadata")[TEXT_STREAM_EXTENSION],
            json!({"offset": 0, "kind": "reasoning"})
        );
        assert_eq!((first.append, first.last_chunk), (Some(false), Some(false)));
        let last =
            reasoning_chunk("t1", "c1", "s1-r", 8, String::new(), true, true).expect("a chunk");
        assert_eq!((last.append, last.last_chunk), (Some(true), Some(true)));
        assert_eq!(
            last.artifact.metadata.expect("metadata")[TEXT_STREAM_EXTENSION],
            json!({"offset": 8, "kind": "reasoning", "abandoned": true})
        );
        // A reply chunk has no `kind`: the marker is the only difference a reader needs.
        let reply = chunk("t1", "c1", "s1", 0, "Hi".into(), false, false).expect("a chunk");
        assert!(
            reply.artifact.metadata.expect("metadata")[TEXT_STREAM_EXTENSION]
                .get("kind")
                .is_none()
        );
        assert!(reasoning_chunk("t1", "c1", "", 0, "x".into(), false, false).is_none());
    }

    #[test]
    fn the_last_chunk_may_be_empty_and_says_when_the_model_failed() {
        let done = chunk("t1", "c1", "s1", 10, String::new(), true, false).expect("a chunk");
        assert_eq!(done.last_chunk, Some(true));
        assert_eq!(done.artifact.parts[0], Part::text(""));
        assert_eq!(
            done.artifact.metadata.expect("metadata")[TEXT_STREAM_EXTENSION],
            json!({"offset": 10})
        );
        let failed = chunk("t1", "c1", "s1", 10, "in R".into(), true, true).expect("a chunk");
        assert_eq!(
            failed.artifact.metadata.expect("metadata")[TEXT_STREAM_EXTENSION],
            json!({"offset": 10, "abandoned": true})
        );
    }

    #[test]
    fn a_piece_of_a_stream_the_contract_does_not_take_is_dropped() {
        assert!(chunk("t1", "c1", "", 0, "x".into(), false, false).is_none());
        assert!(chunk("t1", "c1", "a\nb", 0, "x".into(), false, false).is_none());
    }

    #[test]
    fn the_words_before_a_tool_call_are_a_status_message_that_says_whose_they_are() {
        let message = words_message("s1", "Let me look.".into());
        assert_eq!(message.role, Role::Agent);
        assert_eq!(message.message_id, "s1");
        assert_eq!(message.text(), Some("Let me look."));
        assert_eq!(message.parts.len(), 1);
        assert_eq!(
            message.metadata.expect("metadata"),
            HashMap::from([(TEXT_STREAM_EXTENSION.to_owned(), json!({"streamId": "s1"}))])
        );
        assert_eq!(
            message.extensions,
            Some(vec![TEXT_STREAM_EXTENSION.to_owned()])
        );
    }

    #[test]
    fn an_agent_text_event_names_a_stream_only_when_the_words_were_streamed_and_are_words() {
        assert_eq!(
            words_of(&json!({"text": "Hi", "turn": 0, "stream": "s1"})),
            Some(("s1", "Hi"))
        );
        // Not streamed (a model that did not stream, or a journal from before): nothing to state.
        assert_eq!(words_of(&json!({"text": "Hi", "turn": 0})), None);
        // Not words, or not an id the contract takes.
        assert_eq!(words_of(&json!({"text": " \n", "stream": "s1"})), None);
        assert_eq!(words_of(&json!({"text": "Hi", "stream": ""})), None);
        assert_eq!(words_of(&json!({"text": "Hi", "stream": 3})), None);
        assert_eq!(words_of(&json!({"stream": "s1"})), None);
    }

    /// A run in `status` whose output is `output` and whose state is `state`.
    fn view(status: adam_core::RunStatus, output: Option<Value>, state: Value) -> RunView {
        RunView {
            id: adam_core::RunId::new(),
            agent: "a".into(),
            conversation_id: Some("s:c".into()),
            status,
            wake_at: None,
            waiting: status == adam_core::RunStatus::Parked,
            claimed: false,
            output,
            error: None,
            attempt: 0,
            pending_inbox: 0,
            artifacts: Vec::new(),
            state,
            version: 2,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_finished_run_says_its_answer_was_streamed_in_its_output() {
        use adam_core::RunStatus::Done;
        let done = |output| view(Done, Some(output), json!({}));
        let streamed = done(json!({"text": "Hi", "stream": "s1"}));
        assert_eq!(
            streamed_status(&streamed, &TaskState::Completed, "Hi"),
            Some("s1")
        );
        for output in [json!({"text": "Hi"}), json!("Hi"), json!({"stream": ""})] {
            assert_eq!(
                streamed_status(&done(output), &TaskState::Completed, "Hi"),
                None
            );
        }
        // The output says it only of the status that ends the run.
        assert_eq!(streamed_status(&streamed, &TaskState::Working, "Hi"), None);
        assert_eq!(streamed_status(&streamed, &TaskState::Failed, "Hi"), None);
    }

    #[test]
    fn a_reply_turned_into_a_question_says_its_stream_only_while_the_status_says_that_question() {
        use adam_core::RunStatus::Parked;
        let asking = |wait: Value| view(Parked, None, json!({"pending_wait": wait}));
        let reply = asking(json!({"call_id": "stop00001", "tool": "ask_user",
                                  "question": "Hi! I'm Coder.", "stream": "s1"}));
        assert_eq!(
            streamed_status(&reply, &TaskState::InputRequired, "Hi! I'm Coder."),
            Some("s1")
        );
        // Another text (a custom prompt function): it was not streamed.
        assert_eq!(
            streamed_status(&reply, &TaskState::InputRequired, "Which repository?"),
            None
        );
        // A tool's own question has no stream.
        let tools = asking(json!({"call_id": "c1", "tool": "ask_user", "question": "Which?"}));
        assert_eq!(
            streamed_status(&tools, &TaskState::InputRequired, "Which?"),
            None
        );
        // Not an id the contract takes.
        let bad = asking(json!({"call_id": "c1", "tool": "t", "question": "Q", "stream": ""}));
        assert_eq!(streamed_status(&bad, &TaskState::InputRequired, "Q"), None);
        // No question at all (a run that is not parked on one).
        let idle = view(Parked, None, json!({}));
        assert_eq!(streamed_status(&idle, &TaskState::InputRequired, "Q"), None);
    }

    #[test]
    fn a_status_that_states_a_stream_carries_only_the_marker() {
        let mut message = Message::new(Role::Agent, vec![Part::text("Fibonacci in Rust.")]);
        mark_status(&mut message, "s1");
        assert_eq!(
            message.metadata.expect("metadata")[TEXT_STREAM_EXTENSION],
            json!({"streamId": "s1"})
        );
        assert_eq!(message.extensions, None);
    }
}
