//! The v5 head: a run's state without its transcript, beside the transcript's body generation.
//!
//! The expand release (S2a) writes the head next to the full v4 `state` and reads neither; the
//! read path (S2b) serves the head and joins the rows it names. [`Stored`] is the enum that makes
//! the two documents interchangeable: every commit path re-encodes the variant it read, so a
//! delivery or a cancel never converts a run's format by accident.
//!
//! Layout: [`Head`] is the v4 envelope with `v: 5`, `agent` reduced to the agent's head, file
//! artifacts by reference (`digest` and `size`, never the bytes), and a `body` naming the
//! generation: `{"generation": "<uuid>", "len": <rows>}`. The body rows are
//! `(run, generation, idx)` in the store.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use adam_core::{BlobId, RunId, StoreError};

use crate::agent::Inbound;
use crate::blob::blob_id;
use crate::envelope::{Envelope, VERSION as FULL_VERSION};
use crate::events::Artifact;
use crate::runtime::RuntimeError;

/// The head's layout version. A run's `state` is `1` (the v4 envelope) or this.
pub(crate) const VERSION: u32 = 5;

/// The body generation a head names: the rows are `(run, generation, idx)` in the store, and `len`
/// is the integrity check a reader makes against `body_list(run, generation).len()`.
///
/// The runtime leaves `generation` `null`: only the store sees the current head, so it resolves the
/// generation an append belongs to (the current one, or a new one for a first split or a
/// compaction) and patches this member before it writes the head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BodyRef {
    /// The generation the rows belong to; `null` until the store resolves it.
    pub generation: Option<Uuid>,
    /// How many rows the generation has.
    pub len: u32,
}

/// A file artifact in a head: its name and content address, not its bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HeadFile {
    /// The name the file is saved under.
    pub filename: String,
    /// The SHA-256 of its bytes, as 64 lower-case hex.
    pub digest: BlobId,
    /// Its size in bytes.
    pub size: u64,
}

/// An artifact in a head: a JSON artifact carries its `data`, a file artifact carries a
/// [`HeadFile`] reference instead of the bytes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct HeadArtifact {
    /// The artifact's identity within the run, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Its name.
    pub name: String,
    /// Its media type, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The content of a JSON artifact; `null` for a file.
    pub data: Value,
    /// The file, for a file artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<HeadFile>,
}

/// The v5 head: everything a reader of a run needs and nothing that grows with the transcript.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Head {
    /// Layout version ([`VERSION`]).
    pub v: u32,
    /// The agent's head: its state without the messages.
    pub agent: Value,
    /// Delivered, not yet consumed messages, oldest first.
    #[serde(default)]
    pub inbox: Vec<Inbound>,
    /// Next journal seq at the last commit.
    #[serde(default)]
    pub seq: u64,
    /// Consecutive failed tries of the current transition.
    #[serde(default)]
    pub attempt: u32,
    /// Bumped by every commit a worker makes.
    #[serde(default)]
    pub rev: u64,
    /// Result of a `Done` run (`null` otherwise).
    #[serde(default)]
    pub output: Value,
    /// Reason of a `Failed` run.
    #[serde(default)]
    pub error: Option<String>,
    /// Artifacts emitted so far, files by reference.
    #[serde(default)]
    pub artifacts: Vec<HeadArtifact>,
    /// When the run must have finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    /// The limit the deadline was set from, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_secs: Option<u64>,
    /// What is left of the time limit while the run waits for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
    /// When the agent itself asked to be woken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_wake: Option<DateTime<Utc>>,
    /// The runtime set the wake of this parked run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hb: bool,
    /// The transcript set this head names.
    pub body: BodyRef,
}

