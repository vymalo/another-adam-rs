//! The in-process [`Admission`]: one ceiling, held by one process.
//!
//! [ADR 0065](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md)
//! decision 3. Every pool is a `tokio::sync::Semaphore` of its limit:
//! [`Pool::CommandMemory`]'s permits are MiB, so its semaphore is weighted; the rest are one
//! permit per call, run or process. Each pool also keeps one semaphore per conversation
//! ([`Budget::per_conversation_share`]), so no conversation holds more than its share while
//! another waits. When the budget names [`tokens_per_minute`](Budget::tokens_per_minute), one
//! bucket refills continuously and a model call charges its estimate.
//!
//! Nothing is persisted and nothing crosses a process: a second replica counting the same person
//! is `StoreAdmission`, which ADR 0065 leaves undecided. Two replicas each admit up to the whole
//! budget, so a deployment that runs several workers sets each one's ceiling to its share.
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use adam_admission_local::LocalAdmission;
//! use adam_budget::{Budget, DynAdmission, Ratio};
//!
//! let budget = Budget::builder()
//!     .runs(2)
//!     .tool_calls(4)
//!     .model_calls(2)
//!     .per_conversation_share(Ratio::HALF)
//!     .build();
//! let admission: DynAdmission = Arc::new(LocalAdmission::new(budget));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use adam_budget::{
    Admission, Ask, Budget, ConversationId, Permit, Pool, PoolUsage, Refused, TokenUsage, Usage,
};
use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How long [`Refused::Busy`] says to wait when a pool stayed full for the ask's whole wait: a
/// short, honest hint, because the holder that frees first is not known here. A caller parks the
/// run and asks again ([ADR 0065] decision 4).
///
/// [ADR 0065]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0065-resource-governance-admission-pools-and-os-limits.md
pub const RETRY_AFTER: Duration = Duration::from_millis(250);

/// The number of millionths that count as one token in the bucket's arithmetic.
const MICRO: u128 = 1_000_000;

/// The in-process admission: a semaphore per pool and per conversation, and one token bucket.
pub struct LocalAdmission {
    budget: Budget,
    pools: Vec<PoolSlot>,
    bucket: Option<Mutex<Bucket>>,
}

struct PoolSlot {
    pool: Pool,
    /// The ceiling, in the pool's unit.
    limit: u64,
    /// The most one conversation holds, in the pool's unit.
    share: u64,
    sem: Arc<Semaphore>,
    /// One semaphore per conversation that has ever asked, so a conversation's share is enforced.
    conversations: Mutex<HashMap<ConversationId, ConversationSlot>>,
    /// The asks waiting for room in this pool.
    waiting: AtomicU32,
}

struct ConversationSlot {
    sem: Arc<Semaphore>,
    capacity: u64,
}

impl LocalAdmission {
    /// An admission with `budget`'s ceilings: one pool per [`Pool::ALL`], each of its limit
    /// ([`Budget::limit`], MiB for [`Pool::CommandMemory`]), and a bucket when the budget names
    /// tokens per minute.
    #[must_use]
    pub fn new(budget: Budget) -> Self {
        let pools = Pool::ALL
            .into_iter()
            .map(|pool| {
                let limit = budget.limit(pool);
                let share = budget.per_conversation_share.apply_u64(limit).max(1);
                PoolSlot {
                    pool,
                    limit,
                    share: share.min(limit.max(1)),
                    sem: Arc::new(Semaphore::new(permits(limit))),
                    conversations: Mutex::new(HashMap::new()),
                    waiting: AtomicU32::new(0),
                }
            })
            .collect();
        let bucket = budget.tokens_per_minute.map(|capacity| {
            Mutex::new(Bucket {
                capacity,
                available_micro: u128::from(capacity) * MICRO,
                at: Instant::now(),
            })
        });
        Self {
            budget,
            pools,
            bucket,
        }
    }

    /// The ceiling this admission counts against.
    #[must_use]
    pub fn budget(&self) -> Budget {
        self.budget.clone()
    }

    fn slot(&self, pool: Pool) -> &PoolSlot {
        // Built from `Pool::ALL`, one slot per pool, in that order.
        &self.pools[Pool::ALL
            .iter()
            .position(|known| *known == pool)
            .unwrap_or_default()]
    }

