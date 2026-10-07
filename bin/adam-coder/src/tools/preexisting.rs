//! Does a failing check also fail on the base? ([ADR 0026](../../../../docs/decisions/0026-a-failure-the-base-has-too-is-not-the-runs.md))
//!
//! The first time a command fails in a repository's worktree, it is run once on `origin/<base>`
//! (a [`BaseCheckout`](adam_workspace::BaseCheckout)) in the run's environment session. The result
//! is kept in the run notes per command, directory and base commit, so the base runs once. Only a
//! conclusive answer counts: a timeout, a missing tool, a signal or an environment that cannot run
//! it is [`OnBase::Unknown`].

use adam::prelude::*;
use adam_workspace::{EnvSession, Worktree};

use crate::redact::Redactor;

use super::checks::cut_tail;
use super::notes::{BaseResult, RunNotes};
use super::shell::{RunError, ShellOutcome, missing_tool, resolve_cwd, run_in, shell_spec};
use super::{ToolEnv, notes_error};

/// Most bytes of the base's output kept in the notes and shown to the model.
const BASE_TAIL_CAP: usize = 2000;

/// What running a failing command on the base commit found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OnBase {
    /// It fails there too, with a non-zero exit code: the failure is the repository's.
    Fails(BaseResult),
    /// It passes there: the run's change caused the failure.
    Passes(BaseResult),
    /// Not known: nothing to check out, a timeout, a missing tool, an environment that cannot run
    /// it. The failure is the run's.
    Unknown,
}

impl OnBase {
    fn of(result: &BaseResult) -> Self {
        if result.failed {
            Self::Fails(result.clone())
        } else {
            Self::Passes(result.clone())
        }
    }
}

/// The failing command and where it ran.
pub(super) struct Failing<'a> {
    pub env: &'a ToolEnv,
    pub ctx: &'a ToolCtx,
    pub session: &'a dyn EnvSession,
    pub run: &'a str,
    pub wt: &'a Worktree,
    /// The command as the notes keep it (scrubbed).
    pub shown: &'a str,
    pub command: &'a str,
    /// The directory the model gave, relative to the worktree.
    pub cwd: Option<&'a str>,
}

/// Run the failing command on the base commit, unless that was done for this command, directory and
/// commit before. The notes are saved when the base was run.
///
/// # Errors
///
/// The run was cancelled, or the notes cannot be written.
pub(super) async fn on_base(
    failing: Failing<'_>,
    notes: &mut RunNotes,
) -> Result<OnBase, ToolError> {
    let Failing {
        env,
        ctx,
        session,
        run,
        wt,
        shown,
        command,
        cwd,
    } = failing;
    let cwd_key = cwd
        .map(str::trim)
        .filter(|c| !c.is_empty() && *c != ".")
        .unwrap_or("");
    let base = match wt.base_commit().await {
        Ok(Some(base)) => base,
        Ok(None) => return Ok(OnBase::Unknown),
        Err(e) => {
            tracing::warn!(error = %e, "cannot tell what the base commit is");
            return Ok(OnBase::Unknown);
        }
    };
    if let Some(known) = notes.base_result(shown, cwd_key, &base) {
        return Ok(OnBase::of(known));
    }

    ctx.emit_progress(format!(
        "checking whether it fails on {} too: {shown}",
        wt.repo().base_branch
    ))
    .await;
    let checkout = match wt.add_base_checkout().await {
        Ok(Some(checkout)) => checkout,
        Ok(None) => return Ok(OnBase::Unknown),
        Err(e) => {
            tracing::warn!(error = %e, "cannot check the base out");
            return Ok(OnBase::Unknown);
        }
    };
    let ran = match resolve_cwd(checkout.path(), Some(cwd_key)) {
        Ok(dir) => Some(
            run_in(
                session,
                shell_spec(&dir, command),
                env.settings.check_timeout,
                env.settings.check_output_tail,
                &ctx.cancel_token(),
            )
            .await,
        ),
        // The directory does not exist on the base: nothing to compare.
        Err(_) => None,
    };
    if let Err(e) = wt.remove_base_checkout(&checkout).await {
        tracing::warn!(error = %e, "cannot remove the base checkout");
    }
    let outcome = match ran {
        Some(Ok(outcome)) => outcome,
        Some(Err(RunError::Cancelled)) => return Err(super::cancelled("the command was stopped")),
        Some(Err(e)) => {
            tracing::warn!(error = %e, "the command could not be run on the base");
            return Ok(OnBase::Unknown);
        }
        None => return Ok(OnBase::Unknown),
    };
    let Some(result) = conclusion(&outcome, command, shown, cwd_key, &base, &env.redactor) else {
        return Ok(OnBase::Unknown);
    };
    notes.record_base_result(result.clone());
    env.notes
        .save(run, notes)
        .await
        .map_err(|e| notes_error(&e))?;
    Ok(OnBase::of(&result))
}

