//! The owned manifest: what a directory of agent files means, after parsing and validation.

use crate::when::{Cron, When, WhenError, parse_after, parse_at};
use std::path::{Path, PathBuf};

use serde::{Serialize, Serializer};

use crate::schema::{AgentFrontmatter, McpConfig, SessionDef, SkillFrontmatter, ToolList};
use crate::{Diagnostic, Error};

/// The most bytes of resources (`scripts/`, `references/`, `assets/`) one skill may bundle: 1 MiB.
/// A build script refuses a skill over it, and the run-time readers that load a skill's bytes
/// (`adam-assembly`) refuse it again, whatever the source.
pub const SKILL_RESOURCE_LIMIT: u64 = 1024 * 1024;

/// The most commands one agent may have.
pub const MAX_COMMANDS: usize = 50;
/// The most characters of a command's `description`.
pub const COMMAND_DESCRIPTION_MAX: usize = 200;
/// The most characters of a command's `argument-hint`.
pub const COMMAND_HINT_MAX: usize = 100;
/// The most bytes of a command's template: the same cap as the text it expands to.
pub const COMMAND_TEMPLATE_MAX: usize = 32 * 1024;

/// The most modes one agent may declare.
pub const MAX_MODES: usize = 16;
/// The most characters of a mode's `title`.
pub const MODE_TITLE_MAX: usize = 60;
/// The most characters of a mode's `description`.
pub const MODE_DESCRIPTION_MAX: usize = 200;

/// How a skill is laid out on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SkillLayout {
    /// `skills/<name>/SKILL.md`, with optional `scripts/`, `references/` and `assets/`.
    Directory,
    /// `skills/<name>.md`: an eve convenience. It may omit the frontmatter.
    Flat,
}

/// A skill, in the Agent Skills sense: a catalog entry and a body loaded on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    /// The name the catalog uses: the directory name (or the file stem of a flat skill).
    pub name: String,
    /// What the skill does and when to use it.
    pub description: String,
    /// The `license` field.
    pub license: Option<String>,
    /// The `compatibility` field.
    pub compatibility: Option<String>,
    /// The `metadata` map.
    pub metadata: std::collections::BTreeMap<String, String>,
    /// The `allowed-tools` entries. Parsed, ignored in v1.
    pub allowed_tools: Vec<String>,
    /// The Markdown after the frontmatter, LF line endings, trimmed.
    pub body: String,
    /// Directory or flat.
    pub layout: SkillLayout,
    /// The `SKILL.md` (or flat) file, relative to the source root.
    #[serde(serialize_with = "portable_path")]
    pub path: PathBuf,
    /// The other files of a directory skill, relative to the skill directory with `/`
    /// separators, sorted. Their contents are not read here.
    pub resources: Vec<String>,
}

impl Skill {
    pub(crate) fn from_parts(
        name: String,
        description: String,
        fm: SkillFrontmatter,
        body: String,
        layout: SkillLayout,
        path: PathBuf,
    ) -> Self {
        Self {
            name,
            description,
            license: fm.license,
            compatibility: fm.compatibility,
            metadata: fm.metadata,
            allowed_tools: fm.allowed_tools,
            body,
            layout,
            path,
            resources: Vec::new(),
        }
    }
}

/// How to authenticate to a remote subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum RemoteAuth {
    /// `bearer:VAR`: the token is in the environment variable `VAR`, read at startup.
    Bearer {
        /// The variable's name (never its value).
        env: String,
    },
}

/// A subagent that lives elsewhere: an A2A agent, addressed by its agent-card URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteAgent {
    /// The tool name the parent sees.
    pub name: String,
    /// The tool description the parent reads.
    pub description: String,
    /// The agent-card URL.
    pub url: String,
    /// Credentials, if the agent needs them.
    pub auth: Option<RemoteAuth>,
    /// `files: true`: the file parts of the agent's answer are shared as files of the calling run.
    /// Left out of the JSON (and so of the digest) when `false`.
    #[serde(skip_serializing_if = "is_false")]
    pub files: bool,
    /// The frontmatter's `session:`: the defaults and the ceiling of the sessions a parent starts
    /// on it (ADR 0087 decision 4). Left out of the JSON (and so of the digest) when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionDef>,
    /// The body of the file, if any: it extends the tool description.
    pub note: String,
    /// The file, relative to the source root.
    #[serde(serialize_with = "portable_path")]
    pub path: PathBuf,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A subagent: a local agent of its own, or a remote A2A agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Subagent {
    /// Hosted in this process, with its own instructions, tools, skills and subagents.
    Local(Box<AgentManifest>),
    /// An A2A agent (`a2a:` in the frontmatter).
    Remote(RemoteAgent),
}

