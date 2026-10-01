//! `read_file`, `write_file` and `apply_patch`: the coder edits small, well-located things itself.
//!
//! The model used to change a worktree only by delegating to OpenCode, which costs a process, a
//! model and a conversation for a one-line fix. These three tools read and change files of the
//! worktree directly, in the coder's own process, and are **confined to it**: the model chooses
//! the path, so a path is checked by [`confine`] before anything is opened, and a patch is checked
//! by the paths `git` itself says it would touch.
//!
//! # The confinement rule ([`confine`])
//!
//! * An empty path, an absolute path, any `..` component and any component equal to `.git`
//!   (compared without regard to case: a case-insensitive file system has the same `.git`) are
//!   refused. The path is relative to the root of the worktree.
//! * To **read**, the path is canonicalised (every symlink followed) and the result must still be
//!   inside the canonical root, and not inside a `.git` either. So a symlink that stays inside the
//!   worktree is read, and one that leads out is refused.
//! * To **write**, the deepest part of the path that exists is looked at one component at a
//!   time: any symlink on the way, the file itself included, is refused (a link that stays inside
//!   is refused too: a write through a link is a write to a place the model did not name), and what
//!   exists must be inside the canonical root.
//!
//! A refusal names its reason and is a tool **result**, not a failed run.
//!
//! # What stays out of these tools
//!
//! Git is not touched. The tools change files; `commit_and_push` commits what the files say, and a
//! change made here changes the worktree's tree, so the next `commit_and_push` is bound to a check
//! only after a new `run_checks` (the same rule as for OpenCode's edits).
//!
//! # Races
//!
//! The checks and the write are not one atomic step. The tools of one run are called one at a
//! time and what a command leaves running is killed with it, so nothing else changes the worktree
//! between them; the write itself goes to a new file next to its target and is renamed over it,
//! which replaces a symlink that appeared meanwhile instead of following it.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use adam::prelude::*;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{Outcome, ToolEnv, non_empty};

/// Most bytes `read_file` returns. A longer file is cut and the result says so.
pub const READ_CAP: usize = 256 * 1024;

/// Most bytes of `content` `write_file` accepts.
pub const WRITE_CAP: usize = 1024 * 1024;

/// Most bytes of `patch` `apply_patch` accepts.
pub const PATCH_CAP: usize = 1024 * 1024;

/// How long one `git apply` may run.
const APPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// Longest message of `git apply` kept in a result.
const APPLY_MESSAGE_CAP: usize = 2000;

/// Most changed files `apply_patch` lists in its result.
const MAX_LISTED: usize = 40;

/// The flag of the second reading of a patch ([`apply_in`]).
const RECOUNT: &str = "--recount";

/// What a path is used for; see [`confine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The file is read. A symlink is followed, and must lead to something inside the root.
    Read,
    /// The file is created or replaced. No symlink may be on the way.
    Write,
}

/// Whether `name` is a `.git` (any case).
fn is_dot_git(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().eq_ignore_ascii_case(".git")
}

