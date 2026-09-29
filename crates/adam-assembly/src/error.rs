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
    /// The A2A card needs a description and the agent has none.
    #[error(
        "{origin}: the A2A card needs a description: set `description` (or `card.description`) in the frontmatter"
    )]
    MissingCardDescription {
        /// The root agent and its file.
        origin: Origin,
    },
}

impl Classify for Error {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Manifest(source) => source.class(),
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
    }
}

/// `; did you mean `x`?`, or nothing.
pub(crate) fn hint(suggestion: Option<&str>) -> String {
    suggestion.map_or_else(String::new, |s| format!("; did you mean `{s}`?"))
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
}
