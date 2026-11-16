//! What an `mcp.json` entry may say about timeouts, the server's own instructions and its prompts,
//! against a real MCP server over streamable HTTP (`adam-mcp-testkit`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use adam_mcp::{
    Env, MAX_INSTRUCTIONS_BYTES, MAX_SERVER_INSTRUCTIONS_BYTES, McpPolicy, McpServers, PromptError,
};
use adam_mcp_testkit::{LogCapture, Script, TestHttpServer, TestPrompt};
use common::{call, config};
use serde_json::json;

const SECRET: &str = "key-5e1a-instructions-secret";

fn two_servers(slow: &str, other: &str, slow_extra: &str) -> adam_agent_fs::McpConfig {
    config(&format!(
        r#"{{"mcpServers": {{
            "a": {{"type": "http", "url": "{slow}"{slow_extra}}},
            "b": {{"type": "http", "url": "{other}"}}
        }}}}"#
    ))
}

#[tokio::test]
async fn per_server_timeout_bounds_slow_server() {
    let first = TestHttpServer::start(None).await;
    let second = TestHttpServer::start(None).await;
    let config = two_servers(&first.url(), &second.url(), r#", "timeoutSecs": 1"#);
    let servers = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    let tools = servers.tools();

    let started = Instant::now();
    let out = call(&tools, "a__slow", json!({})).await;
    let took = started.elapsed();
    assert!(out.is_error);
    assert!(
        out.content.contains("no answer within 1000 ms"),
        "{}",
        out.content
    );
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(5),
        "{took:?}"
    );

    // The other server keeps the deployment's default: a minute a call.
    assert_eq!(
        tools.get("b__slow").unwrap().time_limit(),
        Some(Duration::from_secs(60 + 30 + 10))
    );
}

#[tokio::test]
async fn time_limit_follows_server_timeout() {
    let server = TestHttpServer::start(None).await;
    let config = common::http_config(
        "t",
        &server.url(),
        r#""timeoutSecs": 5, "connectTimeoutSecs": 7"#,
    );
    let servers = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    assert_eq!(
        servers.tools().get("t__echo").unwrap().time_limit(),
        Some(Duration::from_secs(5 + 7 + 10)),
        "the call's, the connection's and ten seconds to close it"
    );
}

