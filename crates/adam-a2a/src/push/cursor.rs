//! How far a webhook has been told about a task: the cursor a config's delivery keeps in the
//! store, and the events it derives from the task.
//!
//! The durable truth of a task is the task itself (its status and its artifacts, as
//! [`TaskBackend::get`](crate::TaskBackend::get) reads them). There is no durable log of every
//! status the task passed through, so the cursor does not count events: it remembers **what the
//! webhook has been told**, and the next event is whatever the task says that the webhook has not
//! heard yet, artifacts first and then the status, the order a stream gives them in.
//!
//! * **Status.** Identified by its state and a digest of its message, so a status that changes
//!   only in its timestamp is not news.
//! * **Artifacts.** Identified by `artifactId`. An artifact the webhook has been told about is
//!   not sent again, unless it was replaced since (an artifact keeps its id when the run changes it,
//!   as a plan does): the cursor then also holds a digest of the content the webhook was told, and
//!   a different one is news. A cursor written before digests existed has none and treats a known
//!   id as told.
//! * **Pending.** An event that was sent and not acknowledged (the webhook answered 500, or did
//!   not answer) is kept whole, so the retry sends *that* event even if the task has moved on
//!   since: a webhook that was down for a while still hears each state it was meant to hear, in
//!   order. What no deliverer saw between two reads (a state the task was in and left between two
//!   polls, or while every replica was down) is never delivered: the next event is the state the
//!   task is in. That is why a notification is a hint and `GetTask` is the truth.

use a2a::{
    StreamResponse, Task, TaskArtifactUpdateEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What a webhook has been told about a task. Stored as JSON, in the store's `cursor` column.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PushCursor {
    /// The key of the last status delivered (or the status the task had when the config was
    /// created), or `None` before there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The artifacts delivered (or present when the config was created), by id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
    /// A digest of the content of each artifact delivered, by id: what tells an artifact that was
    /// replaced under its id from one the webhook has.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub revisions: BTreeMap<String, String>,
    /// The event being delivered: sent, not acknowledged, to be sent again as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<StreamResponse>,
    /// When the pending event first failed: what the give-up bound counts from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing_since: Option<DateTime<Utc>>,
}

/// The key that tells two statuses apart for "did anything change": the state and a digest of the
/// message (which also covers a form or any data part in it), never the timestamp.
pub fn status_key(status: &TaskStatus) -> String {
    let digest = status
        .message
        .as_ref()
        .and_then(|m| serde_json::to_vec(&m.parts).ok())
        .map(|bytes| {
            let hash = Sha256::digest(&bytes);
            hash.iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        })
        .unwrap_or_default();
    format!("{}|{digest}", state_name(&status.state))
}

