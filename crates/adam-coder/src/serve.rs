//! The coder as one process: [`serve`] composes the Postgres store, the model,
//! GitHub and the workspaces from a [`Config`], and runs the halves its
//! [`adam_host::Role`] asks for until told to stop.
//!
//! The binary is `serve(Config::from_env()?, sigterm)` and nothing else; tests
//! drive it with their own shutdown future. The halves are components of an
//! [`adam_host::Host`], which starts only the ones the role runs and stops them
//! in a fixed order.
//!
//! | Role | Components | Also |
//! |---|---|---|
//! | `all` (default) | `a2a-server` (control plane), `worker` | |
//! | `control-plane` | `a2a-server` | a runtime that only starts, delivers, cancels and views runs |
//! | `worker` | `worker`, `health` | `/healthz` on [`Config::listen_addr`], no A2A |

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use adam_a2a::{A2aServer, AuthConfig};
use adam_core::DynStore;
use adam_host::Host;
use adam_model::DynModel;
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use adam_store_postgres::PgStore;
use adam_workspace::{DynCodeHost, GitHub, GitIdentity};
use anyhow::Context as _;
use secrecy::ExposeSecret as _;
use tokio::net::TcpListener;

use crate::opencode::OpenCodeLaunch;
use crate::redact::Redactor;
use crate::repos::workspaces_for;
use crate::{Coder, CoderAgent, CoderSettings, Config, RuntimeOptions, ToolEnv};

/// How long open connections (SSE streams never end on their own) get to
/// finish after the shutdown signal before the server is dropped.
const SERVER_DRAIN: Duration = Duration::from_secs(10);

/// Run the coder until `shutdown` resolves (SIGTERM in the binary).
///
/// Which components run depends on [`Config::role`]; see the module docs. On
/// shutdown the server stops taking connections (open ones get
/// [`SERVER_DRAIN`]), and the workers finish and commit the steps they are in
/// before this returns; a step cut short by a hard kill is picked up by
/// another replica when its lease expires. If a component stops on its own the
/// others are stopped the same way and the error is returned.
///
/// # Errors
///
/// Connecting to or migrating Postgres, building the clients, binding
/// [`Config::listen_addr`], or a component stopping unexpectedly (an
/// [`adam_host::HostError`], exit code 70). Each error says which step failed
/// and never contains a credential.
pub async fn serve(
    config: Config,
    shutdown: impl Future<Output = ()> + Send,
) -> anyhow::Result<()> {
    let role = config.role;
    let store: DynStore = {
        let store = PgStore::connect(config.database_url.expose_secret())
            .await
            .context("connecting to Postgres")?;
        adam_core::Store::migrate(&store)
            .await
            .context("migrating the schema")?;
        Arc::new(store)
    };

    // Every role builds the complete agent: `Runtime::start` looks it up by name and calls its
    // `init`, so even a control plane, which never steps a run, registers it. The clients are
    // only constructed here; nothing calls the model or GitHub until a worker steps a run.
    let model: DynModel = Arc::new(
        OpenAiCompatible::new(OpenAiConfig::new(
            config.model_base_url.clone(),
            config.model_api_key.clone(),
        ))
        .context("building the model client")?,
    );

    // The workspace root is the workers' business: only a role that runs them creates it.
    if role.runs_workers() {
        tokio::fs::create_dir_all(&config.workspace_root)
            .await
            .with_context(|| format!("creating {}", config.workspace_root.display()))?;
    }
    let (workspaces, creds) = workspaces_for(&config);
    let code_host: DynCodeHost = Arc::new(
        GitHub::new(creds)
            .context("building the GitHub client")?
            .with_api_base(config.github_api_url.as_str()),
    );

    let mut settings = CoderSettings::new(OpenCodeLaunch::from_command(
        &config.opencode_command,
        &config.model_base_url,
        &config.opencode_model,
    ));
    settings.max_check_cycles = config.max_check_cycles;
    settings.check_timeout = config.check_timeout;
    settings.check_output_tail = config.check_output_tail;
    settings.draft_pull_requests = config.pr_draft;
    settings.identity = GitIdentity::new(&config.git_author_name, &config.git_author_email);

    let env = Arc::new(
        ToolEnv::new(workspaces, code_host, settings).with_redactor(Redactor::from_config(&config)),
    );
    let agent = CoderAgent::new(model, config.model.clone(), env);
    let coder = Coder::new(
        store,
        agent,
        &RuntimeOptions {
            concurrency: config.workers,
            ..RuntimeOptions::default()
        },
    );

    // Bind before anything runs: an address that cannot be bound fails the process at once.
    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    // The bound address, not the configured one: `LISTEN_ADDR=…:0` picks a
    // free port, and this line is how a supervisor (or a test) learns it.
    let addr = listener.local_addr().context("reading the bound address")?;
    tracing::info!(
        %addr,
        %role,
        workers = role.runs_workers().then_some(config.workers),
        "listening"
    );

    let host = Host::new(role)
        .control_plane_drain(Some(SERVER_DRAIN))
        // Workers are never cut short here; the orchestrator's grace period bounds the wait.
        .worker_grace(None);
    let host = if role.runs_control_plane() {
        let public_url = config
            .public_url
            .as_ref()
            .context("PUBLIC_URL is unset for a role that serves A2A")?;
        let app = coder.router(
            public_url,
            AuthConfig::BearerTokens(config.a2a_bearer_tokens.clone()),
        );
        host.control_plane("a2a-server", |stop| async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .map_err(Into::into)
        })
    } else {
        // No A2A here, but probes still need an answer: the same `/healthz` the A2A router serves.
        host.worker("health", |stop| async move {
            axum::serve(listener, A2aServer::health_router())
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
                .map_err(Into::into)
        })
    };
    let host = if role.runs_workers() {
        let runtime = coder.runtime.clone();
        host.worker("worker", |stop| async move {
            runtime
                .run_worker(stop.cancelled_owned())
                .await
                .map_err(Into::into)
        })
    } else {
        host
    };

    host.run(shutdown).await?;
    tracing::info!("stopped");
    Ok(())
}
