//! Progress events for observers (A2A streaming, UIs) and the sinks that
//! receive them.
//!
//! Events are **best effort and not durable**: a sink may miss events if the
//! process dies. Anything a consumer needs after a restart is derivable from
//! the durable run record via [`Runtime::view`](crate::Runtime::view) (status,
//! output, error, artifacts).

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use adam_core::{RunId, RunStatus};

use crate::step::StepEvent;
use crate::text::is_false;

/// The most bytes of one file artifact: 4 MiB.
///
/// A file artifact is journaled with the run: its bytes, as base64 (a third more), are in the
/// journal entry of the tool call that made it and in every commit of the run's state after
/// it. The cap keeps that bounded, and sits under the orchestration layer's own limit for a file
/// it keeps (10 MiB), so an agent never makes a file the other side would refuse.
pub const MAX_ARTIFACT_FILE_BYTES: usize = 4 * 1024 * 1024;

/// The most bytes of file artifacts one run keeps, all of its files together: 6 MiB. Enforced by
/// the agent loop (`adam-llm-agent`), which refuses a tool's file that would go over it and tells
/// the model; the runtime itself keeps what it is given. About 8 MiB of base64 in a run's state, which
/// stays under the 16 MiB document of MongoDB.
pub const MAX_RUN_FILE_BYTES: usize = 6 * 1024 * 1024;

/// The longest filename of a file artifact, in bytes (what a file system takes).
pub const MAX_ARTIFACT_FILENAME_BYTES: usize = 255;

/// Why a file cannot be an artifact.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ArtifactFileError {
    /// The file is over [`MAX_ARTIFACT_FILE_BYTES`].
    #[error("the file is {len} bytes, over the limit of {max} bytes for a shared file")]
    TooLarge {
        /// Its size.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The filename is empty, too long, or holds a path separator or a control character: it is a
    /// name, not a path.
    #[error("`{0}` is not a file name (no path, no control characters, at most 255 bytes)")]
    BadFilename(String),
    /// The media type is not of the form `type/subtype`.
    #[error("`{0}` is not a media type (type/subtype)")]
    BadMediaType(String),
}

