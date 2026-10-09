//! `adam-mcp` against a real MCP server over streamable HTTP (`adam-mcp-testkit`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adam_error::{Classify, ErrorClass};
use adam_llm_agent::ToolError;
use adam_mcp::{CallBearer, Env, Error, MAX_RESULT_BYTES, McpPolicy, McpServers, VarProblem};
use adam_mcp_testkit::{LogCapture, TestHttpServer, TestServer, wait_until};
use adam_runtime::CancelToken;
use async_trait::async_trait;
use common::{call, cancellable, http_config};
use secrecy::SecretString;
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
async fn a_tool_is_labelled_in_its_step_with_the_title_its_server_gave() {
    use adam_llm_agent::StepStyle;

    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("t", &server.url(), "")).await;
    let tools = servers.tools();
    // `echo` has a title: that is what its step is called. The others have none: the step says
    // the name the model knows (the default label), whatever the description says.
    assert_eq!(
        tools.get("t__echo").unwrap().step_style(),
        StepStyle::default().with_label("Echo it back")
    );
    assert_eq!(
        tools.get("t__pid").unwrap().step_style(),
        StepStyle::default()
    );
    // The title is for the person: the model still sees the description.
    assert_eq!(
        tools.get("t__echo").unwrap().spec().description,
        "Answers with the text it is given."
    );
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

