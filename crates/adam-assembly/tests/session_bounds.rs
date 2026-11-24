//! The bounds of a session on one of a root's subagents ([ADR 0054], slice 1 part 2): what the
//! subagent's folder declares, what the deployment allows, and the one-way rule between them. The
//! bounds are computed once, at startup, from the same files and settings the agents are built
//! from, and handed to the five session tools, where `SessionBounds::admit` holds every
//! `subagent_start` request to them.
//!
//! [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_assembly::{AgentDef, Assembly, Error};
use adam_model::MockModel;
use adam_runtime::{SessionBounds, SessionConfig, SessionLimits};
use common::{def, instructions, tools};
use serde_json::json;

/// A root and one `reviewer` whose file holds `limits` (the YAML lines of a `limits:` block, or
/// nothing).
fn family(limits: &str) -> AgentDef {
    let block = if limits.is_empty() {
        String::new()
    } else {
        format!("\nlimits:\n{limits}")
    };
    def(&[
        (
            "agent/instructions.md",
            &instructions("name: coder", "You are the root."),
        ),
        (
            "agent/subagents/reviewer.md",
            &instructions(
                &format!("description: Reviews a change.{block}"),
                "You review.",
            ),
        ),
    ])
}

/// The definition, assembled with the deployment's session limits and `MAX_PARALLEL_TOOL_CALLS`.
fn assemble(
    definition: AgentDef,
    limits: SessionLimits,
    parallel: Option<usize>,
) -> Result<Assembly, Error> {
    let mut bound = definition.session_limits(limits).bind(tools(&[]))?;
    if let Some(n) = parallel {
        bound = bound.max_parallel(n);
    }
    bound.model(Arc::new(MockModel::new()), "m")
}

/// The one bound the definition has: `reviewer`'s.
fn bound_of(assembly: &Assembly) -> SessionBounds {
    let all = assembly.session_bounds();
    assert_eq!(all.len(), 1, "one helper: {all:?}");
    assert_eq!(all[0].0, "reviewer");
    all[0].1.clone()
}

/// Every field, in the order of `SessionBounds`: turns, tool calls, output tokens, window, calls
/// at once, minutes and rounds.
fn fields(bound: &SessionBounds) -> (u32, u32, u32, Option<u32>, u32, u32, u32) {
    (
        bound.max_turns,
        bound.max_tool_calls,
        bound.max_output_tokens,
        bound.context_window,
        bound.max_parallel_tool_calls,
        bound.time_limit,
        bound.max_rounds,
    )
}

/// The loop's defaults as a folder with no `limits:` leaves them: 50 turns, 200 tool calls, the
/// output cap left to the model (`0`), no window known, 10 calls at once, 30 minutes, and the
/// deployment's rounds.
#[test]
fn with_nothing_said_the_loop_defaults_are_the_bounds() {
    let assembly = assemble(family(""), SessionLimits::default(), None).unwrap();
    assert_eq!(
        fields(&bound_of(&assembly)),
        (50, 200, 0, None, 10, 30, 20),
        "nothing is invented and the cap stays the model's"
    );
}

/// The deployment's own settings are the bounds where the folder says nothing: the session limits
/// of the root (`SESSION_MAX_ROUNDS`) and `MAX_PARALLEL_TOOL_CALLS`.
#[test]
fn the_deployments_settings_are_the_bounds_the_folder_narrows() {
    let assembly = assemble(family(""), SessionLimits::new(16, 4, 9).unwrap(), Some(6)).unwrap();
    let bound = bound_of(&assembly);
    assert_eq!(
        bound.max_rounds, 9,
        "the root's effective max_session_rounds"
    );
    assert_eq!(bound.max_parallel_tool_calls, 6, "MAX_PARALLEL_TOOL_CALLS");
}

/// A folder that names a value of its own sets the bound where the deployment sets none, and its
/// time limit is the one its rounds already run under.
#[test]
fn a_folder_alone_sets_the_bounds_the_deployment_does_not() {
    let assembly = assemble(
        family(
            "  max_turns: 20\n  max_tool_calls: 40\n  max_output_tokens: 2048\n  max_minutes: 7",
        ),
        SessionLimits::default(),
        None,
    )
    .unwrap();
    assert_eq!(
        fields(&bound_of(&assembly)),
        (20, 40, 2048, None, 10, 7, 20),
        "the folder's four, the loop's for the rest"
    );
}

