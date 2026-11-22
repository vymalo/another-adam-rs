//! Tool groups and their loaders (ADR 0066).
//!
//! A **group** is a named set of tools that load together. Every group the agent may load is
//! announced by one generated tool, `load__group__<name>`: its call appends the group's tools to
//! the run's loaded set, and the loop offers them on the next request **after every other tool**,
//! never inserted, so the request before stays a byte prefix of the one after (ADR 0044, decision
//! 1). A group whose members the agent was not given — a `tools:` allow-list narrowed them, or the
//! deployment withheld them — has **no loader**: lazy never widens access, and a forbidden tool is
//! in no loader, so its existence is not revealed.
//!
//! The group's tools are the agent's own tools, withheld by [`Groups`] until the group is loaded.
//! A group whose [`keywords`](Group::keywords) the run's task holds is loaded **before the first
//! model call**, the way `mcp.json`'s `when` was: the task is the same text at every turn and at
//! every call, so the tools array does not move and `tools_appended` records nothing.
//!
//! The loader's own schema and description are constants (the group's name and its short note, never
//! a member count), so the tool list of a run never depends on what a group happens to hold. A
//! group the **task** loads is the `when` behaviour: its tools stand where the agent's list puts
//! them and its note reaches the first request's prompt. A group the run loads during itself says
//! its note in the load's result, so the prompt does not grow mid-run.

use std::sync::Arc;

use adam_llm_agent::{
    DynTool, Loaded, Loader, SourceCtx, StepStyle, Tool, ToolAccess, ToolCtx, ToolError,
    ToolOutput, ToolSource, Withheld,
};
use adam_model::ToolSpec;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{Error, Origin};

/// The most characters a group's name holds: the rule `mcp.json`'s loader applies to a `when`
/// server's name, because that name becomes its group's name.
pub const MAX_GROUP_NAME_CHARS: usize = adam_agent_fs::MAX_GROUP_NAME_CHARS;

/// The most keywords a group may hold: the bound of `mcp.json`'s `when`
/// (`adam_agent_fs::MAX_WHEN_KEYWORDS`).
pub const MAX_KEYWORDS: usize = 16;
/// The longest keyword a group may hold, in characters: the bound of `mcp.json`'s `when`
/// (`adam_agent_fs::MAX_WHEN_KEYWORD_CHARS`).
pub const MAX_KEYWORD_CHARS: usize = 64;

/// The prefix of a group loader's tool name: the one constant `adam-llm-agent` exports, so the
/// state's migration and the assembly cannot disagree.
pub const GROUP_LOADER_PREFIX: &str = adam_llm_agent::GROUP_LOADER_PREFIX;

/// A named set of tools that load together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The group's name: `^[a-z0-9]+(_[a-z0-9]+)*$`, at most [`MAX_GROUP_NAME_CHARS`] characters.
    pub name: String,
    /// What the group's tools are for: the text a load's result says, in full.
    pub note: String,
    /// The note as a compaction's recap keeps it: one line, at most
    /// [`adam_llm_agent::MAX_LOADED_NOTE_BYTES`] bytes.
    pub short_note: String,
    /// Whether the loader searches the group (S2; refused until then).
    pub searchable: bool,
    /// The words that load the group before the first call, any one of which the task may hold.
    pub keywords: Vec<String>,
    /// The tools it holds, in the order the request offers them.
    pub members: Vec<String>,
    /// The MCP server whose `when` made this group, if it came from one: its name is the group's
    /// name, and its note is the server's own words (labelled in a compaction's recap).
    pub server: Option<String>,
}

impl Group {
    /// A group of `members`, with `note` as what its loader says it is for.
    pub fn new(name: impl Into<String>, note: impl Into<String>, members: Vec<String>) -> Self {
        let note = note.into();
        Self {
            name: name.into(),
            short_note: short_note(&note),
            note,
            searchable: false,
            keywords: Vec::new(),
            members,
            server: None,
        }
    }

