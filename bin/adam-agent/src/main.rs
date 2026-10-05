//! `adam-agent`: one binary that serves any agent folder: the A2A server, the workers, or both
//! (`ROLE`).
//!
//! Configuration is read from the environment, see [`adam_agent::config`]; `ADAM_AGENT_DIR` names
//! the folder, and there is no default. SIGTERM (and Ctrl-C) stop accepting connections and let the
//! workers finish the steps they are in before the process exits; a step cut short by a hard kill
//! is picked up by another replica when its lease expires. The work is done by
//! [`adam_agent::serve`].

use std::future::Future;
use std::process::ExitCode;

use adam_agent::{Config, exit_code};
use anyhow::Context as _;

/// Exit code 0 after a clean shutdown; otherwise the sysexits-style code of the error's root cause
/// ([`adam_agent::exit`]: 78 configuration, 69 dependency unreachable, 71 OS error, 70 internal, 1
/// anything else). The failure is one structured log line, with the whole cause chain, written by
/// the same logger as everything else.
#[tokio::main]
async fn main() -> ExitCode {
    // `RUST_LOG` when set, else `info` with the MCP client library quiet (`adam_service::logging`).
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(adam_service::logging::env_filter())
        .init();

    // Before anything else is read: a same-user child (an MCP server's command) cannot read this
    // process's `/proc/<pid>/environ`, where the model key and the database URL are.
    adam_service::harden::make_non_dumpable();

    // Its errors name variables, never their values.
    let config = match Config::from_env().context("reading the configuration") {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    tracing::info!(config = ?config, "starting adam-agent");
    match adam_agent::serve(config, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&anyhow::Error::from(e)),
    }
}

/// Log `e` once, chain included, and turn it into the process's exit code.
fn fail(e: &anyhow::Error) -> ExitCode {
    let code = exit_code(e.as_ref());
    tracing::error!(error = %format!("{e:#}"), code, "adam-agent failed");
    ExitCode::from(code)
}

/// Resolves on SIGTERM or Ctrl-C.
///
/// The handlers are installed when this function is called, not when the future is first polled:
/// `serve` polls it only after connecting to Postgres, and a SIGTERM during startup must not kill
/// the process by default action.
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
