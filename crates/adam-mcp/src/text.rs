//! Bounded text: what a server says is text a stranger wrote, and it goes into a context window.

/// The most bytes of a tool's answer that reach the model (the same limit as the answer of a
/// remote subagent).
pub const MAX_RESULT_BYTES: usize = 64 * 1024;

/// The most bytes of a tool description that reach the model.
pub(crate) const MAX_DESCRIPTION_BYTES: usize = 8 * 1024;

/// The longest message of a failure that is shown or logged.
pub(crate) const MAX_MESSAGE_BYTES: usize = 2_000;

/// `text`, cut to at most `max` bytes on a character boundary, with a note when it was cut.
///
/// Copied from `adam-assembly/src/remote.rs` (slice S9b), like the URL helpers: one rule for what
/// a stranger's text may cost, in both crates.
pub(crate) fn cap_text(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let total = text.len();
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!(
        "\n[cut here: {} of {total} bytes shown]",
        text.len()
    ));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_untouched_and_long_text_is_cut_on_a_boundary() {
        assert_eq!(cap_text("abc".into(), 3), "abc");
        let cut = cap_text("é".repeat(10), 5);
        assert!(
            cut.starts_with("éé\n[cut here: 4 of 20 bytes shown]"),
            "{cut}"
        );
    }
}
