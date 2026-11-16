//! Kept output: a long result that is kept whole and read back a page at a time.
//!
//! A tool whose result is long (a command's output, a big answer of an MCP server) does not cut it
//! and does not drop it: the model is shown the first page, and the rest is **kept** under an
//! output id (`o-` and 16 hex characters) that [`ReadOutput`] (`read_output { id, offset?, limit? }`)
//! reads back by lines, in the [paging convention](crate::paging): the same call gives the same
//! bytes.
//!
//! # Where it is kept
//!
//! * **A [`KeptStore`] in the agent's state** ([`KeptOutputs`]): the coder keeps outputs in files on
//!   its workspace volume. The text is **not** in the journal, and a worker that takes the run over
//!   reads the same files.
//! * **Otherwise, the run's durable state**: the tool hands the text back in
//!   [`ToolOutput::kept`](crate::ToolOutput::kept) and the loop keeps it in
//!   [`Conversation::kept_outputs`](crate::Conversation::kept_outputs), at most
//!   [`MAX_RUN_KEPT_BYTES`] for the run. Past that a result is **not kept**: the model is shown the
//!   first page and told, in the last line, that the rest is lost. This is what an agent that is only
//!   a folder gets.
//!
//! # What is kept
//!
//! Redaction is the keeper's: what is kept is as scrubbed as what is shown, and the pages are made
//! from what was kept. A kept text is at most [`MAX_KEPT_BYTES`] (the rest of a longer one is not
//! kept, and the first page says so). A line longer than [`MAX_LINE_CHARS`](crate::paging::MAX_LINE_CHARS)
//! is **folded** onto several lines and a NUL becomes U+FFFD, so that a page never cuts a line and no
//! byte of the text is out of reach.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::{Arc, Mutex, PoisonError};

use adam_model::ToolSpec;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::paging::{MAX_LINE_CHARS, PAGE_LINES, PageError, page_lines, window_args};
use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput};

/// The tool that reads a kept output.
pub const READ_OUTPUT: &str = "read_output";
/// The most bytes of one kept text.
pub const MAX_KEPT_BYTES: usize = 16 * 1024 * 1024;
/// What a producer captures beyond [`MAX_KEPT_BYTES`] (and before a tail it shows), and the margin a
/// keeper cuts back at the edge of a text that was cut ([`scrub_open_end`]). No secret is longer.
pub const SCRUB_OVERLAP: usize = 64 * 1024;
/// The most bytes a run keeps in its own state ([`Conversation::kept_outputs`](crate::Conversation::kept_outputs)).
pub const MAX_RUN_KEPT_BYTES: usize = 4 * 1024 * 1024;

/// An output a tool wants the run to keep in its state: `text` under `id`
/// ([`ToolOutput::kept`](crate::ToolOutput::kept)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptOutput {
    /// The output id ([`output_id`]).
    pub id: String,
    /// The text, as [`prepare`] left it.
    pub text: String,
}

/// What a keeper kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeptInfo {
    /// Bytes kept.
    pub bytes: u64,
    /// Lines kept.
    pub lines: u64,
    /// The text was longer than [`MAX_KEPT_BYTES`] and only its first bytes are kept.
    pub cut: bool,
}

/// Why a kept output could not be kept or read. The text is for the model.
#[derive(Debug, thiserror::Error)]
pub enum KeptError {
    /// Not of the shape `o-` and 16 hex characters.
    #[error(
        "`{0}` is not an output id (an id looks like o-0123456789abcdef and is in the result that kept the output)"
    )]
    Malformed(String),
    /// No such output in this run.
    #[error(
        "no kept output `{0}` in this run: it was kept by an earlier task or another run, or its files have expired, and it is gone. Run the command again to get its output"
    )]
    NotFound(String),
    /// The offset or limit asked for is not usable, or past the end.
    #[error("{0}")]
    Window(String),
    /// The store would not keep it (a bound, a disk).
    #[error("{0}")]
    Refused(String),
    /// The store failed.
    #[error("cannot read kept output: {0}")]
    Io(String),
}

/// A place that keeps outputs for [`ReadOutput`]. Put in the agent's state as a [`KeptStore`].
///
/// An implementation scrubs what it keeps (what it keeps is what it shows), keeps at most
/// [`MAX_KEPT_BYTES`], refuses an id that is not an [`is_output_id`], never lets one run read
/// another's output, and keeps what a worker that takes the run over can read.
/// [`kept_outputs_conformance`] checks it.
#[async_trait]
pub trait KeptOutputs: Send + Sync + 'static {
    /// Keep `text` under `id` for `run` (the root run), replacing what that id held (a call that is
    /// run again keeps the same text). `omitted` is how many bytes of the output the caller left out
    /// before it gave `text` (0 when it gave all of it): when it is not 0, the kept text ends with a
    /// line that says so, and [`KeptInfo::cut`] is set. (Scrubbing changes lengths, so the count is of
    /// what was left out and not of what was printed.)
    ///
    /// A `text` with `omitted` not 0 is the **first part** of a longer output, so it may end in the
    /// first half of a secret that scrubbing cannot recognise. The keeper cuts such a text back at its
    /// end with [`scrub_open_end`] (it keeps no more than the text less [`SCRUB_OVERLAP`] bytes), and
    /// a producer gives it unscrubbed.
    ///
    /// # Errors
    ///
    /// [`KeptError::Malformed`] for an id that is not one, [`KeptError::Refused`] when a bound is
    /// reached, [`KeptError::Io`].
    async fn keep(
        &self,
        run: &str,
        id: &str,
        text: &str,
        omitted: u64,
    ) -> Result<KeptInfo, KeptError>;

    /// The page of the output `id` of `run` from `offset` (1-based; negative: from the end) of at
    /// most `limit` lines, rendered as [`Page::render`](crate::paging::Page::render) does: its lines,
    /// then one final line.
    ///
    /// # Errors
    ///
    /// [`KeptError::Malformed`], [`KeptError::NotFound`] (another run's id is a miss like any
    /// other: no store scans for it), [`KeptError::Window`], [`KeptError::Io`].
    async fn page(&self, run: &str, id: &str, offset: i64, limit: u32)
    -> Result<String, KeptError>;
}

/// The keeper an agent's tools reach through [`ToolCtx::state`](crate::ToolCtx::state).
#[derive(Clone)]
pub struct KeptStore(pub Arc<dyn KeptOutputs>);

impl std::fmt::Debug for KeptStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeptStore")
    }
}

