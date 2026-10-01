//! [`CodeHost`] for GitHub (and GitHub Enterprise) over the REST API.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use crate::code_host::{
    CodeHost, CreatedRepository, NewPullRequest, NewRepository, OwnerKind, PullRequest,
};
use crate::credentials::DynGitCredentials;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::repo::RepoRef;

const DEFAULT_API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const DEFAULT_USER_AGENT: &str = concat!("adam-workspace/", env!("CARGO_PKG_VERSION"));

/// GitHub REST client implementing [`CodeHost`].
///
/// The token for each call comes from the [`GitCredentials`](crate::GitCredentials)
/// the client was built with and is sent as `Authorization: Bearer`.
///
/// For GitHub Enterprise Server, or a mock server in tests, point it at a
/// different API root with [`GitHub::with_api_base`] (for GHE that is
/// `https://<host>/api/v3`).
#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    api_base: String,
    creds: DynGitCredentials,
}

impl fmt::Debug for GitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHub")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

impl GitHub {
    /// A client for `https://api.github.com`.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Invalid`] if the HTTP client cannot be constructed
    /// (for example when the TLS backend fails to initialise).
    pub fn new(creds: DynGitCredentials) -> WorkspaceResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(DEFAULT_USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| WorkspaceError::Invalid(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            http,
            api_base: DEFAULT_API_BASE.to_owned(),
            creds,
        })
    }

    /// Use a different API root, e.g. `https://ghe.example.com/api/v3`.
    #[must_use]
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into().trim_end_matches('/').to_owned();
        self
    }

    fn request(&self, method: Method, path: &str, token: &SecretString) -> RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.api_base))
            .header(AUTHORIZATION, format!("Bearer {}", token.expose_secret()))
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, DEFAULT_USER_AGENT)
    }

    /// The open pull request from `head`; against `repo.base_branch` when `on_base`, against any
    /// base otherwise.
    async fn find(
        &self,
        token: &SecretString,
        repo: &RepoRef,
        head: &str,
        on_base: bool,
    ) -> WorkspaceResult<Option<PullRequest>> {
        let (owner, name) = slug(repo)?;
        let (head_owner, branch) = split_head(&owner, head);
        let mut query = vec![
            ("head", format!("{head_owner}:{branch}")),
            ("state", "open".to_owned()),
            ("per_page", "100".to_owned()),
        ];
        if on_base {
            query.push(("base", repo.base_branch.clone()));
        }
        let resp = self
            .request(Method::GET, &format!("/repos/{owner}/{name}/pulls"), token)
            .query(&query)
            .send()
            .await
            .map_err(|e| transport(e, token))?;
        let resp = check(resp, token).await?;
        let pulls: Vec<ApiPull> = resp.json().await.map_err(|e| decode_error(e, token))?;
        // The API filters by `owner:branch` and base; double-check both so a lenient server (or
        // mock) cannot make us return somebody else's PR, or one against another base branch.
        // A PR without a `head` or a `base` cannot be proven to be ours, so it never matches.
        Ok(pulls
            .into_iter()
            .find(|p| {
                p.head.as_ref().is_some_and(|h| h.branch == branch)
                    && (!on_base
                        || p.base
                            .as_ref()
                            .is_some_and(|b| b.branch == repo.base_branch))
            })
            .map(|p| p.into_pull_request(branch)))
    }
}

