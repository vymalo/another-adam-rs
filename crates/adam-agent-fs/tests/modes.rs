//! `modes/<name>.md` (ADR 0039, decision 3): what is read, what is refused at load, that exactly
//! one mode says `default: true`, that a subagent's `modes/` is ignored, and that a folder without
//! modes keeps the digest it had before modes existed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;

use adam_agent_fs::{Dir, ManifestSource, Report, Severity, ToolList};

const ROOT: &str = "---\nname: root\n---\nYou are root.\n";
const SUB: &str = "---\ndescription: Does one thing.\n---\nYou do one thing.\n";

fn write(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }
    dir
}

fn load(dir: &Path) -> Report {
    Dir::new(dir).default_name("root").load().unwrap()
}

fn errors(report: &Report) -> Vec<&adam_agent_fs::Diagnostic> {
    report
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

fn digest_of(dir: &Path) -> String {
    let report = load(dir);
    let agent = &report.package.agents[0];
    Dir::new(dir).digest(agent).unwrap().to_string()
}

#[test]
fn modes_are_read_with_their_title_description_tools_and_prompt() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/build.md",
            "---\ntitle: Build\ndescription: Changes things.\ndefault: true\n---\n",
        ),
        (
            "agent/modes/plan.md",
            "---\ndescription: Read and propose.\ntools: [read_file, list_files]\n---\r\n\
             You are in plan mode: change nothing.\r\n",
        ),
    ]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let modes = &report.package.agents[0].modes;
    let names: Vec<_> = modes.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["build", "plan"], "sorted by name");
    assert_eq!(modes[0].title, "Build");
    assert_eq!(modes[0].description, "Changes things.");
    assert!(modes[0].default);
    assert_eq!(modes[0].tools, None, "no `tools:` leaves the agent's own");
    assert_eq!(modes[0].prompt, "", "a mode that only narrows has no words");
    // The title defaults to the name; the tools are the mode's allowlist.
    assert_eq!(modes[1].title, "plan");
    assert_eq!(
        modes[1].tools,
        Some(ToolList::Named(vec![
            "read_file".to_owned(),
            "list_files".to_owned()
        ]))
    );
    assert_eq!(modes[1].prompt, "You are in plan mode: change nothing.");
    assert!(!modes[1].default);
}

#[test]
fn tools_takes_the_spellings_an_agent_file_takes() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/a.md",
            "---\ndescription: d\ndefault: true\ntools: a, b\n---\n",
        ),
        (
            "agent/modes/b.md",
            "---\ndescription: d\ntools: \"*\"\n---\n",
        ),
    ]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let modes = &report.package.agents[0].modes;
    assert_eq!(
        modes[0].tools,
        Some(ToolList::Named(vec!["a".to_owned(), "b".to_owned()]))
    );
    assert_eq!(modes[1].tools, Some(ToolList::All));
}

#[test]
fn a_mode_without_a_description_or_with_a_bad_name_is_refused() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/modes/a.md", "---\ndefault: true\n---\nGo.\n"),
        ("agent/modes/Bad Name.md", "---\ndescription: d\n---\nGo.\n"),
    ]);
    let report = load(dir.path());
    let errors = errors(&report);
    assert_eq!(errors.len(), 2, "{:#?}", report.diagnostics);
    let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("needs a `description`")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("not a mode name")),
        "{messages:?}"
    );
    // A mode with an error is left out: nothing of it can be served.
    assert!(report.package.agents[0].modes.is_empty());
}

#[test]
fn exactly_one_mode_says_default() {
    let one = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/a.md",
            "---\ndescription: d\ndefault: true\n---\n",
        ),
        ("agent/modes/b.md", "---\ndescription: d\n---\n"),
    ]);
    assert!(load(one.path()).diagnostics.is_empty());

    let none = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/modes/a.md", "---\ndescription: d\n---\n"),
        ("agent/modes/b.md", "---\ndescription: d\n---\n"),
    ]);
    let report = load(none.path());
    let none_default = errors(&report);
    assert_eq!(none_default.len(), 1, "{:#?}", report.diagnostics);
    assert!(
        none_default[0]
            .message
            .contains("one mode says `default: true`"),
        "{none_default:?}"
    );

    let two = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/a.md",
            "---\ndescription: d\ndefault: true\n---\n",
        ),
        (
            "agent/modes/b.md",
            "---\ndescription: d\ndefault: true\n---\n",
        ),
    ]);
    let report = load(two.path());
    let both = errors(&report);
    assert_eq!(both.len(), 1, "{:#?}", report.diagnostics);
    assert!(both[0].message.contains("exactly one may"), "{both:?}");
    assert!(
        both[0].message.contains("a, b"),
        "the message names them: {both:?}"
    );
}

