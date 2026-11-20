//! `read_file`, `read_files`, `write_file`, `edit_file` and `apply_patch`: the coder edits small, well-located things itself.
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

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use adam::prelude::*;
use adam_llm_agent::{keep_image, paging};
use adam_model::ImageOrigin;
use adam_workspace::safe_fs::{BeneathError, Meta, SlotDir, race};

use super::gitcli::GitAt;
use super::{Outcome, ToolEnv, non_empty, notes_error};

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

pub use adam_code_tools::confine::{Access, Confined, confine};
pub(super) use adam_code_tools::confine::{
    Located, is_dot_git, locate_for_write, open_for_read, parts_of,
};

/// Largest file `read_file` pages: counting its lines reads all of it, once.
const READ_MAX_FILE: u64 = 512 * 1024 * 1024;

/// What the model is told when `shown` is not there ([`closest_paths`] may add to it).
fn does_not_exist(shown: &str) -> String {
    format!("`{shown}` does not exist")
}

/// The `offset` and `limit` that `start_line` and `end_line`, the old names, and `offset` and
/// `limit` ask for together.
///
/// `start_line` is `offset`; `end_line` is `limit = end_line - start_line + 1`. `offset` and
/// `start_line` together are refused: which one counts would be a guess.
fn range_args(
    offset: Option<i64>,
    limit: Option<i64>,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> Result<(Option<i64>, Option<i64>), String> {
    if start_line == Some(0) || end_line == Some(0) {
        return Err("lines are numbered from 1: start_line and end_line must be at least 1".into());
    }
    if offset.is_some() && start_line.is_some() {
        return Err(
            "give offset or start_line, not both (start_line is the old name of offset)".into(),
        );
    }
    let offset = offset.or(start_line.map(i64::from));
    let limit = match end_line {
        None => limit,
        Some(end) => {
            let start = offset.unwrap_or(1);
            if start < 1 {
                return Err(
                    "end_line needs a first line from 1: give start_line, or use offset and limit"
                        .into(),
                );
            }
            let end = i64::from(end);
            if end < start {
                return Err(format!(
                    "end_line ({end}) is before start_line ({start}): the range is empty"
                ));
            }
            let from_end = end - start + 1;
            Some(limit.map_or(from_end, |l| l.min(from_end)))
        }
    };
    Ok((offset, limit))
}

/// A reader that ends with an error once `stop` is set: the read of a call that ran out of time.
struct StopRead<'a, R> {
    inner: R,
    stop: &'a AtomicBool,
}

impl<R: Read> Read for StopRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "out of time"));
        }
        self.inner.read(buf)
    }
}

/// One page of `rel` under `root`: its lines behind their numbers and the final line of
/// [`paging`](adam_llm_agent::paging) (see the tool).
///
/// A page is **one pass** over the file and keeps only the page: the final line gives the exact
/// number of lines left (or the file's total), so every line is counted; the lines outside the
/// page are counted as they go and never stored. A file is at most [`READ_MAX_FILE`].
#[cfg(test)]
fn read_in(
    root: &SlotDir,
    rel: &str,
    offset: Option<i64>,
    limit: Option<i64>,
) -> Result<String, String> {
    read_in_until(root, rel, offset, limit, &AtomicBool::new(false))
}

/// [`read_in`] that gives up when `stop` is set.
fn read_in_until(
    root: &SlotDir,
    rel: &str,
    offset: Option<i64>,
    limit: Option<i64>,
    stop: &AtomicBool,
) -> Result<String, String> {
    let (offset, limit) = paging::window_args(offset, limit, paging::PAGE_LINES)?;
    let (file, meta) = open_for_read(root, rel)?;
    page_open(file, meta, rel.trim(), offset, limit, stop)
}

/// The page of an open file: the checks a read makes before it pages, and the paging itself (one
/// pass, the page only: see [`read_in`]).
fn page_open(
    file: impl Read,
    meta: Meta,
    shown: &str,
    offset: i64,
    limit: u32,
    stop: &AtomicBool,
) -> Result<String, String> {
    if meta.is_dir() {
        return Err(format!(
            "`{shown}` is a directory: list it with list_files (`{{\"path\":\"{shown}\"}}`)"
        ));
    }
    if !meta.is_file() {
        return Err(format!("`{shown}` is not a regular file"));
    }
    if meta.size() > READ_MAX_FILE {
        return Err(format!(
            "`{shown}` is {} bytes, too big to read by lines (the limit is {READ_MAX_FILE}): \
             search it with search_files",
            meta.size()
        ));
    }
    let reader = io::BufReader::new(StopRead { inner: file, stop });
    match paging::page_lines(reader, offset, limit, true) {
        Ok(page) if page.total == 0 => Ok(format!("`{shown}` is empty\n{}", page.render())),
        Ok(page) => Ok(page.render()),
        Err(paging::PageError::PastEnd { offset, total }) => Err(format!(
            "`{shown}` has {total} lines: offset {offset} is past the end"
        )),
        Err(paging::PageError::Binary) => Err(binary_notice(shown, meta.size())),
        Err(paging::PageError::Io(e)) => Err(format!("cannot read `{shown}`: {e}")),
        Err(paging::PageError::Window(why)) => Err(why),
    }
}

/// The most bytes of a file read as an image: what `MODEL_IMAGE_MAX_BYTES` can be at most.
const IMAGE_READ_MAX: u64 = 4 * 1024 * 1024;

/// What a file that is an image gave.
enum ImageFile {
    Bytes(Vec<u8>),
    /// Its size, when it is over [`IMAGE_READ_MAX`]: nothing was read.
    TooBig(u64),
}

/// `rel` as an image, or `None` when it is not one (or cannot be read: the text read says why). The
/// type is the file's first bytes: PNG, JPEG, GIF or WebP, never an SVG, which is text. The path is
/// confined as every read is.
fn read_image_in(root: &SlotDir, rel: &str, stop: &AtomicBool) -> Option<ImageFile> {
    use std::io::Read as _;
    let (file, meta) = open_for_read(root, rel).ok()?;
    if !meta.is_file() || meta.size() < 8 {
        return None;
    }
    let mut file = StopRead { inner: file, stop };
    let mut head = [0u8; 16];
    let n = file.read(&mut head).ok()?;
    let kind = adam_runtime::sniff_image(&head[..n])?;
    if kind == "image/svg+xml" {
        return None;
    }
    if meta.size() > IMAGE_READ_MAX {
        return Some(ImageFile::TooBig(meta.size()));
    }
    let mut bytes = head[..n].to_vec();
    file.take(IMAGE_READ_MAX + 1 - n as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= IMAGE_READ_MAX).then_some(ImageFile::Bytes(bytes))
}

/// Offer the image at `path` of `slot` to the model: the result says it is one and the image
/// follows it, or says why it is not shown. `None` when the file is not an image.
async fn read_image(ctx: &ToolCtx, slot: &adam_workspace::Slot, path: &str) -> Option<ToolOutput> {
    let (root, rel) = (Arc::clone(slot.root()), path.to_owned());
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stop);
    let work = async {
        tokio::task::spawn_blocking(move || read_image_in(&root, &rel, &seen))
            .await
            .map_err(|e| format!("the read was interrupted: {e}"))
    };
    let read = super::find::within(super::find::DEADLINE, &stop, work)
        .await
        .ok()??;
    let name = path.rsplit('/').next().unwrap_or(path);
    let bytes = match read {
        ImageFile::Bytes(bytes) => bytes,
        ImageFile::TooBig(size) => {
            return Some(ToolOutput::text(format!(
                "[image not shown: `{path}` is {size} bytes, over the 4 MiB the most this \
                 deployment can send to the model]"
            )));
        }
    };
    let outcome = keep_image(ctx, &bytes, None, Some(name), ImageOrigin::Workspace).await;
    let said = match (&outcome.image, &outcome.stand_in) {
        (Some(image), _) => format!(
            "`{path}` is an image ({}, {}, {}\u{d7}{}), shown to you after this result",
            image.media_type,
            adam_model::human_bytes(image.bytes),
            image.width,
            image.height
        ),
        (None, Some(line)) => line.clone(),
        (None, None) => return None,
    };
    Some(ToolOutput::text(said).with_image(outcome))
}

