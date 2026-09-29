//! [`AgentDef`]: a manifest plus the values the code supplies, and [`AgentDef::bind`], which
//! checks it against the registered tools and renders the prompts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use adam_agent_fs::{
    AgentManifest, EmbeddedAgent, InstructionPart, Instructions, Limits as FileLimits,
    ManifestSource, ModelRef, Strictness, Subagent, ToolList,
};
use adam_llm_agent::{DynTool, Limits, ToolSet};

use crate::assembly::BoundDef;
use crate::error::{Error, Origin, ToolClash};
use crate::skills::{self, SkillFiles};
use crate::subagent::SubagentTool;
use crate::suggest::closest;
use crate::template::{self, Piece};

/// Something an [`AgentDef`] can be made from: the owned manifest that a directory produces, or
/// the `'static` one that `build.rs` embeds. Both end up as the same [`AgentManifest`], so an
/// agent binds the same way wherever its files came from.
pub trait IntoManifest {
    /// The owned manifest.
    ///
    /// # Errors
    ///
    /// An embedded manifest whose frontmatter cannot be decoded: the generated code and
    /// `adam-agent-fs` are not the same version.
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error>;

    /// The bytes of the files the skills bundle. The default is none, which is right for a
    /// manifest whose skills bundle nothing; a directory's manifest gets them from
    /// [`AgentDef::resources_from`] and an embedded agent brings its own.
    fn skill_files(&self) -> SkillFiles {
        SkillFiles::default()
    }
}

impl IntoManifest for AgentManifest {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        Ok(self)
    }
}

impl IntoManifest for &AgentManifest {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        Ok(self.clone())
    }
}

impl IntoManifest for EmbeddedAgent {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        self.to_manifest()
    }

    fn skill_files(&self) -> SkillFiles {
        SkillFiles::from_embedded(self)
    }
}

impl IntoManifest for &EmbeddedAgent {
    fn into_manifest(self) -> Result<AgentManifest, adam_agent_fs::Error> {
        self.to_manifest()
    }

    fn skill_files(&self) -> SkillFiles {
        SkillFiles::from_embedded(self)
    }
}

/// An agent as its files describe it, before it is tied to tools, state and a model.
///
/// Made from a manifest with [`from_manifest`](Self::from_manifest) (the embedded one from
/// `adam::include_agent!()`, or the one a directory loads: the same code path), it takes the
/// values the code supplies for `{{placeholders}}`, and [`bind`](Self::bind) checks it against
/// the registered tools:
///
/// ```
/// # use adam_assembly::AgentDef;
/// # use adam_agent_fs::{Dir, ManifestSource, Strictness};
/// # use adam_llm_agent::ToolSet;
/// # let root = std::env::temp_dir().join("adam-assembly-doc-def");
/// # let _ = std::fs::remove_dir_all(&root);
/// # std::fs::create_dir_all(root.join("agent")).unwrap();
/// # std::fs::write(root.join("agent/instructions.md"),
/// #     "---\nname: helper\nvars: { tone: plain }\n---\nAnswer in a {{tone}} style.\n").unwrap();
/// let package = Dir::new(&root).load()?.into_package(Strictness::Lenient)?;
/// let def = AgentDef::from_manifest(package.agents[0].clone())?
///     .var("tone", "formal")
///     .bind(ToolSet::new())?;
/// # let _ = std::fs::remove_dir_all(&root);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct AgentDef {
    manifest: AgentManifest,
    /// Values supplied by the code, by agent (its registration name) and var.
    values: BTreeMap<String, BTreeMap<String, String>>,
    /// The bytes of the files the skills bundle.
    files: SkillFiles,
}

