//! One convention for every tool that reads: `offset` and `limit` in, a page and one last line out.
//!
//! The last line of a result says what is left and where the next page starts, so a model never
//! guesses and the same call gives the same bytes (a repeated read is a cached read):
//!
//! ```text
//! [more: 3123 lines left; next page: offset=2001]
//! [end: 5123 lines]
//! [end: 5123 lines; no newline at the end of the file]
//! ```
//!
//! Offsets count from 1. A negative offset counts from the end (`-50`: the last 50 items).
//! [`page_lines`] pages a text stream, [`window`] and [`render_items`] page a sorted list, and
//! [`backstop`] is the loop's own cap on any tool result, paged or not.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, BufRead};

/// Most lines of one page of text.
pub const PAGE_LINES: u32 = 2000;
/// Most rendered bytes of one page of text (a page always has at least one line).
pub const PAGE_BYTES: usize = 50 * 1024;
/// Longest line shown, in characters; a longer one is cut and says how long it was.
pub const MAX_LINE_CHARS: usize = 2000;
/// Most bytes of any tool result the loop lets into the history.
pub const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
/// How far back a negative offset may reach when paging a stream.
pub const MAX_BACK: i64 = 10_000;

/// The width of a line number in the numbered form (`{n:>6}\t`).
const NUMBER_WIDTH: usize = 6;
/// A NUL in this many first bytes of a stream makes it binary.
const BINARY_PROBE: u64 = 8 * 1024;

/// Why a page could not be made.
#[derive(Debug)]
pub enum PageError {
    /// The stream could not be read.
    Io(io::Error),
    /// A NUL byte: the text is binary.
    Binary,
    /// The offset is after the last item.
    PastEnd {
        /// The offset asked for.
        offset: i64,
        /// How many items there are.
        total: u64,
    },
    /// The offset or the limit is not usable; the text says why and is for the model.
    Window(String),
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read: {e}"),
            Self::Binary => f.write_str("binary content"),
            Self::PastEnd { offset, total } => {
                write!(f, "{total} items: offset {offset} is past the end")
            }
            Self::Window(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for PageError {}

/// The `offset` and `limit` a call gave, checked: `(offset, limit)` with the default offset 1 and
/// the limit defaulting to, and clamped at, `page`.
///
/// # Errors
///
/// An offset of 0 (items count from 1) or a limit below 1, as text for the model.
pub fn window_args(
    offset: Option<i64>,
    limit: Option<i64>,
    page: u32,
) -> Result<(i64, u32), String> {
    let offset = offset.unwrap_or(1);
    if offset == 0 {
        return Err(
            "offset counts from 1; a negative offset counts from the end (-50: the last 50)"
                .to_owned(),
        );
    }
    let limit = match limit {
        None => page,
        Some(n) if n < 1 => return Err("limit must be at least 1".to_owned()),
        Some(n) => u32::try_from(n).unwrap_or(u32::MAX).min(page),
    };
    Ok((offset, limit))
}

/// `[more: <left> <unit> left; next page: offset=<next>]`
pub fn more_line(left: u64, unit: &str, next_offset: u64) -> String {
    format!("[more: {left} {unit} left; next page: offset={next_offset}]")
}

/// `[end: <total> <unit>]`, with each of `notes` after a `; `.
pub fn end_line(total: u64, unit: &str, notes: &[&str]) -> String {
    let mut line = format!("[end: {total} {unit}");
    for note in notes {
        line.push_str("; ");
        line.push_str(note);
    }
    line.push(']');
    line
}

/// One line of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageLine {
    /// Its number, from 1.
    pub number: u64,
    /// Its text without the line end (a `\r` stays), cut at [`MAX_LINE_CHARS`].
    pub text: String,
}

/// A page of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The lines of the page.
    pub lines: Vec<PageLine>,
    /// How many lines the stream has.
    pub total: u64,
    /// Whether the last line of the stream has no line end.
    pub no_final_newline: bool,
    numbered: bool,
}

