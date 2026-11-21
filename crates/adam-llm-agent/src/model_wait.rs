//! A run parked on the model: a call the endpoint refused, or could not reach, and the run that
//! waits to make it again ([ADR 0067] decisions 4, 6, 7 and 8).
//!
//! [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md

use std::time::Duration;

use adam_core::time::Timestamp;
use adam_model::{FIRST_PARK, ModelError, Park};
use serde::{Deserialize, Serialize};

/// The [`Inbound::kind`](adam_runtime::Inbound) of a **resume**: a message that wakes a run parked on
/// the model and adds nothing to its conversation ([ADR 0067] decision 4). The A2A server delivers
/// one for a message whose metadata holds `adam.resume: true`; the loop reads none of its payload.
/// It is the runtime's wake-only kind ([`adam_runtime::RESUME_KIND`]): one that lands while the run
/// works neither counts as a message that arrived nor reopens a run that finishes.
///
/// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
pub const MODEL_RESUME_KIND: &str = adam_runtime::RESUME_KIND;

/// How long parks on a timer in a row may last, from the first of them, unless
/// [`ModelParking::limit`] says otherwise: 24 hours.
pub const DEFAULT_MODEL_PARK_LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest step of the backoff of parks on a timer: 15 minutes. A park the endpoint asked to be
/// longer (`Retry-After`) is that long.
pub const MAX_MODEL_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// How a run waits on a model that refused it, or could not be reached
/// ([`LlmAgentBuilder::model_parking`](crate::LlmAgentBuilder::model_parking)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelParking {
    /// How long parks on a timer in a row may last, from the first, on the journaled clock: past
    /// it the run fails with the last park's reason (`MODEL_PARK_LIMIT` of `adam-service`).
    pub limit: Duration,
    /// Whether a call that could not reach the endpoint ([`ModelError::Unreachable`]) parks the run
    /// on a timer once the step's retries are spent, instead of failing it: for a provider that
    /// parks (`MODEL_AUTH=injector`), whose gateway may be away for longer than a retry waits.
    pub when_unreachable: bool,
}

impl Default for ModelParking {
    fn default() -> Self {
        Self {
            limit: DEFAULT_MODEL_PARK_LIMIT,
            when_unreachable: false,
        }
    }
}

impl ModelParking {
    /// Parks bounded by `limit`, and an unreachable endpoint failing the run as it always has.
    pub fn new(limit: Duration) -> Self {
        Self {
            limit,
            when_unreachable: false,
        }
    }

    /// Whether an unreachable endpoint parks the run (see [`when_unreachable`](Self::when_unreachable)).
    #[must_use]
    pub fn when_unreachable(mut self, parks: bool) -> Self {
        self.when_unreachable = parks;
        self
    }
}

/// How a run parked on the model wakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPark {
    /// On its own, at [`ModelWait::wake_at`]; a message wakes it sooner.
    Timer,
    /// Only when a message or a resume wakes it (a usage limit reached): it never fails on its own,
    /// and its deadline is paused, as for a question.
    UntilWoken,
}

/// The model call a run waits to make again ([`Conversation::model_wait`](crate::Conversation)).
///
/// It holds the reason, the status and the slug of the refusal, **never a header, a body or a
/// credential**: that is all a client is told, and all the journal holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelWait {
    /// What a person reads: "not authorized by the model gateway", "the model gateway is busy".
    pub reason: String,
    /// The refusal's HTTP status; `None` for an endpoint that could not be reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The refusal's problem type ([`Refusal::slug`](adam_model::Refusal::slug)), when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// How the run wakes.
    pub park: ModelPark,
    /// The journaled time of the first park of this kind in a row: the start of what
    /// [`ModelParking::limit`] bounds. A call that succeeds clears the whole wait.
    pub parked_since: Timestamp,
    /// How many parks on a timer in a row this is (the backoff's step); `0` for a park until woken.
    #[serde(default)]
    pub parks: u32,
    /// When a park on a timer wakes; `None` for a park until woken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<Timestamp>,
}

/// [`Conversation::model_wait`](crate::Conversation) read leniently: a wait this build cannot read
/// (a [`ModelPark`] a later build added, a member of another shape) reads as none, so the rest of
/// the conversation still loads. The run then makes its call when it next steps, which is what a
/// wake would do.
pub(crate) fn lenient<'de, D>(deserializer: D) -> Result<Option<ModelWait>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

/// A model call's failure that parks the run, as the journal records it: the status, the slug and
/// the wait, and nothing of what the endpoint said beyond them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordedPark {
    pub(crate) status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) slug: Option<String>,
    /// The wait the provider asked for, in milliseconds; absent for a park until woken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timer_ms: Option<u64>,
}

