//! The coder as one process: [`serve`] composes the Postgres store from a [`Config`] and, for the
//! roles that run workers, the model, GitHub and the workspaces, and runs the halves its
//! [`adam_host::Role`] asks for until told to stop.
//!
//! The binary is `serve(Config::from_env()?, sigterm)` and nothing else; tests
//! drive it with their own shutdown future. The halves are components of an
//! [`adam_host::Host`], which starts only the ones the role runs and stops them
//! in a fixed order.
//!
//! | Role | Components | Also |
//! |---|---|---|
//! | `all` (default) | `a2a-server` (control plane), `worker`, `notify` | |
//! | `control-plane` | `a2a-server`, `notify` | [`Coder::control_plane_with`]: a runtime with the agent's starter only, no model or GitHub configuration |
//! | `worker` | `worker`, `health`, `notify` | `/healthz` on [`Config::listen_addr`], no A2A |
//!
//! `notify` is the [`adam_notify_postgres::PgNotify`] listener and publisher: live events and
//! wake-up/cancel signals cross processes over Postgres `LISTEN`/`NOTIFY`, so a worker takes a
//! run another process started at once instead of at its next poll, and a control plane streams
//! the progress of a run a worker steps as it happens. It is a latency optimisation: polling
//! stays on and correctness never depends on a notification (see `adam-notify-postgres`). With a
//! worker it stops only after the worker has finished, so the last step's events and signals are
//! still sent.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use adam_a2a::{A2aServer, AuthConfig};
use adam_core::DynStore;
use adam_host::Host;
use adam_model::DynModel;
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use adam_notify_postgres::PgNotify;
use adam_runtime::BroadcastSink;
use adam_store_postgres::PgStore;
use adam_workspace::{DynCodeHost, GitHub, GitIdentity};
use anyhow::Context as _;
use secrecy::ExposeSecret as _;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::opencode::OpenCodeLaunch;
use crate::redact::Redactor;
use crate::repos::workspaces_for;
use crate::{
    Coder, CoderAgent, CoderSettings, Config, LiveSignals, RuntimeOptions, ToolEnv, WorkerConfig,
};

/// How long open connections (SSE streams never end on their own) get to
/// finish after the shutdown signal before the server is dropped.
const SERVER_DRAIN: Duration = Duration::from_secs(10);

/// The complete coder agent for a role that runs workers: the model client, the GitHub client and
/// the workspaces. The clients are only constructed here; nothing calls the model or GitHub until
/// a worker steps a run. `redactor` is built from the whole configuration, so it also knows the
/// database password and the A2A tokens, which a step's error text must not carry either.
async fn build_agent(worker: &WorkerConfig, redactor: Redactor) -> anyhow::Result<CoderAgent> {
    let model: DynModel = Arc::new(
        OpenAiCompatible::new(OpenAiConfig::new(
            worker.model_base_url.clone(),
            worker.model_api_key.clone(),
        ))
        .context("building the model client")?,
    );

    tokio::fs::create_dir_all(&worker.workspace_root)
        .await
        .with_context(|| format!("creating {}", worker.workspace_root.display()))?;
    let (workspaces, creds) = workspaces_for(worker);
    let code_host: DynCodeHost = Arc::new(
        GitHub::new(creds)
            .context("building the GitHub client")?
            .with_api_base(worker.github_api_url.as_str()),
    );

    let mut settings = CoderSettings::new(OpenCodeLaunch::from_command(
        &worker.opencode_command,
        &worker.model_base_url,
        &worker.opencode_model,
    ));
    settings.max_check_cycles = worker.max_check_cycles;
    settings.check_timeout = worker.check_timeout;
    settings.check_output_tail = worker.check_output_tail;
    settings.draft_pull_requests = worker.pr_draft;
    settings.identity = GitIdentity::new(&worker.git_author_name, &worker.git_author_email);

    let env = Arc::new(ToolEnv::new(workspaces, code_host, settings).with_redactor(redactor));
    Ok(CoderAgent::new(model, worker.model.clone(), env))
}

/// Drive the notifier until `stop`, and log once `LISTEN` is active.
async fn run_notify(
    notify: PgNotify,
    stop: impl Future<Output = ()> + Send,
) -> Result<(), adam_error::BoxError> {
    let listening = async {
        notify.wait_listening().await;
        tracing::info!("listening for notifications");
        std::future::pending::<Result<(), adam_notify_postgres::NotifyError>>().await
    };
    tokio::select! {
        result = notify.run(stop) => result.map_err(Into::into),
        result = listening => result.map_err(Into::into),
    }
}

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
    let (store, pool): (DynStore, _) = {
        let store = PgStore::connect(config.database_url.expose_secret())
            .await
            .context("connecting to Postgres")?;
        adam_core::Store::migrate(&store)
            .await
            .context("migrating the schema")?;
        let pool = store.pool().clone();
        (Arc::new(store), pool)
    };

    // Events and wake-up/cancel signals cross processes over `NOTIFY` on the store's own pool. The
    // listener (`notify.run`) is a host component below; the runtime only holds the two halves.
    let broadcast = BroadcastSink::default();
    let notify = PgNotify::new(pool, broadcast.clone());
    let live = LiveSignals {
        broadcast,
        sink: Arc::new(notify.event_sink()),
        notifier: Some(Arc::new(notify.notifier())),
    };

    // Only a role that runs workers builds the agent: model, GitHub client, workspaces. A control
    // plane starts, delivers to, cancels and views runs, which needs the agent's name and `init`
    // only (`CoderStarter`), so it takes no model or GitHub configuration.
    let (coder, workers) = match &config.worker {
        Some(worker) => (
            Coder::new_with(
                store,
                build_agent(worker, Redactor::from_config(&config)).await?,
                &RuntimeOptions {
                    concurrency: worker.workers,
                    ..RuntimeOptions::default()
                },
                live,
            ),
            Some(worker.workers),
        ),
        None => (
            Coder::control_plane_with(store, &RuntimeOptions::default(), live),
            None,
        ),
    };

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
        workers,
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
    // With workers, `notify` is a worker component that stops when the `worker` component is done
    // (or gone): the host cancels the components of a tier together, and a step finishing after
    // the cancel still emits events and signals that should be sent. Without workers it stops with
    // the control plane.
    let notify_stop = CancellationToken::new();
    let host = if role.runs_workers() {
        let runtime = coder.runtime.clone();
        let done = notify_stop.clone().drop_guard();
        host.worker("worker", |stop| async move {
            let _done = done;
            runtime
                .run_worker(stop.cancelled_owned())
                .await
                .map_err(Into::into)
        })
        .worker("notify", |_| {
            run_notify(notify, notify_stop.cancelled_owned())
        })
    } else {
        host.control_plane("notify", |stop| run_notify(notify, stop.cancelled_owned()))
    };

    host.run(shutdown).await?;
    tracing::info!("stopped");
    Ok(())
}
