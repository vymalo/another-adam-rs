//! Why the process cannot run, and what exit code that is.

use std::io;
use std::net::SocketAddr;

use adam_error::{Classify, ErrorClass};

use crate::ConfigError;

/// `EX_CONFIG`: the configuration is wrong; restarting will not help.
pub const EX_CONFIG: u8 = 78;
/// `EX_OSERR`: the operating system refused something (the port).
pub const EX_OSERR: u8 = 71;

/// Why [`run`](crate::run) could not start. The message names the step; the cause is the
/// [`source`](std::error::Error::source), so an error chain printed whole says each thing once.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EgressError {
    /// The environment does not describe a proxy ([`ErrorClass::Invalid`], exit 78).
    #[error("reading the configuration")]
    Config(#[source] ConfigError),
    /// The listen address cannot be bound ([`ErrorClass::Transient`]: a port in use may free up;
    /// exit 71).
    #[error("cannot listen on {addr} (EGRESS_LISTEN)")]
    Bind {
        /// The address that was asked for.
        addr: SocketAddr,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
}

impl Classify for EgressError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Config(_) => ErrorClass::Invalid,
            Self::Bind { .. } => ErrorClass::Transient,
        }
    }
}

/// The exit code of a process that ends with `err`: the BSD `sysexits.h` numbers the other
/// binaries use (`adam_service::exit_code`; *unverified*, from memory). A clean shutdown is 0.
pub fn exit_code(err: &EgressError) -> u8 {
    match err {
        EgressError::Config(_) => EX_CONFIG,
        EgressError::Bind { .. } => EX_OSERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_error_has_its_class_and_its_exit_code() {
        let config = EgressError::Config(ConfigError {
            problems: vec!["x".to_owned()],
        });
        let bind = EgressError::Bind {
            addr: "0.0.0.0:3128".parse().unwrap(),
            source: io::Error::from(io::ErrorKind::AddrInUse),
        };
        for (err, class, code) in [
            (config, ErrorClass::Invalid, 78),
            (bind, ErrorClass::Transient, 71),
        ] {
            // Exhaustive: a new variant fails to compile here until it has a class and a code.
            match &err {
                EgressError::Config(_) | EgressError::Bind { .. } => {}
            }
            assert_eq!(err.class(), class, "{err}");
            assert_eq!(exit_code(&err), code, "{err}");
        }
    }

    #[test]
    fn the_chain_says_each_thing_once() {
        let err = EgressError::Config(ConfigError {
            problems: vec!["EGRESS_PORTS names no port".to_owned()],
        });
        assert_eq!(
            adam_error::report(&err),
            "reading the configuration: invalid configuration:\n  - EGRESS_PORTS names no port"
        );
        let err = EgressError::Bind {
            addr: "127.0.0.1:1".parse().unwrap(),
            source: io::Error::new(io::ErrorKind::AddrInUse, "in use"),
        };
        assert_eq!(
            adam_error::report(&err),
            "cannot listen on 127.0.0.1:1 (EGRESS_LISTEN): in use"
        );
    }
}
