//! Binding: tools, templating and the errors, each with the text a person reads.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use adam_assembly::{AgentDef, Error, TemplateProblem};
use common::{def, instructions, tools};

const INSTRUCTIONS: &str = "agent/instructions.md";

fn one(frontmatter: &str, body: &str) -> AgentDef {
    def(&[(INSTRUCTIONS, &instructions(frontmatter, body))])
}

// --- tools ---------------------------------------------------------------------------------

#[test]
fn an_unknown_tool_fails_at_bind_with_a_suggestion() {
    let error = one("name: coder\ntools: [run_check]", "Hi.")
        .bind(tools(&["prepare_workspace", "run_checks", "ask_user"]))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): `tools` names `run_check`, which is not a \
         registered tool; did you mean `run_checks`?; registered: `prepare_workspace`, \
         `run_checks`, `ask_user`"
    );
    let Error::UnknownTool {
        origin,
        tool,
        suggestion,
        available,
    } = error
    else {
        panic!("wrong variant");
    };
    assert_eq!(origin.agent, "coder");
    assert_eq!(origin.file.to_string_lossy(), INSTRUCTIONS);
    assert_eq!(tool, "run_check");
    assert_eq!(suggestion.as_deref(), Some("run_checks"));
    assert_eq!(available, ["prepare_workspace", "run_checks", "ask_user"]);
}

#[test]
fn a_claude_code_tool_name_gets_the_closest_adam_tool() {
    // `Read` is a Claude Code tool; adam fails closed and says which of its own is near.
    let error = one("name: coder\ntools: Read, Grep", "Hi.")
        .bind(tools(&["read_diff", "list_files"]))
        .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("`tools` names `Read`"), "{text}");
    assert!(text.contains("did you mean `read_diff`?"), "{text}");
}

#[test]
fn an_unknown_tool_without_a_near_name_lists_what_exists() {
    let error = one("name: coder\ntools: [zzz]", "Hi.")
        .bind(tools(&["a_tool"]))
        .unwrap_err();
    let text = error.to_string();
    assert!(!text.contains("did you mean"), "{text}");
    assert!(text.ends_with("registered: `a_tool`"), "{text}");
    let none = one("name: coder\ntools: [zzz]", "Hi.")
        .bind(tools(&[]))
        .unwrap_err();
    assert!(none.to_string().ends_with("registered: none"), "{none}");
}

