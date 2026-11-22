//! What replaces a secret, and where a value is a value: the rule the whole-value redactors share.
//!
//! A streaming scrubber must not use [`is_value_position`]: its replacement may not depend on where
//! a text starts (`kept.rs`, [`crate::StreamScrubber`]), so it replaces every occurrence. The rule is
//! for the sites that hold a whole value at once (a tool's result, a step's output).

/// What replaces a secret where it is removed.
///
/// It says what happened, so a model that reads a tool's output does not copy it back as if it were
/// a value: the bench's patches carried `[redacted]://localhost:5432` once.
pub const REDACTED_MARKER: &str = "[redacted: secret removed, do not reuse]";

/// Whether `text[at..at + len]` stands where a **value** stands: at the start or end of the text, or
/// between bytes that cannot be part of an identifier, and not as the scheme of a URL
/// (`postgres://…`).
///
/// So a held secret that is also an ordinary word is replaced where it is a value (a DSN's user and
/// password, a bare value, a header value) and kept where it is an identifier (`fn postgres_handler`,
/// `my_postgres_pool`).
#[must_use]
pub fn is_value_position(text: &str, at: usize, len: usize) -> bool {
    let bytes = text.as_bytes();
    let before = at == 0 || !is_identifier_byte(bytes[at - 1]);
    let end = at + len;
    let after = end >= bytes.len() || !is_identifier_byte(bytes[end]);
    let scheme = text[end..].starts_with("://");
    before && after && !scheme
}

fn is_identifier_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, needle: &str) -> bool {
        let at = text.find(needle).expect("the needle is there");
        is_value_position(text, at, needle.len())
    }

    #[test]
    fn a_dsn_keeps_its_scheme_and_its_user_and_password_are_values() {
        let dsn = "postgres://postgres:postgres@db:5432/app";
        assert!(!is_value_position(dsn, 0, "postgres".len()), "the scheme");
        let user = "postgres://".len();
        assert!(is_value_position(dsn, user, "postgres".len()), "the user");
        let password = dsn.find(":postgres@").unwrap() + 1;
        assert!(
            is_value_position(dsn, password, "postgres".len()),
            "the password"
        );
    }

    #[test]
    fn an_identifier_is_not_a_value() {
        assert!(!at("fn postgres_handler() {}", "postgres"));
        assert!(!at("my_postgres_pool", "postgres"));
        assert!(!at("postgres_handler", "postgres"));
        assert!(!at("xpostgres", "postgres"));
    }

    #[test]
    fn a_bare_value_and_a_header_value_are_values() {
        assert!(at("postgres", "postgres"), "the whole text");
        assert!(at("password=postgres", "postgres"));
        assert!(at("Authorization: Basic cG9zdGdyZXM=", "cG9zdGdyZXM="));
        assert!(at("{\"password\": \"postgres\"}", "postgres"));
        assert!(at("user:postgres@host", "postgres"));
    }

    #[test]
    fn the_marker_says_what_happened() {
        assert_eq!(REDACTED_MARKER, "[redacted: secret removed, do not reuse]");
    }

    #[test]
    fn a_prefix_of_a_longer_word_is_not_a_value() {
        assert!(!at("abcdef", "abcd"));
    }
}
