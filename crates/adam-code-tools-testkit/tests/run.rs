//! `adam_code_tools::make::run` over a scripted workspace: for each verdict of the guard, what the
//! tool answers and the progress line it adds; the guard's refusal (nothing runs); the refusal note
//! on refusals only; `after_run` called once, after the command and **after** the guard is
//! settled, and not at all for a `run` refused before its command; its text appended only when
//! files changed; a failing `after_run`, whose error is returned with the guard already settled; and
//! its `output`: one line for a command that succeeded by default (the output kept for
//! `read_output`), the whole output with `full` or for a command that failed, and the workspace's
//! default for a call that gives none.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use adam_code_tools::make::run;
use adam_code_tools::shell::MissingTool;
use adam_code_tools::{
    CodeWorkspace, CommandGuard, CommandLimits, CommandWorkspace, Guarding, Intent, Outcome,
    OutputDefaults, OutputMode, Place, Redactor, Settled,
};
use adam_code_tools_testkit::{DEFAULT_LIMITS, DirWorkspace};
use adam_llm_agent::{KeptOutputs, ToolCtx, ToolError, ToolOutput, output_id_of};
use adam_runtime::{CollectingSink, RunEvent};
use adam_workspace::EnvSession;
use async_trait::async_trait;

const NOTE: &str = "\nA note of the workspace.";
const PUBLISHED: &str = "\nPUBLISHED NOTE";
const SECRET: &str = "s3cret-scripted";

/// What the workspace and its guard were asked, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Guard(Guarding),
    AfterRun,
    Settle,
}

type Log = Arc<Mutex<Vec<Call>>>;

/// `DirWorkspace`, with a guard that answers as scripted and hooks that record their calls.
struct Scripted {
    inner: DirWorkspace,
    /// The guard's refusal, or what it settles to.
    verdict: Result<Settled, String>,
    /// What the workspace says of a missing tool.
    missing: Option<String>,
    /// `after_run` fails.
    after_run_fails: bool,
    calls: Log,
}

