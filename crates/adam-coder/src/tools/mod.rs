//! The coder's tools, exactly the set the issue lists:
//!
//! | Tool | Module |
//! |---|---|
//! | `prepare_workspace { repo_url, base_branch }` | [`prepare`] |
//! | `delegate_to_opencode { instructions }` | [`delegate`] |
//! | `run_checks { command }` | [`checks`] |
//! | `commit_and_push { message }` / `open_pull_request { title, body }` | [`publish`] |
//! | `ask_user { question }` | [`ask`] |
//!
//! # Retry safety
//!
//! Every side effect runs inside `LlmAgent`'s journaled `tool:<call id>` step,
//! so a finished call is never repeated. A call that dies *before* its result
//! is recorded runs again (crash mid-call, or a transient retry, which starts
//! at a fresh journal position), so each tool is also safe to repeat by
//! construction:
//!
//! * `prepare_workspace`: `Workspaces::prepare` reuses the run's worktree.
//! * `commit_and_push`: `commit_all` is a no-op without changes and the push of
//!   a commit the remote already has is a no-op; the reported sha is `HEAD`.
//! * `open_pull_request`: `CodeHost::open_pull_request` returns the open pull
//!   request of the same head instead of a second one.
//! * `run_checks`: failures are counted per call id ([`notes`]).
//!
//! # The rules, in code
//!
//! The prompt tells the model the rules; these make them hold: after
//! `MAX_CHECK_CYCLES` failed check runs `run_checks` refuses to run, and
//! `open_pull_request` refuses unless the last check run passed on exactly the
//! code the pull request contains (the tree of the pushed `HEAD`) or the model
//! passes `accept_red_checks: true` (which the prompt reserves for explicit
//! user consent obtained with `ask_user`).

use std::sync::Arc;
use std::time::Duration;

use adam_llm_agent::{DynTool, ToolCtx, ToolError, ToolOutput};
use adam_workspace::{DynCodeHost, GitIdentity, WorkspaceError, Workspaces, Worktree};
use serde_json::Value;

use crate::opencode::OpenCodeLaunch;

pub mod ask;
pub mod checks;
pub mod delegate;
mod gitcli;
pub mod notes;
pub mod prepare;
pub mod publish;
pub mod shell;

pub use notes::{NotesStore, RunNotes};

/// What a tool call returns.
pub(crate) type Outcome = Result<ToolOutput, ToolError>;

/// Tunables of the tools.
#[derive(Debug, Clone)]
pub struct CoderSettings {
    /// Failed `run_checks` calls after which the agent must stop.
    pub max_check_cycles: u32,
    /// Time limit of one `run_checks` command.
    pub check_timeout: Duration,
    /// Bytes of output tail `run_checks` returns.
    pub check_output_tail: usize,
    /// Author and committer of the commits.
    pub identity: GitIdentity,
    /// Open pull requests as drafts.
    pub draft_pull_requests: bool,
    /// How to start OpenCode.
    pub opencode: OpenCodeLaunch,
}

impl CoderSettings {
    /// Defaults: 3 cycles, 15 minutes and 16 KiB per check run, the
    /// `adam-coder` identity, ready-for-review pull requests.
    pub fn new(opencode: OpenCodeLaunch) -> Self {
        Self {
            max_check_cycles: 3,
            check_timeout: Duration::from_secs(900),
            check_output_tail: 16 * 1024,
            identity: GitIdentity::new("adam-coder", "adam-coder@users.noreply.github.com"),
            draft_pull_requests: false,
            opencode,
        }
    }
}

/// What every tool shares: the workspaces, the code host, the settings and the
/// per-run notes.
pub struct ToolEnv {
    /// Mirrors and worktrees.
    pub workspaces: Workspaces,
    /// Where pull requests are opened.
    pub code_host: DynCodeHost,
    /// Tunables.
    pub settings: CoderSettings,
    /// Per-run bookkeeping.
    pub notes: NotesStore,
}

impl ToolEnv {
    /// Environment over `workspaces` and `code_host`.
    pub fn new(workspaces: Workspaces, code_host: DynCodeHost, settings: CoderSettings) -> Self {
        let notes = NotesStore::new(workspaces.root());
        Self {
            workspaces,
            code_host,
            settings,
            notes,
        }
    }

    /// The worktree of the run `ctx` belongs to, or the message to give the
    /// model when there is none yet.
    pub(crate) async fn worktree(&self, ctx: &ToolCtx) -> Result<Worktree, Outcome> {
        match self
            .workspaces
            .open_existing(&ctx.run_id().to_string())
            .await
        {
            Ok(Some(wt)) => Ok(wt),
            Ok(None) => Err(Ok(ToolOutput::error(
                "there is no workspace yet: call prepare_workspace first",
            ))),
            Err(e) => Err(Err(workspace_error(&e))),
        }
    }
}

/// Every coder tool over `env`, in the order they are offered to the model.
pub fn coder_tools(env: &Arc<ToolEnv>) -> Vec<DynTool> {
    vec![
        Arc::new(prepare::PrepareWorkspace::new(env.clone())),
        Arc::new(delegate::DelegateToOpenCode::new(env.clone())),
        Arc::new(checks::RunChecks::new(env.clone())),
        Arc::new(publish::CommitAndPush::new(env.clone())),
        Arc::new(publish::OpenPullRequest::new(env.clone())),
        Arc::new(ask::AskUser),
    ]
}

/// A non-empty string argument.
pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// A workspace failure as a tool error: worth retrying, or a report to the
/// model.
pub(crate) fn workspace_error(e: &WorkspaceError) -> ToolError {
    if e.is_retryable() {
        ToolError::Transient(e.to_string())
    } else {
        ToolError::Permanent(e.to_string())
    }
}

/// A failure to read or write the run's notes.
pub(crate) fn notes_error(e: &std::io::Error) -> ToolError {
    ToolError::Transient(format!("cannot access the run notes: {e}"))
}