/// A server whose entry says `files: true` hands its images and blobs to the person as files: each is
/// a file artifact of the result and the model reads one line about it. The same server without the
/// key describes them and keeps no byte (fail closed).
#[tokio::test]
async fn a_server_with_files_true_shares_its_images_and_blobs_as_files() {
    use adam_mcp_testkit::{PDF, PNG};

    let server = TestHttpServer::start(None).await;
    let servers = connect(&http_config("browser", &server.url(), r#""files": true"#)).await;
    let tools = servers.tools();

    let shot = call(&tools, "browser__screenshot", json!({})).await;
    assert!(!shot.is_error, "{}", shot.content);
    assert_eq!(
        shot.content,
        "Shared screenshot-1.png (67 bytes, image/png). To show it in your answer, write \
         ![description](screenshot-1.png)."
    );
    assert_eq!(shot.artifacts.len(), 1);
    let artifact = &shot.artifacts[0];
    assert_eq!(artifact.name, "screenshot-1.png");
    assert_eq!(artifact.mime_type.as_deref(), Some("image/png"));
    let file = artifact.file.as_ref().unwrap();
    assert_eq!(
        (file.filename.as_str(), file.bytes.as_slice()),
        ("screenshot-1.png", PNG)
    );

    let pdf = call(&tools, "browser__pdf", json!({})).await;
    assert_eq!(
        pdf.content,
        format!("Shared pdf-1.pdf ({} bytes, application/pdf).", PDF.len())
    );
    assert_eq!(pdf.artifacts[0].file.as_ref().unwrap().bytes, PDF);

    // Over the cap of one file: the model is told, nothing is kept, the result is an error.
    let big = call(
        &tools,
        "browser__png",
        json!({"bytes": adam_runtime::MAX_ARTIFACT_FILE_BYTES + 1}),
    )
    .await;
    assert!(big.is_error);
    assert!(big.artifacts.is_empty());
    assert!(
        big.content
            .starts_with("Not shared: a file (image/png) of 4194305 bytes is over the limit"),
        "{}",
        big.content
    );
    servers.shutdown().await;

    // Without `files`, the same server's files are described and none is kept.
    let plain = connect(&http_config("browser", &server.url(), "")).await;
    let shot = call(&plain.tools(), "browser__screenshot", json!({})).await;
    assert_eq!(shot.content, "[image not included: image/png]");
    assert!(shot.artifacts.is_empty());
    plain.shutdown().await;
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

// ---- a bearer per call (`McpPolicy::bearer_per_call`) ----

const LISTING_TOKEN: &str = "listing-token-5b81e0";

/// What the deployment's [`CallBearer`] answers, and what it was asked.
struct Scripted {
    /// How `for_call` answers: a new token each call (`call-token-<n>-<hex>`), or an error.
    answer: Mutex<Answer>,
    issued: AtomicUsize,
    listings: AtomicUsize,
    asked: Mutex<Vec<(String, serde_json::Map<String, serde_json::Value>)>>,
}

#[derive(Clone)]
enum Answer {
    Token,
    Permanent(&'static str),
    Transient(&'static str),
    Empty,
}

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(Answer::Token),
            issued: AtomicUsize::new(0),
            listings: AtomicUsize::new(0),
            asked: Mutex::default(),
        })
    }

    fn answer(&self, answer: Answer) {
        *self.answer.lock().unwrap() = answer;
    }

    /// The token `for_call` gives the `n`th time it answers with one (from 1).
    fn token(n: usize) -> String {
        format!("call-token-{n}-3fa9c1")
    }

    fn asked(&self) -> Vec<(String, serde_json::Map<String, serde_json::Value>)> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl CallBearer for Scripted {
    async fn for_listing(&self) -> Result<SecretString, ToolError> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        Ok(SecretString::from(LISTING_TOKEN.to_owned()))
    }

    async fn for_call(
        &self,
        tool: &str,
        arguments: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<SecretString, ToolError> {
        self.asked
            .lock()
            .unwrap()
            .push((tool.to_owned(), arguments.clone()));
        let answer = self.answer.lock().unwrap().clone();
        match answer {
            Answer::Token => {
                let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(SecretString::from(Self::token(n)))
            }
            Answer::Permanent(why) => Err(ToolError::Permanent(why.to_owned())),
            Answer::Transient(why) => Err(ToolError::Transient(why.to_owned())),
            Answer::Empty => Ok(SecretString::from(String::new())),
        }
    }
}

fn bound(server: &TestHttpServer, bearer: &Arc<Scripted>) -> McpPolicy {
    // The whole URL is given and only its origin counts.
    McpPolicy::default().bearer_per_call("t", &server.url(), bearer.clone())
}

async fn connect_bound(server: &TestHttpServer, bearer: &Arc<Scripted>) -> McpServers {
    McpServers::connect(
        &http_config("t", &server.url(), ""),
        &Env::new(),
        &bound(server, bearer),
    )
    .await
    .unwrap()
}

/// The `Authorization` headers that came in after the first `seen`, each as it was sent.
fn since(server: &TestHttpServer, seen: usize) -> Vec<String> {
    server.authorizations().split_off(seen)
}

#[tokio::test]
async fn listing_uses_the_listing_bearer() {
    let server = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    let servers = connect_bound(&server, &bearer).await;

    // The tools are those of the server, as without a binding...
    assert_eq!(servers.names(), expected_names("t"));
    // ...listed once, with the listing bearer on every request of that connection, and the
    // per-call bearer not asked for.
    assert_eq!(bearer.listings.load(Ordering::SeqCst), 1);
    assert!(bearer.asked().is_empty());
    let sent = server.authorizations();
    assert!(!sent.is_empty());
    assert!(
        sent.iter().all(|h| h == &format!("Bearer {LISTING_TOKEN}")),
        "{sent:?}"
    );
    assert_eq!(server.initializations(), 1);
    servers.shutdown().await;
}

#[tokio::test]
async fn a_per_call_bearer_is_sent_with_each_call() {
    let logs = LogCapture::start();
    let server = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    let servers = connect_bound(&server, &bearer).await;
    let tools = servers.tools();
    let listed = server.authorizations().len();

    let first = call(&tools, "t__echo", json!({"text": "one"})).await;
    assert!(!first.is_error, "{}", first.content);
    assert_eq!(first.content, "one");
    let first_sent = since(&server, listed);
    assert!(!first_sent.is_empty());
    assert!(
        first_sent
            .iter()
            .all(|h| h == &format!("Bearer {}", Scripted::token(1))),
        "{first_sent:?}"
    );

    let seen = server.authorizations().len();
    let second = call(&tools, "t__echo", json!({"text": "two"})).await;
    assert_eq!(second.content, "two");
    let second_sent = since(&server, seen);
    assert!(
        second_sent
            .iter()
            .all(|h| h == &format!("Bearer {}", Scripted::token(2))),
        "{second_sent:?}"
    );

    // The bearer was asked for each call, with the tool's name on the server and the model's
    // arguments as they were.
    assert_eq!(
        bearer.asked(),
        vec![
            (
                "echo".to_owned(),
                json!({"text": "one"}).as_object().unwrap().clone()
            ),
            (
                "echo".to_owned(),
                json!({"text": "two"}).as_object().unwrap().clone()
            ),
        ]
    );
    // One connection per call, on top of the listing's: three `initialize`s, none kept.
    assert_eq!(server.initializations(), 3);
    assert_eq!(bearer.listings.load(Ordering::SeqCst), 1);

    // Neither token is in the logs (at `TRACE`, from `rmcp`, `hyper` and `reqwest`) or in `Debug`.
    let text = logs.text();
    assert!(
        text.contains("connected to the MCP server"),
        "the logs are being captured: {text}"
    );
    for token in [
        LISTING_TOKEN.to_owned(),
        Scripted::token(1),
        Scripted::token(2),
    ] {
        assert!(!text.contains(&token), "{token} is in the logs");
        for shown in [format!("{servers:?}"), format!("{tools:?}")] {
            assert!(!shown.contains(&token), "{shown}");
        }
    }
    servers.shutdown().await;
}

#[tokio::test]
async fn a_refused_bearer_is_the_tools_error_result_and_nothing_is_sent() {
    let server = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    let servers = connect_bound(&server, &bearer).await;
    let tools = servers.tools();
    let (requests, calls) = (server.requests(), server.calls());

    bearer.answer(Answer::Permanent(
        "this account is not installed: ask the person",
    ));
    let out = call(&tools, "t__echo", json!({"text": "x"})).await;
    assert!(out.is_error);
    assert!(
        out.content.contains("was not sent")
            && out
                .content
                .contains("this account is not installed: ask the person"),
        "{}",
        out.content
    );
    assert_eq!(server.requests(), requests, "nothing was sent");
    assert_eq!(server.calls(), calls);

    // A token that cannot be sent as a header is refused the same way, without being shown.
    bearer.answer(Answer::Empty);
    let out = call(&tools, "t__echo", json!({"text": "x"})).await;
    assert!(
        out.is_error && out.content.contains("was not sent"),
        "{}",
        out.content
    );
    assert_eq!(server.requests(), requests, "nothing was sent");

    // The next call, with a token again, works.
    bearer.answer(Answer::Token);
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "back"}))
            .await
            .content,
        "back"
    );
    servers.shutdown().await;
}