#[test]
fn a_subagent_error_names_the_subagent_and_its_file() {
    let error = def(&[
        (INSTRUCTIONS, &instructions("name: coder", "Hi.")),
        (
            "agent/subagents/reviewer.md",
            &instructions("description: Reviews.\ntools: [read_dif]", "Review."),
        ),
    ])
    .bind(tools(&["read_diff"]))
    .unwrap_err();
    let Error::UnknownTool {
        origin, suggestion, ..
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder/reviewer");
    assert_eq!(origin.file.to_string_lossy(), "agent/subagents/reviewer.md");
    assert_eq!(suggestion.as_deref(), Some("read_diff"));
}

#[test]
fn a_pattern_takes_the_tools_it_matches_and_fails_when_it_matches_none() {
    let bound = one("name: coder\ntools: ['linear__*', ask_user]", "Hi.")
        .bind(tools(&[
            "ask_user",
            "linear__list",
            "github__pr",
            "linear__get",
        ]))
        .unwrap();
    let assembly = bound
        .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
        .unwrap();
    // The file's order: the pattern's matches (in registration order), then `ask_user`.
    assert_eq!(
        assembly.info()[0].tools,
        ["linear__list", "linear__get", "ask_user"]
    );

    let error = one("name: coder\ntools: ['linear__*']", "Hi.")
        .bind(tools(&["github__pr"]))
        .unwrap_err();
    assert!(
        matches!(&error, Error::NoToolMatches { pattern, .. } if pattern == "linear__*"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("the `tools` pattern `linear__*` matches no registered tool"),
        "{error}"
    );
}

#[test]
fn a_tool_listed_twice_is_bound_once() {
    let assembly = one("name: coder\ntools: [b, a, 'a*', b]", "Hi.")
        .bind(tools(&["a", "b", "c"]))
        .unwrap()
        .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
        .unwrap();
    assert_eq!(assembly.info()[0].tools, ["b", "a"]);
}

#[test]
fn default_tool_access_follows_the_owner_decision_d3() {
    let assembly = def(&[
        (INSTRUCTIONS, &instructions("name: coder", "Hi.")),
        (
            "agent/subagents/quiet.md",
            &instructions("description: No tools listed.", "Quiet."),
        ),
        (
            "agent/subagents/all.md",
            &instructions("description: All tools.\ntools: '*'", "All."),
        ),
        (
            "agent/subagents/none.md",
            &instructions("description: No tools.\ntools: []", "None."),
        ),
    ])
    .bind(tools(&["a", "b"]))
    .unwrap()
    .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
    .unwrap();
    let tools_of = |name: &str| {
        assembly
            .info()
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .tools
            .clone()
    };
    // The root with no `tools:` gets everything, then a tool for each subagent; a subagent with
    // none listed gets none (and not the parent's, nor the parent's subagent tools).
    assert_eq!(tools_of("coder"), ["a", "b", "all", "none", "quiet"]);
    assert!(tools_of("coder/quiet").is_empty());
    assert_eq!(tools_of("coder/all"), ["a", "b"]);
    assert!(tools_of("coder/none").is_empty());
}

#[test]
fn two_tools_with_one_name_are_refused() {
    let set = tools(&["a", "b"]).extend(tools(&["a"]));
    let error = one("name: coder", "Hi.").bind(set).unwrap_err();
    assert!(
        matches!(&error, Error::DuplicateTool { tool } if tool == "a"),
        "{error}"
    );
}

// --- templating ----------------------------------------------------------------------------

#[test]
fn vars_are_substituted_and_the_code_can_override_a_default() {
    let files = instructions(
        "name: coder\nvars: { cycles: 3, mode: plain }",
        "Stop after {{cycles}} cycles in {{ mode }} mode. Again: {{cycles}}.",
    );
    let render = |d: AgentDef| {
        d.bind(tools(&[]))
            .unwrap()
            .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
            .unwrap()
            .info()[0]
            .prompt
            .clone()
    };
    let base = def(&[(INSTRUCTIONS, &files)]);
    assert_eq!(
        render(base.clone()),
        "Stop after 3 cycles in plain mode. Again: 3."
    );
    // Any `ToString` works as a value.
    assert_eq!(
        render(base.var("cycles", 7).var("mode", String::from("strict"))),
        "Stop after 7 cycles in strict mode. Again: 7."
    );
}

#[test]
fn an_unknown_var_fails_at_bind_with_a_suggestion() {
    let error = one(
        "name: coder\nvars: { max_check_cycles: 3 }",
        "One line.\nStop after {{max_check_cycle}} cycles.",
    )
    .bind(tools(&[]))
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): prompt line 2: uses `{{max_check_cycle}}`, which \
         `vars` does not declare; did you mean `max_check_cycles`?; declared: `max_check_cycles`"
    );
    assert!(
        matches!(&error, Error::UnknownVar { line: 2, var, .. } if var == "max_check_cycle"),
        "{error}"
    );
}

#[test]
fn a_placeholder_with_no_vars_at_all_says_none_are_declared() {
    let error = one("name: coder", "Use {{thing}}.")
        .bind(tools(&[]))
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("uses `{{thing}}`, which `vars` does not declare"),
        "{text}"
    );
    assert!(text.ends_with("declared: none"), "{text}");
}

