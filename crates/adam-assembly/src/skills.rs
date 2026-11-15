//! Skills at run time: [Agent Skills](https://agentskills.io/specification) progressive
//! disclosure in three tiers.
//!
//! 1. **The catalog** (tier 1): the name and description of every skill the agent may use, appended
//!    to its prompt ([`SkillSet::prompt_section`]).
//! 2. **`load_skill`** (tier 2): a tool that returns the body of `SKILL.md`, wrapped in
//!    `<skill_content>` with the list of the skill's bundled files.
//! 3. **`read_skill_file`** (tier 3): a tool that returns one bundled file of a skill.
//!
//! A skill named in `preload_skills` skips the catalog and `load_skill`: its `<skill_content>` is
//! part of the prompt.
//!
//! Everything here is a pure function of the manifest and the resource bytes, so a run sees the
//! same text whether the files were embedded by `build.rs` or read from a directory, and both
//! tools are ordinary [`Tool`]s: their results are journaled with the step that ran them.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use adam_agent_fs::{
    AgentManifest, EmbeddedAgent, EmbeddedSubagent, ManifestSource, SKILL_RESOURCE_LIMIT, Skill,
    SkillSelection, Subagent,
};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::paging::{self, PAGE_LINES, PageError, page_lines, window_args};
use adam_llm_agent::{DynTool, StepStyle, Tool, ToolCtx, ToolError, ToolOutput};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{Error, Origin, SkillField, hint, list};
use crate::suggest::closest;

/// The name of the tool that loads a skill's instructions (tier 2).
pub const LOAD_SKILL: &str = "load_skill";
/// The name of the tool that reads a file bundled with a skill (tier 3).
pub const READ_SKILL_FILE: &str = "read_skill_file";

/// The bytes of the files bundled with skills (`references/`, `scripts/`, `assets/`), by the
/// skill's file (`agent/skills/release-notes/SKILL.md`) and the file's path within the skill.
///
/// An [`AgentManifest`] lists resources without their bytes. They come with an embedded agent
/// (borrowed from the binary, so nothing is copied) or from
/// [`AgentDef::resources_from`](crate::AgentDef::resources_from) for a directory, which reads
/// them once, at startup.
#[derive(Clone, Default)]
pub struct SkillFiles {
    files: BTreeMap<(String, String), Cow<'static, [u8]>>,
}

impl std::fmt::Debug for SkillFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillFiles")
            .field("files", &self.files.len())
            .field(
                "bytes",
                &self.files.values().map(|b| b.len()).sum::<usize>(),
            )
            .finish()
    }
}

/// A skill's file as a map key: `/` separators on every platform.
fn skill_key(skill: &Skill) -> String {
    skill.path.to_string_lossy().replace('\\', "/")
}

impl SkillFiles {
    /// The resources of an embedded agent and of its subagents, borrowed from the binary.
    pub(crate) fn from_embedded(agent: &EmbeddedAgent) -> Self {
        fn walk(agent: &EmbeddedAgent, out: &mut SkillFiles) {
            for skill in agent.skills {
                for file in skill.files {
                    out.files.insert(
                        (skill.path.to_owned(), file.path.to_owned()),
                        Cow::Borrowed(file.bytes),
                    );
                }
            }
            for sub in agent.subagents {
                if let EmbeddedSubagent::Local(local) = sub {
                    walk(local, out);
                }
            }
        }
        let mut files = Self::default();
        walk(agent, &mut files);
        files
    }

    /// Read the resources of every skill of `manifest` and of its local subagents from `source`,
    /// and refuse a skill over [`SKILL_RESOURCE_LIMIT`].
    pub(crate) fn read(
        manifest: &AgentManifest,
        source: &impl ManifestSource,
    ) -> Result<Self, Error> {
        fn walk(
            agent: &AgentManifest,
            name: &str,
            source: &impl ManifestSource,
            out: &mut SkillFiles,
        ) -> Result<(), Error> {
            for skill in &agent.skills {
                let mut total = 0_u64;
                for resource in &skill.resources {
                    let bytes = source.read_resource(skill, resource)?;
                    total += bytes.len() as u64;
                    if total > SKILL_RESOURCE_LIMIT {
                        return Err(Error::SkillTooLarge {
                            origin: Origin::new(name, skill.path.clone()),
                            skill: skill.name.clone(),
                            bytes: total,
                            limit: SKILL_RESOURCE_LIMIT,
                        });
                    }
                    out.files
                        .insert((skill_key(skill), resource.clone()), bytes);
                }
            }
            for sub in &agent.subagents {
                if let Subagent::Local(local) = sub {
                    walk(local, &format!("{name}/{}", local.name), source, out)?;
                }
            }
            Ok(())
        }
        let mut files = Self::default();
        walk(manifest, &manifest.name, source, &mut files)?;
        Ok(files)
    }

