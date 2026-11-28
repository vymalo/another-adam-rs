//! OAuth 2.0 device-grant credentials for adam-rs model calls ([ADR 0058]).
//!
//! [`DeviceGrant`] is a [`ModelCredentials`] provider that signs a process in to an OAuth 2.0
//! authorization server with the device authorization grant (RFC 8628) and keeps a bearer token
//! for the model endpoint:
//!
//! 1. it reads the issuer's discovery document (`/.well-known/openid-configuration`) for the
//!    `device_authorization_endpoint` and the `token_endpoint`, which must be on the issuer's origin;
//! 2. it asks for a code and shows it through a [`SignInPrompt`] (by default [`LogPrompt`], a log
//!    line): the person opens the link, signs in and allows the process;
//! 3. it polls the token endpoint at the server's interval (`slow_down` adds five seconds) until the
//!    person answers or the code expires;
//! 4. it answers every model call with the access token, renews it with the refresh token
//!    [`renew_before`](DeviceGrantSettings::renew_before) its expiry (one renewal at a time, a
//!    rotated refresh token replacing the old one), and renews at once after a `401`.
//!
//! A refused renewal, a denied or expired code, or a token type other than `Bearer` signs the
//! process **out**: every later call is [`ModelError::Auth`], and no new code is asked for until the
//! process starts again (a code is the person's decision, never the process's). A server that does
//! not answer is a transient error and nothing is lost.
//!
//! ```no_run
//! use std::sync::Arc;
//! use adam_model_oauth::{DeviceGrant, DeviceGrantSettings, LogPrompt};
//! use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
//! use secrecy::SecretString;
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let grant = DeviceGrant::new(
//!     DeviceGrantSettings::new("https://id.example.test", "my-tools-client"),
//!     Arc::new(LogPrompt),
//! )?;
//! grant.start(); // ask for the code now, not at the first model call
//! let model = OpenAiCompatible::new(OpenAiConfig::new(
//!     "https://models.example.test/v1",
//!     SecretString::from(""),
//! ))?
//! .with_credentials(Arc::new(grant));
//! # let _ = model; Ok(()) }
//! ```
//!
//! [ADR 0058]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0058-the-models-credentials-come-from-a-provider-asked-for-each-call.md

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use adam_model::{
    AfterRejection, CredentialHeaders, CredentialRequest, ModelCredentials, ModelError, Rejection,
};
use async_trait::async_trait;
use reqwest::Url;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::{Mutex, watch};
use tokio::time::Instant;

/// RFC 8628 §3.4: the device-code grant type.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// RFC 8628 §3.2: the interval when the server gives none.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// RFC 8628 §3.5: what `slow_down` adds to the interval.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// How long a token without `expires_in` is taken to last.
const DEFAULT_LIFETIME: Duration = Duration::from_secs(300);

/// Where to sign in, and how.
#[derive(Debug, Clone)]
pub struct DeviceGrantSettings {
    /// The issuer's URL (`https://id.example.test`): the discovery document is read from
    /// `{issuer}/.well-known/openid-configuration`. `https`, or `http` on a loopback address only.
    pub issuer: String,
    /// The public client the authorization server knows this process as (no secret).
    pub client_id: String,
    /// The scope asked for; `offline_access` (the default) asks for a refresh token.
    pub scope: String,
    /// How long before its expiry the access token is renewed (60 s by default).
    pub renew_before: Duration,
    /// How long a model call waits for the person to finish signing in before it fails with a
    /// transient error the runtime retries (120 s by default).
    pub sign_in_wait: Duration,
    /// The timeout of one request to the authorization server (30 s by default).
    pub timeout: Duration,
}

impl DeviceGrantSettings {
    /// Settings with the defaults: scope `offline_access`, renewal 60 s early, a call waits 120 s
    /// for the sign-in, 30 s per request.
    #[must_use]
    pub fn new(issuer: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            client_id: client_id.into(),
            scope: "offline_access".into(),
            renew_before: Duration::from_secs(60),
            sign_in_wait: Duration::from_secs(120),
            timeout: Duration::from_secs(30),
        }
    }
}