#[tokio::test]
async fn a_transient_bearer_failure_is_a_transient_tool_error() {
    let server = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    let servers = connect_bound(&server, &bearer).await;
    let tools = servers.tools();
    let requests = server.requests();

    bearer.answer(Answer::Transient("the token service is busy"));
    let tool = tools.get("t__echo").unwrap();
    let error = tool
        .call(&common::ctx("t__echo"), json!({"text": "x"}))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ToolError::Transient(why) if why == "the token service is busy"),
        "{error}"
    );
    // Safe to retry: nothing reached the server.
    assert_eq!(server.requests(), requests, "nothing was sent");
    servers.shutdown().await;
}

#[tokio::test]
async fn a_binding_to_another_origin_is_refused_at_connect() {
    let server = TestHttpServer::start(None).await;
    let elsewhere = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    // The deployment bound `t` at `elsewhere`; the folder points it at `server`.
    let policy = McpPolicy::default().bearer_per_call("t", &elsewhere.url(), bearer.clone());
    let error = McpServers::connect(&http_config("t", &server.url(), ""), &Env::new(), &policy)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::BearerBinding { server, why }
            if server == "t" && why.contains("bearer only at")),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid);
    let origin_of = |s: &TestHttpServer| s.url().trim_end_matches("/mcp").to_owned();
    let text = error.to_string();
    assert!(
        text.contains(&origin_of(&elsewhere)) && text.contains(&origin_of(&server)),
        "{text}"
    );
    // Nothing was asked for and nothing was sent, to either.
    assert_eq!(server.requests() + elsewhere.requests(), 0);
    assert_eq!(bearer.listings.load(Ordering::SeqCst), 0);
    assert!(bearer.asked().is_empty());

    // A binding that is not an origin at all matches nothing.
    let policy = McpPolicy::default().bearer_per_call("t", "not a url", bearer.clone());
    let error = McpServers::connect(&http_config("t", &server.url(), ""), &Env::new(), &policy)
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::BearerBinding { .. }), "{error}");
    assert_eq!(server.requests(), 0);

    // The same origin by another spelling (another path, a query) is the same origin.
    let policy = McpPolicy::default().bearer_per_call(
        "t",
        &format!("{}/elsewhere?x=1", server.url().trim_end_matches("/mcp")),
        bearer,
    );
    let servers = McpServers::connect(&http_config("t", &server.url(), ""), &Env::new(), &policy)
        .await
        .unwrap();
    servers.shutdown().await;
}

