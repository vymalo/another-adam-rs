//! `read_file` and `read_files`: read the files of a place without a shell (moved from the coder's
//! file tools, ADR 0047 decision 7). The write tools (`write_file`, `edit_file`, `apply_patch`) are
//! still the coder's, and come here next.
//!
//! The tools read in the agent's own process, through the place's descriptor ([`SlotDir`]), and
//! are **confined to it**: the model chooses the path, so a path is checked by
//! [`confine`](crate::confine) (through [`open_for_read`] and [`parts_of`]) before anything is
//! opened. A read follows a symlink only while it stays inside the place, and never into a `.git`.
//!
//! # Relative to the place
//!
//! The **root** is the place's directory *as it was opened* ([`Place::root`]): the directory may be
//! renamed by the run's commands, and a link put at its name (to another run's directory) would be
//! followed by anything that uses the name. Nothing here uses the name. A path is resolved and
//! opened relative to the descriptor; a link that appears on a path after it was resolved is
//! refused when the file is opened. A swap of the directory, or of a directory below it, at any
//! moment is not followed: the tools go on in the directory they opened, or fail.
//!
//! # What a read shows
//!
//! * `read_file` gives one page of a text file in the [paging convention](adam_llm_agent::paging):
//!   at most 2000 lines and 50 KiB, each behind its number, and one last line that says what is
//!   left (`offset` and `limit`, or the old `start_line` and `end_line`). A page is **one pass**
//!   over the file and keeps only the page; a file over 512 MiB is not read by lines.
//! * A binary file is not shown, except an image (PNG, JPEG, GIF or WebP, by its first bytes; an SVG
//!   is text) for a model that can see the workspace's images: it is offered after the result, or a
//!   line says why it is not shown (over 4 MiB, or the deployment's limits).
//! * A path that is not there names the closest paths `git` lists, when there are any like it.
//! * `read_files` gives the first page (400 lines or 8 KiB) of 1 to 16 files in one result of at
//!   most 32 KiB; a file that does not fit, is not there, is binary or is not UTF-8 is one line, and
//!   the others are still shown.
//! * The first read below a directory that has project instructions shows them, before the page
//!   ([`CodeWorkspace::nested_instructions`]); `read_files` leaves room for a block
//!   ([`MAX_INSTRUCTIONS_BLOCK`]) so that one that was claimed is always shown.
//!
//! The work is bounded by what is done, and a call that runs out of time ([`DEADLINE`]) is an
//! **error result, never a partial page**.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adam_llm_agent::{ToolCtx, ToolOutput, keep_image, paging};
use adam_model::ImageOrigin;
use adam_workspace::safe_fs::{BeneathError, Meta, SlotDir};