/// Settings that cannot work: refused when the provider is built, before any request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OAuthConfigError {
    /// The issuer is not an absolute URL, or not `https` (nor `http` on a loopback address).
    #[error("the issuer must be an absolute https URL (http only on a loopback address): {0}")]
    InvalidIssuer(String),
    /// The client id is empty.
    #[error("the client id is empty")]
    EmptyClientId,
    /// The HTTP client could not be built.
    #[error("could not build the HTTP client")]
    Client(#[source] reqwest::Error),
}

/// What the person is shown to sign in: a code and where to enter it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInCode {
    /// The code the person types (`ABCD-1234`).
    pub user_code: String,
    /// Where to type it.
    pub verification_uri: String,
    /// The same link with the code in it, when the server gives one.
    pub verification_uri_complete: Option<String>,
    /// How long the code is valid.
    pub expires_in: Duration,
}

/// How the code reaches the person: a log line, a desktop notification, a screen of the host.
pub trait SignInPrompt: Send + Sync + 'static {
    /// A new code to show. Never called with a token.
    fn show(&self, code: &SignInCode);
    /// The person signed in; the code may be taken off the screen.
    fn signed_in(&self) {}
}

/// Shows the code as a `WARN` log line (the code and the link, never a token).
#[derive(Debug, Clone, Copy, Default)]
pub struct LogPrompt;

impl SignInPrompt for LogPrompt {
    fn show(&self, code: &SignInCode) {
        let link = code
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&code.verification_uri);
        tracing::warn!(
            user_code = %code.user_code,
            verification_uri = %link,
            expires_in_secs = code.expires_in.as_secs(),
            "sign in to use the model: open {link} and enter the code {}",
            code.user_code
        );
    }

    fn signed_in(&self) {
        tracing::info!("signed in: the model's token is renewed from now on");
    }
}

/// Where this provider stands. Tokens never leave it but as a call's header.
enum Phase {
    /// Nobody asked for a code yet.
    Idle,
    /// A code was asked for; the person has not answered.
    SigningIn,
    /// A token for the model.
    SignedIn {
        access: SecretString,
        valid_until: Instant,
        refresh: Option<SecretString>,
    },
    /// Refused, denied or expired: every call fails until the process starts again.
    SignedOut(String),
}

struct Endpoints {
    device_authorization: Url,
    token: Url,
}

struct Inner {
    http: reqwest::Client,
    settings: DeviceGrantSettings,
    issuer: Url,
    prompt: Arc<dyn SignInPrompt>,
    phase: Mutex<Phase>,
    /// Bumped each time the phase leaves `SigningIn`.
    changed: watch::Sender<u64>,
}

/// A [`ModelCredentials`] provider that signs in with the OAuth 2.0 device grant and renews its
/// token (see the [crate docs](crate)). Cheap to clone; clones share one sign-in.
#[derive(Clone)]
pub struct DeviceGrant {
    inner: Arc<Inner>,
}

impl fmt::Debug for DeviceGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceGrant")
            .field("issuer", &self.inner.issuer.origin().ascii_serialization())
            .field("client_id", &self.inner.settings.client_id)
            .finish_non_exhaustive()
    }
}

