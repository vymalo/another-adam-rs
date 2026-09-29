//! Splitting a Markdown file into its YAML frontmatter and its body, and reading the YAML.
//!
//! The rules are those of Claude Code, Copilot and Agent Skills files:
//!
//! * The frontmatter is optional. It exists only when the **first line** is `---`; the next
//!   line that is `---` closes it.
//! * A first line of `---` with no closing `---` is an error ([`SplitError::Unterminated`]),
//!   never a silent "no frontmatter": the file would otherwise turn into a prompt that starts
//!   with a YAML block.
//! * A UTF-8 byte-order mark is skipped, and CRLF line endings work (a delimiter line may end
//!   with `\r` or trailing spaces).
//! * A file with no frontmatter is all body.

use adam_error::{Classify, ErrorClass};
use serde::de::DeserializeOwned;

/// A Markdown file split at its frontmatter. Both parts borrow from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split<'a> {
    /// The YAML between the two `---` lines, or `None` when the file has no frontmatter.
    pub frontmatter: Option<&'a str>,
    /// The 1-based line of the first YAML line: `2` when there is frontmatter, `0` when there is
    /// none.
    pub frontmatter_line: u32,
    /// Everything after the closing `---` line, or the whole file (without a BOM) when there is
    /// no frontmatter. Line endings are untouched.
    pub body: &'a str,
    /// The 1-based line where `body` starts.
    pub body_line: u32,
}

/// Why a file could not be split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SplitError {
    /// The first line is `---` and no later line closes it.
    #[error("the frontmatter opened by `---` on line 1 is never closed by a `---` line")]
    Unterminated,
}

impl Classify for SplitError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// Split `text` at its frontmatter. Never panics, whatever `text` is.
pub fn split(text: &str) -> Result<Split<'_>, SplitError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let opens = lines.next().is_some_and(is_delimiter);
    if !opens {
        return Ok(Split {
            frontmatter: None,
            frontmatter_line: 0,
            body: text,
            body_line: 1,
        });
    }
    let yaml_start = text.split_inclusive('\n').next().map_or(0, str::len);
    let mut offset = yaml_start;
    let mut line_no = 2_u32;
    for line in lines {
        if is_delimiter(line) {
            return Ok(Split {
                frontmatter: Some(&text[yaml_start..offset]),
                frontmatter_line: 2,
                body: &text[offset + line.len()..],
                body_line: line_no.saturating_add(1),
            });
        }
        offset += line.len();
        line_no = line_no.saturating_add(1);
    }
    Err(SplitError::Unterminated)
}

/// A `---` line: the three dashes and nothing else but trailing whitespace (`\r` included).
fn is_delimiter(line: &str) -> bool {
    line.trim_end() == "---"
}

/// A body as the manifest keeps it: LF line endings, no leading blank lines, no trailing
/// whitespace. The first line keeps its indentation.
pub(crate) fn clean_body(body: &str) -> String {
    let unix = body.replace("\r\n", "\n");
    let mut rest = unix.as_str();
    while let Some(nl) = rest.find('\n') {
        if rest[..nl].trim().is_empty() {
            rest = &rest[nl + 1..];
        } else {
            break;
        }
    }
    rest.trim_end().to_owned()
}

/// A YAML failure, ready to become a diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct YamlError {
    pub(crate) message: String,
    /// The 1-based line inside the YAML text.
    pub(crate) line: Option<u32>,
}

/// Read `yaml` (YAML 1.2 booleans: `no` is a string) into `T`. Blank or comment-only text is
/// an empty mapping, so `T`'s serde defaults apply.
pub(crate) fn parse_yaml<T: DeserializeOwned>(yaml: &str) -> Result<T, YamlError> {
    let blank = yaml.lines().all(|l| {
        let l = l.trim();
        l.is_empty() || l.starts_with('#')
    });
    let text = if blank { "{}" } else { yaml };
    let mut options = serde_saphyr::Options::default();
    options.strict_booleans = true;
    serde_saphyr::from_str_with_options::<T>(text, options).map_err(|e| YamlError {
        message: first_line(&e.to_string()),
        line: e.location().and_then(|l| u32::try_from(l.line()).ok()),
    })
}

/// The serde-saphyr message can carry a rendered snippet; the diagnostic keeps the first line
/// and gets the line number from the location instead.
fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or(message).trim().to_owned()
}