    /// The group an MCP server's `when` makes: named after the server, its note the server's words.
    pub fn for_server(
        server: impl Into<String>,
        note: impl Into<String>,
        members: Vec<String>,
    ) -> Self {
        let server = server.into();
        let mut group = Self::new(server.clone(), note, members);
        group.server = Some(server);
        group
    }

    /// The words that load it before the first call.
    #[must_use]
    pub fn with_keywords(mut self, keywords: Vec<String>) -> Self {
        self.keywords = keywords;
        self
    }

    /// A searchable group (S2; [`Groups::bind`] refuses it until then).
    #[must_use]
    pub fn searchable(mut self) -> Self {
        self.searchable = true;
        self
    }

    /// The name of the loader that loads it.
    pub fn loader(&self) -> String {
        format!("{GROUP_LOADER_PREFIX}{}", self.name)
    }

    /// Whether a run whose task is `task` loads it before the first call: any keyword is held by
    /// the task, ignoring case. No task matches nothing.
    fn matches(&self, task: Option<&str>) -> bool {
        match task {
            Some(task) => {
                let task = task.to_lowercase();
                self.keywords
                    .iter()
                    .any(|word| !word.is_empty() && task.contains(&word.to_lowercase()))
            }
            None => false,
        }
    }
}

/// `note` as one line, at most [`SHORT_NOTE_MAX_BYTES`], ending with an ellipsis when cut: the one
/// rule, from `adam-llm-agent`, so a note the loop receives and a note a group builds are bounded
/// the same way.
fn short_note(note: &str) -> String {
    adam_llm_agent::loaded_note(note)
}

/// Whether `name` is a group name: `^[a-z0-9]+(_[a-z0-9]+)*$`, at most [`MAX_GROUP_NAME_CHARS`].
pub fn name_fits(name: &str) -> bool {
    adam_agent_fs::group_name_fits(name)
}

/// Whether `keywords` are within the bounds `mcp.json`'s `when` sets: 1 to [`MAX_KEYWORDS`], each
/// 1 to [`MAX_KEYWORD_CHARS`] characters with no control or format character.
pub fn keywords_fit(keywords: &[String]) -> bool {
    (1..=MAX_KEYWORDS).contains(&keywords.len())
        && keywords.iter().all(|word| {
            let count = word.chars().count();
            (1..=MAX_KEYWORD_CHARS).contains(&count)
                && !word.chars().any(adam_agent_fs::is_format_or_control)
        })
}

/// The groups of one agent, and the source that withholds their members until they are loaded.
#[derive(Debug, Clone, Default)]
pub struct Groups {
    /// The groups with at least one member the agent was given, in name order.
    groups: Vec<Group>,
    /// The spec of every member, by name: what a load appends.
    specs: Vec<(String, ToolSpec)>,
}

