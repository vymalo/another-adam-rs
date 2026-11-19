//! `mcp.json` `when` (an adam extension): what is read and refused at load, and that a folder
//! without it keeps the digest it had before the key existed.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;

use adam_agent_fs::{Dir, ManifestSource, Report, Severity};

const ROOT: &str = "---\nname: root\n---\nYou are root.\n";

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

/// One `linear` entry with `when` as written (a leading comma, or empty).
fn mcp(entry: &str) -> String {
    format!(
        r#"{{"mcpServers": {{"linear": {{"type": "http", "url": "https://mcp.linear.app/mcp", "tools": ["list_issues"]{entry}}}}}}}"#
    )
}

fn with_when(when: &str) -> String {
    mcp(&format!(r#", "when": {when}"#))
}

const NO_WHEN: &str = r#"{"mcpServers": {"linear": {"type": "http", "url": "https://mcp.linear.app/mcp", "tools": ["list_issues"]}}}"#;

/// The parsed config as JSON and the diagnostics as `(severity, message)`.
fn parse(text: &str) -> (Option<serde_json::Value>, Vec<(Severity, String)>) {
    let mut diagnostics = Vec::new();
    let config = adam_agent_fs::parse_mcp(Path::new("agent/mcp.json"), text, &mut diagnostics);
    let json = config.map(|config| serde_json::to_value(&config).unwrap());
    let messages = diagnostics
        .into_iter()
        .map(|d| (d.severity, d.message))
        .collect();
    (json, messages)
}

fn errors(messages: &[(Severity, String)]) -> Vec<&str> {
    messages
        .iter()
        .filter(|(severity, _)| *severity == Severity::Error)
        .map(|(_, message)| message.as_str())
        .collect()
}

fn warnings(messages: &[(Severity, String)]) -> Vec<&str> {
    messages
        .iter()
        .filter(|(severity, _)| *severity == Severity::Warning)
        .map(|(_, message)| message.as_str())
        .collect()
}

#[test]
fn a_when_is_read_and_written_back() {
    let (json, messages) = parse(&with_when(r#"{"keywords": ["linear", "ticket"]}"#));
    assert!(errors(&messages).is_empty(), "{messages:#?}");
    let json = json.expect("the file is kept");
    let text = serde_json::to_string(&json).unwrap();
    assert!(
        text.contains(r#""when":{"keywords":["linear","ticket"]}"#),
        "the trigger is part of the parsed server, so it is part of the digest: {text}"
    );
}

#[test]
fn a_when_is_not_written_when_the_file_has_none() {
    let (json, messages) = parse(NO_WHEN);
    assert!(messages.is_empty(), "{messages:#?}");
    let json = json.expect("the file is kept");
    let text = serde_json::to_string(&json).unwrap();
    assert!(
        !text.contains("\"when\""),
        "a folder without `when` writes no `when` key: {text}"
    );
}

#[test]
fn a_when_with_no_keywords_is_refused_and_the_server_left_out() {
    let (json, messages) = parse(&with_when(r#"{}"#));
    assert!(
        errors(&messages).iter().any(|m| m.contains("keywords")),
        "{messages:#?}"
    );
    assert!(json.is_none() || json.unwrap()["servers"].get("linear").is_none());
}

#[test]
fn an_empty_or_non_string_keyword_is_refused() {
    for when in [
        r#"{"keywords": []}"#,
        r#"{"keywords": ["linear", ""]}"#,
        r#"{"keywords": ["linear", 3]}"#,
        r#"{"keywords": "linear"}"#,
    ] {
        let (_, messages) = parse(&with_when(when));
        assert!(
            !errors(&messages).is_empty(),
            "`{when}` must be refused: {messages:#?}"
        );
    }
}

#[test]
fn a_keyword_with_a_control_character_or_over_the_bound_is_refused() {
    let too_long = format!(r#"{{"keywords": ["{}"]}}"#, "x".repeat(65));
    let too_many = format!(
        r#"{{"keywords": [{}]}}"#,
        (0..17)
            .map(|n| format!("\"k{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for when in [
        r#"{"keywords": ["line\u0000ar"]}"#.to_owned(),
        too_long,
        too_many,
    ] {
        let (_, messages) = parse(&with_when(&when));
        assert!(
            !errors(&messages).is_empty(),
            "`{when}` must be refused: {messages:#?}"
        );
    }
}

#[test]
fn paths_is_not_supported_in_v1_and_says_so() {
    let (json, messages) = parse(&with_when(
        r#"{"keywords": ["linear"], "paths": ["src/**"]}"#,
    ));
    assert!(errors(&messages).is_empty(), "{messages:#?}");
    assert!(
        warnings(&messages)
            .iter()
            .any(|m| m.contains("paths") && m.contains("not supported")),
        "{messages:#?}"
    );
    // The server stays, eager as a server without `when`: a key that is ignored must not make it
    // disappear.
    assert!(json.unwrap()["servers"].get("linear").is_some());
}

#[test]
fn a_folder_without_when_keeps_the_digest_it_had() {
    let dir = write(&[("agent/instructions.md", ROOT), ("agent/mcp.json", NO_WHEN)]);
    let without = digest_of(dir.path());
    // Captured at `8f6fd2f` (main before `when` existed): the key is left out of the manifest's
    // JSON when absent, so the digest is the one a folder had before the key existed.
    assert_eq!(
        without, "sha256:423f4aa5556c103a83a77426d1043464cff2861f576f93ebc20ed68b083c4bba",
        "the digest of a folder with an mcp.json but no `when`"
    );
    // A trigger changes it.
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/mcp.json", &with_when(r#"{"keywords": ["linear"]}"#)),
    ]);
    assert_ne!(digest_of(dir.path()), without);
}