impl Page {
    /// The number of the line after the page, when there is one.
    pub fn next_offset(&self) -> Option<u64> {
        let next = self.lines.last().map_or(1, |l| l.number + 1);
        (next <= self.total).then_some(next)
    }

    /// The page as text: its lines, then the final line (which has no line end).
    pub fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            if self.numbered {
                let _ = writeln!(out, "{:>NUMBER_WIDTH$}\t{}", line.number, line.text);
            } else {
                let _ = writeln!(out, "{}", line.text);
            }
        }
        match self.next_offset() {
            Some(next) => out.push_str(&more_line(self.total - next + 1, "lines", next)),
            None if self.no_final_newline => out.push_str(&end_line(
                self.total,
                "lines",
                &["no newline at the end of the file"],
            )),
            None => out.push_str(&end_line(self.total, "lines", &[])),
        }
        out
    }
}

struct RawLine {
    text: String,
    ends_with_newline: bool,
}

/// The next line of `reader`, or `None` at the end. With `keep` unset the line is only skipped.
/// A line is read in chunks, so one with no end costs no more memory than a short one.
fn next_line(
    reader: &mut impl BufRead,
    keep: bool,
    probed: &mut u64,
) -> Result<Option<RawLine>, PageError> {
    let mut kept: Vec<u8> = Vec::new();
    let mut chars: u64 = 0;
    let mut any = false;
    let ends_with_newline = loop {
        let buf = reader.fill_buf().map_err(PageError::Io)?;
        if buf.is_empty() {
            break false;
        }
        any = true;
        let (used, found) = match buf.iter().position(|&b| b == b'\n') {
            Some(at) => (at, true),
            None => (buf.len(), false),
        };
        let chunk = &buf[..used];
        if *probed < BINARY_PROBE {
            let probe = usize::try_from(BINARY_PROBE - *probed)
                .unwrap_or(usize::MAX)
                .min(chunk.len());
            if chunk[..probe].contains(&0) {
                return Err(PageError::Binary);
            }
        }
        *probed = probed.saturating_add(used as u64 + u64::from(found));
        if keep {
            chars += chunk.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u64;
            let room = (MAX_LINE_CHARS * 4).saturating_sub(kept.len());
            kept.extend_from_slice(&chunk[..room.min(chunk.len())]);
        }
        reader.consume(used + usize::from(found));
        if found {
            break true;
        }
    };
    if !any {
        return Ok(None);
    }
    if !keep {
        return Ok(Some(RawLine {
            text: String::new(),
            ends_with_newline,
        }));
    }
    if kept.contains(&0) {
        return Err(PageError::Binary);
    }
    let lossy = String::from_utf8_lossy(&kept);
    let mut text: String = lossy.chars().take(MAX_LINE_CHARS).collect();
    if chars > MAX_LINE_CHARS as u64 {
        let _ = write!(text, "… [line cut: {chars} chars]");
    }
    Ok(Some(RawLine {
        text,
        ends_with_newline,
    }))
}

/// Bytes a line takes when rendered, its line end included.
fn rendered_len(number: u64, text: &str, numbered: bool) -> usize {
    let prefix = if numbered {
        NUMBER_WIDTH.max(number.to_string().len()) + 1
    } else {
        0
    };
    prefix + text.len() + 1
}