/// What `outcome`, the command's run on the base commit `base`, says: `None` unless it is
/// conclusive (see the module's rules).
fn conclusion(
    outcome: &ShellOutcome,
    command: &str,
    shown: &str,
    cwd: &str,
    base: &str,
    redactor: &Redactor,
) -> Option<BaseResult> {
    if outcome.timed_out || missing_tool(outcome, command).is_some() {
        return None;
    }
    let code = outcome.exit_code?;
    // A shell's "command not found", which says nothing about the base's code.
    if code == 127 {
        return None;
    }
    let tail = redactor.scrub_string(outcome.tail.trim().to_owned());
    Some(BaseResult {
        command: shown.to_owned(),
        cwd: cwd.to_owned(),
        commit: base.to_owned(),
        failed: code != 0,
        exit_code: Some(code).filter(|c| *c != 0),
        tail: cut_tail(&tail, BASE_TAIL_CAP).to_owned(),
    })
}

/// What the model is told when the command fails on the base too: the failure is the repository's,
/// no check cycle was used, what is expected of the change, the base's output beside the run's, and
/// the way on.
pub(super) fn fails_text(
    result: &BaseResult,
    base_branch: &str,
    own_tail: &str,
    used: u32,
    max: u32,
) -> String {
    let short = &result.commit[..result.commit.len().min(10)];
    let code = super::notes::exit_phrase(result.exit_code);
    let same = if own_tail.trim() == result.tail.trim() {
        "The two outputs are identical."
    } else {
        "The two outputs are not identical (timings and paths differ, or this run changed the \
         failure): compare them, and make sure yours has no error that the base's does not have."
    };
    format!(
        "\nThis failure is PRE-EXISTING: the same command also fails on `origin/{base_branch}` \
         ({short}, the code this branch started from, run in the same environment: {code}). It is \
         the repository's failure and not yours, so it used no check cycle ({used} of {max} \
         used).\nDo not try to make it pass unless the task is about it, and never silence or skip \
         the check. Your change must not make it worse: no error beyond the ones `origin/{base_branch}` \
         already has.\n--- output on origin/{base_branch} ---\n{}\n{same}\nWhen the rest of your \
         work is done and this is the only red check, commit_and_push, then open_pull_request (do \
         not set accept_red_checks) and say in the body that `{}` already fails on \
         origin/{base_branch}. The other checks are run as usual.\n",
        result.tail, result.command
    )
}

/// What the model is told when the command passes on the base: the failure is the change's.
pub(super) fn passes_text(result: &BaseResult, base_branch: &str) -> String {
    let short = &result.commit[..result.commit.len().min(10)];
    format!(
        "\nThe same command passes on `origin/{base_branch}` ({short}): this failure was caused by \
         the change.\n"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn outcome(code: Option<i32>, timed_out: bool, tail: &str) -> ShellOutcome {
        ShellOutcome {
            exit_code: code,
            timed_out,
            tail: tail.to_owned(),
            truncated: false,
        }
    }

    fn conclude(o: &ShellOutcome) -> Option<BaseResult> {
        conclusion(
            o,
            "yarn check",
            "yarn check",
            "",
            BASE,
            &Redactor::default(),
        )
    }

    #[test]
    fn only_a_conclusive_run_on_the_base_counts() {
        let fails = conclude(&outcome(Some(2), false, "error TS2304\n")).unwrap();
        assert!(fails.failed);
        assert_eq!(fails.exit_code, Some(2));
        assert_eq!(fails.tail, "error TS2304");
        assert_eq!(fails.commit, BASE);

        let passes = conclude(&outcome(Some(0), false, "ok")).unwrap();
        assert!(!passes.failed);
        assert_eq!(passes.exit_code, None);

        assert_eq!(conclude(&outcome(Some(1), true, "")), None, "a timeout");
        assert_eq!(conclude(&outcome(None, false, "")), None, "a signal");
        assert_eq!(
            conclude(&outcome(Some(127), false, "sh: yarn: command not found")),
            None,
            "a tool the shell cannot find"
        );
        assert_eq!(conclude(&outcome(Some(127), false, "")), None);
    }

    #[test]
    fn the_output_kept_from_the_base_is_cut_to_its_end() {
        let long = format!("{}\nthe end", "x".repeat(5000));
        let kept = conclude(&outcome(Some(1), false, &long)).unwrap();
        assert!(kept.tail.len() <= BASE_TAIL_CAP);
        assert!(kept.tail.ends_with("the end"));
    }

    #[test]
    fn the_model_is_told_the_failure_is_the_repositorys() {
        let result = conclude(&outcome(Some(2), false, "error TS2304")).unwrap();
        let text = fails_text(&result, "main", "error TS2304\n", 0, 3);
        assert!(text.contains("PRE-EXISTING"), "{text}");
        assert!(text.contains("`origin/main`"), "{text}");
        assert!(text.contains("0123456789"), "{text}");
        assert!(text.contains("used no check cycle (0 of 3 used)"), "{text}");
        assert!(text.contains("must not make it worse"), "{text}");
        assert!(text.contains("The two outputs are identical."), "{text}");
        assert!(
            text.contains("do not set accept_red_checks")
                || text.contains("not set accept_red_checks")
        );
        let other = fails_text(&result, "main", "error TS2304\nerror TS9999\n", 0, 3);
        assert!(other.contains("are not identical"), "{other}");
        assert!(passes_text(&result, "main").contains("caused by the change"));
    }
}
