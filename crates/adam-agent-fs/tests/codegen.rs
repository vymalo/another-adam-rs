//! The `build` feature: what `adam_agent_fs::build("agent").emit()` prints and writes.
//!
//! * A golden test of the generated source (`tests/golden/adam_agent.rs.golden`; the absolute
//!   root is replaced by `{ROOT}`). Regenerate with `ADAM_UPDATE_GOLDEN=1 cargo test -p
//!   adam-agent-fs --features build --test codegen`.
//! * Invalid directories fail with `path:line` diagnostics before rustc runs.
//! * `rerun-if-changed` covers the directory and every file.
#![cfg(feature = "build")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use adam_agent_fs::{BuildError, Error, build};

fn valid_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/valid")
}

fn write(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let full = dir.path().join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
    dir
}

const ROOT_AGENT: &str = "---\nname: helper\n---\nYou help.\n";

/// Run `emit_to` into a fresh out dir; the directives it printed, and the result.
fn emit(
    b: adam_agent_fs::Build,
    root: &Path,
) -> (
    String,
    Result<adam_agent_fs::Emitted, BuildError>,
    tempfile::TempDir,
) {
    let out = tempfile::tempdir().unwrap();
    let mut printed = Vec::new();
    let result = b.root(root).out_dir(out.path()).emit_to(&mut printed);
    (String::from_utf8(printed).unwrap(), result, out)
}

fn directives<'a>(printed: &'a str, key: &str) -> Vec<&'a str> {
    let prefix = format!("cargo::{key}=");
    printed
        .lines()
        .filter_map(|l| l.strip_prefix(prefix.as_str()))
        .collect()
}

#[test]
fn the_generated_source_matches_the_golden_file() {
    let root = valid_root();
    let generated = build("agent")
        .root(&root)
        .name("fixture-agent")
        .crate_path("::adam_agent_fs")
        .generate()
        .unwrap();
    let source = generated.source.replace(root.to_str().unwrap(), "{ROOT}");
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/adam_agent.rs.golden");
    if std::env::var_os("ADAM_UPDATE_GOLDEN").is_some() {
        fs::write(&golden, &source).unwrap();
    }
    let expected = fs::read_to_string(&golden).unwrap();
    assert_eq!(
        source, expected,
        "the generated source changed; if that is intended, regenerate the golden file \
         (ADAM_UPDATE_GOLDEN=1)"
    );
}

#[test]
fn generation_is_deterministic() {
    let a = build("agent").root(valid_root()).generate().unwrap();
    let b = build("agent").root(valid_root()).generate().unwrap();
    assert_eq!(a.source, b.source);
    assert_eq!(a.manifest_json, b.manifest_json);
    assert!(a.manifest_json.contains("\"digests\""));
}

