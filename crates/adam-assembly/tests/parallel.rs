//! `limits.max_parallel` of an agent file and the deployment's setting (ADR 0049): the file may only
//! lower what the deployment allows, and a subagent has its own value.
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
fn a_file_never_raises_the_setting() {
    // 8 asks for more than the deployment allows: the deployment's 4 stands.
    let got = assemble(family("max_parallel: 8", "max_parallel: 32"), Some(4));
    assert_eq!(parallel_of(&got), (4, 4));
    // And at the deployment's 1 nothing can be raised.
    let got = assemble(family("max_parallel: 5", ""), Some(1));
    assert_eq!(parallel_of(&got), (1, 1));
}

#[test]
fn a_file_value_equal_to_the_setting_is_that_value() {
    let got = assemble(family("max_parallel: 4", ""), Some(4));
    assert_eq!(parallel_of(&got), (4, 4));
}

#[test]
fn a_subagent_tool_runs_alone() {
    use adam_llm_agent::{Tool, ToolAccess};
    let tool = adam_assembly::SubagentTool::new("helper", "coder/helper", "Does a piece of work.");
    assert_eq!(tool.access(&serde_json::json!({})), ToolAccess::Alone);
}
