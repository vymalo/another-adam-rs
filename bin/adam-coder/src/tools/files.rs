//! `read_file`, `write_file`, `edit_file` and `apply_patch`: the coder edits small, well-located things itself.
//!
//! The model used to change a worktree only by delegating to OpenCode, which costs a process, a
//! model and a conversation for a one-line fix. These tools read and change files of the
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
//! The **root** is the slot's directory *as the coder opened it* ([`SlotDir`], held by the slot):
//! the directory is the run's to rename, and a link put at its name (to another run's slot) would
//! be followed by anything that uses the name. Nothing here uses the name. A path is resolved and
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
//! only after a new `run_checks` (the same rule as for OpenCode's edits).
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

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, Read, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use adam::prelude::*;
use adam_workspace::safe_fs::{BeneathError, Dir, Meta, SlotDir, WalkError, race};

use super::gitcli::GitAt;
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

/// A path that [`confine`] accepted: the components below the slot's root. For a read they are the
/// path with every link on the way followed (it has none), for a write the path as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confined {
    parts: Vec<OsString>,
}

impl Confined {
    /// The path, relative to the slot's root.
    pub fn rel(&self) -> PathBuf {
        self.parts.iter().collect()
    }
}

/// Check `rel` against the slot `root` for `access`, or give the reason the model is told.
///
/// The rule is in the [module docs](self#the-confinement-rule-confine). Everything is looked at
/// relative to `root`, the slot's directory as the coder opened it, never by its name. For
/// [`Access::Read`] the path must exist; for [`Access::Write`] it may not exist yet (nor its
/// parents).
///
/// This is a **check**: it opens nothing the caller can use. A caller that goes on to read or write
/// does it with `open_for_read` and `locate_for_write`, which look at the path again as they
/// open it.
///
/// # Errors
///
/// A message for the model: the path is empty, absolute, goes up, names `.git`, leaves the
/// worktree through a symlink, goes through a symlink to write, or (to read) does not exist.
pub fn confine(root: &SlotDir, rel: &str, access: Access) -> Result<Confined, String> {
    let parts = parts_of(rel)?;
    let shown = rel.trim();
    match access {
        Access::Read => {
            let path: PathBuf = parts.iter().collect();
            let resolved = root
                .resolve_inside(&path, &is_dot_git)
                .map_err(|e| read_refusal(e, shown))?;
            Ok(Confined { parts: resolved })
        }
        Access::Write => {
            locate_for_write(root, &parts, shown, false)?;
            Ok(Confined { parts })
        }
    }
}

/// What the model is told when a path cannot be resolved for a read.
fn read_refusal(e: BeneathError, shown: &str) -> String {
    match e {
        BeneathError::Escapes => {
            format!("`{shown}` leads outside the worktree (through a symlink): refused")
        }
        BeneathError::Avoided(_) => {
            format!("`{shown}` leads into .git (through a symlink): refused")
        }
        BeneathError::TooManyLinks => {
            format!("cannot resolve `{shown}`: too many levels of symbolic links")
        }
        BeneathError::Changed => {
            format!("`{shown}` changed while it was opened (a symlink is on its way now): refused")
        }
        BeneathError::Io(e) if e.kind() == io::ErrorKind::NotFound => {
            format!("`{shown}` does not exist")
        }
        BeneathError::Io(e) => format!("cannot resolve `{shown}`: {e}"),
    }
}

/// Open `rel` of the slot `root` for reading, and say what it is (a file, a directory or
/// something else: the caller tells the model).
///
/// The path is resolved inside the slot, following the links that stay inside it; the file is then
/// opened by the path that resolution found, which has no link in it, relative to `root`. So a
/// link put on the way after the resolution is refused and never followed, and so is a slot
/// swapped for a link meanwhile (the descriptor is the directory that was opened).
pub(super) fn open_for_read(root: &SlotDir, rel: &str) -> Result<(fs::File, Meta), String> {
    let parts = parts_of(rel)?;
    let shown = rel.trim();
    let path: PathBuf = parts.iter().collect();
    root.open_following(&path, &is_dot_git)
        .map_err(|e| read_refusal(e, shown))
}