impl Head {
    /// The head of `env` whose agent state is `agent` (the split's head), whose artifacts are
    /// `artifacts` and whose transcript has `len` rows. The generation is left for the store to
    /// resolve.
    ///
    /// `artifacts` is the list the commit's upload pass saw, not `env.artifacts`: a CAS retry can
    /// merge deliveries into the envelope, and the head's file references must stay paired with
    /// the digests that pass stored. `files` is positional, one entry per file artifact.
    pub(crate) fn of(
        env: &Envelope,
        agent: Value,
        len: u32,
        artifacts: &[Artifact],
        files: &[(BlobId, u64)],
    ) -> Self {
        Self {
            v: VERSION,
            agent,
            inbox: env.inbox.clone(),
            seq: env.seq,
            attempt: env.attempt,
            rev: env.rev,
            output: env.output.clone(),
            error: env.error.clone(),
            artifacts: {
                let mut files = files.iter();
                artifacts
                    .iter()
                    .map(|artifact| head_artifact(artifact, &mut files))
                    .collect()
            },
            deadline: env.deadline,
            limit_secs: env.limit_secs,
            remaining_secs: env.remaining_secs,
            agent_wake: env.agent_wake,
            hb: env.hb,
            body: BodyRef {
                generation: None,
                len,
            },
        }
    }

    /// Decode `state` as a v5 head; [`RuntimeError::Corrupt`] for another version or a document
    /// that does not decode.
    pub(crate) fn decode(run: RunId, state: &Value) -> Result<Self, RuntimeError> {
        let corrupt = |reason: String| RuntimeError::Corrupt {
            run,
            reason,
            source: None,
        };
        match state.get("v").and_then(Value::as_u64) {
            Some(v) if v == u64::from(VERSION) => {}
            Some(v) => return Err(corrupt(format!("unsupported head version {v}"))),
            None => return Err(corrupt("missing head version".into())),
        }
        Self::deserialize(state).map_err(|e| RuntimeError::Corrupt {
            run,
            reason: "the head does not decode".into(),
            source: Some(Box::new(e)),
        })
    }

    /// Encode the head as the JSON a store keeps.
    pub(crate) fn encode(&self) -> Result<Value, StoreError> {
        serde_json::to_value(self)
            .map_err(|e| StoreError::InvalidInput(format!("the head does not serialize: {e}")))
    }
}

/// An artifact as a head keeps it: the same shape, with a file's bytes replaced by its address.
///
/// `files` is the commit's upload pass, in artifact order; it is advanced once per file artifact.
/// A file the pass did not see (a head built without an upload, as in the unit tests) is hashed
/// here.
fn head_artifact(
    artifact: &Artifact,
    files: &mut std::slice::Iter<'_, (BlobId, u64)>,
) -> HeadArtifact {
    HeadArtifact {
        id: artifact.id.clone(),
        name: artifact.name.clone(),
        mime_type: artifact.mime_type.clone(),
        data: artifact.data.clone(),
        file: artifact.file.as_ref().map(|file| {
            let (digest, size) = files.next().cloned().unwrap_or_else(|| {
                let digest = blob_id(&file.bytes);
                (digest, u64::try_from(file.bytes.len()).unwrap_or(u64::MAX))
            });
            HeadFile {
                filename: file.filename.clone(),
                digest,
                size,
            }
        }),
    }
}

/// The two documents a run's `state` can be in the expand release: the v4 envelope, or the v5
/// head. Every commit path re-encodes the variant it read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Stored {
    /// The whole run, the v4 format (`v: 1`).
    Full(Envelope),
    /// The head and the body generation it names (`v: 5`).
    Head(Head),
}

// S2a writes and verifies the head but reads neither; the read path (S2b) is what calls these.
// The allow keeps `Stored` live, and through `Stored::decode` it keeps `Head::decode` live too:
// without it a non-test build reports both, and S2b is the caller.
#[allow(
    dead_code,
    reason = "S2a writes and verifies the head; S2b's read path decodes and serves it"
)]
impl Stored {
    /// Decode `state` as the variant its `v` names.
    pub(crate) fn decode(run: RunId, state: &Value) -> Result<Self, RuntimeError> {
        match state.get("v").and_then(Value::as_u64) {
            Some(v) if v == u64::from(FULL_VERSION) => {
                Ok(Self::Full(Envelope::decode(run, state)?))
            }
            Some(v) if v == u64::from(VERSION) => Ok(Self::Head(Head::decode(run, state)?)),
            Some(v) => Err(RuntimeError::Corrupt {
                run,
                reason: format!("unsupported envelope version {v}"),
                source: None,
            }),
            None => Err(RuntimeError::Corrupt {
                run,
                reason: "missing envelope version".into(),
                source: None,
            }),
        }
    }

