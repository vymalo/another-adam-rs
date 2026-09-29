//! The serde shapes of every file kind. They only *read*; the rules live in `crate::load`.

mod agent;
mod mcp;
mod names;
mod scalar;
mod schedule;
mod skill;

pub use agent::{AgentFrontmatter, Card, CardSkill, Limits, ModelRef, SkillSelection, ToolList};
pub use mcp::{EnvRef, McpConfig, McpServer, RemoteKind, Segment, split_env_references};
pub use names::{is_agent_name, is_env_name, is_skill_name, is_tool_name};
pub use schedule::ScheduleFrontmatter;
pub use skill::SkillFrontmatter;

pub(crate) use mcp::{RawMcp, RawServer, scan_references};