#[async_trait]
impl CodeHost for GitHub {
    #[tracing::instrument(skip(self, pr), fields(repo = %pr.repo.url, head = %pr.head))]
    async fn open_pull_request(&self, pr: NewPullRequest) -> Result<PullRequest, WorkspaceError> {
        let token = self.creds.token_for(&pr.repo).await?;
        if let Some(existing) = self.find(&token, &pr.repo, &pr.head, true).await? {
            return Ok(existing);
        }
        let (owner, name) = slug(&pr.repo)?;
        let resp = self
            .request(
                Method::POST,
                &format!("/repos/{owner}/{name}/pulls"),
                &token,
            )
            .json(&json!({
                "title": pr.title,
                "body": pr.body,
                "head": pr.head,
                "base": pr.repo.base_branch,
                "draft": pr.draft,
            }))
            .send()
            .await
            .map_err(|e| transport(e, &token))?;

        match check(resp, &token).await {
            Ok(resp) => {
                let created: ApiPull = resp.json().await.map_err(|e| decode_error(e, &token))?;
                let (_, branch) = split_head(&owner, &pr.head);
                Ok(created.into_pull_request(branch))
            }
            // Lost a race with another opener: the PR now exists, so return it.
            Err(WorkspaceError::Invalid(message)) if message.contains("already exists") => {
                match self.find(&token, &pr.repo, &pr.head, true).await? {
                    Some(existing) => Ok(existing),
                    None => Err(WorkspaceError::Invalid(message)),
                }
            }
            Err(e) => Err(e),
        }
    }

