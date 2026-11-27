//! [`RunEvents`]: a run's events as a stream of [`EngineEvent`]s, bounded in memory whatever the
//! reader does, and the sink that keeps file bytes out of the ring.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Weak;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use adam_core::RunId;
use adam_runtime::{BroadcastSink, EventSink, Received, RunEvent, RunSubscription, blob_id};
use async_trait::async_trait;
use futures::Stream;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tokio::sync::watch;

use crate::engine::{Ended, Shared};
use crate::{ApprovalRequest, Engine, EngineError, RunInfo};

/// How long a stream waits for an event before it reads the run from the store anyway: the events
/// are a latency aid, and the store decides when a run has ended.
const POLL: Duration = Duration::from_secs(2);

/// How often a stream carries a [`EngineEvent::Resources`] while its run works: ADR 0065 decision
/// 9 says at most every two seconds.
const REPORT_INTERVAL: Duration = Duration::from_secs(2);

/// What a [`RunEvents`] stream says about its run. Every event is a prompt; the store is the
/// truth, and [`Snapshot`](Self::Snapshot) and [`Ended`](Self::Ended) are read from it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// An event of the run as the worker emitted it: a status, a step, a piece of text, a usage
    /// report, an artifact (a file's bytes are withheld: [`WithheldFile::of`] says how many and
    /// their SHA-256). Live events are
    /// best effort: a reader that falls behind loses the oldest, and is told with
    /// [`Behind`](Self::Behind).
    Live(RunEvent),
    /// The reader fell behind and lost live events: `skipped` is how many events of the engine,
    /// every run's, it missed, so at most that many of this run's. A
    /// [`Snapshot`](Self::Snapshot) or [`Ended`](Self::Ended) follows at once.
    Behind {
        /// How many events were lost.
        skipped: u64,
    },
    /// The run as the store has it, read after [`Behind`](Self::Behind). The live events that
    /// follow may have been emitted before the read: take a text piece by its `offset` and a step
    /// by its `id`.
    Snapshot(RunInfo),
    /// The run has ended (`Done`, `Failed` or `Cancelled`), as the store has it: the last item of
    /// the stream.
    Ended(RunInfo),
    /// What the process's pools hold and what the host's device says
    /// ([ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
    /// decision 9), while the run works: at most every two seconds. A host that shows the report
    /// on request reads [`Engine::resources`](crate::Engine::resources) instead.
    Resources(crate::ResourceReport),
    /// A hook asked the person to approve a tool call, and the run waits
    /// ([`RunState::AwaitingApproval`](crate::RunState::AwaitingApproval)): what the host renders
    /// with a yes and a no, which [`Engine::approve`](crate::Engine::approve) answers. Read from
    /// the store, once per approval in a stream (a stream opened on a run that already waits starts
    /// with it), so a reader that fell behind still hears it.
    ApprovalAsked(ApprovalRequest),
    /// The approval of `call_id` was answered, as the store has it: `approved` when the call ran in
    /// its step `tool:<call id>:approved`, `false` when it was answered without it (refused) and
    /// the run went on. Said once for each approval the stream heard of: one it said with
    /// [`ApprovalAsked`](Self::ApprovalAsked), or one a reader that reads only after the answer
    /// finds in the run's live events (the agent's `input_required` with `"approval": true`), which
    /// it hears answered without an `ApprovalAsked`: an approval that no longer waits is not
    /// offered. A run that ends before the answer (a cancel) says none.
    ApprovalAnswered {
        /// The call that was asked about.
        call_id: String,
        /// Whether the person approved it.
        approved: bool,
    },
}

/// The events of one run, from [`Engine::events`]: a [`Stream`] of
/// `Result<`[`EngineEvent`]`, `[`EngineError`]`>`.
///
/// * While the run works it carries an [`EngineEvent::Resources`] at most every two seconds
///   (ADR 0065 decision 9).
/// * It ends after [`EngineEvent::Ended`], or after an error: [`EngineError::Stopped`] once the
///   worker has stopped with the run unfinished ([`Engine::shutdown`], the last [`Engine`]
///   dropped, or the worker failed: nothing would step the run), or the store's error when a read
///   fails.
/// * It says a hook's approval as [`EngineEvent::ApprovalAsked`], and its answer as
///   [`EngineEvent::ApprovalAnswered`], both read from the store.
/// * It is bounded in memory whatever the reader does: the engine's ring holds
///   [`Limits::event_capacity`](crate::Limits::event_capacity) events, shared by every reader,
///   and a reader that does not keep up loses the oldest instead of holding more. A stream holds
///   besides only the run's recent events it started with (at most
///   [`REPLAY_EVENTS_PER_RUN`](adam_runtime::REPLAY_EVENTS_PER_RUN)) and at most four items.
/// * It does not keep the engine alive: drop every [`Engine`] and the worker stops, whatever
///   streams are open.
pub struct RunEvents {
    run: RunId,
    inner: BoxStream<'static, Result<EngineEvent, EngineError>>,
}

