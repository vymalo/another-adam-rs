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
//! The installation is either named ([`GitHubApp::new`]) or found by the owner of the repository
//! ([`GitHubApp::discovering`], ADR 0017): the App's JWT, as a bearer, asks
//! `GET /orgs/{owner}/installation` and, on a `404`, `GET /users/{owner}/installation`.
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
//!
//! *Verified 2026-10-03 against the OpenAPI description (`github/rest-api-description`) and
//! docs.github.com:* `GET /orgs/{org}/installation`, `GET /users/{username}/installation` and
//! `GET /app` are authenticated by the App's JWT; an installation has `id`, a nullable `account`
//! and `suspended_at`; `GET /app` has `slug` and `html_url`. *Unverified:* that the two lookups
//! answer `404` when the App is not installed (the description lists only `200`; the code takes a
//! `404` as "not installed" on both), whether the user lookup also answers for an organisation,
//! that logins are compared without case (common knowledge, not a document), what a lookup by a
//! former login answers, and the rate limits of the lookups (the caches keep them to about one an
//! owner).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Method, Response, StatusCode};
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

/// How many installations' tokens a [`GitHubApp`] keeps. The least recently used is forgotten
/// first (and minted again, one request, if it is asked for later).
pub const MAX_CACHED_INSTALLATIONS: usize = 64;
/// How many accounts' installations a [`GitHubApp`] remembers (four for each installation it keeps
/// a token for, since an account that is not installed takes a place too).
const MAX_CACHED_ACCOUNTS: usize = 4 * MAX_CACHED_INSTALLATIONS;
/// How long "the App is not installed on this account" is believed before GitHub is asked again.
const NOT_INSTALLED_FOR: TimeDelta = TimeDelta::seconds(60);

/// Which accounts a [`GitHubApp`] that finds installations by itself may act for.
///
/// A public App can be installed by anyone, so the installation of an account is no proof that the
/// deployment wants the App to act there: this list is. Logins are compared without case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppOwners {
    /// Every account the App is installed on.
    Any,
    /// These accounts only, lowercased (build it with [`AppOwners::only`]).
    Only(BTreeSet<String>),
}

impl AppOwners {
    /// Only these logins, compared without case. Blanks are dropped, so an empty list allows
    /// nothing.
    pub fn only<I, S>(owners: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::Only(
            owners
                .into_iter()
                .map(|o| o.as_ref().trim().to_ascii_lowercase())
                .filter(|o| !o.is_empty())
                .collect(),
        )
    }

    /// Whether the App may act for `owner`.
    #[must_use]
    pub fn allows(&self, owner: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Only(owners) => owners.contains(&owner.trim().to_ascii_lowercase()),
        }
    }
}

/// An installation of the App, as GitHub reported it for an account.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Installation {
    /// The installation's ID: what the token is minted for.
    pub id: u64,
    /// The account's login, in the case GitHub spells it.
    pub account: String,
}

/// How a [`GitHubApp`] knows which installation to mint for.
enum Mode {
    /// One installation, named by the deployment: no lookup, one token for every repository.
    Pinned(u64),
    /// The installation of the repository's owner, found with the App's JWT, for the accounts
    /// `AppOwners` allows.
    Discovering(AppOwners),
}

/// What is known about one account.
enum Entry {
    /// Nothing, or something that was dropped.
    Unknown,
    Installed(Installation),
    /// Both lookups answered 404 and the clock is before `until`.
    NotInstalled {
        until: DateTime<Utc>,
    },
}

/// The App's name and page, from `GET /app`.
struct Identity {
    slug: String,
    html_url: Option<String>,
}

/// A map of at most `cap` entries that forgets the least recently used one first. `cap` is at most
/// a few hundred, so the scan that finds it is cheaper than a list that tracks it.
struct Lru<K, V> {
    cap: usize,
    tick: u64,
    map: HashMap<K, (u64, V)>,
}

