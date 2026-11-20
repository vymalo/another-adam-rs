//! Which MCP servers of an agent are loaded for a run: the `when` of its `mcp.json`.
//!
//! A server with no `when` is **eager**: its tools are in the agent's own tool list and its
//! instructions are in the agent's prompt, as before the key existed. A server with a `when` is
//! **triggered**: its tools are in the same list but withheld on every turn of a run whose task
//! does not mention one of its keywords, and its instructions are added to the prompt by this
//! source only when it is loaded.
//!
//! The decision is made from the task ([`SourceCtx::task`]), which is the same text at every turn
//! and at every call of a run, so the tools array is the same bytes from the run's first request to
//! its last (ADR 0044, decision 1). It is **recomputed** rather than stored: the run's state already
//! holds the task, and a stored field would be a second place where the same decision lives (a
//! resumed or continued run reads the same first message, so it decides identically).
//!
//! A task that matches nothing is not the end of the run's options: the model may call `use_server`
//! ([`crate::use_server`]) to load one triggered server. That is the fallback, and it costs one full
//! re-prefill: the server is added to the run's state ([`SourceCtx::loaded_servers`]), its
//! instructions are added to the prompt by this source from the next turn on, and its tools are
//! offered by [`ToolSource::appended`] **after every other tool** of the turn, never inserted, so
//! the request before stays a byte prefix of the one after it. The turns where that happened are
//! recorded by the loop (`Conversation::tools_appended`).

use adam_llm_agent::{SourceCtx, ToolCtx, ToolError, ToolOutput, ToolSource, Withheld};
use adam_model::ToolSpec;
use async_trait::async_trait;

/// The most the instructions of the loaded servers may add to a prompt, all together: the bound
/// `adam-mcp` applies to the whole set when it is appended at bind
/// (`adam_mcp::MAX_INSTRUCTIONS_BYTES`, 8 KiB). A `when` splits that set in two, so the eager
/// servers and the loaded triggered ones are each bounded by it. A test with the feature `mcp`
/// holds the two values equal.
const MAX_INSTRUCTIONS_BYTES: usize = 8 * 1024;

/// One server of an agent's `mcp.json` that declares `when`.
#[derive(Clone)]
pub(crate) struct Triggered {
    /// The server's name, as its tools and the prompt's headings name it.
    pub(crate) server: String,
    /// The words as the file wrote them, for the refusal's message.
    pub(crate) keywords: Vec<String>,
    /// The words folded for matching, in the same order.
    folded: Vec<String>,
    /// The tools of this server that **this agent selected**, in the order the binding connected
    /// them: the one table `bind` built, never a name parsed again here, narrowed by the agent's
    /// `tools:`. A server that offered none (an `optional` server that was not reachable, or a
    /// selection that matches nothing) has none.
    pub(crate) specs: Vec<ToolSpec>,
    /// What the server said about itself when it initialized, heading included; `None` when it
    /// said nothing or the file turned it off.
    pub(crate) instructions: Option<String>,
}

impl Triggered {
    pub(crate) fn new(
        server: String,
        keywords: Vec<String>,
        specs: Vec<ToolSpec>,
        instructions: Option<String>,
    ) -> Self {
        let folded = keywords.iter().map(|word| word.to_lowercase()).collect();
        Self {
            server,
            keywords,
            folded,
            specs,
            instructions,
        }
    }

    /// The names of its tools, in the order it connected them.
    pub(crate) fn tools(&self) -> impl Iterator<Item = &str> + '_ {
        self.specs.iter().map(|spec| spec.name.as_str())
    }

    /// Whether it gave the run anything to load: a server that offered no tool cannot be loaded.
    pub(crate) fn available(&self) -> bool {
        !self.specs.is_empty()
    }

    /// Whether a run whose task is `task` loads it from the start: any keyword is held by the task,
    /// ignoring case. No task (a message with no words) matches nothing.
    pub(crate) fn matches(&self, task: Option<&str>) -> bool {
        match task {
            Some(task) => {
                let task = task.to_lowercase();
                self.folded
                    .iter()
                    .any(|word| !word.is_empty() && task.contains(word.as_str()))
            }
            None => false,
        }
    }
}

/// The triggered servers of one agent, and what a run's task loads of them.
pub(crate) struct McpSelect {
    servers: Vec<Triggered>,
}

impl McpSelect {
    pub(crate) fn new(servers: Vec<Triggered>) -> Self {
        Self { servers }
    }

