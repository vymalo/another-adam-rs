//! The coder's tools, exactly the set the issue lists:
//!
//! | Tool | Module |
//! |---|---|
//! | `prepare_workspace { repo_url, base_branch?, branch? }` | [`prepare`] |
//! | `start_scratch { name? }`, `publish_scratch { repo_url, scratch?, base_branch?, path?, overwrite? }` | [`scratch`] |
//! | `request_repository { repo_url, reason, reference? }` | [`consent`]; with `reference`, to read a repository a prepared repository declared, and never change it ([`references`]) |
//! | `create_repository { owner, name, private?, description? }` | [`create`] |
//! | `run_command { command, cwd?, repo?, timeout? }`: look around, changes undone | [`inspect`] |
//! | `run { command, cwd?, repo?, timeout? }`: make something with a command, changes kept, never a check | [`make`] |
//! | `start_command { command, cwd?, repo?, wait?, wait_for? }`, `stop_command { id }`, `list_commands {}`: a command that keeps running (a development server), read with `read_output`, gone with its run | [`background`] |
//! | `read_output { id, offset?, limit? }`: a kept output (a command's, or a long MCP result), paged | `adam_llm_agent::ReadOutput`, kept by [`kept`](crate::kept) |
//! | `list_files { path?, offset?, limit? }`, `find_files { pattern, path?, offset?, limit? }`, `search_files { pattern, path?, include?, literal?, offset?, limit? }`: paged, git-aware looking around | [`find`] |
//! | `read_file { path, offset?, limit? }` (paged; `start_line` and `end_line` are aliases), `write_file { path, content }`, `edit_file { path, old, new, replace_all? }`, `apply_patch { patch }` | [`files`] |
//! | `file_diagnostics { path, repo?, wait?, offset?, limit? }`, `find_definition { path, line, symbol, occurrence?, repo?, wait? }`, `find_references { path, line, symbol, occurrence?, include_declaration?, repo?, wait?, offset?, limit? }`: what a language server says about a file; read only, kept between calls | [`lsp`] |
//! | `share_file { path, repo?, name? }` | [`share`]: a file of the workspace, shown to the person as a file artifact |
//! | `run_checks { command }` | [`checks`] |
//! | `rebuild_environment { use_default? }` | [`environment`] |
//! | `commit_and_push { message }` / `open_pull_request { title, body }` | [`publish`] |
//! | `merge_pull_request { repo, number, method?, delete_branch? }` (tier `auto`), `rename_repository { repo, new_name }` and `delete_repository { repo, confirm }` (tier `bypass`) | [`admin`] |
//! | `ask_user { question, choices? }`, `show { blocks, title? }`, `ui_catalog {}`, `plan_work { title?, items }`, `plan_check { id, status, note? }`, `plan_add { items, after? }` | [`adam_ui`]: the person's screen as tools, and the plan |
//!
//! A tool that takes `repo` acts in a slot of the run's workspace, and a slot may be a **reference**: read, never written ([`references`]). Every tool that only looks asks `ToolEnv::slot_to_read`; every tool that changes anything asks `ToolEnv::slot_to_write`, which refuses a reference.
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
use adam::{
    AnswerCheck, DynTool, StateKey, StepEvent, StepIcon, StepKind, StepState, StepStyle, ToolAccess,
};
use adam_a2a::AccessTier;
use adam_error::{Classify, report};
use adam_model::ToolSpec;
use adam_ui::Ui;
use adam_workspace::{
    DynCodeHost, DynEnvironment, EnvError, EnvProgress, EnvSession, EnvStep, EnvStepState,
    ExecSpec, GitIdentity, Local, RunWorkspace, Slot, WorkspaceError, Workspaces,
};
use serde_json::Value;

use crate::images::FileImages;
use crate::kept::FileKept;
use crate::redact::Redactor;
use sandbox::Meter;

/// What the description of `ask_user` opens with: the coder's own words about when to ask.
const ASK_LEAD: &str = "Ask the person who gave you the task a question and wait for the answer. Use it only when you cannot proceed without it, or to get explicit consent (for example to open a pull request with failing checks). Be specific.";

