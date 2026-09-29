//! Files that other tools wrote must read here without an error.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::{Path, PathBuf};

use adam_agent_fs::{
    Dir, ManifestSource, ModelRef, Severity, SkillLayout, Subagent, ToolList, parse_skill,
};

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn repo_skills() -> PathBuf {
    manifest_dir().join("../../.agents/skills")
}

/// The 75 vendored skills of this repository, written by other people for other clients.
#[test]
fn every_vendored_skill_parses_with_no_error() {
    let mut parsed = 0_usize;
    let mut problems = Vec::new();
    let mut warnings = 0_usize;
    for entry in std::fs::read_dir(repo_skills()).unwrap() {
        let entry = entry.unwrap();
        let file = entry.path().join("SKILL.md");
        if !file.is_file() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&file).unwrap();
        let mut diagnostics = Vec::new();
        let skill = parse_skill(
            Path::new(&dir_name),
            &text,
            &dir_name,
            SkillLayout::Directory,
            &mut diagnostics,
        );
        problems.extend(
            diagnostics
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .map(ToString::to_string),
        );
        warnings += diagnostics.len();
        match skill {
            Some(s) => {
                assert_eq!(s.name, dir_name);
                assert!(!s.description.is_empty(), "{dir_name}");
                assert!(!s.body.is_empty(), "{dir_name} has no body");
                parsed += 1;
            }
            None => problems.push(format!("{dir_name}: skipped")),
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    assert!(
        parsed >= 75,
        "expected the 75 vendored skills, found {parsed}"
    );
    eprintln!("{parsed} skills parsed, {warnings} warnings");
}

/// The same corpus through discovery, as the skills of an agent.
#[cfg(unix)]
#[test]
fn the_vendored_skills_load_as_the_skills_of_an_agent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent")).unwrap();
    std::fs::write(
        dir.path().join("agent/instructions.md"),
        "---\nname: corpus\n---\nHi.\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(
        repo_skills().canonicalize().unwrap(),
        dir.path().join("agent/skills"),
    )
    .unwrap();
    let report = Dir::new(dir.path()).load().unwrap();
    assert_eq!(report.errors().count(), 0, "{:#?}", report.diagnostics);
    assert!(report.package.agents[0].skills.len() >= 75);
}

/// Copy `source` into `agent/subagents/` byte for byte, next to a root agent, and load.
fn load_as_subagent(source: &Path) -> (adam_agent_fs::Report, String) {
    let dir = tempfile::tempdir().unwrap();
    let name = source.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::create_dir_all(dir.path().join("agent/subagents")).unwrap();
    std::fs::write(
        dir.path().join("agent/instructions.md"),
        "---\nname: root\n---\nHi.\n",
    )
    .unwrap();
    std::fs::copy(source, dir.path().join("agent/subagents").join(&name)).unwrap();
    (Dir::new(dir.path()).load().unwrap(), name)
}

fn warnings_mention(report: &adam_agent_fs::Report, needle: &str) -> bool {
    report.warnings().any(|d| d.message.contains(needle))
}

#[test]
fn a_claude_code_agent_file_parses_unchanged() {
    let source = manifest_dir().join("tests/fixtures/claude-agents/code-reviewer.md");
    let (report, _) = load_as_subagent(&source);
    assert_eq!(report.errors().count(), 0, "{:#?}", report.diagnostics);
    let Subagent::Local(agent) = &report.package.agents[0].subagents[0] else {
        panic!("a local subagent");
    };
    assert_eq!(agent.name, "code-reviewer");
    assert!(
        agent
            .frontmatter
            .description
            .as_deref()
            .unwrap()
            .starts_with("Expert code review")
    );
    assert_eq!(agent.frontmatter.model, Some(ModelRef::Inherit));
    // Claude's `maxTurns` is read as `limits.max_turns`.
    assert_eq!(
        agent.frontmatter.limits.as_ref().unwrap().max_turns,
        Some(25)
    );
    let Some(ToolList::Named(tools)) = &agent.frontmatter.tools else {
        panic!("named tools");
    };
    assert_eq!(tools, &["Read", "Grep", "Glob", "Bash"]);
    assert!(
        agent
            .instructions
            .body
            .starts_with("You are a senior code reviewer")
    );
    // What adam does not act on is said, not silently dropped.
    for key in ["`permissionMode`", "`color`", "`memory`"] {
        assert!(
            warnings_mention(&report, key),
            "{key}: {:#?}",
            report.diagnostics
        );
    }
    assert!(warnings_mention(
        &report,
        "`Read`, `Grep`, `Glob`, `Bash` are not adam tool names"
    ));
    assert_eq!(report.warnings().count(), 4);
}

