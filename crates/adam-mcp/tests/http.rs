//! `adam-mcp` against a real MCP server over streamable HTTP (`adam-mcp-testkit`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_mcp::{Env, Error, MAX_RESULT_BYTES, McpPolicy, McpServers, VarProblem};
use adam_mcp_testkit::{LogCapture, TestHttpServer, TestServer, wait_until};
use adam_runtime::CancelToken;
use common::{call, cancellable, http_config};
use serde_json::json;

const TOKEN: &str = "tok-7f3c9a1e-secret";

async fn connect(config: &adam_agent_fs::McpConfig) -> McpServers {
    McpServers::connect(config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap()
}

/// The tools of the test server that a name-fitting, allow-list-free config gets.
fn expected_names(server: &str) -> Vec<String> {
    TestServer::tool_names()
        .into_iter()
        .filter(|n| *n != "a.b")
        .map(|n| format!("{server}__{n}"))
        .collect()
}

#[tokio::test]
async fn lists_and_calls_over_streamable_http() {
    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;

    // `a.b` cannot be shown to a model as `t__a.b`: skipped, the rest are kept in the server's order.
    assert_eq!(servers.names(), expected_names("t"));
    let tools = servers.tools();
    let spec = tools.get("t__echo").unwrap().spec();
    assert_eq!(spec.description, "Answers with the text it is given.");
    assert_eq!(spec.parameters["properties"]["text"]["type"], "string");

    let out = call(&tools, "t__echo", json!({"text": "hello"})).await;
    assert!(!out.is_error);
    assert_eq!(out.content, "hello");
    // `null` arguments are an empty object.
    let pid = call(&tools, "t__pid", json!(null)).await;
    assert_eq!(pid.content, std::process::id().to_string());
    servers.shutdown().await;
}

#[tokio::test]
async fn servers_connect_in_name_order_and_tools_are_prefixed() {
    let server = TestHttpServer::start(None).await;
    let url = server.url();
    let config = common::config(&format!(
        r#"{{"mcpServers": {{
            "beta":  {{"type": "http", "url": "{url}", "tools": ["pid", "echo"]}},
            "alpha": {{"type": "http", "url": "{url}", "tools": ["echo"]}}
        }}}}"#
    ));
    let servers = connect(&config).await;
    assert_eq!(servers.names(), ["alpha__echo", "beta__pid", "beta__echo"]);
    assert_eq!(server.initializations(), 2, "one session per server");
    let debug = format!("{servers:?}");
    assert!(
        debug.contains("alpha") && debug.contains("beta__pid"),
        "{debug}"
    );
}

#[tokio::test]
async fn allow_list_keeps_only_listed() {
    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config(
        "t",
        &server.url(),
        r#""tools": ["pid", "echo"]"#,
    ))
    .await;
    // Only what is listed, in the allow-list's order: `fail` and `exit` are not tools.
    assert_eq!(servers.names(), ["t__pid", "t__echo"]);
    assert!(servers.tools().get("t__fail").is_none());
}

#[tokio::test]
async fn allow_listed_tool_missing_fails_startup() {
    let server = TestHttpServer::start(None).await;
    let config = http_config("t", &server.url(), r#""tools": ["echo", "ecoh"]"#);
    let error = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::UnknownTool { server, tool, available }
        if server == "t" && tool == "ecoh" && available.contains(&"echo".to_owned())),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid);
    assert!(error.to_string().contains("`ecoh`"), "{error}");
}

#[tokio::test]
async fn mixed_content_and_is_error() {
    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let tools = servers.tools();

    let mixed = call(&tools, "t__mixed", json!({})).await;
    assert!(!mixed.is_error);
    assert_eq!(
        mixed.content,
        "a line of text\n[image not included: image/png]\n[audio not included: audio/wav]\n\
         embedded text\n[binary resource not included: file:///data.bin (application/octet-stream)]\n\
         [resource link: file:///linked.txt]"
    );
    let failed = call(&tools, "t__fail", json!({})).await;
    assert!(failed.is_error);
    assert_eq!(failed.content, "failed on purpose");
}