/// Where a write goes: the directory the file is in, as a descriptor, and the file.
struct Located {
    /// What the model called the file.
    shown: String,
    /// The slot, for the checkpoints of the tests.
    slot: PathBuf,
    /// The directory of the file; `None` when it (or a directory above it) is not there yet and
    /// nothing was made.
    parent: Option<Dir>,
    name: OsString,
    /// What is at `name`, without following it.
    existing: Option<Meta>,
}

/// Find the directory a write to `parts` goes in, relative to `root`, making the directories that
/// are missing when `create` is set, and what is at the file now. Refuses (as [`confine`] says) a
/// link anywhere on the way, the file itself included, a file where a directory is needed and a
/// directory where the file is to be.
fn locate_for_write(
    root: &SlotDir,
    parts: &[OsString],
    shown: &str,
    create: bool,
) -> Result<Located, String> {
    let Some((name, dirs)) = parts.split_last() else {
        return Err("path is required".to_owned());
    };
    let prefix = |upto: usize| -> String {
        parts[..=upto]
            .iter()
            .collect::<PathBuf>()
            .display()
            .to_string()
    };
    let linked = |upto: usize| {
        format!(
            "`{shown}` goes through the symlink `{}`: writing through a symlink is refused",
            prefix(upto)
        )
    };
    let dir_path: PathBuf = dirs.iter().collect();
    let parent = match root.walk(&dir_path, create) {
        Ok(dir) => Some(dir),
        Err(WalkError::Missing(_)) => None,
        Err(WalkError::Link(at)) => return Err(linked(at)),
        Err(WalkError::NotDirectory(at)) => {
            return Err(format!(
                "`{}` is a file, not a directory: `{shown}` cannot be inside it",
                prefix(at)
            ));
        }
        Err(WalkError::Io(e)) if create => {
            return Err(format!("cannot create the directories of `{shown}`: {e}"));
        }
        Err(WalkError::Io(e)) => return Err(format!("cannot look at `{shown}`: {e}")),
    };
    let existing = match &parent {
        Some(dir) => match dir.lstat(name) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("cannot look at `{shown}`: {e}")),
        },
        None => None,
    };
    match existing {
        Some(meta) if meta.is_symlink() => return Err(linked(parts.len() - 1)),
        Some(meta) if meta.is_dir() => {
            return Err(format!("`{shown}` is a directory: name a file"));
        }
        _ => {}
    }
    Ok(Located {
        shown: shown.to_owned(),
        slot: root.path().to_path_buf(),
        parent,
        name: name.clone(),
        existing,
    })
}

