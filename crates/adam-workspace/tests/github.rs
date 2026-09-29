//! `GitHub` against a mock server (no network).
#![cfg(feature = "github")]

use std::sync::Arc;

use adam_workspace::{
    CodeHost, GitHub, NewPullRequest, PullRequest, RepoRef, StaticToken, WorkspaceError,
};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "ghp_FAKEtoken0123456789abcdefghijklmnop";

fn repo() -> RepoRef {
    RepoRef::new("https://github.com/octo/widgets.git", "main")
}

fn new_pr() -> NewPullRequest {
    NewPullRequest {
        repo: repo(),
        head: "agent/018f3a2b".to_owned(),
        title: "Fix the widget".to_owned(),
        body: "Automated change.".to_owned(),
        draft: true,
    }
}

fn client(server: &MockServer) -> GitHub {
    GitHub::new(Arc::new(StaticToken::new(TOKEN)))
        .unwrap()
        .with_api_base(server.uri())
}

fn api_pull(number: u64, branch: &str) -> serde_json::Value {
    json!({
        "number": number,
        "html_url": format!("https://github.com/octo/widgets/pull/{number}"),
        "head": {"ref": branch, "label": format!("octo:{branch}")},
        "state": "open",
    })
}

fn list_mock(branch: &str) -> wiremock::MockBuilder {
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/pulls"))
        .and(query_param("head", format!("octo:{branch}")))
        .and(query_param("state", "open"))
}

#[tokio::test]
async fn opens_a_pull_request_with_the_documented_request_shape() {
    let server = MockServer::start().await;
    // The idempotency probe comes first and finds nothing.
    list_mock("agent/018f3a2b")
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/octo/widgets/pulls"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .and(body_json(json!({
            "title": "Fix the widget",
            "body": "Automated change.",
            "head": "agent/018f3a2b",
            "base": "main",
            "draft": true,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(api_pull(7, "agent/018f3a2b")))
        .expect(1)
        .mount(&server)
        .await;

    let pr = client(&server).open_pull_request(new_pr()).await.unwrap();
    assert_eq!(
        pr,
        PullRequest {
            number: 7,
            url: "https://github.com/octo/widgets/pull/7".to_owned(),
            head: "agent/018f3a2b".to_owned(),
        }
    );

    // Every request identified itself.
    for r in server.received_requests().await.unwrap() {
        let ua = r
            .headers
            .get("user-agent")
            .expect("user agent")
            .to_str()
            .unwrap();
        assert!(ua.starts_with("adam-workspace/"), "{ua}");
    }
}

#[tokio::test]
async fn returns_the_existing_open_pull_request_without_creating_another() {
    let server = MockServer::start().await;
    list_mock("agent/018f3a2b")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([api_pull(3, "agent/018f3a2b")])),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;

    let gh = client(&server);
    let first = gh.open_pull_request(new_pr()).await.unwrap();
    let second = gh.open_pull_request(new_pr()).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first.number, 3);
}

#[tokio::test]
async fn a_422_already_exists_is_resolved_by_finding_the_pull_request() {
    let server = MockServer::start().await;
    // First probe: nothing yet. After the racing creator wins: it is there.
    list_mock("agent/018f3a2b")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    list_mock("agent/018f3a2b")
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([api_pull(11, "agent/018f3a2b")])),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed",
            "errors": [{
                "resource": "PullRequest",
                "code": "custom",
                "message": "A pull request already exists for octo:agent/018f3a2b."
            }],
        })))
        .expect(1)
        .mount(&server)
        .await;

    let pr = client(&server).open_pull_request(new_pr()).await.unwrap();
    assert_eq!(pr.number, 11);
}

#[tokio::test]
async fn find_pull_request_matches_the_head_branch_exactly() {
    let server = MockServer::start().await;
    list_mock("agent/x")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([api_pull(1, "agent/xy"), api_pull(2, "agent/x"),])),
        )
        .mount(&server)
        .await;
    list_mock("agent/none")
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let gh = client(&server);
    assert_eq!(
        gh.find_pull_request(&repo(), "agent/x")
            .await
            .unwrap()
            .unwrap()
            .number,
        2
    );
    assert_eq!(
        gh.find_pull_request(&repo(), "agent/none").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn fork_heads_are_passed_through_as_owner_colon_branch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octo/widgets/pulls"))
        .and(query_param("head", "someone:feature"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([api_pull(5, "feature")])))
        .mount(&server)
        .await;
    let found = client(&server)
        .find_pull_request(&repo(), "someone:feature")
        .await
        .unwrap();
    assert_eq!(found.unwrap().number, 5);
}