    fn get(&self, skill: &Skill, resource: &str) -> Option<&Cow<'static, [u8]>> {
        self.files.get(&(skill_key(skill), resource.to_owned()))
    }
}

/// One selected skill, ready to serve.
#[derive(Debug)]
struct Entry {
    name: String,
    description: String,
    body: String,
    /// Whether the body is in the prompt (`preload_skills`).
    preloaded: bool,
    /// The bundled files, by path, sorted.
    files: Vec<(String, Cow<'static, [u8]>)>,
}

impl Entry {
    fn file(&self, path: &str) -> Option<&Cow<'static, [u8]>> {
        self.files.iter().find(|(p, _)| p == path).map(|(_, b)| b)
    }

    fn file_paths(&self) -> Vec<String> {
        self.files.iter().map(|(p, _)| p.clone()).collect()
    }
}

/// The skills one agent may use, in the order its `skills:` lists them (the manifest's order,
/// sorted by name, for `all`).
#[derive(Debug)]
pub(crate) struct SkillSet {
    entries: Vec<Entry>,
}

/// Which skills an agent gets, from its own `skills/` and its `skills:` and `preload_skills:`.
///
/// `None` when the agent has no skill to offer: it then gets no catalog and no tool.
///
/// # Errors
///
/// A name in `skills:` or `preload_skills:` that is not a skill of the agent, a preloaded skill
/// that `skills:` does not select, and a selected skill whose files were not supplied.
pub(crate) fn resolve(
    origin: &Origin,
    manifest: &AgentManifest,
    files: &SkillFiles,
) -> Result<Option<SkillSet>, Error> {
    let own: Vec<&str> = manifest.skills.iter().map(|s| s.name.as_str()).collect();
    let find = |field: SkillField, name: &str| -> Result<&Skill, Error> {
        manifest
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| Error::UnknownSkill {
                origin: origin.clone(),
                field,
                skill: name.to_owned(),
                suggestion: closest(name, own.iter().copied()).map(str::to_owned),
                available: own.iter().map(|s| (*s).to_owned()).collect(),
            })
    };

    let mut selected: Vec<&Skill> = Vec::new();
    match manifest.frontmatter.skills.as_ref() {
        None | Some(SkillSelection::All) => selected.extend(&manifest.skills),
        Some(SkillSelection::Named(names)) => {
            for name in names {
                let skill = find(SkillField::Skills, name)?;
                if !selected.iter().any(|s| s.name == skill.name) {
                    selected.push(skill);
                }
            }
        }
    }

    let mut preloaded: Vec<&str> = Vec::new();
    for name in manifest.frontmatter.preload_skills.iter().flatten() {
        let skill = find(SkillField::PreloadSkills, name)?;
        if !selected.iter().any(|s| s.name == skill.name) {
            return Err(Error::PreloadNotSelected {
                origin: origin.clone(),
                skill: name.clone(),
                selected: selected.iter().map(|s| s.name.clone()).collect(),
            });
        }
        preloaded.push(&skill.name);
    }

    let mut entries = Vec::with_capacity(selected.len());
    for skill in selected {
        let mut bundled = Vec::with_capacity(skill.resources.len());
        for resource in &skill.resources {
            let bytes = files
                .get(skill, resource)
                .ok_or_else(|| Error::SkillFilesUnavailable {
                    origin: origin.clone(),
                    skill: skill.name.clone(),
                    file: resource.clone(),
                })?;
            bundled.push((resource.clone(), bytes.clone()));
        }
        let total: u64 = bundled.iter().map(|(_, b)| b.len() as u64).sum();
        if total > SKILL_RESOURCE_LIMIT {
            return Err(Error::SkillTooLarge {
                origin: origin.clone(),
                skill: skill.name.clone(),
                bytes: total,
                limit: SKILL_RESOURCE_LIMIT,
            });
        }
        entries.push(Entry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            body: skill.body.clone(),
            preloaded: preloaded.contains(&skill.name.as_str()),
            files: bundled,
        });
    }
    Ok((!entries.is_empty()).then_some(SkillSet { entries }))
}

