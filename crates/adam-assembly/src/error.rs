//! The errors of binding a manifest. A closed enum: every variant names what went wrong, and the
//! ones about the agent's files carry the agent and the file they came from ([`Origin`]).

use std::fmt;
use std::path::{Path, PathBuf};

use adam_error::{Classify, ErrorClass};
use adam_llm_agent::BuildError;

use crate::template::TemplateProblem;

/// Where a problem comes from: the agent, by its registration name (`coder`,
/// `coder/reviewer`), and the file of the manifest that says it, relative to the source root
/// (`agent/instructions.md`, `agent/subagents/reviewer.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// The agent's name as the runtime registers it. A subagent is `<parent>/<name>`.
    pub agent: String,
    /// The file, with `/` separators on every platform.
    pub file: PathBuf,
}

impl Origin {
    pub(crate) fn new(agent: impl Into<String>, file: impl Into<PathBuf>) -> Self {
        Self {
            agent: agent.into(),
            file: file.into(),
        }
    }

    /// The same agent, another file (an `instructions/*.md` part).
    pub(crate) fn in_file(&self, file: impl Into<PathBuf>) -> Self {
        Self {
            agent: self.agent.clone(),
            file: file.into(),
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "agent `{}` ({})", self.agent, portable(&self.file))
    }
}

fn portable(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Why a model alias was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasProblem {
    /// The alias is empty (or only blanks).
    Empty,
    /// The alias has whitespace or a control character: a gateway alias is one token.
    NotAToken,
    /// [`BoundDef::model_aliases`](crate::BoundDef::model_aliases) lists the aliases the
    /// deployment serves, and this is not one of them.
    NotAllowed {
        /// The closest allowed alias, when one is close.
        suggestion: Option<String>,
        /// Every allowed alias, in the order given.
        allowed: Vec<String>,
    },
}

impl fmt::Display for AliasProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the alias is empty"),
            Self::NotAToken => {
                f.write_str("a gateway alias is one token: no whitespace or control characters")
            }
            Self::NotAllowed {
                suggestion,
                allowed,
            } => write!(
                f,
                "not one of the aliases this deployment serves{}; allowed: {}",
                hint(suggestion.as_deref()),
                list(allowed)
            ),
        }
    }
}

/// The frontmatter key that names a skill that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillField {
    /// `skills:`, the skills the agent may use.
    Skills,
    /// `preload_skills:`, the skills whose body is in the prompt.
    PreloadSkills,
}

impl fmt::Display for SkillField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Skills => "skills",
            Self::PreloadSkills => "preload_skills",
        })
    }
}

/// What a subagent's tool name collides with, in [`Error::SubagentToolClash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolClash {
    /// A tool of the parent: one registered with
    /// [`AgentDef::bind`](crate::AgentDef::bind) that the parent's `tools:` (or its default) gives
    /// it.
    Tool,
    /// `load_skill` or `read_skill_file`, the tools the parent's skills bring.
    SkillTool,
    /// Another subagent of the same parent, defined in this file.
    Subagent {
        /// The other subagent's file, relative to the source root.
        file: PathBuf,
    },
    /// A tool of an MCP server of the parent's own `mcp.json` (`<server>__<tool>`).
    McpTool {
        /// The server whose tool it is. A `Box<str>` and not a `String` so that this enum stays as
        /// small as the `PathBuf` of the other variant: [`Error`] is returned by value everywhere.
        server: Box<str>,
    },
}

/// Why the bearer token of a remote subagent was refused, in [`Error::RemoteAuth`]. Never carries
/// the value of the variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAuthProblem {
    /// The variable is not set, in the environment or through
    /// [`AgentDef::env`](crate::AgentDef::env).
    Missing,
    /// The variable is set, and empty (or only blanks).
    Empty,
    /// The value cannot be a bearer token: it has whitespace or a control character inside it, a
    /// character outside printable ASCII, or is not valid Unicode.
    NotAToken,
}

impl fmt::Display for RemoteAuthProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "it is not set",
            Self::Empty => "it is empty",
            Self::NotAToken => "its value is not a bearer token (printable ASCII, no whitespace)",
        })
    }
}