impl Groups {
    /// The groups `defs` as this agent can offer them: every member the agent was not given
    /// (`given`, the names of its own tools) is dropped, and a group left empty has no loader.
    ///
    /// A name that does not fit [`name_fits`], two groups with one name, a searchable group (S2 is
    /// not built yet) and a loader whose name is already a tool of the agent are refused.
    pub fn bind(
        origin: &Origin,
        defs: Vec<Group>,
        given: &[(String, DynTool)],
    ) -> Result<Self, Error> {
        let mut groups: Vec<Group> = Vec::new();
        let mut specs: Vec<(String, ToolSpec)> = Vec::new();
        for mut group in defs {
            if !name_fits(&group.name) {
                return Err(Error::GroupName {
                    origin: origin.clone(),
                    group: group.name,
                    reason: if group.server.is_some() {
                        "an MCP server with a `when` has its name as its group's name: lowercase \
                         letters and digits, single underscores between them, at most 40 \
                         characters; rename the server"
                    } else {
                        "a group's name is lowercase letters and digits, single underscores \
                         between them, at most 40 characters"
                    },
                });
            }
            if !group.keywords.is_empty() && !keywords_fit(&group.keywords) {
                return Err(Error::GroupName {
                    origin: origin.clone(),
                    group: group.name,
                    reason: "a group has 1 to 16 keywords, each 1 to 64 characters with no control \
                             or format character",
                });
            }
            if group.searchable {
                return Err(Error::SearchableGroup {
                    origin: origin.clone(),
                    group: group.name,
                });
            }
            if groups.iter().any(|one| one.name == group.name) {
                return Err(Error::GroupName {
                    origin: origin.clone(),
                    group: group.name,
                    reason: "another group has this name",
                });
            }
            if given.iter().any(|(tool, _)| *tool == group.loader()) {
                return Err(Error::ReservedLoaderName {
                    origin: origin.clone(),
                    tool: group.loader(),
                });
            }
            let mut kept: Vec<String> = Vec::new();
            for member in group.members.drain(..) {
                if let Some((_, tool)) = given.iter().find(|(name, _)| name == &member) {
                    if !specs.iter().any(|(name, _)| name == &member) {
                        specs.push((member.clone(), tool.spec()));
                    }
                    kept.push(member);
                }
            }
            group.members = kept;
            // A tool in two groups would be refused by one group's loader and offered by the
            // other's: a load must append it once, from one group.
            if let Some((tool, first)) = group.members.iter().find_map(|member| {
                groups
                    .iter()
                    .find(|one| one.members.contains(member))
                    .map(|one| (member.clone(), one.name.clone()))
            }) {
                return Err(Error::GroupOverlap {
                    origin: origin.clone(),
                    tool,
                    first,
                    second: group.name,
                });
            }
            if group.members.is_empty() {
                // Nothing this agent may load: no loader, so its existence is not revealed.
                continue;
            }
            groups.push(group);
        }
        groups.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self { groups, specs })
    }

    /// The loaders of the groups the agent can offer, in name order, before the task is read: the
    /// catalogue S2/S3 and the tests read. The run-time path is [`ToolSource::loaders`], which
    /// leaves out the groups the task already loads.
    pub fn declared_loaders(&self) -> Vec<Loader> {
        self.groups
            .iter()
            .map(|group| Loader {
                name: group.loader(),
                holds: group.members.clone(),
                note: group.short_note.clone(),
                note_is_server: group.server.is_some(),
            })
            .collect()
    }

    /// One loader tool per group, in name order: the tools the agent gets in addition to its own.
    pub fn loader_tools(&self) -> Vec<(String, DynTool)> {
        self.groups
            .iter()
            .map(|group| {
                let name = group.loader();
                let tool: DynTool = Arc::new(GroupLoader {
                    group: group.clone(),
                });
                (name, tool)
            })
            .collect()
    }

    /// The group whose loader is `loader`, if one is.
    fn group_of(&self, loader: &str) -> Option<&Group> {
        self.groups.iter().find(|group| group.loader() == loader)
    }

    /// Whether `group` is loaded for this run: the run loaded it, or the task holds one of its
    /// keywords.
    fn is_loaded(&self, group: &Group, ctx: &SourceCtx) -> bool {
        group.matches(ctx.task()) || ctx.loaded().iter().any(|one| one.loader == group.loader())
    }

    /// The spec of `member`, if it is one of the group's tools this agent has.
    fn spec(&self, member: &str) -> Option<&ToolSpec> {
        self.specs
            .iter()
            .find(|(name, _)| name == member)
            .map(|(_, spec)| spec)
    }
}

/// What the model is told when it calls a tool of a group that is not loaded.
fn reason(group: &Group, tool: &str) -> String {
    let words: Vec<&str> = group.keywords.iter().take(8).map(String::as_str).collect();
    let more = group.keywords.len().saturating_sub(words.len());
    let when = match (words.is_empty(), more) {
        (true, _) => String::new(),
        (false, 0) => format!(
            ", which the task loads when it mentions one of its keywords ({})",
            words.join(", ")
        ),
        (false, n) => format!(
            ", which the task loads when it mentions one of its keywords ({} and {n} more)",
            words.join(", ")
        ),
    };
    format!(
        "`{tool}` is not loaded for this run: it belongs to the group `{}`{when}. Call `{}` to load \
         the group, and its tools are available from your next call.",
        group.name,
        group.loader()
    )
}

