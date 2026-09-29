//! `adam-coder`: the coder agent as one process: A2A server and workers.
//!
//! Configuration is read from the environment, see [`adam_coder::config`].
//! SIGTERM (and Ctrl-C) stop accepting connections and let the workers finish
//! the steps they are in before the process exits; a step cut short by a hard
//! kill is picked up by another replica when its lease expires. The work is
//! done by [`adam_coder::serve`].

use std::future::Future;

use adam_coder::Config;
use anyhow::Context as _;
use tracing_subscriber::EnvFilter;

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
    adam_coder::serve(config, shutdown_signal()).await
}

/// Resolves on SIGTERM or Ctrl-C.
///
/// The handlers are installed when this function is called, not when the
/// future is first polled: `serve` polls it only after connecting to Postgres,
/// and a SIGTERM during startup must not kill the process by default action.
fn shutdown_signal() -> impl Future<Output = ()> + Send {
    #[cfg(unix)]
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    async move {
        #[cfg(unix)]
        match term {
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
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
    }
}
