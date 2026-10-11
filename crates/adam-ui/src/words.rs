//! What counts as the same words: the typed answer of a form compared with the default it started
//! with.
//!
//! A page may change the words of a typed question on their way back (a `trim()` that also removes
//! U+FEFF, a different handling of a line break), so two spellings of the same words are compared
//! after what is not visible has been taken out.

/// Whether `c` is a control character (category Cc: `U+0000..=U+001F`, `U+007F..=U+009F`) or a format
/// character (category Cf: U+00AD, U+FEFF, the zero-width and bidirectional marks, the tag
/// characters, ...). They draw nothing, and a page may keep them or drop them.
///
/// The Cf ranges are those of Unicode 14.0, *verified 2026-10-11* by listing the category `Cf` with
/// Python 3's `unicodedata` (Unicode 14.0.0), and widened where later versions add to a range or the
/// rest of its block is unassigned (`U+2060..=U+206F`, `U+13430..=U+1343F`, `U+E0000..=U+E007F`).
/// A character a later version of Unicode adds elsewhere is not covered.
#[must_use]
pub fn is_format_or_control(c: char) -> bool {
    const FORMAT: [(u32, u32); 19] = [
        (0x00AD, 0x00AD),
        (0x0600, 0x0605),
        (0x061C, 0x061C),
        (0x06DD, 0x06DD),
        (0x070F, 0x070F),
        (0x0890, 0x0891),
        (0x08E2, 0x08E2),
        (0x180E, 0x180E),
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x206F),
        (0xFEFF, 0xFEFF),
        (0xFFF9, 0xFFFB),
        (0x110BD, 0x110BD),
        (0x110CD, 0x110CD),
        (0x13430, 0x1343F),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0000, 0xE007F),
    ];
    let code = u32::from(c);
    c.is_control() || FORMAT.iter().any(|&(from, to)| (from..=to).contains(&code))
}

/// `text` as it is compared: without whitespace and without any control or format character
/// ([`is_format_or_control`]), and in lowercase (the letters that lowercase to another ASCII letter,
/// as U+212A KELVIN SIGN does, are covered by it). Two texts that read the same to a person have the
/// same one.
#[must_use]
pub fn comparable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && !is_format_or_control(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_formats_are_not_visible() {
        for c in [
            '\u{0}',
            '\n',
            '\t',
            '\u{7f}',
            '\u{85}',
            '\u{ad}',
            '\u{600}',
            '\u{61c}',
            '\u{180e}',
            '\u{200b}',
            '\u{200c}',
            '\u{200d}',
            '\u{200e}',
            '\u{200f}',
            '\u{202a}',
            '\u{202e}',
            '\u{2060}',
            '\u{2066}',
            '\u{2069}',
            '\u{feff}',
            '\u{fff9}',
            '\u{110bd}',
            '\u{1d173}',
            '\u{e0001}',
            '\u{e0041}',
            '\u{e007f}',
        ] {
            assert!(is_format_or_control(c), "{:?}", c);
        }
        for c in [
            'a',
            'Z',
            '0',
            ' ',
            '#',
            '/',
            'é',
            '\u{2028}',
            '\u{a0}',
            '\u{3000}',
            '\u{1f600}',
        ] {
            assert!(!is_format_or_control(c), "{:?}", c);
        }
    }

    #[test]
    fn what_is_compared_is_what_a_person_would_read() {
        let default = "merge acme/widgets#7";
        for same in [
            "merge acme/widgets#7",
            "\u{feff}merge acme/widgets#7",
            " merge\u{200b} acme/widgets#7\u{2060} ",
            "MERGE acme/widgets#7",
            "merge\u{a0}acme/widgets#7",
            "mer\u{ad}ge acme/widgets#7\n",
            "\u{202e}merge acme/widgets#7\u{202c}",
            "merge acme/widgets#7\u{e0041}",
        ] {
            assert_eq!(comparable(same), comparable(default), "{same:?}");
        }
        assert_ne!(comparable("merge acme/widgets#8"), comparable(default));
        // U+212A KELVIN SIGN is a `k` once it is lowercase.
        assert_eq!(comparable("Mer\u{212a}"), comparable("merk"));
    }
}