/// `&`, `<`, `>` (and `"` for an attribute) as entities, so a description cannot close a tag.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

impl SkillSet {
    /// The names of the selected skills, in order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.name.clone()).collect()
    }

    /// The names of the preloaded skills, in order.
    pub(crate) fn preloaded(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.preloaded)
            .map(|e| e.name.clone())
            .collect()
    }

    fn loadable(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| !e.preloaded)
    }

    fn with_files(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| !e.files.is_empty())
    }

    fn has_load_tool(&self) -> bool {
        self.loadable().next().is_some()
    }

    fn has_read_tool(&self) -> bool {
        self.with_files().next().is_some()
    }

    /// What is appended to the agent's prompt, after a blank line: the catalog of the skills to
    /// load, then the preloaded skills. The format is part of the crate's contract (see the
    /// README) and is asserted against a golden file.
    ///
    /// ```text
    /// The following skills provide specialized instructions for specific tasks.
    /// When a task matches a skill's description, call the load_skill tool with the skill's name to load its full instructions.
    /// <available_skills>
    ///   <skill>
    ///     <name>release-notes</name>
    ///     <description>Drafts release notes ...</description>
    ///   </skill>
    /// </available_skills>
    /// ```
    pub(crate) fn prompt_section(&self) -> String {
        let mut sections: Vec<String> = Vec::new();
        if self.has_load_tool() {
            let mut s = String::from(
                "The following skills provide specialized instructions for specific tasks.\n\
                 When a task matches a skill's description, call the load_skill tool with the \
                 skill's name to load its full instructions.",
            );
            if self.has_read_tool() {
                s.push_str(&format!(
                    "\nFiles a skill bundles are listed under <skill_resources> when it is loaded; \
                     read one with the {READ_SKILL_FILE} tool."
                ));
            }
            s.push_str("\n<available_skills>");
            for entry in self.loadable() {
                // A description may span lines (a YAML block scalar): the catalog keeps one line.
                let description = entry
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let _ = write!(
                    s,
                    "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n  </skill>",
                    escape(&entry.name),
                    escape(&description),
                );
            }
            s.push_str("\n</available_skills>");
            sections.push(s);
        }
        let preloaded: Vec<&Entry> = self.entries.iter().filter(|e| e.preloaded).collect();
        if !preloaded.is_empty() {
            let mut s = String::from(
                "The following skills are already loaded. Follow them whenever a task matches \
                 their description.",
            );
            if self.has_read_tool() {
                s.push_str(&format!(
                    " Files a skill bundles are listed under <skill_resources>; read one with the \
                     {READ_SKILL_FILE} tool."
                ));
            }
            for entry in preloaded {
                s.push_str("\n\n");
                s.push_str(&content(entry));
            }
            sections.push(s);
        }
        sections.join("\n\n")
    }

    /// The tools this set adds: `load_skill` when a skill is left to load, and `read_skill_file`
    /// when a selected skill bundles a file.
    pub(crate) fn tools(self: &Arc<Self>) -> Vec<DynTool> {
        let mut tools: Vec<DynTool> = Vec::new();
        if self.has_load_tool() {
            tools.push(Arc::new(LoadSkill {
                set: Arc::clone(self),
            }));
        }
        if self.has_read_tool() {
            tools.push(Arc::new(ReadSkillFile {
                set: Arc::clone(self),
            }));
        }
        tools
    }

    /// The tier 2 result: a page of the body of the skill and the list of its files.
    fn load(&self, name: &str, offset: i64, limit: u32) -> Result<String, SkillError> {
        match self.entries.iter().find(|e| e.name == name) {
            Some(entry) if entry.preloaded => Err(SkillError::AlreadyLoaded {
                skill: name.to_owned(),
            }),
            Some(entry) => {
                let page = paged(name, &format!("{}\n", content(entry)), offset, limit)?;
                Ok(with_files_note(page, entry, offset))
            }
            None => Err(SkillError::UnknownSkill {
                skill: name.to_owned(),
                suggestion: closest(name, self.loadable().map(|e| e.name.as_str()))
                    .map(str::to_owned),
                available: self.loadable().map(|e| e.name.clone()).collect(),
            }),
        }
    }

    /// The tier 3 result: a page of the text of one bundled file.
    fn read(&self, skill: &str, path: &str, offset: i64, limit: u32) -> Result<String, SkillError> {
        let Some(entry) = self.entries.iter().find(|e| e.name == skill) else {
            return Err(SkillError::UnknownSkill {
                skill: skill.to_owned(),
                suggestion: closest(skill, self.with_files().map(|e| e.name.as_str()))
                    .map(str::to_owned),
                available: self.with_files().map(|e| e.name.clone()).collect(),
            });
        };
        let path = check_path(path)?;
        let Some(bytes) = entry.file(path) else {
            return Err(SkillError::NoSuchFile {
                skill: skill.to_owned(),
                path: path.to_owned(),
                available: entry.file_paths(),
            });
        };
        match std::str::from_utf8(bytes) {
            Ok(text) if !text.contains('\0') => {
                let page = paged(path, text, offset, limit)?;
                Ok(if text.is_empty() {
                    format!("(the file is empty)\n{page}")
                } else {
                    page
                })
            }
            _ => Err(SkillError::NotText {
                skill: skill.to_owned(),
                path: path.to_owned(),
                bytes: bytes.len(),
            }),
        }
    }
}

