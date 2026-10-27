//! `adam-egress`: an HTTPS-only egress proxy for run pods.
//!
//! Configuration is read from the environment, see [`adam_egress::config`]. SIGTERM (and Ctrl-C)
//! stop accepting connections and let the open tunnels finish, for up to 25 seconds.

use std::future::Future;
use std::process::ExitCode;

use adam_egress::{Config, EgressError, exit_code};
use tracing_subscriber::EnvFilter;

/// Exit code 0 after a clean shutdown; otherwise the sysexits-style code of the error
/// ([`adam_egress::exit_code`]: 78 configuration, 71 the port cannot be bound). The failure is one
/// structured log line, with the whole cause chain, written by the same logger as everything else.
#[tokio::main]
async fn main() -> ExitCode {
    // `RUST_LOG` when set, else `info`.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();

    let config = match Config::from_env().map_err(EgressError::Config) {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    tracing::info!(config = ?config, "starting adam-egress");
    match adam_egress::run(config, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// Log `e` once, chain included, and turn it into the process's exit code.
fn fail(e: &EgressError) -> ExitCode {
    let code = exit_code(e);
    tracing::error!(error = %adam_error::report(e), code, "adam-egress failed");
    ExitCode::from(code)
}

/// Resolves on SIGTERM or Ctrl-C.
///
/// The handler is installed when this function is called, not when the future is first polled:
/// a SIGTERM before the first poll must not kill the process by default action.
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