/// What the model is told when it calls a loaded tool that a request did not carry.
fn removed(tool: &str) -> String {
    format!(
        "`{tool}` was loaded earlier and is not available in this conversation now: the request \
         that followed left it out. Do not call it again; use what you have."
    )
}

#[async_trait]
impl ToolSource for Groups {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        // The tools are the agent's own (`bind` put them there); this source only decides which of
        // them a run may see.
        Vec::new()
    }

    /// The notes of the groups the **task** loaded, in name order: the `when` behaviour, whose
    /// instructions reached the prompt of a matching run. A group the run loads during itself is
    /// not here: its note is in the load's result, so the prompt does not grow mid-run.
    async fn instructions(&self, ctx: &SourceCtx) -> Option<String> {
        let sections: Vec<(String, String)> = self
            .groups
            .iter()
            .filter(|group| group.matches(ctx.task()))
            .map(|group| (group.name.clone(), group.note.clone()))
            .collect();
        let text = crate::mcp::join_sections(&sections);
        (!text.is_empty()).then_some(text)
    }

    async fn loaders(&self, ctx: &SourceCtx) -> Vec<Loader> {
        // A group the task already loads is loaded, not available: it is not a loader to call.
        let removed = ctx.removed_tools();
        self.groups
            .iter()
            .filter(|group| !group.matches(ctx.task()))
            .map(|group| Loader {
                name: group.loader(),
                holds: group
                    .members
                    .iter()
                    .filter(|member| !removed.contains(member))
                    .cloned()
                    .collect(),
                note: group.short_note.clone(),
                note_is_server: group.server.is_some(),
            })
            .collect()
    }

    /// The loaded tools the groups still hold, minus the journaled removals: a member the group no
    /// longer has is a permanent removal; one the tier hides on this message is still here.
    async fn loaded_tools(&self, ctx: &SourceCtx) -> Option<Vec<String>> {
        let removed = ctx.removed_tools();
        let mut out = Vec::new();
        for group in &self.groups {
            if !self.is_loaded(group, ctx) {
                continue;
            }
            for member in &group.members {
                if !removed.contains(member) && !out.contains(member) {
                    out.push(member.clone());
                }
            }
        }
        Some(out)
    }

    /// The groups the task loaded, with their members and short notes: journaled on the run's
    /// first turn, so a compaction's recap names them. They record no growth: their tools were
    /// never withheld.
    async fn loaded_by_task(&self, ctx: &SourceCtx) -> Vec<Loaded> {
        let removed = ctx.removed_tools();
        self.groups
            .iter()
            .filter(|group| group.matches(ctx.task()))
            .map(|group| Loaded {
                loader: group.loader(),
                tools: group
                    .members
                    .iter()
                    .filter(|member| !removed.contains(member))
                    .cloned()
                    .collect(),
                note: group.short_note.clone(),
                note_is_server: group.server.is_some(),
            })
            .collect()
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let mut held = Vec::new();
        for tool in tools {
            if ctx.removed_tools().iter().any(|removed| removed == tool) {
                held.push(Withheld::new(*tool, removed(tool)));
                continue;
            }
            let Some(group) = self
                .groups
                .iter()
                .find(|group| group.members.iter().any(|member| member == tool))
            else {
                continue;
            };
            if !self.is_loaded(group, ctx) {
                held.push(Withheld::new(*tool, reason(group, tool)));
            }
        }
        held
    }

    /// The tools of the groups the run loaded, in the order it loaded them: a load appends to the
    /// tools array, never inserts into it. A group the **task** loaded is not here: its tools were
    /// never withheld, so they stand where the agent's own list puts them, as `mcp.json`'s `when`
    /// always did. A member a request did not carry earlier is not offered again.
    async fn appended(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        let removed = ctx.removed_tools();
        let mut out: Vec<ToolSpec> = Vec::new();
        let mut push = |member: &String| {
            if removed.contains(member) || out.iter().any(|spec| spec.name == *member) {
                return;
            }
            if let Some(spec) = self.spec(member) {
                out.push(spec.clone());
            }
        };
        for loaded in ctx.loaded() {
            if let Some(group) = self.group_of(&loaded.loader) {
                // A task-loaded group is journaled in `loaded` for the recap, but its tools were
                // never withheld: they stand where the agent's own list puts them, not appended.
                if group.matches(ctx.task()) {
                    continue;
                }
                for member in &group.members {
                    push(member);
                }
            }
        }
        out
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        // Every tool is an own tool of the agent: the agent calls it, not this source.
        None
    }
}