fn state_name(state: &TaskState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The member of an artifact's `metadata` that says its id is **its own and kept**: the run replaces
/// the artifact under that id when it changes, so the same id can carry different content later. An
/// artifact without it has an id made from its content (a different content is a different id),
/// and a delivery cursor keeps nothing more about it than the id.
pub const REPLACEABLE_ARTIFACT: &str = "vymalo.replaceable";

/// Whether `artifact` is one whose id can carry different content later ([`REPLACEABLE_ARTIFACT`]).
fn replaceable(artifact: &a2a::Artifact) -> bool {
    artifact
        .metadata
        .as_ref()
        .is_some_and(|m| m.contains_key(REPLACEABLE_ARTIFACT))
}

/// `value` with the members of every object in sorted order, whether or not this build of
/// `serde_json` keeps insertion order.
fn sorted(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// The key that tells two contents of one artifact apart: a digest of its name and parts as
/// canonical JSON (members of every object in sorted order), so the same artifact read back from the
/// store, or written by another build, digests the same.
fn revision(artifact: &a2a::Artifact) -> String {
    let canonical = serde_json::to_value((&artifact.name, &artifact.parts))
        .map(sorted)
        .and_then(|v| serde_json::to_vec(&v))
        .unwrap_or_default();
    Sha256::digest(&canonical)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl PushCursor {
    /// The cursor of a config created while the task is `task`: what the task already says is
    /// not news to the webhook, only what changes after.
    pub fn baseline(task: &Task) -> Self {
        Self {
            status: Some(status_key(&task.status)),
            artifacts: task
                .artifacts
                .iter()
                .flatten()
                .map(|a| a.artifact_id.clone())
                .collect(),
            revisions: task
                .artifacts
                .iter()
                .flatten()
                .filter(|a| replaceable(a))
                .map(|a| (a.artifact_id.clone(), revision(a)))
                .collect(),
            pending: None,
            failing_since: None,
        }
    }

    /// The next event to deliver for `task`, or `None` when the webhook has heard everything.
    ///
    /// The pending event first; otherwise the first artifact the webhook has not been told of; then
    /// the status, if it is not the one the webhook was told.
    pub fn next_event(&self, task: &Task) -> Option<StreamResponse> {
        if let Some(pending) = &self.pending {
            return Some(pending.clone());
        }
        if let Some(artifact) = task.artifacts.iter().flatten().find(|a| {
            !self.artifacts.contains(&a.artifact_id)
                || self
                    .revisions
                    .get(&a.artifact_id)
                    .is_some_and(|told| *told != revision(a))
        }) {
            return Some(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: task.id.clone(),
                context_id: task.context_id.clone(),
                artifact: artifact.clone(),
                append: None,
                last_chunk: Some(true),
                metadata: None,
            }));
        }
        if self.status.as_deref() != Some(status_key(&task.status).as_str()) {
            return Some(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task.id.clone(),
                context_id: task.context_id.clone(),
                status: task.status.clone(),
                metadata: None,
            }));
        }
        None
    }

    /// The webhook acknowledged `event`: it is told now, and nothing is pending.
    pub fn acknowledge(&mut self, event: &StreamResponse) {
        match event {
            StreamResponse::StatusUpdate(update) => self.status = Some(status_key(&update.status)),
            StreamResponse::ArtifactUpdate(update) => {
                let id = &update.artifact.artifact_id;
                if !self.artifacts.contains(id) {
                    self.artifacts.push(id.clone());
                }
                if replaceable(&update.artifact) {
                    self.revisions
                        .insert(id.clone(), revision(&update.artifact));
                }
            }
            StreamResponse::Task(task) => *self = Self::baseline(task),
            StreamResponse::Message(_) => {}
        }
        self.pending = None;
        self.failing_since = None;
    }

    /// `event` failed to be delivered at `now`: keep it to be sent again as it is.
    pub fn fail(&mut self, event: StreamResponse, now: DateTime<Utc>) {
        self.pending = Some(event);
        self.failing_since.get_or_insert(now);
    }
}

/// Whether delivering is over for `task` once `cursor` has heard everything: the task is
/// terminal, so nothing more will happen to it.
pub fn is_final(task: &Task) -> bool {
    task.status.state.is_terminal()
}

#[cfg(test)]
mod tests {
    use a2a::{Artifact, Message, Part, Role};

    use super::*;

