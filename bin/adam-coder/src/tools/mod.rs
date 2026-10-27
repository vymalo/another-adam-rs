//! The coder's tools, exactly the set the issue lists:
//!
//! | Tool | Module |
//! |---|---|
//! | `prepare_workspace { repo_url, base_branch?, branch? }` | [`prepare`] |
//! | `start_scratch { name? }`, `publish_scratch { repo_url, scratch?, base_branch?, path?, overwrite? }` | [`scratch`] |
//! | `request_repository { repo_url, reason }` | [`consent`] |
//! | `create_repository { owner, name, private?, description? }` | [`create`] |
//! | `run_command { command, cwd? }`: look around, changes undone | [`inspect`] |
//! | `run { command, cwd? }`: make something with a command, changes kept, never a check | [`make`] |
//! | `read_file { path, start_line?, end_line? }`, `write_file { path, content }`, `edit_file { path, old, new, replace_all? }`, `apply_patch { patch }` | [`files`] |
//! | `share_file { path, repo?, name? }` | [`share`]: a file of the workspace, shown to the person as a file artifact |
//! | `delegate_to_opencode { instructions }` | [`delegate`] |
//! | `run_checks { command }` | [`checks`] |
//! | `rebuild_environment { use_default? }` | [`environment`] |
//! | `commit_and_push { message }` / `open_pull_request { title, body }` | [`publish`] |
//! | `merge_pull_request { repo, number, method?, delete_branch? }` (tier `auto`), `rename_repository { repo, new_name }` and `delete_repository { repo, confirm }` (tier `bypass`) | [`admin`] |
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
//! * `start_scratch`: `RunWorkspace::add_scratch` returns the project the run already has under
//!   that name, with whatever is in it.
//! * `publish_scratch`: the repository's slot, once there, is found and not made again; an empty
//!   remote is given its first commit only while it has no ref at all, so a repeat that finds the
//!   first commit there goes on to the copy, which skips what is already the same.
//! * `commit_and_push`: `commit_all` is a no-op without changes and the push of
//!   a commit the remote already has is a no-op; the reported sha is `HEAD`.
//! * `open_pull_request`: moving the continued branch to the pushed commit is a no-op the second
//!   time, a pull request already open for the head is reported as it is (the run continued a
//!   branch an earlier task opened it for, or the call is repeated), and
//!   `CodeHost::open_pull_request` returns it instead of a second one in any case.
//! * `run_checks`: failures are counted per call id ([`notes`]).
//! * `run`: a command that was killed or never recorded runs again and makes the same files (it
//!   is for commands that can run twice, as every build is); it writes nothing to the notes.
//! * `edit_file`: the replacement is exact and the file is written whole, so a repeat after a
//!   crash finds `old` gone and says so (the result of the first call is what the model had).
//! * `share_file`: shares the file again and notes it once (`RunNotes::shared`).
//! * `rebuild_environment`: throwing the environment away and making it again twice is the same as once (the
//!   second call finds the first one's result and makes it once more, which costs time and nothing else); a
//!   choice of the default image is kept in the notes and in the environment's own state, and a repeat of it
//!   changes nothing.
//! * `request_repository { repo_url, reason }`: it only asks. A repeat after the person's yes finds the
//!   repository granted and says so; after a no it says so; with no answer yet it asks again.
//! * The seal that ends `prepare_workspace`, `start_scratch` and `publish_scratch`, and the agent's
//!   adoption of a conversation's scratch projects (`ToolEnv::seal`), is made every time and not only
//!   when a slot was made: a retry of a call that died between the slot and the seal seals, and an
//!   environment that has nothing to do does nothing.
//! * `merge_pull_request`: the host reports a pull request merged already as it is.
//! * `rename_repository`, `delete_repository`: noted in the run's notes once the host answered, and a
//!   repeat after that is the same answer ([`admin`]).
//! * `create_repository { owner, name, private?, description? }`: a repository made is recorded under
//!   its `owner/name` and a repeat returns the record. The call writes its intent to the run's notes
//!   before it asks the host, so a repeat after a crash between the host's answer and the record
//!   finds the name taken *and* the intent there: it looks the repository up, applies the same
//!   address policy and records it. Without the intent a taken name is left alone ([`create`]).
//!
//! # The rules, in code
//!
//! The prompt tells the model the rules; these make them hold: `prepare_workspace` and
//! `publish_scratch` refuse a repository that is not **granted**: one the person named ([`named`]; the agent records
//! the repositories of the person's own messages in the run notes before each step) or agreed to add
//! when `request_repository` asked ([`consent`]; the agreeing answer is recorded the same way, from
//! the conversation and never from the model). It continues a branch only if a
//! `commit_and_push` of the conversation recorded it in the notes, which the agent carries from
//! the run it continues, and as a fallback read from the result text, `publish::pushed_in`),
//! after `MAX_CHECK_CYCLES` failed check runs in repositories (`SCRATCH_CHECK_CYCLES` in scratch projects, counted apart) `run_checks` refuses to run, and
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
use adam_a2a::AccessTier;
use adam_error::{Classify, report};
use adam_model::ToolSpec;
use adam_ui::Ui;
use adam_workspace::{
    DynCodeHost, DynEnvironment, EnvError, EnvProgress, EnvSession, EnvStep, EnvStepState,
    GitIdentity, Local, RunWorkspace, Slot, WorkspaceError, Workspaces,
};
use serde_json::Value;

