//! Retry policy for [`AgentError::Transient`](crate::AgentError::Transient)
//! with a `retry_after` hint ([`AgentError::with_retry_after`](crate::AgentError::with_retry_after)).

use std::collections::BTreeMap;
use std::time::Duration;

use adam_error::ErrorClass;
use serde::{Deserialize, Serialize};

/// The longest wait a retry hint ([`AgentError::with_retry_after`]) can ask
/// for: 24 hours. Longer hints are capped to it, so a misbehaving upstream
/// cannot park a run for years.
///
/// [`AgentError::with_retry_after`]: crate::AgentError::with_retry_after
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The retryable [`ErrorClass`]es, by the names a policy writes them with.
const RETRYABLE: [(ErrorClass, &str); 3] = [
    (ErrorClass::Transient, "transient"),
    (ErrorClass::RateLimited, "rate-limited"),
    (ErrorClass::Conflict, "conflict"),
];

/// The name of a retryable class (`transient`, `rate-limited`, `conflict`), or its `Debug` form for
/// another.
pub(crate) fn class_name(class: ErrorClass) -> String {
    RETRYABLE
        .iter()
        .find(|(c, _)| *c == class)
        .map_or_else(|| format!("{class:?}"), |(_, name)| (*name).to_owned())
}

/// A set of the retryable [`ErrorClass`]es (`Transient`, `RateLimited`, `Conflict`): the ones a
/// [`RetryPolicy`] retries ([ADR 0061](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0061-a-step-names-its-retry-policy.md)).
/// A class that is not retryable is never in it: such an error is already permanent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RetryClasses(u8);

impl RetryClasses {
    /// All three retryable classes: what a policy retries by default, and what the runtime
    /// retried before policies named classes.
    pub const ALL: Self = Self(0b111);
    /// No class: a policy with it retries nothing.
    pub const NONE: Self = Self(0);

    /// The retryable ones among `classes`; the others are left out.
    pub fn of(classes: impl IntoIterator<Item = ErrorClass>) -> Self {
        classes.into_iter().fold(Self::NONE, |set, class| {
            match RETRYABLE.iter().position(|(c, _)| *c == class) {
                Some(i) => Self(set.0 | (1 << i)),
                None => set,
            }
        })
    }

    /// Whether `class` is in the set.
    pub fn contains(self, class: ErrorClass) -> bool {
        RETRYABLE
            .iter()
            .position(|(c, _)| *c == class)
            .is_some_and(|i| self.0 & (1 << i) != 0)
    }

    /// The set's classes by name (`transient`, `rate-limited`, `conflict`), in that order.
    pub fn names(self) -> Vec<&'static str> {
        RETRYABLE
            .iter()
            .enumerate()
            .filter(|(i, _)| self.0 & (1 << i) != 0)
            .map(|(_, (_, name))| *name)
            .collect()
    }

    /// The set of `names`; `Err` names the first one that is not a retryable class's.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        names.into_iter().try_fold(Self::NONE, |set, name| {
            match RETRYABLE.iter().position(|(_, n)| *n == name) {
                Some(i) => Ok(Self(set.0 | (1 << i))),
                None => Err(name.to_owned()),
            }
        })
    }

    /// The set of `names`, leaving out a name this build does not know: what a stored policy is
    /// read with, so that it retries less, never more.
    pub fn from_names_lenient<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        names.into_iter().fold(Self::NONE, |set, name| {
            match RETRYABLE.iter().position(|(_, n)| *n == name) {
                Some(i) => Self(set.0 | (1 << i)),
                None => set,
            }
        })
    }
}

impl Default for RetryClasses {
    fn default() -> Self {
        Self::ALL
    }
}