/// The normal components of `rel`, or why the path is refused (see the module docs).
fn parts_of(rel: &str) -> Result<Vec<OsString>, String> {
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        return Err("path is required".to_owned());
    }
    if trimmed.contains('\0') {
        return Err(format!("`{}` is not a path", trimmed.escape_debug()));
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(format!(
            "`{trimmed}` is an absolute path: give a path relative to the root of the worktree"
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if is_dot_git(name) => {
                return Err(format!(
                    "`{trimmed}` is inside .git: the repository's own files are not for these tools"
                ));
            }
            Component::Normal(name) => parts.push(name.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "`{trimmed}` goes up with `..`: a path must stay inside the worktree"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "`{trimmed}` is an absolute path: give a path relative to the root of the worktree"
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(format!(
            "`{trimmed}` is the root of the worktree, not a file: name a file"
        ));
    }
    Ok(parts)
}

/// The absolute path of `rel` under `slot_root` for `access`, or the reason the model is told.
///
/// The rule is in the [module docs](self#the-confinement-rule-confine). The result starts with the
/// canonical `slot_root`; for [`Access::Read`] it is the canonical path of an existing file or
/// directory, for [`Access::Write`] the path of a file that may not exist yet (its parents may not
/// either).
///
/// # Errors
///
/// A message for the model: the path is empty, absolute, goes up, names `.git`, leaves the
/// worktree through a symlink, goes through a symlink to write, or (to read) does not exist.
pub fn confine(slot_root: &Path, rel: &str, access: Access) -> Result<PathBuf, String> {
    let root = slot_root
        .canonicalize()
        .map_err(|e| format!("cannot resolve the worktree: {e}"))?;
    let parts = parts_of(rel)?;
    let shown = rel.trim();
    match access {
        Access::Read => {
            let mut joined = root.clone();
            joined.extend(&parts);
            let canonical = joined.canonicalize().map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => format!("`{shown}` does not exist"),
                _ => format!("cannot resolve `{shown}`: {e}"),
            })?;
            let Ok(inside) = canonical.strip_prefix(&root) else {
                return Err(format!(
                    "`{shown}` leads outside the worktree (through a symlink): refused"
                ));
            };
            if inside
                .components()
                .any(|c| matches!(c, Component::Normal(name) if is_dot_git(name)))
            {
                return Err(format!(
                    "`{shown}` leads into .git (through a symlink): refused"
                ));
            }
            Ok(canonical)
        }
        Access::Write => {
            let mut current = root.clone();
            let mut existing = root.clone();
            let last = parts.len() - 1;
            for (i, name) in parts.iter().enumerate() {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(format!(
                            "`{shown}` goes through the symlink `{}`: writing through a symlink is refused",
                            parts[..=i].iter().collect::<PathBuf>().display()
                        ));
                    }
                    Ok(meta) if i < last && !meta.is_dir() => {
                        return Err(format!(
                            "`{}` is a file, not a directory: `{shown}` cannot be inside it",
                            parts[..=i].iter().collect::<PathBuf>().display()
                        ));
                    }
                    Ok(meta) if i == last && meta.is_dir() => {
                        return Err(format!("`{shown}` is a directory: name a file"));
                    }
                    Ok(_) => existing.clone_from(&current),
                    // The rest of the path is new.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                    Err(e) => return Err(format!("cannot look at `{shown}`: {e}")),
                }
            }
            // Nothing on the way is a symlink, so this holds by construction; it is checked
            // anyway, since it is what the rule is for.
            let canonical = existing
                .canonicalize()
                .map_err(|e| format!("cannot resolve `{shown}`: {e}"))?;
            if !canonical.starts_with(&root) {
                return Err(format!("`{shown}` resolves outside the worktree: refused"));
            }
            let mut path = root;
            path.extend(&parts);
            Ok(path)
        }
    }
}

/// The text of `rel` under `root`: the whole file up to [`READ_CAP`], or the lines `start..=end`
/// with their numbers.
fn read_in(root: &Path, rel: &str, start: Option<u32>, end: Option<u32>) -> Result<String, String> {
    if start == Some(0) || end == Some(0) {
        return Err("lines are numbered from 1: start_line and end_line must be at least 1".into());
    }
    if let (Some(start), Some(end)) = (start, end)
        && end < start
    {
        return Err(format!(
            "end_line ({end}) is before start_line ({start}): the range is empty"
        ));
    }
    let path = confine(root, rel, Access::Read)?;
    let shown = rel.trim();
    let meta = fs::metadata(&path).map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    if meta.is_dir() {
        return Err(format!(
            "`{shown}` is a directory: list it with run_command (`ls {shown}`)"
        ));
    }
    if !meta.is_file() {
        return Err(format!("`{shown}` is not a regular file"));
    }
    let file = fs::File::open(&path).map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    if start.is_none() && end.is_none() {
        read_whole(file, meta.len(), shown)
    } else {
        read_range(
            io::BufReader::new(file),
            meta.len(),
            shown,
            start.unwrap_or(1),
            end,
        )
    }
}

fn binary_notice(shown: &str, len: u64) -> String {
    format!("`{shown}` is a binary file, {len} bytes, not shown")
}

/// A file as it is, cut at [`READ_CAP`] bytes (on a character boundary).
fn read_whole(file: fs::File, len: u64, shown: &str) -> Result<String, String> {
    let mut bytes = Vec::new();
    file.take(READ_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    if bytes.contains(&0) {
        return Err(binary_notice(shown, len));
    }
    let cut = bytes.len() > READ_CAP;
    let mut slice = &bytes[..bytes.len().min(READ_CAP)];
    if cut
        && let Err(e) = std::str::from_utf8(slice)
        && e.error_len().is_none()
    {
        // The cap fell inside a character: drop its start.
        slice = &slice[..e.valid_up_to()];
    }
    let mut text = String::from_utf8_lossy(slice).into_owned();
    if text.is_empty() && !cut {
        return Ok(format!("`{shown}` is empty"));
    }
    if cut {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "[cut: `{shown}` is {len} bytes; the first {} are shown. Read the rest with \
             start_line and end_line]\n",
            slice.len()
        ));
    }
    Ok(text)
}

