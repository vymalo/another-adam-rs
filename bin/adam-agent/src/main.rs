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

use adam_agent::{Config, build_version, exit_code};
use anyhow::Context as _;

/// The process-wide allocator: mimalloc in place of musl's own, which is slow under many threads
/// (ADR 0053 decision 11).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Exit code 0 after a clean shutdown; otherwise the sysexits-style code of the error's root cause
/// ([`adam_agent::exit`]: 78 configuration, 69 dependency unreachable, 71 OS error, 70 internal, 1
/// anything else). The failure is one structured log line, with the whole cause chain, written by
/// the same logger as everything else.
#[tokio::main]
async fn main() -> ExitCode {
    // `--version` and `--help` answer before anything is read or logged: they need no configuration, no
    // database and no folder, so `brew test` and a person can run them anywhere. Any other argument is
    // ignored, as it always was; the process is configured by its environment alone.
    if let Some(text) = info_for(std::env::args().skip(1)) {
        println!("{text}");
        return ExitCode::SUCCESS;
    }

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

/// The text of `--version` or `--help` when the first argument asks for it.
fn info_for(mut args: impl Iterator<Item = String>) -> Option<String> {
    match args.next().as_deref() {
        Some("--version" | "-V") => Some(format!("adam-agent {}", build_version())),
        Some("--help" | "-h") => Some(format!(
            "adam-agent {}\n\n\
             Serves one agent folder over A2A. It takes no arguments: it is configured by its environment.\n\n\
             A laptop needs, at least:\n  \
             ADAM_AGENT_DIR       the folder that holds agent/instructions.md (and skills/, mcp.json, ...)\n  \
             DATABASE_URL         sqlite:///path/to/adam.db (one file, ROLE=all), or postgres:// or mongodb://\n  \
             MODEL_BASE_URL       an OpenAI-compatible endpoint, with /v1\n  \
             MODEL, MODEL_API_KEY the model alias and its key (the key may be empty)\n  \
             A2A_BEARER_TOKENS    the tokens a client must present (none: no server)\n  \
             PUBLIC_URL           where clients reach it, e.g. http://127.0.0.1:8080/\n\n\
             Every variable: https://github.com/vymalo/another-adam-rs/blob/main/bin/adam-agent/README.md#configuration",
            build_version()
        )),
        _ => None,
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
