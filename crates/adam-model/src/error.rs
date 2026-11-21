use std::time::Duration;

use adam_error::{BoxError, Classify, ErrorClass};

use crate::credentials::Park;

/// Why a model call failed.
///
/// Implementations map their transport's failures onto these variants; the runtime decides
/// what to do from [`Classify::class`] (see [`Classify::is_retryable`] and
/// [`Classify::retry_after`]), never from the variant. A variant that wraps a lower error
/// keeps it as its [`source`](std::error::Error::source): [`Display`](std::fmt::Display) prints
/// this layer only, and [`adam_error::report`] prints the whole chain.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ModelError {
    /// The provider asked us to slow down (HTTP 429).
    #[error("rate limited{}", .retry_after.map(|d| format!(" (retry after {d:?})")).unwrap_or_default())]
    RateLimited {
        /// How long the provider asked us to wait, when it said.
        retry_after: Option<Duration>,
    },
    /// A failure that may go away on its own: 5xx, timeouts, connection resets.
    #[error("transient model error: {message}")]
    Transient {
        /// What happened, without the underlying error's text.
        message: String,
        /// The transport's own error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// The prompt (plus requested output) does not fit the model's context window.
    #[error("context length exceeded: {0}")]
    ContextLength(String),
    /// The provider rejected the request (4xx other than auth and rate limit), or the request
    /// could not be built.
    #[error("invalid request: {message}")]
    InvalidRequest {
        /// What was wrong, without the underlying error's text.
        message: String,
        /// The lower error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// The credentials were rejected (401/403).
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The response could not be understood: malformed JSON, malformed tool
    /// arguments, or a stream that ended without a final message.
    #[error("protocol error: {message}")]
    Protocol {
        /// What was wrong, without the underlying error's text.
        message: String,
        /// The decoder's own error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
    /// The endpoint refused the call and its provider parks the run on it
    /// ([`ModelCredentials::parks`](crate::ModelCredentials::parks), [ADR 0067] decision 3): a
    /// refusal cured outside the run (a lease not bound yet, a usage limit reached). It holds **no
    /// body and no header**; its `Display` is the status and the slug. Classed by its status: `401`
    /// and `403` [`Unauthenticated`](ErrorClass::Unauthenticated), `402`
    /// [`Rejected`](ErrorClass::Rejected), `429` [`RateLimited`](ErrorClass::RateLimited) (with its
    /// `retry_after`), `503` [`Transient`](ErrorClass::Transient), any other
    /// [`Rejected`](ErrorClass::Rejected).
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    #[error("the model endpoint refused the call: HTTP {status}{}", .slug.as_deref().map(|s| format!(" {s}")).unwrap_or_default())]
    Refused {
        /// The status code.
        status: u16,
        /// The problem type's slug, when there was one ([`Refusal::slug`](crate::Refusal::slug)).
        slug: Option<String>,
        /// The `Retry-After` header, when there was one.
        retry_after: Option<Duration>,
        /// What the provider said: never [`Park::No`], which keeps the error the call has without a park.
        park: Park,
    },
    /// The endpoint does not allow this model at all (the problem type `model_locked`, for every
    /// provider, [ADR 0067] decision 7): waiting cures nothing.
    ///
    /// [ADR 0067]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0067-a-model-call-through-an-injector-names-its-run-and-a-refusal-parks-the-run.md
    #[error("the model endpoint does not allow the model `{model}`")]
    ModelLocked {
        /// The model the request named (its alias, [`ModelRequest::model`](crate::ModelRequest::model)).
        model: String,
    },
    /// No connection to the endpoint could be made (refused, no route, a name that does not
    /// resolve): not a timeout, not a stream cut off. Retryable, so a short drop costs seconds.
    #[error("the model endpoint could not be reached")]
    Unreachable {
        /// The transport's own error, when there is one.
        #[source]
        source: Option<BoxError>,
    },
}

impl ModelError {
    /// A failure that may go away on its own ([`Transient`](Self::Transient)).
    pub fn transient(message: impl Into<String>) -> Self {
        Self::Transient {
            message: message.into(),
            source: None,
        }
    }

    /// A rejected or unbuildable request ([`InvalidRequest`](Self::InvalidRequest)).
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
            source: None,
        }
    }

    /// No connection to the endpoint could be made ([`Unreachable`](Self::Unreachable)).
    pub fn unreachable() -> Self {
        Self::Unreachable { source: None }
    }

    /// A response that cannot be understood ([`Protocol`](Self::Protocol)).
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
            source: None,
        }
    }

    /// Whether the provider rejected the prompt as too long for the model's context window
    /// ([`ContextLength`](Self::ContextLength)). The class stays [`ErrorClass::Invalid`] (the same
    /// request never succeeds), so a caller that can shorten the request and ask again decides from
    /// this predicate, and every other caller from the class.
    #[must_use]
    pub fn is_context_overflow(&self) -> bool {
        matches!(self, Self::ContextLength(_))
    }

    /// Attach the lower error as the source. Variants that carry no source are returned as is.
    #[must_use]
    pub fn with_source(mut self, err: impl std::error::Error + Send + Sync + 'static) -> Self {
        if let Self::Transient { source, .. }
        | Self::InvalidRequest { source, .. }
        | Self::Protocol { source, .. }
        | Self::Unreachable { source } = &mut self
        {
            *source = Some(Box::new(err));
        }
        self
    }
}