fn binary_notice(shown: &str, len: u64) -> String {
    format!("`{shown}` is a binary file, {len} bytes, not shown")
}

/// Most paths [`closest_paths`] names.
const CLOSEST_PATHS: usize = 3;

/// Below this ([`similarity`], 0 to 1000) a path is not like the one asked for.
const CLOSEST_MIN: usize = 600;

/// A name asked for longer than this (in characters) is not matched against anything, and a
/// candidate longer than [`CLOSEST_CANDIDATE_CHARS`] is not considered: the likeness costs time by
/// the length of both.
const CLOSEST_WANTED_CHARS: usize = 256;
const CLOSEST_CANDIDATE_CHARS: usize = 512;

/// How often [`closest_paths`] looks at `stop`.
const CLOSEST_CHECK_EVERY: usize = 1024;

/// The paths of `candidates` (bytes: a name need not be UTF-8) that look most like `wanted`, best
/// first and then by path, so the same files give the same answer. Empty when `wanted` is longer than
/// [`CLOSEST_WANTED_CHARS`], and when `stop` is set: the caller has run out of time and there is no
/// suggestion to make.
fn closest_paths(wanted: &str, candidates: &[Vec<u8>], stop: &AtomicBool) -> Vec<String> {
    if wanted.chars().count() > CLOSEST_WANTED_CHARS {
        return Vec::new();
    }
    let wanted = wanted.trim();
    let mut wanted_pairs = Vec::new();
    bigrams(wanted, &mut wanted_pairs);
    let mut pairs = Vec::new();
    let mut scored: Vec<(usize, &[u8])> = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        if i % CLOSEST_CHECK_EVERY == 0 && stop.load(Ordering::Relaxed) {
            return Vec::new();
        }
        if candidate.len() > CLOSEST_CANDIDATE_CHARS * 4 {
            continue;
        }
        let shown = String::from_utf8_lossy(candidate);
        let shown = shown.trim();
        if shown.chars().count() > CLOSEST_CANDIDATE_CHARS {
            continue;
        }
        let score = if shown == wanted {
            1000
        } else {
            // Two pair lists can share at most the shorter one: the best score they could have.
            let (n, m) = (wanted_pairs.len(), shown.chars().count().saturating_sub(1));
            if n + m == 0 || 2 * n.min(m) * 1000 / (n + m) < CLOSEST_MIN {
                continue;
            }
            bigrams(shown, &mut pairs);
            dice(&wanted_pairs, &pairs)
        };
        if score >= CLOSEST_MIN {
            scored.push((score, candidate));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(CLOSEST_PATHS)
        .map(|(_, path)| paging::escape_name(path))
        .collect()
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
             apply_patch",
            content.len()
        ));
    }
    let located = locate_for_write(root, rel, true)?;
    write_located(&located, content)
}

/// The write of [`write_in`], to the directory [`locate_for_write`] found: a new file made in it
/// (`O_EXCL`, so nothing is followed), written, and renamed over the target.
fn write_located(located: &Located, content: &str) -> Result<Written, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let shown = located.shown();
    let Some(parent) = located.parent() else {
        return Err(format!("`{shown}` has no directory to be written in"));
    };
    race::checkpoint(located.slot(), "write");
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
        if let Some(meta) = located.existing() {
            file.set_permissions(fs::Permissions::from_mode(meta.permissions()))?;
        }
        drop(file);
        parent.rename(&tmp, located.name())
    })();
    if let Err(e) = written {
        let _ = parent.unlink(&tmp);
        return Err(format!("cannot write `{shown}`: {e}"));
    }
    Ok(Written {
        created: located.existing().is_none(),
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

/// What a change to a scratch project that was already published adds to the tool's result: the
/// change does not reach the repository ([`published_note`](super::scratch::published_note)).
pub(super) fn push_published_note(text: &mut String, slot: &adam_workspace::Slot) {
    if let Some(note) = super::scratch::published_note(slot) {
        text.push('\n');
        text.push_str(&note);
    }
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
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let (offset, limit) = match range_args(offset, limit, start_line, end_line) {
        Ok(window) => window,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let slot = match env.slot_to_read(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("read {path} ({})", slot.dir()))
        .await;
    // An image is offered to a model that can see it: shown after the result, or said in a line
    // when the deployment's limits keep it from the model. For a model that cannot see (or a
    // source the deployment does not send) the file is read as any other, and a binary one is
    // "not shown", as it always was.
    if ctx.sees_images_from(ImageOrigin::Workspace)
        && let Some(output) = read_image(ctx, &slot, path).await
    {
        return Ok(output);
    }
    let (root, rel) = (Arc::clone(slot.root()), path.to_owned());
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stop);
    let work = async {
        tokio::task::spawn_blocking(move || read_in_until(&root, &rel, offset, limit, &seen))
            .await
            .map_err(|e| format!("the read was interrupted: {e}"))
    };
    let read = match super::find::within(super::find::DEADLINE, &stop, work).await {
        Ok(read) => read,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    Ok(match read {
        // The first read below a directory that has instructions shows them, before the page.
        Ok(text) => {
            return super::project::with_nested(
                &env,
                ctx,
                &slot,
                Some(path),
                super::project::Target::File,
                text,
            )
            .await;
        }
        Err(reason) if reason == does_not_exist(path) => {
            let at = GitAt::slot(&slot);
            let wanted = path.to_owned();
            // The suggestion is a courtesy: it has what is left of the call's time, and none is
            // made when it runs out.
            let near = match super::gitcli::list_files(&at, Path::new("")).await {
                Ok(all) => {
                    let look = Arc::new(AtomicBool::new(false));
                    let seen = Arc::clone(&look);
                    let work = async {
                        tokio::task::spawn_blocking(move || closest_paths(&wanted, &all, &seen))
                            .await
                            .map_err(|e| e.to_string())
                    };
                    super::find::within(super::find::DEADLINE, &look, work)
                        .await
                        .unwrap_or_default()
                }
                Err(_) => Vec::new(),
            };
            let mut reason = reason;
            if !near.is_empty() {
                let near: Vec<String> = near.iter().map(|p| format!("`{p}`")).collect();
                reason.push_str(&format!(". Closest paths: {}", near.join(", ")));
            }
            ToolOutput::error(reason)
        }
        Err(reason) => ToolOutput::error(reason),
    })
}

/// The check epoch moves: a file the project **ignores** does not change the tree id, and a check
/// may read it, so a failed record from before this change is not reused
/// (`ChecksNotes::epoch`). A write to a file the project tracks changes the tree anyway; the bump
/// costs nothing there.
async fn moved_the_epoch(env: &ToolEnv, ctx: &ToolCtx) -> Result<(), ToolError> {
    let run = ctx.root_run_id().to_string();
    env.notes
        .update(&run, |notes| {
            notes.checks.epoch = notes.checks.epoch.saturating_add(1);
        })
        .await
        .map_err(|e| notes_error(&e))
}

// ------------------------------------------------------------------ read_files

/// Most paths one `read_files` call accepts.
pub const READ_FILES_MAX: usize = 16;
/// Most lines of one file's page in a `read_files` result.
pub const READ_FILES_LINES: u32 = 400;
/// Most bytes of one file's page in a `read_files` result.
pub const READ_FILES_PAGE_BYTES: usize = 8 * 1024;
/// Most bytes of the whole `read_files` result.
pub const READ_FILES_TOTAL_BYTES: usize = 32 * 1024;
/// Most bytes the escaped paths of one call may take together: they bound what the lines for the
/// files a result does not show cost, so the whole result stays under [`READ_FILES_TOTAL_BYTES`].
const READ_FILES_PATHS_BYTES: usize = 8 * 1024;
/// The room a nested project-instructions block may take (its own cap and its frame), reserved
/// before a file's page is shown so that a block that was claimed is always shown.
const BLOCK_ROOM: usize = super::project::NESTED_CAP + 4 * 1024;

/// Why one file of a `read_files` call is not shown.
enum Unreadable {
    /// A NUL byte in it.
    Binary,
    /// Bytes that are not UTF-8 (where `read_file` shows the replacement character).
    NotUtf8,
    /// It could not be read.
    Io(io::Error),
}

/// One file of a `read_files` call: its numbered rows, how many lines it has, the first line not
/// shown, and whether its last line has no line end.
struct Paged {
    rows: String,
    total: u64,
    next: Option<u64>,
    no_final_newline: bool,
}

/// Push the row of the line just read, when the page has room for it, and count it as shown.
/// Once a row does not fit, `full` is set and every later row is skipped: a page is contiguous, so
/// the line after the last one shown is the line the next page starts at.
fn push_row(
    rows: &mut String,
    shown: &mut u64,
    full: &mut bool,
    line: &[u8],
    chars: u64,
    number: u64,
) {
    if *full {
        return;
    }
    if *shown >= u64::from(READ_FILES_LINES) {
        *full = true;
        return;
    }
    let text: String = String::from_utf8_lossy(line)
        .chars()
        .take(paging::MAX_LINE_CHARS)
        .collect();
    let mut row = format!("{number:>6}\t{text}");
    if chars > paging::MAX_LINE_CHARS as u64 {
        row.push_str(&format!("… [line cut: {chars} chars]"));
    }
    row.push('\n');
    // The first row is always shown, as a page of `read_file` always has a line.
    if *shown > 0 && rows.len() + row.len() > READ_FILES_PAGE_BYTES {
        *full = true;
        return;
    }
    rows.push_str(&row);
    *shown += 1;
}

/// The page of `reader`: the first [`READ_FILES_LINES`] lines (at most [`READ_FILES_PAGE_BYTES`]
/// of rows) behind the numbers and tabs `read_file` uses, and how many lines the file has.
///
/// The whole stream is read **once**, to count its lines and to check every byte: a NUL is
/// [`Unreadable::Binary`] and a byte that is not UTF-8 is [`Unreadable::NotUtf8`]; only the page's
/// rows are kept. A line over [`paging::MAX_LINE_CHARS`] characters is cut and says how long it
/// was, as `read_file` cuts one.
fn page_in(mut reader: impl io::BufRead, stop: &AtomicBool) -> Result<Paged, Unreadable> {
    let mut rows = String::new();
    let mut total: u64 = 0;
    let mut shown: u64 = 0;
    let mut full = false;
    let mut line: Vec<u8> = Vec::new();
    let mut chars: u64 = 0;
    let mut ends_with_newline = true;
    let mut carry: Vec<u8> = Vec::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Unreadable::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "out of time",
            )));
        }
        let buf = reader.fill_buf().map_err(Unreadable::Io)?;
        if buf.is_empty() {
            break;
        }
        // UTF-8 across the chunks: the bytes of a sequence the end of a chunk split wait in
        // `carry` for the next one.
        let mut whole = std::mem::take(&mut carry);
        whole.extend_from_slice(buf);
        match std::str::from_utf8(&whole) {
            Ok(_) => {}
            Err(e) if e.error_len().is_none() => carry = whole[e.valid_up_to()..].to_vec(),
            Err(_) => return Err(Unreadable::NotUtf8),
        }
        if buf.contains(&0) {
            return Err(Unreadable::Binary);
        }
        for &byte in buf {
            if byte == b'\n' {
                total += 1;
                ends_with_newline = true;
                push_row(&mut rows, &mut shown, &mut full, &line, chars, total);
                line.clear();
                chars = 0;
            } else {
                chars += u64::from(byte & 0xC0 != 0x80);
                if line.len() < paging::MAX_LINE_CHARS * 4 {
                    line.push(byte);
                }
            }
        }
        let used = buf.len();
        reader.consume(used);
    }
    if !carry.is_empty() {
        return Err(Unreadable::NotUtf8);
    }
    if !line.is_empty() || chars > 0 {
        total += 1;
        ends_with_newline = false;
        push_row(&mut rows, &mut shown, &mut full, &line, chars, total);
    }
    Ok(Paged {
        rows,
        total,
        next: (shown < total).then(|| shown + 1),
        no_final_newline: total > 0 && !ends_with_newline,
    })
}