/// The fixed `load__group__<name>` tool of one group.
pub(crate) struct GroupLoader {
    group: Group,
}

/// The loader's description: the group's name and short note, never a member count, so the tool
/// list of a run is the same bytes whatever the group holds.
fn description(group: &Group) -> String {
    let what = if group.short_note.is_empty() {
        format!("the `{}` group of tools", group.name)
    } else {
        format!("the `{}` group: {}", group.name, group.short_note)
    };
    format!(
        "Load {what}. Its tools are available from your next call, added after the ones you have. \
         Call it when the task needs them; a group already loaded is refused, and the call costs \
         one full re-read of the prompt."
    )
}

#[async_trait]
impl Tool for GroupLoader {
    /// Loads state; it touches no file.
    fn access(&self, _args: &Value) -> ToolAccess {
        ToolAccess::Independent
    }

    fn step_style(&self) -> StepStyle {
        StepStyle::default().with_label("Load a group of tools")
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.group.loader(),
            description: description(&self.group),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
        let loader = self.group.loader();
        if ctx.loaded().iter().any(|one| one.loader == loader) {
            return Ok(ToolOutput::error(format!(
                "The group `{}` is already loaded (this run loaded it): its tools are available \
                 ({}). Do not load it again.",
                self.group.name,
                self.group.members.join(", ")
            )));
        }
        if self.group.matches(ctx.task()) {
            return Ok(ToolOutput::error(format!(
                "The group `{}` is already loaded (the task mentions one of its keywords): its \
                 tools are available ({}). Do not load it again.",
                self.group.name,
                self.group.members.join(", ")
            )));
        }
        let mut loaded: Vec<Loaded> = ctx.loaded().to_vec();
        loaded.push(Loaded {
            loader,
            tools: self.group.members.clone(),
            note: self.group.short_note.clone(),
            note_is_server: self.group.server.is_some(),
        });
        let mut text = format!(
            "Loaded the group `{}`. Its tools are available from your next call, added after the \
             ones you have: {}.",
            self.group.name,
            self.group.members.join(", ")
        );
        if !self.group.note.is_empty() {
            text.push('\n');
            text.push_str(&self.group.note);
        }
        text.push_str(
            "\nThat call re-reads the whole prompt, because the tool list grew: it is slower than \
             the calls before it.",
        );
        Ok(ToolOutput::text(text).with_loaded(loaded))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT_NOTE_MAX_BYTES: usize = adam_llm_agent::MAX_LOADED_NOTE_BYTES;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: format!("the {name} tool"),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn given(names: &[&str]) -> Vec<(String, DynTool)> {
        names
            .iter()
            .map(|name| {
                let tool: DynTool = Arc::new(Fixed((*name).to_owned()));
                ((*name).to_owned(), tool)
            })
            .collect()
    }

    struct Fixed(String);

    #[async_trait]
    impl Tool for Fixed {
        fn spec(&self) -> ToolSpec {
            spec(&self.0)
        }

        async fn call(&self, _ctx: &ToolCtx, _args: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(format!("{}-out", self.0)))
        }
    }

