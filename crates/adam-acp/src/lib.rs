//! An ACP (Agent Client Protocol) client that drives a coding agent over
//! stdio, first of all `opencode acp`.
//!
//! ```no_run
//! use adam_acp::{AcpClient, AcpCommand, AcpUpdate, ClientPolicy};
//! use futures::StreamExt as _;
//!
//! # async fn demo() -> Result<(), adam_acp::AcpError> {
//! let dir = std::path::Path::new("/work/tree");
//! let client = AcpClient::spawn(AcpCommand::opencode(dir), ClientPolicy::new(dir)).await?;
//! let session = client.new_session(dir, vec![]).await?;
//! let mut turn = session.prompt("create hello.txt containing hi".into());
//! while let Some(update) = turn.next().await {
//!     if let AcpUpdate::TurnEnded { stop_reason } = update? {
//!         println!("done: {stop_reason}");
//!     }
//! }
//! client.shutdown().await?;
//! # Ok(()) }
//! ```
//!
//! # What the client answers on the agent's behalf
//!
//! * `fs/read_text_file`, `fs/write_text_file`: only under
//!   [`ClientPolicy::fs_root`]; anything else is refused with a message that
//!   says why. Writes are idempotent.
//! * `session/request_permission`: per [`PermissionMode`].
//! * everything else (`terminal/*`, ...): `method_not_found`.
//!
//! # The fake agent
//!
//! `adam-acp-fake-agent` is a scripted ACP agent for tests, built as a normal
//! binary of this crate: `cargo build -p adam-acp --bin adam-acp-fake-agent`
//! puts it at `target/<profile>/adam-acp-fake-agent`. Integration tests of
//! this crate get its path from `env!("CARGO_BIN_EXE_adam-acp-fake-agent")`;
//! other crates build it with the command above and locate it next to their
//! own test binary. Scenarios are selected with `FAKE_ACP_SCENARIO`
//! (`script`, `slow`, `crash`, `write-file`, `hang-init`); see the binary's
//! docs for the variables each one reads.

#![warn(missing_docs)]

mod client;
mod command;
mod error;
mod guard;
mod policy;
mod update;

pub use client::{AcpClient, AcpOptions, AgentInfo, Session};
pub use command::AcpCommand;
pub use error::{AcpError, AcpResult};
pub use policy::{
    ClientPolicy, DynPermissionPrompt, PermissionChoice, PermissionDecision, PermissionKind,
    PermissionMode, PermissionPrompt, PermissionRequest, StaticPrompt,
};
pub use update::{AcpUpdate, McpServerSpec, PlanEntry};