/// Why the URL of a remote subagent was refused, in [`Error::RemoteUrl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteUrlProblem {
    /// It does not parse as a URL.
    Unparseable,
    /// It is not `http` or `https`, or has no host.
    NotHttp,
    /// It has a user name or a password in it. The URL is logged and shown in errors; the token
    /// goes in `auth: bearer:VAR`.
    Credentials,
    /// It is plain `http` to a host that is not this machine (`localhost`, `127.0.0.0/8`, `::1`).
    /// The messages and the token would cross the network in the clear.
    Insecure,
}

impl fmt::Display for RemoteUrlProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unparseable => "it is not a URL",
            Self::NotHttp => "it is not an http(s) URL with a host",
            Self::Credentials => {
                "it has a user name or password in it: put the token in `auth: bearer:VAR` instead"
            }
            Self::Insecure => {
                "it is plain http to a host that is not this machine, so every message and the \
                 token would cross the network in the clear: use https (or, for development \
                 only, call AgentDef::allow_insecure_remotes)"
            }
        })
    }
}

/// Why an [`AgentDef`](crate::AgentDef) could not be bound to agents.
///
/// A mistake in the agent's files or in how it is put together, found when the process
/// starts. The same input never succeeds, so every variant is [`ErrorClass::Invalid`] (except
/// [`Manifest`](Self::Manifest), which keeps the class of its source).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The manifest could not be read: an embedded manifest that does not decode, or a source
    /// that could not be loaded.
    #[error("cannot read the agent manifest: {0}")]
    Manifest(#[from] adam_agent_fs::Error),
    /// Two registered tools have one name, so `tools:` could not tell them apart.
    #[error("two registered tools are called `{tool}`")]
    DuplicateTool {
        /// The repeated name.
        tool: String,
    },
    /// `tools:` names a tool nobody registered.
    #[error(
        "{origin}: `tools` names `{tool}`, which is not a registered tool{}; registered: {}",
        hint(.suggestion.as_deref()),
        list(.available)
    )]
    UnknownTool {
        /// The agent and file that name it.
        origin: Origin,
        /// The name as written.
        tool: String,
        /// The closest registered name, when one is close.
        suggestion: Option<String>,
        /// Every registered tool, in registration order.
        available: Vec<String>,
    },
    /// A `tools:` pattern (`linear__*`) matches no registered tool.
    #[error(
        "{origin}: the `tools` pattern `{pattern}` matches no registered tool; registered: {}",
        list(.available)
    )]
    NoToolMatches {
        /// The agent and file that name it.
        origin: Origin,
        /// The pattern as written.
        pattern: String,
        /// Every registered tool, in registration order.
        available: Vec<String>,
    },
    /// A `{{` in the prompt that is not a placeholder.
    #[error("{origin}: prompt line {line}: {problem}")]
    Template {
        /// The agent and the prompt file (`instructions.md` or an `instructions/*.md` part).
        origin: Origin,
        /// The line within the body of that file: the frontmatter is not counted.
        line: u32,
        /// What is wrong.
        problem: TemplateProblem,
    },
    /// The prompt uses a placeholder the agent does not declare under `vars`.
    #[error(
        "{origin}: prompt line {line}: uses `{}`, which `vars` does not declare{}; declared: {}",
        placeholder(.var),
        hint(.suggestion.as_deref()),
        list(.declared)
    )]
    UnknownVar {
        /// The agent and the prompt file.
        origin: Origin,
        /// The line within the body of that file: the frontmatter is not counted.
        line: u32,
        /// The var as written.
        var: String,
        /// The closest declared var, when one is close.
        suggestion: Option<String>,
        /// Every declared var, sorted.
        declared: Vec<String>,
    },
    /// The code supplied a value for a var the agent does not declare.
    #[error(
        "{origin}: a value was supplied for var `{var}`, which `vars` does not declare{}; declared: {}",
        hint(.suggestion.as_deref()),
        list(.declared)
    )]
    UnknownVarValue {
        /// The agent and file whose `vars` lack it.
        origin: Origin,
        /// The var as supplied.
        var: String,
        /// The closest declared var, when one is close.
        suggestion: Option<String>,
        /// Every declared var, sorted.
        declared: Vec<String>,
    },
    /// A var is declared and the prompt never uses it.
    #[error(
        "{origin}: var `{var}` is declared under `vars` but the prompt never uses `{}`: use it or remove it",
        placeholder(.var)
    )]
    UnusedVar {
        /// The agent and file that declare it.
        origin: Origin,
        /// The var.
        var: String,
    },
    /// A var the prompt uses has no value: it is declared with an empty default (`vars: {
    /// repo: }`) and the code did not supply one.
    #[error(
        "{origin}: var `{var}` has no value: give it a default under `vars`, or supply one with `AgentDef::var`"
    )]
    UnsetVar {
        /// The agent and file that declare it.
        origin: Origin,
        /// The var.
        var: String,
    },
    /// Values were supplied for an agent the definition does not contain.
    #[error(
        "no agent `{agent}` to set vars on{}; agents: {}",
        hint(.suggestion.as_deref()),
        list(.known)
    )]
    UnknownAgent {
        /// The name as given to `AgentDef::agent_var`.
        agent: String,
        /// The closest agent, when one is close.
        suggestion: Option<String>,
        /// Every agent of the definition, root first.
        known: Vec<String>,
    },
    /// A model alias was refused.
    #[error("{origin}: model alias `{alias}`: {problem}")]
    ModelAlias {
        /// The agent that uses the alias, and its file.
        origin: Origin,
        /// The alias.
        alias: String,
        /// Why it was refused.
        problem: AliasProblem,
    },
    /// [`LlmAgent`](adam_llm_agent::LlmAgent) refused to be built: a tool needs state the
    /// composition root did not give ([`BoundDef::state`](crate::BoundDef::state)).
    #[error("{origin}: {source}")]
    Build {
        /// The agent that could not be built.
        origin: Origin,
        /// Why.
        #[source]
        source: BuildError,
    },
    /// `skills:` or `preload_skills:` names a skill the agent does not have. An agent's skills
    /// are the ones under its own `skills/`; a subagent inherits none.
    #[error(
        "{origin}: `{field}` names `{skill}`, which is not a skill of this agent{}; its skills: {}",
        hint(.suggestion.as_deref()),
        list(.available)
    )]
    UnknownSkill {
        /// The agent and file that name it.
        origin: Origin,
        /// The key that names it.
        field: SkillField,
        /// The name as written.
        skill: String,
        /// The closest skill of the agent, when one is close.
        suggestion: Option<String>,
        /// Every skill of the agent, sorted.
        available: Vec<String>,
    },
    /// `preload_skills:` names a skill that `skills:` does not select.
    #[error(
        "{origin}: `preload_skills` names `{skill}`, which `skills` does not select: add it to \
         `skills` or remove it; selected: {}",
        list(.selected)
    )]
    PreloadNotSelected {
        /// The agent and file that name it.
        origin: Origin,
        /// The skill.
        skill: String,
        /// The skills `skills:` selects, in order.
        selected: Vec<String>,
    },
    /// A selected skill bundles a file whose bytes were not supplied: the manifest was made
    /// without its source. An embedded agent, [`AgentDef::from_source`](crate::AgentDef::from_source)
    /// and [`AgentDef::resources_from`](crate::AgentDef::resources_from) supply them.
    #[error(
        "{origin}: skill `{skill}` bundles `{file}`, but its bytes were not supplied: read the agent with \
         AgentDef::from_source, or call AgentDef::resources_from with the source of its files"
    )]
    SkillFilesUnavailable {
        /// The agent that uses the skill.
        origin: Origin,
        /// The skill.
        skill: String,
        /// The first file without bytes.
        file: String,
    },
    /// A skill bundles more than [`SKILL_RESOURCE_LIMIT`](adam_agent_fs::SKILL_RESOURCE_LIMIT)
    /// bytes: they are held in memory, so the build script refuses it too.
    #[error(
        "{origin}: the files bundled with skill `{skill}` are {bytes} bytes; at most {limit} may be \
         bundled: move the rest into the sandbox seed or shrink them"
    )]
    SkillTooLarge {
        /// The agent that owns the skill and the skill's `SKILL.md`.
        origin: Origin,
        /// The skill.
        skill: String,
        /// The size of its files (or the running total when the limit was passed).
        bytes: u64,
        /// The limit.
        limit: u64,
    },
    /// The agent has skills, so it gets the tools `load_skill` and `read_skill_file`, and a
    /// registered tool already has one of those names.
    #[error(
        "{origin}: the agent has skills, which bring a tool called `{tool}`, but a tool with that \
         name is registered too: rename it, or leave it out of `tools`, or set `skills: []`"
    )]
    ReservedToolName {
        /// The agent and file.
        origin: Origin,
        /// The name in conflict.
        tool: String,
    },
    /// A subagent is called as a tool named after it, and its parent already has a tool of that
    /// name.
    #[error(
        "{origin}: the parent `{parent}` would get a tool called `{tool}` to call this subagent, \
         but {}",
        clash_reason(.clash)
    )]
    SubagentToolClash {
        /// The subagent and its file.
        origin: Origin,
        /// The registration name of the parent.
        parent: String,
        /// The tool name, which is the subagent's name.
        tool: String,
        /// What it collides with.
        clash: ToolClash,
    },
    /// A subagent has a tool that asks the user a question ([`Tool::asks_user`]). A subagent runs as
    /// a child of another run, so nobody could answer, and the child would wait for ever.
    ///
    /// [`Tool::asks_user`]: adam_llm_agent::Tool::asks_user
    #[error(
        "{origin}: a subagent cannot have `{tool}`: it asks the user a question, and a subagent runs \
         as a child of another run, so nobody could answer it. List its `tools` by name, without \
         `{tool}`, or let the parent ask instead"
    )]
    SubagentAsksUser {
        /// The subagent and its file.
        origin: Origin,
        /// The tool that asks.
        tool: String,
    },
    /// A remote subagent's `auth: bearer:VAR` names an environment variable that cannot give a
    /// token. The token is read at [`AgentDef::bind`](crate::AgentDef::bind), and a remote
    /// subagent with no usable credential is refused (fail closed) rather than called without.
    #[error(
        "{origin}: `auth: bearer:{var}` needs the environment variable `{var}` to hold the token, \
         but {problem}: set it, or remove `auth` from the remote subagent"
    )]
    RemoteAuth {
        /// The remote subagent and its file.
        origin: Origin,
        /// The variable's name (never its value).
        var: String,
        /// What is wrong with it.
        problem: RemoteAuthProblem,
    },
    /// A remote subagent's `a2a:` URL cannot be used.
    #[error("{origin}: `a2a: {url}` is refused: {problem}")]
    RemoteUrl {
        /// The remote subagent and its file.
        origin: Origin,
        /// The URL as it can be shown: without a user name or password, and without a query.
        url: String,
        /// Why.
        problem: RemoteUrlProblem,
    },
    /// An agent's `mcp.json` lists MCP servers and nothing was connected for it: the tools of those
    /// servers would be missing, and an agent must not start without tools its files promise (fail
    /// closed).
    #[error(
        "{origin}: `mcp.json` lists the MCP servers {}, but none is connected, so their tools do \
         not exist: {}",
        list(.servers),
        not_connected_how()
    )]
    McpNotConnected {
        /// The agent and its `mcp.json`.
        origin: Origin,
        /// The servers the file lists, sorted.
        servers: Vec<String>,
    },
    /// The servers were connected from an `mcp.json` that is not the one the agent has now.
    #[error(
        "{origin}: `mcp.json` is not the file its servers were connected with: connections are \
         made once, when the process starts, so restart the process to use the change"
    )]
    McpChanged {
        /// The agent and its `mcp.json`.
        origin: Origin,
    },
    /// A tool given for an agent with [`AgentDef::mcp_tools`](crate::AgentDef::mcp_tools) is not
    /// named after one of the servers of the agent's own `mcp.json`.
    #[error(
        "{origin}: the MCP tool `{tool}` does not belong to a server of this agent's `mcp.json` \
         (its servers: {}): a tool is named `<server>__<tool>`",
        list(.servers)
    )]
    McpForeignTool {
        /// The agent and its `mcp.json`.
        origin: Origin,
        /// The tool's name.
        tool: String,
        /// The servers of the agent's `mcp.json`, sorted.
        servers: Vec<String>,
    },
    /// An MCP tool has the name of a registered tool, so `tools:` could not tell them apart.
    #[error(
        "{origin}: the MCP tool `{tool}` has the name of a registered tool: rename the registered \
         tool, or list the tools of the MCP server under `tools` in `mcp.json` without it"
    )]
    McpToolClash {
        /// The agent and its `mcp.json`.
        origin: Origin,
        /// The name in conflict.
        tool: String,
    },
    /// [`AgentDef::mcp_tools`](crate::AgentDef::mcp_tools) names an agent the definition does not
    /// contain.
    #[error(
        "no agent `{agent}` to give MCP tools to{}; agents: {}",
        hint(.suggestion.as_deref()),
        list(.known)
    )]
    McpUnknownAgent {
        /// The name as given to `AgentDef::mcp_tools`.
        agent: String,
        /// The closest agent, when one is close.
        suggestion: Option<String>,
        /// Every agent of the definition, root first.
        known: Vec<String>,
    },
    /// The MCP servers of an agent could not be connected. Only
    /// [`AgentDef::connect_mcp`](crate::AgentDef::connect_mcp) (feature `mcp`) makes it, but the
    /// variant is always there, so that the feature adds no variant to this exhaustive enum and a
    /// `match` that compiles with the feature compiles without it.
    #[error("{origin}: {source}")]
    Mcp {
        /// The agent and its `mcp.json`.
        origin: Origin,
        /// The class of the client's error, kept here because `source` is opaque: a server that is
        /// down is [`ErrorClass::Transient`], the rest of what the client refuses is
        /// [`ErrorClass::Invalid`].
        class: ErrorClass,
        /// Why: with the feature `mcp`, an `adam_mcp::Error`, which `downcast_ref` gets back.
        /// Boxed: the error of the client is larger than the rest of this enum, and opaque so
        /// that this crate's public type does not name the client without the feature.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A folder that was to hold one agent holds none, or several
    /// ([`AgentFolder::load`](crate::AgentFolder::load)): a process serves one agent.
    #[error(
        "{} holds {}: a process serves exactly one agent, so point it at a folder with a single \
         `agent/` (or an `agents/` with one agent in it)",
        portable(.root),
        found_agents(.found)
    )]
    NotOneAgent {
        /// The folder that was read.
        root: PathBuf,
        /// The root agents it holds, by name; empty when it holds none.
        found: Vec<String>,
    },
    /// The A2A card needs a description and the agent has none.
    #[error(
        "{origin}: the A2A card needs a description: set `description` (or `card.description`) in the frontmatter"
    )]
    MissingCardDescription {
        /// The root agent and its file.
        origin: Origin,
    },
}

