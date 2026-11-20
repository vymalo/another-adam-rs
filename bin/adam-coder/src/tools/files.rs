//! `read_file`, `read_files`, `write_file`, `edit_file` and `apply_patch`: the coder edits small, well-located things itself.
//!
//! The coder declares the read tools, `write_file` and `edit_file` here and their logic is
//! `adam_code_tools::files` (ADR 0047), over the run's slots (`ToolEnv` as a `CodeWorkspace`): what
//! a page shows, the images, the closest paths, the write and the edit are there, and what follows a
//! write is the coder's `CodeWorkspace::after_write`. `apply_patch` is still the coder's.
//!
//! These tools read and change files of the
//! worktree directly, in the coder's own process, and are **confined to it**: the model chooses
//! the path, so a path is checked by [`confine`] before anything is opened, and a patch is checked
//! by the paths `git` itself says it would touch.
//!
//! # The confinement rule ([`confine`])
//!
//! * An empty path, an absolute path, any `..` component and any component equal to `.git`
//!   (compared without regard to case: a case-insensitive file system has the same `.git`) are
//!   refused. The path is relative to the root of the worktree.
//! * To **read**, the path is resolved inside the worktree, following every symlink on the way
//!   that stays inside it, and the result must not be inside a `.git` either. So a symlink that
//!   stays inside the worktree is read, and one that leads out (an absolute one, or one that goes
//!   up past the root) is refused.
//! * To **write**, every component of the path that exists is looked at: any symlink on the way, the
//!   file itself included, is refused (a link that stays inside is refused too: a write through a
//!   link is a write to a place the model did not name).
//!
//! # Relative to the slot
//!
//! The **root** is the slot's directory *as the coder opened it*
//! ([`SlotDir`](adam_workspace::safe_fs::SlotDir), held by the slot): the directory is the run's to
//! rename, and a link put at its name (to another run's slot) would be followed by anything that
//! uses the name. Nothing here uses the name. A path is resolved and
//! opened relative to the descriptor, a file is written to a name nobody chose in the directory's
//! own descriptor and renamed over its target, and a directory is made with `mkdirat`. A swap of the
//! slot, or of a directory below it, at any moment is not followed: the tools go on in the directory
//! they opened, or fail.
//!
//! A refusal names its reason and is a tool **result**, not a failed run.
//!
//! # What stays out of these tools
//!
//! Git is not touched. The tools change files; `commit_and_push` commits what the files say, and a
//! change made here changes the worktree's tree, so the next `commit_and_push` is bound to a check
//! only after a new `run_checks` (the same rule as for every change).
//!
//! # Races
//!
//! A command in the run's pod, and a process it left running (which outlives the command in a
//! pod), can rename the slot's directory, or a directory below it, and put a link at the old name
//! while a tool works: a path may be checked and then used. So the tools do not use a name for what
//! they check. The checks and the use are relative to the same descriptors (see above): the slot's,
//! and the directory a file is written in; a link that appears on a path after it was resolved
//! is refused when the file is opened (`RESOLVE_NO_SYMLINKS`, or `O_NOFOLLOW` one component at a
//! time where `openat2` is missing), and the write is a new file renamed over its target, which
//! replaces a symlink that appeared meanwhile instead of following it.
//!
//! What these tools cannot do alone is `git`: `git apply` writes the paths of a patch by name (they
//! are checked here first), and `git` finds its work tree by name inside its own process. A name is
//! safe only if the run cannot rename it, so **in a run pod the slot's directory is a mount point of
//! its own** (ADR 0041, decision 3): the pod's commands, and the processes they left running, cannot
//! rename, move, remove or exchange it, and the coder's `git` can rely on its name. The coder tells
//! the environment of each slot it adds (`ToolEnv::seal`), because these tools never ask for the pod
//! and a pod made before the slot lacks the mount.
//!
//! What is **not** covered: while a command runs in a pod that lacks the new slot's mount (the seal
//! waits for it to end, and the result of the tool that added the slot says so), the directory can be
//! renamed; the creation of a slot; the files below the slot, which these tools open relative to the
//! descriptor they hold; `git`'s own handling of what is below the slot; and everything a command
//! itself does. Local mode and Podman have no mounts: the slot's directory is a name there.