/// The 1-based line (in the whole file) of the top-level key `key` in `yaml`, when the text has
/// it at column 0. A heuristic for diagnostics: nested keys point at their parent.
pub(crate) fn key_line(split: &Split<'_>, key: &str) -> Option<u32> {
    let yaml = split.frontmatter?;
    let mut line_no = split.frontmatter_line;
    for line in yaml.lines() {
        let trimmed = line.trim_end();
        let unquoted = trimmed
            .strip_prefix('"')
            .and_then(|r| r.strip_prefix(key))
            .and_then(|r| r.strip_prefix('"'))
            .or_else(|| {
                trimmed
                    .strip_prefix('\'')
                    .and_then(|r| r.strip_prefix(key))
                    .and_then(|r| r.strip_prefix('\''))
            })
            .or_else(|| trimmed.strip_prefix(key));
        if unquoted.is_some_and(|rest| rest.trim_start().starts_with(':')) {
            return Some(line_no);
        }
        line_no = line_no.saturating_add(1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_is_all_body() {
        let s = split("# Title\n\ntext\n").unwrap();
        assert_eq!(s.frontmatter, None);
        assert_eq!(s.body, "# Title\n\ntext\n");
        assert_eq!(s.body_line, 1);
    }

    #[test]
    fn splits_at_the_second_delimiter() {
        let s = split("---\nname: x\n---\nbody\n").unwrap();
        assert_eq!(s.frontmatter, Some("name: x\n"));
        assert_eq!(s.body, "body\n");
        assert_eq!((s.frontmatter_line, s.body_line), (2, 4));
    }

    #[test]
    fn empty_frontmatter_and_empty_body() {
        let s = split("---\n---").unwrap();
        assert_eq!(s.frontmatter, Some(""));
        assert_eq!(s.body, "");
        let s = split("---\r\n---\r\n").unwrap();
        assert_eq!(s.frontmatter, Some(""));
    }

    #[test]
    fn unterminated_is_an_error() {
        assert_eq!(split("---\nname: x\n"), Err(SplitError::Unterminated));
        assert_eq!(split("---"), Err(SplitError::Unterminated));
        assert_eq!(split("---\n"), Err(SplitError::Unterminated));
    }

    #[test]
    fn crlf_and_bom() {
        let s = split("\u{feff}---\r\nname: x\r\n---  \r\nbody\r\n").unwrap();
        assert_eq!(s.frontmatter, Some("name: x\r\n"));
        assert_eq!(s.body, "body\r\n");
    }

    #[test]
    fn a_delimiter_must_be_exactly_three_dashes() {
        for text in ["----\nx\n----\n", " ---\nx\n---\n", "--- x\ny\n--- z\n"] {
            let s = split(text).unwrap();
            assert_eq!(s.frontmatter, None, "{text:?}");
            assert_eq!(s.body, text);
        }
    }

    #[test]
    fn a_horizontal_rule_later_in_the_body_is_not_a_delimiter() {
        let s = split("---\na: 1\n---\ntext\n---\nmore\n").unwrap();
        assert_eq!(s.frontmatter, Some("a: 1\n"));
        assert_eq!(s.body, "text\n---\nmore\n");
    }

    #[test]
    fn clean_body_normalises() {
        assert_eq!(
            clean_body("\r\n\r\n  indented\r\nnext\r\n\r\n"),
            "  indented\nnext"
        );
        assert_eq!(clean_body(""), "");
        assert_eq!(clean_body("\n \n"), "");
    }

    #[test]
    fn yaml_blank_is_an_empty_map() {
        #[derive(serde::Deserialize, Default, PartialEq, Debug)]
        #[serde(default)]
        struct T {
            a: Option<u8>,
        }
        assert_eq!(parse_yaml::<T>("").unwrap(), T::default());
        assert_eq!(parse_yaml::<T>("# only a comment\n").unwrap(), T::default());
        assert_eq!(parse_yaml::<T>("a: 3\n").unwrap(), T { a: Some(3) });
    }

    #[test]
    fn yaml_1_2_booleans() {
        #[derive(serde::Deserialize)]
        struct T {
            v: String,
        }
        assert_eq!(parse_yaml::<T>("v: no\n").unwrap().v, "no");
    }

    #[test]
    fn yaml_errors_carry_a_line() {
        #[derive(serde::Deserialize, Debug)]
        struct T {
            #[allow(dead_code)]
            a: u8,
        }
        let e = parse_yaml::<T>("a: 1\nb: [unclosed\n").unwrap_err();
        assert!(e.line.is_some(), "{e:?}");
        assert!(!e.message.contains('\n'));
    }

    #[test]
    fn key_lines() {
        let s = split("---\nname: a\n\"model\": b\nlimits:\n  x: 1\n---\n").unwrap();
        assert_eq!(key_line(&s, "name"), Some(2));
        assert_eq!(key_line(&s, "model"), Some(3));
        assert_eq!(key_line(&s, "limits"), Some(4));
        assert_eq!(key_line(&s, "x"), None);
        assert_eq!(key_line(&s, "nam"), None);
    }
}