/// `permission:` is refused, not ignored: the rules of ADR 0039 decision 4 are not built, and a
/// deny nobody reads would let through what the file forbids.
#[test]
fn a_modes_permission_is_refused_rather_than_ignored() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/a.md",
            "---\ndescription: d\ndefault: true\npermission:\n  edit: deny\n---\n",
        ),
    ]);
    let report = load(dir.path());
    let errors = errors(&report);
    assert_eq!(errors.len(), 1, "{:#?}", report.diagnostics);
    assert!(
        errors[0].message.contains("`permission:` is not read yet"),
        "{errors:?}"
    );
    assert!(report.package.agents[0].modes.is_empty());
}

#[test]
fn a_title_and_a_description_have_their_caps() {
    let title = "t".repeat(61);
    let description = "d".repeat(201);
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/a.md",
            &format!("---\ndescription: {description}\ntitle: \"{title}\"\ndefault: true\n---\n"),
        ),
    ]);
    let report = load(dir.path());
    let errors = errors(&report);
    assert_eq!(errors.len(), 2, "{:#?}", report.diagnostics);
    assert!(
        errors.iter().any(|e| e.message.contains("at most 200")),
        "{errors:?}"
    );
    assert!(
        errors.iter().any(|e| e.message.contains("at most 60")),
        "{errors:?}"
    );
}

#[test]
fn more_than_16_modes_are_refused() {
    let files: Vec<(String, String)> = (0..17)
        .map(|n| {
            (
                format!("agent/modes/m{n:02}.md"),
                format!(
                    "---\ndescription: d\n{}---\n",
                    if n == 0 { "default: true\n" } else { "" }
                ),
            )
        })
        .collect();
    let mut all: Vec<(&str, &str)> = vec![("agent/instructions.md", ROOT)];
    all.extend(files.iter().map(|(p, c)| (p.as_str(), c.as_str())));
    let dir = write(&all);
    let report = load(dir.path());
    let errors = errors(&report);
    assert_eq!(errors.len(), 1, "{:#?}", report.diagnostics);
    assert!(errors[0].message.contains("at most 16"), "{errors:?}");
}

#[test]
fn a_subagents_modes_folder_is_ignored_with_a_warning() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/subagents/x/instructions.md", SUB),
        (
            "agent/subagents/x/modes/a.md",
            "---\ndescription: d\ndefault: true\n---\n",
        ),
    ]);
    let report = load(dir.path());
    assert_eq!(report.diagnostics.len(), 1, "{:#?}", report.diagnostics);
    assert_eq!(report.diagnostics[0].severity, Severity::Warning);
    assert!(
        report.diagnostics[0]
            .message
            .contains("modes belong to the root agent")
    );
    let root = &report.package.agents[0];
    assert!(root.modes.is_empty());
    let adam_agent_fs::Subagent::Local(sub) = &root.subagents[0] else {
        panic!("x is local");
    };
    assert!(sub.modes.is_empty());
}

#[test]
fn a_mode_that_is_not_a_flat_markdown_file_is_ignored_with_a_warning() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/ok.md",
            "---\ndescription: d\ndefault: true\n---\n",
        ),
        ("agent/modes/nested/deep.md", "---\ndescription: d\n---\n"),
        ("agent/modes/notes.txt", "scratch"),
    ]);
    let report = load(dir.path());
    assert!(errors(&report).is_empty(), "{:#?}", report.diagnostics);
    assert_eq!(report.package.agents[0].modes.len(), 1);
    assert_eq!(
        report
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count(),
        2,
        "{:#?}",
        report.diagnostics
    );
}

#[test]
fn digest_unchanged_without_modes() {
    let dir = write(&[("agent/instructions.md", ROOT)]);
    let without = digest_of(dir.path());
    // The manifest's JSON has no `modes` key at all, so the digest is the recipe's as it was.
    let report = load(dir.path());
    let json = serde_json::to_string(&report.package.agents[0]).unwrap();
    assert!(!json.contains("modes"), "{json}");
    // An empty `modes/` changes nothing either.
    let dir = write(&[("agent/instructions.md", ROOT), ("agent/modes/.keep", "")]);
    assert_eq!(digest_of(dir.path()), without);
    // A mode changes it, and so does a change to a mode's prompt.
    let with = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/a.md",
            "---\ndescription: d\ndefault: true\n---\nGo.\n",
        ),
    ]);
    let one = digest_of(with.path());
    assert_ne!(one, without);
    let edited = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/modes/a.md",
            "---\ndescription: d\ndefault: true\n---\nGo on.\n",
        ),
    ]);
    assert_ne!(digest_of(edited.path()), one);
}

