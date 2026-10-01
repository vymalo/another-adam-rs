//! The coder's tools, exactly the set the issue lists:
//!
//! | Tool | Module |
//! |---|---|
//! | `prepare_workspace { repo_url, base_branch?, branch? }` | [`prepare`] |
//! | `run_command { command, cwd? }` | [`inspect`] |
//! | `read_file { path, start_line?, end_line? }`, `write_file { path, content }`, `apply_patch { patch }` | [`files`] |
//! | `delegate_to_opencode { instructions }` | [`delegate`] |
//! | `run_checks { command }` | [`checks`] |
//! | `commit_and_push { message }` / `open_pull_request { title, body }` | [`publish`] |
//! | `ask_user { question, choices? }`, `show { blocks, title? }`, `ui_catalog {}` | [`adam_ui`]: the person's screen as tools |
//!
//! # Retry safety
//!
//! Every side effect runs inside `LlmAgent`'s journaled `tool:<call id>` step,
//! so a finished call is never repeated. A call that dies *before* its result
//! is recorded runs again (crash mid-call, or a transient retry, which starts
//! at a fresh journal position), so each tool is also safe to repeat by
//! construction:
//!
//! * `prepare_workspace`: `RunWorkspace::add_repository` (or `add_repository_continuing`) returns the
//!   slot the run already has for the repository, with whatever is in it.
//! * `commit_and_push`: `commit_all` is a no-op without changes and the push of
//!   a commit the remote already has is a no-op; the reported sha is `HEAD`.
//! * `open_pull_request`: moving the continued branch to the pushed commit is a no-op the second
//!   time, a pull request already open for the head is reported as it is (the run continued a
//!   branch an earlier task opened it for, or the call is repeated), and
//!   `CodeHost::open_pull_request` returns it instead of a second one in any case.
//! * `run_checks`: failures are counted per call id ([`notes`]).
//!
//! # The rules, in code
//!
//! The prompt tells the model the rules; these make them hold: `prepare_workspace` refuses a
//! repository the person did not name ([`named`]; the agent records the repositories of the
//! person's own messages in the run notes before each step; it continues a branch only if a
//! `commit_and_push` of the conversation recorded it in the notes, which the agent carries from
//! the run it continues, and as a fallback read from the result text, `publish::pushed_in`),
//! after `MAX_CHECK_CYCLES` failed check runs `run_checks` refuses to run, and
//! `open_pull_request` refuses unless the most recent check run of exactly the
//! code the pull request contains (the tree of the pushed `HEAD`, whichever slot of the
//! workspace ran it: [`notes::RunNotes::checked`]) passed, or the model
//! passes `accept_red_checks: true` (which the prompt reserves for explicit
//! user consent obtained with `ask_user`).
//!
//! A run that continues a branch pushes to a branch of its own (`commit_and_push` never touches
//! the continued one); only `open_pull_request`, after that check, fast-forwards the continued
//! branch to the pushed commit (never forced), so a pull request open for it only ever carries
//! code the gate has seen. On an accepted red check the update is noted in a comment on the pull
//! request. A branch that moved on the remote is not overwritten.

use std::sync::Arc;
use std::time::Duration;

use adam::mcp::McpPolicy;
use adam::prelude::*;
use adam::{DynTool, StateKey, StepEvent, StepIcon, StepKind, StepState, StepStyle};
use adam_error::{Classify, report};
use adam_model::ToolSpec;
use adam_ui::Ui;
use adam_workspace::{
    DynCodeHost, DynEnvironment, EnvError, EnvProgress, EnvSession, EnvStep, EnvStepState,
    GitIdentity, Local, Slot, WorkspaceError, Workspaces, Worktree,
};
use serde_json::Value;

use crate::opencode::OpenCodeLaunch;
use crate::redact::Redactor;

/// What the description of `ask_user` opens with: the coder's own words about when to ask.
const ASK_LEAD: &str = "Ask the person who gave you the task a question and wait for the answer. Use it only when you cannot proceed without it, or to get explicit consent (for example to open a pull request with failing checks). Be specific.";

