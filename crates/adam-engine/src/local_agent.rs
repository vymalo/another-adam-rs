//! The local agent over a person's directory (ADR 0047 slice 7, part 2): the file and command
//! tools of [`adam-code-tools`] over a [`PersonDir`], with a host-side hook that asks before every
//! write or command.
//!
//! The builder option [`EngineBuilder::local_agent`](crate::EngineBuilder::local_agent) adds this
//! to the agent: the [`PersonDir`] it owns is shared with the tools through
//! [`State<T>`](adam_llm_agent::State), and the [`AsksBeforeWritingOrCommand`] hook is installed
//! with [`EngineBuilder::plugin`](crate::EngineBuilder::plugin). Reads pass without asking; writes
//! and commands park the run for the person's yes, decided with the existing
//! [`Engine::approve`](crate::Engine::approve).
//!
//! The schemas and doc comments are the coder's, the source of the tool contract the model sees:
//! [`read_file`], [`read_files`], [`write_file`], [`edit_file`] and [`apply_patch`] follow
//! `bin/adam-coder/src/tools/files.rs`, [`run_command`] follows `bin/adam-coder/src/tools/inspect.rs`
//! and [`run`] follows `bin/adam-coder/src/tools/make.rs`. The `repo` (slot/repo) argument of every
//! coder tool is dropped here: [`PersonDir`] has one place, named `person` (or whatever name
//! [`PersonDir::named`] was given), and the model never picks one.

use std::sync::Arc;

use adam_code_tools::{PersonDir, files, inspect, make};
use adam_llm_agent::{
    HookResult, HookedCall, Plugin, State, ToolCtx, ToolError, ToolOutput, Verdict,
};
use adam_macros::tool;

/// What the local agent's hook asks: writes (file tools that change a file, `apply_patch` included)
/// and commands (`run_command`, `run`). Reads never ask.
///
/// A yes runs the call once, with the existing [`Engine::approve`](crate::Engine::approve); a no
/// refuses it. The call's `tool` and the arguments the host reads are shown through
/// [`EngineBuilder::step_io`](crate::EngineBuilder::step_io), never the raw arguments.
pub struct AsksBeforeWritingOrCommand;

impl Plugin for AsksBeforeWritingOrCommand {
    fn name(&self) -> &str {
        "asks-before-writing-or-command"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        let question = match call.tool {
            // File writes — every path that changes, named, so the person reads what the run
            // would touch.
            "write_file" => {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if path.is_empty() {
                    return Ok(Verdict::Continue);
                }
                format!("Write the file `{path}` in the person's directory?")
            }
            "edit_file" => {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if path.is_empty() {
                    return Ok(Verdict::Continue);
                }
                format!("Edit the file `{path}` in the person's directory?")
            }
            "apply_patch" => {
                let patch = call
                    .arguments
                    .get("patch")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                // A patch may touch several paths; name the ones it names (the first line of each
                // hunk header), so the person reads the surface, not the diff.
                let paths: Vec<&str> = patch
                    .lines()
                    .filter_map(|line| line.strip_prefix("+++ b/"))
                    .take(8)
                    .collect();
                if paths.is_empty() {
                    return Ok(Verdict::Continue);
                }
                format!(
                    "Apply a patch to {} file(s) in the person's directory ({})?",
                    paths.len(),
                    paths.join(", ")
                )
            }
            // Commands — the shell line, so the person reads what the run would run.
            "run_command" => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if command.is_empty() {
                    return Ok(Verdict::Continue);
                }
                format!("Run a command in the person's directory: `{command}`?")
            }
            "run" => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if command.is_empty() {
                    return Ok(Verdict::Continue);
                }
                format!(
                    "Run a command in the person's directory (it may change files): `{command}`?"
                )
            }
            // Reads — `read_file`, `read_files`, and any future read-only tool the host adds.
            _ => return Ok(Verdict::Continue),
        };
        Ok(Verdict::Ask(question))
    }
}

/// The local agent's tools, over `State<PersonDir>`. The bodies are [`adam-code-tools`]', with the
/// coder's `repo` argument dropped (`PersonDir` has one place).
///
/// A tool that names `State<PersonDir>` declares the type in its generated `required_state`, so
/// [`crate::EngineBuilder::local_agent`] failing to register the directory is a build error, not a
/// run error.

