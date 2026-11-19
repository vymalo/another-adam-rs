//! Progress events for observers (A2A streaming, UIs) and the sinks that
//! receive them.
//!
//! Events are **best effort and not durable**: a sink may miss events if the
//! process dies. ([`BroadcastSink`] keeps the last few seconds of each run for a subscriber that
//! attaches late; that is a fidelity aid, not a record.) Anything a consumer needs after a restart is derivable from
//! the durable run record via [`Runtime::view`](crate::Runtime::view) (status,
//! output, error, artifacts).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::broadcast;

use adam_core::{RunId, RunStatus};

use crate::step::StepEvent;
use crate::text::is_false;
use crate::usage::UsageEvent;

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
    /// The filename is empty, too long, or holds a path separator, a `:` or a control character: it
    /// is a name, not a path or a URL.
    #[error("`{0}` is not a file name (no path, no `:`, no control characters, at most 255 bytes)")]
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
    /// The artifact's identity within the run, when it has one: **a later artifact of the run with
    /// the same id replaces it, whichever tool made it**, in place (it keeps its position) instead of
    /// being added after it, so a thing that changes while a run works (a plan, a surface) is one
    /// artifact, not one per change. `None` (artifacts journaled before ids existed, and every
    /// artifact that is only ever made once) is not replaced by anything. **Files have no id**: a
    /// file artifact is content-addressed ([ADR 0012](../../../docs/decisions/0012-files-as-a2a-artifacts.md)),
    /// so [`with_id`](Self::with_id) leaves a file without one, and the run never replaces a file nor
    /// replaces anything with a file (an id on a file is dropped, both are kept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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
            id: None,
            name: name.into(),
            mime_type,
            data,
            file: None,
        }
    }

    /// Keep this artifact under `id`: see [`id`](Self::id). A file artifact stays without one.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        if self.file.is_none() {
            self.id = Some(id.into());
        }
        self
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
        if !is_file_name(&filename) {
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
            id: None,
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

    /// The SHA-256 of the file's bytes, as 64 lower-case hexadecimal digits: the name the person's
    /// screen keeps the file under, and what a form's picture of it (`ask_user`'s `image.sha256`)
    /// refers to. `None` for a JSON artifact.
    pub fn file_sha256(&self) -> Option<String> {
        self.file
            .as_ref()
            .map(|f| crate::file::sha256_hex(&f.bytes))
    }
}

/// Whether `filename` is a name a file can be saved under: not empty, at most
/// [`MAX_ARTIFACT_FILENAME_BYTES`], not `.` or `..`, no path separator, no control character and
/// no `:` (which would let a name read as a URL, `http:evil.example`, where a screen or a Markdown
/// link resolves it).
fn is_file_name(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= MAX_ARTIFACT_FILENAME_BYTES
        && filename != "."
        && filename != ".."
        && !filename
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
}

/// Something observers may want to know about a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    /// The run's status changed (emitted by the runtime after the commit), or a worker took the
    /// run for the first time: then `status` is the run's own (`runnable`, nothing was committed)
    /// and `detail` is `claimed`, to say that the run is being stepped now. A status event is a
    /// prompt to read the durable run (`Runtime::view`), which is the truth.
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
    /// A piece of the **reasoning** the model is writing before its answer (a model in thinking
    /// mode), shaped like [`RunEvent::TextDelta`] and with a stream of its own: one per model turn
    /// that reasoned, ended (`last`) before the turn's words begin. Reasoning is not the answer:
    /// it is in no run output, no turn output and no step output, and it is not durable (no whole
    /// text follows; a client that wants it keeps the pieces). The A2A server serves it as
    /// `text-stream/v1` chunks marked `kind: "reasoning"` to a client that activated the extension,
    /// and as nothing to one that did not. The bounds are [`RunEvent::TextDelta`]'s.
    ///
    /// A new variant: a process that does not know it ignores the event (the wire of
    /// `adam-notify-postgres` drops what it cannot read), so reasoning is never taken for text.
    ReasoningDelta {
        /// Which reasoning this is a piece of: unique within the run (and different from every
        /// text stream's), at most [`MAX_STREAM_ID_BYTES`](crate::MAX_STREAM_ID_BYTES) bytes.
        stream: String,
        /// Where `text` begins in the whole reasoning of the stream, in UTF-8 bytes.
        offset: u64,
        /// The piece. At most [`MAX_TEXT_DELTA_BYTES`](crate::MAX_TEXT_DELTA_BYTES) bytes, whole
        /// characters; empty only on the last piece.
        text: String,
        /// The last piece of the stream: nothing follows it.
        #[serde(default, skip_serializing_if = "is_false")]
        last: bool,
        /// With `last`: the model failed before it finished.
        #[serde(default, skip_serializing_if = "is_false")]
        abandoned: bool,
    },
    /// A model call completed: its tokens, its model, and the step of the task it ran under (see
    /// [`UsageEvent`]). The A2A server serves it as a `usage/v1` call report to a client that
    /// activated the extension, and as nothing to one that did not.
    ///
    /// A child run's call is emitted as an event of the **root** run it works for
    /// ([`Emitter::emit_for`](crate::Emitter::emit_for)), with the root's step it runs under, so that
    /// the root's subscribers, the ones who asked for the task, hear it.
    Usage(UsageEvent),
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
        /// The id the artifact is kept under, if it has one (see [`Artifact::id`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
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
            id: a.id,
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

