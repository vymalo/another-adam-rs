//! `adam_code_tools::files` over a workspace that is not the coder's: `read_file` and `read_files`
//! through the port, over `DirWorkspace` and a git repository of a test's own.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;
use std::process::Command;

use adam_code_tools::files::{read_file, read_files};
use adam_code_tools_testkit::{DirWorkspace, detached_ctx};

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

/// A repository with `src/lib.rs` (five numbered lines) and `README.md`, as the only place, and a
/// file beside it that the place does not hold.
fn workspace() -> (tempfile::TempDir, DirWorkspace) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("work");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let lines: String = (1..=5).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.join("src/lib.rs"), lines).unwrap();
    std::fs::write(dir.join("README.md"), "widgets\n").unwrap();
    std::fs::write(tmp.path().join("outside.txt"), "not the place's\n").unwrap();
    std::os::unix::fs::symlink(tmp.path().join("outside.txt"), dir.join("out.txt")).unwrap();
    git(&dir, &["init", "--quiet"]);
    git(&dir, &["add", "-A"]);
    let ws = DirWorkspace::new(["unused-secret-of-the-files-test"])
        .place("work", &dir)
        .unwrap();
    (tmp, ws)
}

#[tokio::test]
async fn read_file_gives_a_page_of_a_file() {
    let (_tmp, ws) = workspace();
    let ctx = detached_ctx();
    let out = read_file(
        &ws,
        &ctx,
        "src/lib.rs".into(),
        Some(2),
        Some(2),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "     2\tline 2\n     3\tline 3\n[more: 2 lines left; next page: offset=4]"
    );
    // The old names, in the place named.
    let out = read_file(
        &ws,
        &ctx,
        "src/lib.rs".into(),
        None,
        None,
        Some(5),
        Some(5),
        Some("work".into()),
    )
    .await
    .unwrap();
    assert_eq!(out.content, "     5\tline 5\n[end: 5 lines]");
}

#[tokio::test]
async fn a_missing_file_names_the_closest_path() {
    let (_tmp, ws) = workspace();
    let ctx = detached_ctx();
    let out = read_file(&ws, &ctx, "src/lbi.rs".into(), None, None, None, None, None)
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "`src/lbi.rs` does not exist. Closest paths: `src/lib.rs`"
    );
}

#[tokio::test]
async fn read_files_shows_two_files_in_one_result() {
    let (_tmp, ws) = workspace();
    let ctx = detached_ctx();
    let out = read_files(
        &ws,
        &ctx,
        vec!["README.md".into(), "src/lib.rs".into()],
        None,
    )
    .await
    .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "== README.md ==\n     1\twidgets\n[end: 1 lines]\n\n\
         == src/lib.rs ==\n     1\tline 1\n     2\tline 2\n     3\tline 3\n     4\tline 4\n     5\tline 5\n\
         [end: 5 lines]"
    );
}

#[tokio::test]
async fn a_path_that_leaves_the_place_is_refused() {
    let (_tmp, ws) = workspace();
    let ctx = detached_ctx();
    for path in ["../outside.txt", "out.txt"] {
        let out = read_file(&ws, &ctx, path.into(), None, None, None, None, None)
            .await
            .unwrap();
        assert!(out.is_error, "{path}: {}", out.content);
        assert!(!out.content.contains("not the place's"), "{}", out.content);
    }
    // In `read_files`, each is one line and the file that is inside is still shown.
    let out = read_files(
        &ws,
        &ctx,
        vec![
            "../outside.txt".into(),
            "out.txt".into(),
            "README.md".into(),
        ],
        None,
    )
    .await
    .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "`../outside.txt` goes up with `..`: a path must stay inside the worktree\n\n\
         out.txt: leads outside the worktree (through a symlink): refused\n\n\
         == README.md ==\n     1\twidgets\n[end: 1 lines]"
    );
}
