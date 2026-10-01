//! Credentials of a GitHub App installation: short-lived installation access tokens, minted from
//! the App's private key.
//!
//! A GitHub App authenticates in two steps. It signs a **JWT** with its private key (RS256, valid
//! for ten minutes at most) and trades it at
//! `POST {api}/app/installations/{id}/access_tokens` for an **installation access token**, which
//! is good for an hour and is what `git` (as `x-access-token:<token>`) and the REST API are given.
//! [`GitHubApp`] does the trade, keeps the token until five minutes before it expires and mints
//! the next one then, with one request however many callers are waiting.
//!
//! The key is parsed once ([`AppKey::from_pem`]), so a key that cannot be used is found at startup
//! and not at the first push. Neither the JWT nor a token is ever in an error message, a `Debug`
//! output or a log line.
//!
//! *Verified 2026-10-01 against docs.github.com* ("Generating a JSON Web Token (JWT) for a GitHub
//! App", the REST reference of `POST /app/installations/{installation_id}/access_tokens`, and
//! "Authenticating as a GitHub App installation"): the JWT is signed with `RS256`, its `iat` is
//! best set 60 seconds in the past against clock drift, its `exp` is at most 10 minutes ahead and
//! its `iss` is the App's client ID or application ID; the endpoint answers `201` with `token` and
//! `expires_at`, authenticated by the JWT as a bearer; an installation token expires after one
//! hour and works with git over HTTPS as the password of `x-access-token`. The endpoint answers
//! `401`, `403` and `404` for a bad JWT, a forbidden or an unknown installation. *Not assumed:*
//! the length or the shape of a token (GitHub began a staged rollout of a longer, stateless
//! format on 2026-04-27), so nothing here reads a token's characters.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Response, StatusCode};
use rustls_pki_types::PrivateKeyDer;
use rustls_pki_types::pem::PemObject as _;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use crate::credentials::GitCredentials;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::github::{api_message, retry_after};
use crate::repo::RepoRef;

const API_VERSION: &str = "2022-11-28";
const DEFAULT_USER_AGENT: &str = concat!("adam-workspace/", env!("CARGO_PKG_VERSION"));

/// How long before its expiry a cached token is replaced.
const REFRESH_MARGIN: TimeDelta = TimeDelta::minutes(5);
/// How far in the past a JWT's `iat` is set (GitHub's advice against clock drift).
const JWT_BACKDATE: TimeDelta = TimeDelta::seconds(60);
/// How far ahead a JWT's `exp` is set (GitHub allows ten minutes at most).
const JWT_LIFETIME: TimeDelta = TimeDelta::seconds(540);

/// The private key of a GitHub App: an RSA key, parsed from the PEM that GitHub lets the owner
/// download (PKCS#1, `BEGIN RSA PRIVATE KEY`) or from its PKCS#8 form (`BEGIN PRIVATE KEY`).
///
/// Cheap to clone. `Debug` shows nothing of it.
#[derive(Clone)]
pub struct AppKey(Arc<RsaKeyPair>);

impl AppKey {
    /// Parse `pem`: the first private key in it, which must be an RSA key of 2048 to 8192 bits.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] when `pem` holds no private key, one that is not RSA (an EC
    /// key, an encrypted one) or one that is not acceptable. The message never carries the key.
    pub fn from_pem(pem: &str) -> WorkspaceResult<Self> {
        let not_usable = |what: &str| {
            WorkspaceError::Invalid(format!(
                "the GitHub App's private key is not usable: {what} (an unencrypted RSA private \
                 key in PEM form is needed, PKCS#1 `BEGIN RSA PRIVATE KEY` as GitHub gives it, or PKCS#8)"
            ))
        };
        let der = PrivateKeyDer::from_pem_slice(pem.as_bytes())
            .map_err(|_| not_usable("no private key was found in it"))?;
        let pair = match &der {
            PrivateKeyDer::Pkcs1(key) => RsaKeyPair::from_der(key.secret_pkcs1_der()),
            PrivateKeyDer::Pkcs8(key) => RsaKeyPair::from_pkcs8(key.secret_pkcs8_der()),
            _ => return Err(not_usable("it is not an RSA key")),
        }
        .map_err(|_| not_usable("it is not an RSA key of 2048 to 8192 bits"))?;
        Ok(Self(Arc::new(pair)))
    }
}

impl fmt::Debug for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppKey(<redacted>)")
    }
}

/// The time source of a [`GitHubApp`]: a function, so that a test can move the clock.
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// An installation access token and when it stops working.
struct Cached {
    token: SecretString,
    expires_at: DateTime<Utc>,
}