impl Scripted {
    fn over(dir: &Path, verdict: Result<Settled, String>) -> Self {
        Self {
            inner: DirWorkspace::new([SECRET])
                .place("work", dir)
                .unwrap()
                .read_only_place("docs", dir)
                .unwrap(),
            verdict,
            missing: None,
            after_run_fails: false,
            calls: Log::default(),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// The same workspace, whose tools default to `output` (the coder's folder sets these).
    fn with_output(mut self, output: OutputDefaults) -> Self {
        self.inner = self.inner.with_limits(CommandLimits {
            output,
            ..DEFAULT_LIMITS
        });
        self
    }
}

struct Guard {
    settled: Settled,
    calls: Log,
}

#[async_trait]
impl CommandGuard for Guard {
    async fn settle(self: Box<Self>) -> Settled {
        self.calls.lock().unwrap().push(Call::Settle);
        self.settled
    }
}

#[async_trait]
impl CodeWorkspace for Scripted {
    async fn place(
        &self,
        ctx: &ToolCtx,
        repo: Option<&str>,
        intent: Intent,
    ) -> Result<Place, Outcome> {
        CodeWorkspace::place(&self.inner, ctx, repo, intent).await
    }

    fn redactor(&self) -> &Redactor {
        self.inner.redactor()
    }

    async fn after_write(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        _paths: &[String],
    ) -> Result<String, ToolError> {
        Ok(String::new())
    }
}

#[async_trait]
impl CommandWorkspace for Scripted {
    async fn session(&self, ctx: &ToolCtx) -> Result<Arc<dyn EnvSession>, ToolError> {
        self.inner.session(ctx).await
    }

    fn limits(&self) -> CommandLimits {
        self.inner.limits()
    }

    fn kept(&self) -> &dyn KeptOutputs {
        self.inner.kept()
    }

    async fn guard(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        guarding: Guarding,
    ) -> Result<Box<dyn CommandGuard>, String> {
        self.calls.lock().unwrap().push(Call::Guard(guarding));
        self.verdict.clone().map(|settled| {
            Box::new(Guard {
                settled,
                calls: Arc::clone(&self.calls),
            }) as Box<dyn CommandGuard>
        })
    }

    async fn after_run(&self, _ctx: &ToolCtx, _place: &Place) -> Result<String, ToolError> {
        self.calls.lock().unwrap().push(Call::AfterRun);
        if self.after_run_fails {
            return Err(ToolError::Permanent("the notes cannot be written".into()));
        }
        Ok(PUBLISHED.into())
    }

    fn refusal_note(&self, _ctx: &ToolCtx, _place: &Place) -> String {
        NOTE.to_owned()
    }

    async fn missing_tool_answer(
        &self,
        _ctx: &ToolCtx,
        _place: &Place,
        missing: &MissingTool,
        _dirs: &[&Path],
        _session: &dyn EnvSession,
    ) -> Result<Option<String>, ToolError> {
        Ok(self
            .missing
            .as_ref()
            .map(|said| format!("{said}: {}", missing.name)))
    }
}

/// A context whose progress lines are collected.
fn ctx() -> (ToolCtx, CollectingSink) {
    let sink = CollectingSink::new();
    let ctx = ToolCtx::detached("scripted", "call-1", Arc::new(sink.clone()));
    (ctx, sink)
}

fn progress(sink: &CollectingSink) -> Vec<String> {
    sink.events()
        .into_iter()
        .filter_map(|e| match e.event {
            RunEvent::Step(step) if step.parent.is_none() => step.detail,
            _ => None,
        })
        .collect()
}

/// `command` run by `run` over a workspace whose guard answers `verdict`: the answer, the progress
/// lines, the workspace, and the call's context (its output id, its run).
async fn make(
    dir: &Path,
    verdict: Result<Settled, String>,
    command: &str,
) -> (ToolOutput, Vec<String>, Scripted, ToolCtx) {
    make_as(dir, verdict, command, None).await
}

/// [`make`] with the call's `output`.
async fn make_as(
    dir: &Path,
    verdict: Result<Settled, String>,
    command: &str,
    output: Option<&str>,
) -> (ToolOutput, Vec<String>, Scripted, ToolCtx) {
    let ws = Scripted::over(dir, verdict);
    let (ctx, sink) = ctx();
    let out = run(
        &ws,
        &ctx,
        command.into(),
        None,
        None,
        None,
        output.map(str::to_owned),
    )
    .await
    .unwrap();
    (out, progress(&sink), ws, ctx)
}

/// What `echo hi` is shown as by default: it succeeded, so one line names where its output is kept.
fn rendered(ctx: &ToolCtx) -> String {
    let id = output_id_of(ctx);
    format!(
        "$ echo hi\nexit code 0\n[output: 3 bytes, not shown because the command succeeded; kept \
         as {id}; read_output {{\"id\":\"{id}\"}} reads it]\n"
    )
}

/// The first page of the output the call of `ctx` kept, as `read_output` reads it.
async fn kept_page(ws: &Scripted, ctx: &ToolCtx) -> String {
    let run = ctx.root_run_id().to_string();
    ws.kept()
        .page(&run, &output_id_of(ctx), 1, 100)
        .await
        .unwrap()
}

/// What a command that ran asked, in order: the guard, the guard settled, then `after_run` (ANR-1455:
/// the report `after_run` reads is what `settle` records).
const RAN: [Call; 3] = [Call::Guard(Guarding::Make), Call::Settle, Call::AfterRun];

/// What `echo hi` is shown as with `output: "full"`.
const FULL: &str = "$ echo hi\nexit code 0\n--- output ---\nhi\n";
const NOT_LISTED: &str = "--- the worktree now differs from HEAD in (git status; files the \
                          project ignores are kept too) ---\n(nothing that git lists: the files \
                          it changed are ignored by the project, or are back as HEAD has them)\nThe \
                          files changed, so run the \
                          project's checks again before you commit.\n";
const NO_STATUS: &str = "--- the worktree now differs from HEAD in (git status; files the \
                         project ignores are kept too) ---\n(git could not say which files \
                         changed)\nThe files changed, so run the project's checks again before \
                         you commit.\n";
const RESTORED: &str =
    "Everything it did was undone, its files included: the worktree is exactly as it was.";
const NOT_RESTORED: &str = "It could not be fully undone (HEAD and the branch are back, but the \
                            files may differ): run `git status` with run_command to see the \
                            worktree before you go on.";

fn touched_git(command: &str, what: &str, output: &str) -> String {
    format!(
        "`{command}` changed git (HEAD, the branch, a ref, `.git` or the git configuration), and \
         `run` may change files and nothing of git. {what} Commit with commit_and_push, and run \
         the command again without the git part. Output of the command, for what it is \
         worth:\n{output}{NOTE}"
    )
}

fn unverified(command: &str, what: &str, output: &str) -> String {
    format!(
        "After `{command}` the state of the worktree's git could not be read, so it is not known \
         whether the command changed git; to be safe it was treated as if it had, since `run` may \
         change files and nothing of git. {what} If the command touches git (HEAD, the branch, a \
         ref, `.git` or the git configuration), commit with commit_and_push and run it again \
         without the git part; if it does not, try it again, and if this persists, tell the \
         person. Output of the command, for what it is worth:\n{output}{NOTE}"
    )
}

#[tokio::test]
async fn files_that_changed_are_listed_with_the_text_of_after_run() {
    let dir = tempfile::tempdir().unwrap();
    let (out, said, ws, ctx) = make(dir.path(), Ok(Settled::FilesChanged), "echo hi").await;
    assert!(!out.is_error, "{}", out.content);
    // No repository here: git has no answer, and the note says so.
    assert_eq!(
        out.content,
        format!("{}{NO_STATUS}{PUBLISHED}", rendered(&ctx))
    );
    assert_eq!(said, ["running: echo hi", "changed files: echo hi"]);
    assert_eq!(ws.calls(), RAN);
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// In a repository, what `git status` lists after the command, exactly as it lists it: the first
/// entry keeps its leading space (` M a.txt`, a change not staged).
#[tokio::test]
async fn the_files_listed_are_what_git_status_says() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-qm", "a"]);
    let command = "printf b > a.txt; printf x > made.txt";
    let (out, said, ws, _) = make(dir.path(), Ok(Settled::FilesChanged), command).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        format!(
            "$ {command}\nexit code 0\n(no output)\n--- the worktree now differs from HEAD \
             in (git status; files the project ignores are kept too) ---\n M a.txt\n?? made.txt\n\
             The files changed, so run the project's checks again before you commit.\n{PUBLISHED}"
        )
    );
    assert_eq!(
        said,
        [
            format!("running: {command}"),
            format!("changed files: {command}")
        ]
    );
    assert_eq!(ws.calls(), RAN);
}

/// In a repository where git lists nothing (the command put a file back as `HEAD` has it), files
/// that changed are said to be ignored or back as `HEAD` has them.
#[tokio::test]
async fn files_that_changed_with_none_listed_are_said_ignored_or_back_as_head() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-qm", "a"]);
    let (out, _said, ws, ctx) = make(dir.path(), Ok(Settled::FilesChanged), "echo hi").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        format!("{}{NOT_LISTED}{PUBLISHED}", rendered(&ctx))
    );
    assert_eq!(ws.calls(), RAN);
}

