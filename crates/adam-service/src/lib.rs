#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod exit;
pub mod harden;
pub mod logging;
#[cfg(feature = "mcp")]
mod prompt_commands;
mod serve;
mod service;

pub use adam_model_openai::{OpenAiConfigError, endpoint_for_logs};
#[cfg(feature = "mcp")]
pub use config::McpSettings;
pub use config::{
    A2aSettings, CardSigning, ConfigError, MAX_CONTEXT_WINDOW, ModelConfig, PromptCacheKey,
    PushSettings, ServiceConfig, SessionSettings, WorkerSettings, is_worker_id, parse_file,
    parse_flag, parse_or,
};
pub use exit::{
    EX_CONFIG, EX_GENERAL, EX_OSERR, EX_SOFTWARE, EX_UNAVAILABLE, exit_code, exit_code_with,
};
#[cfg(feature = "mcp")]
pub use prompt_commands::{card_with_commands, refuse_prompt_commands, with_prompt_commands};
pub use serve::{
    Agents, Register, ServeError, card_with_reasoning, claim_scope_for, serve, serve_on,
};
pub use service::{LiveSignals, RuntimeOptions, Service, router, router_with_options};