impl DeviceGrant {
    /// A provider for `settings`, showing codes through `prompt`. Sends nothing yet: the sign-in
    /// starts with [`start`](Self::start) or at the first model call.
    ///
    /// # Errors
    ///
    /// [`OAuthConfigError`] for an issuer that is not `https` (nor `http` on a loopback address), an
    /// empty client id, or an HTTP client that cannot be built.
    pub fn new(
        settings: DeviceGrantSettings,
        prompt: Arc<dyn SignInPrompt>,
    ) -> Result<Self, OAuthConfigError> {
        let issuer = Url::parse(settings.issuer.trim_end_matches('/'))
            .ok()
            .filter(allowed_scheme)
            .ok_or_else(|| OAuthConfigError::InvalidIssuer(settings.issuer.clone()))?;
        if settings.client_id.trim().is_empty() {
            return Err(OAuthConfigError::EmptyClientId);
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("adam-model-oauth/", env!("CARGO_PKG_VERSION")))
            .timeout(settings.timeout)
            // A token request is never replayed somewhere else.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(OAuthConfigError::Client)?;
        let (changed, _) = watch::channel(0);
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                settings,
                issuer,
                prompt,
                phase: Mutex::new(Phase::Idle),
                changed,
            }),
        })
    }

    /// Ask for a code now, in the background, so the person can sign in before the first model
    /// call. Does nothing outside a Tokio runtime, or when a sign-in already started.
    pub fn start(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut phase = inner.phase.lock().await;
            if matches!(*phase, Phase::Idle) {
                *phase = Phase::SigningIn;
                drop(phase);
                inner.sign_in().await;
            }
        });
    }

    /// Whether the process is signed in (it holds a token or a refresh token).
    pub async fn is_signed_in(&self) -> bool {
        matches!(*self.inner.phase.lock().await, Phase::SignedIn { .. })
    }
}

#[async_trait]
impl ModelCredentials for DeviceGrant {
    async fn headers(&self, _call: CredentialRequest<'_>) -> Result<CredentialHeaders, ModelError> {
        let inner = &self.inner;
        let mut waited = false;
        loop {
            let mut changes = inner.changed.subscribe();
            let mut phase = inner.phase.lock().await;
            match &*phase {
                Phase::SignedIn {
                    access,
                    valid_until,
                    refresh,
                } => {
                    let now = Instant::now();
                    if now + inner.settings.renew_before < *valid_until {
                        return Ok(CredentialHeaders::bearer(access));
                    }
                    let Some(refresh) = refresh.clone() else {
                        if now < *valid_until {
                            return Ok(CredentialHeaders::bearer(access));
                        }
                        let reason = "the token expired and there is no refresh token: sign in again (restart the process)";
                        *phase = Phase::SignedOut(reason.into());
                        return Err(ModelError::Auth(reason.into()));
                    };
                    // One renewal at a time: the lock is held across it.
                    match inner.refresh(&refresh).await {
                        Ok(next) => {
                            let headers = CredentialHeaders::bearer(&next.access);
                            *phase = next.into_phase(refresh);
                            return Ok(headers);
                        }
                        Err(Renewal::Refused(reason)) => {
                            *phase = Phase::SignedOut(reason.clone());
                            return Err(ModelError::Auth(reason));
                        }
                        Err(Renewal::Unreachable(error)) => {
                            if now < *valid_until {
                                tracing::warn!(
                                    "renewing the model's token failed, the current one is still valid: {error}"
                                );
                                return Ok(CredentialHeaders::bearer(access));
                            }
                            return Err(error);
                        }
                    }
                }
                Phase::SignedOut(reason) => return Err(ModelError::Auth(reason.clone())),
                Phase::Idle => {
                    *phase = Phase::SigningIn;
                    drop(phase);
                    let inner = inner.clone();
                    tokio::spawn(async move { inner.sign_in().await });
                }
                Phase::SigningIn => drop(phase),
            }
            if waited {
                return Err(ModelError::transient(
                    "waiting for the person to sign in: the code and the link are in the process's logs",
                ));
            }
            waited = true;
            // Wait for the sign-in to end, at most `sign_in_wait`, then look again.
            let _ = tokio::time::timeout(inner.settings.sign_in_wait, changes.changed()).await;
        }
    }

    async fn rejected(
        &self,
        _call: CredentialRequest<'_>,
        _rejection: &Rejection,
    ) -> AfterRejection {
        let mut phase = self.inner.phase.lock().await;
        if let Phase::SignedIn {
            valid_until,
            refresh,
            ..
        } = &mut *phase
        {
            // The endpoint refused this token: it is renewed at the next call.
            *valid_until = Instant::now();
            if refresh.is_some() {
                return AfterRejection::Retry;
            }
        }
        AfterRejection::GiveUp
    }
}