#[test]
fn emit_writes_the_source_and_the_manifest_and_only_when_they_change() {
    let (printed, result, out) = emit(build("agent"), &valid_root());
    let emitted = result.unwrap();
    assert_eq!(emitted.source, out.path().join("adam_agent.rs"));
    assert_eq!(emitted.manifest, out.path().join("adam_manifest.json"));
    assert_eq!(emitted.warnings, 0, "{printed}");
    assert!(
        fs::read_to_string(&emitted.source)
            .unwrap()
            .contains("pub static AGENTS")
    );
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&emitted.manifest).unwrap()).unwrap();
    assert!(
        manifest["digests"]["coder"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(manifest["package"]["layout"], "Single");

    // A second run with the same files leaves the file alone: rustc does not rebuild for it.
    let before = fs::metadata(&emitted.source).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    let mut sink = Vec::new();
    build("agent")
        .root(valid_root())
        .out_dir(out.path())
        .emit_to(&mut sink)
        .unwrap();
    assert_eq!(
        fs::metadata(&emitted.source).unwrap().modified().unwrap(),
        before
    );
}

#[test]
fn an_invalid_directory_fails_with_file_and_line() {
    let dir = write(&[
        ("agent/instructions.md", ROOT_AGENT),
        (
            "agent/subagents/bad.md",
            "---\ndescription: Does a thing.\nmodel: [unclosed\n---\nYou do it.\n",
        ),
        ("agent/skills/pdf/SKILL.md", "---\nname: pdf\n---\nBody.\n"),
        ("agent/mcp.json", "{\n  \"mcpServers\": {\n"),
    ]);
    let (printed, result, out) = emit(build("agent"), dir.path());
    let err = result.unwrap_err();
    assert!(
        matches!(err, BuildError::Load(Error::Invalid { .. })),
        "{err}"
    );

    let errors = directives(&printed, "error");
    assert_eq!(errors.len(), 3, "{printed}");
    // `path:line: message`, relative to the package root: what editors and CI turn into a link.
    for file in ["agent/subagents/bad.md", "agent/mcp.json"] {
        let error = errors
            .iter()
            .find(|e| e.starts_with(&format!("{file}:")))
            .unwrap_or_else(|| panic!("no error for {file}: {printed}"));
        let line: u32 = error[file.len() + 1..]
            .split(':')
            .next()
            .unwrap()
            .parse()
            .unwrap_or_else(|_| panic!("no line number in {error}"));
        assert!(line >= 2, "{error}");
    }
    assert!(
        errors
            .iter()
            .any(|e| e.starts_with("agent/skills/pdf/SKILL.md") && e.contains("description")),
        "{printed}"
    );
    assert_eq!(err.diagnostics().len(), 3);
    // Nothing is written for a refused directory, and cargo is still told what to watch, so that
    // fixing the file rebuilds.
    assert!(!out.path().join("adam_agent.rs").exists());
    assert!(!directives(&printed, "rerun-if-changed").is_empty());
}

#[test]
fn every_finding_is_one_line() {
    let dir = write(&[("agent/instructions.md", "---\nname: helper\n---\n")]);
    let (printed, result, _out) = emit(build("agent"), dir.path());
    result.unwrap_err();
    for line in printed.lines() {
        assert!(line.starts_with("cargo::"), "{line}");
    }
}

#[test]
fn warnings_are_warnings_and_strict_makes_them_errors() {
    let dir = write(&[(
        "agent/instructions.md",
        "---\nname: helper\nunknown_key: 1\n---\nYou help.\n",
    )]);
    let (printed, result, _out) = emit(build("agent"), dir.path());
    assert_eq!(result.unwrap().warnings, 1);
    let warnings = directives(&printed, "warning");
    assert_eq!(warnings.len(), 1, "{printed}");
    assert!(
        warnings[0].starts_with("agent/instructions.md:"),
        "{printed}"
    );
    assert!(directives(&printed, "error").is_empty());

    let (printed, result, out) = emit(build("agent").strict(), dir.path());
    assert!(matches!(
        result,
        Err(BuildError::Load(Error::Invalid { .. }))
    ));
    assert_eq!(directives(&printed, "error").len(), 1, "{printed}");
    assert!(directives(&printed, "warning").is_empty());
    assert!(!out.path().join("adam_agent.rs").exists());
}

/// Every file under `dir`, and every directory, recursively.
fn walk(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    out.insert(dir.to_path_buf());
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.insert(path);
        }
    }
}

#[test]
fn rerun_if_changed_covers_the_directory_and_every_file() {
    let root = valid_root();
    let (printed, result, _out) = emit(build("agent"), &root);
    result.unwrap();
    let watched: BTreeSet<PathBuf> = directives(&printed, "rerun-if-changed")
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let mut expected = BTreeSet::new();
    walk(&root.join("agent"), &mut expected);
    assert_eq!(watched, expected);
    assert!(
        watched.contains(&root.join("agent")),
        "the directory itself"
    );
    // Files the loader ignores are watched too: they may become part of the agent when renamed.
    assert!(watched.contains(&root.join("agent/.notes.md")));
    assert!(watched.contains(&root.join("agent/skills/release-notes/scripts/run.sh")));
}

#[test]
fn a_new_file_is_watched_after_the_next_run() {
    let dir = write(&[("agent/instructions.md", ROOT_AGENT)]);
    let (before, result, _out) = emit(build("agent"), dir.path());
    result.unwrap();
    assert_eq!(directives(&before, "rerun-if-changed").len(), 2); // the directory and its file
    fs::create_dir_all(dir.path().join("agent/skills")).unwrap();
    fs::write(
        dir.path().join("agent/skills/triage.md"),
        "Sort the issue into a queue.\n",
    )
    .unwrap();
    let (after, result, _out) = emit(build("agent"), dir.path());
    result.unwrap();
    let watched = directives(&after, "rerun-if-changed");
    assert!(
        watched
            .iter()
            .any(|p| p.ends_with("agent/skills/triage.md")),
        "{after}"
    );
}

