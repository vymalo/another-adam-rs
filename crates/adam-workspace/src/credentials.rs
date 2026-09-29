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