/// [`GitCredentials`] for one installation of a GitHub App: [`token_for`](GitCredentials::token_for)
/// is an installation access token, the same for every repository the installation can reach.
///
/// **It hands the token out for any host.** Wrap it in
/// [`HostScoped`](crate::HostScoped) so that the host of the repository is checked before a token
/// is minted, as [`ScopedToken`](crate::ScopedToken) does for a personal access token.
///
/// # The token cycle
///
/// A cached token with more than five minutes left is returned as it is. Otherwise one caller at
/// a time (the others wait for it, and find the new token) signs a JWT (`iat` a minute ago, `exp`
/// nine minutes ahead, `iss` the App) and trades it for a token. A failed mint is not cached: the
/// next call tries again.
///
/// # Errors
///
/// [`token_for`](GitCredentials::token_for) fails with [`WorkspaceError::Auth`] when GitHub says
/// `401`, `403` or `404` (the message names the variables to check), with
/// [`WorkspaceError::RateLimited`] for a `429` or a rate-limited `403`, and with
/// [`WorkspaceError::Transient`] for a `5xx`, a transport failure or an answer that cannot be read.
pub struct GitHubApp {
    http: reqwest::Client,
    api_base: String,
    app_id: String,
    installation_id: u64,
    key: AppKey,
    clock: Clock,
    /// Held while a token is minted, so that concurrent callers share one request.
    cache: tokio::sync::Mutex<Option<Cached>>,
}

impl fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubApp")
            .field("api_base", &self.api_base)
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .finish_non_exhaustive()
    }
}

impl GitHubApp {
    /// The installation `installation_id` of the App `app_id` (its application ID, or its client
    /// ID: what the JWT's `iss` says), with its private key, against the API root `api_base`
    /// (`https://api.github.com`, `https://<host>/api/v3` for GitHub Enterprise Server, or a mock).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] if the HTTP client cannot be constructed.
    pub fn new(
        api_base: impl Into<String>,
        app_id: impl Into<String>,
        installation_id: u64,
        key: AppKey,
    ) -> WorkspaceResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(DEFAULT_USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| WorkspaceError::Invalid(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_owned(),
            app_id: app_id.into(),
            installation_id,
            key,
            clock: Arc::new(Utc::now),
            cache: tokio::sync::Mutex::new(None),
        })
    }

    /// Read the time from `clock` instead of the system clock: what the JWT says, and when a
    /// cached token counts as about to expire.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// The JWT for `now`: `{"alg":"RS256","typ":"JWT"}.{"iat","exp","iss"}`, signed.
    fn jwt(&self, now: DateTime<Utc>) -> WorkspaceResult<SecretString> {
        let b64 = |bytes: &[u8]| URL_SAFE_NO_PAD.encode(bytes);
        // The classic application ID is a number, a client ID is a string: say each as it is.
        let issuer = match self.app_id.parse::<u64>() {
            Ok(id) => json!(id),
            Err(_) => json!(self.app_id),
        };
        let claims = json!({
            "iat": (now - JWT_BACKDATE).timestamp(),
            "exp": (now + JWT_LIFETIME).timestamp(),
            "iss": issuer,
        });
        let signing_input = format!(
            "{}.{}",
            b64(br#"{"alg":"RS256","typ":"JWT"}"#),
            b64(claims.to_string().as_bytes())
        );
        let mut signature = vec![0; self.key.0.public_modulus_len()];
        self.key
            .0
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| {
                WorkspaceError::Invalid("the GitHub App's private key cannot sign".to_owned())
            })?;
        Ok(SecretString::from(format!(
            "{signing_input}.{}",
            b64(&signature)
        )))
    }

    /// Trade a fresh JWT for an installation access token.
    async fn mint(&self, now: DateTime<Utc>) -> WorkspaceResult<Cached> {
        let jwt = self.jwt(now)?;
        let response = self
            .http
            .post(format!(
                "{}/app/installations/{}/access_tokens",
                self.api_base, self.installation_id
            ))
            .header(AUTHORIZATION, format!("Bearer {}", jwt.expose_secret()))
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .send()
            .await
            // `without_url`: nothing of the request, whatever it names, rides along in the error.
            .map_err(|e| {
                WorkspaceError::transient("the GitHub App's installation token was not minted")
                    .with_source(e.without_url())
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.refused(response, &jwt).await);
        }
        #[derive(Deserialize)]
        struct Minted {
            token: String,
            expires_at: String,
        }
        let unreadable = || {
            WorkspaceError::transient(
                "GitHub answered the installation token request with a success status and an \
                 unreadable body",
            )
        };
        let minted: Minted = response.json().await.map_err(|_| unreadable())?;
        let expires_at = DateTime::parse_from_rfc3339(&minted.expires_at)
            .map_err(|_| unreadable())?
            .with_timezone(&Utc);
        Ok(Cached {
            token: SecretString::from(minted.token),
            expires_at,
        })
    }

    /// The error for a response that is not a success. Nothing of the JWT is in it.
    async fn refused(&self, response: Response, jwt: &SecretString) -> WorkspaceError {
        let status = response.status();
        let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
            || (status == StatusCode::FORBIDDEN
                && response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0"));
        let wait = rate_limited
            .then(|| retry_after(response.headers(), std::time::SystemTime::now()))
            .flatten();
        let body = response.text().await.unwrap_or_default();
        let said = api_message(&body).replace(jwt.expose_secret(), "[REDACTED]");
        if rate_limited {
            return WorkspaceError::RateLimited { retry_after: wait };
        }
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                WorkspaceError::Auth(format!(
                    "GitHub refused the App's credentials (HTTP {}: {said}): check \
                     GITHUB_APP_ID, GITHUB_APP_INSTALLATION_ID and the private key \
                     (GITHUB_APP_PRIVATE_KEY_PATH), and that the App is installed",
                    status.as_u16()
                ))
            }
            s if s.is_server_error() => {
                WorkspaceError::transient(format!("GitHub answered HTTP {}: {said}", s.as_u16()))
            }
            s => WorkspaceError::Http {
                status: s.as_u16(),
                message: said,
            },
        }
    }
}

