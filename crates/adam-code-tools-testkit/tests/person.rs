//! `PersonDir` — the host's own implementation of the two ports — over the conformance cases, and
//! the two things this slice is about: **nothing is restored** after a command (a person's
//! directory is never undone) and **a command's change list names the file it wrote**.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use adam_code_tools::person::PLACE;
use adam_code_tools::{CodeWorkspace, CommandWorkspace, Guarding, PersonDir};
use adam_code_tools_testkit::{
    CommandHarness, Harness, code_workspace_conformance, command_workspace_conformance,
    detached_ctx,
};
use tempfile::TempDir;

const SECRET: &str = "s3cret-person-dir";

/// A directory of its own for each case, kept alive by the harness, and a workspace over it.
fn workspace(kept: &TempDir) -> (Arc<PersonDir>, PathBuf) {
    let root = kept.path().join("person");
    fs::create_dir_all(&root).unwrap();
    let workspace = Arc::new(PersonDir::new(&root, [SECRET]).unwrap());
    (workspace, root)
}

fn harness(kept: TempDir) -> Harness {
    let (workspace, _) = workspace(&kept);
    Harness {
        workspace,
        ctx: detached_ctx(),
        writable: PLACE.to_owned(),
        read_only: None,
        secret: SECRET.to_owned(),
        alive: Box::new(kept),
    }
}

fn command_harness(kept: TempDir) -> CommandHarness {
    let (workspace, _) = workspace(&kept);
    CommandHarness {
        workspace,
        ctx: detached_ctx(),
        writable: PLACE.to_owned(),
        secret: SECRET.to_owned(),
        alive: Box::new(kept),
    }
}

code_workspace_conformance!({
    let kept = TempDir::new().unwrap();
    harness(kept)
});

command_workspace_conformance!({
    let kept = TempDir::new().unwrap();
    command_harness(kept)
});

/// The slice's first claim: a command's change stays. The guard settles `FilesChanged` and the file
/// is still there afterwards — a `Restored`/`Undone` verdict here would be a workspace that owns
/// what is the person's.
#[tokio::test]
async fn a_command_that_writes_leaves_the_file_and_is_never_undone() {
    let kept = TempDir::new().unwrap();
    let (workspace, root) = workspace(&kept);
    let ctx = detached_ctx();
    let place = workspace
        .place(&ctx, None, adam_code_tools::Intent::Write)
        .await
        .unwrap();

    let guard = workspace.guard(&ctx, &place, Guarding::Make).await.unwrap();
    fs::write(root.join("notes.md"), "what the person asked for\n").unwrap();
    let settled = guard.settle().await;

    assert_eq!(settled, adam_code_tools::Settled::FilesChanged);
    assert_eq!(
        fs::read_to_string(root.join("notes.md")).unwrap(),
        "what the person asked for\n",
        "a person's directory is never restored"
    );
}

/// The slice's second claim: the report names the files the command changed, added and removed.
#[tokio::test]
async fn a_commands_change_list_names_the_files_it_wrote_changed_and_removed() {
    let kept = TempDir::new().unwrap();
    let (workspace, root) = workspace(&kept);
    let ctx = detached_ctx();
    let place = workspace
        .place(&ctx, None, adam_code_tools::Intent::Write)
        .await
        .unwrap();

    fs::write(root.join("kept.txt"), "one\n").unwrap();
    fs::write(root.join("gone.txt"), "two\n").unwrap();

    let guard = workspace.guard(&ctx, &place, Guarding::Make).await.unwrap();
    fs::write(root.join("kept.txt"), "one\ntwo\n").unwrap();
    fs::write(root.join("added.txt"), "three\n").unwrap();
    fs::remove_file(root.join("gone.txt")).unwrap();
    assert_eq!(guard.settle().await, adam_code_tools::Settled::FilesChanged);

    let report = workspace.after_run(&ctx, &place).await.unwrap();
    assert!(report.contains("added.txt"), "the file it wrote: {report}");
    assert!(report.contains("kept.txt"), "the file it changed: {report}");
    assert!(report.contains("gone.txt"), "the file it removed: {report}");

    // The report is read once: a second call says nothing, so an abandoned command cannot leak into
    // the next one.
    let again = workspace.after_run(&ctx, &place).await.unwrap();
    assert_eq!(again, "");
}

/// A command that changes nothing says nothing, and settles `Unchanged`.
#[tokio::test]
async fn a_command_that_changes_nothing_reports_nothing() {
    let kept = TempDir::new().unwrap();
    let (workspace, root) = workspace(&kept);
    let ctx = detached_ctx();
    let place = workspace
        .place(&ctx, None, adam_code_tools::Intent::Write)
        .await
        .unwrap();
    fs::write(root.join("kept.txt"), "one\n").unwrap();

    let guard = workspace.guard(&ctx, &place, Guarding::Look).await.unwrap();
    assert_eq!(guard.settle().await, adam_code_tools::Settled::Unchanged);
    assert_eq!(workspace.after_run(&ctx, &place).await.unwrap(), "");
}

/// A repository's `.git` is not the person's change: a `git init` inside the place moves nothing in
/// the report.
#[tokio::test]
async fn the_repository_bookkeeping_is_not_a_change() {
    let kept = TempDir::new().unwrap();
    let (workspace, root) = workspace(&kept);
    let ctx = detached_ctx();
    let place = workspace
        .place(&ctx, None, adam_code_tools::Intent::Write)
        .await
        .unwrap();

    let guard = workspace.guard(&ctx, &place, Guarding::Make).await.unwrap();
    fs::create_dir_all(root.join(".git/objects")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    assert_eq!(guard.settle().await, adam_code_tools::Settled::Unchanged);
    assert_eq!(workspace.after_run(&ctx, &place).await.unwrap(), "");
}