#[test]
fn an_unknown_var_in_an_instructions_part_names_that_file() {
    let error = def(&[
        (
            INSTRUCTIONS,
            &instructions("name: coder\nvars: { a: 1 }", "{{a}}"),
        ),
        ("agent/instructions/10-style.md", "Style\n\n{{b}}\n"),
    ])
    .bind(tools(&[]))
    .unwrap_err();
    let Error::UnknownVar { origin, line, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(
        origin.file.to_string_lossy(),
        "agent/instructions/10-style.md"
    );
    assert_eq!(*line, 3);
    // A var used only in a part still counts as used.
    let ok = def(&[
        (
            INSTRUCTIONS,
            &instructions("name: coder\nvars: { a: 1 }", "Body."),
        ),
        ("agent/instructions/10-style.md", "Style {{a}}\n"),
    ])
    .bind(tools(&[]))
    .unwrap()
    .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
    .unwrap();
    assert_eq!(ok.info()[0].prompt, "Body.\n\nStyle 1");
}

#[test]
fn an_unused_var_fails_at_bind() {
    let error = one(
        "name: coder\nvars: { cycles: 3, strict: true }",
        "Stop after {{cycles}}.",
    )
    .bind(tools(&[]))
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): var `strict` is declared under `vars` but the \
         prompt never uses `{{strict}}`: use it or remove it"
    );
    assert!(matches!(&error, Error::UnusedVar { var, .. } if var == "strict"));
    // Supplying a value does not make an unused var used.
    let supplied = one("name: coder\nvars: { strict: true }", "No placeholder.")
        .var("strict", "false")
        .bind(tools(&[]))
        .unwrap_err();
    assert!(matches!(supplied, Error::UnusedVar { .. }));
}

#[test]
fn a_var_declared_without_a_value_must_be_supplied() {
    let files = instructions("name: coder\nvars:\n  repo:", "Work in {{repo}}.");
    let error = def(&[(INSTRUCTIONS, &files)]).bind(tools(&[])).unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): var `repo` has no value: give it a default under \
         `vars`, or supply one with `AgentDef::var`"
    );
    assert!(matches!(&error, Error::UnsetVar { var, .. } if var == "repo"));

    let prompt = def(&[(INSTRUCTIONS, &files)])
        .var("repo", "acme/widgets")
        .bind(tools(&[]))
        .unwrap()
        .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
        .unwrap()
        .info()[0]
        .prompt
        .clone();
    assert_eq!(prompt, "Work in acme/widgets.");
}

#[test]
fn a_value_for_an_undeclared_var_is_refused_with_a_suggestion() {
    let error = one("name: coder\nvars: { cycles: 3 }", "{{cycles}}")
        .var("cyles", 5)
        .bind(tools(&[]))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "agent `coder` (agent/instructions.md): a value was supplied for var `cyles`, which \
         `vars` does not declare; did you mean `cycles`?; declared: `cycles`"
    );
}

#[test]
fn values_for_a_subagent_go_by_its_registered_name() {
    let files = [
        (INSTRUCTIONS, instructions("name: coder", "Root.")),
        (
            "agent/subagents/reviewer.md",
            instructions(
                "description: Reviews.\nvars: { focus: }",
                "Focus on {{focus}}.",
            ),
        ),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    let error = def(&files).bind(tools(&[])).unwrap_err();
    let Error::UnsetVar { origin, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder/reviewer");

    let assembly = def(&files)
        .agent_var("coder/reviewer", "focus", "security")
        .bind(tools(&[]))
        .unwrap()
        .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
        .unwrap();
    assert_eq!(assembly.info()[1].prompt, "Focus on security.");
}

#[test]
fn values_for_an_agent_that_does_not_exist_are_refused() {
    let files = [
        (INSTRUCTIONS, instructions("name: coder", "Root.")),
        (
            "agent/subagents/reviewer.md",
            instructions("description: Reviews.", "Review."),
        ),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    let error = def(&files)
        .agent_var("coder/reviwer", "x", "1")
        .bind(tools(&[]))
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "no agent `coder/reviwer` to set vars on; did you mean `coder/reviewer`?; agents: \
         `coder`, `coder/reviewer`"
    );
}

#[test]
fn a_brace_that_is_not_a_placeholder_is_a_syntax_error_with_its_line() {
    let error = one("name: coder", "Line one.\nLine two {{oops\nmore")
        .bind(tools(&[]))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Template {
                line: 2,
                problem: TemplateProblem::Unclosed,
                ..
            }
        ),
        "{error}"
    );
    assert!(
        error.to_string().starts_with(
            "agent `coder` (agent/instructions.md): prompt line 2: `{{` is never closed by `}}`"
        ),
        "{error}"
    );
    let bad = one("name: coder", "{{ not a var }}")
        .bind(tools(&[]))
        .unwrap_err();
    assert!(
        matches!(&bad, Error::Template { problem: TemplateProblem::BadName(n), .. } if n == "not a var"),
        "{bad}"
    );
}

