//! The tools against the **real** language servers (ADR 0044 slice 17), on five tiny projects with
//! one deliberate error each (`tests/fixtures/language-servers/`): Rust, TypeScript, Python, Go and
//! Dart.
//!
//! Gated, like the tests that need a database (Rule 7): without `ADAM_TEST_LANGUAGE_SERVERS=1` they
//! do nothing and say so; with it, a language whose server is not on `PATH` is skipped, and
//! `ADAM_TEST_REQUIRE_LANGUAGE_SERVERS=1` turns every skip into a failure (it implies the gate). **CI
//! runs them in the built image**: the `image` job of `.github/workflows/coder.yml` sets both
//! variables, so a server the workspace base lacks fails the job. What is proved on every run is
//! `language_servers.rs`, over a scripted server.
//!
//! ```sh
//! ADAM_TEST_LANGUAGE_SERVERS=1 cargo test --workspace --locked -- real_
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::path::Path;
use std::sync::Arc;

use adam_coder::coder_tools;
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_llm_agent::{ToolCtx, ToolOutput};
use adam_runtime::CollectingSink;
use common::Fixture;
use serde_json::{Value, json};

const GATE: &str = "ADAM_TEST_LANGUAGE_SERVERS";
const REQUIRE: &str = "ADAM_TEST_REQUIRE_LANGUAGE_SERVERS";

/// Whether to run the test for the server `program`: `false` (and a line saying why) for a skip,
/// a panic for a skip that `REQUIRE` forbids.
fn run_for(program: &str) -> bool {
    let required = std::env::var(REQUIRE).is_ok_and(|v| v == "1");
    if !required && !std::env::var(GATE).is_ok_and(|v| v == "1") {
        eprintln!("skipped: set {GATE}=1 to run the real language servers");
        return false;
    }
    let found = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()));
    if found {
        return true;
    }
    assert!(
        !required,
        "{REQUIRE}=1 and `{program}` is not on PATH: the real language server test cannot be skipped"
    );
    eprintln!("skipped: `{program}` is not on PATH");
    false
}

struct Real {
    fx: Fixture,
    ctx: ToolCtx,
}

impl Real {
    /// A workspace whose repository holds the fixture project `name`.
    async fn new(name: &str) -> Self {
        let fx = Fixture::new().await;
        let ctx = ToolCtx::detached("tool", "call-1", Arc::new(CollectingSink::new()))
            .with_state(fx.env.clone());
        let rig = Self { fx, ctx };
        let run = rig.ctx.run_id().to_string();
        let mut notes = rig.fx.env.notes.load(&run).await.unwrap();
        notes.name_repos(named_in(
            &without_untrusted(&rig.fx.remote_url()),
            &rig.fx.env.settings.default_repo_host,
        ));
        rig.fx.env.notes.save(&run, &notes).await.unwrap();
        let made = rig
            .call(
                "prepare_workspace",
                json!({"repo_url": rig.fx.remote_url(), "base_branch": "main"}),
            )
            .await;
        assert!(!made.is_error, "{}", made.content);
        let from = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/language-servers")
            .join(name);
        copy(&from, &common::slot_dir(&rig.fx.root, &run));
        rig
    }

    async fn call(&self, name: &str, args: Value) -> ToolOutput {
        let tools = coder_tools(&self.fx.env);
        tools
            .get(name)
            .expect("the tool is in the set")
            .call(&self.ctx, args)
            .await
            .expect("a result, not a failure")
    }
}

fn copy(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// What every language is asked: the error is reported at its line, the definition of `add` is
/// found from the call, and the references of `add` hold the declaration and the call.
async fn check(project: &str, file: &str, error_line: u32, call_line: u32, definition: &str) {
    let real = Real::new(project).await;
    let diagnostics = real
        .call("file_diagnostics", json!({"path": file, "wait": 120}))
        .await;
    assert!(!diagnostics.is_error, "{}", diagnostics.content);
    assert!(
        diagnostics
            .content
            .contains(&format!("{file}:{error_line}:")),
        "the error of line {error_line} is reported: {}",
        diagnostics.content
    );
    assert!(
        diagnostics.content.contains("error"),
        "{}",
        diagnostics.content
    );
    let found = real
        .call(
            "find_definition",
            json!({"path": file, "line": call_line, "symbol": "add", "wait": 120}),
        )
        .await;
    assert!(!found.is_error, "{}", found.content);
    assert!(found.content.contains(definition), "{}", found.content);
    let used = real
        .call(
            "find_references",
            json!({"path": file, "line": call_line, "symbol": "add", "wait": 120}),
        )
        .await;
    assert!(!used.is_error, "{}", used.content);
    assert!(
        used.content.contains(&format!("{file}:{call_line}:")),
        "{}",
        used.content
    );
    // No path of this machine in any of it.
    let root = real.fx.root.display().to_string();
    for out in [&diagnostics, &found, &used] {
        assert!(!out.content.contains(&root), "{}", out.content);
    }
    real.fx
        .env
        .commands
        .stop_run(&real.ctx.run_id().to_string(), "the test is over")
        .await;
}

#[tokio::test]
async fn real_rust_analyzer_reports_an_error_and_finds_a_definition_and_references() {
    if run_for("rust-analyzer") {
        check("rust", "src/lib.rs", 6, 6, "src/lib.rs:1:8").await;
    }
}

#[tokio::test]
async fn real_typescript_language_server_reports_an_error_and_finds_a_definition_and_references() {
    if run_for("typescript-language-server") {
        check("typescript", "src/index.ts", 5, 5, "src/index.ts:1:17").await;
    }
}

#[tokio::test]
async fn real_pyright_reports_an_error_and_finds_a_definition_and_references() {
    if run_for("pyright-langserver") {
        check("python", "main.py", 5, 5, "main.py:1:5").await;
    }
}

#[tokio::test]
async fn real_gopls_reports_an_error_and_finds_a_definition_and_references() {
    if run_for("gopls") {
        check("go", "main.go", 8, 8, "main.go:3:6").await;
    }
}

#[tokio::test]
async fn real_dart_language_server_reports_an_error_and_finds_a_definition_and_references() {
    if run_for("dart") {
        check("dart", "lib/main.dart", 4, 4, "lib/main.dart:1:5").await;
    }
}
