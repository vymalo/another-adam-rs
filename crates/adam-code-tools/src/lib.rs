//! The file and command tools of an adam agent, as a library ([ADR 0047](https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0047-adam-rs-can-be-embedded-in-an-application.md), decision 7).
//!
//! They move here from `adam-coder` in steps, behind a `CodeWorkspace` port that the coder
//! implements over its worktrees and a host can implement over a person's own directory. This first
//! step holds what the tools are built on: running a command ([`shell`]), metering it in a sandbox
//! ([`sandbox`]), the scopes the tools share with the other calls of a turn ([`access`]), and the
//! process's secrets kept out of everything that leaves it ([`redact`]).
//! The file and search tools are here ([`files`], [`find`]); the commands have their port and the
//! plumbing `run_command` and `run` share ([`commands`], [`output`]), and move here next.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod access;
pub mod commands;
pub mod confine;
pub mod files;
pub mod find;
pub mod git;
pub mod output;
pub mod redact;
pub mod sandbox;
pub mod shell;
pub mod workspace;

pub use commands::{CommandLimits, CommandWorkspace, DynCommandWorkspace};
pub use redact::Redactor;
pub use workspace::{
    CodeWorkspace, DynCodeWorkspace, Intent, MAX_INSTRUCTIONS_BLOCK, Outcome, Place, Target,
};
