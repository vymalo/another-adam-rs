//! What `write_file`, `edit_file` and `apply_patch` answer, to the byte: the sentence, the tail every
//! write adds and, in a scratch project that was published, the note that the change does not reach
//! the repository. And that every write moves the run's check epoch by one.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_coder::tools::files::{ApplyPatch, EditFile, WriteFile};
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::scratch::{PublishScratch, StartScratch};
use adam_llm_agent::{Tool, ToolCtx, ToolOutput};
use adam_runtime::{CollectingSink, RunEvent};
use common::Fixture;
use serde_json::{Value, json};

/// The tail every write adds today.
const TAIL: &str = " Run the checks again before you commit.";

struct Rig {
    fx: Fixture,
    sink: CollectingSink,
    ctx: ToolCtx,
}

impl Rig {
    async fn new() -> Self {
        let fx = Fixture::new().await;
        let sink = CollectingSink::new();
        let ctx =
            ToolCtx::detached("tool", "call-1", Arc::new(sink.clone())).with_state(fx.env.clone());
        Self { fx, sink, ctx }
    }

    /// The person says `text`: what the agent records in the run notes before each step.
    async fn say(&self, text: &str) {
        let run = self.ctx.run_id().to_string();
        let mut notes = self.fx.env.notes.load(&run).await.unwrap();
        notes.name_repos(named_in(
            &without_untrusted(text),
            &self.fx.env.settings.default_repo_host,
        ));
        self.fx.env.notes.save(&run, &notes).await.unwrap();
    }

