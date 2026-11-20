//! `adam_code_tools::find` over a workspace that is not the coder's: the three tools through the
//! port, over `DirWorkspace` and a git repository of a test's own.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;
use std::process::Command;

use adam_code_tools::find::{find_files, list_files, search_files};
use adam_code_tools_testkit::{DirWorkspace, detached_ctx};

const SECRET: &str = "s3cret-for-the-find-test";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with `src/lib.rs` (which holds the secret) and `README.md`, as the only place.
fn workspace() -> (tempfile::TempDir, DirWorkspace) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        format!("pub fn token() -> &'static str {{\n    \"{SECRET}\"\n}}\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("README.md"), "widgets\n").unwrap();
    git(dir.path(), &["init", "--quiet"]);
    git(dir.path(), &["add", "-A"]);
    let ws = DirWorkspace::new([SECRET])
        .place("work", dir.path())
        .unwrap();
    (dir, ws)
}

#[tokio::test]
async fn the_three_tools_answer_through_the_port() {
    let (_dir, ws) = workspace();
    let ctx = detached_ctx();

    let listed = list_files(&ws, &ctx, None, None, None, None).await.unwrap();
    assert!(!listed.is_error, "{}", listed.content);
    assert!(
        listed.content.contains("src/") && listed.content.contains("README.md"),
        "{}",
        listed.content
    );

    let found = find_files(
        &ws,
        &ctx,
        "*.rs".into(),
        None,
        None,
        None,
        Some("work".into()),
    )
    .await
    .unwrap();
    assert!(found.content.contains("src/lib.rs"), "{}", found.content);
    assert!(!found.content.contains("README.md"), "{}", found.content);

    // A line is scrubbed before it is matched: the secret is neither shown nor findable.
    let searched = search_files(
        &ws,
        &ctx,
        "token".into(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        searched.content.contains("src/lib.rs:1:"),
        "{}",
        searched.content
    );
    let secret = search_files(
        &ws,
        &ctx,
        SECRET.into(),
        None,
        None,
        Some(true),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!secret.content.contains(SECRET), "{}", secret.content);
    assert!(
        !secret.content.contains("src/lib.rs:2:"),
        "the secret matched: {}",
        secret.content
    );
}

#[tokio::test]
async fn a_place_the_workspace_does_not_have_is_an_answer_for_the_model() {
    let (_dir, ws) = workspace();
    let ctx = detached_ctx();
    let out = list_files(&ws, &ctx, None, None, None, Some("elsewhere".into()))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(out.content.contains("no such place"), "{}", out.content);
}