/// Why a path could not be resolved for a `read_files` call, as a phrase that follows the path.
fn why_not(e: BeneathError) -> String {
    match e {
        BeneathError::Escapes => {
            "leads outside the worktree (through a symlink): refused".to_owned()
        }
        BeneathError::Avoided(_) => "leads into .git (through a symlink): refused".to_owned(),
        BeneathError::TooManyLinks => {
            "cannot be resolved: too many levels of symbolic links".to_owned()
        }
        BeneathError::Changed => {
            "changed while it was opened (a symlink is on its way now): refused".to_owned()
        }
        BeneathError::Io(e) if e.kind() == io::ErrorKind::NotFound => "does not exist".to_owned(),
        BeneathError::Io(e) => format!("cannot be resolved: {e}"),
    }
}

/// One path of a `read_files` call.
enum One {
    /// Its page.
    Page(Paged),
    /// The one line the result gives for it.
    Refused(String),
}

/// Read `rel` of the slot `root` for a `read_files` call: its page, or the one line that says why
/// it is not shown (a path that is not there, is not text, is over the read cap or leaves the
/// slot: never a failure of the whole call).
fn read_one(root: &SlotDir, rel: &str, stop: &AtomicBool) -> One {
    let shown = rel.trim();
    // The name starts the one line a refused path gets, so a leading `[` is escaped too.
    let name = paging::escape_item_name(shown.as_bytes());
    let parts = match parts_of(rel) {
        Ok(parts) => parts,
        // The message names the path already (`\`../a\` goes up with `..`), and the path is the
        // model's: its control characters are escaped so the refusal stays one line.
        Err(line) => return One::Refused(paging::escape_text(line.as_bytes())),
    };
    let path: PathBuf = parts.iter().collect();
    let (file, meta) = match root.open_following(&path, &is_dot_git) {
        Ok(opened) => opened,
        Err(e) => return One::Refused(format!("{name}: {}", why_not(e))),
    };
    if meta.is_dir() {
        return One::Refused(format!("{name}: is a directory: list it with list_files"));
    }
    if !meta.is_file() {
        return One::Refused(format!("{name}: is not a regular file"));
    }
    if meta.size() > READ_MAX_FILE {
        return One::Refused(format!(
            "{name}: is {} bytes, over the {READ_MAX_FILE} a read opens: search it with search_files",
            meta.size()
        ));
    }
    let reader = io::BufReader::new(StopRead { inner: file, stop });
    match page_in(reader, stop) {
        Ok(page) => One::Page(page),
        Err(Unreadable::Binary) => One::Refused(format!(
            "{name}: is a binary file, {} bytes, not shown",
            meta.size()
        )),
        Err(Unreadable::NotUtf8) => One::Refused(format!("{name}: is not UTF-8 text, not shown")),
        Err(Unreadable::Io(e)) => One::Refused(format!("{name}: cannot be read: {e}")),
    }
}

