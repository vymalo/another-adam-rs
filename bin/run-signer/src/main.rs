//! `run-signer`: the loopback sidecar that signs a run's model requests with the run's own
//! certificate ([ADR 0086](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md), A4).
//!
//! An agent that holds no key points `MODEL_BASE_URL` at it with `MODEL_AUTH=injector`: every call
//! carries `X-Run-Id` and the prompt statement, the sidecar adds `Turn-Signature` from the run's
//! [`RunKeyring`](adam_core::turn::RunKeyring), and forwards the call to `RUN_SIGNER_UPSTREAM`.
//!
//! Configuration is read from the environment, see [`adam_run_signer::config`] and
//! [`adam_run_signer::from_process_env`]. Two halves, both required: the sidecar's own
//! (`RUN_SIGNER_*`) and the run certificate's (`RUN_CERT_*`, read by `adam-service` exactly as the
//! agents read them). SIGTERM (and Ctrl-C) stop accepting connections and let the calls in flight
//! finish, for up to the shutdown grace.

use std::future::Future;
use std::process::ExitCode;

use adam_run_signer::{SignerError, exit_code, from_process_env};
use tracing_subscriber::EnvFilter;

/// The process-wide allocator: mimalloc in place of musl's own, which is slow under many threads
/// (ADR 0053 decision 11), as every binary of the image has it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Exit code 0 after a clean shutdown; otherwise the sysexits-style code of the error
/// ([`adam_run_signer::exit_code`]: 78 configuration, 71 the port cannot be bound or served). The
/// failure is one structured log line, with the whole cause chain, written by the same logger as
/// everything else.
#[tokio::main]
async fn main() -> ExitCode {
    // `RUST_LOG` when set, else `info`.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .init();

    let config = match from_process_env() {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    tracing::info!(config = ?config, "starting run-signer");
    match adam_run_signer::run(config, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// Log `e` once, chain included, and turn it into the process's exit code.
fn fail(e: &SignerError) -> ExitCode {
    let code = exit_code(e);
    tracing::error!(error = %adam_error::report(e), code, "run-signer failed");
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
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
