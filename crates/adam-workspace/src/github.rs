//! [`CodeHost`] for GitHub (and GitHub Enterprise) over the REST API.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use crate::code_host::{CodeHost, NewPullRequest, PullRequest};
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

    async fn find(
        &self,
        token: &SecretString,
        repo: &RepoRef,
        head: &str,
    ) -> WorkspaceResult<Option<PullRequest>> {
        let (owner, name) = slug(repo)?;
        let (head_owner, branch) = split_head(&owner, head);
        let resp = self
            .request(Method::GET, &format!("/repos/{owner}/{name}/pulls"), token)
            .query(&[
                ("head", format!("{head_owner}:{branch}")),
                ("state", "open".to_owned()),
                ("per_page", "100".to_owned()),
            ])
            .send()
            .await
            .map_err(|e| transport(e, token))?;
        let resp = check(resp, token).await?;
        let pulls: Vec<ApiPull> = resp.json().await.map_err(|e| decode_error(e, token))?;
        // The API filters by `owner:branch`; double-check the branch so a
        // lenient server (or mock) cannot make us return somebody else's PR.
        // A PR without a `head` cannot be proven to be ours, so it never matches.
        Ok(pulls
            .into_iter()
            .find(|p| p.head.as_ref().is_some_and(|h| h.branch == branch))
            .map(|p| p.into_pull_request(branch)))
    }
}

#[async_trait]
impl CodeHost for GitHub {
    #[tracing::instrument(skip(self, pr), fields(repo = %pr.repo.url, head = %pr.head))]
    async fn open_pull_request(&self, pr: NewPullRequest) -> Result<PullRequest, WorkspaceError> {
        let token = self.creds.token_for(&pr.repo).await?;
        if let Some(existing) = self.find(&token, &pr.repo, &pr.head).await? {
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
                match self.find(&token, &pr.repo, &pr.head).await? {
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
        self.find(&token, repo, head).await
    }
}

#[derive(Deserialize)]
struct ApiPull {
    number: u64,
    html_url: String,
    head: Option<ApiHead>,
}

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
    WorkspaceError::Transient(scrub(&e.without_url().to_string(), token))
}

/// A 2xx response whose body does not decode: usually a truncated or
/// proxy-mangled response, so it is worth retrying.
fn decode_error(e: reqwest::Error, token: &SecretString) -> WorkspaceError {
    WorkspaceError::Transient(scrub(
        &format!(
            "code host sent a success status with an unreadable body: {}",
            e.without_url()
        ),
        token,
    ))
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
    let body = resp.text().await.unwrap_or_default();
    let message = scrub(&api_message(&body), token);
    Err(match status {
        StatusCode::UNAUTHORIZED => WorkspaceError::Auth(message),
        _ if rate_limited && status == StatusCode::FORBIDDEN => WorkspaceError::Transient(message),
        StatusCode::FORBIDDEN => WorkspaceError::Auth(message),
        StatusCode::NOT_FOUND => WorkspaceError::NotFound(message),
        StatusCode::UNPROCESSABLE_ENTITY => WorkspaceError::Invalid(message),
        s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => {
            WorkspaceError::Transient(format!("HTTP {}: {message}", s.as_u16()))
        }
        s => WorkspaceError::Http {
            status: s.as_u16(),
            message,
        },
    })
}

/// GitHub's `{"message": .., "errors": [{"message": .., "code": ..}]}`
/// flattened into one line; falls back to the (truncated) raw body.
fn api_message(body: &str) -> String {
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
