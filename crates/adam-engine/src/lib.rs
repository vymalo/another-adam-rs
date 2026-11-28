//! adam-rs embedded in an application ([ADR 0047]): the runtime on the host's tokio runtime, a
//! store and a model client the host gives, no server, no protocol and nothing on the network of
//! its own.
//!
//! ```no_run
//! # async fn demo(agent: adam_assembly::AgentDef, model: adam_model::DynModel) -> Result<(), adam_engine::EngineError> {
//! use std::sync::Arc;
//! use adam_engine::{Engine, Say};
//!
//! let store = Arc::new(adam_core::MemoryStore::new());
//! let engine = Engine::builder(store, agent, model, "my-alias").start().await?;
//! let turn = engine.send(None, Say::new("hello")).await?;
//! // later: the next message of the same conversation continues it
//! engine.send(Some(&turn.conversation), Say::new("and then?")).await?;
//! engine.shutdown().await?;
//! # Ok(()) }
//! ```
//!
//! [`Engine::builder`] takes the store, the agent's definition, the model and its alias; `start`
//! binds the agent with the screen's tools of an embedded agent and starts the worker. Then
//! [`Engine::send`] starts or continues a conversation, [`Engine::steer`] and [`Engine::answer`]
//! reach a run that works or waits, [`Engine::approve`] answers a hook's ask to run a tool call
//! ([`RunState::AwaitingApproval`], the host's hooks given with [`EngineBuilder::plugin`]),
//! [`Engine::cancel`] ends one, and [`Engine::list`] and
//! [`Engine::get`] read them. [`Engine::events`] follows a run's events to its end, bounded in
//! memory however slowly it is read ([`RunEvents`]). No A2A type is in a signature: inside, every
//! call is the code the A2A server calls (`RuntimeTaskBackend`).
//!
//! **The embedded set.** The dependencies are the crates an embedded agent needs, each with its
//! server parts turned off (`adam-a2a-runtime` without `push`, `adam-ui` without `thread-tools`,
//! `adam-assembly` without `remote`, and through them `adam-a2a` without `server`), and
//! `tools/embedded-deps/check.sh` fails when its tree holds an HTTP server or client, a database
//! driver, the Kubernetes client, a TLS stack or the MCP client.
//!
//! **Secrets** ([ADR 0047] decision 5): an embedded agent reads none of the person's environment.
//! [`default_secrets`] refuses every name, and a host that has secrets for the agent's files gives
//! a source of its own ([`EngineBuilder::secrets`]).
//!
//! [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use adam_a2a_runtime;
pub use adam_agent_fs;
pub use adam_assembly;
pub use adam_core;
pub use adam_device_state;
pub use adam_error;
pub use adam_llm_agent;
pub use adam_model;
pub use adam_runtime;
pub use adam_schedule;
pub use adam_ui;

mod engine;
mod error;
mod events;
pub mod local_agent;
mod resources;
mod types;

pub use engine::{Engine, EngineBuilder, Limits};
pub use error::EngineError;
pub use events::{EngineEvent, RunEvents, WithheldFile};
pub use local_agent::AsksBeforeWritingOrCommand;
pub use resources::ResourceReport;
pub use types::{
    ApprovalRequest, ConversationId, DEFAULT_PAGE_SIZE, Decision, Delivered, Firing, FiringQuery,
    ListQuery, MAX_PAGE_SIZE, Page, RunFile, RunInfo, RunState, Say, SessionHelper, SessionInfo,
    SessionState, StartSession, Turn,
};

#[doc(hidden)]
pub use adam_code_tools::PersonDir;

use std::sync::Arc;

use adam_core::{DynSecretSource, NoSecrets};

/// The secrets an embedded agent's files may name until the host gives a source of its own:
/// none. Every name is refused, the ones the process environment has included.
pub fn default_secrets() -> DynSecretSource {
    Arc::new(NoSecrets)
}

#[cfg(test)]
mod tests {
    use adam_core::SecretError;

    use super::*;

    /// ADR 0047 slice 2's proof: a name the environment has is refused by the engine's default.
    #[test]
    fn a_name_the_environment_has_is_refused_by_the_engines_default() {
        assert!(std::env::var_os("PATH").is_some());
        let secrets = default_secrets();
        assert_eq!(secrets.secret("PATH").unwrap_err(), SecretError::Refused);
        assert_eq!(format!("{secrets:?}"), "SecretSource(none)");
    }
}