#[cfg(feature = "mcp")]
impl Error {
    /// The error of connecting `origin`'s servers, as [`Error::Mcp`].
    pub(crate) fn mcp(origin: Origin, source: adam_mcp::Error) -> Self {
        Self::Mcp {
            origin,
            class: source.class(),
            source: Box::new(source),
        }
    }
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Manifest(source) => source.class(),
            Self::Mcp { class, .. } => *class,
            _ => ErrorClass::Invalid,
        }
    }
}

/// The second half of [`Error::SubagentToolClash`].
fn clash_reason(clash: &ToolClash) -> String {
    match clash {
        ToolClash::Tool => {
            "it already has a tool with that name: rename the subagent, or leave the \
             tool out of the parent's `tools`"
                .to_owned()
        }
        ToolClash::SkillTool => "its skills bring a tool with that name (`load_skill` and \
             `read_skill_file` are reserved): rename the subagent"
            .to_owned(),
        ToolClash::Subagent { file } => format!(
            "another subagent of the parent, in {}, has the same name: rename one of them",
            portable(file)
        ),
        ToolClash::McpTool { server } => format!(
            "the parent's MCP server `{server}` has a tool with that name: rename the subagent, \
             or list the server's tools under `tools` in `mcp.json` without it"
        ),
    }
}

