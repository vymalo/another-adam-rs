#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod config;
mod error;
pub mod exit;
pub mod folder;
pub mod redact;
mod serve;

pub use agent::{
    BUILD_REVISION, VERSION, WorkerParts, agents, assemble, assemble_with, build_version, card_of,
    card_of_folder,
};
pub use config::{Config, ConfigError, McpSettings, WorkerConfig};
pub use error::AgentError;
pub use exit::exit_code;
pub use serve::serve;
