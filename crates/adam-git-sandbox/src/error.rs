//! Why the server cannot run, why a command could not be run through it, and the exit codes.

use std::io;
use std::path::PathBuf;

use adam_error::{Classify, ErrorClass};

use crate::config::ConfigError;
use crate::protocol::{ProtocolError, Refusal, RefusalReason};

/// `EX_CONFIG`: the configuration is wrong; restarting will not help.
pub const EX_CONFIG: u8 = 78;
/// `EX_OSERR`: the operating system refused something (the socket).
pub const EX_OSERR: u8 = 71;
/// `EX_UNAVAILABLE`: the sandbox could not be reached (the relay's code).
pub const EX_UNAVAILABLE: u8 = 69;
/// `EX_NOPERM`: the sandbox refused the command (the relay's code).
pub const EX_NOPERM: u8 = 77;
/// `EX_IOERR`: the conversation with the sandbox failed (the relay's code).
pub const EX_IOERR: u8 = 74;

/// Why [`Server::bind`](crate::Server::bind) cannot start the server. The message names the step;
/// the cause is the [`source`](std::error::Error::source), so an error chain printed whole says each
/// thing once.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// The environment does not describe a server ([`ErrorClass::Invalid`], exit 78).
    #[error("reading the configuration")]
    Config(#[source] ConfigError),
    /// The socket cannot be made: the path is taken by something that is not a socket, a server
    /// answers on it, or the directory refuses ([`ErrorClass::Transient`], exit 71).
    #[error("cannot listen on {path} (GIT_SANDBOX_LISTEN)")]
    Bind {
        /// The path that was asked for.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
}

impl Classify for SandboxError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Config(_) => ErrorClass::Invalid,
            Self::Bind { .. } => ErrorClass::Transient,
        }
    }
}

/// The exit code of a server that ends with `err`: the BSD `sysexits.h` numbers the other binaries
/// use (*unverified*, from memory). A clean shutdown is 0.
pub fn exit_code(err: &SandboxError) -> u8 {
    match err {
        SandboxError::Config(_) => EX_CONFIG,
        SandboxError::Bind { .. } => EX_OSERR,
    }
}

/// Why a command could not be run through the sandbox. None of these is an exit code of the command:
/// it did not run, or the answer did not arrive.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Nobody answers at the socket. The command was **not** run anywhere else: a client never falls
    /// back to running git itself.
    #[error("cannot reach the git sandbox at {}", socket.display())]
    Unreachable {
        /// The socket that was tried.
        socket: PathBuf,
        /// What the operating system said.
        #[source]
        source: io::Error,
    },
    /// The server did not start the command.
    #[error("the git sandbox refused the command: {0}")]
    Refused(Refusal),
    /// The connection ended before the command did.
    #[error("the git sandbox closed the connection before the command ended")]
    Disconnected,
    /// The conversation broke the protocol, or the transport failed in it.
    #[error("the conversation with the git sandbox failed")]
    Protocol(#[source] ProtocolError),
    /// Writing the command's output where the caller wanted it failed.
    #[error("cannot write the command's output")]
    Output(#[source] io::Error),
}

impl Classify for ClientError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Unreachable { .. } | Self::Disconnected | Self::Output(_) => {
                ErrorClass::Transient
            }
            Self::Refused(refusal) => match refusal.reason {
                RefusalReason::ShuttingDown => ErrorClass::Transient,
                RefusalReason::SpawnFailed => ErrorClass::Internal,
                _ => ErrorClass::Invalid,
            },
            Self::Protocol(e) => e.class(),
        }
    }
}

impl ClientError {
    /// The code the relay ends with: 69 when nobody answers, 77 when the command is refused, 74 when
    /// the conversation fails.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Unreachable { .. } => EX_UNAVAILABLE,
            Self::Refused(_) => EX_NOPERM,
            Self::Disconnected | Self::Protocol(_) | Self::Output(_) => EX_IOERR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_server_error_has_its_class_and_its_exit_code() {
        let config = SandboxError::Config(ConfigError {
            problems: vec!["x".to_owned()],
        });
        let bind = SandboxError::Bind {
            path: "/run/git.sock".into(),
            source: io::Error::from(io::ErrorKind::AddrInUse),
        };
        for (err, class, code) in [
            (config, ErrorClass::Invalid, 78),
            (bind, ErrorClass::Transient, 71),
        ] {
            // Exhaustive: a new variant fails to compile here until it has a class and a code.
            match &err {
                SandboxError::Config(_) | SandboxError::Bind { .. } => {}
            }
            assert_eq!(err.class(), class, "{err}");
            assert_eq!(exit_code(&err), code, "{err}");
        }
    }

    #[test]
    fn each_client_error_has_its_class_and_its_exit_code() {
        let refused = |reason| {
            ClientError::Refused(Refusal {
                reason,
                message: "m".to_owned(),
            })
        };
        let cases = [
            (
                ClientError::Unreachable {
                    socket: "/s".into(),
                    source: io::Error::from(io::ErrorKind::NotFound),
                },
                ErrorClass::Transient,
                69,
            ),
            (
                refused(RefusalReason::BadDirectory),
                ErrorClass::Invalid,
                77,
            ),
            (
                refused(RefusalReason::ShuttingDown),
                ErrorClass::Transient,
                77,
            ),
            (
                refused(RefusalReason::SpawnFailed),
                ErrorClass::Internal,
                77,
            ),
            (ClientError::Disconnected, ErrorClass::Transient, 74),
            (
                ClientError::Protocol(ProtocolError::Malformed("exit")),
                ErrorClass::Corrupt,
                74,
            ),
            (
                ClientError::Output(io::Error::from(io::ErrorKind::BrokenPipe)),
                ErrorClass::Transient,
                74,
            ),
        ];
        for (err, class, code) in cases {
            match &err {
                ClientError::Unreachable { .. }
                | ClientError::Refused(_)
                | ClientError::Disconnected
                | ClientError::Protocol(_)
                | ClientError::Output(_) => {}
            }
            assert_eq!(err.class(), class, "{err}");
            assert_eq!(err.exit_code(), code, "{err}");
        }
    }

    #[test]
    fn the_chain_says_each_thing_once() {
        let err = SandboxError::Bind {
            path: "/run/git.sock".into(),
            source: io::Error::new(io::ErrorKind::AddrInUse, "in use"),
        };
        assert_eq!(
            adam_error::report(&err),
            "cannot listen on /run/git.sock (GIT_SANDBOX_LISTEN): in use"
        );
    }
}
