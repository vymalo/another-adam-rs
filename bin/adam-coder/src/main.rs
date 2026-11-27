//! `adam-coder`: the coder agent as one process: the A2A server, the workers, or both (`ROLE`).
//!
//! Configuration is read from the environment, see [`adam_coder::config`].
//! SIGTERM (and Ctrl-C) stop accepting connections and let the workers finish
//! the steps they are in before the process exits; a step cut short by a hard
//! kill is picked up by another replica when its lease expires. The work is
//! done by [`adam_coder::serve`].
//!
//! `adam-coder export [...]` serves nothing: it writes the coder's runs as training records
//! ([`adam_coder::export`], ADR 0085) and exits.

use std::future::Future;
use std::process::ExitCode;

use adam_coder::{Config, Redactor, RedactorFromConfig, exit_code};
use anyhow::Context as _;

/// The process-wide allocator: mimalloc in place of musl's own, which is slow under many threads
/// (ADR 0053 decision 11).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Exit code 0 after a clean shutdown; otherwise the sysexits-style code of the error's root cause
/// ([`adam_coder::exit`]: 78 configuration, 69 dependency unreachable, 71 OS error, 70 internal, 1
/// anything else). The failure is one structured log line, with the whole cause chain and none of
/// the process's secrets, written by the same logger as everything else.
#[tokio::main]
async fn main() -> ExitCode {
    // `adam-coder export ...` writes records to standard output, so its log goes to standard error.
    let export = adam_coder::export::subcommand(std::env::args_os().skip(1));
    // `RUST_LOG` when set, else `info` with the MCP client library quiet (`adam_service::logging`).
    let logger = tracing_subscriber::fmt()
        .json()
        .with_env_filter(adam_service::logging::env_filter());
    if export.is_some() {
        logger.with_writer(std::io::stderr).init();
    } else {
        logger.init();
    }

    adam_service::harden::make_non_dumpable();

    if let Some(args) = export {
        return export_runs(args).await;
    }

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

/// `adam-coder export`: 0 when the records are written, 64 for a wrong command line, otherwise
/// the code of the error, as for serving. A failure's chain is scrubbed of the database's password.
async fn export_runs(args: Vec<std::ffi::OsString>) -> ExitCode {
    use adam_service::export::{Command, EX_USAGE, parse_args, usage};
    match parse_args(args) {
        Ok(Command::Help) => {
            println!("{}", usage("adam-coder"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, code = EX_USAGE, usage = %usage("adam-coder"), "adam-coder export: wrong command line");
            ExitCode::from(EX_USAGE)
        }
        Ok(Command::Export(args)) => match adam_coder::export::run(&args).await {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                let redactor = Config::from_env()
                    .map(|config| Redactor::from_config(&config))
                    .unwrap_or_default();
                fail(&e, &redactor)
            }
        },
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