#[test]
fn a_copilot_agent_file_parses_unchanged() {
    let source = manifest_dir().join("tests/fixtures/copilot-agents/security-reviewer.agent.md");
    let (report, _) = load_as_subagent(&source);
    assert_eq!(report.errors().count(), 0, "{:#?}", report.diagnostics);
    let Subagent::Local(agent) = &report.package.agents[0].subagents[0] else {
        panic!("a local subagent");
    };
    // `name: Security Reviewer` is a display name; the file name (without `.agent`) is the id.
    assert_eq!(agent.name, "security-reviewer");
    assert!(warnings_mention(
        &report,
        "`name: Security Reviewer` is not a valid agent name"
    ));
    assert_eq!(
        agent.frontmatter.tools,
        Some(ToolList::Named(vec![
            "read".into(),
            "search".into(),
            "edit".into()
        ]))
    );
    assert_eq!(
        agent.frontmatter.model,
        Some(ModelRef::Alias("review-large".into()))
    );
    assert_eq!(agent.frontmatter.metadata["team"], "security");
    for key in [
        "`target`",
        "`disable-model-invocation`",
        "`user-invocable`",
        "`mcp-servers`",
    ] {
        assert!(
            warnings_mention(&report, key),
            "{key}: {:#?}",
            report.diagnostics
        );
    }
}

#[test]
fn a_copilot_file_name_with_capitals_is_lower_cased_with_a_warning() {
    let source = manifest_dir().join("tests/fixtures/copilot-agents/Test-Specialist.agent.md");
    let (report, _) = load_as_subagent(&source);
    assert_eq!(report.errors().count(), 0, "{:#?}", report.diagnostics);
    assert_eq!(
        report.package.agents[0].subagents[0].name(),
        "test-specialist"
    );
    assert_eq!(report.warnings().count(), 1);
    assert!(warnings_mention(&report, "using `test-specialist`"));
}

#[test]
fn copilot_tools_accept_a_string_and_a_list() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agent/subagents")).unwrap();
    std::fs::write(
        dir.path().join("agent/instructions.md"),
        "---\nname: root\n---\nHi.\n",
    )
    .unwrap();
    for (file, tools, want) in [
        (
            "a.md",
            "tools: read, search",
            ToolList::Named(vec!["read".into(), "search".into()]),
        ),
        (
            "b.md",
            "tools: [\"read\", \"search\"]",
            ToolList::Named(vec!["read".into(), "search".into()]),
        ),
        ("c.md", "tools: [\"*\"]", ToolList::All),
        ("d.md", "tools: \"*\"", ToolList::All),
        ("e.md", "tools: []", ToolList::Named(vec![])),
    ] {
        std::fs::write(
            dir.path().join("agent/subagents").join(file),
            format!("---\ndescription: d\n{tools}\n---\nBody.\n"),
        )
        .unwrap();
        let report = Dir::new(dir.path()).load().unwrap();
        let sub = report.package.agents[0]
            .subagents
            .iter()
            .find(|s| s.name() == file.trim_end_matches(".md"))
            .unwrap();
        let Subagent::Local(agent) = sub else {
            panic!()
        };
        assert_eq!(agent.frontmatter.tools, Some(want), "{file}");
    }
}
