//! What `run_command` and `run` answer, to the byte: the command, how it ended and its output, the
//! refusals of a command that changed what it may not, and the changes `run` keeps. Pinned before the
//! two tools move to a library crate (ADR 0047), so that the move cannot change what the model is
//! told. And that every `run` that ran moves the run's check epoch by one, and `run_command` never.
//! `run` shows a command that succeeded as one line by default (`output: "on_error"`, ADR 0090);
//! `run_command` shows its output.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

mod common;

use std::sync::Arc;

use adam_coder::tools::background::{StartCommand, StopCommand};
use adam_coder::tools::inspect::RunCommand;
use adam_coder::tools::make::Run;
use adam_coder::tools::named::{named_in, without_untrusted};
use adam_coder::tools::prepare::PrepareWorkspace;
use adam_coder::tools::scratch::{PublishScratch, StartScratch};
use adam_llm_agent::{KeptStore, Tool, ToolCtx, ToolOutput, output_id_of};
use adam_runtime::{CollectingSink, RunEvent};
use common::{Fixture, GITHUB_TOKEN};
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

/// The line a command of `rig`'s call that succeeded printing `bytes` is shown as by `run`'s default.
fn hidden(rig: &Rig, bytes: usize) -> String {
    let id = output_id_of(&rig.ctx);
    format!(
        "[output: {bytes} bytes, not shown because the command succeeded; kept as {id}; \
         read_output {{\"id\":\"{id}\"}} reads it]\n"
    )
}