/// The file of a file artifact: its name and its bytes. The media type is the artifact's
/// [`mime_type`](Artifact::mime_type).
///
/// Journaled as `{"filename": "...", "bytes": "<base64>"}`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ArtifactFile {
    /// The name the file is saved under: a name, never a path.
    pub filename: String,
    /// The content.
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ArtifactFile {
    // The bytes are not printed: a log line or a failed assertion must not carry megabytes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtifactFile")
            .field("filename", &self.filename)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

/// Bytes as a base64 string, the way A2A's `raw` part carries them.
mod base64_bytes {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// A named piece of output produced while a run works (a file, a report, a
/// structured result). A2A maps these to task artifacts.
///
/// Two forms. A **JSON artifact** has `data` (a string becomes a text part, anything else a data
/// part). A **file artifact** ([`Artifact::file`]) has `file`, and its media type is
/// `mime_type`; its `data` is `null`. The A2A server serves a file as a `raw` part with the media
/// type and the filename, which any A2A client reads as a file. Artifacts journaled before the
/// file form existed have no `file` and read as they did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Artifact {
    /// Artifact name.
    pub name: String,
    /// Media type of `data` (or of the file), if known.
    pub mime_type: Option<String>,
    /// The content of a JSON artifact; `null` for a file.
    pub data: Value,
    /// The file, for a file artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<ArtifactFile>,
}

impl Artifact {
    /// A JSON artifact: `data` with its media type, if it has one.
    pub fn new(name: impl Into<String>, mime_type: Option<String>, data: Value) -> Self {
        Self {
            name: name.into(),
            mime_type,
            data,
            file: None,
        }
    }

    /// A file artifact: `bytes`, saved as `filename`, of `media_type`.
    ///
    /// # Errors
    ///
    /// [`ArtifactFileError`]: the file is over [`MAX_ARTIFACT_FILE_BYTES`], the filename is not a name,
    /// or the media type is not `type/subtype`.
    pub fn file(
        name: impl Into<String>,
        media_type: impl Into<String>,
        filename: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<Self, ArtifactFileError> {
        let (media_type, filename) = (media_type.into(), filename.into());
        if bytes.len() > MAX_ARTIFACT_FILE_BYTES {
            return Err(ArtifactFileError::TooLarge {
                len: bytes.len(),
                max: MAX_ARTIFACT_FILE_BYTES,
            });
        }
        let bad_name = filename.is_empty()
            || filename.len() > MAX_ARTIFACT_FILENAME_BYTES
            || filename == "."
            || filename == ".."
            || filename
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\');
        if bad_name {
            return Err(ArtifactFileError::BadFilename(filename));
        }
        let bad_type = media_type.split_once('/').is_none_or(|(kind, sub)| {
            kind.is_empty()
                || sub.is_empty()
                || media_type
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
        });
        if bad_type {
            return Err(ArtifactFileError::BadMediaType(media_type));
        }
        Ok(Self {
            name: name.into(),
            mime_type: Some(media_type),
            data: Value::Null,
            file: Some(ArtifactFile { filename, bytes }),
        })
    }

    /// The bytes of the file, `0` for a JSON artifact.
    pub fn file_len(&self) -> usize {
        self.file.as_ref().map_or(0, |f| f.bytes.len())
    }
}

/// Something observers may want to know about a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// The run's status changed (emitted by the runtime after the commit).
    Status {
        /// The new status.
        status: RunStatus,
        /// Human-readable context (failure reason, retry info).
        detail: Option<String>,
    },
    /// Free-form progress text.
    Progress {
        /// What is going on.
        message: String,
    },
    /// A step of the run's work started, moved, or ended, and under which step it runs (a tool
    /// call, a sub-agent's work, a command it ran): see [`StepEvent`]. The A2A server serves it as
    /// `steps/v1` to a client that asked for it, and as a line of text to one that did not.
    Step(StepEvent),
    /// A piece of the text the model is writing, sent as it arrives: the words of one model turn, in
    /// pieces, so a client can show them growing. The pieces of one `stream` follow each other
    /// (`offset` is where each begins) and the last one says so. The A2A server serves them as
    /// `text-stream/v1` to a client that asked for it and as nothing to one that did not: the whole
    /// text always arrives in the end, with the turn. See [`MAX_TEXT_DELTA_BYTES`](crate::MAX_TEXT_DELTA_BYTES).
    ///
    /// Live, like every event, and meant to be lost: a stream whose pieces were not all heard is
    /// completed by the whole text, which is durable.
    TextDelta {
        /// Which text this is a piece of: one per model turn that wrote words, unique within the run
        /// and at most [`MAX_STREAM_ID_BYTES`](crate::MAX_STREAM_ID_BYTES) bytes. It is also the id
        /// of the message the whole text becomes.
        stream: String,
        /// Where `text` begins in the whole text of the stream, in UTF-8 bytes: 0 for the first
        /// piece, then the sum of the lengths of the pieces before it.
        offset: u64,
        /// The piece. At most [`MAX_TEXT_DELTA_BYTES`](crate::MAX_TEXT_DELTA_BYTES) bytes, whole
        /// characters; empty only on the last piece.
        text: String,
        /// The last piece of the stream: nothing follows it.
        #[serde(default, skip_serializing_if = "is_false")]
        last: bool,
        /// With `last`: the model failed before it finished, so the text so far is all there is and
        /// no whole text follows.
        #[serde(default, skip_serializing_if = "is_false")]
        abandoned: bool,
    },
    /// Application-defined event.
    Custom {
        /// Event kind.
        kind: String,
        /// Event content.
        payload: Value,
    },
    /// An output of the run. Also recorded durably with the next commit, so
    /// it survives restarts in `RunView::artifacts`.
    Artifact {
        /// Artifact name.
        name: String,
        /// Media type of `data` (or of the file), if known.
        mime_type: Option<String>,
        /// The content of a JSON artifact; `null` for a file.
        data: Value,
        /// The file, for a file artifact (see [`Artifact::file`]). Absent in events written before
        /// the file form existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<ArtifactFile>,
    },
}

impl From<Artifact> for RunEvent {
    fn from(a: Artifact) -> Self {
        Self::Artifact {
            name: a.name,
            mime_type: a.mime_type,
            data: a.data,
            file: a.file,
        }
    }
}

/// An event together with the run it belongs to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SinkEvent {
    /// Run the event is about.
    pub run: RunId,
    /// Name of the run's agent.
    pub agent: String,
    /// The event.
    pub event: RunEvent,
}

