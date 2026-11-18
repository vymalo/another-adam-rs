//! What the coder's tools share with the other calls of their model turn (ADR 0049).
//!
//! The scopes are the coder's own words: `files` covers every slot of the workspace (the `repo`
//! argument is not looked at, because two spellings can name one slot; the cost is a false conflict
//! between equal paths in different slots), `commands` is the one command at a time of a workspace,
//! `screen` the person's screen, `lsp` the language servers' own queue. A tool with one use says it
//! in its `#[tool(parallel = .., path = ..)]`; the tools with two name a function here.
//! `docs/reference/coder-agent.md` ("Calls at the same time") lists every tool's class, and
//! `tests/tool_access.rs` pins the list.

use adam_llm_agent::{ToolAccess, Use};
use serde_json::Value;

/// The scope of the files of every slot, under which a path names one file.
pub const FILES: &str = "files";
/// The scope of the commands of the workspace: one runs at a time.
pub const COMMANDS: &str = "commands";
/// The scope of the language servers: their calls queue in the loop, not behind the servers' gate.
pub const LSP: &str = "lsp";

/// `run_command`: it reads the files (a command may show any of them) and takes the one command's
/// place. It waits for a write that came before it and runs beside reads.
pub fn runs_a_command(_args: &Value) -> ToolAccess {
    ToolAccess::Uses(vec![Use::read(FILES), Use::write(COMMANDS)])
}

/// `run` and `start_command`: a command that may change any file, and takes the command's place.
pub fn makes_files_with_a_command(_args: &Value) -> ToolAccess {
    ToolAccess::Uses(vec![Use::write(FILES), Use::write(COMMANDS)])
}

/// `stop_command`: it ends a command, so it is ordered with the ones that start them.
pub fn stops_a_command(_args: &Value) -> ToolAccess {
    ToolAccess::writes(COMMANDS)
}

/// `list_commands`: it looks at the commands, so it waits for one that is starting or stopping.
pub fn lists_the_commands(_args: &Value) -> ToolAccess {
    ToolAccess::reads(COMMANDS)
}

/// `file_diagnostics`, `find_definition`, `find_references`: they read the whole slot (the server
/// syncs what changed first) and take turns on the servers.
pub fn asks_a_language_server(_args: &Value) -> ToolAccess {
    ToolAccess::Uses(vec![Use::read(FILES), Use::write(LSP)])
}