    fn task(state: TaskState, text: Option<&str>, artifacts: &[&str]) -> Task {
        Task {
            id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state,
                message: text.map(|t| Message::new(Role::Agent, vec![Part::text(t)])),
                timestamp: Some(Utc::now()),
            },
            artifacts: (!artifacts.is_empty()).then(|| {
                artifacts
                    .iter()
                    .map(|id| Artifact {
                        artifact_id: (*id).to_owned(),
                        name: None,
                        description: None,
                        parts: vec![Part::text(*id)],
                        metadata: None,
                        extensions: None,
                    })
                    .collect()
            }),
            history: None,
            metadata: None,
        }
    }

    fn kind(event: &StreamResponse) -> String {
        match event {
            StreamResponse::StatusUpdate(u) => state_name(&u.status.state),
            StreamResponse::ArtifactUpdate(u) => format!("artifact:{}", u.artifact.artifact_id),
            StreamResponse::Task(_) => "task".into(),
            StreamResponse::Message(_) => "message".into(),
        }
    }

    #[test]
    fn a_new_config_hears_only_what_changes_after_it() {
        let start = task(TaskState::Submitted, None, &[]);
        let cursor = PushCursor::baseline(&start);
        assert!(cursor.next_event(&start).is_none(), "nothing changed");
        let mut later = task(TaskState::Working, None, &[]);
        later.status.timestamp = None;
        assert_eq!(
            kind(&cursor.next_event(&later).unwrap()),
            "TASK_STATE_WORKING"
        );
    }

    #[test]
    fn a_timestamp_alone_is_not_news() {
        let a = task(TaskState::Working, Some("on it"), &[]);
        let mut b = a.clone();
        b.status.timestamp = Some(Utc::now() + chrono::Duration::seconds(5));
        assert!(PushCursor::baseline(&a).next_event(&b).is_none());
        let c = task(TaskState::Working, Some("still on it"), &[]);
        assert!(
            PushCursor::baseline(&a).next_event(&c).is_some(),
            "another message is"
        );
    }

    #[test]
    fn artifacts_come_first_in_order_then_the_status_and_each_is_told_once() {
        let mut cursor = PushCursor::baseline(&task(TaskState::Submitted, None, &[]));
        let now = task(TaskState::Completed, None, &["a1", "a2"]);
        let mut told = Vec::new();
        while let Some(event) = cursor.next_event(&now) {
            told.push(kind(&event));
            cursor.acknowledge(&event);
        }
        assert_eq!(told, ["artifact:a1", "artifact:a2", "TASK_STATE_COMPLETED"]);
        assert!(cursor.next_event(&now).is_none());
        assert!(is_final(&now));
    }

    #[test]
    fn a_pending_event_is_sent_again_as_it_was_even_if_the_task_moved_on() {
        let mut cursor = PushCursor::baseline(&task(TaskState::Submitted, None, &[]));
        let working = task(TaskState::Working, None, &[]);
        let event = cursor.next_event(&working).unwrap();
        let t0 = Utc::now();
        cursor.fail(event.clone(), t0);
        cursor.fail(event, t0 + chrono::Duration::seconds(9));
        assert_eq!(
            cursor.failing_since,
            Some(t0),
            "the bound counts from the first failure"
        );
        // The task completed while the webhook was failing: it still hears `working` first.
        let done = task(TaskState::Completed, None, &["a"]);
        let first = cursor.next_event(&done).unwrap();
        assert_eq!(kind(&first), "TASK_STATE_WORKING");
        cursor.acknowledge(&first);
        assert!(cursor.pending.is_none() && cursor.failing_since.is_none());
        assert_eq!(kind(&cursor.next_event(&done).unwrap()), "artifact:a");
    }

    #[test]
    fn the_cursor_survives_the_store() {
        let mut cursor = PushCursor::baseline(&task(TaskState::Working, Some("x"), &["a"]));
        cursor.fail(
            cursor
                .next_event(&task(TaskState::Completed, None, &["a"]))
                .unwrap(),
            Utc::now(),
        );
        let back: PushCursor =
            serde_json::from_value(serde_json::to_value(&cursor).unwrap()).unwrap();
        assert_eq!(back, cursor);
        let empty: PushCursor = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(empty, PushCursor::default());
    }
}

#[cfg(test)]
mod revision_tests {
    //! An artifact whose id is its own can change under it: the cursor tells it again when its
    //! content differs from what the webhook was last told, and stores nothing for any other.
    use std::collections::HashMap;

    use a2a::{Artifact, Message, Part, Role};

    use super::*;

