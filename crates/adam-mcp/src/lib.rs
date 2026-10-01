//! The servers of an `mcp.json`, as tools an [`LlmAgent`](adam_llm_agent::LlmAgent) can call.
//!
//! [`McpServers::connect`] takes the [`McpConfig`](adam_agent_fs::McpConfig) that `adam-agent-fs`
//! parsed, an [`Env`] (values for `${VAR}`, before the process environment) and an [`McpPolicy`] (what
//! the deployment allows), connects to every server over streamable HTTP or, when the policy allows a
//! local process, stdio, lists their tools, and [`McpServers::tools`] gives them back as tools named
//! `<server>__<tool>`. Everything that can be wrong is an [`Error`] at startup, naming the server and
//! never a value that came from a variable.
//!
//! A call is one MCP `tools/call`. Every failure of it is an error *result* for the model, never a
//! `ToolError::Transient`: MCP has no idempotency key, so the call may or may not have run and retrying
//! it is not this crate's decision. Inside an `LlmAgent` the call is a journaled step, so a replay of a
//! committed call does not repeat it, and a transition that fails before it commits does (at-least-once).
//! `adam-assembly` (feature `mcp`) wires this into each agent; the README has the diagrams, the
//! security notes and the facts about the SDK.
#![warn(missing_docs)]

mod connection;
mod error;
mod expand;
mod once;
mod policy;
mod redact;
mod servers;
mod text;
mod tool;
mod url;

pub use error::{Error, UrlProblem, VarProblem};
pub use expand::Env;
pub use once::{Endpoint, EndpointError, RemoteResult, RemoteTool};
pub use policy::{DEFAULT_CALL_TIMEOUT, DEFAULT_CONNECT_TIMEOUT, McpPolicy};
pub use servers::McpServers;
pub use text::MAX_RESULT_BYTES;