/// The lines `start..=end` (`end` absent: to the end of the file), each behind its number, at
/// most [`READ_CAP`] bytes of them.
fn read_range(
    mut reader: impl BufRead,
    len: u64,
    shown: &str,
    start: u32,
    end: Option<u32>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut line = Vec::new();
    let mut number: u64 = 0;
    let mut shown_any = false;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("cannot read `{shown}`: {e}"))?;
        if read == 0 {
            break;
        }
        if line.contains(&0) {
            return Err(binary_notice(shown, len));
        }
        number += 1;
        if number < u64::from(start) {
            continue;
        }
        if end.is_some_and(|end| number > u64::from(end)) {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if out.len() + text.len() > READ_CAP {
            out.push_str(&format!(
                "[cut: the output is limited to {READ_CAP} bytes; the next line is {number}]\n"
            ));
            return Ok(out);
        }
        out.push_str(&format!("{number:>6}\t{text}\n"));
        shown_any = true;
    }
    if !shown_any {
        return Err(if number == 0 {
            format!("`{shown}` is empty: it has no lines to show")
        } else {
            format!("`{shown}` has {number} lines: start_line {start} is past the end")
        });
    }
    Ok(out)
}

/// What a write did.
#[derive(Debug, PartialEq, Eq)]
struct Written {
    created: bool,
    bytes: usize,
}

/// Write `content` to `rel` under `root`: parents are created, the file is written next to its
/// target and renamed over it (an interrupted write leaves the old file or the new one, never half
/// of one), and the mode of a file that is replaced is kept (an executable script stays one).
fn write_in(root: &Path, rel: &str, content: &str) -> Result<Written, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    if content.len() > WRITE_CAP {
        return Err(format!(
            "content is {} bytes, over the limit of {WRITE_CAP}: write the file in parts with \
             apply_patch, or have OpenCode do it",
            content.len()
        ));
    }
    let shown = rel.trim();
    let path = confine(root, rel, Access::Write)?;
    let existing = fs::symlink_metadata(&path).ok();
    let parent = path
        .parent()
        .ok_or_else(|| format!("`{shown}` has no directory to be written in"))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create the directories of `{shown}`: {e}"))?;
    let tmp = parent.join(format!(
        ".adam-write-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> io::Result<()> {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        if let Some(meta) = &existing {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, &path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot write `{shown}`: {e}"));
    }
    Ok(Written {
        created: existing.is_none(),
        bytes: content.len(),
    })
}

/// Why a patch was not applied.
#[derive(Debug)]
enum PatchError {
    /// The model's problem: what to tell it.
    Refused(String),
    /// Git cannot read the patch, or it does not apply (what to tell the model if no reading of
    /// it works).
    Unreadable(String),
    /// `git` could not be run.
    Git(io::Error),
}

impl From<io::Error> for PatchError {
    fn from(e: io::Error) -> Self {
        Self::Git(e)
    }
}

/// What a `git apply` printed and whether it succeeded.
struct Applied {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}

