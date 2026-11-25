//! The sweep of finished runs: a worker component that deletes what is older than the retention.
//!
//! Every [`RetentionSettings::every`] it calls [`Store::purge_finished`] for the served agent with a
//! cutoff of now minus the retention (`RUN_RETENTION_DAYS`, 90 by default, `off` keeps everything).
//! The store deletes only runs that are done or failed, with their journals; a run that is open is
//! never touched. Several workers may sweep at once: the store hands each the rows it could lock.
//! A failed call is logged and tried again at the next tick; it never stops the process.
//! Counts are logged, with no run or person label.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use adam_core::{Store, Timestamp};
use adam_error::BoxError;
use jiff::SignedDuration;
use tokio_util::sync::CancellationToken;

/// Days a finished run is kept when `RUN_RETENTION_DAYS` is unset.
pub const DEFAULT_RETENTION_DAYS: u64 = 90;
/// The most `RUN_RETENTION_DAYS` may say.
pub const MAX_RETENTION_DAYS: u64 = 36_500;
/// Seconds between two sweeps when `RUN_RETENTION_SWEEP_SECS` is unset.
pub const DEFAULT_SWEEP_SECS: u64 = 3600;

/// `RUN_RETENTION_DAYS` and `RUN_RETENTION_SWEEP_SECS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionSettings {
    /// How long a finished run is kept; `None` (`RUN_RETENTION_DAYS=off`) deletes nothing.
    pub keep: Option<Duration>,
    /// The time between two sweeps.
    pub every: Duration,
}

impl Default for RetentionSettings {
    fn default() -> Self {
        Self {
            keep: Some(Duration::from_secs(DEFAULT_RETENTION_DAYS * 86_400)),
            every: Duration::from_secs(DEFAULT_SWEEP_SECS),
        }
    }
}

impl RetentionSettings {
    /// Read the two variables; a bad value is a problem and the default.
    pub fn parse(lookup: &impl Fn(&str) -> Option<String>, problems: &mut Vec<String>) -> Self {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let default = Self::default();
        let keep = match get("RUN_RETENTION_DAYS") {
            None => default.keep,
            Some(raw) if raw.trim().eq_ignore_ascii_case("off") => None,
            Some(raw) => match raw.trim().parse::<u64>() {
                // Longer than the minute a child run looks at its parent (`Store::purge_finished`).
                Ok(days) if (1..=MAX_RETENTION_DAYS).contains(&days) => {
                    Some(Duration::from_secs(days * 86_400))
                }
                _ => {
                    problems.push(format!(
                        "RUN_RETENTION_DAYS must be off or between 1 and {MAX_RETENTION_DAYS}, got {raw:?}"
                    ));
                    default.keep
                }
            },
        };
        let every = match get("RUN_RETENTION_SWEEP_SECS") {
            None => default.every,
            Some(raw) => match raw.trim().parse::<u64>() {
                Ok(secs) if (60..=86_400).contains(&secs) => Duration::from_secs(secs),
                _ => {
                    problems.push(format!(
                        "RUN_RETENTION_SWEEP_SECS must be between 60 and 86400, got {raw:?}"
                    ));
                    default.every
                }
            },
        };
        Self { keep, every }
    }
}

/// Runs deleted by this process since it started.
pub type Deleted = Arc<AtomicU64>;

/// One sweep at `now`: delete the finished runs of `agent` last updated before `now - keep`.
/// Returns the number deleted, 0 when `keep` is `None`.
pub async fn sweep_once(
    store: &dyn Store,
    agent: &str,
    keep: Option<Duration>,
    now: Timestamp,
) -> Result<u64, adam_core::StoreError> {
    let Some(keep) = keep else { return Ok(0) };
    store.purge_finished(agent, cutoff(now, keep)).await
}

/// The instant before which a finished run is deleted.
#[must_use]
pub fn cutoff(now: Timestamp, keep: Duration) -> Timestamp {
    now - SignedDuration::try_from(keep).unwrap_or(SignedDuration::MAX)
}

/// The worker component: sweep once at start and then every `settings.every`, until `stop`.
pub async fn run(
    store: Arc<dyn Store>,
    agent: String,
    settings: RetentionSettings,
    deleted: Deleted,
    stop: CancellationToken,
) -> Result<(), BoxError> {
    if settings.keep.is_none() {
        tracing::info!("run retention is off: finished runs are kept");
        stop.cancelled().await;
        return Ok(());
    }
    loop {
        match sweep_once(store.as_ref(), &agent, settings.keep, Timestamp::now()).await {
            Ok(0) => {}
            Ok(n) => {
                let total = deleted.fetch_add(n, Ordering::Relaxed) + n;
                tracing::info!(
                    deleted = n,
                    total,
                    "deleted finished runs past their retention"
                );
            }
            Err(e) => tracing::warn!(error = %adam_error::report(&e), "the retention sweep failed"),
        }
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            () = tokio::time::sleep(settings.every) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> (RetentionSettings, Vec<String>) {
        let mut problems = Vec::new();
        let s = RetentionSettings::parse(
            &|n| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, v)| (*v).to_owned())
            },
            &mut problems,
        );
        (s, problems)
    }

    #[test]
    fn unset_keeps_ninety_days() {
        let (s, p) = parse(&[]);
        assert!(p.is_empty());
        assert_eq!(s.keep, Some(Duration::from_secs(90 * 86_400)));
    }

    #[test]
    fn a_value_overrides_and_off_keeps_everything() {
        assert_eq!(
            parse(&[("RUN_RETENTION_DAYS", "7")]).0.keep,
            Some(Duration::from_secs(7 * 86_400))
        );
        assert_eq!(parse(&[("RUN_RETENTION_DAYS", "OFF")]).0.keep, None);
    }

    #[test]
    fn bad_values_are_problems() {
        for v in ["0", "-1", "x", "99999999"] {
            let (s, p) = parse(&[("RUN_RETENTION_DAYS", v)]);
            assert_eq!(p.len(), 1, "{v}");
            assert_eq!(s.keep, RetentionSettings::default().keep);
        }
        assert_eq!(parse(&[("RUN_RETENTION_SWEEP_SECS", "5")]).1.len(), 1);
    }

    #[test]
    fn the_cutoff_separates_ninety_days_from_eighty_nine() {
        let now = Timestamp::now();
        let keep = RetentionSettings::default().keep.unwrap();
        let day = SignedDuration::from_hours(24);
        assert!(now - day * 90 < cutoff(now, keep) || now - day * 90 == cutoff(now, keep));
        assert!(now - day * 89 > cutoff(now, keep));
        assert!(now - day * 91 < cutoff(now, keep));
    }

    #[tokio::test]
    async fn off_deletes_nothing() {
        let store = adam_core::MemoryStore::default();
        let n = sweep_once(&store, "a", None, Timestamp::now())
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
}
