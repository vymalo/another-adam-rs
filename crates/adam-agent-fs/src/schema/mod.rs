//! The serde shapes of every file kind. They only *read*; the rules live in `crate::load`.

mod agent;
mod command;
mod mcp;
mod mode;
mod names;
mod reference;
mod scalar;
mod schedule;
mod skill;

pub use agent::{
    AgentFrontmatter, Card, CardSkill, DEFAULT_RETRY_BACKOFF, DEFAULT_RETRY_MAX_BACKOFF,
    ExtendedCard, Limits, MAX_INLINE_UPLOAD_BYTES_LIMIT, MAX_MINUTES_LIMIT,
    MAX_OPEN_SESSIONS_LIMIT, MAX_PARALLEL_LIMIT, MAX_PROCESSES_LIMIT, MAX_RETRY_ATTEMPTS,
    MAX_RETRY_BACKOFF, MAX_SESSION_ROUNDS_LIMIT, MAX_WORKING_SESSIONS_LIMIT, ModelRef, OutputDef,
    OutputModeDef, PluginRef, RETRY_CLASS_NAMES, RetryDef, SessionDef, SessionModeDef,
    SkillSelection, ToolList, ToolsLoading, is_retry_tool_name, retry_duration,
};
pub use command::CommandFrontmatter;
pub use mcp::{
    EnvRef, MAX_GROUP_NAME_CHARS, MAX_WHEN_KEYWORD_CHARS, MAX_WHEN_KEYWORDS, McpConfig, McpServer,
    McpSettings, McpWhen, RemoteKind, Segment, group_name_fits, split_env_references,
};
pub use mode::ModeFrontmatter;
pub use names::{is_agent_name, is_env_name, is_skill_name, is_tool_name};
pub use reference::{
    MAX_REFERENCES, REFERENCE_BRANCH_MAX, REFERENCE_DESCRIPTION_MAX, REFERENCE_URL_MAX,
    REFERENCES_FILE_MAX, ReferenceDef, ReferencesRead, check_references, is_reference_name,
    parse_references_file, read_references_leniently,
};
pub use schedule::ScheduleFrontmatter;
pub use skill::SkillFrontmatter;

pub(crate) use mcp::{RawMcp, RawServer, scan_references};
