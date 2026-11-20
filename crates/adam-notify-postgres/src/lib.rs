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
//! A batch is one statement, and PostgreSQL folds identical channel-and-payload
//! pairs of one transaction into one notification: whether the second copy of an
//! identical event arrives therefore depends on how the queue happened to be
//! batched when it was sent, so nothing may depend on the count. A duplicate is
//! indistinguishable from the first.
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
//! [`PgNotify::run`] drives one publisher (a queue of 1024 drained with one
//! `SELECT pg_notify(..)` statement per batch of up to [`MAX_BATCH_ITEMS`], in
//! order) and one listener, until `stop` resolves. **The listener opens its own
//! connection from the [`PgConfig`] it was built with** — a pooled client can
//! never receive a notification — and `LISTEN` needs a session, so connect
//! directly or through a session-mode pooler, not a transaction-mode one. The
//! publisher never blocks the caller: a full queue drops the item.
//!
//! A process whose events no one else needs (`ROLE=all`, one pod) can keep them
//! local: set `NOTIFY_LOCAL_ONLY` (`true`/`1`, default `false`) and
//! [`PgEventSink`] delivers to the local sink without encoding or queueing
//! anything for `NOTIFY`. With `topology: split` (a front and its workers, even
//! one replica each) it silences the live events between them; their state
//! changes still reach the 250 ms poll. It is a deployment choice, never
//! detected.
//!
//! Its lifecycle, and the guarantees, are in the [crate README](https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-notify-postgres/README.md)
//! and `docs/architecture.md`.

#![warn(missing_docs)]

mod error;
mod wire;

use std::ffi::OsString;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use adam_core::RunId;
use adam_postgres::PgConfig;
use adam_runtime::{BroadcastSink, Delivery, EventSink, LocalNotifier, Notifier, RunEvent, Signal};
use async_trait::async_trait;
use deadpool_postgres::Pool;
use futures::future::poll_fn;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, watch};
use tokio_postgres::{AsyncMessage, Client};
use uuid::Uuid;

use crate::wire::{Encoded, EventIn, SignalIn, WIRE_VERSION, encode_event, encode_signal};

pub use crate::error::NotifyError;
pub use crate::wire::MAX_PAYLOAD_BYTES;

/// How long [`PgNotify::run`] keeps sending the items still queued when its
/// `stop` resolves.
pub const DRAIN_ON_STOP: Duration = Duration::from_secs(2);

/// The most items one `pg_notify` statement carries: a batch is one round trip,
/// and a statement of 32 payloads of at most [`MAX_PAYLOAD_BYTES`] stays small.
pub const MAX_BATCH_ITEMS: usize = 32;

/// The variable that keeps this process's events local: `true`/`1` or
/// `false`/`0` (ASCII case ignored), absent is `false`. Read by
/// [`PgNotify::new`]; [`PgNotify::local_only`] overrides it.
pub const LOCAL_ONLY_ENV: &str = "NOTIFY_LOCAL_ONLY";

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
    /// The publisher's connections (`pg_notify`).
    pool: Pool,
    /// The listener's own connection: a pooled client would never see a notification.
    pg: PgConfig,
    /// Where events of other processes are delivered.
    local: BroadcastSink,
    /// Tells our own events from those of others.
    origin: Uuid,
    prefix: String,
    events_channel: String,
    signals_channel: String,
    /// Events stay in this process: no encoding, no outbox, no `NOTIFY`.
    local_only: bool,
    /// The value of [`LOCAL_ONLY_ENV`] when it was not a boolean: `run` refuses it.
    local_only_problem: Option<String>,
    queue: mpsc::Sender<Outgoing>,
    /// The receiving end of `queue`, held by `run` while it runs.
    outbox: Mutex<Option<mpsc::Receiver<Outgoing>>>,
    /// Fan-out of the signals (and resyncs) the listener receives.
    fanout: LocalNotifier,
    listening: watch::Sender<bool>,
    dropped: AtomicU64,
    failed: AtomicU64,
    /// Failed statements, whatever the size of the batch each carried.
    failed_statements: AtomicU64,
    /// `pg_notify` statements issued, a batch counting once.
    statements: AtomicU64,
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

/// `true`/`1` or `false`/`0`, ASCII case ignored; anything else is not a boolean.
fn parse_flag(raw: &str) -> Option<bool> {
    if raw.eq_ignore_ascii_case("true") || raw == "1" {
        Some(true)
    } else if raw.eq_ignore_ascii_case("false") || raw == "0" {
        Some(false)
    } else {
        None
    }
}

