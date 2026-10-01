//! How the binary ends: which exit code an error maps to.
//!
//! The binary is `serve` under an [`anyhow`] chain: every I/O step adds a `.context()`, and the
//! root cause is a typed error somewhere down the chain. [`exit_code`] walks the chain from the
//! outside in and returns the first code that fits, so a supervisor can tell a deployment that is
//! misconfigured (do not restart) from one whose database is down (restart later) from a bug.
//!
//! | Code | Name | Root cause |
//! |---|---|---|
//! | 0 | | clean shutdown after a signal (not an error) |
//! | 78 | `EX_CONFIG` | [`ConfigError`], `OpenAiConfigError`, or any error whose class is `Invalid` |
//! | 69 | `EX_UNAVAILABLE` | a dependency is unreachable: a `Transient`, `RateLimited` or `Conflict` error, such as Postgres |
//! | 71 | `EX_OSERR` | an [`std::io::Error`]: a listener that cannot bind, a directory that cannot be created |
//! | 70 | `EX_SOFTWARE` | [`HostError`] (a component of the process stopped, panicked or ended while still needed), a panicked task, or a `Corrupt` or `Internal` error |
//! | 1 | | anything else |
//!
//! The values are those of BSD `sysexits.h`, *unverified* (from memory; the header is not part of
//! this repository's sources).

use std::error::Error;

use adam_core::StoreError;
use adam_error::{Classify, ErrorClass};
use adam_host::HostError;
use adam_model_openai::OpenAiConfigError;
use adam_runtime::RuntimeError;
use adam_workspace::WorkspaceError;

use crate::ConfigError;

/// `EX_CONFIG`: the configuration is wrong; restarting will not help.
pub const EX_CONFIG: u8 = 78;
/// `EX_UNAVAILABLE`: a dependency is unreachable; restarting later may help.
pub const EX_UNAVAILABLE: u8 = 69;
/// `EX_OSERR`: the operating system refused something (a port, a directory).
pub const EX_OSERR: u8 = 71;
/// `EX_SOFTWARE`: an internal error: a bug, or a component of the process that stopped.
pub const EX_SOFTWARE: u8 = 70;
/// Anything else.
pub const EX_GENERAL: u8 = 1;

/// The exit code for `err`, found by walking its chain from the outside in. See the module docs.
pub fn exit_code(err: &anyhow::Error) -> u8 {
    // A context layer is not part of `chain()`'s downcastable items, so ask for it directly.
    if err.downcast_ref::<HostError>().is_some() {
        return EX_SOFTWARE;
    }
    for cause in err.chain() {
        if cause.is::<ConfigError>() {
            return EX_CONFIG;
        }
        if let Some(class) = class_of(cause) {
            return match class {
                ErrorClass::Invalid => EX_CONFIG,
                ErrorClass::Transient | ErrorClass::RateLimited | ErrorClass::Conflict => {
                    EX_UNAVAILABLE
                }
                ErrorClass::Corrupt | ErrorClass::Internal => EX_SOFTWARE,
                _ => EX_GENERAL,
            };
        }
        if cause.is::<tokio::task::JoinError>() {
            return EX_SOFTWARE;
        }
        if cause.is::<std::io::Error>() {
            return EX_OSERR;
        }
    }
    EX_GENERAL
}

/// The class of `cause` when it is one of the typed errors the binary's steps can return.
fn class_of(cause: &(dyn Error + 'static)) -> Option<ErrorClass> {
    if let Some(e) = cause.downcast_ref::<StoreError>() {
        return Some(e.class());
    }
    if let Some(e) = cause.downcast_ref::<OpenAiConfigError>() {
        return Some(e.class());
    }
    if let Some(e) = cause.downcast_ref::<WorkspaceError>() {
        return Some(e.class());
    }
    if let Some(e) = cause.downcast_ref::<RuntimeError>() {
        return Some(e.class());
    }
    cause.downcast_ref::<HostError>().map(Classify::class)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    fn coded(e: impl Error + Send + Sync + 'static) -> u8 {
        exit_code(&anyhow::Error::from(e))
    }

    #[test]
    fn a_configuration_problem_is_78() {
        let e = ConfigError {
            problems: vec!["DATABASE_URL is required".into()],
        };
        assert_eq!(
            exit_code(
                &Err::<(), _>(e)
                    .context("reading the configuration")
                    .unwrap_err()
            ),
            78
        );
        let e = OpenAiConfigError::InvalidApiKey;
        assert_eq!(
            exit_code(
                &Err::<(), _>(e)
                    .context("building the model client")
                    .unwrap_err()
            ),
            78
        );
        // A connection string the driver cannot parse is a configuration problem too.
        let e = StoreError::Backend {
            class: ErrorClass::Invalid,
            source: Box::new(std::io::Error::other("bad url")),
        };
        assert_eq!(coded(e), 78);
    }

    #[test]
    fn an_unreachable_dependency_is_69_even_though_an_io_error_is_the_root() {
        // Postgres refusing connections: an io::Error at the bottom, but the StoreError above it
        // is what says "unreachable", and the walk goes from the outside in.
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let e = StoreError::unavailable(io);
        let chained = Err::<(), _>(e)
            .context("connecting to Postgres")
            .unwrap_err();
        assert_eq!(exit_code(&chained), 69);
    }

    #[test]
    fn an_os_error_is_71() {
        let io = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let e = Err::<(), _>(io)
            .context("binding 0.0.0.0:8080")
            .unwrap_err();
        assert_eq!(exit_code(&e), 71);
    }

    #[test]
    fn a_component_that_stopped_or_a_bug_is_70() {
        let ended = || HostError::EndedEarly {
            component: "a2a-server".into(),
        };
        assert_eq!(coded(ended()), 70);
        assert_eq!(coded(HostError::NothingToRun), 70);
        assert_eq!(
            coded(HostError::Panicked {
                component: "worker".into(),
                source: "boom".into(),
            }),
            70
        );
        // Whatever the component failed with, the outermost layer decides: an `io::Error` from
        // the server is not a 71, and a `Transient` store error from a worker is not a 69.
        let stopped = HostError::Stopped {
            component: "a2a-server".into(),
            source: Box::new(std::io::Error::other("accept failed")),
        };
        assert_eq!(coded(stopped), 70);
        let stopped = HostError::Stopped {
            component: "worker".into(),
            source: Box::new(StoreError::unavailable(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            ))),
        };
        assert_eq!(coded(stopped), 70);
        // Under a context layer too.
        let e = Err::<(), _>(ended()).context("running").unwrap_err();
        assert_eq!(exit_code(&e), 70);
        assert_eq!(
            coded(StoreError::internal(std::io::Error::other("syntax error"))),
            70
        );
        assert_eq!(
            coded(StoreError::corrupt_source(std::io::Error::other("bad row"))),
            70
        );
    }

    #[test]
    fn anything_else_is_1() {
        assert_eq!(exit_code(&anyhow::anyhow!("something odd")), 1);
        assert_eq!(coded(WorkspaceError::Auth("bad token".into())), 1);
    }

    #[tokio::test]
    async fn a_panicked_task_is_70() {
        let join = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        let e = Err::<(), _>(join)
            .context("task panicked or was cancelled")
            .unwrap_err();
        assert_eq!(exit_code(&e), 70);
    }
}