impl AgentDef {
    /// A definition from a manifest: `&AGENT` from `adam::include_agent!()`, or an
    /// [`AgentManifest`] from a directory.
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] when an embedded manifest cannot be decoded.
    pub fn from_manifest(manifest: impl IntoManifest) -> Result<Self, Error> {
        let files = manifest.skill_files();
        Ok(Self {
            manifest: manifest.into_manifest()?,
            values: BTreeMap::new(),
            files,
        })
    }

    /// Read the files the skills bundle (`references/`, `scripts/`, `assets/`) from `source`,
    /// the source the manifest came from, so that `read_skill_file` can serve them. Only a
    /// manifest made without its source needs this: an embedded agent brings its files, and
    /// [`from_source`](Self::from_source) calls this itself.
    ///
    /// The files are read now, once, so that a missing one is a startup error and a run never
    /// touches the disk.
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] when a file cannot be read, and [`Error::SkillTooLarge`] for a skill
    /// whose files are over [`SKILL_RESOURCE_LIMIT`](adam_agent_fs::SKILL_RESOURCE_LIMIT).
    pub fn resources_from(mut self, source: &impl ManifestSource) -> Result<Self, Error> {
        self.files = SkillFiles::read(&self.manifest, source)?;
        Ok(self)
    }

    /// One definition per agent of a source: one for an `agent/` package, one per directory of
    /// an `agents/` package. `strictness` decides whether warnings fail the load, as they do for
    /// a build script (`Strictness::Strict` is `build("agent").strict()`).
    ///
    /// # Errors
    ///
    /// [`Error::Manifest`] when the source cannot be read or has errors.
    pub fn from_source(
        source: &impl ManifestSource,
        strictness: Strictness,
    ) -> Result<Vec<Self>, Error> {
        source
            .load()?
            .into_package(strictness)?
            .agents
            .into_iter()
            .map(|agent| Self::from_manifest(agent)?.resources_from(source))
            .collect()
    }

    /// The root agent's name.
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// The manifest this definition was made from.
    pub fn manifest(&self) -> &AgentManifest {
        &self.manifest
    }

    /// Supply the value of a var of the root agent. It overrides the default under `vars`, and
    /// is the only way to give a value to a var declared without one (`vars: { repo: }`).
    #[must_use]
    pub fn var(self, name: impl Into<String>, value: impl ToString) -> Self {
        let root = self.manifest.name.clone();
        self.agent_var(root, name, value)
    }

    /// Supply the value of a var of another agent, by the name it is registered under
    /// (`coder/reviewer`). Naming an agent the definition does not contain fails at
    /// [`bind`](Self::bind).
    #[must_use]
    pub fn agent_var(
        mut self,
        agent: impl Into<String>,
        name: impl Into<String>,
        value: impl ToString,
    ) -> Self {
        self.values
            .entry(agent.into())
            .or_default()
            .insert(name.into(), value.to_string());
        self
    }

    /// Check the definition against the registered tools and render the prompts.
    ///
    /// Everything that can be wrong with the files, given these tools, is found here, at
    /// startup, and not in the middle of a run:
    ///
    /// * a name in `tools:` that no tool has ([`Error::UnknownTool`], with a "did you mean"),
    ///   or a pattern (`linear__*`) that matches none;
    /// * a `{{placeholder}}` that `vars` does not declare, a var that is declared and never
    ///   used, a var with no value, a value supplied for an undeclared var, or a `{{` that is
    ///   not a placeholder;
    /// * a value supplied for an agent the definition does not contain.
    ///
    /// The root agent gets every registered tool when it has no `tools:`; a subagent gets none
    /// (decision D3 of `docs/authoring.md`), whatever its parent has. Each agent then gets a
    /// tool per local subagent, named after it ([`SubagentTool`]), after its own tools and its
    /// skills' tools. Two more things are refused here:
    ///
    /// * a subagent tool whose name is already a tool of the parent, or another subagent's
    ///   ([`Error::SubagentToolClash`]);
    /// * a subagent with a tool that asks the user ([`Error::SubagentAsksUser`]): nobody would
    ///   answer it.
    ///
    /// Nothing is built yet: the model and the state come next ([`BoundDef`]).
    ///
    /// # Errors
    ///
    /// The first problem found, in the order above; see [`Error`].
    pub fn bind(self, tools: ToolSet) -> Result<BoundDef, Error> {
        let catalog = Catalog::new(&tools)?;
        let mut walk = Walk {
            catalog: &catalog,
            values: &self.values,
            files: &self.files,
            nodes: Vec::new(),
            remotes: Vec::new(),
        };
        walk.agent(&self.manifest, self.manifest.name.clone(), None)?;
        let Walk { nodes, remotes, .. } = walk;

        let known: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
        for agent in self.values.keys() {
            if !known.contains(agent) {
                return Err(Error::UnknownAgent {
                    agent: agent.clone(),
                    suggestion: closest(agent, known.iter().map(String::as_str)).map(str::to_owned),
                    known,
                });
            }
        }
        Ok(BoundDef::new(self.manifest, nodes, remotes))
    }
}

