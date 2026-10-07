//! The loop that delivers notifications: it claims the configs that are due from the
//! [`PushStore`], tells each webhook what it has not heard, and records how far it got.
//!
//! The sequence is in `docs/architecture.md` ("Push notification delivery"). In short, per
//! config, per round:
//!
//! 1. `claim_due` leases the config (a lease is a latency-and-duplicates optimisation; the
//!    version compare-and-swap of `commit` is what keeps the cursor from going backwards).
//! 2. The task is read as its owner ([`TaskBackend::get`]); a task that is gone ends the config.
//! 3. The cursor says what the webhook has not heard; the first such event is sent.
//! 4. A 2xx answer advances the cursor and makes the config due **at once**, so the next event
//!    follows without waiting for a poll. A failure keeps the event pending and schedules a
//!    retry with capped exponential backoff; after [`PushDeliveryOptions::give_up_after`] of
//!    failures the config is [`PushState::GaveUp`] and the reason recorded.
//! 5. A config that has heard everything is due again after the poll interval, and is
//!    [`PushState::Done`] once the task is terminal.
//!
//! A notification (the [`nudge`](PushDeliverer::nudge)) only makes a round start sooner. Polling
//! and the store decide.

use std::sync::Arc;
use std::time::Duration;

use a2a::StreamResponse;
use adam_error::Classify;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use tokio::sync::Notify;

use super::cursor::{PushCursor, is_final};
use super::sender::{PushSender, SendError};
use super::store::{DynPushStore, PushProgress, PushRecord, PushState, PushStoreError};
use crate::backend::{Caller, DynTaskBackend};

/// How delivery is tuned. Every field has a default for a production deployment; tests shorten
/// them.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PushDeliveryOptions {
    /// How often an idle config is looked at again, and how often the loop asks the store for due
    /// configs when nothing nudged it (default 2 s).
    pub poll_interval: Duration,
    /// The wait after the first failure (default 1 s); it doubles per failure.
    pub backoff_initial: Duration,
    /// The longest wait between attempts (default 5 min).
    pub backoff_cap: Duration,
    /// How long one event may keep failing before the config gives up (default 1 hour).
    pub give_up_after: Duration,
    /// How long a lease lasts (default 60 s): longer than a request can take.
    pub lease_ttl: Duration,
    /// Configs claimed per round, and delivered at the same time (default 16).
    pub batch: usize,
    /// How long one request to a webhook may take (default 15 s; the specification recommends
    /// 10 to 30 seconds).
    pub request_timeout: Duration,
}

impl Default for PushDeliveryOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            backoff_initial: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(300),
            give_up_after: Duration::from_secs(3600),
            lease_ttl: Duration::from_secs(60),
            batch: 16,
            request_timeout: Duration::from_secs(15),
        }
    }
}

impl PushDeliveryOptions {
    /// Defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`poll_interval`](Self::poll_interval).
    #[must_use]
    pub fn with_poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d.max(Duration::from_millis(1));
        self
    }

    /// Set the backoff: the first wait, and the cap.
    #[must_use]
    pub fn with_backoff(mut self, initial: Duration, cap: Duration) -> Self {
        self.backoff_initial = initial.max(Duration::from_millis(1));
        self.backoff_cap = cap.max(self.backoff_initial);
        self
    }

    /// Set [`give_up_after`](Self::give_up_after).
    #[must_use]
    pub fn with_give_up_after(mut self, d: Duration) -> Self {
        self.give_up_after = d;
        self
    }

    /// Set [`lease_ttl`](Self::lease_ttl).
    #[must_use]
    pub fn with_lease_ttl(mut self, d: Duration) -> Self {
        self.lease_ttl = d.max(Duration::from_millis(1));
        self
    }

    /// Set [`batch`](Self::batch).
    #[must_use]
    pub fn with_batch(mut self, n: usize) -> Self {
        self.batch = n.max(1);
        self
    }

    /// Set [`request_timeout`](Self::request_timeout).
    #[must_use]
    pub fn with_request_timeout(mut self, d: Duration) -> Self {
        self.request_timeout = d.max(Duration::from_millis(1));
        self
    }

    /// The wait before attempt number `attempts + 1`: the initial wait doubled per failure, capped,
    /// with up to a fifth added so replicas that failed together do not retry together.
    fn backoff(&self, attempts: u32, jitter: u32) -> Duration {
        let doublings = attempts.saturating_sub(1).min(20);
        let wait = self
            .backoff_initial
            .saturating_mul(1u32 << doublings)
            .min(self.backoff_cap);
        wait + wait.mul_f64(f64::from(jitter % 200) / 1000.0)
    }
}

