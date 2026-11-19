//! The credentials of a model call, asked of a provider the host gives ([ADR 0058]).
//!
//! A [`ModelClient`](crate::ModelClient) that talks HTTP asks its [`ModelCredentials`] for the headers of
//! **every** call, so a provider can mint, renew or prove a credential at the moment it is used: a
//! fixed key ([`StaticBearer`]), a token the host renews from the person's session, a proof bound to
//! the request (its method and URL are given), or a header that names the run for an injector in front
//! of the model, with no key at all. The client never stores what it is given and never journals it.
//!
//! [ADR 0058]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};

use crate::ModelError;

/// The call a provider is asked to authorise.
#[derive(Debug, Clone, Copy)]
pub struct CredentialRequest<'a> {
    /// The HTTP method, upper case (`POST`).
    pub method: &'a str,
    /// The full URL the call goes to, query included (a proof bound to the request needs it).
    pub url: &'a str,
    /// `0` for the first try, `1` for the one retry after [`AfterRejection::Retry`].
    pub attempt: u32,
}

/// Why the endpoint refused a call: what a provider needs to decide whether a new credential would help.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rejection {
    /// The status code (`401`).
    pub status: u16,
    /// The `WWW-Authenticate` header, when there was one (`Bearer error="invalid_token"`).
    pub www_authenticate: Option<String>,
    /// The `DPoP-Nonce` header, when there was one (RFC 9449 §8: the server asks for a fresh nonce).
    pub dpop_nonce: Option<String>,
}

/// What a provider says after a [`Rejection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterRejection {
    /// A new credential may work: the client asks for headers again and sends the call once more.
    Retry,
    /// It would not: the call fails with the endpoint's answer.
    GiveUp,
}

/// The headers of one call. Names are plain; values are secrets, never shown by `Debug`.
#[derive(Default, Clone)]
pub struct CredentialHeaders(Vec<(String, SecretString)>);

impl CredentialHeaders {
    /// No header: the call goes out without credentials (a local server).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(token: &SecretString) -> Self {
        Self::none().with(
            "authorization",
            SecretString::from(format!("Bearer {}", token.expose_secret())),
        )
    }

    /// The same headers and one more; a later header of the same name replaces an earlier one.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: SecretString) -> Self {
        let name = name.into();
        self.0.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        self.0.push((name, value));
        self
    }

    /// The headers, in the order they were added.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SecretString)> {
        self.0.iter().map(|(n, v)| (n.as_str(), v))
    }

    /// Whether there is no header.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for CredentialHeaders {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(n, _)| n))
            .finish()
    }
}

/// Where the credentials of model calls come from. Asked for every call, so an implementation that
/// mints or renews must be cheap when it has a valid credential at hand (a cache and a lock, not a
/// network call per request).
///
/// Errors are the provider's choice of class: [`ModelError::Auth`] when the person must act (signed
/// out, consent withdrawn), a transient one when the issuer could not be reached, so the runtime
/// retries it as it retries the model's own failures.
#[async_trait]
pub trait ModelCredentials: Send + Sync + 'static {
    /// The headers to add to `call`. They replace any header of the same name the client would send.
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError>;

    /// The endpoint answered `401` to `call`, sent with the headers of [`headers`](Self::headers).
    /// The default gives up: a fixed credential will not get better by asking again. A provider
    /// that renews drops what it holds and answers [`AfterRejection::Retry`]; the client retries
    /// once, never more.
    async fn rejected(&self, call: CredentialRequest<'_>, rejection: &Rejection) -> AfterRejection {
        let _ = (call, rejection);
        AfterRejection::GiveUp
    }
}

/// A shared, type-erased [`ModelCredentials`].
pub type DynCredentials = Arc<dyn ModelCredentials>;

#[async_trait]
impl<T: ModelCredentials + ?Sized> ModelCredentials for Arc<T> {
    async fn headers(&self, call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        (**self).headers(call).await
    }

    async fn rejected(&self, call: CredentialRequest<'_>, rejection: &Rejection) -> AfterRejection {
        (**self).rejected(call, rejection).await
    }
}

/// A fixed bearer token, the same for every call: what `MODEL_API_KEY` gives. An empty token sends
/// no `Authorization` header.
#[derive(Clone)]
pub struct StaticBearer(SecretString);

impl StaticBearer {
    /// A provider that always answers `token`.
    #[must_use]
    pub fn new(token: SecretString) -> Self {
        Self(token)
    }
}

impl fmt::Debug for StaticBearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = if self.0.expose_secret().is_empty() {
            "empty"
        } else {
            "[REDACTED]"
        };
        f.debug_tuple("StaticBearer").field(&set).finish()
    }
}