/// One local agent, bound: its rendered prompt and its tools. The model comes later.
#[derive(Clone)]
pub(crate) struct Node {
    /// The registration name: `coder`, `coder/reviewer`.
    pub(crate) name: String,
    /// The index of the parent in the node list; `None` for the root.
    pub(crate) parent: Option<usize>,
    pub(crate) file: PathBuf,
    pub(crate) description: Option<String>,
    pub(crate) model: ModelRef,
    pub(crate) prompt: String,
    pub(crate) tools: Vec<(String, DynTool)>,
    pub(crate) limits: Limits,
    /// The skills the agent may use, in order.
    pub(crate) skills: Vec<String>,
    /// The skills whose body is in the prompt.
    pub(crate) preloaded: Vec<String>,
}

/// A remote (A2A) subagent, found on the way. Data only until slice S9b.
#[derive(Debug, Clone)]
pub(crate) struct Remote {
    /// The registration name of the agent that owns it.
    pub(crate) parent: String,
    pub(crate) agent: adam_agent_fs::RemoteAgent,
}

/// The registered tools by name, in registration order.
struct Catalog {
    entries: Vec<(String, DynTool)>,
}

impl Catalog {
    fn new(tools: &ToolSet) -> Result<Self, Error> {
        let mut entries: Vec<(String, DynTool)> = Vec::new();
        for tool in tools {
            let name = tool.spec().name;
            if entries.iter().any(|(n, _)| *n == name) {
                return Err(Error::DuplicateTool { tool: name });
            }
            entries.push((name, tool.clone()));
        }
        Ok(Self { entries })
    }

    fn names(&self) -> Vec<String> {
        self.entries.iter().map(|(n, _)| n.clone()).collect()
    }

    fn get(&self, name: &str) -> Option<&(String, DynTool)> {
        self.entries.iter().find(|(n, _)| n == name)
    }
}

/// The walk over the agent tree, root first, each agent before its subagents.
struct Walk<'a> {
    catalog: &'a Catalog,
    values: &'a BTreeMap<String, BTreeMap<String, String>>,
    files: &'a SkillFiles,
    nodes: Vec<Node>,
    remotes: Vec<Remote>,
}

