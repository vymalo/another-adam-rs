//! `prepare_workspace { repo_url, base_branch }`.

use adam::prelude::*;
use adam_workspace::RepoRef;

use super::{Outcome, ToolEnv, non_empty};

/// Branch used when the model leaves `base_branch` out.
const DEFAULT_BASE_BRANCH: &str = "main";

// Checks the repository out into this run's worktree.
//
// The run id is the workspace's run key, so a restarted or retried call finds
// the worktree it already made (`Workspaces::prepare` is idempotent per run)
// and keeps whatever is in it.

/// Check the repository out into your private worktree, on a fresh branch
/// created from origin/<base_branch>. Call it once, first. Calling it again
/// for the same repository is harmless and keeps your changes.
#[tool]
pub async fn prepare_workspace(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// https://github.com/<owner>/<repo>
    repo_url: String,
    // Optional for the code (a missing or empty branch means `main`), but the model is told it is
    // required: it must say which branch the pull request goes against.
    /// Branch to start from and to open the pull request against, e.g. main
    #[schemars(required)]
    base_branch: Option<String>,
) -> Outcome {
    let Some(url) = non_empty(&repo_url) else {
        return Ok(ToolOutput::error("repo_url is required"));
    };
    let base = base_branch
        .as_deref()
        .and_then(non_empty)
        .unwrap_or(DEFAULT_BASE_BRANCH);
    let run = ctx.run_id().to_string();
    ctx.emit_progress(format!("preparing a worktree of {url} ({base})"))
        .await;
    let repo = RepoRef::new(url, base);
    let wt = match env.workspaces.prepare(&repo, &run).await {
        Ok(wt) => wt,
        Err(
            e @ (adam_workspace::WorkspaceError::Invalid(_)
            | adam_workspace::WorkspaceError::NotFound(_)
            | adam_workspace::WorkspaceError::Conflict(_)),
        ) => {
            // The model gave a bad repository or branch: tell it, do not fail.
            return Ok(ToolOutput::error(e.to_string()));
        }
        Err(e) => return Err(env.delivery_error(ctx, &e).await),
    };
    Ok(ToolOutput::text(format!(
        "Worktree ready.\nrepository: {url}\nbase branch: {base}\nbranch: {}\npath: {}",
        wt.branch(),
        wt.path().display()
    )))
}
