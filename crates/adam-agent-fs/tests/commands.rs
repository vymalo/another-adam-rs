//! `commands/<name>.md`: what is read, what is refused at load, and that a folder without commands
//! keeps the digest it had before commands existed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;

use adam_agent_fs::{Dir, ManifestSource, Report, Severity};

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

fn digest_of(dir: &Path) -> String {
    let report = load(dir);
    let agent = &report.package.agents[0];
    Dir::new(dir).digest(agent).unwrap().to_string()
}

#[test]
fn commands_are_read_with_their_description_hint_and_template() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/commands/review.md",
            "---\ndescription: Review a pull request.\nargument-hint: \"<repo>\"\n---\r\nReview $1.\r\n",
        ),
        (
            "agent/commands/explain.md",
            "---\ndescription: Explain code.\n---\nExplain $ARGUMENTS.\n",
        ),
        ("agent/commands/README.md", "not a command"),
        ("agent/commands/.hidden.md", "not a command"),
    ]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let commands = &report.package.agents[0].commands;
    let names: Vec<_> = commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["explain", "review"], "sorted by name");
    assert_eq!(commands[1].description, "Review a pull request.");
    assert_eq!(commands[1].argument_hint.as_deref(), Some("<repo>"));
    assert_eq!(commands[1].template, "Review $1.");
    assert_eq!(commands[0].argument_hint, None);
}

#[test]
fn a_template_with_a_shell_form_is_refused_at_load() {
    for body in [
        "Run !`ls` first.",
        "!`rm -rf /`",
        "text\nmore\nBranch: !`git branch`",
    ] {
        let dir = write(&[
            ("agent/instructions.md", ROOT),
            (
                "agent/commands/a.md",
                &format!("---\ndescription: d\n---\n{body}\n"),
            ),
        ]);
        let report = load(dir.path());
        let errors: Vec<_> = report
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1, "{body}: {:#?}", report.diagnostics);
        assert!(errors[0].message.contains("shell form"), "{body}");
        // The command is left out: nothing of it can be served.
        assert!(report.package.agents[0].commands.is_empty(), "{body}");
    }
    // A bang or a backtick alone is text.
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/commands/a.md",
            "---\ndescription: d\n---\nGo! Use `code` here.\n",
        ),
    ]);
    assert!(load(dir.path()).diagnostics.is_empty());
}

#[test]
fn a_description_over_200_characters_and_more_than_50_commands_are_refused() {
    let long = "x".repeat(201);
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/commands/a.md",
            &format!("---\ndescription: {long}\n---\nGo.\n"),
        ),
    ]);
    let report = load(dir.path());
    assert_eq!(report.diagnostics.len(), 1, "{:#?}", report.diagnostics);
    assert!(report.diagnostics[0].message.contains("at most 200"));

    let exactly: Vec<(String, String)> = (0..50)
        .map(|n| {
            (
                format!("agent/commands/c{n:02}.md"),
                "---\ndescription: d\n---\nGo.\n".to_owned(),
            )
        })
        .collect();
    let mut files: Vec<(&str, &str)> = vec![("agent/instructions.md", ROOT)];
    files.extend(exactly.iter().map(|(p, c)| (p.as_str(), c.as_str())));
    let dir = write(&files);
    assert!(load(dir.path()).diagnostics.is_empty());

    let over = ("agent/commands/c50.md", "---\ndescription: d\n---\nGo.\n");
    files.push(over);
    let dir = write(&files);
    let report = load(dir.path());
    assert_eq!(report.diagnostics.len(), 1, "{:#?}", report.diagnostics);
    assert!(report.diagnostics[0].message.contains("at most 50"));
}

#[test]
fn a_template_over_32_kib_is_refused() {
    let big = "y".repeat(32 * 1024 + 1);
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        (
            "agent/commands/a.md",
            &format!("---\ndescription: d\n---\n{big}\n"),
        ),
    ]);
    let report = load(dir.path());
    assert_eq!(report.diagnostics.len(), 1, "{:#?}", report.diagnostics);
    assert!(report.diagnostics[0].message.contains("at most 32768"));
}

#[test]
fn a_subagents_commands_folder_is_ignored_with_a_warning() {
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/subagents/x/instructions.md", SUB),
        (
            "agent/subagents/x/commands/a.md",
            "---\ndescription: d\n---\nGo.\n",
        ),
    ]);
    let report = load(dir.path());
    assert_eq!(report.diagnostics.len(), 1, "{:#?}", report.diagnostics);
    assert_eq!(report.diagnostics[0].severity, Severity::Warning);
    assert!(
        report.diagnostics[0]
            .message
            .contains("belong to the root agent")
    );
    let root = &report.package.agents[0];
    assert!(root.commands.is_empty());
    let adam_agent_fs::Subagent::Local(sub) = &root.subagents[0] else {
        panic!("x is local");
    };
    assert!(sub.commands.is_empty());
}

#[test]
fn digest_unchanged_without_commands() {
    let dir = write(&[("agent/instructions.md", ROOT)]);
    let without = digest_of(dir.path());
    // The manifest's JSON has no `commands` key at all, so the digest is the recipe's as it was.
    let report = load(dir.path());
    let json = serde_json::to_string(&report.package.agents[0]).unwrap();
    assert!(!json.contains("commands"), "{json}");
    // An empty `commands/` changes nothing either.
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/commands/.keep", ""),
    ]);
    assert_eq!(digest_of(dir.path()), without);
    // A command changes it.
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/commands/a.md", "---\ndescription: d\n---\nGo.\n"),
    ]);
    assert_ne!(digest_of(dir.path()), without);
}

#[test]
fn each_agent_of_a_package_has_its_own_commands() {
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
            "agents/a/commands/one.md",
            "---\ndescription: d\n---\nGo.\n",
        ),
    ]);
    let report = Dir::new(dir.path()).load().unwrap();
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let agents = &report.package.agents;
    assert_eq!(agents[0].commands.len(), 1);
    assert!(agents[1].commands.is_empty());
}