impl Walk<'_> {
    fn agent(
        &mut self,
        manifest: &AgentManifest,
        name: String,
        parent: Option<usize>,
    ) -> Result<(), Error> {
        let origin = Origin::new(name.clone(), manifest.path.clone());
        let mut tools = resolve_tools(
            &origin,
            manifest.frontmatter.tools.as_ref(),
            parent.is_none(),
            self.catalog,
        )?;
        if parent.is_some() {
            refuse_asking_tools(&origin, &tools)?;
        }
        let mut prompt = render_prompt(&origin, manifest, self.values.get(&name))?;
        let registered = tools.len();
        let (skills, preloaded) =
            add_skills(&origin, manifest, self.files, &mut prompt, &mut tools)?;
        // The tools `add_skills` appended, so a clash can say whose tool it hit.
        let skill_tools: Vec<String> = tools[registered..].iter().map(|(n, _)| n.clone()).collect();
        add_subagent_tools(&origin, manifest, &skill_tools, &mut tools)?;
        let index = self.nodes.len();
        self.nodes.push(Node {
            name: name.clone(),
            parent,
            file: manifest.path.clone(),
            description: manifest.frontmatter.description.clone(),
            model: manifest
                .frontmatter
                .model
                .clone()
                .unwrap_or(ModelRef::Inherit),
            prompt,
            tools,
            limits: limits(manifest.frontmatter.limits.as_ref()),
            skills,
            preloaded,
        });
        for sub in &manifest.subagents {
            match sub {
                Subagent::Local(child) => {
                    self.agent(child, format!("{name}/{}", child.name), Some(index))?;
                }
                Subagent::Remote(remote) => self.remotes.push(Remote {
                    parent: name.clone(),
                    agent: remote.clone(),
                }),
            }
        }
        Ok(())
    }
}

/// Give the agent its skills: the catalog and the preloaded skills after the prompt, then the
/// tools `load_skill` and `read_skill_file` after the agent's own. Returns the names of the
/// selected and of the preloaded skills. Nothing is added for an agent without skills.
fn add_skills(
    origin: &Origin,
    manifest: &AgentManifest,
    files: &SkillFiles,
    prompt: &mut String,
    tools: &mut Vec<(String, DynTool)>,
) -> Result<(Vec<String>, Vec<String>), Error> {
    let Some(set) = skills::resolve(origin, manifest, files)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let set = Arc::new(set);
    let added = set.tools();
    for tool in &added {
        let name = tool.spec().name;
        if tools.iter().any(|(n, _)| *n == name) {
            return Err(Error::ReservedToolName {
                origin: origin.clone(),
                tool: name,
            });
        }
    }
    tools.extend(added.into_iter().map(|tool| (tool.spec().name, tool)));
    let section = set.prompt_section();
    // A prompt is never empty (the loader refuses an agent without instructions), and a set
    // with skills always has a section.
    prompt.push_str("\n\n");
    prompt.push_str(&section);
    Ok((set.names(), set.preloaded()))
}

/// A subagent runs as a child of another run: there is nobody to answer a question, and a run
/// that asks parks until someone does. Refuse the tools that say they ask.
fn refuse_asking_tools(origin: &Origin, tools: &[(String, DynTool)]) -> Result<(), Error> {
    match tools.iter().find(|(_, tool)| tool.asks_user()) {
        Some((name, _)) => Err(Error::SubagentAsksUser {
            origin: origin.clone(),
            tool: name.clone(),
        }),
        None => Ok(()),
    }
}

/// Give the agent a tool for each of its local subagents, named after the subagent and placed
/// after the agent's own tools and its skills' tools, in the order of the manifest. The name must
/// not be one the agent's model already sees.
fn add_subagent_tools(
    origin: &Origin,
    manifest: &AgentManifest,
    skill_tools: &[String],
    tools: &mut Vec<(String, DynTool)>,
) -> Result<(), Error> {
    // The subagent tools added so far, by name, with the file of the subagent that owns each.
    let mut subagents: Vec<(&str, &std::path::Path)> = Vec::new();
    for sub in &manifest.subagents {
        let Subagent::Local(child) = sub else {
            continue;
        };
        let clash = if let Some((_, file)) = subagents.iter().find(|(n, _)| *n == child.name) {
            Some(ToolClash::Subagent {
                file: file.to_path_buf(),
            })
        } else if tools.iter().any(|(n, _)| *n == child.name) {
            // Whose tool it is depends on what `add_skills` added, not on the name: without
            // skills, a registered tool may be called `load_skill`.
            Some(if skill_tools.contains(&child.name) {
                ToolClash::SkillTool
            } else {
                ToolClash::Tool
            })
        } else {
            None
        };
        if let Some(clash) = clash {
            return Err(Error::SubagentToolClash {
                origin: Origin::new(
                    format!("{}/{}", origin.agent, child.name),
                    child.path.clone(),
                ),
                parent: origin.agent.clone(),
                tool: child.name.clone(),
                clash,
            });
        }
        let tool = SubagentTool::new(
            child.name.clone(),
            format!("{}/{}", origin.agent, child.name),
            child.frontmatter.description.as_deref().unwrap_or_default(),
        );
        subagents.push((&child.name, &child.path));
        tools.push((child.name.clone(), Arc::new(tool)));
    }
    Ok(())
}