/// Most bundled files a first page lists when the body goes on.
const MAX_LISTED_FILES: usize = 200;

/// `load_skill`'s description and schema: the same whatever skills exist, so the tool list of a
/// run never depends on them.
fn load_spec() -> ToolSpec {
    ToolSpec {
        name: LOAD_SKILL.into(),
        description: "Load the instructions of one of your skills, a page of lines at a time: \
                      the last line of the result says where the next page starts. Call it when \
                      a task matches the description of a skill in the available_skills list. \
                      The same name, offset and limit give the same page."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The name of the skill to load."
                },
                "offset": {
                    "type": "integer",
                    "description": "First line, from 1; -50 = the last 50 lines"
                },
                "limit": {
                    "type": "integer",
                    "description": "Most lines (at most 2000)"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}

/// `read_skill_file`'s description and schema: fixed, like [`load_spec`].
fn read_spec() -> ToolSpec {
    ToolSpec {
        name: READ_SKILL_FILE.into(),
        description: "Read a text file bundled with one of your skills, a page of lines at a \
                      time: a path listed under skill_resources when the skill was loaded. The \
                      last line of the result says where the next page starts."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The skill the file belongs to."
                },
                "path": {
                    "type": "string",
                    "description": "The file's path relative to the skill's directory, \
                                    for example references/style.md."
                },
                "offset": {
                    "type": "integer",
                    "description": "First line, from 1; -50 = the last 50 lines"
                },
                "limit": {
                    "type": "integer",
                    "description": "Most lines (at most 2000)"
                }
            },
            "required": ["skill", "path"],
            "additionalProperties": false
        }),
    }
}

/// One page of `text`: its lines, then the line that says where the next page starts.
fn paged(what: &str, text: &str, offset: i64, limit: u32) -> Result<String, SkillError> {
    match page_lines(std::io::Cursor::new(text.as_bytes()), offset, limit, false) {
        Ok(page) => Ok(page.render()),
        Err(PageError::PastEnd { offset, total }) => Err(SkillError::Window(format!(
            "`{what}` has {total} lines: offset {offset} is past the end"
        ))),
        Err(other) => Err(SkillError::Window(format!("`{what}`: {other}"))),
    }
}

