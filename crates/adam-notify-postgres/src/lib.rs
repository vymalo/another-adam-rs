//! Run events and wake-up/cancel signals across processes, over PostgreSQL
//! `LISTEN`/`NOTIFY`.
//!
//! One [`PgNotify`] per process gives that process two things over the same
//! database its [`Store`](adam_core::Store) already uses:
//!
//! * [`PgEventSink`], an [`EventSink`]: what a worker's steps emit reaches the
//!   [`BroadcastSink`] of every other process (a front serving SSE), so a
//!   subscriber sees `Progress` from a run another process is stepping without
//!   waiting for its next durable poll.
//! * [`PgNotifier`], a [`Notifier`]: [`Signal::Runnable`] wakes idle workers of
//!   other processes, [`Signal::Finished`] fires the cancel token of a step
//!   running elsewhere.
//!
//! **Everything here is a latency optimisation.** `NOTIFY` is at most once and
//! not durable: a notification sent while no one listens, or while the
//! listener reconnects, is gone. Correctness stays with the store's
//! compare-and-swap, the workers' polling and the durable
//! [`Runtime::view`](adam_runtime::Runtime::view); nothing may depend on a
//! notification arriving. A listener that reconnects tells its subscribers
//! ([`Delivery::Resync`]) so they look again at once.
//!
//! # Channels and payloads
//!
//! Two channels, `{prefix}events` and `{prefix}signals` (prefix `adam_` by
//! default, [`PgNotify::with_channel_prefix`]). Payloads are JSON with a
//! version, `"v": 1`; one of another version is ignored.
//!
//! ```text
//! events:  {"v":1,"o":"<origin uuid>","run":"<run id>","agent":"<name>","event":{"type":"progress",...}}
//! signals: {"v":1,"type":"runnable","run":"<run id>","agent":"<name>"}
//!          {"v":1,"type":"finished","run":"<run id>"}
//! ```
//!
//! `NOTIFY` rejects a payload of 8000 bytes or more, so nothing over
//! [`MAX_PAYLOAD_BYTES`] is sent. An oversize `Status` keeps going with its
//! `detail` cut on a character boundary; an oversize `Step` keeps going without its `input` and
//! `output` (up to 4 KiB and 8 KiB: a courtesy, the step and its state are what matter); any other
//! oversize event is dropped (its artifact is still in the durable run). There is no events table and no
//! sequence number: events are not replayable (the `BroadcastSink` a process delivers into keeps the
//! last seconds of each run for a late subscriber, which covers a race, not a gap).
//!
//! # Running
//!
//! [`PgNotify::run`] drives one publisher (a queue of 1024 drained with
//! `SELECT pg_notify(..)` on the pool, one at a time, in order) and one
//! listener, until `stop` resolves. **The listener holds one connection of the
//! pool for as long as `run` runs**, and `LISTEN` needs a session, so connect
//! directly or through a session-mode pooler, not a transaction-mode one. The
//! publisher never blocks the caller: a full queue drops the item.
//!
//! Its lifecycle, and the guarantees, are in the [crate README](https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-notify-postgres/README.md)
//! and `docs/architecture.md`.

#![warn(missing_docs)]

mod error;
mod wire;

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use adam_core::RunId;
use adam_runtime::{BroadcastSink, Delivery, EventSink, LocalNotifier, Notifier, RunEvent, Signal};
use async_trait::async_trait;
use futures::stream::BoxStream;
use sqlx::postgres::{PgListener, PgPool};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::wire::{Encoded, EventIn, SignalIn, WIRE_VERSION, encode_event, encode_signal};

pub use crate::error::NotifyError;
pub use crate::wire::MAX_PAYLOAD_BYTES;

/// How long [`PgNotify::run`] keeps sending the items still queued when its
/// `stop` resolves.
pub const DRAIN_ON_STOP: Duration = Duration::from_secs(2);

/// Channel-name prefix unless [`PgNotify::with_channel_prefix`] says otherwise.
const DEFAULT_PREFIX: &str = "adam_";
/// Items waiting to be sent; more are dropped.
const QUEUE_CAPACITY: usize = 1024;
/// First wait after a failed connect, doubled up to [`BACKOFF_MAX`].
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// A listener session that lived this long counts as healthy: the backoff
/// starts over after it.
const HEALTHY_AFTER: Duration = Duration::from_secs(5);

