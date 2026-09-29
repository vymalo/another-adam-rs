//! Time source of the runtime, replaceable in tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};

/// Where the runtime reads "now" from: claiming due runs, computing retry
/// backoff, [`Ctx::now`](crate::Ctx::now).
pub trait Clock: Send + Sync + 'static {
    /// The current time, at millisecond precision.
    fn now(&self) -> DateTime<Utc>;
}

/// Shared clock handle.
pub type DynClock = Arc<dyn Clock>;

/// The wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        adam_core::store::now()
    }
}

/// The wall clock plus an offset that tests move forward with
/// [`advance`](Self::advance), so timers of hours fire instantly.
///
/// It is an offset rather than a frozen instant because stores stamp
/// `updated_at` with the real time; a frozen clock could sit behind a store
/// timestamp and make a runnable run look not yet due.
#[derive(Clone, Debug, Default)]
pub struct ManualClock {
    offset_ms: Arc<AtomicI64>,
}

impl ManualClock {
    /// A clock equal to the wall clock until advanced.
    pub fn new() -> Self {
        Self::default()
    }

    /// Move the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let ms = i64::try_from(by.as_millis()).unwrap_or(i64::MAX);
        self.offset_ms
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |o| {
                Some(o.saturating_add(ms))
            })
            .ok();
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        let offset = chrono::Duration::milliseconds(self.offset_ms.load(Ordering::SeqCst));
        adam_core::store::truncate_ms(
            Utc::now()
                .checked_add_signed(offset)
                .unwrap_or(DateTime::<Utc>::MAX_UTC),
        )
    }
}
