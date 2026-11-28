//! `adam-agent`: one binary that serves any agent folder: the A2A server, the workers, or both
//! (`ROLE`).
//!
//! Configuration is read from the environment, see [`adam_agent::config`]; `ADAM_AGENT_DIR` names
//! the folder, and there is no default. SIGTERM (and Ctrl-C) stop accepting connections and let the
//! workers finish the steps they are in before the process exits; a step cut short by a hard kill
//! is picked up by another replica when its lease expires. The work is done by
//! [`adam_agent::serve`].
//!
//! `adam-agent export [...]` serves nothing: it writes the folder's runs as training records
//! ([`adam_agent::export`], ADR 0087) and exits.

use std::future::Future;
use std::process::ExitCode;

use adam_agent::{Config, exit_code};
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
    // `adam-agent export ...` writes records to standard output, so its log goes to standard error.
    let export = adam_agent::export::subcommand(std::env::args_os().skip(1));

    // Before the logger and the configuration: a package's `test do` runs `adam-agent --version`,
    // and starting the server there would hang (or exit 78 without `ADAM_AGENT_DIR`). `export`
    // answers its own `--help`.
    if export.is_none()
        && let Some(code) = early_exit(std::env::args().skip(1))
    {
        return code;
    }

    // `RUST_LOG` when set, else `info` with the MCP client library quiet (`adam_service::logging`).
    let logger = tracing_subscriber::fmt()
        .json()
        .with_env_filter(adam_service::logging::env_filter());
    if export.is_some() {
        logger.with_writer(std::io::stderr).init();
    } else {
        logger.init();
    }

    // Before anything else is read: a same-user child (an MCP server's command) cannot read this
    // process's `/proc/<pid>/environ`, where the model key and the database URL are.
    adam_service::harden::make_non_dumpable();

    if let Some(args) = export {
        return export_runs(args).await;
    }

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

/// `adam-agent export`: 0 when the records are written, 64 for a wrong command line, otherwise
/// the code of the error, as for serving.
async fn export_runs(args: Vec<std::ffi::OsString>) -> ExitCode {
    use adam_service::export::{Command, EX_USAGE, parse_args, usage};
    match parse_args(args) {
        Ok(Command::Help) => {
            println!("{}", usage("adam-agent"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            tracing::error!(error = %e, code = EX_USAGE, usage = %usage("adam-agent"), "adam-agent export: wrong command line");
            ExitCode::from(EX_USAGE)
        }
        Ok(Command::Export(args)) => match adam_agent::export::run(&args).await {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => fail(&anyhow::Error::from(e)),
        },
    }
}

/// What a bare invocation with `--version` or `--help` answers, before any configuration is read:
/// a package's `test do` runs `adam-agent --version`, and starting the server there would hang (or
/// exit 78 without `ADAM_AGENT_DIR`). Any other argument is ignored, as it always was.
fn early_exit(args: impl Iterator<Item = String>) -> Option<ExitCode> {
    for arg in args {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("adam-agent {}", env!("CARGO_PKG_VERSION"));
                return Some(ExitCode::SUCCESS);
            }
            "--help" | "-h" => {
                println!("{}", USAGE);
                return Some(ExitCode::SUCCESS);
            }
            _ => {}
        }
    }
    None
}

/// What `--help` prints: the binary, its version, and the configuration it reads (the page that
/// documents every variable is linked, not copied).
const USAGE: &str = concat!(
    "adam-agent ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "Serves any agent folder: the A2A server, the workers, or both (`ROLE`).\n",
    "\n",
    "Configuration comes from the environment: `ADAM_AGENT_DIR` names the folder,\n",
    "`DATABASE_URL` the run store, `TURN_SIGNING_KEY_FILE` the turn key. Every variable is\n",
    "documented at https://github.com/vymalo/another-adam-rs/blob/main/docs/reference/environment.md\n",
    "\n",
    "Commands:\n",
    "  export         Write the folder's runs as training records (`adam-agent export --help`).\n",
    "\n",
    "Options:\n",
    "  -h, --help     Print this help and exit.\n",
    "  -V, --version  Print the version and exit.\n",
);

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