#[tokio::test]
async fn an_unchanged_run_says_git_sees_no_change_and_no_more() {
    let dir = tempfile::tempdir().unwrap();
    let (out, said, ws, ctx) = make(dir.path(), Ok(Settled::Unchanged), "echo hi").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        format!(
            "{}(git sees no change in the worktree; files the project ignores are kept too, and \
             are not listed)\n",
            rendered(&ctx)
        )
    );
    assert_eq!(said, ["running: echo hi", "ran: echo hi"]);
    assert_eq!(ws.calls(), RAN);
}

#[tokio::test]
async fn an_unwatched_run_is_the_result_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (out, said, ws, ctx) = make(dir.path(), Ok(Settled::Unwatched), "echo hi").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, rendered(&ctx));
    assert_eq!(said, ["running: echo hi", "ran: echo hi"]);
    assert_eq!(ws.calls(), RAN);
}

#[tokio::test]
async fn a_run_that_touched_git_is_refused_with_the_note() {
    let dir = tempfile::tempdir().unwrap();
    for (restored, what) in [(true, RESTORED), (false, NOT_RESTORED)] {
        let (out, said, ws, _) =
            make(dir.path(), Ok(Settled::Undone { restored }), "echo hi").await;
        assert!(out.is_error, "{restored}: {}", out.content);
        assert_eq!(
            out.content,
            touched_git("echo hi", what, "hi\n"),
            "{restored}"
        );
        assert!(!out.content.contains(PUBLISHED), "{}", out.content);
        assert_eq!(
            said,
            ["running: echo hi", "undid a change to git made by: echo hi"],
            "{restored}"
        );
        assert_eq!(ws.calls(), RAN, "{restored}");
    }
}

