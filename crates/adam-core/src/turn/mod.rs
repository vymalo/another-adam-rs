//! The turn key and the turn signature: adam signs the history it produces and verifies every
//! history that comes back ([ADR 0068]).
//!
//! * [`TurnKeys`] is the port: it **signs** bytes with the key that signs now and gives the
//!   [`KeySet`] of public keys a signature is verified with. It never hands out a private key, so
//!   a host may sign in a keychain, a KMS or an HSM. The current key is named by a key id or by a
//!   certificate chain ([`KeyIdentity`]), and says whether it is being renewed ([`KeyStatus`]).
//! * [`Ed25519TurnKeys`] is the implementation over one Ed25519 private key: a PKCS#8 PEM (a file a
//!   server mounts) with a verify set from a JWKS, or a key made at start that dies with the process
//!   ([`Ed25519TurnKeys::ephemeral`], `<dyn TurnKeys>::ephemeral()`).
//! * The form: a detached compact JWS (RFC 7515 appendix F) with the header
//!   `{"alg":"EdDSA","kid":"…","typ":"adam-turn/v1"}` ([`sign_detached`], [`verify_detached`]); the
//!   turn's payload `{typ, run, cnv, idx, prv, msg}` ([`turn_payload`]) and the chain `prv`
//!   ([`ChainHash`]); every byte string signed is RFC 8785 ([`crate::jcs`]).
//! * [`testkit`]: the cases every `TurnKeys` passes, [`turn_keys_conformance!`](crate::turn_keys_conformance).
//!
//! Slice T1 of the ADR builds these; nothing signs or verifies a run's history yet (T3).
//!
//! [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md

mod chain;
mod ed25519;
mod form;
mod key_set;
pub mod testkit;

use std::fmt;
use std::sync::Arc;

use adam_error::{BoxError, Classify, ErrorClass};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

pub use chain::{
    ChainHash, ConversationHash, TurnPosition, set_turn_signature, sign_turn, turn_payload,
    turn_signature, verify_turn,
};
pub use ed25519::{Ed25519TurnKeys, TurnKeyConfigError};
pub use form::{
    MAX_SIGNATURE_LEN, SignatureType, TurnSignatureError, sign_detached, verify_detached,
};
pub use key_set::{KeySet, KeySetError, PublicKey};

/// Signs with the turn key and gives the keys a turn signature is verified with ([ADR 0068]
/// decision 7). The host provides it, adam asks: as `ModelCredentials` does for a model call, and
/// no value is ever logged, journaled or put in an error.
///
/// Signing is two calls, [`current`](Self::current) then [`sign`](Self::sign), because the key's
/// id is inside the bytes signed (the JWS header). An adapter that renews its key between the two
/// may still sign with the key it named (it is still in its set during the overlap of a rotation),
/// or answer [`TurnKeyError::NotSigning`], and the caller asks for the current key again.
///
/// `Debug` of a `dyn TurnKeys`, and of what holds one, shows [`kind`](Self::kind) only.
///
/// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
#[async_trait]
pub trait TurnKeys: Send + Sync + 'static {
    /// The key that signs now: how it is named, and whether it is being renewed.
    ///
    /// # Errors
    ///
    /// [`TurnKeyError`]: the keys cannot be reached or refuse.
    async fn current(&self) -> Result<CurrentKey, TurnKeyError>;

    /// The Ed25519 signature of `input` with the key `kid` names.
    ///
    /// # Errors
    ///
    /// [`TurnKeyError::NotSigning`] when this source cannot sign with that key (never could, or
    /// no longer can); the others when the keys cannot be reached or refuse.
    async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError>;

    /// The public keys a turn signature is verified with, and the ids no longer accepted. Holds
    /// the current key's public key, and every earlier key whose turns are still valid.
    ///
    /// # Errors
    ///
    /// [`TurnKeyError`]: the keys cannot be reached or refuse.
    async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError>;

    /// What this source is, for `Debug` and startup logs. Never a key.
    fn kind(&self) -> &'static str {
        "host"
    }
}

impl fmt::Debug for dyn TurnKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TurnKeys({})", self.kind())
    }
}

/// A shared [`TurnKeys`].
pub type DynTurnKeys = Arc<dyn TurnKeys>;

impl dyn TurnKeys {
    /// A key made now, that dies with the process, as a history in memory does: what an engine or a
    /// test on a `MemoryStore` uses ([ADR 0068] decision 9). Its key set is its own public key.
    ///
    /// # Errors
    ///
    /// [`TurnKeyConfigError::NoRandomness`] when the system has no random numbers to give.
    ///
    /// [ADR 0068]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0068-adam-signs-its-own-history.md
    pub fn ephemeral() -> Result<DynTurnKeys, TurnKeyConfigError> {
        Ok(Arc::new(Ed25519TurnKeys::ephemeral()?))
    }
}

#[async_trait]
impl<T: TurnKeys + ?Sized> TurnKeys for Arc<T> {
    async fn current(&self) -> Result<CurrentKey, TurnKeyError> {
        (**self).current().await
    }