#[tokio::test]
async fn big_result_capped() {
    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let out = call(&servers.tools(), "t__big", json!({"bytes": 300_000})).await;
    assert!(!out.is_error);
    assert!(out.content.starts_with("xxxx"));
    assert!(
        out.content
            .contains("[cut here: 65536 of 300000 bytes shown]"),
        "{}",
        &out.content[MAX_RESULT_BYTES..]
    );
    assert!(out.content.len() < MAX_RESULT_BYTES + 100);
}

#[tokio::test]
async fn arguments_must_be_an_object_and_server_refusals_are_error_results() {
    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let tools = servers.tools();
    let calls_before = server.calls();
    let out = call(&tools, "t__echo", json!("just a string")).await;
    assert!(out.is_error);
    assert!(
        out.content.contains("takes a JSON object"),
        "{}",
        out.content
    );
    assert_eq!(server.calls(), calls_before, "nothing was sent");

    // The server refuses `echo` without `text` (a protocol error): an error result, session kept.
    let refused = call(&tools, "t__echo", json!({})).await;
    assert!(refused.is_error);
    assert!(
        refused.content.contains("refused the call to `t__echo`"),
        "{}",
        refused.content
    );
    assert!(
        refused.content.contains("`text` must be a string"),
        "{}",
        refused.content
    );
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "still fine"}))
            .await
            .content,
        "still fine"
    );
    assert_eq!(server.initializations(), 1);
}

#[tokio::test]
async fn header_token_sent_and_never_logged() {
    let logs = LogCapture::start();
    let server = TestHttpServer::start(Some(TOKEN)).await;
    let config = http_config(
        "t",
        &server.url(),
        r#""headers": {"Authorization": "Bearer ${MCP_TEST_TOKEN}"}"#,
    );
    let env = Env::new().var("MCP_TEST_TOKEN", TOKEN);
    let servers = McpServers::connect(&config, &env, &McpPolicy::default())
        .await
        .unwrap();
    let out = call(&servers.tools(), "t__echo", json!({"text": "hi"})).await;
    assert_eq!(out.content, "hi");

    // The token went with every request...
    let sent = server.authorizations();
    assert!(!sent.is_empty());
    assert!(
        sent.iter().all(|h| h == &format!("Bearer {TOKEN}")),
        "{sent:?}"
    );
    // ...and nowhere else: not the logs (at TRACE, from rmcp, hyper and reqwest), not `Debug`.
    let text = logs.text();
    assert!(
        text.contains("connected to the MCP server"),
        "the logs are being captured: {text}"
    );
    assert!(!text.contains(TOKEN), "the token is in the logs");
    for shown in [
        format!("{servers:?}"),
        format!("{:?}", servers.tools()),
        format!("{env:?}"),
        format!("{:?}", config),
    ] {
        // The config holds `${VAR}` as written, and the others hold names only.
        assert!(!shown.contains(TOKEN), "{shown}");
    }
    servers.shutdown().await;
}

#[tokio::test]
async fn debug_shows_no_header_values() {
    let server = TestHttpServer::start(Some(TOKEN)).await;
    let config = http_config(
        "t",
        &server.url(),
        r#""headers": {"Authorization": "Bearer ${MCP_TEST_TOKEN}", "X-Api-Key": "${MCP_TEST_TOKEN}"}"#,
    );
    let env = Env::new().var("MCP_TEST_TOKEN", TOKEN);
    let servers = McpServers::connect(&config, &env, &McpPolicy::default())
        .await
        .unwrap();
    let tools = servers.tools();
    for shown in [
        format!("{servers:?}"),
        format!("{tools:?}"),
        format!("{env:?}"),
        format!("{:#?}", servers),
    ] {
        assert!(!shown.contains(TOKEN), "{shown}");
    }
    // Names only: the servers and their tools.
    let shown = format!("{servers:?}");
    assert!(
        shown.contains("\"t\"") && shown.contains("t__echo"),
        "{shown}"
    );
    assert!(
        !shown.contains("Authorization") && !shown.contains("headers"),
        "{shown}"
    );
}

