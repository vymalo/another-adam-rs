//! Run-time folders (no feature): `AgentFolder::load` reads the one agent of a folder when the
//! process starts. The fixture of `adam-agent-fixture` is both embedded and read from disk, so
//! the two ways to the same files are compared; the rest is written to a temp dir.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::Path;

use adam_agent_fixture::AGENT;
use adam_agent_fs::Severity;
use adam_assembly::{AgentDef, AgentFolder, Error};
use adam_error::{Classify, ErrorClass};
use adam_llm_agent::ToolSet;
use common::{instructions, tools, write};

/// The directory the fixture is embedded from.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../adam-agent-fs/tests/fixtures/valid"
);

fn helper(body: &str) -> String {
    named("helper", body)
}

fn named(name: &str, body: &str) -> String {
    instructions(&format!("name: {name}\ndescription: Helps."), body)
}

#[test]
fn the_fixture_folder_loads_with_the_digest_of_the_embedded_copy() {
    let folder = AgentFolder::load(FIXTURE).unwrap();
    assert_eq!(folder.def.name(), "coder");
    assert_eq!(folder.root, Path::new(FIXTURE));
    assert!(folder.warnings.is_empty(), "{:?}", folder.warnings);
    // The same files give the same digest wherever they were read from, resources included.
    assert_eq!(folder.digest, AGENT.digest);
    let embedded = AgentDef::from_manifest(AGENT).unwrap();
    assert_eq!(folder.def.manifest(), embedded.manifest());
}

#[test]
fn a_path_is_the_root_or_the_agent_directory() {
    let from_root = AgentFolder::load(FIXTURE).unwrap();
    let from_agent = AgentFolder::load(format!("{FIXTURE}/agent")).unwrap();
    assert_eq!(from_agent.root, from_root.root);
    assert_eq!(from_agent.digest, from_root.digest);
    assert_eq!(from_agent.def.name(), "coder");
}

#[test]
fn the_loaded_definition_binds_and_renders_like_any_other() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[(
            "agent/instructions.md",
            &instructions(
                "name: helper\ndescription: Helps.\nvars:\n  tone: plain",
                "Answer in a {{tone}} style.",
            ),
        )],
    );
    let folder = AgentFolder::load(dir.path()).unwrap();
    let assembly = folder
        .def
        .var("tone", "formal")
        .bind(ToolSet::new())
        .unwrap()
        .model(std::sync::Arc::new(adam_model::MockModel::new()), "alias")
        .unwrap();
    assert_eq!(assembly.info()[0].prompt, "Answer in a formal style.");
}

#[test]
fn an_edited_file_gives_another_digest_and_another_prompt() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &[("agent/instructions.md", &helper("One."))]);
    let before = AgentFolder::load(dir.path()).unwrap();
    let again = AgentFolder::load(dir.path()).unwrap();
    assert_eq!(
        before.digest, again.digest,
        "the same files, the same digest"
    );

    write(dir.path(), &[("agent/instructions.md", &helper("Two."))]);
    let after = AgentFolder::load(dir.path()).unwrap();
    assert_ne!(before.digest, after.digest);
    let prompt = |folder: AgentFolder| {
        let body = folder.def.manifest().instructions.body.clone();
        folder.def.bind(ToolSet::new()).unwrap();
        body
    };
    assert_eq!(prompt(before), "One.");
    assert_eq!(prompt(after), "Two.");
}

#[test]
fn the_bytes_of_a_bundled_file_are_part_of_the_digest() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("agent/instructions.md", &helper("Hi.")),
            (
                "agent/skills/style/SKILL.md",
                "---\nname: style\ndescription: How to write.\n---\nRead the guide.\n",
            ),
            ("agent/skills/style/references/guide.md", "Short sentences."),
        ],
    );
    let before = AgentFolder::load(dir.path()).unwrap().digest;
    write(
        dir.path(),
        &[("agent/skills/style/references/guide.md", "Long sentences.")],
    );
    let after = AgentFolder::load(dir.path()).unwrap().digest;
    assert_ne!(before, after);
}

#[test]
fn warnings_are_returned_and_do_not_stop_the_load() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[(
            "agent/instructions.md",
            &instructions(
                "name: helper\ndescription: Helps.\nfavourite_colour: green",
                "Hi.",
            ),
        )],
    );
    let folder = AgentFolder::load(dir.path()).unwrap();
    assert_eq!(folder.def.name(), "helper");
    assert!(
        folder
            .warnings
            .iter()
            .any(|w| w.severity == Severity::Warning
                && w.path == Path::new("agent/instructions.md")
                && w.message.contains("favourite_colour")),
        "{:?}",
        folder.warnings
    );
}

