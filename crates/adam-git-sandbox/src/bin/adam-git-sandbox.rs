//! `adam-git-sandbox`: the server that runs the coder's local `git` where no secret is readable.
//!
//! Configuration is read from the environment, see [`adam_git_sandbox::config`]. SIGTERM (and
//! Ctrl-C) stop it: the commands that still run are killed, the socket is removed.

use std::future::Future;
use std::process::ExitCode;

use adam_git_sandbox::{Config, SandboxError, Server, exit_code};
use tracing_subscriber::EnvFilter;

/// Exit code 0 after a clean shutdown; otherwise the sysexits-style code of the error
/// ([`adam_git_sandbox::exit_code`]: 78 configuration, 71 the socket cannot be made). The failure is
/// one structured log line, with the whole cause chain.
#[tokio::main]
async fn main() -> ExitCode {
    // `RUST_LOG` when set, else `info`.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();

    let config = match Config::from_env().map_err(SandboxError::Config) {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    let server = match Server::bind(&config) {
        Ok(server) => server,
        Err(e) => return fail(&e),
    };
    server.serve(shutdown_signal()).await;
    ExitCode::SUCCESS
}

/// Log `e` once, chain included, and turn it into the process's exit code.
fn fail(e: &SandboxError) -> ExitCode {
    let code = exit_code(e);
    tracing::error!(error = %adam_error::report(e), code, "adam-git-sandbox failed");
    ExitCode::from(code)
}

/// Resolves on SIGTERM or Ctrl-C.
///
/// The handler is installed when this function is called, not when the future is first polled:
/// a SIGTERM before the first poll must not kill the process by default action.
fn shutdown_signal() -> impl Future<Output = ()> + Send {
    let term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    async move {
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
    }
}