    #[tracing::instrument(skip(self), fields(repo = %repo.url))]
    async fn find_pull_request(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError> {
        let token = self.creds.token_for(repo).await?;
        self.find(&token, repo, head, true).await
    }

    #[tracing::instrument(skip(self), fields(repo = %repo.url))]
    async fn find_pull_request_on_head(
        &self,
        repo: &RepoRef,
        head: &str,
    ) -> Result<Option<PullRequest>, WorkspaceError> {
        let token = self.creds.token_for(repo).await?;
        self.find(&token, repo, head, false).await
    }

    #[tracing::instrument(skip(self, body), fields(repo = %repo.url, number))]
    async fn comment_on_pull_request(
        &self,
        repo: &RepoRef,
        number: u64,
        body: &str,
    ) -> Result<(), WorkspaceError> {
        let token = self.creds.token_for(repo).await?;
        let (owner, name) = slug(repo)?;
        // Pull requests are issues for comments.
        let resp = self
            .request(
                Method::POST,
                &format!("/repos/{owner}/{name}/issues/{number}/comments"),
                &token,
            )
            .json(&json!({ "body": body }))
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        check(resp, &token).await.map(|_| ())
    }

    #[tracing::instrument(skip(self, new), fields(repo = %new.repo.url, private = new.private))]
    async fn create_repository(
        &self,
        new: NewRepository,
    ) -> Result<CreatedRepository, WorkspaceError> {
        // The credentials are asked for the address the repository will have: the host check and
        // the token are those of that host, before anything is sent.
        let token = self.creds.token_for(&new.repo).await?;
        let (owner, name) = slug(&new.repo)?;
        let path = match new.kind {
            OwnerKind::Organization => format!("/orgs/{owner}/repos"),
            OwnerKind::User => "/user/repos".to_owned(),
        };
        let mut body = json!({"name": name, "private": new.private, "auto_init": false});
        if let Some(description) = &new.description {
            body["description"] = json!(description);
        }
        let resp = self
            .request(Method::POST, &path, &token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        let resp = check(resp, &token).await?;
        let created: ApiRepository = resp.json().await.map_err(|e| decode_error(e, &token))?;
        Ok(CreatedRepository {
            full_name: created.full_name,
            clone_url: created.clone_url,
            html_url: created.html_url,
            default_branch: created.default_branch.unwrap_or_else(|| "main".to_owned()),
        })
    }

    #[tracing::instrument(skip(self, repo), fields(repo = %repo.url))]
    async fn find_repository(
        &self,
        repo: &RepoRef,
    ) -> Result<Option<CreatedRepository>, WorkspaceError> {
        let token = self.creds.token_for(repo).await?;
        let (owner, name) = slug(repo)?;
        let resp = self
            .request(Method::GET, &format!("/repos/{owner}/{name}"), &token)
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        let found = match check(resp, &token).await {
            Ok(resp) => resp,
            Err(WorkspaceError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let found: ApiRepository = found.json().await.map_err(|e| decode_error(e, &token))?;
        Ok(Some(CreatedRepository {
            full_name: found.full_name,
            clone_url: found.clone_url,
            html_url: found.html_url,
            default_branch: found.default_branch.unwrap_or_else(|| "main".to_owned()),
        }))
    }

    #[tracing::instrument(skip(self, host_repo), fields(owner))]
    async fn owner_kind(
        &self,
        owner: &str,
        host_repo: &RepoRef,
    ) -> Result<OwnerKind, WorkspaceError> {
        let token = self.creds.token_for(host_repo).await?;
        // The login is a path segment: only what a login can be.
        if owner.is_empty()
            || !owner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(WorkspaceError::Invalid(format!(
                "{owner:?} is not an account name"
            )));
        }
        let resp = self
            .request(Method::GET, &format!("/users/{owner}"), &token)
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        let account: ApiAccount = check(resp, &token)
            .await?
            .json()
            .await
            .map_err(|e| decode_error(e, &token))?;
        Ok(if account.kind.eq_ignore_ascii_case("Organization") {
            OwnerKind::Organization
        } else {
            OwnerKind::User
        })
    }

    #[tracing::instrument(skip(self, host_repo))]
    async fn authenticated_login(
        &self,
        host_repo: &RepoRef,
    ) -> Result<Option<String>, WorkspaceError> {
        let token = self.creds.token_for(host_repo).await?;
        let resp = self
            .request(Method::GET, "/user", &token)
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        // An installation token is not a user: GitHub answers `GET /user` with 403 "Resource not
        // accessible by integration". That is "no login", not a failure.
        if resp.status() == StatusCode::FORBIDDEN && !is_rate_limited(&resp) {
            return Ok(None);
        }
        let account: ApiAccount = check(resp, &token)
            .await?
            .json()
            .await
            .map_err(|e| decode_error(e, &token))?;
        Ok(account.login)
    }
}

/// What a repository creation answers.
#[derive(Deserialize)]
struct ApiRepository {
    full_name: String,
    clone_url: String,
    html_url: String,
    default_branch: Option<String>,
}

/// A user or an organisation (`GET /users/{login}`, `GET /user`).
#[derive(Deserialize)]
struct ApiAccount {
    login: Option<String>,
    #[serde(rename = "type", default)]
    kind: String,
}

/// Whether a response says its rate limit is used up (as [`check`] reads it).
fn is_rate_limited(resp: &Response) -> bool {
    resp.status() == StatusCode::TOO_MANY_REQUESTS
        || resp
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0")
}

#[derive(Deserialize)]
struct ApiPull {
    number: u64,
    html_url: String,
    head: Option<ApiHead>,
    base: Option<ApiHead>,
}

/// One end of a pull request: `head` or `base`.
#[derive(Deserialize)]
struct ApiHead {
    #[serde(rename = "ref")]
    branch: String,
}

impl ApiPull {
    fn into_pull_request(self, fallback_branch: &str) -> PullRequest {
        PullRequest {
            number: self.number,
            url: self.html_url,
            head: self
                .head
                .map_or_else(|| fallback_branch.to_owned(), |h| h.branch),
        }
    }
}

/// `(owner, repo)` of a GitHub repository URL.
fn slug(repo: &RepoRef) -> WorkspaceResult<(String, String)> {
    let loc = repo.locate()?;
    if loc.is_local() {
        return Err(WorkspaceError::Invalid(format!(
            "{} is a local path, not a GitHub repository",
            repo.url
        )));
    }
    Ok((loc.owner, loc.name))
}

/// `head` may be `branch` (same repository) or `owner:branch` (a fork).
fn split_head<'a>(default_owner: &'a str, head: &'a str) -> (&'a str, &'a str) {
    head.split_once(':').unwrap_or((default_owner, head))
}

fn transport(e: reqwest::Error, token: &SecretString) -> WorkspaceError {
    // `without_url` drops the only place a request can name a secret; the token itself travels
    // in a header, which reqwest errors never print.
    let e = e.without_url();
    WorkspaceError::transient(scrub("code host request failed", token)).with_source(e)
}

/// A 2xx response whose body does not decode: usually a truncated or
/// proxy-mangled response, so it is worth retrying.
fn decode_error(e: reqwest::Error, token: &SecretString) -> WorkspaceError {
    WorkspaceError::transient(scrub(
        "code host sent a success status with an unreadable body",
        token,
    ))
    .with_source(e.without_url())
}

/// Pass 2xx responses through, map everything else to a [`WorkspaceError`].
async fn check(resp: Response, token: &SecretString) -> WorkspaceResult<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || resp
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|v| v == "0");
    let retry_after = rate_limited
        .then(|| retry_after(resp.headers(), std::time::SystemTime::now()))
        .flatten();
    let body = resp.text().await.unwrap_or_default();
    let message = scrub(&api_message(&body), token);
    Err(match status {
        StatusCode::UNAUTHORIZED => WorkspaceError::Auth(message),
        _ if rate_limited
            && matches!(
                status,
                StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            ) =>
        {
            WorkspaceError::RateLimited { retry_after }
        }
        StatusCode::FORBIDDEN => WorkspaceError::Auth(message),
        StatusCode::NOT_FOUND => WorkspaceError::NotFound(message),
        StatusCode::UNPROCESSABLE_ENTITY => WorkspaceError::Invalid(message),
        s if s.is_server_error() => {
            WorkspaceError::transient(format!("HTTP {}: {message}", s.as_u16()))
        }
        s => WorkspaceError::Http {
            status: s.as_u16(),
            message,
        },
    })
}

