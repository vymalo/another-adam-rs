//! GitHub, read through the official MCP server, with the coder's credentials chosen **per call**.
//!
//! The agent folder's `mcp.json` names `github-mcp-server` in `http` mode (a sidecar at
//! `GITHUB_MCP_URL`, 127.0.0.1:8082 by default) with no credential at all: that server reads the
//! token from each request's `Authorization` header and holds nothing. The deployment binds the
//! server's name and origin to a [`GitHubReadBearer`] ([`McpPolicy::bearer_per_call`](adam::mcp::McpPolicy::bearer_per_call)), which says
//! whose token each call carries, so one process serves every account the coder can act for and
//! no key or token is at rest in the MCP container ([ADR 0017](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0017-a-github-app-works-on-every-account-it-is-installed-on.md),
//! D4).
//!
//! ```mermaid
//! sequenceDiagram
//!     participant M as the model
//!     participant T as McpTool (adam-mcp)
//!     participant B as GitHubReadBearer
//!     participant C as the coder's credentials (host check, redactor, cache)
//!     participant S as github-mcp-server http (sidecar)
//!     M->>T: github__get_file_contents {owner, repo, path}
//!     T->>B: for_call("get_file_contents", arguments)
//!     B->>B: target_of: Repository { owner, repo }
//!     B->>C: token_for(https://host/owner/repo)
//!     alt refused (host, owner list, not installed) or transient
//!         C-->>B: error
//!         B-->>T: error result for the model, nothing is sent (a transient one is retried)
//!     else a token
//!         C-->>B: token
//!         B-->>T: token
//!         T->>S: initialize, tools/call (Authorization: Bearer token), close
//!         S-->>T: result, scrubbed of the token
//!     end
//! ```
//!
//! **Which credentials a call gets** is decided by what the call is about ([`target_of`]): the
//! `owner` and `repo` of its arguments, or for a search the one non-negated `repo:`, `org:` or
//! `user:` qualifier of its `query`. The token comes from
//! [`GitCredentials::token_for`](adam_workspace::GitCredentials::token_for) of a repository on the default host, so the allowed-hosts check,
//! the owner list of a GitHub App and the redactor apply exactly as they do to `git` and the REST
//! API. With a personal access token or a pinned installation one token serves every call, whatever
//! the call is about. For an App that finds the installation of each owner a call must be about one
//! account: a search that names none or several is an error result asking for one, and `get_me`
//! (the authenticated *user*, which an App is not) is an error result.
//!
//! The listing at startup uses [`LISTING_ONLY_BEARER`], a placeholder of the form the server
//! accepts: `tools/list` makes no request to GitHub (*verified 2026-10-03* against
//! `github-mcp-server` v1.12.2 `http` mode by `tests/binary.rs`, which counts the requests a mock
//! GitHub receives).

use adam::mcp::CallBearer;
use adam_llm_agent::ToolError;
use adam_workspace::{DynGitCredentials, RepoRef};
use async_trait::async_trait;
use secrecy::SecretString;
use serde_json::{Map, Value};

use crate::tools::workspace_error;

/// The name the agent folder gives the GitHub server in its `mcp.json`, and the deployment binds
/// the bearer to.
pub const SERVER_NAME: &str = "github";

/// The bearer the server's tools are listed with at startup. Not a credential: it has the form of
/// an installation token (`ghs_`), which is all `github-mcp-server`'s `http` mode checks, and
/// listing the tools makes no request to GitHub with it.
pub const LISTING_ONLY_BEARER: &str = "ghs_adam_listing_only";

/// The repository name a token is asked for when a call is about an account and not a repository:
/// every [`GitCredentials`](adam_workspace::GitCredentials) of the coder looks at the host and, at
/// most, the owner.
const ANY_REPOSITORY: &str = "-";

/// What a call to the GitHub server is about, as far as the credentials go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One repository: `owner` and `repo` are in the arguments, or the query has one `repo:o/r`.
    Repository {
        /// The owner, as written.
        owner: String,
        /// The repository name, as written.
        repo: String,
    },
    /// One account (an organisation or a user) and no one repository: `owner` alone, or the query
    /// has an `org:` or a `user:`, or several repositories of one owner.
    Account(String),
    /// More than one account: the query names several owners.
    Several,
    /// No account at all: a search that scopes none, or `get_me`.
    None,
}