#[async_trait]
impl ModelCredentials for StaticBearer {
    async fn headers(&self, _call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        if self.0.expose_secret().is_empty() {
            Ok(CredentialHeaders::none())
        } else {
            Ok(CredentialHeaders::bearer(&self.0))
        }
    }
}

/// A test double that mints `token-1`, `token-2`, ... and renews on a rejection: every call to
/// [`headers`](ModelCredentials::headers) answers the current token, every [`rejected`](ModelCredentials::rejected)
/// moves to the next one and asks for the retry. It records what it was asked, for the tests of a
/// client: [`issued`](Self::issued), [`rejections`](Self::rejections).
#[derive(Debug, Default)]
pub struct RenewingCredentials {
    current: AtomicU32,
    issued: AtomicU32,
    rejections: std::sync::Mutex<Vec<Rejection>>,
}

impl RenewingCredentials {
    /// Starts at `token-1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: AtomicU32::new(1),
            ..Self::default()
        }
    }

    /// How many times headers were asked for.
    #[must_use]
    pub fn issued(&self) -> u32 {
        self.issued.load(Ordering::SeqCst)
    }

    /// The rejections it was told about, oldest first.
    #[must_use]
    pub fn rejections(&self) -> Vec<Rejection> {
        self.rejections
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl ModelCredentials for RenewingCredentials {
    async fn headers(&self, _call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        self.issued.fetch_add(1, Ordering::SeqCst);
        let n = self.current.load(Ordering::SeqCst).max(1);
        Ok(CredentialHeaders::bearer(&SecretString::from(format!(
            "token-{n}"
        ))))
    }

    async fn rejected(
        &self,
        _call: CredentialRequest<'_>,
        rejection: &Rejection,
    ) -> AfterRejection {
        if let Ok(mut r) = self.rejections.lock() {
            r.push(rejection.clone());
        }
        self.current.fetch_add(1, Ordering::SeqCst);
        AfterRejection::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call() -> CredentialRequest<'static> {
        CredentialRequest {
            method: "POST",
            url: "https://models.example.test/v1/chat/completions",
            attempt: 0,
        }
    }

    #[tokio::test]
    async fn static_bearer_sends_the_key_and_an_empty_one_sends_nothing() {
        let set = StaticBearer::new(SecretString::from("sk-test"))
            .headers(call())
            .await
            .unwrap();
        let got: Vec<_> = set
            .iter()
            .map(|(n, v)| (n.to_owned(), v.expose_secret().to_owned()))
            .collect();
        assert_eq!(
            got,
            vec![("authorization".to_owned(), "Bearer sk-test".to_owned())]
        );
        assert!(
            StaticBearer::new(SecretString::from(""))
                .headers(call())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_fixed_key_gives_up_after_a_rejection() {
        let after = StaticBearer::new(SecretString::from("sk-test"))
            .rejected(
                call(),
                &Rejection {
                    status: 401,
                    ..Rejection::default()
                },
            )
            .await;
        assert_eq!(after, AfterRejection::GiveUp);
    }

    #[test]
    fn debug_never_shows_a_value() {
        let headers = CredentialHeaders::bearer(&SecretString::from("sk-secret"))
            .with("dpop", SecretString::from("proof-secret"));
        let shown = format!(
            "{headers:?} {:?}",
            StaticBearer::new(SecretString::from("sk-secret"))
        );
        assert!(!shown.contains("secret"), "{shown}");
        assert!(
            shown.contains("authorization") && shown.contains("dpop"),
            "{shown}"
        );
    }

    #[test]
    fn a_later_header_replaces_one_of_the_same_name() {
        let headers = CredentialHeaders::bearer(&SecretString::from("a"))
            .with("Authorization", SecretString::from("Bearer b"));
        let values: Vec<_> = headers
            .iter()
            .map(|(_, v)| v.expose_secret().to_owned())
            .collect();
        assert_eq!(values, vec!["Bearer b".to_owned()]);
    }

    #[tokio::test]
    async fn the_double_renews_after_each_rejection() {
        let c = RenewingCredentials::new();
        let first = c.headers(call()).await.unwrap();
        assert_eq!(
            first.iter().next().unwrap().1.expose_secret(),
            "Bearer token-1"
        );
        let rejection = Rejection {
            status: 401,
            ..Rejection::default()
        };
        assert_eq!(c.rejected(call(), &rejection).await, AfterRejection::Retry);
        let second = c.headers(call()).await.unwrap();
        assert_eq!(
            second.iter().next().unwrap().1.expose_secret(),
            "Bearer token-2"
        );
        assert_eq!(c.issued(), 2);
        assert_eq!(c.rejections(), vec![rejection]);
    }
}
