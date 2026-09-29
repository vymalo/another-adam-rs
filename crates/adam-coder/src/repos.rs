//! Which repositories the coder works on, and with which credentials.

use std::sync::Arc;

use adam_workspace::{DynGitCredentials, ScopedToken, Workspaces};
use secrecy::ExposeSecret as _;

use crate::Config;

/// The workspaces the coder works in: the workspace root, restricted to the
/// configured repository hosts, with the GitHub token scoped to the same
/// hosts (defence in depth: the allowlist refuses a foreign host before any
/// credential is requested, the scoped token refuses it again if a caller
/// ever forgets the check).
pub fn workspaces_for(config: &Config) -> (Workspaces, DynGitCredentials) {
    let mut hosts = config.allowed_repo_hosts.iter();
    let first = hosts.next().map_or("github.com", String::as_str);
    let creds = hosts.fold(
        ScopedToken::new(first, config.github_token.expose_secret().to_owned()),
        |token, host| token.and_host(host.as_str()),
    );
    let creds: DynGitCredentials = Arc::new(creds);
    let workspaces = Workspaces::new(config.workspace_root.clone(), creds.clone())
        .allow_hosts(&config.allowed_repo_hosts)
        .allow_local(config.allow_local_repos);
    (workspaces, creds)
}