#[test]
fn a_missing_directory_is_an_error_unless_optional() {
    let dir = tempfile::tempdir().unwrap();
    let (printed, result, _out) = emit(build("agent"), dir.path());
    assert!(result.is_err());
    assert_eq!(directives(&printed, "error").len(), 1, "{printed}");
    // Nothing exists to watch: cargo's default (rerun when the package changes) applies, and a
    // path that does not exist would make the script rerun on every build.
    assert!(directives(&printed, "rerun-if-changed").is_empty());

    let (_printed, result, out) = emit(build("agent").optional(), dir.path());
    result.unwrap();
    let source = fs::read_to_string(out.path().join("adam_agent.rs")).unwrap();
    assert!(source.contains("Layout::Absent"), "{source}");
    assert!(!source.contains("pub static AGENT:"), "{source}");
}

#[test]
fn the_directory_name_is_agent_or_agents_and_must_match_the_layout() {
    let dir = write(&[("agent/instructions.md", ROOT_AGENT)]);
    let (_printed, result, _out) = emit(build("prompts"), dir.path());
    let err = result.unwrap_err();
    assert!(matches!(err, BuildError::BadDir { .. }), "{err}");

    // `agents` when the package has `agent/`.
    let (printed, result, _out) = emit(build("agents"), dir.path());
    assert!(result.is_err());
    assert!(
        directives(&printed, "error")
            .iter()
            .any(|e| e.contains("expects `agents/`")),
        "{printed}"
    );
}

#[test]
fn the_build_environment_is_needed_only_where_the_builder_gives_nothing() {
    let dir = write(&[("agent/instructions.md", ROOT_AGENT)]);
    if std::env::var_os("OUT_DIR").is_none() {
        let mut sink = Vec::new();
        let err = build("agent")
            .root(dir.path())
            .emit_to(&mut sink)
            .unwrap_err();
        assert!(matches!(err, BuildError::Env { name: "OUT_DIR" }), "{err}");
        assert_eq!(format!("{err:?}"), err.to_string(), "Debug is the message");
    }
    if std::env::var_os("CARGO_MANIFEST_DIR").is_some() {
        // Cargo sets it for tests: the default root is the package, which has no `agent/`.
        let mut sink = Vec::new();
        let err = build("agent")
            .out_dir(dir.path())
            .emit_to(&mut sink)
            .unwrap_err();
        assert!(
            matches!(err, BuildError::Load(Error::Invalid { .. })),
            "{err}"
        );
    }
}

#[test]
fn a_skill_over_one_mebibyte_of_resources_is_refused() {
    let dir = write(&[
        ("agent/instructions.md", ROOT_AGENT),
        (
            "agent/skills/big/SKILL.md",
            "---\nname: big\ndescription: Carries a lot.\n---\nBody.\n",
        ),
    ]);
    fs::create_dir_all(dir.path().join("agent/skills/big/assets")).unwrap();
    fs::write(
        dir.path().join("agent/skills/big/assets/data.bin"),
        vec![0_u8; 1024 * 1024 + 1],
    )
    .unwrap();
    let (printed, result, _out) = emit(build("agent"), dir.path());
    assert!(result.is_err());
    let errors = directives(&printed, "error");
    assert_eq!(errors.len(), 1, "{printed}");
    assert!(
        errors[0].starts_with("agent/skills/big/SKILL.md"),
        "{printed}"
    );
    assert!(errors[0].contains("1 MiB"), "{printed}");
}

#[test]
fn several_agents_have_no_single_agent_static() {
    let dir = write(&[
        ("agents/alpha/instructions.md", "You are alpha.\n"),
        ("agents/beta/instructions.md", "You are beta.\n"),
    ]);
    let generated = build("agents")
        .root(dir.path())
        .crate_path("::adam_agent_fs")
        .generate()
        .unwrap();
    assert!(generated.source.contains("Layout::Multi"));
    assert!(!generated.source.contains("pub static AGENT:"));
    assert!(generated.source.contains("name: \"alpha\""));
    assert!(generated.source.contains("name: \"beta\""));
}