/// Receiver of [`RunEvent`]s. Best effort: implementations should not block
/// for long, and the runtime ignores what they do with the event.
#[async_trait]
pub trait EventSink: Send + Sync + 'static {
    /// Handle one event of `run`, which is executed by `agent`.
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent);
}

/// Shared sink handle.
pub type DynEventSink = Arc<dyn EventSink>;

#[async_trait]
impl<T: EventSink + ?Sized> EventSink for Arc<T> {
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent) {
        (**self).emit(run, agent, event).await;
    }
}

/// Discards every event. The default sink.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSink;

#[async_trait]
impl EventSink for NoopSink {
    async fn emit(&self, _run: RunId, _agent: &str, _event: RunEvent) {}
}

/// Records every event in memory. A test double; clones share one log.
#[derive(Clone, Debug, Default)]
pub struct CollectingSink {
    events: Arc<Mutex<Vec<SinkEvent>>>,
}

impl CollectingSink {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far, in emission order.
    pub fn events(&self) -> Vec<SinkEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The recorded events of one run.
    pub fn events_for(&self, run: RunId) -> Vec<RunEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.run == run)
            .map(|e| e.event)
            .collect()
    }

    /// Forget everything recorded so far.
    pub fn clear(&self) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

#[async_trait]
impl EventSink for CollectingSink {
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(SinkEvent {
                run,
                agent: agent.to_owned(),
                event,
            });
    }
}

/// Fans events out to any number of in-process subscribers through a tokio
/// broadcast channel. Slow subscribers lose the oldest events (best effort);
/// clones share one channel.
#[derive(Clone, Debug)]
pub struct BroadcastSink {
    tx: broadcast::Sender<SinkEvent>,
}

impl BroadcastSink {
    /// A sink whose subscribers may lag by up to `capacity` events.
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: broadcast::channel(capacity.max(1)).0,
        }
    }

    /// Subscribe to the events of every run, from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<SinkEvent> {
        self.tx.subscribe()
    }

    /// Subscribe to the events of one run, from now on.
    pub fn subscribe_run(&self, run: RunId) -> RunSubscription {
        RunSubscription {
            run,
            rx: self.tx.subscribe(),
        }
    }
}

impl Default for BroadcastSink {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[async_trait]
impl EventSink for BroadcastSink {
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent) {
        // No subscribers is fine: events are best effort.
        let _ = self.tx.send(SinkEvent {
            run,
            agent: agent.to_owned(),
            event,
        });
    }
}

/// The events of a single run from a [`BroadcastSink`].
#[derive(Debug)]
pub struct RunSubscription {
    run: RunId,
    rx: broadcast::Receiver<SinkEvent>,
}