#[tokio::test]
async fn wrong_token_fails_startup_without_showing_it() {
    let logs = LogCapture::start();
    let server = TestHttpServer::start(Some(TOKEN)).await;
    let wrong = "tok-wrong-51d0b7e2";
    let config = http_config(
        "t",
        &server.url(),
        r#""headers": {"Authorization": "Bearer ${MCP_TEST_TOKEN}"}"#,
    );
    let env = Env::new().var("MCP_TEST_TOKEN", wrong);
    let error = McpServers::connect(&config, &env, &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Connect { .. }), "{error}");
    assert_eq!(error.class(), ErrorClass::Transient);
    let text = format!("{error} / {error:?}");
    assert!(!text.contains(wrong) && !text.contains(TOKEN), "{text}");
    assert!(server.requests() >= 1);
    let logged = logs.text();
    assert!(
        !logged.contains(wrong) && !logged.contains(TOKEN),
        "the token is in the logs"
    );
}

#[tokio::test]
async fn errors_scrubbed_of_expanded_values() {
    // A key in the query (which needs the opt-in) is a secret too. The server is stopped, so the
    // request fails with "connection refused", and the HTTP client's message repeats the URL it
    // dialled, key included: only the redactor keeps the key out of the error.
    let mut server = TestHttpServer::start(None).await;
    let url = server.url();
    server.stop().await;
    let key = "key-93b1d4c0";
    let config = http_config("t", &format!("{url}?key=${{MCP_TEST_KEY}}"), "");
    let env = Env::new().var("MCP_TEST_KEY", key);
    let policy = McpPolicy::default().allow_url_secrets(true);
    let error = McpServers::connect(&config, &env, &policy)
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::Connect { .. }), "{error}");
    let text = format!("{error} / {error:?}");
    assert!(!text.contains(key), "{text}");
    // It is the redactor that did it: the client's message repeats the URL it dialled (the
    // mutation check of this test: with the redactor turned into the identity, the key is in it).
    assert!(
        text.contains("error sending request for url ([REDACTED])"),
        "{text}"
    );
}

#[tokio::test]
async fn a_variable_in_the_url_is_refused_by_default_and_works_with_the_opt_in() {
    let server = TestHttpServer::start(None).await;
    let key = "key-2f6c81d5";
    let config = http_config("t", &format!("{}?key=${{MCP_TEST_KEY}}", server.url()), "");
    let env = Env::new().var("MCP_TEST_KEY", key);

    // The default: refused before anything is sent, naming the variable and never its value.
    let error = McpServers::connect(&config, &env, &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::UrlSecret { server, var }
            if server == "t" && var == "MCP_TEST_KEY"),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid);
    let text = format!("{error} / {error:?}");
    assert!(
        text.contains("MCP_TEST_KEY") && text.contains("headers"),
        "{text}"
    );
    assert!(!text.contains(key), "{text}");
    assert_eq!(server.requests(), 0, "nothing was sent");

    // With the opt-in it connects, and the key is what the server was dialled with.
    let policy = McpPolicy::default().allow_url_secrets(true);
    let servers = McpServers::connect(&config, &env, &policy).await.unwrap();
    assert_eq!(
        call(&servers.tools(), "t__echo", json!({"text": "hi"}))
            .await
            .content,
        "hi"
    );
    assert!(server.requests() > 0);
    // Errors, results and `Debug` still do not show it.
    for shown in [format!("{servers:?}"), format!("{:?}", servers.tools())] {
        assert!(!shown.contains(key), "{shown}");
    }
    servers.shutdown().await;
}