/// The most live events [`BroadcastSink`] keeps per run for a late subscriber: the newest 64, the
/// oldest dropped. A run's first seconds are a few steps and a few text pieces; the cap is only
/// there so a chatty run cannot grow the buffer.
pub const REPLAY_EVENTS_PER_RUN: usize = 64;

/// How long [`BroadcastSink`] keeps a live event for a late subscriber: 30 seconds. A subscriber
/// attaches within moments of the run's start (the gap between submitting a run and subscribing to
/// it); an event older than this is not replayed, and the durable record says what happened since.
pub const REPLAY_MAX_AGE: Duration = Duration::from_secs(30);

/// The most runs [`BroadcastSink`] keeps events for: 64. Past it the run whose newest event is the
/// oldest is forgotten. This only decides which runs are kept; [`REPLAY_MAX_BYTES`] bounds what they
/// hold, since a step's end can carry about 12 KiB of input and output and a tool result can be
/// megabytes.
pub const REPLAY_MAX_RUNS: usize = 64;

/// The most bytes the replay of a [`BroadcastSink`] keeps for every run together: 16 MiB, a per
/// process cap, as an estimate of the heap the events hold (`event_bytes`). The count bounds
/// alone leave the worst case near 50 MiB (64 runs of 64 events, each a step end with its input
/// and output); one run's tool results of megabytes would pin it. Past the bound the globally
/// oldest events go first, so every retained run keeps its newest events, and an event larger than
/// the whole bound is not kept at all.
pub const REPLAY_MAX_BYTES: usize = 16 * 1024 * 1024;

/// The bounds of the replay of a [`BroadcastSink`].
#[derive(Clone, Copy, Debug)]
struct ReplayBounds {
    per_run: usize,
    max_age: Duration,
    max_runs: usize,
    max_bytes: usize,
}

impl ReplayBounds {
    const DEFAULT: Self = Self {
        per_run: REPLAY_EVENTS_PER_RUN,
        max_age: REPLAY_MAX_AGE,
        max_runs: REPLAY_MAX_RUNS,
        max_bytes: REPLAY_MAX_BYTES,
    };
}

/// The recent live events of each run, so that a subscriber that attaches a moment after a run
/// began still hears how it began. Not the truth: only what [`BroadcastSink::emit`] saw, for a
/// while.
#[derive(Debug)]
struct Replay {
    bounds: ReplayBounds,
    runs: HashMap<RunId, VecDeque<(Instant, SinkEvent)>>,
    /// The estimate of the heap every kept event holds, as [`event_bytes`] counts it.
    bytes: usize,
}

impl Replay {
    fn new(bounds: ReplayBounds) -> Self {
        Self {
            bounds,
            runs: HashMap::new(),
            bytes: 0,
        }
    }

