//! The command cases over the reference implementation, `DirWorkspace`.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use adam_code_tools::{CommandLimits, OutputDefaults};

use adam_code_tools_testkit::{CommandHarness, DirWorkspace, detached_ctx};

const SECRET: &str = "s3cret-dir-commands";

/// The directory outlives every test of the binary.
fn dir() -> &'static tempfile::TempDir {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap())
}

fn harness() -> CommandHarness {
    let workspace = DirWorkspace::new([SECRET])
        .place("work", dir().path())
        .unwrap()
        // A small view, so that a long output is a few hundred lines.
        .with_limits(CommandLimits {
            timeout: Duration::from_secs(20),
            output_tail: 2048,
            output: OutputDefaults::TOOLS,
        });
    CommandHarness {
        workspace: Arc::new(workspace),
        ctx: detached_ctx(),
        writable: "work".into(),
        secret: SECRET.into(),
        alive: Box::new(()),
    }
}

adam_code_tools_testkit::command_workspace_conformance!(harness());