    /// Encode the same variant that was decoded.
    pub(crate) fn encode(&self) -> Result<Value, StoreError> {
        match self {
            Self::Full(envelope) => envelope.encode(),
            Self::Head(head) => head.encode(),
        }
    }

    /// The inbox, in the variant's own document.
    pub(crate) fn inbox_mut(&mut self) -> &mut Vec<Inbound> {
        match self {
            Self::Full(envelope) => &mut envelope.inbox,
            Self::Head(head) => &mut head.inbox,
        }
    }

    /// The failure reason, in the variant's own document.
    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Full(envelope) => envelope.error.as_deref(),
            Self::Head(head) => head.error.as_deref(),
        }
    }

    /// The agent's state as the document carries it: the whole state for `Full`, the agent's head
    /// for `Head`.
    pub(crate) fn agent(&self) -> &Value {
        match self {
            Self::Full(envelope) => &envelope.agent,
            Self::Head(head) => &head.agent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file_artifact() -> Artifact {
        Artifact::file("chart", "image/svg+xml", "chart.svg", b"<svg/>".to_vec())
            .expect("a file artifact")
    }

    #[test]
    fn a_head_of_an_envelope_keeps_the_agent_head_and_names_the_body() {
        let mut env = Envelope::new(json!({"messages": [1, 2], "turns": 3}));
        env.seq = 7;
        env.attempt = 1;
        env.rev = 4;
        env.output = json!({"answer": 42});
        env.error = Some("not yet".into());
        env.artifacts.push(file_artifact());
        env.artifacts
            .push(Artifact::new("plan", None, json!({"steps": [1]})));
        env.hb = true;
        let head = Head::of(&env, json!({"turns": 3}), 2, &env.artifacts, &[]);
        assert_eq!(head.v, 5);
        assert_eq!(head.agent, json!({"turns": 3}));
        assert_eq!((head.seq, head.attempt, head.rev), (7, 1, 4));
        assert_eq!(head.output, env.output);
        assert_eq!(head.error, env.error);
        assert_eq!(
            head.body,
            BodyRef {
                generation: None,
                len: 2
            }
        );
        assert_eq!(head.artifacts.len(), 2);
        // A file is by reference: its digest is the content address and its size the byte length,
        // never the bytes.
        let file = head.artifacts[0].file.as_ref().expect("a file");
        assert_eq!(file.filename, "chart.svg");
        assert_eq!(file.digest, blob_id(b"<svg/>"));
        assert_eq!(file.size, 6);
        assert_eq!(head.artifacts[0].data, Value::Null);
        // A JSON artifact keeps its data.
        assert_eq!(head.artifacts[1].file, None);
        assert_eq!(head.artifacts[1].data, json!({"steps": [1]}));

        // And it round-trips through the JSON a store keeps.
        let encoded = head.encode().expect("encodes");
        assert_eq!(Head::decode(RunId::new(), &encoded).expect("decodes"), head);
    }

    /// The head's file references are paired **positionally** with the upload pass, and only a
    /// file artifact consumes an entry: a JSON artifact before a file must not shift its digest.
    /// The list is the one the pass saw, not whatever a retried envelope carries.
    #[test]
    fn the_file_list_is_paired_with_the_file_artifacts_of_the_upload_pass() {
        let mut env = Envelope::new(json!({}));
        env.artifacts
            .push(Artifact::new("plan", None, json!({"steps": [1]})));
        env.artifacts
            .push(Artifact::file("a", "image/png", "a.png", b"aaa".to_vec()).expect("a file"));
        env.artifacts
            .push(Artifact::file("b", "image/png", "b.png", b"bbbb".to_vec()).expect("a file"));
        let first = BlobId::from_hex(&"a".repeat(64)).expect("a digest");
        let second = BlobId::from_hex(&"b".repeat(64)).expect("a digest");
        let files = [(first.clone(), 3_u64), (second.clone(), 4_u64)];
        let head = Head::of(&env, json!({}), 0, &env.artifacts, &files);
        assert_eq!(head.artifacts[0].file, None);
        let a = head.artifacts[1].file.as_ref().expect("the first file");
        assert_eq!((a.digest.clone(), a.size), (first, 3));
        let b = head.artifacts[2].file.as_ref().expect("the second file");
        assert_eq!((b.digest.clone(), b.size), (second, 4));

        // A retry's envelope may have grown, but the head pairs with the list the pass stored.
        let mut retried = env.clone();
        retried
            .artifacts
            .push(Artifact::new("late", None, json!({"steps": []})));
        let head = Head::of(&retried, json!({}), 0, &env.artifacts, &files);
        assert_eq!(
            head.artifacts.len(),
            env.artifacts.len(),
            "the head describes the pass's list, not the retried envelope's"
        );
    }

    #[test]
    fn a_head_is_v5_and_a_v4_envelope_is_not_mistaken_for_one() {
        let head = Head::of(&Envelope::new(json!({})), json!({}), 0, &[], &[]);
        let encoded = head.encode().expect("encodes");
        assert_eq!(encoded["v"], 5);
        // `Stored` tells them apart by `v`, and re-encodes the variant it read.
        let stored = Stored::decode(RunId::new(), &encoded).expect("a head");
        assert!(matches!(stored, Stored::Head(_)));
        assert_eq!(stored.agent(), &json!({}));
        assert_eq!(stored.encode().expect("encodes"), encoded);

        let full = Envelope::new(json!({"messages": [1]}))
            .encode()
            .expect("encodes");
        let stored = Stored::decode(RunId::new(), &full).expect("a full envelope");
        assert!(matches!(stored, Stored::Full(_)));
        assert_eq!(stored.encode().expect("encodes"), full);

        // A v5 head is not a v4 envelope: `Envelope::decode` refuses it.
        assert!(Envelope::decode(RunId::new(), &encoded).is_err());
    }

    #[test]
    fn a_head_artifact_has_the_v4_keys_with_the_file_by_reference() {
        let mut env = Envelope::new(json!({}));
        env.artifacts.push(file_artifact());
        env.artifacts.push(Artifact::new(
            "plan",
            Some("application/json".into()),
            json!({"steps": [1]}),
        ));
        let encoded = Head::of(&env, json!({}), 0, &env.artifacts, &[])
            .encode()
            .expect("encodes");
        let artifacts = encoded["artifacts"].as_array().expect("artifacts");
        // A file artifact: the v4 keys, with `file` naming the digest and size, never the bytes.
        let mut keys: Vec<&str> = artifacts[0]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["data", "file", "mime_type", "name"]);
        let mut file_keys: Vec<&str> = artifacts[0]["file"]
            .as_object()
            .expect("a file")
            .keys()
            .map(String::as_str)
            .collect();
        file_keys.sort_unstable();
        assert_eq!(file_keys, ["digest", "filename", "size"]);
        assert_eq!(artifacts[0]["data"], Value::Null);
        // A JSON artifact: the same keys as v4, and no `file`.
        let mut keys: Vec<&str> = artifacts[1]
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["data", "mime_type", "name"]);
        assert_eq!(artifacts[1]["data"], json!({"steps": [1]}));
    }

    #[test]
    fn stored_exposes_the_inbox_and_the_error_of_either_variant() {
        let mut env = Envelope::new(json!({}));
        env.error = Some("boom".into());
        let mut full = Stored::Full(env);
        full.inbox_mut()
            .push(Inbound::new("message", json!({"text": "hi"})).with_id("m-1"));
        assert_eq!(full.error(), Some("boom"));
        assert_eq!(full.inbox_mut().len(), 1);

        let mut head = Head::of(&Envelope::new(json!({})), json!({}), 0, &[], &[]);
        head.error = Some("later".into());
        let mut stored = Stored::Head(head);
        stored
            .inbox_mut()
            .push(Inbound::new("message", json!({"text": "hi"})).with_id("m-2"));
        assert_eq!(stored.error(), Some("later"));
        assert_eq!(stored.inbox_mut().len(), 1);
    }
}