    /// Whether each server is loaded for this turn: the task matched its keywords, or the run
    /// loaded it during itself ([`SourceCtx::loaded_servers`]).
    fn loaded(&self, ctx: &SourceCtx) -> Vec<bool> {
        let during = ctx.loaded_servers();
        self.servers
            .iter()
            .map(|server| {
                server.matches(ctx.task()) || during.iter().any(|name| name == &server.server)
            })
            .collect()
    }
}

/// What the model is told when it calls a tool of a server that was not loaded.
fn reason(server: &Triggered, tool: &str) -> String {
    let words: Vec<&str> = server.keywords.iter().take(8).map(String::as_str).collect();
    let more = server.keywords.len().saturating_sub(words.len());
    let list = match more {
        0 => words.join(", "),
        n => format!("{} and {n} more", words.join(", ")),
    };
    format!(
        "`{tool}` is not loaded for this run: the MCP server `{}` loads only when the task mentions \
         one of its keywords ({list})",
        server.server
    )
}

/// `sections` (a server's name and the section `adam_mcp::McpServers::instructions_by_server` gives
/// it) as one text: blank-line separated, in order, cut at [`MAX_INSTRUCTIONS_BYTES`] together (the
/// server that would pass it and the ones after are left out, with a warning).
pub(crate) fn join_sections(sections: &[(String, String)]) -> String {
    let mut out = String::new();
    for (server, section) in sections {
        let separator = if out.is_empty() { "" } else { "\n\n" };
        if out.len() + separator.len() + section.len() > MAX_INSTRUCTIONS_BYTES {
            tracing::warn!(
                server = %server,
                "the MCP servers' instructions are over {MAX_INSTRUCTIONS_BYTES} bytes together: \
                 this server's and the next ones' are left out"
            );
            break;
        }
        out.push_str(separator);
        out.push_str(section);
    }
    out
}

#[async_trait]
impl ToolSource for McpSelect {
    async fn specs(&self, _ctx: &SourceCtx) -> Vec<ToolSpec> {
        // The tools are the agent's own (`bind` put them there); this source only decides which of
        // them a run may see.
        Vec::new()
    }

    async fn instructions(&self, ctx: &SourceCtx) -> Option<String> {
        let loaded = self.loaded(ctx);
        let sections: Vec<(String, String)> = self
            .servers
            .iter()
            .zip(loaded)
            .filter(|(_, loaded)| *loaded)
            .filter_map(|(server, _)| {
                server
                    .instructions
                    .as_ref()
                    .map(|text| (server.server.clone(), text.clone()))
            })
            .collect();
        let text = join_sections(&sections);
        (!text.is_empty()).then_some(text)
    }

    async fn withhold(&self, ctx: &SourceCtx, tools: &[&str]) -> Vec<Withheld> {
        let loaded = self.loaded(ctx);
        let mut held = Vec::new();
        for (server, loaded) in self.servers.iter().zip(loaded) {
            if loaded {
                continue;
            }
            for tool in server.tools() {
                if tools.contains(&tool) {
                    held.push(Withheld::new(tool, reason(server, tool)));
                }
            }
        }
        held
    }

    /// The tools of the servers the run loaded during itself, in the order it loaded them: the
    /// loop puts them after every other tool of the turn, so the array grows by a suffix and the
    /// request before stays a byte prefix of the one after it.
    async fn appended(&self, ctx: &SourceCtx) -> Vec<ToolSpec> {
        let mut out = Vec::new();
        for name in ctx.loaded_servers() {
            if let Some(server) = self.servers.iter().find(|server| &server.server == name) {
                out.extend(server.specs.iter().cloned());
            }
        }
        out
    }

