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

use adam_core::time::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use adam_core::{BlobId, DynStore, RunId, StoreError, StoreResult};

use crate::agent::Inbound;
use crate::artifact_ref::{ArtifactRef, FileRef};
use crate::blob::blob_id;
use crate::envelope::{Envelope, VERSION as FULL_VERSION};
use crate::events::{Artifact, ArtifactFile};
use crate::retry::RetryRecord;
use crate::runtime::RuntimeError;

/// The head's layout version. A run's `state` is `1` (the v4 envelope) or this.
pub(crate) const VERSION: u32 = 5;

/// The body generation a head names: the rows are `(run, generation, idx)` in the store, and `len`
/// is the integrity check a reader makes against the **prefix** of
/// `body_list(run, generation)` — the first `len` rows. A generation is append-only, so rows past
/// `len` are a later commit's (or a lost attempt's, not yet collected) and are not this head's.
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
    /// What a failing run recorded of its retries: the failures per key and each key's policy
    /// (ADR 0061). Cleared by a transition that succeeds; absent from state written before it
    /// existed, and not written while `None`; one that does not decode reads as `None`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::retry::lenient_record"
    )]
    pub retry: Option<RetryRecord>,
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
    pub deadline: Option<Timestamp>,
    /// The limit the deadline was set from, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_secs: Option<u64>,
    /// What is left of the time limit while the run waits for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<u64>,
    /// When the agent itself asked to be woken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_wake: Option<Timestamp>,
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
            retry: env.retry.clone(),
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

    /// Drop every file artifact whose bytes the store does not hold, so the head never names a
    /// digest that cannot be read. An inherited file whose bytes are gone is left out (and
    /// logged): a head carries only its digest, so refusing would retry for ever. The other
    /// artifacts stay.
    pub(crate) async fn prune_missing_files(&mut self, store: &DynStore) -> StoreResult<()> {
        drop_missing_files(store, &mut self.artifacts).await
    }

    /// The head as the full v4 envelope a transition reads, and the parts a commit carries from it.
    ///
    /// `agent` is the joined agent state and the transcript is no longer a member (it is in
    /// `body`), so the envelope's `artifacts` are the head's own references
    /// ([`carried_artifacts`](Self::carried_artifacts)). `v` is the v4 version: the envelope is a
    /// working value, and a commit that cannot write a head (a refused split) falls back to
    /// encoding it as the whole state, which is v4.
    pub(crate) fn into_parts(self, agent: Value) -> (Envelope, HeadParts) {
        let Self {
            inbox,
            seq,
            attempt,
            retry,
            rev,
            output,
            error,
            artifacts,
            deadline,
            limit_secs,
            remaining_secs,
            agent_wake,
            hb,
            body,
            ..
        } = self;
        let parts = HeadParts { artifacts, body };
        let env = Envelope {
            v: FULL_VERSION,
            agent,
            inbox,
            seq,
            attempt,
            retry,
            rev,
            output,
            error,
            artifacts: carried_artifacts(&parts.artifacts),
            deadline,
            limit_secs,
            remaining_secs,
            agent_wake,
            hb,
        };
        (env, parts)
    }
}

/// What a commit carries from the head it read, without the agent state: the transcript set the
/// head names and its artifact references.
///
/// A commit that does not re-split writes the head back from these, and one that does keeps the
/// file references so it never reads a file's bytes (the upload pass covers only what the step
/// adds).
pub(crate) struct HeadParts {
    /// The head's artifacts, in order.
    pub artifacts: Vec<HeadArtifact>,
    /// The generation and the prefix of it the head names.
    pub body: BodyRef,
}

impl HeadParts {
    /// The content address and size of every file artifact, in artifact order: what a commit that
    /// carries this head pairs its next head's file references with, so it never reads the bytes.
    pub(crate) fn file_refs(&self) -> Vec<(BlobId, u64)> {
        self.artifacts
            .iter()
            .filter_map(|artifact| artifact.file.as_ref())
            .map(|file| (file.digest.clone(), file.size))
            .collect()
    }

    /// This head with every file artifact whose bytes the store does not hold left out: what a
    /// commit that carries it writes, so it never names a digest that is not there.
    pub(crate) async fn without_missing_files(&self, store: &DynStore) -> StoreResult<Self> {
        let mut artifacts = self.artifacts.clone();
        drop_missing_files(store, &mut artifacts).await?;
        Ok(Self {
            artifacts,
            body: self.body,
        })
    }