#[tokio::test]
async fn maps_http_errors_to_workspace_errors() {
    async fn open_with(status: u16, body: serde_json::Value) -> WorkspaceError {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        client(&server)
            .open_pull_request(new_pr())
            .await
            .unwrap_err()
    }

    let err = open_with(401, json!({"message": "Bad credentials"})).await;
    assert!(
        matches!(&err, WorkspaceError::Auth(m) if m.contains("Bad credentials")),
        "{err:?}"
    );
    assert!(!err.is_retryable());

    let err = open_with(404, json!({"message": "Not Found"})).await;
    assert!(
        matches!(&err, WorkspaceError::NotFound(m) if m.contains("Not Found")),
        "{err:?}"
    );

    let err = open_with(
        422,
        json!({
            "message": "Validation Failed",
            "errors": [{"resource": "PullRequest", "code": "custom", "message": "No commits between main and agent/018f3a2b"}],
        }),
    )
    .await;
    match &err {
        WorkspaceError::Invalid(m) => {
            assert!(m.contains("Validation Failed"), "{m}");
            assert!(
                m.contains("No commits between main and agent/018f3a2b"),
                "{m}"
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert!(!err.is_retryable());

    for status in [500, 502, 503] {
        let err = open_with(status, json!({"message": "boom"})).await;
        assert!(
            matches!(err, WorkspaceError::Transient(_)),
            "{status}: {err:?}"
        );
        assert!(err.is_retryable());
    }

    let err = open_with(418, json!({"message": "teapot"})).await;
    assert!(
        matches!(err, WorkspaceError::Http { status: 418, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn errors_on_the_probe_are_mapped_too() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(0)
        .mount(&server)
        .await;
    let err = client(&server)
        .open_pull_request(new_pr())
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn rate_limiting_is_transient_and_forbidden_is_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("head", "octo:agent/limited"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .set_body_json(json!({"message": "API rate limit exceeded"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("head", "octo:agent/forbidden"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"message": "Resource not accessible"})),
        )
        .mount(&server)
        .await;
    let gh = client(&server);
    let err = gh
        .find_pull_request(&repo(), "agent/limited")
        .await
        .unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    let err = gh
        .find_pull_request(&repo(), "agent/forbidden")
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn a_token_echoed_by_the_server_is_scrubbed_from_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"message": format!("Bad credentials for {TOKEN}")})),
        )
        .mount(&server)
        .await;
    let err = client(&server)
        .find_pull_request(&repo(), "agent/x")
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains(TOKEN), "{text}");
}

#[tokio::test]
async fn unreachable_servers_are_transient() {
    let gh = GitHub::new(Arc::new(StaticToken::new(TOKEN)))
        .unwrap()
        .with_api_base("http://127.0.0.1:1");
    let err = gh.find_pull_request(&repo(), "agent/x").await.unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    assert!(!format!("{err}").contains(TOKEN));
}

#[tokio::test]
async fn api_base_supports_enterprise_prefixes_and_trailing_slashes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/octo/widgets/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([api_pull(9, "agent/x")])))
        .mount(&server)
        .await;
    let gh = GitHub::new(Arc::new(StaticToken::new(TOKEN)))
        .unwrap()
        .with_api_base(format!("{}/api/v3/", server.uri()));
    let ghe_repo = RepoRef::new("https://ghe.example.com/octo/widgets", "main");
    assert_eq!(
        gh.find_pull_request(&ghe_repo, "agent/x")
            .await
            .unwrap()
            .unwrap()
            .number,
        9
    );
}

#[tokio::test]
async fn local_repositories_are_not_github_repositories() {
    let server = MockServer::start().await;
    let err = client(&server)
        .find_pull_request(&RepoRef::new("/tmp/some/remote.git", "main"), "agent/x")
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Invalid(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}
