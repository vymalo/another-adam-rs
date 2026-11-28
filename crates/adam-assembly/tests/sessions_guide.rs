//! The system prompt documents the session tools (ADR 0054 decision 9): a root that has subagents
//! ends its instructions with the *Child sessions* section, after the skills catalog, and one that
//! has none gets nothing. The section is made by `SessionTools::guide_for`, which `adam-llm-agent`
//! holds to the tools' specs.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_llm_agent::SessionTools;
use adam_model::MockModel;
use adam_runtime::SessionLimits;
use common::{def, instructions, tools};

const SECTION: &str = "## Child sessions";

/// The prompt of every agent of the directory `files` makes, root first, bound under `limits`
/// (the deployment's defaults for `None`).
fn prompts(files: &[(&str, &str)], limits: Option<SessionLimits>) -> Vec<String> {
    let mut definition = def(files);
    if let Some(limits) = limits {
        definition = definition.session_limits(limits);
    }
    let assembly = definition
        .bind(tools(&[]))
        .unwrap()
        .model(Arc::new(MockModel::new()), "m")
        .unwrap();
    assembly
        .info()
        .iter()
        .map(|info| info.prompt.clone())
        .collect()
}

fn root(extra: &str) -> String {
    instructions(&format!("name: coder\n{extra}"), "You are the coder.")
}

fn helper(description: &str) -> String {
    instructions(&format!("description: {description}"), "You help.")
}

const SKILL: &str = "---\ndescription: Sorts an incoming issue into a queue.\n---\nDo it.\n";

#[test]
fn a_root_with_subagents_ends_its_prompt_with_the_section_after_the_catalog() {
    let (root, reviewer, explorer) = (root("skills: all"), helper("Reviews."), helper("Reads."));
    let all = prompts(
        &[
            ("agent/instructions.md", &root),
            ("agent/skills/triage.md", SKILL),
            ("agent/subagents/reviewer.md", &reviewer),
            ("agent/subagents/explorer.md", &explorer),
        ],
        None,
    );
    let guide = SessionTools::guide_for(["explorer", "reviewer"]);
    let prompt = &all[0];
    assert!(prompt.starts_with("You are the coder.\n\nThe following skills"));
    assert!(
        prompt.ends_with(&format!("</available_skills>\n\n{guide}")),
        "{prompt}"
    );
    assert_eq!(prompt.matches(SECTION).count(), 1, "{prompt}");
    assert!(
        prompt.contains("`explorer`, `reviewer`."),
        "the helpers, sorted, whatever order the folder lists them in: {prompt}"
    );
    // A subagent has no session tools, so its prompt has no section.
    assert!(
        all[1..].iter().all(|prompt| !prompt.contains(SECTION)),
        "{all:#?}"
    );
}

#[test]
fn a_root_without_subagents_has_no_section() {
    let root = root("");
    let all = prompts(&[("agent/instructions.md", &root)], None);
    assert_eq!(all, ["You are the coder."]);
}

#[test]
fn a_root_with_subagents_and_no_skills_has_the_section_after_its_instructions() {
    let (root, reviewer) = (root(""), helper("Reviews."));
    let all = prompts(
        &[
            ("agent/instructions.md", &root),
            ("agent/subagents/reviewer.md", &reviewer),
        ],
        None,
    );
    assert_eq!(
        all[0],
        format!(
            "You are the coder.\n\n{}",
            SessionTools::guide_for(["reviewer"])
        )
    );
}

#[test]
fn the_section_is_the_same_bytes_under_any_session_limits() {
    let (root, reviewer) = (root(""), helper("Reviews."));
    let files = [
        ("agent/instructions.md", root.as_str()),
        ("agent/subagents/reviewer.md", reviewer.as_str()),
    ];
    let default = prompts(&files, None);
    for (open, working, rounds) in [(1, 1, 2), (3, 2, 5), (20, 8, 99)] {
        let limits = SessionLimits::new(open, working, rounds).unwrap();
        assert_eq!(
            default,
            prompts(&files, Some(limits)),
            "limits {open}/{working}/{rounds}"
        );
    }
}