#[tokio::test]
async fn a_static_authorization_header_with_a_binding_is_refused() {
    let server = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    let file_secret = "file-secret-71d4";
    for header in ["Authorization", "authorization", "AUTHORIZATION"] {
        let config = http_config(
            "t",
            &server.url(),
            &format!(r#""headers": {{"{header}": "Bearer ${{MCP_TEST_TOKEN}}"}}"#),
        );
        let env = Env::new().var("MCP_TEST_TOKEN", file_secret);
        let error = McpServers::connect(&config, &env, &bound(&server, &bearer))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::BearerBinding { server, why }
                if server == "t" && why.contains("Authorization")),
            "{header}: {error}"
        );
        assert_eq!(error.class(), ErrorClass::Invalid);
        let text = format!("{error} / {error:?}");
        assert!(!text.contains(file_secret), "{text}");
    }
    // Even one whose variable is not set is refused as such, not as a missing variable.
    let config = http_config(
        "t",
        &server.url(),
        r#""headers": {"Authorization": "Bearer ${MCP_TEST_SURELY_UNSET_9C2E}"}"#,
    );
    let error = McpServers::connect(&config, &Env::new(), &bound(&server, &bearer))
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::BearerBinding { .. }), "{error}");
    assert_eq!(server.requests(), 0, "nothing was sent");

    // Other headers are kept, and sent beside the bearer.
    let config = http_config("t", &server.url(), r#""headers": {"X-Team": "blue"}"#);
    let servers = McpServers::connect(&config, &Env::new(), &bound(&server, &bearer))
        .await
        .unwrap();
    servers.shutdown().await;
}

#[tokio::test]
async fn the_per_call_bearer_is_scrubbed_from_results() {
    let server = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    let servers = connect_bound(&server, &bearer).await;
    let tools = servers.tools();

    // A server that says the credential it was given: here the model's own argument carries it
    // back (the first call is given the first token).
    let token = Scripted::token(1);
    let out = call(
        &tools,
        "t__echo",
        json!({"text": format!("you sent Bearer {token}, and {token} alone")}),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "you sent [REDACTED], and [REDACTED] alone");
    assert!(!out.content.contains(&token));

    // The token of the call is not the redactor's forever: a later call, with another token,
    // is scrubbed of its own (and says the earlier one, which nobody holds any more, as it is).
    let second = Scripted::token(2);
    let out = call(&tools, "t__echo", json!({"text": second.clone()})).await;
    assert_eq!(out.content, "[REDACTED]");
    servers.shutdown().await;
}

#[tokio::test]
async fn a_server_that_refuses_the_listing_bearer_fails_connect_without_showing_it() {
    let server = TestHttpServer::start(Some("the-token-the-server-wants")).await;
    let bearer = Scripted::new();
    // The listing bearer is not accepted either, so this fails at startup, and says no token.
    let error = McpServers::connect(
        &http_config("t", &server.url(), ""),
        &Env::new(),
        &bound(&server, &bearer),
    )
    .await
    .unwrap_err();
    assert!(matches!(&error, Error::Connect { .. }), "{error}");
    let text = format!("{error} / {error:?}");
    assert!(!text.contains(LISTING_TOKEN), "{text}");
}

