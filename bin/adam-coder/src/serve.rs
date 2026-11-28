//! The coder as one process: [`serve`] reads the agent files, assembles the agent (for the roles
//! that run workers: the model, GitHub and the workspaces) and hands it to [`adam_service::serve`],
//! which connects Postgres and runs the halves its [`adam_host::Role`] asks for until told to stop.
//!
//! The binary is `serve(Config::from_env()?, sigterm)` and nothing else; tests drive it with their
//! own shutdown future. Everything that is not the coder's (the store, the notifications, the
//! components of a role, the drain) is [`adam_service`]; see its README for the process.
//!
//! Before anything connects, `serve` reads the agent files ([`AgentFiles`]: the folder
//! `ADAM_AGENT_DIR` names, else the embedded copy) for every role, logs what it runs (`agent files`:
//! source, path, digest, agent, warning count; then each warning) and refuses a folder with errors.
//! The control plane serves the card of those files, and the workers assemble the agent from them,
//! connecting the MCP servers of the folder's `mcp.json` first.
//!
//! | Role | Components | Also |
//! |---|---|---|
//! | `all` (default) | `a2a-server` (control plane), `worker`, `notify` | |
//! | `control-plane` | `a2a-server`, `notify` | a runtime with the starters of the agent and of its helpers only (`AgentDef::session_starters`), no model or GitHub configuration |
//! | `worker` | `worker`, `health`, `notify` | `/healthz` on [`ServiceConfig::listen_addr`](adam_service::ServiceConfig::listen_addr), no A2A |
//!
//! `notify` is the `adam_notify_postgres::PgNotify` listener and publisher: live events and
//! wake-up/cancel signals cross processes over Postgres `LISTEN`/`NOTIFY`, so a worker takes a
//! run another process started at once instead of at its next poll, and a control plane streams
//! the progress of a run a worker steps as it happens. It is a latency optimisation: polling
//! stays on and correctness never depends on a notification (see `adam-notify-postgres`).

use std::future::Future;
use std::sync::Arc;

use adam_a2a::AccessTier;
use adam_model::DynModel;
use adam_service::{Agents, RuntimeOptions, claim_scope_for};
use adam_workspace::{DynCodeHost, DynEnvironment, GitHub, GitIdentity};
use anyhow::Context as _;

use crate::agent::CoderStarter;
use crate::github_mcp::{GitHubReadBearer, SERVER_NAME as GITHUB_SERVER};
use crate::janitor::Janitor;
use crate::redact::{Redactor, RedactorFromConfig};
use crate::repos::workspaces_for;
use crate::tools::CommandsSweep;
use crate::{
    AGENT_NAME, AgentFiles, CoderAgent, CoderSettings, Config, RunEnvironment, RunPodsConfig,
    ToolEnv, WorkerConfig, agent_card_for, coder_tools,
};

/// The folder's `limits.max_inline_upload_bytes`, when it sets one: the first of the three places
/// the inline threshold of `uploads/v1` comes from (then `UPLOADS_INLINE_MAX_BYTES`, then the
/// default), read once for the card and once for `share_file`.
fn folder_inline_upload_limit(files: &AgentFiles) -> Option<u64> {
    files
        .def()
        .ok()?
        .manifest()
        .frontmatter
        .limits
        .as_ref()?
        .max_inline_upload_bytes
}

