//! Retry policy for [`AgentError::Transient`](crate::AgentError::Transient).

use std::time::Duration;

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
}