impl RecordedPark {
    /// The park a refusal carries ([`ModelError::Refused`]), if it is one.
    pub(crate) fn of(e: &ModelError) -> Option<Self> {
        let ModelError::Refused {
            status, slug, park, ..
        } = e
        else {
            return None;
        };
        let timer_ms = match park {
            Park::No => return None,
            Park::OnTimer(d) => Some(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            Park::UntilWoken => None,
        };
        Some(Self {
            status: *status,
            slug: slug.clone(),
            timer_ms,
        })
    }
}

/// The reason a person reads for a refusal, from its status and slug alone.
pub(crate) fn reason_of(status: Option<u16>, slug: Option<&str>) -> &'static str {
    let Some(status) = status else {
        return "the model gateway cannot be reached";
    };
    match slug {
        Some("unauthenticated") => "not authorized by the model gateway",
        Some("run_not_bound") => "the model gateway has not bound this run yet",
        Some("lease_expired") => "the model gateway's lease for this run has ended",
        Some("over_limit") => "the model gateway's limit is reached",
        Some("rate_limited") => "the model gateway's rate limit is reached",
        Some("unavailable") => "the model gateway is busy",
        _ => match status {
            401 | 403 => "not authorized by the model gateway",
            402 => "the model gateway's limit is reached",
            429 => "the model gateway's rate limit is reached",
            503 => "the model gateway is busy",
            _ => "the model gateway refused the call",
        },
    }
}

/// The reason with its status and slug, as a run's error and a log line say it:
/// `the model gateway is busy (HTTP 503 unavailable)`.
pub(crate) fn detail_of(reason: &str, status: Option<u16>, slug: Option<&str>) -> String {
    match (status, slug) {
        (Some(status), Some(slug)) => format!("{reason} (HTTP {status} {slug})"),
        (Some(status), None) => format!("{reason} (HTTP {status})"),
        (None, _) => reason.to_owned(),
    }
}

/// The wait of the `parks`-th park on a timer in a row: 30 s, doubling, at most
/// [`MAX_MODEL_BACKOFF`], and at least what the provider asked for.
pub(crate) fn backoff(parks: u32, at_least: Duration) -> Duration {
    let doublings = parks.saturating_sub(1).min(16);
    let step = FIRST_PARK
        .saturating_mul(1u32 << doublings)
        .min(MAX_MODEL_BACKOFF);
    step.max(at_least)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_from_thirty_seconds_to_fifteen_minutes() {
        let secs = |parks| backoff(parks, Duration::ZERO).as_secs();
        assert_eq!(
            (1..=8).map(secs).collect::<Vec<_>>(),
            [30, 60, 120, 240, 480, 900, 900, 900]
        );
        assert_eq!(secs(u32::MAX), 900);
        // What the provider asked for is the least of it.
        assert_eq!(
            backoff(1, Duration::from_secs(3600)),
            Duration::from_secs(3600)
        );
        assert_eq!(
            backoff(3, Duration::from_secs(45)),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn a_reason_is_read_from_the_slug_then_the_status() {
        assert_eq!(
            reason_of(Some(403), Some("run_not_bound")),
            "the model gateway has not bound this run yet"
        );
        assert_eq!(
            reason_of(Some(401), None),
            "not authorized by the model gateway"
        );
        assert_eq!(
            reason_of(Some(402), Some("over_limit")),
            "the model gateway's limit is reached"
        );
        assert_eq!(
            reason_of(Some(503), Some("other")),
            "the model gateway is busy"
        );
        assert_eq!(reason_of(None, None), "the model gateway cannot be reached");
        assert_eq!(
            detail_of("x", Some(429), Some("rate_limited")),
            "x (HTTP 429 rate_limited)"
        );
        assert_eq!(detail_of("x", Some(401), None), "x (HTTP 401)");
        assert_eq!(detail_of("x", None, None), "x");
    }

    #[test]
    fn only_a_refusal_that_parks_is_recorded_as_a_park() {
        let refused = |park| ModelError::Refused {
            status: 402,
            slug: Some("over_limit".into()),
            retry_after: None,
            park,
        };
        assert_eq!(RecordedPark::of(&refused(Park::No)), None);
        assert_eq!(
            RecordedPark::of(&refused(Park::UntilWoken)),
            Some(RecordedPark {
                status: 402,
                slug: Some("over_limit".into()),
                timer_ms: None
            })
        );
        assert_eq!(
            RecordedPark::of(&refused(Park::OnTimer(Duration::from_secs(30))))
                .and_then(|p| p.timer_ms),
            Some(30_000)
        );
        assert_eq!(RecordedPark::of(&ModelError::unreachable()), None);
    }

    /// A state written by a later build, whose model wait this build cannot read, loads with no
    /// model wait and everything else intact.
    #[test]
    fn a_model_wait_this_build_cannot_read_is_none() {
        let state = serde_json::json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}]}],
            "turns": 3,
            "model_wait": {"reason": "r", "park": "a_later_kind", "parked_since": "2026-11-21T00:00:00Z"},
        });
        let conversation: crate::Conversation = serde_json::from_value(state).expect("loads");
        assert!(conversation.model_wait.is_none());
        assert_eq!(conversation.turns, 3);
        let readable = serde_json::json!({
            "model_wait": {"reason": "r", "park": "timer", "parked_since": "2026-11-21T00:00:00Z"},
        });
        let conversation: crate::Conversation = serde_json::from_value(readable).expect("loads");
        assert_eq!(conversation.model_wait.unwrap().park, ModelPark::Timer);
    }
}