/// One process's connection to the notification channels. Cheap to clone;
/// clones share everything.
#[derive(Clone, Debug)]
pub struct PgNotify {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Outgoing {
    signals: bool,
    payload: String,
}

#[derive(Debug)]
struct Inner {
    pool: PgPool,
    /// Where events of other processes are delivered.
    local: BroadcastSink,
    /// Tells our own events from those of others.
    origin: Uuid,
    events_channel: String,
    signals_channel: String,
    queue: mpsc::Sender<Outgoing>,
    /// The receiving end of `queue`, held by `run` while it runs.
    outbox: Mutex<Option<mpsc::Receiver<Outgoing>>>,
    /// Fan-out of the signals (and resyncs) the listener receives.
    fanout: LocalNotifier,
    listening: watch::Sender<bool>,
    dropped: AtomicU64,
    failed: AtomicU64,
}

/// Whether the `n`th occurrence of something noisy deserves a log line: the
/// first, then every thousandth.
fn should_log(n: u64) -> bool {
    n == 1 || n.is_multiple_of(1000)
}

fn valid_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 40
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !prefix.starts_with(|c: char| c.is_ascii_digit())
}

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_MAX)
}

impl PgNotify {
    /// Notifications over `pool`, delivering the events of other processes to
    /// `local` (the sink the process's A2A backend subscribes to). Call
    /// [`run`](Self::run) for anything to flow.
    pub fn new(pool: PgPool, local: BroadcastSink) -> Self {
        Self::build(pool, local, DEFAULT_PREFIX, Uuid::new_v4())
    }

    /// Use channels `{prefix}events` and `{prefix}signals` instead of the
    /// default prefix `adam_`, e.g. to host several isolated environments in
    /// one database. The rule is the store's table prefix: `[a-z0-9_]`, at most
    /// 40 characters, not starting with a digit. Call it before
    /// [`event_sink`](Self::event_sink) and [`notifier`](Self::notifier):
    /// handles taken earlier stay on the old channels.
    pub fn with_channel_prefix(self, prefix: &str) -> Result<Self, NotifyError> {
        if !valid_prefix(prefix) {
            return Err(NotifyError::InvalidPrefix(prefix.to_owned()));
        }
        Ok(Self::build(
            self.inner.pool.clone(),
            self.inner.local.clone(),
            prefix,
            self.inner.origin,
        ))
    }

    fn build(pool: PgPool, local: BroadcastSink, prefix: &str, origin: Uuid) -> Self {
        let (queue, outbox) = mpsc::channel(QUEUE_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                pool,
                local,
                origin,
                events_channel: format!("{prefix}events"),
                signals_channel: format!("{prefix}signals"),
                queue,
                outbox: Mutex::new(Some(outbox)),
                fanout: LocalNotifier::default(),
                listening: watch::channel(false).0,
                dropped: AtomicU64::new(0),
                failed: AtomicU64::new(0),
            }),
        }
    }

    /// The [`EventSink`] for this process's runtime
    /// (`RuntimeBuilder::event_sink`). Each event goes to the local sink at
    /// once and to the other processes through `NOTIFY`.
    pub fn event_sink(&self) -> PgEventSink {
        PgEventSink {
            inner: self.inner.clone(),
        }
    }

    /// The [`Notifier`] for this process's runtime
    /// (`RuntimeBuilder::notifier`).
    pub fn notifier(&self) -> PgNotifier {
        PgNotifier {
            inner: self.inner.clone(),
        }
    }

    /// Resolves once `LISTEN` is active on the current connection (at once if
    /// it already is). Notifications published before that by other
    /// processes are not received, so a caller that must not miss any waits
    /// for this before starting the work that causes them.
    pub async fn wait_listening(&self) {
        let mut listening = self.inner.listening.subscribe();
        // The sender lives in `self`, so the wait cannot see it closed.
        let _ = listening.wait_for(|on| *on).await;
    }

    /// Publish queued items and listen for others', until `stop` resolves.
    ///
    /// Returns `Ok(())` only when stopped. Connection trouble is not an error:
    /// the listener logs it, reconnects with a doubling backoff (100 ms to
    /// 10 s) and tells its subscribers to [`Resync`](Delivery::Resync), and a
    /// failed `pg_notify` drops its item. Fails with
    /// [`NotifyError::AlreadyRunning`] if it is already running (also through
    /// a clone), and with [`NotifyError::PoolClosed`] once the pool is closed.
    ///
    /// The listener holds one connection of the pool for as long as this
    /// runs. When `stop` resolves, the `pg_notify` in flight completes and the
    /// items still queued are sent for up to [`DRAIN_ON_STOP`] before `run`
    /// returns, so the events of a step that ended just before the stop still
    /// go out; what cannot be sent in that time is dropped (best effort, like
    /// any notification).
    pub async fn run(&self, stop: impl Future<Output = ()> + Send) -> Result<(), NotifyError> {
        let inner = &*self.inner;
        let mut outbox = inner.take_outbox()?;
        // The listener ends the run; the publisher notices between two items
        // (never in the middle of a `pg_notify`) and then drains the queue.
        let (stopped_tx, stopped_rx) = watch::channel(false);
        let listener = async {
            let result = inner.listen_loop(stop).await;
            stopped_tx.send_replace(true);
            result
        };
        let (result, ()) =
            tokio::join!(listener, inner.publish_loop(outbox.receiver(), stopped_rx));
        result
    }
}

