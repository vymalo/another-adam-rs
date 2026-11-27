//! The bounds of a session on one of the root's subagents ([ADR 0054] decisions 2 and 3), made
//! from the folder and the deployment's settings alone.
//!
//! **One function, two callers.** [`Assembly::session_bounds`](crate::Assembly::session_bounds)
//! (the session tools' bounds, which a worker holds a `subagent_start` to) and
//! [`AgentDef::session_bounds`](crate::AgentDef::session_bounds) (the bounds a process that binds
//! no agent, the control plane of a split deployment, holds a person's `start.config` to) both
//! call [`bounds_of`], so the two cannot disagree. It reads the root's manifest, the roster
//! ([`SessionTarget`]) and three settings of the deployment: `MAX_PARALLEL_TOOL_CALLS`, the root's
//! effective `max_session_rounds` and nothing else. It is pure: no model, no I/O.
//!
//! What it does **not** know is the model's window (a fact of the model alias): `context_window`
//! is `None` for every caller, and `SessionBounds::admit` then applies only the floor. The
//! ceiling is enforced where the run is, by the minimum with the model's window
//! (`effective_window` in `adam-llm-agent`). So a control plane and a worker admit the same
//! `contextWindow`s, and the same ones are narrowed at run time.
//!
//! [ADR 0054]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0054-a-child-session-can-be-configured-within-what-its-folder-allows.md

use adam_agent_fs::{AgentManifest, Limits as FileLimits, Subagent};
use adam_llm_agent::{DEFAULT_MAX_PARALLEL, Limits, MAX_PARALLEL_LIMIT};
use adam_runtime::SessionBounds;

use crate::def::{SessionTarget, limits};

/// The number of tool calls of a model turn that run at once for an agent: the deployment's
/// (`MAX_PARALLEL_TOOL_CALLS`, or the loop's default) clamped to 1 to [`MAX_PARALLEL_LIMIT`],
/// **lowered** by the agent's own `limits.max_parallel`, which may never raise it.
///
/// The agent's build and the session bounds both use it, so a session can never be allowed more
/// than the agent runs at.
pub(crate) fn parallel_limit(deployment: Option<usize>, file: Option<usize>) -> usize {
    let allowed = deployment.map_or(DEFAULT_MAX_PARALLEL, |n| n.clamp(1, MAX_PARALLEL_LIMIT));
    match file {
        Some(asked) if asked > allowed => allowed,
        Some(asked) => asked.clamp(1, MAX_PARALLEL_LIMIT),
        None => allowed,
    }
}

/// The folder's `limits:` block of the root's local subagent that `target` names.
fn folder_limits<'a>(
    manifest: &'a AgentManifest,
    target: &SessionTarget,
) -> Option<&'a FileLimits> {
    manifest.subagents.iter().find_map(|sub| match sub {
        Subagent::Local(child) if !target.remote && child.name == target.name => {
            child.frontmatter.limits.as_ref()
        }
        _ => None,
    })
}

/// Whether `target` names a local subagent of the root's manifest (a remote one, or a name the
/// manifest does not have, runs under the loop's defaults).
fn is_local(manifest: &AgentManifest, target: &SessionTarget) -> bool {
    !target.remote
        && manifest
            .subagents
            .iter()
            .any(|sub| matches!(sub, Subagent::Local(child) if child.name == target.name))
}

/// The bounds of one subagent a session may be started on (ADR 0054 decision 2): each option as
/// the subagent's folder or the deployment resolves it.
///
/// * `max_turns`, `max_tool_calls` and `max_output_tokens` are the folder's (`0` for the cap
///   when it names none, which leaves it to the model: ADR 0064), else the loop's defaults.
/// * `context_window` is `None`: see the module.
/// * `max_parallel_tool_calls` is [`parallel_limit`]: the deployment's lowered by the folder's.
///   What a *round asks for* above this bound is what `SessionBounds::admit` refuses, naming the
///   option (decision 2); a *folder* above the deployment's is clamped (decision 3).
/// * `time_limit` is the round's own limit (`limits.max_minutes`, or the remote's wait), and
///   `max_rounds` the root's effective `max_session_rounds`.
///
/// A remote subagent's rounds are runs of an agent of its own, and the ADR refuses every option
/// but `time_limit`, `max_rounds` and `instructions` on one: the loop's defaults stand for the
/// rest.
pub(crate) fn bounds_of(
    manifest: &AgentManifest,
    target: &SessionTarget,
    deployment_parallel: Option<usize>,
    max_rounds: u32,
) -> SessionBounds {
    let (max_turns, max_tool_calls, max_output_tokens, parallel) = if is_local(manifest, target) {
        let file = folder_limits(manifest, target);
        let loop_limits: Limits = limits(file);
        (
            loop_limits.max_turns,
            loop_limits.max_tool_calls,
            file.and_then(|file| file.max_output_tokens).unwrap_or(0),
            parallel_limit(
                deployment_parallel,
                file.and_then(|file| file.max_parallel)
                    .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
            ),
        )
    } else {
        (
            Limits::default().max_turns,
            Limits::default().max_tool_calls,
            0,
            DEFAULT_MAX_PARALLEL,
        )
    };
    SessionBounds {
        max_turns,
        max_tool_calls,
        max_output_tokens,
        context_window: None,
        // `parallel_limit` never gives more than the loop's own limit (32), so nothing is lost
        // here. If a hand-made value ever could not be carried, the bound is that limit, the
        // most the loop runs at once, and never "no bound", which would widen.
        max_parallel_tool_calls: u32::try_from(parallel).unwrap_or(MAX_PARALLEL_LIMIT as u32),
        // A round with no limit (`None`) is `0` here, which means the same: no limit.
        time_limit: target.within.map_or(0, |within| {
            u32::try_from(within.as_secs() / 60).unwrap_or(u32::MAX)
        }),
        max_rounds,
    }
}