#[test]
fn four_braces_write_a_literal_pair() {
    let assembly = one("name: coder", "Write {{{{name}} for a placeholder.")
        .bind(tools(&[]))
        .unwrap()
        .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
        .unwrap();
    assert_eq!(
        assembly.info()[0].prompt,
        "Write {{name}} for a placeholder."
    );
}

// --- MCP tools (`mcp.json`) ----------------------------------------------------------------
//
// Tools made by hand stand in for a connected server here: these tests are about what `bind`
// does with them, and run without the feature `mcp`. `tests/mcp.rs` connects for real.

const LINEAR_JSON: &str = r#"{"mcpServers": {
    "linear": {"type": "http", "url": "https://mcp.example.com/mcp"},
    "fs": {"command": "mcp-server-filesystem", "args": ["/work"]}
}}"#;
const SEARCH_JSON: &str =
    r#"{"mcpServers": {"search": {"type": "http", "url": "https://search.example.com/mcp"}}}"#;

/// A root with an `mcp.json`, and a subagent directory `researcher` with its own.
fn with_mcp(root_frontmatter: &str, researcher_frontmatter: &str) -> AgentDef {
    def(&[
        (INSTRUCTIONS, &instructions(root_frontmatter, "Hi.")),
        ("agent/mcp.json", LINEAR_JSON),
        (
            "agent/subagents/researcher/instructions.md",
            &instructions(researcher_frontmatter, "Research."),
        ),
        ("agent/subagents/researcher/mcp.json", SEARCH_JSON),
    ])
}

fn mcp(names: &[&str]) -> adam_llm_agent::ToolSet {
    tools(names)
}

fn info_of(def: AgentDef, registered: &[&str]) -> Vec<(String, Vec<String>)> {
    def.bind(tools(registered))
        .unwrap()
        .model(std::sync::Arc::new(adam_model::MockModel::new()), "m")
        .unwrap()
        .info()
        .iter()
        .map(|i| (i.name.clone(), i.tools.clone()))
        .collect()
}

