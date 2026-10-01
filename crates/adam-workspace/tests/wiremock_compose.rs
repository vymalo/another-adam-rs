//! `GitHub` against the `mock-github` WireMock of `compose.yaml`.
//!
//! Runs only when `ADAM_TEST_MOCK_GITHUB_URL` is set (the mock's root, for
//! example `http://127.0.0.1:8082`); otherwise it passes without doing
//! anything. Start the mock with `docker compose up -d --wait mock-github`;
//! the scenario switches used here are documented in the README ("Local
//! development").
//!
//! One test, run in order: the `already-exists` scenario is a state machine
//! inside the mock, so parallel tests would see each other's state.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping
#![cfg(feature = "github")]

use std::sync::Arc;

use adam_workspace::testing::TestAppKey;
use adam_workspace::{
    AppKey, CodeHost, GitCredentials, GitHub, GitHubApp, HostScoped, NewPullRequest, PullRequest,
    RepoRef, StaticToken, WorkspaceError,
};
use secrecy::ExposeSecret as _;

fn repo() -> RepoRef {
    // The same shape the coder gets from the `git-server` service:
    // `http://<host>/<owner>/<repo>.git`.
    RepoRef::new("http://git-server:8080/local/sandbox.git", "main")
}

fn new_pr(head: &str, title: &str) -> NewPullRequest {
    NewPullRequest {
        repo: repo(),
        head: head.to_owned(),
        title: title.to_owned(),
        body: "Automated change.\n\n## Verification\n\n`just check` passed, \"quoted\".".to_owned(),
        draft: false,
    }
}

fn client(root: &str, token: &str) -> GitHub {
    GitHub::new(Arc::new(StaticToken::new(token)))
        .expect("client")
        .with_api_base(root)
}

#[tokio::test]
async fn pull_requests_and_scenarios_against_the_mock() {
    let Some(root) = std::env::var("ADAM_TEST_MOCK_GITHUB_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    else {
        eprintln!("skipping: ADAM_TEST_MOCK_GITHUB_URL not set");
        return;
    };
    let root = root.trim_end_matches('/').to_owned();
    let github = client(&root, "dev-github-token");

    // Nothing is open by default.
    let none = github
        .find_pull_request(&repo(), "agent/one")
        .await
        .expect("find");
    assert_eq!(none, None);

    // Opening one: probe (empty), then create; number, URL and branch come
    // from the mock's templated response.
    let pr = github
        .open_pull_request(new_pr("agent/one", "Add hello.txt"))
        .await
        .expect("open");
    assert!(pr.number >= 100, "{pr:?}");
    assert!(
        pr.url
            .ends_with(&format!("/local/sandbox/pull/{}", pr.number)),
        "{pr:?}"
    );
    assert_eq!(pr.head, "agent/one");

    // `owner:branch` heads (forks) come back as the bare branch.
    let fork = github
        .open_pull_request(new_pr("someone:agent/two", "From a fork"))
        .await
        .expect("open from a fork");
    assert_eq!(fork.head, "agent/two");

    // A head containing `already-open`: the probe finds a pull request, so
    // opening is idempotent and nothing is created.
    let existing = github
        .open_pull_request(new_pr("agent/already-open", "Ignored"))
        .await
        .expect("open idempotently");
    assert_eq!(existing.number, 7);
    assert_eq!(existing.head, "agent/already-open");
    let found: Option<PullRequest> = github
        .find_pull_request(&repo(), "agent/already-open")
        .await
        .expect("find");
    assert_eq!(found, Some(existing));

    // Lost race: probe empty, create answers 422 "already exists", the next
    // probe finds the pull request. The mock then resets by itself.
    let raced = github
        .open_pull_request(new_pr("agent/raced", "Racing [mock:already-exists]"))
        .await
        .expect("open after a lost race");
    assert_eq!(raced.number, 42);
    assert_eq!(raced.head, "agent/raced");
    assert_eq!(
        github
            .find_pull_request(&repo(), "agent/after-the-race")
            .await
            .expect("find"),
        None,
        "the scenario must reset after the second probe"
    );

    // A comment on a pull request (what an update with accepted red checks leaves): posted with a
    // good token, refused with a bad one.
    github
        .comment_on_pull_request(&repo(), 7, "the update was not verified")
        .await
        .expect("comment");
    let err = client(&root, "bad-token")
        .comment_on_pull_request(&repo(), 7, "x")
        .await
        .expect_err("401");
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");

    // Bad credentials, by token and by keyword.
    let err = client(&root, "bad-token")
        .open_pull_request(new_pr("agent/three", "Bad token"))
        .await
        .expect_err("401");
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
    let err = github
        .open_pull_request(new_pr("agent/three", "[mock:unauthorized]"))
        .await
        .expect_err("401");
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");

    // Retryable failures: a rate limit is its own class, a 5xx is transient.
    let err = github
        .open_pull_request(new_pr("agent/four", "[mock:rate-limit]"))
        .await
        .expect_err("rate-limit");
    assert!(
        matches!(err, WorkspaceError::RateLimited { .. }),
        "rate-limit: {err:?}"
    );
    let err = github
        .open_pull_request(new_pr("agent/four", "[mock:server-error]"))
        .await
        .expect_err("server-error");
    assert!(
        matches!(err, WorkspaceError::Transient { .. }),
        "server-error: {err:?}"
    );

    // A GitHub App: a JWT signed with a key made just now is traded at the mock for the installation
    // token the mock gives (`ghs_mockinstallationtoken...`, good for an hour), which then opens a
    // pull request like any token. A JWT-less request is refused by the mock, so this also proves the
    // coder's request is shaped as GitHub's is (a three-part `Bearer eyJ...`). Last, because the
    // scenarios above are a state machine inside the mock.
    let key = TestAppKey::generate();
    let app = GitHubApp::new(
        &root,
        "12345",
        67890,
        AppKey::from_pem(&key.pkcs1_pem).expect("the key is read"),
    )
    .expect("the App");
    let scoped = HostScoped::new(["git-server"], app);
    let token = scoped.token_for(&repo()).await.expect("a token is minted");
    assert!(
        token
            .expose_secret()
            .starts_with("ghs_mockinstallationtoken"),
        "the mock's installation token"
    );
    let with_app = GitHub::new(Arc::new(scoped))
        .expect("client")
        .with_api_base(&root);
    let pr = with_app
        .open_pull_request(new_pr("agent/app", "Opened by a GitHub App"))
        .await
        .expect("open with an installation token");
    assert_eq!(pr.head, "agent/app");
}

