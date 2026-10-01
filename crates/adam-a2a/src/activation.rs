//! Which extensions a request activates.
//!
//! A client asks for an extension by naming its URI in the `A2A-Extensions` request header (a
//! comma-separated list) and, in a message it sends, in `message.extensions`. The agent honours
//! the ones its card declares, and says which in the `A2A-Extensions` header of its response
//! (*verified* 2026-10-01, <https://a2a-protocol.org/latest/topics/extensions/>: "the response
//! SHOULD include the `A2A-Extensions` header, listing all extensions that were successfully
//! activated for that request"). The page does not mention `Message.extensions`; the orchestration
//! layer sends both, so both count.
//!
//! The rule is one pure function, [`activated`], used by the handler (to fill
//! [`Caller::extensions`](crate::Caller::extensions)) and by the layer that echoes the header, so
//! they cannot disagree.

/// The header, as the SDK's `ServiceParams` and `http` spell it (lower case).
pub(crate) const HEADER: &str = "a2a-extensions";

/// The URIs a header value names: split at commas, trimmed, empty entries skipped.
pub(crate) fn split(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|uri| !uri.is_empty())
}

/// The extensions a request activates: the URIs it names (the header values first, then the
/// message's own list) that the card `declared`, each once, in the order they were named. An exact
/// match: no other version, no trailing slash, no other case.
pub(crate) fn activated<'a>(
    declared: &[String],
    header: impl IntoIterator<Item = &'a str>,
    message: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for uri in header
        .into_iter()
        .flat_map(split)
        .chain(message.into_iter().flat_map(split))
    {
        if declared.iter().any(|d| d == uri) && !out.iter().any(|o| o == uri) {
            out.push(uri.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared() -> Vec<String> {
        vec!["urn:a".into(), "urn:b".into()]
    }

    #[test]
    fn only_declared_uris_are_activated_each_once_in_the_order_named() {
        assert_eq!(
            activated(&declared(), ["urn:b, urn:x ,urn:a", "urn:b"], ["urn:a"]),
            ["urn:b", "urn:a"]
        );
    }

    #[test]
    fn a_near_miss_is_not_a_match() {
        for near in ["URN:A", "urn:a/", "urn:", "urn:aa", " ", "urn:a;q=1"] {
            let got = activated(&declared(), [near], []);
            assert!(got.is_empty(), "{near:?} activated {got:?}");
        }
        // Spaces around a URI are not part of it.
        assert_eq!(activated(&declared(), [" urn:a "], []), ["urn:a"]);
    }

    #[test]
    fn nothing_declared_or_nothing_named_activates_nothing() {
        assert!(activated(&[], ["urn:a"], ["urn:b"]).is_empty());
        assert!(activated(&declared(), [], []).is_empty());
        assert!(activated(&declared(), [",,, ,"], [""]).is_empty());
    }

    #[test]
    fn the_message_alone_activates_too() {
        assert_eq!(activated(&declared(), [], ["urn:a"]), ["urn:a"]);
    }
}
