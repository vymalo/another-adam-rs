//! Delivering one notification to a webhook.
//!
//! What goes out, per the A2A 1.0 specification (§4.3.3, *verified* 2026-10-07,
//! <https://a2a-protocol.org/latest/specification/>):
//!
//! ```text
//! POST {webhook url}
//! Content-Type: application/a2a+json
//! Authorization: {scheme} {credentials}        (when the config has `authentication`)
//! A2A-Notification-Token: {token}              (when the config has a `token`)
//!
//! {"statusUpdate": {...}}  or  {"artifactUpdate": {...}}
//! ```
//!
//! The body is a `StreamResponse`: exactly one of `task`, `message`, `statusUpdate` and
//! `artifactUpdate`. The specification does not name a header for `token`; `A2A-Notification-Token`
//! is the one the official Rust SDK this crate builds on sends (`a2a-server-lf` 0.4.4,
//! `src/push/sender.rs`, *verified* 2026-10-07), so a receiver written against that SDK reads it.
//! The specification does **not** ask the server to sign notifications (no JWT, no JWKS): it asks
//! for the credentials in `authentication`, and that is all this sends.
//!
//! The client never follows a redirect, ignores `HTTP(S)_PROXY` (the address check below is the
//! one that connects), and resolves names through the [`GuardedResolver`], so it connects only to
//! addresses the [`PushPolicy`] allows. A 2xx answer is the acknowledgement.

use std::time::Duration;

use a2a::{StreamResponse, TaskPushNotificationConfig};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::{StatusCode, redirect};

use super::policy::{GuardedResolver, PushPolicy};

/// The media type of a notification (specification §4.3.3).
pub const NOTIFICATION_CONTENT_TYPE: &str = "application/a2a+json";

/// The header that carries a config's `token`.
pub const TOKEN_HEADER: &str = "a2a-notification-token";

/// Longest `last_error` kept: short on purpose, and never a URL or a credential.
const MAX_ERROR_LEN: usize = 160;

/// How a delivery failed, and so whether trying again can help.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// The webhook may recover (an error status, a timeout, a refused connection). The text is
    /// short and has no URL and no credential in it.
    Retryable(String),
    /// Trying again cannot help: the address is not allowed, or the webhook said it is gone.
    Permanent(String),
}

impl SendError {
    /// The text recorded as the config's `last_error`.
    pub fn message(&self) -> &str {
        match self {
            Self::Retryable(m) | Self::Permanent(m) => m,
        }
    }
}

/// Sends notifications. Cheap to clone.
#[derive(Clone, Debug)]
pub struct PushSender {
    client: reqwest::Client,
    policy: PushPolicy,
}

impl PushSender {
    /// A sender that follows `policy` and gives up on a request after `timeout`.
    ///
    /// # Errors
    ///
    /// The HTTP client cannot be built (no TLS backend).
    pub fn new(policy: PushPolicy, timeout: Duration) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .no_proxy()
            .dns_resolver(GuardedResolver::new(&policy))
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            .timeout(timeout)
            .user_agent(concat!("adam-a2a-push/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client, policy })
    }

    /// Deliver `event` to the webhook of `config`.
    ///
    /// # Errors
    ///
    /// [`SendError`], by whether a retry can help.
    pub async fn send(
        &self,
        config: &TaskPushNotificationConfig,
        event: &StreamResponse,
    ) -> Result<(), SendError> {
        // Judged again now: the policy may have changed since the config was created, and an IP
        // literal never reaches the resolver.
        let url = self
            .policy
            .check(&config.url)
            .map_err(|r| SendError::Permanent(format!("webhook refused: {r}")))?;
        let body = serde_json::to_vec(event)
            .map_err(|_| SendError::Permanent("the notification cannot be encoded".into()))?;

        let mut request = self
            .client
            .post(url)
            .header(CONTENT_TYPE, NOTIFICATION_CONTENT_TYPE)
            .body(body);
        if let Some(token) = config.token.as_deref().filter(|t| !t.is_empty()) {
            request = request.header(HeaderName::from_static(TOKEN_HEADER), secret_header(token)?);
        }
        if let Some(auth) = &config.authentication {
            let value = match auth.credentials.as_deref().filter(|c| !c.is_empty()) {
                Some(credentials) => format!("{} {credentials}", auth.scheme),
                None => auth.scheme.clone(),
            };
            request = request.header(AUTHORIZATION, secret_header(&value)?);
        }

        match request.send().await {
            Ok(response) => classify(response.status()),
            // `without_url`: the error text must never carry the webhook URL (it may hold a
            // secret in its path or query).
            Err(err) => Err(SendError::Retryable(describe(&err.without_url()))),
        }
    }
}

/// A header value for a secret: marked sensitive so it never shows in debug output, and refused
/// if it cannot be a header value (a CR, an LF, a control character).
fn secret_header(value: &str) -> Result<HeaderValue, SendError> {
    let mut value = HeaderValue::from_str(value).map_err(|_| {
        SendError::Permanent("the webhook credentials are not valid in a header".into())
    })?;
    value.set_sensitive(true);
    Ok(value)
}

fn classify(status: StatusCode) -> Result<(), SendError> {
    if status.is_success() {
        Ok(())
    } else if status == StatusCode::GONE {
        Err(SendError::Permanent("the webhook answered 410 Gone".into()))
    } else if status.is_redirection() {
        Err(SendError::Retryable(format!(
            "the webhook answered {} (redirects are not followed)",
            status.as_u16()
        )))
    } else {
        Err(SendError::Retryable(format!(
            "the webhook answered {}",
            status.as_u16()
        )))
    }
}

fn describe(err: &reqwest::Error) -> String {
    let text = if err.is_timeout() {
        "the webhook timed out".to_owned()
    } else if err.is_connect() {
        // The resolver's refusal and a refused connection both land here; the cause chain says
        // which, without the URL.
        let mut cause = String::new();
        let mut source = std::error::Error::source(err);
        while let Some(s) = source {
            cause = s.to_string();
            source = s.source();
        }
        if cause.contains("not allowed") {
            "the webhook host resolves only to addresses that are not allowed".to_owned()
        } else {
            "the webhook could not be reached".to_owned()
        }
    } else {
        "the webhook request failed".to_owned()
    };
    text.chars().take(MAX_ERROR_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_are_classified() {
        assert!(classify(StatusCode::OK).is_ok());
        assert!(classify(StatusCode::NO_CONTENT).is_ok());
        assert_eq!(
            classify(StatusCode::INTERNAL_SERVER_ERROR),
            Err(SendError::Retryable("the webhook answered 500".into()))
        );
        assert!(matches!(
            classify(StatusCode::FOUND),
            Err(SendError::Retryable(m)) if m.contains("redirects are not followed")
        ));
        assert!(matches!(
            classify(StatusCode::GONE),
            Err(SendError::Permanent(_))
        ));
        assert!(matches!(
            classify(StatusCode::UNAUTHORIZED),
            Err(SendError::Retryable(_))
        ));
    }

    #[test]
    fn a_credential_that_is_not_a_header_value_is_permanent_and_never_echoed() {
        let err = secret_header("Bearer a\r\nX-Evil: 1").unwrap_err();
        assert!(matches!(&err, SendError::Permanent(m) if !m.contains("Evil")));
        let ok = secret_header("Bearer abc").unwrap();
        assert!(ok.is_sensitive());
        assert_eq!(format!("{ok:?}"), "Sensitive");
    }
}