/// Page the lines of `reader`: from line `offset` (negative: from the end), at most `limit` lines
/// and [`PAGE_BYTES`] of rendered text (`numbered`: with the number of each line in front), and
/// at least one line. A line over [`MAX_LINE_CHARS`] is cut.
///
/// The whole stream is read, to count its lines. A NUL in the first 8 KiB, or in a line of the
/// page, is [`PageError::Binary`].
///
/// # Errors
///
/// See [`PageError`]. An empty stream is a page with no lines (`total` 0), not an error.
pub fn page_lines(
    mut reader: impl BufRead,
    offset: i64,
    limit: u32,
    numbered: bool,
) -> Result<Page, PageError> {
    if offset == 0 {
        return Err(PageError::Window(
            "offset counts from 1; a negative offset counts from the end".to_owned(),
        ));
    }
    if limit == 0 {
        return Err(PageError::Window("limit must be at least 1".to_owned()));
    }
    if offset < -MAX_BACK {
        return Err(PageError::Window(format!(
            "an offset from the end reaches back at most {MAX_BACK} lines"
        )));
    }
    let limit = limit.min(PAGE_LINES) as usize;
    let mut probed = 0;
    let mut number: u64 = 0;
    let mut last_has_newline = true;
    let mut chosen: Vec<PageLine> = Vec::new();

    if offset > 0 {
        let start = offset.unsigned_abs();
        let mut bytes = 0usize;
        let mut closed = false;
        loop {
            let keep = number + 1 >= start && !closed;
            let Some(line) = next_line(&mut reader, keep, &mut probed)? else {
                break;
            };
            number += 1;
            last_has_newline = line.ends_with_newline;
            if keep {
                let size = rendered_len(number, &line.text, numbered);
                if !chosen.is_empty() && (chosen.len() >= limit || bytes + size > PAGE_BYTES) {
                    closed = true;
                } else {
                    bytes += size;
                    chosen.push(PageLine {
                        number,
                        text: line.text,
                    });
                }
            }
        }
        if start > number.max(1) {
            return Err(PageError::PastEnd {
                offset,
                total: number,
            });
        }
    } else {
        let back = usize::try_from(offset.unsigned_abs()).unwrap_or(usize::MAX);
        let mut ring: VecDeque<PageLine> = VecDeque::new();
        while let Some(line) = next_line(&mut reader, true, &mut probed)? {
            number += 1;
            last_has_newline = line.ends_with_newline;
            if ring.len() == back {
                ring.pop_front();
            }
            ring.push_back(PageLine {
                number,
                text: line.text,
            });
        }
        let mut bytes = 0usize;
        for line in ring {
            let size = rendered_len(line.number, &line.text, numbered);
            if !chosen.is_empty() && (chosen.len() >= limit || bytes + size > PAGE_BYTES) {
                break;
            }
            bytes += size;
            chosen.push(line);
        }
    }
    Ok(Page {
        lines: chosen,
        total: number,
        no_final_newline: number > 0 && !last_has_newline,
        numbered,
    })
}

/// Which items of a list a page shows: `start..end`, 0-based, of `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The first item shown.
    pub start: usize,
    /// One past the last item shown.
    pub end: usize,
    /// How many items there are.
    pub total: usize,
}

impl Window {
    /// The `offset` of the next page, when there is one.
    pub fn next_offset(&self) -> Option<u64> {
        (self.end < self.total).then_some(self.end as u64 + 1)
    }
}

/// The window of a list of `total` items for `offset` (negative: from the end) and `limit`.
///
/// # Errors
///
/// [`PageError::PastEnd`] when the list has items and the offset is after the last one.
pub fn window(total: usize, offset: i64, limit: u32) -> Result<Window, PageError> {
    let start = if offset < 0 {
        total.saturating_sub(usize::try_from(offset.unsigned_abs()).unwrap_or(usize::MAX))
    } else {
        let start = usize::try_from(offset.unsigned_abs() - 1).unwrap_or(usize::MAX);
        if total > 0 && start >= total {
            return Err(PageError::PastEnd {
                offset,
                total: total as u64,
            });
        }
        start.min(total)
    };
    let end = start.saturating_add(limit as usize).min(total);
    Ok(Window { start, end, total })
}

