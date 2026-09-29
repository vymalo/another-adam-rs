//! `adam-coder`: the coder agent as one process: A2A server and workers.
//!
//! Configuration is read from the environment, see [`adam_coder::config`].
//! SIGTERM (and Ctrl-C) stop accepting connections and let the workers finish
//! the steps they are in before the process exits; a step cut short by a hard
//! kill is picked up by another replica when its lease expires.

use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use adam_a2a::AuthConfig;
use adam_coder::opencode::OpenCodeLaunch;
use adam_coder::{
    Coder, CoderAgent, CoderSettings, Config, Redactor, RuntimeOptions, ToolEnv, workspaces_for,
};
use adam_core::DynStore;
use adam_model::DynModel;
use adam_model_openai::{OpenAiCompatible, OpenAiConfig};
use adam_store_postgres::PgStore;
use adam_workspace::{DynCodeHost, GitHub, GitIdentity};
use anyhow::Context as _;
use secrecy::ExposeSecret as _;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// How long open connections (SSE streams never end on their own) get to
/// finish after the shutdown signal before the server is dropped.
const SERVER_DRAIN: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().context("reading the configuration")?;
    tracing::info!(config = ?config, "starting adam-coder");
    run(config).await
}

async fn run(config: Config) -> anyhow::Result<()> {
    let store: DynStore = {
        let store = PgStore::connect(config.database_url.expose_secret())
            .await
            .context("connecting to Postgres")?;
        adam_core::Store::migrate(&store)
            .await
            .context("migrating the schema")?;
        Arc::new(store)
    };

    let model: DynModel = Arc::new(
        OpenAiCompatible::new(OpenAiConfig::new(
            config.model_base_url.clone(),
            config.model_api_key.clone(),
        ))
        .context("building the model client")?,
    );

    tokio::fs::create_dir_all(&config.workspace_root)
        .await
        .with_context(|| format!("creating {}", config.workspace_root.display()))?;
    let (workspaces, creds) = workspaces_for(&config);
    let code_host: DynCodeHost =
        Arc::new(GitHub::new(creds).context("building the GitHub client")?);

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

    let app = coder.router(
        &config.public_url,
        AuthConfig::BearerTokens(config.a2a_bearer_tokens.clone()),
    );
    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    tracing::info!(addr = %config.listen_addr, workers = config.workers, "listening");

    let (stop_tx, stop_rx) = watch::channel(false);
    let mut server = tokio::spawn({
        let mut stop = stop_rx.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stop.wait_for(|stop| *stop).await;
            })
            .into_future()
    });
    let mut worker = tokio::spawn({
        let mut stop = stop_rx;
        let runtime = coder.runtime.clone();
        async move {
            runtime
                .run_worker(async move {
                    let _ = stop.wait_for(|stop| *stop).await;
                })
                .await
        }
    });

    // Run until a signal, or until either half dies on its own.
    let mut early = None;
    tokio::select! {
        () = shutdown_signal() => tracing::info!("shutdown requested"),
        res = &mut server => early = Some(("server", flatten(res))),
        res = &mut worker => early = Some(("worker", flatten(res))),
    }
    let _ = stop_tx.send(true);

    // The server first (it stops taking new work), bounded because SSE streams
    // outlive the signal; then the worker, unbounded: in-flight steps finish
    // and commit. The orchestrator's grace period bounds the wait.
    if early.as_ref().is_none_or(|(name, _)| *name != "server")
        && tokio::time::timeout(SERVER_DRAIN, &mut server)
            .await
            .is_err()
    {
        tracing::warn!("open connections did not drain in time; closing them");
        server.abort();
    }
    if early.as_ref().is_none_or(|(name, _)| *name != "worker") {
        flatten(worker.await).context("worker failed while shutting down")?;
    }
    match early {
        Some((name, Err(e))) => Err(e.context(format!("{name} stopped unexpectedly"))),
        Some((name, Ok(()))) => anyhow::bail!("{name} stopped unexpectedly"),
        None => {
            tracing::info!("stopped");
            Ok(())
        }
    }
}

fn flatten<E: Into<anyhow::Error>>(
    res: Result<Result<(), E>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    res.context("task panicked or was cancelled")?
        .map_err(Into::into)
}

/// Resolves on SIGTERM or Ctrl-C.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot install the SIGTERM handler; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
