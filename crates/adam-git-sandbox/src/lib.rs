#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod client;
pub mod config;
pub mod cwd;
pub mod environment;
mod error;
pub mod protocol;
mod server;
pub mod subcommand;

pub use client::{Client, Output};
pub use config::{Config, ConfigError, DEFAULT_MAX_COMMANDS, SOCKET_ENV, socket_from_env};
pub use error::{
    ClientError, EX_CONFIG, EX_IOERR, EX_NOPERM, EX_OSERR, EX_UNAVAILABLE, SandboxError, exit_code,
};
pub use protocol::{Exit, Refusal, RefusalReason, Request};
pub use server::Server;