/// Exponential backoff for transient failures.
///
/// `max_attempts` counts every try of a transition, the first included, so
/// `max_attempts = 1` never retries. After a failed try the runtime commits
/// the run as runnable with `wake_at = now + backoff(failures)`. A transient error whose class
/// the policy does not list in [`retry_on`](Self::retry_on) fails the run at once.
///
/// `#[non_exhaustive]`: build one with [`RetryPolicy::new`] and the `with_` methods, or start from
/// [`Default`].
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct RetryPolicy {
    /// Total tries allowed for one transition (first try included).
    pub max_attempts: u32,
    /// Delay after the first failure.
    pub initial_backoff: Duration,
    /// Upper bound of any delay.
    pub max_backoff: Duration,
    /// Growth factor per further failure (2.0 doubles).
    pub multiplier: f64,
    /// The classes it retries; all three retryable ones by default.
    pub retry_on: RetryClasses,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            multiplier: 2.0,
            retry_on: RetryClasses::ALL,
        }
    }
}

impl RetryPolicy {
    /// The default policy with `max_attempts` tries (at least one).
    pub fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            ..Self::default()
        }
    }

    /// With the delay after the first failure and the bound of any delay (raised to `initial`
    /// when it is shorter).
    #[must_use]
    pub fn with_backoff(mut self, initial: Duration, max: Duration) -> Self {
        self.initial_backoff = initial;
        self.max_backoff = max.max(initial);
        self
    }

    /// With the growth factor per further failure. A value that is not finite and positive is
    /// refused: the policy keeps the factor it had (a stored policy must decode, and `NaN` does
    /// not survive JSON).
    #[must_use]
    pub fn with_multiplier(mut self, multiplier: f64) -> Self {
        if multiplier.is_finite() && multiplier > 0.0 {
            self.multiplier = multiplier;
        }
        self
    }

    /// With the classes it retries.
    #[must_use]
    pub fn with_retry_on(mut self, classes: RetryClasses) -> Self {
        self.retry_on = classes;
        self
    }

    /// Whether it retries an error of `class`.
    pub fn retries(&self, class: ErrorClass) -> bool {
        self.retry_on.contains(class)
    }

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

/// The most consecutive failed transitions a run is retried through, whatever their keys: two
/// failing steps that take turns, each under its own policy, cannot retry for ever (ADR 0061,
/// decision 4).
pub const MAX_CONSECUTIVE_FAILURES: u32 = 100;

/// The key an error that names no policy counts under, with the runtime's policy.
pub const RUNTIME_RETRY_KEY: &str = "runtime";

/// A step's own retry policy, under the key its failures are counted with (ADR 0061, decision 3):
/// what [`AgentError::with_retry`](crate::AgentError::with_retry) attaches to a transient error.
#[derive(Clone, Debug, PartialEq)]
pub struct StepRetry {
    /// What the step's failures are counted under: the step's name, or `tool:<name>` for a tool.
    pub key: String,
    /// The policy of that key, recorded at its first failure.
    pub policy: RetryPolicy,
}

impl StepRetry {
    /// `policy` under `key`.
    pub fn new(key: impl Into<String>, policy: RetryPolicy) -> Self {
        Self {
            key: key.into(),
            policy,
        }
    }
}

/// A policy as a run records it: durations in integer milliseconds, classes by name (ADR 0061,
/// decision 7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredPolicy {
    max_attempts: u32,
    initial_backoff_ms: u64,
    max_backoff_ms: u64,
    multiplier: f64,
    retry_on: Vec<String>,
}

impl StoredPolicy {
    pub(crate) fn of(policy: &RetryPolicy) -> Self {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Self {
            max_attempts: policy.max_attempts,
            initial_backoff_ms: ms(policy.initial_backoff),
            max_backoff_ms: ms(policy.max_backoff),
            multiplier: policy.multiplier,
            retry_on: policy
                .retry_on
                .names()
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }
    }

    /// The policy again; a class name this build does not know is dropped, so it retries less,
    /// never more.
    pub(crate) fn policy(&self) -> RetryPolicy {
        RetryPolicy::new(self.max_attempts)
            .with_backoff(
                Duration::from_millis(self.initial_backoff_ms),
                Duration::from_millis(self.max_backoff_ms),
            )
            .with_multiplier(self.multiplier)
            .with_retry_on(RetryClasses::from_names_lenient(
                self.retry_on.iter().map(String::as_str),
            ))
    }
}