/// Read a text file of the person's directory, one page at a time (at most 2000 lines). Each line
/// starts with its number. If the last line says `[more: ... offset=N]`, call again with offset N.
/// A negative offset reads from the end. A binary file is not shown, except an image (PNG, JPEG,
/// GIF or WebP) when you can see images: it comes after the result. Example:
/// {"path":"notes.md","offset":2001}
///
/// The coder's `repo` argument is dropped: a `PersonDir` has one place.
#[tool(
    label = "Read a file",
    icon = "read",
    paged,
    parallel = "read",
    path = "path",
    crate = ::adam_llm_agent
)]
#[allow(clippy::too_many_arguments)] // the model's arguments, one each
pub async fn read_file(
    person: State<PersonDir>,
    ctx: &ToolCtx,
    /// Path relative to the person's directory
    path: String,
    /// First line, from 1; -50 = the last 50 lines
    offset: Option<i64>,
    /// Most lines (at most 2000)
    limit: Option<i64>,
    /// Old name of offset
    start_line: Option<u32>,
    /// Last line; limit = end_line - start_line + 1
    end_line: Option<u32>,
) -> Result<ToolOutput, ToolError> {
    files::read_file(
        &*person, ctx, path, offset, limit, start_line, end_line, None,
    )
    .await
}

/// Read several text files of the person's directory in one call: 1 to 16 paths, relative to the
/// directory, in the order you give them. Each file comes behind a line that names it, then its
/// numbered lines (the numbers of `read_file`): its first page, at most 400 lines or 8 KiB. A page
/// that was cut ends with `[more: N lines left; next page: read_file path=<path> offset=K]`: read on
/// with read_file. The whole result is at most 32 KiB: a file that did not fit is one line,
/// `[not shown: read it alone with read_file path=<path>]`. A file that is not there, is binary or
/// is not UTF-8 is one line that says why, and the other files are still shown. Example:
/// {"paths":["notes.md","todo.md"]}
///
/// The coder's `repo` argument is dropped: a `PersonDir` has one place.
#[tool(label = "Read files", icon = "read", parallel = "read", crate = ::adam_llm_agent)]
pub async fn read_files(
    person: State<PersonDir>,
    ctx: &ToolCtx,
    /// Paths relative to the person's directory, in the order to show them (1 to 16)
    paths: Vec<String>,
) -> Result<ToolOutput, ToolError> {
    files::read_files(&*person, ctx, paths, None).await
}

/// Create a file, or replace one, with exactly `content`. Parent directories are created. The path
/// is relative to the person's directory; nothing inside `.git` and nothing through a symlink can
/// be written. The person is asked before the write (`AsksBeforeWritingOrCommand`); `/approve` runs
/// it once, `/deny [reason]` leaves the file alone and tells you why. For a change to part of a file
/// use edit_file (exact text replaced) or apply_patch (a diff); for a broad, multi-file change write
/// the files one by one.
///
/// The coder's `repo` argument is dropped: a `PersonDir` has one place.
#[tool(
    label = "Write a file",
    icon = "edit",
    parallel = "write",
    path = "path",
    crate = ::adam_llm_agent
)]
pub async fn write_file(
    person: State<PersonDir>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the person's directory
    path: String,
    /// The whole new content of the file (at most 1 MiB)
    content: String,
) -> Result<ToolOutput, ToolError> {
    files::write_file(&*person, ctx, path, content, None).await
}

/// Apply a unified diff to the person's directory: one or several files, `--- a/<path>` and
/// `+++ b/<path>` then hunks (`@@ -12,3 +12,4 @@`, with context lines), as `git diff` writes them.
/// Every path is checked first (inside the directory, not `.git`, not a symlink); a patch that
/// creates a symlink is refused; and it is all or nothing: if a hunk does not match the file,
/// nothing is changed and you are told why. Read the file first, so the context lines match it
/// exactly. The person is asked before the patch (`AsksBeforeWritingOrCommand`); `/approve` runs it
/// once, `/deny [reason]` leaves the files alone. Use it for several changes at once, in one file
/// or several; for one exact replacement edit_file is simpler. A broad, multi-file change is several
/// of these, or several files written one by one.
///
/// The coder's `repo` argument is dropped: a `PersonDir` has one place.
#[tool(label = "Apply a patch", icon = "edit", parallel = "write", crate = ::adam_llm_agent)]
pub async fn apply_patch(
    person: State<PersonDir>,
    ctx: &ToolCtx,
    /// The unified diff, at most 1 MiB, with `a/` and `b/` before the paths
    patch: String,
) -> Result<ToolOutput, ToolError> {
    files::apply_patch(&*person, ctx, patch, None).await
}

