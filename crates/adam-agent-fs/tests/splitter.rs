//! The splitter and the parsers take arbitrary text without panicking.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration tests assert by unwrapping

use std::path::Path;

use adam_agent_fs::{SkillLayout, parse_mcp, parse_skill, split};
use proptest::prelude::*;

/// Text that looks like the interesting cases: delimiters, CR, LF, a BOM, YAML punctuation.
fn textish() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just("---".to_owned()),
            Just("\n".to_owned()),
            Just("\r\n".to_owned()),
            Just("\u{feff}".to_owned()),
            Just(" ".to_owned()),
            Just(": ".to_owned()),
            Just("- ".to_owned()),
            Just("name".to_owned()),
            Just("description".to_owned()),
            Just("[".to_owned()),
            Just("{".to_owned()),
            Just("\"".to_owned()),
            Just("&a ".to_owned()),
            Just("*a".to_owned()),
            Just("!!".to_owned()),
            "\\PC{0,6}",
        ],
        0..24,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #[test]
    fn split_never_panics_on_arbitrary_text(s in "\\PC*") {
        let _ = split(&s);
    }

    #[test]
    fn split_never_panics_on_delimiter_heavy_text(s in textish()) {
        let _ = split(&s);
    }

    #[test]
    fn split_keeps_every_byte_in_order(s in textish()) {
        let bare = s.strip_prefix('\u{feff}').unwrap_or(&s);
        if let Ok(parts) = split(&s) {
            match parts.frontmatter {
                None => {
                    prop_assert_eq!(parts.body, bare);
                    prop_assert_eq!(parts.body_line, 1);
                }
                Some(front) => {
                    // opening line + front + closing line + body is the whole text.
                    let opening = bare.split_inclusive('\n').next().unwrap();
                    prop_assert_eq!(opening.trim_end(), "---");
                    let closing = &bare[opening.len() + front.len()..bare.len() - parts.body.len()];
                    prop_assert_eq!(closing.trim_end(), "---");
                    prop_assert!(bare.starts_with(opening));
                    prop_assert!(bare[opening.len()..].starts_with(front));
                    prop_assert!(bare.ends_with(parts.body));
                    prop_assert_eq!(parts.frontmatter_line, 2);
                    if closing.ends_with('\n') {
                        let lines_before_body = bare[..bare.len() - parts.body.len()].matches('\n').count();
                        prop_assert_eq!(parts.body_line as usize, lines_before_body + 1);
                    }
                }
            }
        }
    }

    #[test]
    fn a_missing_closing_delimiter_is_always_an_error(rest in "[a-z: \\n]{0,40}") {
        // No line of `rest` can be `---`, so an opening `---` is never closed.
        let text = format!("---\n{rest}");
        prop_assert!(split(&text).is_err());
    }

    #[test]
    fn the_skill_parser_never_panics(s in textish()) {
        let mut d = Vec::new();
        let _ = parse_skill(Path::new("x/SKILL.md"), &s, "x", SkillLayout::Directory, &mut d);
        let _ = parse_skill(Path::new("x.md"), &s, "x", SkillLayout::Flat, &mut d);
    }

    #[test]
    fn the_skill_parser_never_panics_inside_a_frontmatter(y in "\\PC{0,80}") {
        let text = format!("---\n{y}\n---\nbody\n");
        let mut d = Vec::new();
        let _ = parse_skill(Path::new("x/SKILL.md"), &text, "x", SkillLayout::Directory, &mut d);
    }

    #[test]
    fn the_mcp_parser_never_panics(s in "\\PC*") {
        let mut d = Vec::new();
        let _ = parse_mcp(Path::new("mcp.json"), &s, &mut d);
    }
}
