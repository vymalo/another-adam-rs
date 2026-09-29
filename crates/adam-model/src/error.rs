use std::time::Duration;

use adam_error::{BoxError, Classify, ErrorClass};

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

    /// A response that cannot be understood ([`Protocol`](Self::Protocol)).
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol {
            message: message.into(),
            source: None,
        }
    }

    /// Attach the lower error as the source. Variants that carry no source are returned as is.
    #[must_use]
    pub fn with_source(mut self, err: impl std::error::Error + Send + Sync + 'static) -> Self {
        if let Self::Transient { source, .. }
        | Self::InvalidRequest { source, .. }
        | Self::Protocol { source, .. } = &mut self
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
            Self::Transient { .. } => ErrorClass::Transient,
            Self::ContextLength(_) | Self::InvalidRequest { .. } => ErrorClass::Invalid,
            Self::Auth(_) => ErrorClass::Unauthenticated,
            Self::Protocol { .. } => ErrorClass::Corrupt,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
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
        ]
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