/// Replace exact text in a file of the person's directory: every character of `old`, blanks and
/// line breaks included, is replaced by `new`. It is the way to make a small change without
/// writing a diff: read the file, copy the lines you mean to change as `old`, write what they become
/// as `new`. The exact match is tried first; when it fails, line endings and trailing blanks are
/// tolerated; leading whitespace is not. It refuses, and changes nothing, when `old` is not in the
/// file (the error shows the closest region with line numbers, so you can copy it right) or when
/// `old` is in the file more than once and `replace_all` is not set (the error lists the lines:
/// add more of the text around it to make it unique). The path is relative to the person's
/// directory; nothing inside `.git` and nothing through a symlink can be edited. The file must be
/// text of at most 1 MiB. The person is asked before the edit
/// (`AsksBeforeWritingOrCommand`); `/approve` runs it once, `/deny [reason]` leaves the file alone.
/// To create a file or replace it whole use write_file, for several files at once apply_patch.
///
/// The coder's `repo` argument is dropped: a `PersonDir` has one place.
#[tool(
    label = "Edit a file",
    icon = "edit",
    parallel = "write",
    path = "path",
    crate = ::adam_llm_agent
)]
pub async fn edit_file(
    person: State<PersonDir>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the person's directory
    path: String,
    /// The text to replace, copied from the file (blanks and line breaks too). Must be in the file; and once, unless replace_all is set.
    old: String,
    /// What it becomes. May be empty, to delete the text.
    new: String,
    /// Replace every place `old` is in. Leave out to replace the one place that has it.
    replace_all: Option<bool>,
) -> Result<ToolOutput, ToolError> {
    files::edit_file(&*person, ctx, path, old, new, replace_all, None).await
}

/// Look around in the person's directory with a shell command: `ls`, `cat README.md`, `grep -rn name
/// .`. It runs in the workspace's environment (a `LocalSession`), with the directory's time limit
/// and output cap. You get the exit code and the tail of the output. It costs no check cycle and
/// reports no checks, and it is for looking: changes it makes to the directory are reported (the
/// directory is **never restored**, and the run's listing tells you what changed). The person is
/// asked before it runs (`AsksBeforeWritingOrCommand`); `/approve` runs it once, `/deny [reason]`
/// leaves the directory alone. Use it to look; for a change use `run`, `write_file`, `edit_file` or
/// `apply_patch`. Long output is kept: you get its head and tail and an id to read the rest with
/// `read_output`.
///
/// The coder's `repo` argument is dropped: a `PersonDir` has one place.
#[tool(label = "Run a command", icon = "execute", crate = ::adam_llm_agent)]
pub async fn run_command(
    person: State<PersonDir>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the directory
    command: String,
    /// Optional sub-directory of the person's directory to run in (relative, inside it)
    cwd: Option<String>,
    /// Seconds before the command is killed; at most the directory's limit, which is also the default. Example: {"command":"ls -la","timeout":10}
    timeout: Option<i64>,
) -> Result<ToolOutput, ToolError> {
    inspect::run_command(&*person, ctx, command, cwd, None, timeout).await
}

/// Make something with a shell command in the person's directory and keep what it changes:
/// generate or export a file, install dependencies, run a formatter, build an artifact. It runs in
/// the workspace's environment (a `LocalSession`), with the directory's time limit and output cap.
/// You get the exit code, the tail of the output and the files that changed. Unlike `run_command`
/// (for looking, the directory is reported) the files stay, and unlike `run_checks` it is not a
/// check: it costs no check cycle and nothing reports it. The person is asked before it runs
/// (`AsksBeforeWritingOrCommand`); `/approve` runs it once, `/deny [reason]` leaves the directory
/// alone. After it changes files, the run's listing tells you which ones. Show a file it made with
/// `share_file`.
///
/// The coder's `repo` argument is dropped: a `PersonDir` has one place.
#[tool(label = "Make files with a command", icon = "execute", crate = ::adam_llm_agent)]
pub async fn run(
    person: State<PersonDir>,
    ctx: &ToolCtx,
    /// Shell command, run with `bash -lc` (`sh -lc` without bash) in the directory
    command: String,
    /// Optional sub-directory of the person's directory to run in (relative, inside it)
    cwd: Option<String>,
    /// Seconds before the command is killed; at most the directory's limit, which is also the default. Example: {"command":"npm install","timeout":120}
    timeout: Option<i64>,
) -> Result<ToolOutput, ToolError> {
    make::run(&*person, ctx, command, cwd, None, timeout).await
}

/// The tool set the local agent registers on top of the engine's screen tools: every file and
/// command tool over `PersonDir`, ready to share the same [`State<PersonDir>`](adam_llm_agent::State)
/// with the hook.
pub fn tool_set() -> adam_llm_agent::ToolSet {
    adam_llm_agent::tools![
        ReadFile, ReadFiles, WriteFile, EditFile, ApplyPatch, RunCommand, Run,
    ]
}

/// The hook the local agent installs: asks before every write or command, lets reads through.
pub fn plugin() -> Arc<dyn Plugin> {
    Arc::new(AsksBeforeWritingOrCommand)
}