/// The output id of a call: `o-` and the first 16 hex characters of the SHA-256 of the root run and
/// the call's key, so a call that runs again keeps under the same id. An id is not a secret: what
/// keeps a run from another's output is that [`ReadOutput`] takes the run from the call's context.
pub fn output_id(root_run: &str, call_key: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(root_run.as_bytes());
    hash.update([0]);
    hash.update(call_key.as_bytes());
    let digest = hash.finalize();
    let mut id = String::from("o-");
    for byte in &digest[..8] {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// What a call is called within its root run: its call id, and for a subagent's call its own run
/// before it (a call id is only unique within the run that made it).
pub fn call_key(ctx: &ToolCtx) -> String {
    if ctx.run_id() == ctx.root_run_id() {
        ctx.call_id().to_owned()
    } else {
        format!("{}/{}", ctx.run_id(), ctx.call_id())
    }
}

/// The id of the output kept by the call of `ctx`.
pub fn output_id_of(ctx: &ToolCtx) -> String {
    output_id(&ctx.root_run_id().to_string(), &call_key(ctx))
}

/// Whether `id` has the shape of an output id: `o-` and 16 lowercase hex characters.
pub fn is_output_id(id: &str) -> bool {
    id.strip_prefix("o-").is_some_and(|hex| {
        hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// `text` as it is kept, for an output of which `omitted` bytes were left out before: at most [`MAX_KEPT_BYTES`] of it
/// (cut on a character boundary), a NUL as U+FFFD, and every line over [`MAX_LINE_CHARS`] characters
/// folded onto several lines, so that paging by lines loses nothing. When the output was longer than
/// what is kept (`omitted` not 0, or `text` over the bound) the kept text ends with a
/// line that says how much is kept, and the flag is set.
pub fn prepare(text: &str, omitted: u64) -> (Cow<'_, str>, bool) {
    let (text, cut_here, tail_dropped) = if text.len() > MAX_KEPT_BYTES {
        let mut end = MAX_KEPT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (&text[..end], true, (text.len() - end) as u64)
    } else {
        (text, false, 0)
    };
    let cut = cut_here || omitted > 0;
    let long = text
        .split('\n')
        .any(|line| line.len() > MAX_LINE_CHARS && line.chars().nth(MAX_LINE_CHARS).is_some());
    if !long && !text.contains('\0') && !cut {
        return (Cow::Borrowed(text), false);
    }
    let mut out = String::with_capacity(text.len() + text.len() / MAX_LINE_CHARS + 128);
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let mut chars = 0;
        for c in line.chars() {
            if chars == MAX_LINE_CHARS {
                out.push('\n');
                chars = 0;
            }
            out.push(if c == '\0' { '\u{fffd}' } else { c });
            chars += 1;
        }
    }
    if cut {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        let _ = writeln!(
            out,
            "(output cut: only the first {} of {} bytes are kept; the first number counts the scrubbed text, the second the output)",
            text.len(),
            text.len() as u64 + omitted + tail_dropped
        );
    }
    (Cow::Owned(out), cut)
}

/// Count the lines of a prepared text.
pub fn count_lines(text: &str) -> u64 {
    let newlines = text.bytes().filter(|&b| b == b'\n').count() as u64;
    if text.is_empty() {
        0
    } else if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}

/// A page of a prepared text, as [`KeptOutputs::page`] renders it.
///
/// # Errors
///
/// [`KeptError::Window`] for an offset or limit that cannot be used, or past the end.
pub fn page_of(id: &str, text: &str, offset: i64, limit: u32) -> Result<String, KeptError> {
    match page_lines(Cursor::new(text.as_bytes()), offset, limit, false) {
        Ok(page) => Ok(page.render()),
        Err(PageError::PastEnd { offset, total }) => Err(KeptError::Window(format!(
            "output `{id}` has {total} lines: offset {offset} is past the end"
        ))),
        Err(other) => Err(KeptError::Window(other.to_string())),
    }
}

/// The line that opens what the model is shown of a kept text: the id, how long the text is, and
/// that `read_output` reads it.
pub fn kept_header(id: &str, info: &KeptInfo, total_bytes: u64) -> String {
    let mut line = format!(
        "[kept as {id}: {} lines; read_output {{\"id\":\"{id}\"}} reads it",
        info.lines
    );
    if info.cut {
        let _ = write!(
            line,
            "; only the first {} of {total_bytes} bytes are kept (the first number counts the scrubbed text, the second the output)",
            info.bytes
        );
    }
    line.push_str("]\n");
    line
}

/// The one line a shortened history keeps to say how to read the kept output `id`: written from
/// the id alone, never copied from a result.
pub fn pointer_line(id: &str) -> String {
    format!("[kept as {id}; read_output {{\"id\":\"{id}\"}} reads it]")
}

/// The id of a line that is exactly [`pointer_line`] of a well-formed id.
pub fn canonical_pointer_id(line: &str) -> Option<&str> {
    let id = line.strip_prefix("[kept as ")?.get(..18)?;
    (is_output_id(id) && line == pointer_line(id)).then_some(id)
}

/// Where in `text` the line is that points to the kept output `id`: the byte range (without the
/// newline) of the first line with the shape of what the kept-output code writes
/// ([`kept_header`], and the closing line of a command's view): it starts with `[`, ends with `]`
/// and holds `read_output {"id":"<id>"}`. A line that only mentions the words, or names another
/// id, is none. `id` is the one of the call whose result `text` is, so text that a tool copied from
/// somewhere (a file, a page) cannot name one.
pub fn find_pointer(text: &str, id: &str) -> Option<std::ops::Range<usize>> {
    if !is_output_id(id) {
        return None;
    }
    let needle = format!("read_output {{\"id\":\"{id}\"}}");
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.starts_with('[') && body.ends_with(']') && body.contains(&needle) {
            return Some(start..start + body.len());
        }
        start += line.len();
    }
    None
}

/// The canonical pointer line ([`pointer_line`]) when `text` has a line for the kept output `id`
/// ([`find_pointer`]). A shortening of a history keeps this line, so the id outlives the cut.
pub fn read_pointer(text: &str, id: &str) -> Option<String> {
    find_pointer(text, id).map(|_| pointer_line(id))
}

/// What the model is shown of a text that was kept: [`kept_header`], the first page, and the
/// page's final line (`[more: ...; next page: offset=N]`).
pub fn kept_view(id: &str, kept: &str, info: &KeptInfo, total_bytes: u64) -> String {
    let first = page_of(id, kept, 1, PAGE_LINES).unwrap_or_default();
    kept_header(id, info, total_bytes) + &first
}

/// What the model is shown of a text that could **not** be kept: its first page, and a last line
/// that says why the rest is lost.
pub fn unkept_view(kept: &str, why: &str) -> String {
    let mut view = String::new();
    if let Ok(page) = page_lines(Cursor::new(kept.as_bytes()), 1, PAGE_LINES, false) {
        for line in &page.lines {
            // As on a page: only the last line of the view may start with `[`.
            if let Some(rest) = line.text.strip_prefix('[') {
                view.push_str("\\x5b");
                view.push_str(rest);
            } else {
                view.push_str(&line.text);
            }
            view.push('\n');
        }
        let shown = page.lines.last().map_or(0, |l| l.number);
        let _ = write!(
            view,
            "[not kept: {why}; the first {shown} of {} lines are shown and the rest is lost]",
            page.total
        );
    }
    view
}

/// What the model is shown when a keeper failed: nothing of the text, because only the keeper
/// knows how to scrub it.
fn lost_view(why: &KeptError) -> String {
    format!("[not kept: {why}; the result is lost and none of it is shown]")
}

/// The most bytes a replacement of a secret can share with the secret's own bytes where the two
/// scrubs of an open edge diverge (the replacement is `[redacted]`: 10 bytes, 9 of them could match a
/// secret that starts `[red`). A cut that is not at the end of the agreement is backed off by this.
const REPLACEMENT_SLACK: usize = 16;

/// `text`, which is only the **first part** of a longer output, scrubbed with `scrub` and cut back
/// so that no part of a secret is left at its end.
///
/// The end of such a text may be the first half of a secret that `scrub` cannot recognise, and the
/// scrubbed length says nothing about where in the raw text a cut falls (a replacement is shorter or
/// longer than its value). So the text is scrubbed whole, and again without its last `margin` bytes:
/// what the two agree on is the text up to the first place where a secret straddles the new end,
/// which is replaced in the one and cut in the other. `margin` is the length of the longest value
/// `scrub` replaces, derived forms included, and at least [`SCRUB_OVERLAP`]: a caller that holds a
/// longer value must say so, because a value longer than the margin can be kept in part. Where the
/// two diverge the agreement is backed off a little (a secret that starts like the replacement
/// agrees with it for a few bytes). The result is scrubbed.
pub fn scrub_open_end(text: &str, scrub: impl Fn(&str) -> String, margin: usize) -> String {
    let margin = margin.max(SCRUB_OVERLAP);
    let mut keep = text.len().saturating_sub(margin);
    while !text.is_char_boundary(keep) {
        keep -= 1;
    }
    let mut whole = scrub(text);
    let part = scrub(&text[..keep]);
    let agree = whole
        .bytes()
        .zip(part.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let mut end = if agree < part.len() {
        agree.saturating_sub(REPLACEMENT_SLACK)
    } else {
        agree
    };
    while !whole.is_char_boundary(end) {
        end -= 1;
    }
    whole.truncate(end);
    whole
}

/// [`scrub_open_end`] for a text that is only the **last part** of a longer output: scrubbed, and
/// without a part of a secret at its start.
pub fn scrub_open_start(text: &str, scrub: impl Fn(&str) -> String, margin: usize) -> String {
    let margin = margin.max(SCRUB_OVERLAP);
    let mut from = margin.min(text.len());
    while !text.is_char_boundary(from) {
        from += 1;
    }
    let whole = scrub(text);
    let part = scrub(&text[from..]);
    let agree = whole
        .bytes()
        .rev()
        .zip(part.bytes().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let used = if agree < part.len() {
        agree.saturating_sub(REPLACEMENT_SLACK)
    } else {
        agree
    };
    let mut start = whole.len() - used;
    while !whole.is_char_boundary(start) {
        start += 1;
    }
    whole[start..].to_owned()
}

/// Scrubs an output **as it streams**, so that what is written is exactly the scrub of the whole
/// and no secret is ever on disk in part.
///
/// The bytes of a command arrive in reads of any size, and a secret can be split between two of
/// them. The scrubber holds raw bytes back, and lets a part go only when scrubbing it alone and
/// scrubbing it as part of the buffer agree (`scrub(buf) == scrub(buf[..k]) + scrub(buf[k..])`): no
/// secret straddles `k`. What it emits, concatenated, is `scrub(whole)`.
///
/// * [`push`](Self::push) lets a part go once the buffer holds two margins: it cuts `margin` bytes
///   before the end, so a secret that straddles the cut lies whole in the buffer (no secret is
///   longer than the margin) and the check sees it. It holds at most `2 * margin` bytes.
/// * [`release`](Self::release) lets everything go except the tail that can still be the start of a
///   secret, which the caller measures (`partial`): an output that goes quiet is not held for a
///   margin of bytes that will never come.
/// * [`finish`](Self::finish) scrubs the rest.
///
/// The `scrub` closures passed to each call are the same, and replace values (they never depend on
/// where a text starts).
#[derive(Debug, Clone)]
pub struct StreamScrubber {
    /// Raw text not written yet (what is not UTF-8 is already U+FFFD, as a lossy read makes it).
    pending: String,
    /// The first bytes of a character the last read ended in the middle of (at most 3).
    carry: Vec<u8>,
    margin: usize,
}

impl StreamScrubber {
    /// A scrubber for secrets of at most `margin` bytes (at least [`SCRUB_OVERLAP`]).
    pub fn new(margin: usize) -> Self {
        Self {
            pending: String::new(),
            carry: Vec::new(),
            margin: margin.max(SCRUB_OVERLAP),
        }
    }

    /// How many raw bytes are held back now.
    pub fn held(&self) -> usize {
        self.pending.len() + self.carry.len()
    }

    /// A scrubber with a margin below [`SCRUB_OVERLAP`], so that a test reaches the cuts with a
    /// little text.
    #[cfg(test)]
    fn with_small_margin(margin: usize) -> Self {
        Self {
            margin,
            ..Self::new(0)
        }
    }

    /// Add `bytes`; the scrubbed text that can be written now (maybe none).
    pub fn push(&mut self, bytes: &[u8], scrub: &dyn Fn(&str) -> String) -> String {
        self.add(bytes);
        if self.pending.len() < 2 * self.margin {
            return String::new();
        }
        // Try the cut `margin` bytes before the end, and, when a secret straddles it, one margin
        // earlier, down to the start: a cut that a secret straddles is never taken.
        let mut at = self.pending.len() - self.margin;
        loop {
            let cut = floor_boundary(&self.pending, at);
            if cut > 0
                && let Some(text) = self.cut_at(cut, scrub)
            {
                return text;
            }
            match at.checked_sub(self.margin) {
                Some(earlier) if earlier > 0 => at = earlier,
                _ => return String::new(),
            }
        }
    }

    /// Everything that cannot be the start of a secret: `partial(text)` is how many bytes at the
    /// end of `text` could still be the first part of a secret (0 when none), and they stay.
    pub fn release(
        &mut self,
        scrub: &dyn Fn(&str) -> String,
        partial: &dyn Fn(&str) -> usize,
    ) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let window = floor_boundary(&self.pending, self.pending.len().saturating_sub(self.margin));
        let held = partial(&self.pending[window..]);
        let cut = floor_boundary(&self.pending, self.pending.len().saturating_sub(held));
        if cut == 0 {
            return String::new();
        }
        self.cut_at(cut, scrub).unwrap_or_default()
    }

    /// What is left, scrubbed: the output has ended.
    pub fn finish(&mut self, scrub: &dyn Fn(&str) -> String) -> String {
        if !self.carry.is_empty() {
            // A character that never ended.
            self.pending.push('\u{fffd}');
            self.carry.clear();
        }
        scrub(&std::mem::take(&mut self.pending))
    }

    /// Decode `bytes` as a lossy read does and add them to `pending`, holding back the start of a
    /// character the read ended in.
    fn add(&mut self, bytes: &[u8]) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut rest = data.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    self.pending.push_str(valid);
                    return;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    if let Ok(valid) = std::str::from_utf8(valid) {
                        self.pending.push_str(valid);
                    }
                    match e.error_len() {
                        // The read ended inside a character.
                        None => {
                            self.carry = after.to_vec();
                            return;
                        }
                        Some(n) => {
                            self.pending.push('\u{fffd}');
                            rest = &after[n..];
                        }
                    }
                }
            }
        }
    }

    /// The scrub of `pending[..cut]`, taken off the buffer, when no secret straddles `cut`.
    fn cut_at(&mut self, cut: usize, scrub: &dyn Fn(&str) -> String) -> Option<String> {
        let whole = scrub(&self.pending);
        let head = scrub(&self.pending[..cut]);
        let tail = scrub(&self.pending[cut..]);
        if whole.len() != head.len() + tail.len()
            || !whole.starts_with(&head)
            || !whole.ends_with(&tail)
        {
            return None;
        }
        self.pending.drain(..cut);
        Some(head)
    }
}

