//! `limits.max_parallel` of an agent file and the deployment's setting (ADR 0049): the file may
//! only lower what the deployment allows (a larger value is clamped to it), and a subagent has its
//! own value.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::{AgentDef, Assembly};
use adam_model::MockModel;
use common::{def, instructions, tools};

/// A root and one child, each with the `limits:` line given (empty for none).
fn family(root_limits: &str, child_limits: &str) -> AgentDef {
    let line = |limits: &str| {
        if limits.is_empty() {
            String::new()
        } else {
            format!("\nlimits: {{ {limits} }}")
        }
    };
    def(&[
        (
            "agent/instructions.md",
            &instructions(&format!("name: coder{}", line(root_limits)), "Root."),
        ),
        (
            "agent/subagents/child.md",
            &instructions(
                &format!("description: Child.{}", line(child_limits)),
                "Child.",
            ),
        ),
    ])
}

fn assemble(def: AgentDef, process: Option<usize>) -> Assembly {
    let mut bound = def.bind(tools(&[])).unwrap();
    if let Some(n) = process {
        bound = bound.max_parallel(n);
    }
    bound.model(Arc::new(MockModel::new()), "m").unwrap()
}

fn parallel_of(assembly: &Assembly) -> (usize, usize) {
    (
        assembly.agents()[0].max_parallel(),
        assembly.agents()[1].max_parallel(),
    )
}

#[test]
fn with_nothing_said_every_agent_runs_ten_at_once() {
    assert_eq!(parallel_of(&assemble(family("", ""), None)), (10, 10));
}

#[test]
fn the_deployments_setting_reaches_every_agent() {
    assert_eq!(parallel_of(&assemble(family("", ""), Some(4))), (4, 4));
    assert_eq!(parallel_of(&assemble(family("", ""), Some(1))), (1, 1));
}

#[test]
fn a_file_may_lower_the_setting_for_its_agent_only() {
    let got = assemble(family("max_parallel: 2", ""), Some(6));
    assert_eq!(
        parallel_of(&got),
        (2, 6),
        "the child is not the root's file"
    );
    let got = assemble(family("", "max_parallel: 3"), None);
    assert_eq!(parallel_of(&got), (10, 3), "lowering the default of 10");
}

#[test]
fn a_file_above_the_setting_is_clamped_to_it() {
    // 8 asks for more than the deployment allows: clamped to it, not refused (ADR 0049, ADR 0054
    // decision 3 — a folder is portable across deployments, and a lower setting narrows it). The
    // root runs at the deployment's value; the child's file says nothing and gets the same.
    for (limits, deployment, clamped) in [
        ("max_parallel: 8", 4, 4),
        ("max_parallel: 32", 4, 4),
        ("max_parallel: 5", 1, 1),
    ] {
        let got = assemble(family(limits, ""), Some(deployment));
        assert_eq!(
            parallel_of(&got),
            (clamped, deployment),
            "{limits} under a deployment of {deployment}"
        );
    }
}

#[test]
fn a_file_value_equal_to_the_setting_is_that_value() {
    let got = assemble(family("max_parallel: 4", ""), Some(4));
    assert_eq!(parallel_of(&got), (4, 4));
}

/// A parent delegates with the session tools (ADR 0089 slice C4: there is no tool named after a
/// subagent), and each of them runs alone, `subagent_start` waiting or not: a start that waits
/// parks the run on the child, as the subagent's own tool did.
#[test]
fn the_tools_that_start_a_subagent_run_alone() {
    use adam_llm_agent::{SESSION_TOOLS, SessionTools, ToolAccess};
    let assembly = assemble(family("", ""), None);
    let root = &assembly.info()[0];
    assert!(!root.tools.iter().any(|tool| tool == "child"));
    assert!(root.tools.ends_with(&SESSION_TOOLS.map(str::to_owned)));
    for tool in SessionTools::new(Vec::new()).tools() {
        for args in [
            serde_json::json!({}),
            serde_json::json!({"agent": "child", "description": "d", "prompt": "p", "wait": true}),
        ] {
            assert_eq!(
                tool.access(&args),
                ToolAccess::Alone,
                "{}",
                tool.spec().name
            );
        }
    }
}

/// How a run waits on a model that refused it (ADR 0067) is the deployment's, for every agent: the
/// root and its subagents alike; without it, the loop's default.
#[test]
fn the_deployments_model_parking_reaches_every_agent() {
    use adam_llm_agent::ModelParking;
    use std::time::Duration;
    let parking = ModelParking::new(Duration::from_secs(3600)).when_unreachable(true);
    let assembly = family("", "")
        .bind(tools(&[]))
        .unwrap()
        .model_parking(parking)
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assert!(assembly.agents().len() >= 2);
    for agent in assembly.agents() {
        assert_eq!(agent.model_parking(), parking);
    }
    let plain = assemble(family("", ""), None);
    for agent in plain.agents() {
        assert_eq!(agent.model_parking(), ModelParking::default());
    }
}
