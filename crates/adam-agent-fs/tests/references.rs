//! `references:` in the frontmatter of the root agent: what is read, what is refused at load, and
//! that a folder without references keeps the digest it had before references existed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;

use adam_agent_fs::{Dir, ManifestSource, Report, Severity};

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
    Dir::new(dir)
        .digest(&report.package.agents[0])
        .unwrap()
        .to_string()
}

fn errors(report: &Report) -> Vec<String> {
    report
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.message.clone())
        .collect()
}

const WITH: &str = "---
name: root
references:
  - name: design-system
    url: https://github.com/acme/design-system
    branch: main
    description: Tokens and components the UI follows.
  - name: api
    url: https://github.com/acme/api
    description: The API's source.
---
You are root.
";

#[test]
fn references_are_read_from_the_frontmatter_of_the_root_agent() {
    let dir = write(&[("agent/instructions.md", WITH)]);
    let report = load(dir.path());
    assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
    let references = &report.package.agents[0].frontmatter.references;
    assert_eq!(references.len(), 2);
    assert_eq!(references[0].name, "design-system");
    assert_eq!(references[0].branch.as_deref(), Some("main"));
    assert_eq!(references[1].branch, None);
}

#[test]
fn a_folder_without_references_keeps_the_digest_it_had_and_one_with_them_has_another() {
    let plain = "---\nname: root\n---\nYou are root.\n";
    let without = write(&[("agent/instructions.md", plain)]);
    let with = write(&[("agent/instructions.md", WITH)]);
    assert_ne!(digest_of(without.path()), digest_of(with.path()));
    let manifest = serde_json::to_string(&load(without.path()).package.agents[0]).unwrap();
    assert!(!manifest.contains("references"), "{manifest}");
}

#[test]
fn a_bad_reference_is_an_error_at_load() {
    for (what, entry, message) in [
        (
            "a name that is not a directory name",
            "{ name: Design System, url: 'https://github.com/a/b', description: d }",
            "not a directory name",
        ),
        (
            "no description",
            "{ name: a, url: 'https://github.com/a/b', description: ' ' }",
            "needs a `description`",
        ),
        (
            "credentials in the address",
            "{ name: a, url: 'https://user:tok@github.com/a/b', description: d }",
            "credentials",
        ),
        (
            "a branch with a space",
            "{ name: a, url: 'https://github.com/a/b', branch: 'a b', description: d }",
            "branch",
        ),
    ] {
        let text = format!("---\nname: root\nreferences:\n  - {entry}\n---\nHi.\n");
        let dir = write(&[("agent/instructions.md", &text)]);
        let report = load(dir.path());
        let found = errors(&report);
        assert!(
            found.iter().any(|m| m.contains(message)),
            "{what}: {:#?}",
            report.diagnostics
        );
    }
    // A name twice, and more than sixteen.
    let twice = "---\nname: root\nreferences:\n  - { name: a, url: 'https://x/a/b', description: d }\n  - { name: a, url: 'https://x/a/c', description: d }\n---\nHi.\n";
    let dir = write(&[("agent/instructions.md", twice)]);
    assert!(errors(&load(dir.path())).iter().any(|m| m.contains("twice")));
    let many: String = (0..17)
        .map(|n| format!("  - {{ name: r{n}, url: 'https://x/a/b', description: d }}\n"))
        .collect();
    let text = format!("---\nname: root\nreferences:\n{many}---\nHi.\n");
    let dir = write(&[("agent/instructions.md", &text)]);
    assert!(
        errors(&load(dir.path()))
            .iter()
            .any(|m| m.contains("at most 16"))
    );
}

#[test]
fn a_subagent_has_no_references_and_says_so() {
    let sub = "---\ndescription: Does one thing.\nreferences:\n  - { name: a, url: 'https://x/a/b', description: d }\n---\nYou do one thing.\n";
    let dir = write(&[
        ("agent/instructions.md", "---\nname: root\n---\nHi.\n"),
        ("agent/subagents/helper.md", sub),
    ]);
    let report = load(dir.path());
    assert!(
        report.diagnostics.iter().any(|d| d.severity == Severity::Warning
            && d.message.contains("`references` is for the root agent")),
        "{:#?}",
        report.diagnostics
    );
}
