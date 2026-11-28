//! `adam_code_tools::inspect::run_command` over a scripted workspace: for each verdict of the guard,
//! what the tool answers and the progress line it adds; the guard's refusal (nothing runs); the
//! refusal note on refusals only; a missing tool answered by the workspace or left to the result;
//! that `run_command` never calls `after_run`; and its `output`: the whole output by default,
//! one line for a command that succeeded with `on_error` (the output kept for `read_output`), and
//! the workspace's default for a call that gives none.
//! (`output` is unit-tested in `adam_code_tools::output`.)
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use adam_code_tools::inspect::run_command;
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
const SECRET: &str = "s3cret-scripted";

/// `DirWorkspace`, with a guard that answers as scripted and hooks that record their calls.
struct Scripted {
    inner: DirWorkspace,
    /// The guard's refusal, or what it settles to.
    verdict: Result<Settled, String>,
    /// What the workspace says of a missing tool.
    missing: Option<String>,
    guarded: Mutex<Vec<Guarding>>,
    after_runs: AtomicUsize,
}

impl Scripted {
    fn over(dir: &Path, verdict: Result<Settled, String>) -> Self {
        Self {
            inner: DirWorkspace::new([SECRET]).place("work", dir).unwrap(),
            verdict,
            missing: None,
            guarded: Mutex::default(),
            after_runs: AtomicUsize::new(0),
        }
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

struct Guard(Settled);

#[async_trait]
impl CommandGuard for Guard {
    async fn settle(self: Box<Self>) -> Settled {
        self.0
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
        self.guarded.lock().unwrap().push(guarding);
        self.verdict
            .clone()
            .map(|settled| Box::new(Guard(settled)) as Box<dyn CommandGuard>)
    }

    async fn after_run(&self, _ctx: &ToolCtx, _place: &Place) -> Result<String, ToolError> {
        self.after_runs.fetch_add(1, Ordering::SeqCst);
        Ok("\nAFTER RUN".into())
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

/// `command` run by `run_command` over a workspace whose guard answers `verdict`: the answer, the
/// progress lines, and the workspace.
async fn look(
    dir: &Path,
    verdict: Result<Settled, String>,
    command: &str,
) -> (ToolOutput, Vec<String>, Scripted) {
    let (out, said, ws, _) = look_as(dir, verdict, command, None).await;
    (out, said, ws)
}

/// [`look`] with the call's `output`, and the call's context (its output id, its run).
async fn look_as(
    dir: &Path,
    verdict: Result<Settled, String>,
    command: &str,
    output: Option<&str>,
) -> (ToolOutput, Vec<String>, Scripted, ToolCtx) {
    let ws = Scripted::over(dir, verdict);
    let (ctx, sink) = ctx();
    let out = run_command(
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

/// The line a command that succeeded is shown as with `output: "on_error"`.
fn hidden(ctx: &ToolCtx, bytes: u64) -> String {
    let id = output_id_of(ctx);
    format!(
        "[output: {bytes} bytes, not shown because the command succeeded; kept as {id}; \
         read_output {{\"id\":\"{id}\"}} reads it]\n"
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

fn undone(command: &str, what: &str, output: &str) -> String {
    format!(
        "`{command}` changed the worktree (files, HEAD, the branch, a ref or the git \
         configuration), and run_command is for looking around only. {what} To make something \
         with a command use `run`; to change code use write_file, edit_file or apply_patch. \
         Output of the command, for what it is worth:\n{output}{NOTE}"
    )
}

const RESTORED: &str = "The change was undone: the worktree is exactly as it was.";
const NOT_RESTORED: &str = "The change could not be fully undone (HEAD and the branch are back, \
                            but the files may differ): run `git status` with run_command to see \
                            the worktree before you go on.";

#[tokio::test]
async fn an_unchanged_or_unwatched_look_is_the_result_with_no_note() {
    let dir = tempfile::tempdir().unwrap();
    for verdict in [Settled::Unchanged, Settled::Unwatched] {
        let (out, said, ws) = look(dir.path(), Ok(verdict), "echo hi").await;
        assert!(!out.is_error, "{verdict:?}: {}", out.content);
        assert_eq!(
            out.content, "$ echo hi\nexit code 0\n--- output ---\nhi\n",
            "{verdict:?}"
        );
        assert_eq!(said, ["running: echo hi"], "{verdict:?}");
        assert_eq!(*ws.guarded.lock().unwrap(), [Guarding::Look]);
        assert_eq!(ws.after_runs.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn files_a_guard_saw_change_and_kept_are_said_in_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let (out, said, ws) = look(dir.path(), Ok(Settled::FilesChanged), "echo hi").await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        "$ echo hi\nexit code 0\n--- output ---\nhi\n(the command changed files here, and this \
         workspace does not undo them: they stay)\n"
    );
    assert_eq!(said, ["running: echo hi", "changed files: echo hi"]);
    assert_eq!(ws.after_runs.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_undone_or_unreadable_look_is_refused_with_the_note() {
    let dir = tempfile::tempdir().unwrap();
    for (verdict, what) in [
        (Settled::Undone { restored: true }, RESTORED),
        (Settled::Undone { restored: false }, NOT_RESTORED),
        (Settled::Unreadable { restored: true }, RESTORED),
        (Settled::Unreadable { restored: false }, NOT_RESTORED),
    ] {
        let (out, said, ws) = look(dir.path(), Ok(verdict), "echo hi").await;
        assert!(out.is_error, "{verdict:?}: {}", out.content);
        assert_eq!(out.content, undone("echo hi", what, "hi\n"), "{verdict:?}");
        assert_eq!(
            said,
            ["running: echo hi", "undid a change made by: echo hi"],
            "{verdict:?}"
        );
        assert_eq!(ws.after_runs.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn a_refusal_of_the_guard_is_the_answer_with_the_note_and_nothing_runs() {
    let dir = tempfile::tempdir().unwrap();
    let (out, said, ws) = look(
        dir.path(),
        Err("Nothing was run.".into()),
        "printf x > ran.txt",
    )
    .await;
    assert!(out.is_error);
    assert_eq!(out.content, format!("Nothing was run.{NOTE}"));
    assert!(said.is_empty(), "{said:?}");
    assert!(!dir.path().join("ran.txt").exists());
    assert_eq!(ws.after_runs.load(Ordering::SeqCst), 0);
}

/// The note is the guard's: an unknown place, a `cwd` outside the place, a timeout out of bounds and
/// a blank command are answered without it, and the guard is not taken.
#[tokio::test]
async fn a_refusal_before_the_guard_has_no_note() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Scripted::over(dir.path(), Ok(Settled::Undone { restored: true }));
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
            Some("quiet"),
            "output must be \"on_error\" (a command that succeeds is one line, and read_output \
             reads its output) or \"full\" (the output whatever the exit), not \"quiet\"; leave \
             it out for the tool's default, \"full\"",
        ),
        ("ls", None, Some("elsewhere"), None, None, "no such place"),
        (
            "ls",
            Some(".."),
            None,
            None,
            None,
            "must be a relative path",
        ),
    ] {
        let out = run_command(
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
    assert!(ws.guarded.lock().unwrap().is_empty());
    assert!(progress(&sink).is_empty());
}

#[tokio::test]
async fn a_missing_tool_is_the_workspaces_answer_or_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let command = "no-such-tool-7c41 build";

    let mut ws = Scripted::over(dir.path(), Ok(Settled::Unchanged));
    ws.missing = Some("missing".into());
    let (ctx, sink) = ctx();
    let out = run_command(&ws, &ctx, command.into(), None, None, None, None)
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

    // No answer of the workspace: the command's own result, exit code and all.
    let (out, said, ws) = look(dir.path(), Ok(Settled::Unchanged), command).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content
            .starts_with(&format!("$ {command}\nexit code 127\n--- output ---\n")),
        "{}",
        out.content
    );
    assert!(!out.content.contains(NOTE), "{}", out.content);
    assert_eq!(said, [format!("running: {command}")]);
    assert_eq!(ws.after_runs.load(Ordering::SeqCst), 0);
}

/// A secret is scrubbed from the `$` line and the progress, and a result of a command that failed
/// carries the repeat key of how it ended.
#[tokio::test]
async fn the_command_is_scrubbed_and_a_result_has_its_repeat_key() {
    let dir = tempfile::tempdir().unwrap();
    let command = format!("echo {SECRET} >/dev/null; exit 3");
    let (out, said, _) = look(dir.path(), Ok(Settled::Unchanged), &command).await;
    assert!(!out.is_error, "{}", out.content);
    assert!(!out.content.contains(SECRET), "{}", out.content);
    assert!(said.iter().all(|line| !line.contains(SECRET)), "{said:?}");
    assert_eq!(
        out.repeat_key.as_deref().map(String::as_str),
        Some("exit:3")
    );
}

/// The default of `run_command` is `full`, asked or not: what a look printed is its answer.
#[tokio::test]
async fn a_look_shows_its_output_by_default_and_with_full() {
    let dir = tempfile::tempdir().unwrap();
    for output in [None, Some("full")] {
        let (out, _, _, _) = look_as(dir.path(), Ok(Settled::Unchanged), "echo hi", output).await;
        assert!(!out.is_error, "{output:?}: {}", out.content);
        assert_eq!(
            out.content, "$ echo hi\nexit code 0\n--- output ---\nhi\n",
            "{output:?}"
        );
    }
}

/// With `on_error`, a look that succeeded is one line, and its output, short as it is, is kept for
/// `read_output`.
#[tokio::test]
async fn a_look_that_succeeded_with_on_error_is_one_line_and_its_output_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (out, said, ws, ctx) = look_as(
        dir.path(),
        Ok(Settled::Unchanged),
        "echo hi",
        Some("on_error"),
    )
    .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        out.content,
        format!("$ echo hi\nexit code 0\n{}", hidden(&ctx, 3))
    );
    assert_eq!(said, ["running: echo hi"]);
    assert!(kept_page(&ws, &ctx).await.starts_with("hi\n"));
}

/// With `on_error`, a look that failed shows its output as `full` does, and one that succeeded
/// printing nothing says so and names no id.
#[tokio::test]
async fn a_look_with_on_error_shows_a_failure_and_says_when_nothing_was_printed() {
    let dir = tempfile::tempdir().unwrap();
    let (out, _, _, _) = look_as(
        dir.path(),
        Ok(Settled::Unchanged),
        "echo no; exit 2",
        Some("on_error"),
    )
    .await;
    assert_eq!(
        out.content,
        "$ echo no; exit 2\nexit code 2\n--- output ---\nno\n"
    );
    assert_eq!(
        out.repeat_key.as_deref().map(String::as_str),
        Some("exit:2")
    );
    let (out, _, _, _) =
        look_as(dir.path(), Ok(Settled::Unchanged), "true", Some("on_error")).await;
    assert_eq!(out.content, "$ true\nexit code 0\n(no output)\n");
}

/// A workspace that defaults `run_command` to `on_error` (the coder's folder,
/// `limits.output.run_command`) shows a look that succeeded and gives no `output` as one line; a
/// call's `full` still wins; and `run`'s default does not touch `run_command` (ADR 0088 slice T3).
#[tokio::test]
async fn the_workspaces_default_is_what_a_look_without_output_shows() {
    let dir = tempfile::tempdir().unwrap();
    let quiet = OutputDefaults {
        run_command: OutputMode::OnError,
        ..OutputDefaults::TOOLS
    };
    let ws = Scripted::over(dir.path(), Ok(Settled::Unchanged)).with_output(quiet);
    let (call, _) = ctx();
    let out = run_command(&ws, &call, "echo hi".into(), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(
        out.content,
        format!("$ echo hi\nexit code 0\n{}", hidden(&call, 3)),
        "the workspace's default"
    );
    assert!(kept_page(&ws, &call).await.starts_with("hi\n"));

    let (call, _) = ctx();
    let asked = Some("full".to_owned());
    let out = run_command(&ws, &call, "echo hi".into(), None, None, None, asked)
        .await
        .unwrap();
    assert_eq!(
        out.content, "$ echo hi\nexit code 0\n--- output ---\nhi\n",
        "the call's `output` wins"
    );

    let builds = OutputDefaults {
        run: OutputMode::Full,
        ..OutputDefaults::TOOLS
    };
    let ws = Scripted::over(dir.path(), Ok(Settled::Unchanged)).with_output(builds);
    let (call, _) = ctx();
    let out = run_command(&ws, &call, "echo hi".into(), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(
        out.content, "$ echo hi\nexit code 0\n--- output ---\nhi\n",
        "`run_command` keeps its own default"
    );
}