#[cfg(test)]
use std::fs;
use std::io;
#[cfg(test)]
use std::path::Path;
use std::time::Duration;

use adam::prelude::*;
use adam_code_tools::CodeWorkspace as _;
#[cfg(test)]
use adam_workspace::safe_fs::SlotDir;

use super::gitcli::GitAt;
use super::workspace::place_of;
use super::{Outcome, ToolEnv, non_empty};

pub use adam_code_tools::files::WRITE_CAP;

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

pub use adam_code_tools::confine::{Access, Confined, confine};
pub(super) use adam_code_tools::confine::{is_dot_git, open_for_read};
#[cfg(test)]
use adam_code_tools::files::{Edited, Written, edit_in, read_in, write_in};

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

/// `git apply <flags> -` where `at` says, with `patch` on its standard input.
///
/// Hooks and the file system monitor are off, and so are the user's and the system's git
/// configuration and every variable that would point git elsewhere: the patch comes from the model.
/// The repository is the slot's, pinned (`GitAt`), never found through files a command wrote.
async fn git_apply(at: &GitAt, flags: &[&str], patch: &str) -> io::Result<Applied> {
    // From an empty environment: the repository's own configuration can run a program. In the git
    // sandbox when the deployment has one: the patch is the model's, and applying it writes files
    // the repository's attributes may run a filter on.
    let Some(cmd) = at.command().await else {
        return Err(io::Error::other(
            "git is not run here: the worktree's repository cannot be trusted",
        ));
    };
    // The patch is the command's input. A patch git stops reading early (a parse error) breaks the
    // pipe: its verdict is what counts.
    let cmd = cmd
        .arg("apply")
        .args(flags)
        .arg("-")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(patch.as_bytes().to_vec());
    let output = match tokio::time::timeout(APPLY_TIMEOUT, cmd.output()).await {
        Ok(output) => output?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("git apply took more than {}s", APPLY_TIMEOUT.as_secs()),
            ));
        }
    };
    Ok(Applied {
        success: output.success,
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
async fn plan(at: &GitAt, patch: &str, recount: bool) -> Result<Plan, PatchError> {
    let with = |flags: &[&'static str]| -> Vec<&'static str> {
        let mut all = flags.to_vec();
        if recount {
            all.push(RECOUNT);
        }
        all
    };
    let listed = git_apply(at, &with(&["--numstat", "-z"]), patch).await?;
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
    // Only a check: `git apply` writes these paths by name, which the coder cannot protect (see the
    // module docs, *Races*).
    for path in &paths {
        confine(at.root(), path, Access::Write).map_err(PatchError::Refused)?;
    }
    let summary = git_apply(at, &with(&["--summary"]), patch).await?;
    if summary.success && summary_has_link(&String::from_utf8_lossy(&summary.stdout)) {
        return Err(PatchError::Refused(
            "the patch creates or changes a symlink or a submodule: refused".to_owned(),
        ));
    }
    let check = git_apply(at, &with(&["--check", "--whitespace=nowarn"]), patch).await?;
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
async fn apply_in(at: &GitAt, patch: &str) -> Result<Vec<String>, PatchError> {
    if patch.len() > PATCH_CAP {
        return Err(PatchError::Refused(format!(
            "the patch is {} bytes, over the limit of {PATCH_CAP}: split it into several patches",
            patch.len()
        )));
    }
    if patch.contains('\0') {
        return Err(PatchError::Refused(
            "the patch contains a NUL byte: binary patches are not accepted".to_owned(),
        ));
    }
    let Plan { paths, recount } = match plan(at, patch, false).await {
        Ok(plan) => plan,
        Err(PatchError::Unreadable(first)) => match plan(at, patch, true).await {
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
    let applied = git_apply(at, &flags, patch).await?;
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

/// Read a text file of your worktree, one page at a time (at most 2000 lines). Each line starts
/// with its number. If the last line says `[more: ... offset=N]`, call again with offset N. A
/// negative offset reads from the end. A binary file is not shown, except an image (PNG, JPEG, GIF
/// or WebP) when you can see images: it comes after the result. Example:
/// {"path":"src/main.rs","offset":2001}
#[tool(
    label = "Read a file",
    icon = "read",
    paged,
    parallel = "read",
    path = "path"
)]
#[allow(clippy::too_many_arguments)] // the model's arguments, one each
pub async fn read_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path relative to the worktree root
    path: String,
    /// First line, from 1; -50 = the last 50 lines
    offset: Option<i64>,
    /// Most lines (at most 2000)
    limit: Option<i64>,
    /// Old name of offset
    start_line: Option<u32>,
    /// Last line; limit = end_line - start_line + 1
    end_line: Option<u32>,
    /// The slot to read in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::read_file(&*env, ctx, path, offset, limit, start_line, end_line, repo)
        .await
}

pub use adam_code_tools::files::{
    READ_FILES_LINES, READ_FILES_MAX, READ_FILES_PAGE_BYTES, READ_FILES_TOTAL_BYTES,
};

/// Read several text files of your worktree in one call: 1 to 16 paths, relative to the root, in
/// the order you give them. Each file comes behind a line that names it, then its numbered lines
/// (the numbers of `read_file`): its first page, at most 400 lines or 8 KiB. A page that was cut
/// ends with `[more: N lines left; next page: read_file path=<path> offset=K]`: read on with
/// read_file. The whole result is at most 32 KiB: a file that did not fit is one line, `[not
/// shown: read it alone with read_file path=<path>]`. A file that is not there, is binary or is
/// not UTF-8 is one line that says why, and the other files are still shown. Read the manifest,
/// the CI file and the other small files of a repository together with it. Example:
/// {"paths":["Cargo.toml","justfile"]}
#[tool(label = "Read files", icon = "read", parallel = "read")]
pub async fn read_files(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Paths relative to the worktree root, in the order to show them (1 to 16)
    paths: Vec<String>,
    /// The slot to read in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::read_files(&*env, ctx, paths, repo).await
}

/// Create a file, or replace one, with exactly `content`. Parent directories are created. The
/// path is relative to the root of the worktree; nothing inside `.git` and nothing through a
/// symlink can be written. For a change to part of a file use edit_file (exact text replaced) or
/// apply_patch (a diff); for a broad, multi-file change write the files one by one.
#[tool(
    label = "Write a file",
    icon = "edit",
    parallel = "write",
    path = "path"
)]
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
    adam_code_tools::files::write_file(&*env, ctx, path, content, repo).await
}

/// Apply a unified diff to the worktree: one or several files, `--- a/<path>` and `+++ b/<path>`
/// then hunks (`@@ -12,3 +12,4 @@`, with context lines), as `git diff` writes them. Every path is
/// checked first (inside the worktree, not `.git`, not a symlink); a patch that creates a symlink
/// is refused; and it is all or nothing: if a hunk does not match the file, nothing is changed
/// and you are told why. Read the file first, so the context lines match it exactly. Use it for
/// several changes at once, in one file or several; for one exact replacement edit_file is simpler.
/// A broad, multi-file change is several of these, or several files written one by one.
#[tool(label = "Apply a patch", icon = "edit", parallel = "write")]
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
    let slot = match env.slot_to_write(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    match apply_in(&GitAt::slot(&slot), &patch).await {
        Ok(paths) => {
            let tail = env
                .after_write(ctx, &place_of(slot.clone()), &paths)
                .await?;
            ctx.emit_progress(format!("patched {} ({})", listed(&paths), slot.dir()))
                .await;
            Ok(ToolOutput::text(format!(
                "Applied the patch to {} file(s): {}.{tail}",
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

/// Replace exact text in a file of your worktree: every character of `old`, blanks and line breaks
/// included, is replaced by `new`. It is the way to make a small change without writing a diff:
/// read the file, copy the lines you mean to change as `old`, write what they become as `new`. The
/// exact match is tried first; when it fails, line endings and trailing blanks are tolerated;
/// leading whitespace is not. It refuses, and changes nothing, when `old` is not in the file (the
/// error shows the closest region with line numbers, so you can copy it right) or when `old` is in
/// the file more than once and `replace_all` is not set (the error lists the lines: add more of the
/// text around it to make it unique). The path is relative to the root of the worktree; nothing
/// inside `.git` and nothing through a symlink can be edited. The file must be text of at most
/// 1 MiB. To create a file or replace it whole use write_file, for several files at once
/// apply_patch.
#[tool(
    label = "Edit a file",
    icon = "edit",
    parallel = "write",
    path = "path"
)]
pub async fn edit_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the root of the worktree
    path: String,
    /// The text to replace, copied from the file (blanks and line breaks too). Must be in the file; and once, unless replace_all is set.
    old: String,
    /// What it becomes. May be empty, to delete the text.
    new: String,
    /// Replace every place `old` is in. Leave out to replace the one place that has it.
    replace_all: Option<bool>,
    /// The slot to edit in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    adam_code_tools::files::edit_file(&*env, ctx, path, old, new, replace_all, repo).await
}

/// A slot and another run's directory of the same shape, and what a command of the run can do
/// to the slot between the coder's check of a path and its use of it
/// (vymalo/another-adam-rs#125).
#[cfg(test)]
pub(super) mod swapped {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// What the other run's files say; nothing of the slot's may ever show it.
    pub(crate) const SECRET: &str = "ANOTHER RUN'S SECRET";
    /// The files of the slot and of the other run share their first line, so that an edit of the
    /// one that is read in the place of the other says, in its error, what the other holds.
    pub(crate) const MINE_TOP: &str = "shared line\noriginal top\n";
    pub(crate) const MINE_SUB: &str = "shared line\noriginal sub\n";
    const THEIRS_TOP: &str = "shared line\nANOTHER RUN'S SECRET\n";
    const THEIRS_SUB: &str = "shared line\nANOTHER RUN'S SECRET\n";

    pub(crate) struct Pair {
        pub(crate) _tmp: tempfile::TempDir,
        /// `top.txt`, `sub/file.txt` and a `.git` file.
        pub(crate) slot: PathBuf,
        /// The same files, saying [`SECRET`] on their second line.
        pub(crate) other: PathBuf,
    }

    pub(crate) fn pair() -> Pair {
        let tmp = tempfile::tempdir().unwrap();
        let slot = tmp.path().join("slot");
        let other = tmp.path().join("other");
        for (dir, top, sub) in [
            (&slot, MINE_TOP, MINE_SUB),
            (&other, THEIRS_TOP, THEIRS_SUB),
        ] {
            fs::create_dir_all(dir.join("sub")).unwrap();
            fs::write(dir.join("top.txt"), top).unwrap();
            fs::write(dir.join("sub/file.txt"), sub).unwrap();
            fs::write(dir.join(".git"), "gitdir: /elsewhere\n").unwrap();
        }
        Pair {
            _tmp: tmp,
            slot,
            other,
        }
    }

    /// Where a swapped directory is moved to.
    pub(crate) fn away_of(dir: &Path) -> PathBuf {
        dir.with_file_name(format!(
            "{}.away",
            dir.file_name().unwrap().to_string_lossy()
        ))
    }

    /// The slot's directory moved away and a link to `other` at its name.
    pub(crate) fn swap_slot(slot: &Path, other: &Path) {
        fs::rename(slot, away_of(slot)).unwrap();
        std::os::unix::fs::symlink(other, slot).unwrap();
    }

    /// `sub` of the slot moved away and a link to `sub` of `other` at its name.
    pub(crate) fn swap_sub(slot: &Path, other: &Path) {
        fs::rename(slot.join("sub"), away_of(&slot.join("sub"))).unwrap();
        std::os::unix::fs::symlink(other.join("sub"), slot.join("sub")).unwrap();
    }

    /// Every file below `dir`, by relative path.
    pub(crate) fn tree(dir: &Path) -> BTreeMap<String, String> {
        fn walk(dir: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    walk(&path, at, out);
                } else {
                    out.insert(
                        path.strip_prefix(at).unwrap().display().to_string(),
                        fs::read_to_string(&path).unwrap_or_default(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }
}

#[cfg(test)]
mod swap_tests {
    use adam_workspace::safe_fs::race;

    use super::swapped::*;
    use super::*;

    /// The slot at `path`, opened as the tools open it: the check of the tests below, after which
    /// the swap happens.
    fn open(path: &Path) -> SlotDir {
        SlotDir::open(path).unwrap()
    }

    /// The text of the file, with the numbers and the final line of its page taken off.
    fn read(root: &SlotDir, rel: &str) -> Result<String, String> {
        read_in(root, rel, None, None).map(|page| {
            let mut parts: Vec<&str> = page.split('\n').collect();
            parts.pop();
            parts
                .iter()
                .map(|line| format!("{}\n", line.split_once('\t').map_or("", |(_, text)| text)))
                .collect()
        })
    }

    fn write(root: &SlotDir, rel: &str, content: &str) -> Result<Written, String> {
        write_in(root, rel, content)
    }

    fn edit(root: &SlotDir, rel: &str, old: &str, new: &str) -> Result<Edited, String> {
        edit_in(root, rel, old, new, false)
    }

    /// Run `swap` when the checkpoint `at` is reached for `slot`.
    fn at(slot: &Path, checkpoint: &'static str, swap: impl Fn() + Send + 'static) -> race::Hook {
        race::on_checkpoint(slot, move |at, _| {
            if at == checkpoint {
                swap();
            }
        })
    }

    #[test]
    fn a_read_does_not_follow_a_slot_swapped_after_the_path_was_checked() {
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_slot(&slot, &other));
        let got = read(&open(&p.slot), "top.txt");
        assert_eq!(hook.reached("read"), 1, "the swap was made");
        assert_eq!(got, Ok(MINE_TOP.to_owned()), "the slot's own file");
    }

    #[test]
    fn a_read_does_not_follow_a_directory_swapped_after_the_path_was_checked() {
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_sub(&slot, &other));
        let got = read(&open(&p.slot), "sub/file.txt");
        assert_eq!(hook.reached("read"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");
    }

    #[test]
    fn search_does_not_follow_a_slot_swapped_after_the_check() {
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        // The candidates are listed; then the slot is swapped for a link to another run's files.
        let hook = at(&p.slot, "search", move || swap_slot(&slot, &other));
        let paths = ["sub/file.txt".to_owned(), "top.txt".to_owned()];
        let got = super::super::find::search_files_in(
            &open(&p.slot),
            &paths,
            "shared|original|SECRET",
            None,
            None,
        )
        .unwrap();
        assert_eq!(hook.reached("search"), 1, "the swap was made");
        assert!(!got.contains(SECRET), "{got}");
        assert_eq!(
            got,
            "sub/file.txt:1:shared line\nsub/file.txt:2:original sub\n\
             top.txt:1:shared line\ntop.txt:2:original top\n[end: 4 matches in 2 files]"
        );
    }

    #[test]
    fn a_write_does_not_land_in_a_slot_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_slot(&slot, &other));
        let got = write(&open(&p.slot), "top.txt", "overwritten\n");
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(
            fs::read_to_string(away_of(&p.slot).join("top.txt")).unwrap(),
            "overwritten\n",
            "it went to the directory the slot was"
        );
    }

    #[test]
    fn a_write_does_not_land_in_a_directory_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_sub(&slot, &other));
        let _ = write(&open(&p.slot), "sub/new.txt", "written\n");
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
    }

    #[test]
    fn an_edit_does_not_read_a_slot_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "edit", move || swap_slot(&slot, &other));
        // The text is in the slot's file and not in the other run's: an edit that read the other
        // run's file would say what that file holds, to the model.
        let got = edit(
            &open(&p.slot),
            "top.txt",
            MINE_TOP,
            "shared line\nchanged top\n",
        );
        assert_eq!(hook.reached("edit"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");
        assert_eq!(tree(&p.other), before);
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(
            fs::read_to_string(away_of(&p.slot).join("top.txt")).unwrap(),
            "shared line\nchanged top\n"
        );
    }

    #[test]
    fn an_edit_does_not_write_into_a_slot_swapped_between_reading_and_writing() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_slot(&slot, &other));
        let got = edit(
            &open(&p.slot),
            "top.txt",
            MINE_TOP,
            "shared line\nchanged top\n",
        );
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
        assert!(got.is_ok(), "{got:?}");
        assert_eq!(
            fs::read_to_string(away_of(&p.slot).join("top.txt")).unwrap(),
            "shared line\nchanged top\n"
        );
    }

    #[test]
    fn an_edit_does_not_follow_a_directory_swapped_after_the_path_was_checked() {
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "edit", move || swap_sub(&slot, &other));
        let got = edit(
            &open(&p.slot),
            "sub/file.txt",
            MINE_SUB,
            "shared line\nchanged sub\n",
        );
        assert_eq!(hook.reached("edit"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");
        assert_eq!(tree(&p.other), before);
    }

    /// Where the kernel has no `openat2` the same refusals are made by a walk of one component at a
    /// time (`Backend::Walk`): the tests above run on whatever the system has, this one on the walk.
    #[test]
    fn the_fallback_walk_does_not_follow_a_slot_or_a_directory_swapped_after_the_check_either() {
        use adam_workspace::safe_fs::Backend;

        let walk = |path: &Path| SlotDir::open_using(path, Backend::Walk).unwrap();

        // A read, with the slot swapped.
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_slot(&slot, &other));
        assert_eq!(read(&walk(&p.slot), "top.txt"), Ok(MINE_TOP.to_owned()));
        assert_eq!(hook.reached("read"), 1, "the swap was made");

        // A read, with a directory swapped.
        let p = pair();
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "read", move || swap_sub(&slot, &other));
        let got = read(&walk(&p.slot), "sub/file.txt");
        assert_eq!(hook.reached("read"), 1, "the swap was made");
        assert!(!format!("{got:?}").contains(SECRET), "{got:?}");

        // A write, with a directory swapped.
        let p = pair();
        let before = tree(&p.other);
        let (slot, other) = (p.slot.clone(), p.other.clone());
        let hook = at(&p.slot, "write", move || swap_sub(&slot, &other));
        let _ = write(&walk(&p.slot), "sub/new.txt", "written\n");
        assert_eq!(hook.reached("write"), 1, "the swap was made");
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
    }

    /// The slot flips between a directory and a link to another run's files while the coder
    /// writes: nothing it writes shows in the other run's files. (Extra to the deterministic tests
    /// above, which are the evidence: this one only needs to never fail.)
    #[test]
    fn files_written_while_the_slot_flips_never_reach_another_runs_files() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let p = pair();
        let before = tree(&p.other);
        // The slot is opened once, before it starts to flip: every write goes to that directory,
        // whatever its name is at the moment.
        let root = open(&p.slot);
        let stop = Arc::new(AtomicBool::new(false));
        let flipper = {
            let (slot, other, stop) = (p.slot.clone(), p.other.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if fs::rename(&slot, away_of(&slot)).is_err() {
                        continue;
                    }
                    let _ = std::os::unix::fs::symlink(&other, &slot);
                    let _ = fs::remove_file(&slot);
                    let _ = fs::rename(away_of(&slot), &slot);
                }
            })
        };
        for i in 0..500 {
            write(&root, &format!("sub/f{i}.txt"), "flip\n").unwrap();
            write(&root, "top.txt", "flip\n").unwrap();
            assert_eq!(read(&root, "sub/file.txt").unwrap(), MINE_SUB);
        }
        stop.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        assert_eq!(
            tree(&p.other),
            before,
            "the other run's files are as they were"
        );
        // All of it is in the directory that was opened, wherever its name led meanwhile.
        assert_eq!(read(&root, "sub/f499.txt").unwrap(), "flip\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot at `root`, opened as the tools open it: the check and the hold of a slot.
    fn open(root: &Path) -> Result<SlotDir, String> {
        SlotDir::open(root).map_err(|e| {
            format!(
                "the worktree's directory is not a plain directory (a command replaced it by a link or a file): {e}"
            )
        })
    }

    fn confine(root: &Path, rel: &str, access: Access) -> Result<Confined, String> {
        super::confine(&open(root)?, rel, access)
    }

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
        let accepted = |rel: &str, access| confine(dir.path(), rel, access).unwrap().rel();
        assert_eq!(
            accepted("src/main.rs", Access::Read),
            Path::new("src/main.rs")
        );
        assert_eq!(
            accepted("./src//main.rs", Access::Read),
            Path::new("src/main.rs")
        );
        // A file that is not there yet, in a directory that is not there yet.
        assert_eq!(
            accepted("new/deep/file.txt", Access::Write),
            Path::new("new/deep/file.txt")
        );
        assert_eq!(
            accepted("src/main.rs", Access::Write),
            Path::new("src/main.rs")
        );
        // Nothing was made by asking.
        assert!(!dir.path().join("new").exists());
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
    fn a_root_that_is_a_link_to_another_directory_is_not_a_root() {
        let dir = root();
        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join(".env"), "TOKEN=another run's").unwrap();
        let link = other.path().join("slot");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        // A link to a worktree is canonicalised into that worktree: refused before.
        for access in [Access::Read, Access::Write] {
            let why = refused(&link, "README.md", access);
            assert!(why.contains("not a plain directory"), "{why}");
        }
        // And the real directory is as it was.
        assert!(confine(dir.path(), "README.md", Access::Read).is_ok());
        // A file is no root either.
        let why = refused(&dir.path().join("README.md"), "x", Access::Write);
        assert!(why.contains("not a plain directory"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_dot_git_is_not_read_either() {
        let dir = root();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::os::unix::fs::symlink("../.git", dir.path().join("sub/g")).unwrap();
        let why = refused(dir.path(), "sub/g", Access::Read);
        assert!(why.contains(".git"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_is_absolute_is_refused_even_when_it_leads_into_the_worktree() {
        let dir = root();
        std::os::unix::fs::symlink(dir.path().join("README.md"), dir.path().join("abs")).unwrap();
        let why = refused(dir.path(), "abs", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_goes_up_past_the_root_is_refused_and_one_that_comes_back_is_not() {
        let dir = root();
        std::os::unix::fs::symlink("../x", dir.path().join("up")).unwrap();
        std::os::unix::fs::symlink("src/../README.md", dir.path().join("around")).unwrap();
        let why = refused(dir.path(), "up", Access::Read);
        assert!(why.contains("outside the worktree"), "{why}");
        assert_eq!(
            confine(dir.path(), "around", Access::Read).unwrap().rel(),
            Path::new("README.md")
        );
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

/// The patch the model writes is applied by the git of the sandbox when the deployment has one: it
/// travels as the command's standard input.
#[cfg(test)]
mod apply_sandbox_tests {
    use std::sync::Arc;

    use adam_workspace::testing_sandbox::GitSandboxServer;
    use adam_workspace::{GitIdentity, StaticToken, Workspaces};

    use super::*;
    use crate::tools::gitcli::GitAt;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_patch_is_applied_by_the_git_of_the_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let server = GitSandboxServer::start(tmp.path(), &root);
        let workspaces = Workspaces::new(root, Arc::new(StaticToken::new("unused")))
            .with_git_sandbox(server.socket());
        let slot = workspaces
            .run("run-aaaaaaaa-1")
            .unwrap()
            .add_scratch("proj", &GitIdentity::new("t", "t@example.com"))
            .await
            .unwrap();
        fs::write(slot.path().join("a.txt"), "one\ntwo\n").unwrap();
        let at = GitAt::slot(&slot);
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+three\n";

        let checked = git_apply(&at, &["--check"], patch).await.unwrap();
        assert!(checked.success, "{}", checked.stderr);
        assert_eq!(
            fs::read_to_string(slot.path().join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        let applied = git_apply(&at, &[], patch).await.unwrap();
        assert!(applied.success, "{}", applied.stderr);
        assert_eq!(
            fs::read_to_string(slot.path().join("a.txt")).unwrap(),
            "one\nthree\n"
        );
        // A patch that does not apply is git's verdict, not an error of the call.
        let refused = git_apply(&at, &[], patch).await.unwrap();
        assert!(!refused.success);
        assert!(!refused.stderr.is_empty());

        let ran = server.subcommands();
        assert_eq!(
            ran.iter().filter(|s| *s == "apply").count(),
            3,
            "all three went through the server: {ran:?}"
        );
    }
}