/// Gives the outbox back when `run` ends, however it ends.
struct OutboxGuard<'a> {
    inner: &'a Inner,
    rx: Option<mpsc::Receiver<Outgoing>>,
}

impl OutboxGuard<'_> {
    fn receiver(&mut self) -> &mut mpsc::Receiver<Outgoing> {
        // Set in `take_outbox`, taken only by `drop`.
        self.rx
            .as_mut()
            .unwrap_or_else(|| unreachable!("the receiver is held until drop"))
    }
}

impl Drop for OutboxGuard<'_> {
    fn drop(&mut self) {
        *self
            .inner
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = self.rx.take();
    }
}

/// How a listener session ended.
enum Session {
    Stopped,
    Failed(sqlx::Error),
}

impl Inner {
    fn take_outbox(&self) -> Result<OutboxGuard<'_>, NotifyError> {
        let rx = self
            .outbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .ok_or(NotifyError::AlreadyRunning)?;
        Ok(OutboxGuard {
            inner: self,
            rx: Some(rx),
        })
    }

    /// Queue one item without waiting; a full queue drops it.
    fn enqueue(&self, item: Outgoing) {
        if self.queue.try_send(item).is_err() {
            let n = self.dropped.fetch_add(1, Relaxed) + 1;
            if should_log(n) {
                tracing::warn!(
                    dropped = n,
                    "notification queue is full or closed, dropping (best effort)"
                );
            }
        }
    }

    /// Send queued items, one `pg_notify` at a time, in order, until `stopped`;
    /// then send what is left for at most [`DRAIN_ON_STOP`]. A send is never
    /// cut short by the stop: it runs in the arm, after the `select!`.
    async fn publish_loop(
        &self,
        outbox: &mut mpsc::Receiver<Outgoing>,
        mut stopped: watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                biased;
                // Drop the watch guard inside, so the future stays `Send`.
                () = async { drop(stopped.wait_for(|stopped| *stopped).await); } => break,
                item = outbox.recv() => match item {
                    Some(item) => self.send(item).await,
                    // The sender lives in `self`; unreachable, but never spin.
                    None => break,
                },
            }
        }
        self.drain(outbox, DRAIN_ON_STOP).await;
    }

    /// Send what is already queued, in order, for at most `within`.
    async fn drain(&self, outbox: &mut mpsc::Receiver<Outgoing>, within: Duration) {
        let sending = async {
            let mut sent = 0_usize;
            while let Ok(item) = outbox.try_recv() {
                self.send(item).await;
                sent += 1;
            }
            sent
        };
        match tokio::time::timeout(within, sending).await {
            Ok(0) => {}
            Ok(sent) => tracing::debug!(sent, "sent the notifications queued at stop"),
            Err(_) => tracing::warn!(
                "stopped before every queued notification was sent, dropping the rest (best effort)"
            ),
        }
    }

    /// One `pg_notify`; a failure drops the item.
    async fn send(&self, item: Outgoing) {
        let channel = if item.signals {
            &self.signals_channel
        } else {
            &self.events_channel
        };
        let sent = sqlx::query("SELECT pg_notify($1, $2)")
            .bind(channel)
            .bind(&item.payload)
            .execute(&self.pool)
            .await;
        if let Err(e) = sent {
            let n = self.failed.fetch_add(1, Relaxed) + 1;
            if should_log(n) {
                tracing::warn!(error = %e, failed = n, "pg_notify failed, dropping (best effort)");
            }
        }
    }

    /// Connect, `LISTEN`, dispatch, and reconnect until `stop`.
    async fn listen_loop(&self, stop: impl Future<Output = ()>) -> Result<(), NotifyError> {
        tokio::pin!(stop);
        let mut backoff = BACKOFF_START;
        loop {
            let connect = async {
                let mut listener = PgListener::connect_with(&self.pool).await?;
                listener
                    .listen_all([self.events_channel.as_str(), self.signals_channel.as_str()])
                    .await?;
                Ok::<_, sqlx::Error>(listener)
            };
            let connected = tokio::select! {
                () = &mut stop => return Ok(()),
                connected = connect => connected,
            };
            let failure = match connected {
                Ok(listener) => {
                    let started = Instant::now();
                    // LISTEN is active first, then subscribers catch up.
                    self.listening.send_replace(true);
                    self.fanout.resync();
                    let session = self.session(listener, &mut stop).await;
                    self.listening.send_replace(false);
                    if started.elapsed() >= HEALTHY_AFTER {
                        backoff = BACKOFF_START;
                    }
                    match session {
                        Session::Stopped => return Ok(()),
                        Session::Failed(e) => e,
                    }
                }
                Err(e) => e,
            };
            if matches!(failure, sqlx::Error::PoolClosed) {
                return Err(NotifyError::PoolClosed);
            }
            tracing::warn!(error = %failure, retry_in = ?backoff, "notification listener lost, reconnecting");
            tokio::select! {
                () = &mut stop => return Ok(()),
                () = tokio::time::sleep(backoff) => {}
            }
            backoff = next_backoff(backoff);
        }
    }

    /// Receive notifications on one listener until it fails or `stop`.
    async fn session(
        &self,
        mut listener: PgListener,
        stop: &mut (impl Future<Output = ()> + Unpin),
    ) -> Session {
        loop {
            tokio::select! {
                () = &mut *stop => return Session::Stopped,
                received = listener.try_recv() => match received {
                    Ok(Some(n)) => self.dispatch(n.channel(), n.payload()).await,
                    // The connection dropped and sqlx has already replaced it
                    // and listened again; what was sent in between is lost.
                    Ok(None) => {
                        tracing::warn!("notification connection was lost and re-established, resyncing");
                        self.fanout.resync();
                    }
                    Err(e) => return Session::Failed(e),
                },
            }
        }
    }

    /// Route one notification. Never re-publishes.
    async fn dispatch(&self, channel: &str, payload: &str) {
        if channel == self.events_channel {
            match serde_json::from_str::<EventIn>(payload) {
                Ok(e) if e.v == WIRE_VERSION => {
                    // Our own events were delivered locally when emitted.
                    if e.o != self.origin {
                        self.local.emit(e.run, &e.agent, e.event).await;
                    }
                }
                Ok(e) => tracing::debug!(version = e.v, "ignoring an event of another version"),
                Err(e) => tracing::debug!(error = %e, "ignoring an unreadable event payload"),
            }
        } else if channel == self.signals_channel {
            match serde_json::from_str::<SignalIn>(payload) {
                Ok(s) if s.v == WIRE_VERSION => self.fanout.publish(s.signal).await,
                Ok(s) => tracing::debug!(version = s.v, "ignoring a signal of another version"),
                Err(e) => tracing::debug!(error = %e, "ignoring an unreadable signal payload"),
            }
        }
    }
}

