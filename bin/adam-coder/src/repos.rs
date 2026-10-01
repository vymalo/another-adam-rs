//! Which repositories the coder works on, and with which credentials.

use std::sync::Arc;

use adam_workspace::{
    DynGitCredentials, GitHubApp, HostScoped, ScopedToken, WorkspaceError, Workspaces,
};
use secrecy::ExposeSecret as _;

use crate::WorkerConfig;
use crate::config::GitHubAuth;
use crate::redact::{RedactingCredentials, Redactor};

/// The workspaces the coder works in: the workspace root, restricted to the
/// configured repository hosts, with the GitHub credentials scoped to the same
/// hosts (defence in depth: the allowlist refuses a foreign host before any
/// credential is requested, the scoped credentials refuse it again if a caller
/// ever forgets the check). The root is [`WorkerConfig::placed_root`]: with the `affinity`
/// placement, the folder named after the worker under `WORKSPACE_ROOT`.
///
/// The credentials are the installation's own: the personal access token of `GITHUB_TOKEN`
/// ([`ScopedToken`]), or the installation tokens of the GitHub App ([`GitHubApp`], minted against
/// `GITHUB_API_URL` and checked for the host first, [`HostScoped`]). Either way they are wrapped so
/// that every token handed out is registered with `redactor` ([`RedactingCredentials`]), and they
/// are returned for the code host to use too: one source of tokens for git and for the REST API.
///
/// # Errors
///
/// [`WorkspaceError::Invalid`] when the HTTP client of a GitHub App cannot be built.
pub fn workspaces_for(
    config: &WorkerConfig,
    redactor: &Redactor,
) -> Result<(Workspaces, DynGitCredentials), WorkspaceError> {
    let scoped: DynGitCredentials = match &config.github {
        GitHubAuth::Token(token) => {
            let mut hosts = config.allowed_repo_hosts.iter();
            let first = hosts.next().map_or("github.com", String::as_str);
            Arc::new(hosts.fold(
                ScopedToken::new(first, token.expose_secret().to_owned()),
                |token, host| token.and_host(host.as_str()),
            ))
        }
        GitHubAuth::App(app) => {
            let minting = GitHubApp::new(
                config.github_api_url.as_str(),
                app.app_id.clone(),
                app.installation_id,
                app.key.clone(),
            )?;
            Arc::new(HostScoped::new(
                config.allowed_repo_hosts.iter().map(String::as_str),
                minting,
            ))
        }
    };
    let creds: DynGitCredentials = Arc::new(RedactingCredentials::new(scoped, redactor.clone()));
    let workspaces = Workspaces::new(config.placed_root(), creds.clone())
        .allow_hosts(&config.allowed_repo_hosts)
        .allow_local(config.allow_local_repos);
    Ok((workspaces, creds))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use adam_workspace::{RepoRef, WorkspaceError};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// [`workspaces_for`], with nothing to redact.
    fn built(config: &WorkerConfig) -> (Workspaces, DynGitCredentials) {
        workspaces_for(config, &Redactor::default()).unwrap()
    }

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

        let (ws, _) = built(&config(&[], &tmp.path().join("a")));
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
        let (ws, _) = built(&config(
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
        let (ws, _) = built(&config(
            &[("ALLOWED_REPO_HOSTS", &host), ("ALLOW_LOCAL_REPOS", "true")],
            &tmp.path().join("c"),
        ));
        let err = ws.prepare(&evil_repo, "run-cfg-0004").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
        assert!(!evil.received_requests().await.unwrap().is_empty());
    }

    /// `affinity` keeps mirrors and worktrees in `WORKSPACE_ROOT/<WORKER_ID>`; `shared` and
    /// `isolated` use `WORKSPACE_ROOT` itself.
    #[tokio::test]
    async fn the_placement_decides_the_folder_the_workspaces_live_in() {
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("nowhere.git");
        let repo = RepoRef::new(local.to_string_lossy(), "main");
        let cases = [
            ("shared", "a", ""),
            ("affinity", "b", "coder-9"),
            ("isolated", "c", ""),
        ];
        for (placement, root, folder) in cases {
            let root = tmp.path().join(root);
            let (ws, _) = built(&config(
                &[
                    ("ALLOW_LOCAL_REPOS", "true"),
                    ("WORKSPACE_PLACEMENT", placement),
                    ("WORKER_ID", "coder-9"),
                ],
                &root,
            ));
            // The remote is missing, but the mirror is created before it is fetched.
            let err = ws.prepare(&repo, "run-place-0001").await.unwrap_err();
            assert!(matches!(err, WorkspaceError::NotFound(_)), "{err:?}");
            assert!(
                root.join(folder).join("git").is_dir(),
                "{placement}: mirrors under {}",
                root.join(folder).display()
            );
            if !folder.is_empty() {
                assert!(
                    !root.join("git").exists(),
                    "{placement}: nothing outside the worker's folder"
                );
            }
        }
    }

    /// A GitHub App: the credentials trade a JWT for an installation token at `GITHUB_API_URL`, only
    /// for a repository on an allowed host (a foreign one is refused before anything is asked of
    /// GitHub), and what they hand out is registered with the redactor.
    #[tokio::test]
    async fn a_github_app_mints_its_token_at_the_api_root_for_the_allowed_hosts_only() {
        use adam_workspace::testing::TestAppKey;
        use secrecy::ExposeSecret as _;
        use wiremock::matchers::{header_regex, method, path};

        let key = TestAppKey::generate();
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("app.pem");
        std::fs::write(&file, &key.pkcs8_pem).unwrap();
        let github = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/app/installations/67890/access_tokens"))
            .and(header_regex(
                "authorization",
                r"^Bearer eyJ[\w-]+\.[\w-]+\.[\w-]+$",
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_appInstallationToken0123456789",
                "expires_at": "2099-01-01T00:00:00Z",
            })))
            .expect(1)
            .mount(&github)
            .await;
        let worker = config(
            &[
                ("GITHUB_TOKEN", ""),
                ("GITHUB_APP_ID", "12345"),
                ("GITHUB_APP_INSTALLATION_ID", "67890"),
                ("GITHUB_APP_PRIVATE_KEY_PATH", &file.to_string_lossy()),
                ("GITHUB_API_URL", &github.uri()),
            ],
            &tmp.path().join("work"),
        );
        assert!(matches!(worker.github, GitHubAuth::App(_)));
        let redactor = Redactor::default();
        let (ws, creds) = workspaces_for(&worker, &redactor).unwrap();

        // A repository on a host that is not allowed: refused, nothing asked of GitHub.
        let evil = RepoRef::new("https://evil.example/o/r.git", "main");
        let err = creds.token_for(&evil).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
        let err = ws.prepare(&evil, "run-app-0001").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
        assert!(github.received_requests().await.unwrap().is_empty());

        // The allowed host: the token, and it is scrubbed from then on (and not before).
        assert_eq!(
            redactor.scrub("ghs_appInstallationToken0123456789"),
            "ghs_appInstallationToken0123456789"
        );
        let token = creds
            .token_for(&RepoRef::new("https://github.com/o/r.git", "main"))
            .await
            .unwrap();
        assert_eq!(token.expose_secret(), "ghs_appInstallationToken0123456789");
        assert_eq!(redactor.scrub(token.expose_secret()), "[redacted]");
        // A second call is the cached token: the mock expects one request only.
        creds
            .token_for(&RepoRef::new("https://github.com/o/r.git", "main"))
            .await
            .unwrap();
    }
}