    async fn sign(&self, kid: &KeyId, input: &[u8]) -> Result<Signature, TurnKeyError> {
        (**self).sign(kid, input).await
    }

    async fn key_set(&self) -> Result<Arc<KeySet>, TurnKeyError> {
        (**self).key_set().await
    }

    fn kind(&self) -> &'static str {
        (**self).kind()
    }
}

/// The key that signs now ([`TurnKeys::current`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentKey {
    /// How the key is named in a signature.
    pub identity: KeyIdentity,
    /// Whether it is being renewed.
    pub status: KeyStatus,
}

impl CurrentKey {
    /// A key named by `kid`, not being renewed.
    pub fn active(kid: KeyId) -> Self {
        Self {
            identity: KeyIdentity::Id(kid),
            status: KeyStatus::Active,
        }
    }

    /// The id a signature names it by (`kid`).
    pub fn kid(&self) -> &KeyId {
        self.identity.kid()
    }
}

/// How the signing key is named in a signature's header.
///
/// Closed on purpose: the header writer and every verifier handle each way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyIdentity {
    /// By its id alone (`kid`): the vault reference `vault:<name>#v<n>`, or the RFC 7638 thumbprint
    /// of its public key.
    Id(KeyId),
    /// By a certificate chain that vouches for it (`x5c`, leaf first), with the chain's
    /// [`kid`](CertificateChain::kid) beside it: a key a PKI issues. The key set still names the
    /// key by that id; the chain is carried, and trusting it is the verifier's (slice T1 verifies
    /// by `kid` alone).
    Certificate(CertificateChain),
}

impl KeyIdentity {
    /// The id a signature names the key by (`kid`).
    pub fn kid(&self) -> &KeyId {
        match self {
            Self::Id(kid) => kid,
            Self::Certificate(chain) => chain.kid(),
        }
    }

    /// The certificate chain, when the key is named by one.
    pub fn chain(&self) -> Option<&CertificateChain> {
        match self {
            Self::Id(_) => None,
            Self::Certificate(chain) => Some(chain),
        }
    }
}

/// Whether the signing key is being renewed.
///
/// Closed on purpose: a caller decides what each state means for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyStatus {
    /// It signs, and nothing replaces it yet.
    Active,
    /// It still signs, and a new key is being made (a certificate near its end, a rotation in
    /// progress): its signatures stay valid, and the next [`TurnKeys::current`] may name another.
    Renewing,
}

/// A key id (`kid`): 1 to [`KeyId::MAX_LEN`] bytes of printable ASCII, no space. Not a secret
/// (ADR 0068 decision 7: the vault reference names a key, it does not hold it).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyId(String);

impl KeyId {
    /// The longest key id, in bytes.
    pub const MAX_LEN: usize = 256;

    /// `text` as a key id.
    ///
    /// # Errors
    ///
    /// [`BadKeyId`] when it is empty, longer than [`MAX_LEN`](Self::MAX_LEN), or holds anything but
    /// printable ASCII without space. The error does not repeat the text.
    pub fn new(text: impl Into<String>) -> Result<Self, BadKeyId> {
        let text = text.into();
        if text.is_empty()
            || text.len() > Self::MAX_LEN
            || !text.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(BadKeyId);
        }
        Ok(Self(text))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyId({})", self.0)
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A key id that is not one ([`KeyId::new`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "a turn key id is 1 to {max} bytes of printable ASCII without space",
    max = KeyId::MAX_LEN
)]
pub struct BadKeyId;

impl Classify for BadKeyId {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// The certificates that vouch for a signing key, leaf first, as DER: the `x5c` of RFC 7515
/// §4.1.6. Bounded so a signature stays within [`MAX_SIGNATURE_LEN`].
#[derive(Clone, PartialEq, Eq)]
pub struct CertificateChain {
    der: Vec<Vec<u8>>,
    kid: KeyId,
}

impl CertificateChain {
    /// The most certificates in a chain.
    pub const MAX_CERTIFICATES: usize = 4;
    /// The longest certificate, in DER bytes.
    pub const MAX_CERTIFICATE_LEN: usize = 2048;

    /// The chain `der`, leaf first, named by the `x5t#S256` thumbprint of its leaf (the base64url
    /// SHA-256 of the leaf's DER, RFC 7515 §4.1.8) unless [`with_kid`](Self::with_kid) names it.
    /// The certificates are not parsed: trusting them is the verifier's.
    ///
    /// # Errors
    ///
    /// [`BadCertificateChain`] for an empty chain, more than
    /// [`MAX_CERTIFICATES`](Self::MAX_CERTIFICATES), or an empty certificate or one longer than
    /// [`MAX_CERTIFICATE_LEN`](Self::MAX_CERTIFICATE_LEN).
    pub fn new(der: Vec<Vec<u8>>) -> Result<Self, BadCertificateChain> {
        if der.is_empty()
            || der.len() > Self::MAX_CERTIFICATES
            || der
                .iter()
                .any(|c| c.is_empty() || c.len() > Self::MAX_CERTIFICATE_LEN)
        {
            return Err(BadCertificateChain);
        }
        let thumbprint = URL_SAFE_NO_PAD.encode(Sha256::digest(&der[0]));
        let kid = KeyId::new(thumbprint).map_err(|_| BadCertificateChain)?;
        Ok(Self { der, kid })
    }

