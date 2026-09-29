//! Where manifests come from, and the directory implementation.
//!
//! [`ManifestSource`] is the seam between "where the files are" and "what they mean": its
//! signature holds only manifest types and diagnostics, so an embedded source (a generated
//! static manifest) and a remote one can sit next to [`Dir`] without touching a caller.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Error;
use crate::diagnostic::{Diagnostic, Sink};
use crate::frontmatter::clean_body;
use crate::load::{self, AgentFile, FileKind, RemoteSpec};
use crate::manifest::{
    AgentManifest, InstructionPart, Instructions, Layout, Package, RemoteAgent, Report, Schedule,
    Skill, SkillLayout, Subagent,
};

/// Something that can produce a [`Report`]: the agents it holds and every finding about them.
pub trait ManifestSource {
    /// Read and validate. `Err` is for a source that cannot be read at all; problems in the
    /// content are diagnostics in the [`Report`].
    fn load(&self) -> Result<Report, Error>;
}

/// A directory on disk.
///
/// `root` is the directory that holds `agent/` (one agent) or `agents/<name>/` (several); it is
/// usually the Cargo package root. The rules are those of `docs/authoring.md`:
///
/// * `agent/` and `agents/` together are an error; neither is an error unless
///   [`optional`](Self::optional).
/// * Dotfiles, `*.test.md`, `__tests__/` and `README.md` are ignored; a directory under
///   `skills/` without `SKILL.md` is not a skill.
/// * Names come from paths, and a subagent file may end in `.md` or `.agent.md`.
///
/// Symbolic links to files and directories are followed one step; directories reached through
/// a link are not searched for resources, so a link cannot make the walk loop.
#[derive(Debug, Clone)]
pub struct Dir {
    root: PathBuf,
    optional: bool,
    default_name: Option<String>,
}

impl Dir {
    /// A source over `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            optional: false,
            default_name: None,
        }
    }

    /// Accept a `root` with neither `agent/` nor `agents/`: the package is empty and there is no
    /// error.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    /// The name of the root agent when its frontmatter has none (a build script passes
    /// `CARGO_PKG_NAME`).
    #[must_use]
    pub fn default_name(mut self, name: impl Into<String>) -> Self {
        self.default_name = Some(name.into());
        self
    }

    /// The directory this source reads.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl ManifestSource for Dir {
    fn load(&self) -> Result<Report, Error> {
        let meta = fs::metadata(&self.root).map_err(|e| Error::io(&self.root, e))?;
        if !meta.is_dir() {
            return Err(Error::io(
                &self.root,
                std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory"),
            ));
        }
        let mut loader = Loader {
            root: &self.root,
            diagnostics: Vec::new(),
        };
        let package = loader.package(self)?;
        Ok(Report {
            package,
            diagnostics: loader.diagnostics,
        })
    }
}

/// What a directory entry is, after following a symbolic link once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File,
}

#[derive(Debug)]
struct Entry {
    name: String,
    path: PathBuf,
    kind: EntryKind,
    /// Reached through a symbolic link.
    linked: bool,
}

struct Loader<'a> {
    root: &'a Path,
    diagnostics: Vec<Diagnostic>,
}

