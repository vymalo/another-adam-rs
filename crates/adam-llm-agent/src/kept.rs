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
            "(output cut: only the first {} of {} bytes are kept)",
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
            "; only the first {} of {total_bytes} bytes are kept",
            info.bytes
        );
    }
    line.push_str("]\n");
    line
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

/// `text`, which is only the **first part** of a longer output, scrubbed with `scrub` and cut back
/// so that no part of a secret is left at its end.
///
/// The end of such a text may be the first half of a secret that `scrub` cannot recognise, and the
/// scrubbed length says nothing about where in the raw text a cut falls (a replacement is shorter or
/// longer than the value). So the text is scrubbed whole, and again without its last
/// [`SCRUB_OVERLAP`] bytes (no secret is longer): what the two agree on is the text up to the first
/// place where a secret straddles the new end, which is replaced in the one and cut in the other.
/// The result is scrubbed, and never longer than the text scrubbed whole.
pub fn scrub_open_end(text: &str, scrub: impl Fn(&str) -> String) -> String {
    let mut keep = text.len().saturating_sub(SCRUB_OVERLAP);
    while !text.is_char_boundary(keep) {
        keep -= 1;
    }
    let whole = scrub(text);
    let part = scrub(&text[..keep]);
    let agree = whole
        .bytes()
        .zip(part.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let mut end = agree;
    while !whole.is_char_boundary(end) {
        end -= 1;
    }
    let mut whole = whole;
    whole.truncate(end);
    whole
}

/// [`scrub_open_end`] for a text that is only the **last part** of a longer output: scrubbed, and
/// without a part of a secret at its start.
pub fn scrub_open_start(text: &str, scrub: impl Fn(&str) -> String) -> String {
    let mut from = SCRUB_OVERLAP.min(text.len());
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
    let mut start = whole.len() - agree;
    while !whole.is_char_boundary(start) {
        start += 1;
    }
    whole[start..].to_owned()
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
    let (scrubbed, omitted) = scrubbed_for_keeping(text, omitted, |t| ctx.scrub(t));
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
/// at its end by [`scrub_open_end`]. With the `omitted` to keep: what was left out, counted again
/// for what the cut back took (never 0 for a text that was cut).
pub fn scrubbed_for_keeping(
    text: &str,
    omitted: u64,
    scrub: impl Fn(&str) -> String,
) -> (String, u64) {
    if omitted == 0 {
        return (scrub(text), 0);
    }
    let scrubbed = scrub_open_end(text, scrub);
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
}

type Scrub = Arc<dyn Fn(&str) -> String + Send + Sync>;

impl MemoryKept {
    /// An empty one.
    pub fn new() -> Self {
        Self::default()
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
        let (scrubbed, omitted) = scrubbed_for_keeping(text, omitted, |t| match &self.scrub {
            Some(scrub) => scrub(t),
            None => t.to_owned(),
        });
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
        page.contains("(output cut: only the first") && page.contains(" bytes are kept)"),
        "{page}"
    );
    assert!(
        page.contains(&format!(" of {} bytes are kept)", first_part.len() + 996)),
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

    fn scrubber(t: &str) -> String {
        t.replace(LONG_SECRET, "[redacted]")
            .replace(SHORT_SECRET, "[redacted]")
    }

    /// A text of filler with no secret in it, `n` bytes long.
    fn filler(n: usize, lines: bool) -> String {
        let unit = if lines { "xyxz yz\n" } else { "xyxzyzx" };
        unit.repeat(n / unit.len() + 1)[..n].to_owned()
    }

    /// No 8-byte window of a secret is in `text`: a window cannot be in filler by chance.
    fn assert_no_part(text: &str, what: &str) {
        for secret in [LONG_SECRET, SHORT_SECRET] {
            for w in secret.as_bytes().windows(secret.len().min(8)) {
                let w = std::str::from_utf8(w).unwrap();
                assert!(!text.contains(w), "{what}: `{w}` is left in the text");
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
        let out = scrub_open_end(&text, scrubber);
        assert_no_part(&out, "the capture's end");
        assert!(out.starts_with("[redacted]\n[redacted]\n"), "scrubbed");
        assert!(out.len() > 200_000, "the rest is kept: {}", out.len());
    }

    #[test]
    fn a_secret_across_the_margin_is_replaced_not_cut_and_nothing_else_is_lost() {
        for lines in [false, true] {
            for offset in [-40i64, -29, -15, -1, 1, 15] {
                let mut text = filler(200_000, lines);
                let at = (text.len() as i64 - SCRUB_OVERLAP as i64 + offset) as usize;
                text.replace_range(at..at + LONG_SECRET.len(), LONG_SECRET);
                let out = scrub_open_end(&text, scrubber);
                assert_no_part(&out, &format!("offset {offset}"));
                assert!(out.len() > 100_000);
            }
        }
    }

    #[test]
    fn a_short_secret_that_makes_the_text_longer_leaves_no_part_either() {
        let mut text = format!("{SHORT_SECRET}\n").repeat(40_000);
        text.push_str(&filler(1000, true));
        text.push_str(&LONG_SECRET[..20]);
        let out = scrub_open_end(&text, scrubber);
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
            let out = scrub_open_start(&text, scrubber);
            assert_no_part(&out, "the start");
            assert!(out.len() > 90_000, "{}", out.len());
        }
    }

    #[test]
    fn a_text_shorter_than_the_margin_keeps_nothing_of_its_end() {
        assert_eq!(scrub_open_end("short", scrubber), "");
        assert_eq!(scrub_open_start("short", scrubber), "");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(48))]

        /// Whatever the secrets and the cut: random filler with secrets of both lengths at random
        /// offsets, and a random cut that may fall inside one, never leaves an 8-byte part of one at
        /// the end (the start) of what is kept. Filler holds only `x`, `y`, `z`, space and newline, so
        /// an 8-byte window of a secret is not there by chance.
        #[test]
        fn no_part_of_a_secret_survives_a_cut_at_either_end(
            len in 70_000usize..160_000,
            places in proptest::collection::vec((0usize..160_000, proptest::bool::ANY), 0..400),
            cut in 66_000usize..160_000,
            lines in proptest::bool::ANY,
        ) {
            let mut text = filler(len, lines);
            // On a 64-byte grid, one secret to a slot: no two overlap (a half-overwritten secret
            // is not a secret, and would be left by any scrubber).
            let mut used = std::collections::HashSet::new();
            for (at, long) in places {
                let at = at / 64 * 64;
                let secret = if long { LONG_SECRET } else { SHORT_SECRET };
                if used.insert(at) && at + secret.len() <= text.len() {
                    text.replace_range(at..at + secret.len(), secret);
                }
            }
            let at = cut.min(text.len());
            assert_no_part(&scrub_open_end(&text[..at], scrubber), "end");
            assert_no_part(&scrub_open_start(&text[text.len() - at..], scrubber), "start");
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
}