use crate::opencode::OpenCodeLaunch;
use crate::redact::Redactor;

/// What the description of `ask_user` opens with: the coder's own words about when to ask.
const ASK_LEAD: &str = "Ask the person who gave you the task a question and wait for the answer. Use it only when you cannot proceed without it, or to get explicit consent (for example to open a pull request with failing checks). Be specific.";

pub mod admin;
pub mod checks;
pub mod consent;
pub mod create;
pub mod delegate;
pub mod environment;
pub mod files;
pub(crate) mod gitcli;
pub mod inspect;
pub mod make;
pub mod named;
pub mod notes;
mod preexisting;
pub mod prepare;
pub mod publish;
pub mod scratch;
pub mod share;
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

/// The first pause between two tries for an environment that is not available now, doubled each time.
const SLOT_PAUSE_START: Duration = Duration::from_secs(2);

/// The longest pause between two such tries.
const SLOT_PAUSE_MAX: Duration = Duration::from_secs(30);

/// Why [`ToolEnv::session`] did not get a session from one `ensure`.
enum Ensure {
    /// The run was cancelled while it waited.
    Cancelled(ToolError),
    /// The environment said no.
    Failed(EnvError),
}

/// Tunables of the tools.
#[derive(Debug, Clone)]
pub struct CoderSettings {
    /// Failed `run_checks` calls in repositories after which the agent must stop.
    pub max_check_cycles: u32,
    /// The same for scratch projects, which are counted apart: building something new takes more
    /// tries than a change to a project that has checks of its own.
    pub scratch_check_cycles: u32,
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
    /// The owners `create_repository` may create repositories for, lowercased: `CREATE_REPO_OWNERS`.
    /// A list restricts to exactly these owners. Empty: the owners the credentials act for when
    /// [`create_for_installed_owners`](Self::create_for_installed_owners) (a GitHub App's
    /// installations), else the tool is not offered and refuses every call.
    pub create_repo_owners: Vec<String>,
    /// With no `CREATE_REPO_OWNERS`, `create_repository` is on for the accounts the credentials act
    /// for ([`Workspaces::acts_for`]): a GitHub App's installations. Set by the process for an App,
    /// left off for a token, which has no list of accounts and keeps the tool off until the
    /// variable names owners.
    pub create_for_installed_owners: bool,
    /// The highest access tier a run may work under, whatever a message asks for
    /// (`MAX_ACCESS_TIER`): the tools check it against the tier the notes hold. The default is
    /// [`DEFAULT_MAX`](crate::tier::DEFAULT_MAX), `auto`: renaming and deleting a repository are
    /// the deployment's opt-in.
    pub max_access_tier: AccessTier,
    /// The devcontainers have no network (`DEVCONTAINER_NETWORK=none`): OpenCode, inside one,
    /// cannot reach its model, so `delegate_to_opencode` refuses there and says what to use instead.
    pub container_network_none: bool,
}

impl CoderSettings {
    /// The check-cycle budget of one kind of work: a scratch project's, or a repository's.
    pub fn cycle_limit(&self, scratch: bool) -> u32 {
        if scratch {
            self.scratch_check_cycles
        } else {
            self.max_check_cycles
        }
    }

    /// Whether `create_repository` is offered at all: some owner is listed, or the credentials say
    /// which accounts they act for.
    pub fn can_create_repositories(&self) -> bool {
        !self.create_repo_owners.is_empty() || self.create_for_installed_owners
    }