impl Subagent {
    /// The subagent's name.
    pub fn name(&self) -> &str {
        match self {
            Self::Local(a) => &a.name,
            Self::Remote(r) => &r.name,
        }
    }
}

/// One extra file of `instructions/`, appended after `instructions.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstructionPart {
    /// The file name.
    pub file: String,
    /// Its content, LF line endings, trimmed.
    pub body: String,
}

/// The system prompt of an agent, in the pieces it is written in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Instructions {
    /// The body of `instructions.md` (or of the subagent file).
    pub body: String,
    /// `instructions/*.md` in filename order.
    pub parts: Vec<InstructionPart>,
}

impl Instructions {
    /// The whole prompt: the body, then each part, separated by a blank line. `{{placeholders}}`
    /// are still in it.
    pub fn prompt(&self) -> String {
        std::iter::once(self.body.as_str())
            .chain(self.parts.iter().map(|p| p.body.as_str()))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// A scheduled prompt (`schedules/<name>.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Schedule {
    /// From the path: `schedules/a/b.md` is `a/b`.
    pub name: String,
    /// A five-field cron expression; empty for an `at` or `after` schedule. Left out of the JSON
    /// when empty, and `at` and `after` when absent, so a cron schedule's digest is what it was
    /// before they existed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cron: String,
    /// `at`, as written: see [`parse_at`](crate::parse_at).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// `after`, as written: see [`parse_after`](crate::parse_after).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// An IANA time zone name; `UTC` when the file names none. Its spelling is checked here, the
    /// zone itself where schedules are evaluated (`adam-schedule`).
    pub timezone: String,
    /// The agent it runs.
    pub agent: String,
    /// The body of the file.
    pub prompt: String,
    /// The file, relative to the source root.
    #[serde(serialize_with = "portable_path")]
    pub path: PathBuf,
}

impl Schedule {
    /// When it fires, parsed again from the keys the loader checked. `Err` only for a `Schedule`
    /// built by hand (or embedded) with a value the loader would have refused, or with none or
    /// more than one of `cron`, `at` and `after`.
    pub fn when(&self) -> Result<When, WhenError> {
        match (
            self.cron.as_str(),
            self.at.as_deref(),
            self.after.as_deref(),
        ) {
            (cron, None, None) if !cron.is_empty() => Cron::parse(cron).map(When::Cron),
            ("", Some(at), None) => parse_at(at).map(When::At),
            ("", None, Some(after)) => parse_after(after).map(When::After),
            _ => Err(WhenError::new(
                "needs exactly one of `cron`, `at` or `after`",
            )),
        }
    }
}

/// A command (`commands/<name>.md`): a prompt template an agent serves, expanded by the agent
/// when a message invokes it (`commands/v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Command {
    /// The file stem; the agent-name rule.
    pub name: String,
    /// What it does, at most 200 characters.
    pub description: String,
    /// What follows the name, for a client to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// The body, with `$ARGUMENTS` and `$1` to `$9`, LF line endings, trimmed.
    pub template: String,
    /// The file, relative to the source root.
    #[serde(serialize_with = "portable_path")]
    pub path: PathBuf,
}

/// A mode (`modes/<name>.md`): a prompt, its own `tools:`, and the name a person picks for a
/// message (`agent-mode/v1`, ADR 0039 decision 3).
///
/// A mode only narrows: its `tools:` is the allowlist of the tools the model is shown while it is
/// in force, and its prompt is added to the agent's instructions. The access tier still caps what
/// those tools may do, and a mode never widens it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mode {
    /// The file stem; the agent-name rule.
    pub name: String,
    /// What a client shows.
    pub title: String,
    /// One line on what the mode does.
    pub description: String,
    /// The tools the model is shown in this mode, when the file narrows them. `None`: the agent's
    /// own list, unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolList>,
    /// `true` on the one mode a run works in when its message names none.
    #[serde(skip_serializing_if = "is_false")]
    pub default: bool,
    /// The body: words added to the agent's instructions while the mode is in force. Empty for a
    /// mode that only narrows.
    pub prompt: String,
    /// The file, relative to the source root.
    #[serde(serialize_with = "portable_path")]
    pub path: PathBuf,
}