#[async_trait]
impl GitCredentials for GitHubApp {
    #[tracing::instrument(skip(self, _repo), fields(installation = self.installation_id))]
    async fn token_for(&self, _repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
        // One caller mints; the others wait here and find the token it made.
        let mut cache = self.cache.lock().await;
        let now = (self.clock)();
        if let Some(cached) = cache.as_ref()
            && cached.expires_at - now > REFRESH_MARGIN
        {
            return Ok(cached.token.clone());
        }
        let fresh = self.mint(now).await?;
        let token = fresh.token.clone();
        *cache = Some(fresh);
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestAppKey;

    #[test]
    fn a_key_in_either_form_is_accepted_and_anything_else_is_not() {
        let key = TestAppKey::generate();
        assert!(AppKey::from_pem(&key.pkcs1_pem).is_ok(), "GitHub's form");
        assert!(AppKey::from_pem(&key.pkcs8_pem).is_ok(), "PKCS#8");
        // Windows line ends, and text before the key.
        let crlf = format!("# my key\r\n{}", key.pkcs1_pem.replace('\n', "\r\n"));
        assert!(AppKey::from_pem(&crlf).is_ok());
        for bad in [
            "",
            "not a key",
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
            "-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n",
        ] {
            let err = AppKey::from_pem(bad).unwrap_err();
            assert!(
                matches!(err, WorkspaceError::Invalid(_)),
                "{bad:?}: {err:?}"
            );
            assert!(err.to_string().contains("not usable"), "{err}");
            assert!(!err.to_string().contains("AAAA"), "{err}");
        }
        assert_eq!(
            format!("{:?}", AppKey::from_pem(&key.pkcs8_pem).unwrap()),
            "AppKey(<redacted>)"
        );
    }

    #[test]
    fn the_jwt_says_what_github_asks_for_and_is_signed() {
        let key = TestAppKey::generate();
        let app = GitHubApp::new(
            "https://api.github.com/",
            "12345",
            67890,
            AppKey::from_pem(&key.pkcs1_pem).unwrap(),
        )
        .unwrap();
        let now = DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let jwt = app.jwt(now).unwrap();
        let claims = key.verify_jwt(jwt.expose_secret()).unwrap();
        assert_eq!(claims["iss"], 12345, "a numeric application ID is a number");
        assert_eq!(claims["iat"], now.timestamp() - 60);
        assert_eq!(claims["exp"], now.timestamp() + 540);
        // A client ID is a string.
        let by_client_id = GitHubApp::new(
            "https://api.github.com",
            "Iv23liExample",
            1,
            AppKey::from_pem(&key.pkcs8_pem).unwrap(),
        )
        .unwrap();
        let claims = key
            .verify_jwt(by_client_id.jwt(now).unwrap().expose_secret())
            .unwrap();
        assert_eq!(claims["iss"], "Iv23liExample");
        // Not valid for another key.
        assert!(
            TestAppKey::generate()
                .verify_jwt(jwt.expose_secret())
                .is_err()
        );
    }

    #[test]
    fn debug_output_shows_neither_a_key_nor_a_token() {
        let key = TestAppKey::generate();
        let app = GitHubApp::new(
            "https://api.github.com",
            "1",
            2,
            AppKey::from_pem(&key.pkcs8_pem).unwrap(),
        )
        .unwrap();
        let text = format!("{app:?}");
        assert!(text.contains("installation_id: 2"), "{text}");
        assert!(!text.contains("BEGIN"), "{text}");
    }
}