/// Delivers push notifications from a [`PushStore`](super::PushStore): see the module docs.
#[derive(Clone)]
pub struct PushDeliverer {
    backend: DynTaskBackend,
    store: DynPushStore,
    sender: PushSender,
    options: PushDeliveryOptions,
    worker: String,
    nudge: Arc<Notify>,
}

impl std::fmt::Debug for PushDeliverer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushDeliverer")
            .field("worker", &self.worker)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl PushDeliverer {
    /// A deliverer that reads tasks from `backend`, keeps its state in `store` and sends with
    /// `sender`.
    pub fn new(
        backend: DynTaskBackend,
        store: DynPushStore,
        sender: PushSender,
        options: PushDeliveryOptions,
    ) -> Self {
        Self {
            backend,
            store,
            sender,
            options,
            worker: format!("push-{}", uuid::Uuid::new_v4().simple()),
            nudge: Arc::new(Notify::new()),
        }
    }

    /// Share `nudge` with whoever creates configs, so a new config is looked at at once.
    #[must_use]
    pub fn with_nudge(mut self, nudge: Arc<Notify>) -> Self {
        self.nudge = nudge;
        self
    }

    /// Start a round now instead of at the next poll. A hint: nothing depends on it. Call it when
    /// a task changed, or a config was created.
    pub fn nudge(&self) {
        self.nudge.notify_one();
    }

    /// A handle that nudges this deliverer, for whoever learns that a task changed.
    pub fn nudger(&self) -> Arc<Notify> {
        self.nudge.clone()
    }

    /// Deliver until `stop` resolves. Returns when it does; an in-flight request finishes first.
    pub async fn run(&self, stop: impl std::future::Future<Output = ()> + Send) {
        tokio::pin!(stop);
        loop {
            let worked = tokio::select! {
                () = &mut stop => return,
                worked = self.round() => worked,
            };
            if worked {
                // There was something to do: look again at once (the next event of a config is
                // due immediately), but let a stop through.
                tokio::task::yield_now().await;
                continue;
            }
            tokio::select! {
                () = &mut stop => return,
                () = self.nudge.notified() => {}
                () = tokio::time::sleep(self.options.poll_interval) => {}
            }
        }
    }

    /// One round: claim what is due and deliver it. Whether anything was claimed.
    pub async fn round(&self) -> bool {
        let claimed = match self
            .store
            .claim_due(
                &self.worker,
                Utc::now(),
                self.options.lease_ttl,
                self.options.batch,
            )
            .await
        {
            Ok(claimed) => claimed,
            Err(err) => {
                tracing::warn!(error = %adam_error::report(&err), retryable = err.is_retryable(), "claiming push notifications failed");
                return false;
            }
        };
        if claimed.is_empty() {
            return false;
        }
        stream::iter(claimed)
            .for_each_concurrent(self.options.batch, |record| self.deliver(record))
            .await;
        true
    }

    /// One config, one step: send the next event, or find there is none.
    async fn deliver(&self, record: PushRecord) {
        let (task_id, id, version) = (record.task_id.clone(), record.id.clone(), record.version);
        let progress = self.step(record).await;
        match self.store.commit(&task_id, &id, version, progress).await {
            Ok(_) => {}
            // Replaced or deleted while it was being delivered: the newer write wins, and this
            // round's progress is dropped. (An in-flight request may still have reached the
            // webhook once: at least once, never exactly once.)
            Err(PushStoreError::NotFound | PushStoreError::Conflict) => {
                tracing::debug!(
                    task_id,
                    config_id = id,
                    "a push config changed during delivery; the progress is dropped"
                );
            }
            Err(err) => {
                // The lease runs out and the config is claimed again; the event is sent again.
                tracing::warn!(task_id, config_id = id, error = %adam_error::report(&err), "recording push progress failed");
            }
        }
    }

    /// The progress to record for `record` after one delivery attempt.
    async fn step(&self, record: PushRecord) -> PushProgress {
        let now = Utc::now();
        let idle = |cursor: PushCursor, state: PushState| PushProgress {
            state,
            cursor,
            attempts: 0,
            last_error: None,
            next_attempt_at: now + to_chrono(self.options.poll_interval),
        };
        let gave_up = |cursor: PushCursor, reason: String| {
            tracing::warn!(task_id = %record.task_id, config_id = %record.id, reason, "push notifications abandoned for this config");
            PushProgress {
                state: PushState::GaveUp,
                cursor,
                attempts: record.attempts,
                last_error: Some(reason),
                next_attempt_at: now,
            }
        };

        let mut cursor = record.cursor.clone();
        let caller = Caller::new(record.owner.clone());
        let task = match self.backend.get(&caller, &record.task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => return gave_up(cursor, "the task no longer exists".into()),
            Err(err) => {
                // Not the webhook's fault: look again later, and do not count it.
                tracing::warn!(task_id = %record.task_id, error = %adam_error::report(&err), "reading the task for a push notification failed");
                return self.retry_without_counting(&record, cursor, now);
            }
        };

        let Some(event) = cursor.next_event(&task) else {
            let state = if is_final(&task) {
                PushState::Done
            } else {
                PushState::Active
            };
            return idle(cursor, state);
        };

        match self.sender.send(&record.config, &event).await {
            Ok(()) => {
                cursor.acknowledge(&event);
                // Heard everything and the task is over: done now, no extra round.
                let state = if cursor.next_event(&task).is_none() && is_final(&task) {
                    PushState::Done
                } else {
                    PushState::Active
                };
                PushProgress {
                    state,
                    cursor,
                    attempts: 0,
                    last_error: None,
                    // Due at once: the next event does not wait for a poll.
                    next_attempt_at: now,
                }
            }
            Err(SendError::Permanent(reason)) => gave_up(cursor, reason),
            Err(SendError::Retryable(reason)) => self.failed(&record, cursor, event, reason, now),
        }
    }

    fn failed(
        &self,
        record: &PushRecord,
        mut cursor: PushCursor,
        event: StreamResponse,
        reason: String,
        now: DateTime<Utc>,
    ) -> PushProgress {
        cursor.fail(event, now);
        let attempts = record.attempts.saturating_add(1);
        let since = cursor.failing_since.unwrap_or(now);
        if now - since >= to_chrono(self.options.give_up_after) {
            tracing::warn!(task_id = %record.task_id, config_id = %record.id, attempts, reason, "push notifications abandoned: the webhook kept failing");
            return PushProgress {
                state: PushState::GaveUp,
                cursor,
                attempts,
                last_error: Some(format!("gave up after {attempts} attempts: {reason}")),
                next_attempt_at: now,
            };
        }
        tracing::debug!(task_id = %record.task_id, config_id = %record.id, attempts, reason, "push notification failed; will retry");
        PushProgress {
            state: PushState::Active,
            cursor,
            attempts,
            last_error: Some(reason),
            next_attempt_at: now
                + to_chrono(
                    self.options
                        .backoff(attempts, now.timestamp_subsec_millis()),
                ),
        }
    }

    fn retry_without_counting(
        &self,
        record: &PushRecord,
        cursor: PushCursor,
        now: DateTime<Utc>,
    ) -> PushProgress {
        PushProgress {
            state: PushState::Active,
            cursor,
            attempts: record.attempts,
            last_error: record.last_error.clone(),
            next_attempt_at: now
                + to_chrono(
                    self.options
                        .backoff(record.attempts.max(1), now.timestamp_subsec_millis()),
                ),
        }
    }
}

fn to_chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        let o = PushDeliveryOptions::new()
            .with_backoff(Duration::from_secs(1), Duration::from_secs(60));
        let waits: Vec<u64> = [1, 2, 3, 4, 5, 6, 7, 40]
            .into_iter()
            .map(|n| o.backoff(n, 0).as_secs())
            .collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
        // Jitter adds at most a fifth.
        assert!(o.backoff(3, 199) <= Duration::from_millis(4800));
        assert!(o.backoff(3, 199) >= Duration::from_secs(4));
    }
}