impl<K: Hash + Eq + Clone, V: Clone> Lru<K, V> {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            tick: 0,
            map: HashMap::new(),
        }
    }

    /// The value of `key`, made with `make` if it has none, and now the most recently used.
    fn get_or_insert_with(&mut self, key: &K, make: impl FnOnce() -> V) -> V {
        self.tick += 1;
        if let Some((used, value)) = self.map.get_mut(key) {
            *used = self.tick;
            return value.clone();
        }
        if self.map.len() >= self.cap
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(k, _)| k.clone())
        {
            self.map.remove(&oldest);
        }
        let value = make();
        self.map.insert(key.clone(), (self.tick, value.clone()));
        value
    }

    fn get(&self, key: &K) -> Option<V> {
        self.map.get(key).map(|(_, v)| v.clone())
    }
}

/// A std mutex that a panic elsewhere does not turn into a second panic: what it guards is a
/// cache, always in a state that is safe to read.
fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A failed trade, and whether GitHub said the installation does not exist (`404`).
struct Failure {
    error: WorkspaceError,
    gone: bool,
}

impl From<WorkspaceError> for Failure {
    fn from(error: WorkspaceError) -> Self {
        Self { error, gone: false }
    }
}

/// [`GitCredentials`] for a GitHub App: [`token_for`](GitCredentials::token_for) is an installation
/// access token.
///
/// Two ways to say which installation:
///
/// * **Pinned** ([`GitHubApp::new`]): one installation, named by the deployment. Its token is the
///   same for every repository the installation can reach, and nothing is looked up.
/// * **Discovering** ([`GitHubApp::discovering`]): the installation is the one on the **owner** of
///   the repository, found with the App's JWT (`GET /orgs/{owner}/installation`, and on a `404`
///   `GET /users/{owner}/installation`), so one process serves every account the App is installed
///   on that [`AppOwners`] allows. The owner is checked before anything is signed.
///
/// **It hands the token out for any host.** Wrap it in
/// [`HostScoped`](crate::HostScoped) so that the host of the repository is checked before a token
/// is minted, as [`ScopedToken`](crate::ScopedToken) does for a personal access token.
///
/// # The token cycle
///
/// A cached token with more than five minutes left is returned as it is. Otherwise one caller at
/// a time **for that installation** (the others wait for it, and find the new token; another
/// installation's callers do not wait) signs a JWT (`iat` a minute ago, `exp` nine minutes ahead,
/// `iss` the App) and trades it for a token. A failed mint is not cached: the next call tries
/// again. At most [`MAX_CACHED_INSTALLATIONS`] installations' tokens are kept.
///
/// # Finding the installation
///
/// The answer for an account is kept (at most 256 accounts) until a mint for its installation
/// answers `404` (uninstalled, or installed again under a new ID): the entry is dropped and the
/// lookup is done once more. "Not installed" is believed for 60 seconds. Logins are compared
/// without case. A lookup of an account that was renamed is not kept under the old name. A lookup
/// that fails (rate limit, `5xx`, transport) is never kept. Callers that ask for one owner at once
/// make one lookup.
///
/// # Errors
///
/// [`token_for`](GitCredentials::token_for) fails with
///
/// * [`WorkspaceError::Auth`] when GitHub says `401`, `403` or `404` to a mint (the message names
///   the variables to check), when the App is not installed on the owner (the message names the
///   App, from `GET /app`, and the owner) and when the installation is suspended;
/// * [`WorkspaceError::Invalid`] for an owner `AppOwners` does not allow, or a repository that is
///   not on a host (a local path);
/// * [`WorkspaceError::RateLimited`] for a `429` or a rate-limited `403`, and
///   [`WorkspaceError::Transient`] for a `5xx`, a transport failure or an answer that cannot be
///   read, on a lookup as on a mint.
pub struct GitHubApp {
    http: reqwest::Client,
    api_base: String,
    app_id: String,
    mode: Mode,
    key: AppKey,
    clock: Clock,
    /// One slot per installation, each held while a token is minted, so that concurrent callers of
    /// an installation share one request and callers of two do not wait for each other.
    tokens: Mutex<Lru<u64, Arc<tokio::sync::Mutex<Option<Cached>>>>>,
    /// One slot per account (lowercased login), each held while the account is looked up, so that
    /// concurrent callers of an owner share one lookup.
    accounts: Mutex<Lru<String, Arc<tokio::sync::Mutex<Entry>>>>,
    /// The App's name for the messages, fetched the first time one is needed and then kept; `None`
    /// when `GET /app` failed, and the messages name the App's ID.
    identity: tokio::sync::OnceCell<Option<Identity>>,
}