impl std::fmt::Debug for RunEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunEvents")
            .field("run", &self.run)
            .finish_non_exhaustive()
    }
}

impl RunEvents {
    /// `run`'s events from `subscription`, which was made before `info` was read: an event emitted
    /// after the read is in it.
    pub(crate) fn new(
        run: RunId,
        subscription: RunSubscription,
        engine: Weak<Shared>,
        worker: watch::Receiver<Option<Ended>>,
        info: RunInfo,
    ) -> Self {
        let mut follow = Follow {
            run,
            subscription,
            engine,
            worker,
            queue: VecDeque::with_capacity(4),
            ended: false,
            report_at: Instant::now() + REPORT_INTERVAL,
            asked: None,
            owed: None,
        };
        if info.state.is_terminal() {
            follow.end(EngineEvent::Ended(info));
        } else if let Some(approval) = info.approval {
            follow.asked = Some(approval.call_id.clone());
            follow.owed = Some(approval.call_id.clone());
            follow
                .queue
                .push_back(Ok(EngineEvent::ApprovalAsked(approval)));
        }
        let inner = futures::stream::unfold(follow, |mut follow| async move {
            let item = follow.next().await?;
            Some((item, follow))
        })
        .boxed();
        Self { run, inner }
    }

    /// The run these are the events of.
    pub fn run(&self) -> RunId {
        self.run
    }
}

impl Stream for RunEvents {
    type Item = Result<EngineEvent, EngineError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

/// The state of one stream.
struct Follow {
    run: RunId,
    subscription: RunSubscription,
    engine: Weak<Shared>,
    /// Says when the worker has stopped: then nothing steps the run any more.
    worker: watch::Receiver<Option<Ended>>,
    /// What is ready to be handed out: at most four items (`Live` or `Behind`, then an approval's
    /// answer, a new approval, and `Snapshot` or `Ended`).
    queue: VecDeque<Result<EngineEvent, EngineError>>,
    /// Nothing more is received once the queue is empty.
    ended: bool,
    /// When the next [`EngineEvent::Resources`] may be handed out: a report is sent at most every
    /// [`REPORT_INTERVAL`], not on every live event of a busy run.
    report_at: Instant,
    /// The call whose approval this stream said ([`EngineEvent::ApprovalAsked`]) while it waited.
    asked: Option<String>,
    /// The call whose answer this stream owes ([`EngineEvent::ApprovalAnswered`]): one it said, or
    /// one the agent's `input_required` event named, which a reader that reads late may find
    /// answered already.
    owed: Option<String>,
}

impl Follow {
    async fn next(&mut self) -> Option<Result<EngineEvent, EngineError>> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Some(item);
            }
            if self.ended {
                return None;
            }
            let now = Instant::now();
            if now >= self.report_at {
                self.report_at = now + REPORT_INTERVAL;
                match Engine::from_weak(&self.engine) {
                    Some(engine) => return Some(Ok(EngineEvent::Resources(engine.resources()))),
                    None => {
                        self.fail(EngineError::Stopped);
                        continue;
                    }
                }
            }
            // The events the ring holds first: the last of them may be the run's end.
            let heard = tokio::select! {
                biased;
                received = self.subscription.recv_or_lag() => Heard::Received(received),
                _ = self.worker.wait_for(Option::is_some) => Heard::WorkerStopped,
                () = tokio::time::sleep(POLL) => Heard::Nothing,
            };
            match heard {
                Heard::Received(Some(Received::Event(event))) => {
                    let status = matches!(event, RunEvent::Status { .. });
                    let approval = approval_named(&event);
                    // The end of the owed call's step: its approved step ran (or it was refused).
                    let approval_heard = approval.is_some()
                        || self
                            .owed
                            .as_deref()
                            .is_some_and(|call| ends_the_step_of(&event, call));
                    self.queue.push_back(Ok(EngineEvent::Live(event)));
                    if let Some(call) = approval {
                        self.owed = Some(call);
                    }
                    // A status is a prompt to read the record: it says whether the run ended. So is
                    // an approval asked, and the end of its call's step: the store says whether it
                    // still waits, and how it was answered.
                    if status || approval_heard {
                        self.read(false).await;
                    }
                }
                Heard::Received(Some(Received::Lagged(skipped))) => {
                    self.queue.push_back(Ok(EngineEvent::Behind { skipped }));
                    self.read(true).await;
                }
                // The sink is gone: so is the engine.
                Heard::Received(None) => self.fail(EngineError::Stopped),
                // The step in flight committed before the worker stopped: the run may have ended.
                Heard::WorkerStopped => {
                    self.read(false).await;
                    if !self.ended {
                        self.fail(EngineError::Stopped);
                    }
                }
                // Nothing heard for a while: the store says whether the run ended meanwhile.
                Heard::Nothing => self.read(false).await,
            }
        }
    }

    /// Read the run from the store: [`EngineEvent::Ended`] once it has ended, and, with
    /// `snapshot`, [`EngineEvent::Snapshot`] while it has not.
    async fn read(&mut self, snapshot: bool) {
        let Some(engine) = Engine::from_weak(&self.engine) else {
            self.fail(EngineError::Stopped);
            return;
        };
        match engine.get(self.run).await {
            Ok(Some(info)) => {
                if let Err(error) = self.approvals(&engine, &info).await {
                    self.fail(error);
                } else if info.state.is_terminal() {
                    self.end(EngineEvent::Ended(info));
                } else if snapshot {
                    self.queue.push_back(Ok(EngineEvent::Snapshot(info)));
                }
            }
            Ok(None) => self.fail(EngineError::NotFound(format!("run {}", self.run))),
            Err(error) => self.fail(error),
        }
    }

    /// Say what the store shows of the run's approvals since the last read: the answer this stream
    /// owes, once the store has it, then an approval the run waits for that it has not said.
    async fn approvals(&mut self, engine: &Engine, info: &RunInfo) -> Result<(), EngineError> {
        let waits_for = info.approval.as_ref().map(|a| a.call_id.as_str());
        if let Some(owed) = self.owed.clone()
            && waits_for != Some(owed.as_str())
        {
            match engine.approval_outcome(self.run, &owed).await? {
                Some(approved) => {
                    self.queue.push_back(Ok(EngineEvent::ApprovalAnswered {
                        call_id: owed,
                        approved,
                    }));
                    self.owed = None;
                    self.asked = None;
                }
                // Never answered: the run ended first (a cancel).
                None if info.state.is_terminal() => {
                    self.owed = None;
                    self.asked = None;
                }
                // Not committed yet, or the store moved between the two reads: a later read says it.
                None => {}
            }
        }
        if let Some(approval) = &info.approval
            && self.asked.as_deref() != Some(approval.call_id.as_str())
        {
            self.asked = Some(approval.call_id.clone());
            self.owed = Some(approval.call_id.clone());
            self.queue
                .push_back(Ok(EngineEvent::ApprovalAsked(approval.clone())));
        }
        Ok(())
    }

    fn end(&mut self, last: EngineEvent) {
        self.queue.push_back(Ok(last));
        self.ended = true;
    }

    fn fail(&mut self, error: EngineError) {
        self.queue.push_back(Err(error));
        self.ended = true;
    }
}