pub mod checks;
pub mod delegate;
pub mod files;
mod gitcli;
pub mod inspect;
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
    /// The host `owner/name` stands for when the person writes a repository that way: the first
    /// of `ALLOWED_REPO_HOSTS` in the binary.
    pub default_repo_host: String,
}

impl CoderSettings {
    /// Defaults: 3 cycles, 15 minutes and 16 KiB per check run, the
    /// `adam-coder` identity, ready-for-review pull requests, `github.com` for `owner/name`.
    pub fn new(opencode: OpenCodeLaunch) -> Self {
        Self {
            max_check_cycles: 3,
            check_timeout: Duration::from_secs(900),
            check_output_tail: 16 * 1024,
            identity: GitIdentity::new("adam-coder", "adam-coder@users.noreply.github.com"),
            draft_pull_requests: false,
            opencode,
            default_repo_host: named::DEFAULT_HOST.to_owned(),
        }
    }
}

/// What every tool shares: the workspaces, the code host, the settings and the
/// per-run notes.
pub struct ToolEnv {
    /// Mirrors, and the workspace of each run: its slots (worktrees of repositories).
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
    /// The person's screen as tools (`ask_user` with choices, `show`, `ui_catalog`) and as a
    /// tool source (the tools of the conversation's endpoint): the catalogs this process has read
    /// are shared by the tools and the source, so both come from this one value. Under the
    /// default [`McpPolicy`] until [`ToolEnv::with_mcp_policy`].
    pub ui: Ui,
    /// Where the run's processes run: the project's checks, the commands that look around and
    /// OpenCode. [`Local`], this container, until [`ToolEnv::with_environment`]. The file tools and
    /// everything git does stay in this process whatever it is: they act on the shared files.
    pub environment: DynEnvironment,
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
            ui: Ui::new(McpPolicy::default()).with_ask_lead(ASK_LEAD),
            environment: Arc::new(Local),
        }
    }

    /// Run the processes of runs in `environment` instead of this container. The janitor of the
    /// process ([`Janitor::with_environment`](crate::Janitor::with_environment)) must be given the
    /// same one, so that what it holds for a run is released when the run's workspace is.
    #[must_use]
    pub fn with_environment(mut self, environment: DynEnvironment) -> Self {
        self.environment = environment;
        self
    }

    /// Reach the conversation's tool endpoint under `policy` (the deployment's
    /// `MCP_ALLOW_INSECURE` and timeouts): the URL a message announces is checked like the URL of
    /// any MCP server. The catalogs read so far are forgotten (a new [`Ui`]).
    #[must_use]
    pub fn with_mcp_policy(mut self, policy: McpPolicy) -> Self {
        self.ui = Ui::new(policy).with_ask_lead(ASK_LEAD);
        self
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

    /// The session of the run's environment, for a tool that runs a process: made on the first
    /// need and the same after ([`Environment::ensure`](adam_workspace::Environment::ensure)).
    ///
    /// What the environment says while it makes one (pulling an image, building it) is shown as
    /// steps under the tool call, `env:<run>:<step>`, scrubbed like everything else the coder
    /// shows. A cancel of the run stops the wait. Needs no workspace: it is the run's, whether or
    /// not a slot is there yet.
    pub(crate) async fn session(&self, ctx: &ToolCtx) -> Result<Arc<dyn EnvSession>, ToolError> {
        let run = ctx.run_id().to_string();
        let workspace = self.workspaces.run(&run).map_err(|e| workspace_error(&e))?;
        let (steps, mut reported) = tokio::sync::mpsc::unbounded_channel();
        let progress = StepChannel(steps);
        let ensure = self.environment.ensure(&workspace, &progress);
        tokio::pin!(ensure);
        let made = loop {
            tokio::select! {
                biased;
                () = ctx.cancelled() => {
                    return Err(cancelled("the environment of the run was not made"));
                }
                Some(step) = reported.recv() => self.show_step(ctx, &run, step).await,
                made = &mut ensure => break made,
            }
        };
        while let Ok(step) = reported.try_recv() {
            self.show_step(ctx, &run, step).await;
        }
        made.map_err(|e| environment_error(&self.redactor, &e))
    }

    /// A step of making the environment, as a step of the tool call.
    async fn show_step(&self, ctx: &ToolCtx, run: &str, step: EnvStep) {
        let state = match step.state {
            EnvStepState::Running => StepState::Running,
            EnvStepState::Completed => StepState::Completed,
            EnvStepState::Failed => StepState::Failed,
        };
        let label = self.redactor.scrub(&step.label).into_owned();
        let mut event = StepEvent::new(
            format!("env:{run}:{}", step.id),
            StepKind::Command,
            label,
            state,
        )
        .with_icon(StepIcon::Execute);
        if let Some(detail) = step.detail {
            event = event.with_detail(self.redactor.scrub(&detail));
        }
        ctx.report_step(event).await;
    }

    /// The slot of the run's workspace that a tool acts in, or the message to give the model: it
    /// has no workspace yet, or it did not say which of several slots (see [`resolve_slot`]).
    ///
    /// `repo` is what the model passed: the slot's directory (`sandbox`) or the address of the
    /// repository the slot holds. It may be left out when the workspace has one slot.
    pub(crate) async fn slot(&self, ctx: &ToolCtx, repo: Option<&str>) -> Result<Slot, Outcome> {
        let run = ctx.run_id().to_string();
        let workspace = self
            .workspaces
            .run(&run)
            .map_err(|e| Err(workspace_error(&e)))?;
        let slots = workspace
            .slots()
            .await
            .map_err(|e| Err(workspace_error(&e)))?;
        resolve_slot(slots, repo, &self.settings.default_repo_host)
            .map_err(|why| Ok(ToolOutput::error(why)))
    }

    /// The worktree of the slot a tool acts in ([`slot`](Self::slot)), for the tools that need a
    /// repository: a scratch project has none to push to.
    pub(crate) async fn worktree(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
    ) -> Result<Worktree, Outcome> {
        let slot = self.slot(ctx, repo).await?;
        match slot.worktree() {
            Some(worktree) => Ok(worktree.clone()),
            None => Err(Ok(ToolOutput::error(format!(
                "`{}` is a scratch project, not a repository: there is nothing here to check, \
                 commit or push",
                slot.dir()
            )))),
        }
    }
}