    /// Defaults: 3 check cycles in a repository and 5 in a scratch project, 15 minutes and 16 KiB per check run, the
    /// `adam-coder` identity, ready-for-review pull requests, `github.com` for `owner/name`, and no
    /// owner a repository may be created for.
    pub fn new(opencode: OpenCodeLaunch) -> Self {
        Self {
            max_check_cycles: 3,
            scratch_check_cycles: 5,
            check_timeout: Duration::from_secs(900),
            check_output_tail: 16 * 1024,
            identity: GitIdentity::new("adam-coder", "adam-coder@users.noreply.github.com"),
            draft_pull_requests: false,
            opencode,
            default_repo_host: named::DEFAULT_HOST.to_owned(),
            create_repo_owners: Vec::new(),
            create_for_installed_owners: false,
            max_access_tier: crate::tier::DEFAULT_MAX,
            container_network_none: false,
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
    /// How long a run waits for an environment that is **not available now** (the namespace's quota of
    /// run pods is used up, no node has room, the API does not answer), trying again with a growing
    /// pause and saying so as a step, before the tool fails. `None` (the default, and every
    /// environment but run pods): the first `EnvError::Unavailable` is the tool's result at once.
    pub unavailable_wait: Option<Duration>,
    /// What a run that ends on rejected GitHub credentials says to check: the variables of the kind
    /// of credentials the process has (a token, or a GitHub App). `GITHUB_TOKEN` until
    /// [`ToolEnv::with_credentials_hint`].
    pub credentials_hint: &'static str,
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
            unavailable_wait: None,
            credentials_hint: "GITHUB_TOKEN is valid and may push and open pull requests for the repository",
        }
    }

    /// Say `hint` when the GitHub credentials are rejected, as the end of "check that ...": see
    /// [`GitHubAuth::check_hint`](crate::GitHubAuth::check_hint).
    #[must_use]
    pub fn with_credentials_hint(mut self, hint: &'static str) -> Self {
        self.credentials_hint = hint;
        self
    }

    /// Run the processes of runs in `environment` instead of this container. The janitor of the
    /// process ([`Janitor::with_environment`](crate::Janitor::with_environment)) must be given the
    /// same one, so that what it holds for a run is released when the run's workspace is.
    #[must_use]
    pub fn with_environment(mut self, environment: DynEnvironment) -> Self {
        self.environment = environment;
        self
    }