/// The complete coder agent for a role that runs workers: the model client, the GitHub client and
/// the workspaces, assembled from `files`. The clients are only constructed here; nothing calls the
/// model or GitHub until a worker steps a run. `redactor` is built from the whole configuration, so
/// it also knows the database password and the A2A tokens, which a step's error text must not carry
/// either.
async fn build_agent(
    worker: &WorkerConfig,
    max_access_tier: AccessTier,
    redactor: Redactor,
    files: &AgentFiles,
    sessions: adam_service::SessionSettings,
    inline_upload_threshold: u64,
) -> anyhow::Result<(CoderAgent, Janitor, CommandsSweep)> {
    let model: DynModel = worker.model.client().context("building the model client")?;
    if worker.commands_can_read_the_app_key() {
        tracing::warn!(
            "GitHub App mode with the private key in a file (GITHUB_APP_PRIVATE_KEY_PATH), and the commands of a run \
             (run_command, run, run_checks) run in this container as the user that can read it: a \
             command can read the key and mint installation tokens, so NO access tier holds against a command, \
             a `plan` included. ALLOW_COMMANDS_NEXT_TO_SECRETS=true accepted this; to remove it run the commands in \
             run pods (RUN_ENVIRONMENT=kubernetes), or use a token with the permissions you can live \
             with"
        );
    }

    let root = worker.placed_root();
    tokio::fs::create_dir_all(&root)
        .await
        .with_context(|| format!("creating {}", root.display()))?;
    tracing::info!(
        placement = worker.placement.as_str(),
        worker_id = worker.worker_id.as_deref(),
        root = %root.display(),
        "workspace placement"
    );
    // The tokens of the GitHub credentials (an App's installation tokens are minted as the run goes)
    // are registered with the redactor as they are handed out, so it is shared with them.
    let (workspaces, creds) =
        workspaces_for(worker, &redactor).context("building the GitHub credentials")?;
    let code_host: DynCodeHost = Arc::new(
        GitHub::new(creds.clone())
            .context("building the GitHub client")?
            .with_api_base(worker.github_api_url.as_str()),
    );

    let mut settings = CoderSettings::new();
    settings.max_check_cycles = worker.max_check_cycles;
    settings.scratch_check_cycles = worker.scratch_check_cycles;
    if let Some(host) = worker.allowed_repo_hosts.first() {
        settings.default_repo_host.clone_from(host);
    }
    settings
        .create_repo_owners
        .clone_from(&worker.create_repo_owners);
    // With no `CREATE_REPO_OWNERS`, a GitHub App creates for the accounts it is installed on (the
    // access tiers, ADR 0036); a token has no such list and keeps the tool off until owners are named.
    // `GITHUB_APP_OWNERS=*` is not such a list: it needs `CREATE_REPO_OWNERS`.
    settings.create_for_installed_owners = worker.github.names_its_accounts();
    settings.max_access_tier = max_access_tier;
    settings.check_timeout = worker.check_timeout;
    settings.environment_start = worker.run_environment.start_bound();
    settings.check_output_tail = worker.check_output_tail;
    settings.background_commands_max = worker.background_commands_max;
    settings.background_command_max = worker.background_command_max;
    settings.background_sweep = worker.background_sweep;
    settings.language_servers = worker.language_servers;
    settings.language_server_idle = worker.language_server_idle;
    settings.language_servers_max = worker.language_servers_max;
    settings.max_parallel_tool_calls = worker.max_parallel_tool_calls;
    // A refused model call parks the run (ADR 0067); behind the injector, so does a gateway that
    // cannot be reached.
    settings.model_parking = adam_llm_agent::ModelParking::new(worker.model.park_limit)
        .when_unreachable(worker.model.injected_run);
    settings.draft_pull_requests = worker.pr_draft;
    settings.identity = GitIdentity::new(&worker.git_author_name, &worker.git_author_email);

    // The agent's definition: its own files, with the extra MCP servers `ADAM_EXTRA_MCP_FILE` names
    // added (a name the agent already has is refused: exit 78). Read before the environment, because
    // the variables its servers refer to (`${VAR}` in a header) are secrets of this process: they are
    // hidden from every process a run starts (a repository's checks, a command) and
    // redacted from what the tools answer, like the fixed ones.
    let mut def = files
        .def()
        .map_err(|e| *e)
        .context("reading the agent definition")?;
    if let Some(file) = &worker.extra_mcp_file {
        let (extended, warnings) = def
            .with_extra_mcp_file(file)
            .context("adding the extra MCP servers (ADAM_EXTRA_MCP_FILE)")?;
        for warning in &warnings {
            tracing::warn!(warning = %warning, "a warning while adding the extra MCP servers");
        }
        tracing::info!(file = %file.display(), "extra MCP servers added to the agent's own");
        def = extended;
    }
    // The repositories the agent may read and never write: its own (the frontmatter) with the
    // deployment's file added; a name both declare is refused (exit 78). Read before the process
    // serves anything, so a mistake is a startup error and not a run's.
    let references = crate::tools::References::load(
        &def.manifest().frontmatter.references,
        worker.references_file.as_deref(),
    )?;
    // An address the workspaces' policy refuses is found now, not at the first read.
    references
        .check_against(&workspaces)
        .context("checking the references against the workspaces' policy")?;
    if !references.is_empty() {
        tracing::info!(
            references = ?references.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            "repositories the agent may read and never write"
        );
    }
    let protected = crate::mcp_secrets::protect(&def.mcp_env_references(), &redactor, |name| {
        std::env::var(name).ok()
    });
    tracing::info!(
        hidden = ?protected.hidden,
        skipped = ?protected.skipped,
        registered = protected.registered,
        "variables the MCP servers refer to are hidden from the processes of runs, and their values never reach the model"
    );
    let environment =
        crate::mcp_secrets::hiding(environment_for(worker, &root).await?, &protected.hidden);
    // The process's pools (ADR 0065): the tool calls, model calls and processes in flight of every
    // run, one admission per process shared by the agent and its subagents. The folder's `limits:`
    // may only lower the deployment's ceiling (`Budget::lower`).
    let folder = def.manifest().frontmatter.limits.as_ref();
    let budget = worker.budget.lower(&folder_budget(
        folder.and_then(|l| l.max_processes),
        folder.and_then(|l| l.command_memory.as_deref()),
        folder.and_then(|l| l.command_cpu.as_deref()),
    ));
    // What the command tools show when a call gives no `output` (ADR 0090 slice T3): the folder's
    // `limits.output`, else each tool's own default.
    if let Some(output) = folder.and_then(|l| l.output.as_ref()) {
        settings.set_output_defaults(output);
    }
    let admission: adam_budget::DynAdmission =
        Arc::new(adam_admission_local::LocalAdmission::new(budget.clone()));
    settings.admission = Some(Arc::clone(&admission));
    // Every git of this process takes a seat of `MAX_GIT_PROCESSES` (ADR 0065 decision 5).
    adam_workspace::govern_git(Arc::clone(&admission), adam_code_tools::governor::WAIT);
    // The commands' OS limits. The probe starts a transient scope, so it runs off the async threads,
    // once.
    #[cfg(unix)]
    {
        let confine = tokio::task::spawn_blocking(adam_confine::best_effort)
            .await
            .context("probing for the OS limits of commands")?;
        tracing::info!(
            enforcement = %confine.describe(),
            memory = %adam_workspace::human_bytes(budget.command_memory),
            cpu_millicores = budget.command_cpu,
            "OS limits on commands"
        );
        settings.governor = Some(adam_code_tools::governor::Governor::from_budget(
            admission, confine, &budget,
        ));
    }
    let mut tool_env = ToolEnv::new(workspaces, code_host, settings).with_environment(environment);
    // A run pod the cluster cannot give now (the quota is used up) is waited for, not failed at once
    // and not replaced by this container (ADR 0019, decision 6).
    if let RunEnvironment::Kubernetes(pods) = &worker.run_environment {
        tool_env = tool_env.with_unavailable_wait(pods.wait);
    }
    let env = Arc::new(
        tool_env
            .with_redactor(redactor)
            .with_references(references)
            // What a rejected credential tells the model to check depends on which kind they are.
            .with_credentials_hint(worker.github.check_hint())
            // The URL a message announces for the conversation's tools is an MCP server's: the
            // deployment's policy (MCP_ALLOW_INSECURE, timeouts, the result bound) decides.
            .with_mcp_policy(worker.mcp_policy())
            // A file over the threshold `uploads/v1` serves (the same number the card is built
            // with) goes to the bucket by reference, not inline.
            .with_inline_upload_threshold(inline_upload_threshold),
    );
    // The GitHub MCP server holds no credentials: the deployment binds its name and its origin
    // (`GITHUB_MCP_URL`) to the coder's own credentials, which give each call the token that fits
    // what it is about (ADR 0017, D4). A folder that points `github` elsewhere, or gives it an
    // `Authorization` header, is refused at connect. The conversation's own tools (the policy of
    // `ToolEnv` above) are unbound: they carry the sender's bearer.
    let bearer = Arc::new(GitHubReadBearer::new(
        creds,
        worker
            .allowed_repo_hosts
            .first()
            .map_or("github.com", String::as_str),
        // An App that finds the installation of each owner needs each call to name its account.
        worker.github.finds_installations(),
    ));
    // A result over CHECK_OUTPUT_TAIL_BYTES is kept and paged, as a command's output is (ADR 0090).
    let policy =
        worker
            .mcp_policy()
            .bearer_per_call(GITHUB_SERVER, worker.github_mcp_url.as_str(), bearer);
    // The MCP servers the folder's `mcp.json` names are connected now, at startup, before the
    // agent is bound: a server that is down, a local process the policy does not allow, a
    // `${VAR}` that is unset are startup errors with their own exit code (69 or 78), never
    // something found in the middle of a run. Without an `mcp.json` this connects to nothing.
    // The deployment's session limits (SESSION_*); the root's file may only lower them.
    let def = def
        .session_limits(sessions.limits)
        .connect_mcp(&policy)
        .await
        .context(
            "connecting the MCP servers of the agent files (MCP_ALLOW_STDIO, MCP_ALLOW_INSECURE and \
             MCP_ALLOW_URL_VARS decide which kinds they may be; GITHUB_MCP_URL is where the `github` \
             server must be)",
        )?;
    // The alias comes from the environment, and the files may come from a folder, so a bad one of
    // either is a startup error, not a panic. The error is unboxed so its class (exit 78 for a
    // mistake in the files) reaches `exit_code`.
    let tools = coder_tools(&env);
    // The workspaces of finished runs are swept from this worker's volume, never removing one that
    // holds unpushed work (ADR 0065 decision 7: a worktree with a change no `origin` has, or any
    // scratch project). The images of a conversation follow `SCRATCH_IDLE_DAYS`.
    let janitor = Janitor::new(env.workspaces.clone(), worker.workspace_sweep)
        .with_environment(env.environment.clone())
        .with_scratch_idle(worker.scratch_idle);
    let (janitor, sweep) = wire_commands(&env, janitor, worker);
    let agent = CoderAgent::try_from_def_with_profiles(
        def,
        model,
        worker.model.alias.clone(),
        worker.model.profiles.clone(),
        env,
        tools,
    )
    .map_err(|e| *e)
    .context("assembling the coder agent")?;
    Ok((agent, janitor, sweep))
}