use crate::confine::{is_dot_git, open_for_read, parts_of};
use crate::find::{DEADLINE, non_empty, with_nested, within};
use crate::git as gitcli;
use crate::workspace::{CodeWorkspace, Intent, MAX_INSTRUCTIONS_BLOCK, Outcome, Place, Target};

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
/// [`paging`] (see the tool).
///
/// A page is **one pass** over the file and keeps only the page: the final line gives the exact
/// number of lines left (or the file's total), so every line is counted; the lines outside the
/// page are counted as they go and never stored. A file is at most `READ_MAX_FILE` (512 MiB).
#[cfg(any(test, feature = "test-util"))]
pub fn read_in(
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

/// Offer the image at `path` of `place` to the model: the result says it is one and the image
/// follows it, or says why it is not shown. `None` when the file is not an image.
async fn read_image(ctx: &ToolCtx, place: &Place, path: &str) -> Option<ToolOutput> {
    let (root, rel) = (Arc::clone(place.root()), path.to_owned());
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stop);
    let work = async {
        tokio::task::spawn_blocking(move || read_image_in(&root, &rel, &seen))
            .await
            .map_err(|e| format!("the read was interrupted: {e}"))
    };
    let read = within(DEADLINE, &stop, work).await.ok()??;
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

/// What a read or an edit says of a binary file. Public for the coder's `edit_file` until it moves
/// here.
#[doc(hidden)]
pub fn binary_notice(shown: &str, len: u64) -> String {
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

/// The tool `read_file` over `ws`. Its description is the caller's own declaration (the coder's
/// `tools::files`).
#[allow(clippy::too_many_arguments)] // the model's arguments, one each
pub async fn read_file(
    ws: &dyn CodeWorkspace,
    ctx: &ToolCtx,
    // Path relative to the worktree root
    path: String,
    // First line, from 1; -50 = the last 50 lines
    offset: Option<i64>,
    // Most lines (at most 2000)
    limit: Option<i64>,
    // Old name of offset
    start_line: Option<u32>,
    // Last line; limit = end_line - start_line + 1
    end_line: Option<u32>,
    // The slot to read in: its name, or the repository's address. Leave out when the workspace has one.
    repo: Option<String>,
) -> Outcome {
    let Some(path) = non_empty(&path) else {
        return Ok(ToolOutput::error("path is required"));
    };
    let (offset, limit) = match range_args(offset, limit, start_line, end_line) {
        Ok(window) => window,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    let place = match ws.place(ctx, repo.as_deref(), Intent::Read).await {
        Ok(place) => place,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("read {path} ({})", place.name()))
        .await;
    // An image is offered to a model that can see it: shown after the result, or said in a line
    // when the deployment's limits keep it from the model. For a model that cannot see (or a
    // source the deployment does not send) the file is read as any other, and a binary one is
    // "not shown", as it always was.
    if ctx.sees_images_from(ImageOrigin::Workspace)
        && let Some(output) = read_image(ctx, &place, path).await
    {
        return Ok(output);
    }
    let (root, rel) = (Arc::clone(place.root()), path.to_owned());
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&stop);
    let work = async {
        tokio::task::spawn_blocking(move || read_in_until(&root, &rel, offset, limit, &seen))
            .await
            .map_err(|e| format!("the read was interrupted: {e}"))
    };
    let read = match within(DEADLINE, &stop, work).await {
        Ok(read) => read,
        Err(reason) => return Ok(ToolOutput::error(reason)),
    };
    Ok(match read {
        // The first read below a directory that has instructions shows them, before the page.
        Ok(text) => {
            return with_nested(ws, ctx, &place, Some(path), Target::File, text).await;
        }
        Err(reason) if reason == does_not_exist(path) => {
            let at = place.git().clone();
            let wanted = path.to_owned();
            // The suggestion is a courtesy: it has what is left of the call's time, and none is
            // made when it runs out.
            let near = match gitcli::list_files(&at, Path::new("")).await {
                Ok(all) => {
                    let look = Arc::new(AtomicBool::new(false));
                    let seen = Arc::clone(&look);
                    let work = async {
                        tokio::task::spawn_blocking(move || closest_paths(&wanted, &all, &seen))
                            .await
                            .map_err(|e| e.to_string())
                    };
                    within(DEADLINE, &look, work).await.unwrap_or_default()
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

/// The tool `read_files` over `ws`. Its description is the caller's own declaration (the coder's
/// `tools::files`).
pub async fn read_files(
    ws: &dyn CodeWorkspace,
    ctx: &ToolCtx,
    // Paths relative to the worktree root, in the order to show them (1 to 16)
    paths: Vec<String>,
    // The slot to read in: its name, or the repository's address. Leave out when the workspace has one.
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
    let place = match ws.place(ctx, repo.as_deref(), Intent::Read).await {
        Ok(place) => place,
        Err(outcome) => return outcome,
    };
    ctx.emit_progress(format!("read {} files ({})", paths.len(), place.name()))
        .await;
    let root = Arc::clone(place.root());
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
    let read = match within(DEADLINE, &stop, work).await {
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
                if page.len() + MAX_INSTRUCTIONS_BLOCK <= room {
                    // Room for a block: claim it. A block that was claimed is always shown.
                    match ws
                        .nested_instructions(ctx, &place, Some(&wanted[at]), Target::File)
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

/// How alike two lines are, from 0 to 1000: the Sorensen-Dice coefficient of their character pairs
/// (whitespace at the ends does not count). Two empty lines are alike; an empty line and one with
/// text are not. Public for the coder's `edit_file` until it moves here.
#[doc(hidden)]
pub fn similarity(a: &str, b: &str) -> usize {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::*;

    /// The place at `root`, opened as the tools open it: the check and the hold of a directory.
    fn open(root: &Path) -> Result<SlotDir, String> {
        SlotDir::open(root).map_err(|e| {
            format!(
                "the worktree's directory is not a plain directory (a command replaced it by a link or a file): {e}"
            )
        })
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

    fn root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        // What a worktree has: a `.git` file.
        fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        dir
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
}