/// The first page of a body that goes on names the files the skill bundles (the list inside
/// `<skill_content>` comes with the last page): one line before the paging line.
fn with_files_note(page: String, entry: &Entry, offset: i64) -> String {
    if offset != 1 || entry.files.is_empty() {
        return page;
    }
    let Some((body, last)) = page.rsplit_once('\n') else {
        return page;
    };
    if !last.starts_with("[more:") {
        return page;
    }
    let mut note = String::from("Files this skill bundles (read one with read_skill_file):");
    for (path, _) in entry.files.iter().take(MAX_LISTED_FILES) {
        let _ = write!(note, " {}", paging::escape_name(path.as_bytes()));
    }
    if entry.files.len() > MAX_LISTED_FILES {
        let _ = write!(
            note,
            " ... and {} more",
            entry.files.len() - MAX_LISTED_FILES
        );
    }
    format!("{body}\n{note}\n{last}")
}

/// `<skill_content name="x">`, the body, and the list of the skill's files.
fn content(entry: &Entry) -> String {
    let mut s = format!(
        "<skill_content name=\"{}\">\n{}",
        escape(&entry.name),
        entry.body
    );
    if !entry.files.is_empty() {
        let _ = write!(
            s,
            "\n\nRelative paths in this skill are relative to the skill's directory.\n\
             <skill_resources>"
        );
        for (path, _) in &entry.files {
            let _ = write!(s, "\n  <file>{}</file>", escape(path));
        }
        s.push_str("\n</skill_resources>");
    }
    s.push_str("\n</skill_content>");
    s
}

/// The path the model gave, checked and normalised (a leading `./` is dropped), or why it is
/// refused. Nothing here touches a file system: a path that passes is only looked up in the
/// skill's list of bundled files, so this is a second lock on a door that has no key.
fn check_path(path: &str) -> Result<&str, SkillError> {
    let mut plain = path;
    while let Some(rest) = plain.strip_prefix("./") {
        plain = rest;
    }
    if plain.is_empty() {
        return Err(SkillError::EmptyPath);
    }
    let windows_drive = plain.as_bytes().get(1) == Some(&b':')
        && plain
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    if plain.starts_with('/') || plain.starts_with('\\') || windows_drive {
        return Err(SkillError::AbsolutePath {
            path: path.to_owned(),
        });
    }
    if plain.split(['/', '\\']).any(|part| part == "..") {
        return Err(SkillError::PathTraversal {
            path: path.to_owned(),
        });
    }
    if plain.contains('\\') || plain.chars().any(char::is_control) {
        return Err(SkillError::InvalidPath {
            path: path.to_owned(),
        });
    }
    Ok(plain)
}

/// Why `load_skill` or `read_skill_file` refused a call. The message is the tool result the
/// model reads (with `is_error` set), so each says what to do instead. A refusal is not a failure
/// of the run: the model corrects the call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    /// The tool's arguments are not what its schema says.
    #[error("`{tool}` needs {expected}")]
    BadArguments {
        /// The tool that was called.
        tool: &'static str,
        /// The arguments it needs, as text.
        expected: &'static str,
    },
    /// The skill is not one of the skills this agent may use: unknown, or not selected by its
    /// `skills:`.
    #[error("no skill `{skill}` is available to you{}; available: {}", hint(.suggestion.as_deref()), list(.available))]
    UnknownSkill {
        /// The name as given.
        skill: String,
        /// The closest available name, when one is close.
        suggestion: Option<String>,
        /// The skills the tool accepts.
        available: Vec<String>,
    },
    /// The skill is in `preload_skills`: its instructions are already in the prompt.
    #[error("skill `{skill}` is already in your instructions; there is nothing to load")]
    AlreadyLoaded {
        /// The skill.
        skill: String,
    },
    /// The path is empty.
    #[error(
        "`path` is empty; give a path relative to the skill's directory, for example `references/style.md`"
    )]
    EmptyPath,
    /// The path is absolute.
    #[error("`{path}` is an absolute path; give a path relative to the skill's directory")]
    AbsolutePath {
        /// The path as given.
        path: String,
    },
    /// The path has a `..` component.
    #[error("`{path}` leaves the skill's directory: `..` is not allowed")]
    PathTraversal {
        /// The path as given.
        path: String,
    },
    /// The path has a backslash or a control character.
    #[error("`{path}` is not a plain path: use `/` separators and no control characters")]
    InvalidPath {
        /// The path as given.
        path: String,
    },
    /// The path is not one of the skill's bundled files.
    #[error(
        "skill `{skill}` has no file `{path}`{}",
        no_such_file(.path, .available)
    )]
    NoSuchFile {
        /// The skill.
        skill: String,
        /// The path, normalised.
        path: String,
        /// The skill's bundled files, sorted.
        available: Vec<String>,
    },
    /// The `offset` or `limit` is not usable, or a page cannot be made of the text.
    #[error("{0}")]
    Window(String),
    /// The file is not text (it is not UTF-8, or it has a NUL byte).
    #[error(
        "`{path}` of skill `{skill}` is a binary file ({bytes} bytes) and cannot be shown as text"
    )]
    NotText {
        /// The skill.
        skill: String,
        /// The path, normalised.
        path: String,
        /// Its size.
        bytes: usize,
    },
}

