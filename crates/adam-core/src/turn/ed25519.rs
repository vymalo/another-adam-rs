//! [`TurnKeys`] over one Ed25519 private key held in the process.

use std::path::Path;
use std::sync::Arc;

use adam_error::{Classify, ErrorClass};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer as _, SigningKey};
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};

use super::{CurrentKey, KeyId, KeySet, KeySetError, PublicKey, Signature, TurnKeyError, TurnKeys};

/// The DER of a PKCS#8 v1 Ed25519 private key up to the 32-byte seed (RFC 8410 §7: `openssl
/// genpkey -algorithm ed25519`).
const PKCS8_V1: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
/// The same for PKCS#8 v2 (RFC 5958 `OneAsymmetricKey` version 1, the public key after the seed).
const PKCS8_V2_HEAD: [u8; 16] = [
    0x30, 0x53, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
/// The public key of a v2 document as ring and aws-lc-rs write it: `[1]` around a BIT STRING.
const PKCS8_V2_EXPLICIT: [u8; 5] = [0xa1, 0x23, 0x03, 0x21, 0x00];
/// The public key of a v2 document as RFC 5958 writes it: an implicit `[1]` BIT STRING.
const PKCS8_V2_IMPLICIT: [u8; 3] = [0x81, 0x21, 0x00];

/// Signs with one Ed25519 private key and verifies with a [`KeySet`] that holds its public key:
/// the key a server mounts from a file ([`from_files`](Self::from_files), the variables of
/// [ADR 0068] decision 7 are slice T2's), or one made at start ([`ephemeral`](Self::ephemeral)).
///
/// The key never leaves it: no method gives it out, `Debug` shows its id, and the bytes it was
/// read from are wiped once it is built; the key itself is wiped on drop.
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
pub struct Ed25519TurnKeys {
    key: SigningKey,
    kid: KeyId,
    set: Arc<KeySet>,
    origin: &'static str,
}

impl std::fmt::Debug for Ed25519TurnKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ed25519TurnKeys")
            .field("origin", &self.origin)
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl Ed25519TurnKeys {
    /// A key made now from the system's random numbers, named by its RFC 7638 thumbprint, with
    /// its own public key as the key set. It dies with the process.
    ///
    /// # Errors
    ///
    /// [`TurnKeyConfigError::NoRandomness`] when the system gives no random numbers.
    pub fn ephemeral() -> Result<Self, TurnKeyConfigError> {
        let mut seed = Zeroizing::new([0_u8; 32]);
        getrandom::fill(seed.as_mut_slice()).map_err(|_| TurnKeyConfigError::NoRandomness)?;
        Self::new(SigningKey::from_bytes(&seed), None, None, "ephemeral")
    }

    /// The key a PKCS#8 PEM private key holds (`-----BEGIN PRIVATE KEY-----`, v1 or v2), named
    /// `kid` (default: its RFC 7638 thumbprint), verifying with `verify` (default: none but its
    /// own public key, which is always added).
    ///
    /// # Errors
    ///
    /// [`TurnKeyConfigError`]: the PEM is not an Ed25519 PKCS#8 private key, the set already holds
    /// another key under `kid`, or revokes `kid`.
    pub fn from_pkcs8_pem(
        pem: &SecretString,
        kid: Option<KeyId>,
        verify: Option<KeySet>,
    ) -> Result<Self, TurnKeyConfigError> {
        let der = pem_body(pem.expose_secret())?;
        let key = key_from_pkcs8(&der)?;
        Self::new(key, kid, verify, "pkcs8")
    }

    /// The key of the PEM file `key` and, when given, the verify set of the JWKS file `verify`:
    /// the files `TURN_SIGNING_KEY_FILE` and `TURN_VERIFY_KEYS_FILE` name ([ADR 0068] decision 7).
    ///
    /// # Errors
    ///
    /// [`TurnKeyConfigError::Unreadable`] naming which file, and as
    /// [`from_pkcs8_pem`](Self::from_pkcs8_pem).
    ///
    /// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
    pub fn from_files(
        key: &Path,
        kid: Option<KeyId>,
        verify: Option<&Path>,
    ) -> Result<Self, TurnKeyConfigError> {
        let bytes = Zeroizing::new(std::fs::read(key).map_err(|source| {
            TurnKeyConfigError::Unreadable {
                which: "signing key",
                source,
            }
        })?);
        let text = std::str::from_utf8(&bytes).map_err(|_| TurnKeyConfigError::NotPkcs8)?;
        let pem = SecretString::from(text);
        let verify = match verify {
            None => None,
            Some(path) => {
                let bytes =
                    std::fs::read(path).map_err(|source| TurnKeyConfigError::Unreadable {
                        which: "verify key set",
                        source,
                    })?;
                Some(KeySet::from_jwks_slice(&bytes)?)
            }
        };
        let mut keys = Self::from_pkcs8_pem(&pem, kid, verify)?;
        keys.origin = "file";
        Ok(keys)
    }

    fn new(
        key: SigningKey,
        kid: Option<KeyId>,
        verify: Option<KeySet>,
        origin: &'static str,
    ) -> Result<Self, TurnKeyConfigError> {
        let public = PublicKey::from_verifying(key.verifying_key());
        let kid = match kid {
            Some(kid) => kid,
            None => KeyId::new(public.thumbprint()).map_err(|_| TurnKeyConfigError::NotPkcs8)?,
        };
        let mut set = verify.unwrap_or_default();
        if set.is_revoked(&kid) {
            return Err(TurnKeyConfigError::SigningKeyRevoked { kid });
        }
        set.insert(kid.clone(), public)?;
        Ok(Self {
            key,
            kid,
            set: Arc::new(set),
            origin,
        })
    }

    /// The id its signatures name.
    pub fn kid(&self) -> &KeyId {
        &self.kid
    }

    /// Its public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey::from_verifying(self.key.verifying_key())
    }

    /// The keys it verifies with: its own and those it was given.
    pub fn keys(&self) -> &KeySet {
        &self.set
    }
}

