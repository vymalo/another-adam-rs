//! Why the process cannot run, and what exit code that is.

use std::io;
use std::net::SocketAddr;

use adam_error::{Classify, ErrorClass};

use crate::ConfigError;

/// `EX_CONFIG`: the configuration is wrong; restarting will not help.
pub const EX_CONFIG: u8 = 78;
/// `EX_OSERR`: the operating system refused something (the port, the listener).
pub const EX_OSERR: u8 = 71;

/// Why the sidecar could not start, or stopped. The message names the step; the cause is the
/// [`source`](std::error::Error::source), so an error chain printed whole says each thing once.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SignerError {
    /// The environment does not describe a sidecar ([`ErrorClass::Invalid`], exit 78).
    #[error("reading the configuration")]
    Config(#[source] ConfigError),
    /// The listen address cannot be bound ([`ErrorClass::Transient`]: a port in use may free up;
    /// exit 71).
    #[error("cannot listen on {addr} (RUN_SIGNER_LISTEN)")]
    Bind {
        /// The address that was asked for.
        addr: SocketAddr,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The listener failed while serving ([`ErrorClass::Transient`]; exit 71). Not the answer to a
    /// request, which is a problem document: this is the server itself stopping.
    #[error("the listener failed")]
    Serve {
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
}

impl Classify for SignerError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Config(_) => ErrorClass::Invalid,
            Self::Bind { .. } | Self::Serve { .. } => ErrorClass::Transient,
        }
    }
}

/// The exit code of a process that ends with `err`: the BSD `sysexits.h` numbers the other binaries
/// use (`adam_service::exit_code`). A clean shutdown is 0.
pub fn exit_code(err: &SignerError) -> u8 {
    match err {
        SignerError::Config(_) => EX_CONFIG,
        SignerError::Bind { .. } | SignerError::Serve { .. } => EX_OSERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_error_has_its_class_and_its_exit_code() {
        let config = SignerError::Config(ConfigError {
            problems: vec!["x".to_owned()],
        });
        let bind = SignerError::Bind {
            addr: "127.0.0.1:8443".parse().unwrap(),
            source: io::Error::from(io::ErrorKind::AddrInUse),
        };
        let serve = SignerError::Serve {
            source: io::Error::from(io::ErrorKind::BrokenPipe),
        };
        for (err, class, code) in [
            (config, ErrorClass::Invalid, 78),
            (bind, ErrorClass::Transient, 71),
            (serve, ErrorClass::Transient, 71),
        ] {
            // Exhaustive: a new variant fails to compile here until it has a class and a code.
            match &err {
                SignerError::Config(_) | SignerError::Bind { .. } | SignerError::Serve { .. } => {}
            }
            assert_eq!(err.class(), class, "{err}");
            assert_eq!(exit_code(&err), code, "{err}");
        }
    }

    #[test]
    fn the_chain_says_each_thing_once() {
        let err = SignerError::Config(ConfigError {
            problems: vec!["RUN_SIGNER_UPSTREAM is required".to_owned()],
        });
        assert_eq!(
            adam_error::report(&err),
            "reading the configuration: invalid configuration:\n  - RUN_SIGNER_UPSTREAM is required"
        );
        let err = SignerError::Bind {
            addr: "127.0.0.1:1".parse().unwrap(),
            source: io::Error::new(io::ErrorKind::AddrInUse, "in use"),
        };
        assert_eq!(
            adam_error::report(&err),
            "cannot listen on 127.0.0.1:1 (RUN_SIGNER_LISTEN): in use"
        );
    }
}
