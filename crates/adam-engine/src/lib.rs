//! adam-rs embedded in an application ([ADR 0047]): the runtime on the host's tokio runtime, a
//! store and a model client the host gives, no server, no protocol and nothing on the network of
//! its own.
//!
//! **Slice 1 of ADR 0047:** this crate is the embedded set and nothing more yet. Its dependencies
//! are the crates an embedded agent needs, each with its server parts turned off
//! (`adam-a2a-runtime` without `push`, `adam-ui` without `thread-tools`, and through them
//! `adam-a2a` without `server`), and `tools/embedded-deps/check.sh` fails when its tree holds an
//! HTTP server or client, a database driver, the Kubernetes client, a TLS stack or the MCP client.
//! The API (start, send, steer, answer, cancel, list and the events) is slice 4.
//!
//! **Secrets** ([ADR 0047] decision 5): an embedded agent reads none of the person's environment.
//! [`default_secrets`] refuses every name, and a host that has secrets for the agent's files gives
//! a source of its own (`adam_core::SecretSource`) to the agent's definition.
//!
//! [ADR 0047]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use adam_a2a_runtime;
pub use adam_agent_fs;
pub use adam_core;
pub use adam_error;
pub use adam_llm_agent;
pub use adam_model;
pub use adam_runtime;
pub use adam_ui;

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