fn no_such_file(path: &str, available: &[String]) -> String {
    let mut s = if available.is_empty() {
        "; the skill bundles no files".to_owned()
    } else {
        format!("; its files: {}", list(available))
    };
    if path == "SKILL.md" {
        s.push_str(&format!("; its instructions are loaded with {LOAD_SKILL}"));
    }
    s
}

impl Classify for SkillError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// A refusal is a tool result with `is_error`, not a failed step.
fn answer(result: Result<String, SkillError>) -> Result<ToolOutput, ToolError> {
    Ok(match result {
        Ok(text) => ToolOutput::text(text),
        Err(refusal) => ToolOutput::error(refusal.to_string()),
    })
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// The window a call asked for, checked: `offset` from 1 (negative from the end) and `limit` at
/// most one page.
fn window_of(args: &Value) -> Result<(i64, u32), SkillError> {
    let number = |key: &str| args.get(key).and_then(Value::as_i64);
    window_args(number("offset"), number("limit"), PAGE_LINES).map_err(SkillError::Window)
}

struct LoadSkill {
    set: Arc<SkillSet>,
}

#[async_trait]
impl Tool for LoadSkill {
    /// The step of a call is called "Load a skill".
    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("Load a skill")
    }

    fn spec(&self) -> ToolSpec {
        load_spec()
    }

    /// A page of a long skill is read with the same call and a later `offset`.
    fn paged(&self) -> bool {
        true
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        answer(match string_arg(&args, "name") {
            Some(name) => {
                window_of(&args).and_then(|(offset, limit)| self.set.load(name, offset, limit))
            }
            None => Err(SkillError::BadArguments {
                tool: LOAD_SKILL,
                expected: "a string argument `name`",
            }),
        })
    }
}

struct ReadSkillFile {
    set: Arc<SkillSet>,
}