    /// The head a commit that does not split writes: these transcript set and artifacts, with
    /// `env`'s own members — the status the commit carries is on the record, not here — and the
    /// transition's own `added` artifacts merged in (a failed transition keeps what it emitted).
    ///
    /// The generation is named explicitly: the store patches it only for a commit that writes rows,
    /// and a head that names none is unreadable. The head these came from is current (a stale one
    /// is served as the state, not the head), so its generation is the run's.
    pub(crate) fn carried(&self, env: &Envelope, added: &[Artifact]) -> Head {
        Head {
            v: VERSION,
            agent: env.agent.clone(),
            inbox: env.inbox.clone(),
            seq: env.seq,
            attempt: env.attempt,
            retry: env.retry.clone(),
            rev: env.rev,
            output: env.output.clone(),
            error: env.error.clone(),
            artifacts: merge_head_artifacts(&self.artifacts, added),
            deadline: env.deadline,
            limit_secs: env.limit_secs,
            remaining_secs: env.remaining_secs,
            agent_wake: env.agent_wake,
            hb: env.hb,
            body: self.body,
        }
    }
}

/// The head's artifacts with `added` merged in the way a step's commit merges them: an artifact
/// whose id a JSON artifact of the head has replaces it in place, and a file is appended (a file
/// is content-addressed and never replaces anything). A file's reference comes from its bytes.
fn merge_head_artifacts(base: &[HeadArtifact], added: &[Artifact]) -> Vec<HeadArtifact> {
    let mut artifacts = base.to_vec();
    for artifact in added {
        let file = artifact.file.as_ref().map(|file| HeadFile {
            filename: file.filename.clone(),
            digest: blob_id(&file.bytes),
            size: u64::try_from(file.bytes.len()).unwrap_or(u64::MAX),
        });
        let head = HeadArtifact {
            // A file has no identity: it is never replaced, and an id on one is dropped.
            id: file.is_none().then(|| artifact.id.clone()).flatten(),
            name: artifact.name.clone(),
            mime_type: artifact.mime_type.clone(),
            data: artifact.data.clone(),
            file,
        };
        if let Some(id) = &head.id
            && let Some(held) = artifacts
                .iter_mut()
                .find(|held| held.file.is_none() && held.id.as_deref() == Some(id.as_str()))
        {
            *held = head;
            continue;
        }
        artifacts.push(head);
    }
    artifacts
}

/// Drop the file artifacts of `artifacts` whose digests the store does not hold; each one is
/// logged by [`missing_blobs`](crate::blob::missing_blobs).
async fn drop_missing_files(
    store: &DynStore,
    artifacts: &mut Vec<HeadArtifact>,
) -> StoreResult<()> {
    let refs = artifacts
        .iter()
        .filter_map(|artifact| artifact.file.as_ref())
        .map(|file| file.digest.clone());
    let missing = crate::blob::missing_blobs(store, refs).await?;
    if !missing.is_empty() {
        artifacts.retain(|artifact| {
            artifact
                .file
                .as_ref()
                .is_none_or(|file| !missing.contains(&file.digest))
        });
    }
    Ok(())
}