#[async_trait]
impl TurnKeys for Ed25519TurnKeys {
    async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
        Ok(CurrentKey::active(self.kid.clone()))
    }

    async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
        if *kid != self.kid {
            return Err(TurnKeyError::NotSigning { kid: kid.clone() });
        }
        Ok(Signature::from_bytes(self.key.sign(input).to_bytes()))
    }

    async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
        Ok(Arc::clone(&self.set))
    }

    fn kind(&self) -> &'static str {
        self.origin
    }
}

/// The DER between the first `-----BEGIN PRIVATE KEY-----` and its end line.
fn pem_body(pem: &str) -> Result<Zeroizing<Vec<u8>>, TurnKeyConfigError> {
    let mut lines = pem.lines().map(str::trim);
    let label = lines
        .find_map(|line| {
            line.strip_prefix("-----BEGIN ")
                .and_then(|rest| rest.strip_suffix("-----"))
        })
        .ok_or(TurnKeyConfigError::NotPkcs8)?;
    match label {
        "PRIVATE KEY" => {}
        "PUBLIC KEY" => return Err(TurnKeyConfigError::NotPrivate),
        _ => return Err(TurnKeyConfigError::NotPkcs8),
    }
    let mut body = Zeroizing::new(String::new());
    let mut ended = false;
    for line in lines {
        if line == "-----END PRIVATE KEY-----" {
            ended = true;
            break;
        }
        body.push_str(line);
    }
    if !ended {
        return Err(TurnKeyConfigError::NotPkcs8);
    }
    STANDARD
        .decode(body.as_bytes())
        .map(Zeroizing::new)
        .map_err(|_| TurnKeyConfigError::NotPkcs8)
}