/// The call `event` says the agent asked the person to approve: its `input_required` event with
/// `"approval": true` (ADR 0050 slice H8), emitted before the park is committed.
fn approval_named(event: &RunEvent) -> Option<String> {
    let RunEvent::Custom { kind, payload } = event else {
        return None;
    };
    if kind != "input_required" || payload.get("approval") != Some(&serde_json::Value::Bool(true)) {
        return None;
    }
    payload
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Whether `event` ends the step of the call `call` (`tool:<call>`, completed or failed).
fn ends_the_step_of(event: &RunEvent, call: &str) -> bool {
    matches!(event, RunEvent::Step(step)
        if matches!(step.state, adam_runtime::StepState::Completed | adam_runtime::StepState::Failed)
            && step.id.strip_prefix("tool:") == Some(call))
}

/// What a stream heard while it waited.
#[allow(
    clippy::large_enum_variant,
    reason = "one at a time, on the stack of the stream's wait; a box would allocate per event"
)]
enum Heard {
    Received(Option<Received>),
    WorkerStopped,
    Nothing,
}

/// What an engine's event says of a file whose bytes it withholds: their length and their SHA-256,
/// as the file is kept with the run. Every file artifact of an engine's events is one, an empty
/// file included (`len` 0), so a host never takes a withheld file for an empty one.
///
/// In the event, the artifact's `data` (`null` for a file in the runtime's own events) is
/// `{"withheld": {"bytes": <len>, "sha256": "<64 hex digits>"}}`, and the file's `bytes` are
/// empty.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WithheldFile {
    /// How many bytes the file has.
    pub len: u64,
    /// Their SHA-256 as 64 lower-case hexadecimal digits: what `Artifact::file_sha256` says of the
    /// file.
    pub sha256: String,
}