/// Why a renewal did not give a token.
enum Renewal {
    /// The server refused the refresh token: the person must sign in again.
    Refused(String),
    /// The server did not answer usefully: try again later.
    Unreachable(ModelError),
}

/// A token from the token endpoint.
struct Issued {
    access: SecretString,
    lifetime: Duration,
    refresh: Option<SecretString>,
}

impl Issued {
    /// The phase it gives; a renewal that returns no refresh token keeps the one it spent.
    fn into_phase(self, previous_refresh: SecretString) -> Phase {
        Phase::SignedIn {
            access: self.access,
            valid_until: Instant::now() + self.lifetime,
            refresh: Some(self.refresh.unwrap_or(previous_refresh)),
        }
    }
}

#[derive(Deserialize)]
struct Discovery {
    device_authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    error: Option<String>,
}

impl TokenAnswer {
    /// The token of a successful answer, or why it cannot be used.
    fn issued(self) -> Result<Issued, String> {
        let access = self
            .access_token
            .filter(|t| !t.is_empty())
            .ok_or("the token endpoint answered without an access token")?;
        if let Some(kind) = &self.token_type
            && !kind.eq_ignore_ascii_case("bearer")
        {
            return Err(format!(
                "the token type `{kind}` is not supported (only Bearer)"
            ));
        }
        Ok(Issued {
            access: SecretString::from(access),
            lifetime: self
                .expires_in
                .map_or(DEFAULT_LIFETIME, Duration::from_secs),
            refresh: self
                .refresh_token
                .filter(|t| !t.is_empty())
                .map(SecretString::from),
        })
    }
}

impl Inner {
    /// The whole sign-in: discovery, a code, the person's answer. Ends in `SignedIn`, in
    /// `SignedOut`, or back in `Idle` when the server could not be reached (the next call asks
    /// again), and wakes the calls that wait.
    async fn sign_in(&self) {
        let next = match self.device_flow().await {
            Ok(issued) => {
                self.prompt.signed_in();
                Phase::SignedIn {
                    access: issued.access,
                    valid_until: Instant::now() + issued.lifetime,
                    refresh: issued.refresh,
                }
            }
            Err(Renewal::Refused(reason)) => {
                // The reason is free text from the provider (it can carry a URL): a field the
                // allowlist drops, so the collector sees the class and the JSON log the detail.
                tracing::warn!(reason = %reason, "the sign-in for the model ended");
                Phase::SignedOut(reason)
            }
            Err(Renewal::Unreachable(error)) => {
                tracing::warn!(problem = %error, "the sign-in for the model could not start");
                Phase::Idle
            }
        };
        *self.phase.lock().await = next;
        self.changed.send_modify(|n| *n += 1);
    }