/// Make the commands that keep running of `env` the janitor's too (it stops them before it releases
/// what holds them) and the sweep's (every `BACKGROUND_SWEEP_SECS`): one registry for the tools, the
/// janitor and the sweep.
pub fn wire_commands(
    env: &ToolEnv,
    janitor: Janitor,
    worker: &WorkerConfig,
) -> (Janitor, CommandsSweep) {
    (
        janitor.with_commands(env.commands.clone()),
        CommandsSweep::new(&env.commands, worker.background_sweep),
    )
}

/// Where this worker's runs run their commands: in this container (the default), or in a pod of the
/// run's own.
///
/// [`Local`](adam_workspace::Local) is the coder's own container, and on a desktop that **is** the
/// person's work sandbox: adam adds no container inside it. A run pod is a pod of the run's own
/// (`kubernetes_environment`).
///
/// The orphan sweep is the janitor's: it asks this environment what it holds, at startup and every
/// `WORKSPACE_SWEEP_SECS` ([`Janitor`]).
///
/// # Errors
///
/// A run pod template that cannot be read or is not usable, or no cluster to talk to. Nothing here
/// fails a deployment whose commands run in this container.
pub async fn environment_for(
    worker: &WorkerConfig,
    root: &std::path::Path,
) -> anyhow::Result<DynEnvironment> {
    match &worker.run_environment {
        RunEnvironment::Local => {
            tracing::info!(
                "commands run in this container; a repository's own environment is a run pod of its image"
            );
            Ok(Arc::new(adam_workspace::Local))
        }
        RunEnvironment::Kubernetes(pods) => kubernetes_environment(worker, pods, root).await,
    }
}

