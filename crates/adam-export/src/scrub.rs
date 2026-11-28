//! What removes secrets from a record's text (ADR 0087 decision 4).

use std::borrow::Cow;
use std::sync::LazyLock;

use adam_llm_agent::REDACTED_MARKER;
use regex::Regex;

/// Removes the secrets a deployment knows from a text. Every string of a record passes it.
///
/// A binary builds it from the redactor its tools use, the secrets its configuration holds:
/// `adam-export` cannot know them. A closure `Fn(&str) -> String` is one.
pub trait Scrub: Send + Sync {
    /// `text` with every known secret replaced.
    fn scrub(&self, text: &str) -> String;
}

impl<F> Scrub for F
where
    F: Fn(&str) -> String + Send + Sync,
{
    fn scrub(&self, text: &str) -> String {
        self(text)
    }
}

/// A [`Scrub`] that knows no secret: only the [`well-known shapes`](scrub_known_shapes) are
/// removed, which every export does anyway.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSecrets;

impl Scrub for NoSecrets {
    fn scrub(&self, text: &str) -> String {
        text.to_owned()
    }
}

/// Tokens of well-known shapes, which no deployment needs to declare: GitHub tokens, OpenAI- and
/// Anthropic-style keys, AWS access key ids, Slack tokens, bearer headers, PEM private keys, and
/// the password of a URL.
static KNOWN_SHAPES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}",
        r"\bgithub_pat_[A-Za-z0-9_]{20,}",
        r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}",
        r"\bAKIA[0-9A-Z]{16}\b",
        r"\bxox[abprs]-[A-Za-z0-9-]{10,}",
        r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{16,}",
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
    ]
    .iter()
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// The password of a `scheme://user:password@host` URL.
static URL_PASSWORD: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"([a-zA-Z][a-zA-Z0-9+.-]*://[^\s:/@]+:)[^\s@/]+@").ok());

/// `text` with every token of a well-known shape replaced by [`REDACTED_MARKER`], and a URL's
/// password too. Applied after the deployment's [`Scrub`], to every string of every record.
pub fn scrub_known_shapes(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for shape in KNOWN_SHAPES.iter() {
        if shape.is_match(&out) {
            out = Cow::Owned(shape.replace_all(&out, REDACTED_MARKER).into_owned());
        }
    }
    if let Some(url) = URL_PASSWORD.as_ref()
        && url.is_match(&out)
    {
        let replaced = url.replace_all(&out, format!("${{1}}{REDACTED_MARKER}@"));
        out = Cow::Owned(replaced.into_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_shape_is_removed() {
        let secrets = [
            "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
            "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
            "sk-proj-abcdefghijklmnopqrstuvwxyz012345",
            "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
            "AKIAIOSFODNN7EXAMPLE",
            "xoxb-1234567890-abcdefghij",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----",
        ];
        for secret in secrets {
            let text = format!("before {secret} after");
            let scrubbed = scrub_known_shapes(&text);
            assert!(!scrubbed.contains(secret), "{secret} survived: {scrubbed}");
            assert!(scrubbed.contains(REDACTED_MARKER), "{scrubbed}");
            assert!(scrubbed.starts_with("before ") && scrubbed.ends_with(" after"));
        }
        let header = scrub_known_shapes("Authorization: Bearer abcdefghijklmnop0123");
        assert!(!header.contains("abcdefghijklmnop0123"), "{header}");
    }

    #[test]
    fn a_url_keeps_everything_but_its_password() {
        let scrubbed = scrub_known_shapes("postgres://adam:hunter2@db:5432/adam");
        assert_eq!(
            scrubbed,
            format!("postgres://adam:{REDACTED_MARKER}@db:5432/adam")
        );
    }

    #[test]
    fn ordinary_text_is_borrowed() {
        let text = "fn main() { let sk = 1; } // ask the person";
        assert!(matches!(scrub_known_shapes(text), Cow::Borrowed(_)));
    }

    #[test]
    fn a_closure_is_a_scrub() {
        let scrub = |text: &str| text.replace("s3cret", "[gone]");
        assert_eq!(Scrub::scrub(&scrub, "a s3cret"), "a [gone]");
        assert_eq!(NoSecrets.scrub("a s3cret"), "a s3cret");
    }
}
