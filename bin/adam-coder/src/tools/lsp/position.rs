//! Positions and addresses, the two things a language server and a model say differently.
//!
//! The model gives a **line** (1-based) and the **symbol** on it (text), because it counts columns
//! badly. The server wants a line (0-based) and a column in the encoding it agreed to: UTF-8 bytes,
//! UTF-16 code units (the default of the protocol) or characters. This module converts both ways,
//! and between a path and a `file://` URI.

use std::path::{Path, PathBuf};

/// How a server counts the columns of a line: what it answered in `positionEncoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// Bytes of the UTF-8 text.
    Utf8,
    /// UTF-16 code units: the protocol's default, and what a server that says nothing uses.
    #[default]
    Utf16,
    /// Characters (Unicode scalar values).
    Utf32,
}

impl Encoding {
    /// The encoding a server named (`utf-8`, `utf-16`, `utf-32`), or `None` for anything else.
    pub fn from_lsp(name: &str) -> Option<Self> {
        match name {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            "utf-32" => Some(Self::Utf32),
            _ => None,
        }
    }

    /// The units of `c` in this encoding.
    fn units(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

/// The column the server wants for the character at index `chars` of `line`: the units of the
/// characters before it. A `chars` past the end of the line is the end of the line.
pub fn to_server(line: &str, chars: usize, encoding: Encoding) -> u32 {
    let units: usize = line.chars().take(chars).map(|c| encoding.units(c)).sum();
    u32::try_from(units).unwrap_or(u32::MAX)
}

/// The index, in characters, of the column `offset` the server gave for `line`. A column inside a
/// character (the second half of a surrogate pair) is the character; one past the end of the line
/// is the line's length.
pub fn to_chars(line: &str, offset: u32, encoding: Encoding) -> usize {
    let mut units = 0_usize;
    let wanted = offset as usize;
    for (at, c) in line.chars().enumerate() {
        let next = units + encoding.units(c);
        if wanted < next {
            return at;
        }
        units = next;
    }
    line.chars().count()
}

/// What [`find_symbol`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    /// The character index of the start of the match.
    At(usize),
    /// The symbol is not on the line.
    Missing,
    /// It is there, but fewer times than `occurrence` asked.
    Fewer(usize),
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The start (in characters) of the `occurrence`-th (1-based) match of `symbol` on `line`.
///
/// A symbol made of word characters matches whole words first (`id` is not found inside `valid`
/// while a whole `id` is on the line); with no whole word it matches as text.
pub fn find_symbol(line: &str, symbol: &str, occurrence: usize) -> Found {
    let hay: Vec<char> = line.chars().collect();
    let needle: Vec<char> = symbol.chars().collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return Found::Missing;
    }
    let mut all = Vec::new();
    let mut whole = Vec::new();
    for start in 0..=(hay.len() - needle.len()) {
        if hay[start..start + needle.len()] != needle[..] {
            continue;
        }
        all.push(start);
        let before = start.checked_sub(1).and_then(|at| hay.get(at));
        let after = hay.get(start + needle.len());
        if !before.is_some_and(|c| is_word(*c)) && !after.is_some_and(|c| is_word(*c)) {
            whole.push(start);
        }
    }
    let word_like = needle.iter().all(|c| is_word(*c));
    let found = if word_like && !whole.is_empty() {
        whole
    } else {
        all
    };
    match found.get(occurrence.saturating_sub(1)) {
        Some(at) => Found::At(*at),
        None if found.is_empty() => Found::Missing,
        None => Found::Fewer(found.len()),
    }
}

/// The `file://` URI of an absolute `path`, percent-encoded.
pub fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(char::from(*byte));
            }
            other => {
                uri.push_str(&format!("%{other:02X}"));
            }
        }
    }
    uri
}

/// The path a `file://` URI names (percent-decoded), or `None` for any other kind of address.
pub fn path_of_uri(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    let rest = uri.strip_prefix("file://")?;
    // `file://localhost/x` and `file:///x`: the authority is empty or this machine.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && let Some(hex) = rest.get(at + 1..at + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A line with an accent (2 bytes, 1 unit), a CJK character (3 bytes, 1 unit) and an emoji
    /// (4 bytes, 2 UTF-16 units), then the symbol.
    const LINE: &str = "é世😀 value";

    #[test]
    fn columns_are_counted_in_the_encoding_the_server_agreed() {
        // The symbol starts at character 5 (é, 世, 😀, a space, then `v` at index 4)
        let at = LINE.chars().position(|c| c == 'v').unwrap();
        assert_eq!(at, 4);
        assert_eq!(to_server(LINE, at, Encoding::Utf32), 4);
        assert_eq!(to_server(LINE, at, Encoding::Utf16), 1 + 1 + 2 + 1);
        assert_eq!(to_server(LINE, at, Encoding::Utf8), 2 + 3 + 4 + 1);
        // Past the end is the end.
        assert_eq!(to_server("ab", 9, Encoding::Utf16), 2);
    }

    #[test]
    fn a_column_from_the_server_is_a_character_again() {
        for encoding in [Encoding::Utf8, Encoding::Utf16, Encoding::Utf32] {
            for chars in 0..=LINE.chars().count() {
                let column = to_server(LINE, chars, encoding);
                assert_eq!(to_chars(LINE, column, encoding), chars, "{encoding:?}");
            }
        }
        // The second half of a surrogate pair is the emoji; past the end is the length.
        assert_eq!(to_chars(LINE, 3, Encoding::Utf16), 2);
        assert_eq!(to_chars(LINE, 3, Encoding::Utf8), 1);
        assert_eq!(to_chars("ab", 50, Encoding::Utf16), 2);
    }

    #[test]
    fn a_symbol_is_found_as_a_whole_word_first_and_by_occurrence() {
        let line = "let valid = id + id; // id";
        assert_eq!(find_symbol(line, "id", 1), Found::At(12));
        assert_eq!(find_symbol(line, "id", 2), Found::At(17));
        assert_eq!(find_symbol(line, "id", 3), Found::At(24));
        assert_eq!(find_symbol(line, "id", 4), Found::Fewer(3));
        // With no whole word, text matches.
        assert_eq!(find_symbol("let valid = 1;", "id", 1), Found::At(7));
        assert_eq!(find_symbol("let valid = 1;", "nothing", 1), Found::Missing);
        assert_eq!(find_symbol("abc", "", 1), Found::Missing);
        // A symbol with punctuation is matched as text.
        assert_eq!(find_symbol("a.b(c)", ".b(", 1), Found::At(1));
    }

    #[test]
    fn a_path_and_its_uri_are_each_others_inverse() {
        let path = Path::new("/work/my project/src/é#.rs");
        let uri = file_uri(path);
        assert!(uri.starts_with("file:///work/my%20project/"), "{uri}");
        assert!(!uri.contains(' ') && !uri.contains('#'));
        assert_eq!(path_of_uri(&uri).unwrap(), path);
        assert_eq!(
            path_of_uri("file://localhost/a/b").unwrap(),
            Path::new("/a/b")
        );
        for refused in [
            "http://x/y",
            "file://host/x",
            "untitled:Untitled-1",
            "file:x",
        ] {
            assert!(path_of_uri(refused).is_none(), "{refused}");
        }
    }
}
