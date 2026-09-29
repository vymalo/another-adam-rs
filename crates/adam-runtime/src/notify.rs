//! The [`Notifier`] port: cross-process wake-up and cancel.
//!
//! Workers find due runs by polling the store (`poll_interval`), and a step
//! running in one process learns of a cancel issued in another by reading its
//! run once per poll. That is correct on its own, and slow: up to one poll
//! interval of latency for a run started or resumed by another process, and
//! for a cancel. A [`Notifier`] removes the latency and nothing else.
//!
//! **A signal is a hint, never the truth.** It may be lost, duplicated or
//! late; the store's compare-and-swap, the lease and the polling loop stay
//! what make the runtime correct, and polling stays on with a notifier
//! configured. A consumer that receives a signal re-reads the store rather
//! than believing the signal.
//!
//! [`LocalNotifier`] is the in-process implementation, for tests with several
//! runtimes in one process and for a process that is both front and worker.
//! Across processes, use an adapter crate such as `adam-notify-postgres`.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use adam_core::RunId;

/// Something worth telling other processes about, cheaply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Signal {
    /// A run of `agent` became runnable now (started, or resumed by a message).
    /// A worker that can step `agent` polls at once instead of waiting.
    Runnable {
        /// The run.
        run: RunId,
        /// Name of its agent.
        agent: String,
    },
    /// A run was finished by a cancel. A process stepping it fires the step's
    /// [`CancelToken`](crate::CancelToken) at once.
    Finished {
        /// The run.
        run: RunId,
    },
}

/// What a [`Notifier`] subscriber receives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// A signal from some process (this one's included).
    Signal(Signal),
    /// Signals may have been lost (the subscriber lagged, or the connection
    /// behind the notifier dropped): re-check everything.
    Resync,
}

/// Cross-process wake-up and cancel signals, to make workers of other
/// processes react at once instead of at their next poll.
///
/// Best effort by contract: a signal is a hint that may be lost, duplicated or
/// late, and the store's compare-and-swap and the workers' polling stay what
/// makes the runtime correct. A consumer re-reads the store rather than
/// believing a signal.
#[async_trait]
pub trait Notifier: Send + Sync + 'static {
    /// Tell every subscriber, of every process, about `signal`. Returns at
    /// once and never fails: an implementation that cannot deliver drops the
    /// signal (and may log it).
    async fn publish(&self, signal: Signal);

    /// The signals of every process, this one's included, from now on. The
    /// subscription exists as soon as this returns, so a signal published
    /// afterwards is not missed. A subscriber too slow to keep up gets
    /// [`Delivery::Resync`] in place of what it missed. The stream ends when
    /// the notifier is gone.
    fn subscribe(&self) -> BoxStream<'static, Delivery>;
}

/// Shared notifier handle.
pub type DynNotifier = Arc<dyn Notifier>;

#[async_trait]
impl<T: Notifier + ?Sized> Notifier for Arc<T> {
    async fn publish(&self, signal: Signal) {
        (**self).publish(signal).await;
    }

    fn subscribe(&self) -> BoxStream<'static, Delivery> {
        (**self).subscribe()
    }
}

/// A [`Notifier`] inside one process, over a tokio broadcast channel. Clones
/// share the channel.
///
/// It also serves as the fan-out inside adapters: an adapter that receives
/// signals from elsewhere calls [`publish`](Notifier::publish) on one, and
/// [`resync`](Self::resync) when it may have missed some.
#[derive(Clone, Debug)]
pub struct LocalNotifier {
    tx: broadcast::Sender<Delivery>,
}

impl LocalNotifier {
    /// A notifier whose subscribers may lag by up to `capacity` deliveries
    /// before they get [`Delivery::Resync`].
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: broadcast::channel(capacity.max(1)).0,
        }
    }

    /// Tell every subscriber that signals may have been lost.
    pub fn resync(&self) {
        // No subscribers is fine.
        let _ = self.tx.send(Delivery::Resync);
    }
}

impl Default for LocalNotifier {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[async_trait]
impl Notifier for LocalNotifier {
    async fn publish(&self, signal: Signal) {
        let _ = self.tx.send(Delivery::Signal(signal));
    }

    fn subscribe(&self) -> BoxStream<'static, Delivery> {
        futures::stream::unfold(self.tx.subscribe(), |mut rx| async move {
            match rx.recv().await {
                Ok(delivery) => Some((delivery, rx)),
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "notifier subscriber lagged, resyncing");
                    Some((Delivery::Resync, rx))
                }
                Err(broadcast::error::RecvError::Closed) => None,
            }
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn runnable(agent: &str) -> Signal {
        Signal::Runnable {
            run: RunId::new(),
            agent: agent.to_owned(),
        }
    }

    async fn next(s: &mut BoxStream<'static, Delivery>) -> Delivery {
        tokio::time::timeout(Duration::from_secs(5), s.next())
            .await
            .expect("a delivery in time")
            .expect("the stream is open")
    }

    #[test]
    fn signals_roundtrip_through_json() {
        for signal in [runnable("a"), Signal::Finished { run: RunId::new() }] {
            let json = serde_json::to_value(&signal).expect("serialize");
            assert!(json["type"].is_string(), "{json}");
            assert_eq!(
                serde_json::from_value::<Signal>(json).expect("parse"),
                signal
            );
        }
    }

    #[tokio::test]
    async fn every_subscriber_gets_every_signal_in_order() {
        let n = LocalNotifier::default();
        let (mut a, mut b) = (n.subscribe(), n.subscribe());
        let sent = [
            runnable("x"),
            Signal::Finished { run: RunId::new() },
            runnable("y"),
        ];
        for s in &sent {
            n.publish(s.clone()).await;
        }
        for stream in [&mut a, &mut b] {
            for s in &sent {
                assert_eq!(next(stream).await, Delivery::Signal(s.clone()));
            }
        }
    }

    #[tokio::test]
    async fn subscribing_sees_only_what_comes_after() {
        let n = LocalNotifier::default();
        n.publish(runnable("before")).await;
        let mut s = n.subscribe();
        let after = runnable("after");
        n.publish(after.clone()).await;
        assert_eq!(next(&mut s).await, Delivery::Signal(after));
    }

    #[tokio::test]
    async fn a_lagging_subscriber_gets_a_resync_then_carries_on() {
        let n = LocalNotifier::new(2);
        let mut s = n.subscribe();
        for _ in 0..8 {
            n.publish(runnable("x")).await;
        }
        assert_eq!(next(&mut s).await, Delivery::Resync);
        // Whatever is still buffered, then live again.
        let last = runnable("last");
        while tokio::time::timeout(Duration::from_millis(20), s.next())
            .await
            .is_ok()
        {}
        n.publish(last.clone()).await;
        assert_eq!(next(&mut s).await, Delivery::Signal(last));
    }

    #[tokio::test]
    async fn resync_reaches_subscribers() {
        let n = LocalNotifier::default();
        let mut s = n.subscribe();
        n.resync();
        assert_eq!(next(&mut s).await, Delivery::Resync);
    }

    #[tokio::test]
    async fn the_stream_ends_with_the_notifier() {
        let n = LocalNotifier::default();
        let mut s = n.subscribe();
        drop(n);
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn publishing_without_subscribers_is_fine() {
        LocalNotifier::default().publish(runnable("x")).await;
    }
}
