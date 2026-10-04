#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod exit;
pub mod harden;
pub mod logging;
mod serve;
mod service;

pub use adam_model_openai::{OpenAiConfigError, endpoint_for_logs};
#[cfg(feature = "mcp")]
pub use config::McpSettings;
pub use config::{
    ConfigError, ModelConfig, ServiceConfig, WorkerSettings, is_worker_id, parse_file, parse_flag,
    parse_or,
};
pub use exit::{
    EX_CONFIG, EX_GENERAL, EX_OSERR, EX_SOFTWARE, EX_UNAVAILABLE, exit_code, exit_code_with,
};
pub use serve::{Agents, Register, ServeError, claim_scope_for, serve};
pub use service::{LiveSignals, RuntimeOptions, Service, router};
