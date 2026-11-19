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
