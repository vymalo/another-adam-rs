//! The local agent over a person's directory (ADR 0047 slice 7, part 2): the file and command
//! tools of [`adam-code-tools`](https://github.com/vymalo/another-adam-rs/blob/main/crates/adam-code-tools)
//! over a [`PersonDir`], with a host-side hook that asks before every
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
    Decision, HookResult, HookedCall, Plugin, State, ToolCtx, ToolError, ToolOutput,
};
use adam_macros::tool;

/// What the local agent's hook asks: writes (file tools that change a file, `apply_patch` included)
/// and commands (`run_command`, `run`). Reads never ask.
///
/// The hook is fail-closed: every one of the five mutating tools returns `Decision::Ask` always — even
/// when its argument is missing or empty, so a script the model writes without a path cannot write
/// to whatever path the tool would pick on its own. The question names what it can and degrades
/// gracefully, so the person reads the surface and the run never bypasses the approval.
///
/// A yes runs the call once, with the existing [`Engine::approve`](crate::Engine::approve); a no
/// refuses it. The call's `tool` and the arguments the host reads are shown through
/// [`EngineBuilder::step_io`](crate::EngineBuilder::step_io), never the raw arguments.
pub struct AsksBeforeWritingOrCommand;

/// The longest list of patch paths the question names before saying "and N more".
const PATHS_IN_QUESTION: usize = 8;

impl Plugin for AsksBeforeWritingOrCommand {
    fn name(&self) -> &str {
        "asks-before-writing-or-command"
    }

    fn before_tool(&self, call: &mut HookedCall<'_>) -> HookResult {
        let question = match call.tool {
            // File writes — every path that changes, named, so the person reads what the run
            // would touch. A missing or empty path still asks: the tool would write somewhere, and
            // the person reads the question before approving a name the model did not name.
            "write_file" => match call
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .filter(|p| !p.is_empty())
            {
                Some(path) => format!("Write the file `{path}` in the person's directory?"),
                None => "Write a file in the person's directory? The model did not name a path."
                    .to_owned(),
            },
            "edit_file" => match call
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .filter(|p| !p.is_empty())
            {
                Some(path) => format!("Edit the file `{path}` in the person's directory?"),
                None => "Edit a file in the person's directory? The model did not name a path."
                    .to_owned(),
            },
            "apply_patch" => {
                // A patch may touch several paths; name the ones it names, the path after `+++`,
                // whatever the first component is. `git apply` defaults to `-p1`, so the first
                // component is dropped — keep it the same way.
                let patch = call
                    .arguments
                    .get("patch")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let names = patch_paths(patch);
                if let Some(named) = listed_paths(&names) {
                    named
                } else {
                    "Apply a patch in the person's directory? The patch names no file.".to_owned()
                }
            }
            // Commands — the shell line, so the person reads what the run would run, and the cwd
            // when one is set.
            "run_command" | "run" => {
                let tool = call.tool;
                let command = call
                    .arguments
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let cwd = call
                    .arguments
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .filter(|c| !c.is_empty());
                let in_dir = match cwd {
                    Some(cwd) => format!("in `{cwd}` of the person's directory"),
                    None => "in the person's directory".to_owned(),
                };
                // A whitespace-only command is treated as missing: the model did not name one.
                // The raw command is shown verbatim when it is not empty, so the person reads
                // exactly what the model asked for.
                if command.trim().is_empty() {
                    format!(
                        "Run a command {in_dir} with `{}`? The model did not name a command.",
                        tool
                    )
                } else {
                    format!("Run a command {in_dir} with `{tool}`: `{command}`?")
                }
            }
            // Reads — `read_file`, `read_files`, and any future read-only tool the host adds.
            _ => return Ok(Decision::Continue),
        };
        Ok(Decision::Ask(question))
    }
}