/// `at`, moved back to the start of a character of `text`.
fn floor_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Folds a streamed text the way [`prepare`] does, a piece at a time: a line over
/// [`MAX_LINE_CHARS`] characters is folded onto several, a NUL is U+FFFD, and only **complete**
/// lines are returned, so that a file written from them never ends in the middle of one (the pages
/// of the lines already there do not change when more is written).
#[derive(Debug, Clone, Default)]
pub struct LineFolder {
    line: String,
    chars: usize,
}

impl LineFolder {
    /// An empty folder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Characters of the line that is not complete yet.
    pub fn open_chars(&self) -> usize {
        self.chars
    }

    /// Add `text`; the lines it completed, each ending in `\n`.
    pub fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            if c == '\n' {
                out.push_str(&self.line);
                out.push('\n');
                self.line.clear();
                self.chars = 0;
                continue;
            }
            if self.chars == MAX_LINE_CHARS {
                out.push_str(&self.line);
                out.push('\n');
                self.line.clear();
                self.chars = 0;
            }
            self.line.push(if c == '\0' { '\u{fffd}' } else { c });
            self.chars += 1;
        }
        out
    }

    /// The line that was never ended, without a line end: the output has ended.
    pub fn finish(&mut self) -> String {
        self.chars = 0;
        std::mem::take(&mut self.line)
    }
}