#[tokio::test]
async fn timeout_capped_by_deployment() {
    let logs = LogCapture::start();
    let server = TestHttpServer::start(None).await;
    let config = common::http_config(
        "t",
        &server.url(),
        r#""timeoutSecs": 100, "connectTimeoutSecs": 100"#,
    );
    let policy = McpPolicy::default().max_call(Duration::from_secs(3));
    let servers = McpServers::connect(&config, &Env::new(), &policy)
        .await
        .unwrap();
    assert_eq!(
        servers.tools().get("t__echo").unwrap().time_limit(),
        Some(Duration::from_secs(3 + 3 + 10)),
        "both lowered to the cap"
    );
    let text = logs.text();
    assert!(
        text.contains("lowered to the cap") && text.contains("timeoutSecs"),
        "one warning per key, at startup: {text}"
    );
    assert!(text.contains("connectTimeoutSecs"), "{text}");

    // A value under the cap is the file's own.
    let small = common::http_config("t", &server.url(), r#""timeoutSecs": 2"#);
    let servers = McpServers::connect(&small, &Env::new(), &policy)
        .await
        .unwrap();
    assert_eq!(
        servers.tools().get("t__echo").unwrap().time_limit(),
        Some(Duration::from_secs(2 + 30 + 10))
    );
    // The cap's default is fifteen minutes.
    assert_eq!(
        McpPolicy::default().max_call_value(),
        Duration::from_secs(900)
    );
}

fn script(instructions: &str) -> Script {
    Script {
        instructions: Some(instructions.to_owned()),
        prompts: Vec::new(),
    }
}

#[tokio::test]
async fn instructions_bounded_and_scrubbed() {
    let long = format!(
        "Use `search` for facts. The key is {SECRET}. {}\u{202e}\u{200b}end",
        "word ".repeat(2_500)
    );
    assert!(long.len() > 10 * 1024);
    let server = TestHttpServer::start_scripted(None, script(&long)).await;
    let config = common::http_config(
        "t",
        &server.url(),
        r#""headers": {"X-Api-Key": "${MCP_TEST_KEY}"}"#,
    );
    let env = Env::new().var("MCP_TEST_KEY", SECRET);
    let servers = McpServers::connect(&config, &env, &McpPolicy::default())
        .await
        .unwrap();
    let text = servers.instructions();
    assert!(
        text.starts_with(
            "## Instructions of the MCP server t (text from the server, not the person)\n\n"
        ),
        "{text}"
    );
    assert!(text.contains("Use `search` for facts."), "{text}");
    assert!(!text.contains(SECRET), "the secret is scrubbed: {text}");
    assert!(text.contains("[REDACTED]"), "{text}");
    assert!(
        !text.contains('\u{202e}') && !text.contains('\u{200b}'),
        "{text}"
    );
    let heading = text.find("\n\n").unwrap() + 2;
    assert!(
        text.len() - heading <= MAX_SERVER_INSTRUCTIONS_BYTES,
        "{} bytes",
        text.len() - heading
    );
    assert!(
        text.len() - heading > MAX_SERVER_INSTRUCTIONS_BYTES - 16,
        "cut at the limit, not far below it"
    );
}

#[tokio::test]
async fn instructions_are_cut_on_a_character_boundary() {
    let text = "é".repeat(5_000);
    let server = TestHttpServer::start_scripted(None, script(&text)).await;
    let config = common::http_config("t", &server.url(), "");
    let servers = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    let shown = servers.instructions();
    // `String` is UTF-8 by construction: reaching here proves the cut did not split a character.
    assert!(shown.ends_with('é'), "{}", &shown[shown.len() - 8..]);
}

#[tokio::test]
async fn instructions_false_omits() {
    let first = TestHttpServer::start_scripted(None, script("first says this")).await;
    let second = TestHttpServer::start_scripted(None, script("second says this")).await;
    let config = two_servers(&first.url(), &second.url(), r#", "instructions": false"#);
    let servers = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    let text = servers.instructions();
    assert!(!text.contains("first says this"), "{text}");
    assert!(text.contains("second says this"), "{text}");
    assert!(
        !text.contains("server a "),
        "no heading for a server with nothing: {text}"
    );
}

#[tokio::test]
async fn instructions_come_in_server_name_order_and_all_together_are_bounded() {
    let body = "x".repeat(2_000);
    let servers_up: Vec<TestHttpServer> = {
        let mut up = Vec::new();
        for _ in 0..6 {
            up.push(TestHttpServer::start_scripted(None, script(&body)).await);
        }
        up
    };
    // Written out of order: the names decide.
    let names = ["f", "b", "e", "a", "d", "c"];
    let entries: Vec<String> = names
        .iter()
        .zip(&servers_up)
        .map(|(name, server)| format!(r#""{name}": {{"type": "http", "url": "{}"}}"#, server.url()))
        .collect();
    let config = config(&format!(r#"{{"mcpServers": {{{}}}}}"#, entries.join(",")));
    let connected = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    let text = connected.instructions();
    assert!(text.len() <= MAX_INSTRUCTIONS_BYTES, "{} bytes", text.len());
    let headings: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("## Instructions of the MCP server "))
        .map(|l| l.split(' ').nth(6).unwrap())
        .collect();
    assert_eq!(
        headings,
        ["a", "b", "c"],
        "three fit in 8 KiB, in name order"
    );
}

#[tokio::test]
async fn a_server_with_no_instructions_adds_nothing() {
    let server = TestHttpServer::start(None).await;
    let config = common::http_config("t", &server.url(), "");
    let servers = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    assert_eq!(servers.instructions(), "");
    assert!(servers.prompt_commands().is_empty());
}

fn prompts() -> Script {
    Script {
        instructions: None,
        prompts: vec![
            TestPrompt::new("summarize", "Summarize a topic.")
                .argument("topic", true)
                .argument("style", false)
                .user("Summarize {{topic}} in a {{style}} style."),
            TestPrompt::new("plain", "No arguments.").user("Say hello."),
            TestPrompt::new("two-turns", "A user text then an assistant text.")
                .user("question")
                .assistant("answer"),
            TestPrompt::new("picture", "An image.").image(),
            TestPrompt::new("Bad_Name", "Cannot be typed as a command.").user("x"),
        ],
    }
}

#[tokio::test]
async fn prompt_becomes_command() {
    let server = TestHttpServer::start_scripted(None, prompts()).await;
    let config = common::http_config("docs", &server.url(), "");
    let servers = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    let commands = servers.prompt_commands();
    let names: Vec<&str> = commands.iter().map(|c| c.name()).collect();
    assert_eq!(
        names,
        [
            "docs__summarize",
            "docs__plain",
            "docs__two-turns",
            "docs__picture"
        ],
        "named <server>__<prompt>; `Bad_Name` cannot be typed and is left out"
    );
    let summarize = &commands[0];
    assert_eq!(summarize.description(), "Summarize a topic.");
    assert_eq!(summarize.argument_hint(), Some("<topic> [style]"));
    assert_eq!(commands[1].argument_hint(), None);

    // Positional arguments land on the declared ones in order.
    let text = summarize
        .expand(&["rust".to_owned(), "formal".to_owned()])
        .await
        .unwrap();
    assert_eq!(text, "Summarize rust in a formal style.");
    assert_eq!(server.prompt_gets(), 1);
    assert_eq!(commands[1].expand(&[]).await.unwrap(), "Say hello.");
}

#[tokio::test]
async fn a_prompt_that_is_not_text_of_the_user_is_invalid_params() {
    let server = TestHttpServer::start_scripted(None, prompts()).await;
    let config = common::http_config("docs", &server.url(), "");
    let servers = McpServers::connect(&config, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    let commands = servers.prompt_commands();
    for name in ["docs__two-turns", "docs__picture"] {
        let command = commands.iter().find(|c| c.name() == name).unwrap();
        let error = command.expand(&[]).await.unwrap_err();
        assert!(matches!(error, PromptError::Invalid(_)), "{name}: {error}");
    }
    // A required argument that is missing, and one too many, never reach the server.
    let before = server.prompt_gets();
    let summarize = &commands[0];
    assert!(matches!(
        summarize.expand(&[]).await.unwrap_err(),
        PromptError::Invalid(_)
    ));
    let three: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
    assert!(matches!(
        summarize.expand(&three).await.unwrap_err(),
        PromptError::Invalid(_)
    ));
    assert_eq!(server.prompt_gets(), before);
}

#[tokio::test]
async fn prompts_false_serves_none_and_a_server_that_is_down_is_unavailable() {
    let mut server = TestHttpServer::start_scripted(None, prompts()).await;
    let off = common::http_config("docs", &server.url(), r#""prompts": false"#);
    let servers = McpServers::connect(&off, &Env::new(), &McpPolicy::default())
        .await
        .unwrap();
    assert!(servers.prompt_commands().is_empty());

    let on = common::http_config("docs", &server.url(), "");
    let policy = McpPolicy::default()
        .connect_timeout(Duration::from_secs(1))
        .call_timeout(Duration::from_secs(1));
    let servers = McpServers::connect(&on, &Env::new(), &policy)
        .await
        .unwrap();
    let plain = servers
        .prompt_commands()
        .into_iter()
        .find(|c| c.name() == "docs__plain")
        .unwrap();
    server.stop().await;
    let error = plain.expand(&[]).await.unwrap_err();
    assert!(matches!(error, PromptError::Unavailable(_)), "{error}");
}