    /// Note an event that is about to be sent. A status is not kept: it is a prompt to read the
    /// durable record, which is the truth, and a stale one is noise. It also **ends the run's
    /// buffer**: the replay holds what happened since the run last changed state. A run that parks
    /// (its turn ended, it waits for a message) and is taken up again is one run, and its next
    /// subscriber is there for the next turn, not to hear the last one again. An artifact is not
    /// kept either: it is durable (`RunView::artifacts`) and can be large.
    fn record(&mut self, now: Instant, event: &SinkEvent) {
        match &event.event {
            RunEvent::Status { .. } => {
                self.forget(event.run);
                return;
            }
            RunEvent::Artifact { .. } => return,
            _ => {}
        }
        if !self.runs.contains_key(&event.run) {
            self.prune(now);
            if self.runs.len() >= self.bounds.max_runs {
                let oldest = self
                    .runs
                    .iter()
                    .min_by_key(|(_, events)| events.back().map(|(at, _)| *at))
                    .map(|(run, _)| *run);
                if let Some(run) = oldest {
                    self.forget(run);
                }
            }
        }
        let size = event_bytes(event);
        let events = self.runs.entry(event.run).or_default();
        events.push_back((now, event.clone()));
        self.bytes += size;
        while events.len() > self.bounds.per_run {
            if let Some((_, dropped)) = events.pop_front() {
                self.bytes -= event_bytes(&dropped);
            }
        }
        // The stamps only grow, so the old events are at the front.
        while events
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > self.bounds.max_age)
        {
            if let Some((_, dropped)) = events.pop_front() {
                self.bytes -= event_bytes(&dropped);
            }
        }
        // The byte bound: the globally oldest event goes first, so a run keeps its newest events
        // even when its older ones are large. An event over the whole bound is dropped at once.
        while self.bytes > self.bounds.max_bytes {
            let oldest = self
                .runs
                .iter()
                .filter_map(|(run, events)| events.front().map(|(at, _)| (*at, *run)))
                .min();
            let Some((_, run)) = oldest else { break };
            self.pop_front(run);
        }
    }

    /// The events of `run` that are young enough, oldest first.
    fn snapshot(&mut self, now: Instant, run: RunId) -> VecDeque<RunEvent> {
        let max_age = self.bounds.max_age;
        let Some(events) = self.runs.get_mut(&run) else {
            return VecDeque::new();
        };
        while events
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > max_age)
        {
            if let Some((_, dropped)) = events.pop_front() {
                self.bytes -= event_bytes(&dropped);
            }
        }
        if events.is_empty() {
            self.runs.remove(&run);
            return VecDeque::new();
        }
        events.iter().map(|(_, e)| e.event.clone()).collect()
    }

    /// Forget every run whose newest event is too old.
    fn prune(&mut self, now: Instant) {
        let max_age = self.bounds.max_age;
        let mut freed = 0;
        self.runs.retain(|_, events| {
            let keep = events
                .back()
                .is_some_and(|(at, _)| now.saturating_duration_since(*at) <= max_age);
            if !keep {
                freed += events.iter().map(|(_, e)| event_bytes(e)).sum::<usize>();
            }
            keep
        });
        self.bytes -= freed;
    }

    /// Forget everything kept for `run`.
    fn forget(&mut self, run: RunId) {
        if let Some(events) = self.runs.remove(&run) {
            self.bytes -= events.iter().map(|(_, e)| event_bytes(e)).sum::<usize>();
        }
    }

    /// Drop the oldest event of `run`, and the run with its last one.
    fn pop_front(&mut self, run: RunId) {
        let Some(events) = self.runs.get_mut(&run) else {
            return;
        };
        if let Some((_, dropped)) = events.pop_front() {
            self.bytes -= event_bytes(&dropped);
        }
        if events.is_empty() {
            self.runs.remove(&run);
        }
    }
}

/// An estimate of the heap a [`SinkEvent`] keeps alive: the event's own storage, its strings, its
/// maps and its JSON payloads, by their lengths (not the allocator's capacities). It is what
/// [`REPLAY_MAX_BYTES`] bounds, so the same event always counts the same.
fn event_bytes(event: &SinkEvent) -> usize {
    std::mem::size_of::<SinkEvent>() + event.agent.len() + run_event_bytes(&event.event)
}

