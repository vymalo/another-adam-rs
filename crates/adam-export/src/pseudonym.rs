//! Pseudonymous ids: HMAC-SHA256 under the operator's key (ADR 0087 decision 4).

use std::fmt;

use sha2::{Digest as _, Sha256};

/// The size of the key [`Pseudonyms::random`] makes.
const KEY_BYTES: usize = 32;

/// SHA-256's block size, which HMAC pads the key to.
const BLOCK: usize = 64;

/// Turns a run's or a conversation's id into a pseudonym: the first 16 bytes of
/// `HMAC-SHA256(key, kind || 0x00 || id)`, as 32 lower-case hex digits.
///
/// Two exports under the same key give an id the same pseudonym, so their records link. Without
/// the key a pseudonym does not lead back to the store. Its `Debug` never shows the key.
#[derive(Clone)]
pub struct Pseudonyms {
    /// The key, padded or hashed to one block, as HMAC defines it.
    block: [u8; BLOCK],
}

impl Pseudonyms {
    /// Pseudonyms under `key`. A key longer than a block is hashed first, as HMAC says.
    pub fn new(key: &[u8]) -> Self {
        let mut block = [0u8; BLOCK];
        if key.len() > BLOCK {
            block[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        Self { block }
    }

    /// Pseudonyms under a fresh random key: the records of one export link to each other and to
    /// nothing else.
    ///
    /// # Errors
    ///
    /// [`NoRandomness`] when the system has no source of randomness.
    pub fn random() -> Result<Self, NoRandomness> {
        let mut key = [0u8; KEY_BYTES];
        getrandom::fill(&mut key).map_err(|_| NoRandomness)?;
        Ok(Self::new(&key))
    }

    /// The pseudonym of `id`, an id of `kind` (`run`, `conversation`): the same id of two kinds
    /// gets two pseudonyms.
    pub fn of(&self, kind: &str, id: &str) -> String {
        let mut message = Vec::with_capacity(kind.len() + 1 + id.len());
        message.extend_from_slice(kind.as_bytes());
        message.push(0);
        message.extend_from_slice(id.as_bytes());
        hex(&self.mac(&message)[..16])
    }

    /// `HMAC-SHA256(key, message)` (RFC 2104).
    fn mac(&self, message: &[u8]) -> [u8; 32] {
        let pad = |byte: u8| self.block.map(|k| k ^ byte);
        let inner = Sha256::new()
            .chain_update(pad(0x36))
            .chain_update(message)
            .finalize();
        Sha256::new()
            .chain_update(pad(0x5c))
            .chain_update(inner)
            .finalize()
            .into()
    }
}

impl fmt::Debug for Pseudonyms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pseudonyms(..)")
    }
}

/// The system had no randomness for a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the system has no source of randomness for a pseudonym key")]
pub struct NoRandomness;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4231, test case 2: key "Jefe", data "what do ya want for nothing?".
    #[test]
    fn the_mac_is_hmac_sha256() {
        let p = Pseudonyms::new(b"Jefe");
        assert_eq!(
            hex(&p.mac(b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// RFC 4231, test case 6: a key longer than a block is hashed first.
    #[test]
    fn a_long_key_is_hashed_first() {
        let p = Pseudonyms::new(&[0xaa; 131]);
        assert_eq!(
            hex(&p.mac(b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_pseudonym_depends_on_the_key_and_the_kind() {
        let a = Pseudonyms::new(b"one");
        let b = Pseudonyms::new(b"two");
        assert_eq!(a.of("run", "x"), a.of("run", "x"));
        assert_ne!(a.of("run", "x"), b.of("run", "x"));
        assert_ne!(a.of("run", "x"), a.of("conversation", "x"));
        assert_eq!(a.of("run", "x").len(), 32);
        assert_eq!(format!("{a:?}"), "Pseudonyms(..)");
    }
}