    /// The same chain, named `kid` (the id the key set knows the key by).
    #[must_use]
    pub fn with_kid(mut self, kid: KeyId) -> Self {
        self.kid = kid;
        self
    }

    /// The id a signature names the key by.
    pub fn kid(&self) -> &KeyId {
        &self.kid
    }

    /// The certificates, leaf first, as DER.
    pub fn certificates(&self) -> &[Vec<u8>] {
        &self.der
    }
}

impl fmt::Debug for CertificateChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CertificateChain")
            .field("kid", &self.kid)
            .field("certificates", &self.der.len())
            .finish()
    }
}

/// A certificate chain that is not one ([`CertificateChain::new`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "a certificate chain holds 1 to {n} certificates of 1 to {len} bytes each",
    n = CertificateChain::MAX_CERTIFICATES,
    len = CertificateChain::MAX_CERTIFICATE_LEN
)]
pub struct BadCertificateChain;

impl Classify for BadCertificateChain {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// An Ed25519 signature: 64 bytes. Not a secret.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; 64]);

impl Signature {
    /// The signature `bytes`.
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    /// The signature's bytes.
    pub const fn to_bytes(&self) -> [u8; 64] {
        self.0
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", URL_SAFE_NO_PAD.encode(self.0))
    }
}

/// Why [`TurnKeys`] could not sign or give its key set. Carries no key, no input and no signature.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TurnKeyError {
    /// This source cannot sign with the key `kid`: it never could, or it was replaced. Ask for
    /// the current key again.
    #[error("the turn keys do not sign with the key `{kid}`")]
    NotSigning {
        /// The key asked for.
        kid: KeyId,
    },
    /// The keys cannot be reached now (a KMS, a keychain that is not up yet).
    #[error("the turn keys are unavailable")]
    Unavailable(#[source] BoxError),
    /// The keys refuse to sign (a locked keychain, a policy).
    #[error("the turn keys refuse to sign")]
    Refused(#[source] BoxError),
}

impl Classify for TurnKeyError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::NotSigning { .. } => ErrorClass::Conflict,
            Self::Unavailable(_) => ErrorClass::Transient,
            Self::Refused(_) => ErrorClass::Unauthenticated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_id_is_printable_ascii_without_space_and_bounded() {
        assert!(KeyId::new("vault:turn-key#v12").is_ok());
        assert!(KeyId::new("a".repeat(KeyId::MAX_LEN)).is_ok());
        for bad in [
            String::new(),
            "a".repeat(KeyId::MAX_LEN + 1),
            "with space".into(),
            "line\nbreak".into(),
            "é".into(),
        ] {
            assert_eq!(KeyId::new(bad), Err(BadKeyId));
        }
    }

    #[test]
    fn a_certificate_chain_is_bounded_and_named_by_its_leaf() {
        let leaf = vec![1_u8; 300];
        let chain = CertificateChain::new(vec![leaf.clone(), vec![2; 400]]).unwrap();
        assert_eq!(
            chain.kid().as_str(),
            URL_SAFE_NO_PAD.encode(Sha256::digest(&leaf))
        );
        let named = chain
            .clone()
            .with_kid(KeyId::new("vault:pki-turn#serial-1").unwrap());
        assert_eq!(named.kid().as_str(), "vault:pki-turn#serial-1");
        assert_eq!(CertificateChain::new(vec![]), Err(BadCertificateChain));
        assert_eq!(
            CertificateChain::new(vec![vec![1]; CertificateChain::MAX_CERTIFICATES + 1]),
            Err(BadCertificateChain)
        );
        assert_eq!(
            CertificateChain::new(vec![vec![1; CertificateChain::MAX_CERTIFICATE_LEN + 1]]),
            Err(BadCertificateChain)
        );
        assert_eq!(
            CertificateChain::new(vec![vec![]]),
            Err(BadCertificateChain)
        );
    }

    #[test]
    fn each_error_has_its_class() {
        let kid = KeyId::new("k").unwrap();
        assert_eq!(
            TurnKeyError::NotSigning { kid }.class(),
            ErrorClass::Conflict
        );
        assert_eq!(
            TurnKeyError::Unavailable("down".into()).class(),
            ErrorClass::Transient
        );
        assert_eq!(
            TurnKeyError::Refused("locked".into()).class(),
            ErrorClass::Unauthenticated
        );
        assert_eq!(BadKeyId.class(), ErrorClass::Invalid);
        assert_eq!(BadCertificateChain.class(), ErrorClass::Invalid);
    }
}