fn run_event_bytes(event: &RunEvent) -> usize {
    match event {
        RunEvent::Status { detail, .. } => opt_str_bytes(detail),
        RunEvent::Progress { message } => message.len(),
        RunEvent::Step(step) => {
            step.id.len()
                + opt_str_bytes(&step.parent)
                + step.label.len()
                + opt_str_bytes(&step.detail)
                + step.input.as_ref().map_or(0, map_bytes)
                + step.output.as_ref().map_or(0, |output| output.text.len())
        }
        RunEvent::TextDelta { stream, text, .. }
        | RunEvent::ReasoningDelta { stream, text, .. } => stream.len() + text.len(),
        RunEvent::Usage(usage) => {
            usage.call.len()
                + opt_str_bytes(&usage.step)
                + opt_str_bytes(&usage.provider)
                + usage.model.len()
        }
        RunEvent::Custom { kind, payload } => kind.len() + value_bytes(payload),
        RunEvent::Artifact {
            id,
            name,
            mime_type,
            data,
            file,
        } => {
            opt_str_bytes(id)
                + name.len()
                + opt_str_bytes(mime_type)
                + value_bytes(data)
                + file
                    .as_ref()
                    .map_or(0, |file| file.filename.len() + file.bytes.len())
        }
    }
}

fn opt_str_bytes(text: &Option<String>) -> usize {
    text.as_ref().map_or(0, String::len)
}

fn map_bytes(map: &Map<String, Value>) -> usize {
    map.iter()
        .map(|(key, value)| key.len() + value_bytes(value))
        .sum::<usize>()
        + map.len() * std::mem::size_of::<Value>()
}

fn value_bytes(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
        Value::String(text) => text.len(),
        Value::Array(items) => {
            items.iter().map(value_bytes).sum::<usize>()
                + items.len() * std::mem::size_of::<Value>()
        }
        Value::Object(map) => map_bytes(map),
    }
}

/// Fans events out to any number of in-process subscribers through a tokio
/// broadcast channel. Slow subscribers lose the oldest events (best effort);
/// clones share one channel.
///
/// It also keeps a small, bounded **replay** of each run's recent live events, so that
/// [`subscribe_run`](Self::subscribe_run) can start with what happened just before it was called:
/// a run is submitted and then subscribed to, and a worker may take the run, and emit its first
/// steps, in between. The replay is a latency and fidelity aid, never the truth: it holds at most
/// [`REPLAY_EVENTS_PER_RUN`] events per run, none older than [`REPLAY_MAX_AGE`], for at most
/// [`REPLAY_MAX_RUNS`] runs, and no more than [`REPLAY_MAX_BYTES`] in all; it holds no `Status` (read
/// the durable record) and no `Artifact` (durable too); and a run's events are dropped whenever its
/// status changes (it holds what happened since the run last changed state).
#[derive(Clone, Debug)]
pub struct BroadcastSink {
    tx: broadcast::Sender<SinkEvent>,
    /// Held across record-and-send in `emit` and across snapshot-and-subscribe in
    /// `subscribe_run`, never across an `.await`: a subscriber gets exactly the buffered events
    /// and everything sent after them.
    replay: Arc<Mutex<Replay>>,
}

impl BroadcastSink {
    /// A sink whose subscribers may lag by up to `capacity` events.
    pub fn new(capacity: usize) -> Self {
        Self::with_bounds(capacity, ReplayBounds::DEFAULT)
    }

    fn with_bounds(capacity: usize, bounds: ReplayBounds) -> Self {
        Self {
            tx: broadcast::channel(capacity.max(1)).0,
            replay: Arc::new(Mutex::new(Replay::new(bounds))),
        }
    }

    /// Subscribe to the events of every run, from now on. No replay: this is the feed of every
    /// run, and a consumer of it follows the runs it cares about with
    /// [`subscribe_run`](Self::subscribe_run).
    pub fn subscribe(&self) -> broadcast::Receiver<SinkEvent> {
        self.tx.subscribe()
    }