/// Reads a run's [`RetryRecord`]: one that does not decode is ignored (`None`), so the next
/// failure counts afresh, under the runtime's policy when it names none (decision 7).
pub(crate) fn lenient_record<'de, D>(deserializer: D) -> Result<Option<RetryRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// What a failing run records of its retries, beside its `attempt` (ADR 0061, decisions 4 and 5):
/// the failures per key and each key's policy, recorded at that key's first failure and read back,
/// never recomputed. A transition that succeeds clears it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetryRecord {
    #[serde(default)]
    pub(crate) failures: BTreeMap<String, u32>,
    #[serde(default)]
    pub(crate) policies: BTreeMap<String, StoredPolicy>,
}

impl RetryRecord {
    /// The record of a run that failed `attempt` times before it had one: those failures under
    /// [`RUNTIME_RETRY_KEY`], whose policy is recorded at the next failure.
    pub(crate) fn seeded(attempt: u32) -> Self {
        let mut record = Self::default();
        if attempt > 0 {
            record
                .failures
                .insert(RUNTIME_RETRY_KEY.to_owned(), attempt);
        }
        record
    }

    /// Counts one more failure under `key` and returns the count with the key's policy: the
    /// recorded one when there is one, else `policy`, which is recorded.
    pub(crate) fn fail(&mut self, key: &str, policy: &RetryPolicy) -> (u32, RetryPolicy) {
        let recorded = self
            .policies
            .entry(key.to_owned())
            .or_insert_with(|| StoredPolicy::of(policy))
            .policy();
        let failures = self.failures.entry(key.to_owned()).or_insert(0);
        *failures = failures.saturating_add(1);
        (*failures, recorded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_are_the_retryable_ones_by_name() {
        assert_eq!(RetryClasses::default(), RetryClasses::ALL);
        assert_eq!(
            RetryClasses::ALL.names(),
            ["transient", "rate-limited", "conflict"]
        );
        let only = RetryClasses::of([ErrorClass::RateLimited, ErrorClass::Invalid]);
        assert!(only.contains(ErrorClass::RateLimited));
        assert!(!only.contains(ErrorClass::Transient) && !only.contains(ErrorClass::Invalid));
        assert_eq!(only.names(), ["rate-limited"]);
        assert_eq!(
            RetryClasses::from_names(["conflict", "transient"]),
            Ok(RetryClasses::of([
                ErrorClass::Conflict,
                ErrorClass::Transient
            ]))
        );
        assert_eq!(
            RetryClasses::from_names(["invalid"]),
            Err("invalid".to_owned())
        );
        assert_eq!(
            RetryClasses::from_names_lenient(["conflict", "someday"]),
            RetryClasses::of([ErrorClass::Conflict])
        );
        assert!(!RetryClasses::NONE.contains(ErrorClass::Transient));
    }

    #[test]
    fn the_builder_keeps_a_policy_sound() {
        let p = RetryPolicy::new(0)
            .with_backoff(Duration::from_secs(2), Duration::from_secs(1))
            .with_multiplier(f64::NAN)
            .with_multiplier(-1.0)
            .with_retry_on(RetryClasses::of([ErrorClass::RateLimited]));
        assert_eq!(p.max_attempts, 1, "at least one try");
        assert_eq!(
            p.max_backoff,
            Duration::from_secs(2),
            "the bound is not under the first delay"
        );
        assert_eq!(
            p.multiplier, 2.0,
            "a factor that is not finite and positive is refused"
        );
        assert!(p.retries(ErrorClass::RateLimited) && !p.retries(ErrorClass::Transient));
        assert_eq!(RetryPolicy::new(3).with_multiplier(1.5).multiplier, 1.5);
        assert!(RetryPolicy::default().retries(ErrorClass::Conflict));
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let p = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(500),
            multiplier: 2.0,
            retry_on: RetryClasses::ALL,
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
            retry_on: RetryClasses::ALL,
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
                retry_on: RetryClasses::ALL,
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
