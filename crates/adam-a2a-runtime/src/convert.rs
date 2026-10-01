//! Pure mappings between the runtime's durable view of a run and A2A types.

use std::collections::HashMap;
use std::sync::Arc;

use a2a::{Artifact, Message, Part, Role, Task, TaskState, TaskStatus};
use adam_a2a::A2UI_MEDIA_TYPE;
use adam_core::RunStatus;
use adam_runtime::{Inbound, RunView};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ids::status_message_id;

/// Prefix `Runtime::cancel` puts on the error of a cancelled run.
pub(crate) const CANCEL_PREFIX: &str = "cancelled: ";

/// Extracts the question a run that is waiting for input asks its caller.
///
/// Given the durable view of the run, return the text to show as the
/// `input-required` status message, or `None` when there is none.
pub type PromptFn = Arc<dyn Fn(&RunView) -> Option<String> + Send + Sync>;

/// Turns an incoming A2A message into what the agent reads (start input and
/// follow-ups alike). An `Err` becomes `invalid params` for the client.
pub type InboundFn = Arc<dyn Fn(&Message) -> Result<Inbound, String> + Send + Sync>;

/// The default [`PromptFn`]: `state.pending_wait.question` (what
/// `adam_llm_agent::Conversation` stores while it waits for the user), or
/// `state.pending_question.question` (what it stored before it could also wait
/// for a child run, so runs parked by an older build keep their prompt) or,
/// failing both, a string at `state.question`. A run that waits for a child run
/// has no question and is `working`, not `input-required`.
pub fn default_prompt(view: &RunView) -> Option<String> {
    let state = &view.state;
    state
        .pointer("/pending_wait/question")
        .or_else(|| state.pointer("/pending_question/question"))
        .or_else(|| state.get("question"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The default [`InboundFn`]: the message's text parts (and data parts,
/// rendered as JSON) joined by blank lines become `{"text": ...}` with kind
/// `message` and the A2A `messageId` as the inbound id. A message without any
/// text is rejected.
pub fn default_inbound(message: &Message) -> Result<Inbound, String> {
    let mut chunks = Vec::new();
    for part in &message.parts {
        match &part.content {
            a2a::PartContent::Text(t) => chunks.push(t.clone()),
            a2a::PartContent::Data(v) => chunks.push(v.to_string()),
            _ => {}
        }
    }
    let text = chunks.join("\n\n");
    if text.trim().is_empty() {
        return Err("the message has no text".to_owned());
    }
    Ok(Inbound::new("message", json!({ "text": text })).with_id(message.message_id.clone()))
}

/// The A2A task state of a run.
///
/// | Run | Task |
/// |---|---|
/// | runnable, never committed by a worker | `submitted` |
/// | runnable, or parked on a timer | `working` |
/// | parked with no timer (`RunView::waiting`) | `input-required` |
/// | done | `completed` |
/// | failed with `cancelled: ...` | `canceled` |
/// | failed otherwise | `failed` |
pub fn task_state(view: &RunView) -> TaskState {
    match view.status {
        RunStatus::Done => TaskState::Completed,
        RunStatus::Failed => {
            if view
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with(CANCEL_PREFIX))
            {
                TaskState::Canceled
            } else {
                TaskState::Failed
            }
        }
        RunStatus::Parked if view.waiting => TaskState::InputRequired,
        RunStatus::Parked => TaskState::Working,
        // Version 1 is the record as created: no worker has committed yet.
        RunStatus::Runnable if view.version <= 1 => TaskState::Submitted,
        RunStatus::Runnable => TaskState::Working,
    }
}

/// Text of a run's output: `output.text` or the output itself if a string.
fn output_text(output: &Value) -> Option<String> {
    match output {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("text").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
    .filter(|t| !t.trim().is_empty())
}

/// Stable name of a state, for ids (the wire name, not `Debug`).
fn state_name(state: &TaskState) -> &'static str {
    match state {
        TaskState::Unspecified => "unspecified",
        TaskState::Submitted => "submitted",
        TaskState::Working => "working",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Canceled => "canceled",
        TaskState::InputRequired => "input-required",
        TaskState::Rejected => "rejected",
        TaskState::AuthRequired => "auth-required",
    }
}

/// The interface that comes with the question of a run waiting for input: an array of A2UI
/// messages at `state.pending_wait.ui` (what `adam_llm_agent::PendingQuestion::ui` stores), only
/// while the run is `input-required` and only when it is a non-empty array.
fn status_ui<'v>(view: &'v RunView, state: &TaskState) -> Option<&'v Value> {
    (*state == TaskState::InputRequired)
        .then(|| view.state.pointer("/pending_wait/ui"))
        .flatten()
        .filter(|ui| ui.as_array().is_some_and(|a| !a.is_empty()))
}