impl RunSubscription {
    /// The next event of the run, or `None` once the sink is gone. Events
    /// dropped because this subscriber lagged are skipped.
    pub async fn recv(&mut self) -> Option<RunEvent> {
        loop {
            match self.rx.recv().await {
                Ok(e) if e.run == self.run => return Some(e.event),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(run = %self.run, skipped = n, "event subscriber lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step::{StepIcon, StepKind, StepState};

    #[test]
    fn events_roundtrip_through_json() {
        let events = [
            RunEvent::Status {
                status: RunStatus::Parked,
                detail: None,
            },
            RunEvent::Progress {
                message: "hi".into(),
            },
            RunEvent::Custom {
                kind: "k".into(),
                payload: serde_json::json!({"a": 1}),
            },
            RunEvent::Artifact {
                name: "report".into(),
                mime_type: Some("text/plain".into()),
                data: serde_json::json!("x"),
                file: None,
            },
            RunEvent::Step(
                StepEvent::new("acp:c2:1", StepKind::Command, "npm test", StepState::Failed)
                    .under("tool:c2")
                    .with_icon(StepIcon::Execute)
                    .with_detail("1 failed"),
            ),
            RunEvent::TextDelta {
                stream: "run-m0-a1b2c3d4".into(),
                offset: 3,
                text: "onacci ".into(),
                last: false,
                abandoned: false,
            },
            RunEvent::TextDelta {
                stream: "run-m0-a1b2c3d4".into(),
                offset: 10,
                text: String::new(),
                last: true,
                abandoned: true,
            },
        ];
        for e in events {
            let json = serde_json::to_value(&e).expect("serialize");
            assert_eq!(serde_json::from_value::<RunEvent>(json).expect("parse"), e);
        }
    }

    #[test]
    fn a_step_event_is_tagged_step_and_carries_the_steps_own_fields() {
        let event = RunEvent::Step(StepEvent::new(
            "tool:c1",
            StepKind::Tool,
            "run_checks",
            StepState::Running,
        ));
        assert_eq!(
            serde_json::to_value(&event).expect("serialize"),
            serde_json::json!({"type": "step", "id": "tool:c1", "kind": "tool",
                               "label": "run_checks", "state": "running"})
        );
    }

    #[test]
    fn a_text_delta_is_tagged_text_delta_and_says_only_what_is_true() {
        let piece = |last, abandoned| RunEvent::TextDelta {
            stream: "s1".into(),
            offset: 3,
            text: "onacci ".into(),
            last,
            abandoned,
        };
        assert_eq!(
            serde_json::to_value(piece(false, false)).expect("serialize"),
            serde_json::json!({"type": "text_delta", "stream": "s1", "offset": 3, "text": "onacci "})
        );
        assert_eq!(
            serde_json::to_value(piece(true, true)).expect("serialize"),
            serde_json::json!({"type": "text_delta", "stream": "s1", "offset": 3, "text": "onacci ",
                               "last": true, "abandoned": true})
        );
        // What a sender that left out the flags meant.
        let bare = serde_json::json!({"type": "text_delta", "stream": "s1", "offset": 3, "text": "onacci "});
        assert_eq!(
            serde_json::from_value::<RunEvent>(bare).expect("parse"),
            piece(false, false)
        );
    }

    const SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg'/>";

    /// A file artifact is journaled as JSON with its bytes in base64, and comes back the same;
    /// a JSON artifact has no `file` member at all.
    #[test]
    fn a_file_artifact_roundtrips_through_json_as_base64() {
        let artifact = Artifact::file("logo", "image/svg+xml", "logo.svg", SVG.to_vec())
            .expect("a small file");
        assert_eq!(artifact.data, Value::Null);
        assert_eq!(artifact.mime_type.as_deref(), Some("image/svg+xml"));
        assert_eq!(artifact.file_len(), SVG.len());

        let json = serde_json::to_value(&artifact).expect("serialize");
        assert_eq!(json["file"]["filename"], "logo.svg");
        assert_eq!(
            json["file"]["bytes"],
            "PHN2ZyB4bWxucz0naHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmcnLz4="
        );
        assert_eq!(
            serde_json::from_value::<Artifact>(json).expect("parse"),
            artifact
        );

        let plain = serde_json::to_value(Artifact::new("r", None, serde_json::json!("x")))
            .expect("serialize");
        assert!(plain.get("file").is_none(), "{plain}");

        // The same through the event.
        let event = RunEvent::from(artifact.clone());
        let back: RunEvent =
            serde_json::from_value(serde_json::to_value(&event).expect("serialize"))
                .expect("parse");
        assert_eq!(back, event);
    }

    /// What was journaled before files existed (an envelope's artifact, a `NOTIFY` payload) still
    /// reads, as a JSON artifact.
    #[test]
    fn artifacts_journaled_before_the_file_form_still_decode() {
        let old = serde_json::json!({"name": "checks", "mime_type": "application/json", "data": {"ok": true}});
        let artifact: Artifact = serde_json::from_value(old).expect("an old artifact");
        assert_eq!(artifact.file, None);
        assert_eq!(artifact.data, serde_json::json!({"ok": true}));

        let old =
            serde_json::json!({"type": "artifact", "name": "n", "mime_type": null, "data": "x"});
        let event: RunEvent = serde_json::from_value(old).expect("an old event");
        assert_eq!(
            event,
            RunEvent::Artifact {
                name: "n".into(),
                mime_type: None,
                data: serde_json::json!("x"),
                file: None,
            }
        );
    }

    #[test]
    fn a_file_must_be_within_the_cap_and_named_and_typed_properly() {
        let big = vec![0u8; MAX_ARTIFACT_FILE_BYTES + 1];
        assert_eq!(
            Artifact::file("f", "text/plain", "f.txt", big),
            Err(ArtifactFileError::TooLarge {
                len: MAX_ARTIFACT_FILE_BYTES + 1,
                max: MAX_ARTIFACT_FILE_BYTES
            })
        );
        let at_cap = vec![0u8; MAX_ARTIFACT_FILE_BYTES];
        assert!(Artifact::file("f", "application/octet-stream", "f.bin", at_cap).is_ok());
        assert!(Artifact::file("f", "text/plain", "empty.txt", Vec::new()).is_ok());

        for bad in [
            "",
            ".",
            "..",
            "a/b.txt",
            "a\\b.txt",
            "x\ny.txt",
            &"n".repeat(256),
        ] {
            assert!(
                matches!(
                    Artifact::file("f", "text/plain", bad, vec![1]),
                    Err(ArtifactFileError::BadFilename(_))
                ),
                "{bad:?}"
            );
        }
        for bad in [
            "",
            "text",
            "text/",
            "/plain",
            "text/plain; x",
            "te xt/plain",
        ] {
            assert!(
                matches!(
                    Artifact::file("f", bad, "f.txt", vec![1]),
                    Err(ArtifactFileError::BadMediaType(_))
                ),
                "{bad:?}"
            );
        }
    }

    /// A log line or a failed assertion does not carry the file.
    #[test]
    fn debug_does_not_print_the_bytes() {
        let artifact =
            Artifact::file("f", "text/plain", "f.txt", b"SECRETBYTES".to_vec()).expect("file");
        let shown = format!("{artifact:?}");
        assert!(
            !shown.contains("SECRETBYTES") && shown.contains("<11 bytes>"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn run_subscription_filters_by_run() {
        let sink = BroadcastSink::new(8);
        let (a, b) = (RunId::new(), RunId::new());
        let mut sub = sink.subscribe_run(b);
        sink.emit(
            a,
            "x",
            RunEvent::Progress {
                message: "a".into(),
            },
        )
        .await;
        sink.emit(
            b,
            "x",
            RunEvent::Progress {
                message: "b".into(),
            },
        )
        .await;
        assert_eq!(
            sub.recv().await,
            Some(RunEvent::Progress {
                message: "b".into()
            })
        );
    }
}