/// The [`EventSink`] of a [`PgNotify`]: local subscribers first, then the other
/// processes through `NOTIFY`. Best effort, like every sink: it never blocks
/// and never fails.
#[derive(Clone, Debug)]
pub struct PgEventSink {
    inner: Arc<Inner>,
}

#[async_trait]
impl EventSink for PgEventSink {
    async fn emit(&self, run: RunId, agent: &str, event: RunEvent) {
        let encoded = encode_event(self.inner.origin, run, agent, &event);
        self.inner.local.emit(run, agent, event).await;
        match encoded {
            Encoded::Fits(payload) => self.inner.enqueue(Outgoing {
                signals: false,
                payload,
            }),
            Encoded::Truncated(payload) => {
                tracing::debug!(%run, "event cut to fit a notification (a status's detail, a step's input and output)");
                self.inner.enqueue(Outgoing {
                    signals: false,
                    payload,
                });
            }
            Encoded::Dropped => {
                tracing::debug!(%run, max = MAX_PAYLOAD_BYTES, "event too big for a notification, not sent to other processes");
            }
        }
    }
}

/// The [`Notifier`] of a [`PgNotify`]. A published signal travels through the
/// database and comes back to this process's subscribers as well, once the
/// listener is running.
#[derive(Clone, Debug)]
pub struct PgNotifier {
    inner: Arc<Inner>,
}