#[tokio::test]
async fn the_sdk_logs_the_expanded_url_which_is_why_secrets_in_urls_are_opt_in() {
    // The documented leak path, pinned: the SDK logs the URL it dials, in its own log lines, which
    // no redactor of ours reaches (at TRACE for a request that failed; at ERROR, "fail to delete
    // session", when the server is gone at shutdown and the SDK cannot end the session). This is
    // what `McpPolicy::allow_url_secrets` warns about; if a newer `rmcp` stops doing it, this
    // test fails, and the README and the rustdoc of the policy can say so.
    let logs = LogCapture::start();
    let mut server = TestHttpServer::start(None).await;
    let key = "key-c5e07a19";
    let config = http_config("t", &format!("{}?key=${{MCP_TEST_KEY}}", server.url()), "");
    let env = Env::new().var("MCP_TEST_KEY", key);
    let policy = McpPolicy::default().allow_url_secrets(true);
    let servers = McpServers::connect(&config, &env, &policy).await.unwrap();
    let tools = servers.tools();
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "x"})).await.content,
        "x"
    );
    server.stop().await;
    // What this crate hands out is scrubbed all the same...
    let down = call(&tools, "t__echo", json!({"text": "y"})).await;
    assert!(
        down.is_error && !down.content.contains(key),
        "{}",
        down.content
    );
    servers.shutdown().await;
    drop(tools);
    // ...but the SDK's own log line has the key.
    wait_until("the SDK to log the URL it dialled", || async {
        logs.text()
            .lines()
            .any(|line| line.contains("rmcp::") && line.contains(key))
    })
    .await;
    // Without the opt-in there is no such URL to log: the file is refused (tested above).
}

#[tokio::test]
async fn the_whole_url_from_a_variable_needs_the_opt_in_too() {
    let server = TestHttpServer::start(None).await;
    let config = http_config("t", "${MCP_TEST_URL}", "");
    let env = Env::new().var("MCP_TEST_URL", server.url());
    let error = McpServers::connect(&config, &env, &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::UrlSecret { var, .. } if var == "MCP_TEST_URL"),
        "{error}"
    );
    assert_eq!(server.requests(), 0);
    let policy = McpPolicy::default().allow_url_secrets(true);
    let servers = McpServers::connect(&config, &env, &policy).await.unwrap();
    assert_eq!(servers.names(), expected_names("t"));
}

#[tokio::test]
async fn missing_var_fails_before_any_request() {
    let server = TestHttpServer::start(None).await;
    let config = http_config(
        "t",
        &server.url(),
        r#""headers": {"Authorization": "Bearer ${MCP_TEST_SURELY_UNSET_3B7E}"}"#,
    );
    let error = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Var { var, problem: VarProblem::Missing, .. }
        if var == "MCP_TEST_SURELY_UNSET_3B7E"),
        "{error}"
    );
    assert_eq!(server.requests(), 0, "nothing was sent");
}

#[tokio::test]
async fn refusals_before_any_request() {
    let server = TestHttpServer::start(None).await;
    // `type: sse`, and a URL with credentials, are refused before a socket is opened.
    let sse = common::config(&format!(
        r#"{{"mcpServers": {{"t": {{"type": "sse", "url": "{}"}}}}}}"#,
        server.url()
    ));
    let error = McpServers::connect(&sse, &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::SseUnsupported { .. }), "{error}");
    let creds = http_config(
        "t",
        &server.url().replace("http://", "http://carol:hunter2@"),
        "",
    );
    let error = McpServers::connect(&creds, &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Url { .. }), "{error}");
    assert!(!error.to_string().contains("hunter2"));
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn server_down_at_startup_fails_closed() {
    let mut server = TestHttpServer::start(None).await;
    let url = server.url();
    server.stop().await;
    let error = McpServers::connect(
        &http_config("t", &url, ""),
        &Env::new(),
        &McpPolicy::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, Error::Connect { server, .. } if server == "t"),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Transient);
}

#[tokio::test]
async fn server_stopping_mid_run_gives_error_result_then_reconnects() {
    let mut server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let tools = servers.tools();
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "one"}))
            .await
            .content,
        "one"
    );
    assert_eq!(server.initializations(), 1);

    server.stop().await;
    // The server is gone: an error result (the run goes on), never a `ToolError`, never a panic.
    let down = call(&tools, "t__echo", json!({"text": "two"})).await;
    assert!(down.is_error, "{}", down.content);
    assert!(down.content.contains("`t`"), "{}", down.content);

    // It comes back (with no session of the old one): the next call reconnects, once, from the
    // same recipe, and the tools are not listed again.
    server.restart().await;
    let back = call(&tools, "t__echo", json!({"text": "three"})).await;
    assert!(!back.is_error, "{}", back.content);
    assert_eq!(back.content, "three");
    assert_eq!(server.initializations(), 2);
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "four"}))
            .await
            .content,
        "four"
    );
    assert_eq!(
        server.initializations(),
        2,
        "one reconnect, not one per call"
    );
}