    async fn device_flow(&self) -> Result<Issued, Renewal> {
        let endpoints = self.discover().await?;
        let code: DeviceCode = self
            .form(
                endpoints.device_authorization.clone(),
                &[
                    ("client_id", self.settings.client_id.as_str()),
                    ("scope", self.settings.scope.as_str()),
                ],
            )
            .await
            .and_then(|(status, body)| {
                if status.is_success() {
                    serde_json::from_slice(&body).map_err(|_| {
                        unreachable("the device authorization answer is not understood")
                    })
                } else {
                    Err(unreachable(format!(
                        "the device authorization endpoint answered {status}"
                    )))
                }
            })
            .map_err(Renewal::Unreachable)?;
        let device_code = SecretString::from(code.device_code);
        self.prompt.show(&SignInCode {
            user_code: code.user_code,
            verification_uri: code.verification_uri,
            verification_uri_complete: code.verification_uri_complete,
            expires_in: Duration::from_secs(code.expires_in),
        });

        let deadline = Instant::now() + Duration::from_secs(code.expires_in);
        let mut interval = code
            .interval
            .map_or(DEFAULT_INTERVAL, Duration::from_secs)
            .max(Duration::from_secs(1));
        loop {
            tokio::time::sleep(interval).await;
            if Instant::now() >= deadline {
                return Err(Renewal::Refused(
                    "the code expired before the person signed in: restart the process for a new one".into(),
                ));
            }
            let answer = self
                .form(
                    endpoints.token.clone(),
                    &[
                        ("grant_type", DEVICE_GRANT),
                        ("device_code", device_code.expose_secret()),
                        ("client_id", self.settings.client_id.as_str()),
                    ],
                )
                .await;
            let Ok((status, body)) = answer else {
                continue; // the server did not answer: poll again until the code expires
            };
            let Ok(answer) = serde_json::from_slice::<TokenAnswer>(&body) else {
                continue;
            };
            if status.is_success() {
                return answer.issued().map_err(Renewal::Refused);
            }
            match answer.error.as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval = next_interval(interval),
                Some("access_denied") => {
                    return Err(Renewal::Refused("the person denied the sign-in".into()));
                }
                Some("expired_token") => {
                    return Err(Renewal::Refused(
                        "the code expired before the person signed in: restart the process for a new one".into(),
                    ));
                }
                Some(other) => {
                    return Err(Renewal::Refused(format!(
                        "the sign-in was refused: {other}"
                    )));
                }
                None if status.is_server_error() => {}
                None => {
                    return Err(Renewal::Refused(format!(
                        "the token endpoint answered {status}"
                    )));
                }
            }
        }
    }

    async fn refresh(&self, refresh: &SecretString) -> Result<Issued, Renewal> {
        let endpoints = self.discover().await?;
        let (status, body) = self
            .form(
                endpoints.token,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh.expose_secret()),
                    ("client_id", self.settings.client_id.as_str()),
                ],
            )
            .await
            .map_err(Renewal::Unreachable)?;
        if status.is_server_error() {
            return Err(Renewal::Unreachable(ModelError::transient(format!(
                "the token endpoint answered {status} to a renewal"
            ))));
        }
        let answer: TokenAnswer = serde_json::from_slice(&body).map_err(|_| {
            Renewal::Unreachable(unreachable("the renewal answer is not understood"))
        })?;
        if status.is_success() {
            return answer.issued().map_err(Renewal::Refused);
        }
        let error = answer.error.unwrap_or_else(|| status.to_string());
        Err(Renewal::Refused(format!(
            "the renewal of the sign-in was refused ({error}): sign in again (restart the process)"
        )))
    }

    /// The endpoints, from the issuer's discovery document; both on the issuer's origin.
    async fn discover(&self) -> Result<Endpoints, Renewal> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.as_str().trim_end_matches('/')
        );
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| Renewal::Unreachable(transport(&e)))?;
        if !response.status().is_success() {
            return Err(Renewal::Unreachable(unreachable(format!(
                "the issuer's discovery document answered {}",
                response.status()
            ))));
        }
        let document: Discovery = response.json().await.map_err(|_| {
            Renewal::Unreachable(unreachable(
                "the issuer's discovery document is not understood",
            ))
        })?;
        let on_issuer = |field: &str, value: Option<String>| -> Result<Url, Renewal> {
            let url = value.and_then(|v| Url::parse(&v).ok()).ok_or_else(|| {
                Renewal::Refused(format!(
                    "the issuer's discovery document has no usable {field}"
                ))
            })?;
            if url.origin() != self.issuer.origin() {
                return Err(Renewal::Refused(format!(
                    "the issuer's {field} is on another origin than the issuer: refused"
                )));
            }
            Ok(url)
        };
        Ok(Endpoints {
            device_authorization: on_issuer(
                "device_authorization_endpoint",
                document.device_authorization_endpoint,
            )?,
            token: on_issuer("token_endpoint", document.token_endpoint)?,
        })
    }

    /// One form POST; the status and the body, or a transient error when nothing came back.
    async fn form(
        &self,
        url: Url,
        fields: &[(&str, &str)],
    ) -> Result<(reqwest::StatusCode, Vec<u8>), ModelError> {
        let response = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(fields))
            .send()
            .await
            .map_err(|e| transport(&e))?;
        let status = response.status();
        let body = response.bytes().await.map_err(|e| transport(&e))?;
        Ok((status, body.to_vec()))
    }
}

