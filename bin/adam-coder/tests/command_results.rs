//! What `run_command` and `run` answer, to the byte: the command, how it ended and its output, the
//! refusals of a command that changed what it may not, and the changes `run` keeps. Pinned before the
//! two tools move to a library crate (ADR 0047), so that the move cannot change what the model is
//! told. And that every `run` that ran moves the run's check epoch by one, and `run_command` never.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_coder::tools::inspect::RunCommand;
use adam_coder::tools::make::Run;
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::scratch::{PublishScratch, StartScratch};
use adam_llm_agent::{Tool, ToolCtx, ToolOutput};
use adam_runtime::{CollectingSink, RunEvent};
use common::Fixture;
use serde_json::{Value, json};

/// What `run_command` says after it undid a change, up to the command's output.
fn undone_look(command: &str) -> String {
    format!(
        "`{command}` changed the worktree (files, HEAD, the branch, a ref or the git \
         configuration), and run_command is for looking around only. The change was undone: the \
         worktree is exactly as it was. To make something with a command use `run`; to change code \
         use write_file, edit_file or apply_patch. Output of the command, for what it is worth:\n"
    )
}

/// The line `run` ends its list of changes with.
const CHECK_AGAIN: &str =
    "The files changed, so run the project's checks again before you commit.\n";

/// The heading of `run`'s list of changes.
const CHANGES: &str = "--- the worktree now differs from HEAD in (git status; files the project ignores are kept too) ---\n";

const NO_WORKSPACE: &str = "there is no workspace yet: call prepare_workspace (a repository the \
                            person named) or start_scratch (a project to build before there is a \
                            repository) first";