    async fn acquire_pool(
        &self,
        slot: &PoolSlot,
        units: u64,
        wait: Duration,
    ) -> Option<OwnedSemaphorePermit> {
        let asked = u32::try_from(units).unwrap_or(u32::MAX);
        if wait.is_zero() {
            return slot.sem.clone().try_acquire_many_owned(asked).ok();
        }
        match tokio::time::timeout(wait, slot.sem.clone().acquire_many_owned(asked)).await {
            Ok(Ok(permit)) => Some(permit),
            Ok(Err(_)) | Err(_) => None,
        }
    }

    /// The conversation's own seat: the pool's ceiling bounds the process, the share bounds one
    /// conversation.
    ///
    /// An ask larger than the share is let through without a seat: a command's memory reservation
    /// cannot be split, and the pool's own permit still bounds the process. Such an ask is the
    /// one case where a conversation may hold more than its share, and only for the memory pool.
    async fn acquire_seat(
        &self,
        slot: &PoolSlot,
        ask: &Ask,
        units: u64,
        wait: Duration,
    ) -> Result<Option<OwnedSemaphorePermit>, Refused> {
        let capacity = slot.share.max(units).min(slot.limit.max(1));
        let sem = {
            let mut conversations = slot
                .conversations
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let entry = conversations
                .entry(ask.conversation.clone())
                .or_insert_with(|| ConversationSlot {
                    sem: Arc::new(Semaphore::new(permits(capacity))),
                    capacity,
                });
            if entry.capacity < units {
                return Ok(None);
            }
            Arc::clone(&entry.sem)
        };
        let asked = u32::try_from(units).unwrap_or(u32::MAX);
        let acquired = if wait.is_zero() {
            sem.try_acquire_many_owned(asked).ok()
        } else {
            match tokio::time::timeout(wait, sem.acquire_many_owned(asked)).await {
                Ok(Ok(permit)) => Some(permit),
                Ok(Err(_)) | Err(_) => None,
            }
        };
        acquired.map(Some).ok_or(Refused::Busy {
            pool: slot.pool,
            retry_after: Some(RETRY_AFTER),
        })
    }

    /// Charge `tokens` to the bucket, waiting up to `wait` for enough to refill.
    async fn take_tokens(
        &self,
        bucket: &Mutex<Bucket>,
        tokens: u32,
        wait: Duration,
    ) -> Result<(), Refused> {
        let need = u128::from(tokens) * MICRO;
        let started = Instant::now();
        loop {
            let shortfall = {
                let mut bucket = bucket.lock().unwrap_or_else(PoisonError::into_inner);
                bucket.refill(Instant::now());
                if need > u128::from(bucket.capacity) * MICRO {
                    return Err(Refused::TooLarge {
                        what: "a model call's tokens",
                        asked: u64::from(tokens),
                        limit: u64::from(bucket.capacity),
                    });
                }
                if bucket.available_micro >= need {
                    bucket.available_micro -= need;
                    return Ok(());
                }
                let missing = need - bucket.available_micro;
                // Microseconds until `missing` tokens refill, at the bucket's rate.
                let micros = missing * 60 / u128::from(bucket.capacity).max(1);
                Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX))
            };
            if started.elapsed() + shortfall > wait {
                return Err(Refused::TokensPerMinute {
                    retry_after: shortfall,
                });
            }
            tokio::time::sleep(shortfall.max(Duration::from_millis(1))).await;
        }
    }
}

#[async_trait]
impl Admission for LocalAdmission {
    async fn admit(&self, ask: Ask) -> Result<Permit, Refused> {
        let pool = ask.pool;
        let units = u64::from(ask.units.max(1));
        let slot = self.slot(pool);
        if units > slot.limit {
            return Err(Refused::TooLarge {
                what: pool.unit(),
                asked: units,
                limit: slot.limit,
            });
        }
        if pool == Pool::ModelCalls
            && let Some(bucket) = &self.bucket
            && ask.tokens > 0
        {
            self.take_tokens(bucket, ask.tokens, ask.wait).await?;
        }
        // The conversation's seat first, so that a conversation waiting for its own share never
        // holds a pool permit another conversation needs.
        let seat = self.acquire_seat(slot, &ask, units, ask.wait).await?;
        // Counting it as a waiter for as long as this future lives, so a cancelled ask stops
        // counting too (a `fetch_sub` after the await would never run for it).
        let waiting = Waiting::of(slot);
        let permit = self.acquire_pool(slot, units, ask.wait).await;
        drop(waiting);
        match permit {
            Some(permit) => Ok(Permit::new(
                pool,
                ask.conversation,
                u32::try_from(units).unwrap_or(u32::MAX),
                move || drop((permit, seat)),
            )),
            None => {
                drop(seat);
                Err(Refused::Busy {
                    pool,
                    retry_after: Some(RETRY_AFTER),
                })
            }
        }
    }

