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
//! The prompt tells the model the rules; these make them hold: `prepare_workspace` refuses a
//! repository the person did not name ([`named`]; the agent records the repositories of the
//! person's own messages in the run notes before each step), after
//! `MAX_CHECK_CYCLES` failed check runs `run_checks` refuses to run, and
//! `open_pull_request` refuses unless the last check run passed on exactly the
//! code the pull request contains (the tree of the pushed `HEAD`) or the model
//! passes `accept_red_checks: true` (which the prompt reserves for explicit
//! user consent obtained with `ask_user`).

use std::sync::Arc;
use std::time::Duration;

use adam::prelude::*;
use adam::{DynTool, StateKey};
use adam_error::{Classify, report};
use adam_model::ToolSpec;
use adam_workspace::{DynCodeHost, GitIdentity, WorkspaceError, Workspaces, Worktree};
use serde_json::Value;

use crate::opencode::OpenCodeLaunch;
use crate::redact::Redactor;

pub mod ask;
pub mod checks;
pub mod delegate;
mod gitcli;
pub mod named;
pub mod notes;
pub mod prepare;
pub mod publish;
pub mod shell;

pub use notes::{NotesStore, RunNotes};

/// What a tool call returns.
pub(crate) type Outcome = Result<ToolOutput, ToolError>;

/// What a tool fails with when the run was cancelled under (or before) it.
///
/// The run is already `Failed` (`cancelled: ...`) by then and whatever the
/// call returns is dropped, but the loop that called the tool goes on to the
/// next call of the same model turn: the tools with effects outside the
/// worktree ([`publish`]) therefore check [`ToolCtx::is_cancelled`] first and
/// refuse with this.
pub(crate) fn cancelled(what: &str) -> ToolError {
    ToolError::Permanent(format!("cancelled: the run was cancelled; {what}"))
}

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
    /// Removes the process's secrets from everything a tool returns, reports
    /// or fails with. Empty (a no-op) until [`ToolEnv::with_redactor`].
    pub redactor: Redactor,
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
            redactor: Redactor::default(),
        }
    }

    /// Scrub the values `redactor` knows from every tool result, tool error and
    /// progress line of OpenCode and the checks.
    #[must_use]
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// [`workspace_error`], and when the credentials were rejected also a note
    /// that the run cannot deliver. The model sees the error either way, but a
    /// bad token is not something it can fix, so a run that ends without a
    /// pull request after this must fail rather than complete.
    pub(crate) async fn delivery_error(&self, ctx: &ToolCtx, e: &WorkspaceError) -> ToolError {
        if matches!(e, WorkspaceError::Auth(_)) {
            let run = ctx.run_id().to_string();
            let recorded = async {
                let mut notes = self.notes.load(&run).await?;
                notes.blocker = Some(format!(
                    "the credentials were rejected ({e}); check that GITHUB_TOKEN is valid and \
                     may push and open pull requests for the repository"
                ));
                self.notes.save(&run, &notes).await
            }
            .await;
            if let Err(err) = recorded {
                tracing::warn!(error = %err, "cannot record the credential failure in the run notes");
            }
        }
        workspace_error(e)
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

/// Every coder tool, in the order they are offered to the model.
///
/// Each tool is wrapped so that what it returns or fails with passes through
/// [`ToolEnv::redactor`] first. The tools read `env` from the agent's state:
/// give it to the agent that gets these tools
/// (`LlmAgentBuilder::state(env.clone())`), or they refuse every call. The
/// argument only names the redactor to wrap them with.
pub fn coder_tools(env: &Arc<ToolEnv>) -> ToolSet {
    tools![
        prepare::PrepareWorkspace,
        delegate::DelegateToOpenCode,
        checks::RunChecks,
        publish::CommitAndPush,
        publish::OpenPullRequest,
        ask::AskUser,
    ]
    .wrap(Redacting::layer(env.redactor.clone()))
}

/// A tool whose results and errors are scrubbed by a [`Redactor`].
struct Redacting {
    inner: DynTool,
    redactor: Redactor,
}

impl Redacting {
    /// The wrapper for [`ToolSet::wrap`](adam::ToolSet::wrap).
    fn layer(redactor: Redactor) -> impl FnMut(DynTool) -> DynTool {
        move |inner| {
            Arc::new(Self {
                inner,
                redactor: redactor.clone(),
            })
        }
    }
}

#[async_trait::async_trait]
impl Tool for Redacting {
    fn spec(&self) -> ToolSpec {
        self.inner.spec()
    }

    fn required_state(&self) -> Vec<StateKey> {
        self.inner.required_state()
    }

    fn asks_user(&self) -> bool {
        self.inner.asks_user()
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let r = &self.redactor;
        match self.inner.call(ctx, args).await {
            Ok(mut out) => {
                out.content = r.scrub_string(out.content);
                for artifact in &mut out.artifacts {
                    r.scrub_value(&mut artifact.data);
                }
                Ok(out)
            }
            Err(ToolError::Transient(m)) => Err(ToolError::Transient(r.scrub_string(m))),
            Err(ToolError::Permanent(m)) => Err(ToolError::Permanent(r.scrub_string(m))),
            Err(ToolError::NeedsInput { question }) => Err(ToolError::NeedsInput {
                question: r.scrub_string(question),
            }),
            // `ToolError` is non_exhaustive: a variant added later must not slip past the
            // redactor, so it is reported to the model as a scrubbed permanent error.
            Err(other) => Err(ToolError::Permanent(r.scrub_string(report(&other)))),
        }
    }
}

/// `text` trimmed, unless nothing is left: what the model wrote in a required argument.
pub(crate) fn non_empty(text: &str) -> Option<&str> {
    Some(text.trim()).filter(|s| !s.is_empty())
}

/// A workspace failure as a tool error: worth retrying, or a report to the
/// model.
pub(crate) fn workspace_error(e: &WorkspaceError) -> ToolError {
    // The tool error is journaled and shown to the model: a boundary, so the chain is flattened
    // here, once.
    let text = report(e);
    if e.is_retryable() {
        ToolError::Transient(text)
    } else {
        ToolError::Permanent(text)
    }
}

/// A failure to read or write the run's notes.
pub(crate) fn notes_error(e: &std::io::Error) -> ToolError {
    ToolError::Transient(format!("cannot access the run notes: {e}"))
}