    async fn call(
        &self,
        _ctx: &ToolCtx,
        _name: &str,
        _args: serde_json::Value,
    ) -> Option<Result<ToolOutput, ToolError>> {
        // Every tool is an own tool of the agent: the agent calls it, not this source.
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value};

    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: format!("the {name} tool"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn triggered(
        server: &str,
        keywords: &[&str],
        tools: &[&str],
        instructions: Option<&str>,
    ) -> Triggered {
        Triggered::new(
            server.to_owned(),
            keywords.iter().map(|word| (*word).to_owned()).collect(),
            tools.iter().map(|tool| spec(tool)).collect(),
            instructions.map(str::to_owned),
        )
    }

    fn source() -> McpSelect {
        McpSelect::new(vec![
            triggered(
                "linear",
                &["linear", "ticket"],
                &["linear__list_issues"],
                Some("## Linear\n\n> linear words"),
            ),
            triggered(
                "docs",
                &["docs"],
                &["docs__search", "docs__read"],
                Some("## Docs\n\n> docs words"),
            ),
        ])
    }

    fn ctx(task: Option<&str>) -> SourceCtx {
        SourceCtx::detached(Map::<String, Value>::new()).with_task(task)
    }

    /// A context of a run that has loaded `loaded` during itself.
    fn ctx_loaded(task: Option<&str>, loaded: &[&str]) -> SourceCtx {
        ctx(task).with_loaded_servers(
            loaded
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn a_keyword_loads_the_server_and_the_others_are_withheld() {
        let source = source();
        let held = source
            .withhold(
                &ctx(Some("fix the linear ticket")),
                &["linear__list_issues", "docs__search"],
            )
            .await;
        assert_eq!(
            held.iter().map(|h| h.tool.as_str()).collect::<Vec<_>>(),
            ["docs__search"],
            "only the server whose keyword did not match is held back"
        );
        assert!(held[0].reason.contains("not loaded for this run"));
        assert!(held[0].reason.contains("docs"));
    }

    #[tokio::test]
    async fn the_match_ignores_case_and_an_empty_task_loads_nothing() {
        let source = source();
        let held = source
            .withhold(&ctx(Some("LINEAR")), &["linear__list_issues"])
            .await;
        assert!(held.is_empty(), "the keyword matches whatever its case");
        let held = source
            .withhold(&ctx(Some("")), &["linear__list_issues"])
            .await;
        assert_eq!(held.len(), 1, "an empty task loads no triggered server");
        let held = source.withhold(&ctx(None), &["linear__list_issues"]).await;
        assert_eq!(held.len(), 1, "a message with no words loads none either");
    }

    #[tokio::test]
    async fn the_instructions_of_a_loaded_server_only() {
        let source = source();
        let text = source
            .instructions(&ctx(Some("read the docs")))
            .await
            .expect("the loaded server said something");
        assert!(text.contains("docs words"), "{text}");
        assert!(!text.contains("linear words"), "{text}");
        assert!(
            source.instructions(&ctx(Some("a typo"))).await.is_none(),
            "nothing loaded, nothing added"
        );
    }

    #[test]
    fn a_reason_names_the_tool_the_server_and_its_keywords() {
        let server = triggered("linear", &["linear", "ticket"], &["linear__x"], None);
        let text = reason(&server, "linear__x");
        assert!(
            text.starts_with("`linear__x` is not loaded for this run"),
            "{text}"
        );
        assert!(text.contains("`linear`"), "{text}");
        assert!(text.contains("linear, ticket"), "{text}");
    }

    /// The keyword is folded too: a file may write it in any case.
    #[tokio::test]
    async fn a_keyword_written_in_capitals_still_matches() {
        let source = McpSelect::new(vec![triggered(
            "linear",
            &["LINEAR"],
            &["linear__list_issues"],
            None,
        )]);
        assert!(
            source
                .withhold(
                    &ctx(Some("fix the linear ticket")),
                    &["linear__list_issues"]
                )
                .await
                .is_empty(),
            "the file's keyword is folded before the match"
        );
    }

    /// The match is a case-insensitive substring, as the reference page says: `docs` matches a
    /// task that says `docstring`. This pins the documented behaviour, not a word boundary.
    #[tokio::test]
    async fn a_keyword_matches_inside_a_longer_word_as_documented() {
        let source = source();
        let held = source
            .withhold(&ctx(Some("parse the docstring")), &["docs__search"])
            .await;
        assert!(
            held.is_empty(),
            "a substring match loads `docs`, and the reference page says so"
        );
        let held = source
            .withhold(&ctx(Some("parse the docstring")), &["linear__list_issues"])
            .await;
        assert_eq!(
            held.len(),
            1,
            "a word the task does not hold stays withheld"
        );
    }

    /// The refusal is bounded, and says how many keywords it did not list.
    #[test]
    fn a_long_keyword_list_is_summarized_in_the_refusal() {
        let keywords: Vec<String> = (0..16).map(|n| format!("k{n}")).collect();
        let words: Vec<&str> = keywords.iter().map(String::as_str).collect();
        let server = triggered("x", &words, &["x__t"], None);
        let text = reason(&server, "x__t");
        assert!(
            text.contains("k0, k1, k2, k3, k4, k5, k6, k7 and 8 more"),
            "{text}"
        );
    }

    /// The loaded set is bounded like the eager set: the section that would pass the bound and the
    /// ones after it are left out, so a `when` cannot add more than `adam-mcp` would at bind.
    #[test]
    fn the_loaded_sections_are_bounded() {
        let half = MAX_INSTRUCTIONS_BYTES / 2;
        let sections = vec![
            ("a".to_owned(), "a".repeat(half)),
            ("b".to_owned(), "b".repeat(half + 1)),
            ("c".to_owned(), "c".to_owned()),
        ];
        let text = join_sections(&sections);
        assert!(text.len() <= MAX_INSTRUCTIONS_BYTES, "{}", text.len());
        assert!(text.starts_with('a'), "the first section fits");
        assert!(
            !text.contains('b'),
            "the section that would pass is left out"
        );
        assert!(!text.contains('c'), "and the ones after it");
    }

    /// A loaded server that said nothing adds nothing, and one that is not loaded adds nothing
    /// even when it did.
    #[tokio::test]
    async fn instructions_follow_the_loaded_set_only() {
        let source = McpSelect::new(vec![
            triggered("quiet", &["quiet"], &["quiet__x"], None),
            triggered(
                "loud",
                &["loud"],
                &["loud__x"],
                Some("## Loud\n\n> loud words"),
            ),
        ]);
        assert!(
            source.instructions(&ctx(Some("be quiet"))).await.is_none(),
            "the only loaded server said nothing, so nothing is added"
        );
        let text = source
            .instructions(&ctx(Some("be loud")))
            .await
            .expect("the loaded server said something");
        assert!(text.contains("loud words"), "{text}");
        assert!(
            !text.contains("quiet"),
            "the server that was not loaded is not in it: {text}"
        );
        assert!(
            source.instructions(&ctx(Some("neither"))).await.is_none(),
            "nothing loaded, nothing added"
        );
    }

    /// A server the run loaded during itself stops being withheld, and its tools are what the
    /// source appends: the loop puts them after every other tool of the turn.
    #[tokio::test]
    async fn a_server_the_run_loaded_is_not_withheld_and_is_appended() {
        let source = source();
        let ctx = ctx_loaded(Some("fix a typo"), &["docs"]);
        let held = source
            .withhold(&ctx, &["linear__list_issues", "docs__search", "docs__read"])
            .await;
        assert_eq!(
            held.iter().map(|h| h.tool.as_str()).collect::<Vec<_>>(),
            ["linear__list_issues"],
            "only the server the run did not load is held back"
        );
        let appended = source.appended(&ctx).await;
        assert_eq!(
            appended
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["docs__search", "docs__read"],
            "the loaded server's tools, in the order it connected them"
        );
        assert!(
            source.appended(&ctx_loaded(None, &[])).await.is_empty(),
            "nothing loaded, nothing appended"
        );
    }

    /// The order is the run's, not the file's: a second load follows the first.
    #[tokio::test]
    async fn the_appended_tools_follow_the_order_the_run_loaded_them() {
        let source = source();
        let appended = source
            .appended(&ctx_loaded(None, &["docs", "linear"]))
            .await;
        assert_eq!(
            appended
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["docs__search", "docs__read", "linear__list_issues"]
        );
    }

    /// A loaded server's instructions reach the prompt, as they do when the task loads it.
    #[tokio::test]
    async fn the_instructions_of_a_server_the_run_loaded() {
        let source = source();
        let text = source
            .instructions(&ctx_loaded(Some("fix a typo"), &["docs"]))
            .await
            .expect("the loaded server said something");
        assert!(text.contains("docs words"), "{text}");
        assert!(!text.contains("linear words"), "{text}");
    }

    /// A server with no tools has nothing to append, even if a state names it.
    #[tokio::test]
    async fn a_server_that_offered_no_tools_appends_nothing() {
        let source = McpSelect::new(vec![triggered("quiet", &["quiet"], &[], None)]);
        assert!(
            source
                .appended(&ctx_loaded(None, &["quiet"]))
                .await
                .is_empty()
        );
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn the_instruction_bound_is_the_one_adam_mcp_uses() {
        assert_eq!(MAX_INSTRUCTIONS_BYTES, adam_mcp::MAX_INSTRUCTIONS_BYTES);
    }
}