/// [`LOCAL_ONLY_ENV`] from `lookup`: the value, and the raw text when it is not a boolean (then
/// the value is `false`, and `run` refuses it).
fn local_only_from(lookup: impl Fn(&str) -> Option<OsString>) -> (bool, Option<String>) {
    let Some(raw) = lookup(LOCAL_ONLY_ENV) else {
        return (false, None);
    };
    let raw = raw.to_string_lossy().into_owned();
    match parse_flag(&raw) {
        Some(on) => (on, None),
        None => (false, Some(raw)),
    }
}

fn local_only_from_env() -> (bool, Option<String>) {
    local_only_from(|name| std::env::var_os(name))
}

/// The item just received plus whatever else is already queued, up to
/// [`MAX_BATCH_ITEMS`], in queue order. Never waits for the batch to fill.
fn take_batch(first: Outgoing, outbox: &mut mpsc::Receiver<Outgoing>) -> Vec<Outgoing> {
    let mut batch = Vec::with_capacity(MAX_BATCH_ITEMS);
    batch.push(first);
    while batch.len() < MAX_BATCH_ITEMS {
        match outbox.try_recv() {
            Ok(item) => batch.push(item),
            Err(_) => break,
        }
    }
    batch
}

impl PgNotify {
    /// Notifications over `pool`, with the listener's connection built from `pg`, delivering the
    /// events of other processes to `local` (the sink the process's A2A backend subscribes to).
    /// Call [`run`](Self::run) for anything to flow.
    ///
    /// `pg` is the same [`PgConfig`] the store was built from, so `DATABASE_URL` is parsed once.
    /// The listener needs a connection of its own: deadpool drives a connection as a `Future`,
    /// which drops notifications, so a pooled client could never receive one.
    ///
    /// [`LOCAL_ONLY_ENV`] is read here, once: an unset variable is `false`, and
    /// [`local_only`](Self::local_only) overrides it.
    pub fn new(pool: Pool, pg: PgConfig, local: BroadcastSink) -> Self {
        let (local_only, local_only_problem) = local_only_from_env();
        Self::build(
            pool,
            pg,
            local,
            DEFAULT_PREFIX,
            Uuid::new_v4(),
            local_only,
            local_only_problem,
        )
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
            self.inner.pg.clone(),
            self.inner.local.clone(),
            prefix,
            self.inner.origin,
            self.inner.local_only,
            self.inner.local_only_problem.clone(),
        ))
    }

    /// Keep this process's events local: [`PgEventSink::emit`] delivers them to
    /// the local sink and does not encode them, queue them or `NOTIFY` them.
    /// Signals still cross: wake-ups and cancels are not events.
    ///
    /// Default `false`; [`new`](Self::new) takes the value from
    /// [`LOCAL_ONLY_ENV`], and this call overrides it, including a value that
    /// is not a boolean. Call it before [`event_sink`](Self::event_sink), for
    /// the same reason as [`with_channel_prefix`](Self::with_channel_prefix).
    pub fn local_only(self, local_only: bool) -> Self {
        Self::build(
            self.inner.pool.clone(),
            self.inner.pg.clone(),
            self.inner.local.clone(),
            &self.inner.prefix,
            self.inner.origin,
            local_only,
            None,
        )
    }

    fn build(
        pool: Pool,
        pg: PgConfig,
        local: BroadcastSink,
        prefix: &str,
        origin: Uuid,
        local_only: bool,
        local_only_problem: Option<String>,
    ) -> Self {
        let (queue, outbox) = mpsc::channel(QUEUE_CAPACITY);
        Self {
            inner: Arc::new(Inner {
                pool,
                pg,
                local,
                origin,
                prefix: prefix.to_owned(),
                events_channel: format!("{prefix}events"),
                signals_channel: format!("{prefix}signals"),
                local_only,
                local_only_problem,
                queue,
                outbox: Mutex::new(Some(outbox)),
                fanout: LocalNotifier::default(),
                listening: watch::channel(false).0,
                dropped: AtomicU64::new(0),
                failed: AtomicU64::new(0),
                failed_statements: AtomicU64::new(0),
                statements: AtomicU64::new(0),
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

    /// How many `pg_notify` statements this instance has issued: one per batch,
    /// up to [`MAX_BATCH_ITEMS`] items each. A counter for tests and for a
    /// process's own metric, not a durable count.
    pub fn statements_sent(&self) -> u64 {
        self.inner.statements.load(Relaxed)
    }

    /// Publish queued items and listen for others', until `stop` resolves.
    ///
    /// Returns `Ok(())` only when stopped. Connection trouble is not an error:
    /// the listener logs it, reconnects with a doubling backoff (100 ms to
    /// 10 s) and tells its subscribers to [`Resync`](Delivery::Resync), and a
    /// failed `pg_notify` drops its batch (up to [`MAX_BATCH_ITEMS`] items).
    /// Fails with [`NotifyError::AlreadyRunning`] if it is already running
    /// (also through a clone), and with [`NotifyError::PoolClosed`] once the
    /// pool is closed.
    ///
    /// The listener holds one connection of its own (outside the pool) for as
    /// long as this runs. When `stop` resolves, the `pg_notify` in flight completes and the
    /// items still queued are sent for up to [`DRAIN_ON_STOP`] before `run`
    /// returns, so the events of a step that ended just before the stop still
    /// go out; what cannot be sent in that time is dropped (best effort, like
    /// any notification).
    ///
    /// Fails at once with [`NotifyError::InvalidLocalOnly`] when
    /// [`LOCAL_ONLY_ENV`] was set to something that is not a boolean and
    /// [`local_only`](Self::local_only) did not override it.
    pub async fn run(&self, stop: impl Future<Output = ()> + Send) -> Result<(), NotifyError> {
        let inner = &*self.inner;
        if let Some(raw) = &inner.local_only_problem {
            return Err(NotifyError::InvalidLocalOnly(raw.clone()));
        }
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
    Failed(String),
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

    /// Send queued items, one statement per batch, in order, until `stopped`;
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
                    Some(item) => {
                        self.send(take_batch(item, outbox)).await;
                    }
                    // The sender lives in `self`; unreachable, but never spin.
                    None => break,
                },
            }
        }
        self.drain(outbox, DRAIN_ON_STOP).await;
    }

    /// Send what is already queued, one statement per batch, in order, for at
    /// most `within`. Only a batch whose statement succeeded is counted.
    async fn drain(&self, outbox: &mut mpsc::Receiver<Outgoing>, within: Duration) {
        let sending = async {
            let mut sent = 0_usize;
            while let Ok(first) = outbox.try_recv() {
                let batch = take_batch(first, outbox);
                let count = batch.len();
                if self.send(batch).await {
                    sent += count;
                }
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

    /// The two `text[]` columns of one statement: the channel and the payload of
    /// each item, in queue order.
    fn columns(&self, items: Vec<Outgoing>) -> (Vec<String>, Vec<String>) {
        let mut channels = Vec::with_capacity(items.len());
        let mut payloads = Vec::with_capacity(items.len());
        for item in items {
            channels.push(if item.signals {
                self.signals_channel.clone()
            } else {
                self.events_channel.clone()
            });
            payloads.push(item.payload);
        }
        (channels, payloads)
    }

    /// One `pg_notify` statement for a whole batch; a failure drops the items in
    /// it. PostgreSQL delivers the notifications in row order, and folds
    /// identical channel-and-payload pairs of one statement into one. `true`
    /// when the statement succeeded.
    async fn send(&self, items: Vec<Outgoing>) -> bool {
        debug_assert!(!items.is_empty(), "a batch has at least one item");
        let (channels, payloads) = self.columns(items);
        let count = channels.len();
        let sent: Result<u64, Box<dyn std::error::Error + Send + Sync>> = async {
            let client = self.pool.get().await?;
            let stmt = client
                .prepare_cached(
                    "SELECT pg_notify(channel, payload) \
                     FROM unnest($1::text[], $2::text[]) AS t(channel, payload)",
                )
                .await?;
            Ok(client.execute(&stmt, &[&channels, &payloads]).await?)
        }
        .await;
        self.statements.fetch_add(1, Relaxed);
        match sent {
            Ok(_) => true,
            Err(e) => {
                let items = self.failed.fetch_add(count as u64, Relaxed) + count as u64;
                let statements = self.failed_statements.fetch_add(1, Relaxed) + 1;
                // The first failed statement, then every thousandth, whatever
                // the batch size: a batch of 32 must not swallow the first one.
                if should_log(statements) {
                    tracing::warn!(
                        error = %e,
                        batch = count,
                        failed = items,
                        statements,
                        "pg_notify failed, dropping the batch (best effort)"
                    );
                }
                false
            }
        }
    }

    /// Connect, `LISTEN`, dispatch, and reconnect until `stop`.
    async fn listen_loop(&self, stop: impl Future<Output = ()>) -> Result<(), NotifyError> {
        tokio::pin!(stop);
        let mut backoff = BACKOFF_START;
        loop {
            // A closed pool is a shutdown: the publisher has nothing left to send on either.
            if self.pool.is_closed() {
                return Err(NotifyError::PoolClosed);
            }
            let connect = async {
                let (client, driver) = self.pg.connect_raw().await?;
                Ok::<_, adam_core::StoreError>((client, driver))
            };
            let connected = tokio::select! {
                () = &mut stop => return Ok(()),
                connected = connect => connected,
            };
            let failure = match connected {
                Ok((client, driver)) => {
                    let started = Instant::now();
                    let session = self.session(client, driver, &mut stop).await;
                    self.listening.send_replace(false);
                    if started.elapsed() >= HEALTHY_AFTER {
                        backoff = BACKOFF_START;
                    }
                    match session {
                        Session::Stopped => return Ok(()),
                        Session::Failed(e) => e,
                    }
                }
                Err(e) => e.to_string(),
            };
            if self.pool.is_closed() {
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

    /// `LISTEN` on both channels, then receive notifications on one connection until it fails or
    /// `stop`. The connection is driven here, not by a spawned future: that future ignores
    /// asynchronous messages.
    async fn session(
        &self,
        client: Client,
        mut driver: adam_postgres::ConnectionDriver,
        stop: &mut (impl Future<Output = ()> + Unpin),
    ) -> Session {
        let listen = format!(
            "LISTEN {}; LISTEN {}",
            self.events_channel, self.signals_channel
        );
        // The driver must be polled for the `LISTEN` response to arrive, so its future is polled
        // beside the messages until it completes.
        let listen = client.batch_execute(&listen);
        tokio::pin!(listen);
        let mut listening = false;
        loop {
            tokio::select! {
                () = &mut *stop => return Session::Stopped,
                result = &mut listen, if !listening => match result {
                    Ok(()) => {
                        // LISTEN is active first, then subscribers catch up.
                        listening = true;
                        self.listening.send_replace(true);
                        self.fanout.resync();
                    }
                    Err(e) => return Session::Failed(e.to_string()),
                },
                message = poll_fn(|cx| driver.poll_message(cx)) => match message {
                    Some(Ok(AsyncMessage::Notification(n))) => {
                        self.dispatch(n.channel(), n.payload()).await;
                    }
                    Some(Ok(AsyncMessage::Notice(notice))) => {
                        tracing::debug!(error = %notice, "notice on the notification connection");
                    }
                    // `AsyncMessage` is non-exhaustive; a future variant is not a notification.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Session::Failed(e.to_string()),
                    // The connection task ended: the session is gone, and what it missed with it.
                    None => return Session::Failed("the notification connection closed".to_owned()),
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
        // Local only: no encoding and no outbox entry. The listener of every
        // process drops an event of its own origin, this one included, so
        // nothing would come of sending it.
        let encoded =
            (!self.inner.local_only).then(|| encode_event(self.inner.origin, run, agent, &event));
        self.inner.local.emit(run, agent, event).await;
        match encoded {
            None => {}
            Some(Encoded::Fits(payload)) => self.inner.enqueue(Outgoing {
                signals: false,
                payload,
            }),
            Some(Encoded::Truncated(payload)) => {
                tracing::debug!(%run, "event cut to fit a notification (a status's detail, a step's input and output)");
                self.inner.enqueue(Outgoing {
                    signals: false,
                    payload,
                });
            }
            Some(Encoded::Dropped) => {
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

    use super::*;

    /// A pool that never connects: enough to test everything that does not
    /// touch the database. The explicit `local_only(false)` keeps an ambient
    /// `NOTIFY_LOCAL_ONLY` from changing what these tests measure.
    fn notify() -> (PgNotify, BroadcastSink) {
        let pg = PgConfig::parse("postgres://nobody@127.0.0.1:1/none").expect("a URL");
        let pool = pg.pool(2).expect("lazy pool");
        let local = BroadcastSink::default();
        (
            PgNotify::new(pool, pg, local.clone()).local_only(false),
            local,
        )
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

    #[test]
    fn the_local_only_variable_is_a_boolean_or_a_problem() {
        assert_eq!(local_only_from(|_| None), (false, None));
        for (raw, want) in [
            ("true", true),
            ("TRUE", true),
            ("True", true),
            ("1", true),
            ("false", false),
            ("False", false),
            ("0", false),
        ] {
            assert_eq!(
                local_only_from(|_| Some(raw.into())),
                (want, None),
                "{raw:?}"
            );
        }
        for bad in ["", "yes", "on", "2", " true", "false "] {
            assert_eq!(
                local_only_from(|_| Some(bad.into())),
                (false, Some(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_bad_local_only_value_fails_run_without_taking_the_outbox() {
        let (n, _) = notify();
        let bad = PgNotify::build(
            n.inner.pool.clone(),
            n.inner.pg.clone(),
            n.inner.local.clone(),
            "adam_",
            n.inner.origin,
            false,
            Some("yes".into()),
        );
        assert!(matches!(
            bad.run(std::future::ready(())).await,
            Err(NotifyError::InvalidLocalOnly(raw)) if raw == "yes"
        ));
        assert!(bad.inner.take_outbox().is_ok(), "nothing was started");
    }

    #[tokio::test]
    async fn local_only_keeps_every_event_in_this_process() {
        let (n, local) = notify();
        let n = n.local_only(true);
        let sink = n.event_sink();
        let mut rx = local.subscribe();
        let run = RunId::new();
        for i in 0..100 {
            sink.emit(run, "a", progress(&format!("{i}"))).await;
        }
        // Every event reached the local sink, in order and whole.
        for i in 0..100 {
            let got = rx.try_recv().expect("the local sink gets every event");
            assert_eq!((got.run, got.event), (run, progress(&format!("{i}"))));
        }
        // Nothing was encoded or queued for the other processes.
        let mut outbox = n.inner.take_outbox().expect("not running");
        assert!(
            outbox.receiver().try_recv().is_err(),
            "no outbox entry with the flag on"
        );
        assert_eq!(n.inner.dropped.load(Relaxed), 0);
    }

    #[tokio::test]
    async fn a_batch_takes_what_is_queued_up_to_the_cap() {
        let (tx, mut rx) = mpsc::channel(2 * MAX_BATCH_ITEMS);
        for i in 0..2 * MAX_BATCH_ITEMS {
            tx.send(Outgoing {
                signals: false,
                payload: format!("{i}"),
            })
            .await
            .expect("queue");
        }
        let first = take_batch(rx.try_recv().expect("one"), &mut rx);
        assert_eq!(first.len(), MAX_BATCH_ITEMS);
        assert_eq!(first[0].payload, "0");
        assert_eq!(
            first[MAX_BATCH_ITEMS - 1].payload,
            (MAX_BATCH_ITEMS - 1).to_string()
        );
        let second = take_batch(rx.try_recv().expect("one"), &mut rx);
        assert_eq!(second.len(), MAX_BATCH_ITEMS);
        assert_eq!(
            second[MAX_BATCH_ITEMS - 1].payload,
            (2 * MAX_BATCH_ITEMS - 1).to_string()
        );
        assert!(rx.try_recv().is_err(), "exactly what was queued, no wait");
    }

    #[tokio::test]
    async fn a_batch_keeps_its_order_and_each_items_channel() {
        let (n, _) = notify();
        let batch = vec![
            Outgoing {
                signals: false,
                payload: "e1".into(),
            },
            Outgoing {
                signals: true,
                payload: "s1".into(),
            },
            Outgoing {
                signals: false,
                payload: "e2".into(),
            },
        ];
        let (channels, payloads) = n.inner.columns(batch);
        assert_eq!(channels, ["adam_events", "adam_signals", "adam_events"]);
        assert_eq!(payloads, ["e1", "s1", "e2"]);
    }

    /// The log rate limit counts failed **statements**, so the first one is
    /// loud whatever its batch carried; the item counter keeps counting items.
    #[tokio::test]
    async fn a_failed_statement_is_counted_once_however_many_items_it_carried() {
        let (n, _) = notify();
        n.inner.pool.close();
        // The first failed statement is the big batch: it is the one the old item counter would
        // have swallowed (32, not 1), so the counters must still see one statement.
        let batch: Vec<Outgoing> = (0..MAX_BATCH_ITEMS)
            .map(|_| Outgoing {
                signals: false,
                payload: "y".into(),
            })
            .collect();
        assert!(
            !n.inner.send(batch).await,
            "a closed pool fails the statement"
        );
        assert_eq!(
            n.inner.failed.load(Relaxed),
            MAX_BATCH_ITEMS as u64,
            "the item counter counts every dropped item"
        );
        assert_eq!(n.inner.failed_statements.load(Relaxed), 1);
        assert!(
            should_log(1),
            "the first failed statement is logged whatever its size"
        );

        let one = vec![Outgoing {
            signals: false,
            payload: "x".into(),
        }];
        assert!(!n.inner.send(one).await);
        assert_eq!(
            n.inner.failed.load(Relaxed),
            1 + MAX_BATCH_ITEMS as u64,
            "the item counter counts every dropped item"
        );
        assert_eq!(n.inner.failed_statements.load(Relaxed), 2);
        assert!(!should_log(2), "the second failed statement is not logged");
    }
}