    /// Subscribe to the events of one run: first its recent live events (see the type's docs for
    /// the bounds), then everything emitted from now on, once each and in order, with nothing
    /// missed in between.
    ///
    /// A consumer that subscribes again to a run it already follows may see a recent event a
    /// second time, so it must take an event as a snapshot (a step is one by id) or by position (a
    /// text piece carries its `offset`), as the A2A server does.
    pub fn subscribe_run(&self, run: RunId) -> RunSubscription {
        let mut replay = self.replay.lock().unwrap_or_else(PoisonError::into_inner);
        let backlog = replay.snapshot(Instant::now(), run);
        // Subscribing under the lock: `emit` records and sends under it, so an event is either in
        // the backlog or will be received, never both and never neither.
        let rx = self.tx.subscribe();
        drop(replay);
        RunSubscription { run, backlog, rx }
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
        let event = SinkEvent {
            run,
            agent: agent.to_owned(),
            event,
        };
        // No `.await` while the lock is held.
        let mut replay = self.replay.lock().unwrap_or_else(PoisonError::into_inner);
        replay.record(Instant::now(), &event);
        // No subscribers is fine: events are best effort.
        let _ = self.tx.send(event);
        drop(replay);
    }
}

/// The events of a single run from a [`BroadcastSink`]: the run's recent events first, then the
/// live ones.
#[derive(Debug)]
pub struct RunSubscription {
    run: RunId,
    /// What the sink remembered of the run when this subscription was made, oldest first.
    backlog: VecDeque<RunEvent>,
    rx: broadcast::Receiver<SinkEvent>,
}