impl Loader<'_> {
    fn rel(&self, abs: &Path) -> PathBuf {
        abs.strip_prefix(self.root).unwrap_or(abs).to_path_buf()
    }

    fn sink<'s>(&'s mut self, rel: &'s Path) -> Sink<'s> {
        Sink {
            out: &mut self.diagnostics,
            path: rel,
        }
    }

    fn error(&mut self, rel: &Path, line: Option<u32>, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::error(rel, line, message));
    }

    fn warn(&mut self, rel: &Path, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::warning(rel, None, message));
    }

    /// The entries of `dir`, sorted by name. A name that is not UTF-8 and a broken link are
    /// skipped with a warning.
    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>, Error> {
        let mut out = Vec::new();
        for item in fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
            let item = item.map_err(|e| Error::io(dir, e))?;
            let path = item.path();
            let Some(name) = item.file_name().to_str().map(str::to_owned) else {
                let rel = self.rel(&path);
                self.warn(&rel, "ignored: the name is not valid UTF-8");
                continue;
            };
            let file_type = item.file_type().map_err(|e| Error::io(&path, e))?;
            let linked = file_type.is_symlink();
            let meta = if linked {
                match fs::metadata(&path) {
                    Ok(m) => m,
                    Err(_) => {
                        let rel = self.rel(&path);
                        self.warn(&rel, "ignored: a broken symbolic link");
                        continue;
                    }
                }
            } else {
                item.metadata().map_err(|e| Error::io(&path, e))?
            };
            let kind = if meta.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            out.push(Entry {
                name,
                path,
                kind,
                linked,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// The text of a file, or `None` (with an error) when it is not UTF-8.
    fn read(&mut self, path: &Path) -> Result<Option<String>, Error> {
        let bytes = fs::read(path).map_err(|e| Error::io(path, e))?;
        match String::from_utf8(bytes) {
            Ok(t) => Ok(Some(t)),
            Err(_) => {
                let rel = self.rel(path);
                self.error(&rel, None, "the file is not valid UTF-8");
                Ok(None)
            }
        }
    }

    fn package(&mut self, source: &Dir) -> Result<Package, Error> {
        let agent = self.root.join("agent");
        let agents = self.root.join("agents");
        let has_agent = agent.is_dir();
        let has_agents = agents.is_dir();
        match (has_agent, has_agents) {
            (true, true) => {
                self.error(
                    Path::new("agents"),
                    None,
                    "both `agent/` and `agents/` exist: use `agent/` for one agent or \
                     `agents/<name>/` for several, not both",
                );
                Ok(Package {
                    layout: Layout::Absent,
                    agents: Vec::new(),
                })
            }
            (true, false) => {
                let manifest =
                    self.agent_dir(&agent, FileKind::Root, source.default_name.as_deref())?;
                Ok(Package {
                    layout: Layout::Single,
                    agents: manifest.into_iter().collect(),
                })
            }
            (false, true) => {
                let mut found = Vec::new();
                let mut children = 0_usize;
                for entry in self.list(&agents)? {
                    if entry.kind != EntryKind::Dir || ignored(&entry.name) {
                        continue;
                    }
                    children += 1;
                    if !entry.path.join("instructions.md").is_file() {
                        let rel = self.rel(&entry.path);
                        self.warn(&rel, "ignored: no `instructions.md`, so it is not an agent");
                        continue;
                    }
                    found.extend(self.agent_dir(
                        &entry.path,
                        FileKind::Named,
                        Some(&entry.name),
                    )?);
                }
                if children == 0 {
                    self.error(
                        Path::new("agents"),
                        None,
                        "`agents/` holds no agent directory",
                    );
                }
                found.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(Package {
                    layout: Layout::Multi,
                    agents: found,
                })
            }
            (false, false) => {
                if !source.optional {
                    self.error(
                        Path::new("."),
                        None,
                        format!(
                            "neither `agent/` nor `agents/` exists in {}",
                            self.root.display()
                        ),
                    );
                }
                Ok(Package {
                    layout: Layout::Absent,
                    agents: Vec::new(),
                })
            }
        }
    }

    /// One agent directory: `agent/`, `agents/<name>/` or `subagents/<name>/`.
    fn agent_dir(
        &mut self,
        dir: &Path,
        kind: FileKind,
        path_name: Option<&str>,
    ) -> Result<Option<AgentManifest>, Error> {
        let rel_dir = self.rel(dir);
        let entries = self.list(dir)?;
        let find = |name: &str, want: EntryKind| {
            entries
                .iter()
                .find(|e| e.name == name && e.kind == want)
                .map(|e| e.path.clone())
        };

        let Some(instructions_path) = find("instructions.md", EntryKind::File) else {
            self.error(
                &rel_dir,
                None,
                "no `instructions.md`: an agent directory needs one",
            );
            return Ok(None);
        };
        let rel_instructions = self.rel(&instructions_path);
        let Some(text) = self.read(&instructions_path)? else {
            return Ok(None);
        };
        let file = {
            let mut sink = self.sink(&rel_instructions);
            load::parse_agent_file(&mut sink, &text, kind, path_name)
        };
        let Some(AgentFile {
            name,
            frontmatter,
            body,
            remote,
        }) = file
        else {
            return Ok(None);
        };
        if remote.is_some() && kind == FileKind::Sub {
            self.error(
                &rel_instructions,
                None,
                "`a2a` makes a remote subagent, which is a single file: \
                 use `subagents/<name>.md`, not a directory",
            );
            return Ok(None);
        }

        let mut parts = Vec::new();
        if let Some(parts_dir) = find("instructions", EntryKind::Dir) {
            for entry in self.list(&parts_dir)? {
                if entry.kind != EntryKind::File
                    || ignored(&entry.name)
                    || !is_markdown(&entry.name)
                {
                    continue;
                }
                if let Some(part) = self.read(&entry.path)? {
                    let body = clean_body(part.strip_prefix('\u{feff}').unwrap_or(&part));
                    if !body.is_empty() {
                        parts.push(InstructionPart {
                            file: entry.name,
                            body,
                        });
                    }
                }
            }
        }
        let instructions = Instructions { body, parts };
        let prompt = instructions.prompt();
        if prompt.is_empty() {
            self.error(
                &rel_instructions,
                None,
                format!("agent `{name}` has no instructions: the body is its system prompt"),
            );
        }
        {
            let mut sink = self.sink(&rel_instructions);
            load::check_prompt_length(&mut sink, &prompt);
        }

        let skills = match find("skills", EntryKind::Dir) {
            Some(d) => self.skills(&d)?,
            None => Vec::new(),
        };
        let subagents = match find("subagents", EntryKind::Dir) {
            Some(d) => self.subagents(&d)?,
            None => Vec::new(),
        };
        let mcp = match find("mcp.json", EntryKind::File) {
            Some(p) => match self.read(&p)? {
                Some(t) => {
                    let rel = self.rel(&p);
                    load::parse_mcp(&rel, &t, &mut self.diagnostics)
                }
                None => None,
            },
            None => None,
        };
        let schedules = match (find("schedules", EntryKind::Dir), kind) {
            (Some(d), FileKind::Root | FileKind::Named) => self.schedules(&d, &name)?,
            (Some(d), FileKind::Sub) => {
                let rel = self.rel(&d);
                self.warn(&rel, "ignored: schedules belong to the root agent");
                Vec::new()
            }
            (None, _) => Vec::new(),
        };

        for entry in &entries {
            let known = matches!(
                (entry.name.as_str(), entry.kind),
                ("instructions.md" | "mcp.json", EntryKind::File)
                    | (
                        "instructions" | "skills" | "subagents" | "schedules",
                        EntryKind::Dir
                    )
            );
            if !known && !ignored(&entry.name) && !is_readme(&entry.name) {
                let rel = self.rel(&entry.path);
                self.warn(
                    &rel,
                    format!(
                        "`{}` is not part of an agent directory and is ignored (tools are Rust: \
                         see `#[tool]`)",
                        entry.name
                    ),
                );
            }
        }

        Ok(Some(AgentManifest {
            name,
            path: rel_instructions,
            frontmatter,
            instructions,
            skills,
            subagents,
            mcp,
            schedules,
        }))
    }

    fn skills(&mut self, dir: &Path) -> Result<Vec<Skill>, Error> {
        let mut skills: Vec<Skill> = Vec::new();
        for entry in self.list(dir)? {
            if ignored(&entry.name) || is_readme(&entry.name) {
                continue;
            }
            let (file, dir_name, layout) = match entry.kind {
                EntryKind::Dir => (
                    entry.path.join("SKILL.md"),
                    entry.name.clone(),
                    SkillLayout::Directory,
                ),
                EntryKind::File if is_markdown(&entry.name) => (
                    entry.path.clone(),
                    entry.name.trim_end_matches(".md").to_owned(),
                    SkillLayout::Flat,
                ),
                EntryKind::File => continue,
            };
            if !file.is_file() {
                continue; // a directory without SKILL.md is not a skill
            }
            let Some(text) = self.read(&file)? else {
                continue;
            };
            let rel = self.rel(&file);
            let Some(mut skill) =
                load::parse_skill(&rel, &text, &dir_name, layout, &mut self.diagnostics)
            else {
                continue;
            };
            if layout == SkillLayout::Directory {
                skill.resources = self.resources(&entry.path)?;
            }
            if let Some(first) = skills.iter().find(|s| s.name == skill.name) {
                let first_path = first.path.display().to_string();
                self.error(
                    &rel,
                    None,
                    format!("skill `{}` is already defined by {first_path}", skill.name),
                );
                continue;
            }
            skills.push(skill);
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(skills)
    }

    /// The files of a skill directory other than its `SKILL.md`, as `a/b` paths, sorted.
    fn resources(&mut self, skill_dir: &Path) -> Result<Vec<String>, Error> {
        fn walk(
            me: &mut Loader<'_>,
            dir: &Path,
            prefix: &str,
            depth: u32,
            out: &mut Vec<String>,
        ) -> Result<(), Error> {
            for entry in me.list(dir)? {
                if entry.name.starts_with('.') || (prefix.is_empty() && entry.name == "SKILL.md") {
                    continue;
                }
                let rel = format!("{prefix}{}", entry.name);
                match entry.kind {
                    EntryKind::File => out.push(rel),
                    // A directory reached through a link is not entered: no cycles.
                    EntryKind::Dir if !entry.linked && depth < 8 => {
                        walk(me, &entry.path, &format!("{rel}/"), depth + 1, out)?;
                    }
                    EntryKind::Dir => {}
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        walk(self, skill_dir, "", 0, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn subagents(&mut self, dir: &Path) -> Result<Vec<Subagent>, Error> {
        let mut found: Vec<Subagent> = Vec::new();
        let mut origin: Vec<PathBuf> = Vec::new();
        for entry in self.list(dir)? {
            if ignored(&entry.name) || is_readme(&entry.name) {
                continue;
            }
            let rel = self.rel(&entry.path);
            let sub = match entry.kind {
                EntryKind::File => {
                    let Some(stem) = subagent_stem(&entry.name) else {
                        continue;
                    };
                    self.subagent_file(&entry.path, stem)?
                }
                EntryKind::Dir => {
                    if !entry.path.join("instructions.md").is_file() {
                        self.error(
                            &rel,
                            None,
                            "a subagent directory needs an `instructions.md`",
                        );
                        continue;
                    }
                    self.agent_dir(&entry.path, FileKind::Sub, Some(&entry.name))?
                        .map(|m| Subagent::Local(Box::new(m)))
                }
            };
            let Some(sub) = sub else { continue };
            if let Some(i) = found.iter().position(|s| s.name() == sub.name()) {
                let first = origin[i].display().to_string();
                self.error(
                    &rel,
                    None,
                    format!("subagent `{}` is already defined by {first}", sub.name()),
                );
                continue;
            }
            origin.push(rel);
            found.push(sub);
        }
        found.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(found)
    }

    /// A flat subagent file: local (a prompt) or remote (`a2a:`).
    fn subagent_file(&mut self, path: &Path, stem: &str) -> Result<Option<Subagent>, Error> {
        let rel = self.rel(path);
        let Some(text) = self.read(path)? else {
            return Ok(None);
        };
        let file = {
            let mut sink = self.sink(&rel);
            load::parse_agent_file(&mut sink, &text, FileKind::Sub, Some(stem))
        };
        let Some(AgentFile {
            name,
            frontmatter,
            body,
            remote,
        }) = file
        else {
            return Ok(None);
        };
        if let Some(RemoteSpec { url, auth }) = remote {
            return Ok(Some(Subagent::Remote(RemoteAgent {
                name,
                description: frontmatter
                    .description
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                url,
                auth,
                note: body,
                path: rel,
            })));
        }
        if frontmatter.a2a.is_some() {
            return Ok(None); // an invalid `a2a`: already reported
        }
        {
            let mut sink = self.sink(&rel);
            load::check_prompt_length(&mut sink, &body);
        }
        Ok(Some(Subagent::Local(Box::new(AgentManifest {
            name,
            path: rel,
            frontmatter,
            instructions: Instructions {
                body,
                parts: Vec::new(),
            },
            skills: Vec::new(),
            subagents: Vec::new(),
            mcp: None,
            schedules: Vec::new(),
        }))))
    }

    fn schedules(&mut self, dir: &Path, owner: &str) -> Result<Vec<Schedule>, Error> {
        fn walk(
            me: &mut Loader<'_>,
            dir: &Path,
            prefix: &str,
            owner: &str,
            depth: u32,
            out: &mut Vec<Schedule>,
        ) -> Result<(), Error> {
            for entry in me.list(dir)? {
                if ignored(&entry.name) || is_readme(&entry.name) {
                    continue;
                }
                match entry.kind {
                    EntryKind::Dir if !entry.linked && depth < 8 => {
                        walk(
                            me,
                            &entry.path,
                            &format!("{prefix}{}/", entry.name),
                            owner,
                            depth + 1,
                            out,
                        )?;
                    }
                    EntryKind::File if is_markdown(&entry.name) => {
                        let name = format!("{prefix}{}", entry.name.trim_end_matches(".md"));
                        let Some(text) = me.read(&entry.path)? else {
                            continue;
                        };
                        let rel = me.rel(&entry.path);
                        let mut sink = me.sink(&rel);
                        out.extend(load::parse_schedule(&mut sink, &rel, &text, &name, owner));
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        let mut out = Vec::new();
        walk(self, dir, "", owner, 0, &mut out)?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// Dotfiles, `*.test.md` and `__tests__`.
fn ignored(name: &str) -> bool {
    name.starts_with('.') || name.ends_with(".test.md") || name == "__tests__"
}

fn is_readme(name: &str) -> bool {
    name.eq_ignore_ascii_case("README.md")
}

fn is_markdown(name: &str) -> bool {
    name.ends_with(".md")
}

/// `x.md` and `x.agent.md` are subagent `x`; anything else is not a subagent file.
fn subagent_stem(file: &str) -> Option<&str> {
    let stem = file
        .strip_suffix(".agent.md")
        .or_else(|| file.strip_suffix(".md"))?;
    (!stem.is_empty()).then_some(stem)
}