/// What to do about [`Error::McpNotConnected`], which depends on whether this build has the
/// feature `mcp`.
fn not_connected_how() -> &'static str {
    if cfg!(feature = "mcp") {
        "call AgentDef::connect_mcp before bind (or AgentDef::mcp_tools to give the tools of a \
         client of your own)"
    } else {
        "enable the feature `mcp` of adam-assembly and call AgentDef::connect_mcp before bind (or \
         call AgentDef::mcp_tools to give the tools of a client of your own)"
    }
}

/// `; did you mean `x`?`, or nothing.
pub(crate) fn hint(suggestion: Option<&str>) -> String {
    suggestion.map_or_else(String::new, |s| format!("; did you mean `{s}`?"))
}

/// What a folder holds, for [`Error::NotOneAgent`].
fn found_agents(found: &[String]) -> String {
    match found {
        [] => "no agent".to_owned(),
        _ => format!("{} agents ({})", found.len(), list(found)),
    }
}

/// The names in backticks, or `none`.
pub(crate) fn list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_owned()
    } else {
        items
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// `{{name}}`, as written in a prompt.
fn placeholder(var: &str) -> String {
    format!("{{{{{var}}}}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin::new("coder", PathBuf::from("agent").join("instructions.md"))
    }

    #[test]
    fn an_origin_names_the_agent_and_the_file() {
        assert_eq!(
            origin().to_string(),
            "agent `coder` (agent/instructions.md)"
        );
        let part = origin().in_file("agent/instructions/10-style.md");
        assert_eq!(part.agent, "coder");
        assert_eq!(
            part.to_string(),
            "agent `coder` (agent/instructions/10-style.md)"
        );
        // Windows separators are shown as `/`.
        assert_eq!(
            Origin::new("a", "agent\\subagents\\x.md").to_string(),
            "agent `a` (agent/subagents/x.md)"
        );
    }

    #[test]
    fn a_suggestion_reads_as_a_question() {
        assert_eq!(hint(Some("run_checks")), "; did you mean `run_checks`?");
        assert_eq!(hint(None), "");
        assert_eq!(list(&[]), "none");
        assert_eq!(list(&["a".into(), "b".into()]), "`a`, `b`");
        assert_eq!(placeholder("n"), "{{n}}");
    }

    #[test]
    fn every_message_names_the_agent_and_says_what_to_do() {
        let o = origin();
        let cases = [
            Error::UnknownTool {
                origin: o.clone(),
                tool: "run_check".into(),
                suggestion: Some("run_checks".into()),
                available: vec!["run_checks".into()],
            },
            Error::NoToolMatches {
                origin: o.clone(),
                pattern: "linear__*".into(),
                available: vec![],
            },
            Error::Template {
                origin: o.clone(),
                line: 2,
                problem: TemplateProblem::Unclosed,
            },
            Error::UnknownVar {
                origin: o.clone(),
                line: 3,
                var: "max_check_cycle".into(),
                suggestion: Some("max_check_cycles".into()),
                declared: vec!["max_check_cycles".into()],
            },
            Error::UnknownVarValue {
                origin: o.clone(),
                var: "x".into(),
                suggestion: None,
                declared: vec![],
            },
            Error::UnusedVar {
                origin: o.clone(),
                var: "strict".into(),
            },
            Error::UnsetVar {
                origin: o.clone(),
                var: "repo".into(),
            },
            Error::ModelAlias {
                origin: o.clone(),
                alias: "big".into(),
                problem: AliasProblem::NotAllowed {
                    suggestion: Some("bigger".into()),
                    allowed: vec!["bigger".into()],
                },
            },
            Error::UnknownSkill {
                origin: o.clone(),
                field: SkillField::PreloadSkills,
                skill: "triag".into(),
                suggestion: Some("triage".into()),
                available: vec!["triage".into()],
            },
            Error::PreloadNotSelected {
                origin: o.clone(),
                skill: "triage".into(),
                selected: vec![],
            },
            Error::SkillFilesUnavailable {
                origin: o.clone(),
                skill: "triage".into(),
                file: "a.md".into(),
            },
            Error::SkillTooLarge {
                origin: o.clone(),
                skill: "triage".into(),
                bytes: 2_000_000,
                limit: 1_048_576,
            },
            Error::ReservedToolName {
                origin: o.clone(),
                tool: "load_skill".into(),
            },
            Error::SubagentToolClash {
                origin: o.clone(),
                parent: "coder".into(),
                tool: "reviewer".into(),
                clash: ToolClash::Tool,
            },
            Error::SubagentToolClash {
                origin: o.clone(),
                parent: "coder".into(),
                tool: "load_skill".into(),
                clash: ToolClash::SkillTool,
            },
            Error::SubagentToolClash {
                origin: o.clone(),
                parent: "coder".into(),
                tool: "reviewer".into(),
                clash: ToolClash::Subagent {
                    file: "agent/subagents/reviewer.md".into(),
                },
            },
            Error::SubagentAsksUser {
                origin: o.clone(),
                tool: "ask_user".into(),
            },
            Error::RemoteAuth {
                origin: o.clone(),
                var: "BILLING_TOKEN".into(),
                problem: RemoteAuthProblem::Missing,
            },
            Error::RemoteUrl {
                origin: o.clone(),
                url: "http://billing.example.com/".into(),
                problem: RemoteUrlProblem::Insecure,
            },
            Error::McpNotConnected {
                origin: o.clone(),
                servers: vec!["linear".into(), "fs".into()],
            },
            Error::McpChanged { origin: o.clone() },
            Error::McpForeignTool {
                origin: o.clone(),
                tool: "github__list".into(),
                servers: vec!["linear".into()],
            },
            Error::McpToolClash {
                origin: o.clone(),
                tool: "linear__list".into(),
            },
            Error::SubagentToolClash {
                origin: o.clone(),
                parent: "coder".into(),
                tool: "linear__list".into(),
                clash: ToolClash::McpTool {
                    server: "linear".into(),
                },
            },
            Error::MissingCardDescription { origin: o },
        ];
        for error in cases {
            let text = error.to_string();
            assert!(
                text.contains("agent `coder` (agent/instructions.md)"),
                "{text}"
            );
            assert_eq!(error.class(), ErrorClass::Invalid);
        }
        let text = Error::UnknownVar {
            origin: origin(),
            line: 3,
            var: "max_check_cycle".into(),
            suggestion: Some("max_check_cycles".into()),
            declared: vec!["max_check_cycles".into()],
        }
        .to_string();
        assert_eq!(
            text,
            "agent `coder` (agent/instructions.md): prompt line 3: uses `{{max_check_cycle}}`, \
             which `vars` does not declare; did you mean `max_check_cycles`?; declared: `max_check_cycles`"
        );
    }

    #[test]
    fn remote_problems_name_the_variable_and_never_a_value() {
        let error = Error::RemoteAuth {
            origin: origin(),
            var: "BILLING_TOKEN".into(),
            problem: RemoteAuthProblem::Missing,
        };
        assert_eq!(
            error.to_string(),
            "agent `coder` (agent/instructions.md): `auth: bearer:BILLING_TOKEN` needs the \
             environment variable `BILLING_TOKEN` to hold the token, but it is not set: set it, \
             or remove `auth` from the remote subagent"
        );
        for problem in [
            RemoteAuthProblem::Empty,
            RemoteAuthProblem::NotAToken,
            RemoteAuthProblem::Missing,
        ] {
            assert!(!problem.to_string().is_empty());
        }
        let url = Error::RemoteUrl {
            origin: origin(),
            url: "http://billing.example.com/card".into(),
            problem: RemoteUrlProblem::Insecure,
        };
        assert!(url.to_string().contains("in the clear"), "{url}");
        for problem in [
            RemoteUrlProblem::Unparseable,
            RemoteUrlProblem::NotHttp,
            RemoteUrlProblem::Credentials,
        ] {
            assert!(!problem.to_string().is_empty());
        }
    }

    #[test]
    fn alias_problems_read_plainly() {
        assert_eq!(AliasProblem::Empty.to_string(), "the alias is empty");
        assert!(AliasProblem::NotAToken.to_string().contains("one token"));
        assert_eq!(
            AliasProblem::NotAllowed {
                suggestion: None,
                allowed: vec![]
            }
            .to_string(),
            "not one of the aliases this deployment serves; allowed: none"
        );
    }

    #[test]
    fn the_mcp_variant_exists_without_the_feature_and_keeps_its_class() {
        // Built by hand, as a client of one's own would: no feature `mcp` is needed to name it, to
        // match it, or to ask its class (the enum stays additive).
        let down = Error::Mcp {
            origin: Origin::new("coder", "agent/mcp.json"),
            class: ErrorClass::Transient,
            source: "connection refused".into(),
        };
        let Error::Mcp { origin, source, .. } = &down else {
            panic!("{down}");
        };
        assert_eq!(origin.agent, "coder");
        assert_eq!(source.to_string(), "connection refused");
        assert_eq!(down.class(), ErrorClass::Transient);
        assert_eq!(
            down.to_string(),
            "agent `coder` (agent/mcp.json): connection refused"
        );
        assert!(std::error::Error::source(&down).is_some());
        let refused = Error::Mcp {
            origin: Origin::new("coder", "agent/mcp.json"),
            class: ErrorClass::Invalid,
            source: "not allowed".into(),
        };
        assert_eq!(refused.class(), ErrorClass::Invalid);
    }

    #[test]
    fn mcp_problems_say_what_to_do() {
        let not_connected = Error::McpNotConnected {
            origin: Origin::new("coder", "agent/mcp.json"),
            servers: vec!["fs".into(), "linear".into()],
        };
        let text = not_connected.to_string();
        assert!(
            text.starts_with(
                "agent `coder` (agent/mcp.json): `mcp.json` lists the MCP servers `fs`, `linear`"
            ),
            "{text}"
        );
        assert!(
            text.contains("AgentDef::connect_mcp") && text.contains("AgentDef::mcp_tools"),
            "{text}"
        );
        let unknown = Error::McpUnknownAgent {
            agent: "coder/reserch".into(),
            suggestion: Some("coder/research".into()),
            known: vec!["coder".into(), "coder/research".into()],
        };
        assert_eq!(
            unknown.to_string(),
            "no agent `coder/reserch` to give MCP tools to; did you mean `coder/research`?; \
             agents: `coder`, `coder/research`"
        );
        assert_eq!(unknown.class(), ErrorClass::Invalid);
    }

    #[test]
    fn the_other_variants_are_described() {
        let dup = Error::DuplicateTool { tool: "t".into() };
        assert_eq!(dup.to_string(), "two registered tools are called `t`");
        assert_eq!(dup.class(), ErrorClass::Invalid);
        let unknown = Error::UnknownAgent {
            agent: "coder/reviwer".into(),
            suggestion: Some("coder/reviewer".into()),
            known: vec!["coder".into(), "coder/reviewer".into()],
        };
        assert_eq!(
            unknown.to_string(),
            "no agent `coder/reviwer` to set vars on; did you mean `coder/reviewer`?; \
             agents: `coder`, `coder/reviewer`"
        );
        let build = Error::Build {
            origin: origin(),
            source: BuildError::MissingState {
                tool: "run_checks".into(),
                state: "ToolEnv".into(),
            },
        };
        assert!(build.to_string().contains("needs shared state `ToolEnv`"));
        assert_eq!(build.class(), ErrorClass::Invalid);
        let missing = Error::Manifest(adam_agent_fs::Error::Io {
            path: "agent".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        });
        assert_eq!(missing.class(), ErrorClass::NotFound);
    }

    #[test]
    fn a_folder_that_does_not_hold_one_agent_says_what_it_holds() {
        let many = Error::NotOneAgent {
            root: PathBuf::from("/etc/adam"),
            found: vec!["coder".into(), "reviewer".into()],
        };
        assert_eq!(
            many.to_string(),
            "/etc/adam holds 2 agents (`coder`, `reviewer`): a process serves exactly one agent, \
             so point it at a folder with a single `agent/` (or an `agents/` with one agent in it)"
        );
        let none = Error::NotOneAgent {
            root: PathBuf::from("/etc/adam"),
            found: vec![],
        };
        assert!(
            none.to_string().starts_with("/etc/adam holds no agent:"),
            "{none}"
        );
        assert_eq!(many.class(), ErrorClass::Invalid);
        assert_eq!(none.class(), ErrorClass::Invalid);
    }
}
