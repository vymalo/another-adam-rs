//! The coder agent: a coding task in, a verified pull request out, over A2A.
//!
//! Given "in repo X, do Y", the agent
//!
//! 1. prepares a git worktree (`prepare_workspace`, `adam-workspace`),
//! 2. has OpenCode make the change over ACP (`delegate_to_opencode`, `adam-acp`),
//! 3. runs the project's own checks, at most a configured number of failing
//!    cycles (`run_checks`),
//! 4. commits, pushes and opens a pull request (`commit_and_push`,
//!    `open_pull_request`), and
//! 5. streams progress throughout and reports the pull request as an artifact.
//!
//! It is durable (an [`adam_runtime::Runtime`] journals every model and tool
//! step, so a restarted worker replays instead of repeating side effects) and
//! addressable (an [`adam_a2a`] server over [`adam_a2a_runtime`]).
//!
//! # Pieces
//!
//! | Module | What |
//! |---|---|
//! | [`agent`] | [`CoderAgent`]: `LlmAgent` + the completion policy (red checks and no PR = failed) |
//! | [`tools`] | the six tools, [`ToolEnv`] and [`CoderSettings`] |
//! | [`instructions`] | the system prompt |
//! | [`redact`] | [`Redactor`]: the process's own secrets never leave in an error, an event or a tool result |
//! | [`opencode`] | OpenCode's generated configuration and how it is launched |
//! | [`app`] | [`Coder`]: runtime + A2A backend + router |
//! | [`config`] | the binary's environment variables |
//!
//! # Composition
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use adam_coder::*;
//! # use adam_coder::opencode::OpenCodeLaunch;
//! # async fn demo(
//! #     store: adam_core::DynStore,
//! #     model: adam_model::DynModel,
//! #     workspaces: adam_workspace::Workspaces,
//! #     code_host: adam_workspace::DynCodeHost,
//! # ) {
//! let launch = OpenCodeLaunch::opencode("https://gateway.example/v1", "coder-large");
//! let env = Arc::new(ToolEnv::new(workspaces, code_host, CoderSettings::new(launch)));
//! let agent = CoderAgent::new(model, "coder-large", env);
//! let coder = Coder::new(store, agent, &RuntimeOptions::default());
//! # let _ = coder;
//! # }
//! ```
//!
//! Every infrastructure piece is a trait object handed in from outside: the
//! store, the model, the code host and the git credentials. The binary
//! (`adam-coder`) is only a composition of the Postgres store, the
//! OpenAI-compatible model and GitHub.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod app;
pub mod config;
pub mod instructions;
pub mod opencode;
pub mod redact;
mod repos;
pub mod tools;

pub use agent::{AGENT_NAME, CoderAgent, coder_limits};
pub use app::{Coder, RuntimeOptions, agent_card};
pub use config::{Config, ConfigError};
pub use instructions::instructions;
pub use redact::Redactor;
pub use repos::workspaces_for;
pub use tools::{CoderSettings, ToolEnv, coder_tools};