pub mod admin;
pub mod background;
pub mod checks;
pub mod consent;
pub mod create;
pub mod environment;
pub mod files;
pub mod find;
pub(crate) mod gitcli;
pub mod inspect;
pub mod lsp;
pub mod make;
pub mod named;
pub mod notes;
mod output;
mod preexisting;
pub mod prepare;
mod project;
pub mod publish;
pub mod references;
mod sandbox;
pub mod scratch;
pub mod share;
pub mod shell;
pub mod skills;

pub use background::{BACKGROUND_TOOLS, Commands, CommandsSweep};
pub use lsp::{LANGUAGE_SERVER_TOOLS, Language, ServerSpec, ServerTable};
pub use notes::{NotesStore, RunNotes};
pub use references::{References, ReferencesError, ReferencesSource};

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
    /// Commands of one run that keep running at once (`BACKGROUND_COMMANDS_MAX`): `start_command` refuses
    /// the next.
    pub background_commands_max: usize,
    /// How long a command that keeps running may run (`BACKGROUND_COMMAND_MAX_SECS`), whatever else.
    pub background_command_max: Duration,
    /// The most bytes of one command's output that are kept on the volume
    /// (`adam_llm_agent::MAX_KEPT_BYTES`, 16 MiB): the file stops there, with the line `read_output`
    /// shows for every cut output. A setting of its own so that a test reaches it with a little text.
    pub background_output_max: u64,
    /// How often the sweep looks at the runs that have commands (`BACKGROUND_SWEEP_SECS`): a cancel
    /// never calls the agent, so this is how long a cancelled run's commands can outlive it.
    pub background_sweep: Duration,
    /// How long a call to the environment's `kill` of a command's exec id is waited for.
    pub background_kill_wait: Duration,
    /// The most bytes of output one run keeps on the volume ([`crate::kept::RUN_KEPT_MAX`]); a
    /// setting so that a test reaches it with a little text.
    pub run_kept_max: u64,
    /// The longest `ToolEnv::session` can take to make the run's environment ready: nothing for the
    /// coder's own container; for a devcontainer `DEVCONTAINER_UP_TIMEOUT_SECS` plus
    /// `DEVCONTAINER_SETUP_TIMEOUT_SECS`; for run pods `RUN_POD_WAIT_SECS` plus
    /// `RUN_POD_READY_TIMEOUT_SECS`. The time limit of a tool that makes the environment is this and
    /// whatever else it does.
    pub environment_start: Duration,
    /// Whether the three language server tools are offered (`LANGUAGE_SERVERS`, `on` by default).
    pub language_servers: bool,
    /// How long a language server that no call uses is kept (`LANGUAGE_SERVER_IDLE_SECS`).
    pub language_server_idle: Duration,
    /// The most language servers this worker holds at once, for every run (`LANGUAGE_SERVERS_MAX`).
    pub language_servers_max: usize,
    /// The most tool calls of one model turn that run at once (`MAX_PARALLEL_TOOL_CALLS`, 1 to 32).
    pub max_parallel_tool_calls: usize,
}