/// What `run`'s default shows of a command that succeeded printing nothing.
const NO_OUTPUT: &str = "(no output)\n";

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
        Self::from(Fixture::new().await)
    }

    fn from(fx: Fixture) -> Self {
        let sink = CollectingSink::new();
        let ctx = Self::ctx_of(&fx, "call-1", &sink);
        Self { fx, sink, ctx }
    }

    /// A call `call` of a run, as the agent makes it: the tools and the kept outputs.
    fn ctx_of(fx: &Fixture, call: &str, sink: &CollectingSink) -> ToolCtx {
        ToolCtx::detached("tool", call, Arc::new(sink.clone()))
            .with_state(fx.env.clone())
            .with_state(Arc::new(KeptStore(fx.env.kept.clone())))
    }

    /// Another call of the same run, whose progress is not this rig's.
    fn other_call(&self, call: &str) -> ToolCtx {
        Self::ctx_of(&self.fx, call, &CollectingSink::new()).with_root_run(self.ctx.run_id())
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

    /// `run` with `args`, whose command ran: the epoch moved by exactly one, whether the answer is
    /// a result or a refusal that came after the command (git touched or unreadable, a missing tool).
    async fn make(&self, args: Value) -> ToolOutput {
        self.call(&Run, args, 1).await
    }

    /// `run` with `args`, refused before its command ran (no slot, a reference, a bad argument, a
    /// worktree that cannot be snapshotted or whose `HEAD` is not the run's): the epoch did not move.
    async fn make_refused(&self, args: Value) -> ToolOutput {
        self.call(&Run, args, 0).await
    }

    /// The run's own branch, `agent/...`.
    fn own_branch(&self) -> String {
        common::git(&self.worktree(), &["symbolic-ref", "--short", "HEAD"])
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
    let command = "printf 'gadgets\\n' > README.md";
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
    // As `git status` lists them: a change not staged keeps its leading space, first or not.
    assert_eq!(
        made.content,
        format!(
            "$ {command}\nexit code 0\n{}{CHANGES} M README.md\n?? out/a.txt\n{CHECK_AGAIN}",
            hidden(&rig, "rendered\n".len())
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
            "$ echo here > f.txt\nexit code 0\n{NO_OUTPUT}{CHANGES}?? partial.txt\n?? \
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
            "$ echo 0 1 1 2 > fib.sh\nexit code 0\n{NO_OUTPUT}{CHANGES}?? fib.sh\n{CHECK_AGAIN}"
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
            "$ echo 3 >> fib.sh\nexit code 0\n{NO_OUTPUT}{CHANGES}?? fib.sh\n{CHECK_AGAIN}\n\
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

// ------------------------------------------------ both: what every answer shares

/// A secret of the process is scrubbed from the `$` line, the output and the progress lines, short
/// output included.
#[tokio::test]
async fn both_tools_scrub_a_secret_from_the_command_the_output_and_the_progress() {
    let rig = Rig::prepared().await;
    let command = format!("echo {GITHUB_TOKEN}");
    let looked = rig.look(json!({ "command": command })).await;
    assert!(!looked.is_error);
    assert_eq!(
        looked.content,
        "$ echo [redacted: secret removed, do not reuse]\nexit code 0\n--- output ---\n[redacted: secret removed, do not reuse]\n"
    );
    let made = rig
        .make(json!({ "command": command, "output": "full" }))
        .await;
    assert!(!made.is_error);
    assert_eq!(
        made.content,
        "$ echo [redacted: secret removed, do not reuse]\nexit code 0\n--- output ---\n[redacted: secret removed, do not reuse]\n(git sees no change in the \
         worktree; files the project ignores are kept too, and are not listed)\n"
    );
    assert_eq!(
        rig.progress(),
        [
            "running: echo [redacted: secret removed, do not reuse]",
            "running: echo [redacted: secret removed, do not reuse]",
            "ran: echo [redacted: secret removed, do not reuse]",
        ]
    );
}

/// The repeat key says how a command ended (`exit:<code>`, `timeout`, `signal`) and is not set for
/// a command that passed, nor for a refusal.
#[tokio::test]
async fn both_tools_give_the_repeat_key_of_how_the_command_ended() {
    let rig = Rig::prepared().await;
    const NO_CHANGE: &str = "(git sees no change in the worktree; files the project ignores are \
                             kept too, and are not listed)\n";
    // `run`'s default shows a command that succeeded printing nothing as `(no output)`.
    let cases = [
        (
            json!({"command": "true"}),
            "$ true\nexit code 0\n",
            None,
            NO_OUTPUT,
        ),
        (
            json!({"command": "exit 3"}),
            "$ exit 3\nexit code 3\n",
            Some("exit:3"),
            "--- output ---\n\n",
        ),
        (
            json!({"command": "sleep 60", "timeout": 2}),
            "$ sleep 60\ntimed out (the command was killed)\n",
            Some("timeout"),
            "--- output ---\n\n",
        ),
        (
            json!({"command": "kill -KILL $$"}),
            "$ kill -KILL $$\nkilled by a signal\n",
            Some("signal"),
            "--- output ---\n\n",
        ),
    ];
    for (args, said, key, made_output) in &cases {
        let looked = rig.look(args.clone()).await;
        assert!(!looked.is_error, "{args}");
        assert_eq!(
            looked.content,
            format!("{said}--- output ---\n\n"),
            "{args}"
        );
        assert_eq!(
            looked.repeat_key.as_deref().map(String::as_str),
            *key,
            "{args}"
        );
        let made = rig.make(args.clone()).await;
        assert!(!made.is_error, "{args}");
        assert_eq!(
            made.content,
            format!("{said}{made_output}{NO_CHANGE}"),
            "{args}"
        );
        assert_eq!(
            made.repeat_key.as_deref().map(String::as_str),
            *key,
            "{args}"
        );
    }
    // Refusals: before the command ran, and after it (a change undone).
    for out in [
        rig.look(json!({"command": " "})).await,
        rig.make_refused(json!({"command": " "})).await,
        rig.look(json!({"command": "echo x > README.md; exit 3"}))
            .await,
        rig.make(json!({"command": "git branch zz; exit 3"})).await,
    ] {
        assert!(out.is_error, "{}", out.content);
        assert_eq!(out.repeat_key, None, "{}", out.content);
    }
}

/// A refused command's output follows the refusal.
#[tokio::test]
async fn a_refusal_shows_what_the_command_printed() {
    let rig = Rig::prepared().await;
    for command in ["echo said; echo x > README.md", "echo said; git branch zz"] {
        let out = rig.look(json!({ "command": command })).await;
        assert!(out.is_error, "{command}");
        assert_eq!(out.content, format!("{}said\n", undone_look(command)));
    }
    let command = "echo said; git branch zz";
    let out = rig.make(json!({ "command": command })).await;
    assert!(out.is_error);
    assert_eq!(
        out.content,
        format!(
            "`{command}` changed git (HEAD, the branch, a ref, `.git` or the git configuration), \
             and `run` may change files and nothing of git. Everything it did was undone, its files \
             included: the worktree is exactly as it was. Commit with commit_and_push, and run the \
             command again without the git part. Output of the command, for what it is worth:\n\
             said\n"
        )
    );
}

/// Output over `check_output_tail` (64 bytes here) is shown as its head and its tail, and kept.
#[tokio::test]
async fn long_output_is_shown_as_its_head_and_tail_and_kept() {
    let rig = Rig::from(Fixture::with(|s| s.check_output_tail = 64).await);
    rig.prepare().await;
    // The id is made from the run's id, which is new in every test: it is the call's, computed.
    let id = output_id_of(&rig.ctx);
    let view = format!(
        "$ seq 1 200\nexit code 0\n--- output: 692 bytes, head and tail shown, kept as {id} ---\n\
         1\n2\n3\n4\n5\n6\n7\n8\n[... 632 bytes not shown ...]\n\
         190\n191\n192\n193\n194\n195\n196\n197\n198\n199\n200\n\
         [more: 200 lines are kept as {id}; read_output {{\"id\":\"{id}\"}} reads them page by page]\n"
    );
    let looked = rig.look(json!({"command": "seq 1 200"})).await;
    assert!(!looked.is_error);
    assert_eq!(looked.content, view);
    let made = rig
        .make(json!({"command": "seq 1 200", "output": "full"}))
        .await;
    assert!(!made.is_error);
    assert_eq!(
        made.content,
        format!(
            "{view}(git sees no change in the worktree; files the project ignores are kept too, \
             and are not listed)\n"
        )
    );
    // By default it succeeded, so it is one line, and the output is kept all the same.
    let made = rig.make(json!({"command": "seq 1 200"})).await;
    assert_eq!(
        made.content,
        format!(
            "$ seq 1 200\nexit code 0\n{}(git sees no change in the worktree; files the project \
             ignores are kept too, and are not listed)\n",
            hidden(&rig, 692)
        )
    );
}

/// `run_command` answers a missing tool as `run` does, and a tool the project brings itself with
/// the project's install command.
#[tokio::test]
async fn run_command_says_a_tool_is_missing_or_a_dependency_of_the_project() {
    let rig = Rig::prepared().await;
    let missing = rig
        .look(json!({"command": "no-such-toolchain build"}))
        .await;
    assert!(missing.is_error);
    assert_eq!(missing.content, MISSING);

    let dir = rig.worktree();
    std::fs::write(
        dir.join("package.json"),
        r#"{"devDependencies": {"zzz-local-tool": "1"}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
    let dependency = rig.look(json!({"command": "zzz-local-tool --run"})).await;
    assert!(dependency.is_error);
    assert_eq!(
        dependency.content,
        "`zzz-local-tool` is not on the PATH, but it is one of this project's own dependencies, \
         which are not installed in the workspace yet: that is not a missing toolchain and not a \
         failing check (no check cycle was used, nothing was recorded as a check). Install the \
         project's dependencies with its own command (`pnpm install`), with run (not run_checks: \
         that is only for the project's real checks), then run this again. Only if that fails, \
         tell the person with ask_user."
    );
    assert_eq!(
        rig.progress(),
        [
            "running: no-such-toolchain build",
            "the workspace lacks a tool",
            "running: zzz-local-tool --run",
            "the workspace lacks a tool",
        ]
    );
}

/// A reference is read by `run_command` and never written by `run`.
#[tokio::test]
async fn a_reference_is_looked_at_and_never_made_in() {
    let fx = Fixture::new().await;
    let design = fx.extra_remote("design", &[("tokens.css", "--blue: #00f;\n")]);
    let fx = fx.referencing(vec![adam::agent_fs::ReferenceDef {
        name: "design".into(),
        url: design.to_string_lossy().into_owned(),
        branch: None,
        description: "Tokens".into(),
    }]);
    let rig = Rig::from(fx);
    rig.prepare().await;
    let looked = rig
        .look(json!({"command": "cat tokens.css", "repo": "design"}))
        .await;
    assert!(!looked.is_error);
    assert_eq!(
        looked.content,
        "$ cat tokens.css\nexit code 0\n--- output ---\n--blue: #00f;\n"
    );
    let made = rig
        .make_refused(json!({"command": "true", "repo": "design"}))
        .await;
    assert!(made.is_error);
    assert_eq!(
        made.content,
        "`design` is a reference: it is read and never changed, so nothing is written, committed, \
         pushed or opened there. Read it with read_file, list_files, find_files or search_files \
         (`repo: design`). If the task needs a change to that repository, it is not a reference \
         any more: the person names it, or you ask with request_repository, and prepare_workspace \
         gives it a slot of its own."
    );
    assert_eq!(
        rig.progress(),
        [
            "checking out the reference design (main)",
            "running: cat tokens.css"
        ]
    );
}

/// `HEAD` off the run's own branch: neither tool runs anything.
#[tokio::test]
async fn a_head_off_the_runs_branch_is_refused_by_both_tools() {
    let rig = Rig::prepared().await;
    let own = rig.own_branch();
    common::git(&rig.worktree(), &["checkout", "--quiet", "--detach"]);
    let refusal = format!(
        "HEAD of this worktree is on no branch (it is detached), not on the run's own branch \
         {own}: something moved it (it is a file that commands of the run can write). Nothing was \
         run, because a command that changes git could not be undone here. Do not try to move \
         HEAD back with git; tell the person."
    );
    let looked = rig.look(json!({"command": "ls"})).await;
    assert!(looked.is_error);
    assert_eq!(looked.content, refusal);
    let made = rig.make_refused(json!({"command": "ls"})).await;
    assert!(made.is_error);
    assert_eq!(made.content, refusal);
    assert!(rig.progress().is_empty(), "{:?}", rig.progress());
}

/// A command after which git cannot be read (it deleted the run's branch) is undone by both; a
/// worktree that cannot be read before is refused by both, and nothing runs.
#[tokio::test]
async fn git_that_cannot_be_read_after_or_before_the_command() {
    let rig = Rig::prepared().await;
    let own = rig.own_branch();
    let command = format!("git update-ref -d refs/heads/{own}");
    let made = rig.make(json!({ "command": command })).await;
    assert!(made.is_error);
    assert_eq!(
        made.content,
        format!(
            "After `{command}` the state of the worktree's git could not be read, so it is not \
             known whether the command changed git; to be safe it was treated as if it had, since \
             `run` may change files and nothing of git. Everything it did was undone, its files \
             included: the worktree is exactly as it was. If the command touches git (HEAD, the \
             branch, a ref, `.git` or the git configuration), commit with commit_and_push and run \
             it again without the git part; if it does not, try it again, and if this persists, \
             tell the person. Output of the command, for what it is worth:\n"
        )
    );
    let looked = rig.look(json!({ "command": command })).await;
    assert!(looked.is_error);
    assert_eq!(looked.content, undone_look(&command));
    assert_eq!(
        rig.progress(),
        [
            format!("running: {command}"),
            format!("could not check git after: {command}; undid it"),
            format!("running: {command}"),
            format!("undid a change made by: {command}"),
        ]
    );

    // Now deleted by hand: `HEAD` names nothing, and the snapshot before the command fails.
    common::git(
        &rig.worktree(),
        &["update-ref", "-d", &format!("refs/heads/{own}")],
    );
    let looked = rig.look(json!({"command": "ls"})).await;
    assert!(looked.is_error);
    assert_eq!(
        looked.content,
        "Cannot look at the worktree: the state of the worktree cannot be read. Nothing was run, \
         because a command that might change the worktree cannot be undone. Try again; if it \
         persists, tell the person."
    );
    let made = rig.make_refused(json!({"command": "ls"})).await;
    assert!(made.is_error);
    assert_eq!(
        made.content,
        "Cannot look at the worktree: the state of the worktree cannot be read. Nothing was run, \
         because a command that might change git cannot be undone. Try again; if it persists, tell \
         the person."
    );
    assert_eq!(rig.progress().len(), 4, "nothing ran: {:?}", rig.progress());
}

/// A command that keeps running in the slot is named after every refusal.
#[tokio::test]
async fn a_refusal_names_the_commands_that_keep_running_in_the_slot() {
    let rig = Rig::prepared().await;
    let start = rig.other_call("start-1");
    let started = StartCommand
        .call(
            &start,
            json!({"command": "echo ready; sleep 630", "wait_for": "ready", "wait": 20}),
        )
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    // Its id is made from the run's id, new in every test: computed from the call that started it.
    let id = output_id_of(&start);
    let note = format!(
        "\nCommands that keep running are writing in this slot, and what they write is not undone: \
         {id} (echo ready; sleep 630). Stop one with stop_command when it is the cause."
    );

    let looked = rig.look(json!({"command": "echo x > README.md"})).await;
    assert!(looked.is_error);
    assert_eq!(
        looked.content,
        format!("{}{note}", undone_look("echo x > README.md"))
    );
    let made = rig.make(json!({"command": "git branch q"})).await;
    assert!(made.is_error);
    assert_eq!(
        made.content,
        format!(
            "`git branch q` changed git (HEAD, the branch, a ref, `.git` or the git \
             configuration), and `run` may change files and nothing of git. Everything it did was \
             undone, its files included: the worktree is exactly as it was. Commit with \
             commit_and_push, and run the command again without the git part. Output of the \
             command, for what it is worth:\n{note}"
        )
    );
    // A result is not a refusal: no note.
    let ok = rig.look(json!({"command": "true"})).await;
    assert_eq!(ok.content, "$ true\nexit code 0\n--- output ---\n\n");
    let stopped = StopCommand.call(&start, json!({ "id": id })).await.unwrap();
    assert!(!stopped.is_error, "{}", stopped.content);
}

/// Over twenty changes, the list is cut and counted; a change git does not list is said.
#[tokio::test]
async fn run_cuts_a_long_list_of_changes_and_says_when_git_lists_none() {
    let rig = Rig::prepared().await;
    let command = "for i in $(seq 1 25); do echo > f$i; done";
    let many = rig.make(json!({ "command": command })).await;
    assert!(!many.is_error);
    let listed: String = [
        1, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 2, 20, 21, 22, 23, 24, 25, 3, 4,
    ]
    .iter()
    .map(|i| format!("?? f{i}\n"))
    .collect();
    assert_eq!(
        many.content,
        format!("$ {command}\nexit code 0\n{NO_OUTPUT}{CHANGES}{listed}and 5 more\n{CHECK_AGAIN}")
    );

    // An edit of the run's put back as `HEAD` has it: the tree changed, and `git status` lists
    // nothing, and the note says why it may be: ignored, or back as `HEAD` has them.
    let rig = Rig::prepared().await;
    std::fs::write(rig.worktree().join("README.md"), "edited\n").unwrap();
    let command = "printf 'widgets\\n' > README.md";
    let none = rig.make(json!({ "command": command })).await;
    assert!(!none.is_error);
    assert_eq!(
        none.content,
        format!(
            "$ {command}\nexit code 0\n{NO_OUTPUT}{CHANGES}(nothing that git lists: the \
             files it changed are ignored by the project, or are back as HEAD has them)\n{CHECK_AGAIN}"
        )
    );
    assert_eq!(
        rig.progress(),
        [
            format!("running: {command}"),
            format!("changed files: {command}"),
        ]
    );
}

#[tokio::test]
async fn run_command_looks_in_a_scratch_project() {
    let rig = Rig::new().await;
    let started = StartScratch
        .call(&rig.ctx, json!({"name": "fib"}))
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    let looked = rig.look(json!({"command": "ls -a"})).await;
    assert!(!looked.is_error);
    assert_eq!(
        looked.content,
        "$ ls -a\nexit code 0\n--- output ---\n.\n..\n.git\n"
    );
}

/// Two slots and no `repo`, and a `cwd` that does not exist: refused by both before anything runs.
#[tokio::test]
async fn both_tools_refuse_an_unnamed_slot_of_two_and_a_cwd_that_does_not_exist() {
    let rig = Rig::prepared().await;
    let started = StartScratch
        .call(&rig.ctx, json!({"name": "fib"}))
        .await
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    let which = format!(
        "this workspace has 2 slots: `fib` (a scratch project), `remote` ({}). Say which one with \
         `repo` (its name, or the repository's address)",
        rig.fx.remote_url()
    );
    let nowhere = "cwd `nowhere` does not exist in the worktree: No such file or directory (os \
                   error 2)";
    for tool in [&RunCommand as &dyn Tool, &Run] {
        let out = rig.call(tool, json!({"command": "ls"}), 0).await;
        assert!(out.is_error);
        assert_eq!(out.content, which);
        let out = rig
            .call(
                tool,
                json!({"command": "ls", "cwd": "nowhere", "repo": "remote"}),
                0,
            )
            .await;
        assert!(out.is_error);
        assert_eq!(out.content, nowhere);
    }
}
