//! `prepare_workspace { repo_url, base_branch }`.

use std::sync::Arc;

use adam_llm_agent::{Tool, ToolCtx, ToolOutput};
use adam_model::ToolSpec;
use adam_workspace::RepoRef;
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{Outcome, ToolEnv, str_arg};

/// Branch used when the model leaves `base_branch` out.
const DEFAULT_BASE_BRANCH: &str = "main";

/// Checks the repository out into this run's worktree.
///
/// The run id is the workspace's run key, so a restarted or retried call finds
/// the worktree it already made (`Workspaces::prepare` is idempotent per run)
/// and keeps whatever is in it.
pub struct PrepareWorkspace {
    env: Arc<ToolEnv>,
}

impl PrepareWorkspace {
    /// The tool over `env`.
    pub fn new(env: Arc<ToolEnv>) -> Self {
        Self { env }
    }
}

#[async_trait]
impl Tool for PrepareWorkspace {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "prepare_workspace".into(),
            description: "Check the repository out into your private worktree, on a fresh branch \
                          created from origin/<base_branch>. Call it once, first. Calling it again \
                          for the same repository is harmless and keeps your changes."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "repo_url": {
                        "type": "string",
                        "description": "https://github.com/<owner>/<repo>"
                    },
                    "base_branch": {
                        "type": "string",
                        "description": "Branch to start from and to open the pull request against, e.g. main"
                    }
                },
                "required": ["repo_url", "base_branch"]
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Outcome {
        let Some(url) = str_arg(&args, "repo_url") else {
            return Ok(ToolOutput::error("repo_url is required"));
        };
        let base = str_arg(&args, "base_branch").unwrap_or(DEFAULT_BASE_BRANCH);
        let run = ctx.run_id().to_string();
        ctx.emit_progress(format!("preparing a worktree of {url} ({base})"))
            .await;
        let repo = RepoRef::new(url, base);
        let wt = match self.env.workspaces.prepare(&repo, &run).await {
            Ok(wt) => wt,
            Err(
                e @ (adam_workspace::WorkspaceError::Invalid(_)
                | adam_workspace::WorkspaceError::NotFound(_)
                | adam_workspace::WorkspaceError::Conflict(_)),
            ) => {
                // The model gave a bad repository or branch: tell it, do not fail.
                return Ok(ToolOutput::error(e.to_string()));
            }
            Err(e) => return Err(self.env.delivery_error(ctx, &e).await),
        };
        Ok(ToolOutput::text(format!(
            "Worktree ready.\nrepository: {url}\nbase branch: {base}\nbranch: {}\npath: {}",
            wt.branch(),
            wt.path().display()
        )))
    }
}