impl fmt::Debug for GitHubApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut shown = f.debug_struct("GitHubApp");
        shown
            .field("api_base", &self.api_base)
            .field("app_id", &self.app_id);
        match &self.mode {
            Mode::Pinned(id) => shown.field("installation_id", id),
            Mode::Discovering(owners) => shown
                .field("installation_id", &"found per owner")
                .field("owners", owners),
        };
        shown.finish_non_exhaustive()
    }
}

impl GitHubApp {
    /// The installation `installation_id` of the App `app_id` (its application ID, or its client
    /// ID: what the JWT's `iss` says), with its private key, against the API root `api_base`
    /// (`https://api.github.com`, `https://<host>/api/v3` for GitHub Enterprise Server, or a mock).
    /// Every repository gets this installation's token and nothing is looked up.
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
        Self::build(api_base, app_id, Mode::Pinned(installation_id), key)
    }

    /// The App `app_id` with no installation named: a repository gets the token of the
    /// installation on its owner, found with the App's JWT, for the accounts `owners` allows
    /// ([`AppOwners::Any`] says every account the App is installed on, which for a public App is
    /// every account whose owner chose to install it).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] if the HTTP client cannot be constructed.
    pub fn discovering(
        api_base: impl Into<String>,
        app_id: impl Into<String>,
        key: AppKey,
        owners: AppOwners,
    ) -> WorkspaceResult<Self> {
        Self::build(api_base, app_id, Mode::Discovering(owners), key)
    }

    fn build(
        api_base: impl Into<String>,
        app_id: impl Into<String>,
        mode: Mode,
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
            mode,
            key,
            clock: Arc::new(Utc::now),
            tokens: Mutex::new(Lru::new(MAX_CACHED_INSTALLATIONS)),
            accounts: Mutex::new(Lru::new(MAX_CACHED_ACCOUNTS)),
            identity: tokio::sync::OnceCell::new(),
        })
    }

    /// Read the time from `clock` instead of the system clock: what the JWT says, when a
    /// cached token counts as about to expire, and how long "not installed" is believed.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// The installation this App was pinned to, or `None` when it finds them by owner.
    #[must_use]
    pub fn pinned_installation(&self) -> Option<u64> {
        match self.mode {
            Mode::Pinned(id) => Some(id),
            Mode::Discovering(_) => None,
        }
    }

    /// The installation of the App on the account `owner`: the answer kept for it, or a lookup
    /// (`GET /orgs/{owner}/installation`, then on a `404` `GET /users/{owner}/installation`).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] for an owner that is not an account name, one `AppOwners` does
    /// not allow (before anything is signed) or an App that is pinned (it has no lookup);
    /// [`WorkspaceError::Auth`] when the App is not installed on `owner` or the installation is
    /// suspended; [`WorkspaceError::RateLimited`] and [`WorkspaceError::Transient`] when GitHub
    /// could not answer, which is not remembered.
    #[tracing::instrument(skip(self))]
    pub async fn installation_for(&self, owner: &str) -> WorkspaceResult<Installation> {
        let Mode::Discovering(owners) = &self.mode else {
            return Err(WorkspaceError::Invalid(
                "this GitHub App is pinned to one installation and does not look installations up"
                    .to_owned(),
            ));
        };
        check_account_name(owner)?;
        if !owners.allows(owner) {
            return Err(not_allowed(owner));
        }
        let key = owner.to_ascii_lowercase();
        let slot = self.account_slot(&key);
        let mut entry = slot.lock().await;
        let now = (self.clock)();
        match &*entry {
            Entry::Installed(found) => return Ok(found.clone()),
            Entry::NotInstalled { until } if now < *until => {
                return Err(self.not_installed(owner).await);
            }
            Entry::Unknown | Entry::NotInstalled { .. } => {}
        }
        // A failure leaves the entry as it was: nothing that went wrong is kept.
        let Some(found) = self.look_up(owner, now).await? else {
            *entry = Entry::NotInstalled {
                until: now + NOT_INSTALLED_FOR,
            };
            return Err(self.not_installed(owner).await);
        };
        if found.suspended {
            *entry = Entry::Unknown;
            return Err(WorkspaceError::Auth(format!(
                "the GitHub App's installation on `{owner}` is suspended: unsuspend it in the \
                 account's settings"
            )));
        }
        let installation = Installation {
            id: found.id,
            account: found.login.unwrap_or_else(|| owner.to_owned()),
        };
        if installation.account.eq_ignore_ascii_case(owner) {
            *entry = Entry::Installed(installation.clone());
            return Ok(installation);
        }
        // The account was renamed: GitHub answered for the old name with the new one. The old
        // name is not kept (it may be somebody else's tomorrow), the new one is, and it has to be
        // an account the deployment allows too.
        *entry = Entry::Unknown;
        if !owners.allows(&installation.account) {
            return Err(WorkspaceError::Invalid(format!(
                "`{owner}` is now `{}`, which GITHUB_APP_OWNERS does not allow",
                installation.account
            )));
        }
        let renamed = self.account_slot(&installation.account.to_ascii_lowercase());
        // Not waited for: this is only a head start for the next caller.
        if let Ok(mut other) = renamed.try_lock() {
            *other = Entry::Installed(installation.clone());
        }
        Ok(installation)
    }

    fn account_slot(&self, key: &String) -> Arc<tokio::sync::Mutex<Entry>> {
        locked(&self.accounts)
            .get_or_insert_with(key, || Arc::new(tokio::sync::Mutex::new(Entry::Unknown)))
    }

    /// Forget what is kept for `owner`, if it is still the installation `id`.
    async fn forget(&self, owner: &str, id: u64) {
        let slot = locked(&self.accounts).get(&owner.to_ascii_lowercase());
        if let Some(slot) = slot {
            let mut entry = slot.lock().await;
            if matches!(&*entry, Entry::Installed(i) if i.id == id) {
                *entry = Entry::Unknown;
            }
        }
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

    /// A request to the API with the App's JWT as the bearer.
    async fn send_as_app(
        &self,
        method: Method,
        path: &str,
        jwt: &SecretString,
        what: &str,
    ) -> Result<Response, WorkspaceError> {
        self.http
            .request(method, format!("{}{path}", self.api_base))
            .header(AUTHORIZATION, format!("Bearer {}", jwt.expose_secret()))
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, DEFAULT_USER_AGENT)
            .send()
            .await
            // `without_url`: nothing of the request, whatever it names, rides along in the error.
            .map_err(|e| WorkspaceError::transient(what).with_source(e.without_url()))
    }

    /// Trade a fresh JWT for an installation access token of `installation`.
    async fn mint(&self, installation: u64, now: DateTime<Utc>) -> Result<Cached, Failure> {
        let jwt = self.jwt(now)?;
        let response = self
            .send_as_app(
                Method::POST,
                &format!("/app/installations/{installation}/access_tokens"),
                &jwt,
                "the GitHub App's installation token was not minted",
            )
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Failure {
                gone: status == StatusCode::NOT_FOUND,
                error: self.refused(response, &jwt).await,
            });
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

    /// The token of `installation`: the kept one, or a new one, one caller minting at a time.
    async fn token_of(&self, installation: u64) -> Result<SecretString, Failure> {
        let slot = locked(&self.tokens)
            .get_or_insert_with(&installation, || Arc::new(tokio::sync::Mutex::new(None)));
        // One caller mints; the others wait here and find the token it made.
        let mut cache = slot.lock().await;
        let now = (self.clock)();
        if let Some(cached) = cache.as_ref()
            && cached.expires_at - now > REFRESH_MARGIN
        {
            return Ok(cached.token.clone());
        }
        match self.mint(installation, now).await {
            Ok(fresh) => {
                let token = fresh.token.clone();
                *cache = Some(fresh);
                Ok(token)
            }
            Err(failure) => {
                if failure.gone {
                    *cache = None;
                }
                Err(failure)
            }
        }
    }

    /// What GitHub says about the App's installation on `owner`: the organisation's first, then
    /// the person's. `None` when it has none (both said `404`).
    async fn look_up(&self, owner: &str, now: DateTime<Utc>) -> WorkspaceResult<Option<Found>> {
        let jwt = self.jwt(now)?;
        for kind in ["orgs", "users"] {
            let response = self
                .send_as_app(
                    Method::GET,
                    &format!("/{kind}/{owner}/installation"),
                    &jwt,
                    "the GitHub App's installation was not looked up",
                )
                .await?;
            let status = response.status();
            if status == StatusCode::NOT_FOUND {
                continue;
            }
            if !status.is_success() {
                return Err(self.refused(response, &jwt).await);
            }
            #[derive(Deserialize)]
            struct Body {
                id: u64,
                account: Option<Account>,
                suspended_at: Option<String>,
            }
            #[derive(Deserialize)]
            struct Account {
                login: Option<String>,
            }
            let body: Body = response.json().await.map_err(|_| {
                WorkspaceError::transient(
                    "GitHub answered the installation lookup with a success status and an \
                     unreadable body",
                )
            })?;
            return Ok(Some(Found {
                id: body.id,
                login: body.account.and_then(|a| a.login),
                suspended: body.suspended_at.is_some(),
            }));
        }
        Ok(None)
    }

    /// The App's slug and page, from `GET /app`, asked the first time a message needs them.
    async fn identity(&self) -> Option<&Identity> {
        self.identity
            .get_or_init(|| async {
                #[derive(Deserialize)]
                struct Body {
                    slug: String,
                    html_url: Option<String>,
                }
                let jwt = self.jwt((self.clock)()).ok()?;
                let response = self
                    .send_as_app(Method::GET, "/app", &jwt, "the GitHub App was not read")
                    .await
                    .ok()?;
                if !response.status().is_success() {
                    return None;
                }
                let body: Body = response.json().await.ok()?;
                Some(Identity {
                    slug: body.slug,
                    html_url: body.html_url,
                })
            })
            .await
            .as_ref()
    }

    /// The error for an account the App is not installed on.
    async fn not_installed(&self, owner: &str) -> WorkspaceError {
        let message = match self.identity().await {
            Some(Identity {
                slug,
                html_url: Some(url),
            }) => format!(
                "the GitHub App `{slug}` is not installed on `{owner}` (install it at \
                 {}/installations/new, or grant it the repository)",
                url.trim_end_matches('/')
            ),
            Some(Identity {
                slug,
                html_url: None,
            }) => format!(
                "the GitHub App `{slug}` is not installed on `{owner}` (install it on that \
                 account, or grant it the repository)"
            ),
            None => format!(
                "the GitHub App `{}` is not installed on `{owner}` (install it on that account, \
                 or grant it the repository)",
                self.app_id
            ),
        };
        WorkspaceError::Auth(message)
    }

    /// What to check when GitHub refuses the App's credentials: the installation is a variable
    /// only when it is pinned.
    fn check_hint(&self) -> &'static str {
        match self.mode {
            Mode::Pinned(_) => {
                "check GITHUB_APP_ID, GITHUB_APP_INSTALLATION_ID and the private key \
                 (GITHUB_APP_PRIVATE_KEY_PATH), and that the App is installed"
            }
            Mode::Discovering(_) => {
                "check GITHUB_APP_ID and the private key (GITHUB_APP_PRIVATE_KEY_PATH), and that \
                 the App is installed"
            }
        }
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
                    "GitHub refused the App's credentials (HTTP {}: {said}): {}",
                    status.as_u16(),
                    self.check_hint()
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