/// The longest wait a rate-limited response can ask for; a larger value is a broken header.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// How long the host asked us to wait: `Retry-After` in seconds, else the moment its rate limit
/// resets (`x-ratelimit-reset`, epoch seconds) minus `now`. `None` when it said neither.
pub(crate) fn retry_after(
    headers: &reqwest::header::HeaderMap,
    now: std::time::SystemTime,
) -> Option<Duration> {
    let number =
        |name: &str| -> Option<u64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    let wait = match number("retry-after") {
        Some(secs) => Duration::from_secs(secs),
        None => {
            let reset = std::time::UNIX_EPOCH + Duration::from_secs(number("x-ratelimit-reset")?);
            reset.duration_since(now).unwrap_or_default()
        }
    };
    Some(wait.min(MAX_RETRY_AFTER))
}

/// GitHub's `{"message": .., "errors": [{"message": .., "code": ..}]}`
/// flattened into one line; falls back to the (truncated) raw body.
pub(crate) fn api_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct ApiError {
        message: Option<String>,
        #[serde(default)]
        errors: Vec<serde_json::Value>,
    }
    let Ok(parsed) = serde_json::from_str::<ApiError>(body) else {
        return truncate(body.trim(), 500);
    };
    let mut parts: Vec<String> = parsed.message.into_iter().collect();
    for e in parsed.errors {
        match e {
            serde_json::Value::String(s) => parts.push(s),
            serde_json::Value::Object(o) => {
                let text = o
                    .get("message")
                    .or_else(|| o.get("code"))
                    .and_then(|v| v.as_str());
                if let Some(text) = text {
                    parts.push(text.to_owned());
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        truncate(body.trim(), 500)
    } else {
        truncate(&parts.join(": "), 1000)
    }
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

fn scrub(text: &str, token: &SecretString) -> String {
    let secret = token.expose_secret();
    if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, "[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn retry_after_prefers_the_header_then_the_reset_time() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(
            retry_after(&headers(&[("retry-after", "7")]), now),
            Some(Duration::from_secs(7))
        );
        // Retry-After wins over the reset time.
        assert_eq!(
            retry_after(
                &headers(&[("retry-after", "7"), ("x-ratelimit-reset", "1090")]),
                now
            ),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            retry_after(&headers(&[("x-ratelimit-reset", "1090")]), now),
            Some(Duration::from_secs(90))
        );
        // A reset in the past is "now", not an underflow.
        assert_eq!(
            retry_after(&headers(&[("x-ratelimit-reset", "900")]), now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after(&headers(&[]), now), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "soon")]), now), None);
        // A broken header cannot park a run for a day.
        assert_eq!(
            retry_after(&headers(&[("retry-after", "86400")]), now),
            Some(MAX_RETRY_AFTER)
        );
    }
}