    fn origin() -> Origin {
        Origin {
            agent: "root".into(),
            file: "agent/instructions.md".into(),
        }
    }

    fn group(name: &str, members: &[&str]) -> Group {
        Group::new(
            name,
            format!("what {name} is for"),
            members.iter().map(|m| (*m).to_owned()).collect(),
        )
    }

    #[test]
    fn a_group_whose_members_are_all_given_has_a_loader() {
        let groups = Groups::bind(
            &origin(),
            vec![group("bash", &["run_command", "read_file"])],
            &given(&["run_command", "read_file"]),
        )
        .unwrap();
        assert_eq!(
            groups.declared_loaders(),
            [Loader {
                name: "load__group__bash".into(),
                holds: vec!["run_command".into(), "read_file".into()],
                note: "what bash is for".into(),
                note_is_server: false,
            }]
        );
        assert_eq!(groups.loader_tools().len(), 1);
    }

    #[test]
    fn a_member_the_agent_was_not_given_is_dropped_and_an_empty_group_has_no_loader() {
        let groups = Groups::bind(
            &origin(),
            vec![group("bash", &["run_command", "write_file"])],
            &given(&["run_command"]),
        )
        .unwrap();
        assert_eq!(groups.declared_loaders()[0].holds, ["run_command"]);
        let empty = Groups::bind(
            &origin(),
            vec![group("repo", &["write_file"])],
            &given(&["run_command"]),
        )
        .unwrap();
        assert!(empty.declared_loaders().is_empty());
        assert!(empty.loader_tools().is_empty());
    }

    #[test]
    fn a_searchable_group_is_refused_until_s2() {
        let error = Groups::bind(
            &origin(),
            vec![group("mine", &["a"]).searchable()],
            &given(&["a"]),
        )
        .unwrap_err();
        assert!(matches!(error, Error::SearchableGroup { .. }), "{error}");
        let text = error.to_string();
        assert!(
            text.contains("cannot bind a searchable group until S2"),
            "{text}"
        );
    }

    #[test]
    fn a_group_name_that_does_not_fit_is_refused() {
        for bad in ["", "Bash", "a__b", "a_", "_a", "a-b", &"a".repeat(41)] {
            let error =
                Groups::bind(&origin(), vec![group(bad, &["a"])], &given(&["a"])).unwrap_err();
            assert!(matches!(error, Error::GroupName { .. }), "{bad}: {error}");
        }
        for good in ["bash", "a", "a_b", "a1_b2"] {
            assert!(name_fits(good), "{good}");
        }
    }

    #[test]
    fn a_registered_tool_named_after_a_loader_is_refused() {
        let error = Groups::bind(
            &origin(),
            vec![group("bash", &["run_command"])],
            &given(&["run_command", "load__group__bash"]),
        )
        .unwrap_err();
        assert!(
            matches!(error, Error::ReservedLoaderName { ref tool, .. } if tool == "load__group__bash"),
            "{error}"
        );
    }

    #[test]
    fn the_keywords_are_bounded_like_a_when() {
        assert!(keywords_fit(&["linear".into()]));
        assert!(!keywords_fit(&[]));
        assert!(!keywords_fit(&vec!["k".into(); MAX_KEYWORDS + 1]));
        assert!(!keywords_fit(&["x".repeat(MAX_KEYWORD_CHARS + 1)]));
        assert!(keywords_fit(&["x".repeat(MAX_KEYWORD_CHARS)]));
        assert!(!keywords_fit(&["a\nb".into()]));
    }

    /// The refusal names at most eight keywords and says how many more, as `when`'s did.
    #[test]
    fn a_keyword_refusal_lists_at_most_eight_and_says_how_many_more() {
        let keywords: Vec<String> = (0..16).map(|n| format!("k{n}")).collect();
        let group = group("x", &["x__t"]).with_keywords(keywords);
        let text = reason(&group, "x__t");
        assert!(
            text.contains("k0, k1, k2, k3, k4, k5, k6, k7 and 8 more"),
            "{text}"
        );
        assert!(
            !text.contains("k8"),
            "the ninth is counted, not named: {text}"
        );
    }