/// An Ed25519 key from a PKCS#8 document: v1, or v2 whose public key must be the seed's.
fn key_from_pkcs8(der: &[u8]) -> Result<SigningKey, TurnKeyConfigError> {
    let seed_of = |rest: &[u8]| -> Option<Zeroizing<[u8; 32]>> {
        rest.get(..32)
            .and_then(|s| <[u8; 32]>::try_from(s).ok())
            .map(Zeroizing::new)
    };
    if let Some(rest) = der.strip_prefix(&PKCS8_V1) {
        if rest.len() != 32 {
            return Err(TurnKeyConfigError::NotEd25519);
        }
        let seed = seed_of(rest).ok_or(TurnKeyConfigError::NotEd25519)?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    let (rest, public) = if let Some(rest) = der.strip_prefix(&PKCS8_V2_HEAD) {
        let public = rest
            .get(32..)
            .and_then(|tail| tail.strip_prefix(&PKCS8_V2_EXPLICIT))
            .ok_or(TurnKeyConfigError::NotEd25519)?;
        (rest, public)
    } else if let Some(rest) = der.strip_prefix(&V2_IMPLICIT_HEAD) {
        let public = rest
            .get(32..)
            .and_then(|tail| tail.strip_prefix(&PKCS8_V2_IMPLICIT))
            .ok_or(TurnKeyConfigError::NotEd25519)?;
        (rest, public)
    } else {
        return Err(TurnKeyConfigError::NotEd25519);
    };
    let seed = seed_of(rest).ok_or(TurnKeyConfigError::NotEd25519)?;
    let key = SigningKey::from_bytes(&seed);
    if public != key.verifying_key().as_bytes() {
        return Err(TurnKeyConfigError::PublicKeyMismatch);
    }
    Ok(key)
}

/// The v2 head when the public key is the implicit `[1]` (two bytes shorter).
const V2_IMPLICIT_HEAD: [u8; 16] = [
    0x30, 0x51, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// Why [`Ed25519TurnKeys`] cannot be built. Says what is wrong, never what the key or file holds.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TurnKeyConfigError {
    /// A file cannot be read.
    #[error("the turn {which} file cannot be read")]
    Unreadable {
        /// `signing key` or `verify key set`.
        which: &'static str,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The text is not a PKCS#8 PEM private key (`-----BEGIN PRIVATE KEY-----`).
    #[error(
        "the turn signing key is not a PKCS#8 PEM private key (`openssl genpkey -algorithm ed25519`)"
    )]
    NotPkcs8,
    /// The PEM is a public key: it verifies, it cannot sign.
    #[error("the turn signing key is a public key: signing needs the private key")]
    NotPrivate,
    /// The PKCS#8 key is not Ed25519, or not a form this reads.
    #[error("the turn signing key is not an Ed25519 key")]
    NotEd25519,
    /// A v2 PKCS#8 key whose public key is not its private key's.
    #[error("the turn signing key's public key does not match its private key")]
    PublicKeyMismatch,
    /// The verify set revokes the signing key.
    #[error("the turn signing key `{kid}` is revoked in the verify key set")]
    SigningKeyRevoked {
        /// The signing key's id.
        kid: KeyId,
    },
    /// The verify set cannot be read, or holds another key under the signing key's id.
    #[error("the turn verify key set cannot be used")]
    KeySet(#[from] KeySetError),
    /// The system gives no random numbers (an ephemeral key).
    #[error("no random numbers to make a turn key")]
    NoRandomness,
}

impl Classify for TurnKeyConfigError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::NoRandomness => ErrorClass::Internal,
            Self::Unreadable { .. }
            | Self::NotPkcs8
            | Self::NotPrivate
            | Self::NotEd25519
            | Self::PublicKeyMismatch
            | Self::SigningKeyRevoked { .. }
            | Self::KeySet(_) => ErrorClass::Invalid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];

    /// The three PKCS#8 forms read to the same key: v1, and v2 with the public key written as ring
    /// and aws-lc-rs write it or as RFC 5958 does.
    #[test]
    fn pkcs8_v1_and_both_forms_of_v2_read_alike() {
        let public = SigningKey::from_bytes(&SEED).verifying_key().to_bytes();
        let v1 = [&PKCS8_V1[..], &SEED].concat();
        let explicit = [&PKCS8_V2_HEAD[..], &SEED, &PKCS8_V2_EXPLICIT, &public].concat();
        let implicit = [&V2_IMPLICIT_HEAD[..], &SEED, &PKCS8_V2_IMPLICIT, &public].concat();
        for der in [v1, explicit, implicit] {
            let key = key_from_pkcs8(&der).unwrap();
            assert_eq!(key.verifying_key().to_bytes(), public);
        }
        let mut trailing = [&PKCS8_V1[..], &SEED].concat();
        trailing.push(0);
        assert!(matches!(
            key_from_pkcs8(&trailing),
            Err(TurnKeyConfigError::NotEd25519)
        ));
    }
}
