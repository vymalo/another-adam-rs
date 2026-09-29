//! Parse and validate agent directories.
//!
//! An agent is a directory of Markdown and JSON (`docs/authoring.md`): `instructions.md`, skills,
//! subagents, `mcp.json`, schedules. This crate reads such a directory into an [`AgentManifest`]
//! and reports every problem as a [`Diagnostic`], with the file and the line. It is the one
//! parser and the one validator behind both the build-time and the run-time path. No async, no
//! adam runtime dependency, and nothing here expands `${VAR}` or reads a secret.
//!
//! ```
//! use adam_agent_fs::{Dir, ManifestSource, Strictness};
//!
//! # fn main() -> Result<(), adam_agent_fs::Error> {
//! # let root = std::env::temp_dir().join("adam-agent-fs-doc");
//! # let _ = std::fs::remove_dir_all(&root);
//! # std::fs::create_dir_all(root.join("agent")).unwrap();
//! # std::fs::write(root.join("agent/instructions.md"), "---\nname: helper\n---\nYou help.\n").unwrap();
//! let report = Dir::new(&root).load()?;
//! for finding in &report.diagnostics {
//!     eprintln!("{finding}");
//! }
//! let package = report.into_package(Strictness::Lenient)?;
//! assert_eq!(package.agents[0].name, "helper");
//! # let _ = std::fs::remove_dir_all(&root);
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod diagnostic;
mod error;
mod frontmatter;
mod load;
mod manifest;
mod schema;
mod source;

pub use diagnostic::{Diagnostic, Severity};
pub use error::Error;
pub use frontmatter::{Split, SplitError, split};
pub use load::{parse_mcp, parse_skill};
pub use manifest::{
    AgentManifest, InstructionPart, Instructions, Layout, Package, RemoteAgent, RemoteAuth, Report,
    Schedule, Skill, SkillLayout, Strictness, Subagent,
};
pub use schema::{
    AgentFrontmatter, Card, CardSkill, EnvRef, Limits, McpConfig, McpServer, ModelRef, RemoteKind,
    ScheduleFrontmatter, SkillFrontmatter, SkillSelection, ToolList, is_agent_name, is_env_name,
    is_skill_name, is_tool_name,
};
pub use source::{Dir, ManifestSource};