#[async_trait]
impl Tool for ReadSkillFile {
    /// The step of a call is called "Read a skill file".
    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("Read a skill file")
    }

    fn spec(&self) -> ToolSpec {
        read_spec()
    }

    /// A page of a long file is read with the same call and a later `offset`.
    fn paged(&self) -> bool {
        true
    }

    async fn call(&self, _ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        answer(
            match (string_arg(&args, "skill"), string_arg(&args, "path")) {
                (Some(skill), Some(path)) => window_of(&args)
                    .and_then(|(offset, limit)| self.set.read(skill, path, offset, limit)),
                _ => Err(SkillError::BadArguments {
                    tool: READ_SKILL_FILE,
                    expected: "string arguments `skill` and `path`",
                }),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, preloaded: bool, files: &[(&str, &[u8])]) -> Entry {
        Entry {
            name: name.into(),
            description: format!("Does {name} things."),
            body: format!("Steps of {name}."),
            preloaded,
            files: files
                .iter()
                .map(|(p, b)| ((*p).to_owned(), Cow::Owned(b.to_vec())))
                .collect(),
        }
    }

    fn set(entries: Vec<Entry>) -> Arc<SkillSet> {
        Arc::new(SkillSet { entries })
    }

    #[test]
    fn a_path_is_checked_before_it_is_looked_up() {
        assert_eq!(check_path("references/style.md"), Ok("references/style.md"));
        assert_eq!(check_path("./a/b"), Ok("a/b"));
        assert_eq!(check_path("././a"), Ok("a"));
        assert_eq!(check_path(""), Err(SkillError::EmptyPath));
        assert_eq!(check_path("./"), Err(SkillError::EmptyPath));
        for absolute in ["/etc/passwd", "\\windows", "C:\\x", "c:/x"] {
            assert!(
                matches!(check_path(absolute), Err(SkillError::AbsolutePath { .. })),
                "{absolute}"
            );
        }
        for up in ["../x", "a/../../b", "a/..", "..", "a\\..\\b", "./../x"] {
            assert!(
                matches!(check_path(up), Err(SkillError::PathTraversal { .. })),
                "{up}"
            );
        }
        for odd in ["a\\b", "a\0b", "a\nb"] {
            assert!(
                matches!(check_path(odd), Err(SkillError::InvalidPath { .. })),
                "{odd:?}"
            );
        }
        // `..` inside a name is only a name.
        assert_eq!(check_path("a..b/c"), Ok("a..b/c"));
        assert_eq!(check_path("...x"), Ok("...x"));
    }

    #[test]
    fn xml_special_characters_cannot_close_a_tag() {
        assert_eq!(
            escape("a < b & \"c\" > d"),
            "a &lt; b &amp; &quot;c&quot; &gt; d"
        );
        let mut e = entry("s", false, &[]);
        e.description = "Use </description>\n  when\ta > b.".into();
        let text = set(vec![e]).prompt_section();
        assert!(
            text.contains("<description>Use &lt;/description&gt; when a &gt; b.</description>"),
            "{text}"
        );
    }

    #[test]
    fn the_tools_follow_what_there_is_to_load_and_read() {
        let none: Vec<DynTool> = set(vec![entry("a", false, &[])]).tools();
        assert_eq!(
            none.iter().map(|t| t.spec().name).collect::<Vec<_>>(),
            [LOAD_SKILL]
        );
        let both = set(vec![entry("a", false, &[("f.md", b"x")])]).tools();
        assert_eq!(
            both.iter().map(|t| t.spec().name).collect::<Vec<_>>(),
            [LOAD_SKILL, READ_SKILL_FILE]
        );
        // Everything preloaded and no files: nothing to call.
        assert!(set(vec![entry("a", true, &[])]).tools().is_empty());
        // Everything preloaded, with a file: only the reader.
        let only_read = set(vec![entry("a", true, &[("f.md", b"x")])]).tools();
        assert_eq!(
            only_read.iter().map(|t| t.spec().name).collect::<Vec<_>>(),
            [READ_SKILL_FILE]
        );
    }

    #[test]
    fn the_specs_do_not_depend_on_the_skills() {
        let few = set(vec![entry("a", false, &[("f.md", b"x")])]);
        let many = set(vec![
            entry("a", false, &[]),
            entry("b", true, &[("f.md", b"x")]),
            entry("c", false, &[("g.md", b"y")]),
        ]);
        let spec = |s: &Arc<SkillSet>, name: &str| {
            s.tools()
                .into_iter()
                .find(|t| t.spec().name == name)
                .map(|t| serde_json::to_string(&t.spec()).unwrap())
                .unwrap()
        };
        for tool in [LOAD_SKILL, READ_SKILL_FILE] {
            assert_eq!(spec(&few, tool), spec(&many, tool), "{tool}");
        }
        let load = load_spec();
        assert!(load.parameters["properties"]["name"]["enum"].is_null());
        assert!(load.parameters["properties"]["offset"].is_object());
        assert!(load.parameters["properties"]["limit"].is_object());
        assert!(read_spec().parameters["properties"]["skill"]["enum"].is_null());
        assert_eq!(many.names(), ["a", "b", "c"]);
        assert_eq!(many.preloaded(), ["b"]);
    }

    #[test]
    fn a_long_body_is_cut_at_a_page_and_says_where_the_next_starts() {
        let mut long = entry("long", false, &[("f.md", b"x")]);
        long.body = (1..=5)
            .map(|n| format!("row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let s = set(vec![long]);
        let first = s.load("long", 1, 3).unwrap();
        assert_eq!(
            first,
            "<skill_content name=\"long\">\nrow 1\nrow 2\n\
             Files this skill bundles (read one with read_skill_file): f.md\n\
             [more: 9 lines left; next page: offset=4]"
        );
        let last = s.load("long", 9, 100).unwrap();
        assert!(
            last.ends_with("</skill_content>\n[end: 12 lines]"),
            "{last}"
        );
        assert!(!last.contains("Files this skill bundles"), "{last}");
        let from_end = s.load("long", -2, 100).unwrap();
        assert!(from_end.ends_with("</skill_resources>\n</skill_content>\n[end: 12 lines]"));
        assert!(matches!(
            s.load("long", 13, 3),
            Err(SkillError::Window(w)) if w.contains("past the end")
        ));
    }

    #[test]
    fn load_returns_the_body_and_refuses_the_rest() {
        let s = set(vec![
            entry("alpha", false, &[("references/x.md", b"x")]),
            entry("beta", true, &[]),
        ]);
        let text = s.load("alpha", 1, 2000).unwrap();
        assert_eq!(
            text,
            "<skill_content name=\"alpha\">\nSteps of alpha.\n\n\
             Relative paths in this skill are relative to the skill's directory.\n\
             <skill_resources>\n  <file>references/x.md</file>\n</skill_resources>\n</skill_content>\n[end: 8 lines]"
        );
        assert_eq!(
            s.load("beta", 1, 2000),
            Err(SkillError::AlreadyLoaded {
                skill: "beta".into()
            })
        );
        let unknown = s.load("alph", 1, 2000).unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "no skill `alph` is available to you; did you mean `alpha`?; available: `alpha`"
        );
    }

    #[test]
    fn read_returns_text_and_says_why_not() {
        let s = set(vec![
            entry(
                "a",
                false,
                &[
                    ("bin", &[0xff, 0xfe, 0x00]),
                    ("empty", b""),
                    ("nul", b"a\0b"),
                    ("t.md", "héllo".as_bytes()),
                ],
            ),
            entry("plain", false, &[]),
        ]);
        assert_eq!(
            s.read("a", "t.md", 1, 2000).unwrap(),
            "héllo\n[end: 1 lines; no newline at the end of the file]"
        );
        assert_eq!(
            s.read("a", "./t.md", 1, 2000).unwrap(),
            "héllo\n[end: 1 lines; no newline at the end of the file]"
        );
        assert_eq!(
            s.read("a", "empty", 1, 2000).unwrap(),
            "(the file is empty)\n[end: 0 lines]"
        );
        for binary in ["bin", "nul"] {
            assert!(
                matches!(
                    s.read("a", binary, 1, 2000),
                    Err(SkillError::NotText { .. })
                ),
                "{binary}"
            );
        }
        let missing = s.read("a", "T.md", 1, 2000).unwrap_err();
        assert_eq!(
            missing.to_string(),
            "skill `a` has no file `T.md`; its files: `bin`, `empty`, `nul`, `t.md`"
        );
        assert!(
            s.read("plain", "x", 1, 2000)
                .unwrap_err()
                .to_string()
                .ends_with("the skill bundles no files")
        );
        assert!(
            s.read("plain", "SKILL.md", 1, 2000)
                .unwrap_err()
                .to_string()
                .ends_with(
                    "the skill bundles no files; its instructions are loaded with load_skill"
                )
        );
        let unknown = s.read("b", "x", 1, 2000).unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "no skill `b` is available to you; available: `a`"
        );
        assert_eq!(unknown.class(), ErrorClass::Invalid);
    }

    #[test]
    fn every_refusal_says_what_to_do() {
        let cases = [
            SkillError::BadArguments {
                tool: LOAD_SKILL,
                expected: "a string argument `name`",
            },
            SkillError::EmptyPath,
            SkillError::Window("offset counts from 1".into()),
            SkillError::AbsolutePath { path: "/x".into() },
            SkillError::PathTraversal {
                path: "../x".into(),
            },
            SkillError::InvalidPath {
                path: "a\\b".into(),
            },
            SkillError::NotText {
                skill: "s".into(),
                path: "p".into(),
                bytes: 3,
            },
        ];
        for case in cases {
            assert!(!case.to_string().is_empty());
        }
        assert_eq!(
            SkillError::PathTraversal {
                path: "a/../../b".into()
            }
            .to_string(),
            "`a/../../b` leaves the skill's directory: `..` is not allowed"
        );
    }
}