    /// The fixture's remote, prepared: one slot, `remote`.
    async fn prepared() -> Self {
        let rig = Self::new().await;
        rig.say(&rig.fx.remote_url()).await;
        let out = PrepareWorkspace
            .call(
                &rig.ctx,
                json!({"repo_url": rig.fx.remote_url(), "base_branch": "main"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        rig
    }

    fn worktree(&self) -> std::path::PathBuf {
        common::slot_dir(&self.fx.root, &self.ctx.run_id().to_string())
    }

    async fn epoch(&self) -> u64 {
        let run = self.ctx.run_id().to_string();
        self.fx.env.notes.load(&run).await.unwrap().checks.epoch
    }

    /// What the tools said with `emit_progress`, after `prepare_workspace`'s own line.
    fn progress(&self) -> Vec<String> {
        self.sink
            .events()
            .into_iter()
            .filter_map(|e| match e.event {
                RunEvent::Step(step) if step.parent.is_none() => step.detail,
                _ => None,
            })
            .filter(|p| !p.starts_with("preparing a worktree of "))
            .collect()
    }

    /// `tool` called with `args`: an answer that is not an error, and the epoch moved by one.
    async fn wrote(&self, tool: &dyn Tool, args: Value) -> String {
        let before = self.epoch().await;
        let out: ToolOutput = tool.call(&self.ctx, args).await.unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(self.epoch().await, before + 1, "{}", out.content);
        out.content
    }
}

#[tokio::test]
async fn write_file_says_created_then_replaced_with_the_tail() {
    let rig = Rig::prepared().await;
    assert_eq!(rig.epoch().await, 0);
    let created = rig
        .wrote(
            &WriteFile,
            json!({"path": "src/new.txt", "content": "hello\n"}),
        )
        .await;
    assert_eq!(created, format!("Created src/new.txt (6 bytes).{TAIL}"));
    let replaced = rig
        .wrote(
            &WriteFile,
            json!({"path": "src/new.txt", "content": "hi\n"}),
        )
        .await;
    assert_eq!(replaced, format!("Replaced src/new.txt (3 bytes).{TAIL}"));
    assert_eq!(rig.epoch().await, 2);
    assert_eq!(
        rig.progress(),
        ["wrote src/new.txt (remote)", "wrote src/new.txt (remote)"]
    );
}

#[tokio::test]
async fn edit_file_says_how_many_places_where_and_how_big_with_the_tail() {
    let rig = Rig::prepared().await;
    std::fs::write(rig.worktree().join("a.txt"), "one\ntwo\none\n").unwrap();
    let once = rig
        .wrote(
            &EditFile,
            json!({"path": "a.txt", "old": "two", "new": "2"}),
        )
        .await;
    assert_eq!(
        once,
        format!("Edited a.txt: replaced 1 place (line 2), 10 bytes now.{TAIL}")
    );
    let everywhere = rig
        .wrote(
            &EditFile,
            json!({"path": "a.txt", "old": "one", "new": "1", "replace_all": true}),
        )
        .await;
    assert_eq!(
        everywhere,
        format!("Edited a.txt: replaced 2 places (lines 1, 3), 6 bytes now.{TAIL}")
    );
    assert_eq!(rig.epoch().await, 2);
    assert_eq!(
        rig.progress(),
        ["edited a.txt (remote)", "edited a.txt (remote)"]
    );
}

#[tokio::test]
async fn apply_patch_lists_the_files_with_the_tail() {
    let rig = Rig::prepared().await;
    let said = rig
        .wrote(
            &ApplyPatch,
            json!({"patch": "--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-widgets\n+gadgets\n"}),
        )
        .await;
    assert_eq!(
        said,
        format!("Applied the patch to 1 file(s): README.md.{TAIL}")
    );
    assert_eq!(rig.progress(), ["patched README.md (remote)"]);
}

/// A scratch project `fib` with one file, published to a repository that was just created: the
/// writes that follow in the project say, after the tail, that they do not reach the repository.
#[tokio::test]
async fn a_write_in_a_published_scratch_project_adds_the_note_after_the_tail() {
    let rig = Rig::new().await;
    let started = StartScratch
        .call(&rig.ctx, json!({"name": "fib"}))
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    let first = rig
        .wrote(
            &WriteFile,
            json!({"path": "fib.sh", "content": "echo 0 1 1 2\n"}),
        )
        .await;
    // Not published yet: no note.
    assert_eq!(first, format!("Created fib.sh (13 bytes).{TAIL}"));

    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&format!("Publish it to {url}")).await;
    let published = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    assert!(!published.is_error, "{}", published.content);

    let note = format!(
        "Note: `fib` was published to {url}, and what changes here is not part of that \
         repository's pull request: make changes in its slot, or call publish_scratch again to \
         copy them (a file that differs needs `overwrite: true`)."
    );
    let before = rig.epoch().await;
    let wrote = rig
        .wrote(
            &WriteFile,
            json!({"path": "late.txt", "content": "x\n", "repo": "fib"}),
        )
        .await;
    assert_eq!(wrote, format!("Created late.txt (2 bytes).{TAIL}\n{note}"));
    let edited = rig
        .wrote(
            &EditFile,
            json!({"path": "late.txt", "old": "x", "new": "y", "repo": "fib"}),
        )
        .await;
    assert_eq!(
        edited,
        format!("Edited late.txt: replaced 1 place (line 1), 2 bytes now.{TAIL}\n{note}")
    );
    let patched = rig
        .wrote(
            &ApplyPatch,
            json!({"patch": "--- a/late.txt\n+++ b/late.txt\n@@ -1 +1 @@\n-y\n+z\n", "repo": "fib"}),
        )
        .await;
    assert_eq!(
        patched,
        format!("Applied the patch to 1 file(s): late.txt.{TAIL}\n{note}")
    );
    assert_eq!(rig.epoch().await, before + 3);
}

/// A write that is refused says why and moves nothing: no tail, no epoch.
#[tokio::test]
async fn a_refused_write_has_no_tail_and_keeps_the_epoch() {
    let rig = Rig::prepared().await;
    let out = WriteFile
        .call(&rig.ctx, json!({"path": "../out.txt", "content": "x"}))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(!out.content.contains(TAIL.trim()), "{}", out.content);
    let out = EditFile
        .call(
            &rig.ctx,
            json!({"path": "README.md", "old": "nowhere", "new": "x"}),
        )
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.content);
    assert!(!out.content.contains(TAIL.trim()), "{}", out.content);
    assert_eq!(rig.epoch().await, 0);
}