/// One agent with everything that belongs to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentManifest {
    /// The agent's name.
    pub name: String,
    /// The instructions file (or the subagent file), relative to the source root.
    #[serde(serialize_with = "portable_path")]
    pub path: PathBuf,
    /// The parsed frontmatter, with Claude's `maxTurns` folded into `limits`.
    pub frontmatter: AgentFrontmatter,
    /// The prompt.
    pub instructions: Instructions,
    /// The agent's own skills, sorted by name.
    pub skills: Vec<Skill>,
    /// The agent's subagents, sorted by name. Subagents inherit nothing from their parent.
    pub subagents: Vec<Subagent>,
    /// The agent's own `mcp.json`.
    pub mcp: Option<McpConfig>,
    /// The agent's schedules (root agents only), sorted by name.
    pub schedules: Vec<Schedule>,
    /// The agent's commands (root agents only), sorted by name. Left out of the JSON (and so of
    /// the digest) when there are none, so the digest of a folder without `commands/` is what it
    /// was before commands existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
    /// The agent's modes (root agents only), sorted by name (ADR 0039 decision 3). Left out of the
    /// JSON (and so of the digest) when there are none, so the digest of a folder without `modes/`
    /// is what it was before modes existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<Mode>,
}

/// How the agents of a package are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Layout {
    /// No agent was read: there is no agent directory (and the source was told that is
    /// fine), or both `agent/` and `agents/` exist (an error).
    Absent,
    /// `agent/`: one agent.
    Single,
    /// `agents/<name>/`: several.
    Multi,
}

/// Everything a source holds: one agent or several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Package {
    /// How the agents are laid out.
    pub layout: Layout,
    /// The agents that loaded (one for [`Layout::Single`]), sorted by name.
    pub agents: Vec<AgentManifest>,
}

/// Whether warnings fail a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// Only errors fail. The default: the Agent Skills client guide asks for leniency.
    Lenient,
    /// Warnings fail too (`build("agent").strict()`).
    Strict,
}

/// The result of a load: the package and every finding.
///
/// Items with errors are left out of the package, so a package next to errors is what *could*
/// be read, not something to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// What loaded.
    pub package: Package,
    /// Everything found, in discovery order.
    pub diagnostics: Vec<Diagnostic>,
}

impl Report {
    /// The errors.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.is_error())
    }

    /// The warnings.
    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| !d.is_error())
    }

    /// Whether there is no error.
    pub fn is_ok(&self) -> bool {
        self.errors().next().is_none()
    }

    /// The package, or [`Error::Invalid`] when the policy refuses the findings.
    pub fn into_package(self, strictness: Strictness) -> Result<Package, Error> {
        let refused = match strictness {
            Strictness::Lenient => !self.is_ok(),
            Strictness::Strict => !self.diagnostics.is_empty(),
        };
        if refused {
            Err(Error::Invalid {
                diagnostics: self.diagnostics,
            })
        } else {
            Ok(self.package)
        }
    }
}

/// A path with `/` separators on every platform, so a manifest and its digest do not depend on
/// where they were built.
fn portable_path<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&path.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn schedule(cron: &str, at: Option<&str>, after: Option<&str>) -> Schedule {
        Schedule {
            name: "daily".into(),
            cron: cron.into(),
            at: at.map(Into::into),
            after: after.map(Into::into),
            timezone: "UTC".into(),
            agent: "a".into(),
            prompt: "Go.".into(),
            path: PathBuf::from("agent/schedules/daily.md"),
        }
    }

    /// The JSON a `cron` schedule had before `at` and `after` existed: the digest depends on it.
    #[test]
    fn a_cron_schedule_serializes_as_before() {
        assert_eq!(
            serde_json::to_string(&schedule("0 9 * * *", None, None)).unwrap(),
            r#"{"name":"daily","cron":"0 9 * * *","timezone":"UTC","agent":"a","prompt":"Go.","path":"agent/schedules/daily.md"}"#
        );
        assert_eq!(
            serde_json::to_string(&schedule("", None, Some("30m"))).unwrap(),
            r#"{"name":"daily","after":"30m","timezone":"UTC","agent":"a","prompt":"Go.","path":"agent/schedules/daily.md"}"#
        );
    }

    #[test]
    fn when_needs_exactly_one_key() {
        assert!(matches!(
            schedule("0 9 * * *", None, None).when(),
            Ok(When::Cron(_))
        ));
        assert!(matches!(
            schedule("", Some("2026-12-24T18:00"), None).when(),
            Ok(When::At(_))
        ));
        assert!(matches!(
            schedule("", None, Some("30m")).when(),
            Ok(When::After(_))
        ));
        for bad in [
            schedule("", None, None),
            schedule("0 9 * * *", Some("2026-12-24T18:00"), None),
            schedule("0 9 * * *", None, Some("30m")),
            schedule("", Some("2026-12-24T18:00"), Some("30m")),
            schedule("0 9 * *", None, None),
        ] {
            assert!(bad.when().is_err(), "{bad:?}");
        }
    }
}