    #[test]
    fn the_short_note_ends_with_an_ellipsis_when_cut() {
        let note = short_note(&"x".repeat(300));
        assert!(note.ends_with('…'), "{note}");
        assert!(note.len() <= SHORT_NOTE_MAX_BYTES, "{}", note.len());
        assert!(note.len() > SHORT_NOTE_MAX_BYTES - 10, "{}", note.len());
        // A Unicode line separator is flattened like a newline.
        let note = short_note("a\u{2028}b\u{2029}c");
        assert_eq!(note, "a b c");
    }

    /// A seeded old state's loader is the group's, so its tools are offered and a call is not
    /// refused.
    #[tokio::test]
    async fn a_seeded_loaded_set_offers_the_groups_tools() {
        let groups = Groups::bind(
            &origin(),
            vec![group("docs", &["docs__search"])],
            &given(&["docs__search"]),
        )
        .unwrap();
        let seeded = vec![Loaded {
            loader: "load__group__docs".into(),
            tools: Vec::new(),
            note: String::new(),
            note_is_server: true,
        }];
        let ctx = ctx(Some("a typo"), seeded);
        assert!(
            groups.withhold(&ctx, &["docs__search"]).await.is_empty(),
            "the seeded loader is the group's: the tool is not withheld"
        );
        assert_eq!(
            groups.appended(&ctx).await[0].name,
            "docs__search",
            "and the tool is offered"
        );
    }

    #[test]
    fn the_short_note_is_one_line_of_at_most_two_hundred_bytes() {
        let long = format!("a\nb {}", "x".repeat(300));
        let note = short_note(&long);
        assert!(!note.contains('\n'), "{note}");
        assert!(note.len() <= SHORT_NOTE_MAX_BYTES, "{}", note.len());
    }

    #[test]
    fn the_task_loads_a_group_by_its_keywords() {
        let group = group("linear", &["linear__x"]).with_keywords(vec!["linear".into()]);
        assert!(group.matches(Some("fix the LINEAR ticket")));
        assert!(!group.matches(Some("a typo")));
        assert!(!group.matches(None));
    }

    fn ctx(task: Option<&str>, loaded: Vec<Loaded>) -> SourceCtx {
        SourceCtx::detached(serde_json::Map::new())
            .with_task(task)
            .with_loaded(loaded)
    }

    #[tokio::test]
    async fn an_unloaded_group_is_withheld_and_a_loaded_one_is_appended() {
        let groups = Groups::bind(
            &origin(),
            vec![
                group("bash", &["run_command", "read_file"]),
                group("repo", &["write_file"]),
            ],
            &given(&["run_command", "read_file", "write_file"]),
        )
        .unwrap();
        let held = groups
            .withhold(
                &ctx(Some("a typo"), Vec::new()),
                &["run_command", "read_file", "write_file"],
            )
            .await;
        assert_eq!(
            held.iter().map(|h| h.tool.as_str()).collect::<Vec<_>>(),
            ["run_command", "read_file", "write_file"]
        );
        assert!(
            held[0].reason.contains("load__group__bash"),
            "{}",
            held[0].reason
        );
        assert!(
            groups
                .appended(&ctx(Some("a typo"), Vec::new()))
                .await
                .is_empty()
        );
        let loaded = vec![Loaded {
            loader: "load__group__bash".into(),
            tools: vec!["run_command".into(), "read_file".into()],
            note: "what bash is for".into(),
            note_is_server: false,
        }];
        assert!(
            groups
                .withhold(
                    &ctx(Some("a typo"), loaded.clone()),
                    &["run_command", "read_file"]
                )
                .await
                .is_empty()
        );
        let appended = groups.appended(&ctx(Some("a typo"), loaded)).await;
        assert_eq!(
            appended.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["run_command", "read_file"]
        );
    }

