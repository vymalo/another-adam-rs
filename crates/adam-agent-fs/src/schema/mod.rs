//! The serde shapes of every file kind. They only *read*; the rules live in `crate::load`.

mod agent;
mod command;
mod mcp;
mod names;
mod reference;
mod scalar;
mod schedule;
mod skill;

pub use agent::{
    AgentFrontmatter, Card, CardSkill, ExtendedCard, Limits, MAX_MINUTES_LIMIT, ModelRef,
    SkillSelection, ToolList,
};
pub use command::CommandFrontmatter;
pub use mcp::{EnvRef, McpConfig, McpServer, RemoteKind, Segment, split_env_references};
pub use names::{is_agent_name, is_env_name, is_skill_name, is_tool_name};
pub use reference::{
    MAX_REFERENCES, REFERENCE_BRANCH_MAX, REFERENCE_DESCRIPTION_MAX, REFERENCE_URL_MAX,
    REFERENCES_FILE_MAX, ReferenceDef, ReferencesRead, check_references, is_reference_name,
    parse_references_file, read_references_leniently,
};
pub use schedule::ScheduleFrontmatter;
pub use skill::SkillFrontmatter;

pub(crate) use mcp::{RawMcp, RawServer, scan_references};