impl RunSubscription {
    /// The next event of the run, or `None` once the sink is gone. The events the sink
    /// remembered when the subscription was made come first, then the live ones, each once and in
    /// order. Events dropped because this subscriber lagged are skipped.
    pub async fn recv(&mut self) -> Option<RunEvent> {
        if let Some(event) = self.backlog.pop_front() {
            return Some(event);
        }
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
                id: None,
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
            RunEvent::ReasoningDelta {
                stream: "run-r0-a1b2c3d4".into(),
                offset: 0,
                text: "The user asks".into(),
                last: false,
                abandoned: false,
            },
            RunEvent::Usage(
                UsageEvent::new(
                    "run-c0-a1b2c3d4",
                    "glm-5.3",
                    adam_model::Usage::new(41250, 812).with_reasoning_tokens(300),
                )
                .under("tool:c2")
                .with_provider("openai")
                .with_context_window(131_072),
            ),
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
    fn a_usage_event_is_tagged_usage() {
        let event = RunEvent::Usage(UsageEvent::new("c1", "m", adam_model::Usage::new(3, 4)));
        assert_eq!(
            serde_json::to_value(&event).expect("serialize"),
            serde_json::json!({"type": "usage", "call": "c1", "model": "m",
                               "usage": {"input_tokens": 3, "output_tokens": 4}})
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

    #[test]
    fn a_reasoning_delta_is_tagged_apart_from_text() {
        let piece = RunEvent::ReasoningDelta {
            stream: "s-r".into(),
            offset: 0,
            text: "hm".into(),
            last: true,
            abandoned: false,
        };
        assert_eq!(
            serde_json::to_value(&piece).expect("serialize"),
            serde_json::json!({"type": "reasoning_delta", "stream": "s-r", "offset": 0, "text": "hm",
                               "last": true})
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
                id: None,
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
            "http:evil.example",
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

    fn progress(n: usize) -> RunEvent {
        RunEvent::Progress {
            message: format!("p{n}"),
        }
    }

    fn status(status: RunStatus) -> RunEvent {
        RunEvent::Status {
            status,
            detail: None,
        }
    }

    /// What `sub` has ready now, without waiting for more.
    async fn drain(sub: &mut RunSubscription) -> Vec<RunEvent> {
        let mut got = Vec::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_millis(20), sub.recv()).await {
            got.push(e);
        }
        got
    }

    /// The bug: a run is submitted, a worker emits, and only then does the subscriber attach.
    #[tokio::test]
    async fn an_event_emitted_before_subscribe_run_is_received() {
        let sink = BroadcastSink::default();
        let run = RunId::new();
        let step = RunEvent::Step(StepEvent::new(
            "tool:c1",
            StepKind::Tool,
            "prepare_workspace",
            StepState::Running,
        ));
        sink.emit(run, "coder", step.clone()).await;
        let mut sub = sink.subscribe_run(run);
        assert_eq!(sub.recv().await, Some(step));
    }

    #[tokio::test]
    async fn the_replay_comes_first_then_live_events_once_each_in_order() {
        let sink = BroadcastSink::default();
        let run = RunId::new();
        sink.emit(run, "x", progress(1)).await;
        sink.emit(run, "x", progress(2)).await;
        let mut sub = sink.subscribe_run(run);
        sink.emit(run, "x", progress(3)).await;
        sink.emit(run, "x", progress(4)).await;
        assert_eq!(
            drain(&mut sub).await,
            (1..=4).map(progress).collect::<Vec<_>>()
        );
        // And nothing is left over.
        assert_eq!(drain(&mut sub).await, Vec::new());
    }

    #[tokio::test]
    async fn another_runs_events_are_not_replayed() {
        let sink = BroadcastSink::default();
        let (a, b) = (RunId::new(), RunId::new());
        sink.emit(a, "x", progress(1)).await;
        sink.emit(b, "x", progress(2)).await;
        let mut sub = sink.subscribe_run(b);
        assert_eq!(drain(&mut sub).await, vec![progress(2)]);
    }

    #[tokio::test]
    async fn the_replay_of_a_run_keeps_the_newest_events() {
        let sink = BroadcastSink::default();
        let run = RunId::new();
        let total = REPLAY_EVENTS_PER_RUN + 10;
        for n in 0..total {
            sink.emit(run, "x", progress(n)).await;
        }
        let mut sub = sink.subscribe_run(run);
        let got = drain(&mut sub).await;
        assert_eq!(got.len(), REPLAY_EVENTS_PER_RUN);
        assert_eq!(got.first(), Some(&progress(total - REPLAY_EVENTS_PER_RUN)));
        assert_eq!(got.last(), Some(&progress(total - 1)));
    }

    /// A run whose tool results are large must not pin the replay: the byte bound decides, and
    /// every run it keeps still has its newest event.
    #[tokio::test]
    async fn the_replay_is_bounded_by_bytes_and_keeps_each_runs_newest_event() {
        let sink = BroadcastSink::default();
        let runs: Vec<RunId> = (0..8).map(|_| RunId::new()).collect();
        let padding = "x".repeat(64 * 1024);
        let piece = |n: usize| RunEvent::Progress {
            message: format!("{n}:{padding}"),
        };
        for n in 0..REPLAY_EVENTS_PER_RUN {
            for run in &runs {
                sink.emit(*run, "x", piece(n)).await;
            }
        }

        let replay = sink.replay.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(replay.bytes > 0);
        assert!(
            replay.bytes <= REPLAY_MAX_BYTES,
            "{} bytes kept, over the {REPLAY_MAX_BYTES} bound",
            replay.bytes
        );
        for run in &runs {
            let events = replay
                .runs
                .get(run)
                .unwrap_or_else(|| panic!("the replay dropped run {run}"));
            // The byte bound decided, not the count bound: a run's 64 events do not fit.
            assert!(
                events.len() < REPLAY_EVENTS_PER_RUN,
                "{} events kept, so the count bound was the one that applied",
                events.len()
            );
            assert_eq!(
                events.back().map(|(_, event)| &event.event),
                Some(&piece(REPLAY_EVENTS_PER_RUN - 1))
            );
        }
    }

    #[tokio::test]
    async fn status_events_are_not_replayed_but_are_delivered_live() {
        let sink = BroadcastSink::default();
        let run = RunId::new();
        sink.emit(run, "x", status(RunStatus::Runnable)).await;
        sink.emit(run, "x", progress(1)).await;
        let mut sub = sink.subscribe_run(run);
        assert_eq!(drain(&mut sub).await, vec![progress(1)]);
        sink.emit(run, "x", status(RunStatus::Done)).await;
        assert_eq!(drain(&mut sub).await, vec![status(RunStatus::Done)]);
    }

    #[tokio::test]
    async fn a_run_taken_up_again_replays_only_its_new_turn() {
        // A turn that asks a question parks the run; the answer makes it runnable again. The
        // subscriber of the answer must not hear the question's words again (a text stream of
        // the last turn would be read as part of this one).
        let sink = BroadcastSink::default();
        let run = RunId::new();
        sink.emit(run, "x", progress(1)).await;
        sink.emit(run, "x", status(RunStatus::Parked)).await;
        sink.emit(run, "x", status(RunStatus::Runnable)).await;
        sink.emit(run, "x", progress(2)).await;
        let mut sub = sink.subscribe_run(run);
        assert_eq!(drain(&mut sub).await, vec![progress(2)]);
    }

    #[tokio::test]
    async fn any_status_drops_the_runs_replay() {
        for end in RunStatus::ALL {
            let sink = BroadcastSink::default();
            let run = RunId::new();
            sink.emit(run, "x", progress(1)).await;
            sink.emit(run, "x", status(end)).await;
            let mut sub = sink.subscribe_run(run);
            assert_eq!(drain(&mut sub).await, Vec::new(), "{end}");
        }
    }

    #[tokio::test]
    async fn an_artifact_is_not_replayed_because_the_run_record_has_it() {
        let sink = BroadcastSink::default();
        let run = RunId::new();
        sink.emit(
            run,
            "x",
            Artifact::new("a", None, serde_json::json!(1)).into(),
        )
        .await;
        let mut sub = sink.subscribe_run(run);
        assert_eq!(drain(&mut sub).await, Vec::new());
    }

    #[tokio::test]
    async fn the_feed_of_every_run_has_no_replay() {
        let sink = BroadcastSink::default();
        sink.emit(RunId::new(), "x", progress(1)).await;
        let mut all = sink.subscribe();
        assert!(all.try_recv().is_err());
    }

    fn sink_event(run: RunId, n: usize) -> SinkEvent {
        SinkEvent {
            run,
            agent: "x".into(),
            event: progress(n),
        }
    }

    /// The clock is a parameter of the replay, so the age bound is tested without waiting.
    #[test]
    fn an_event_older_than_the_age_bound_is_not_replayed() {
        let mut replay = Replay::new(ReplayBounds::DEFAULT);
        let run = RunId::new();
        let t0 = Instant::now();
        replay.record(t0, &sink_event(run, 1));
        replay.record(t0 + Duration::from_secs(20), &sink_event(run, 2));

        // Both are young at 29 s, only the second at 40 s, neither at 60 s.
        let at = |n| t0 + Duration::from_secs(n);
        assert_eq!(
            replay.snapshot(at(29), run),
            VecDeque::from([progress(1), progress(2)])
        );
        assert_eq!(replay.snapshot(at(40), run), VecDeque::from([progress(2)]));
        assert_eq!(replay.snapshot(at(60), run), VecDeque::new());
        // An empty run is forgotten, not kept as an empty buffer.
        assert!(replay.runs.is_empty());
    }

    #[test]
    fn a_new_event_prunes_the_old_ones_of_its_run_and_of_runs_gone_quiet() {
        let mut replay = Replay::new(ReplayBounds::DEFAULT);
        let (quiet, busy) = (RunId::new(), RunId::new());
        let t0 = Instant::now();
        replay.record(t0, &sink_event(quiet, 1));
        replay.record(t0, &sink_event(busy, 1));
        let later = t0 + REPLAY_MAX_AGE + Duration::from_secs(1);
        replay.record(later, &sink_event(busy, 2));
        assert_eq!(replay.runs[&busy].len(), 1);
        // A run that begins prunes the quiet one.
        replay.record(later, &sink_event(RunId::new(), 1));
        assert!(!replay.runs.contains_key(&quiet));
    }

    #[test]
    fn the_run_whose_newest_event_is_oldest_is_evicted_past_the_run_cap() {
        let mut replay = Replay::new(ReplayBounds {
            max_runs: 3,
            ..ReplayBounds::DEFAULT
        });
        let runs: Vec<RunId> = (0..4).map(|_| RunId::new()).collect();
        let t0 = Instant::now();
        for (n, run) in runs.iter().take(3).enumerate() {
            replay.record(t0 + Duration::from_millis(n as u64), &sink_event(*run, 0));
        }
        // The first run speaks again: now the second is the quietest.
        replay.record(t0 + Duration::from_millis(10), &sink_event(runs[0], 1));
        replay.record(t0 + Duration::from_millis(11), &sink_event(runs[3], 0));
        assert_eq!(replay.runs.len(), 3);
        assert!(!replay.runs.contains_key(&runs[1]));
        for kept in [0, 2, 3] {
            assert!(replay.runs.contains_key(&runs[kept]), "{kept}");
        }
    }
}
