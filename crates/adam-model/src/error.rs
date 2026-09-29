use std::time::Duration;

/// Why a model call failed.
///
/// Implementations map their transport's failures onto these variants; the
/// runtime decides what to do with them (see [`ModelError::is_retryable`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelError {
    /// The provider asked us to slow down (HTTP 429).
    #[error("rate limited{}", .retry_after.map(|d| format!(" (retry after {d:?})")).unwrap_or_default())]
    RateLimited {
        /// How long the provider asked us to wait, when it said.
        retry_after: Option<Duration>,
    },
    /// A failure that may go away on its own: 5xx, timeouts, connection resets.
    #[error("transient model error: {0}")]
    Transient(String),
    /// The prompt (plus requested output) does not fit the model's context window.
    #[error("context length exceeded: {0}")]
    ContextLength(String),
    /// The provider rejected the request (4xx other than auth and rate limit).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The credentials were rejected (401/403).
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The response could not be understood: malformed JSON, malformed tool
    /// arguments, or a stream that ended without a final message.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl ModelError {
    /// Whether retrying the same request later may succeed
    /// ([`RateLimited`](Self::RateLimited) and [`Transient`](Self::Transient)).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Transient(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_set() {
        assert!(ModelError::RateLimited { retry_after: None }.is_retryable());
        assert!(ModelError::Transient("x".into()).is_retryable());
        assert!(!ModelError::ContextLength("x".into()).is_retryable());
        assert!(!ModelError::InvalidRequest("x".into()).is_retryable());
        assert!(!ModelError::Auth("x".into()).is_retryable());
        assert!(!ModelError::Protocol("x".into()).is_retryable());
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
}