/// The slot `repo` names among `slots` (see [`ToolEnv::slot`]), or what to tell the model.
///
/// * No slot at all: there is no workspace yet.
/// * `repo` left out: the only slot; with several, an error that lists them (it is the model's to
///   say which).
/// * `repo` given: the slot with that directory, else the slot of the repository it addresses
///   (`https://github.com/acme/lib`, `acme/lib`, a local path: compared as [`named`] keys).
pub(crate) fn resolve_slot(
    mut slots: Vec<Slot>,
    repo: Option<&str>,
    default_host: &str,
) -> Result<Slot, String> {
    if slots.is_empty() {
        return Err("there is no workspace yet: call prepare_workspace first".to_owned());
    }
    let listed = |slots: &[Slot]| {
        slots
            .iter()
            .map(|slot| match slot.worktree() {
                Some(wt) => format!("`{}` ({})", slot.dir(), wt.repo().url),
                None => format!("`{}` (a scratch project)", slot.dir()),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(repo) = repo.and_then(non_empty) else {
        return if slots.len() == 1 {
            Ok(slots.remove(0))
        } else {
            Err(format!(
                "this workspace has {} slots: {}. Say which one with `repo` (its name, or the \
                 repository's address)",
                slots.len(),
                listed(&slots)
            ))
        };
    };
    let key_of = |slot: &Slot| {
        slot.worktree()
            .and_then(|wt| named::key_of_argument(&wt.repo().url))
    };
    let wanted = named::key_of_argument(repo)
        .or_else(|| named::named_in(repo, default_host).into_iter().next());
    let at = slots
        .iter()
        .position(|slot| slot.dir() == repo)
        .or_else(|| {
            let wanted = wanted.as_deref()?;
            slots
                .iter()
                .position(|slot| key_of(slot).as_deref() == Some(wanted))
        });
    match at {
        Some(at) => Ok(slots.remove(at)),
        None => Err(format!(
            "no slot of this workspace is `{repo}`; its slots are {}. Use one of them as `repo` \
             (a repository that is not in the workspace is added with prepare_workspace)",
            listed(&slots)
        )),
    }
}

/// Every coder tool, in the order they are offered to the model: the nine of the coding workflow,
/// then the screen's (`ask_user`, `show`, `ui_catalog`, from [`ToolEnv::ui`]).
///
/// Each tool is wrapped so that what it returns or fails with passes through
/// [`ToolEnv::redactor`] first. The tools read `env` from the agent's state:
/// give it to the agent that gets these tools
/// (`LlmAgentBuilder::state(env.clone())`), or they refuse every call. The
/// argument only names the redactor to wrap them with.
///
/// `prepare_workspace` works only on a repository the person named, and what the person named is
/// recorded in the run notes by [`CoderAgent`](crate::CoderAgent) before each step. Tools used
/// under another agent see no named repository, so `prepare_workspace` refuses every one there
/// (a caller that composes its own agent records them with
/// [`RunNotes::name_repos`](notes::RunNotes::name_repos) and [`named::named_in`]).
pub fn coder_tools(env: &Arc<ToolEnv>) -> ToolSet {
    tools![
        prepare::PrepareWorkspace,
        inspect::RunCommand,
        files::ReadFile,
        files::WriteFile,
        files::ApplyPatch,
        delegate::DelegateToOpenCode,
        checks::RunChecks,
        publish::CommitAndPush,
        publish::OpenPullRequest,
    ]
    .extend(env.ui.tools())
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

    fn step_style(&self) -> StepStyle {
        self.inner.step_style()
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
            Err(ToolError::NeedsInput { question, mut ui }) => {
                if let Some(ui) = &mut ui {
                    r.scrub_value(ui);
                }
                Err(ToolError::NeedsInput {
                    question: r.scrub_string(question),
                    ui,
                })
            }
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

/// An environment failure as a tool error: worth retrying (it is not available now, too slow, lost),
/// or a report to the model (the repository's configuration is wrong, the build failed). The end of
/// a failed build's output is in the report.
pub(crate) fn environment_error(redactor: &Redactor, e: &EnvError) -> ToolError {
    // Journaled and shown to the model: a boundary, so the chain is flattened here, once.
    let mut text = format!("the work environment: {}", report(e));
    if let EnvError::Build { log_tail, .. } = e
        && !log_tail.trim().is_empty()
    {
        text.push('\n');
        text.push_str(log_tail.trim_end());
    }
    let text = redactor.scrub(&text).into_owned();
    if e.is_retryable() {
        ToolError::Transient(text)
    } else {
        ToolError::Permanent(text)
    }
}

/// A command that did not run, as a tool error.
pub(crate) fn run_error(redactor: &Redactor, e: &shell::RunError) -> ToolError {
    match e {
        shell::RunError::Prepare(e) => environment_error(redactor, e),
        shell::RunError::Spawn(e) => ToolError::Transient(format!("cannot start the shell: {e}")),
    }
}

/// Where [`Environment::ensure`](adam_workspace::Environment::ensure) reports its steps: a channel
/// the tool drains while it waits.
struct StepChannel(tokio::sync::mpsc::UnboundedSender<EnvStep>);

impl EnvProgress for StepChannel {
    fn step(&self, step: EnvStep) {
        // The tool stopped waiting (the run was cancelled): nobody needs the step.
        let _ = self.0.send(step);
    }
}

/// A failure to read or write the run's notes.
pub(crate) fn notes_error(e: &std::io::Error) -> ToolError {
    ToolError::Transient(format!("cannot access the run notes: {e}"))
}
