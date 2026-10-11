//! [`CodeHost`] for GitHub (and GitHub Enterprise) over the REST API.

use std::fmt;
use std::time::Duration;

use adam_error::{Classify as _, ErrorClass};
use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use crate::code_host::{
    Checks, CodeHost, CreatedRepository, MergePullRequest, MergedPullRequest, NewPullRequest,
    NewRepository, OwnerKind, PullRequest, PullRequestStatus,
};
use crate::credentials::DynGitCredentials;
use crate::error::{WorkspaceError, WorkspaceResult};
use crate::repo::RepoRef;

const DEFAULT_API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
/// Check runs read for one commit: a page and how many pages. More are reported as unfinished.
const CHECK_RUNS_PER_PAGE: u32 = 100;
const MAX_CHECK_RUN_PAGES: u32 = 3;
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
    /// For what changes or reads a named repository, a pull request or its checks, before a merge
    /// or a deletion: **it follows no redirect**, so a repository that was renamed or transferred
    /// is never reached through its old address (see [`GitHub::verify_repository`]).
    exact: reqwest::Client,
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
        let build = |redirects: reqwest::redirect::Policy| {
            reqwest::Client::builder()
                .user_agent(DEFAULT_USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(redirects)
                .build()
                .map_err(|e| WorkspaceError::Invalid(format!("cannot build HTTP client: {e}")))
        };
        Ok(Self {
            http: build(reqwest::redirect::Policy::default())?,
            exact: build(reqwest::redirect::Policy::none())?,
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
        self.build(&self.http, method, path, token)
    }

    /// [`request`](Self::request) on the client that follows no redirect.
    fn exact_request(&self, method: Method, path: &str, token: &SecretString) -> RequestBuilder {
        self.build(&self.exact, method, path, token)
    }

    fn build(
        &self,
        http: &reqwest::Client,
        method: Method,
        path: &str,
        token: &SecretString,
    ) -> RequestBuilder {
        http.request(method, format!("{}{path}", self.api_base))
            .header(AUTHORIZATION, format!("Bearer {}", token.expose_secret()))
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(USER_AGENT, DEFAULT_USER_AGENT)
    }

    /// Check that `owner/name` is **that** repository on the host and not another one that GitHub
    /// redirects its address to. A repository that was renamed or transferred keeps answering at its
    /// old address with a redirect (and an address that is free again can be somebody else's), so what
    /// a person named a while ago can be a different repository now. Everything that merges, renames
    /// or deletes asks first, and none of it follows a redirect.
    ///
    /// [`WorkspaceError::Conflict`] when the host redirects or names the repository differently.
    async fn verify_repository(
        &self,
        token: &SecretString,
        owner: &str,
        name: &str,
    ) -> WorkspaceResult<()> {
        let resp = self
            .exact_request(Method::GET, &format!("/repos/{owner}/{name}"), token)
            .send()
            .await
            .map_err(|e| transport(e, token))?;
        if resp.status().is_redirection() {
            return Err(WorkspaceError::Conflict(format!(
                "{owner}/{name} has moved (the host answers {} for its address): it is not the \
                 repository that was named any more, so nothing was done to it",
                resp.status().as_u16()
            )));
        }
        let found: ApiRepoName = check(resp, token)
            .await?
            .json()
            .await
            .map_err(|e| decode_error(e, token))?;
        if found
            .full_name
            .eq_ignore_ascii_case(&format!("{owner}/{name}"))
        {
            Ok(())
        } else {
            Err(WorkspaceError::Conflict(format!(
                "{owner}/{name} is called {} on the host: it is not the repository that was named,                  so nothing was done to it",
                found.full_name
            )))
        }
    }

    /// `GET /repos/{owner}/{name}/pulls/{number}`.
    async fn read_pull(
        &self,
        token: &SecretString,
        (owner, name): (&str, &str),
        number: u64,
    ) -> WorkspaceResult<ApiPullDetail> {
        let resp = self
            .exact_request(
                Method::GET,
                &format!("/repos/{owner}/{name}/pulls/{number}"),
                token,
            )
            .send()
            .await
            .map_err(|e| transport(e, token))?;
        check(resp, token)
            .await?
            .json()
            .await
            .map_err(|e| decode_error(e, token))
    }

    /// What the host reports for the checks of commit `sha`: the combined status and every check run
    /// as [`Checks`], and whether it reported any at all. `mergeable_state` `clean` is the host calling
    /// the pull request ready, which stands in for success when nothing was reported.
    async fn checks_of(
        &self,
        token: &SecretString,
        (owner, name): (&str, &str),
        sha: &str,
        mergeable_state: Option<&str>,
    ) -> WorkspaceResult<(Checks, bool)> {
        if sha.is_empty() || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(WorkspaceError::Invalid(format!(
                "{sha:?} is not a commit id"
            )));
        }
        let resp = self
            .exact_request(
                Method::GET,
                &format!("/repos/{owner}/{name}/commits/{sha}/status"),
                token,
            )
            .send()
            .await
            .map_err(|e| transport(e, token))?;
        let status: ApiCombinedStatus = check(resp, token)
            .await?
            .json()
            .await
            .map_err(|e| decode_error(e, token))?;
        let mut runs = Vec::new();
        let mut truncated = false;
        for page in 1..=MAX_CHECK_RUN_PAGES {
            let resp = self
                .exact_request(
                    Method::GET,
                    &format!("/repos/{owner}/{name}/commits/{sha}/check-runs"),
                    token,
                )
                .query(&[
                    ("per_page", CHECK_RUNS_PER_PAGE.to_string()),
                    ("page", page.to_string()),
                ])
                .send()
                .await
                .map_err(|e| transport(e, token))?;
            let got: ApiCheckRuns = check(resp, token)
                .await?
                .json()
                .await
                .map_err(|e| decode_error(e, token))?;
            let empty = got.check_runs.is_empty();
            runs.extend(got.check_runs);
            if runs.len() as u64 >= got.total_count || empty {
                break;
            }
            truncated = page == MAX_CHECK_RUN_PAGES;
        }
        let reported = status.total_count > 0 || !runs.is_empty();
        Ok((
            classify_checks(mergeable_state, &status, &runs, truncated),
            reported,
        ))
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

    /// Delete the head branch of a merged pull request, if it is this repository's own and not
    /// the base: `(the branch deleted, why a branch was kept)`. A failure here is a note, never an
    /// error: the merge happened.
    async fn delete_head(
        &self,
        token: &SecretString,
        (owner, name): (&str, &str),
        pull: &ApiPullDetail,
    ) -> (Option<String>, Option<String>) {
        let Some(head) = &pull.head else {
            return (
                None,
                Some("the pull request names no head branch".to_owned()),
            );
        };
        let own = format!("{owner}/{name}");
        let in_this_repository = head
            .repo
            .as_ref()
            .is_some_and(|r| r.full_name.eq_ignore_ascii_case(&own));
        if !in_this_repository {
            return (
                None,
                Some(format!(
                    "the branch {} is not in {own} (a fork, or a repository that is gone), so it is kept",
                    head.branch
                )),
            );
        }
        if pull.base.as_ref().is_some_and(|b| b.branch == head.branch) {
            return (
                None,
                Some(format!(
                    "the branch {} is the base, so it is kept",
                    head.branch
                )),
            );
        }
        let path = format!(
            "/repos/{owner}/{name}/git/refs/heads/{}",
            encode_ref(&head.branch)
        );
        let sent = self
            .exact_request(Method::DELETE, &path, token)
            .send()
            .await;
        let outcome = match sent {
            Ok(resp) => check(resp, token).await.map(|_| ()),
            Err(e) => Err(transport(e, token)),
        };
        match outcome {
            Ok(()) => (Some(head.branch.clone()), None),
            // Deleted already (a repository that deletes merged branches by itself).
            Err(e) if e.class() == ErrorClass::NotFound => (
                None,
                Some(format!("the branch {} was already gone", head.branch)),
            ),
            Err(e) => (
                None,
                Some(format!("the branch {} was not deleted: {e}", head.branch)),
            ),
        }
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

    #[tracing::instrument(skip(self, repo), fields(repo = %repo.url, number))]
    async fn pull_request_status(
        &self,
        repo: &RepoRef,
        number: u64,
    ) -> Result<PullRequestStatus, WorkspaceError> {
        let token = self.creds.token_for(repo).await?;
        let (owner, name) = slug(repo)?;
        self.verify_repository(&token, &owner, &name).await?;
        let pull = self.read_pull(&token, (&owner, &name), number).await?;
        let Some(head) = pull.head else {
            return Err(WorkspaceError::Invalid(format!(
                "pull request #{number} names no head branch"
            )));
        };
        let head_sha = head.sha.unwrap_or_default();
        let (checks, ci_reported) = self
            .checks_of(
                &token,
                (&owner, &name),
                &head_sha,
                pull.mergeable_state.as_deref(),
            )
            .await?;
        Ok(PullRequestStatus {
            number,
            url: pull.html_url,
            open: pull.state == "open" && !pull.merged,
            merged: pull.merged,
            draft: pull.draft,
            head_branch: head.branch,
            head_repo: head.repo.map(|r| r.full_name),
            head_sha,
            base_branch: pull.base.map(|b| b.branch).unwrap_or_default(),
            checks,
            ci_reported,
        })
    }

    #[tracing::instrument(skip(self, merge), fields(repo = %merge.repo.url, number = merge.number))]
    async fn merge_pull_request(
        &self,
        merge: MergePullRequest,
    ) -> Result<MergedPullRequest, WorkspaceError> {
        let token = self.creds.token_for(&merge.repo).await?;
        let (owner, name) = slug(&merge.repo)?;
        self.verify_repository(&token, &owner, &name).await?;
        let path = format!("/repos/{owner}/{name}/pulls/{}", merge.number);
        // Read it first: what is merged is the head the caller decided on (`expected_sha`), else the
        // head the host shows now; a pull request that was merged already is reported as it is, and
        // the head branch is known for the delete.
        let pull = self
            .read_pull(&token, (&owner, &name), merge.number)
            .await?;
        if pull.merged {
            return Ok(MergedPullRequest {
                sha: pull.merge_commit_sha.unwrap_or_default(),
                url: pull.html_url,
                already_merged: true,
                deleted_branch: None,
                branch_note: None,
            });
        }
        if pull.state != "open" {
            return Err(WorkspaceError::Invalid(format!(
                "pull request #{} is {}, not open",
                merge.number, pull.state
            )));
        }
        let head_sha = pull.head.as_ref().and_then(|h| h.sha.as_deref());
        if let Some(expected) = &merge.expected_sha
            && head_sha != Some(expected.as_str())
        {
            return Err(WorkspaceError::Conflict(format!(
                "the head of pull request #{} is {} now, not {expected}, the commit that was \
                 looked at: it was not merged",
                merge.number,
                head_sha.unwrap_or("unknown")
            )));
        }
        let mut body = json!({"merge_method": merge.method.as_str()});
        if let Some(sha) = merge.expected_sha.as_deref().or(head_sha) {
            body["sha"] = json!(sha);
        }
        let resp = self
            .exact_request(Method::PUT, &format!("{path}/merge"), &token)
            .json(&body)
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        let merged: ApiMerge = check_merge(resp, &token)
            .await?
            .json()
            .await
            .map_err(|e| decode_error(e, &token))?;
        let (deleted_branch, branch_note) = if merge.delete_branch {
            self.delete_head(&token, (&owner, &name), &pull).await
        } else {
            (None, None)
        };
        Ok(MergedPullRequest {
            sha: merged.sha.unwrap_or_default(),
            url: pull.html_url,
            already_merged: false,
            deleted_branch,
            branch_note,
        })
    }

    #[tracing::instrument(skip(self, repo), fields(repo = %repo.url, new_name))]
    async fn rename_repository(
        &self,
        repo: &RepoRef,
        new_name: &str,
    ) -> Result<CreatedRepository, WorkspaceError> {
        let token = self.creds.token_for(repo).await?;
        let (owner, name) = slug(repo)?;
        // The new name is sent in a body, but it is still only ever a repository's name.
        if new_name.is_empty()
            || new_name.len() > 100
            || !new_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(WorkspaceError::Invalid(format!(
                "{new_name:?} is not a repository name"
            )));
        }
        self.verify_repository(&token, &owner, &name).await?;
        let resp = self
            .exact_request(Method::PATCH, &format!("/repos/{owner}/{name}"), &token)
            .json(&json!({ "name": new_name }))
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        let renamed: ApiRepository = check(resp, &token)
            .await?
            .json()
            .await
            .map_err(|e| decode_error(e, &token))?;
        Ok(CreatedRepository {
            full_name: renamed.full_name,
            clone_url: renamed.clone_url,
            html_url: renamed.html_url,
            default_branch: renamed.default_branch.unwrap_or_else(|| "main".to_owned()),
        })
    }

    #[tracing::instrument(skip(self, repo), fields(repo = %repo.url))]
    async fn delete_repository(&self, repo: &RepoRef) -> Result<(), WorkspaceError> {
        let token = self.creds.token_for(repo).await?;
        let (owner, name) = slug(repo)?;
        self.verify_repository(&token, &owner, &name).await?;
        let resp = self
            .exact_request(Method::DELETE, &format!("/repos/{owner}/{name}"), &token)
            .send()
            .await
            .map_err(|e| transport(e, &token))?;
        check(resp, &token).await.map(|_| ())
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

/// What `GET /repos/{owner}/{name}/pulls/{number}` says that a merge needs.
#[derive(Deserialize)]
struct ApiPullDetail {
    html_url: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    draft: bool,
    /// `clean`, `dirty`, `blocked`, `behind`, `unstable`...; `unknown` while GitHub computes it.
    mergeable_state: Option<String>,
    merge_commit_sha: Option<String>,
    head: Option<ApiHeadDetail>,
    base: Option<ApiHead>,
}

/// The head of a pull request: its branch, the commit the host shows, and the repository it is in.
#[derive(Deserialize)]
struct ApiHeadDetail {
    #[serde(rename = "ref")]
    branch: String,
    sha: Option<String>,
    repo: Option<ApiRepoName>,
}

/// A repository, by its `owner/name`.
#[derive(Deserialize)]
struct ApiRepoName {
    full_name: String,
}

/// `GET /repos/{owner}/{name}/commits/{sha}/status`.
#[derive(Deserialize)]
struct ApiCombinedStatus {
    #[serde(default)]
    state: String,
    #[serde(default)]
    total_count: u64,
}

/// `GET /repos/{owner}/{name}/commits/{sha}/check-runs`.
#[derive(Deserialize)]
struct ApiCheckRuns {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    check_runs: Vec<ApiCheckRun>,
}

#[derive(Deserialize)]
struct ApiCheckRun {
    name: String,
    #[serde(default)]
    status: String,
    conclusion: Option<String>,
}

/// The checks of a commit as [`Checks`]: a status that failed or errored, or a check run that
/// finished with anything but success, neutral or skipped, is failing; one that has not finished is
/// pending; none reported, with the pull request not `clean`, is unreported; anything else passes.
fn classify_checks(
    mergeable_state: Option<&str>,
    status: &ApiCombinedStatus,
    runs: &[ApiCheckRun],
    truncated: bool,
) -> Checks {
    let mut failing = Vec::new();
    let mut pending = Vec::new();
    match status.state.as_str() {
        "failure" | "error" => failing.push(format!("commit status: {}", status.state)),
        "pending" if status.total_count > 0 => pending.push("commit status: pending".to_owned()),
        _ => {}
    }
    for run in runs {
        if run.status != "completed" {
            pending.push(format!("{} ({})", run.name, run.status));
            continue;
        }
        match run.conclusion.as_deref() {
            Some("success" | "neutral" | "skipped") => {}
            other => failing.push(format!(
                "{}: {}",
                run.name,
                other.unwrap_or("no conclusion")
            )),
        }
    }
    if truncated {
        pending.push(format!(
            "more than {} check runs, the rest are not read",
            CHECK_RUNS_PER_PAGE * MAX_CHECK_RUN_PAGES
        ));
    }
    if !failing.is_empty() {
        Checks::Failing(failing.join("; "))
    } else if !pending.is_empty() {
        Checks::Pending(pending.join("; "))
    } else if status.total_count == 0 && runs.is_empty() && mergeable_state != Some("clean") {
        Checks::Unreported(format!(
            "the host reports no status and no check run for this commit, and the pull request is \
             {}",
            mergeable_state.unwrap_or("not clean")
        ))
    } else {
        Checks::Passing
    }
}

/// What a merge answers.
#[derive(Deserialize)]
struct ApiMerge {
    sha: Option<String>,
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

/// [`check`] for a merge: GitHub answers `405` ("Pull Request is not mergeable": a protected branch,
/// failing required checks, a conflict, a draft) and `409` ("Head branch was modified") when its own
/// rules refuse, which is [`WorkspaceError::Conflict`] with its words, not an unexpected status.
async fn check_merge(resp: Response, token: &SecretString) -> WorkspaceResult<Response> {
    let status = resp.status();
    if status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::CONFLICT {
        let body = resp.text().await.unwrap_or_default();
        return Err(WorkspaceError::Conflict(scrub(&api_message(&body), token)));
    }
    check(resp, token).await
}

/// `branch` as the path of a ref: every segment percent-encoded, the slashes kept.
fn encode_ref(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    for byte in branch.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
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

    fn run(name: &str, status: &str, conclusion: Option<&str>) -> ApiCheckRun {
        ApiCheckRun {
            name: name.to_owned(),
            status: status.to_owned(),
            conclusion: conclusion.map(str::to_owned),
        }
    }

    fn combined(state: &str, total_count: u64) -> ApiCombinedStatus {
        ApiCombinedStatus {
            state: state.to_owned(),
            total_count,
        }
    }

    #[test]
    fn checks_are_passing_only_when_everything_reported_finished_well() {
        let ok = [
            run("build", "completed", Some("success")),
            run("lint", "completed", Some("skipped")),
            run("docs", "completed", Some("neutral")),
        ];
        assert_eq!(
            classify_checks(Some("blocked"), &combined("success", 1), &ok, false),
            Checks::Passing
        );
        // A run with no commit status at all (GitHub says `pending`, count 0) is fine.
        assert_eq!(
            classify_checks(None, &combined("pending", 0), &ok, false),
            Checks::Passing
        );
        let red = [
            run("build", "completed", Some("failure")),
            ok[0].clone_run(),
        ];
        assert!(matches!(
            classify_checks(None, &combined("success", 1), &red, false),
            Checks::Failing(m) if m == "build: failure"
        ));
        for (conclusion, failing) in [
            ("cancelled", true),
            ("timed_out", true),
            ("action_required", true),
            ("stale", true),
        ] {
            let one = [run("x", "completed", Some(conclusion))];
            assert_eq!(
                matches!(
                    classify_checks(None, &combined("success", 1), &one, false),
                    Checks::Failing(_)
                ),
                failing,
                "{conclusion}"
            );
        }
        assert!(matches!(
            classify_checks(None, &combined("failure", 2), &ok, false),
            Checks::Failing(m) if m == "commit status: failure"
        ));
        let running = [run("build", "in_progress", None)];
        assert!(matches!(
            classify_checks(None, &combined("success", 1), &running, false),
            Checks::Pending(m) if m == "build (in_progress)"
        ));
        assert!(matches!(
            classify_checks(None, &combined("pending", 2), &ok, false),
            Checks::Pending(_)
        ));
        // Failing wins over pending, and a cut-off list is never passing.
        let mixed = [
            run("a", "queued", None),
            run("b", "completed", Some("failure")),
        ];
        assert!(matches!(
            classify_checks(None, &combined("success", 1), &mixed, false),
            Checks::Failing(_)
        ));
        assert!(matches!(
            classify_checks(None, &combined("success", 1), &ok, true),
            Checks::Pending(_)
        ));
        assert!(matches!(
            classify_checks(Some("unstable"), &combined("pending", 0), &[], false),
            Checks::Unreported(m) if m.contains("unstable")
        ));
        // A pull request the host calls clean with nothing reported passes (the caller is told that
        // nothing was reported), and one with a failure does not, whatever the host calls it.
        assert_eq!(
            classify_checks(Some("clean"), &combined("pending", 0), &[], false),
            Checks::Passing
        );
        let red = [run("build", "completed", Some("failure"))];
        assert!(matches!(
            classify_checks(Some("clean"), &combined("success", 1), &red, false),
            Checks::Failing(_)
        ));
    }

    impl ApiCheckRun {
        fn clone_run(&self) -> Self {
            run(&self.name, &self.status, self.conclusion.as_deref())
        }
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
