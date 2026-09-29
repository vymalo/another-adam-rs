//! Which repositories the coder works on, and with which credentials.

use std::sync::Arc;

use adam_workspace::{DynGitCredentials, ScopedToken, Workspaces};
use secrecy::ExposeSecret as _;

use crate::WorkerConfig;

/// The workspaces the coder works in: the workspace root, restricted to the
/// configured repository hosts, with the GitHub token scoped to the same
/// hosts (defence in depth: the allowlist refuses a foreign host before any
/// credential is requested, the scoped token refuses it again if a caller
/// ever forgets the check).
pub fn workspaces_for(config: &WorkerConfig) -> (Workspaces, DynGitCredentials) {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use adam_workspace::{RepoRef, WorkspaceError};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn config(extra: &[(&str, &str)], root: &std::path::Path) -> WorkerConfig {
        let mut vars = HashMap::from([
            ("DATABASE_URL", "postgres://u:p@db/adam".to_owned()),
            ("MODEL_BASE_URL", "https://gw.example/v1".to_owned()),
            ("MODEL_API_KEY", "sk-key".to_owned()),
            ("MODEL", "m".to_owned()),
            ("GITHUB_TOKEN", "ghp_secret_token".to_owned()),
            ("A2A_BEARER_TOKENS", "t".to_owned()),
            ("PUBLIC_URL", "http://coder:8080/".to_owned()),
            ("WORKSPACE_ROOT", root.to_string_lossy().into_owned()),
        ]);
        vars.extend(extra.iter().map(|(k, v)| (*k, (*v).to_owned())));
        let config = crate::Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
        config.worker.expect("the default role runs workers")
    }

    /// The configuration reaches the workspaces: by default a foreign host and
    /// a local path are refused (before any request), and `ALLOW_LOCAL_REPOS`
    /// and `ALLOWED_REPO_HOSTS` open exactly what they say.
    #[tokio::test]
    async fn the_configuration_decides_which_repositories_are_accepted() {
        let evil = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&evil)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("nowhere.git");
        let evil_repo = RepoRef::new(format!("{}/o/r.git", evil.uri()), "main");
        let local_repo = RepoRef::new(local.to_string_lossy(), "main");

        let (ws, _) = workspaces_for(&config(&[], &tmp.path().join("a")));
        for (i, repo) in [&evil_repo, &local_repo].into_iter().enumerate() {
            let err = ws
                .prepare(repo, &format!("run-cfg-000{i}"))
                .await
                .unwrap_err();
            assert!(
                matches!(err, WorkspaceError::Invalid(_)),
                "{repo:?}: {err:?}"
            );
        }
        assert!(evil.received_requests().await.unwrap().is_empty());

        // ALLOW_LOCAL_REPOS: the local path is now a real (missing) remote.
        let (ws, _) = workspaces_for(&config(
            &[("ALLOW_LOCAL_REPOS", "true")],
            &tmp.path().join("b"),
        ));
        let err = ws.prepare(&local_repo, "run-cfg-0002").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
        let err = ws.prepare(&evil_repo, "run-cfg-0003").await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::Invalid(_)),
            "a local flag does not open other hosts: {err:?}"
        );

        // ALLOWED_REPO_HOSTS names the mock's host (http needs the dev flag).
        let host = format!("127.0.0.1:{}", evil.address().port());
        let (ws, _) = workspaces_for(&config(
            &[("ALLOWED_REPO_HOSTS", &host), ("ALLOW_LOCAL_REPOS", "true")],
            &tmp.path().join("c"),
        ));
        let err = ws.prepare(&evil_repo, "run-cfg-0004").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
        assert!(!evil.received_requests().await.unwrap().is_empty());
    }
}