    #[tokio::test]
    async fn a_task_that_holds_a_keyword_keeps_the_groups_tools_in_place() {
        let groups = Groups::bind(
            &origin(),
            vec![group("linear", &["linear__x"]).with_keywords(vec!["linear".into()])],
            &given(&["linear__x"]),
        )
        .unwrap();
        // The group is loaded from the first request, so its tool is not withheld and stands where
        // the agent's own list puts it: it is not appended, and no turn records a growth.
        assert!(
            groups
                .withhold(&ctx(Some("the linear ticket"), Vec::new()), &["linear__x"])
                .await
                .is_empty()
        );
        assert!(
            groups
                .appended(&ctx(Some("the linear ticket"), Vec::new()))
                .await
                .is_empty(),
            "a task-loaded group is not appended: its tools were never withheld"
        );
        // The loader of a group the task loaded is not "still available".
        assert!(
            groups
                .loaders(&ctx(Some("the linear ticket"), Vec::new()))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_removed_tool_is_withheld_and_not_appended() {
        let groups = Groups::bind(
            &origin(),
            vec![group("bash", &["run_command"])],
            &given(&["run_command"]),
        )
        .unwrap();
        let loaded = vec![Loaded {
            loader: "load__group__bash".into(),
            tools: vec!["run_command".into()],
            note: String::new(),
            note_is_server: false,
        }];
        let ctx = ctx(Some("a typo"), loaded).with_removed_tools(vec!["run_command".to_owned()]);
        let held = groups.withhold(&ctx, &["run_command"]).await;
        assert_eq!(held.len(), 1);
        assert!(
            held[0].reason.contains("not available"),
            "{}",
            held[0].reason
        );
        assert!(groups.appended(&ctx).await.is_empty());
    }

    #[tokio::test]
    async fn a_loader_loads_its_group_once_and_says_what_arrived() {
        let groups = Groups::bind(
            &origin(),
            vec![group("bash", &["run_command", "read_file"])],
            &given(&["run_command", "read_file"]),
        )
        .unwrap();
        let loader = groups.loader_tools().pop().unwrap().1;
        let tool_ctx =
            ToolCtx::detached("load__group__bash", "c1", Arc::new(adam_runtime::NoopSink));
        let out = loader.call(&tool_ctx, json!({})).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("run_command, read_file"),
            "{}",
            out.content
        );
        assert!(out.content.contains("what bash is for"), "{}", out.content);
        assert!(out.content.contains("re-reads"), "{}", out.content);
        assert_eq!(out.loaded.len(), 1);
        assert_eq!(out.loaded[0].loader, "load__group__bash");
        assert_eq!(out.loaded[0].note, "what bash is for");

        let again = ToolCtx::detached("load__group__bash", "c2", Arc::new(adam_runtime::NoopSink))
            .with_loaded(out.loaded.clone());
        let refused = loader.call(&again, json!({})).await.unwrap();
        assert!(refused.is_error);
        assert!(
            refused.content.contains("already loaded"),
            "{}",
            refused.content
        );
        assert!(refused.loaded.is_empty());
    }

    #[tokio::test]
    async fn a_loader_for_a_group_the_task_loaded_is_refused() {
        let groups = Groups::bind(
            &origin(),
            vec![group("linear", &["linear__x"]).with_keywords(vec!["linear".into()])],
            &given(&["linear__x"]),
        )
        .unwrap();
        let loader = groups.loader_tools().pop().unwrap().1;
        let ctx = ToolCtx::detached(
            "load__group__linear",
            "c1",
            Arc::new(adam_runtime::NoopSink),
        )
        .with_task(Some(Arc::from("the linear ticket")));
        let out = loader.call(&ctx, json!({})).await.unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("the task mentions"), "{}", out.content);
    }
}