/// Keep a long `text` for the call of `ctx` and say what the model is shown: the first page and an
/// id to read the rest with [`ReadOutput`], or, when it cannot be kept, the first page and why the
/// rest is lost. The second value is what the loop keeps in the run's state
/// ([`ToolOutput::kept`](crate::ToolOutput::kept)); it is `Some` only when no [`KeptStore`] is in the
/// agent's state.
pub async fn keep_long(ctx: &ToolCtx, text: &str) -> (String, Option<Box<KeptOutput>>) {
    keep_long_cut(ctx, text, 0).await
}

/// [`keep_long`] for a `text` that is the first part of a result of which `omitted` more bytes were
/// left out. A keeper cuts such a text back at its end ([`KeptOutputs::keep`]); with no keeper the
/// context's scrub hook ([`ToolCtx::with_scrub`]) does.
pub async fn keep_long_cut(
    ctx: &ToolCtx,
    text: &str,
    omitted: u64,
) -> (String, Option<Box<KeptOutput>>) {
    let id = output_id_of(ctx);
    let total = text.len() as u64 + omitted;
    if let Some(store) = ctx.state::<KeptStore>() {
        let run = ctx.root_run_id().to_string();
        return match store.0.keep(&run, &id, text, omitted).await {
            Ok(info) => {
                // The page is made from what was kept (and scrubbed by the keeper), never from `text`.
                match store.0.page(&run, &id, 1, PAGE_LINES).await {
                    Ok(first) => (kept_header(&id, &info, total) + &first, None),
                    Err(e) => (lost_view(&e), None),
                }
            }
            Err(e) => (lost_view(&e), None),
        };
    }
    // No keeper to scrub: the context's hook does it, before the fold, which could split a value
    // across two lines.
    let (scrubbed, omitted) =
        scrubbed_for_keeping(text, omitted, |t| ctx.scrub(t), ctx.scrub_margin());
    let (prepared, cut) = prepare(&scrubbed, omitted);
    if prepared.len() <= ctx.kept_left() {
        let info = KeptInfo {
            bytes: prepared.len() as u64,
            lines: count_lines(&prepared),
            cut,
        };
        let view = kept_view(&id, &prepared, &info, total);
        return (
            view,
            Some(Box::new(KeptOutput {
                id,
                text: prepared.into_owned(),
            })),
        );
    }
    let why = format!(
        "a run keeps at most {} MiB of long results here, and this one is {} bytes",
        MAX_RUN_KEPT_BYTES / (1024 * 1024),
        prepared.len()
    );
    (unkept_view(&prepared, &why), None)
}

/// What a keeper makes of the `text` it is given, before it cuts and folds it: scrubbed with
/// `scrub`, and, when `omitted` is not 0 (the text is the first part of a longer output), cut back
/// at its end by [`scrub_open_end`] with `margin` (the longest value `scrub` replaces). With the `omitted` to keep: what was left out, counted again
/// for what the cut back took (never 0 for a text that was cut).
pub fn scrubbed_for_keeping(
    text: &str,
    omitted: u64,
    scrub: impl Fn(&str) -> String,
    margin: usize,
) -> (String, u64) {
    if omitted == 0 {
        return (scrub(text), 0);
    }
    let scrubbed = scrub_open_end(text, scrub, margin);
    let total = text.len() as u64 + omitted;
    let omitted = total.saturating_sub(scrubbed.len() as u64).max(1);
    (scrubbed, omitted)
}

/// `read_output { id, offset?, limit? }`: reads a kept output page by page.
///
/// It reads the agent's [`KeptStore`] when there is one, and the run's own state otherwise (what a
/// tool handed back in [`ToolOutput::kept`](crate::ToolOutput::kept)). The run is the call's, never
/// an argument, so an id of another run is refused. A [paged](Tool::paged) tool.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOutput;

#[async_trait]
impl Tool for ReadOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: READ_OUTPUT.into(),
            description: "Read a kept output page by page: the output of a command, or a long \
                          tool result, whose result named its id (o- and 16 hex characters). You \
                          get its lines, then a last line that says where the next page starts. The \
                          same id and offset give the same page: a third call in a row with the \
                          same arguments, after two identical results, is not run. Example: {\"id\":\"o-3f9c2a17b0d4e8a1\",\"offset\":2001}"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The output id from the result that kept it"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "First line, from 1; -50 = the last 50 lines"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Most lines (at most 2000)"
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        }
    }

    fn paged(&self) -> bool {
        true
    }

    fn step_style(&self) -> crate::tool::StepStyle {
        crate::tool::StepStyle::default()
            .with_label("Read an output")
            .with_icon(crate::StepIcon::Read)
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let Some(id) = args.get("id").and_then(Value::as_str) else {
            return Ok(ToolOutput::error("id is required"));
        };
        if !is_output_id(id) {
            return Ok(ToolOutput::error(
                KeptError::Malformed(id.to_owned()).to_string(),
            ));
        }
        let number = |key: &str| args.get(key).and_then(Value::as_i64);
        let (offset, limit) = match window_args(number("offset"), number("limit"), PAGE_LINES) {
            Ok(window) => window,
            Err(reason) => return Ok(ToolOutput::error(reason)),
        };
        let page = if let Some(store) = ctx.state::<KeptStore>() {
            let run = ctx.root_run_id().to_string();
            store.0.page(&run, id, offset, limit).await
        } else {
            match ctx.kept_text(id) {
                // A secret the agent learned after the text was kept: scrubbed at read, as the
                // layer around this tool scrubs its results (no hook: the text as it is).
                Some(text) => page_of(id, &ctx.scrub(text), offset, limit),
                None => Err(KeptError::NotFound(id.to_owned())),
            }
        };
        Ok(match page {
            Ok(page) => ToolOutput::text(page),
            Err(e) => ToolOutput::error(e.to_string()),
        })
    }
}

/// A [`KeptOutputs`] in memory, for tests and for the conformance of the trait.
#[derive(Default)]
pub struct MemoryKept {
    kept: Mutex<HashMap<(String, String), String>>,
    scrub: Option<Scrub>,
    margin: usize,
}

type Scrub = Arc<dyn Fn(&str) -> String + Send + Sync>;

impl MemoryKept {
    /// An empty one.
    pub fn new() -> Self {
        Self::default()
    }

    /// The length of the longest value the scrubber replaces, when it is over [`SCRUB_OVERLAP`]: the
    /// margin the end of a cut text is kept back by.
    #[must_use]
    pub fn with_margin(mut self, longest: usize) -> Self {
        self.margin = longest;
        self
    }

    /// Scrub what is kept with `scrub` first.
    #[must_use]
    pub fn scrubbing(mut self, scrub: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.scrub = Some(Arc::new(scrub));
        self
    }
}

