//! What counts as the same words: the typed answer of a form compared with the default it started
//! with.
//!
//! A page may change the words of a typed question on their way back (a `trim()` that also removes
//! U+FEFF, a different handling of a line break), so two spellings of the same words are compared
//! after what is not visible has been taken out.

pub use adam_agent_fs::is_format_or_control;

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
