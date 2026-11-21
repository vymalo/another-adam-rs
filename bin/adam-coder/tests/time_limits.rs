//! The time limit of each coder tool (ADR 0044 slice 10). The agent stops a call after
//! `Tool::time_limit`, 300 s for a tool that declares none, so a tool that has a bound of its own
//! (a command's timeout, the time to make an environment ready, a git operation) must declare it, from
//! the deployment's settings, or the agent would cut it short and tell the model its work was stopped.
//! A tool added later must decide too: this walks the whole list.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use adam_coder::{QUICK_TOOLS, coder_tools};
use adam_llm_agent::DEFAULT_TOOL_TIME_LIMIT;
use common::Fixture;

const MARGIN: u64 = 30;

async fn limits_with(environment_start: u64) -> BTreeMap<String, Option<Duration>> {
    let fx = Fixture::with(|settings| {
        settings.check_timeout = Duration::from_secs(700);
        settings.environment_start = Duration::from_secs(environment_start);
    })
    .await
    .offering_creation();
    coder_tools(&fx.env)
        .into_iter()
        .map(|tool| (tool.spec().name, tool.time_limit()))
        .collect()
}

#[tokio::test]
async fn every_tool_declares_its_own_bound_or_is_quick_by_construction() {
    let limits = limits_with(0).await;
    assert_eq!(limits.len(), 35, "the whole list: {:?}", limits.keys());
    for (name, limit) in &limits {
        let quick = QUICK_TOOLS.contains(&name.as_str());
        match limit {
            None => assert!(
                quick,
                "`{name}` has no bound of its own declared, so the agent would stop it at the default \
                 {DEFAULT_TOOL_TIME_LIMIT:?}: declare its limit in `own_limit` (tools/mod.rs) from the \
                 settings, or, if it is quick by construction, add it to QUICK_TOOLS with the reason"
            ),
            Some(_) => assert!(
                !quick,
                "`{name}` declares a limit and is on the quick list: it is one or the other"
            ),
        }
    }
    for name in QUICK_TOOLS {
        assert!(
            limits.contains_key(name),
            "`{name}` is on the quick list but is not a coder tool"
        );
    }
}

/// A command's own `timeout` is at most the deployment's command limit, so it always fits under the
/// time limit of the tool that runs it (the environment's start, the command and the margin): the loop
/// never cuts a command the deployment allowed.
#[tokio::test]
async fn a_commands_timeout_at_the_cap_fits_under_its_tools_limit() {
    for environment_start in [0, 90] {
        let limits = limits_with(environment_start).await;
        // The cap of `limits_with`: 700 s. The largest `timeout` a call may ask for.
        for command in ["run_command", "run", "run_checks"] {
            let limit = limits[command].unwrap();
            assert!(
                limit >= Duration::from_secs(700 + environment_start + MARGIN),
                "{command}: {limit:?}"
            );
        }
    }
}

#[tokio::test]
async fn the_limits_come_from_the_deployments_settings() {
    // A deployment whose environment takes up to 2100 s to be made ready (up 1200 + setup 900) and
    // whose commands may run 700 s.
    let limits = limits_with(2100).await;
    let secs = |name: &str| limits[name].unwrap().as_secs();
    for command in ["run_command", "run", "run_checks"] {
        assert_eq!(secs(command), 2100 + 700 + MARGIN, "{command}");
    }
    // A command that keeps running: the environment is made ready and the call waits for it up to
    // 120 s; stopping it, 60 s, after the environment is made ready for a command this worker does not
    // hold; listing asks for the session and reads records (the finders' deadline, 30 s).
    assert_eq!(secs("start_command"), 2100 + 120 + MARGIN);
    assert_eq!(secs("stop_command"), 2100 + 60 + MARGIN);
    assert_eq!(secs("list_commands"), 2100 + 30 + MARGIN);
    // A language server: the environment is made ready, and the call waits for the server at most
    // `wait` (120 s), `initialize` (60 s) and the probe and the sync (60 s).
    for tool in ["file_diagnostics", "find_definition", "find_references"] {
        assert_eq!(secs(tool), 2100 + 120 + 60 + 60 + MARGIN, "{tool}");
    }
    // The four read tools have a deadline of 30 s of their own, whatever the environment.
    for read in [
        "read_file",
        "list_files",
        "find_files",
        "search_files",
        "read_output",
    ] {
        assert_eq!(secs(read), 30 + MARGIN, "{read}");
    }
    // One network operation of git (30 minutes) and the local ones around it (5 minutes).
    for git in [
        "prepare_workspace",
        "commit_and_push",
        "publish_scratch",
        "create_repository",
    ] {
        assert_eq!(secs(git), 1800 + 300 + MARGIN, "{git}");
    }
    // Every tool that makes an environment or runs git is longer than the agent's default: none would
    // be cut at 300 s (the read tools are shorter, by their own deadline).
    for (name, limit) in &limits {
        if let Some(limit) = limit
            && !name.ends_with("_files")
            && name != "read_file"
            && name != "read_output"
        {
            assert!(*limit > DEFAULT_TOOL_TIME_LIMIT, "{name}: {limit:?}");
        }
    }
    // The same deployment with the coder's own container: the commands need no environment.
    let own = limits_with(0).await;
    assert_eq!(own["run_command"].unwrap().as_secs(), 700 + MARGIN);
}