/// Creating a repository against the mock: the owners of the dev stack (`local`, `scratch`) are
/// organisations and anyone else a person, `GET /user` is a person's token's and not an installation
/// token's, an organisation repository is created empty with the clone URL git-server serves it at,
/// and the `already-exists` keyword is a 422 the client calls invalid.
#[tokio::test]
async fn repositories_are_created_by_the_mock_like_github_does() {
    use adam_workspace::{NewRepository, OwnerKind};
    let Some(root) = std::env::var("ADAM_TEST_MOCK_GITHUB_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    else {
        eprintln!("skipping: ADAM_TEST_MOCK_GITHUB_URL not set");
        return;
    };
    let root = root.trim_end_matches('/').to_owned();
    let github = client(&root, "dev-github-token");
    let at = |owner: &str, name: &str| {
        RepoRef::new(format!("http://git-server:8080/{owner}/{name}"), "main")
    };

    assert_eq!(
        github
            .owner_kind("scratch", &at("scratch", "x"))
            .await
            .unwrap(),
        OwnerKind::Organization
    );
    assert_eq!(
        github
            .owner_kind("somebody", &at("scratch", "x"))
            .await
            .unwrap(),
        OwnerKind::User
    );
    assert_eq!(
        github
            .authenticated_login(&at("scratch", "x"))
            .await
            .unwrap()
            .as_deref(),
        Some("dev-user")
    );
    // An installation token is not a user.
    let installation = client(&root, "ghs_mockinstallationtoken000000000000000000");
    assert_eq!(
        installation
            .authenticated_login(&at("scratch", "x"))
            .await
            .unwrap(),
        None
    );

    let unique = format!("probe-{}", std::process::id());
    let created = github
        .create_repository(NewRepository {
            repo: at("scratch", &unique),
            private: true,
            description: Some("made by a test".to_owned()),
            kind: OwnerKind::Organization,
        })
        .await
        .unwrap();
    assert_eq!(created.full_name, format!("scratch/{unique}"));
    assert_eq!(
        created.clone_url,
        format!("http://git-server:8080/scratch/{unique}.git")
    );
    assert_eq!(created.default_branch, "main");

    let err = github
        .create_repository(NewRepository {
            repo: at("scratch", "taken"),
            private: true,
            description: Some("[mock:already-exists]".to_owned()),
            kind: OwnerKind::Organization,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::Invalid(m) if m.contains("already exists")),
        "{err:?}"
    );
}
