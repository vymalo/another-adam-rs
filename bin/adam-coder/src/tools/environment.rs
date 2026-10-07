//! `rebuild_environment { use_default? }`: the way out of a work environment that is broken.
//!
//! The commands of a run (`run_command`, `run_checks`, OpenCode) run in the repository's devcontainer
//! when the deployment has a container runtime ([`adam_devcontainer`](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0010-a-run-works-in-its-repositorys-devcontainer.md)).
//! A repository whose `devcontainer.json` is wrong, refused by the policy, or does not build leaves the
//! run with an environment that is **broken**, and every command says so: there is no silent fallback
//! to a default image, because the person may want the file fixed more than they want the work done
//! in another environment. The model asks them (`ask_user`), and after their answer calls this tool:
//!
//! * `rebuild_environment {}`: the file was fixed (or the person wants it tried again, for a tool that
//!   was added to it): the environment is thrown away and made again from the file as it is now;
//! * `rebuild_environment { use_default: true }`: the person agreed to go on in the default
//!   environment: the repository's own file is not used for the rest of the run.
//!
//! The environment is made **now**, so that its steps are shown under this call and a failure is this
//! call's result. With no container runtime there is nothing to rebuild and the tool says so.

use adam::prelude::*;

use super::{Outcome, ToolEnv, cancelled, environment_error, notes_error};

/// Make the run's work environment again, after the person has decided what to do about a broken one.
/// Call it only after they answered: when the commands report that the environment is broken, ask
/// them (ask_user) whether they will fix `.devcontainer/devcontainer.json` or you should go on in the
/// default environment, and then call this with `use_default: true` only if they chose the default.
/// Without `use_default` the environment is built again from the repository's file as it is now (after
/// the person fixed it, or after the file was changed to add a tool). Your files are not touched. It
/// builds the environment before it returns, which can take minutes.
#[tool(label = "Rebuild the environment")]
pub async fn rebuild_environment(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// true only when the person chose to go on in the default environment instead of the repository's own
    use_default: Option<bool>,
) -> Outcome {
    if ctx.is_cancelled() {
        return Err(cancelled("the environment was not rebuilt"));
    }
    let use_default = use_default.unwrap_or(false);
    let run = ctx.root_run_id().to_string();
    let made_again = env
        .environment
        .rebuild(&run, use_default)
        .await
        .map_err(|e| environment_error(&env.redactor, &e))?;
    if !made_again {
        return Ok(ToolOutput::text(
            "Nothing was rebuilt: the commands of this run run in the coder's own environment (this \
             deployment has no container runtime for work environments), so there is no separate \
             environment to rebuild. Go on with the task.",
        ));
    }
    // What the rebuilt environment will be asked again, and what the person chose.
    let mut notes = env.notes.load(&run).await.map_err(|e| notes_error(&e))?;
    notes.environment.use_default |= use_default;
    notes.environment.opencode = None;
    env.notes
        .save(&run, &notes)
        .await
        .map_err(|e| notes_error(&e))?;

    // Make it now: its steps are shown under this call, and a failure is this call's result.
    let session = env.session(ctx).await?;
    let summary = session.describe().summary;
    let mut text = format!("The work environment was made again: {summary}.");
    if notes.environment.use_default {
        text.push_str(
            " The repository's own devcontainer.json is not used for the rest of this run: the \
             commands run in the default environment, which the person chose. Say so in your final \
             answer, so that the pull request's reader knows the work was checked there.",
        );
    }
    Ok(ToolOutput::text(text))
}
