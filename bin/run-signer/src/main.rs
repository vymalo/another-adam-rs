//! `run-signer`: the loopback sidecar that signs a run's model requests with the run's own
//! certificate ([ADR 0086](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md), A4).
//!
//! An agent that holds no key points `MODEL_BASE_URL` at it with `MODEL_AUTH=injector`: every call
//! carries `X-Run-Id` and the prompt statement, the sidecar adds `Turn-Signature` from the run's
//! [`RunKeyring`](adam_core::turn::RunKeyring), and forwards the call to `RUN_SIGNER_UPSTREAM`.
//!
//! Configuration is read from the environment, see [`adam_run_signer::config`]. Two halves, both
//! required: the sidecar's own (`RUN_SIGNER_*`) and the run certificate's (`RUN_CERT_*`, read by
//! `adam-service` exactly as the agents read them). SIGTERM (and Ctrl-C) stop accepting connections
//! and let the calls in flight finish, for up to the shutdown grace.

use std::future::Future;
use std::process::ExitCode;
use std::sync::Arc;

use adam_core::RunId;
use adam_core::turn::run_cert::DynRunCertIssuer;
use adam_error::BoxError;
use adam_run_cert_broker::MAX_CA_PEM_BYTES;
use adam_run_signer::{ConfigError, IssuerFactory, SignerConfig, SignerError, exit_code};
use adam_service::config::RunCertSettings;
use tracing_subscriber::EnvFilter;

/// The process-wide allocator: mimalloc in place of musl's own, which is slow under many threads
/// (ADR 0053 decision 11), as every binary of the image has it.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// `RUN_CERT_*` as an [`IssuerFactory`]: one broker issuer per run, from the same settings the
/// agents read ([`RunCertSettings`]).
struct Broker(RunCertSettings);

impl IssuerFactory for Broker {
    fn issuer(&self, run: RunId) -> Result<DynRunCertIssuer, BoxError> {
        Ok(self.0.issuer(run)?)
    }

    fn kind(&self) -> &'static str {
        "broker"
    }
}

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

    let config = match read_config() {
        Ok(config) => config,
        Err(e) => return fail(&e),
    };
    tracing::info!(config = ?config, "starting run-signer");
    match adam_run_signer::run(config, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

/// The sidecar's configuration, from `RUN_SIGNER_*` and `RUN_CERT_*`.
///
/// The run certificate is **required** here, unlike for an agent: a signer with no issuer cannot
/// sign at all. Every `RUN_CERT_*` unset is therefore this binary's own problem, and a half-set
/// configuration is refused with the problems `adam-service` reports.
///
/// `RUN_CERT_CA_FILE` is also the upstream client's default trust anchor: the broker and the
/// gateway are one trust domain by design today. `RUN_SIGNER_UPSTREAM_CA_FILE` overrides it for the
/// upstream alone.
fn read_config() -> Result<SignerConfig, SignerError> {
    let mut problems = Vec::new();
    let settings = RunCertSettings::parse(&|name| std::env::var(name).ok(), &mut problems);
    if settings.is_none() && problems.is_empty() {
        problems.push(
            "run certificates are required: set RUN_CERT_ISSUER, RUN_CERT_ISSUER_URL and \
             RUN_CERT_TOKEN_FILE (the signer signs only with a run's certificate)"
                .to_owned(),
        );
    }
    let settings = match (settings, problems.is_empty()) {
        (Some(settings), true) => settings,
        _ => return Err(SignerError::Config(ConfigError::new(problems))),
    };

    // The default anchors of the upstream client: the broker's file. `RunCertSettings::parse` has
    // already read and checked it, so a failure here is a race, and it is still a problem.
    let ca_pem = match settings.ca_file.as_deref() {
        Some(path) => match read_ca(path) {
            Ok(pem) => Some(pem),
            Err(why) => {
                return Err(SignerError::Config(ConfigError::new(vec![format!(
                    "RUN_CERT_CA_FILE is unusable: {why}"
                )])));
            }
        },
        None => None,
    };

    let issuer: Arc<dyn IssuerFactory> = Arc::new(Broker(settings));
    SignerConfig::from_env(issuer, ca_pem).map_err(SignerError::Config)
}

/// The PEM of `path`, read under [`MAX_CA_PEM_BYTES`], the bound the broker's own client uses, so
/// the two clients cannot disagree on it.
fn read_ca(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("it cannot be read: {e}"))?
        .len();
    if size > MAX_CA_PEM_BYTES as u64 {
        return Err(format!("it is longer than {MAX_CA_PEM_BYTES} bytes"));
    }
    std::fs::read(path).map_err(|e| format!("it cannot be read: {e}"))
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
