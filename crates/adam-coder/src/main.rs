//! `adam-coder`: the coder agent as one process: A2A server and workers.
//!
//! Configuration is read from the environment, see [`adam_coder::config`].
//! SIGTERM (and Ctrl-C) stop accepting connections and let the workers finish
//! the steps they are in before the process exits; a step cut short by a hard
//! kill is picked up by another replica when its lease expires. The work is
//! done by [`adam_coder::serve`].

use std::future::Future;
use std::process::ExitCode;

use adam_coder::{Config, Redactor, exit_code};
use anyhow::Context as _;
use tracing_subscriber::EnvFilter;

/// Exit code 0 after a clean shutdown; otherwise the sysexits-style code of the error's root cause
/// ([`adam_coder::exit`]: 78 configuration, 69 dependency unreachable, 71 OS error, 70 internal, 1
/// anything else). The failure is one structured log line, with the whole cause chain and none of
/// the process's secrets, written by the same logger as everything else.
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Nothing is known to be secret until the configuration is read; its errors name variables,
    // never their values.
    let config = match Config::from_env().context("reading the configuration") {
        Ok(config) => config,
        Err(e) => return fail(&e, &Redactor::default()),
    };
    let redactor = Redactor::from_config(&config);
    tracing::info!(config = ?config, "starting adam-coder");
    match adam_coder::serve(config, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e, &redactor),
    }
}

/// Log `e` once, chain included, and turn it into the process's exit code.
fn fail(e: &anyhow::Error, redactor: &Redactor) -> ExitCode {
    let code = exit_code(e);
    let chain = redactor.scrub_string(format!("{e:#}"));
    tracing::error!(error = %chain, code, "adam-coder failed");
    ExitCode::from(code)
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