    fn artifact(id: &str, text: &str, replaceable: bool) -> Artifact {
        Artifact {
            artifact_id: id.to_owned(),
            name: Some("plan".into()),
            description: None,
            parts: vec![Part::text(text)],
            metadata: replaceable.then(|| {
                HashMap::from([(REPLACEABLE_ARTIFACT.to_owned(), serde_json::json!(true))])
            }),
            extensions: None,
        }
    }

    fn task_of(artifacts: Vec<Artifact>) -> Task {
        Task {
            id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some(Message::new(Role::Agent, vec![Part::text("w")])),
                timestamp: None,
            },
            artifacts: Some(artifacts),
            history: None,
            metadata: None,
        }
    }

    /// Deliver what the cursor says next until it says nothing; the texts of the artifacts told.
    fn drain(cursor: &mut PushCursor, task: &Task) -> Vec<String> {
        let mut told = Vec::new();
        while let Some(event) = cursor.next_event(task) {
            if let StreamResponse::ArtifactUpdate(update) = &event {
                told.push(format!("{:?}", update.artifact.parts[0].content));
            }
            cursor.acknowledge(&event);
        }
        told
    }

    #[test]
    fn a_replaced_artifact_is_told_again_and_a_change_back_is_told_too() {
        let mut cursor = PushCursor::default();
        let a = task_of(vec![artifact("plan-x", "A", true)]);
        let b = task_of(vec![artifact("plan-x", "B", true)]);
        assert_eq!(drain(&mut cursor, &a).len(), 1, "A is new");
        assert!(drain(&mut cursor, &a).is_empty(), "A is not told twice");
        assert_eq!(drain(&mut cursor, &b).len(), 1, "B replaced A");
        assert_eq!(
            drain(&mut cursor, &a).len(),
            1,
            "A again is a change from B"
        );
        assert!(drain(&mut cursor, &a).is_empty());
        assert_eq!(cursor.artifacts, ["plan-x"], "one entry per id");
        assert_eq!(cursor.revisions.len(), 1);
    }

    #[test]
    fn an_artifact_with_no_id_of_its_own_stores_no_revision() {
        let t = task_of(vec![
            artifact("report-1", "r", false),
            artifact("p", "A", true),
        ]);
        let baseline = PushCursor::baseline(&t);
        assert_eq!(baseline.artifacts.len(), 2);
        assert_eq!(baseline.revisions.keys().collect::<Vec<_>>(), ["p"]);
        let mut cursor = PushCursor::default();
        drain(&mut cursor, &t);
        assert_eq!(cursor.revisions.keys().collect::<Vec<_>>(), ["p"]);
    }

    #[test]
    fn a_cursor_stored_before_revisions_treats_a_known_id_as_told_and_hears_the_next_change() {
        // What an older worker wrote: the id, and no revisions.
        let stored = r#"{"artifacts": ["plan-x"]}"#;
        let mut cursor: PushCursor = serde_json::from_str(stored).unwrap();
        let a = task_of(vec![artifact("plan-x", "A", true)]);
        let b = task_of(vec![artifact("plan-x", "B", true)]);
        assert!(drain(&mut cursor, &a).is_empty(), "a known id is told");
        assert_eq!(
            drain(&mut cursor, &b).len(),
            0,
            "and with nothing recorded, so is B"
        );
        // Once a delivery records a revision, a later change is told.
        cursor.artifacts.clear();
        assert_eq!(drain(&mut cursor, &b).len(), 1);
        assert_eq!(drain(&mut cursor, &a).len(), 1);
    }

    #[test]
    fn the_digest_does_not_depend_on_the_order_of_a_data_parts_members() {
        let with = |data: serde_json::Value| Artifact {
            parts: vec![Part::data(data)],
            ..artifact("p", "", true)
        };
        assert_eq!(
            revision(&with(serde_json::json!({"a": 1, "b": {"c": 2, "d": 3}}))),
            revision(&with(serde_json::json!({"b": {"d": 3, "c": 2}, "a": 1})))
        );
    }
}