#[async_trait]
impl KeptOutputs for MemoryKept {
    async fn keep(
        &self,
        run: &str,
        id: &str,
        text: &str,
        omitted: u64,
    ) -> Result<KeptInfo, KeptError> {
        if !is_output_id(id) {
            return Err(KeptError::Malformed(id.to_owned()));
        }
        // Scrubbed first, then folded and cut: a secret is never split by either.
        let (scrubbed, omitted) = scrubbed_for_keeping(
            text,
            omitted,
            |t| match &self.scrub {
                Some(scrub) => scrub(t),
                None => t.to_owned(),
            },
            self.margin,
        );
        let (prepared, cut) = prepare(&scrubbed, omitted);
        let info = KeptInfo {
            bytes: prepared.len() as u64,
            lines: count_lines(&prepared),
            cut,
        };
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((run.to_owned(), id.to_owned()), prepared.into_owned());
        Ok(info)
    }

    async fn page(
        &self,
        run: &str,
        id: &str,
        offset: i64,
        limit: u32,
    ) -> Result<String, KeptError> {
        if !is_output_id(id) {
            return Err(KeptError::Malformed(id.to_owned()));
        }
        let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        match kept.get(&(run.to_owned(), id.to_owned())) {
            Some(text) => page_of(id, text, offset, limit),
            None => Err(KeptError::NotFound(id.to_owned())),
        }
    }
}

/// The conformance of a [`KeptOutputs`]: what [`ReadOutput`] and the tools that keep rely on. Call
/// it from a test of an implementation (it panics on a violation).
///
/// # Panics
///
/// When `store` does not behave as the trait says.
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub async fn kept_outputs_conformance(store: &dyn KeptOutputs) {
    let run = "00000000-0000-4000-8000-000000000001";
    let other = "00000000-0000-4000-8000-000000000002";
    let id = output_id(run, "call-1");
    let text: String = (1..=5000).map(|i| format!("line {i}\n")).collect();

    // A kept text pages back, whole, in order.
    let info = store.keep(run, &id, &text, 0).await.unwrap();
    assert_eq!((info.lines, info.cut), (5000, false));
    let first = store.page(run, &id, 1, 2000).await.unwrap();
    assert!(first.starts_with("line 1\n"), "{first}");
    assert!(
        first.ends_with("[more: 3000 lines left; next page: offset=2001]"),
        "{first}"
    );
    let mut all = String::new();
    let mut offset = 1;
    loop {
        let page = store.page(run, &id, offset, 2000).await.unwrap();
        let (body, last) = page.rsplit_once('\n').unwrap();
        all.push_str(body);
        all.push('\n');
        match last
            .strip_prefix("[more: ")
            .and_then(|l| l.split("offset=").nth(1))
        {
            Some(next) => offset = next.trim_end_matches(']').parse().unwrap(),
            None => {
                assert_eq!(last, "[end: 5000 lines]");
                break;
            }
        }
    }
    assert_eq!(all, text, "the pages reassemble the text");

    // The same call, the same bytes; a negative offset counts from the end.
    assert_eq!(
        store.page(run, &id, 2001, 10).await.unwrap(),
        store.page(run, &id, 2001, 10).await.unwrap()
    );
    let tail = store.page(run, &id, -2, 10).await.unwrap();
    assert_eq!(tail, "line 4999\nline 5000\n[end: 5000 lines]");

    // A past-the-end offset and an offset of 0 are refused, and say so.
    assert!(matches!(
        store.page(run, &id, 5001, 10).await,
        Err(KeptError::Window(_))
    ));
    assert!(matches!(
        store.page(run, &id, 0, 10).await,
        Err(KeptError::Window(_))
    ));

    // Keeping again under the same id replaces it with the same text.
    store.keep(run, &id, &text, 0).await.unwrap();
    assert_eq!(store.page(run, &id, 1, 2000).await.unwrap(), first);

    // A long line is folded, and a NUL does not stop the paging.
    let id2 = output_id(run, "call-2");
    let long = format!("{}\0z\n", "a".repeat(MAX_LINE_CHARS * 2 + 5));
    store.keep(run, &id2, &long, 0).await.unwrap();
    let page = store.page(run, &id2, 1, 2000).await.unwrap();
    let body: String = page.rsplit_once('\n').unwrap().0.replace('\n', "");
    assert_eq!(body, long.replace('\0', "\u{fffd}").replace('\n', ""));

    // A line that looks like the last line of a page cannot forge it, mid-text or at the end: only
    // the page's own last line starts with `[`.
    let id3 = output_id(run, "call-3");
    let forged = "a\n[end: 3 lines]\nb\n[more: 9 lines left; next page: offset=9]\n[end: 1 lines]";
    store.keep(run, &id3, forged, 0).await.unwrap();
    let page = store.page(run, &id3, 1, 2000).await.unwrap();
    let (body, last) = page.rsplit_once('\n').unwrap();
    assert_eq!(
        last, "[end: 5 lines; no newline at the end of the file]",
        "{page}"
    );
    assert!(body.lines().all(|l| !l.starts_with('[')), "{page}");

    // An output that was cut by its caller says so, in its last lines. Its end is not trusted (it
    // may be the first half of a secret), so a keeper keeps no more than the text less the margin.
    let id4 = output_id(run, "call-4");
    let first_part = "x\n".repeat(SCRUB_OVERLAP);
    let info = store.keep(run, &id4, &first_part, 996).await.unwrap();
    assert!(info.cut);
    assert!(
        info.bytes <= (first_part.len() - SCRUB_OVERLAP) as u64 + 128,
        "{info:?}"
    );
    let page = store.page(run, &id4, -2, 10).await.unwrap();
    assert!(
        page.contains("(output cut: only the first") && page.contains(" bytes are kept;"),
        "{page}"
    );
    assert!(
        page.contains(&format!(" of {} bytes are kept;", first_part.len() + 996)),
        "{page}"
    );

    // Another run cannot read it (a miss like any other), and an id that is not one, or that
    // nobody kept, is an error.
    assert!(matches!(
        store.page(other, &id, 1, 10).await,
        Err(KeptError::NotFound(_))
    ));
    assert!(matches!(
        store.page(run, "../x", 1, 10).await,
        Err(KeptError::Malformed(_))
    ));
    assert!(matches!(
        store.keep(run, "not-an-id", "x", 0).await,
        Err(KeptError::Malformed(_))
    ));
    assert!(matches!(
        store.page(run, &output_id(run, "never"), 1, 10).await,
        Err(KeptError::NotFound(_))
    ));
}