/// A short digest of the interface, for the status message id and the "did anything change" key:
/// two statuses with the same question and different interfaces are different statuses.
fn ui_digest(ui: &Value) -> String {
    let digest = Sha256::digest(serde_json::to_vec(ui).unwrap_or_default());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// A data part of A2UI messages, spelled the way A2A 1.0 does (`mediaType`) and the way the A2UI
/// extension does (`metadata.mimeType`), so that a client reads either.
fn a2ui_part(ui: Value) -> Part {
    let mut part = Part::data(ui).with_media_type(A2UI_MEDIA_TYPE);
    part.metadata = Some(HashMap::from([(
        "mimeType".to_owned(),
        json!(A2UI_MEDIA_TYPE),
    )]));
    part
}

/// The message that goes with the state, if there is anything to say.
fn status_text(view: &RunView, state: &TaskState, prompt: &PromptFn) -> Option<String> {
    match state {
        TaskState::InputRequired => prompt(view),
        TaskState::Completed => view.output.as_ref().and_then(output_text),
        TaskState::Failed => view.error.clone(),
        TaskState::Canceled => view
            .error
            .as_deref()
            .and_then(|e| e.strip_prefix(CANCEL_PREFIX))
            .map(str::to_owned),
        _ => None,
    }
}

/// The task status of a run, from its durable record only.
pub(crate) fn status_of(view: &RunView, prompt: &PromptFn) -> TaskStatus {
    let state = task_state(view);
    // The id follows the status, not the read: every event and snapshot of the
    // same status carries the same message id.
    let ui = status_ui(view, &state);
    let message = status_text(view, &state, prompt).map(|text| {
        let mut parts = vec![Part::text(text.clone())];
        // The interface of a question goes beside it, in a part of its own. Only a status that has
        // one gets the digest in its id, so every other id is what it was.
        let id_text = match ui {
            Some(ui) => {
                parts.push(a2ui_part(ui.clone()));
                format!("{text}\u{0}ui:{}", ui_digest(ui))
            }
            None => text,
        };
        let mut message = Message::new(Role::Agent, parts);
        message.message_id = status_message_id(&view.id.to_string(), state_name(&state), &id_text);
        message
    });
    TaskStatus {
        state,
        message,
        timestamp: Some(view.updated_at),
    }
}

/// What distinguishes two statuses for "did anything change": the state, the message text and a
/// digest of the A2UI part the message carries, if any.
pub(crate) type StatusKey = (TaskState, Option<String>, Option<String>);

/// The [`StatusKey`] of `status`.
pub(crate) fn status_key(status: &TaskStatus) -> StatusKey {
    let ui = status.message.as_ref().and_then(|m| {
        m.parts.iter().find_map(|part| match &part.content {
            a2a::PartContent::Data(ui) if crate::vymalo::claims_a2ui(part) => Some(ui_digest(ui)),
            _ => None,
        })
    });
    (
        status.state.clone(),
        status
            .message
            .as_ref()
            .and_then(|m| m.text())
            .map(str::to_owned),
        ui,
    )
}

/// The whole task, from the durable record only.
pub(crate) fn task_from_view(view: &RunView, context_id: &str, prompt: &PromptFn) -> Task {
    let artifacts: Vec<Artifact> = view.artifacts.iter().map(artifact_of).collect();
    Task {
        id: view.id.to_string(),
        context_id: context_id.to_owned(),
        status: status_of(view, prompt),
        artifacts: (!artifacts.is_empty()).then_some(artifacts),
        history: None,
        metadata: None,
    }
}

/// A run artifact as an A2A artifact.
///
/// String data becomes a text part, anything else a data part; the media type
/// rides on the part. The artifact id is derived from the content, so the
/// live event and the durable copy of the same artifact carry the same id and
/// a subscriber sees it once.
///
/// **A link is offered as a link.** When the data is a JSON object whose `url`
/// is an absolute `http(s)` URL (a pull request, a report), the artifact gets
/// a second part after the data part: an A2A `url` part (`Part.url` of the
/// A2A v1 `a2a.proto`: "a `url` pointing to the file's content") carrying that
/// URL, so a client shows a link where it would otherwise show JSON. The data
/// part is unchanged, and the artifact id does not depend on the extra part.
pub fn artifact_of(artifact: &adam_runtime::Artifact) -> Artifact {
    let part = match &artifact.data {
        Value::String(s) => Part::text(s.clone()),
        other => Part::data(other.clone()),
    };
    let mut part = match &artifact.mime_type {
        Some(mime) => part.with_media_type(mime.clone()),
        None => part,
    };
    // A2UI messages are also marked the way the A2UI extension marks them, so a client that reads
    // only `metadata.mimeType` finds them.
    if artifact.mime_type.as_deref() == Some(A2UI_MEDIA_TYPE) {
        part.metadata = Some(HashMap::from([(
            "mimeType".to_owned(),
            json!(A2UI_MEDIA_TYPE),
        )]));
    }
    let mut parts = vec![part];
    if let Some(url) = link_of(&artifact.data) {
        parts.push(Part::url(url));
    }
    Artifact {
        artifact_id: artifact_id(artifact),
        name: Some(artifact.name.clone()),
        description: None,
        parts,
        metadata: None,
        extensions: None,
    }
}

/// The `url` of an object-shaped artifact, when it is an absolute `http(s)` URL.
fn link_of(data: &Value) -> Option<&str> {
    let url = data.get("url")?.as_str()?;
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    (!rest.is_empty() && !url.chars().any(char::is_whitespace)).then_some(url)
}

/// Content-derived artifact id: `<name>-<12 hex digits>`.
pub fn artifact_id(artifact: &adam_runtime::Artifact) -> String {
    let digest = Sha256::digest(
        serde_json::to_vec(&(&artifact.name, &artifact.mime_type, &artifact.data))
            .unwrap_or_default(),
    );
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let name: String = artifact
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{name}-{hex}")
}

/// Conversation id under which a caller's context is stored: the subject is
/// part of it, so ownership is durable (it survives restarts and is visible in
/// the store) and two callers never share a context by accident.
pub(crate) fn encode_conversation(subject: &str, context_id: &str) -> String {
    let subject = subject.replace('%', "%25").replace(':', "%3A");
    format!("{subject}:{context_id}")
}

/// Inverse of [`encode_conversation`]: `(subject, context id)`.
pub(crate) fn decode_conversation(conversation: &str) -> Option<(String, String)> {
    let (subject, context) = conversation.split_once(':')?;
    Some((
        subject.replace("%3A", ":").replace("%25", "%"),
        context.to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use adam_runtime::Artifact as RunArtifact;
    use chrono::Utc;

    use super::*;

    fn view(status: RunStatus) -> RunView {
        RunView {
            id: adam_core::RunId::new(),
            agent: "a".into(),
            conversation_id: Some("s:c".into()),
            status,
            wake_at: None,
            waiting: status == RunStatus::Parked,
            output: None,
            error: None,
            attempt: 0,
            pending_inbox: 0,
            artifacts: Vec::new(),
            state: json!({}),
            version: 2,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn prompt() -> PromptFn {
        Arc::new(default_prompt)
    }

    #[test]
    fn states_follow_the_documented_table() {
        let mut v = view(RunStatus::Runnable);
        v.version = 1;
        assert_eq!(task_state(&v), TaskState::Submitted);
        v.version = 2;
        assert_eq!(task_state(&v), TaskState::Working);

        let mut v = view(RunStatus::Parked);
        assert_eq!(task_state(&v), TaskState::InputRequired);
        v.waiting = false;
        v.wake_at = Some(Utc::now());
        assert_eq!(task_state(&v), TaskState::Working);

        assert_eq!(task_state(&view(RunStatus::Done)), TaskState::Completed);

        let mut v = view(RunStatus::Failed);
        v.error = Some("boom".into());
        assert_eq!(task_state(&v), TaskState::Failed);
        v.error = Some("cancelled: by client".into());
        assert_eq!(task_state(&v), TaskState::Canceled);
    }

    #[test]
    fn status_messages_carry_the_question_output_and_error() {
        // The field is `pending_wait` now; a run parked by an older build stored `pending_question`.
        for state in [
            json!({"pending_wait": {"call_id": "c1", "tool": "ask", "question": "which branch?"}}),
            json!({"pending_question": {"question": "which branch?"}}),
        ] {
            let mut v = view(RunStatus::Parked);
            v.state = state;
            let s = status_of(&v, &prompt());
            assert_eq!(s.message.unwrap().text(), Some("which branch?"));
        }
        // A wait on a child run asks nobody anything.
        let mut v = view(RunStatus::Parked);
        v.state = json!({"pending_wait": {"call_id": "c1", "tool": "sub", "run": "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"}});
        assert_eq!(default_prompt(&v), None);
        // Nor does a wait on a task of another system.
        v.state = json!({"pending_wait": {"call_id": "c1", "tool": "billing", "task": "t-9"}});
        assert_eq!(default_prompt(&v), None);

        let mut v = view(RunStatus::Done);
        v.output = Some(json!({"text": "all done", "artifacts": []}));
        assert_eq!(
            status_of(&v, &prompt()).message.unwrap().text(),
            Some("all done")
        );

        let mut v = view(RunStatus::Failed);
        v.error = Some("cancelled: by client".into());
        assert_eq!(
            status_of(&v, &prompt()).message.unwrap().text(),
            Some("by client")
        );
        v.error = Some("checks red".into());
        assert_eq!(
            status_of(&v, &prompt()).message.unwrap().text(),
            Some("checks red")
        );

        // No question known: no message rather than a made-up one.
        assert!(
            status_of(&view(RunStatus::Parked), &prompt())
                .message
                .is_none()
        );
    }

    #[test]
    fn status_message_ids_follow_the_status_not_the_read() {
        let mut v = view(RunStatus::Done);
        v.output = Some(json!({"text": "all done"}));
        let id = |v: &RunView| status_of(v, &prompt()).message.unwrap().message_id;
        assert_eq!(id(&v), id(&v));
        // Time passing (a later snapshot of the same record) changes nothing.
        let mut later = v.clone();
        later.updated_at = Utc::now() + chrono::Duration::seconds(5);
        assert_eq!(id(&v), id(&later));

        let mut other_text = v.clone();
        other_text.output = Some(json!({"text": "all done!"}));
        assert_ne!(id(&v), id(&other_text));

        let mut failed = view(RunStatus::Failed);
        failed.id = v.id;
        failed.error = Some("all done".into());
        assert_ne!(id(&v), id(&failed));

        let mut other_task = v.clone();
        other_task.id = adam_core::RunId::new();
        assert_ne!(id(&v), id(&other_task));
    }

    fn ui() -> Value {
        json!([{"version": "v0.9.1", "createSurface": {"surfaceId": "s", "catalogId": "c"}}])
    }

    fn parked_with(ui: Option<Value>) -> RunView {
        let mut v = view(RunStatus::Parked);
        // One run: the ids of its statuses differ only by what the statuses say.
        v.id = adam_core::RunId(uuid::Uuid::nil());
        let mut wait = json!({"call_id": "c1", "tool": "ask", "question": "which branch?"});
        if let Some(ui) = ui {
            wait["ui"] = ui;
        }
        v.state = json!({ "pending_wait": wait });
        v
    }

    #[test]
    fn a_question_with_an_interface_carries_it_in_a_part_of_its_own() {
        let v = parked_with(Some(ui()));
        let status = status_of(&v, &prompt());
        let message = status.message.expect("a message");
        assert_eq!(message.parts.len(), 2);
        assert_eq!(message.text(), Some("which branch?"));
        assert_eq!(message.parts[1].content, a2a::PartContent::Data(ui()));
        assert_eq!(
            message.parts[1].media_type.as_deref(),
            Some(A2UI_MEDIA_TYPE)
        );
        assert_eq!(
            message.parts[1]
                .metadata
                .as_ref()
                .and_then(|m| m.get("mimeType")),
            Some(&json!(A2UI_MEDIA_TYPE))
        );
        // The same read gives the same id (the id follows the status, not the read).
        assert_eq!(
            message.message_id,
            status_of(&v, &prompt()).message.unwrap().message_id
        );
    }

    #[test]
    fn the_interface_makes_the_status_another_status_and_a_status_without_one_is_unchanged() {
        let with = status_of(&parked_with(Some(ui())), &prompt());
        let mut other = ui();
        other[0]["createSurface"]["surfaceId"] = json!("t");
        let other = status_of(&parked_with(Some(other)), &prompt());
        let without = status_of(&parked_with(None), &prompt());
        let id = |s: &TaskStatus| s.message.as_ref().unwrap().message_id.clone();
        assert_ne!(id(&with), id(&without));
        assert_ne!(id(&with), id(&other));
        assert_ne!(status_key(&with), status_key(&without));
        assert_ne!(status_key(&with), status_key(&other));
        assert_eq!(status_key(&with), status_key(&with));
        // Every status that has no interface is exactly what it was before interfaces existed.
        let v = parked_with(None);
        assert_eq!(
            id(&without),
            status_message_id(&v.id.to_string(), "input-required", "which branch?")
        );
        assert_eq!(without.message.as_ref().unwrap().parts.len(), 1);
        assert_eq!(status_key(&without).2, None);
    }

    #[test]
    fn only_a_non_empty_array_of_a_question_in_input_required_is_an_interface() {
        for not_ui in [json!([]), json!({"a": 1}), json!("x"), Value::Null] {
            let status = status_of(&parked_with(Some(not_ui.clone())), &prompt());
            assert_eq!(status.message.unwrap().parts.len(), 1, "{not_ui}");
        }
        // A run that is no longer waiting says nothing of the interface it once had.
        let mut done = parked_with(Some(ui()));
        done.status = RunStatus::Done;
        done.output = Some(json!({"text": "ok"}));
        assert_eq!(status_of(&done, &prompt()).message.unwrap().parts.len(), 1);
    }

    #[test]
    fn an_a2ui_artifact_is_marked_in_both_spellings() {
        let artifact = artifact_of(&RunArtifact {
            name: "ui".into(),
            mime_type: Some(A2UI_MEDIA_TYPE.into()),
            data: ui(),
        });
        let part = &artifact.parts[0];
        assert_eq!(part.media_type.as_deref(), Some(A2UI_MEDIA_TYPE));
        assert_eq!(
            part.metadata.as_ref().and_then(|m| m.get("mimeType")),
            Some(&json!(A2UI_MEDIA_TYPE))
        );
        assert_eq!(part.content, a2a::PartContent::Data(ui()));
        // Any other artifact carries no part metadata.
        let other = artifact_of(&RunArtifact {
            name: "d".into(),
            mime_type: Some("application/json".into()),
            data: json!({"k": 1}),
        });
        assert!(other.parts[0].metadata.is_none());
    }

    #[test]
    fn artifacts_map_to_text_or_data_parts_with_stable_ids() {
        let text = RunArtifact {
            name: "pull request".into(),
            mime_type: Some("text/uri-list".into()),
            data: json!("https://example.com/pr/1"),
        };
        let a = artifact_of(&text);
        assert_eq!(a.name.as_deref(), Some("pull request"));
        assert_eq!(a.parts[0].as_text(), Some("https://example.com/pr/1"));
        assert_eq!(a.parts[0].media_type.as_deref(), Some("text/uri-list"));
        assert_eq!(a.artifact_id, artifact_of(&text).artifact_id);
        assert!(a.artifact_id.starts_with("pull-request-"));

        let data = RunArtifact {
            name: "d".into(),
            mime_type: None,
            data: json!({"k": "v"}),
        };
        assert!(matches!(
            artifact_of(&data).parts[0].content,
            a2a::PartContent::Data(_)
        ));
        assert_ne!(artifact_id(&data), artifact_id(&text));
    }

    #[test]
    fn a_link_in_the_data_is_also_a_url_part_after_the_data_part() {
        let pr = RunArtifact {
            name: "pull_request".into(),
            mime_type: Some("application/json".into()),
            data: json!({"url": "https://github.com/octo/widgets/pull/7", "number": "7"}),
        };
        let a = artifact_of(&pr);
        assert_eq!(a.parts.len(), 2);
        assert_eq!(
            a.parts[0].content,
            a2a::PartContent::Data(
                json!({"url": "https://github.com/octo/widgets/pull/7", "number": "7"})
            ),
            "the data part is unchanged"
        );
        assert_eq!(a.parts[0].media_type.as_deref(), Some("application/json"));
        assert_eq!(
            a.parts[1].content,
            a2a::PartContent::Url("https://github.com/octo/widgets/pull/7".into())
        );
        assert_eq!(
            serde_json::to_value(&a.parts[1]).unwrap(),
            json!({"url": "https://github.com/octo/widgets/pull/7"}),
            "and it is a `url` part on the wire"
        );
        // The id is a function of name, media type and data only: the extra part changes nothing.
        assert_eq!(a.artifact_id, artifact_id(&pr));
    }

    #[test]
    fn only_an_absolute_http_url_in_an_object_is_a_link() {
        let parts = |data: Value| {
            artifact_of(&RunArtifact {
                name: "x".into(),
                mime_type: None,
                data,
            })
            .parts
            .len()
        };
        assert_eq!(parts(json!({"url": "http://example.com/a"})), 2);
        for not_a_link in [
            json!({"url": "ftp://example.com/a"}),
            json!({"url": "javascript:alert(1)"}),
            json!({"url": "/relative"}),
            json!({"url": "https://"}),
            json!({"url": "https://example.com/a b"}),
            json!({"url": 7}),
            json!({"link": "https://example.com/a"}),
            json!({"nested": {"url": "https://example.com/a"}}),
            json!("https://example.com/a"),
            json!(["https://example.com/a"]),
            Value::Null,
        ] {
            assert_eq!(parts(not_a_link.clone()), 1, "{not_a_link}");
        }
    }

    #[test]
    fn conversation_ids_round_trip_even_with_awkward_subjects() {
        for subject in ["token-0", "anonymous", "we:ird%3A", "%"] {
            let enc = encode_conversation(subject, "ctx:1:2");
            assert_eq!(
                decode_conversation(&enc),
                Some((subject.to_owned(), "ctx:1:2".to_owned()))
            );
        }
        assert_eq!(decode_conversation("no-separator"), None);
    }

    #[test]
    fn default_inbound_reads_text_and_rejects_empty_messages() {
        let m = Message::new(Role::User, vec![Part::text("add hello.txt")]);
        let inbound = default_inbound(&m).unwrap();
        assert_eq!(inbound.payload, json!({"text": "add hello.txt"}));
        assert_eq!(inbound.id, m.message_id);
        assert_eq!(inbound.kind, "message");

        let empty = Message::new(Role::User, vec![Part::text("  ")]);
        assert!(default_inbound(&empty).is_err());
        let raw = Message::new(Role::User, vec![Part::raw(vec![1])]);
        assert!(default_inbound(&raw).is_err());
    }

    mod prop {
        use proptest::prelude::*;

        use super::*;

        proptest! {
            /// Any subject and any context id (colons, percents and the
            /// escape sequences themselves included) come back unchanged, and
            /// the subject cannot smuggle a separator: two callers never map
            /// to the same conversation.
            #[test]
            fn prop_conversation_id_roundtrip(subject in any::<String>(), context in any::<String>()) {
                let enc = encode_conversation(&subject, &context);
                prop_assert_eq!(decode_conversation(&enc), Some((subject.clone(), context.clone())));
            }

            #[test]
            fn prop_conversation_ids_are_injective(
                s1 in "[%:a-zA-Z0-9]{0,6}", c1 in "[%:a-zA-Z0-9]{0,6}",
                s2 in "[%:a-zA-Z0-9]{0,6}", c2 in "[%:a-zA-Z0-9]{0,6}",
            ) {
                prop_assume!((&s1, &c1) != (&s2, &c2));
                prop_assert_ne!(encode_conversation(&s1, &c1), encode_conversation(&s2, &c2));
            }

            /// Decoding arbitrary text never panics.
            #[test]
            fn prop_decode_conversation_never_panics(text in any::<String>()) {
                let _ = decode_conversation(&text);
            }
        }
    }
}