#[test]
fn mcp_json_without_connections_fails_closed() {
    let error = with_mcp("name: coder", "description: Researches.")
        .bind(tools(&["ask_user"]))
        .unwrap_err();
    let Error::McpNotConnected { origin, servers } = &error else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder");
    assert_eq!(origin.file.to_string_lossy(), "agent/mcp.json");
    assert_eq!(servers, &["fs", "linear"]);
    let text = error.to_string();
    assert!(
        text.contains("`mcp.json` lists the MCP servers `fs`, `linear`"),
        "{text}"
    );
    assert!(text.contains("AgentDef::connect_mcp"), "{text}");
    assert!(text.contains("AgentDef::mcp_tools"), "{text}");
    // Without the feature the message says to turn it on.
    assert_eq!(
        text.contains("enable the feature `mcp`"),
        !cfg!(feature = "mcp"),
        "{text}"
    );

    // A subagent's own `mcp.json` counts too: the root being connected is not enough.
    let error = with_mcp("name: coder", "description: Researches.")
        .mcp_tools("coder", mcp(&["linear__list"]))
        .bind(tools(&[]))
        .unwrap_err();
    let Error::McpNotConnected { origin, servers } = &error else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder/researcher");
    assert_eq!(
        origin.file.to_string_lossy(),
        "agent/subagents/researcher/mcp.json"
    );
    assert_eq!(servers, &["search"]);

    // An `mcp.json` with no servers is no file.
    def(&[
        (INSTRUCTIONS, &instructions("name: coder", "Hi.")),
        ("agent/mcp.json", r#"{"mcpServers": {}}"#),
    ])
    .bind(tools(&[]))
    .unwrap();
}

#[test]
fn hand_supplied_mcp_tools_bind_like_connected() {
    // `tools:` selects among the registered tools and the agent's own MCP tools, by name or by
    // pattern, in the order the file lists them.
    let bound = info_of(
        with_mcp(
            "name: coder\ntools: ['linear__*', ask_user]",
            "description: Researches.",
        )
        .mcp_tools("coder", mcp(&["linear__list", "linear__get"]))
        .mcp_tools("coder/researcher", mcp(&["search__web"])),
        &["ask_user", "run_checks"],
    );
    assert_eq!(
        bound,
        [
            (
                "coder".to_owned(),
                vec![
                    "linear__list".to_owned(),
                    "linear__get".to_owned(),
                    "ask_user".to_owned(),
                    // The tool that calls the subagent, after the agent's own tools.
                    "researcher".to_owned()
                ]
            ),
            // A subagent inherits nothing, and lists no tools: its own MCP tools are not selected.
            ("coder/researcher".to_owned(), vec![]),
        ]
    );

    // A root without `tools:` gets everything registered and its own MCP tools; a subagent lists
    // the MCP tools it wants, as it lists any other.
    let bound = info_of(
        with_mcp(
            "name: coder",
            "description: Researches.\ntools: [search__web]",
        )
        .mcp_tools("coder", mcp(&["linear__list"]))
        .mcp_tools("coder/researcher", mcp(&["search__web", "search__news"])),
        &["ask_user"],
    );
    assert_eq!(
        bound,
        [
            (
                "coder".to_owned(),
                vec![
                    "ask_user".to_owned(),
                    "linear__list".to_owned(),
                    "researcher".to_owned()
                ]
            ),
            (
                "coder/researcher".to_owned(),
                vec!["search__web".to_owned()]
            ),
        ]
    );

    // A pattern that matches none of them is the usual error, and lists the MCP tools too.
    let error = with_mcp("name: coder\ntools: ['github__*']", "description: R.")
        .mcp_tools("coder", mcp(&["linear__list"]))
        .mcp_tools("coder/researcher", mcp(&[]))
        .bind(tools(&["ask_user"]))
        .unwrap_err();
    assert!(
        matches!(&error, Error::NoToolMatches { pattern, available, .. }
            if pattern == "github__*" && available == &["ask_user", "linear__list"]),
        "{error}"
    );
    // A misspelt MCP tool gets a suggestion, like any other.
    let error = with_mcp("name: coder\ntools: [linear__lst]", "description: R.")
        .mcp_tools("coder", mcp(&["linear__list"]))
        .mcp_tools("coder/researcher", mcp(&[]))
        .bind(tools(&[]))
        .unwrap_err();
    assert!(
        error.to_string().contains("did you mean `linear__list`?"),
        "{error}"
    );
}

#[test]
fn foreign_mcp_tool_name_refused() {
    let bind = |root: &[&str], researcher: &[&str]| {
        with_mcp("name: coder", "description: Researches.")
            .mcp_tools("coder", mcp(root))
            .mcp_tools("coder/researcher", mcp(researcher))
            .bind(tools(&[]))
            .unwrap_err()
    };
    // Not named after any server of the file...
    let error = bind(&["github__pr"], &[]);
    let Error::McpForeignTool {
        origin,
        tool,
        servers,
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(
        (origin.agent.as_str(), tool.as_str()),
        ("coder", "github__pr")
    );
    assert_eq!(servers, &["fs", "linear"]);
    assert!(error.to_string().contains("`<server>__<tool>`"), "{error}");
    // ...a single underscore is not the separator...
    assert!(matches!(
        bind(&["linear_list"], &[]),
        Error::McpForeignTool { .. }
    ));
    // ...and a server of another agent's file is not this agent's: no sharing between directories.
    let error = bind(&["linear__list"], &["linear__list"]);
    assert!(
        matches!(&error, Error::McpForeignTool { origin, .. } if origin.agent == "coder/researcher"),
        "{error}"
    );
    // The same tool twice is one name too many.
    assert!(matches!(
        bind(&["linear__list", "linear__list"], &[]),
        Error::DuplicateTool { .. }
    ));
    // Tools for an agent that does not exist: a typo, with a suggestion.
    let error = with_mcp("name: coder", "description: R.")
        .mcp_tools("coder/researchr", mcp(&[]))
        .bind(tools(&[]))
        .unwrap_err();
    assert!(
        matches!(&error, Error::McpUnknownAgent { suggestion, .. }
            if suggestion.as_deref() == Some("coder/researcher")),
        "{error}"
    );
    // An agent with no `mcp.json` has no servers: any tool given to it is foreign.
    let error = one("name: coder", "Hi.")
        .mcp_tools("coder", mcp(&["linear__list"]))
        .bind(tools(&[]))
        .unwrap_err();
    assert!(matches!(error, Error::McpForeignTool { .. }));
}

#[test]
fn mcp_tool_named_like_registered_is_clash() {
    let error = with_mcp("name: coder", "description: Researches.")
        .mcp_tools("coder", mcp(&["linear__list"]))
        .mcp_tools("coder/researcher", mcp(&[]))
        .bind(tools(&["linear__list"]))
        .unwrap_err();
    let Error::McpToolClash { origin, tool } = &error else {
        panic!("{error}");
    };
    assert_eq!(origin.file.to_string_lossy(), "agent/mcp.json");
    assert_eq!(tool, "linear__list");
    assert!(
        error
            .to_string()
            .contains("has the name of a registered tool"),
        "{error}"
    );
}

#[test]
fn subagent_named_like_mcp_tool_is_clash() {
    let files = |root: &str| {
        def(&[
            (INSTRUCTIONS, &instructions(root, "Hi.")),
            ("agent/mcp.json", LINEAR_JSON),
            (
                "agent/subagents/linear__list.md",
                &instructions("description: Lists.", "List."),
            ),
        ])
    };
    let error = files("name: coder\ntools: ['linear__*']")
        .mcp_tools("coder", mcp(&["linear__list"]))
        .bind(tools(&[]))
        .unwrap_err();
    let Error::SubagentToolClash {
        origin,
        parent,
        tool,
        clash,
    } = &error
    else {
        panic!("{error}");
    };
    assert_eq!(origin.agent, "coder/linear__list");
    assert_eq!((parent.as_str(), tool.as_str()), ("coder", "linear__list"));
    assert_eq!(
        clash,
        &adam_assembly::ToolClash::McpTool {
            server: "linear".into()
        }
    );
    assert!(
        error
            .to_string()
            .contains("the parent's MCP server `linear`"),
        "{error}"
    );
    // As with any tool: one the parent does not select is no clash.
    files("name: coder\ntools: [ask_user]")
        .mcp_tools("coder", mcp(&["linear__list"]))
        .bind(tools(&["ask_user"]))
        .unwrap();
}

#[test]
fn a_definition_without_mcp_json_is_untouched() {
    // No `mcp.json`, nothing given: binds as before, and `mcp_tools` for no agent is not needed.
    let bound = info_of(one("name: coder", "Hi."), &["a"]);
    assert_eq!(bound, [("coder".to_owned(), vec!["a".to_owned()])]);
}
