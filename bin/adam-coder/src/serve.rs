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
//! | `control-plane` | `a2a-server`, `notify` | [`Coder::control_plane_with`]: a runtime with the agent's starter only, no model or GitHub configuration |
//! | `worker` | `worker`, `health`, `notify` | `/healthz` on [`ServiceConfig::listen_addr`](adam_service::ServiceConfig::listen_addr), no A2A |
//!
//! `notify` is the `adam_notify_postgres::PgNotify` listener and publisher: live events and
//! wake-up/cancel signals cross processes over Postgres `LISTEN`/`NOTIFY`, so a worker takes a
//! run another process started at once instead of at its next poll, and a control plane streams
//! the progress of a run a worker steps as it happens. It is a latency optimisation: polling
//! stays on and correctness never depends on a notification (see `adam-notify-postgres`).

use std::future::Future;
use std::sync::Arc;

use adam_model::DynModel;
use adam_service::{Agents, RuntimeOptions, claim_scope_for};
use adam_workspace::{DynCodeHost, GitHub, GitIdentity};
use anyhow::Context as _;

use crate::agent::CoderStarter;
use crate::janitor::Janitor;
use crate::opencode::OpenCodeLaunch;
use crate::redact::Redactor;
use crate::repos::workspaces_for;
use crate::{
    AGENT_NAME, AgentFiles, CoderAgent, CoderSettings, Config, ToolEnv, WorkerConfig,
    agent_card_from, coder_tools,
};

/// The complete coder agent for a role that runs workers: the model client, the GitHub client and
/// the workspaces, assembled from `files`. The clients are only constructed here; nothing calls the
/// model or GitHub until a worker steps a run. `redactor` is built from the whole configuration, so
/// it also knows the database password and the A2A tokens, which a step's error text must not carry
/// either.
async fn build_agent(
    worker: &WorkerConfig,
    redactor: Redactor,
    files: &AgentFiles,
) -> anyhow::Result<(CoderAgent, Janitor)> {
    let model: DynModel = worker.model.client().context("building the model client")?;

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
        GitHub::new(creds)
            .context("building the GitHub client")?
            .with_api_base(worker.github_api_url.as_str()),
    );

    let mut settings = CoderSettings::new(OpenCodeLaunch::from_command(
        &worker.opencode_command,
        &worker.model.base_url,
        &worker.opencode_model,
    ));
    settings.max_check_cycles = worker.max_check_cycles;
    if let Some(host) = worker.allowed_repo_hosts.first() {
        settings.default_repo_host.clone_from(host);
    }
    settings.check_timeout = worker.check_timeout;
    settings.check_output_tail = worker.check_output_tail;
    settings.draft_pull_requests = worker.pr_draft;
    settings.identity = GitIdentity::new(&worker.git_author_name, &worker.git_author_email);

    let env = Arc::new(
        ToolEnv::new(workspaces, code_host, settings)
            .with_redactor(redactor)
            // What a rejected credential tells the model to check depends on which kind they are.
            .with_credentials_hint(worker.github.check_hint())
            // The URL a message announces for the conversation's tools is an MCP server's: the
            // deployment's policy (MCP_ALLOW_INSECURE, timeouts) decides.
            .with_mcp_policy(worker.mcp.policy()),
    );
    // The MCP servers the folder's `mcp.json` names are connected now, at startup, before the
    // agent is bound: a server that is down, a local process the policy does not allow, a
    // `${VAR}` that is unset are startup errors with their own exit code (69 or 78), never
    // something found in the middle of a run. Without an `mcp.json` (the embedded copy has none)
    // this connects to nothing.
    let def = files
        .def()
        .map_err(|e| *e)
        .context("reading the agent definition")?
        .connect_mcp(&worker.mcp.policy())
        .await
        .context(
            "connecting the MCP servers of the agent files (MCP_ALLOW_STDIO, MCP_ALLOW_INSECURE and \
             MCP_ALLOW_URL_VARS decide which kinds they may be)",
        )?;
    // The alias comes from the environment, and the files may come from a folder, so a bad one of
    // either is a startup error, not a panic. The error is unboxed so its class (exit 78 for a
    // mistake in the files) reaches `exit_code`.
    let tools = coder_tools(&env);
    // The workspaces of finished runs are swept from this worker's volume.
    let janitor = Janitor::new(env.workspaces.clone(), worker.workspace_sweep)
        .with_environment(env.environment.clone());
    let agent = CoderAgent::try_from_def(def, model, worker.model.alias.clone(), env, tools)
        .map_err(|e| *e)
        .context("assembling the coder agent")?;
    Ok((agent, janitor))
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

    // Only a role that runs workers builds the agent: model, GitHub client, workspaces, MCP
    // servers. A control plane starts, delivers to, cancels and views runs, which needs the
    // agent's name and `init` only (`CoderStarter`), so it takes no model or GitHub configuration.
    let agents = match &config.worker {
        Some(worker) => {
            let (agent, janitor) =
                build_agent(worker, Redactor::from_config(&config), &files).await?;
            Agents::new(AGENT_NAME, move |builder| agent.register(builder))
                .options(options_of(worker))
                // Beside the runtime's worker: the sweep of the workspaces of finished runs.
                .worker_component("janitor", move |store, stop| janitor.run(store, stop))
        }
        None => Agents::new(AGENT_NAME, |builder| builder.starter(CoderStarter)),
    }
    // The person's screen is the sender: their answers through a form, and the catalog and the
    // tools of the conversation, reach the run (the extensions the card lists).
    .inbound(adam_a2a_runtime::vymalo_inbound);
    // The card of the files this process runs, like the workers' agent.
    let card = match &config.service.public_url {
        Some(public_url) => Some(
            agent_card_from(&files, public_url)
                .map_err(|e| *e)
                .context("building the agent card")?,
        ),
        None => None,
    };

    adam_service::serve(&config.service, agents.card_if(card), shutdown).await?;
    Ok(())
}