#[async_trait]
impl Notifier for PgNotifier {
    async fn publish(&self, signal: Signal) {
        match encode_signal(&signal) {
            Some(payload) => self.inner.enqueue(Outgoing {
                signals: true,
                payload,
            }),
            None => tracing::debug!(
                max = MAX_PAYLOAD_BYTES,
                "signal too big for a notification, dropped"
            ),
        }
    }

    fn subscribe(&self) -> BoxStream<'static, Delivery> {
        self.inner.fanout.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use adam_core::RunStatus;
    use futures::StreamExt;
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    /// A pool that never connects: enough to test everything that does not
    /// touch the database.
    fn notify() -> (PgNotify, BroadcastSink) {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .expect("lazy pool");
        let local = BroadcastSink::default();
        (PgNotify::new(pool, local.clone()), local)
    }

    fn progress(m: &str) -> RunEvent {
        RunEvent::Progress { message: m.into() }
    }

    #[tokio::test]
    async fn prefixes_follow_the_table_prefix_rule() {
        let (n, _) = notify();
        for ok in ["adam_", "a", "_x", "adam_n0123abcd_", &"a".repeat(40)] {
            assert!(n.clone().with_channel_prefix(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "Adam_",
            "1adam_",
            "adam-",
            "adam.",
            "a b",
            "ä",
            &"a".repeat(41),
        ] {
            assert!(
                matches!(
                    n.clone().with_channel_prefix(bad),
                    Err(NotifyError::InvalidPrefix(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn channels_carry_the_prefix() {
        let (n, _) = notify();
        let n = n.with_channel_prefix("zz_").expect("prefix");
        assert_eq!(n.inner.events_channel, "zz_events");
        assert_eq!(n.inner.signals_channel, "zz_signals");
        let default = notify().0;
        assert_eq!(default.inner.events_channel, "adam_events");
    }

    #[tokio::test]
    async fn events_of_other_origins_are_delivered_and_our_own_are_not() {
        let (n, local) = notify();
        let mut rx = local.subscribe();
        let run = RunId::new();
        let event = progress("from elsewhere");
        let Encoded::Fits(theirs) = encode_event(Uuid::new_v4(), run, "a", &event) else {
            panic!("fits");
        };
        let Encoded::Fits(ours) = encode_event(n.inner.origin, run, "a", &progress("echo")) else {
            panic!("fits");
        };
        n.inner.dispatch("adam_events", &ours).await;
        n.inner.dispatch("adam_events", &theirs).await;
        let got = rx.try_recv().expect("the other origin's event");
        assert_eq!((got.run, got.agent.as_str(), got.event), (run, "a", event));
        assert!(rx.try_recv().is_err(), "our own event is not echoed");
    }

    #[tokio::test]
    async fn dispatch_never_republishes() {
        let (n, _) = notify();
        let Encoded::Fits(theirs) = encode_event(Uuid::new_v4(), RunId::new(), "a", &progress("x"))
        else {
            panic!("fits");
        };
        let signal = encode_signal(&Signal::Finished { run: RunId::new() }).expect("fits");
        n.inner.dispatch("adam_events", &theirs).await;
        n.inner.dispatch("adam_signals", &signal).await;
        let mut outbox = n.inner.take_outbox().expect("not running");
        assert!(outbox.receiver().try_recv().is_err(), "nothing queued");
    }

    #[tokio::test]
    async fn signals_reach_notifier_subscribers_and_junk_is_ignored() {
        let (n, local) = notify();
        let mut sub = n.notifier().subscribe();
        let mut events = local.subscribe();
        let signal = Signal::Runnable {
            run: RunId::new(),
            agent: "coder".into(),
        };
        for junk in ["", "not json", r#"{"v":2}"#, r#"{"v":1,"type":"nope"}"#] {
            n.inner.dispatch("adam_signals", junk).await;
            n.inner.dispatch("adam_events", junk).await;
        }
        n.inner.dispatch("someone_elses", "{}").await;
        n.inner
            .dispatch("adam_signals", &encode_signal(&signal).expect("fits"))
            .await;
        assert_eq!(sub.next().await, Some(Delivery::Signal(signal)));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn the_sink_emits_locally_first_and_queues_for_the_others() {
        let (n, local) = notify();
        let mut rx = local.subscribe();
        let sink = n.event_sink();
        let run = RunId::new();
        sink.emit(run, "a", progress("one")).await;
        sink.emit(
            run,
            "a",
            RunEvent::Status {
                status: RunStatus::Failed,
                detail: Some("é".repeat(10_000)),
            },
        )
        .await;
        sink.emit(run, "a", progress(&"x".repeat(20_000))).await;
        // All three reached the local sink, whole.
        for _ in 0..3 {
            assert!(rx.try_recv().is_ok());
        }
        // Two were queued: the small one and the truncated status; the
        // oversize progress was not.
        let mut outbox = n.inner.take_outbox().expect("not running");
        let queued: Vec<_> = std::iter::from_fn(|| outbox.receiver().try_recv().ok()).collect();
        assert_eq!(queued.len(), 2);
        assert!(queued.iter().all(|q| !q.signals));
        assert!(queued.iter().all(|q| q.payload.len() <= MAX_PAYLOAD_BYTES));
    }

    #[tokio::test]
    async fn a_full_queue_drops_instead_of_blocking() {
        let (n, _) = notify();
        let notifier = n.notifier();
        let signal = Signal::Finished { run: RunId::new() };
        tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..QUEUE_CAPACITY + 500 {
                notifier.publish(signal.clone()).await;
            }
        })
        .await
        .expect("publishing never waits");
        assert_eq!(n.inner.dropped.load(Relaxed), 500);
    }

    #[tokio::test]
    async fn run_refuses_a_second_runner_and_gives_the_outbox_back() {
        let (n, _) = notify();
        let guard = n.inner.take_outbox().expect("first");
        assert!(matches!(
            n.run(std::future::ready(())).await,
            Err(NotifyError::AlreadyRunning)
        ));
        drop(guard);
        assert!(n.inner.take_outbox().is_ok(), "returned on drop");
    }

    #[test]
    fn the_backoff_doubles_and_stops_at_ten_seconds() {
        let mut d = BACKOFF_START;
        let mut seen = vec![d];
        for _ in 0..10 {
            d = next_backoff(d);
            seen.push(d);
        }
        assert_eq!(
            seen[..4],
            [
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(800)
            ]
        );
        assert_eq!(*seen.last().expect("some"), BACKOFF_MAX);
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn logging_is_rate_limited() {
        let logged: Vec<u64> = (1..=3000).filter(|n| should_log(*n)).collect();
        assert_eq!(logged, [1, 1000, 2000, 3000]);
    }
}
