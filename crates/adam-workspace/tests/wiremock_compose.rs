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
#![cfg(feature = "github")]

use std::sync::Arc;

use adam_workspace::{
    CodeHost, GitHub, NewPullRequest, PullRequest, RepoRef, StaticToken, WorkspaceError,
};

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

    // Retryable failures.
    for scenario in ["rate-limit", "server-error"] {
        let err = github
            .open_pull_request(new_pr("agent/four", &format!("[mock:{scenario}]")))
            .await
            .expect_err(scenario);
        assert!(
            matches!(err, WorkspaceError::Transient(_)),
            "{scenario}: {err:?}"
        );
    }
}