/// `application/x-www-form-urlencoded` (RFC 6749 appendix B): unreserved bytes as they are, a
/// space as `+`, every other byte as `%XX`.
fn form_body(fields: &[(&str, &str)]) -> String {
    fn encode(out: &mut String, value: &str) {
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(char::from(byte));
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (name, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        encode(&mut out, name);
        out.push('=');
        encode(&mut out, value);
    }
    out
}

/// RFC 8628 §3.5: `slow_down` makes every later poll five seconds slower.
fn next_interval(interval: Duration) -> Duration {
    interval + SLOW_DOWN_STEP
}

fn allowed_scheme(url: &Url) -> bool {
    match (url.scheme(), url.host_str()) {
        ("https", Some(_)) => true,
        ("http", Some(host)) => {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }
        _ => false,
    }
}

fn unreachable(message: impl Into<String>) -> ModelError {
    ModelError::transient(message.into())
}

/// A transport failure, without the URL (it may carry nothing secret, but it is noise).
fn transport(e: &reqwest::Error) -> ModelError {
    if e.is_timeout() {
        unreachable("the authorization server did not answer in time")
    } else {
        unreachable("the authorization server could not be reached")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_is_url_encoded() {
        assert_eq!(
            form_body(&[
                ("grant_type", DEVICE_GRANT),
                ("scope", "offline_access openid"),
                ("x", "a&b=c~")
            ]),
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&scope=offline_access+openid&x=a%26b%3Dc~"
        );
    }

    #[test]
    fn slow_down_adds_five_seconds() {
        assert_eq!(
            next_interval(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn only_https_or_loopback_http_issuers() {
        let ok = |s: &str| {
            DeviceGrant::new(DeviceGrantSettings::new(s, "c"), Arc::new(LogPrompt)).is_ok()
        };
        assert!(ok("https://id.example.test"));
        assert!(ok("http://127.0.0.1:8080"));
        assert!(ok("http://localhost:8080/"));
        assert!(ok("http://[::1]:8080"));
        assert!(!ok("http://id.example.test"));
        assert!(!ok("ftp://id.example.test"));
        assert!(!ok("not a url"));
        assert!(
            DeviceGrant::new(
                DeviceGrantSettings::new("https://id.example.test", " "),
                Arc::new(LogPrompt)
            )
            .is_err()
        );
    }

    #[test]
    fn a_token_type_other_than_bearer_is_refused() {
        let answer = TokenAnswer {
            access_token: Some("t".into()),
            token_type: Some("DPoP".into()),
            expires_in: Some(60),
            refresh_token: None,
            error: None,
        };
        assert!(answer.issued().is_err());
        let bearer = TokenAnswer {
            token_type: Some("bearer".into()),
            access_token: Some("t".into()),
            expires_in: None,
            refresh_token: Some(String::new()),
            error: None,
        };
        let issued = bearer.issued().unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(issued.lifetime, DEFAULT_LIFETIME);
        assert!(issued.refresh.is_none(), "an empty refresh token is none");
    }

    #[test]
    fn debug_shows_the_issuer_origin_and_the_client_only() {
        let grant = DeviceGrant::new(
            DeviceGrantSettings::new("https://id.example.test/tenant/x", "tools"),
            Arc::new(LogPrompt),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let shown = format!("{grant:?}");
        assert!(
            shown.contains("https://id.example.test") && shown.contains("tools"),
            "{shown}"
        );
        assert!(!shown.contains("tenant"), "{shown}");
    }
}
