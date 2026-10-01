//! How a binary ends: which exit code an error maps to.
//!
//! A binary is [`serve`](crate::serve) under an error chain: every step adds context, and the root
//! cause is a typed error somewhere down the chain. [`exit_code`] walks the chain from the
//! outside in and returns the first code that fits, so a supervisor can tell a deployment that is
//! misconfigured (do not restart) from one whose database is down (restart later) from a bug.
//!
//! | Code | Name | Root cause |
//! |---|---|---|
//! | 0 | | clean shutdown after a signal (not an error) |
//! | 78 | `EX_CONFIG` | [`ConfigError`], `OpenAiConfigError`, [`ServeError::NoCard`], or any error whose class is `Invalid` (a mistake in the agent's files, for the errors a binary classifies with [`exit_code_with`]) |
//! | 69 | `EX_UNAVAILABLE` | a dependency is unreachable: a `Transient`, `RateLimited` or `Conflict` error, such as Postgres or an MCP server |
//! | 71 | `EX_OSERR` | an [`std::io::Error`], and [`ServeError::Bind`]: a listener that cannot bind, a directory that cannot be created |
//! | 70 | `EX_SOFTWARE` | [`HostError`] (a component of the process stopped, panicked or ended while still needed), a panicked task, or a `Corrupt` or `Internal` error |
//! | 1 | | anything else |
//!
//! The values are those of BSD `sysexits.h`, *unverified* (from memory; the header is not part of
//! this repository's sources).
//!
//! The errors of this crate and of the crates it composes (the store, the model client, the
//! runtime, the host) are known here. An error of a binary's own (the files of an agent, a
//! workspace) is classified by the closure the binary gives [`exit_code_with`].

use std::error::Error;

use adam_core::StoreError;
use adam_error::{Classify, ErrorClass};
use adam_host::HostError;
use adam_model_openai::OpenAiConfigError;
use adam_runtime::RuntimeError;