    fn snapshot(&self) -> Usage {
        let pools = self.pools.iter().map(|slot| {
            let held = slot
                .limit
                .saturating_sub(slot.sem.available_permits() as u64);
            PoolUsage::new(
                slot.pool,
                self.budget.ceiling(slot.pool),
                held,
                slot.waiting.load(Ordering::SeqCst),
            )
        });
        let tokens = self.bucket.as_ref().map(|bucket| {
            let mut bucket = bucket.lock().unwrap_or_else(PoisonError::into_inner);
            bucket.refill(Instant::now());
            TokenUsage::new(
                bucket.capacity,
                saturating_u32(u64::try_from(bucket.available_micro / MICRO).unwrap_or(u64::MAX)),
            )
        });
        Usage::new(pools, tokens)
    }
}

/// One ask, counted as waiting until it is dropped.
struct Waiting<'a>(&'a AtomicU32);

impl<'a> Waiting<'a> {
    fn of(slot: &'a PoolSlot) -> Self {
        slot.waiting.fetch_add(1, Ordering::SeqCst);
        Self(&slot.waiting)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One bucket: `capacity` tokens per minute, refilled continuously, spent by model calls.
struct Bucket {
    capacity: u32,
    /// Tokens available, in millionths, so that a fraction of a token is not lost.
    available_micro: u128,
    /// When `available_micro` was last refilled.
    at: Instant,
}

impl Bucket {
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_micros();
        if elapsed == 0 {
            return;
        }
        // `capacity` tokens per minute is `capacity / 60` millionths of a token per microsecond.
        let added = elapsed * u128::from(self.capacity) / 60;
        let capacity = u128::from(self.capacity) * MICRO;
        let gained = (self.available_micro + added).min(capacity);
        if gained > self.available_micro {
            // Advance by exactly the time those tokens took, so the fraction is not lost.
            let took = (gained - self.available_micro) * 60 / u128::from(self.capacity);
            self.at += Duration::from_micros(u64::try_from(took).unwrap_or(u64::MAX));
            self.available_micro = gained;
        }
    }
}

/// `limit` as semaphore permits. `Semaphore::new` panics over [`Semaphore::MAX_PERMITS`], and a
/// budget's identity is very large, so the ceiling is clamped there rather than panicking.
fn permits(limit: u64) -> usize {
    usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .min(Semaphore::MAX_PERMITS)
}

fn saturating_u32(limit: u64) -> u32 {
    u32::try_from(limit).unwrap_or(u32::MAX)
}

impl std::fmt::Debug for LocalAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAdmission")
            .field("budget", &self.budget)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adam_budget::Ratio;

    #[test]
    fn a_conversations_share_of_a_pool_is_at_least_one() {
        let budget = Budget::builder()
            .model_calls(1)
            .per_conversation_share(Ratio::HALF)
            .build();
        let admission = LocalAdmission::new(budget);
        assert_eq!(admission.slot(Pool::ModelCalls).share, 1);
    }

    #[test]
    fn the_budget_is_the_one_it_counts_against() {
        let budget = Budget::builder()
            .runs(2)
            .tool_calls(4)
            .model_calls(3)
            .tokens_per_minute(1_000)
            .build();
        assert_eq!(LocalAdmission::new(budget.clone()).budget(), budget);
    }

    #[test]
    fn the_bucket_refills_continuously() {
        let mut bucket = Bucket {
            capacity: 60_000,
            available_micro: 0,
            at: Instant::now(),
        };
        bucket.refill(bucket.at + Duration::from_secs(30));
        assert_eq!(bucket.available_micro, 30_000 * MICRO);
        bucket.refill(bucket.at + Duration::from_secs(300));
        assert_eq!(
            bucket.available_micro,
            60_000 * MICRO,
            "never over the minute"
        );
    }
}
