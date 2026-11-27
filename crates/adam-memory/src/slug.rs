//! Stable file and directory names: a readable slug and a SHA-256 prefix.
//!
//! The hash is what identifies a scope or a problem; the slug is only there so a person looking at
//! the folder can tell what it holds. A different build of adam computes the same hash.

use sha2::{Digest as _, Sha256};

/// A lower-case ASCII stem of at most `max` bytes, with every other byte replaced by `-`.
pub(crate) fn slug(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(max);
    for ch in text.chars() {
        if out.len() >= max {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "scope".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The first 16 hexadecimal digits of SHA-256 of `text`: 64 bits, enough for 64 problems in a
/// folder and unique across builds.
pub(crate) fn digest(text: &str) -> String {
    let hash = Sha256::digest(text.as_bytes());
    hash.iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `<slug>-<digest>` for a file or directory name.
pub(crate) fn name(text: &str, max: usize) -> String {
    format!("{}-{}", slug(text, max), digest(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_keeps_letters_and_digits_and_folds_the_rest() {
        assert_eq!(slug("Stale lockfile!", 40), "stale-lockfile");
        assert_eq!(slug("../../etc/passwd", 40), "etc-passwd");
        assert_eq!(slug("...", 40), "scope");
        assert_eq!(slug("abcdef", 3), "abc");
    }

    #[test]
    fn a_digest_is_stable_and_sixteen_digits() {
        let once = digest("deployment\u{1f}caller\u{1f}agent\u{1f}agent");
        assert_eq!(once, digest("deployment\u{1f}caller\u{1f}agent\u{1f}agent"));
        assert_eq!(once.len(), 16);
        assert_ne!(
            once,
            digest("deployment\u{1f}caller\u{1f}agent\u{1f}repository")
        );
    }

    #[test]
    fn a_name_carries_the_slug_and_the_digest() {
        let name = name("the lockfile is stale", 20);
        assert!(name.starts_with(&format!("{}-", slug("the lockfile is stale", 20))));
        assert!(name.ends_with(&digest("the lockfile is stale")));
    }
}