impl Classify for ModelError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::RateLimited { .. } => ErrorClass::RateLimited,
            Self::Transient { .. } | Self::Unreachable { .. } => ErrorClass::Transient,
            Self::ContextLength(_) | Self::InvalidRequest { .. } => ErrorClass::Invalid,
            Self::Auth(_) => ErrorClass::Unauthenticated,
            Self::Protocol { .. } => ErrorClass::Corrupt,
            Self::Refused { status, .. } => match status {
                401 | 403 => ErrorClass::Unauthenticated,
                429 => ErrorClass::RateLimited,
                503 => ErrorClass::Transient,
                _ => ErrorClass::Rejected,
            },
            Self::ModelLocked { .. } => ErrorClass::Rejected,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after }
            | Self::Refused {
                status: 429,
                retry_after,
                ..
            } => *retry_after,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("lower")]
    struct Lower;

    /// Exhaustive: a new variant forces a class decision here.
    fn expected(e: &ModelError) -> (ErrorClass, bool) {
        match e {
            ModelError::RateLimited { .. } => (ErrorClass::RateLimited, true),
            ModelError::Transient { .. } => (ErrorClass::Transient, true),
            ModelError::ContextLength(_) => (ErrorClass::Invalid, false),
            ModelError::InvalidRequest { .. } => (ErrorClass::Invalid, false),
            ModelError::Auth(_) => (ErrorClass::Unauthenticated, false),
            ModelError::Protocol { .. } => (ErrorClass::Corrupt, false),
            ModelError::Refused { status, .. } => match status {
                401 | 403 => (ErrorClass::Unauthenticated, false),
                429 => (ErrorClass::RateLimited, true),
                503 => (ErrorClass::Transient, true),
                _ => (ErrorClass::Rejected, false),
            },
            ModelError::ModelLocked { .. } => (ErrorClass::Rejected, false),
            ModelError::Unreachable { .. } => (ErrorClass::Transient, true),
        }
    }

    fn refused(status: u16, slug: Option<&str>, retry_after: Option<u64>) -> ModelError {
        ModelError::Refused {
            status,
            slug: slug.map(str::to_owned),
            retry_after: retry_after.map(Duration::from_secs),
            park: Park::OnTimer(Duration::from_secs(30)),
        }
    }

    fn samples() -> Vec<ModelError> {
        vec![
            ModelError::RateLimited { retry_after: None },
            ModelError::transient("x"),
            ModelError::ContextLength("x".into()),
            ModelError::invalid_request("x"),
            ModelError::Auth("x".into()),
            ModelError::protocol("x"),
            refused(401, Some("unauthenticated"), None),
            refused(401, None, None),
            refused(403, Some("run_not_bound"), None),
            refused(402, Some("over_limit"), None),
            refused(429, Some("rate_limited"), Some(60)),
            refused(503, Some("unavailable"), Some(60)),
            refused(400, Some("other"), None),
            refused(404, None, None),
            refused(500, None, None),
            ModelError::ModelLocked {
                model: "big-model".into(),
            },
            ModelError::unreachable(),
        ]
    }

    /// The classes of the new variants, written out (the table above decides by the same rules).
    #[test]
    fn a_refusal_is_classed_by_its_status() {
        let class = |status| refused(status, None, None).class();
        assert_eq!(class(401), ErrorClass::Unauthenticated);
        assert_eq!(class(403), ErrorClass::Unauthenticated);
        assert_eq!(class(402), ErrorClass::Rejected);
        assert_eq!(class(429), ErrorClass::RateLimited);
        assert_eq!(class(503), ErrorClass::Transient);
        for other in [400, 404, 409, 500, 502] {
            assert_eq!(class(other), ErrorClass::Rejected, "{other}");
        }
        let locked = ModelError::ModelLocked { model: "m".into() };
        assert_eq!(locked.class(), ErrorClass::Rejected);
        assert!(!locked.is_retryable());
        let unreachable = ModelError::unreachable();
        assert_eq!(unreachable.class(), ErrorClass::Transient);
        assert!(unreachable.is_retryable());
    }

    #[test]
    fn a_refusals_retry_after_is_said_for_a_rate_limit_only() {
        assert_eq!(
            refused(429, Some("rate_limited"), Some(45)).retry_after(),
            Some(Duration::from_secs(45))
        );
        assert_eq!(refused(429, None, None).retry_after(), None);
        assert_eq!(
            refused(503, Some("unavailable"), Some(45)).retry_after(),
            None
        );
        assert_eq!(refused(403, None, Some(45)).retry_after(), None);
    }

    /// The status and the slug, nothing else: the variant has no body to show.
    #[test]
    fn a_refusal_shows_its_status_and_slug_only() {
        assert_eq!(
            refused(403, Some("run_not_bound"), Some(60)).to_string(),
            "the model endpoint refused the call: HTTP 403 run_not_bound"
        );
        assert_eq!(
            refused(401, None, None).to_string(),
            "the model endpoint refused the call: HTTP 401"
        );
        let e = refused(402, Some("over_limit"), None);
        assert!(std::error::Error::source(&e).is_none());
        assert_eq!(adam_error::report(&e), e.to_string());
        assert_eq!(
            ModelError::ModelLocked {
                model: "big-model".into()
            }
            .to_string(),
            "the model endpoint does not allow the model `big-model`"
        );
        assert_eq!(
            ModelError::unreachable().to_string(),
            "the model endpoint could not be reached"
        );
    }

    #[test]
    fn class_table() {
        for e in samples() {
            let (class, retryable) = expected(&e);
            assert_eq!(e.class(), class, "{e}");
            assert_eq!(e.is_retryable(), retryable, "{e}");
        }
    }

    #[test]
    fn is_context_overflow_follows_the_variant() {
        for e in samples() {
            assert_eq!(
                e.is_context_overflow(),
                matches!(e, ModelError::ContextLength(_)),
                "{e}"
            );
        }
        assert_eq!(
            ModelError::ContextLength("x".into()).class(),
            ErrorClass::Invalid
        );
    }

    #[test]
    fn retry_after_comes_from_the_rate_limit_only() {
        let limited = ModelError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(limited.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(
            ModelError::RateLimited { retry_after: None }.retry_after(),
            None
        );
        assert_eq!(ModelError::transient("x").retry_after(), None);
    }

    #[test]
    fn display_mentions_retry_after() {
        let e = ModelError::RateLimited {
            retry_after: Some(Duration::from_secs(3)),
        };
        assert!(e.to_string().contains("3s"));
        assert_eq!(
            ModelError::RateLimited { retry_after: None }.to_string(),
            "rate limited"
        );
    }

    #[test]
    fn a_source_is_kept_and_printed_once() {
        for e in [
            ModelError::transient("connection failed").with_source(Lower),
            ModelError::invalid_request("not serializable").with_source(Lower),
            ModelError::protocol("response is not JSON").with_source(Lower),
            ModelError::unreachable().with_source(Lower),
        ] {
            let source = std::error::Error::source(&e).expect("source kept");
            assert!(source.is::<Lower>());
            assert!(!e.to_string().contains("lower"), "{e}");
            assert!(adam_error::report(&e).ends_with(": lower"));
        }
        // Variants without a source ignore `with_source`.
        assert!(
            std::error::Error::source(&ModelError::Auth("x".into()).with_source(Lower)).is_none()
        );
    }
}