/// What the call `tool` with `arguments` is about.
///
/// * `get_me` takes no account: [`Target::None`].
/// * `owner` and `repo` (strings) are the repository; `owner` alone is the account. They are
///   required by eight of the twelve tools of the coder's allow-list and optional for
///   `search_issues`.
/// * Otherwise the `query` of a search is read for the qualifiers `repo:o/r`, `org:o` and `user:o`
///   (names compared without case, values in quotes accepted, groups in parentheses seen through).
///   A **negated** qualifier (`-org:o`, `NOT org:o`) says what to leave out and is not read. One
///   owner is [`Target::Account`] (or [`Target::Repository`] when a single repository is named);
///   two owners are [`Target::Several`]; none is [`Target::None`].
///
/// Whatever is read here only chooses a token: the server still runs the call as written, and a
/// token reaches no more than its account.
pub fn target_of(tool: &str, arguments: &Map<String, Value>) -> Target {
    if tool == "get_me" {
        return Target::None;
    }
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match (text("owner"), text("repo")) {
        (Some(owner), Some(repo)) => {
            return Target::Repository {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
            };
        }
        (Some(owner), None) => return Target::Account(owner.to_owned()),
        (None, _) => {}
    }
    text("query").map_or(Target::None, target_of_query)
}

/// The accounts a search query is scoped to: see [`target_of`].
fn target_of_query(query: &str) -> Target {
    let mut owners: Vec<String> = Vec::new();
    let mut repos: Vec<(String, String)> = Vec::new();
    let mut accounts = false;
    let mut negate_next = false;
    for word in words(query) {
        if word == "NOT" {
            negate_next = true;
            continue;
        }
        let negated = std::mem::take(&mut negate_next);
        let word = word.trim_start_matches('(').trim_end_matches(')');
        let Some((key, value)) = word.split_once(':') else {
            continue;
        };
        if negated || key.starts_with('-') {
            continue;
        }
        let value = value.trim_matches('"');
        match key.to_ascii_lowercase().as_str() {
            "repo" => {
                let Some((owner, name)) = value.split_once('/') else {
                    continue;
                };
                if is_name(owner) && is_name(name) {
                    owners.push(owner.to_owned());
                    repos.push((owner.to_ascii_lowercase(), name.to_ascii_lowercase()));
                }
            }
            "org" | "user" if is_name(value) => {
                owners.push(value.to_owned());
                accounts = true;
            }
            _ => {}
        }
    }
    let mut distinct: Vec<String> = owners.iter().map(|o| o.to_ascii_lowercase()).collect();
    distinct.sort();
    distinct.dedup();
    match distinct.len() {
        0 => Target::None,
        1 => {
            repos.sort();
            repos.dedup();
            match repos.as_slice() {
                [_] if !accounts => {
                    // `repos` holds lowercase names; the first spelling is the one given back.
                    let first = owners.first().cloned().unwrap_or_default();
                    let (_, name) = &repos[0];
                    Target::Repository {
                        owner: first,
                        repo: name.clone(),
                    }
                }
                _ => Target::Account(owners.first().cloned().unwrap_or_default()),
            }
        }
        _ => Target::Several,
    }
}