/// The text of `rel` under `root`: the whole file up to [`READ_CAP`], or the lines `start..=end`
/// with their numbers.
fn read_in(
    root: &SlotDir,
    rel: &str,
    start: Option<u32>,
    end: Option<u32>,
) -> Result<String, String> {
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
    let (file, meta) = open_for_read(root, rel)?;
    let shown = rel.trim();
    if meta.is_dir() {
        return Err(format!(
            "`{shown}` is a directory: list it with run_command (`ls {shown}`)"
        ));
    }
    if !meta.is_file() {
        return Err(format!("`{shown}` is not a regular file"));
    }
    if start.is_none() && end.is_none() {
        read_whole(file, meta.size(), shown)
    } else {
        read_range(
            io::BufReader::new(file),
            meta.size(),
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

/// Write `content` to `rel` of the slot `root`: parents are created, the file is written next to
/// its target and renamed over it (an interrupted write leaves the old file or the new one, never
/// half of one), and the mode of a file that is replaced is kept (an executable script stays one).
fn write_in(root: &SlotDir, rel: &str, content: &str) -> Result<Written, String> {
    if content.len() > WRITE_CAP {
        return Err(format!(
            "content is {} bytes, over the limit of {WRITE_CAP}: write the file in parts with \
             apply_patch, or have OpenCode do it",
            content.len()
        ));
    }
    let shown = rel.trim();
    let parts = parts_of(rel)?;
    let located = locate_for_write(root, &parts, shown, true)?;
    write_located(&located, content)
}

/// The write of [`write_in`], to the directory [`locate_for_write`] found: a new file made in it
/// (`O_EXCL`, so nothing is followed), written, and renamed over the target.
fn write_located(located: &Located, content: &str) -> Result<Written, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let shown = &located.shown;
    let Some(parent) = &located.parent else {
        return Err(format!("`{shown}` has no directory to be written in"));
    };
    race::checkpoint(&located.slot, "write");
    let tmp = OsString::from(format!(
        ".adam-write-{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let mut file = parent.create_new(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        if let Some(meta) = &located.existing {
            file.set_permissions(fs::Permissions::from_mode(meta.permissions()))?;
        }
        drop(file);
        parent.rename(&tmp, &located.name)
    })();
    if let Err(e) = written {
        let _ = parent.unlink(&tmp);
        return Err(format!("cannot write `{shown}`: {e}"));
    }
    Ok(Written {
        created: located.existing.is_none(),
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

/// What a change to a scratch project that was already published adds to the tool's result: the
/// change does not reach the repository ([`published_note`](super::scratch::published_note)).
pub(super) fn push_published_note(text: &mut String, slot: &adam_workspace::Slot) {
    if let Some(note) = super::scratch::published_note(slot) {
        text.push('\n');
        text.push_str(&note);
    }
}

/// Read a text file of your worktree. Give `path` relative to the root of the worktree. Without
/// a range you get the whole file (cut at 256 KiB, and the cut is marked); with `start_line` and
/// `end_line` you get just those lines, each behind its number. A binary file is not shown. Use it
/// to read before you change something, and run_command (`ls`, `grep -rn`) to find where to look.
#[tool(label = "Read a file", icon = "read")]
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
    let (root, rel) = (Arc::clone(slot.root()), path.to_owned());
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
/// symlink can be written. For a change to part of a file use edit_file (exact text replaced) or
/// apply_patch (a diff); for a broad, multi-file change use delegate_to_opencode.
#[tool(label = "Write a file", icon = "edit")]
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
    let (root, rel) = (Arc::clone(slot.root()), path.to_owned());
    let written = tokio::task::spawn_blocking(move || write_in(&root, &rel, &content))
        .await
        .map_err(|e| ToolError::Transient(format!("the write was interrupted: {e}")))?;
    match written {
        Ok(done) => {
            ctx.emit_progress(format!("wrote {path} ({})", slot.dir()))
                .await;
            let mut text = format!(
                "{} {path} ({} bytes). Run the checks again before you commit.",
                if done.created { "Created" } else { "Replaced" },
                done.bytes
            );
            push_published_note(&mut text, &slot);
            Ok(ToolOutput::text(text))
        }
        Err(reason) => Ok(ToolOutput::error(reason)),
    }
}

/// Apply a unified diff to the worktree: one or several files, `--- a/<path>` and `+++ b/<path>`
/// then hunks (`@@ -12,3 +12,4 @@`, with context lines), as `git diff` writes them. Every path is
/// checked first (inside the worktree, not `.git`, not a symlink); a patch that creates a symlink
/// is refused; and it is all or nothing: if a hunk does not match the file, nothing is changed
/// and you are told why. Read the file first, so the context lines match it exactly. Use it for
/// several changes at once, in one file or several; for one exact replacement edit_file is simpler.
/// Delegate broad, multi-file changes to OpenCode.
#[tool(label = "Apply a patch", icon = "edit")]
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
    match apply_in(&GitAt::slot(&slot), &patch).await {
        Ok(paths) => {
            ctx.emit_progress(format!("patched {} ({})", listed(&paths), slot.dir()))
                .await;
            let mut text = format!(
                "Applied the patch to {} file(s): {}. Run the checks again before you commit.",
                paths.len(),
                listed(&paths)
            );
            push_published_note(&mut text, &slot);
            Ok(ToolOutput::text(text))
        }
        Err(PatchError::Refused(reason) | PatchError::Unreadable(reason)) => {
            Ok(ToolOutput::error(reason))
        }
        Err(PatchError::Git(e)) => Err(ToolError::Transient(format!("cannot run git apply: {e}"))),
    }
}

/// What an edit did.
#[derive(Debug, PartialEq, Eq)]
struct Edited {
    /// How many places were replaced.
    replaced: usize,
    /// The 1-based line each replaced place started on, in the file as it was.
    lines: Vec<usize>,
    bytes: usize,
}

/// Most places `edit_file` lists by line number.
const MAX_PLACES: usize = 10;

/// Most lines of the closest region an error shows (and of context around it, each side).
const MAX_REGION_LINES: usize = 30;
const REGION_CONTEXT: usize = 2;

/// Longest line an error quotes, in characters.
const MAX_QUOTED_CHARS: usize = 200;

/// Replace `old` with `new` in `rel` of the slot `root`: exactly, once or everywhere.
///
/// The file must exist, be a regular file of at most [`WRITE_CAP`] bytes, and be UTF-8 text without
/// NUL. The path is confined as a write is ([`confine`] with [`Access::Write`]), so nothing inside
/// `.git`, nothing outside the worktree and nothing through a symlink is edited. The file is read
/// and written in the directory the path was located in, and the write is [`write_in`]'s (a new
/// file renamed over the old, the mode kept).
fn edit_in(
    root: &SlotDir,
    rel: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<Edited, String> {
    let shown = rel.trim();
    if old.is_empty() {
        return Err(
            "old is empty: give the exact text to replace (to create a file use write_file)".into(),
        );
    }
    if old == new {
        return Err("old and new are the same text: there is nothing to change".into());
    }
    let parts = parts_of(rel)?;
    let located = locate_for_write(root, &parts, shown, false)?;
    race::checkpoint(root.path(), "edit");
    let (Some(parent), Some(meta)) = (&located.parent, located.existing) else {
        return Err(format!(
            "`{shown}` does not exist: edit_file changes a file that is there (write_file \
             creates one)"
        ));
    };
    if !meta.is_file() {
        return Err(format!("`{shown}` is not a regular file"));
    }
    if meta.size() > WRITE_CAP as u64 {
        return Err(format!(
            "`{shown}` is {} bytes, over the {WRITE_CAP} bytes edit_file works on: use apply_patch, \
             or have OpenCode make the change",
            meta.size()
        ));
    }
    let (file, _) = parent
        .open_regular(&located.name)
        .map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    let mut bytes = Vec::new();
    file.take(WRITE_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    if bytes.len() > WRITE_CAP {
        return Err(format!(
            "`{shown}` is over the {WRITE_CAP} bytes edit_file works on: use apply_patch, or have \
             OpenCode make the change"
        ));
    }
    let Ok(content) = String::from_utf8(bytes) else {
        return Err(format!(
            "`{shown}` is not UTF-8 text: edit_file changes text files"
        ));
    };
    if content.contains('\0') {
        return Err(binary_notice(shown, meta.size()));
    }

    let places: Vec<usize> = content.match_indices(old).map(|(at, _)| at).collect();
    let lines: Vec<usize> = places.iter().map(|at| line_of(&content, *at)).collect();
    match places.len() {
        0 => return Err(not_found(shown, &content, old)),
        1 => {}
        n if !replace_all => {
            return Err(format!(
                "`old` is in `{shown}` {n} times (starting at {}): nothing was changed. Add the \
                 lines around the one you mean to `old` so that it matches once, or set \
                 replace_all to replace every one",
                places_listed(&lines)
            ));
        }
        _ => {}
    }
    let edited = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    let written = write_located(&located, &edited)?;
    Ok(Edited {
        replaced: places.len(),
        lines,
        bytes: written.bytes,
    })
}

/// The 1-based line of the byte offset `at` of `text`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at].bytes().filter(|b| *b == b'\n').count() + 1
}

/// `lines 3, 9, 14` (the first [`MAX_PLACES`], then how many more).
fn places_listed(lines: &[usize]) -> String {
    let mut text = format!(
        "line{} {}",
        if lines.len() == 1 { "" } else { "s" },
        lines
            .iter()
            .take(MAX_PLACES)
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    if lines.len() > MAX_PLACES {
        text.push_str(&format!(" and {} more", lines.len() - MAX_PLACES));
    }
    text
}

/// `line` cut to [`MAX_QUOTED_CHARS`] characters.
fn quoted(line: &str) -> String {
    let mut text: String = line.chars().take(MAX_QUOTED_CHARS).collect();
    if line.chars().count() > MAX_QUOTED_CHARS {
        text.push_str("...");
    }
    text
}

/// How alike two lines are, from 0 to 1000: the Sorensen-Dice coefficient of their character pairs
/// (whitespace at the ends does not count). Two empty lines are alike; an empty line and one with
/// text are not.
fn similarity(a: &str, b: &str) -> usize {
    let (a, b) = (a.trim(), b.trim());
    if a == b {
        return 1000;
    }
    let pairs = |s: &str| -> Vec<(char, char)> {
        let chars: Vec<char> = s.chars().collect();
        chars.windows(2).map(|w| (w[0], w[1])).collect()
    };
    let (mut left, right) = (pairs(a), pairs(b));
    if left.is_empty() || right.is_empty() {
        // A single character each: alike only if equal, which `a == b` above already took.
        return 0;
    }
    let total = left.len() + right.len();
    let mut common = 0;
    for pair in right {
        if let Some(at) = left.iter().position(|p| *p == pair) {
            left.swap_remove(at);
            common += 1;
        }
    }
    2 * common * 1000 / total
}

/// Where the lines of `old` are most nearly in the lines of `content`: the 0-based index of the
/// first line of the window of the same length that matches best, or `None` when nothing in the file
/// is anything like it.
///
/// A line of `old` that is in the file (ignoring the whitespace at its ends) votes for the window
/// it would sit in if `old` were there; the window with the most votes is the closest. When no line
/// matches at all (every line was written a little differently), the most alike pair of one of the
/// longest lines of `old` and a line of the file places the window.
fn closest_window(content_lines: &[&str], old_lines: &[&str]) -> Option<usize> {
    use std::collections::HashMap;
    let mut by_text: HashMap<&str, Vec<usize>> = HashMap::new();
    for (at, line) in content_lines.iter().enumerate() {
        by_text.entry(line.trim()).or_default().push(at);
    }
    let mut votes: HashMap<usize, usize> = HashMap::new();
    for (i, line) in old_lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for &at in by_text.get(line).into_iter().flatten() {
            if let Some(start) = at.checked_sub(i) {
                *votes.entry(start).or_default() += 1;
            }
        }
    }
    // The most votes; the earliest window among equals.
    if let Some((start, _)) = votes.iter().max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0))) {
        return Some(*start);
    }
    // No line is the same: the best pair of one of the longest lines of `old` and a line of the
    // file, when it is alike enough to be worth showing.
    let mut longest: Vec<(usize, &str)> = old_lines
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    longest.sort_by_key(|(_, l)| std::cmp::Reverse(l.trim().len()));
    let mut best: Option<(usize, usize)> = None;
    for (i, line) in longest.into_iter().take(3) {
        for (at, candidate) in content_lines.iter().enumerate() {
            let score = similarity(line, candidate);
            if score >= 500 && best.is_none_or(|(s, _)| score > s) {
                best = Some((score, at.saturating_sub(i)));
            }
        }
    }
    best.map(|(_, start)| start)
}