/// The loop's limits: what the frontmatter sets, the loop's default for the rest.
fn limits(file: Option<&FileLimits>) -> Limits {
    let default = Limits::default();
    let Some(file) = file else {
        return default;
    };
    Limits {
        max_turns: file.max_turns.unwrap_or(default.max_turns),
        max_tool_calls: file.max_tool_calls.unwrap_or(default.max_tool_calls),
        max_output_tokens: file.max_output_tokens.unwrap_or(default.max_output_tokens),
        max_history_tokens: file
            .max_history_tokens
            .unwrap_or(default.max_history_tokens),
    }
}

/// The tools an agent gets, by name, in the order its `tools:` lists them.
fn resolve_tools(
    origin: &Origin,
    list: Option<&ToolList>,
    is_root: bool,
    catalog: &Catalog,
) -> Result<Vec<(String, DynTool)>, Error> {
    let names = match list {
        // A root agent with no `tools:` gets everything registered; a subagent gets nothing.
        None if is_root => return Ok(catalog.entries.clone()),
        None => return Ok(Vec::new()),
        Some(ToolList::All) => return Ok(catalog.entries.clone()),
        Some(ToolList::Named(names)) => names,
    };
    let mut chosen: Vec<(String, DynTool)> = Vec::new();
    let mut add = |entry: &(String, DynTool)| {
        if !chosen.iter().any(|(n, _)| *n == entry.0) {
            chosen.push(entry.clone());
        }
    };
    for wanted in names {
        if wanted.contains('*') {
            let mut any = false;
            for entry in catalog.entries.iter().filter(|(n, _)| glob(wanted, n)) {
                any = true;
                add(entry);
            }
            if !any {
                return Err(Error::NoToolMatches {
                    origin: origin.clone(),
                    pattern: wanted.clone(),
                    available: catalog.names(),
                });
            }
        } else if let Some(entry) = catalog.get(wanted) {
            add(entry);
        } else {
            let available = catalog.names();
            return Err(Error::UnknownTool {
                origin: origin.clone(),
                tool: wanted.clone(),
                suggestion: closest(wanted, available.iter().map(String::as_str))
                    .map(str::to_owned),
                available,
            });
        }
    }
    Ok(chosen)
}

/// Whether `name` matches `pattern`, where `*` stands for any run of characters.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    // `split` yields at least one piece; without a `*` there is exactly one.
    let (first, rest) = (parts[0], &parts[1..]);
    let Some((last, middles)) = rest.split_last() else {
        return pattern == name;
    };
    let Some(mut tail) = name.strip_prefix(first) else {
        return false;
    };
    for middle in middles {
        let Some(at) = tail.find(middle) else {
            return false;
        };
        tail = &tail[at + middle.len()..];
    }
    tail.ends_with(last)
}