impl Default for CoderSettings {
    fn default() -> Self {
        Self::new()
    }
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
    pub fn new() -> Self {
        Self {
            max_check_cycles: 3,
            scratch_check_cycles: 5,
            check_timeout: Duration::from_secs(900),
            check_output_tail: 16 * 1024,
            identity: GitIdentity::new("adam-coder", "adam-coder@users.noreply.github.com"),
            draft_pull_requests: false,
            default_repo_host: named::DEFAULT_HOST.to_owned(),
            create_repo_owners: Vec::new(),
            create_for_installed_owners: false,
            max_access_tier: crate::tier::DEFAULT_MAX,
            environment_start: Duration::ZERO,
            background_commands_max: 4,
            background_command_max: Duration::from_secs(3600),
            background_sweep: Duration::from_secs(15),
            background_kill_wait: Duration::from_secs(10),
            run_kept_max: crate::kept::RUN_KEPT_MAX,
            background_output_max: adam_llm_agent::MAX_KEPT_BYTES as u64,
            language_servers: true,
            language_server_idle: Duration::from_secs(600),
            language_servers_max: 4,
            max_parallel_tool_calls: adam_llm_agent::DEFAULT_MAX_PARALLEL,
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
    /// Where long outputs are kept for `read_output`: files under `<workspace root>/outputs`,
    /// scrubbed with [`redactor`](Self::redactor) before they are written. Rebuilt by
    /// [`ToolEnv::with_redactor`].
    pub kept: Arc<FileKept>,
    /// Where the bytes of the images the model sees are kept: files under
    /// `<workspace root>/images`, per conversation, their hash checked when they are read back.
    pub images: Arc<FileImages>,
    /// The person's screen as tools (`ask_user` with choices, `show`, `ui_catalog`) and as a
    /// tool source (the tools of the conversation's endpoint): the catalogs this process has read
    /// are shared by the tools and the source, so both come from this one value. Under the
    /// default [`McpPolicy`] until [`ToolEnv::with_mcp_policy`].
    pub ui: Ui,
    /// Where the run's processes run: the project's checks, the commands that look around and
    /// the other commands. [`Local`], this container, until [`ToolEnv::with_environment`]. The file tools and
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
    /// The repositories the agent and the deployment declare as references: read beside the
    /// repository a run works on and never written ([`references`]). None until
    /// [`ToolEnv::with_references`].
    pub references: References,
    /// The commands that keep running (`start_command`): the processes this worker holds, by output
    /// id. Cloning shares them.
    pub commands: Commands,
    /// The language servers by language ([`lsp`]): the closed table of the images. A test points a
    /// language at the scripted server; production never changes it.
    pub language_table: ServerTable,
    /// What a tool wants to add to its own result without being the one that builds it (a slot
    /// made while reading: which commit it holds): by call, taken by [`Gated`] when the call ends.
    notices: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
}

impl ToolEnv {
    /// Environment over `workspaces` and `code_host`.
    pub fn new(workspaces: Workspaces, code_host: DynCodeHost, settings: CoderSettings) -> Self {
        let notes = NotesStore::new(workspaces.root());
        let kept = Arc::new(
            FileKept::new(workspaces.root(), Redactor::default())
                .with_run_max(settings.run_kept_max),
        );
        let images = Arc::new(FileImages::new(workspaces.root()));
        Self {
            workspaces,
            code_host,
            settings,
            notes,
            redactor: Redactor::default(),
            kept,
            images,
            ui: Ui::new(McpPolicy::default()).with_ask_lead(ASK_LEAD),
            environment: Arc::new(Local),
            unavailable_wait: None,
            credentials_hint: "GITHUB_TOKEN is valid and may push and open pull requests for the repository",
            references: References::default(),
            commands: Commands::new(),
            language_table: ServerTable::production(),
            notices: std::sync::Mutex::default(),
        }
    }

    /// Declare `references`: the repositories the agent's files and the deployment name. The agent
    /// tells the model their names and descriptions ([`References::source`]); a read tool given a
    /// reference's name as `repo` checks it out on first use.
    #[must_use]
    pub fn with_references(mut self, references: References) -> Self {
        self.references = references;
        self
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
    /// progress line of the checks.
    #[must_use]
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.kept = Arc::new(
            FileKept::new(self.workspaces.root(), redactor.clone())
                .with_run_max(self.settings.run_kept_max),
        );
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
                    // The run asked for its environment: its usage says where its commands run.
                    if let Some(meter) = Meter::of(&*session) {
                        meter.report_session(ctx);
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

    /// [`run_in`](shell::run_in) for a tool (`keep`: the output is captured for `output::settle`,
    /// which the caller then calls): the command runs in `session`, stopped when the run is
    /// cancelled, and in a pod the time it ran is reported as the call's sandbox usage whatever its
    /// end was (it exited, or a timeout or a cancel stopped it): ADR 0043.
    pub(crate) async fn run(
        &self,
        ctx: &ToolCtx,
        session: &dyn EnvSession,
        spec: ExecSpec,
        timeout: Duration,
        tail_cap: usize,
        keep: bool,
    ) -> Result<shell::ShellOutcome, shell::RunError> {
        let cancel = ctx.cancel_token();
        let margin = self.redactor.margin();
        let (outcome, ran) = if keep {
            shell::run_captured(session, spec, timeout, tail_cap, margin, &cancel).await
        } else {
            shell::run_timed(session, spec, timeout, tail_cap, margin, &cancel).await
        };
        // A call that keeps nothing shows the tail of a long output: scrubbed here, with the bytes
        // before it, so that a secret cut by the tail's start leaves no part of itself.
        let outcome = outcome.map(|mut outcome| {
            if !keep && !outcome.tail_raw.is_empty() {
                let bytes = std::mem::take(&mut outcome.tail_raw);
                let reaches_the_start = outcome.total <= bytes.len() as u64;
                let raw = String::from_utf8_lossy(&bytes).into_owned();
                let redactor = &self.redactor;
                let scrub = |t: &str| redactor.scrub(t).into_owned();
                // What was held reaches back to the start of the output: nothing was cut there,
                // so it is scrubbed as it is; else its start is an edge, cut back.
                let scrubbed = if reaches_the_start {
                    scrub(&raw)
                } else {
                    adam_llm_agent::scrub_open_start(&raw, scrub, margin)
                };
                let mut start = scrubbed.len().saturating_sub(tail_cap.max(1));
                while !scrubbed.is_char_boundary(start) {
                    start += 1;
                }
                outcome.tail = scrubbed[start..].to_owned();
            }
            outcome
        });
        if let (Some(meter), Some(ran)) = (Meter::of(session), ran) {
            meter.report_command(ctx, ran);
        }
        outcome
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

    /// Add `line` to the result of the call `ctx` is running, after what the tool wrote: for what a
    /// helper learned that the tool's own text does not carry (see [`references::check_out`]).
    pub(crate) fn tell(&self, ctx: &ToolCtx, line: String) {
        self.notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(Self::call_key(ctx))
            .or_default()
            .push(line);
    }

    /// What was [`tell`](Self::tell)ed for the call `ctx` is running, once.
    fn told(&self, ctx: &ToolCtx) -> Vec<String> {
        self.notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&Self::call_key(ctx))
            .unwrap_or_default()
    }

    /// The slot of the run's workspace that a tool **looks at**, or the message to give the model:
    /// it has no workspace yet, or it did not say which of several slots (see [`resolve_slot`]).
    ///
    /// `repo` is what the model passed: the slot's directory (`sandbox`), the address of the
    /// repository the slot holds, or the name of a reference, which is checked out on first use
    /// ([`references::check_out`]). It may be left out when the workspace has one slot that is not a
    /// reference. Every tool that only reads asks this; a tool that changes anything asks
    /// [`slot_to_write`](Self::slot_to_write).
    pub(crate) async fn slot_to_read(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
    ) -> Result<Slot, Outcome> {
        self.slot_named(ctx, repo, false).await
    }

    /// The slot of the run's workspace that a tool **changes**: [`slot_to_read`](Self::slot_to_read),
    /// and **never a reference**. This is the one gate of the tools: a slot that is not
    /// [`writable`](Slot::writable) is the model's to be told about, and nothing is written. A write
    /// that does not ask here is a defect, and a test calls every tool that writes with a reference
    /// ([`references::slot_use`]). A reference that was never read is refused by its name and **not
    /// checked out**: nothing is fetched for a write that cannot happen.
    pub(crate) async fn slot_to_write(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
    ) -> Result<Slot, Outcome> {
        let slot = self.slot_named(ctx, repo, true).await?;
        if slot.writable() {
            Ok(slot)
        } else {
            Err(Ok(ToolOutput::error(references::refusal(&slot))))
        }
    }

    async fn slot_named(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
        to_write: bool,
    ) -> Result<Slot, Outcome> {
        let run = ctx.root_run_id().to_string();
        let workspace = self
            .workspaces
            .run(&run)
            .map_err(|e| Err(workspace_error(&e)))?;
        let slots = workspace
            .slots()
            .await
            .map_err(|e| Err(workspace_error(&e)))?;
        // A bare name may be a reference's: made now when it is read and was never read, and refused
        // by its name when it is written. A slot that has the name as its directory and is not that
        // reference is never answered in its place.
        if let Some(name) = repo.and_then(non_empty)
            && !name.contains(['/', ':'])
        {
            match slots.iter().find(|slot| slot.dir() == name) {
                Some(held) => {
                    if let Some(said) = references::name_clash(self, ctx, held, name).await? {
                        return Err(Ok(ToolOutput::error(said)));
                    }
                }
                None if to_write => {
                    if references::is_declared(self, ctx, name).await? {
                        return Err(Ok(ToolOutput::error(references::refusal_named(name))));
                    }
                }
                None => {
                    if let Some(slot) = references::check_out(self, ctx, name).await? {
                        return Ok(slot);
                    }
                }
            }
        }
        resolve_slot(slots, repo, &self.settings.default_repo_host)
            .map_err(|why| Ok(ToolOutput::error(why)))
    }
}

/// The slot `repo` names among `slots` (see [`ToolEnv::slot`]), or what to tell the model.
///
/// * No slot at all: there is no workspace yet.
/// * `repo` left out: the only slot **that is not a reference**; with several, an error that lists
///   them all (it is the model's to say which). A reference is never the default: a run with one
///   repository and one reference works in the repository.
/// * `repo` given: the slot with that directory, else the slot of the repository it addresses
///   (`https://github.com/acme/lib`, `acme/lib`, a local path: compared as [`named`] keys); when a
///   reference and a slot the run works in hold that repository, the slot the run works in.
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
            .map(|slot| match (slot.worktree(), slot.reference()) {
                (Some(wt), _) => format!("`{}` ({})", slot.dir(), wt.repo().url),
                (None, Some(wt)) => {
                    format!(
                        "`{}` (a reference, read only: {})",
                        slot.dir(),
                        wt.repo().url
                    )
                }
                (None, None) => format!("`{}` (a scratch project)", slot.dir()),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(repo) = repo.and_then(non_empty) else {
        // The slots the run works in: a reference is read when it is named, never by default.
        let working: Vec<usize> = slots
            .iter()
            .enumerate()
            .filter_map(|(at, slot)| slot.writable().then_some(at))
            .collect();
        return match working.as_slice() {
            [only] => Ok(slots.remove(*only)),
            [] => Err(format!(
                "this workspace has no repository or project to work in, only references, which \
                 are read and never changed: {}. Read one by naming it with `repo`; to work on a \
                 repository call prepare_workspace (one the person named) or start_scratch",
                listed(&slots)
            )),
            _ => Err(format!(
                "this workspace has {} slots: {}. Say which one with `repo` (its name, or the \
                 repository's address)",
                slots.len(),
                listed(&slots)
            )),
        };
    };
    let key_of = |slot: &Slot| {
        slot.checkout()
            .and_then(|wt| named::key_of_argument(&wt.repo().url))
    };
    let wanted = named::key_of_argument(repo)
        .or_else(|| named::named_in(repo, default_host).into_iter().next());
    // The slot with that directory; else the slot of the repository the address names, and when a
    // reference and a slot the run works in hold the same repository, the one the run works in (the
    // reference is still reached by its own name).
    let at = slots
        .iter()
        .position(|slot| slot.dir() == repo)
        .or_else(|| {
            let wanted = wanted.as_deref()?;
            let holding = |slot: &&Slot| key_of(slot).as_deref() == Some(wanted);
            slots
                .iter()
                .position(|slot| slot.writable() && holding(&slot))
                .or_else(|| slots.iter().position(|slot| holding(&slot)))
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
/// then the screen's and the plan's (`ask_user`, `show`, `ui_catalog`, `plan_work`, `plan_check`,
/// `plan_add`, from [`ToolEnv::ui`]).
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
    let looking = tools![
        inspect::RunCommand,
        make::Run,
        background::StartCommand,
        background::StopCommand,
        background::ListCommands,
        files::ReadFile,
        find::ListFiles,
        find::FindFiles,
        find::SearchFiles,
        adam_llm_agent::ReadOutput,
    ];
    let servers = if env.settings.language_servers {
        tools![
            lsp::FileDiagnostics,
            lsp::FindDefinition,
            lsp::FindReferences,
        ]
    } else {
        ToolSet::new()
    };
    let after = tools![
        files::WriteFile,
        files::ApplyPatch,
        files::EditFile,
        share::ShareFile,
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
        .extend(looking)
        .extend(servers)
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

    fn paged(&self) -> bool {
        self.inner.paged()
    }

    fn step_style(&self) -> StepStyle {
        self.inner.step_style()
    }

    fn access(&self, args: &Value) -> ToolAccess {
        self.inner.access(args)
    }

    fn read_answer(
        &self,
        call_id: &str,
        args: &Value,
        ui: Option<&Value>,
        payload: &Value,
    ) -> AnswerCheck {
        self.inner.read_answer(call_id, args, ui, payload)
    }

    /// A tool that has a bound of its own declares that bound and half a minute more, so the
    /// bound, which says what happened, fires first ([`own_limit`]); the agent's default applies
    /// only to a tool with none, and every such tool is named in [`QUICK_TOOLS`].
    fn time_limit(&self) -> Option<Duration> {
        own_limit(&self.name, &self.env.settings).or_else(|| self.inner.time_limit())
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
        let result = self.inner.call(ctx, args).await;
        // Taken whatever the result, so nothing outlives the call.
        let told = self.env.told(ctx);
        match result {
            Ok(mut output) if !told.is_empty() => {
                for line in told {
                    output.content.push_str("\n\n");
                    output.content.push_str(&line);
                }
                Ok(output)
            }
            other => other,
        }
    }
}

/// What a tool's time limit adds to the bound it has of its own.
const LIMIT_MARGIN: Duration = Duration::from_secs(30);

/// The tools that **do not need a limit of their own**, because they are quick by construction: a
/// file operation on the run's own directory, a request to the code host (its HTTP client gives up
/// after 30 s), a question to the person (the call returns at once and the run parks), the catalog
/// and display tools. Any other tool of the coder must have a bound of its own and declare it in
/// `own_limit`: a tool added later is not on this list, so it decides. A test walks the whole tool
/// list (`tests/time_limits.rs`).
pub const QUICK_TOOLS: [&str; 16] = [
    // The run's own files: `apply_patch` runs `git apply` for at most 60 s. (The four read tools,
    // `read_file`, `list_files`, `find_files`, `search_files` and `read_output`, have a deadline of
    // their own, 30 s:
    // they declare it, see `own_limit`.)
    "write_file",
    "apply_patch",
    "edit_file",
    "share_file",
    // A scratch directory and `git init` in it.
    "start_scratch",
    // The code host's API, 30 s per request, a few requests a call.
    "request_repository",
    "open_pull_request",
    "merge_pull_request",
    "rename_repository",
    "delete_repository",
    // The person's screen: the call returns at once.
    "ask_user",
    "show",
    "ui_catalog",
    // The plan: state the loop keeps, drawn on the screen of the catalog the message carried.
    "plan_work",
    "plan_check",
    "plan_add",
];

/// The time limit a coder tool declares: its own bound and [`LIMIT_MARGIN`], from the deployment's
/// configured values. `None` for a tool with no bound of its own (see [`QUICK_TOOLS`]).
///
/// | tool | its own bound |
/// |---|---|
/// | `run_command`, `run`, `run_checks` | the environment is made ready (`environment_start`) and the command runs (`CHECK_TIMEOUT_SECS`) |
/// | `rebuild_environment` | the environment is made ready (`environment_start`) |
/// | `start_command` | the environment is made ready and the call waits for the command at most 120 s |
/// | `stop_command` | the environment is made ready (for a command this worker does not hold) and the call waits for the process 60 s |
/// | `list_commands` | the environment is asked for its session, and the records are read |
/// | `read_file`, `list_files`, `find_files`, `search_files`, `read_output` | their own deadline (`find::DEADLINE`, 30 s) |
/// | `file_diagnostics`, `find_definition`, `find_references` | the environment is made ready, and the call waits for a language server at most `wait` (120 s), `initialize` (60 s) and the probe and the sync (60 s): [`lsp::BEYOND_ENVIRONMENT`] |
/// | `prepare_workspace`, `commit_and_push`, `publish_scratch`, `create_repository` | git: one network operation and the local ones around it |
fn own_limit(name: &str, settings: &CoderSettings) -> Option<Duration> {
    let environment = settings.environment_start;
    let bound = match name {
        "run_command" | "run" | "run_checks" => environment + settings.check_timeout,
        "rebuild_environment" => environment,
        // The environment is made ready, and the call waits at most 120 s for the command.
        "start_command" => {
            environment + Duration::from_secs(background::MAX_WAIT_SECS.unsigned_abs())
        }
        // The process is stopped and waited for (20 s), and the output is read.
        "stop_command" => environment + Duration::from_secs(60),
        // The environment is asked for its session, and the records are read.
        "list_commands" => environment + find::DEADLINE,
        "read_file" | "list_files" | "find_files" | "search_files" | "read_output" => {
            find::DEADLINE
        }
        // The environment is made ready, and the call waits for a server up to `wait` (120 s),
        // `initialize` (60 s) and the probe and the sync (60 s).
        "file_diagnostics" | "find_definition" | "find_references" => {
            environment + lsp::BEYOND_ENVIRONMENT
        }
        "prepare_workspace" | "commit_and_push" | "publish_scratch" | "create_repository" => {
            adam_workspace::NETWORK_TIMEOUT + adam_workspace::LOCAL_TIMEOUT
        }
        _ => return None,
    };
    Some(bound + LIMIT_MARGIN)
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

    fn paged(&self) -> bool {
        self.inner.paged()
    }

    fn step_style(&self) -> StepStyle {
        self.inner.step_style()
    }

    fn access(&self, args: &Value) -> ToolAccess {
        self.inner.access(args)
    }

    fn time_limit(&self) -> Option<Duration> {
        self.inner.time_limit()
    }

    fn read_answer(
        &self,
        call_id: &str,
        args: &Value,
        ui: Option<&Value>,
        payload: &Value,
    ) -> AnswerCheck {
        // A refusal and an ending are made from the call's own arguments, which are not scrubbed:
        // they are scrubbed like the question the call asked.
        let r = &self.redactor;
        self.inner
            .read_answer(call_id, args, ui, payload)
            .scrubbed(|text| r.scrub_string(text), |ui| r.scrub_value(ui))
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let r = &self.redactor;
        // The tool may scrub what it is about to cut: a secret cut in half escapes the scrub below.
        let (hook, size) = (r.clone(), r.clone());
        let hooked = ctx.clone().with_scrub_longest(
            move |text| hook.scrub(text).into_owned(),
            move || size.longest(),
        );
        match self.inner.call(&hooked, args).await {
            Ok(mut out) => {
                out.content = r.scrub_string(out.content);
                // The plan is state the loop keeps and the next plan call prints: it is scrubbed
                // like the words that were printed from it.
                if let Some(plan) = &mut out.plan {
                    plan.title = plan.title.take().map(|t| r.scrub_string(t));
                    for item in &mut plan.items {
                        item.text = r.scrub_string(std::mem::take(&mut item.text));
                        item.note = item.note.take().map(|n| r.scrub_string(n));
                    }
                }
                // A text a tool asks the run to keep in its state (a host with no keeper) is
                // journaled and paged back: as scrubbed as what is shown.
                if let Some(kept) = &mut out.kept {
                    kept.text = r.scrub_string(std::mem::take(&mut kept.text));
                }
                // `out.images` and `out.kept_images` are references and bytes, not text, and are not
                // scrubbed: an image is sent only from the sources the deployment allows
                // (docs/reference/coder-agent.md, Secrets).
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
        let mut settings = CoderSettings::new();
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

#[cfg(test)]
mod redacting_tests {
    //! What the redaction layer does to a plan a tool returns, whichever tool it is.

    use adam_llm_agent::{Plan, PlanItem, PlanStatus};
    use adam_runtime::CollectingSink;

    use super::*;

    const TOKEN: &str = "Zq9#kL2mWx7RvT4pNc8YbH1sJd5FgA3eUo6iXz0Q";

    /// Returns a plan that holds the secret everywhere it can, and a line made with the hook its
    /// context carries.
    struct Planner;

    #[async_trait::async_trait]
    impl Tool for Planner {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "planner".to_owned(),
                description: "test".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            let item = |id, text: &str, note: Option<&str>| PlanItem {
                id,
                text: text.to_owned(),
                status: PlanStatus::Pending,
                note: note.map(str::to_owned),
            };
            let plan = Plan {
                title: Some(format!("T {TOKEN}")),
                items: vec![
                    item(1, &format!("use {TOKEN}"), Some(TOKEN)),
                    item(2, "plain", None),
                ],
            };
            Ok(ToolOutput::text(ctx.scrub(&format!("hook: {TOKEN}"))).with_plan(plan))
        }
    }

    #[tokio::test]
    async fn the_plan_a_tool_returns_is_scrubbed_and_the_tool_is_given_the_hook() {
        let redactor = Redactor::default();
        redactor.add(TOKEN);
        let tool = Redacting::layer(redactor)(Arc::new(Planner));
        let ctx = ToolCtx::detached("planner", "c1", Arc::new(CollectingSink::new()));
        let out = tool.call(&ctx, serde_json::json!({})).await.unwrap();
        assert_eq!(out.content, "hook: [redacted]");
        let plan = out.plan.unwrap();
        assert_eq!(plan.title.as_deref(), Some("T [redacted]"));
        assert_eq!(plan.items[0].text, "use [redacted]");
        assert_eq!(plan.items[0].note.as_deref(), Some("[redacted]"));
        assert_eq!(plan.items[1].text, "plain");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod redacting_kept_tests {
    use super::*;

    /// Gives back a text it asks the run to keep (a host with no keeper journals it).
    struct Keeps;

    #[async_trait::async_trait]
    impl Tool for Keeps {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "keeps".into(),
                description: "x".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            let mut out = ToolOutput::text("shown");
            out.kept = Some(Box::new(adam_llm_agent::KeptOutput {
                id: "o-0123456789abcdef".into(),
                text: "line one\nkey=sk-live-0123456789abcdef\n".into(),
            }));
            Ok(out)
        }
    }

    #[tokio::test]
    async fn the_text_a_tool_asks_the_run_to_keep_is_scrubbed_like_its_result() {
        let mut layer = Redacting::layer(Redactor::new(["sk-live-0123456789abcdef"]));
        let tool = layer(Arc::new(Keeps));
        let ctx = ToolCtx::detached("keeps", "c1", Arc::new(adam_runtime::CollectingSink::new()));
        let out = tool.call(&ctx, Value::Null).await.unwrap();
        let kept = out.kept.unwrap();
        assert_eq!(kept.text, "line one\nkey=[redacted]\n");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod access_forwarding_tests {
    //! A layer around a tool says what the tool says about what a call shares: a layer that did
    //! not would make every tool `Alone`.

    use adam_workspace::{MemoryCodeHost, ScopedToken};

    use super::*;

    /// Reads the file its argument names.
    struct Reader;

    #[async_trait::async_trait]
    impl Tool for Reader {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "read_file".into(),
                description: "x".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        fn access(&self, args: &Value) -> ToolAccess {
            ToolAccess::reads_path("files", args.get("path").and_then(Value::as_str))
        }

        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("x"))
        }
    }

    #[test]
    fn redacting_forwards_what_a_call_shares() {
        let mut layer = Redacting::layer(Redactor::new(["secret-value-0123456789"]));
        let tool = layer(Arc::new(Reader));
        assert_eq!(
            tool.access(&serde_json::json!({"path": "src/a.rs"})),
            ToolAccess::reads("files/src/a.rs")
        );
    }

    #[test]
    fn gated_forwards_what_a_call_shares() {
        let root = tempfile::tempdir().unwrap();
        let workspaces = Workspaces::new(
            root.path().join("work"),
            Arc::new(ScopedToken::new("github.com", "t")),
        );
        let env = Arc::new(ToolEnv::new(
            workspaces,
            Arc::new(MemoryCodeHost::new()),
            CoderSettings::new(),
        ));
        let mut layer = Gated::layer(env);
        let tool = layer(Arc::new(Reader));
        assert_eq!(
            tool.access(&serde_json::json!({"path": "src/a.rs"})),
            ToolAccess::reads("files/src/a.rs")
        );
    }
}