/// The paths a patch names, one per `+++ <path>` line, with `git apply`'s `-p1` already applied
/// (the first `/`-separated component dropped when there is one).
///
/// The lines are read as the model's bytes are read: a `+++ b/<path>` line is parsed the same way
/// `git apply` parses it, so the names in the question match the names git applies. A path
/// component the model wrote without a leading directory (an exact `+++ <path>`) is left alone.
/// A deletion's `+++ /dev/null` (the side git shows for a file being removed) is skipped: `-p1`
/// would turn it into `dev/null`, a path that is not the person's file.
fn patch_paths(patch: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in patch.lines() {
        let Some(rest) = line.strip_prefix("+++ ") else {
            continue;
        };
        let path = rest.trim();
        // A deletion: `+++ /dev/null`. `git apply` would strip the leading `/` and leave
        // `dev/null`, which is not a file in the person's directory; skip it before `-p1`.
        if path == "/dev/null" || path == "dev/null" {
            continue;
        }
        // `git diff` writes `+++ b/<path>`; a model may also write `+++ x/<path>`. The leading
        // `b/` (or whatever) is a label, not part of the path, and `git apply -p1` drops the first
        // component. Strip it when the rest has one, keep the rest otherwise.
        let stripped = match path.split_once('/') {
            Some((_label, rest)) => rest,
            None => path,
        };
        if stripped.is_empty() {
            continue;
        }
        if !out.iter().any(|p| p == stripped) {
            out.push(stripped.to_owned());
        }
    }
    out
}

/// The names of the paths the question names, with "and N more" past [`PATHS_IN_QUESTION`].
fn listed_paths(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let shown = names
        .iter()
        .take(PATHS_IN_QUESTION)
        .cloned()
        .collect::<Vec<_>>();
    let rest = names.len().saturating_sub(shown.len());
    let mut line = format!(
        "Apply a patch to {} file(s) in the person's directory ({})?",
        names.len(),
        shown.join(", ")
    );
    if rest > 0 {
        line.push_str(&format!(" and {rest} more"));
    }
    Some(line)
}