/// The section of one file in a `read_files` result: the line that names it, its rows, and the line
/// that says what is left (or that it is the end). It ends with one line end.
fn section(name: &str, page: &Paged) -> String {
    let mut out = format!("== {name} ==\n");
    out.push_str(&page.rows);
    match page.next {
        Some(next) => out.push_str(&format!(
            "[more: {} lines left; next page: read_file path={name} offset={next}]\n",
            page.total - next + 1
        )),
        None if page.total == 0 => out.push_str("[end: 0 lines]\n"),
        None if page.no_final_newline => out.push_str(&format!(
            "[end: {} lines; no newline at the end of the file]\n",
            page.total
        )),
        None => out.push_str(&format!("[end: {} lines]\n", page.total)),
    }
    out
}

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
    if paths.is_empty() || paths.len() > READ_FILES_MAX {
        return Ok(ToolOutput::error(format!(
            "paths must be 1 to {READ_FILES_MAX} paths: give the files to read in one call"
        )));
    }
    let escaped: Vec<String> = paths
        .iter()
        .map(|path| paging::escape_name(path.trim().as_bytes()))
        .collect();
    let named: usize = escaped.iter().map(|path| path.len() + 1).sum();
    if named > READ_FILES_PATHS_BYTES {
        return Ok(ToolOutput::error(format!(
            "the paths are {named} bytes together, over the {READ_FILES_PATHS_BYTES} one call may \
             name: split it into two calls"
        )));
    }
    let slot = match env.slot_to_read(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("read {} files ({})", paths.len(), slot.dir()))
        .await;
    let root = Arc::clone(slot.root());
    let wanted: Vec<String> = paths.iter().map(|path| path.trim().to_owned()).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stop);
    let reading = wanted.clone();
    let work = async {
        tokio::task::spawn_blocking(move || {
            reading
                .iter()
                .map(|path| read_one(&root, path, &seen))
                .collect::<Vec<One>>()
        })
        .await
        .map_err(|e| format!("the read was interrupted: {e}"))
    };
    let read = match super::find::within(super::find::DEADLINE, &stop, work).await {
        Ok(read) => read,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    // The one line a result that has no room for a file gives for it.
    let fallback = |at: usize| -> String {
        match &read[at] {
            One::Refused(line) => format!("{line}\n"),
            One::Page(_) => format!(
                "[not shown: read it alone with read_file path={}]\n",
                escaped[at]
            ),
        }
    };
    let mut out = String::new();
    // The blocks shown by this call: the same call id shows a directory's file again (a retry),
    // and a call that reads two files of one directory asks for it twice.
    let mut shown: Vec<String> = Vec::new();
    for at in 0..read.len() {
        // What the files after this one cost when the result has no room for them.
        let rest: usize = (at + 1..read.len()).map(|i| fallback(i).len() + 1).sum();
        let alone = fallback(at);
        let mut piece = String::new();
        match &read[at] {
            One::Refused(_) => piece.push_str(&alone),
            One::Page(page) => {
                let page = section(&escaped[at], page);
                let gap = out.len() + usize::from(!out.is_empty());
                let room = READ_FILES_TOTAL_BYTES.saturating_sub(gap + rest);
                if page.len() + BLOCK_ROOM <= room {
                    // Room for a block: claim it. A block that was claimed is always shown.
                    match super::project::nested(
                        &env,
                        ctx,
                        &slot,
                        Some(&wanted[at]),
                        super::project::Target::File,
                    )
                    .await?
                    {
                        Some(block) if !shown.contains(&block) => {
                            shown.push(block.clone());
                            piece.push_str(&block);
                            piece.push_str("\n\n");
                            piece.push_str(&page);
                        }
                        _ => piece.push_str(&page),
                    }
                } else if page.len() <= room {
                    piece.push_str(&page);
                } else {
                    piece.push_str(&alone);
                }
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&piece);
    }
    if out.ends_with('\n') {
        out.pop();
    }
    Ok(ToolOutput::text(out))
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
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let slot = match env.slot_to_write(ctx, repo.as_deref()).await {
        Ok(slot) => slot,
        Err(outcome) => return outcome,
    };
    let (root, rel) = (Arc::clone(slot.root()), path.to_owned());
    let written = tokio::task::spawn_blocking(move || write_in(&root, &rel, &content))
        .await
        .map_err(|e| ToolError::Transient(format!("the write was interrupted: {e}")))?;
    match written {
        Ok(done) => {
            moved_the_epoch(&env, ctx).await?;
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
            moved_the_epoch(&env, ctx).await?;
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
    let located = locate_for_write(root, rel, false)?;
    race::checkpoint(root.path(), "edit");
    let (Some(parent), Some(meta)) = (located.parent(), located.existing().copied()) else {
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
            "`{shown}` is {} bytes, over the {WRITE_CAP} bytes edit_file works on: use apply_patch",
            meta.size()
        ));
    }
    let (file, _) = parent
        .open_regular(located.name())
        .map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    let mut bytes = Vec::new();
    file.take(WRITE_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read `{shown}`: {e}"))?;
    if bytes.len() > WRITE_CAP {
        return Err(format!(
            "`{shown}` is over the {WRITE_CAP} bytes edit_file works on: use apply_patch"
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

    let exact: Vec<usize> = content.match_indices(old).map(|(at, _)| at).collect();
    let (edited, lines) = if exact.is_empty() {
        // The exact match is the rule; the tolerance is the one fallback that keeps a model from
        // spending a turn on a line ending or a blank it copied wrong. Leading whitespace is never
        // ignored: an indentation is semantic.
        let Some(places) = normalized_places(&content, old) else {
            return Err(not_found(shown, &content, old));
        };
        if places.len() > 1 && !replace_all {
            return Err(not_found(shown, &content, old));
        }
        let lines: Vec<usize> = places.iter().map(|(_, _, line)| *line).collect();
        let spans: Vec<(usize, usize)> = places.iter().map(|(at, end, _)| (*at, *end)).collect();
        (replace_spans(&content, &spans, new), lines)
    } else {
        let lines: Vec<usize> = exact.iter().map(|at| line_of(&content, *at)).collect();
        match exact.len() {
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
        (edited, lines)
    };
    let written = write_located(&located, &edited)?;
    Ok(Edited {
        replaced: lines.len(),
        lines,
        bytes: written.bytes,
    })
}

/// The 1-based line of the byte offset `at` of `text`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at].bytes().filter(|b| *b == b'\n').count() + 1
}

/// `text` with `\r\n` and `\r` read as `\n`, and the spaces and tabs at the end of every line
/// dropped, and where each byte of the result came from: `spans[i]` is the byte range of `text`
/// the result's byte `i` covers, one byte or one line ending. The blanks dropped at the end of a
/// line are in no range, so a match that stops at the last character of a line leaves them.
fn normalized(text: &str) -> (String, Vec<(usize, usize)>) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut spans = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let mut end = at;
        while end < bytes.len() && bytes[end] != b'\r' && bytes[end] != b'\n' {
            end += 1;
        }
        let mut kept = end;
        while kept > at && (bytes[kept - 1] == b' ' || bytes[kept - 1] == b'\t') {
            kept -= 1;
        }
        spans.extend((at..kept).map(|byte| (byte, byte + 1)));
        out.push_str(&text[at..kept]);
        if end < bytes.len() {
            let line_end = if bytes[end] == b'\r' && bytes.get(end + 1) == Some(&b'\n') {
                end + 2
            } else {
                end + 1
            };
            spans.push((end, line_end));
            out.push('\n');
            at = line_end;
        } else {
            at = end;
        }
    }
    (out, spans)
}

/// The places `old` is in `content` once [`normalized`]: each as the byte span of `content` the
/// match covers and the 1-based line of the normalized text it starts on. `None` when nothing
/// matches, or when `old` is only blanks (which would match everywhere).
fn normalized_places(content: &str, old: &str) -> Option<Vec<(usize, usize, usize)>> {
    let (needle, _) = normalized(old);
    if needle.is_empty() {
        return None;
    }
    let (haystack, spans) = normalized(content);
    let places: Vec<(usize, usize, usize)> = haystack
        .match_indices(needle.as_str())
        .map(|(at, _)| {
            let (start, _) = spans[at];
            let (_, end) = spans[at + needle.len() - 1];
            (start, end, line_of(&haystack, at))
        })
        .collect();
    (!places.is_empty()).then_some(places)
}

/// `content` with each byte span of `places` (in order, not overlapping) replaced by `new`.
fn replace_spans(content: &str, places: &[(usize, usize)], new: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut at = 0;
    for (start, end) in places {
        out.push_str(&content[at..*start]);
        out.push_str(new);
        at = *end;
    }
    out.push_str(&content[at..]);
    out
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
    let (mut left, mut right) = (Vec::new(), Vec::new());
    bigrams(a, &mut left);
    bigrams(b, &mut right);
    dice(&left, &right)
}

/// The sorted character pairs of `s`, each packed in a `u64`, in `out` (emptied first).
fn bigrams(s: &str, out: &mut Vec<u64>) {
    out.clear();
    let mut chars = s.chars();
    if let Some(mut before) = chars.next() {
        for c in chars {
            out.push((u64::from(u32::from(before)) << 32) | u64::from(u32::from(c)));
            before = c;
        }
    }
    out.sort_unstable();
}

/// The Sorensen-Dice coefficient of two sorted lists of pairs, from 0 to 1000; two lists with no
/// pair at all are not alike (a single character each: alike only if equal, which callers took).
fn dice(left: &[u64], right: &[u64]) -> usize {
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    // The pairs the two have in common, as a multiset: a merge of the two sorted lists.
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2 * common * 1000 / (left.len() + right.len())
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
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let slot = match env.slot_to_write(ctx, repo.as_deref()).await {
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
            moved_the_epoch(&env, ctx).await?;
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

    fn read_in(
        root: &Path,
        rel: &str,
        offset: Option<i64>,
        limit: Option<i64>,
    ) -> Result<String, String> {
        super::read_in(&open(root)?, rel, offset, limit)
    }

    /// What the tool does with its arguments: the aliases first, then the page.
    fn read_with(
        root: &Path,
        rel: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> Result<String, String> {
        let (offset, limit) = range_args(None, None, start_line, end_line)?;
        read_in(root, rel, offset, limit)
    }

    /// A file of `n` numbered lines, `line 1` ... `line n`.
    fn numbered_file(dir: &Path, name: &str, n: usize) {
        let text: String = (1..=n).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.join(name), text).unwrap();
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

    /// A file that is open while `write_file` or `edit_file` replaces it keeps the old content whole
    /// and the path names a new file with the new content whole: the tools write a file of their own and
    /// rename it over the old, never into the old one. That is what lets a read run beside a write
    /// (ADR 0049). The check is made on an open handle, so it does not depend on timing; the control
    /// shows that it fails for a write made in place.
    #[test]
    fn a_write_or_an_edit_replaces_the_file_and_never_changes_the_one_a_read_holds() {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt as _;
        let dir = root();
        let path = dir.path().join("src/shared.txt");
        let (old, new) = ("alpha\n".repeat(2_000), "bravo\n".repeat(2_000));
        let open_old = |text: &str| {
            fs::write(&path, text).unwrap();
            (
                fs::File::open(&path).unwrap(),
                fs::metadata(&path).unwrap().ino(),
            )
        };
        let seen = |mut held: fs::File| {
            let mut text = String::new();
            held.read_to_string(&mut text).unwrap();
            text
        };

        // The control: a write in place changes what the open handle sees.
        let (held, _) = open_old(&old);
        fs::write(&path, &new).unwrap();
        assert_eq!(seen(held), new, "an in-place write reaches the open handle");

        // `write_file`.
        let (held, ino) = open_old(&old);
        write_in(dir.path(), "src/shared.txt", &new).unwrap();
        assert_eq!(seen(held), old, "the open file is untouched");
        assert_eq!(fs::read_to_string(&path).unwrap(), new);
        assert_ne!(fs::metadata(&path).unwrap().ino(), ino, "a new file");

        // `edit_file`.
        let (held, ino) = open_old(&old);
        edit_in(dir.path(), "src/shared.txt", "alpha", "bravo", true).unwrap();
        assert_eq!(seen(held), old, "the open file is untouched");
        assert_eq!(fs::read_to_string(&path).unwrap(), new);
        assert_ne!(fs::metadata(&path).unwrap().ino(), ino, "a new file");
    }

    /// The same, as readers and a writer that run at the same time: no read is half of each.
    #[test]
    fn a_read_beside_a_write_or_an_edit_sees_a_whole_file() {
        let dir = root();
        let path = dir.path().join("src/shared.txt");
        let (a, b) = ("alpha\n".repeat(1_000), "bravo\n".repeat(1_000));
        fs::write(&path, &a).unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 0..10 {
                    let text = if i % 2 == 0 { &b } else { &a };
                    write_in(dir.path(), "src/shared.txt", text).unwrap();
                    // An edit rewrites the whole file too.
                    let (from, to) = if i % 2 == 0 {
                        ("bravo", "alpha")
                    } else {
                        ("alpha", "bravo")
                    };
                    edit_in(dir.path(), "src/shared.txt", from, to, true).unwrap();
                }
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
            });
            let mut reads = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) || reads < 20 {
                let text = fs::read_to_string(&path).unwrap();
                assert_eq!(text.len(), 6_000, "a partial file");
                assert!(text == a || text == b, "a file that is half of each");
                reads += 1;
            }
        });
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
    fn a_small_file_is_one_page_with_its_end_said() {
        let dir = root();
        assert_eq!(
            read_in(dir.path(), "src/main.rs", None, None).unwrap(),
            "     1\tfn main() {}\n[end: 1 lines]"
        );
        fs::write(dir.path().join("empty"), "").unwrap();
        assert_eq!(
            read_in(dir.path(), "empty", None, None).unwrap(),
            "`empty` is empty\n[end: 0 lines]"
        );
    }

    #[test]
    fn a_file_without_a_range_is_paged_at_2000_lines() {
        let dir = root();
        numbered_file(dir.path(), "big.txt", 5000);
        let text = read_in(dir.path(), "big.txt", None, None).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2001, "2000 numbered lines and the final line");
        assert_eq!(lines[0], "     1\tline 1");
        assert_eq!(lines[1999], "  2000\tline 2000");
        assert_eq!(
            lines[2000],
            "[more: 3000 lines left; next page: offset=2001]"
        );
        let next = read_in(dir.path(), "big.txt", Some(2001), None).unwrap();
        assert!(next.starts_with("  2001\tline 2001\n"), "{next}");
        let last = read_in(dir.path(), "big.txt", Some(4001), None).unwrap();
        assert!(last.ends_with("[end: 5000 lines]"), "{last}");
    }

    /// A reader that counts the bytes it gives out: a page must read the file once.
    struct Counted<'a, R> {
        inner: R,
        bytes: &'a std::cell::Cell<u64>,
    }

    impl<R: Read> Read for Counted<'_, R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes.set(self.bytes.get() + n as u64);
            Ok(n)
        }
    }

    /// A page of a 20 MB file is **one pass**, whatever the offset: the exact `[more]`/`[end]` line
    /// needs every line counted, so the whole file is read, but only the page is kept and no byte
    /// is read twice.
    #[test]
    fn a_page_reads_the_file_once_whatever_the_offset() {
        let dir = root();
        let lines = 200_000usize;
        let text: String = (1..=lines).map(|i| format!("{i:099}\n")).collect();
        fs::write(dir.path().join("big.txt"), &text).unwrap();
        let size = fs::metadata(dir.path().join("big.txt")).unwrap().len();
        assert_eq!(size, 20_000_000, "20 MB of 100-byte lines");
        let slot = open(dir.path()).unwrap();
        for (offset, rows, first, last) in [
            (
                1i64,
                478usize,
                1u64,
                "[more: 199522 lines left; next page: offset=479]",
            ),
            (
                100_001,
                478,
                100_001,
                "[more: 99522 lines left; next page: offset=100479]",
            ),
            (199_600, 401, 199_600, "[end: 200000 lines]"),
            (-2, 2, 199_999, "[end: 200000 lines]"),
        ] {
            let (file, meta) = open_for_read(&slot, "big.txt").unwrap();
            let bytes = std::cell::Cell::new(0);
            let counted = Counted {
                inner: file,
                bytes: &bytes,
            };
            let page = page_open(
                counted,
                meta,
                "big.txt",
                offset,
                paging::PAGE_LINES,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(
                bytes.get(),
                size,
                "offset {offset}: the file is read once, not twice"
            );
            assert_eq!(
                page.lines().count(),
                rows + 1,
                "offset {offset}: the page and its last line"
            );
            assert_eq!(page.rsplit('\n').next().unwrap(), last, "offset {offset}");
            assert!(
                page.starts_with(&format!("{first:>6}\t{first:099}\n")),
                "offset {offset}: the page starts at the line it was asked for"
            );
        }
    }

    #[test]
    fn a_negative_offset_reads_the_end() {
        let dir = root();
        numbered_file(dir.path(), "big.txt", 5000);
        let text = read_in(dir.path(), "big.txt", Some(-3), None).unwrap();
        assert_eq!(
            text,
            "  4998\tline 4998\n  4999\tline 4999\n  5000\tline 5000\n[end: 5000 lines]"
        );
    }

    #[test]
    fn the_byte_cap_ends_a_page_before_2000_lines() {
        let dir = root();
        let line = format!("{}\n", "x".repeat(99));
        fs::write(dir.path().join("wide.txt"), line.repeat(1000)).unwrap();
        let text = read_in(dir.path(), "wide.txt", None, None).unwrap();
        let shown = text.lines().count() - 1;
        // 50 KiB of rendered lines of 107 bytes each.
        assert_eq!(shown, 50 * 1024 / 107, "{shown}");
        assert!(
            text.ends_with(&format!(
                "[more: {} lines left; next page: offset={}]",
                1000 - shown,
                shown + 1
            )),
            "{}",
            &text[text.len() - 80..]
        );
    }

    #[test]
    fn a_long_line_is_cut_and_says_how_long() {
        let dir = root();
        fs::write(
            dir.path().join("long.txt"),
            format!("short\n{}\nafter\n", "é".repeat(18_234)),
        )
        .unwrap();
        let text = read_in(dir.path(), "long.txt", None, None).unwrap();
        let second = text.lines().nth(1).unwrap();
        assert!(second.ends_with("… [line cut: 18234 chars]"), "{second}");
        assert_eq!(
            second.chars().filter(|c| *c == 'é').count(),
            paging::MAX_LINE_CHARS
        );
        assert!(
            text.contains("     3\tafter\n"),
            "the next line is still there"
        );
    }

    #[test]
    fn start_line_and_end_line_still_work() {
        let dir = root();
        fs::write(dir.path().join("f.txt"), "one\ntwo\r\nthree\nfour").unwrap();
        assert_eq!(
            read_with(dir.path(), "f.txt", Some(2), Some(3)).unwrap(),
            "     2\ttwo\r\n     3\tthree\n[more: 1 lines left; next page: offset=4]"
        );
        assert_eq!(
            read_with(dir.path(), "f.txt", Some(3), None).unwrap(),
            "     3\tthree\n     4\tfour\n[end: 4 lines; no newline at the end of the file]"
        );
        assert_eq!(
            read_with(dir.path(), "f.txt", None, Some(1)).unwrap(),
            "     1\tone\n[more: 3 lines left; next page: offset=2]"
        );
        assert!(
            read_with(dir.path(), "f.txt", Some(4), Some(99))
                .unwrap()
                .starts_with("     4\tfour\n")
        );
        assert!(
            read_with(dir.path(), "f.txt", Some(9), None)
                .unwrap_err()
                .contains("has 4 lines")
        );
        assert!(
            read_with(dir.path(), "f.txt", Some(3), Some(2))
                .unwrap_err()
                .contains("before start_line")
        );
        assert!(
            read_with(dir.path(), "f.txt", Some(0), None)
                .unwrap_err()
                .contains("from 1")
        );
        assert!(
            read_in(dir.path(), "f.txt", Some(0), None)
                .unwrap_err()
                .contains("counts from 1")
        );
        assert!(
            read_in(dir.path(), "f.txt", None, Some(0))
                .unwrap_err()
                .contains("limit")
        );
    }

    #[test]
    fn offset_with_start_line_is_refused() {
        let why = range_args(Some(3), None, Some(2), None).unwrap_err();
        assert!(why.contains("not both"), "{why}");
        // Either alone is fine, and `limit` and `end_line` meet at the smaller.
        assert_eq!(
            range_args(Some(3), Some(10), None, None),
            Ok((Some(3), Some(10)))
        );
        assert_eq!(
            range_args(None, Some(10), Some(2), Some(4)),
            Ok((Some(2), Some(3)))
        );
    }

    #[test]
    fn no_trailing_newline_is_said_at_the_end() {
        let dir = root();
        fs::write(dir.path().join("a.txt"), "one\ntwo").unwrap();
        assert_eq!(
            read_in(dir.path(), "a.txt", None, None).unwrap(),
            "     1\tone\n     2\ttwo\n[end: 2 lines; no newline at the end of the file]"
        );
    }

    #[test]
    fn closest_paths_rank_by_likeness_then_by_path() {
        let all: Vec<Vec<u8>> = ["src/main.rs", "README.md", "src/mian.rb", "src/mian.rc"]
            .into_iter()
            .map(|p| p.as_bytes().to_vec())
            .collect();
        let near = closest_paths("src/mian.rs", &all, &AtomicBool::new(false));
        // The two that differ by one letter tie, and go by path; the third is further.
        assert_eq!(
            near,
            ["src/mian.rb", "src/mian.rc", "src/main.rs"],
            "{near:?}"
        );
        assert!(!near.contains(&"README.md".to_owned()), "{near:?}");
        assert_eq!(
            near,
            closest_paths("src/mian.rs", &all, &AtomicBool::new(false)),
            "the same answer twice"
        );
    }

    #[test]
    fn closest_paths_over_a_big_tree_is_quick_and_bounded() {
        let all: Vec<Vec<u8>> = (0..200_000)
            .map(|i| format!("crates/module-{:03}/src/part-{i:06}/mod.rs", i % 500).into_bytes())
            .collect();
        let started = std::time::Instant::now();
        let near = closest_paths(
            "crates/module-001/src/part-000001/mod.rc",
            &all,
            &AtomicBool::new(false),
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(near.len(), CLOSEST_PATHS, "{near:?}");
        // A name longer than the bound is matched against nothing; one candidate over it is skipped.
        let long = "a/".repeat(CLOSEST_WANTED_CHARS);
        assert!(closest_paths(&long, &all, &AtomicBool::new(false)).is_empty());
        let big = vec!["x/".repeat(CLOSEST_CANDIDATE_CHARS).into_bytes()];
        assert!(closest_paths(&"x/".repeat(100), &big, &AtomicBool::new(false)).is_empty());
        // Out of time: no suggestion, and it stops looking.
        assert!(
            closest_paths(
                "crates/module-001/src/part-000001/mod.rc",
                &all,
                &AtomicBool::new(true)
            )
            .is_empty()
        );
    }

    #[test]
    fn a_read_that_is_out_of_time_is_an_error_not_a_page() {
        let dir = root();
        numbered_file(dir.path(), "big.txt", 10_000);
        let stop = AtomicBool::new(true);
        let why = super::read_in_until(&open(dir.path()).unwrap(), "big.txt", None, None, &stop)
            .unwrap_err();
        assert!(why.contains("cannot read"), "{why}");
        assert!(
            super::read_in_until(
                &open(dir.path()).unwrap(),
                "big.txt",
                None,
                None,
                &AtomicBool::new(false)
            )
            .is_ok()
        );
    }

    /// The text of a file as `read_file` pages show it, with the numbers taken off: following
    /// `offset` to the end rebuilds the file.
    fn reassembled(dir: &Path, name: &str, limit: i64) -> String {
        let mut out = String::new();
        let mut offset = 1;
        loop {
            let page = read_in(dir, name, Some(offset), Some(limit)).unwrap();
            let mut parts: Vec<&str> = page.split('\n').collect();
            let last = parts.pop().unwrap();
            for line in parts {
                out.push_str(line.split_once('\t').unwrap().1);
                out.push('\n');
            }
            if let Some(rest) = last.strip_prefix("[more: ") {
                offset = rest
                    .rsplit_once("offset=")
                    .unwrap()
                    .1
                    .trim_end_matches(']')
                    .parse()
                    .unwrap();
            } else {
                if last.contains("no newline at the end") {
                    assert!(out.ends_with('\n'));
                    out.pop();
                }
                return out;
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// Following `offset` until `[end` and taking the numbers off gives the file back, byte for
        /// byte, the missing last newline included.
        #[test]
        fn prop_paging_reassembles_the_file(
            lines in proptest::collection::vec("[ -~é\u{4e2d}\t]{0,40}", 0..60),
            last_newline in proptest::bool::ANY,
            limit in 1i64..=2000,
        ) {
            let dir = root();
            let mut text = lines.join("\n");
            if last_newline && !lines.is_empty() {
                text.push('\n');
            }
            // A file of nothing, or of one empty line without its end, is an empty file.
            proptest::prop_assume!(!text.is_empty());
            fs::write(dir.path().join("p.txt"), &text).unwrap();
            proptest::prop_assert_eq!(reassembled(dir.path(), "p.txt", limit), text);
        }

        /// The same call gives the same bytes, whatever the page.
        #[test]
        fn prop_same_call_same_bytes(
            n in 1usize..300,
            offset in -300i64..300,
            limit in proptest::option::of(1i64..=2000),
        ) {
            proptest::prop_assume!(offset != 0);
            let dir = root();
            numbered_file(dir.path(), "p.txt", n);
            let first = read_in(dir.path(), "p.txt", Some(offset), limit);
            let second = read_in(dir.path(), "p.txt", Some(offset), limit);
            proptest::prop_assert_eq!(first, second);
        }
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
    fn a_directory_is_listed_with_list_files_not_read() {
        let dir = root();
        assert!(
            read_in(dir.path(), "src", None, None)
                .unwrap_err()
                .contains("list_files")
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
    fn a_text_nothing_is_like_says_so() {
        let dir = with_main();
        let why = edit(dir.path(), "zzzzzzzz qqqqqqqq", "x", false).unwrap_err();
        assert!(why.contains("Nothing in the file is like it"), "{why}");
    }

    /// The tolerance: a line ending or trailing blanks that differ are not a miss. The edit is
    /// applied to the original bytes at the matched span, so the lines around it keep their own
    /// endings and blanks.
    #[test]
    fn a_crlf_file_is_edited_by_an_old_with_lf_and_keeps_its_other_endings() {
        let dir = root();
        fs::write(dir.path().join("mixed.txt"), "alpha\r\nbeta\r\ngamma\r\n").unwrap();
        let done = edit_in(dir.path(), "mixed.txt", "beta\ngamma", "BETA\nGAMMA", false).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (1, vec![2]));
        assert_eq!(
            fs::read_to_string(dir.path().join("mixed.txt")).unwrap(),
            "alpha\r\nBETA\nGAMMA\r\n",
            "the replaced span has new's bytes, the rest keeps CRLF"
        );
    }

    #[test]
    fn trailing_blanks_on_a_line_do_not_hide_it() {
        let dir = root();
        fs::write(dir.path().join("blanks.txt"), "one\ntwo   \nthree\n").unwrap();
        let done = edit_in(dir.path(), "blanks.txt", "two\nthree", "2\n3", false).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (1, vec![2]));
        assert_eq!(
            fs::read_to_string(dir.path().join("blanks.txt")).unwrap(),
            "one\n2\n3\n"
        );
    }

    /// The tolerance is on both sides: an `old` copied with CRLF endings or with trailing blanks
    /// the file does not have is normalized too.
    #[test]
    fn an_old_with_crlf_or_trailing_blanks_is_normalized_too() {
        let dir = root();
        fs::write(dir.path().join("old.txt"), "one\ntwo\nthree\n").unwrap();
        let done = edit_in(dir.path(), "old.txt", "one\r\ntwo  ", "1\n2", false).unwrap();
        assert_eq!((done.replaced, done.lines), (1, vec![1]));
        assert_eq!(
            fs::read_to_string(dir.path().join("old.txt")).unwrap(),
            "1\n2\nthree\n"
        );
    }

    #[test]
    fn a_normalized_match_in_two_places_is_refused_as_a_miss_and_changes_nothing() {
        let dir = root();
        let body = "foo   \nbar\nfoo\t\n";
        fs::write(dir.path().join("twice.txt"), body).unwrap();
        let why = edit_in(dir.path(), "twice.txt", "foo\n", "x\n", false).unwrap_err();
        assert!(
            why.contains("was not found") && why.contains("nothing was changed"),
            "{why}"
        );
        assert!(why.contains("closest region is lines "), "{why}");
        assert!(
            why.contains(">     1\t") || why.contains(">     3\t"),
            "the region is numbered: {why}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("twice.txt")).unwrap(),
            body,
            "nothing changed"
        );
        // `replace_all` takes every normalized place, each at its own span.
        let done = edit_in(dir.path(), "twice.txt", "foo\n", "x\n", true).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (2, vec![1, 3]));
        assert_eq!(
            fs::read_to_string(dir.path().join("twice.txt")).unwrap(),
            "x\nbar\nx\n"
        );
    }

    #[test]
    fn a_leading_indentation_that_differs_is_still_a_miss() {
        let dir = root();
        let body = "fn main() {\n    let a = 1;\n}\n";
        fs::write(dir.path().join("indent.rs"), body).unwrap();
        let why = edit_in(
            dir.path(),
            "indent.rs",
            "fn main() {\n  let a = 1;\n}\n",
            "x",
            false,
        )
        .unwrap_err();
        assert!(
            why.contains("was not found") && why.contains("nothing was changed"),
            "{why}"
        );
        assert!(
            why.contains("First difference, at line 2: only the blanks differ"),
            "{why}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("indent.rs")).unwrap(),
            body,
            "leading whitespace is not guessed"
        );
    }

    /// The exact match is tried first: `foo\n` is in the file once exactly and twice once the
    /// trailing blanks are ignored. The exact place is the one replaced.
    #[test]
    fn an_exact_match_is_taken_before_the_tolerant_one() {
        let dir = root();
        fs::write(dir.path().join("exact.txt"), "foo  \nbar\nfoo\n").unwrap();
        let done = edit_in(dir.path(), "exact.txt", "foo\n", "bar\n", false).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (1, vec![3]));
        assert_eq!(
            fs::read_to_string(dir.path().join("exact.txt")).unwrap(),
            "foo  \nbar\nbar\n",
            "the trailing blanks of the first line are untouched"
        );
    }

    #[test]
    fn a_multi_line_edit_changes_only_its_span_among_mixed_endings() {
        let dir = root();
        fs::write(dir.path().join("mixed.txt"), "a\r\nb   \nc\r\nd\r\n").unwrap();
        let done = edit_in(dir.path(), "mixed.txt", "b\nc", "B\nC", false).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (1, vec![2]));
        assert_eq!(
            fs::read_to_string(dir.path().join("mixed.txt")).unwrap(),
            "a\r\nB\nC\r\nd\r\n"
        );
    }

    #[test]
    fn a_carriage_return_alone_is_a_line_ending_too() {
        let dir = root();
        fs::write(dir.path().join("cr.txt"), "one\rtwo\r").unwrap();
        let done = edit_in(dir.path(), "cr.txt", "one\ntwo", "1\n2", false).unwrap();
        assert_eq!((done.replaced, done.lines), (1, vec![1]));
        assert_eq!(
            fs::read_to_string(dir.path().join("cr.txt")).unwrap(),
            "1\n2\r"
        );
    }

    #[test]
    fn an_old_of_blanks_only_does_not_match_everywhere() {
        let dir = root();
        let body = "abc\n";
        fs::write(dir.path().join("blank.txt"), body).unwrap();
        for old in ["   ", "\t", " \t "] {
            for all in [false, true] {
                let why = edit_in(dir.path(), "blank.txt", old, "x", all).unwrap_err();
                assert!(why.contains("was not found"), "{old:?}, all={all}: {why}");
                assert_eq!(
                    fs::read_to_string(dir.path().join("blank.txt")).unwrap(),
                    body
                );
            }
        }
    }

    #[test]
    fn trailing_blanks_at_the_end_of_the_file_are_tolerated() {
        let dir = root();
        fs::write(dir.path().join("eof.txt"), "one  \ntwo   ").unwrap();
        let done = edit_in(dir.path(), "eof.txt", "one\ntwo", "1\n2", false).unwrap();
        assert_eq!((done.replaced, done.lines), (1, vec![1]));
        assert_eq!(
            fs::read_to_string(dir.path().join("eof.txt")).unwrap(),
            "1\n2   ",
            "the blanks after the last byte the model matched stay"
        );
    }

    #[test]
    fn the_tolerance_keeps_multibyte_characters_whole() {
        let dir = root();
        fs::write(dir.path().join("utf8.txt"), "héllo  \nwörld\n").unwrap();
        edit_in(dir.path(), "utf8.txt", "héllo\nwörld", "x", false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("utf8.txt")).unwrap(),
            "x\n"
        );
    }

    /// The blanks after the last byte of a match are not the edit's: the two spaces of a Markdown
    /// hard break survive an edit that stops at the last character of the line.
    #[test]
    fn trailing_blanks_after_the_matched_text_are_kept() {
        let dir = root();
        fs::write(dir.path().join("hard.md"), "one  \ntwo  \nthree\n").unwrap();
        let done = edit_in(dir.path(), "hard.md", "one\ntwo", "1\n2", false).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (1, vec![1]));
        assert_eq!(
            fs::read_to_string(dir.path().join("hard.md")).unwrap(),
            "1\n2  \nthree\n",
            "the hard break after `two` is outside the match"
        );
    }

    #[test]
    fn replace_all_keeps_the_blanks_of_every_line_it_does_not_match() {
        let dir = root();
        fs::write(dir.path().join("all.txt"), "x  \ny  \nx  \ny  \n").unwrap();
        let done = edit_in(dir.path(), "all.txt", "x\ny", "X\nY", true).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (2, vec![1, 3]));
        assert_eq!(
            fs::read_to_string(dir.path().join("all.txt")).unwrap(),
            "X\nY  \nX\nY  \n"
        );
    }

    /// `new` is inserted once and never read back as more places to replace.
    #[test]
    fn a_new_that_contains_the_needle_is_inserted_once() {
        let dir = root();
        fs::write(dir.path().join("grow.txt"), "a  \nb  \n").unwrap();
        let done = edit_in(dir.path(), "grow.txt", "a\nb", "a\nb\nc", false).unwrap();
        assert_eq!((done.replaced, done.lines), (1, vec![1]));
        assert_eq!(
            fs::read_to_string(dir.path().join("grow.txt")).unwrap(),
            "a\nb\nc  \n"
        );
    }

    /// A match that ends with the line ending takes the whole `\r\n`, not just the `\r`.
    #[test]
    fn a_tolerant_edit_that_ends_with_a_newline_takes_the_whole_crlf() {
        let dir = root();
        fs::write(dir.path().join("crlf.txt"), "alpha\r\nbeta\r\ngamma\r\n").unwrap();
        let done = edit_in(dir.path(), "crlf.txt", "beta\n", "BETA\n", false).unwrap();
        assert_eq!((done.replaced, done.lines), (1, vec![2]));
        assert_eq!(
            fs::read_to_string(dir.path().join("crlf.txt")).unwrap(),
            "alpha\r\nBETA\ngamma\r\n"
        );
    }

    /// A tolerant deletion takes only the matched bytes; the blanks after the match stay.
    #[test]
    fn a_tolerant_deletion_keeps_the_blanks_after_the_match() {
        let dir = root();
        fs::write(dir.path().join("del.txt"), "a  \nb  \n").unwrap();
        edit_in(dir.path(), "del.txt", "a\nb", "", false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("del.txt")).unwrap(),
            "  \n"
        );
    }

    /// Every span is the bytes the match covers: it starts and ends on a character boundary, its
    /// text normalizes to `old`, and it ends at the last byte matched.
    #[test]
    fn a_tolerant_span_covers_exactly_the_matched_bytes() {
        for (content, old) in [
            ("ab  \ncd", "ab"),
            ("ab\t\r\ncd", "ab"),
            ("ab  ", "ab"),
            ("ab", "ab"),
            ("ab\t\ncd", "ab  \ncd"),
            ("a  \nb\n", "a\nb"),
            ("a\r\nb  \r\nc\r\n", "a\nb"),
            ("a\r\nb\nc\rd", "b\nc"),
            ("a\r\nb\n", "\nb"),
            ("héllo  \nwörld\n", "héllo\nwörld"),
            ("cafe\u{301}  \nx\n", "cafe\u{301}"),
            ("😀  \nx\n", "😀"),
            ("one\rtwo\r", "two"),
            ("a\n   \nb\n", "a\n\nb"),
        ] {
            let (needle, _) = normalized(old);
            let places = normalized_places(content, old)
                .unwrap_or_else(|| panic!("{content:?} has no tolerant match for {old:?}"));
            let mut before = 0;
            for (start, end, _) in places {
                assert!(
                    content.is_char_boundary(start) && content.is_char_boundary(end),
                    "{content:?} {old:?}: {start}..{end} is not on character boundaries"
                );
                assert!(
                    before <= start && start < end,
                    "{content:?} {old:?}: {start}..{end}"
                );
                assert_eq!(
                    normalized(&content[start..end]).0,
                    needle,
                    "{content:?} {old:?}: the span is not the match"
                );
                let last = content.as_bytes()[end - 1];
                assert!(
                    last != b' ' && last != b'\t',
                    "{content:?} {old:?}: the span ends in a blank the match ignored"
                );
                assert_eq!(
                    replace_spans(content, &[(start, end)], "N"),
                    format!("{}N{}", &content[..start], &content[end..]),
                    "the bytes outside the span are untouched"
                );
                before = end;
            }
        }
    }

    /// The map [`normalized`] returns says, for every byte it kept, which byte of the original it
    /// is: one byte copied, or one line ending, and nothing overlaps.
    #[test]
    fn normalized_maps_every_byte_back_to_the_original() {
        for text in [
            "",
            "abc",
            "a  \n b\t\r\nc\r",
            "héllo  \n😀\t\n",
            "\n\n",
            "   ",
            "a\r\r\nb",
        ] {
            let (out, spans) = normalized(text);
            assert_eq!(spans.len(), out.len(), "{text:?}");
            for (i, (start, end)) in spans.iter().enumerate() {
                assert!(start < end, "{text:?} byte {i}: {start}..{end}");
                assert!(*end <= text.len(), "{text:?} byte {i}: {start}..{end}");
                let source = &text.as_bytes()[*start..*end];
                assert!(
                    (source.len() == 1 && source[0] == out.as_bytes()[i])
                        || matches!(source, [b'\n'] | [b'\r'] | [b'\r', b'\n']),
                    "{text:?} byte {i}: {source:?} for {:?}",
                    out.as_bytes()[i] as char
                );
                if i > 0 {
                    assert!(
                        spans[i - 1].1 <= *start,
                        "{text:?} byte {i}: {}..{} overlaps the one before",
                        start,
                        end
                    );
                }
            }
        }
    }

    #[test]
    fn an_old_of_only_a_line_ending_matches_one_place_or_is_refused() {
        let dir = root();
        fs::write(dir.path().join("nl.txt"), "a\rb\r").unwrap();
        let why = edit_in(dir.path(), "nl.txt", "\n", "x", false).unwrap_err();
        assert!(why.contains("was not found"), "{why}");
        assert_eq!(
            fs::read_to_string(dir.path().join("nl.txt")).unwrap(),
            "a\rb\r"
        );
        let done = edit_in(dir.path(), "nl.txt", "\n", "x", true).unwrap();
        assert_eq!((done.replaced, done.lines.clone()), (2, vec![1, 2]));
        assert_eq!(
            fs::read_to_string(dir.path().join("nl.txt")).unwrap(),
            "axbx"
        );
    }

    /// The exact match decides even when it is ambiguous: the tolerance does not turn an error the
    /// model must fix into an edit of some other place.
    #[test]
    fn an_exact_match_that_is_ambiguous_is_not_rescued_by_the_tolerance() {
        let dir = root();
        let body = "foo\nbar\nfoo\nbaz\nfoo  \n";
        fs::write(dir.path().join("exact2.txt"), body).unwrap();
        let why = edit_in(dir.path(), "exact2.txt", "foo\n", "x\n", false).unwrap_err();
        assert!(why.contains("2 times (starting at lines 1, 3)"), "{why}");
        assert_eq!(
            fs::read_to_string(dir.path().join("exact2.txt")).unwrap(),
            body,
            "nothing changed"
        );
    }

    #[test]
    fn an_empty_file_and_a_file_of_blanks_are_clean_misses() {
        let dir = root();
        for (name, body) in [("empty.txt", ""), ("blanks.txt", "   \n\t\n")] {
            fs::write(dir.path().join(name), body).unwrap();
            for all in [false, true] {
                let why = edit_in(dir.path(), name, "a", "b", all).unwrap_err();
                assert!(
                    why.contains("was not found") && why.contains("nothing was changed"),
                    "{name}, all={all}: {why}"
                );
                assert_eq!(fs::read_to_string(dir.path().join(name)).unwrap(), body);
            }
        }
    }

    #[test]
    fn an_edit_of_a_file_over_the_cap_is_refused() {
        let dir = root();
        fs::write(dir.path().join("big.txt"), "x".repeat(WRITE_CAP + 1)).unwrap();
        let why = edit_in(dir.path(), "big.txt", "x", "y", false).unwrap_err();
        assert!(
            why.contains("over the") && why.contains("edit_file works on"),
            "{why}"
        );
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
