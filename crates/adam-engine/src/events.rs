//! [`RunEvents`]: a run's events as a stream of [`EngineEvent`]s, bounded in memory whatever the
//! reader does, and the sink that keeps file bytes out of the ring.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Weak;
use std::task::{Context, Poll};
use std::time::Duration;

use adam_core::RunId;
use adam_runtime::{BroadcastSink, EventSink, Received, RunEvent, RunSubscription};
use async_trait::async_trait;
use futures::Stream;
use futures::stream::{BoxStream, StreamExt};

use crate::engine::Shared;
use crate::{Engine, EngineError, RunInfo};

/// How long a stream waits for an event before it reads the run from the store anyway: the events
/// are a latency aid, and the store decides when a run has ended.
const POLL: Duration = Duration::from_secs(2);

/// What a [`RunEvents`] stream says about its run. Every event is a prompt; the store is the
/// truth, and [`Snapshot`](Self::Snapshot) and [`Ended`](Self::Ended) are read from it.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum EngineEvent {
    /// An event of the run as the worker emitted it: a status, a step, a piece of text, a usage
    /// report, an artifact (a file's `bytes` are empty: read the run for them). Live events are
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
}

/// The events of one run, from [`Engine::events`]: a [`Stream`] of
/// `Result<`[`EngineEvent`]`, `[`EngineError`]`>`.
///
/// * It ends after [`EngineEvent::Ended`], after an error (a failed read of the store), and when
///   the engine is gone (the last [`Engine`] dropped).
/// * It is bounded in memory whatever the reader does: the engine's ring holds
///   [`Limits::event_capacity`](crate::Limits::event_capacity) events, shared by every reader,
///   and a reader that does not keep up loses the oldest instead of holding more. A stream holds
///   besides only the run's recent events it started with (at most
///   [`REPLAY_EVENTS_PER_RUN`](adam_runtime::REPLAY_EVENTS_PER_RUN)) and at most two items.
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
        info: RunInfo,
    ) -> Self {
        let mut follow = Follow {
            run,
            subscription,
            engine,
            queue: VecDeque::with_capacity(2),
            ended: false,
        };
        if info.state.is_terminal() {
            follow.end(EngineEvent::Ended(info));
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
    /// What is ready to be handed out: at most two items (`Live` then `Ended`, `Behind` then
    /// `Snapshot` or `Ended`).
    queue: VecDeque<Result<EngineEvent, EngineError>>,
    /// Nothing more is received once the queue is empty.
    ended: bool,
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
            match tokio::time::timeout(POLL, self.subscription.recv_or_lag()).await {
                Ok(Some(Received::Event(event))) => {
                    let status = matches!(event, RunEvent::Status { .. });
                    self.queue.push_back(Ok(EngineEvent::Live(event)));
                    // A status is a prompt to read the record: it says whether the run ended.
                    if status {
                        self.read(false).await;
                    }
                }
                Ok(Some(Received::Lagged(skipped))) => {
                    self.queue.push_back(Ok(EngineEvent::Behind { skipped }));
                    self.read(true).await;
                }
                // The sink is gone: so is the engine.
                Ok(None) => self.ended = true,
                // Nothing heard for a while: the store says whether the run ended meanwhile.
                Err(_) => self.read(false).await,
            }
        }
    }

    /// Read the run from the store: [`EngineEvent::Ended`] once it has ended, and, with
    /// `snapshot`, [`EngineEvent::Snapshot`] while it has not.
    async fn read(&mut self, snapshot: bool) {
        let Some(engine) = Engine::from_weak(&self.engine) else {
            self.ended = true;
            return;
        };
        match engine.get(self.run).await {
            Ok(Some(info)) if info.state.is_terminal() => self.end(EngineEvent::Ended(info)),
            Ok(Some(info)) => {
                if snapshot {
                    self.queue.push_back(Ok(EngineEvent::Snapshot(info)));
                }
            }
            Ok(None) => {
                self.queue
                    .push_back(Err(EngineError::NotFound(format!("run {}", self.run))));
                self.ended = true;
            }
            Err(error) => {
                self.queue.push_back(Err(error));
                self.ended = true;
            }
        }
    }

    fn end(&mut self, last: EngineEvent) {
        self.queue.push_back(Ok(last));
        self.ended = true;
    }
}

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

/// `event` with a file artifact's bytes emptied; every other event as it is.
fn without_file_bytes(mut event: RunEvent) -> RunEvent {
    if let RunEvent::Artifact {
        file: Some(file), ..
    } = &mut event
    {
        drop(std::mem::take(&mut file.bytes));
    }
    event
}

#[cfg(test)]
mod tests {
    use adam_runtime::Artifact;
    use serde_json::json;

    use super::*;

    /// A file artifact reaches a reader with its name, media type and filename, and no bytes; a
    /// JSON artifact and every other event reach it as they were emitted.
    #[tokio::test]
    async fn the_sink_keeps_file_bytes_out_of_the_ring() {
        let ring = BroadcastSink::new(8);
        let sink = EngineSink(ring.clone());
        let run = RunId::new();
        let mut reader = ring.subscribe_run(run);

        let file = Artifact::file("logo", "image/svg+xml", "logo.svg", vec![7; 64 * 1024])
            .expect("a small file");
        let surface = Artifact::new("surface", None, json!({"a": [1, 2, 3]}));
        let progress = RunEvent::Progress {
            message: "working".into(),
        };
        for event in [
            RunEvent::from(file),
            RunEvent::from(surface.clone()),
            progress.clone(),
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
                assert_eq!(data, serde_json::Value::Null);
                assert_eq!(file.filename, "logo.svg");
                assert!(
                    file.bytes.is_empty(),
                    "{} bytes in the ring",
                    file.bytes.len()
                );
            }
            other => panic!("not a file artifact: {other:?}"),
        }
        assert_eq!(reader.recv().await, Some(RunEvent::from(surface)));
        assert_eq!(reader.recv().await, Some(progress));
    }
}