/// `RUN_ENVIRONMENT=kubernetes`: a pod of its own for each run ([ADR 0019](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0019-a-runs-processes-in-a-pod-of-their-own.md)).
///
/// 1. the pod template is read and checked, which fails the start (exit 78): a template the cluster
///    would refuse, or one that does not mount the workspace volume once, whole, at `root` (the coder
///    narrows that mount to each run's own directory), is a mistake of the deployment, not of a run;
/// 2. the client is made from the pod's ServiceAccount (`KUBERNETES_SERVICE_HOST` and its token), and
///    the process does not start without one (exit 69). **No call is made to the API yet**: the first
///    command of a run makes its pod;
/// 3. the sweep of idle pods runs in the background, and the janitor releases the pods of runs that
///    are over (the pods are found by label, so a crash leaves nothing it cannot find).
///
/// # Errors
///
/// A template that cannot be read or is not usable ([`EnvError::Config`](adam_workspace::EnvError), 78),
/// no cluster to talk to (69).
async fn kubernetes_environment(
    worker: &WorkerConfig,
    pods: &RunPodsConfig,
    root: &std::path::Path,
) -> anyhow::Result<DynEnvironment> {
    let template =
        adam_env_kubernetes::PodTemplate::from_file(&pods.template_file, &pods.container, root)
            .context("reading the run pod template (RUN_POD_TEMPLATE_FILE)")?;
    let image = template.image().map(str::to_owned);
    let settings = pods.settings(worker.worker_id.as_deref().unwrap_or("adam-coder"));
    let environment = adam_env_kubernetes::KubeEnvironment::connect(settings, template)
        .await
        .context("connecting to the Kubernetes API for the run pods")?;
    tracing::info!(
        namespace = %pods.namespace,
        instance = %pods.instance,
        image = ?image,
        idle_secs = pods.idle.map(|d| d.as_secs()),
        wait_secs = pods.wait.as_secs(),
        "a run's commands run in a pod of its own"
    );
    // Lives as long as the process: the runtime ends it.
    drop(environment.spawn_reaper());
    Ok(Arc::new(environment))
}

