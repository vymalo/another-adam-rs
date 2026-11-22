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

/// The loaders reserve `load` and `load__`: a server named `load` is refused at start, so an MCP
/// tool can never be mistaken for a loader (ADR 0066, decision 2).
#[test]
fn a_server_named_load_is_refused_at_start() {
    let text = r#"{"mcpServers": {"load": {"type": "http", "url": "https://mcp.example/x", "tools": ["search"]}}}"#;
    let (json, messages) = parse(text);
    assert!(
        errors(&messages).iter().any(|m| m.contains("reserved")),
        "{messages:#?}"
    );
    assert!(json.is_none() || json.unwrap()["servers"].get("load").is_none());
}

/// A `when` server's name becomes its group's name (ADR 0066): the loader refuses a name that is
/// not a group name rather than renaming it, because a rename would change its tools' names.
#[test]
fn a_when_server_whose_name_is_not_a_group_name_is_refused() {
    let long = "a".repeat(41);
    for name in ["my-server".to_owned(), "MyLinear".to_owned(), long] {
        let text = format!(
            r#"{{"mcpServers": {{"{name}": {{"type": "http", "url": "https://mcp.example/x",
                "tools": ["echo"], "when": {{"keywords": ["x"]}}}}}}}}"#
        );
        let (json, messages) = parse(&text);
        assert!(
            errors(&messages)
                .iter()
                .any(|m| m.contains("group's name") && m.contains("Rename the server")),
            "{name}: {messages:#?}"
        );
        assert!(
            json.is_none() || json.unwrap()["servers"].get(&name).is_none(),
            "{name}"
        );
    }
}

#[test]
fn a_when_server_named_my_server_is_kept() {
    let text = r#"{"mcpServers": {"my_server": {"type": "http", "url": "https://mcp.example/x",
        "tools": ["echo"], "when": {"keywords": ["x"]}}}}"#;
    let (json, messages) = parse(text);
    assert!(errors(&messages).is_empty(), "{messages:#?}");
    assert!(json.unwrap()["servers"].get("my_server").is_some());
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
    // JSON when absent, so the digest is the one a folder had before the key existed. Checked
    // independently against `origin/main` (`crates/adam-agent-fs/tests/digest_main.rs` there).
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

/// The stdio variant: `when` is one field of the enum, so both variants read, write and digest it.
#[test]
fn a_when_on_a_stdio_server_is_read_written_and_digested() {
    const STDIO: &str = r#"{"mcpServers": {"local": {"command": "npx", "args": ["-y", "x"]}}}"#;
    const STDIO_WHEN: &str = r#"{"mcpServers": {"local": {"command": "npx", "args": ["-y", "x"],
        "when": {"keywords": ["local", "offline"]}}}}"#;
    let (json, messages) = parse(STDIO_WHEN);
    assert!(errors(&messages).is_empty(), "{messages:#?}");
    let text = serde_json::to_string(&json.expect("the file is kept")).unwrap();
    assert!(
        text.contains(r#""when":{"keywords":["local","offline"]}"#),
        "a stdio server keeps its `when`: {text}"
    );
    let dir = write(&[("agent/instructions.md", ROOT), ("agent/mcp.json", STDIO)]);
    let without = digest_of(dir.path());
    let dir = write(&[
        ("agent/instructions.md", ROOT),
        ("agent/mcp.json", STDIO_WHEN),
    ]);
    assert_ne!(
        digest_of(dir.path()),
        without,
        "a stdio server's `when` is part of the digest"
    );
}

/// 1 to 16 words, each 1 to 64 characters: the bound itself is accepted, one more is refused.
#[test]
fn the_keyword_bounds_are_inclusive() {
    let exact = format!(r#"{{"keywords": ["{}"]}}"#, "x".repeat(64));
    let (json, messages) = parse(&with_when(&exact));
    assert!(errors(&messages).is_empty(), "{messages:#?}");
    assert!(json.is_some(), "64 characters are allowed");
    let one = format!(
        r#"{{"keywords": [{}]}}"#,
        (0..16)
            .map(|n| format!("\"k{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let (json, messages) = parse(&with_when(&one));
    assert!(errors(&messages).is_empty(), "{messages:#?}");
    assert!(json.is_some(), "16 words are allowed");
}

/// A control or format character is refused wherever it is: the words reach a prompt and a
/// refusal message, and a zero-width or bidirectional character would be invisible there.
#[test]
fn a_control_or_format_character_anywhere_in_a_keyword_is_refused() {
    for when in [
        r#"{"keywords": ["li\u0000near"]}"#,
        r#"{"keywords": ["li\u0009near"]}"#,
        r#"{"keywords": ["li\u000anear"]}"#,
        r#"{"keywords": ["li\u200bnear"]}"#,
        r#"{"keywords": ["li\u202enear"]}"#,
        r#"{"keywords": ["li\ufeffnear"]}"#,
    ] {
        let (_, messages) = parse(&with_when(when));
        assert!(
            !errors(&messages).is_empty(),
            "`{when}` must be refused: {messages:#?}"
        );
    }
}

/// A `when` that is not an object, or whose `keywords` is not a list of strings, is refused.
#[test]
fn a_malformed_when_is_refused() {
    for when in [
        r#"[]"#,
        r#""linear""#,
        r#"3"#,
        r#"{"keywords": {"0": "linear"}}"#,
        r#"{"keywords": null}"#,
    ] {
        let (_, messages) = parse(&with_when(when));
        assert!(
            !errors(&messages).is_empty(),
            "`{when}` must be refused: {messages:#?}"
        );
    }
}