#[test]
fn a_folder_with_an_error_is_refused_with_every_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("agent/instructions.md", &helper("Hi.")),
            // A subagent without a description, and a frontmatter that is not YAML.
            ("agent/subagents/broken.md", "---\n: : [\n---\nSub.\n"),
        ],
    );
    let error = AgentFolder::load(dir.path()).unwrap_err();
    let Error::Manifest(adam_agent_fs::Error::Invalid { diagnostics }) = &error else {
        panic!("not a manifest error: {error}");
    };
    assert!(
        diagnostics
            .iter()
            .any(|d| d.is_error() && d.path == Path::new("agent/subagents/broken.md")),
        "{diagnostics:?}"
    );
    assert!(error.to_string().contains("broken.md"), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);
}

#[test]
fn a_folder_that_is_not_there_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let error = AgentFolder::load(dir.path().join("nowhere")).unwrap_err();
    assert!(
        matches!(error, Error::Manifest(adam_agent_fs::Error::Io { .. })),
        "{error}"
    );
    assert_eq!(error.class(), ErrorClass::NotFound);

    // A directory with neither `agent/` nor `agents/` is an error too, not an empty package.
    let empty = AgentFolder::load(dir.path()).unwrap_err();
    assert!(matches!(empty, Error::Manifest(_)), "{empty}");

    // And so is a file.
    write(dir.path(), &[("file.txt", "x")]);
    let file = AgentFolder::load(dir.path().join("file.txt")).unwrap_err();
    assert!(matches!(file, Error::Manifest(_)), "{file}");
}

#[test]
fn several_agents_are_refused_and_named() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("agents/one/instructions.md", &named("one", "One.")),
            ("agents/two/instructions.md", &named("two", "Two.")),
        ],
    );
    let error = AgentFolder::load(dir.path()).unwrap_err();
    let Error::NotOneAgent { root, found } = &error else {
        panic!("not NotOneAgent: {error}");
    };
    assert_eq!(root, dir.path());
    assert_eq!(found, &["one", "two"]);
    assert!(error.to_string().contains("2 agents"), "{error}");
    assert_eq!(error.class(), ErrorClass::Invalid);
    // `agents/` itself names the same folder.
    assert!(matches!(
        AgentFolder::load(dir.path().join("agents")),
        Err(Error::NotOneAgent { .. })
    ));
}

#[test]
fn an_agents_folder_with_one_agent_is_one_agent() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[("agents/only/instructions.md", &named("only", "Only."))],
    );
    let folder = AgentFolder::load(dir.path()).unwrap();
    assert_eq!(folder.def.name(), "only");
}

/// An `agents/` with nothing in it is already an error of the files (the loader says so), so
/// `NotOneAgent { found: [] }` is only a guard.
#[test]
fn an_agents_folder_with_no_agent_is_refused_by_the_loader() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("agents")).unwrap();
    let error = AgentFolder::load(dir.path()).unwrap_err();
    assert!(matches!(error, Error::Manifest(_)), "{error}");
    assert!(error.to_string().contains("holds no agent"), "{error}");
}

/// `AgentDef::from_dir` needs no feature either, and takes a root or the `agent/` directory.
#[test]
fn from_dir_reads_the_agents_of_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("agent/instructions.md", &helper("Hi.")),
            (
                "agent/subagents/sub.md",
                "---\ndescription: The sub.\n---\nSub.\n",
            ),
        ],
    );
    for path in [dir.path().to_path_buf(), dir.path().join("agent")] {
        let defs = AgentDef::from_dir(path).unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name(), "helper");
    }
    let error = AgentDef::from_dir(dir.path().join("nowhere")).unwrap_err();
    assert!(matches!(error, Error::Manifest(_)), "{error}");
}

/// What `bind` says about a folder's tools is the same as for any definition: a folder that names
/// a tool the process does not register is a startup error naming the file.
#[test]
fn binding_a_loaded_folder_checks_its_tools() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[(
            "agent/instructions.md",
            &instructions("name: helper\ndescription: Helps.\ntools: [clok]", "Hi."),
        )],
    );
    let folder = AgentFolder::load(dir.path()).unwrap();
    let error = folder.def.bind(tools(&["clock"])).unwrap_err();
    assert!(matches!(error, Error::UnknownTool { .. }), "{error}");
    assert!(
        error.to_string().contains("did you mean `clock`"),
        "{error}"
    );
}