/// What an installation lookup found.
struct Found {
    id: u64,
    login: Option<String>,
    suspended: bool,
}

/// `owner` is going into a URL path: only what a login can be.
fn check_account_name(owner: &str) -> WorkspaceResult<()> {
    let ok = !owner.is_empty()
        && owner != "."
        && owner != ".."
        && owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(WorkspaceError::Invalid(format!(
            "{owner:?} is not an account name"
        )))
    }
}

fn not_allowed(owner: &str) -> WorkspaceError {
    WorkspaceError::Invalid(format!(
        "the GitHub App is not allowed to act for `{owner}`: it is not in GITHUB_APP_OWNERS"
    ))
}

#[async_trait]
impl GitCredentials for GitHubApp {
    #[tracing::instrument(
        skip(self, repo),
        fields(owner = tracing::field::Empty, installation = tracing::field::Empty)
    )]
    async fn token_for(&self, repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
        let span = tracing::Span::current();
        let owners = match &self.mode {
            Mode::Pinned(id) => {
                span.record("installation", id);
                return self.token_of(*id).await.map_err(|f| f.error);
            }
            Mode::Discovering(owners) => owners,
        };
        let location = repo.locate()?;
        if location.is_local() {
            return Err(WorkspaceError::Invalid(
                "a local repository has no GitHub owner to look an installation up for".to_owned(),
            ));
        }
        let owner = location.owner;
        span.record("owner", owner.as_str());
        // Before anything is looked up or signed.
        if !owners.allows(&owner) {
            return Err(not_allowed(&owner));
        }
        let installation = self.installation_for(&owner).await?;
        span.record("installation", installation.id);
        match self.token_of(installation.id).await {
            Ok(token) => Ok(token),
            Err(Failure { gone: true, .. }) => {
                // Uninstalled, or installed again with another ID: what was kept is stale. Look
                // the owner up once more, and believe what the second mint says.
                self.forget(&owner, installation.id).await;
                let again = self.installation_for(&owner).await?;
                span.record("installation", again.id);
                match self.token_of(again.id).await {
                    Ok(token) => Ok(token),
                    Err(failure) => {
                        if failure.gone {
                            self.forget(&owner, again.id).await;
                        }
                        Err(failure.error)
                    }
                }
            }
            Err(failure) => Err(failure.error),
        }
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

    #[test]
    fn the_least_recently_used_entry_is_the_one_forgotten() {
        let mut lru: Lru<u64, u64> = Lru::new(2);
        assert_eq!(lru.get_or_insert_with(&1, || 10), 10);
        assert_eq!(lru.get_or_insert_with(&2, || 20), 20);
        // Using 1 again makes 2 the oldest.
        assert_eq!(lru.get_or_insert_with(&1, || 99), 10);
        assert_eq!(lru.get_or_insert_with(&3, || 30), 30);
        assert_eq!(lru.get(&1), Some(10));
        assert_eq!(lru.get(&2), None, "evicted");
        assert_eq!(lru.get(&3), Some(30));
        assert_eq!(lru.map.len(), 2, "never more than the bound");
    }

    #[test]
    fn owners_are_compared_without_case_and_blanks_allow_nothing() {
        let owners = AppOwners::only(["Acme", " other-org ", ""]);
        for yes in ["acme", "ACME", "Other-Org"] {
            assert!(owners.allows(yes), "{yes}");
        }
        for no in ["", "acme-evil", "evil"] {
            assert!(!owners.allows(no), "{no:?}");
        }
        assert!(!AppOwners::only(Vec::<String>::new()).allows("acme"));
        assert!(!AppOwners::only([" ", ""]).allows(""));
        assert!(AppOwners::Any.allows("anyone"));
    }

    #[test]
    fn an_owner_goes_into_a_path_only_if_it_can_be_a_login() {
        for ok in ["acme", "Acme-Co", "a_b", "a.b", "x1"] {
            assert!(check_account_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a b", "a?b", "a%2Fb", "é", "a\nb"] {
            assert!(check_account_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn debug_output_of_a_discovering_app_shows_the_mode_and_the_owners_and_no_secret() {
        let key = TestAppKey::generate();
        let app = GitHubApp::discovering(
            "https://api.github.com",
            "1",
            AppKey::from_pem(&key.pkcs8_pem).unwrap(),
            AppOwners::only(["Acme"]),
        )
        .unwrap();
        let text = format!("{app:?}");
        assert!(text.contains("found per owner"), "{text}");
        assert!(text.contains("acme"), "{text}");
        assert!(!text.contains("BEGIN"), "{text}");
        assert_eq!(app.pinned_installation(), None);
    }
}