const MISSING: &str = "The workspace has no `no-such-toolchain`: the shell could not find it (exit \
                       code 127). That is a missing toolchain, not a failing check: no check cycle \
                       was used and nothing was recorded as a check. Do not retry variants of the \
                       command, do not search the filesystem for the tool and do not try to \
                       install it (system toolchains are not yours to install). Tell the person \
                       which toolchain is missing (`no-such-toolchain`) with ask_user, and wait \
                       for their answer.";

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

    /// Prepare the fixture's remote: one slot, `remote`.
    async fn prepare(&self) {
        self.say(&self.fx.remote_url()).await;
        let out = PrepareWorkspace
            .call(
                &self.ctx,
                json!({"repo_url": self.fx.remote_url(), "base_branch": "main"}),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
    }

    async fn prepared() -> Self {
        let rig = Self::new().await;
        rig.prepare().await;
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

    /// `tool` called with `args`: its answer, with the epoch moved by exactly `moves`.
    async fn call(&self, tool: &dyn Tool, args: Value, moves: u64) -> ToolOutput {
        let before = self.epoch().await;
        let out = tool.call(&self.ctx, args).await.unwrap();
        assert_eq!(self.epoch().await, before + moves, "{}", out.content);
        out
    }

    /// `run_command` with `args`: it never moves the epoch.
    async fn look(&self, args: Value) -> ToolOutput {
        self.call(&RunCommand, args, 0).await
    }

    /// `run` with `args`, which ran: the epoch moved by one, refused or not.
    async fn make(&self, args: Value) -> ToolOutput {
        self.call(&Run, args, 1).await
    }
}

// ------------------------------------------------ run_command

#[tokio::test]
async fn run_command_says_the_command_how_it_ended_and_its_output() {
    let rig = Rig::prepared().await;
    let ok = rig.look(json!({"command": "cat README.md"})).await;
    assert!(!ok.is_error);
    assert_eq!(
        ok.content,
        "$ cat README.md\nexit code 0\n--- output ---\nwidgets\n"
    );
    // A non-zero exit is an answer, not an error.
    let no = rig.look(json!({"command": "echo nope >&2; exit 3"})).await;
    assert!(!no.is_error);
    assert_eq!(
        no.content,
        "$ echo nope >&2; exit 3\nexit code 3\n--- output ---\nnope\n"
    );
    assert_eq!(
        rig.progress(),
        ["running: cat README.md", "running: echo nope >&2; exit 3"]
    );
    assert_eq!(rig.epoch().await, 0);
}

#[tokio::test]
async fn run_command_that_outlives_its_timeout_is_killed_and_says_so() {
    let rig = Rig::prepared().await;
    // Four seconds: long enough for a login shell to start on a loaded machine.
    let slow = rig
        .look(json!({"command": "echo before; sleep 60", "timeout": 4}))
        .await;
    assert!(!slow.is_error);
    assert_eq!(
        slow.content,
        "$ echo before; sleep 60\ntimed out (the command was killed)\n--- output ---\nbefore\n"
    );
    assert_eq!(rig.progress(), ["running: echo before; sleep 60"]);
}

#[tokio::test]
async fn run_command_undoes_and_refuses_a_change_to_a_tracked_file() {
    let rig = Rig::prepared().await;
    let command = "sed -i 's/widgets/gadgets/' README.md";
    let out = rig.look(json!({ "command": command })).await;
    assert!(out.is_error);
    assert_eq!(out.content, undone_look(command));
    assert_eq!(
        std::fs::read_to_string(rig.worktree().join("README.md")).unwrap(),
        "widgets\n"
    );
    assert_eq!(
        rig.progress(),
        [
            format!("running: {command}"),
            format!("undid a change made by: {command}")
        ]
    );
}

#[tokio::test]
async fn run_command_undoes_and_refuses_a_change_to_the_git_configuration_or_a_ref() {
    let rig = Rig::prepared().await;
    let dir = rig.worktree();
    for command in ["git config alias.st status", "git branch created"] {
        let out = rig.look(json!({ "command": command })).await;
        assert!(out.is_error, "{command}");
        assert_eq!(out.content, undone_look(command));
    }
    assert!(
        !common::git(&dir, &["config", "--local", "--list"]).contains("alias.st"),
        "the configuration is back"
    );
    assert_eq!(
        common::git(&dir, &["branch", "--list", "created"]),
        "",
        "the branch is gone"
    );
    assert_eq!(
        rig.progress(),
        [
            "running: git config alias.st status",
            "undid a change made by: git config alias.st status",
            "running: git branch created",
            "undid a change made by: git branch created",
        ]
    );
}

/// An embedded repository with no commit makes `git add -A` fail, so there is no tree id: a look
/// still works, and a change is seen but cannot be written back exactly.
#[tokio::test]
async fn run_command_where_the_tree_cannot_be_computed_looks_and_cannot_fully_undo() {
    let rig = Rig::prepared().await;
    let embedded = rig.worktree().join("vendor/embedded");
    std::fs::create_dir_all(&embedded).unwrap();
    common::git(&embedded, &["init", "--quiet"]);
    std::fs::write(embedded.join("f.txt"), "x\n").unwrap();

    let ls = rig.look(json!({"command": "ls vendor"})).await;
    assert!(!ls.is_error);
    assert_eq!(
        ls.content,
        "$ ls vendor\nexit code 0\n--- output ---\nembedded\n"
    );
    let changed = rig.look(json!({"command": "echo y > added.txt"})).await;
    assert!(changed.is_error);
    assert_eq!(
        changed.content,
        "`echo y > added.txt` changed the worktree (files, HEAD, the branch, a ref or the git \
         configuration), and run_command is for looking around only. The change could not be \
         fully undone (HEAD and the branch are back, but the files may differ): run `git status` \
         with run_command to see the worktree before you go on. To make something with a command \
         use `run`; to change code use write_file, edit_file or apply_patch. Output of the \
         command, for what it is worth:\n"
    );
    assert_eq!(
        rig.progress(),
        [
            "running: ls vendor",
            "running: echo y > added.txt",
            "undid a change made by: echo y > added.txt",
        ]
    );
}

// ------------------------------------------------ both: refused before anything runs

/// No workspace, a blank command, a `cwd` outside the worktree, a timeout out of bounds: the same
/// answer from both tools, nothing run, no progress, and the epoch where it was.
#[tokio::test]
async fn both_tools_refuse_before_running_and_move_nothing() {
    let rig = Rig::new().await;
    for tool in [&RunCommand as &dyn Tool, &Run] {
        let out = rig.call(tool, json!({"command": "ls"}), 0).await;
        assert!(out.is_error);
        assert_eq!(out.content, NO_WORKSPACE);
    }
    rig.prepare().await;
    let refusals = [
        (json!({"command": "  "}), "command is required".to_owned()),
        (
            json!({"command": "ls", "cwd": ".."}),
            "cwd `..` must be a relative path inside the worktree (no absolute paths, no `..`)"
                .to_owned(),
        ),
        (
            json!({"command": "ls", "cwd": "/etc"}),
            "cwd `/etc` must be a relative path inside the worktree (no absolute paths, no `..`)"
                .to_owned(),
        ),
        (
            json!({"command": "true", "timeout": 0}),
            "timeout must be at least 1 second".to_owned(),
        ),
        (
            json!({"command": "true", "timeout": 100_000}),
            format!(
                "timeout 100000 s is over the most this deployment allows, {limit} s: ask for at \
                 most {limit} s, or leave it out",
                limit = rig.fx.env.settings.check_timeout.as_secs()
            ),
        ),
    ];
    for tool in [&RunCommand as &dyn Tool, &Run] {
        for (args, said) in &refusals {
            let out = rig.call(tool, args.clone(), 0).await;
            assert!(out.is_error, "{args}");
            assert_eq!(&out.content, said, "{args}");
        }
    }
    assert!(rig.progress().is_empty(), "{:?}", rig.progress());
    assert_eq!(rig.epoch().await, 0);
}

// ------------------------------------------------ run

#[tokio::test]
async fn run_keeps_what_a_command_writes_and_lists_the_changes() {
    let rig = Rig::prepared().await;
    let command =
        "mkdir -p out && echo made > out/a.txt && echo more >> README.md && echo rendered";
    let made = rig.make(json!({ "command": command })).await;
    assert!(!made.is_error);
    // The first line of `git status` has lost its leading space (` M` reads `M`): the status is
    // read through `git_stdout`, which trims. Pinned as it is today.
    assert_eq!(
        made.content,
        format!(
            "$ {command}\nexit code 0\n--- output ---\nrendered\n{CHANGES}M README.md\n?? out/a.txt\n\
             {CHECK_AGAIN}"
        )
    );
    assert_eq!(
        std::fs::read_to_string(rig.worktree().join("out/a.txt")).unwrap(),
        "made\n"
    );
    // A command that changes nothing git sees, and exits non-zero: an answer, and no list.
    let nothing = rig.make(json!({"command": "echo nope >&2; exit 3"})).await;
    assert!(!nothing.is_error);
    assert_eq!(
        nothing.content,
        "$ echo nope >&2; exit 3\nexit code 3\n--- output ---\nnope\n(git sees no change in the \
         worktree; files the project ignores are kept too, and are not listed)\n"
    );
    assert_eq!(
        rig.progress(),
        [
            format!("running: {command}"),
            format!("changed files: {command}"),
            "running: echo nope >&2; exit 3".to_owned(),
            "ran: echo nope >&2; exit 3".to_owned(),
        ]
    );
    assert_eq!(rig.epoch().await, 2);
}

/// A command that touched git is undone entirely, the file it made too, and refused; it ran, so
/// the epoch moves all the same.
#[tokio::test]
async fn run_undoes_and_refuses_a_command_that_touches_git() {
    let rig = Rig::prepared().await;
    let dir = rig.worktree();
    let command = "echo made > made.txt; git branch rogue";
    let out = rig.make(json!({ "command": command })).await;
    assert!(out.is_error);
    assert_eq!(
        out.content,
        format!(
            "`{command}` changed git (HEAD, the branch, a ref, `.git` or the git configuration), \
             and `run` may change files and nothing of git. Everything it did was undone, its files \
             included: the worktree is exactly as it was. Commit with commit_and_push, and run the \
             command again without the git part. Output of the command, for what it is worth:\n"
        )
    );
    assert!(!dir.join("made.txt").exists());
    assert_eq!(common::git(&dir, &["branch", "--list", "rogue"]), "");
    assert_eq!(
        rig.progress(),
        [
            format!("running: {command}"),
            format!("undid a change to git made by: {command}"),
        ]
    );
    assert_eq!(rig.epoch().await, 1);
}

#[tokio::test]
async fn run_timeout_cwd_and_a_missing_tool_first_and_repeated() {
    let rig = Rig::prepared().await;
    let dir = rig.worktree();
    // What it wrote before it was killed stays, and is listed.
    let slow = rig
        .make(json!({"command": "echo partial > partial.txt; echo before; sleep 60", "timeout": 4}))
        .await;
    assert!(!slow.is_error);
    assert_eq!(
        slow.content,
        format!(
            "$ echo partial > partial.txt; echo before; sleep 60\ntimed out (the command was \
             killed)\n--- output ---\nbefore\n{CHANGES}?? partial.txt\n{CHECK_AGAIN}"
        )
    );

    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let sub = rig
        .make(json!({"command": "echo here > f.txt", "cwd": "sub"}))
        .await;
    assert!(!sub.is_error);
    assert_eq!(
        sub.content,
        format!(
            "$ echo here > f.txt\nexit code 0\n--- output ---\n\n{CHANGES}?? partial.txt\n?? \
             sub/f.txt\n{CHECK_AGAIN}"
        )
    );

    let missing = rig
        .make(json!({"command": "no-such-toolchain build"}))
        .await;
    assert!(missing.is_error);
    assert_eq!(missing.content, MISSING);
    // The same call again (a replay) gets the same answer.
    let replay = rig
        .make(json!({"command": "no-such-toolchain build"}))
        .await;
    assert_eq!(replay.content, MISSING);
    // A later call of the run (another call id) that finds it missing again: stop and ask.
    let run = rig.ctx.run_id().to_string();
    let mut notes = rig.fx.env.notes.load(&run).await.unwrap();
    notes.missing_tools.clear();
    notes.record_missing_tool("an-earlier-call", "no-such-toolchain");
    rig.fx.env.notes.save(&run, &notes).await.unwrap();
    let again = rig
        .make(json!({"command": "no-such-toolchain build"}))
        .await;
    assert!(again.is_error);
    assert_eq!(
        again.content,
        format!(
            "{MISSING} You were told about this tool before and it is still missing: whatever was \
             tried did not provide it, so stop and ask."
        )
    );

    assert_eq!(
        rig.progress(),
        [
            "running: echo partial > partial.txt; echo before; sleep 60",
            "changed files: echo partial > partial.txt; echo before; sleep 60",
            "running: echo here > f.txt",
            "changed files: echo here > f.txt",
            "running: no-such-toolchain build",
            "the workspace lacks a tool",
            "running: no-such-toolchain build",
            "the workspace lacks a tool",
            "running: no-such-toolchain build",
            "the workspace lacks a tool",
        ]
    );
    assert_eq!(rig.epoch().await, 5);
}

/// A scratch project `fib`: `run` lists its changes, and once the project is published to a
/// repository that was just created, adds after the list the note that they do not reach it.
#[tokio::test]
async fn run_in_a_scratch_project_adds_the_published_note_once_published() {
    let rig = Rig::new().await;
    let started = StartScratch
        .call(&rig.ctx, json!({"name": "fib"}))
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    let first = rig.make(json!({"command": "echo 0 1 1 2 > fib.sh"})).await;
    assert!(!first.is_error);
    assert_eq!(
        first.content,
        format!(
            "$ echo 0 1 1 2 > fib.sh\nexit code 0\n--- output ---\n\n{CHANGES}?? fib.sh\n{CHECK_AGAIN}"
        )
    );

    let empty = rig.fx.empty_remote("fibonacci");
    let url = empty.to_string_lossy().into_owned();
    rig.say(&format!("Publish it to {url}")).await;
    let published = PublishScratch
        .call(&rig.ctx, json!({"repo_url": url}))
        .await
        .unwrap();
    assert!(!published.is_error, "{}", published.content);

    let later = rig
        .make(json!({"command": "echo 3 >> fib.sh", "repo": "fib"}))
        .await;
    assert!(!later.is_error);
    assert_eq!(
        later.content,
        format!(
            "$ echo 3 >> fib.sh\nexit code 0\n--- output ---\n\n{CHANGES}?? fib.sh\n{CHECK_AGAIN}\n\
             Note: `fib` was published to {url}, and what changes here is not part of that \
             repository's pull request: make changes in its slot, or call publish_scratch again to \
             copy them (a file that differs needs `overwrite: true`)."
        )
    );
    assert_eq!(
        rig.progress(),
        [
            "started the scratch project fib".to_owned(),
            "running: echo 0 1 1 2 > fib.sh".to_owned(),
            "changed files: echo 0 1 1 2 > fib.sh".to_owned(),
            format!("looking at {url}"),
            format!("giving {url} its first commit on main"),
            "copying fib into fibonacci (.)".to_owned(),
            "running: echo 3 >> fib.sh".to_owned(),
            "changed files: echo 3 >> fib.sh".to_owned(),
        ]
    );
    assert_eq!(rig.epoch().await, 2);
}