impl WithheldFile {
    /// The file `event` withholds: `Some` for a file artifact of an engine's event, `None` for any
    /// other event.
    pub fn of(event: &RunEvent) -> Option<Self> {
        let RunEvent::Artifact {
            data,
            file: Some(_),
            ..
        } = event
        else {
            return None;
        };
        let withheld = data.get(WITHHELD)?;
        Some(Self {
            len: withheld.get("bytes")?.as_u64()?,
            sha256: withheld.get("sha256")?.as_str()?.to_owned(),
        })
    }
}

/// The member of a stripped file artifact's `data` that says what was withheld.
const WITHHELD: &str = "withheld";

/// The engine's event sink: every event into the ring of `BroadcastSink`, a file artifact's bytes
/// taken out first. The file is durable with the run; the ring holds what every reader shares, and
/// a file can be megabytes.
#[derive(Clone, Debug)]
pub(crate) struct EngineSink(pub(crate) BroadcastSink);

#[async_trait]
impl EventSink for EngineSink {
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent) {
        self.0.emit(run, agent, without_file_bytes(event)).await;
    }
}

/// `event` with a file artifact's bytes taken out and said instead ([`WithheldFile`]); every other
/// event as it is.
fn without_file_bytes(mut event: RunEvent) -> RunEvent {
    if let RunEvent::Artifact {
        data,
        file: Some(file),
        ..
    } = &mut event
    {
        let bytes = std::mem::take(&mut file.bytes);
        *data = json!({ WITHHELD: { "bytes": bytes.len(), "sha256": blob_id(&bytes).as_str() } });
    }
    event
}

#[cfg(test)]
mod tests {
    use adam_runtime::Artifact;
    use serde_json::json;

    use super::*;

    /// A file artifact reaches a reader with its name, media type and filename, its length and
    /// digest, and no bytes; a JSON artifact and every other event reach it as they were emitted.
    #[tokio::test]
    async fn the_sink_keeps_file_bytes_out_of_the_ring() {
        let ring = BroadcastSink::new(8);
        let sink = EngineSink(ring.clone());
        let run = RunId::new();
        let mut reader = ring.subscribe_run(run);

        let file = Artifact::file("logo", "image/svg+xml", "logo.svg", vec![7; 64 * 1024])
            .expect("a small file");
        let sha256 = file.file_sha256().expect("a file");
        let empty = Artifact::file("none", "text/plain", "empty.txt", Vec::new()).expect("file");
        let empty_sha256 = empty.file_sha256().expect("a file");
        let surface = Artifact::new("surface", None, json!({"a": [1, 2, 3]}));
        let progress = RunEvent::Progress {
            message: "working".into(),
        };
        for event in [
            RunEvent::from(file),
            RunEvent::from(surface.clone()),
            progress.clone(),
            RunEvent::from(empty),
        ] {
            sink.emit(run, "chat", event).await;
        }

        match reader.recv().await.expect("the file's event") {
            RunEvent::Artifact {
                name,
                mime_type,
                data,
                file: Some(file),
                ..
            } => {
                assert_eq!(name, "logo");
                assert_eq!(mime_type.as_deref(), Some("image/svg+xml"));
                assert_eq!(file.filename, "logo.svg");
                assert!(
                    file.bytes.is_empty(),
                    "{} bytes in the ring",
                    file.bytes.len()
                );
                assert_eq!(
                    data,
                    json!({"withheld": {"bytes": 64 * 1024, "sha256": sha256}})
                );
            }
            other => panic!("not a file artifact: {other:?}"),
        }
        assert_eq!(reader.recv().await, Some(RunEvent::from(surface)));
        assert_eq!(reader.recv().await, Some(progress));
        assert_eq!(
            reader.recv().await.as_ref().and_then(WithheldFile::of),
            Some(WithheldFile {
                len: 0,
                sha256: empty_sha256,
            }),
            "an empty file says it is one"
        );
    }

    /// What a host reads of a withheld file, and of every other event.
    #[test]
    fn a_withheld_file_says_its_length_and_digest() {
        let file = Artifact::file("f", "text/plain", "f.txt", b"hello".to_vec()).expect("file");
        let sha256 = file.file_sha256().expect("a file");
        let stripped = without_file_bytes(RunEvent::from(file.clone()));
        assert_eq!(
            WithheldFile::of(&stripped),
            Some(WithheldFile { len: 5, sha256 })
        );
        // The runtime's own event withholds nothing: it is not read as a withheld file.
        assert_eq!(WithheldFile::of(&RunEvent::from(file)), None);
        let json = Artifact::new("r", None, json!({"withheld": {"bytes": 1, "sha256": "x"}}));
        assert_eq!(
            WithheldFile::of(&RunEvent::from(json)),
            None,
            "a JSON artifact"
        );
        let progress = RunEvent::Progress {
            message: "p".into(),
        };
        assert_eq!(WithheldFile::of(&progress), None);
    }
}
