//! `Endpoint` against the fake of the orchestration layer's per-thread tool endpoint: stateless
//! streamable HTTP, JSON responses, a bearer token on every request, one connection per request.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::time::Duration;

use adam_mcp::{CallOptions, Endpoint, EndpointError, McpPolicy};
use adam_mcp_testkit::{GET_UI_CATALOG, ThreadToolsServer};
use secrecy::SecretString;
use serde_json::{Map, Value, json};

const DIGEST: &str = "sha256:4ed91bcc9db52d5e2262aef2091d2b3eeccbf5bfe51519d7326fdc6641fb7856";

fn token(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn endpoint(server: &ThreadToolsServer, bearer: &str) -> Endpoint {
    Endpoint::new(
        &server.url("thread-1"),
        &token(bearer),
        &McpPolicy::default(),
    )
    .unwrap()
}

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

#[tokio::test]
async fn it_lists_the_tools_and_lists_them_again_when_they_change() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    let endpoint = endpoint(&server, "good-token");

    let first = endpoint.list_tools().await.unwrap();
    assert_eq!(
        first.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        [GET_UI_CATALOG]
    );
    assert_eq!(first[0].input_schema["type"], "object");
    assert!(first[0].input_schema["properties"]["knownDigest"].is_object());
    assert!(first[0].description.contains("UI catalog"));
    assert_eq!(first[0].spec().name, GET_UI_CATALOG);

    // A tool attached a moment ago is listed by the next request: the endpoint holds no state.
    server.add_tool(
        "relay__search",
        "Search.",
        json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        "found",
    );
    let second = endpoint.list_tools().await.unwrap();
    assert_eq!(
        second.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        [GET_UI_CATALOG, "relay__search"]
    );
    server.remove_tool("relay__search");
    assert_eq!(endpoint.list_tools().await.unwrap().len(), 1);
    assert_eq!(server.lists(), 3);
    // One connection per request: nothing is kept between them.
    assert_eq!(server.initializations(), 3);
}

#[tokio::test]
async fn it_calls_a_tool_and_reads_the_structured_content_and_the_text() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    let catalog =
        json!({"catalogId": "https://agents.vymalo.com/a2ui/catalogs/chat", "components": {}});
    server.set_catalog(Some((
        "https://agents.vymalo.com/a2ui/catalogs/chat",
        2,
        DIGEST,
        catalog.clone(),
    )));
    let endpoint = endpoint(&server, "good-token");

    let result = endpoint
        .call_tool(GET_UI_CATALOG, args(json!({})))
        .await
        .unwrap();
    assert!(!result.is_error);
    let structured = result.structured.expect("structuredContent");
    assert_eq!(structured["digest"], DIGEST);
    assert_eq!(structured["unchanged"], false);
    assert_eq!(structured["catalog"], catalog);
    assert_eq!(
        serde_json::from_str::<Value>(&result.text).unwrap(),
        structured
    );

    let unchanged = endpoint
        .call_tool(GET_UI_CATALOG, args(json!({"knownDigest": DIGEST})))
        .await
        .unwrap();
    let structured = unchanged.structured.unwrap();
    assert_eq!(structured["unchanged"], true);
    assert!(structured.get("catalog").is_none());
    assert_eq!(
        server.catalog_requests(),
        [None, Some(DIGEST.to_owned())],
        "the arguments reached the tool"
    );
}

#[tokio::test]
async fn a_tool_that_fails_is_a_result_with_is_error_and_an_unknown_tool_is_a_rejection() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    let endpoint = endpoint(&server, "good-token");
    // No catalog on the thread: the tool answers, and says it failed.
    let none = endpoint
        .call_tool(GET_UI_CATALOG, args(json!({})))
        .await
        .unwrap();
    assert!(none.is_error);
    assert!(none.text.contains("no UI catalog"), "{}", none.text);
    assert!(none.structured.is_none());
    // A name nobody owns is the protocol's error, not a result.
    let unknown = endpoint
        .call_tool("ghost", args(json!({})))
        .await
        .unwrap_err();
    assert!(
        matches!(&unknown, EndpointError::Rejected(m) if m.contains("unknown tool")),
        "{unknown:?}"
    );
}

#[tokio::test]
async fn an_extra_tool_is_called_with_its_arguments() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    server.add_tool(
        "relay__search",
        "Search.",
        json!({"type": "object"}),
        "found",
    );
    let result = endpoint(&server, "good-token")
        .call_tool("relay__search", args(json!({"q": "rust"})))
        .await
        .unwrap();
    assert_eq!(result.text, r#"found {"q":"rust"}"#);
    assert_eq!(
        server.calls(),
        [("relay__search".to_owned(), json!({"q": "rust"}))]
    );
}

#[tokio::test]
async fn the_token_is_sent_on_every_request_and_never_shown() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    let endpoint = endpoint(&server, "good-token");
    endpoint.list_tools().await.unwrap();
    let auths = server.authorizations();
    assert!(!auths.is_empty());
    assert!(auths.iter().all(|a| a == "Bearer good-token"), "{auths:?}");
    assert!(!format!("{endpoint:?}").contains("good-token"));
}

