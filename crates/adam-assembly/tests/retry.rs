//! `retry:` of an agent file (ADR 0061, decision 6): a policy per tool, checked against the agent's
//! tools at bind, each agent's its own, and the policy the run retries the tool under.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use adam_assembly::Error;
use adam_core::{MemoryStore, RunStatus};
use adam_error::ErrorClass;
use adam_llm_agent::{FnTool, ToolError, ToolSet, user_message};
use adam_model::{MockModel, ToolCall};
use adam_runtime::{RetryPolicy, Runtime};
use common::{def, instructions, spawn_worker, tools};
use serde_json::json;

const INSTRUCTIONS: &str = "agent/instructions.md";

#[test]
fn retry_of_a_tool_the_agent_lacks_fails_at_bind_with_a_suggestion() {
    let error = def(&[(
        INSTRUCTIONS,
        &instructions("name: coder\nretry: { web_serch: { attempts: 3 } }", "Hi."),
    )])
    .bind(tools(&["web_search", "run_checks"]))
    .unwrap_err();
    let Error::UnknownRetryTool {
        tool, suggestion, ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(
        (tool.as_str(), suggestion.as_deref()),
        ("web_serch", Some("web_search"))
    );
    assert!(
        error
            .to_string()
            .contains("`retry` names `web_serch`, which is not one of the agent's tools"),
        "{error}"
    );
    // A tool registered but not given to this agent is not one of its tools either.
    let error = def(&[(
        INSTRUCTIONS,
        &instructions(
            "name: coder\ntools: [run_checks]\nretry: { web_search: { attempts: 3 } }",
            "Hi.",
        ),
    )])
    .bind(tools(&["web_search", "run_checks"]))
    .unwrap_err();
    assert!(matches!(error, Error::UnknownRetryTool { .. }), "{error}");
}

#[test]
fn each_agent_has_its_own_retry_and_none_inherits() {
    let assembly = def(&[
        (
            INSTRUCTIONS,
            &instructions(
                "name: coder\nretry: { web_search: { attempts: 3, backoff: 2s } }",
                "Root.",
            ),
        ),
        (
            "agent/subagents/child.md",
            &instructions(
                "description: Child.\ntools: [web_search]\nretry: { web_search: { attempts: 7 } }",
                "Child.",
            ),
        ),
        (
            "agent/subagents/plain.md",
            &instructions("description: Plain.\ntools: [web_search]", "Plain."),
        ),
    ])
    .bind(tools(&["web_search"]))
    .unwrap()
    .model(Arc::new(MockModel::new()), "m")
    .unwrap();
    let retry_of = |name: &str| {
        let info = assembly.info().iter().find(|i| i.name == name).unwrap();
        info.retry
            .iter()
            .map(|(tool, def)| (tool.clone(), def.attempts))
            .collect::<Vec<_>>()
    };
    assert_eq!(retry_of("coder"), [("web_search".to_owned(), Some(3))]);
    assert_eq!(
        retry_of("coder/child"),
        [("web_search".to_owned(), Some(7))]
    );
    assert_eq!(
        retry_of("coder/plain"),
        [],
        "a subagent does not inherit the root's"
    );
}

/// A tool that always fails with a rate limit, with a policy of its own the file's must win over.
fn busy(calls: &Arc<AtomicUsize>) -> ToolSet {
    let calls = calls.clone();
    ToolSet::new().tool(
        FnTool::raw(
            "busy",
            "Busy.",
            json!({"type": "object"}),
            move |_ctx, _args| {
                calls.fetch_add(1, SeqCst);
                async {
                    Err::<String, _>(ToolError::retryable("slow down", ErrorClass::RateLimited))
                }
            },
        )
        .with_retry_policy(RetryPolicy::new(9)),
    )
}

/// Runs `coder` with `retry_line` against the always-busy tool and returns the failure and the calls.
async fn run_busy(retry_line: &str, model_calls: usize) -> (String, usize) {
    let calls = Arc::new(AtomicUsize::new(0));
    let model = Arc::new(MockModel::new());
    for _ in 0..model_calls {
        model.push_tool_calls(vec![ToolCall {
            id: "c1".into(),
            name: "busy".into(),
            arguments: json!({}),
            ..Default::default()
        }]);
    }
    let assembly = def(&[(
        INSTRUCTIONS,
        &instructions(&format!("name: coder\n{retry_line}"), "Hi."),
    )])
    .bind(busy(&calls))
    .unwrap()
    .model(model, "m")
    .unwrap();
    let rt = assembly
        .register(Runtime::builder(Arc::new(MemoryStore::new())))
        .poll_interval(Duration::from_millis(20))
        .retry(RetryPolicy::new(9).with_backoff(Duration::from_millis(5), Duration::from_millis(5)))
        .build();
    let worker = spawn_worker(&rt);
    let run = rt.start("coder", user_message("go"), None).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let view = loop {
        let view = rt.view(run).await.unwrap().unwrap();
        if view.status == RunStatus::Failed {
            break view;
        }
        assert!(Instant::now() < deadline, "timed out: {view:#?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    worker.stop().await;
    (view.error.unwrap(), calls.load(SeqCst))
}

#[tokio::test]
async fn the_files_policy_is_the_one_the_run_retries_the_tool_under() {
    let (error, calls) = run_busy(
        "retry: { busy: { attempts: 2, backoff: 10ms, max_backoff: 10ms, on: [rate-limited] } }",
        2,
    )
    .await;
    assert!(
        error.contains("gave up after 2 attempts of `tool:busy`"),
        "the file's 2, not the tool's 9 nor the runtime's: {error}"
    );
    assert_eq!(calls, 2);
    // `on` is the file's too: a policy that retries only transient errors fails a rate limit at once.
    let (error, calls) = run_busy(
        "retry: { busy: { attempts: 5, backoff: 10ms, on: transient } }",
        1,
    )
    .await;
    assert!(
        error.contains("the policy of `tool:busy` does not retry rate-limited"),
        "{error}"
    );
    assert_eq!(calls, 1);
}

/// An MCP server's tool, connected before binding, can be given a policy by its name, and a
/// tool of no server is still refused.
#[test]
fn retry_names_an_mcp_servers_tool_by_its_full_name() {
    let files = [
        (
            INSTRUCTIONS,
            instructions(
                "name: coder\nretry: { linear__list: { attempts: 4, on: [rate-limited] } }",
                "Hi.",
            ),
        ),
        (
            "agent/mcp.json",
            r#"{"mcpServers": {"linear": {"type": "http", "url": "https://mcp.example.com/mcp"}}}"#
                .to_owned(),
        ),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    let assembly = def(&files)
        .mcp_tools("coder", tools(&["linear__list"]))
        .bind(tools(&[]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assert_eq!(assembly.info()[0].retry[0].0, "linear__list");
    let error = def(&files)
        .mcp_tools("coder", tools(&["linear__get"]))
        .bind(tools(&[]))
        .unwrap_err();
    assert!(matches!(error, Error::UnknownRetryTool { .. }), "{error}");
}

/// The folder's class names are the runtime's: a policy names the same classes in a file as in
/// code.
#[test]
fn the_folders_class_names_are_the_runtimes() {
    assert_eq!(
        adam_agent_fs::RETRY_CLASS_NAMES.to_vec(),
        adam_runtime::RetryClasses::ALL.names()
    );
    assert_eq!(
        adam_agent_fs::MAX_RETRY_BACKOFF,
        Duration::from_secs(3_600),
        "decision 6: at most an hour"
    );
    let default = RetryPolicy::default();
    assert_eq!(
        (
            adam_agent_fs::DEFAULT_RETRY_BACKOFF,
            adam_agent_fs::DEFAULT_RETRY_MAX_BACKOFF
        ),
        (default.initial_backoff, default.max_backoff),
        "what a file leaves out is the runtime's default"
    );
}

/// A subagent's tool is one of the parent's tools, and can be given a policy by its name.
#[test]
fn retry_names_a_subagent_tool() {
    let assembly = def(&[
        (
            INSTRUCTIONS,
            &instructions(
                "name: coder\nretry: { code-reviewer: { attempts: 2 } }",
                "Hi.",
            ),
        ),
        (
            "agent/subagents/code-reviewer.md",
            &instructions("description: Reviews.", "Review."),
        ),
    ])
    .bind(tools(&[]))
    .unwrap()
    .model(Arc::new(MockModel::new()), "m")
    .unwrap();
    assert_eq!(assembly.info()[0].retry[0].0, "code-reviewer");
}

/// A declared MCP server that gave no tools (an optional one that is down) does not stop the
/// agent from binding: its `retry` entry waits for it.
#[test]
fn a_retry_for_a_server_that_gave_no_tools_is_skipped() {
    let files = [
        (
            INSTRUCTIONS,
            instructions("name: coder\nretry: { search__web: { attempts: 3 } }", "Hi."),
        ),
        (
            "agent/mcp.json",
            r#"{"mcpServers": {"search": {"type": "http", "url": "https://search.example.com/mcp"}}}"#
                .to_owned(),
        ),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    let assembly = def(&files)
        .mcp_tools("coder", ToolSet::new())
        .bind(tools(&[]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assert!(assembly.info()[0].retry.is_empty());
}

/// A manifest built in code is checked at bind: an entry the loader would refuse is no policy.
#[test]
fn a_retry_built_in_code_is_checked_at_bind() {
    for (def_, reason) in [
        (adam_agent_fs::RetryDef::default(), "`attempts`"),
        (
            adam_agent_fs::RetryDef {
                attempts: Some(2),
                max_backoff: Some("10h".into()),
                ..Default::default()
            },
            "over 1h",
        ),
        (
            adam_agent_fs::RetryDef {
                attempts: Some(2),
                backoff: Some("1m".into()),
                max_backoff: Some("1s".into()),
                ..Default::default()
            },
            "shorter than `backoff`",
        ),
        (
            adam_agent_fs::RetryDef {
                attempts: Some(2),
                on: Some(vec!["invalid".into()]),
                ..Default::default()
            },
            "`invalid` is not a retryable class",
        ),
    ] {
        let (_dir, mut agents) =
            common::manifests(&[(INSTRUCTIONS, &instructions("name: coder", "Hi."))]);
        let mut manifest = agents.remove(0);
        manifest.frontmatter.retry.insert("web_search".into(), def_);
        let error = adam_assembly::AgentDef::from_manifest(manifest)
            .unwrap()
            .bind(tools(&["web_search"]))
            .unwrap_err();
        let Error::InvalidRetry { reason: said, .. } = &error else {
            panic!("{error}");
        };
        assert!(said.contains(reason), "{said}");
    }
}