/// A line made readable when it differs from another only in blanks: tabs, spaces at the ends and
/// carriage returns are spelled out.
fn visible(line: &str) -> String {
    quoted(&line.replace('\t', "\\t").replace('\r', "\\r"))
}

/// What the model is told when `old` is nowhere in the file: the region that is closest to it with
/// line numbers and a few lines around it, and the first line where the two differ.
fn not_found(shown: &str, content: &str, old: &str) -> String {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old.lines().collect();
    let mut message = format!("`old` was not found in `{shown}`, and nothing was changed.");
    if content.contains("\r\n") && !old.contains('\r') {
        message.push_str(
            " The file uses CRLF line endings, which a multi-line `old` must have as well.",
        );
    }
    let Some(start) =
        closest_window(&content_lines, &old_lines).filter(|s| *s < content_lines.len())
    else {
        message.push_str(
            " Nothing in the file is like it: read the file again with read_file and copy the \
             text from it.",
        );
        return message;
    };
    let len = old_lines.len().max(1);
    let end = (start + len).min(content_lines.len());
    let shown_end = end.min(start + MAX_REGION_LINES);
    let from = start.saturating_sub(REGION_CONTEXT);
    let to = (shown_end + REGION_CONTEXT).min(content_lines.len());
    message.push_str(&format!(
        " The closest region is lines {}-{} (shown with its surroundings):\n",
        start + 1,
        end
    ));
    for (at, line) in content_lines.iter().enumerate().take(to).skip(from) {
        let mark = if (start..shown_end).contains(&at) {
            ">"
        } else {
            " "
        };
        message.push_str(&format!("{mark}{:>6}\t{}\n", at + 1, quoted(line)));
    }
    if end > shown_end {
        message.push_str(&format!(
            "       \t[... {} more lines of the region]\n",
            end - shown_end
        ));
    }
    // The first line where the region and `old` part.
    let differs = (0..len).find(|i| content_lines.get(start + i) != old_lines.get(*i));
    if let Some(i) = differs {
        let mine = old_lines.get(i).copied().unwrap_or("");
        match content_lines.get(start + i) {
            Some(theirs) if theirs.trim() == mine.trim() => message.push_str(&format!(
                "First difference, at line {}: only the blanks differ (tabs or spaces, spaces at \
                 the end, or the line ending). The file has `{}`, your `old` has `{}`.\n",
                start + i + 1,
                visible(theirs),
                visible(mine)
            )),
            Some(theirs) => message.push_str(&format!(
                "First difference, at line {}: the file has `{}`, your `old` has `{}`.\n",
                start + i + 1,
                quoted(theirs),
                quoted(mine)
            )),
            None => message.push_str(&format!(
                "Your `old` goes on past the end of the file, at its line {}: `{}`.\n",
                i + 1,
                quoted(mine)
            )),
        }
    }
    message.push_str("Copy `old` from the file exactly, or use apply_patch.");
    message
}