/// `git apply <flags> -` in `root`, with `patch` on its standard input.
///
/// Hooks and the file system monitor are off, and so are the user's and the system's git
/// configuration and every variable that would point git elsewhere: the patch comes from the model.
async fn git_apply(root: &Path, flags: &[&str], patch: &str) -> io::Result<Applied> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        "apply",
    ])
    .args(flags)
    .arg("-")
    .current_dir(root)
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_CONFIG_NOSYSTEM", "1")
    .env("GIT_CONFIG_GLOBAL", "/dev/null")
    .env("LC_ALL", "C")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        cmd.env_remove(name);
    }
    let mut child = cmd.spawn()?;
    // Written from a task of its own: git may answer before it has read everything.
    let stdin = child.stdin.take();
    let bytes = patch.as_bytes().to_vec();
    let feeder = tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            // A patch git stops reading early (a parse error) breaks the pipe: its verdict is
            // what counts.
            let _ = stdin.write_all(&bytes).await;
        }
    });
    let output = match tokio::time::timeout(APPLY_TIMEOUT, child.wait_with_output()).await {
        Ok(output) => output?,
        Err(_) => {
            feeder.abort();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git apply took more than {}s", APPLY_TIMEOUT.as_secs()),
            ));
        }
    };
    let _ = feeder.await;
    Ok(Applied {
        success: output.status.success(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// `text` cut to [`APPLY_MESSAGE_CAP`] bytes, on a character boundary.
fn clipped(text: &str) -> String {
    let text = text.trim();
    if text.len() <= APPLY_MESSAGE_CAP {
        return text.to_owned();
    }
    let mut end = APPLY_MESSAGE_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// The paths `git apply --numstat -z` reports, which are the ones it would touch (a rename names
/// both ends), each once, in order. `None` for a patch with a binary part.
fn numstat_paths(stdout: &[u8]) -> Option<Vec<String>> {
    let text = String::from_utf8_lossy(stdout);
    let mut tokens = text.split('\0').filter(|t| !t.is_empty());
    let mut paths: Vec<String> = Vec::new();
    let mut add = |path: &str| {
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_owned());
        }
    };
    while let Some(entry) = tokens.next() {
        let mut fields = entry.splitn(3, '\t');
        let (added, deleted, path) = (fields.next()?, fields.next()?, fields.next()?);
        if added == "-" || deleted == "-" {
            return None;
        }
        if path.is_empty() {
            // A rename or a copy: the old and the new path follow, each ended by a NUL.
            add(tokens.next()?);
            add(tokens.next()?);
        } else {
            add(path);
        }
    }
    Some(paths)
}

/// Whether `git apply --summary` says a file becomes a symlink, a submodule, or stops being one
/// (a mode of 120000 or 160000).
fn summary_has_link(summary: &str) -> bool {
    summary.lines().any(|line| {
        let line = line.trim();
        let about_a_mode = line.starts_with("create mode ")
            || line.starts_with("delete mode ")
            || line.starts_with("mode change ");
        about_a_mode && ["120000", "160000"].iter().any(|mode| line.contains(mode))
    })
}

/// What came of reading and checking a patch.
struct Plan {
    /// The files it changes.
    paths: Vec<String>,
    /// Whether it must be applied with `--recount`.
    recount: bool,
}

/// Read `patch` the way `git apply` does and check everything about it that does not need the
/// files to change: the paths it would touch, the modes it sets, and that it applies.
///
/// `PatchError::Unreadable` is git saying it cannot read or apply the patch (the one thing
/// `--recount` may cure); `PatchError::Refused` is a rule of ours, which no flag changes.
async fn plan(root: &Path, patch: &str, recount: bool) -> Result<Plan, PatchError> {
    let with = |flags: &[&'static str]| -> Vec<&'static str> {
        let mut all = flags.to_vec();
        if recount {
            all.push(RECOUNT);
        }
        all
    };
    let listed = git_apply(root, &with(&["--numstat", "-z"]), patch).await?;
    if !listed.success {
        return Err(PatchError::Unreadable(format!(
            "the patch is not a unified diff git can read: {}. Use `--- a/<path>`, `+++ b/<path>` \
             and `@@ -<line>,<count> +<line>,<count> @@` hunks",
            clipped(&listed.stderr)
        )));
    }
    let Some(paths) = numstat_paths(&listed.stdout) else {
        return Err(PatchError::Refused(
            "the patch changes a binary file: binary patches are not accepted".to_owned(),
        ));
    };
    if paths.is_empty() {
        return Err(PatchError::Refused("the patch changes no file".to_owned()));
    }
    for path in &paths {
        confine(root, path, Access::Write).map_err(PatchError::Refused)?;
    }
    let summary = git_apply(root, &with(&["--summary"]), patch).await?;
    if summary.success && summary_has_link(&String::from_utf8_lossy(&summary.stdout)) {
        return Err(PatchError::Refused(
            "the patch creates or changes a symlink or a submodule: refused".to_owned(),
        ));
    }
    let check = git_apply(root, &with(&["--check", "--whitespace=nowarn"]), patch).await?;
    if !check.success {
        return Err(PatchError::Unreadable(format!(
            "the patch does not apply, and nothing was changed: {}. Read the file again with \
             read_file and make the hunks match it exactly (context lines included)",
            clipped(&check.stderr)
        )));
    }
    Ok(Plan { paths, recount })
}

/// Check `patch` and apply it in `root`; the files it changed.
///
/// Every path git reads from the patch (`--numstat -z`: what it would touch, renames and copies
/// included, however the patch spells them) is checked with [`confine`], a mode of a symlink or a
/// submodule is refused, then `git apply --check` and `git apply` run, never with
/// `--unsafe-paths`. `git apply` is all or nothing.
///
/// The line counts of a hunk header are written by a model, which counts badly. A patch git
/// cannot read or apply as it is gets a second reading with `--recount` (git then goes by the
/// lines themselves). It is a second try and not the rule, because `--recount` cannot tell where
/// a hunk ends when the next file's `--- ` line follows it without a `diff --git` line: it takes
/// that line for a removed one.
async fn apply_in(root: &Path, patch: &str) -> Result<Vec<String>, PatchError> {
    if patch.len() > PATCH_CAP {
        return Err(PatchError::Refused(format!(
            "the patch is {} bytes, over the limit of {PATCH_CAP}: split it, or have OpenCode make \
             the change",
            patch.len()
        )));
    }
    if patch.contains('\0') {
        return Err(PatchError::Refused(
            "the patch contains a NUL byte: binary patches are not accepted".to_owned(),
        ));
    }
    let Plan { paths, recount } = match plan(root, patch, false).await {
        Ok(plan) => plan,
        Err(PatchError::Unreadable(first)) => match plan(root, patch, true).await {
            Ok(plan) => plan,
            Err(PatchError::Git(e)) => return Err(PatchError::Git(e)),
            // Neither reading works: the first one, as git said it, is the one to act on.
            Err(_) => return Err(PatchError::Refused(first)),
        },
        Err(e) => return Err(e),
    };
    let mut flags = vec!["--whitespace=nowarn"];
    if recount {
        flags.push(RECOUNT);
    }
    let applied = git_apply(root, &flags, patch).await?;
    if !applied.success {
        return Err(PatchError::Refused(format!(
            "the patch could not be applied: {}",
            clipped(&applied.stderr)
        )));
    }
    Ok(paths)
}

/// The files of a patch as one line: the first few, then how many more.
fn listed(paths: &[String]) -> String {
    let mut line = paths
        .iter()
        .take(MAX_LISTED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > MAX_LISTED {
        line.push_str(&format!(" and {} more", paths.len() - MAX_LISTED));
    }
    line
}

/// Read a text file of your worktree. Give `path` relative to the root of the worktree. Without
/// a range you get the whole file (cut at 256 KiB, and the cut is marked); with `start_line` and
/// `end_line` you get just those lines, each behind its number. A binary file is not shown. Use it
/// to read before you change something, and run_command (`ls`, `grep -rn`) to find where to look.
#[tool]
pub async fn read_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the root of the worktree (no `..`, nothing inside `.git`)
    path: String,
    /// First line to show, counting from 1. Leave out to read from the top.
    start_line: Option<u32>,
    /// Last line to show. Leave out to read to the end of the file (or of what fits).
    end_line: Option<u32>,
    /// The slot to read in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("read {path} ({})", slot.dir()))
        .await;
    let (root, rel) = (slot.path().to_path_buf(), path.to_owned());
    let read = tokio::task::spawn_blocking(move || read_in(&root, &rel, start_line, end_line))
        .await
        .map_err(|e| ToolError::Transient(format!("the read was interrupted: {e}")))?;
    Ok(match read {
        Ok(text) => ToolOutput::text(text),
        Err(reason) => ToolOutput::error(reason),
    })
}

/// Create a file, or replace one, with exactly `content`. Parent directories are created. The
/// path is relative to the root of the worktree; nothing inside `.git` and nothing through a
/// symlink can be written. For a change to part of a file use apply_patch (or read it first with
/// read_file and write it whole); for a broad, multi-file change use delegate_to_opencode.
#[tool]
pub async fn write_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the root of the worktree
    path: String,
    /// The whole new content of the file (at most 1 MiB)
    content: String,
    /// The slot to write in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let (root, rel) = (slot.path().to_path_buf(), path.to_owned());
    let written = tokio::task::spawn_blocking(move || write_in(&root, &rel, &content))
        .await
        .map_err(|e| ToolError::Transient(format!("the write was interrupted: {e}")))?;
    match written {
        Ok(done) => {
            ctx.emit_progress(format!("wrote {path} ({})", slot.dir()))
                .await;
            Ok(ToolOutput::text(format!(
                "{} {path} ({} bytes). Run the checks again before you commit.",
                if done.created { "Created" } else { "Replaced" },
                done.bytes
            )))
        }
        Err(reason) => Ok(ToolOutput::error(reason)),
    }
}

/// Apply a unified diff to the worktree: one or several files, `--- a/<path>` and `+++ b/<path>`
/// then hunks (`@@ -12,3 +12,4 @@`, with context lines), as `git diff` writes them. Every path is
/// checked first (inside the worktree, not `.git`, not a symlink); a patch that creates a symlink
/// is refused; and it is all or nothing: if a hunk does not match the file, nothing is changed
/// and you are told why. Read the file first, so the context lines match it exactly. Use it for
/// small, well-located changes; delegate broad, multi-file changes to OpenCode.
#[tool]
pub async fn apply_patch(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// The unified diff, at most 1 MiB, with `a/` and `b/` before the paths
    patch: String,
    /// The slot to patch: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    if non_empty(&patch).is_none() {
        return Ok(ToolOutput::error("patch is required"));
    }
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    match apply_in(slot.path(), &patch).await {
        Ok(paths) => {
            ctx.emit_progress(format!("patched {} ({})", listed(&paths), slot.dir()))
                .await;
            Ok(ToolOutput::text(format!(
                "Applied the patch to {} file(s): {}. Run the checks again before you commit.",
                paths.len(),
                listed(&paths)
            )))
        }
        Err(PatchError::Refused(reason) | PatchError::Unreadable(reason)) => {
            Ok(ToolOutput::error(reason))
        }
        Err(PatchError::Git(e)) => Err(ToolError::Transient(format!("cannot run git apply: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        // What a worktree has: a `.git` file.
        fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        dir
    }

    fn refused(root: &Path, rel: &str, access: Access) -> String {
        confine(root, rel, access).expect_err(rel)
    }

    #[test]
    fn a_path_inside_the_worktree_is_accepted_for_both() {
        let dir = root();
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(
            confine(dir.path(), "src/main.rs", Access::Read).unwrap(),
            canonical.join("src/main.rs")
        );
        assert_eq!(
            confine(dir.path(), "./src//main.rs", Access::Read).unwrap(),
            canonical.join("src/main.rs")
        );
        // A file that is not there yet, in a directory that is not there yet.
        assert_eq!(
            confine(dir.path(), "new/deep/file.txt", Access::Write).unwrap(),
            canonical.join("new/deep/file.txt")
        );
        assert_eq!(
            confine(dir.path(), "src/main.rs", Access::Write).unwrap(),
            canonical.join("src/main.rs")
        );
    }

    #[test]
    fn an_empty_path_is_refused() {
        let dir = root();
        for rel in ["", "   ", ".", "./", "./."] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(
                    why.contains("path is required") || why.contains("root of the worktree"),
                    "{rel:?}: {why}"
                );
            }
        }
    }

    #[test]
    fn a_path_that_goes_up_is_refused() {
        let dir = root();
        for rel in ["../x", "src/../../x", "..", "src/.."] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains("`..`"), "{rel}: {why}");
            }
        }
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let dir = root();
        let inside = dir.path().join("README.md");
        for rel in ["/etc/passwd", inside.to_str().unwrap()] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains("absolute"), "{rel}: {why}");
            }
        }
    }

    #[test]
    fn nothing_inside_dot_git_is_touched_whatever_the_case() {
        let dir = root();
        for rel in [
            ".git",
            ".git/config",
            ".GIT/x",
            ".Git/hooks/pre-commit",
            "a/.git/x",
            "src/.GiT",
        ] {
            for access in [Access::Read, Access::Write] {
                let why = refused(dir.path(), rel, access);
                assert!(why.contains(".git"), "{rel}: {why}");
            }
        }
        // `.github` is not `.git`.
        fs::create_dir_all(dir.path().join(".github")).unwrap();
        assert!(confine(dir.path(), ".github/workflows/ci.yml", Access::Write).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_file_outside_is_not_read() {
        let dir = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
        let why = refused(dir.path(), "link", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_outside_is_not_read_or_written() {
        let dir = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dir")).unwrap();
        let why = refused(dir.path(), "dir/secret", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
        let why = refused(dir.path(), "dir/new.txt", Access::Write);
        assert!(why.contains("symlink"), "{why}");
        assert!(!outside.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_is_refused_even_when_it_stays_inside() {
        let dir = root();
        std::os::unix::fs::symlink("README.md", dir.path().join("alias")).unwrap();
        std::os::unix::fs::symlink("src", dir.path().join("srclink")).unwrap();
        // Reading follows a link that stays inside.
        assert!(confine(dir.path(), "alias", Access::Read).is_ok());
        assert!(confine(dir.path(), "srclink/main.rs", Access::Read).is_ok());
        // Writing does not.
        let why = refused(dir.path(), "alias", Access::Write);
        assert!(why.contains("symlink `alias`"), "{why}");
        let why = refused(dir.path(), "srclink/main.rs", Access::Write);
        assert!(why.contains("symlink `srclink`"), "{why}");
        // A dangling link is a link too.
        std::os::unix::fs::symlink("nowhere", dir.path().join("dangling")).unwrap();
        assert!(refused(dir.path(), "dangling", Access::Write).contains("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_dot_git_is_not_read_either() {
        let dir = root();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(dir.path().join(".git"), dir.path().join("sub/g")).unwrap();
        let why = refused(dir.path(), "sub/g", Access::Read);
        assert!(why.contains(".git"), "{why}");
    }

    #[test]
    fn a_directory_is_not_a_file_to_write_and_a_file_is_not_a_directory() {
        let dir = root();
        assert!(refused(dir.path(), "src", Access::Write).contains("is a directory"));
        assert!(refused(dir.path(), "README.md/x", Access::Write).contains("is a file"));
    }

    #[test]
    fn reading_says_what_is_missing() {
        let dir = root();
        assert!(refused(dir.path(), "nope.txt", Access::Read).contains("does not exist"));
    }

    #[test]
    fn a_whole_file_is_returned_as_it_is() {
        let dir = root();
        assert_eq!(
            read_in(dir.path(), "src/main.rs", None, None).unwrap(),
            "fn main() {}\n"
        );
        fs::write(dir.path().join("empty"), "").unwrap();
        assert_eq!(
            read_in(dir.path(), "empty", None, None).unwrap(),
            "`empty` is empty"
        );
    }

    #[test]
    fn a_range_is_numbered_and_the_ends_are_reported() {
        let dir = root();
        fs::write(dir.path().join("f.txt"), "one\ntwo\r\nthree\nfour").unwrap();
        let text = read_in(dir.path(), "f.txt", Some(2), Some(3)).unwrap();
        assert_eq!(text, "     2\ttwo\n     3\tthree\n");
        // From a line to the end, and a range that runs past it.
        let text = read_in(dir.path(), "f.txt", Some(3), None).unwrap();
        assert_eq!(text, "     3\tthree\n     4\tfour\n");
        let text = read_in(dir.path(), "f.txt", None, Some(1)).unwrap();
        assert_eq!(text, "     1\tone\n");
        let text = read_in(dir.path(), "f.txt", Some(4), Some(99)).unwrap();
        assert_eq!(text, "     4\tfour\n");
        // Past the end, back to front, zero.
        assert!(
            read_in(dir.path(), "f.txt", Some(9), None)
                .unwrap_err()
                .contains("has 4 lines")
        );
        assert!(
            read_in(dir.path(), "f.txt", Some(3), Some(2))
                .unwrap_err()
                .contains("before start_line")
        );
        assert!(
            read_in(dir.path(), "f.txt", Some(0), None)
                .unwrap_err()
                .contains("from 1")
        );
    }

    #[test]
    fn a_big_file_is_cut_and_the_cut_is_marked() {
        let dir = root();
        let line = format!("{}\n", "x".repeat(99));
        let big = line.repeat(READ_CAP / 100 + 500);
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        let text = read_in(dir.path(), "big.txt", None, None).unwrap();
        assert!(text.len() < READ_CAP + 300, "{}", text.len());
        assert!(
            text.contains("[cut: `big.txt` is"),
            "{}",
            &text[text.len() - 200..]
        );
        assert!(text.starts_with(&line));
        // A cut inside a multi-byte character does not leave half of it.
        fs::write(
            dir.path().join("wide.txt"),
            format!("a{}", "é".repeat(READ_CAP)),
        )
        .unwrap();
        let text = read_in(dir.path(), "wide.txt", None, None).unwrap();
        assert!(!text.contains('\u{fffd}'), "no replacement character");
        // A range is limited too.
        let text = read_in(dir.path(), "big.txt", Some(1), None).unwrap();
        assert!(
            text.contains("[cut: the output is limited to"),
            "{}",
            &text[text.len() - 120..]
        );
        let tail = read_in(dir.path(), "big.txt", Some(10), Some(12)).unwrap();
        assert_eq!(tail.lines().count(), 3);
    }

    #[test]
    fn a_binary_file_is_not_shown() {
        let dir = root();
        fs::write(dir.path().join("a.bin"), b"\x7fELF\0\0\x01 more").unwrap();
        let why = read_in(dir.path(), "a.bin", None, None).unwrap_err();
        assert_eq!(why, "`a.bin` is a binary file, 12 bytes, not shown");
        let why = read_in(dir.path(), "a.bin", Some(1), Some(5)).unwrap_err();
        assert!(why.contains("binary file, 12 bytes"), "{why}");
    }

    #[test]
    fn a_directory_is_listed_with_run_command_not_read() {
        let dir = root();
        assert!(
            read_in(dir.path(), "src", None, None)
                .unwrap_err()
                .contains("run_command")
        );
    }

    #[test]
    fn a_write_creates_parents_replaces_and_leaves_nothing_behind() {
        let dir = root();
        let done = write_in(dir.path(), "a/b/c.txt", "hello\n").unwrap();
        assert_eq!(
            done,
            Written {
                created: true,
                bytes: 6
            }
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "hello\n"
        );
        let done = write_in(dir.path(), "a/b/c.txt", "bye\n").unwrap();
        assert_eq!(
            done,
            Written {
                created: false,
                bytes: 4
            }
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "bye\n"
        );
        let names: Vec<_> = fs::read_dir(dir.path().join("a/b"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["c.txt"], "the temporary file is gone");
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = root();
        let script = dir.path().join("run.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        write_in(dir.path(), "run.sh", "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn too_much_content_is_refused_before_anything_is_written() {
        let dir = root();
        let why = write_in(dir.path(), "big.txt", &"x".repeat(WRITE_CAP + 1)).unwrap_err();
        assert!(why.contains("over the limit"), "{why}");
        assert!(!dir.path().join("big.txt").exists());
        assert!(write_in(dir.path(), "ok.txt", &"x".repeat(WRITE_CAP)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_write_through_a_symlink_changes_nothing() {
        let dir = root();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("target"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.path().join("target"), dir.path().join("link")).unwrap();
        assert!(
            write_in(dir.path(), "link", "overwritten")
                .unwrap_err()
                .contains("symlink")
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("target")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn numstat_gives_the_paths_git_would_touch() {
        assert_eq!(
            numstat_paths(b"1\t1\tsrc/a.rs\x000\t3\tb.txt\0").unwrap(),
            ["src/a.rs", "b.txt"]
        );
        // A rename: empty path, then the old and the new.
        assert_eq!(
            numstat_paths(b"0\t0\t\0old.txt\0new.txt\0").unwrap(),
            ["old.txt", "new.txt"]
        );
        // The same file twice is one.
        assert_eq!(numstat_paths(b"1\t1\ta\x001\t1\ta\0").unwrap(), ["a"]);
        // A binary part.
        assert_eq!(numstat_paths(b"-\t-\timg.png\0"), None);
        assert_eq!(numstat_paths(b"").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_link_in_the_summary_is_found() {
        assert!(summary_has_link(" create mode 120000 link\n"));
        assert!(summary_has_link(" mode change 100644 => 120000 x\n"));
        assert!(summary_has_link(" create mode 160000 sub\n"));
        assert!(!summary_has_link(
            " create mode 100644 a.txt\n delete mode 100755 b.sh\n"
        ));
        assert!(!summary_has_link(" rename a.txt => 120000 (100%)\n"));
    }

    #[test]
    fn the_files_of_a_patch_are_listed_with_a_limit() {
        let few: Vec<String> = (0..3).map(|i| format!("f{i}")).collect();
        assert_eq!(listed(&few), "f0, f1, f2");
        let many: Vec<String> = (0..MAX_LISTED + 5).map(|i| format!("f{i}")).collect();
        assert!(listed(&many).ends_with("and 5 more"));
    }
}
