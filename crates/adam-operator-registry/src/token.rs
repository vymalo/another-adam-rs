//! The bearer token of the registry: one static secret, read from a file, compared in constant time.

use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Why no token could be had. The registry is not served without one (fail closed).
#[derive(Debug)]
pub enum TokenError {
    /// The file could not be read.
    Unreadable(std::io::Error),
    /// The file holds no token: it is empty, or only whitespace.
    Empty,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "the token file cannot be read: {e}"),
            Self::Empty => write!(f, "the token file is empty"),
        }
    }
}

impl std::error::Error for TokenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable(e) => Some(e),
            Self::Empty => None,
        }
    }
}

/// The token a caller must present. `Debug` never prints it.
///
/// It is held as the SHA-256 of its text: two digests of the same length are what is compared (with
/// [`subtle`], in constant time), so neither the length of the token nor the position of the first
/// differing byte of a guess shows in the time a comparison takes.
#[derive(Clone)]
pub struct Token {
    digest: [u8; 32],
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// A token from its text, with the whitespace around it (a trailing newline) removed. `None` when
    /// nothing is left.
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self {
            digest: Sha256::digest(text.as_bytes()).into(),
        })
    }

    /// The token in a file, as a mounted Secret has it.
    ///
    /// # Errors
    ///
    /// [`TokenError`] when the file cannot be read or is empty.
    pub fn from_file(path: &Path) -> Result<Self, TokenError> {
        let text = std::fs::read_to_string(path).map_err(TokenError::Unreadable)?;
        Self::new(&text).ok_or(TokenError::Empty)
    }

    /// Whether `presented` is the token, in constant time.
    pub fn matches(&self, presented: &[u8]) -> bool {
        let theirs: [u8; 32] = Sha256::digest(presented).into();
        self.digest.ct_eq(&theirs).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_matches_only_the_token_and_trims_the_file() {
        let t = Token::new("s3cret-token\n").unwrap();
        assert!(t.matches(b"s3cret-token"));
        assert!(
            !t.matches(b"s3cret-token\n"),
            "the file's newline is not part of it"
        );
        assert!(!t.matches(b"s3cret-tokeN"));
        assert!(!t.matches(b""));
        assert!(!t.matches(b"s3cret-token-and-more"));
    }

    #[test]
    fn nothing_is_not_a_token_and_debug_hides_it() {
        assert!(Token::new("").is_none());
        assert!(Token::new(" \n\t").is_none());
        let shown = format!("{:?}", Token::new("s3cret-token").unwrap());
        assert!(!shown.contains("s3cret"), "{shown}");
    }

    #[test]
    fn a_file_is_read_and_a_missing_or_empty_one_is_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "adam-operator-registry-token-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("token");
        std::fs::write(&file, "from-file\n").unwrap();
        assert!(Token::from_file(&file).unwrap().matches(b"from-file"));
        std::fs::write(&file, "\n").unwrap();
        assert!(matches!(Token::from_file(&file), Err(TokenError::Empty)));
        assert!(matches!(
            Token::from_file(&dir.join("absent")),
            Err(TokenError::Unreadable(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