/// The head's artifacts as the runtime's [`Artifact`], with a file's bytes empty: see
/// [`Head::carried_artifacts`].
fn carried_artifacts(artifacts: &[HeadArtifact]) -> Vec<Artifact> {
    artifacts
        .iter()
        .map(|artifact| Artifact {
            id: artifact.id.clone(),
            name: artifact.name.clone(),
            mime_type: artifact.mime_type.clone(),
            data: artifact.data.clone(),
            file: artifact.file.as_ref().map(|file| ArtifactFile {
                filename: file.filename.clone(),
                bytes: Vec::new(),
            }),
        })
        .collect()
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

/// A head artifact as a view carries it: a file by content address, never its bytes. Consumes the
/// head artifact, so a poll does not clone its members.
fn head_artifact_ref(artifact: HeadArtifact) -> ArtifactRef {
    ArtifactRef {
        id: artifact.id,
        name: artifact.name,
        mime_type: artifact.mime_type,
        data: artifact.data,
        file: artifact
            .file
            .map(|file| FileRef::blob(file.filename, file.size, file.digest)),
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
    #[cfg(test)]
    pub(crate) fn encode(&self) -> Result<Value, StoreError> {
        match self {
            Self::Full(envelope) => envelope.encode(),
            Self::Head(head) => head.encode(),
        }
    }

    /// The inbox, read.
    pub(crate) fn inbox(&self) -> &[Inbound] {
        match self {
            Self::Full(envelope) => &envelope.inbox,
            Self::Head(head) => &head.inbox,
        }
    }

    /// The result of a `Done` run, in either variant.
    pub(crate) fn output(&self) -> &Value {
        match self {
            Self::Full(envelope) => &envelope.output,
            Self::Head(head) => &head.output,
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

    /// The failure reason, to set it (a cancel).
    pub(crate) fn error_mut(&mut self) -> &mut Option<String> {
        match self {
            Self::Full(envelope) => &mut envelope.error,
            Self::Head(head) => &mut head.error,
        }
    }

    /// The body generation a `Head` names; `None` for a `Full` state, whose transcript is in it.
    pub(crate) fn body(&self) -> Option<&BodyRef> {
        match self {
            Self::Full(_) => None,
            Self::Head(head) => Some(&head.body),
        }
    }

    /// The members a [`RunView`](crate::RunView) takes, moved out of the document (a file's bytes
    /// are moved, not cloned, for a v4 run).
    pub(crate) fn into_parts(self) -> ViewParts {
        match self {
            Self::Full(envelope) => ViewParts {
                agent: envelope.agent,
                inbox: envelope.inbox,
                attempt: envelope.attempt,
                output: envelope.output,
                error: envelope.error,
                artifacts: envelope
                    .artifacts
                    .into_iter()
                    .map(ArtifactRef::from)
                    .collect(),
                agent_wake: envelope.agent_wake,
                hb: envelope.hb,
            },
            Self::Head(head) => ViewParts {
                agent: head.agent,
                inbox: head.inbox,
                attempt: head.attempt,
                output: head.output,
                error: head.error,
                artifacts: head.artifacts.into_iter().map(head_artifact_ref).collect(),
                agent_wake: head.agent_wake,
                hb: head.hb,
            },
        }
    }
}

/// The members a view takes from a stored document.
pub(crate) struct ViewParts {
    pub agent: Value,
    pub inbox: Vec<Inbound>,
    pub attempt: u32,
    pub output: Value,
    pub error: Option<String>,
    pub artifacts: Vec<ArtifactRef>,
    pub agent_wake: Option<Timestamp>,
    pub hb: bool,
}

/// A claimed run's state, decoded and joined: what a transition reads.
///
/// A v4 run is its whole state in `env`, with no head and no rows. A v5 run's `env` is the head
/// with the joined agent state, `head` is what a commit carries from it, and `rows` is the
/// transcript the head named — kept so a commit decides append-or-compaction without decoding the
/// previous state again.
pub(crate) struct Claimed {
    /// The envelope the transition reads.
    pub env: Envelope,
    /// What a commit carries from the head, for a v5 run.
    pub head: Option<HeadParts>,
    /// The transcript the head named, oldest first (empty for a v4 run).
    pub rows: Vec<Value>,
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
        let mut record = RetryRecord::default();
        record.fail("fetch", &crate::RetryPolicy::new(4));
        env.retry = Some(record);
        env.hb = true;
        let head = Head::of(&env, json!({"turns": 3}), 2, &env.artifacts, &[]);
        assert_eq!(head.v, 5);
        assert_eq!(head.agent, json!({"turns": 3}));
        assert_eq!((head.seq, head.attempt, head.rev), (7, 1, 4));
        assert_eq!(
            head.retry, env.retry,
            "a v5 run keeps its retry record (ADR 0061)"
        );
        let decoded =
            Head::decode(RunId::new(), &head.encode().expect("encodes")).expect("decodes");
        assert_eq!(decoded.retry, env.retry);
        let (back, parts) = decoded.into_parts(json!({"turns": 3}));
        assert_eq!(
            back.retry, env.retry,
            "and the envelope a transition reads has it"
        );
        assert_eq!(
            parts.carried(&back, &[]).retry,
            env.retry,
            "and a commit carries it"
        );
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

    /// The head's deadline and agent wake are `adam_core::time::Timestamp`s: a `.120` instant
    /// keeps chrono's three digits (jiff's own serde would write `.12`), because the head is the
    /// v5 form of the v4 envelope's stored bytes.
    #[test]
    fn a_head_deadline_keeps_chronos_bytes() {
        let at: Timestamp = "2026-01-02T03:04:05.120Z".parse().expect("a timestamp");
        let mut env = Envelope::new(json!({}));
        env.deadline = Some(at);
        env.agent_wake = Some(at);
        let head = Head::of(&env, json!({}), 0, &[], &[]);
        let value = head.encode().expect("encodes");
        assert_eq!(value["deadline"].as_str(), Some("2026-01-02T03:04:05.120Z"));
        assert_eq!(
            value["agent_wake"].as_str(),
            Some("2026-01-02T03:04:05.120Z")
        );
        assert_eq!(
            Head::decode(RunId::new(), &value)
                .expect("decodes")
                .deadline,
            Some(at)
        );
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
        assert_eq!(stored.encode().expect("encodes"), encoded);

        let full = Envelope::new(json!({"messages": [1]}))
            .encode()
            .expect("encodes");
        let stored = Stored::decode(RunId::new(), &full).expect("a full envelope");
        assert!(matches!(stored, Stored::Full(_)));
        assert_eq!(stored.encode().expect("encodes"), full);

        // A v5 head is not a v4 envelope: `Envelope::decode` refuses it, and the refusal is
        // `Corrupt` — the class a v4 process's worker sees, so it commits the run `Failed` rather
        // than stepping a head as if it were a state (ADR 0055, the deploy boundary).
        let refused =
            Envelope::decode(RunId::new(), &encoded).expect_err("a v5 head is not a v4 envelope");
        assert!(
            matches!(refused, RuntimeError::Corrupt { .. }),
            "{refused:?}"
        );
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