// The local agent's tools, over `State<PersonDir>`. The bodies are `adam-code-tools`', with the
// coder's `repo` argument dropped (`PersonDir` has one place).
//
// A tool that names `State<PersonDir>` declares the type in its generated `required_state`, so
// `EngineBuilder::local_agent` failing to register the directory is a build error, not a run error.

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
    /// What the result shows of the output: `full` (the default) or `on_error`, the exit code alone when it is 0
    output: Option<String>,
) -> Result<ToolOutput, ToolError> {
    inspect::run_command(&*person, ctx, command, cwd, None, timeout, output).await
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
    /// What the result shows of the output: `on_error` (the default: the exit code and the changed files alone when it succeeds) or `full`
    output: Option<String>,
) -> Result<ToolOutput, ToolError> {
    make::run(&*person, ctx, command, cwd, None, timeout, output).await
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Build a `HookedCall` with `tool` and `arguments` JSON for the hook to inspect.
    fn build_call<'a>(tool: &'a str, arguments: &'a mut serde_json::Value) -> HookedCall<'a> {
        HookedCall { tool, arguments }
    }

    /// Drive the hook with `tool` and `arguments` and unwrap the `Decision::Ask(question)`.
    fn ask(tool: &str, arguments: &mut serde_json::Value) -> String {
        let plugin = AsksBeforeWritingOrCommand;
        let mut call = build_call(tool, arguments);
        let verdict = plugin.before_tool(&mut call).expect("the hook returns Ok");
        match verdict {
            Decision::Ask(q) => q,
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    /// `write_file` parks with the path when one is given; still parks (with a graceful
    /// question) when the path is missing or empty.
    #[test]
    fn write_file_asks_when_path_is_present_missing_or_empty() {
        let mut args = json!({ "path": "notes.txt", "content": "hi\n" });
        let q = ask("write_file", &mut args);
        assert!(
            q.contains("`notes.txt`") && q.contains("Write"),
            "the question names the path: {q}"
        );

        let mut args = json!({ "content": "hi\n" });
        let q = ask("write_file", &mut args);
        assert!(
            q.contains("Write") && q.contains("did not name a path"),
            "a missing path still asks, gracefully: {q}"
        );

        let mut args = json!({ "path": "", "content": "hi\n" });
        let q = ask("write_file", &mut args);
        assert!(
            q.contains("Write") && q.contains("did not name a path"),
            "an empty path still asks, gracefully: {q}"
        );
    }

    /// `edit_file` parks with the path when one is given; still parks when the path is missing or
    /// empty.
    #[test]
    fn edit_file_asks_when_path_is_present_missing_or_empty() {
        let mut args = json!({ "path": "notes.txt", "old": "a", "new": "b" });
        let q = ask("edit_file", &mut args);
        assert!(
            q.contains("`notes.txt`") && q.contains("Edit"),
            "the question names the path: {q}"
        );

        let mut args = json!({ "old": "a", "new": "b" });
        let q = ask("edit_file", &mut args);
        assert!(
            q.contains("Edit") && q.contains("did not name a path"),
            "a missing path still asks: {q}"
        );

        let mut args = json!({ "path": "", "old": "a", "new": "b" });
        let q = ask("edit_file", &mut args);
        assert!(
            q.contains("Edit") && q.contains("did not name a path"),
            "an empty path still asks: {q}"
        );
    }

    /// `apply_patch` parks with the patch's paths; the first component is stripped (the way
    /// `git apply -p1` strips it). Still parks when no path parses or the patch is missing.
    #[test]
    fn apply_patch_asks_with_paths_stripped_like_git_apply_p1() {
        let patch = "--- a/notes.txt\n\
                     +++ a/notes.txt\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let mut args = json!({ "patch": patch });
        let q = ask("apply_patch", &mut args);
        assert!(
            q.contains("notes.txt"),
            "the question names the path with the first component stripped: {q}"
        );

        // An arbitrary first component (the bug the test for BLOCKING 1.1 catches).
        let patch = "--- x/notes.txt\n\
                     +++ x/notes.txt\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let mut args = json!({ "patch": patch });
        let q = ask("apply_patch", &mut args);
        assert!(
            q.contains("notes.txt") && !q.contains("x/notes.txt"),
            "the arbitrary `x/` is stripped: {q}"
        );

        // A patch without `+++ ` lines.
        let patch = "@@ -1 +1 @@\n-old\n+new\n";
        let mut args = json!({ "patch": patch });
        let q = ask("apply_patch", &mut args);
        assert!(
            q.contains("patch") && q.contains("did not name"),
            "a patch without `+++ ` still asks, gracefully: {q}"
        );

        // A patch the model did not send.
        let mut args = json!({});
        let q = ask("apply_patch", &mut args);
        assert!(
            q.contains("patch") && q.contains("did not name"),
            "a missing patch still asks: {q}"
        );
    }

    /// A deletion's `+++ /dev/null` does not become `dev/null` in the question: `-p1` would strip
    /// the leading `/`, leaving a path that is not the person's file. The path is skipped, the
    /// question still asks (other paths on the patch may be real), and it does not contain
    /// `dev/null`.
    #[test]
    fn apply_patch_skips_dev_null_so_the_question_does_not_name_it() {
        let patch = "--- a/notes.txt\n\
                     +++ /dev/null\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        let mut args = json!({ "patch": patch });
        let q = ask("apply_patch", &mut args);
        assert!(q.contains("notes.txt"), "the real path is named: {q}");
        assert!(
            !q.contains("dev/null"),
            "the deletion side is not named: {q}"
        );
    }

    /// `run_command` parks with the command and (when set) the cwd; still parks when the command
    /// is missing or empty.
    #[test]
    fn run_command_asks_with_command_and_optional_cwd() {
        let mut args = json!({ "command": "ls -la" });
        let q = ask("run_command", &mut args);
        assert!(
            q.contains("`ls -la`") && q.contains("run_command"),
            "the question names the command: {q}"
        );

        let mut args = json!({ "command": "ls -la", "cwd": "sub" });
        let q = ask("run_command", &mut args);
        assert!(
            q.contains("`sub`") && q.contains("`ls -la`"),
            "the cwd and command are both named: {q}"
        );

        let mut args = json!({ "cwd": "sub" });
        let q = ask("run_command", &mut args);
        assert!(
            q.contains("`sub`") && q.contains("did not name a command"),
            "a missing command still asks: {q}"
        );

        let mut args = json!({ "command": "" });
        let q = ask("run_command", &mut args);
        assert!(
            q.contains("did not name a command"),
            "an empty command still asks: {q}"
        );
    }

    /// A whitespace-only command is treated as missing (`trim().is_empty()`): the model did not
    /// name one. The raw command is shown verbatim when it is not empty.
    #[test]
    fn run_command_treats_a_whitespace_only_command_as_missing() {
        let mut args = json!({ "command": "   \t  " });
        let q = ask("run_command", &mut args);
        assert!(
            q.contains("did not name a command"),
            "whitespace is not a command: {q}"
        );

        // Non-empty raw command: shown verbatim.
        let mut args = json!({ "command": " echo hello " });
        let q = ask("run_command", &mut args);
        assert!(
            q.contains("` echo hello `"),
            "the raw command is shown verbatim: {q}"
        );
    }

    /// `run` parks with the command and the tool's own name; still parks when the command is
    /// missing or empty.
    #[test]
    fn run_asks_with_command_and_optional_cwd() {
        let mut args = json!({ "command": "npm install" });
        let q = ask("run", &mut args);
        assert!(
            q.contains("`npm install`") && q.contains("`run`"),
            "the question names the command and the tool: {q}"
        );

        let mut args = json!({ "command": "npm install", "cwd": "ui" });
        let q = ask("run", &mut args);
        assert!(
            q.contains("`ui`") && q.contains("`npm install`"),
            "the cwd and command are both named: {q}"
        );

        let mut args = json!({ "cwd": "ui" });
        let q = ask("run", &mut args);
        assert!(
            q.contains("`ui`") && q.contains("did not name a command"),
            "a missing command still asks: {q}"
        );
    }

    /// `run` treats a whitespace-only command as missing, like `run_command`.
    #[test]
    fn run_treats_a_whitespace_only_command_as_missing() {
        let mut args = json!({ "command": "   " });
        let q = ask("run", &mut args);
        assert!(
            q.contains("did not name a command"),
            "whitespace is not a command: {q}"
        );
    }

    /// `read_file` and `read_files` never park: a read passes without asking.
    #[test]
    fn reads_never_ask() {
        let plugin = AsksBeforeWritingOrCommand;

        let mut args = json!({ "path": "notes.txt" });
        let mut call = build_call("read_file", &mut args);
        let verdict = plugin.before_tool(&mut call).expect("the hook returns Ok");
        assert!(
            matches!(verdict, Decision::Continue),
            "read_file is Continue: got {verdict:?}"
        );

        let mut args = json!({ "paths": ["a.txt", "b.txt"] });
        let mut call = build_call("read_files", &mut args);
        let verdict = plugin.before_tool(&mut call).expect("the hook returns Ok");
        assert!(
            matches!(verdict, Decision::Continue),
            "read_files is Continue: got {verdict:?}"
        );
    }

    /// A patch with more than `PATHS_IN_QUESTION` paths names the first eight and says "and N more".
    #[test]
    fn apply_patch_truncates_the_paths_list_with_an_and_n_more() {
        let mut patch = String::new();
        for i in 0..(PATHS_IN_QUESTION + 3) {
            patch.push_str(&format!("--- a/file_{i}.txt\n+++ a/file_{i}.txt\n"));
        }
        let mut args = json!({ "patch": patch });
        let q = ask("apply_patch", &mut args);
        let expected_total = PATHS_IN_QUESTION + 3;
        assert!(
            q.contains(&format!("{expected_total} file(s)")),
            "the question names the total: {q}"
        );
        assert!(q.contains("and 3 more"), "the question names the rest: {q}");
        // The first eight are listed in order, not the rest.
        assert!(
            q.contains("file_0.txt") && q.contains("file_7.txt"),
            "the first eight are listed: {q}"
        );
        assert!(
            !q.contains("file_8.txt"),
            "the rest are summarised, not listed: {q}"
        );
    }

    /// The hook's `name` is the plugin's own identifier.
    #[test]
    fn the_hook_has_a_name() {
        assert_eq!(
            AsksBeforeWritingOrCommand.name(),
            "asks-before-writing-or-command"
        );
    }
}