use crate::{ConfigError, ServeError};

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
pub fn exit_code(err: &(dyn Error + 'static)) -> u8 {
    exit_code_with(err, |_| None)
}

/// [`exit_code`] for a binary that has errors of its own: `classify` is asked about each link of
/// the chain first, and returns the class of the ones it knows (the files of its agent, its
/// workspaces), `None` for the others.
pub fn exit_code_with(
    err: &(dyn Error + 'static),
    classify: impl Fn(&(dyn Error + 'static)) -> Option<ErrorClass>,
) -> u8 {
    let mut link = Some(err);
    while let Some(cause) = link {
        if let Some(code) = code_of(cause, &classify) {
            return code;
        }
        link = cause.source();
    }
    EX_GENERAL
}

/// The code `cause` decides on, if it is one of the typed errors the steps of a binary return.
fn code_of(
    cause: &(dyn Error + 'static),
    classify: &impl Fn(&(dyn Error + 'static)) -> Option<ErrorClass>,
) -> Option<u8> {
    // What stopped a component is not what the component failed with: a `HostError` decides, whatever
    // its source is (an `io::Error` from the server is not a 71, a store error from a worker not a 69).
    if cause.is::<HostError>() {
        return Some(EX_SOFTWARE);
    }
    if cause.is::<ConfigError>() {
        return Some(EX_CONFIG);
    }
    if let Some(e) = cause.downcast_ref::<ServeError>() {
        match e {
            ServeError::Host(_) => return Some(EX_SOFTWARE),
            ServeError::NoCard => return Some(EX_CONFIG),
            ServeError::Bind { .. } | ServeError::LocalAddr(_) => return Some(EX_OSERR),
            // The cause, next in the chain, says why.
            ServeError::Connect(_) | ServeError::Migrate(_) => {}
        }
    }
    if let Some(class) = classify(cause).or_else(|| class_of(cause)) {
        return Some(match class {
            ErrorClass::Invalid => EX_CONFIG,
            ErrorClass::Transient | ErrorClass::RateLimited | ErrorClass::Conflict => {
                EX_UNAVAILABLE
            }
            ErrorClass::Corrupt | ErrorClass::Internal => EX_SOFTWARE,
            _ => EX_GENERAL,
        });
    }
    if cause.is::<tokio::task::JoinError>() {
        return Some(EX_SOFTWARE);
    }
    if cause.is::<std::io::Error>() {
        return Some(EX_OSERR);
    }
    None
}

/// The class of `cause` when it is one of the typed errors of the crates this one composes.
fn class_of(cause: &(dyn Error + 'static)) -> Option<ErrorClass> {
    if let Some(e) = cause.downcast_ref::<StoreError>() {
        return Some(e.class());
    }
    if let Some(e) = cause.downcast_ref::<OpenAiConfigError>() {
        return Some(e.class());
    }
    cause.downcast_ref::<RuntimeError>().map(Classify::class)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error with a cause: what a `.context()` layer of a binary is.
    #[derive(Debug, thiserror::Error)]
    #[error("{what}")]
    struct Layer {
        what: &'static str,
        #[source]
        source: Box<dyn Error + Send + Sync>,
    }

    fn layered(what: &'static str, source: impl Error + Send + Sync + 'static) -> Layer {
        Layer {
            what,
            source: Box::new(source),
        }
    }

    fn coded(e: impl Error + 'static) -> u8 {
        exit_code(&e)
    }

    #[test]
    fn a_configuration_problem_is_78() {
        let e = ConfigError::new(vec!["DATABASE_URL is required".into()]);
        assert_eq!(coded(layered("reading the configuration", e)), 78);
        assert_eq!(
            coded(layered(
                "building the model client",
                OpenAiConfigError::InvalidApiKey
            )),
            78
        );
        // A connection string the driver cannot parse is a configuration problem too.
        let e = StoreError::Backend {
            class: ErrorClass::Invalid,
            source: Box::new(std::io::Error::other("bad url")),
        };
        assert_eq!(coded(e), 78);
        assert_eq!(coded(ServeError::NoCard), 78);
    }

    #[test]
    fn an_unreachable_dependency_is_69_even_though_an_io_error_is_the_root() {
        // Postgres refusing connections: an io::Error at the bottom, but the StoreError above it
        // is what says "unreachable", and the walk goes from the outside in.
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let e = ServeError::Connect(StoreError::unavailable(io));
        assert_eq!(coded(layered("starting", e)), 69);
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(coded(ServeError::Migrate(StoreError::unavailable(io))), 69);
    }

    #[test]
    fn an_os_error_is_71() {
        let io = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        assert_eq!(coded(layered("binding 0.0.0.0:8080", io)), 71);
        let bind = ServeError::Bind {
            addr: "0.0.0.0:8080".parse().unwrap(),
            source: std::io::Error::from(std::io::ErrorKind::AddrInUse),
        };
        assert_eq!(coded(bind), 71);
        let local = ServeError::LocalAddr(std::io::Error::other("no address"));
        assert_eq!(coded(local), 71);
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
        assert_eq!(coded(ServeError::Host(stopped)), 70);
        // Under a layer too.
        assert_eq!(coded(layered("running", ended())), 70);
        assert_eq!(coded(layered("running", ServeError::Host(ended()))), 70);
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
        assert_eq!(exit_code(&std::fmt::Error), 1);
        assert_eq!(coded(layered("something odd", std::fmt::Error)), 1);
    }

    #[tokio::test]
    async fn a_panicked_task_is_70() {
        let join = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
        assert_eq!(coded(layered("task panicked or was cancelled", join)), 70);
    }

    /// A binary's own error is classified by what it gives `exit_code_with`, before the known ones.
    #[test]
    fn a_binary_classifies_its_own_errors() {
        #[derive(Debug, thiserror::Error)]
        #[error("the agent files disagree with the code")]
        struct Files;

        let classify = |cause: &(dyn Error + 'static)| {
            cause.downcast_ref::<Files>().map(|_| ErrorClass::Invalid)
        };
        let e = layered("assembling the agent", Files);
        assert_eq!(exit_code_with(&e, classify), 78);
        // Without the closure it is not known.
        assert_eq!(exit_code(&e), 1);
        // The known ones are still known.
        let e = layered("x", StoreError::unavailable(std::io::Error::other("down")));
        assert_eq!(exit_code_with(&e, classify), 69);
    }
}