/// The words of a query: split at white space outside double quotes. A quoted phrase stays in one
/// word, so a qualifier written inside a phrase is not read as one.
fn words(query: &str) -> Vec<&str> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quoted = false;
    for (i, c) in query.char_indices() {
        match c {
            '"' => {
                quoted = !quoted;
                start.get_or_insert(i);
            }
            c if c.is_whitespace() && !quoted => {
                if let Some(s) = start.take() {
                    words.push(&query[s..i]);
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(s) = start {
        words.push(&query[s..]);
    }
    words
}

/// Whether `part` can be an owner or a repository name in a URL: the characters of a repository
/// URL's components (`A-Z a-z 0-9 . - _`), and not `.` or `..`.
fn is_name(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The coder's own credentials as the bearer of the GitHub MCP server's calls (`CallBearer`, from
/// `adam-mcp`).
///
/// `creds` are the coder's: host-scoped, redacting, and for an App the minting cache. `host` is
/// the repository host a call is about (the first of `ALLOWED_REPO_HOSTS`). `needs_owner` is true
/// when the credentials find the installation by owner (a GitHub App without a pin): then a call
/// has to name its account, and `get_me` is refused.
pub struct GitHubReadBearer {
    creds: DynGitCredentials,
    host: String,
    needs_owner: bool,
}

impl GitHubReadBearer {
    /// A bearer over `creds` for repositories on `host`.
    pub fn new(creds: DynGitCredentials, host: impl Into<String>, needs_owner: bool) -> Self {
        Self {
            creds,
            host: host.into(),
            needs_owner,
        }
    }

    /// The repository a token is asked for: `owner`/`repo` on the host, or the placeholder that
    /// stands for "any" when the call is not about one. `None` for a name that cannot be a URL
    /// component (the model's argument is untrusted: nothing of it is put into the URL unchecked).
    fn repository(&self, owner: &str, repo: &str) -> Option<RepoRef> {
        (is_name(owner) && is_name(repo))
            .then(|| RepoRef::new(format!("https://{}/{owner}/{repo}", self.host), "main"))
    }

    fn any_repository(&self) -> RepoRef {
        RepoRef::new(
            format!("https://{}/{ANY_REPOSITORY}/{ANY_REPOSITORY}", self.host),
            "main",
        )
    }
}

impl std::fmt::Debug for GitHubReadBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubReadBearer")
            .field("host", &self.host)
            .field("needs_owner", &self.needs_owner)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl CallBearer for GitHubReadBearer {
    async fn for_listing(&self) -> Result<SecretString, ToolError> {
        Ok(SecretString::from(LISTING_ONLY_BEARER))
    }

    async fn for_call(
        &self,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> Result<SecretString, ToolError> {
        let target = target_of(tool, arguments);
        let repository = if self.needs_owner {
            if tool == "get_me" {
                return Err(ToolError::Permanent(
                    "get_me is not available: the coder acts as a GitHub App, which is not a user. \
                     Read what you need with the other tools, naming the owner and repository"
                        .to_owned(),
                ));
            }
            match &target {
                Target::Repository { owner, repo } => self.repository(owner, repo),
                Target::Account(owner) => self.repository(owner, ANY_REPOSITORY),
                Target::Several | Target::None => None,
            }
            .ok_or_else(|| ask_for_one_account(&target))?
        } else {
            // One token serves every call: the repository only decides what the credentials look
            // at (the host, and their own redaction), and a name that is no URL component is not
            // worth an error here, the server answers it.
            match &target {
                Target::Repository { owner, repo } => self.repository(owner, repo),
                Target::Account(owner) => self.repository(owner, ANY_REPOSITORY),
                Target::Several | Target::None => None,
            }
            .unwrap_or_else(|| self.any_repository())
        };
        self.creds
            .token_for(&repository)
            .await
            .map_err(|e| workspace_error(&e))
    }
}

/// What to tell the model when an App cannot choose an installation for a call.
fn ask_for_one_account(target: &Target) -> ToolError {
    let what = match target {
        Target::Several => "names more than one account",
        Target::Repository { .. } | Target::Account(_) => {
            "names an owner that is not a GitHub login"
        }
        Target::None => "names no account",
    };
    ToolError::Permanent(format!(
        "this call {what}. The coder acts as a GitHub App installed on several accounts and uses \
         the installation of one account per call: give `owner` (and `repo`), or put exactly one \
         `org:`, `user:` or `repo:` qualifier in the `query`, and search one account at a time"
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use adam_workspace::{GitCredentials, HostScoped, ScopedToken, StaticToken, WorkspaceError};
    use secrecy::ExposeSecret as _;
    use serde_json::json;

    use super::*;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn repo(owner: &str, name: &str) -> Target {
        Target::Repository {
            owner: owner.to_owned(),
            repo: name.to_owned(),
        }
    }

    fn account(owner: &str) -> Target {
        Target::Account(owner.to_owned())
    }

    /// Credentials that remember what they were asked for and answer with a token that names it.
    #[derive(Default)]
    struct Recording {
        asked: Mutex<Vec<String>>,
        fail: Mutex<Option<WorkspaceError>>,
    }

    #[async_trait]
    impl GitCredentials for Recording {
        async fn token_for(&self, repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
            self.asked.lock().unwrap().push(repo.url.clone());
            match self.fail.lock().unwrap().take() {
                Some(e) => Err(e),
                None => Ok(SecretString::from(format!("ghs_token_for_{}", repo.url))),
            }
        }
    }

    fn bearer(creds: &Arc<Recording>, needs_owner: bool) -> GitHubReadBearer {
        GitHubReadBearer::new(creds.clone(), "github.com", needs_owner)
    }

    async fn token(bearer: &GitHubReadBearer, tool: &str, arguments: Value) -> String {
        bearer
            .for_call(tool, &args(arguments))
            .await
            .unwrap()
            .expose_secret()
            .to_owned()
    }

    /// `owner` and `repo` of the arguments choose the repository the credentials are asked for, on
    /// the configured host, for every tool that takes them; a name that is not a URL component
    /// never reaches a URL.
    #[tokio::test]
    async fn the_owner_and_repository_of_a_call_choose_the_credentials() {
        let creds = Arc::new(Recording::default());
        let bearer = bearer(&creds, false);
        for tool in [
            "get_file_contents",
            "list_branches",
            "list_commits",
            "get_commit",
            "list_issues",
            "issue_read",
            "list_pull_requests",
            "pull_request_read",
            "search_issues",
        ] {
            let token = token(&bearer, tool, json!({"owner": "acme", "repo": "widgets"})).await;
            assert_eq!(
                token, "ghs_token_for_https://github.com/acme/widgets",
                "{tool}"
            );
        }
        // An owner alone is the account (search_issues takes it).
        let token_of = token(
            &bearer,
            "search_issues",
            json!({"owner": "acme", "query": "bug"}),
        )
        .await;
        assert_eq!(token_of, "ghs_token_for_https://github.com/acme/-");
        // The arguments are the model's: nothing outside [A-Za-z0-9._-] is put into a URL, and the
        // host is never the model's to choose.
        creds.asked.lock().unwrap().clear();
        for (owner, name) in [
            ("evil.example/x", "r"),
            ("a", "b?c=d"),
            ("..", "r"),
            ("a@evil.example", "r"),
            ("a b", "r"),
        ] {
            token(
                &bearer,
                "get_file_contents",
                json!({"owner": owner, "repo": name}),
            )
            .await;
        }
        let asked = creds.asked.lock().unwrap().clone();
        assert_eq!(
            asked, ["https://github.com/-/-"; 5],
            "an unusable name falls back to the placeholder, on the configured host"
        );
    }

    /// A search names its account with `repo:`, `org:` or `user:`; negated qualifiers are not read,
    /// two owners are two accounts, a quoted phrase holds no qualifier.
    #[test]
    fn a_search_names_its_account_with_org_user_or_repo() {
        let target = |query: &str| target_of("search_code", &args(json!({"query": query})));
        assert_eq!(target("fn main repo:acme/widgets"), repo("acme", "widgets"));
        assert_eq!(
            target("REPO:acme/widgets language:rust"),
            repo("acme", "widgets")
        );
        assert_eq!(
            target("fn main repo:\"acme/widgets\""),
            repo("acme", "widgets")
        );
        assert_eq!(target("is:open org:acme"), account("acme"));
        assert_eq!(target("user:octocat stars:>10"), account("octocat"));
        assert_eq!(target("org:Acme (is:open)"), account("Acme"));
        // The same owner twice, or two of its repositories, is still one account.
        assert_eq!(target("org:acme user:ACME"), account("acme"));
        assert_eq!(target("repo:acme/a repo:acme/b"), account("acme"));
        assert_eq!(target("repo:acme/a repo:acme/A"), repo("acme", "a"));
        assert_eq!(target("org:acme repo:acme/a"), account("acme"));
        // Two owners, however they are written.
        assert_eq!(target("org:acme org:other"), Target::Several);
        assert_eq!(target("(org:acme OR user:octocat) bug"), Target::Several);
        assert_eq!(target("repo:acme/a repo:other/b"), Target::Several);
        // Nothing to choose by.
        assert_eq!(target("fn main language:rust"), Target::None);
        assert_eq!(target(""), Target::None);
        // What to leave out is not what to search in.
        assert_eq!(target("bug -org:other"), Target::None);
        assert_eq!(target("bug NOT org:other org:acme"), account("acme"));
        assert_eq!(target("bug -repo:other/x repo:acme/y"), repo("acme", "y"));
        // A phrase is text, not a qualifier; a value that is no name is ignored.
        assert_eq!(target("\"org:acme is great\""), Target::None);
        assert_eq!(target("org: repo:acme"), Target::None);
        assert_eq!(target("org:a/b"), Target::None);
        // `owner` and `repo` of the arguments win over the query; a query is read only without them.
        assert_eq!(
            target_of(
                "search_issues",
                &args(json!({"owner": "acme", "repo": "w", "query": "org:other"}))
            ),
            repo("acme", "w")
        );
        assert_eq!(
            target_of(
                "search_issues",
                &args(json!({"repo": "w", "query": "org:other"}))
            ),
            account("other")
        );
        assert_eq!(
            target_of("search_issues", &args(json!({"query": 5, "owner": 7}))),
            Target::None
        );
        // `get_me` is about nobody.
        assert_eq!(
            target_of("get_me", &args(json!({"owner": "acme"}))),
            Target::None
        );
    }

    /// A personal access token and a pinned installation serve every call: whatever it is about,
    /// even nothing, the one token (through the credentials, so the host is checked and the
    /// redactor learns it), and `get_me` works.
    #[tokio::test]
    async fn token_and_pinned_modes_use_the_one_token() {
        // A token, scoped to its host, as the coder builds it.
        let one: DynGitCredentials = Arc::new(ScopedToken::new("github.com", "ghp_the_one_token"));
        let token_mode = GitHubReadBearer::new(one, "github.com", false);
        // A pinned App is the same shape: HostScoped over credentials that ignore the repository.
        let pinned: DynGitCredentials = Arc::new(HostScoped::new(
            ["github.com"],
            StaticToken::new("ghs_the_pinned_one"),
        ));
        let pinned_mode = GitHubReadBearer::new(pinned, "github.com", false);
        for (bearer, want) in [
            (&token_mode, "ghp_the_one_token"),
            (&pinned_mode, "ghs_the_pinned_one"),
        ] {
            for (tool, arguments) in [
                (
                    "get_file_contents",
                    json!({"owner": "acme", "repo": "widgets"}),
                ),
                (
                    "get_file_contents",
                    json!({"owner": "other", "repo": "thing"}),
                ),
                ("search_issues", json!({"owner": "acme", "query": "bug"})),
                ("search_code", json!({"query": "org:acme org:other"})),
                ("search_repositories", json!({"query": "rust"})),
                ("search_code", json!({})),
                ("get_me", json!({})),
            ] {
                assert_eq!(
                    token(bearer, tool, arguments.clone()).await,
                    want,
                    "{tool} {arguments}"
                );
            }
        }
        // The host check still applies to what the model chose: it chooses no host, so even the
        // listing placeholder is the only token that never touches the credentials.
        let listing = token_mode.for_listing().await.unwrap();
        assert_eq!(listing.expose_secret(), LISTING_ONLY_BEARER);
        assert!(LISTING_ONLY_BEARER.starts_with("ghs_"));
        // A bearer for another host than its credentials are valid for refuses, nothing is sent.
        let wrong_host: DynGitCredentials =
            Arc::new(ScopedToken::new("github.com", "ghp_the_one_token"));
        let wrong = GitHubReadBearer::new(wrong_host, "ghe.example.com", false);
        let err = wrong.for_call("get_me", &Map::new()).await.unwrap_err();
        assert!(matches!(err, ToolError::Permanent(_)), "{err:?}");
        assert!(!err.to_string().contains("ghp_the_one_token"), "{err}");
    }

    /// What the credentials say reaches the call: a refusal is for the model, a transient failure
    /// is retried, and neither carries a token.
    #[tokio::test]
    async fn a_failure_of_the_credentials_is_typed_for_the_call() {
        let creds = Arc::new(Recording::default());
        let bearer = bearer(&creds, false);
        let call = json!({"owner": "acme", "repo": "widgets"});
        for (failure, permanent) in [
            (
                WorkspaceError::Auth("the App is not installed".to_owned()),
                true,
            ),
            (WorkspaceError::Invalid("not allowed".to_owned()), true),
            (WorkspaceError::RateLimited { retry_after: None }, false),
            (
                WorkspaceError::Transient {
                    message: "no answer".to_owned(),
                    source: None,
                },
                false,
            ),
        ] {
            *creds.fail.lock().unwrap() = Some(failure);
            let err = bearer
                .for_call("get_file_contents", &args(call.clone()))
                .await
                .unwrap_err();
            assert_eq!(matches!(err, ToolError::Permanent(_)), permanent, "{err:?}");
            assert!(!err.to_string().contains("ghs_"), "{err}");
        }
    }

    /// An App that finds the installation of each owner needs a call to name one account: a search
    /// that names none, or several, is an error result that says how to name one, and **nothing is
    /// asked of the credentials**, so no installation is looked up for a guess; one account is
    /// served like any call.
    #[tokio::test]
    async fn a_search_with_no_or_several_accounts_is_refused_without_a_pin() {
        let creds = Arc::new(Recording::default());
        let bearer = bearer(&creds, true);
        for (tool, arguments) in [
            ("search_code", json!({"query": "fn main"})),
            (
                "search_repositories",
                json!({"query": "language:rust stars:>100"}),
            ),
            ("search_issues", json!({"query": "bug -org:other"})),
            ("search_issues", json!({})),
            ("search_code", json!({"query": "org:acme org:other"})),
            (
                "search_code",
                json!({"query": "(user:octocat OR org:acme) fn"}),
            ),
            ("get_file_contents", json!({"path": "README.md"})),
            // An owner that is no login is no account either (the model's text is never put in a URL).
            ("get_file_contents", json!({"owner": "a b", "repo": "r"})),
            ("list_branches", json!({"owner": "../x", "repo": "r"})),
        ] {
            let err = bearer
                .for_call(tool, &args(arguments.clone()))
                .await
                .unwrap_err();
            let ToolError::Permanent(why) = &err else {
                panic!("{tool} {arguments}: {err:?}")
            };
            assert!(
                why.contains("`org:`, `user:` or `repo:`") && why.contains("one account"),
                "{tool} {arguments}: {why}"
            );
        }
        assert!(
            creds.asked.lock().unwrap().is_empty(),
            "the credentials were not asked: {:?}",
            creds.asked.lock().unwrap()
        );
        // One account, however it is named, is served from that account's installation.
        for (tool, arguments, url) in [
            (
                "search_code",
                json!({"query": "fn main org:acme"}),
                "https://github.com/acme/-",
            ),
            (
                "search_issues",
                json!({"query": "bug user:Octocat"}),
                "https://github.com/Octocat/-",
            ),
            (
                "search_code",
                json!({"query": "repo:acme/widgets fn"}),
                "https://github.com/acme/widgets",
            ),
            (
                "search_issues",
                json!({"owner": "acme", "query": "bug"}),
                "https://github.com/acme/-",
            ),
            (
                "list_branches",
                json!({"owner": "acme", "repo": "widgets"}),
                "https://github.com/acme/widgets",
            ),
        ] {
            assert_eq!(
                token(&bearer, tool, arguments.clone()).await,
                format!("ghs_token_for_{url}"),
                "{tool} {arguments}"
            );
        }
        // A refusal of the credentials (not installed, not on the list) is the model's to read.
        *creds.fail.lock().unwrap() = Some(WorkspaceError::Auth(
            "the GitHub App `x` is not installed on `acme`".to_owned(),
        ));
        let err = bearer
            .for_call(
                "list_branches",
                &args(json!({"owner": "acme", "repo": "widgets"})),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Permanent(why) if why.contains("not installed")),
            "{err:?}"
        );
    }

    /// `get_me` is the authenticated user, which an App is not (the server asks `GET /user`, which an
    /// installation token gets a `403` for): refused without a pin, with the reason, and the
    /// credentials are not asked.
    #[tokio::test]
    async fn get_me_is_refused_for_an_app_without_a_pin() {
        let creds = Arc::new(Recording::default());
        let discovering = bearer(&creds, true);
        let err = discovering
            .for_call("get_me", &Map::new())
            .await
            .unwrap_err();
        let ToolError::Permanent(why) = &err else {
            panic!("{err:?}")
        };
        assert!(
            why.contains("not a user") && why.contains("GitHub App"),
            "{why}"
        );
        assert!(creds.asked.lock().unwrap().is_empty());
        // With a token or a pinned installation it is asked for like the rest.
        let one = bearer(&creds, false);
        assert_eq!(
            token(&one, "get_me", json!({})).await,
            "ghs_token_for_https://github.com/-/-"
        );
    }

    #[test]
    fn debug_shows_the_host_and_no_credentials() {
        let creds: DynGitCredentials = Arc::new(StaticToken::new("ghp_secret_token"));
        let text = format!("{:?}", GitHubReadBearer::new(creds, "github.com", false));
        assert!(text.contains("github.com"), "{text}");
        assert!(!text.contains("ghp_secret_token"), "{text}");
    }
}