#[tokio::test]
async fn a_token_the_endpoint_does_not_accept_is_unauthorized_and_the_token_is_not_in_the_error() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    server.add_tool("t", "t", json!({"type": "object"}), "x");
    let endpoint = endpoint(&server, "expired-secret-token");
    let listed = endpoint.list_tools().await.unwrap_err();
    assert_eq!(listed, EndpointError::Unauthorized, "{listed:?}");
    let called = endpoint.call_tool("t", args(json!({}))).await.unwrap_err();
    assert_eq!(called, EndpointError::Unauthorized, "{called:?}");
    assert!(server.refused() >= 2);
    assert!(server.calls().is_empty(), "nothing was called");
    assert!(!format!("{listed} {called}").contains("expired-secret-token"));
}

#[tokio::test]
async fn an_endpoint_that_is_down_fails_without_the_token() {
    // A port that is bound and never listened on: a connection to it is refused, and while the
    // socket is held no other test's server can be given the port (a dropped server frees it, and
    // a server of another test, run in parallel, answered 401 there).
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let url = format!(
        "http://{}/thread-tools/thread-1/mcp",
        socket.local_addr().unwrap()
    );
    let endpoint = Endpoint::new(&url, &token("good-token"), &McpPolicy::default()).unwrap();
    let error = endpoint.list_tools().await.unwrap_err();
    assert!(
        matches!(error, EndpointError::Failed(_) | EndpointError::Timeout(_)),
        "{error:?}"
    );
    assert!(!error.to_string().contains("good-token"));
    drop(socket);
}

#[tokio::test]
async fn a_call_that_takes_too_long_times_out() {
    // A listener that accepts and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/thread-tools/t/mcp",
        listener.local_addr().unwrap()
    );
    let _hold = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let policy = McpPolicy::default()
        .connect_timeout(Duration::from_millis(200))
        .call_timeout(Duration::from_millis(200));
    let endpoint = Endpoint::new(&url, &token("t"), &policy).unwrap();
    let error = endpoint.list_tools().await.unwrap_err();
    assert!(
        matches!(error, EndpointError::Failed(_) | EndpointError::Timeout(_)),
        "{error:?}"
    );
}

#[tokio::test]
async fn plain_http_to_another_machine_is_refused_before_anything_is_sent() {
    let error = Endpoint::new(
        "http://orchestrator.internal:8080/thread-tools/t/mcp",
        &token("good-token"),
        &McpPolicy::default(),
    )
    .unwrap_err();
    assert!(
        matches!(&error, EndpointError::Refused(m) if m.contains("plain http")),
        "{error:?}"
    );
    assert!(!error.to_string().contains("good-token"));
}

#[tokio::test]
async fn a_listed_tool_carries_its_own_meta() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    server.add_tool_with_meta(
        "relay__search",
        "Search.",
        json!({"type": "object"}),
        "found",
        json!({"thread-tools/v1": {"reportsStep": true, "timeoutSecs": 125}}),
    );
    let tools = endpoint(&server, "good-token").list_tools().await.unwrap();
    let relay = tools.iter().find(|t| t.name == "relay__search").unwrap();
    assert_eq!(
        relay.meta["thread-tools/v1"],
        json!({"reportsStep": true, "timeoutSecs": 125})
    );
    // A tool that lists none has an empty `_meta`.
    assert!(
        tools
            .iter()
            .find(|t| t.name == GET_UI_CATALOG)
            .unwrap()
            .meta
            .is_empty()
    );
}

#[tokio::test]
async fn a_call_is_sent_with_the_meta_it_was_given_and_a_plain_call_with_none() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    server.add_tool("echo", "Echo.", json!({"type": "object"}), "echo");
    let endpoint = endpoint(&server, "good-token");

    endpoint
        .call_tool("echo", args(json!({"a": 1})))
        .await
        .unwrap();
    let options = CallOptions::new().meta(
        "thread-tools/v1",
        json!({"callId": "run-1:call_7", "parentStepId": "tool:outer"}),
    );
    let result = endpoint
        .call_tool_with("echo", args(json!({"a": 2})), options)
        .await
        .unwrap();
    assert!(!result.is_error);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].meta, None, "a plain call sends no _meta");
    assert_eq!(
        requests[1].meta,
        Some(json!({"thread-tools/v1": {"callId": "run-1:call_7", "parentStepId": "tool:outer"}}))
    );
    assert_eq!(requests[1].arguments, json!({"a": 2}));
}

#[tokio::test]
async fn a_call_waits_as_long_as_its_options_say_and_not_as_long_as_the_policy() {
    let server = ThreadToolsServer::start(&["good-token"]).await;
    server.add_tool("slow", "Slow.", json!({"type": "object"}), "done");
    server.set_delay("slow", Duration::from_millis(600));
    let short = McpPolicy::default().call_timeout(Duration::from_millis(150));
    let endpoint = Endpoint::new(&server.url("t"), &token("good-token"), &short).unwrap();

    // The policy's time is the default: the call is given up on.
    let started = std::time::Instant::now();
    let error = endpoint.call_tool("slow", Map::new()).await.unwrap_err();
    assert!(matches!(error, EndpointError::Timeout(150)), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_millis(550),
        "{:?}",
        started.elapsed()
    );

    // This call says it may take longer, and is waited for.
    let result = endpoint
        .call_tool_with(
            "slow",
            Map::new(),
            CallOptions::new().timeout(Duration::from_secs(5)),
        )
        .await
        .unwrap();
    assert!(result.text.starts_with("done"), "{}", result.text);

    // And a time shorter than the policy's is honoured too, with its own time in the error.
    let error = endpoint
        .call_tool_with(
            "slow",
            Map::new(),
            CallOptions::new().timeout(Duration::from_millis(50)),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, EndpointError::Timeout(50)), "{error:?}");
}