/// The runtime options of a worker: its id, how many runs at once, and whose runs it claims (its own
/// only, for a placement that keeps a run's files on one worker).
fn options_of(worker: &WorkerConfig) -> RuntimeOptions {
    RuntimeOptions {
        worker_id: worker.worker_id.clone(),
        claim_scope: claim_scope_for(worker.placement),
        concurrency: worker.workers,
        ..RuntimeOptions::default()
    }
}

/// Run the coder until `shutdown` resolves (SIGTERM in the binary).
///
/// Which components run depends on [`ServiceConfig::role`](adam_service::ServiceConfig::role); see the
/// module docs. On shutdown the server stops taking connections (open ones get ten seconds
/// to finish), and the workers finish and commit the steps they are in
/// before this returns; a step cut short by a hard kill is picked up by
/// another replica when its lease expires. If a component stops on its own the
/// others are stopped the same way and the error is returned.
///
/// # Errors
///
/// Reading the agent files, building the clients, connecting the MCP servers, connecting to or
/// migrating Postgres, binding the listen address, or a component stopping unexpectedly (an
/// [`adam_host::HostError`], exit code 70). Each error says which step failed and never contains a
/// credential.
pub async fn serve(
    config: Config,
    shutdown: impl Future<Output = ()> + Send,
) -> anyhow::Result<()> {
    // The agent files first, for every role and before anything connects: a folder with a mistake
    // in it stops the process (exit 78) with every diagnostic, whether or not the database is up.
    let files = AgentFiles::load(config.agent_dir.as_deref()).context("reading the agent files")?;
    files.log();
    // The removed variables (ADR 0038's OpenCode, ADR 0052's devcontainer): ignored, and said once,
    // by name only.
    for name in &config.ignored {
        let why = if name.starts_with("DEVCONTAINER_") || *name == "CONTAINER_HOST" {
            "the coder no longer runs a repository's devcontainer (ADR 0052)"
        } else {
            "the coder no longer uses OpenCode (ADR 0038)"
        };
        tracing::warn!(variable = %name, reason = why, "a variable is set and ignored");
    }

    // What a person may open a session on (`child-sessions/v1`): the root's helpers with the
    // effective limits, read from the files alone so every role that serves A2A lists and serves the
    // same. A person's round reads the workspace of the latest task of the context it was opened
    // in (ADR 0045 slice 7), which the backend resolves; nothing here depends on it.
    let helpers = files
        .def()
        .map_err(|e| *e)
        .context("reading the agent definition")?
        .session_limits(config.service.sessions.limits)
        // The same `MAX_PARALLEL_TOOL_CALLS` every role reads, so the bounds below are a worker's.
        .session_max_parallel(config.service.sessions.max_parallel_tool_calls);
    let sessions = helpers.child_sessions();
    // The files' schedules (ADR 0060), run by the roles that run workers.
    let schedules = helpers.manifest().schedules.clone();

    // Only a role that runs workers builds the agent: model, GitHub client, workspaces, MCP
    // servers. A control plane starts, delivers to, cancels and views runs, which needs the
    // agent's name and `init` only (`CoderStarter`), so it takes no model or GitHub configuration.
    let (agents, prompts, session_bounds) = match &config.worker {
        Some(worker) => {
            let (agent, janitor, sweep) = build_agent(
                worker,
                config.max_access_tier,
                Redactor::from_config(&config),
                &files,
                config.service.sessions,
                adam_service::inline_upload_threshold(
                    folder_inline_upload_limit(&files),
                    config.service.uploads_inline_max_bytes,
                ),
            )
            .await?;
            // The prompts of the MCP servers the deployment adds (`ADAM_EXTRA_MCP_FILE`) are
            // commands too; the coder's own `github` server has a bearer per call and gives none.
            let prompts = agent.assembly().prompt_commands().to_vec();
            // What a person's `start.config` is held to: the bounds the session tools use, so the
            // two cannot disagree (ADR 0054 decision 2). A control plane gets the same ones from
            // the files and the settings (`AgentDef::session_bounds`), without binding the agent.
            let session_bounds = agent.assembly().session_bounds().to_vec();
            // The models a message may pick (`model-pick/v1`): the root's choices, the same list
            // its card advertises.
            let choices = agent.model_choices();
            // The reasoning levels of the root's model (`reasoning/v1`), which its card lists and
            // the backend checks a message's effort against.
            let efforts = agent.reasoning_efforts();
            (
                Agents::new(AGENT_NAME, move |builder| {
                    agent
                        .register(builder)
                        .agent(adam_schedule::SchedulerAgent::new(AGENT_NAME))
                })
                .options(options_of(worker))
                .model_choices(choices)
                .reasoning_efforts(efforts)
                // Beside the runtime's worker: the sweep of the workspaces of finished runs.
                .worker_component("janitor", move |store, stop| janitor.run(store, stop))
                // The commands that keep running: stopped with their run, whatever ended it.
                .worker_component("commands", move |store, stop| sweep.run(store, stop))
                // The scheduler runs made to match the files' schedules at start (ADR 0060).
                .worker_component("schedules", move |store, stop| async move {
                    adam_schedule::reconcile_until_stopped(
                        store,
                        AGENT_NAME.to_owned(),
                        schedules,
                        stop,
                    )
                    .await
                    .map_err(Into::into)
                }),
                prompts,
                session_bounds,
            )
        }
        // The starters of the helpers too, so a person's session round can be started here and a
        // worker steps it.
        None => {
            // The bounds of a person's `start.config` on each helper, from the files and the
            // deployment's settings alone: the very function a worker's assembly calls.
            let session_bounds = helpers.session_bounds();
            (
                Agents::new(AGENT_NAME, move |builder| {
                    helpers.session_starters(builder.starter(CoderStarter))
                }),
                Vec::new(),
                session_bounds,
            )
        }
    };
    // A control plane connects no MCP server, so it cannot expand a prompt: a message that names the
    // prompt of one of the files' servers is refused, not sent to the model as plain text.
    let commands = if config.worker.is_none() {
        let def = files
            .def()
            .map_err(|e| *e)
            .context("reading the agent definition")?;
        adam_service::refuse_prompt_commands(files.commands(), def.manifest().mcp.as_ref())
    } else {
        files.commands()
    };
    let (commands, listed) = adam_service::with_prompt_commands(commands, &prompts);
    let agents = agents
        // The person's screen is the sender: their answers through a form, and the catalog and the
        // tools of the conversation, reach the run (the extensions the card lists).
        .inbound(adam_a2a_runtime::vymalo_inbound)
        // The card lists `access-tier/v1` with all four tiers: the backend reads the tier a message
        // names, refuses a bad one and says the tier a task works under.
        .access_tiers(crate::tier::honoured_up_to(config.max_access_tier))
        // The card lists the commands of the files and the prompts of the MCP servers (`commands/v1`);
        // the backend expands a message that names one before it is stored.
        .commands(commands);
    // The modes of the files (`agent-mode/v1`): the card lists them and the backend refuses a
    // message that names another; the agent narrows to the one a message picks.
    let agents = match files.agent_modes() {
        Some(modes) => agents.agent_modes(modes),
        None => agents,
    };
    let agents = match sessions {
        Some(sessions) => agents.child_sessions(sessions.with_bounds(session_bounds)),
        None => agents,
    };
    // The largest upload the card lists (`uploads/v1`): the folder's, else the environment's.
    let agents = match folder_inline_upload_limit(&files) {
        Some(bytes) => agents.uploads_inline_max_bytes(bytes),
        None => agents,
    };
    // The card of the files this process runs, like the workers' agent.
    let card = match &config.service.public_url {
        Some(public_url) => Some(
            agent_card_for(&files, public_url, config.max_access_tier)
                .map_err(|e| *e)
                .context("building the agent card")?,
        )
        .map(|card| adam_service::card_with_commands(card, &listed)),
        None => None,
    };

    adam_service::serve(&config.service, agents.card_if(card), shutdown).await?;
    Ok(())
}