/// Render the prompt of one agent: `instructions.md` and each `instructions/*.md`, then joined
/// the way [`Instructions::prompt`] joins them.
fn render_prompt(
    origin: &Origin,
    manifest: &AgentManifest,
    supplied: Option<&BTreeMap<String, String>>,
) -> Result<String, Error> {
    let declared = &manifest.frontmatter.vars;
    let no_values = BTreeMap::new();
    let supplied = supplied.unwrap_or(&no_values);

    // The files of the prompt. A part sits in `instructions/` next to the instructions file.
    let parts_dir = manifest.path.with_file_name("instructions");
    let files: Vec<(PathBuf, &str)> =
        std::iter::once((manifest.path.clone(), manifest.instructions.body.as_str()))
            .chain(
                manifest
                    .instructions
                    .parts
                    .iter()
                    .map(|p| (parts_dir.join(&p.file), p.body.as_str())),
            )
            .collect();

    let mut parsed: Vec<(PathBuf, Vec<Piece>)> = Vec::with_capacity(files.len());
    for (file, text) in files {
        let pieces = template::parse(text).map_err(|syntax| Error::Template {
            origin: origin.in_file(&file),
            line: syntax.line,
            problem: syntax.problem,
        })?;
        parsed.push((file, pieces));
    }

    let declared_names = || declared.keys().map(String::as_str);
    let mut used: BTreeSet<&str> = BTreeSet::new();
    for (file, pieces) in &parsed {
        for piece in pieces {
            let Piece::Var { name, line } = piece else {
                continue;
            };
            if !declared.contains_key(name) {
                return Err(Error::UnknownVar {
                    origin: origin.in_file(file),
                    line: *line,
                    var: name.clone(),
                    suggestion: closest(name, declared_names()).map(str::to_owned),
                    declared: declared_names().map(str::to_owned).collect(),
                });
            }
            used.insert(name);
        }
    }
    if let Some(name) = supplied.keys().find(|n| !declared.contains_key(*n)) {
        return Err(Error::UnknownVarValue {
            origin: origin.clone(),
            var: name.clone(),
            suggestion: closest(name, declared_names()).map(str::to_owned),
            declared: declared_names().map(str::to_owned).collect(),
        });
    }
    if let Some(name) = declared_names().find(|n| !used.contains(n)) {
        return Err(Error::UnusedVar {
            origin: origin.clone(),
            var: name.to_owned(),
        });
    }
    let mut values = declared.clone();
    values.extend(supplied.clone());
    if let Some(name) = used
        .iter()
        .find(|n| values.get(**n).is_none_or(String::is_empty))
        .copied()
    {
        return Err(Error::UnsetVar {
            origin: origin.clone(),
            var: (*name).to_owned(),
        });
    }

    let mut rendered = parsed
        .iter()
        .map(|(_, pieces)| template::render(pieces, &values));
    let body = rendered.next().unwrap_or_default();
    let parts = manifest
        .instructions
        .parts
        .iter()
        .zip(rendered)
        .map(|(part, body)| InstructionPart {
            file: part.file.clone(),
            body,
        })
        .collect();
    Ok(Instructions { body, parts }.prompt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_matches_any_run() {
        assert!(glob("linear__*", "linear__list_issues"));
        assert!(glob("linear__*", "linear__"));
        assert!(!glob("linear__*", "github__list"));
        assert!(!glob("linear__*", "linear_"));
        assert!(glob("*_issues", "linear__list_issues"));
        assert!(glob("*", "anything"));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(glob("a*b*c", "abc"));
        assert!(!glob("a*b*c", "acb"));
        assert!(!glob("a*b*c", "aXbY"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
        // The pieces of a pattern cannot overlap: `ab*ba` needs four characters.
        assert!(!glob("ab*ba", "aba"));
    }

    #[test]
    fn limits_take_the_file_over_the_default() {
        assert_eq!(limits(None), Limits::default());
        let set = FileLimits {
            max_turns: Some(7),
            max_history_tokens: Some(0),
            ..FileLimits::default()
        };
        let got = limits(Some(&set));
        assert_eq!(got.max_turns, 7);
        assert_eq!(got.max_history_tokens, 0);
        assert_eq!(got.max_tool_calls, Limits::default().max_tool_calls);
        assert_eq!(got.max_output_tokens, Limits::default().max_output_tokens);
    }
}
