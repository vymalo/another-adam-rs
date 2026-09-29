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
    // The root with no `tools:` gets everything; a subagent with none listed gets none.
    assert_eq!(tools_of("coder"), ["a", "b"]);
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