#[tokio::test]
async fn a_run_after_which_git_cannot_be_read_is_refused_with_the_note() {
    let dir = tempfile::tempdir().unwrap();
    for (restored, what) in [(true, RESTORED), (false, NOT_RESTORED)] {
        let (out, said, ws, _) =
            make(dir.path(), Ok(Settled::Unreadable { restored }), "echo hi").await;
        assert!(out.is_error, "{restored}: {}", out.content);
        assert_eq!(
            out.content,
            unverified("echo hi", what, "hi\n"),
            "{restored}"
        );
        assert!(!out.content.contains(PUBLISHED), "{}", out.content);
        assert_eq!(
            said,
            [
                "running: echo hi",
                "could not check git after: echo hi; undid it"
            ],
            "{restored}"
        );
        assert_eq!(ws.calls(), RAN, "{restored}");
    }
}

#[tokio::test]
async fn a_refusal_of_the_guard_is_the_answer_with_the_note_and_nothing_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (out, said, ws, _) = make(
        dir.path(),
        Err("Nothing was run.".into()),
        "printf x > ran.txt",
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.content, format!("Nothing was run.{NOTE}"));
    assert!(said.is_empty(), "{said:?}");
    assert!(!dir.path().join("ran.txt").exists());
    assert_eq!(ws.calls(), [Call::Guard(Guarding::Make)]);
}

/// The note is the guard's: a blank command, a timeout out of bounds, an unknown place, a place that
/// may only be read and a `cwd` outside the place are answered without it, the guard is not taken
/// and nothing follows.
#[tokio::test]
async fn a_refusal_before_the_guard_has_no_note_and_no_after_run() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Scripted::over(dir.path(), Ok(Settled::FilesChanged));
    let (ctx, sink) = ctx();
    for (command, cwd, repo, timeout, output, said) in [
        ("  ", None, None, None, None, "command is required"),
        (
            "ls",
            None,
            None,
            Some(0),
            None,
            "timeout must be at least 1 second",
        ),
        (
            "ls",
            None,
            None,
            None,
            Some("loud"),
            "output must be \"on_error\" (a command that succeeds is one line, and read_output \
             reads its output) or \"full\" (the output whatever the exit), not \"loud\"; leave \
             it out for the tool's default, \"on_error\"",
        ),
        ("ls", None, Some("elsewhere"), None, None, "no such place"),
        (
            "ls",
            None,
            Some("docs"),
            None,
            None,
            "can be read, not written",
        ),
        (
            "ls",
            Some(".."),
            None,
            None,
            None,
            "must be a relative path",
        ),
    ] {
        let out = run(
            &ws,
            &ctx,
            command.into(),
            cwd.map(str::to_owned),
            repo.map(str::to_owned),
            timeout,
            output.map(str::to_owned),
        )
        .await
        .unwrap();
        assert!(out.is_error, "{said}");
        assert!(out.content.contains(said), "{said}: {}", out.content);
        assert!(!out.content.contains(NOTE), "{said}: {}", out.content);
    }
    assert!(ws.calls().is_empty(), "{:?}", ws.calls());
    assert!(progress(&sink).is_empty());
}

/// A failing `after_run` is the tool's error, and the guard is already settled, so a change to git
/// the command made is undone all the same.
#[tokio::test]
async fn a_failing_after_run_is_the_error_once_the_guard_is_settled() {
    let dir = tempfile::tempdir().unwrap();
    let mut ws = Scripted::over(dir.path(), Ok(Settled::Undone { restored: true }));
    ws.after_run_fails = true;
    let (ctx, sink) = ctx();
    let err = run(&ws, &ctx, "echo hi".into(), None, None, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ToolError::Permanent(said) if said == "the notes cannot be written"),
        "{err:?}"
    );
    assert_eq!(ws.calls(), RAN);
    assert_eq!(progress(&sink), ["running: echo hi"]);
}

#[tokio::test]
async fn a_missing_tool_is_the_workspaces_answer_or_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let command = "no-such-tool-7c41 build";

    let mut ws = Scripted::over(dir.path(), Ok(Settled::Unchanged));
    ws.missing = Some("missing".into());
    let (ctx, sink) = ctx();
    let out = run(&ws, &ctx, command.into(), None, None, None, None)
        .await
        .unwrap();
    assert!(out.is_error);
    assert_eq!(out.content, "missing: no-such-tool-7c41");
    assert_eq!(
        progress(&sink),
        [
            format!("running: {command}"),
            "the workspace lacks a tool".to_owned()
        ]
    );
    // The command ran: what follows it was done.
    assert_eq!(ws.calls(), RAN);

    // No answer of the workspace: the command's own result, exit code and all.
    let (out, said, ws, _) = make(dir.path(), Ok(Settled::Unwatched), command).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content
            .starts_with(&format!("$ {command}\nexit code 127\n--- output ---\n")),
        "{}",
        out.content
    );
    assert!(!out.content.contains(NOTE), "{}", out.content);
    assert_eq!(
        said,
        [format!("running: {command}"), format!("ran: {command}")]
    );
    assert_eq!(ws.calls(), RAN);
}