#[tokio::test]
async fn a_bearer_refused_for_listing_fails_connect_by_its_class() {
    struct Refusing(ToolError);
    #[async_trait]
    impl CallBearer for Refusing {
        async fn for_listing(&self) -> Result<SecretString, ToolError> {
            Err(self.0.clone())
        }
        async fn for_call(
            &self,
            _tool: &str,
            _arguments: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<SecretString, ToolError> {
            unreachable!("never listed, never called")
        }
    }
    let server = TestHttpServer::start(None).await;
    let policy =
        |error| McpPolicy::default().bearer_per_call("t", &server.url(), Arc::new(Refusing(error)));
    let config = http_config("t", &server.url(), "");

    let error = McpServers::connect(
        &config,
        &Env::new(),
        &policy(ToolError::Permanent("no key".to_owned())),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, Error::BearerBinding { why, .. } if why.contains("no key")),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid);

    let error = McpServers::connect(
        &config,
        &Env::new(),
        &policy(ToolError::Transient("busy".to_owned())),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, Error::ListTools { message, .. } if message.contains("busy")),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Transient);
    assert_eq!(server.requests(), 0, "nothing was sent");
}

#[tokio::test]
async fn a_server_of_another_name_is_not_bound_and_shutdown_closes_a_bound_one() {
    let server = TestHttpServer::start(None).await;
    let bearer = Scripted::new();
    // `t` is bound (by `bound`), `u` is the same server under another name: it is kept, with no
    // bearer, as without a binding.
    let url = server.url();
    let config = common::config(&format!(
        r#"{{"mcpServers": {{
            "t": {{"type": "http", "url": "{url}", "tools": ["echo"]}},
            "u": {{"type": "http", "url": "{url}", "tools": ["echo"]}}
        }}}}"#
    ));
    let servers = McpServers::connect(&config, &Env::new(), &bound(&server, &bearer))
        .await
        .unwrap();
    let tools = servers.tools();
    let seen = server.authorizations().len();
    assert_eq!(
        call(&tools, "u__echo", json!({"text": "plain"}))
            .await
            .content,
        "plain"
    );
    assert!(bearer.asked().is_empty(), "`u` is not bound");
    assert_eq!(
        since(&server, seen),
        Vec::<String>::new(),
        "`u` sends no bearer"
    );

    // A bound server's calls end with `shutdown`, as a kept one's do.
    assert_eq!(
        call(&tools, "t__echo", json!({"text": "bound"}))
            .await
            .content,
        "bound"
    );
    servers.shutdown().await;
    let requests = server.requests();
    let out = call(&tools, "t__echo", json!({"text": "late"})).await;
    assert!(
        out.is_error && out.content.contains("shut down"),
        "{}",
        out.content
    );
    assert_eq!(server.requests(), requests);
    assert_eq!(
        bearer.asked().len(),
        1,
        "no bearer was asked for after shutdown"
    );
}

/// A URL nothing listens on: the port was free a moment ago.
async fn a_url_nobody_answers() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/mcp")
}

fn two_servers(down: &str, down_extra: &str, up: &str) -> adam_agent_fs::McpConfig {
    common::config(&format!(
        r#"{{"mcpServers": {{
            "down": {{"type": "http", "url": "{down}"{down_extra}}},
            "up": {{"type": "http", "url": "{up}"}}}}}}"#
    ))
}