/// The ceiling a folder's `limits:` sets for processes and commands (ADR 0065 decision 1): a field
/// the file leaves out, or that does not read (the loader already refused those), lowers nothing.
fn folder_budget(
    max_processes: Option<u32>,
    command_memory: Option<&str>,
    command_cpu: Option<&str>,
) -> adam_budget::Budget {
    let mut budget = adam_budget::Budget::builder();
    if let Some(n) = max_processes {
        budget = budget.processes(n);
    }
    if let Some(bytes) = command_memory.and_then(|m| adam_budget::parse_bytes(m).ok()) {
        budget = budget.command_memory(bytes);
    }
    if let Some(millicores) = command_cpu.and_then(|c| adam_budget::parse_millicores(c).ok()) {
        budget = budget.command_cpu(millicores);
    }
    budget.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder lowers the deployment's ceiling and never raises it (ADR 0065 decision 1).
    #[test]
    fn a_folder_lowers_the_processes_and_never_raises_them() {
        let deployment = adam_budget::Budget::builder()
            .processes(4)
            .command_memory(4 << 30)
            .command_cpu(2000)
            .build();
        let lower = deployment.lower(&folder_budget(Some(2), Some("1GB"), Some("500m")));
        assert_eq!(
            (lower.processes, lower.command_memory, lower.command_cpu),
            (2, 1 << 30, 500)
        );
        let raised = deployment.lower(&folder_budget(Some(64), Some("64GB"), Some("32")));
        assert_eq!(
            (raised.processes, raised.command_memory, raised.command_cpu),
            (4, 4 << 30, 2000),
            "a larger value in a file is the deployment's"
        );
        let none = deployment.lower(&folder_budget(None, None, None));
        assert_eq!(none, deployment);
    }
}