    /// Wait up to `wait` for an environment that is not available now, instead of failing at the
    /// first [`EnvError::Unavailable`] (`RUN_POD_WAIT_SECS`: a run waits for a slot, it does not fall
    /// back to this container and does not fail at once). The wait is where a tool asks for its run's session.
    #[must_use]
    pub fn with_unavailable_wait(mut self, wait: Duration) -> Self {
        self.unavailable_wait = Some(wait);
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
            let run = ctx.root_run_id().to_string();
            let recorded = async {
                let mut notes = self.notes.load(&run).await?;
                notes.blocker = Some(format!(
                    "the credentials were rejected ({e}); check that {}",
                    self.credentials_hint
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

    /// Tell the run's environment about a slot the caller has just added or found
    /// ([`Environment::seal`](adam_workspace::Environment::seal)).
    ///
    /// The file and git tools act on a slot by name and never call
    /// [`session`](Self::session), so an environment that shows the run its slots as places the run
    /// cannot rename (a run pod mounts each slot on its own) must learn of a new one now and not at
    /// the next command. The slot stays whatever the answer. A seal that cannot be done now (a
    /// command runs in the pod) is logged and returned as a line for the tool's result, scrubbed like
    /// every environment error: the next command waits for that command to end and makes the pod
    /// again. `None` when it is done, and when the run was cancelled meanwhile.
    pub(crate) async fn seal(&self, ctx: &ToolCtx, workspace: &RunWorkspace) -> Option<String> {
        let sealed = tokio::select! {
            biased;
            () = ctx.cancelled() => return None,
            sealed = self.environment.seal(workspace) => sealed,
        };
        let e = sealed.err()?;
        tracing::warn!(run = %workspace.run(), error = %report(&e), "cannot seal the run's environment for a slot that was added");
        let said = match environment_error(&self.redactor, &e) {
            ToolError::Transient(text) | ToolError::Permanent(text) => text,
            other => report(&other),
        };
        Some(format!(
            "Note: the slot is made, but the run's pod does not mount it yet ({said}). The next \
             command waits for the commands running in the pod to end and makes the pod again."
        ))
    }

    /// The session of the run's environment, for a tool that runs a process: made on the first
    /// need and the same after ([`Environment::ensure`](adam_workspace::Environment::ensure)).
    ///
    /// What the environment says while it makes one (pulling an image, building it) is shown as
    /// steps under the tool call, `env:<run>:<step>`, scrubbed like everything else the coder
    /// shows. A cancel of the run stops the wait. Needs no workspace: it is the run's, whether or
    /// not a slot is there yet.
    ///
    /// An environment that is **not available now** ([`EnvError::Unavailable`]) is waited for when
    /// [`ToolEnv::unavailable_wait`] is set (run pods: the quota is used up): the call is tried again
    /// after 2, 4, 8, ... up to 30 seconds, until the wait is over, and the step `env:<run>:slot` says
    /// the run waits for a slot. Without it the first such error is the tool's result, as it always
    /// was (the devcontainer's behaviour is unchanged).
    pub(crate) async fn session(&self, ctx: &ToolCtx) -> Result<Arc<dyn EnvSession>, ToolError> {
        // The workspace is the root run's, so a subagent works in its parent's environment; the
        // steps it shows are the call's own, under the id of the run that makes them.
        let workspace = self
            .workspaces
            .run(&ctx.root_run_id().to_string())
            .map_err(|e| workspace_error(&e))?;
        let run = ctx.run_id().to_string();
        let started = std::time::Instant::now();
        let mut pause = SLOT_PAUSE_START;
        let mut waiting = false;
        loop {
            match self.ensure_once(ctx, &run, &workspace).await {
                Ok(session) => {
                    if waiting {
                        self.show_slot(ctx, &run, StepState::Completed, "a slot is free")
                            .await;
                    }
                    return Ok(session);
                }
                Err(Ensure::Cancelled(e)) => return Err(e),
                // Not available now, and this deployment says to wait for it: the run is not lost
                // and does not go on elsewhere, it waits for a slot, with a step that says so.
                Err(Ensure::Failed(EnvError::Unavailable(why)))
                    if self
                        .unavailable_wait
                        .is_some_and(|limit| started.elapsed() < limit) =>
                {
                    let limit = self.unavailable_wait.unwrap_or_default();
                    let left = limit.saturating_sub(started.elapsed());
                    let detail = format!(
                        "{why}; trying again in {} s, for up to {} s more",
                        pause.as_secs(),
                        left.as_secs()
                    );
                    waiting = true;
                    self.show_slot(ctx, &run, StepState::Running, &detail).await;
                    tokio::select! {
                        biased;
                        () = ctx.cancelled() => {
                            return Err(cancelled("the environment of the run was not made"));
                        }
                        () = tokio::time::sleep(pause.min(left)) => {}
                    }
                    pause = (pause * 2).min(SLOT_PAUSE_MAX);
                }
                Err(Ensure::Failed(e)) => {
                    if waiting {
                        let detail = format!("no slot after {} s", started.elapsed().as_secs());
                        self.show_slot(ctx, &run, StepState::Failed, &detail).await;
                    }
                    return Err(session_error(&self.redactor, &e));
                }
            }
        }
    }

    /// One `ensure`, with the steps it reports shown as it goes. A cancel of the run stops the wait.
    async fn ensure_once(
        &self,
        ctx: &ToolCtx,
        run: &str,
        workspace: &adam_workspace::RunWorkspace,
    ) -> Result<Arc<dyn EnvSession>, Ensure> {
        let (steps, mut reported) = tokio::sync::mpsc::unbounded_channel();
        let progress = StepChannel(steps);
        let ensure = self.environment.ensure(workspace, &progress);
        tokio::pin!(ensure);
        let made = loop {
            tokio::select! {
                biased;
                () = ctx.cancelled() => {
                    return Err(Ensure::Cancelled(cancelled(
                        "the environment of the run was not made",
                    )));
                }
                Some(step) = reported.recv() => self.show_step(ctx, run, step).await,
                made = &mut ensure => break made,
            }
        };
        while let Ok(step) = reported.try_recv() {
            self.show_step(ctx, run, step).await;
        }
        made.map_err(Ensure::Failed)
    }

    /// The step that says the run waits for a slot for its environment.
    async fn show_slot(&self, ctx: &ToolCtx, run: &str, state: StepState, detail: &str) {
        let event = StepEvent::new(
            format!("env:{run}:slot"),
            StepKind::Command,
            "Waiting for a slot for the run's environment",
            state,
        )
        .with_icon(StepIcon::Execute)
        .with_detail(self.redactor.scrub(detail));
        ctx.report_step(event).await;
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

    /// The owners `create_repository` may create repositories for now: the list of
    /// `CREATE_REPO_OWNERS` when there is one (it restricts), else the accounts the credentials act
    /// for when the deployment says to use them (a GitHub App's installations), else none.
    ///
    /// # Errors
    ///
    /// The credentials' error when they could not be asked.
    pub(crate) async fn creation_owners(&self) -> Result<Vec<String>, WorkspaceError> {
        if !self.settings.create_repo_owners.is_empty() {
            return Ok(self.settings.create_repo_owners.clone());
        }
        if !self.settings.create_for_installed_owners {
            return Ok(Vec::new());
        }
        Ok(self.workspaces.acts_for().await?.unwrap_or_default())
    }

    /// The access tier the run with `notes` works under now: the one the agent noted for the newest
    /// message, never above what the deployment allows ([`CoderSettings::max_access_tier`]).
    pub(crate) fn tier_of(&self, notes: &RunNotes) -> AccessTier {
        notes.tier().min(self.settings.max_access_tier)
    }

    /// The one owner a bare repository name belongs to, if there is exactly one: the owner of the
    /// repositories already in the run's workspace (or created by the run), else the one account the
    /// credentials act for (a GitHub App installed on one account). Two owners, or none, are
    /// `None`: a bare name then needs the person's word, as ever.
    async fn sole_owner(&self, ctx: &ToolCtx, notes: &RunNotes) -> Option<String> {
        let mut owners = std::collections::BTreeSet::new();
        if let Ok(workspace) = self.workspaces.run(&ctx.root_run_id().to_string())
            && let Ok(slots) = workspace.slots().await
        {
            for slot in &slots {
                if let Some(wt) = slot.worktree()
                    && let Ok(loc) = wt.repo().locate()
                    && !loc.is_local()
                {
                    owners.insert(loc.owner.to_ascii_lowercase());
                }
            }
        }
        for created in &notes.created_repos {
            if let Some((owner, _)) = created.full_name.split_once('/') {
                owners.insert(owner.to_ascii_lowercase());
            }
        }
        match owners.len() {
            1 => owners.into_iter().next(),
            0 => match self.workspaces.acts_for().await {
                Ok(Some(accounts)) if accounts.len() == 1 => {
                    accounts.into_iter().next().map(|a| a.to_ascii_lowercase())
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether the repository with `key` is granted to the run under the tier it works under:
    /// the person named it or agreed to add it (or the run created it); or, from `auto` up, the
    /// person wrote its name alone and the owner is the one owner that applies ([`ToolEnv::sole_owner`]),
    /// no form needed; or, at `bypass`, whatever the host allows.
    pub(crate) async fn is_granted(&self, ctx: &ToolCtx, notes: &RunNotes, key: &str) -> bool {
        if notes.named_repos.iter().any(|named| named == key) {
            return true;
        }
        match self.tier_of(notes) {
            AccessTier::Plan | AccessTier::Edits => false,
            AccessTier::Bypass => true,
            AccessTier::Auto => {
                let Some((host, owner, name)) = named::split_key(key) else {
                    return false;
                };
                host == named::default_host_key(&self.settings.default_repo_host)
                    && notes.said(name)
                    && self
                        .sole_owner(ctx, notes)
                        .await
                        .is_some_and(|sole| sole == owner)
            }
        }
    }

    /// What a call is called in the run notes (`record_missing_tool`): the model's call id, which is
    /// only unique within the run that made it, so a subagent's carries its own run.
    pub(crate) fn call_key(ctx: &ToolCtx) -> String {
        if ctx.run_id() == ctx.root_run_id() {
            ctx.call_id().to_owned()
        } else {
            format!("{}/{}", ctx.run_id(), ctx.call_id())
        }
    }

    /// The slot of the run's workspace that a tool acts in, or the message to give the model: it
    /// has no workspace yet, or it did not say which of several slots (see [`resolve_slot`]).
    ///
    /// `repo` is what the model passed: the slot's directory (`sandbox`) or the address of the
    /// repository the slot holds. It may be left out when the workspace has one slot.
    pub(crate) async fn slot(&self, ctx: &ToolCtx, repo: Option<&str>) -> Result<Slot, Outcome> {
        let run = ctx.root_run_id().to_string();
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
        return Err(
            "there is no workspace yet: call prepare_workspace (a repository the person \
             named) or start_scratch (a project to build before there is a repository) first"
                .to_owned(),
        );
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

/// Every coder tool, in the order they are offered to the model: the twenty of the coding workflow,
/// then the screen's (`ask_user`, `show`, `ui_catalog`, from [`ToolEnv::ui`]).
///
/// `create_repository` is offered only when the deployment lets it create repositories for someone
/// ([`CoderSettings::can_create_repositories`]: `CREATE_REPO_OWNERS` names owners, or the credentials
/// are a GitHub App, whose installations are the owners): otherwise the tool is not in the set
/// (nineteen of the workflow), so the model cannot offer a new repository to the person or call it
/// with an owner of its own making. The agent says so in its prompt ([`CoderAgent`](crate::CoderAgent)
/// fills the `repository_creation` var). A folder whose `tools:` names `create_repository` is
/// refused as an unknown tool while nobody is allowed. The settings are read when this is called.
///
/// The set is the same at every access tier: which tools the model is **shown** on a turn, and which
/// calls are **refused**, is [`tier`](crate::tier)'s ([`TierSource`](crate::TierSource)), and every
/// tool here also checks the tier of the run's notes itself when it is called (`Gated`).
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
    let before = tools![
        prepare::PrepareWorkspace,
        scratch::StartScratch,
        scratch::PublishScratch,
        consent::RequestRepository,
    ];
    let creating = if env.settings.can_create_repositories() {
        ToolSet::new().tool(create::CreateRepository)
    } else {
        ToolSet::new()
    };
    let after = tools![
        inspect::RunCommand,
        make::Run,
        files::ReadFile,
        files::WriteFile,
        files::ApplyPatch,
        files::EditFile,
        share::ShareFile,
        delegate::DelegateToOpenCode,
        checks::RunChecks,
        environment::RebuildEnvironment,
        publish::CommitAndPush,
        publish::OpenPullRequest,
        admin::MergePullRequest,
        admin::RenameRepository,
        admin::DeleteRepository,
    ];
    before
        .extend(creating)
        .extend(after)
        .extend(env.ui.tools())
        .wrap(Gated::layer(env.clone()))
        .wrap(Redacting::layer(env.redactor.clone()))
}

/// A tool that refuses a call its run's access tier does not allow.
///
/// The tier is the one the agent noted for the newest message (`RunNotes::tier`), read from the notes
/// of the **root** run: so a subagent, whose own context holds no tier, keeps to the tier of the run
/// it serves. This is the check at the moment of the call; the model is not shown the tools above the
/// tier either ([`TierSource`](crate::TierSource)).
struct Gated {
    inner: DynTool,
    name: String,
    needs: AccessTier,
    env: Arc<ToolEnv>,
}

impl Gated {
    /// The wrapper for [`ToolSet::wrap`](adam::ToolSet::wrap).
    fn layer(env: Arc<ToolEnv>) -> impl FnMut(DynTool) -> DynTool {
        move |inner| {
            let name = inner.spec().name;
            Arc::new(Self {
                needs: crate::tier::needs(&name),
                name,
                inner,
                env: env.clone(),
            })
        }
    }
}

#[async_trait::async_trait]
impl Tool for Gated {
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
        // Every tier allows what needs the lowest one: no read of the notes for a look.
        if self.needs > AccessTier::Plan {
            let run = ctx.root_run_id().to_string();
            // No notes of the root run: nothing recorded a tier, so nothing above a plan is allowed.
            let tier = self
                .env
                .notes
                .load_existing(&run)
                .await
                .map_err(|e| notes_error(&e))?
                .map_or(AccessTier::Plan, |notes| self.env.tier_of(&notes));
            if self.needs > tier {
                return Ok(ToolOutput::error(crate::tier::refusal(
                    &self.name, self.needs, tier,
                )));
            }
        }
        self.inner.call(ctx, args).await
    }
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
                    // A text file may hold what the process knows to hide (a token a command
                    // printed into it); a file that is not text has no string to match.
                    if let Some(file) = &mut artifact.file
                        && let Ok(text) = std::str::from_utf8(&file.bytes)
                        && let std::borrow::Cow::Owned(scrubbed) = r.scrub(text)
                    {
                        file.bytes = scrubbed.into_bytes();
                    }
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

/// What the model is told when the environment of the run cannot be made and will not be made by
/// asking again (a repository's `devcontainer.json` that is wrong, refused, or does not build).
const BROKEN_ENVIRONMENT: &str = "\n\nThe environment is broken and stays broken until the person \
decides, so this is not something to work around: do not run the command another way, and do not \
look for the tool elsewhere. Ask the person with ask_user, in plain words, what is wrong and what \
you can do: they can fix the file in the repository (then you call rebuild_environment), or you can \
go on in the default environment (rebuild_environment with use_default: true). Call \
rebuild_environment only after they have answered.";

/// The failure of [`ToolEnv::session`]: [`environment_error`], and for an environment that cannot be
/// made at all (its file is wrong, refused or does not build; a phase that ran out of time, which
/// the environment keeps as broken) a permanent error that says what to do: ask the person.
pub(crate) fn session_error(redactor: &Redactor, e: &EnvError) -> ToolError {
    let broken = matches!(
        e,
        EnvError::Config { .. }
            | EnvError::Refused(_)
            | EnvError::Build { .. }
            | EnvError::Timeout { .. }
    );
    if !broken {
        return environment_error(redactor, e);
    }
    let mut text = match environment_error(redactor, e) {
        ToolError::Transient(text) | ToolError::Permanent(text) => text,
        other => report(&other),
    };
    text.push_str(BROKEN_ENVIRONMENT);
    ToolError::Permanent(text)
}

/// The file a devcontainer was built from, as the repository calls it: the part of `path` that starts
/// at `.devcontainer` (`.devcontainer/devcontainer.json`, `.devcontainer.json`).
pub(crate) fn repository_file(path: &std::path::Path) -> String {
    let names: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match names.iter().position(|n| n.starts_with(".devcontainer")) {
        Some(at) => names[at..].join("/"),
        None => path.display().to_string(),
    }
}

/// A command that did not run, as a tool error.
pub(crate) fn run_error(redactor: &Redactor, e: &shell::RunError) -> ToolError {
    match e {
        shell::RunError::Prepare(e) => environment_error(redactor, e),
        shell::RunError::Spawn(e) => ToolError::Transient(format!("cannot start the shell: {e}")),
        shell::RunError::Cancelled => cancelled("the command was stopped"),
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

#[cfg(test)]
mod grant_tests {
    //! Which repository a run may work on under the tier it works under (`ToolEnv::is_granted`): the
    //! person's words, or from `auto` a name written alone when exactly one owner applies, or at
    //! `bypass` any repository the host allows.

    use adam_runtime::CollectingSink;
    use adam_workspace::{GitCredentials, MemoryCodeHost, RepoRef, ScopedToken, WorkspaceError};
    use secrecy::SecretString;

    use super::*;
    use crate::opencode::OpenCodeLaunch;
    use crate::tools::notes::CreatedRepo;

    /// Credentials that act for `accounts`, or for no list (a token) when `None`.
    struct Accounts(Option<Vec<String>>);

    #[async_trait::async_trait]
    impl GitCredentials for Accounts {
        async fn token_for(&self, repo: &RepoRef) -> Result<SecretString, WorkspaceError> {
            ScopedToken::new("github.com", "t").token_for(repo).await
        }

        async fn acts_for(&self) -> Result<Option<Vec<String>>, WorkspaceError> {
            Ok(self.0.clone())
        }
    }

    struct Rig {
        env: Arc<ToolEnv>,
        ctx: ToolCtx,
        _root: tempfile::TempDir,
    }

    fn rig(accounts: Option<&[&str]>) -> Rig {
        let root = tempfile::tempdir().unwrap();
        let creds = Arc::new(Accounts(
            accounts.map(|a| a.iter().map(|s| (*s).to_owned()).collect()),
        ));
        let workspaces =
            Workspaces::new(root.path().join("work"), creds).allow_hosts(["github.com"]);
        let mut settings =
            CoderSettings::new(OpenCodeLaunch::opencode("http://gateway.invalid/v1", "m"));
        // These tests are about the tier in the notes: the deployment lets a person pick any.
        settings.max_access_tier = AccessTier::Bypass;
        let env = Arc::new(ToolEnv::new(
            workspaces,
            Arc::new(MemoryCodeHost::new()),
            settings,
        ));
        let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
            .with_state(env.clone());
        Rig {
            env,
            ctx,
            _root: root,
        }
    }

    fn notes(tier: AccessTier, said: &str) -> RunNotes {
        let mut notes = RunNotes {
            tier: Some(tier),
            ..RunNotes::default()
        };
        notes.set_said_words(said);
        notes
    }

    async fn granted(rig: &Rig, notes: &RunNotes, key: &str) -> bool {
        rig.env.is_granted(&rig.ctx, notes, key).await
    }

    #[tokio::test]
    async fn a_repository_the_person_named_is_granted_at_every_tier() {
        let rig = rig(None);
        for tier in AccessTier::ALL {
            let mut notes = notes(tier, "");
            notes.name_repos(["github.com/acme/widgets".to_owned()]);
            assert!(
                granted(&rig, &notes, "github.com/acme/widgets").await,
                "{tier}"
            );
        }
    }

    #[tokio::test]
    async fn a_bare_name_counts_as_named_from_auto_when_one_owner_applies() {
        // The App is installed on one account: that is the owner of a bare name.
        let rig = rig(Some(&["acme"]));
        let said = "fix the typo in widgets";
        // Plan and edits keep the form: a bare name does not count as named there.
        for tier in [AccessTier::Plan, AccessTier::Edits] {
            assert!(
                !granted(&rig, &notes(tier, said), "github.com/acme/widgets").await,
                "{tier}"
            );
        }
        for tier in [AccessTier::Auto, AccessTier::Bypass] {
            let notes = notes(tier, said);
            assert!(
                granted(&rig, &notes, "github.com/acme/widgets").await,
                "{tier}"
            );
        }
        // Under auto only the name the person wrote, only on that owner, only on the default host.
        let auto = notes(AccessTier::Auto, said);
        assert!(
            !granted(&rig, &auto, "github.com/acme/other").await,
            "not written"
        );
        assert!(
            !granted(&rig, &auto, "github.com/evil/widgets").await,
            "another owner"
        );
        assert!(
            !granted(&rig, &auto, "ghe.example.com/acme/widgets").await,
            "another host"
        );
        assert!(
            !granted(&rig, &auto, "local/abc/widgets").await,
            "a local repository"
        );
        // A word is read without case, and is the name and not a part of it.
        assert!(
            granted(
                &rig,
                &notes(AccessTier::Auto, "WIDGETS"),
                "github.com/acme/widgets"
            )
            .await
        );
        assert!(
            !granted(
                &rig,
                &notes(AccessTier::Auto, "widget"),
                "github.com/acme/widgets"
            )
            .await
        );
    }

    #[tokio::test]
    async fn two_owners_or_none_leave_a_bare_name_to_the_form() {
        let said = "fix widgets";
        // The App is on two accounts: which one is not for the agent to choose.
        let two = rig(Some(&["acme", "other"]));
        assert!(
            !granted(
                &two,
                &notes(AccessTier::Auto, said),
                "github.com/acme/widgets"
            )
            .await
        );
        // A token has no list of accounts, and the workspace has no repository yet.
        let token = rig(None);
        assert!(
            !granted(
                &token,
                &notes(AccessTier::Auto, said),
                "github.com/acme/widgets"
            )
            .await
        );
        // The owner of a repository this run created is the owner then.
        let mut with_created = notes(AccessTier::Auto, said);
        with_created.created_repos.push(CreatedRepo {
            full_name: "acme/fib".into(),
            key: "github.com/acme/fib".into(),
            clone_url: "https://github.com/acme/fib.git".into(),
            html_url: "https://github.com/acme/fib".into(),
            private: true,
        });
        assert!(granted(&token, &with_created, "github.com/acme/widgets").await);
        assert!(!granted(&token, &with_created, "github.com/other/widgets").await);
        // Two owners among what the run has: no owner applies.
        with_created.created_repos.push(CreatedRepo {
            full_name: "other/lib".into(),
            key: "github.com/other/lib".into(),
            clone_url: "https://github.com/other/lib.git".into(),
            html_url: "https://github.com/other/lib".into(),
            private: true,
        });
        assert!(!granted(&token, &with_created, "github.com/acme/widgets").await);
    }

    #[tokio::test]
    async fn bypass_grants_any_repository_the_host_allows_and_the_other_tiers_none() {
        let rig = rig(None);
        let key = "github.com/anyone/anything";
        assert!(granted(&rig, &notes(AccessTier::Bypass, ""), key).await);
        for tier in [AccessTier::Plan, AccessTier::Edits, AccessTier::Auto] {
            assert!(
                !granted(&rig, &notes(tier, "anything"), key).await,
                "{tier}"
            );
        }
    }

    #[test]
    fn only_the_words_that_stand_alone_in_the_newest_message_could_be_a_repository() {
        let mut notes = RunNotes::default();
        assert!(notes.set_said_words("Fix the typo in Widgets, not in api.v2!"));
        assert!(notes.said("widgets") && notes.said("API.v2") && notes.said("typo"));
        // A single letter is no repository's name, and punctuation is not part of a word.
        assert!(!notes.said("a") && !notes.said("widgets,"));
        // The same message again changes nothing.
        assert!(!notes.set_said_words("Fix the typo in Widgets, not in api.v2!"));
        // The next message replaces the words: what an older one said no longer stands.
        assert!(notes.set_said_words("thanks, now gadgets"));
        assert!(notes.said("gadgets") && !notes.said("widgets") && !notes.said("typo"));
        // A piece of a URL, a path or a query is not a word of its own.
        notes.set_said_words(
            "see https://github.com/acme/widgets/issues/3, src/gadgets/main.rs, \
             github.com/acme/sprockets?name=cogs and git@github.com:acme/bolts.git",
        );
        for fragment in [
            "widgets",
            "gadgets",
            "sprockets",
            "cogs",
            "bolts",
            "acme",
            "github",
            "main.rs",
            "issues",
        ] {
            assert!(!notes.said(fragment), "{fragment}");
        }
        assert!(notes.said("see") && notes.said("and"));
        // Quotes, backticks and a closing full stop around a word are not part of it.
        notes.set_said_words("use `widgets`, (gadgets) or \"cogs\".");
        assert!(notes.said("widgets") && notes.said("gadgets") && notes.said("cogs"));
        // The list is bounded: the first words stay and the newest are not added.
        let many: Vec<String> = (0..crate::tools::notes::MAX_SAID_WORDS + 50)
            .map(|n| format!("word{n}"))
            .collect();
        let mut full = RunNotes::default();
        full.set_said_words(&many.join(" "));
        assert_eq!(full.said_words.len(), crate::tools::notes::MAX_SAID_WORDS);
        assert!(
            full.said("word0")
                && !full.said(&format!("word{}", crate::tools::notes::MAX_SAID_WORDS + 10))
        );
    }
}
