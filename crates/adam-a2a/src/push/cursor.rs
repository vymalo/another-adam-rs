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
    /// A cheap fingerprint of the content each revision was taken of, by id: what lets a poll tell
    /// an artifact that is what it was from one that changed, without canonicalising and digesting
    /// it again. A hint, never the truth: when it differs, `revision` is computed and decides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fingerprints: BTreeMap<String, String>,
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
    #[cfg(test)]
    work::hashed();
    let canonical = serde_json::to_value((&artifact.name, &artifact.parts))
        .map(sorted)
        .and_then(|v| serde_json::to_vec(&v))
        .unwrap_or_default();
    hex8(&Sha256::digest(&canonical))
}

/// The first eight bytes of `digest`, as hex: the width of a [`revision`] and a [`fingerprint`].
fn hex8(digest: &[u8]) -> String {
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// A cheap fingerprint of an artifact's content: a digest of its name and parts as they are held
/// (a file's bytes themselves, not their base64), so a poll can tell an artifact that is what it
/// was from one that changed without canonicalising it to JSON and digesting that. Every member
/// [`revision`] covers is covered here too (a new member of [`a2a::Part`] must be added here); a
/// fingerprint is a hint, and a hint that differs only makes the poll compute the digest.
fn fingerprint(artifact: &a2a::Artifact) -> String {
    use a2a::PartContent;

    /// Writes a `Value`'s JSON into the hasher without building a string of it.
    struct Sink<'a>(&'a mut Sha256);

    impl std::io::Write for Sink<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A string with its length, so that `("ab", "c")` and `("a", "bc")` differ.
    fn text(hasher: &mut Sha256, text: &str) {
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }

    fn optional(hasher: &mut Sha256, value: Option<&str>) {
        match value {
            Some(value) => {
                hasher.update([1]);
                text(hasher, value);
            }
            None => hasher.update([0]),
        }
    }

    let mut hasher = Sha256::new();
    optional(&mut hasher, artifact.name.as_deref());
    hasher.update((artifact.parts.len() as u64).to_le_bytes());
    for part in &artifact.parts {
        match &part.content {
            PartContent::Text(content) => {
                hasher.update([0]);
                text(&mut hasher, content);
            }
            PartContent::Raw(bytes) => {
                hasher.update([1]);
                hasher.update((bytes.len() as u64).to_le_bytes());
                hasher.update(bytes);
            }
            PartContent::Url(url) => {
                hasher.update([2]);
                text(&mut hasher, url);
            }
            PartContent::Data(value) => {
                hasher.update([3]);
                let _ = serde_json::to_writer(&mut Sink(&mut hasher), value);
            }
        }
        optional(&mut hasher, part.filename.as_deref());
        optional(&mut hasher, part.media_type.as_deref());
        match &part.metadata {
            Some(metadata) if !metadata.is_empty() => {
                hasher.update([1]);
                // A map of the SDK is a `HashMap`: the order it iterates in must not decide.
                let mut keys: Vec<&str> = metadata.keys().map(String::as_str).collect();
                keys.sort_unstable();
                hasher.update((keys.len() as u64).to_le_bytes());
                for key in keys {
                    text(&mut hasher, key);
                    let _ = serde_json::to_writer(&mut Sink(&mut hasher), &metadata[key]);
                }
            }
            _ => hasher.update([0]),
        }
    }
    hex8(&hasher.finalize())
}

impl PushCursor {
    /// The cursor of a config created while the task is `task`: what the task already says is
    /// not news to the webhook, only what changes after.
    pub fn baseline(task: &Task) -> Self {
        let mut revisions = BTreeMap::new();
        let mut fingerprints = BTreeMap::new();
        for artifact in task
            .artifacts
            .iter()
            .flatten()
            .filter(|artifact| replaceable(artifact))
        {
            revisions.insert(artifact.artifact_id.clone(), revision(artifact));
            fingerprints.insert(artifact.artifact_id.clone(), fingerprint(artifact));
        }
        Self {
            status: Some(status_key(&task.status)),
            artifacts: task
                .artifacts
                .iter()
                .flatten()
                .map(|a| a.artifact_id.clone())
                .collect(),
            revisions,
            fingerprints,
            pending: None,
            failing_since: None,
        }
    }

