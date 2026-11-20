//! The cases over the reference implementation, `DirWorkspace`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, OnceLock};

use adam_code_tools_testkit::{DirWorkspace, Harness, detached_ctx};

const SECRET: &str = "s3cret-dir-workspace";

/// The directories outlive every test of the binary.
fn dirs() -> &'static (tempfile::TempDir, tempfile::TempDir) {
    static DIRS: OnceLock<(tempfile::TempDir, tempfile::TempDir)> = OnceLock::new();
    DIRS.get_or_init(|| (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()))
}

fn harness() -> Harness {
    let (work, reference) = dirs();
    let workspace = DirWorkspace::new([SECRET])
        .place("work", work.path())
        .unwrap()
        .read_only_place("reference", reference.path())
        .unwrap();
    Harness {
        workspace: Arc::new(workspace),
        ctx: detached_ctx(),
        writable: "work".into(),
        read_only: Some("reference".into()),
        secret: SECRET.into(),
        // The directories are the binary's own (`dirs`): nothing else to keep.
        alive: Box::new(()),
    }
}

adam_code_tools_testkit::code_workspace_conformance!(harness());