#[tokio::test]
async fn an_optional_server_that_is_down_is_skipped_with_a_warning_and_the_rest_connect() {
    let logs = LogCapture::start();
    let up = TestHttpServer::start(None).await;
    let down = a_url_nobody_answers().await;
    let config = two_servers(&down, r#", "optional": true"#, &up.url());
    let servers = connect(&config).await;
    assert_eq!(
        servers.names(),
        expected_names("up"),
        "only the one that is up"
    );
    let text = logs.text();
    assert!(
        text.contains("the optional MCP server is skipped") && text.contains("down"),
        "{text}"
    );
}

#[tokio::test]
async fn a_required_server_that_is_down_still_stops_startup_as_transient() {
    let up = TestHttpServer::start(None).await;
    let down = a_url_nobody_answers().await;
    let config = two_servers(&down, "", &up.url());
    let error = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Connect { server, .. } if server == "down"),
        "{error}"
    );
    // The class the worker's exit code (69, a supervisor retries) comes from.
    assert_eq!(error.class(), ErrorClass::Transient);
    // `optional: false` is the same as absent.
    let config = two_servers(&down, r#", "optional": false"#, &up.url());
    assert!(
        McpServers::connect(&config, &Env::new(), &McpPolicy::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_optional_server_that_refuses_the_credentials_or_lacks_an_allowed_tool_is_skipped() {
    let guarded = TestHttpServer::start(Some(TOKEN)).await;
    let up = TestHttpServer::start(None).await;
    let config = common::config(&format!(
        r#"{{"mcpServers": {{
            "locked": {{"type": "http", "url": "{}", "optional": true,
                        "headers": {{"Authorization": "Bearer wrong-token-4c2a9e"}}}},
            "renamed": {{"type": "http", "url": "{}", "optional": true, "tools": ["no_such_tool"]}},
            "up": {{"type": "http", "url": "{}"}}}}}}"#,
        guarded.url(),
        up.url(),
        up.url()
    ));
    let servers = connect(&config).await;
    assert_eq!(servers.names(), expected_names("up"));
}

#[tokio::test]
async fn an_optional_server_whose_key_has_no_value_is_skipped_and_a_required_one_is_an_error() {
    let up = TestHttpServer::start(None).await;
    let headers = r#""headers": {"Authorization": "Bearer ${MCP_TEST_OPTIONAL_KEY}"}"#;
    let one = |optional: &str| {
        common::config(&format!(
            r#"{{"mcpServers": {{
                "keyed": {{"type": "http", "url": "{}", {headers}{optional}}},
                "up": {{"type": "http", "url": "{}"}}}}}}"#,
            up.url(),
            up.url()
        ))
    };
    // Unset: skipped when optional, and nothing is sent to it.
    let before = up.requests();
    let servers = connect(&one(r#", "optional": true"#)).await;
    assert_eq!(servers.names(), expected_names("up"));
    let after_optional = up.requests();
    assert!(after_optional > before, "`up` was listed");

    // Empty (a Secret key with no value): the same, and an error when required.
    let empty = Env::new().var("MCP_TEST_OPTIONAL_KEY", "");
    let servers = McpServers::connect(&one(r#", "optional": true"#), &empty, &McpPolicy::default())
        .await
        .unwrap();
    assert_eq!(servers.names(), expected_names("up"));
    let error = McpServers::connect(&one(""), &empty, &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Var { var, problem: VarProblem::Empty, .. }
            if var == "MCP_TEST_OPTIONAL_KEY"),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::Invalid);
    let error = McpServers::connect(&one(""), &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Var {
                problem: VarProblem::Missing,
                ..
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_mistake_in_the_file_is_an_error_even_for_an_optional_server() {
    // Plain http to another machine without the opt-in never succeeds later.
    let config = common::config(
        r#"{"mcpServers": {"s": {"type": "http", "url": "http://search.example.com/mcp", "optional": true}}}"#,
    );
    let error = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::Url { .. }), "{error}");
}

#[tokio::test]
async fn an_optional_server_with_a_mistake_in_its_file_is_still_an_error() {
    let up = TestHttpServer::start(None).await;
    let policy = McpPolicy::default().allow_stdio(true);

    // A command that does not exist is a misspelling, not an outage.
    let config = common::config(
        r#"{"mcpServers": {"fs": {"command": "adam-mcp-test-no-such-command", "optional": true}}}"#,
    );
    let error = McpServers::connect(&config, &Env::new(), &policy)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Spawn { server, .. } if server == "fs"),
        "{error}"
    );

    // So is a server name the model cannot be shown (the file's loader refuses one, so the config is
    // made by hand here).
    let config = adam_agent_fs::McpConfig {
        servers: std::collections::BTreeMap::from([(
            "bad__name".to_owned(),
            adam_agent_fs::McpServer::Remote {
                kind: adam_agent_fs::RemoteKind::Http,
                url: up.url(),
                headers: Default::default(),
                tools: None,
                optional: true,
                files: false,
            },
        )]),
    };
    let error = McpServers::connect(&config, &Env::new(), &policy)
        .await
        .unwrap_err();
    assert!(matches!(&error, Error::Name { .. }), "{error}");
}