    /// The next event to deliver for `task`, or `None` when the webhook has heard everything.
    ///
    /// The pending event first; otherwise the first artifact the webhook has not been told of; then
    /// the status, if it is not the one the webhook was told.
    pub fn next_event(&mut self, task: &Task) -> Option<StreamResponse> {
        if let Some(pending) = &self.pending {
            return Some(pending.clone());
        }
        let mut news = None;
        for artifact in task.artifacts.iter().flatten() {
            if self.news(artifact) {
                news = Some(artifact);
                break;
            }
        }
        if let Some(artifact) = news {
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

    /// Whether `artifact` is news: one the webhook was never told of, or one whose content differs
    /// from the digest it was told under its id. A content that matches its fingerprint is what the
    /// digest was taken of; only a fingerprint that differs recomputes the digest (and a cursor
    /// stored before fingerprints existed is given one here, once).
    fn news(&mut self, artifact: &a2a::Artifact) -> bool {
        let id = &artifact.artifact_id;
        if !self.artifacts.contains(id) {
            return true;
        }
        let Some(told) = self.revisions.get(id).cloned() else {
            // An id the webhook was told of, with nothing recorded: told (a cursor from before
            // digests).
            return false;
        };
        let now = fingerprint(artifact);
        if self.fingerprints.get(id).is_some_and(|hint| *hint == now) {
            return false;
        }
        if revision(artifact) == told {
            self.fingerprints.insert(id.clone(), now);
            return false;
        }
        true
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
                    self.fingerprints
                        .insert(id.clone(), fingerprint(&update.artifact));
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

/// What digesting an artifact costs, for the test that proves a poll of an artifact that did not
/// change does not digest it again (and that a backfilled cursor stays backfilled through the
/// store, which `deliverer`'s test checks). Thread-local: the tests run in parallel, each on its own
/// thread.
#[cfg(test)]
pub(crate) mod work {
    use std::cell::Cell;

    thread_local! {
        static DIGESTS: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn hashed() {
        DIGESTS.with(|n| n.set(n.get() + 1));
    }

    /// Canonical digests taken on this thread so far.
    pub(crate) fn digests() -> u64 {
        DIGESTS.with(Cell::get)
    }
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
        let mut cursor = PushCursor::baseline(&start);
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
        let event = cursor
            .next_event(&task(TaskState::Completed, None, &["a"]))
            .unwrap();
        cursor.fail(event, Utc::now());
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

    /// The digest is what a cursor in a store already holds: changing its shape would re-deliver
    /// every replaceable artifact once, on every replica.
    #[test]
    fn the_revision_digest_is_what_it_always_was() {
        assert_eq!(revision(&artifact("plan", "A", true)), "e0f52366613937e0");
    }

    /// A poll of an artifact that is what it was compares a fingerprint, not the canonical JSON:
    /// ten polls of a 1 MiB artifact digest nothing, and a change of the same length is still news.
    #[test]
    fn a_poll_of_an_unchanged_replaceable_artifact_does_not_digest_it_again() {
        let big = |first: &str| {
            let mut text = String::with_capacity(1024 * 1024);
            text.push_str(first);
            text.push_str(&"x".repeat(1024 * 1024 - 1));
            artifact("plan", &text, true)
        };
        let a = task_of(vec![big("A")]);
        let mut cursor = PushCursor::baseline(&a);
        let before = work::digests();
        for _ in 0..10 {
            assert!(cursor.next_event(&a).is_none(), "nothing changed");
        }
        assert_eq!(
            work::digests(),
            before,
            "an unchanged artifact is not digested"
        );
        assert_eq!(cursor.fingerprints.len(), 1, "the baseline recorded one");

        // The same length, other content: the fingerprint differs, and the digest decides.
        let b = task_of(vec![big("B")]);
        let event = cursor.next_event(&b).expect("B replaced A");
        assert!(matches!(event, StreamResponse::ArtifactUpdate(_)));
        assert_eq!(
            work::digests(),
            before + 1,
            "a changed artifact is digested"
        );
        cursor.acknowledge(&event);
        assert!(cursor.next_event(&b).is_none());
        let after = work::digests();
        assert!(cursor.next_event(&b).is_none());
        assert_eq!(work::digests(), after, "and not again on the next poll");
    }

    /// A cursor written before fingerprints existed has the digest but not the fingerprint: the
    /// first poll digests once, finds the content it was told, and records the fingerprint.
    #[test]
    fn a_cursor_stored_before_fingerprints_records_one_on_its_first_poll() {
        let a = task_of(vec![artifact("plan-x", "A", true)]);
        let stored = serde_json::json!({
            "status": status_key(&a.status),
            "artifacts": ["plan-x"],
            "revisions": {"plan-x": revision(&artifact("plan-x", "A", true))},
        });
        let mut cursor: PushCursor = serde_json::from_value(stored).unwrap();
        assert!(cursor.fingerprints.is_empty());
        let before = work::digests();
        assert!(cursor.next_event(&a).is_none());
        assert_eq!(work::digests(), before + 1, "the digest decides once");
        assert_eq!(
            cursor.fingerprints.len(),
            1,
            "and the fingerprint is recorded"
        );
        assert!(cursor.next_event(&a).is_none());
        assert_eq!(work::digests(), before + 1, "then it is cheap");
    }
}