#[test]
fn each_agent_of_a_package_has_its_own_modes() {
    let dir = write(&[
        (
            "agents/a/instructions.md",
            "---\nname: a\n---\nYou are a.\n",
        ),
        (
            "agents/b/instructions.md",
            "---\nname: b\n---\nYou are b.\n",
        ),
        (
            "agents/a/modes/one.md",
            "---\ndescription: d\ndefault: true\n---\n",
        ),
    ]);
    let report = Dir::new(dir.path()).load().unwrap();
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let agents = &report.package.agents;
    assert_eq!(agents[0].modes.len(), 1);
    assert!(agents[1].modes.is_empty());
}

/// ADR 0089 decision 6, slice C4: a subagent is no tool, so a `tools:` that names one by the tool
/// it used to have (`reviewer`) is refused when the folder is read, at the line of `tools`, naming
/// `subagent_start` instead: the root's own, a mode's (which would quietly lose the subagent), and
/// a subagent's that names one of its own subagents. A remote subagent counts as one; a name that
/// is only some other agent's subagent is no concern of this list.
#[test]
fn a_tools_list_that_names_a_subagent_is_refused_naming_subagent_start() {
    let refusal = |name: &str| {
        format!(
            "`tools` names the subagent `{name}`, and it is not a tool: a subagent is started \
             with `subagent_start` (`agent: {name}`; `mode: one_shot` and `wait: true` to wait for \
             its answer), which an agent with subagents always has. Remove it from `tools`"
        )
    };
    let reviewer = "---\ndescription: Reviews.\n---\nYou review.\n";
    let billing = "---\ndescription: Billing.\na2a: https://billing.example.com/card\n---\n";
    for (files, file, line, name) in [
        (
            vec![
                (
                    "agent/instructions.md",
                    "---\nname: root\ntools: [read_file, reviewer]\n---\nYou are root.\n",
                ),
                ("agent/subagents/reviewer.md", reviewer),
            ],
            "agent/instructions.md",
            3,
            "reviewer",
        ),
        (
            vec![
                ("agent/instructions.md", ROOT),
                ("agent/subagents/billing.md", billing),
                (
                    "agent/modes/plan.md",
                    "---\ndescription: Plans.\ndefault: true\ntools:\n  - read_file\n  - billing\n---\n",
                ),
            ],
            "agent/modes/plan.md",
            4,
            "billing",
        ),
        (
            vec![
                ("agent/instructions.md", ROOT),
                (
                    "agent/subagents/researcher/instructions.md",
                    "---\ndescription: Researches.\ntools: [summarizer]\n---\nYou research.\n",
                ),
                ("agent/subagents/researcher/subagents/summarizer.md", SUB),
            ],
            "agent/subagents/researcher/instructions.md",
            3,
            "summarizer",
        ),
    ] {
        let dir = write(&files);
        let report = load(dir.path());
        let found = errors(&report);
        assert_eq!(found.len(), 1, "{file}: {found:#?}");
        assert_eq!(found[0].path, Path::new(file));
        assert_eq!(found[0].line, Some(line), "{file}");
        assert_eq!(found[0].message, refusal(name), "{file}");
    }
    // Two of them are named in one sentence; a sibling's subagent is not this agent's.
    let dir = write(&[
        (
            "agent/instructions.md",
            "---\nname: root\ntools: [reviewer, billing, summarizer]\n---\nYou are root.\n",
        ),
        ("agent/subagents/reviewer.md", reviewer),
        ("agent/subagents/billing.md", billing),
        ("agent/subagents/researcher/instructions.md", SUB),
        ("agent/subagents/researcher/subagents/summarizer.md", SUB),
    ]);
    let report = load(dir.path());
    let found = errors(&report);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(
        found[0].message.starts_with(
            "`tools` names the subagents `reviewer`, `billing`, and they are not tools: a \
             subagent is started with `subagent_start` (`agent: reviewer`;"
        ) && found[0].message.ends_with("Remove them from `tools`"),
        "{}",
        found[0].message
    );
    // A list that names none, `*`, and the session tools themselves load.
    for tools in ["[read_file]", "'*'", "[subagent_start, subagent_read]"] {
        let root = format!("---\nname: root\ntools: {tools}\n---\nYou are root.\n");
        let dir = write(&[
            ("agent/instructions.md", root.as_str()),
            ("agent/subagents/reviewer.md", reviewer),
        ]);
        assert!(errors(&load(dir.path())).is_empty(), "{tools}");
    }
}