/// The page `window` of `items` as text: one item a line, then the final line (`unit` names the
/// items: `entries`, `paths`, `matches`; `end_notes` go after the count of the `[end: ...]` line).
/// No items give `empty` and `[end: 0 <unit>]`.
pub fn render_items(
    items: &[String],
    window: Window,
    unit: &str,
    empty: &str,
    end_notes: &[&str],
) -> String {
    let mut out = String::new();
    if items.is_empty() {
        out.push_str(empty);
        out.push('\n');
    }
    for item in &items[window.start..window.end] {
        out.push_str(item);
        out.push('\n');
    }
    match window.next_offset() {
        Some(next) => out.push_str(&more_line((window.total - window.end) as u64, unit, next)),
        None => out.push_str(&end_line(window.total as u64, unit, end_notes)),
    }
    out
}

/// The loop's cap on a tool result: at most [`MAX_TOOL_RESULT_BYTES`] of `content` (cut on a
/// character boundary) and a marker that says what was cut. `paged` is whether `tool` takes an
/// `offset`: the marker then says to call it again with a later one.
///
/// A pure function of its arguments, so a replayed run gives the same bytes.
pub fn backstop<'a>(content: &'a str, tool: &str, paged: bool) -> Cow<'a, str> {
    if content.len() <= MAX_TOOL_RESULT_BYTES {
        return Cow::Borrowed(content);
    }
    let mut end = MAX_TOOL_RESULT_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 160);
    out.push_str(&content[..end]);
    let _ = write!(
        out,
        "\n[cut by the loop: {end} of {} bytes shown",
        content.len()
    );
    if paged {
        let _ = write!(
            out,
            ". Call {tool} again with a later offset to read the rest"
        );
    }
    out.push(']');
    Cow::Owned(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("l{i}\n")).collect()
    }

    #[test]
    fn a_page_ends_with_one_final_line() {
        let page = page_lines(lines(5000).as_bytes(), 1, PAGE_LINES, true).unwrap();
        let text = page.render();
        assert_eq!(page.lines.len(), 2000);
        assert!(text.ends_with("[more: 3000 lines left; next page: offset=2001]"));
        let last = page_lines(lines(5000).as_bytes(), 4001, PAGE_LINES, true).unwrap();
        assert!(last.render().ends_with("[end: 5000 lines]"));
    }

    #[test]
    fn a_negative_offset_counts_from_the_end() {
        let page = page_lines(lines(10).as_bytes(), -3, PAGE_LINES, false).unwrap();
        assert_eq!(page.render(), "l8\nl9\nl10\n[end: 10 lines]");
        let all = page_lines(lines(2).as_bytes(), -50, PAGE_LINES, false).unwrap();
        assert_eq!(all.lines.len(), 2);
    }

    #[test]
    fn an_offset_past_the_end_is_an_error() {
        let err = page_lines(lines(3).as_bytes(), 9, PAGE_LINES, true).unwrap_err();
        assert!(matches!(err, PageError::PastEnd { total: 3, .. }), "{err}");
        let empty = page_lines(&b""[..], 1, PAGE_LINES, true).unwrap();
        assert_eq!((empty.lines.len(), empty.total), (0, 0));
        assert!(page_lines(&b""[..], 2, PAGE_LINES, true).is_err());
    }

    #[test]
    fn the_window_of_a_list() {
        let w = window(250, 1, 100).unwrap();
        assert_eq!((w.start, w.end, w.next_offset()), (0, 100, Some(101)));
        let w = window(250, -10, 100).unwrap();
        assert_eq!((w.start, w.end, w.next_offset()), (240, 250, None));
        assert!(window(5, 6, 10).is_err());
        assert!(window(0, 1, 10).is_ok());
    }

    #[test]
    fn the_backstop_cuts_on_a_character_boundary() {
        let text = "é".repeat(MAX_TOOL_RESULT_BYTES);
        let cut = backstop(&text, "read_file", true);
        assert!(cut.contains("[cut by the loop: 65536 of 131072 bytes shown. Call read_file"));
        let plain = backstop(&text, "other", false);
        assert!(plain.ends_with("bytes shown]"), "{plain}");
        assert!(matches!(backstop("short", "t", true), Cow::Borrowed(_)));
    }
}
