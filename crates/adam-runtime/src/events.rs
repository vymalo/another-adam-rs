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

/// A named piece of output produced while a run works (a file, a report, a
/// structured result). A2A maps these to task artifacts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// Artifact name.
    pub name: String,
    /// Media type of `data`, if known.
    pub mime_type: Option<String>,
    /// The content.
    pub data: Value,
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
        /// Media type of `data`, if known.
        mime_type: Option<String>,
        /// The content.
        data: Value,
    },
}

impl From<Artifact> for RunEvent {
    fn from(a: Artifact) -> Self {
        Self::Artifact {
            name: a.name,
            mime_type: a.mime_type,
            data: a.data,
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
            },
        ];
        for e in events {
            let json = serde_json::to_value(&e).expect("serialize");
            assert_eq!(serde_json::from_value::<RunEvent>(json).expect("parse"), e);
        }
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
