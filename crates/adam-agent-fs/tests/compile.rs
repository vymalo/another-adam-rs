//! The generated source compiles, and runs to the same manifest the directory gives (trybuild).
//!
//! For each package this test generates the source (`build(..).generate()`), appends a `main`,
//! and has trybuild compile and run it as a small program that depends on this crate. The
//! program is lint-strict, so the generated code must be clean under `deny(warnings)`. The
//! facade path (`::adam::agent_fs`, the default) is proved by the `adam-agent-fixture` crate,
//! which uses a real `build.rs`.
#![cfg(feature = "build")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::fs;
use std::path::Path;

use adam_agent_fs::build;

/// The prelude of every program: strict lints, so generated code cannot warn.
const HEAD: &str = "//! A program made of generated code.\n\
                    #![deny(warnings, missing_docs, unreachable_pub, unused_qualifications)]\n";

fn program(dir: &Path, name: &str, root: &Path, agent_dir: &str, tail: &str) -> std::path::PathBuf {
    let generated = build(agent_dir)
        .root(root)
        .name("fixture-agent")
        .crate_path("::adam_agent_fs")
        .generate()
        .unwrap();
    let path = dir.join(format!("{name}.rs"));
    fs::write(
        &path,
        format!(
            "{HEAD}{}\nuse adam_agent_fs::{{Dir, ManifestSource, Strictness}};\n\
             const ROOT: &str = {:?};\n{tail}",
            generated.source,
            root.to_str().unwrap()
        ),
    )
    .unwrap();
    path
}

#[test]
fn generated_source_compiles_and_equals_the_directory() {
    // cargo-llvm-cov instruments every build that inherits its flags, and trybuild builds a
    // project of its own: slow, and nothing here measures the generated code.
    if std::env::var_os("CARGO_LLVM_COV").is_some() {
        eprintln!("skipped under cargo-llvm-cov");
        return;
    }
    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("generated-programs");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).unwrap();

    // One agent, everything in it: skills with resources, nested and remote subagents, mcp.json,
    // schedules, an instructions part.
    let valid = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/valid");
    program(
        &work,
        "single",
        &valid,
        "agent",
        "fn main() {\n\
         let dir = Dir::new(ROOT).default_name(\"fixture-agent\");\n\
         let from_dir = dir.load().unwrap().into_package(Strictness::Strict).unwrap();\n\
         assert_eq!(PACKAGE.load().unwrap().package, from_dir);\n\
         assert_eq!(AGENT.name, \"coder\");\n\
         assert_eq!(AGENTS.len(), 1);\n\
         for (embedded, owned) in AGENTS.iter().zip(&from_dir.agents) {\n\
             assert_eq!(dir.digest(owned).unwrap(), embedded.digest);\n\
             assert!(embedded.verify().unwrap());\n\
         }\n\
         }\n",
    );

    // Several agents: `AGENTS` and `PACKAGE`, no `AGENT`.
    let multi = work.join("multi-root");
    for (name, prompt) in [("alpha", "You are alpha."), ("beta", "You are beta.")] {
        let dir = multi.join("agents").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("instructions.md"), format!("{prompt}\n")).unwrap();
    }
    program(
        &work,
        "multi",
        &multi,
        "agents",
        "fn main() {\n\
         let from_dir = Dir::new(ROOT).load().unwrap().into_package(Strictness::Strict).unwrap();\n\
         assert_eq!(PACKAGE.load().unwrap().package, from_dir);\n\
         let names: Vec<_> = AGENTS.iter().map(|a| a.name).collect();\n\
         assert_eq!(names, [\"alpha\", \"beta\"]);\n\
         }\n",
    );

    // Nothing to embed (`optional`): an empty `AGENTS`.
    let empty = work.join("empty-root");
    fs::create_dir_all(&empty).unwrap();
    let generated = build("agent")
        .root(&empty)
        .optional()
        .crate_path("::adam_agent_fs")
        .generate()
        .unwrap();
    fs::write(
        work.join("absent.rs"),
        format!(
            "{HEAD}{}\nfn main() {{\n\
             assert!(AGENTS.is_empty());\n\
             assert!(PACKAGE.to_package().unwrap().agents.is_empty());\n\
             }}\n",
            generated.source
        ),
    )
    .unwrap();

    let t = trybuild::TestCases::new();
    t.pass(work.join("single.rs"));
    t.pass(work.join("multi.rs"));
    t.pass(work.join("absent.rs"));
}
