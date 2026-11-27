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

/// The bounds with the one fact only a worker has taken out: the names of the tools the subagent is
/// bound with (`tools` and `tools_complete`, ADR 0054 slice 3), which a control plane that binds no
/// agent does not know. Every number is equal; the tool names are the one thing that is not.
fn without_tools(bounds: &[(String, SessionBounds)]) -> Vec<(String, SessionBounds)> {
    bounds
        .iter()
        .map(|(name, bound)| {
            (
                name.clone(),
                SessionBounds {
                    tools: Vec::new(),
                    tools_complete: false,
                    ..bound.clone()
                },
            )
        })
        .collect()
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

/// A folder above the deployment's bound is **clamped** to it at startup — a folder is portable
/// across deployments, so a lower setting narrows the agent instead of refusing it to start (ADR
/// 0049, ADR 0054 decision 3) — and the clamped value is what `admit` then enforces: a request
/// above it is refused, naming the option (ADR 0054 decision 2).
#[test]
fn a_folder_above_the_deployment_is_clamped_and_admit_enforces_it() {
    let assembly = assemble(
        family("  max_parallel: 24"),
        SessionLimits::default(),
        Some(16),
    )
    .unwrap();
    let bound = bound_of(&assembly);
    assert_eq!(
        bound.max_parallel_tool_calls, 16,
        "the folder's 24 is clamped to the deployment's MAX_PARALLEL_TOOL_CALLS"
    );

    let asked = |parallel: u32| {
        SessionConfig::from_value(json!({ "maxParallelToolCalls": parallel })).expect("a config")
    };
    assert!(
        bound.admit(asked(16)).is_ok(),
        "the bound itself is admitted"
    );
    let why = bound
        .admit(asked(17))
        .expect_err("one call more than the clamped bound");
    assert_eq!(why.option(), Some("max_parallel_tool_calls"));
    assert!(why.message().contains("at most 16"), "{why}");
}

/// A folder at the loop's own limit is admitted and is the bound: `MAX_PARALLEL_TOOL_CALLS` is
/// clamped to 32 (`MAX_PARALLEL_LIMIT`), so a deployment that asks for more still runs at most 32
/// calls at once, and so is what a session may ask for.
#[test]
fn a_folder_at_the_loops_own_limit_is_the_bound() {
    let assembly = assemble(
        family("  max_parallel: 32"),
        SessionLimits::default(),
        Some(64),
    )
    .unwrap();
    let bound = bound_of(&assembly);
    assert_eq!(
        bound.max_parallel_tool_calls, 32,
        "the deployment's setting is clamped to the loop's 32 (MAX_PARALLEL_LIMIT)"
    );
    assert!(
        bound
            .admit(
                SessionConfig::from_value(json!({ "maxParallelToolCalls": 32 })).expect("a config")
            )
            .is_ok()
    );
    assert!(
        bound
            .admit(
                SessionConfig::from_value(json!({ "maxParallelToolCalls": 33 })).expect("a config")
            )
            .is_err(),
        "33 is over the loop's own limit"
    );
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

/// The A2A front's helpers carry the very bounds the session tools hold a `config` to, so a
/// person's `start.config` and the model's `config` cannot disagree (ADR 0054 decision 2); the
/// definition alone, which binds no model, knows none, and its front refuses a `start.config`.
#[cfg(feature = "sessions")]
#[test]
fn the_front_holds_a_start_config_to_the_bounds_of_the_tools() {
    let assembly = assemble(family("  max_turns: 7"), SessionLimits::default(), Some(3)).unwrap();
    let front = assembly.child_sessions().expect("the root has a helper");
    let helper = front.agents().first().expect("a helper");
    assert_eq!(helper.name, "reviewer");
    assert_eq!(helper.bounds.as_ref(), Some(&bound_of(&assembly)));
    assert_eq!(helper.bounds.as_ref().map(|b| b.max_turns), Some(7));
    let alone = family("  max_turns: 7").child_sessions().expect("a helper");
    assert_eq!(alone.agents()[0].bounds, None, "no model, no bounds");
}

/// ADR 0054, ANR-1093: a role that binds no agent (the control plane of a split deployment) gets
/// the bounds from the folder and the deployment's settings alone, and they are **the very ones a
/// worker binds**: one function makes both. Every folder, session limit and `MAX_PARALLEL_TOOL_CALLS`
/// of the table is asked of both, and they must agree helper by helper, field by field.
#[test]
fn a_definition_computes_the_bounds_a_worker_binds_without_binding() {
    let folders = [
        "",
        "  max_turns: 20\n  max_tool_calls: 40\n  max_output_tokens: 2048\n  max_minutes: 7",
        "  max_turns: 5\n  max_parallel: 2",
        // Above the deployment's `MAX_PARALLEL_TOOL_CALLS` below: clamped to it.
        "  max_parallel: 30\n  max_minutes: 0",
        "  max_parallel: 1",
    ];
    let deployments = [
        (SessionLimits::default(), None),
        (SessionLimits::new(16, 4, 9).unwrap(), Some(6)),
        (SessionLimits::new(8, 2, 2).unwrap(), Some(32)),
        (SessionLimits::new(100, 32, 50).unwrap(), Some(1)),
    ];
    for folder in folders {
        for (limits, parallel) in deployments {
            let worker = assemble(family(folder), limits, parallel).unwrap();
            let mut control_plane = family(folder).session_limits(limits);
            if let Some(n) = parallel {
                control_plane = control_plane.session_max_parallel(n);
            }
            let control_plane = control_plane.session_bounds();
            assert_eq!(
                control_plane,
                without_tools(worker.session_bounds()),
                "folder {folder:?}, {limits:?}, MAX_PARALLEL_TOOL_CALLS {parallel:?}"
            );
        }
    }
}

/// The same for a root with two helpers, so the roster and its order agree too, and for a root
/// with no helper, which has no bounds.
#[test]
fn the_roster_of_the_bounds_is_the_workers() {
    let root = def(&[
        (
            "agent/instructions.md",
            &instructions("name: coder", "You are the root."),
        ),
        (
            "agent/subagents/explorer.md",
            &instructions(
                "description: Explores.\nlimits:\n  max_minutes: 5",
                "You explore.",
            ),
        ),
        (
            "agent/subagents/reviewer.md",
            &instructions("description: Reviews a change.", "You review."),
        ),
    ])
    .session_limits(SessionLimits::new(16, 4, 9).unwrap());
    let worker = root
        .clone()
        .bind(tools(&[]))
        .unwrap()
        .max_parallel(6)
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    let root = root.session_max_parallel(6);
    assert_eq!(
        root.session_bounds(),
        without_tools(worker.session_bounds())
    );
    assert_eq!(root.session_bounds().len(), 2);

    let alone = def(&[(
        "agent/instructions.md",
        &instructions("name: solo", "You are alone."),
    )]);
    assert!(alone.session_bounds().is_empty());
}

/// The model's window is a fact of the model alias, which a role that binds no model does not
/// have: the control plane admits the window by the floor alone, **exactly as a worker's `admit`
/// does** (`context_window: None`); the ceiling is enforced where the run is, by the minimum with
/// the model's window.
#[test]
fn no_role_knows_the_models_window_when_it_admits() {
    let bounds = family("").session_bounds();
    assert_eq!(bounds[0].1.context_window, None);
}

/// A start the folder allows is admitted by the control plane's bounds, one above is refused naming
/// the option, with the same words a worker's would.
#[test]
fn the_bounds_of_a_definition_admit_and_refuse_like_a_workers() {
    let bound = family("  max_turns: 5\n  max_parallel: 2")
        .session_max_parallel(8)
        .session_bounds()
        .remove(0)
        .1;
    let asked = |value| SessionConfig::from_value(value).expect("a config");
    assert!(
        bound
            .admit(asked(json!({"maxTurns": 5, "maxParallelToolCalls": 2})))
            .is_ok()
    );
    let why = bound
        .admit(asked(json!({"maxTurns": 6})))
        .expect_err("one turn more than the folder allows");
    assert_eq!(why.option(), Some("max_turns"));
    assert!(why.message().contains("at most 5"), "{why}");
}

/// `AgentDef::session_max_parallel` is the deployment's value for the agents too: a worker that
/// binds the definition without `BoundDef::max_parallel` runs at it, and a `BoundDef::max_parallel`
/// wins, so the one value a binary gives both places cannot make the roles disagree.
#[test]
fn the_definitions_parallel_limit_is_the_workers_unless_it_says_another() {
    let definition = family("").session_max_parallel(4);
    let worker = definition
        .clone()
        .bind(tools(&[]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assert_eq!(bound_of(&worker).max_parallel_tool_calls, 4);
    assert_eq!(
        definition.session_bounds(),
        without_tools(worker.session_bounds())
    );
    let override_ = definition
        .bind(tools(&[]))
        .unwrap()
        .max_parallel(7)
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assert_eq!(bound_of(&override_).max_parallel_tool_calls, 7);
}

/// ADR 0054 slice 3: a worker knows the names of the tools its subagent is bound with, and
/// `admit` holds `tools` and `deny_tools` to them; a control plane, which binds no agent, knows
/// none and checks the shape alone.
#[test]
fn a_worker_bounds_hold_the_names_of_the_subagents_tools_and_a_definitions_do_not() {
    let definition = def(&[
        (
            "agent/instructions.md",
            &instructions("name: coder", "You are the root."),
        ),
        (
            "agent/subagents/reviewer.md",
            &instructions(
                "description: Reviews a change.\ntools: [read_file, write_file]",
                "You review.",
            ),
        ),
    ]);
    let worker = definition
        .clone()
        .bind(tools(&["read_file", "write_file", "delete_repository"]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    let bound = bound_of(&worker);
    assert_eq!(
        bound.tools,
        ["read_file", "write_file"],
        "only the subagent's own tools, not the root's"
    );
    assert!(bound.tools_complete, "no source adds a tool at run time");
    let asked = |value| SessionConfig::from_value(value).expect("a config");
    assert!(bound.admit(asked(json!({"tools": ["read_*"]}))).is_ok());
    let why = bound
        .admit(asked(json!({"denyTools": ["delete_repository"]})))
        .expect_err("a tool the subagent has not got");
    assert_eq!(why.option(), Some("deny_tools"));
    assert!(
        why.message()
            .contains("matches none of its tools: read_file, write_file"),
        "{why}"
    );

    let control_plane = definition.session_bounds().remove(0).1;
    assert!(control_plane.tools.is_empty() && !control_plane.tools_complete);
    assert!(
        control_plane
            .admit(asked(json!({"denyTools": ["delete_repository"]})))
            .is_ok(),
        "a control plane checks the shape only"
    );
}

/// A source that offers nothing: what matters is that the agent has one, as both shipped agents do.
struct Quiet;

#[async_trait::async_trait]
impl adam_llm_agent::ToolSource for Quiet {
    async fn specs(&self, _ctx: &adam_llm_agent::SourceCtx) -> Vec<adam_model::ToolSpec> {
        Vec::new()
    }

    async fn call(
        &self,
        _ctx: &adam_llm_agent::ToolCtx,
        _name: &str,
        _args: serde_json::Value,
    ) -> Option<Result<adam_llm_agent::ToolOutput, adam_llm_agent::ToolError>> {
        None
    }
}

/// ADR 0054 slice 3: an agent a source adds tools to at run time has a **known but incomplete** set
/// of names. A concrete `deny_tools` entry no tool has is refused (a typo is far likelier than a
/// tool a source adds), and a pattern or a `tools` entry is not checked: a source may add what it
/// selects, and an allow entry that selects nothing offers nothing.
#[test]
fn an_agent_with_a_source_has_an_incomplete_set_that_still_catches_a_deny_typo() {
    let definition = def(&[
        (
            "agent/instructions.md",
            &instructions("name: coder", "You are the root."),
        ),
        (
            "agent/subagents/reviewer.md",
            &instructions(
                "description: Reviews a change.\ntools: [read_file, write_file]",
                "You review.",
            ),
        ),
    ]);
    let worker = definition
        .bind(tools(&["read_file", "write_file"]))
        .unwrap()
        .tool_source(Quiet)
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    let bound = bound_of(&worker);
    assert_eq!(bound.tools, ["read_file", "write_file"]);
    assert!(!bound.tools_complete, "a source adds tools at run time");
    let asked = |value| SessionConfig::from_value(value).expect("a config");
    let why = bound
        .admit(asked(json!({"denyTools": ["write_fil"]})))
        .expect_err("a typo of a concrete name");
    assert_eq!(why.option(), Some("deny_tools"));
    assert!(
        why.message().contains("matches none of its tools: read_file, write_file; the agent may get more tools while it runs"),
        "{why}"
    );
    assert!(
        bound
            .admit(asked(
                json!({"tools": ["github__*", "later"], "denyTools": ["later_*", "write_file"]})
            ))
            .is_ok(),
        "a pattern and an allow entry are not checked on an incomplete set"
    );
}