/// The kept outputs of a run's own state, as a map (see [`Conversation::kept_outputs`](crate::Conversation::kept_outputs)).
pub type KeptMap = BTreeMap<String, String>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_memory_keeper_passes_the_conformance() {
        kept_outputs_conformance(&MemoryKept::new()).await;
    }

    #[test]
    fn the_same_call_gives_the_same_id_and_ids_have_one_shape() {
        let a = output_id("run-a", "call-1");
        assert_eq!(a, output_id("run-a", "call-1"));
        assert_ne!(a, output_id("run-b", "call-1"));
        assert_ne!(a, output_id("run-a", "call-2"));
        assert!(is_output_id(&a), "{a}");
        for bad in [
            "",
            "o-",
            "o-0123456789abcde",
            "o-0123456789abcdef0",
            "o-0123456789ABCDEF",
            "../o-0123456789abcdef",
            "o-0123456789abcdeg",
            "x-0123456789abcdef",
        ] {
            assert!(!is_output_id(bad), "{bad:?}");
        }
    }

    /// A secret that a fold or the paging could split: a scrubber sees the line whole, before it is
    /// folded at column 2,000, and no page holds a part of it.
    #[tokio::test]
    async fn a_secret_across_the_fold_is_scrubbed_before_it_is_folded() {
        let store = MemoryKept::new().scrubbing(|t| t.replace("SECRET-VALUE-XYZ", "[redacted]"));
        let run = "00000000-0000-4000-8000-000000000001";
        let id = output_id(run, "c1");
        let line = format!("{}SECRET-VALUE-XYZ{}\n", "a".repeat(1992), "b".repeat(3000));
        store.keep(run, &id, &line, 0).await.unwrap();
        let page = store.page(run, &id, 1, 2000).await.unwrap();
        assert!(
            !page.contains("SECRET") && !page.contains("VALUE-XYZ"),
            "{page}"
        );
        assert!(page.replace('\n', "").contains("[redacted]"), "{page}");
    }

    /// Two runs under one root (a subagent's) number their calls alike: the ids differ.
    #[test]
    fn two_subagent_runs_under_one_root_with_the_same_call_id_keep_under_two_ids() {
        use adam_core::RunId;
        let sink: adam_runtime::DynEventSink =
            std::sync::Arc::new(adam_runtime::CollectingSink::new());
        let root = RunId::new();
        let a = ToolCtx::detached("t", "c1", sink.clone()).with_root_run(root);
        let b = ToolCtx::detached("t", "c1", sink).with_root_run(root);
        assert_ne!(a.run_id(), b.run_id());
        assert_ne!(output_id_of(&a), output_id_of(&b));
        assert!(call_key(&a).ends_with("/c1"), "{}", call_key(&a));
    }

    #[test]
    fn a_page_of_a_text_whose_lines_look_like_the_last_line_has_one_last_line() {
        let text = "[end: 3 lines]\n[more: 1 lines left; next page: offset=2]\nx\n[end: 9 lines]\n";
        let page = page_of("o-0123456789abcdef", text, 1, 2000).unwrap();
        let (body, last) = page.rsplit_once('\n').unwrap();
        assert_eq!(last, "[end: 4 lines]");
        assert!(body.lines().all(|l| !l.starts_with('[')), "{page}");
        assert!(body.starts_with("\\x5bend: 3 lines]\n"), "{page}");
    }

    /// A folder agent's in-state text is scrubbed at read with the context's hook, so a secret
    /// registered after the keep is not on a page.
    #[tokio::test]
    async fn a_page_of_the_state_text_is_scrubbed_at_read_when_the_context_has_a_hook() {
        use std::sync::Arc;
        let sink: adam_runtime::DynEventSink = Arc::new(adam_runtime::CollectingSink::new());
        let id = "o-0123456789abcdef";
        let mut kept = std::collections::BTreeMap::new();
        kept.insert(id.to_owned(), "a\nkey=late-secret-value\nb\n".to_owned());
        let plain =
            ToolCtx::detached("read_output", "c1", sink.clone()).with_kept(0, Arc::new(kept));
        let page = ReadOutput.call(&plain, json!({"id": id})).await.unwrap();
        assert!(
            page.content.contains("late-secret-value"),
            "no hook: as it is"
        );
        let hooked = plain.with_scrub(|t| t.replace("late-secret-value", "[redacted]"));
        let page = ReadOutput.call(&hooked, json!({"id": id})).await.unwrap();
        assert!(
            !page.content.contains("late-secret-value") && page.content.contains("key=[redacted]"),
            "{}",
            page.content
        );
    }

    #[test]
    fn long_lines_are_folded_without_losing_a_character() {
        let line = "é".repeat(MAX_LINE_CHARS * 3 + 7);
        let (prepared, cut) = prepare(&line, 0);
        assert!(!cut);
        assert_eq!(prepared.replace('\n', ""), line);
        assert!(
            prepared
                .split('\n')
                .all(|l| l.chars().count() <= MAX_LINE_CHARS)
        );
        // Nothing to fold: borrowed, byte for byte.
        let (same, _) = prepare("a\nb\n", 0);
        assert!(matches!(same, Cow::Borrowed("a\nb\n")));
    }

    const LONG_SECRET: &str = "SeCrEt-0123456789-ABCDEF-tail"; // 29 bytes, none of `xyz \n`
    const SHORT_SECRET: &str = "QqQq"; // replacing it makes the text longer
    /// One that is a prefix of another: the scrubber replaces the longer first, as the coder's does.
    const NEST_SHORT: &str = "NeSt-0123456789";
    const NEST_LONG: &str = "NeSt-0123456789-longer-tail-9876";
    /// Secrets that start (end) like the replacement text.
    const LIKE_START: &str = "[redactedXYZ-0123456789-secret";
    const LIKE_END: &str = "secret-0123456789-XYZredacted]";

    /// A secret longer than the margin everyone assumes.
    fn big_secret() -> String {
        format!("BiG-{}", "0123456789ABCDEF".repeat(4400)) // 70,404 bytes
    }

    fn scrubber(t: &str) -> String {
        let big = big_secret();
        let mut out = t.to_owned();
        // Longest first, as a redactor sorts its needles.
        for secret in [
            big.as_str(),
            NEST_LONG,
            LONG_SECRET,
            LIKE_START,
            LIKE_END,
            NEST_SHORT,
            SHORT_SECRET,
        ] {
            if out.contains(secret) {
                out = out.replace(secret, "[redacted]");
            }
        }
        out
    }

    /// A text of filler with no secret in it, `n` bytes long.
    fn filler(n: usize, lines: bool) -> String {
        let unit = if lines { "xyxz yz\n" } else { "xyxzyzx" };
        unit.repeat(n / unit.len() + 1)[..n].to_owned()
    }

    /// No 8-byte window of a secret is in `text`: a window cannot be in filler by chance (the
    /// replacement `[redacted]` holds none of them either).
    fn assert_no_part(text: &str, what: &str) {
        let big = big_secret();
        for secret in [
            LONG_SECRET,
            SHORT_SECRET,
            NEST_LONG,
            LIKE_START,
            LIKE_END,
            big.as_str(),
        ] {
            let bytes = secret.as_bytes();
            let edges = [
                &bytes[..bytes.len().min(300)],
                &bytes[bytes.len().saturating_sub(300)..],
            ];
            for part in edges {
                for w in part.windows(part.len().min(8)) {
                    let w = std::str::from_utf8(w).unwrap();
                    assert!(
                        !text.contains(w) || "[redacted]".contains(w),
                        "{what}: `{w}` is left in the text"
                    );
                }
            }
        }
    }

    /// The reviewer's case: many secrets early shrink the scrubbed text, and the first part of a
    /// secret ends the text, which was cut there. Cutting the scrubbed text at its own length cannot
    /// find it; the end is cut back by what scrubbing does at the raw offset.
    #[test]
    fn the_end_of_a_first_part_keeps_no_part_of_a_secret_that_was_cut() {
        let mut text = format!("{LONG_SECRET}\n").repeat(3000);
        text.push_str(&filler(300_000, true));
        text.push_str(&LONG_SECRET[..20]);
        let out = scrub_open_end(&text, scrubber, SCRUB_OVERLAP);
        assert_no_part(&out, "the capture's end");
        assert!(out.starts_with("[redacted]\n[redacted]\n"), "scrubbed");
        assert!(out.len() > 200_000, "the rest is kept: {}", out.len());
    }

    /// A secret longer than the margin: its first 70 KiB-minus-the-margin would be kept by a
    /// margin that is only assumed, and the margin the caller states (the longest value) keeps none.
    #[test]
    fn a_secret_longer_than_the_default_margin_needs_the_margin_the_caller_states() {
        let big = big_secret();
        // The text ends 66,000 bytes into the secret: more than 64 KiB, less than it all.
        let mut text = filler(200_000, true);
        text.push_str(&big[..66_000]);
        let weak = scrub_open_end(&text, scrubber, SCRUB_OVERLAP);
        assert!(
            weak.contains("BiG-0123456789"),
            "the default margin keeps its start"
        );
        let strong = scrub_open_end(&text, scrubber, big.len());
        assert_no_part(&strong, "the stated margin");
        assert!(strong.len() > 100_000, "the filler before it is kept");
        // The same at the start of a last part.
        let mut text = big[big.len() - 66_000..].to_owned();
        text.push_str(&filler(200_000, true));
        let strong = scrub_open_start(&text, scrubber, big.len());
        assert_no_part(&strong, "the start, stated margin");
        assert!(strong.len() > 100_000);
    }

    #[test]
    fn a_secret_across_the_margin_is_replaced_not_cut_and_nothing_else_is_lost() {
        for lines in [false, true] {
            for offset in [-40i64, -29, -15, -1, 1, 15] {
                let mut text = filler(200_000, lines);
                let at = (text.len() as i64 - SCRUB_OVERLAP as i64 + offset) as usize;
                text.replace_range(at..at + LONG_SECRET.len(), LONG_SECRET);
                let out = scrub_open_end(&text, scrubber, SCRUB_OVERLAP);
                assert_no_part(&out, &format!("offset {offset}"));
                assert!(out.len() > 100_000);
            }
        }
    }

    /// A secret that starts like the replacement (`[red...`) agrees with it for a few bytes where the
    /// two scrubs diverge: the agreement is backed off, so none of it is kept. Mirrored at the start.
    #[test]
    fn a_secret_that_starts_or_ends_like_the_replacement_keeps_none_of_itself() {
        for secret in [LIKE_START, LIKE_END] {
            for cut_in in [3, 8, 14, secret.len() - 1] {
                let mut text = filler(200_000, true);
                let at = text.len() - SCRUB_OVERLAP - cut_in;
                text.replace_range(at..at + secret.len(), secret);
                let out = scrub_open_end(&text, scrubber, SCRUB_OVERLAP);
                assert_no_part(&out, secret);
                // Every `[redacted` is a whole replacement: none is the start of the secret.
                assert_eq!(
                    out.matches("[redacted").count(),
                    out.matches("[redacted]").count()
                );
                let mut text = filler(200_000, true);
                let at = SCRUB_OVERLAP - cut_in;
                text.replace_range(at..at + secret.len(), secret);
                let out = scrub_open_start(&text, scrubber, SCRUB_OVERLAP);
                assert_no_part(&out, secret);
                assert_eq!(
                    out.matches("redacted]").count(),
                    out.matches("[redacted]").count()
                );
            }
        }
    }

    #[test]
    fn a_short_secret_that_makes_the_text_longer_leaves_no_part_either() {
        let mut text = format!("{SHORT_SECRET}\n").repeat(40_000);
        text.push_str(&filler(1000, true));
        text.push_str(&LONG_SECRET[..20]);
        let out = scrub_open_end(&text, scrubber, SCRUB_OVERLAP);
        assert_no_part(&out, "grown");
        assert!(
            out.len() > text.len() - SCRUB_OVERLAP,
            "scrubbed text is longer"
        );
    }

    #[test]
    fn the_start_of_a_last_part_keeps_no_part_of_a_secret_that_was_cut() {
        for lines in [false, true] {
            let mut text = LONG_SECRET[9..].to_owned();
            text.push_str(&format!("{LONG_SECRET}\n").repeat(3000));
            text.push_str(&filler(100_000, lines));
            let out = scrub_open_start(&text, scrubber, SCRUB_OVERLAP);
            assert_no_part(&out, "the start");
            assert!(out.len() > 90_000, "{}", out.len());
        }
    }

    #[test]
    fn a_text_shorter_than_the_margin_keeps_nothing_of_its_end() {
        assert_eq!(scrub_open_end("short", scrubber, SCRUB_OVERLAP), "");
        assert_eq!(scrub_open_start("short", scrubber, SCRUB_OVERLAP), "");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// Whatever the secrets and the cut: random filler with secrets at random offsets, and a
        /// random cut, never leaves an 8-byte part of one at the end (the start) of what is kept.
        /// The secrets are of both lengths, nested (one is a prefix of another), starting or ending
        /// like the replacement, and one of 70 KiB (the margin stated is its length). **Half of the
        /// cases (`straddle`) put one across the point the margin cuts at, by construction**, and the
        /// others place them anywhere, a cut inside one then being by chance. Filler holds only `x`,
        /// `y`, `z`, space and newline, so a window of a secret is not there by chance. Secrets sit on
        /// a 64-byte grid from byte 64 (one to a slot: two overlapping are no secret), so that the
        /// first bytes are filler and are kept.
        #[test]
        fn no_part_of_a_secret_survives_a_cut_at_either_end(
            len in 160_000usize..260_000,
            places in proptest::collection::vec((0usize..260_000, 0usize..8), 0..300),
            cut in 150_000usize..260_000,
            lines in proptest::bool::ANY,
            straddle in proptest::bool::ANY,
            kind in 0usize..8,
            inside in 0usize..400,
        ) {
            let big = big_secret();
            let secrets = [
                LONG_SECRET, SHORT_SECRET, NEST_SHORT, NEST_LONG, LIKE_START, LIKE_END,
                big.as_str(), LONG_SECRET,
            ];
            let margin = big.len();
            let at = cut.min(len);
            // Place on the text for the end (the slice [..at]) and, mirrored, for the start.
            let place = |text: &mut String, wanted: Vec<(usize, usize)>| {
                let mut used = std::collections::HashSet::new();
                for (pos, k) in wanted {
                    let secret = secrets[k];
                    let pos = (pos / 64).max(1) * 64;
                    let slots = secret.len() / 64 + 1;
                    if (pos / 64..pos / 64 + slots).any(|s| used.contains(&s))
                        || pos + secret.len() + 64 > text.len()
                    {
                        continue;
                    }
                    used.extend(pos / 64..pos / 64 + slots);
                    text.replace_range(pos..pos + secret.len(), secret);
                }
            };
            let mut wanted: Vec<(usize, usize)> = places.clone();
            let secret = secrets[kind];
            if straddle {
                // Across the point a keeper cuts at: `margin` before the end of the slice.
                let from = at.saturating_sub(margin + inside % (secret.len() + 1));
                wanted.insert(0, (from, kind));
            }
            let mut head = filler(at, lines);
            place(&mut head, wanted.clone());
            let out = scrub_open_end(&head, scrubber, margin);
            assert_no_part(&out, "end");
            assert!(out.starts_with(&head[..32]), "the first bytes are kept");
            // The start: the same slice, cut so that the margin falls inside a secret.
            let mut tail = filler(at, lines);
            let mut wanted_tail: Vec<(usize, usize)> = places;
            if straddle {
                wanted_tail.insert(0, ((margin.saturating_sub(inside % (secret.len() + 1))).max(64), kind));
            }
            place(&mut tail, wanted_tail);
            let out = scrub_open_start(&tail, scrubber, margin);
            assert_no_part(&out, "start");
            assert!(out.ends_with(&tail[tail.len() - 32..]), "the last bytes are kept");
        }
    }

    /// A host with no keeper but a scrubbing layer: the text is scrubbed before it is folded at
    /// column 2,000, so a secret across the fold is replaced and not split into two lines.
    #[tokio::test]
    async fn keep_long_without_a_keeper_scrubs_before_it_folds() {
        let sink: adam_runtime::DynEventSink =
            std::sync::Arc::new(adam_runtime::CollectingSink::new());
        let line = format!("{}{LONG_SECRET}{}\n", "a".repeat(1990), "b".repeat(3000));
        let text = line.repeat(30);
        let ctx = ToolCtx::detached("t", "c1", sink)
            .with_kept(MAX_RUN_KEPT_BYTES, std::sync::Arc::default())
            .with_scrub(|t| t.replace(LONG_SECRET, "[redacted]"));
        let (_, kept) = keep_long(&ctx, &text).await;
        let kept = kept.expect("kept in the state").text;
        assert!(
            !kept.contains("SeCrEt") && !kept.contains("ABCDEF-tail"),
            "a part of the secret is in the kept text"
        );
        assert!(kept.replace('\n', "").contains("[redacted]"));
    }

    /// An id never has a `.` or a `/` in it, nor a NUL: it is a file name.
    #[test]
    fn an_id_with_a_dot_a_slash_or_a_nul_is_not_an_id() {
        for bad in [
            "o-../../../etc/pas",
            "o-../../../etc/pa",
            "o-./aaaaaaaaaaaaaa",
            "o-.aaaaaaaaaaaaaaa",
            "o-aaaaaaaaaaaaaaa/",
            "o-\0aaaaaaaaaaaaaaa",
            "o-aaaaaaaaaaaaaaa\0",
        ] {
            assert!(!is_output_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_text_over_the_bound_is_cut_and_says_so() {
        let text = "x\n".repeat(MAX_KEPT_BYTES / 2 + 10);
        let (prepared, cut) = prepare(&text, 0);
        assert!(cut);
        assert!(prepared.starts_with("x\nx\n") && prepared.contains("(output cut: only the first"));
    }

    /// The secret of the streaming tests, and what replaces it.
    const STREAM_SECRET: &str = "sk-live-SECRET-0123456789";

    fn stream_scrub(text: &str) -> String {
        text.replace(STREAM_SECRET, "[redacted]")
    }

    /// How many bytes at the end of `text` are the start of the secret.
    fn stream_partial(text: &str) -> usize {
        (1..STREAM_SECRET.len())
            .rev()
            .find(|&n| text.as_bytes().ends_with(&STREAM_SECRET.as_bytes()[..n]))
            .unwrap_or(0)
    }

    /// Feed `chunks` through a scrubber with `margin`, releasing after each read when `releasing`,
    /// and return what was written and the most it held.
    fn stream(chunks: &[Vec<u8>], margin: usize, releasing: bool) -> (String, usize) {
        let mut scrubber = StreamScrubber::with_small_margin(margin);
        let mut out = String::new();
        let mut most = 0;
        for chunk in chunks {
            out.push_str(&scrubber.push(chunk, &stream_scrub));
            if releasing {
                out.push_str(&scrubber.release(&stream_scrub, &stream_partial));
            }
            most = most.max(scrubber.held());
        }
        out.push_str(&scrubber.finish(&stream_scrub));
        (out, most)
    }

    fn chunked(bytes: &[u8], sizes: &[usize]) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        let mut at = 0;
        let mut sizes = sizes.iter().cycle();
        while at < bytes.len() {
            let n = (*sizes.next().unwrap_or(&1)).clamp(1, bytes.len() - at);
            chunks.push(bytes[at..at + n].to_vec());
            at += n;
        }
        chunks
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// Whatever the reads, what is written is the scrub of the whole.
        #[test]
        fn stream_scrubber_equals_whole_scrub_for_any_chunking(
            pieces in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::strategy::Just(STREAM_SECRET.to_owned()),
                    proptest::strategy::Just(STREAM_SECRET[..9].to_owned()),
                    proptest::strategy::Just(STREAM_SECRET[9..].to_owned()),
                    proptest::strategy::Just("\n".to_owned()),
                    "[a-z é€\\n]{0,40}",
                ],
                0..60,
            ),
            sizes in proptest::collection::vec(1usize..37, 1..8),
            margin in 40usize..90,
            releasing in proptest::bool::ANY,
        ) {
            let text: String = pieces.concat();
            let (out, _) = stream(&chunked(text.as_bytes(), &sizes), margin, releasing);
            proptest::prop_assert_eq!(out, stream_scrub(&text));
        }

        /// Text that holds no secret is never held for more than two margins.
        #[test]
        fn stream_scrubber_holds_at_most_two_margins(
            text in "[a-z \\n]{0,600}",
            sizes in proptest::collection::vec(1usize..50, 1..6),
            margin in 40usize..90,
        ) {
            let (out, most) = stream(&chunked(text.as_bytes(), &sizes), margin, false);
            proptest::prop_assert_eq!(out, text);
            proptest::prop_assert!(most <= 2 * margin, "{most} > 2 * {margin}");
        }

        /// The lines a streamed text completes are the lines `prepare` makes of it.
        #[test]
        fn line_folder_makes_what_prepare_makes(
            text in "[a-z\\n\\x00é]{0,9000}",
            sizes in proptest::collection::vec(1usize..3000, 1..5),
        ) {
            let mut folder = LineFolder::new();
            let mut out = String::new();
            let mut at = 0;
            let chars: Vec<char> = text.chars().collect();
            let mut sizes = sizes.iter().cycle();
            while at < chars.len() {
                let n = (*sizes.next().unwrap_or(&1)).min(chars.len() - at);
                out.push_str(&folder.push(&chars[at..at + n].iter().collect::<String>()));
                at += n;
            }
            out.push_str(&folder.finish());
            proptest::prop_assert_eq!(out, prepare(&text, 0).0.into_owned());
        }
    }

    /// A secret that two reads cut in half: nothing of it is written after the first read, and the
    /// whole is replaced after the second. (The default margin: no bytes beyond it are needed.)
    #[test]
    fn a_secret_split_across_reads_is_never_written() {
        let mut scrubber = StreamScrubber::new(SCRUB_OVERLAP);
        let first = format!("ready\nkey={}", &STREAM_SECRET[..12]);
        let mut written = scrubber.push(first.as_bytes(), &stream_scrub);
        written.push_str(&scrubber.release(&stream_scrub, &stream_partial));
        assert_eq!(written, "ready\nkey=", "the start of the secret stays held");
        assert!(!written.contains("sk-"), "{written}");
        let mut more = scrubber.push(STREAM_SECRET[12..].as_bytes(), &stream_scrub);
        more.push_str(&scrubber.release(&stream_scrub, &stream_partial));
        more.push_str(&scrubber.push(b"\nafter\n", &stream_scrub));
        more.push_str(&scrubber.finish(&stream_scrub));
        assert_eq!(written + &more, "ready\nkey=[redacted]\nafter\n");
    }

    /// A character cut by a read is written whole, and a byte that is not UTF-8 is U+FFFD.
    #[test]
    fn a_character_split_across_reads_is_written_whole() {
        let mut scrubber = StreamScrubber::new(SCRUB_OVERLAP);
        let euro = "€".as_bytes();
        let mut out = scrubber.push(&euro[..1], &stream_scrub);
        out.push_str(&scrubber.push(&euro[1..], &stream_scrub));
        out.push_str(&scrubber.push(&[0xff, b'a'], &stream_scrub));
        out.push_str(&scrubber.finish(&stream_scrub));
        assert_eq!(out, "€\u{fffd}a");
    }
}