/// A secret is scrubbed from the `$` line and the progress, and a result of a command that failed
/// carries the repeat key of how it ended.
#[tokio::test]
async fn the_command_is_scrubbed_and_a_result_has_its_repeat_key() {
    let dir = tempfile::tempdir().unwrap();
    let command = format!("echo {SECRET} >/dev/null; exit 3");
    let (out, said, _, _) = make(dir.path(), Ok(Settled::FilesChanged), &command).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(!out.content.contains(SECRET), "{}", out.content);
    assert!(said.iter().all(|line| !line.contains(SECRET)), "{said:?}");
    assert_eq!(
        out.repeat_key.as_deref().map(String::as_str),
        Some("exit:3")
    );
}

/// By default a `run` that succeeded is one line, and its output, short as it is, is kept for
/// `read_output`; with `full` it is shown as it always was.
#[tokio::test]
async fn a_run_that_succeeded_is_one_line_by_default_and_its_output_with_full() {
    let dir = tempfile::tempdir().unwrap();
    for output in [None, Some("on_error")] {
        let (out, said, ws, ctx) =
            make_as(dir.path(), Ok(Settled::Unwatched), "echo hi", output).await;
        assert!(!out.is_error, "{output:?}: {}", out.content);
        assert_eq!(out.content, rendered(&ctx), "{output:?}");
        assert_eq!(said, ["running: echo hi", "ran: echo hi"]);
        assert!(kept_page(&ws, &ctx).await.starts_with("hi\n"), "{output:?}");
    }
    let (out, _, _, _) = make_as(dir.path(), Ok(Settled::Unwatched), "echo hi", Some("full")).await;
    assert_eq!(out.content, FULL);
}

/// A `run` that failed shows its output by default, as `full` does.
#[tokio::test]
async fn a_run_that_failed_shows_its_output_by_default() {
    let dir = tempfile::tempdir().unwrap();
    for output in [None, Some("on_error"), Some("full")] {
        let (out, _, _, _) = make_as(
            dir.path(),
            Ok(Settled::Unwatched),
            "echo no; exit 2",
            output,
        )
        .await;
        assert_eq!(
            out.content, "$ echo no; exit 2\nexit code 2\n--- output ---\nno\n",
            "{output:?}"
        );
    }
}

/// A `run` that succeeded printing nothing says so, and names no id.
#[tokio::test]
async fn a_run_that_succeeded_printing_nothing_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (out, _, _, _) = make(dir.path(), Ok(Settled::Unwatched), "true").await;
    assert_eq!(out.content, "$ true\nexit code 0\n(no output)\n");
}

/// A workspace that defaults `run` to `full` (the coder's folder, `limits.output.run`) shows a call
/// that gives no `output` whole; a call's `on_error` still wins; and `run_command`'s default does
/// not touch `run` (ADR 0090 slice T3).
#[tokio::test]
async fn the_workspaces_default_is_what_a_call_without_output_shows() {
    let dir = tempfile::tempdir().unwrap();
    let full = OutputDefaults {
        run: OutputMode::Full,
        ..OutputDefaults::TOOLS
    };
    let ws = Scripted::over(dir.path(), Ok(Settled::Unwatched)).with_output(full);
    let (call, _) = ctx();
    let out = run(&ws, &call, "echo hi".into(), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(out.content, FULL, "the workspace's default");

    let (call, _) = ctx();
    let asked = Some("on_error".to_owned());
    let out = run(&ws, &call, "echo hi".into(), None, None, None, asked)
        .await
        .unwrap();
    assert_eq!(out.content, rendered(&call), "the call's `output` wins");

    let looks = OutputDefaults {
        run_command: OutputMode::OnError,
        ..OutputDefaults::TOOLS
    };
    let ws = Scripted::over(dir.path(), Ok(Settled::Unwatched)).with_output(looks);
    let (call, _) = ctx();
    let out = run(&ws, &call, "echo hi".into(), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(out.content, rendered(&call), "`run` keeps its own default");
}