/// A folder below the deployment's bound wins, and it is the folder's value that `admit` then
/// enforces: a request the folder allows is admitted, one above it is refused by name.
#[test]
fn a_folder_lower_than_the_deployment_is_what_admit_enforces() {
    let assembly = assemble(
        family("  max_turns: 5\n  max_parallel: 2"),
        SessionLimits::new(16, 4, 12).unwrap(),
        Some(8),
    )
    .unwrap();
    let bound = bound_of(&assembly);
    assert_eq!(bound.max_turns, 5, "the folder's, not the deployment's 8");
    assert_eq!(bound.max_parallel_tool_calls, 2);

    let asked = |turns, parallel| {
        SessionConfig::from_value(json!({
            "maxTurns": turns,
            "maxParallelToolCalls": parallel,
        }))
        .expect("a config of the two options")
    };
    assert!(
        bound.admit(asked(5, 2)).is_ok(),
        "what the folder allows is admitted"
    );
    let why = bound
        .admit(asked(6, 2))
        .expect_err("one turn more than the folder allows");
    assert_eq!(why.option(), Some("max_turns"));
    assert!(why.message().contains("at most 5"), "{why}");
    let why = bound
        .admit(asked(5, 3))
        .expect_err("one call more than the folder allows");
    assert_eq!(why.option(), Some("max_parallel_tool_calls"));
    assert!(why.message().contains("at most 2"), "{why}");
}

/// A folder above the deployment's bound is **refused** at startup, naming the option and what
/// the deployment allows: a bound only narrows, so nothing is silently clamped to it.
#[test]
fn a_folder_above_the_deployment_is_refused_naming_the_option() {
    let why = assemble(
        family("  max_parallel: 24"),
        SessionLimits::default(),
        Some(16),
    )
    .expect_err("the folder asks for more tool calls at once than the deployment allows");
    let text = why.to_string();
    assert!(text.contains("limits.max_parallel"), "{text}");
    assert!(text.contains("24"), "the value the folder asked: {text}");
    assert!(
        text.contains("16"),
        "the value the deployment allows: {text}"
    );
    assert!(text.contains("MAX_PARALLEL_TOOL_CALLS"), "{text}");

    // The positive control: the same file at the deployment's own value is admitted, so the
    // refusal is the value that is above it and not the file.
    let assembly = assemble(
        family("  max_parallel: 16"),
        SessionLimits::default(),
        Some(16),
    )
    .unwrap();
    assert_eq!(bound_of(&assembly).max_parallel_tool_calls, 16);
}

/// The bounds of every subagent of a real definition, in the order of its folder, from the same
/// one list the root's session tools are made with: a local helper's time limit is its
/// `max_minutes`, and a helper that says nothing gets the loop's defaults for the values of its
/// own and the deployment's for the rest.
#[test]
fn every_subagents_bounds_reach_its_session_tools() {
    let root = def(&[
        (
            "agent/instructions.md",
            &instructions("name: coder", "You are the root."),
        ),
        (
            "agent/subagents/explorer.md",
            &instructions(
                "description: Looks around.\nlimits:\n  max_minutes: 5",
                "You explore.",
            ),
        ),
        (
            "agent/subagents/reviewer.md",
            &instructions("description: Reviews a change.", "You review."),
        ),
    ])
    .session_limits(SessionLimits::new(16, 4, 9).unwrap());
    let assembly = root
        .bind(tools(&[]))
        .unwrap()
        .max_parallel(6)
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    let all = assembly.session_bounds();
    let names: Vec<&str> = all.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["explorer", "reviewer"], "the folder's order");
    assert_eq!(
        fields(&all[0].1),
        (50, 200, 0, None, 6, 5, 9),
        "the explorer's `max_minutes`, the deployment's parallelism and rounds"
    );
    assert_eq!(
        fields(&all[1].1),
        (50, 200, 0, None, 6, 30, 9),
        "the reviewer says nothing: 30 minutes, its parallelism from the deployment"
    );
}
