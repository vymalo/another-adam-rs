//! Streamed text: the bounds of a [`RunEvent::TextDelta`](crate::RunEvent::TextDelta) and the one way
//! to cut text into pieces that respect them.
//!
//! The vocabulary is the contract's, `text-stream/v1` of the orchestration layer
//! (`docs/api/text-stream-v1.md` of `vymalo/another-agentic-system`), and what an agent does with
//! it is `adam-llm-agent`'s. Like every [`RunEvent`](crate::RunEvent) a piece is best effort and
//! not durable.

/// The `kind` of the [`RunEvent::Custom`](crate::RunEvent::Custom) an `LlmAgent` emits for the words of
/// a model turn: `{"text": .., "turn": ..}`, and `"stream": ..` when they were streamed and the turn goes
/// on to call tools (the answer that ends the run says its stream in the run's output).
pub const AGENT_TEXT_KIND: &str = "agent_text";

/// The longest a stream's id may be, in bytes: the contract's limit.
pub const MAX_STREAM_ID_BYTES: usize = 128;

/// The most bytes of text one [`RunEvent::TextDelta`](crate::RunEvent::TextDelta) holds. It keeps
/// the event inside what crosses between processes: PostgreSQL's `NOTIFY` takes a payload of under
/// 8000 bytes, and a piece made of control characters, which JSON writes in six bytes each, is
/// still under 7000 (`adam-notify-postgres` tests the largest one).
pub const MAX_TEXT_DELTA_BYTES: usize = 1024;

/// `serde`'s `skip_serializing_if` for a flag that is only written when it is set.
pub(crate) fn is_false(flag: &bool) -> bool {
    !*flag
}

/// The length of the longest prefix of `text` that is at most `max` bytes and ends on a character
/// boundary. At least one character, so a caller that cuts in a loop always makes progress, even when
/// `max` is smaller than the first character; `0` only for an empty `text`.
pub fn floor_boundary(text: &str, max: usize) -> usize {
    if text.len() <= max {
        return text.len();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        // The first character is wider than `max`: it goes whole.
        return text.chars().next().map_or(0, char::len_utf8);
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_never_splits_a_character_and_always_makes_progress() {
        assert_eq!(floor_boundary("", 4), 0);
        assert_eq!(floor_boundary("abc", 4), 3);
        assert_eq!(floor_boundary("abcdef", 4), 4);
        // `é` is two bytes: a cut at 3 would split the second.
        assert_eq!(floor_boundary("abé", 3), 2);
        assert_eq!(floor_boundary("abé", 4), 4);
        // A character wider than the limit goes whole.
        assert_eq!(floor_boundary("😀x", 2), 4);
        assert_eq!(floor_boundary("😀x", 0), 4);
        assert_eq!(floor_boundary("😀x", 5), 5);
    }
}
