//! `adam_code_tools::files`' write tools over a workspace that is not the coder's: what they write,
//! when they call `after_write` (once per successful write, never for a refused one), what an `Err`
//! of the hook does, and that a place the workspace wrongly gives for a write is refused. The same
//! for `apply_patch`, in a git repository the test makes.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::Mutex;

use adam_code_tools::files::{apply_patch, edit_file, write_file};
use adam_code_tools::{CodeWorkspace, Intent, Outcome, Place, Redactor};
use adam_code_tools_testkit::{DirWorkspace, detached_ctx};
use adam_llm_agent::{ToolCtx, ToolError};
use async_trait::async_trait;

/// `DirWorkspace` with a hook that records what it was told and answers as set.
struct Recording {
    inner: DirWorkspace,
    calls: Mutex<Vec<Vec<String>>>,
    fails: bool,
    gives_read_only: bool,
}

impl Recording {
    fn over(dir: &std::path::Path) -> Self {
        Self {
            inner: DirWorkspace::new(["s3cret-recording"])
                .place("work", dir)
                .unwrap(),
            calls: Mutex::default(),
            fails: false,
            gives_read_only: false,
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl CodeWorkspace for Recording {
    async fn place(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
        intent: Intent,
    ) -> Result<Place, Outcome> {
        let place = CodeWorkspace::place(&self.inner, ctx, repo, Intent::Read).await?;
        // A broken workspace: a read-only place for a write, which the tools must refuse.
        let _ = intent;
        Ok(if self.gives_read_only {
            place.read_only()
        } else {
            place
        })
    }

    fn redactor(&self) -> &Redactor {
        self.inner.redactor()
    }

    async fn after_write(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        paths: &[String],
    ) -> Result<String, ToolError> {
        self.calls.lock().unwrap().push(paths.to_vec());
        if self.fails {
            Err(ToolError::Transient("the hook failed".into()))
        } else {
            Ok(" And then.".into())
        }
    }
}

#[tokio::test]
async fn a_write_calls_the_hook_once_and_appends_what_it_says() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Recording::over(dir.path());
    let ctx = detached_ctx();

    let out = write_file(&ws, &ctx, "src/a.txt".into(), "one\n".into(), None)
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "Created src/a.txt (4 bytes). And then.");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/a.txt")).unwrap(),
        "one\n"
    );

    let out = edit_file(
        &ws,
        &ctx,
        "src/a.txt".into(),
        "one".into(),
        "two".into(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.ends_with(". And then."), "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/a.txt")).unwrap(),
        "two\n"
    );

    assert_eq!(
        ws.calls(),
        vec![vec!["src/a.txt".to_owned()], vec!["src/a.txt".to_owned()]]
    );
}

#[tokio::test]
async fn a_refused_write_or_edit_does_not_call_the_hook() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let ws = Recording::over(dir.path());
    let ctx = detached_ctx();

    let out = write_file(&ws, &ctx, ".git/config".into(), "x".into(), None)
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    let out = edit_file(
        &ws,
        &ctx,
        "a.txt".into(),
        "absent".into(),
        "x".into(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(ws.calls().is_empty(), "{:?}", ws.calls());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\n"
    );
}

#[tokio::test]
async fn a_failing_hook_is_the_tools_error_and_the_file_stays_written() {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = Recording::over(dir.path());
    ws.fails = true;
    let ctx = detached_ctx();
    let err = write_file(&ws, &ctx, "b.txt".into(), "kept\n".into(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Transient(_)), "{err:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
        "kept\n"
    );
}

#[tokio::test]
async fn a_read_only_place_given_for_a_write_is_refused_and_nothing_is_written() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    let mut ws = Recording::over(dir.path());
    ws.gives_read_only = true;
    let ctx = detached_ctx();

    let out = write_file(&ws, &ctx, "c.txt".into(), "x".into(), None)
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("can be read, not written"),
        "{}",
        out.content
    );
    assert!(!dir.path().join("c.txt").exists());
    let out = edit_file(
        &ws,
        &ctx,
        "a.txt".into(),
        "one".into(),
        "two".into(),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(ws.calls().is_empty());
}

/// A git repository with `a.txt` (`one\ntwo\n`), for `apply_patch`.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
    dir
}

const PATCH: &str = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+three\n";

#[tokio::test]
async fn a_patch_is_applied_and_calls_the_hook_once_with_the_paths_it_changed() {
    let dir = repository();
    let ws = Recording::over(dir.path());
    let ctx = detached_ctx();

    let out = apply_patch(&ws, &ctx, PATCH.into(), None).await.unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "Applied the patch to 1 file(s): a.txt. And then."
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\nthree\n"
    );
    assert_eq!(ws.calls(), vec![vec!["a.txt".to_owned()]]);
}

#[tokio::test]
async fn a_patch_that_does_not_apply_is_an_error_result_and_does_not_call_the_hook() {
    let dir = repository();
    std::fs::write(dir.path().join("a.txt"), "other\n").unwrap();
    let ws = Recording::over(dir.path());
    let ctx = detached_ctx();

    let out = apply_patch(&ws, &ctx, PATCH.into(), None).await.unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("the patch does not apply"),
        "{}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "other\n"
    );
    assert!(ws.calls().is_empty(), "{:?}", ws.calls());
}

#[tokio::test]
async fn a_patch_to_a_read_only_place_is_refused_and_nothing_is_changed() {
    let dir = repository();
    let mut ws = Recording::over(dir.path());
    ws.gives_read_only = true;
    let ctx = detached_ctx();

    let out = apply_patch(&ws, &ctx, PATCH.into(), None).await.unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(
        out.content.contains("can be read, not written"),
        "{}",
        out.content
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(ws.calls().is_empty());
}
