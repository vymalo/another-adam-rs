//! Git credentials: where the token for a repository comes from.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use secrecy::SecretString;

use crate::error::{WorkspaceError, WorkspaceResult};
use crate::repo::RepoRef;

/// Source of a short-lived token for a repository.
///
/// The token is used two ways, always per invocation and never persisted:
/// as an `Authorization` header for `git` (via `GIT_CONFIG_*` environment
/// variables) and as a bearer token for a [`CodeHost`](crate::CodeHost).
#[async_trait]
pub trait GitCredentials: Send + Sync + 'static {
    /// Short-lived token for this repo (static in the MVP; a broker later).
    async fn token_for(&self, repo: &RepoRef) -> Result<SecretString, WorkspaceError>;
}

/// Shared handle to a [`GitCredentials`] implementation.
pub type DynGitCredentials = Arc<dyn GitCredentials>;

/// One token for every repository, e.g. from `GITHUB_TOKEN`.
///
/// **It hands the token out for any host.** Whoever chooses the repository
/// URL therefore chooses where the token is sent; use it only when the URL is
/// trusted, or restrict the hosts with [`Workspaces::allow_hosts`]
/// (crate::Workspaces::allow_hosts). For a token that belongs to one host use
/// [`ScopedToken`], which refuses every other.
///
/// Also serves as the test double for [`GitCredentials`].
#[derive(Clone)]
pub struct StaticToken(SecretString);

impl StaticToken {
    /// Wrap a token.
    pub fn new(token: impl Into<SecretString>) -> Self {
        Self(token.into())
    }

    /// Read the token from an environment variable.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Auth`] if the variable is unset, not unicode, or empty.
    pub fn from_env(var: &str) -> WorkspaceResult<Self> {
        match std::env::var(var) {
            Ok(v) if !v.trim().is_empty() => Ok(Self::new(v.trim().to_owned())),
            _ => Err(WorkspaceError::Auth(format!(
                "environment variable {var} is unset or empty"
            ))),
        }
    }
}

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(<redacted>)")
    }
}

#[async_trait]
impl GitCredentials for StaticToken {
    async fn token_for(&self, _repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
        Ok(self.0.clone())
    }
}

/// A token that is only valid for certain hosts.
///
/// [`token_for`](GitCredentials::token_for) refuses (with
/// [`WorkspaceError::Invalid`]) every repository whose host is not one of
/// the scoped hosts, and every filesystem remote, so a token for
/// `github.com` cannot be handed to a URL an untrusted party chose, even if a
/// caller forgets to check the host itself. A host entry is a name (any port)
/// or `name:port`, compared case-insensitively.
#[derive(Clone)]
pub struct ScopedToken {
    hosts: Vec<String>,
    token: SecretString,
}

impl ScopedToken {
    /// `token`, valid for `host` only.
    pub fn new(host: impl Into<String>, token: impl Into<SecretString>) -> Self {
        Self {
            hosts: vec![host.into().trim().to_ascii_lowercase()],
            token: token.into(),
        }
    }

    /// Also valid for `host`.
    #[must_use]
    pub fn and_host(mut self, host: impl Into<String>) -> Self {
        self.hosts.push(host.into().trim().to_ascii_lowercase());
        self
    }

    /// Read the token from an environment variable, valid for `host` only.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Auth`] if the variable is unset, not unicode, or empty.
    pub fn from_env(host: impl Into<String>, var: &str) -> WorkspaceResult<Self> {
        let StaticToken(token) = StaticToken::from_env(var)?;
        Ok(Self::new(host, token))
    }

    /// The hosts this token is valid for.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }
}

impl fmt::Debug for ScopedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedToken")
            .field("hosts", &self.hosts)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[async_trait]
impl GitCredentials for ScopedToken {
    async fn token_for(&self, repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
        let loc = repo.locate()?;
        if loc.is_local() {
            return Err(WorkspaceError::Invalid(
                "credentials are only issued for http(s) hosts, not local repositories".to_owned(),
            ));
        }
        if self.hosts.iter().any(|h| loc.matches_host(h)) {
            Ok(self.token.clone())
        } else {
            Err(WorkspaceError::Invalid(format!(
                "no credentials for the host of {}; this token is only valid for: {}",
                repo.url,
                self.hosts.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    async fn token(scoped: &ScopedToken, url: &str) -> Result<String, WorkspaceError> {
        scoped
            .token_for(&RepoRef::new(url, "main"))
            .await
            .map(|t| t.expose_secret().to_owned())
    }

    #[tokio::test]
    async fn a_scoped_token_is_only_issued_for_its_hosts() {
        let scoped = ScopedToken::new("GitHub.com", "s3cret").and_host("ghe.example.com:8443");
        assert_eq!(
            token(&scoped, "https://github.com/o/r.git").await.unwrap(),
            "s3cret"
        );
        assert_eq!(
            token(&scoped, "https://ghe.example.com:8443/o/r")
                .await
                .unwrap(),
            "s3cret"
        );
        for url in [
            "https://evil.example/o/r.git",
            "https://github.com.evil.example/o/r.git",
            "https://ghe.example.com/o/r.git",
            "http://127.0.0.1:9/o/r.git",
            "/tmp/a/remote.git",
            "file:///tmp/a/remote.git",
        ] {
            let err = token(&scoped, url).await.unwrap_err();
            assert!(matches!(err, WorkspaceError::Invalid(_)), "{url}: {err:?}");
            assert!(!err.to_string().contains("s3cret"), "{err}");
        }
    }

    #[test]
    fn debug_output_hides_the_token() {
        let scoped = ScopedToken::new("github.com", "s3cret");
        let text = format!("{scoped:?} {:?}", StaticToken::new("s3cret"));
        assert!(!text.contains("s3cret"), "{text}");
        assert!(text.contains("github.com"));
    }
}
