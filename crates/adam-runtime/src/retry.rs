//! Retry policy for [`AgentError::Transient`](crate::AgentError::Transient)
//! with a `retry_after` hint ([`AgentError::with_retry_after`](crate::AgentError::with_retry_after)).

use std::time::Duration;

/// The longest wait a retry hint ([`AgentError::with_retry_after`]) can ask
/// for: 24 hours. Longer hints are capped to it, so a misbehaving upstream
/// cannot park a run for years.
///
/// [`AgentError::with_retry_after`]: crate::AgentError::with_retry_after
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Exponential backoff for transient failures.
///
/// `max_attempts` counts every try of a transition, the first included, so
/// `max_attempts = 1` never retries. After a failed try the runtime commits
/// the run as runnable with `wake_at = now + backoff(failures)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryPolicy {
    /// Total tries allowed for one transition (first try included).
    pub max_attempts: u32,
    /// Delay after the first failure.
    pub initial_backoff: Duration,
    /// Upper bound of any delay.
    pub max_backoff: Duration,
    /// Growth factor per further failure (2.0 doubles).
    pub multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            multiplier: 2.0,
        }
    }
}

impl RetryPolicy {
    /// Never retry: the first transient error fails the run.
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// The delay before the retry that follows the `failures`-th consecutive
    /// failure when the error carried a minimum wait: the larger of
    /// [`backoff`](Self::backoff) and `at_least` (itself capped at
    /// [`MAX_RETRY_AFTER`]).
    pub fn delay_with_hint(&self, failures: u32, at_least: Duration) -> Duration {
        self.backoff(failures).max(at_least.min(MAX_RETRY_AFTER))
    }

    /// The delay after the `failures`-th consecutive failure (1-based):
    /// `initial * multiplier^(failures - 1)`, capped at `max_backoff`.
    pub fn backoff(&self, failures: u32) -> Duration {
        let exp = i32::try_from(failures.saturating_sub(1)).unwrap_or(i32::MAX);
        let secs = self.initial_backoff.as_secs_f64() * self.multiplier.max(0.0).powi(exp);
        let cap = self.max_backoff.as_secs_f64();
        if secs.is_nan() || secs >= cap {
            return self.max_backoff;
        }
        Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or(self.max_backoff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let p = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(500),
            multiplier: 2.0,
        };
        assert_eq!(p.backoff(1), Duration::from_millis(100));
        assert_eq!(p.backoff(2), Duration::from_millis(200));
        assert_eq!(p.backoff(3), Duration::from_millis(400));
        assert_eq!(p.backoff(4), Duration::from_millis(500));
        assert_eq!(p.backoff(u32::MAX), Duration::from_millis(500));
    }

    #[test]
    fn a_hint_lengthens_the_wait_but_never_shortens_it() {
        let p = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(10),
            multiplier: 2.0,
        };
        assert_eq!(
            p.delay_with_hint(1, Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert_eq!(
            p.delay_with_hint(1, Duration::from_millis(1)),
            Duration::from_secs(2)
        );
        assert_eq!(p.delay_with_hint(3, Duration::ZERO), p.backoff(3));
        // Above the policy's own cap is fine: the hint wins.
        assert_eq!(
            p.delay_with_hint(9, Duration::from_secs(600)),
            Duration::from_secs(600)
        );
        // But not beyond the absolute cap.
        assert_eq!(p.delay_with_hint(1, Duration::MAX), MAX_RETRY_AFTER);
    }

    #[test]
    fn degenerate_multipliers_do_not_panic() {
        let mut p = RetryPolicy {
            multiplier: f64::NAN,
            ..RetryPolicy::default()
        };
        let _ = p.backoff(3);
        p.multiplier = -3.0;
        let _ = p.backoff(3);
        p.multiplier = f64::INFINITY;
        assert_eq!(p.backoff(3), p.max_backoff);
    }

    mod prop {
        use proptest::prelude::*;

        use super::*;

        fn policy(initial_ms: u64, max_ms: u64, multiplier: f64) -> RetryPolicy {
            RetryPolicy {
                max_attempts: u32::MAX,
                initial_backoff: Duration::from_millis(initial_ms),
                max_backoff: Duration::from_millis(max_ms),
                multiplier,
            }
        }

        proptest! {
            /// However the policy is set (any multiplier, even below 1), no
            /// delay exceeds `max_backoff`.
            #[test]
            fn prop_backoff_is_capped(
                initial in 0u64..100_000,
                max in 0u64..1_000_000,
                multiplier in 0.0f64..50.0,
                failures in 0u32..u32::MAX,
            ) {
                let p = policy(initial, max, multiplier);
                prop_assert!(p.backoff(failures) <= p.max_backoff);
            }

            /// With a growth factor of at least 1 the delay never shrinks
            /// from one failure to the next, and the first is the initial
            /// backoff unless the cap is lower.
            #[test]
            fn prop_backoff_monotone_and_capped(
                initial in 0u64..100_000,
                max in 0u64..1_000_000,
                multiplier in 1.0f64..50.0,
                failures in 1u32..500,
            ) {
                let p = policy(initial, max, multiplier);
                prop_assert!(p.backoff(failures) <= p.backoff(failures + 1));
                prop_assert!(p.backoff(failures + 1) <= p.max_backoff);
                prop_assert_eq!(
                    p.backoff(1),
                    Duration::from_millis(initial).min(Duration::from_millis(max))
                );
            }

            /// A retry hint is a floor: never below the backoff, never below
            /// the (capped) hint, and never above the larger of the policy's
            /// cap and the absolute hint cap.
            #[test]
            fn prop_hint_is_a_floor(
                initial in 0u64..100_000,
                max in 0u64..1_000_000,
                multiplier in 0.0f64..50.0,
                failures in 1u32..1000,
                hint_ms in any::<u64>(),
            ) {
                let p = policy(initial, max, multiplier);
                let hint = Duration::from_millis(hint_ms);
                let d = p.delay_with_hint(failures, hint);
                prop_assert!(d >= p.backoff(failures));
                prop_assert!(d >= hint.min(MAX_RETRY_AFTER));
                prop_assert!(d <= p.max_backoff.max(MAX_RETRY_AFTER));
            }
        }
    }
}