#[tokio::test]
async fn a_server_restarted_between_calls_costs_at_most_one_error_result() {
    // The server restarts with no memory of the session, while the client still holds one. What the
    // client notices first (the closed stream, or the 404 of the next request) decides whether the
    // call in between fails or reconnects first; either way the damage is one call, never a hang, a
    // second request behind our back, or a client that stays broken.
    let mut server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let tools = servers.tools();
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "before"}))
            .await
            .content,
        "before"
    );
    let calls_before = server.calls();
    let posts_before = server.posts();

    server.restart().await;
    let between = call(&tools, "t__echo", json!({"text": "between"})).await;
    let after = call(&tools, "t__echo", json!({"text": "after"})).await;
    assert!(
        !after.is_error,
        "the client recovered: {} / {}",
        between.content, after.content
    );
    assert_eq!(after.content, "after");
    if between.is_error {
        assert!(
            between.content.contains("may or may not have run"),
            "{}",
            between.content
        );
    }
    // Never sent twice. Whichever way the client found out, the POSTs are: `between` or `after`
    // that met the 404 (one), then a new session (`initialize` and its notification: two), and the
    // call that ran on it (one), so four. A request sent again behind our back, which is what the
    // SDK's `reinit_on_expired_session` does after a 404, would be a fifth. (The first POST of a
    // stale session never reaches the handler, so `calls()` cannot tell.)
    let posts = server.posts() - posts_before;
    assert!(server.calls() - calls_before <= 2);
    assert_eq!(posts, 4, "POSTs after the restart");
    assert_eq!(server.initializations(), 2, "one new session");
}

#[tokio::test]
async fn slow_call_times_out_as_error_result() {
    let server = TestHttpServer::start(None).await;
    let policy = McpPolicy::default().call_timeout(Duration::from_millis(300));
    let servers = McpServers::connect(&http_config("t", &server.url(), ""), &Env::new(), &policy)
        .await
        .unwrap();
    let tools = servers.tools();
    let out = call(&tools, "t__slow", json!({})).await;
    assert!(out.is_error);
    assert!(
        out.content.contains("no answer within 300 ms"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("may still be running"),
        "{}",
        out.content
    );
    assert!(
        out.content.contains("may or may not have run"),
        "{}",
        out.content
    );
    // The session is fine: the next call works.
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "after"}))
            .await
            .content,
        "after"
    );
    assert_eq!(server.initializations(), 1);
}

#[tokio::test]
async fn cancellation_stops_a_call() {
    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let tools = servers.tools();
    let token = CancelToken::new();
    let tool = tools.get("t__slow").unwrap().clone();
    let ctx = cancellable("t__slow", &token);
    let call = tokio::spawn(async move { tool.call(&ctx, json!({})).await });

    // The call is on the server, and would wait the whole default call timeout (60 s)...
    wait_until("the call to reach the server", || async {
        server.calls() >= 1
    })
    .await;
    token.cancel();
    // ...but it returns as soon as the run is cancelled.
    let out = tokio::time::timeout(Duration::from_secs(10), call)
        .await
        .expect("the cancelled call returns")
        .unwrap()
        .unwrap();
    assert!(out.is_error);
    assert!(out.content.contains("cancelled"), "{}", out.content);
    // Another call is unaffected.
    assert_eq!(
        common::call(&tools, "t__echo", json!({"text": "ok"}))
            .await
            .content,
        "ok"
    );
}

#[tokio::test]
async fn shutdown_closes_and_later_calls_are_error_results() {
    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let tools = servers.tools();
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "x"})).await.content,
        "x"
    );
    servers.shutdown().await;
    let out = call(&tools, "t__echo", json!({"text": "y"})).await;
    assert!(out.is_error);
    assert!(out.content.contains("shut down"), "{}", out.content);
    assert_eq!(
        server.initializations(),
        1,
        "a shut-down connection does not redial"
    );
}