/// Replace exact text in a file of your worktree: every character of `old`, blanks and line breaks
/// included, is replaced by `new`. It is the way to make a small change without writing a diff:
/// read the file, copy the lines you mean to change as `old`, write what they become as `new`. It
/// refuses, and changes nothing, when `old` is not in the file (the error shows the closest region
/// with line numbers, so you can copy it right) or when `old` is in the file more than once and
/// `replace_all` is not set (the error lists the lines: add more of the text around it to make it
/// unique). The path is relative to the root of the worktree; nothing inside `.git` and nothing
/// through a symlink can be edited. The file must be text of at most 1 MiB. To create a file or
/// replace it whole use write_file, for several files at once apply_patch.
#[tool(label = "Edit a file", icon = "edit")]
pub async fn edit_file(
    env: State<ToolEnv>,
    ctx: &ToolCtx,
    /// Path of the file, relative to the root of the worktree
    path: String,
    /// The exact text to replace, copied from the file (blanks and line breaks too). Must be in the file; and once, unless replace_all is set.
    old: String,
    /// What it becomes. May be empty, to delete the text.
    new: String,
    /// Replace every place `old` is in. Leave out to replace the one place that has it.
    replace_all: Option<bool>,
    /// The slot to edit in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let slot = match env.slot(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let (root, rel) = (Arc::clone(slot.root()), path.to_owned());
    let all = replace_all.unwrap_or(false);
    let edited = tokio::task::spawn_blocking(move || edit_in(&root, &rel, &old, &new, all))
        .await
        .map_err(|e| ToolError::Transient(format!("the edit was interrupted: {e}")))?;
    match edited {
        Ok(done) => {
            ctx.emit_progress(format!("edited {path} ({})", slot.dir()))
                .await;
            let mut text = format!(
                "Edited {path}: replaced {} place{} ({}), {} bytes now. Run the checks again \
                 before you commit.",
                done.replaced,
                if done.replaced == 1 { "" } else { "s" },
                places_listed(&done.lines),
                done.bytes
            );
            push_published_note(&mut text, &slot);
            Ok(ToolOutput::text(text))
        }
        Err(reason) => Ok(ToolOutput::error(reason)),
    }
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

    fn read(root: &SlotDir, rel: &str) -> Result<String, String> {
        read_in(root, rel, None, None)
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

    fn read_in(
        root: &Path,
        rel: &str,
        start: Option<u32>,
        end: Option<u32>,
    ) -> Result<String, String> {
        super::read_in(&open(root)?, rel, start, end)
    }

    fn write_in(root: &Path, rel: &str, content: &str) -> Result<Written, String> {
        super::write_in(&open(root)?, rel, content)
    }

    fn edit_in(
        root: &Path,
        rel: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<Edited, String> {
        super::edit_in(&open(root)?, rel, old, new, replace_all)
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

    const MAIN_RS: &str = "use std::io;\n\nfn main() {\n    let name = \"world\";\n    println!(\"hello, {name}\");\n}\n\nfn helper() {\n    let name = \"world\";\n    drop(name);\n}\n";

    fn edit(root: &Path, old: &str, new: &str, all: bool) -> Result<Edited, String> {
        edit_in(root, "src/main.rs", old, new, all)
    }

    fn with_main() -> tempfile::TempDir {
        let dir = root();
        fs::write(dir.path().join("src/main.rs"), MAIN_RS).unwrap();
        dir
    }

    #[test]
    fn an_edit_replaces_the_one_place_and_says_where() {
        let dir = with_main();
        let done = edit(
            dir.path(),
            "println!(\"hello, {name}\");",
            "println!(\"bye, {name}\");",
            false,
        )
        .unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (1, vec![5]));
        let now = fs::read_to_string(dir.path().join("src/main.rs")).unwrap();
        assert_eq!(now, MAIN_RS.replace("hello", "bye"));
        assert_eq!(done.bytes, now.len());
        // A multi-line replacement, and a deletion (an empty `new`).
        edit(
            dir.path(),
            "fn helper() {\n    let name = \"world\";\n    drop(name);\n}\n",
            "",
            false,
        )
        .unwrap();
        assert!(
            !fs::read_to_string(dir.path().join("src/main.rs"))
                .unwrap()
                .contains("helper")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_edit_keeps_the_mode_of_the_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = with_main();
        let path = dir.path().join("src/main.rs");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        edit(dir.path(), "hello", "bye", false).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn a_text_in_two_places_is_refused_with_their_lines_unless_replace_all_is_set() {
        let dir = with_main();
        let why = edit(
            dir.path(),
            "let name = \"world\";",
            "let name = \"you\";",
            false,
        )
        .unwrap_err();
        assert!(why.contains("2 times (starting at lines 4, 9)"), "{why}");
        assert!(why.contains("replace_all"), "{why}");
        assert_eq!(
            fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
            MAIN_RS,
            "nothing changed"
        );
        let done = edit(
            dir.path(),
            "let name = \"world\";",
            "let name = \"you\";",
            true,
        )
        .unwrap();
        assert_eq!((done.replaced, done.lines), (2, vec![4, 9]));
        assert_eq!(
            fs::read_to_string(dir.path().join("src/main.rs"))
                .unwrap()
                .matches("\"you\"")
                .count(),
            2
        );
        // replace_all with one place is that place.
        let one = edit(dir.path(), "use std::io;", "use std::fmt;", true).unwrap();
        assert_eq!(one.replaced, 1);
    }

    #[test]
    fn a_text_that_is_not_there_shows_the_closest_region_with_numbers_and_the_first_difference() {
        let dir = with_main();
        // Indentation is wrong (two spaces for four): every line of it is nearly there.
        let why = edit(
            dir.path(),
            "fn main() {\n  let name = \"world\";\n  println!(\"hello, {name}\");\n}",
            "x",
            false,
        )
        .unwrap_err();
        assert!(
            why.contains("was not found") && why.contains("nothing was changed"),
            "{why}"
        );
        assert!(why.contains("closest region is lines 3-6"), "{why}");
        // The region with line numbers, marked, and the line before it for context.
        assert!(why.contains(">     3\tfn main() {"), "{why}");
        assert!(why.contains(">     4\t    let name = \"world\";"), "{why}");
        assert!(why.contains("      2\t"), "context above: {why}");
        assert!(
            why.contains("First difference, at line 4: only the blanks differ"),
            "{why}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
            MAIN_RS
        );

        // A word that is different: the lines are said, not a blanks hint.
        let why = edit(dir.path(), "println!(\"goodbye, {name}\");", "x", false).unwrap_err();
        assert!(why.contains("closest region is lines 5-5"), "{why}");
        assert!(
            why.contains(
                "First difference, at line 5: the file has `    println!(\"hello, {name}\");`"
            ),
            "{why}"
        );
        assert!(!why.contains("only the blanks"), "{why}");
    }

    #[test]
    fn a_text_nothing_is_like_says_so_and_a_crlf_file_is_noticed() {
        let dir = with_main();
        let why = edit(dir.path(), "zzzzzzzz qqqqqqqq", "x", false).unwrap_err();
        assert!(why.contains("Nothing in the file is like it"), "{why}");
        fs::write(dir.path().join("src/crlf.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
        let why = edit_in(dir.path(), "src/crlf.txt", "one\ntwo", "x", false).unwrap_err();
        assert!(why.contains("CRLF"), "{why}");
        assert!(why.contains("closest region is lines 1-2"), "{why}");
    }

    #[test]
    fn a_very_long_region_is_cut() {
        let dir = root();
        let body: String = (0..100).map(|i| format!("line number {i}\n")).collect();
        fs::write(dir.path().join("long.txt"), &body).unwrap();
        let asked: String = (0..100)
            .map(|i| format!("line number {i}\n"))
            .collect::<String>()
            + "extra\n";
        let why = edit_in(dir.path(), "long.txt", &asked, "x", false).unwrap_err();
        assert!(why.contains("more lines of the region"), "{why}");
        assert!(why.len() < 4000, "{}", why.len());
    }

    #[test]
    fn what_edit_file_cannot_do_is_said_and_nothing_is_touched() {
        let dir = with_main();
        for (rel, old, new, expected) in [
            ("src/main.rs", "", "x", "old is empty"),
            ("src/main.rs", "fn", "fn", "nothing to change"),
            ("src/none.rs", "a", "b", "does not exist"),
            ("src", "a", "b", "is a directory"),
            ("../x", "a", "b", "`..`"),
            ("/etc/passwd", "a", "b", "absolute"),
            (".git", "a", "b", ".git"),
            ("sub/.GIT/config", "a", "b", ".git"),
            ("  ", "a", "b", "path is required"),
        ] {
            let why = edit_in(dir.path(), rel, old, new, false).unwrap_err();
            assert!(why.contains(expected), "{rel}: {why}");
        }
        fs::write(dir.path().join("bin.dat"), b"a\0b").unwrap();
        let why = edit_in(dir.path(), "bin.dat", "a", "b", false).unwrap_err();
        assert!(why.contains("binary"), "{why}");
        fs::write(dir.path().join("latin1.txt"), [0xe9, b'a']).unwrap();
        let why = edit_in(dir.path(), "latin1.txt", "a", "b", false).unwrap_err();
        assert!(why.contains("not UTF-8"), "{why}");
        assert_eq!(
            fs::read_to_string(dir.path().join("src/main.rs")).unwrap(),
            MAIN_RS
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_edited_through() {
        let dir = with_main();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "s3cr3t").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("src/main.rs"), dir.path().join("inside"))
            .unwrap();
        for rel in ["link", "inside"] {
            let why = edit_in(dir.path(), rel, "s3cr3t", "x", false).unwrap_err();
            assert!(why.contains("symlink"), "{rel}: {why}");
        }
        assert_eq!(
            fs::read_to_string(outside.path().join("secret")).unwrap(),
            "s3cr3t"
        );
    }

    #[test]
    fn the_similarity_of_lines_ignores_the_blanks_at_the_ends() {
        assert_eq!(similarity("  a b ", "a b"), 1000);
        assert_eq!(similarity("", "  "), 1000);
        assert_eq!(similarity("abc", ""), 0);
        assert!(similarity("println!(\"hello\")", "println!(\"hallo\")") > 700);
        assert!(similarity("println!(\"hello\")", "fn main() {") < 400);
        assert_eq!(line_of("a\nb\nc", 4), 3);
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
